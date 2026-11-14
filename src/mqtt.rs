use std::{collections::BTreeSet, sync::Arc, time::Duration};

use futures_util::{StreamExt, stream::FuturesUnordered};
use rumqttc::{
    Transport,
    v5::{
        AsyncClient, ConnectionError, Event, MqttOptions,
        mqttbytes::{
            QoS,
            v5::{
                Filter as SubscriptionFilter, LastWill, Packet, PubAckReason, Publish,
                RetainForwardRule, SubscribeReasonCode,
            },
        },
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    time::{interval, sleep},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    camera::ControlHandle,
    config::{Filter, Mqtt, valid_id},
    diagnostics::Diagnostics,
    hub::{Hub, Update},
    reolink::control::Control,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    id: String,
    command: Control,
}

pub fn options(config: &Mqtt, camera: &str, base: &str) -> MqttOptions {
    let tls = config.url.scheme() == "mqtts";
    let host = config.url.host_str().unwrap().trim_matches(['[', ']']);
    let port = config.url.port().unwrap_or(if tls { 8883 } else { 1883 });
    let mut options = MqttOptions::new(format!("{}-{camera}", config.client_id), host, port);
    options
        .set_keep_alive(Duration::from_secs(15))
        .set_clean_start(true)
        .set_max_packet_size(Some(1024 * 1024));
    if tls {
        options.set_transport(Transport::tls_with_default_config());
    }
    if let (Some(user), Some(password)) = (&config.username, &config.password) {
        options.set_credentials(user, password);
    }
    let will = json!({"camera_id":camera,"online":false,"reason":"bridge_disconnected"});
    options.set_last_will(LastWill::new(
        format!("{base}/availability"),
        will.to_string(),
        QoS::AtLeastOnce,
        true,
        None,
    ));
    options
}

pub async fn run(
    options: MqttOptions,
    base: String,
    id: String,
    hub: Arc<Hub>,
    controls: Option<ControlHandle>,
    stop: CancellationToken,
) {
    let command_topic = format!("{base}/command/exec");
    let diagnostics = &hub.diagnostics;
    while !stop.is_cancelled() {
        let (client, mut connection) = AsyncClient::new(options.clone(), 32);
        let (connected, ready) = oneshot::channel();
        let (commands, receiver) = mpsc::channel(32);
        let pump = async {
            let mut connected = Some(connected);
            let mut ready_on_ack = false;
            loop {
                match connection.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        ready_on_ack = controls.is_none();
                        if let Some(connected) = connected.take() {
                            let _ = connected.send(());
                        }
                    }
                    Ok(Event::Incoming(Packet::Publish(packet)))
                        if packet.topic.as_ref() == command_topic.as_bytes() =>
                    {
                        if !packet.retain && commands.try_send(packet).is_err() {
                            diagnostics.error(
                                "mqtt::run",
                                "MQTT_COMMAND_QUEUE_FULL",
                                None,
                                json!({"cameraId": id}),
                                None,
                            );
                        }
                    }
                    Ok(Event::Incoming(Packet::SubAck(ack))) => {
                        ready_on_ack = ack
                            .return_codes
                            .iter()
                            .all(|code| matches!(code, SubscribeReasonCode::Success(_)));
                        if !ready_on_ack {
                            hub.mqtt_ready(&id, false);
                            diagnostics.error(
                                "mqtt::run",
                                "MQTT_COMMAND_SUBSCRIBE_REJECTED",
                                None,
                                json!({"cameraId":id}),
                                None,
                            );
                        }
                    }
                    Ok(Event::Incoming(Packet::PubAck(ack))) => {
                        if matches!(
                            ack.reason,
                            PubAckReason::Success | PubAckReason::NoMatchingSubscribers
                        ) {
                            if ready_on_ack {
                                hub.mqtt_ready(&id, true);
                            }
                        } else {
                            hub.mqtt_ready(&id, false);
                            diagnostics.error(
                                "mqtt::run",
                                "MQTT_PUBLISH_REJECTED",
                                None,
                                json!({"cameraId": id, "reason": format!("{:?}", ack.reason)}),
                                None,
                            );
                            break None;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => break Some(connection_cause(error)),
                }
            }
        };
        let publish = async {
            if ready.await.is_ok() {
                diagnostics.log(
                    "mqtt::run",
                    "MQTT_CONNECTED",
                    "info",
                    None,
                    json!({"cameraId":id}),
                );
                if controls.is_some() {
                    let subscription = SubscriptionFilter {
                        path: command_topic.clone(),
                        qos: QoS::AtLeastOnce,
                        nolocal: true,
                        preserve_retain: true,
                        retain_forward_rule: RetainForwardRule::Never,
                    };
                    if client.subscribe_many([subscription]).await.is_err() {
                        diagnostics.error(
                            "mqtt::run",
                            "MQTT_COMMAND_SUBSCRIBE_FAILED",
                            None,
                            json!({"cameraId":id}),
                            None,
                        );
                        return;
                    }
                }
                publish_session(&client, &base, &id, &hub, controls.as_ref(), receiver).await;
            }
        };
        let error = tokio::select! {
            _ = stop.cancelled() => {
                hub.mqtt_ready(&id, false);
                return;
            }
            error = pump => error,
            _ = publish => None,
        };
        hub.mqtt_ready(&id, false);
        // Recreate the clean session so pending live events cannot be replayed after reconnect.
        diagnostics.error(
            "mqtt::run",
            "MQTT_DISCONNECTED",
            None,
            json!({"cameraId": id}),
            error,
        );
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = sleep(Duration::from_secs(3)) => {}
        }
    }
}

