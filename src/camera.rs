use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    time::{Instant, interval, sleep},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    config::{Camera, EventTransport},
    hub::Hub,
    onvif::{Onvif, events::PushRoutes},
    reolink::{self, control::Control, wire::Device},
};

pub struct CameraTask {
    pub config: Camera,
    pub onvif: Option<Onvif>,
    pub baichuan: Option<Device>,
    pub advertise: Option<Url>,
    pub webhook: Option<Url>,
    pub cgi_webhook: Option<reolink::cgi::Webhook>,
    pub routes: PushRoutes,
    pub hub: Arc<Hub>,
    pub stop: CancellationToken,
}

#[derive(Clone)]
pub struct ControlHandle(mpsc::Sender<Command>);

struct Command {
    control: Control,
    replay_id: Option<String>,
    reply: oneshot::Sender<Value>,
    correlation_id: String,
    expires: Instant,
}

impl ControlHandle {
    pub fn enqueue(
        &self,
        control: Control,
        replay_id: Option<String>,
        correlation_id: String,
    ) -> std::result::Result<oneshot::Receiver<Value>, (&'static str, anyhow::Error)> {
        control
            .validate()
            .map_err(|error| ("CONTROL_VALIDATION_FAILED", error))?;
        let (reply, rx) = oneshot::channel();
        let command = Command {
            control,
            replay_id,
            reply,
            correlation_id,
            expires: Instant::now() + Duration::from_secs(15),
        };
        self.0.try_send(command).map_err(|_| {
            (
                "CONTROL_QUEUE_UNAVAILABLE",
                anyhow!("camera command queue unavailable"),
            )
        })?;
        Ok(rx)
    }
}

impl CameraTask {
    pub fn start(self) -> (ControlHandle, impl Future<Output = ()> + Send) {
        let (tx, rx) = mpsc::channel(12);
        let task = async move {
            let settings_lock = Mutex::new(());
            tokio::join!(
                self.events(),
                self.discovery(&settings_lock),
                self.controls(rx, &settings_lock),
                self.metadata()
            );
        };
        (ControlHandle(tx), task)
    }

    async fn events(&self) {
        let id = &self.config.id;
        let mode = self.config.event_transport;
        self.hub.availability(id, None, "starting");
        while !self.stop.is_cancelled() {
            if let Some(onvif) = &self.onvif
                && mode != EventTransport::Baichuan
            {
                let endpoint = tokio::select! {
                    _ = self.stop.cancelled() => break,
                    endpoint = onvif.event_endpoint() => endpoint,
                };
                if let Some(base) = &self.advertise
                    && matches!(mode, EventTransport::Auto | EventTransport::OnvifPush)
                {
                    let result = onvif
                        .run_push(&endpoint, base, &self.routes, id, &self.hub, &self.stop)
                        .await;
                    self.disconnected("onvif", "ONVIF_PUSH_ENDED", result);
                }
                if !self.stop.is_cancelled()
                    && matches!(mode, EventTransport::Auto | EventTransport::OnvifPull)
                {
                    let result = onvif.run_pull(&endpoint, id, &self.hub, &self.stop).await;
                    self.disconnected("onvif", "ONVIF_PULL_ENDED", result);
                }
            }
            if !self.stop.is_cancelled()
                && matches!(mode, EventTransport::Auto | EventTransport::Baichuan)
                && let Some(device) = &self.baichuan
            {
                let result = reolink::events::run(device, id, &self.hub, &self.stop).await;
                self.disconnected("baichuan", "BAICHUAN_EVENTS_ENDED", result);
            }
            tokio::select! {
                _ = self.stop.cancelled() => break,
                _ = sleep(Duration::from_secs(15)) => {}
            }
        }
        self.hub.availability(id, None, "bridge_stopping");
    }

    fn disconnected(&self, source: &'static str, key: &str, result: Result<()>) {
        self.hub.clear_source(&self.config.id, source);
        self.hub.availability(
            &self.config.id,
            None,
            if self.stop.is_cancelled() {
                "bridge_stopping"
            } else {
                "event_transport_unavailable"
            },
        );
        if let Err(error) = result {
            self.hub.diagnostics.error(
                "camera::events",
                key,
                None,
                json!({"cameraId": self.config.id}),
                Some(error),
            );
        }
    }

