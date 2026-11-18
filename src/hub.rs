use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use anyhow::anyhow;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{self, Callback, Filter},
    diagnostics::Diagnostics,
    model::Observation,
    xml,
};

#[derive(Clone)]
pub enum Update {
    Snapshot(Arc<Snapshot>),
    Event(Arc<LiveEvent>),
}

#[derive(Serialize)]
pub struct Snapshot {
    pub camera_id: String,
    pub topic: String,
    pub payload: Value,
}

impl Snapshot {
    pub fn matches(&self, filter: &Filter) -> bool {
        if !filter.camera(&self.camera_id) {
            return false;
        }
        match self.topic.strip_prefix("state/") {
            Some(kind) if !matches!(kind, "settings" | "ptz") => {
                filter.event(
                    kind,
                    self.payload["last"]["vendor_topic"].as_str().unwrap_or(""),
                ) || self.payload["properties"].as_array().is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| filter.event(kind, item["vendor_topic"].as_str().unwrap_or("")))
                })
            }
            _ => true,
        }
    }
}

pub struct LiveEvent {
    pub camera_id: String,
    pub kind: String,
    pub vendor_topic: String,
    pub payload: Value,
}

impl LiveEvent {
    pub fn matches(&self, filter: &Filter) -> bool {
        filter.camera(&self.camera_id) && filter.event(&self.kind, &self.vendor_topic)
    }
}

#[derive(Default)]
struct CameraState {
    stable_id: String,
    snapshots: BTreeMap<String, Arc<Snapshot>>,
    online: bool,
    mqtt_ready: bool,
    observed: BTreeMap<String, String>,
    properties: BTreeMap<String, BTreeMap<(&'static str, String), Value>>,
}

pub struct Hub {
    cameras: Mutex<BTreeMap<String, CameraState>>,
    updates: broadcast::Sender<Update>,
    pub callbacks: Arc<Callbacks>,
    pub diagnostics: Diagnostics,
    secrets: Vec<String>,
}

impl Hub {
    pub fn new(
        identities: BTreeMap<String, String>,
        callbacks: Arc<Callbacks>,
        secrets: Vec<String>,
        diagnostics: Diagnostics,
    ) -> Arc<Self> {
        let (updates, _) = broadcast::channel(256);
        Arc::new(Self {
            cameras: Mutex::new(
                identities
                    .into_iter()
                    .map(|(id, stable_id)| {
                        (
                            id,
                            CameraState {
                                stable_id,
                                ..CameraState::default()
                            },
                        )
                    })
                    .collect(),
            ),
            updates,
            callbacks,
            diagnostics,
            secrets,
        })
    }