async fn publish_session(
    client: &AsyncClient,
    base: &str,
    id: &str,
    hub: &Hub,
    controls: Option<&ControlHandle>,
    mut commands: mpsc::Receiver<Publish>,
) {
    let diagnostics = &hub.diagnostics;
    let mut updates = hub.subscribe();
    let filter = Filter {
        cameras: vec![id.into()],
        events: Vec::new(),
    };
    let mut dirty = snapshot_topics(hub, &filter);
    let mut flush = interval(Duration::from_millis(50));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut replies = FuturesUnordered::new();
    let event_topic = format!("{base}/events");
    let result_topic = format!("{base}/command/result");
    loop {
        tokio::select! {
            _ = flush.tick(), if !dirty.is_empty() => {
                while let Some(topic) = dirty.pop_first() {
                    if let Some(snapshot) = hub.snapshot(id, &topic)
                        && client.try_publish(format!("{base}/{topic}"), QoS::AtLeastOnce, true, snapshot.payload.to_string()).is_err()
                    {
                        dirty.insert(topic);
                        break;
                    }
                }
            }
            update = updates.recv() => {
                match update {
                    Ok(Update::Snapshot(snapshot)) if snapshot.camera_id == id => { dirty.insert(snapshot.topic.clone()); }
                    Ok(Update::Event(event)) if event.camera_id == id => {
                        if client.try_publish(&event_topic, QoS::AtMostOnce, false, event.payload.to_string()).is_err() {
                            diagnostics.error("mqtt::publish_session", "MQTT_EVENT_QUEUE_FULL", None,
                                json!({"cameraId":id}), None);
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        dirty = snapshot_topics(hub, &filter);
                        diagnostics.error("mqtt::publish_session", "MQTT_SNAPSHOT_LAG", None,
                            json!({"cameraId":id}), None);
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            Some(packet) = commands.recv(), if controls.is_some() && replies.len() < 16 => {
                if let Some(controller) = controls {
                    replies.push(run_command(diagnostics, controller, packet));
                }
            }
            Some(reply) = replies.next(), if !replies.is_empty() => {
                if client.try_publish(&result_topic, QoS::AtLeastOnce, false, reply.to_string()).is_err() {
                    let correlation = reply["correlation_id"].as_str().map(str::to_owned);
                    diagnostics.error("mqtt::publish_session", "MQTT_RESULT_QUEUE_FULL", correlation,
                        json!({"cameraId":id}), None);
                }
            }
        }
    }
}

fn snapshot_topics(hub: &Hub, filter: &Filter) -> BTreeSet<String> {
    hub.snapshots(filter)
        .iter()
        .map(|snapshot| snapshot.topic.clone())
        .collect()
}

async fn run_command(
    diagnostics: &Diagnostics,
    controller: &ControlHandle,
    packet: Publish,
) -> Value {
    let correlation = Uuid::new_v4().to_string();
    let request = if packet.payload.len() > 32 * 1024 {
        Err("MQTT_COMMAND_TOO_LARGE")
    } else {
        serde_json::from_slice::<Command>(&packet.payload)
            .map_err(|_| "MQTT_COMMAND_INVALID")
            .and_then(|request| {
                valid_id(&request.id)
                    .then_some(request)
                    .ok_or("MQTT_COMMAND_ID_INVALID")
            })
    };
    let mut id = None;
    let mut reply = match request {
        Ok(request) => {
            id = Some(request.id);
            match controller.enqueue(request.command, id.clone(), correlation.clone()) {
                Ok(response) => response.await.unwrap_or_else(|_| {
                    diagnostics.error(
                        "mqtt::run_command",
                        "MQTT_COMMAND_WORKER_STOPPED",
                        Some(correlation.clone()),
                        Value::Null,
                        None,
                    )
                }),
                Err((key, error)) => diagnostics.error(
                    "mqtt::run_command",
                    key,
                    Some(correlation.clone()),
                    Value::Null,
                    Some(error),
                ),
            }
        }
        Err(key) => diagnostics.error(
            "mqtt::run_command",
            key,
            Some(correlation.clone()),
            Value::Null,
            None,
        ),
    };
    reply["id"] = json!(id);
    if reply["correlation_id"].is_null() {
        reply["correlation_id"] = correlation.into();
    }
    reply
}

fn connection_cause(error: ConnectionError) -> anyhow::Error {
    match error {
        ConnectionError::Io(error) => error.into(),
        ConnectionError::Timeout(error) => error.into(),
        ConnectionError::ConnectionRefused(code) => {
            anyhow::anyhow!("broker refused connection ({code:?})")
        }
        // Other dependency errors can contain server packets with arbitrary data.
        _ => anyhow::anyhow!("MQTT protocol or transport failure"),
    }
}