    async fn discovery(&self, lock: &Mutex<()>) {
        let mut refresh = interval(Duration::from_secs(120));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = self.stop.cancelled() => return,
                _ = refresh.tick() => {},
            }
            let onvif = async {
                if let Some(onvif) = &self.onvif {
                    let discovery = onvif.discover().await;
                    if let Some(info) = discovery.info {
                        self.hub.section(&self.config.id, "info", "onvif", info);
                    }
                    if let Some(rules) = discovery.rules {
                        self.hub
                            .section(&self.config.id, "rules", "onvif", json!(rules));
                    }
                    self.hub.section(
                        &self.config.id,
                        "capabilities",
                        "onvif",
                        discovery.capabilities,
                    );
                }
            };
            let baichuan = async {
                if let Some(device) = &self.baichuan {
                    let _guard = lock.lock().await;
                    if let Err(error) =
                        reolink::discover(device, self.webhook.as_ref(), &self.config.id, &self.hub)
                            .await
                    {
                        self.hub.diagnostics.error(
                            "camera::discovery",
                            "BAICHUAN_DISCOVERY_FAILED",
                            None,
                            json!({"cameraId": self.config.id}),
                            Some(error),
                        );
                    }
                }
            };
            let cgi_webhook = async {
                if let Some(webhook) = &self.cgi_webhook {
                    let status = webhook.configure().await;
                    if status["verified"] != true {
                        self.hub.diagnostics.error(
                            "camera::discovery",
                            "CGI_WEBHOOK_SETUP_FAILED",
                            None,
                            json!({"cameraId": self.config.id}),
                            None,
                        );
                    }
                    self.hub
                        .section(&self.config.id, "capabilities", "native_webhook", status);
                }
            };
            tokio::select! {
                _ = self.stop.cancelled() => return,
                _ = async { tokio::join!(onvif, baichuan, cgi_webhook); } => {},
            }
        }
    }

    async fn controls(&self, mut rx: mpsc::Receiver<Command>, lock: &Mutex<()>) {
        let mut recent: VecDeque<(String, Control, Value)> = VecDeque::new();
        while let Some(request) = tokio::select! {
            _ = self.stop.cancelled() => None,
            command = rx.recv() => command,
        } {
            if let Some(id) = &request.replay_id
                && let Some((_, command, reply)) =
                    recent.iter().find(|(previous, _, _)| previous == id)
            {
                let reply = if command == &request.control {
                    reply.clone()
                } else {
                    self.hub.diagnostics.error(
                        "camera::controls",
                        "CONTROL_ID_CONFLICT",
                        Some(request.correlation_id.clone()),
                        json!({"cameraId":self.config.id}),
                        None,
                    )
                };
                let _ = request.reply.send(reply);
                continue;
            }
            let result = async {
                let device = self.baichuan.as_ref().ok_or_else(|| {
                    (
                        "CONTROL_TRANSPORT_DISABLED",
                        anyhow!("Baichuan is disabled for this camera"),
                    )
                })?;
                let _guard = lock.lock().await;
                if self.stop.is_cancelled() || Instant::now() > request.expires {
                    return Err((
                        "CONTROL_EXPIRED",
                        anyhow!("command expired before execution"),
                    ));
                }
                reolink::control::apply(device, &request.control)
                    .await
                    .map_err(|error| ("CONTROL_WRITE_OR_READBACK_FAILED", error))
            }
            .await;
            let reply = match result {
                Ok((setting, state, verified)) => {
                    self.hub
                        .section(&self.config.id, "state/settings", setting, state);
                    let snapshot = self
                        .hub
                        .snapshot(&self.config.id, "state/settings")
                        .unwrap();
                    let mut reply = json!({"ok": true, "verified": verified, "state": snapshot.payload[setting]});
                    if !verified {
                        let details = self.hub.diagnostics.error(
                            "camera::controls",
                            "CONTROL_READBACK_UNVERIFIED",
                            Some(request.correlation_id.clone()),
                            json!({"cameraId":self.config.id}),
                            None,
                        );
                        reply["warning"] = details["error"].clone();
                        reply["errorKey"] = details["errorKey"].clone();
                        reply["errorCode"] = details["errorCode"].clone();
                        reply["correlation_id"] = details["correlation_id"].clone();
                    }
                    reply
                }
                Err((key, error)) => self.hub.diagnostics.error(
                    "camera::controls",
                    key,
                    Some(request.correlation_id),
                    json!({"cameraId":self.config.id}),
                    Some(error),
                ),
            };
            if let Some(id) = request.replay_id {
                if recent.len() >= 128 {
                    recent.pop_front();
                }
                recent.push_back((id, request.control, reply.clone()));
            }
            let _ = request.reply.send(reply);
        }
    }

    async fn metadata(&self) {
        let id = &self.config.id;
        let config = &self.config.coordinates;
        if !config.allow_video_stream {
            self.hub.section(
                id,
                "capabilities",
                "coordinate_stream",
                json!({"enabled": false}),
            );
            return;
        }
        while !self.stop.is_cancelled() {
            if config.rtsp_url.is_none()
                && let Some(device) = &self.baichuan
            {
                let result = reolink::metadata::run(device, id, &self.hub, &self.stop).await;
                if let Err(error) = result {
                    self.hub.diagnostics.error(
                        "camera::metadata",
                        "BAICHUAN_METADATA_ENDED",
                        None,
                        json!({"cameraId": id}),
                        Some(error),
                    );
                }
            }
            if !self.stop.is_cancelled()
                && let Some(onvif) = &self.onvif
            {
                let result = async {
                    let stream = onvif.metadata_stream(config.rtsp_url.clone()).await?;
                    onvif
                        .stream_metadata(stream, id, &self.hub, &self.stop)
                        .await
                }
                .await;
                if let Err(error) = result {
                    self.hub.diagnostics.error(
                        "camera::metadata",
                        "ONVIF_METADATA_ENDED",
                        None,
                        json!({"cameraId": id}),
                        Some(error),
                    );
                }
            }
            self.hub.section(
                id,
                "capabilities",
                "coordinate_stream",
                json!({"enabled": true, "connected": false}),
            );
            tokio::select! {
                _ = self.stop.cancelled() => break,
                _ = sleep(Duration::from_secs(30)) => {}
            }
        }
    }
}