    pub fn redacted(&self, mut value: Value) -> Value {
        xml::redact(&mut value, &self.secrets);
        value
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Update> {
        self.updates.subscribe()
    }

    pub fn snapshots(&self, filter: &Filter) -> Vec<Arc<Snapshot>> {
        self.cameras
            .lock()
            .unwrap()
            .values()
            .flat_map(|cam| cam.snapshots.values())
            .filter(|snapshot| snapshot.matches(filter))
            .cloned()
            .collect()
    }

    pub fn snapshot(&self, id: &str, topic: &str) -> Option<Arc<Snapshot>> {
        self.cameras
            .lock()
            .unwrap()
            .get(id)?
            .snapshots
            .get(topic)
            .cloned()
    }

    fn store(&self, camera: &mut CameraState, id: &str, topic: &str, mut payload: Value) {
        xml::redact(&mut payload, &self.secrets);
        payload["stable_id"] = json!(camera.stable_id);
        let snapshot = Arc::new(Snapshot {
            camera_id: id.into(),
            topic: topic.into(),
            payload,
        });
        camera.snapshots.insert(topic.into(), snapshot.clone());
        let _ = self.updates.send(Update::Snapshot(snapshot));
    }

    pub fn readiness(&self) -> (bool, bool) {
        let cameras = self.cameras.lock().unwrap();
        (
            cameras.values().all(|camera| camera.online),
            cameras.values().all(|camera| camera.mqtt_ready),
        )
    }

    pub fn mqtt_ready(&self, id: &str, ready: bool) {
        self.cameras.lock().unwrap().get_mut(id).unwrap().mqtt_ready = ready;
    }

    pub fn availability(&self, id: &str, transport: Option<&str>, reason: &str) {
        let mut all = self.cameras.lock().unwrap();
        let camera = all.get_mut(id).unwrap();
        camera.online = transport.is_some();
        self.store(
            camera,
            id,
            "availability",
            json!({
                "camera_id": id, "online": camera.online, "transport": transport,
                "reason": reason, "updated_at": chrono::Utc::now(),
            }),
        );
    }

    pub fn section(&self, id: &str, topic: &str, section: &str, payload: Value) {
        let mut all = self.cameras.lock().unwrap();
        let camera = all.get_mut(id).unwrap();
        let mut snapshot = camera.snapshots.get(topic).map_or_else(
            || json!({"camera_id": id}),
            |snapshot| snapshot.payload.clone(),
        );
        snapshot[section] = payload;
        self.store(camera, id, topic, snapshot);
    }

    pub fn ingest(&self, message: Observation) {
        let Observation {
            mut event,
            property,
            initial,
            deleted,
        } = message;
        let id = &event.camera_id;
        let mut payload = serde_json::to_value(&event).expect("serialize event");
        xml::redact(&mut payload, &self.secrets);
        let vendor_topic = payload["vendor_topic"].as_str().unwrap_or("").to_owned();
        if event.kind.starts_with("vendor_")
            && (vendor_topic == "[redacted]"
                || payload["type"].as_str() != Some(event.kind.as_str()))
        {
            event.kind = "vendor_redacted".into();
            payload["type"] = json!(event.kind);
        }
        {
            let mut all = self.cameras.lock().unwrap();
            let camera = all.get_mut(id).unwrap();
            payload["stable_id"] = json!(camera.stable_id);
            if camera.observed.len() < 256 && !camera.observed.contains_key(&vendor_topic) {
                camera
                    .observed
                    .insert(vendor_topic.clone(), event.kind.clone());
                let mut capabilities = camera.snapshots.get("capabilities").map_or_else(
                    || json!({"camera_id": id}),
                    |snapshot| snapshot.payload.clone(),
                );
                capabilities["observed_events"] = json!(camera.observed);
                self.store(camera, id, "capabilities", capabilities);
            }
            if let Some(property) = property
                && (camera.properties.len() < 256 || camera.properties.contains_key(&event.kind))
            {
                let properties = camera.properties.entry(event.kind.clone()).or_default();
                let key = (event.source, property);
                if deleted {
                    properties.remove(&key);
                } else if properties.len() < 64 || properties.contains_key(&key) {
                    properties.insert(
                        key,
                        json!({
                            "source": event.source, "vendor_topic": vendor_topic,
                            "rule_id": payload["rule_id"], "channel": event.channel,
                            "active": event.active, "received_at": event.received_at,
                            "camera_time": event.camera_time,
                        }),
                    );
                }
                let snapshot = detection_state(id, &event.kind, properties, &payload);
                self.store(camera, id, &format!("state/{}", event.kind), snapshot);
            }
        }
        if !initial {
            let event = Arc::new(LiveEvent {
                camera_id: event.camera_id,
                kind: event.kind,
                vendor_topic,
                payload,
            });
            let _ = self.updates.send(Update::Event(event.clone()));
            self.callbacks.deliver(&event);
        }
    }

    pub fn clear_source(&self, id: &str, source: &'static str) {
        let mut all = self.cameras.lock().unwrap();
        let camera = all.get_mut(id).unwrap();
        let changed: Vec<_> = camera
            .properties
            .iter_mut()
            .filter_map(|(kind, values)| {
                let original = values.len();
                values.retain(|(owner, _), _| *owner != source);
                (values.len() != original).then(|| {
                    (
                        format!("state/{kind}"),
                        detection_state(id, kind, values, &Value::Null),
                    )
                })
            })
            .collect();
        for (topic, snapshot) in changed {
            self.store(camera, id, &topic, snapshot);
        }
    }
}

fn detection_state(
    id: &str,
    kind: &str,
    properties: &BTreeMap<(&'static str, String), Value>,
    last: &Value,
) -> Value {
    let active = properties
        .values()
        .filter_map(|entry| entry["active"].as_bool())
        .reduce(|a, b| a || b);
    json!({
        "camera_id": id, "type": kind, "active": active,
        "properties": properties.values().collect::<Vec<_>>(), "last": last,
    })
}

pub enum CallbackEdit {
    Add(Callback),
    Update(Callback),
    Remove(String),
}

pub struct CallbackFile {
    raw: String,
    document: Value,
    callbacks: Vec<Callback>,
}

impl CallbackFile {
    pub fn new(raw: String, document: Value, callbacks: Vec<Callback>) -> Self {
        Self {
            raw,
            document,
            callbacks,
        }
    }
}

pub struct Callbacks {
    path: PathBuf,
    ids: BTreeSet<String>,
    file: Mutex<CallbackFile>,
    workers: RwLock<BTreeMap<String, CallbackWorker>>,
    client: reqwest::Client,
    stop: CancellationToken,
    diagnostics: Diagnostics,
}

struct CallbackWorker {
    callback: Callback,
    sender: mpsc::Sender<Arc<Vec<u8>>>,
    task: JoinHandle<()>,
}

impl Drop for CallbackWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Callbacks {
    pub fn new(
        path: PathBuf,
        file: CallbackFile,
        ids: BTreeSet<String>,
        client: reqwest::Client,
        stop: CancellationToken,
        diagnostics: Diagnostics,
    ) -> Arc<Self> {
        let manager = Arc::new(Self {
            path,
            ids,
            file: Mutex::new(file),
            workers: RwLock::default(),
            client,
            stop,
            diagnostics,
        });
        manager.update_workers(&manager.file.lock().unwrap().callbacks);
        manager
    }

    pub fn list(&self) -> Vec<Value> {
        callback_list(&self.file.lock().unwrap())
    }

    pub fn edit(&self, edit: CallbackEdit, correlation_id: String) -> Value {
        match self.update_file(edit, &correlation_id) {
            Ok(callbacks) => json!({"ok": true, "callbacks": callbacks}),
            Err((key, error)) => self.diagnostics.error(
                "callbacks::edit",
                key,
                Some(correlation_id),
                Value::Null,
                Some(error),
            ),
        }
    }

    fn update_file(
        &self,
        edit: CallbackEdit,
        correlation_id: &str,
    ) -> std::result::Result<Vec<Value>, (&'static str, anyhow::Error)> {
        let mut file = self.file.lock().unwrap();
        let mut callbacks = file.callbacks.clone();
        let mut document = file.document.clone();
        let mut entries = document["callbacks"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        match edit {
            CallbackEdit::Add(callback) => {
                entries.push(callback_value(&callback)?);
                callbacks.push(callback);
            }
            CallbackEdit::Update(callback) => {
                let index = callbacks
                    .iter()
                    .position(|item| item.id == callback.id)
                    .ok_or_else(|| ("CALLBACK_UPDATE_MISSING", anyhow!("unknown callback ID")))?;
                entries[index] = callback_value(&callback)?;
                callbacks[index] = callback;
            }
            CallbackEdit::Remove(id) => {
                let index = callbacks
                    .iter()
                    .position(|item| item.id == id)
                    .ok_or_else(|| ("CALLBACK_REMOVE_MISSING", anyhow!("unknown callback ID")))?;
                entries.remove(index);
                callbacks.remove(index);
            }
        }
        config::validate_callbacks(&callbacks, &self.ids)
            .map_err(|error| ("CALLBACK_CONFIG_INVALID", error))?;
        document["callbacks"] = entries.into();
        if std::fs::read_to_string(&self.path).ok().as_ref() != Some(&file.raw) {
            return Err((
                "CALLBACK_CONFIG_CHANGED",
                anyhow!("restart the bridge after editing its config"),
            ));
        }
        let (output, sync_error) = config::write_callbacks(&self.path, &file.raw, document.clone())
            .map_err(|error| ("CALLBACK_CONFIG_SAVE_FAILED", error))?;
        file.raw = output;
        file.document = document;
        self.update_workers(&callbacks);
        file.callbacks = callbacks;
        if let Some(error) = sync_error {
            self.diagnostics.error(
                "callbacks::update_file",
                "CONFIG_DIRECTORY_SYNC_FAILED",
                Some(correlation_id.to_owned()),
                Value::Null,
                Some(error.into()),
            );
        }
        Ok(callback_list(&file))
    }

    fn update_workers(&self, callbacks: &[Callback]) {
        let mut workers = self.workers.write().unwrap();
        workers.retain(|_, worker| callbacks.contains(&worker.callback));
        for callback in callbacks {
            workers.entry(callback.id.clone()).or_insert_with(|| {
                let (sender, mut rx) = mpsc::channel::<Arc<Vec<u8>>>(64);
                let (client, stop) = (self.client.clone(), self.stop.clone());
                let (id, url) = (callback.id.clone(), callback.url.clone());
                let diagnostics = self.diagnostics.clone();
                let task = tokio::spawn(async move {
                    while let Some(payload) = tokio::select! {
                        _ = stop.cancelled() => None,
                        payload = rx.recv() => payload,
                    } {
                        let response = client
                            .post(url.clone())
                            .timeout(Duration::from_secs(5))
                            .header("content-type", "application/json")
                            .body(payload.as_ref().clone())
                            .send()
                            .await;
                        match response {
                            Ok(response) if response.status().is_success() => {}
                            Ok(response) => {
                                diagnostics.error(
                                    "callbacks::deliver",
                                    "CALLBACK_HTTP_REJECTED",
                                    None,
                                    json!({"callbackId": id, "status": response.status().as_u16()}),
                                    None,
                                );
                            }
                            Err(error) => {
                                diagnostics.error(
                                    "callbacks::deliver",
                                    "CALLBACK_DELIVERY_FAILED",
                                    None,
                                    json!({"callbackId": id}),
                                    Some(opaque_http_cause(&error)),
                                );
                            }
                        }
                    }
                });
                CallbackWorker {
                    callback: callback.clone(),
                    sender,
                    task,
                }
            });
        }
    }

    fn deliver(&self, event: &LiveEvent) {
        let workers = self.workers.read().unwrap();
        let mut payload = None;
        for worker in workers
            .values()
            .filter(|worker| event.matches(&worker.callback.filter))
        {
            let data = payload.get_or_insert_with(|| {
                Arc::new(serde_json::to_vec(&event.payload).expect("serialize JSON"))
            });
            if worker.sender.try_send(data.clone()).is_err() {
                self.diagnostics.error(
                    "callbacks::enqueue",
                    "CALLBACK_QUEUE_FULL",
                    None,
                    json!({"callbackId": worker.callback.id}),
                    None,
                );
            }
        }
    }
}

fn callback_list(file: &CallbackFile) -> Vec<Value> {
    file.callbacks
        .iter()
        .enumerate()
        .map(|(index, callback)| {
            let mut result = json!(callback);
            let authored = file.document["callbacks"][index]["url"].as_str();
            if let Some(reference) = authored.filter(|value| config::environment_reference(value)) {
                result["url"] = reference.into();
            } else if callback.url.query().is_some() {
                // Query values may carry callback credentials.
                let mut url = callback.url.clone();
                url.set_query(None);
                result["url"] = url.as_str().into();
                result["query_redacted"] = true.into();
            }
            result
        })
        .collect()
}

fn callback_value(
    callback: &Callback,
) -> std::result::Result<Value, (&'static str, anyhow::Error)> {
    if callback
        .filter
        .events
        .iter()
        .any(|event| config::environment_reference(event))
    {
        return Err((
            "CALLBACK_CONFIG_REFERENCE_REJECTED",
            anyhow!("callback API accepts literal event filters"),
        ));
    }
    serde_json::to_value(callback)
        .map_err(|error| ("CALLBACK_CONFIG_SERIALIZE_FAILED", error.into()))
}

fn opaque_http_cause(error: &reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow!("HTTP request timed out")
    } else if error.is_connect() {
        anyhow!("HTTP connection failed")
    } else {
        anyhow!("HTTP transport failed")
    }
}
