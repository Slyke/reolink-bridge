use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use roxmltree::Node;
use serde_json::json;
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use super::{EVENTS, Onvif, REQUEST_TIMEOUT, WSA, WSN, metadata};
use crate::{
    hub::Hub,
    model::{Event, Observation, boolean, kind, timestamp},
    xml,
};

const LEASE: &str = "PT60S";
const PUSH_START_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Default)]
pub struct PushRoutes(Arc<Mutex<HashMap<String, PushRoute>>>);

struct PushRoute {
    camera_id: String,
    tx: mpsc::Sender<Vec<Observation>>,
}

struct PushLease {
    routes: PushRoutes,
    key: String,
}

impl Drop for PushLease {
    fn drop(&mut self) {
        self.routes.0.lock().unwrap().remove(&self.key);
    }
}

impl PushRoutes {
    fn register(&self, camera_id: &str) -> (PushLease, mpsc::Receiver<Vec<Observation>>) {
        let key = Uuid::new_v4().simple().to_string();
        let (tx, rx) = mpsc::channel(16);
        self.0.lock().unwrap().insert(
            key.clone(),
            PushRoute {
                camera_id: camera_id.into(),
                tx,
            },
        );
        (
            PushLease {
                routes: self.clone(),
                key,
            },
            rx,
        )
    }

    pub fn receiver(&self, id: &str, key: &str) -> Option<mpsc::Sender<Vec<Observation>>> {
        let routes = self.0.lock().unwrap();
        let route = routes.get(key)?;
        (route.camera_id == id).then(|| route.tx.clone())
    }
}

struct Subscription {
    address: Url,
    references: String,
    renew_at: Instant,
}

impl Onvif {
    pub async fn event_endpoint(&self) -> Url {
        self.services().await.events
    }

    async fn subscribe(&self, endpoint: &Url, consumer: Option<Url>) -> Result<Subscription> {
        let (action, body) = match consumer {
            Some(consumer) => (
                format!("{WSN}/NotificationProducer/SubscribeRequest"),
                format!(
                    "<wsnt:Subscribe><wsnt:ConsumerReference><wsa:Address>{}</wsa:Address>\
                    </wsnt:ConsumerReference><wsnt:InitialTerminationTime>{LEASE}</wsnt:InitialTerminationTime>\
                    </wsnt:Subscribe>",
                    xml::escape(consumer.as_str())
                ),
            ),
            None => (
                format!("{EVENTS}/EventPortType/CreatePullPointSubscriptionRequest"),
                format!(
                    "<tev:CreatePullPointSubscription><tev:InitialTerminationTime>{LEASE}</tev:InitialTerminationTime>\
                    </tev:CreatePullPointSubscription>"
                ),
            ),
        };
        let response = self.call(endpoint, &action, &body).await?;
        let doc = xml::document(&response)?;
        let root = doc.root_element();
        let reference = xml::find(root, "SubscriptionReference")
            .ok_or_else(|| anyhow!("ONVIF subscription has no reference"))?;
        let address = xml::value(reference, "Address")
            .and_then(|value| super::camera_url(endpoint, value, &["http", "https"]))
            .ok_or_else(|| anyhow!("ONVIF subscription has no usable address"))?;
        let references = xml::child(reference, "ReferenceParameters")
            .into_iter()
            .flat_map(|parameters| parameters.children())
            .filter(Node::is_element)
            .map(|node| reference_header(node, true))
            .collect();
        Ok(Subscription {
            address,
            references,
            renew_at: renew_at(root),
        })
    }

    async fn renew(&self, subscription: &mut Subscription) -> Result<()> {
        let response = self
            .request(
                &subscription.address,
                &format!("{WSN}/SubscriptionManager/RenewRequest"),
                &format!(
                    "<wsnt:Renew><wsnt:TerminationTime>{LEASE}</wsnt:TerminationTime></wsnt:Renew>"
                ),
                &subscription.references,
                REQUEST_TIMEOUT,
            )
            .await?;
        let doc = xml::document(&response)?;
        ensure!(
            xml::find(doc.root_element(), "RenewResponse").is_some(),
            "ONVIF renewal was not acknowledged"
        );
        subscription.renew_at = renew_at(doc.root_element());
        Ok(())
    }

    async fn unsubscribe(&self, subscription: &Subscription, camera_id: &str, hub: &Hub) {
        let result = self
            .request(
                &subscription.address,
                &format!("{WSN}/SubscriptionManager/UnsubscribeRequest"),
                "<wsnt:Unsubscribe/>",
                &subscription.references,
                Duration::from_secs(3),
            )
            .await;
        match result {
            Ok(_) => hub.diagnostics.log(
                "onvif::unsubscribe",
                "ONVIF_SUBSCRIPTION_CLOSED",
                "info",
                None,
                json!({"cameraId":camera_id}),
            ),
            Err(error) => {
                hub.diagnostics.error(
                    "onvif::unsubscribe",
                    "ONVIF_UNSUBSCRIBE_FAILED",
                    None,
                    json!({"cameraId":camera_id}),
                    Some(error),
                );
            }
        }
    }

    pub async fn run_push(
        &self,
        endpoint: &Url,
        base: &Url,
        routes: &PushRoutes,
        id: &str,
        hub: &Hub,
        stop: &CancellationToken,
    ) -> Result<()> {
        let (lease, mut notifications) = routes.register(id);
        let url = crate::config::advertised(base, &format!("/ingest/onvif/{id}/{}", lease.key));
        let mut subscription = self.subscribe(endpoint, Some(url)).await?;
        let result = async {
            let deadline = Instant::now() + PUSH_START_TIMEOUT;
            let mut online = false;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return Ok(()),
                    _ = sleep_until(deadline), if !online => return Err(anyhow!("ONVIF push received no initial notification")),
                    _ = sleep_until(subscription.renew_at) => self.renew(&mut subscription).await?,
                    Some(messages) = notifications.recv() => {
                        if !online && !messages.is_empty() {
                            online = true;
                            hub.availability(id, Some("onvif_push"), "subscribed");
                            hub.diagnostics.log("onvif::run_push", "ONVIF_PUSH_ACTIVE", "info", None, json!({"cameraId":id}));
                        }
                        for message in messages {
                            hub.ingest(message);
                        }
                    }
                }
            }
        }.await;
        drop(lease);
        self.unsubscribe(&subscription, id, hub).await;
        result
    }

    pub async fn run_pull(
        &self,
        endpoint: &Url,
        id: &str,
        hub: &Hub,
        stop: &CancellationToken,
    ) -> Result<()> {
        let mut subscription = self.subscribe(endpoint, None).await?;
        if !stop.is_cancelled() {
            let _ = self
                .request(
                    &subscription.address,
                    &format!("{EVENTS}/PullPointSubscription/SetSynchronizationPointRequest"),
                    "<tev:SetSynchronizationPoint/>",
                    &subscription.references,
                    Duration::from_secs(3),
                )
                .await;
        }
        let result = async {
            let mut online = false;
            while !stop.is_cancelled() {
                if Instant::now() >= subscription.renew_at {
                    self.renew(&mut subscription).await?;
                }
                let seconds = subscription
                    .renew_at
                    .saturating_duration_since(Instant::now())
                    .as_secs()
                    .clamp(1, 10);
                let query = format!(
                    "<tev:PullMessages><tev:Timeout>PT{seconds}S</tev:Timeout>\
                    <tev:MessageLimit>32</tev:MessageLimit></tev:PullMessages>"
                );
                let started = Instant::now();
                let action = format!("{EVENTS}/PullPointSubscription/PullMessagesRequest");
                let response = tokio::select! {
                    _ = stop.cancelled() => break,
                    reply = self.request(
                        &subscription.address,
                        &action,
                        &query, &subscription.references, Duration::from_secs(seconds + 4),
                    ) => reply?,
                };
                let doc = xml::document(&response)?;
                let root = xml::find(doc.root_element(), "PullMessagesResponse")
                    .ok_or_else(|| anyhow!("invalid ONVIF PullMessages response"))?;
                if !online {
                    online = true;
                    hub.availability(id, Some("onvif_pull"), "subscribed");
                    hub.diagnostics.log(
                        "onvif::run_pull",
                        "ONVIF_PULL_ACTIVE",
                        "info",
                        None,
                        json!({"cameraId":id}),
                    );
                }
                let messages = read_messages(root, id, Utc::now());
                let empty = messages.is_empty();
                for message in messages {
                    hub.ingest(message);
                }
                if empty {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = sleep_until(started + Duration::from_millis(250)) => {},
                    }
                }
            }
            Ok(())
        }
        .await;
        self.unsubscribe(&subscription, id, hub).await;
        result
    }
}

fn renew_at(root: Node<'_, '_>) -> Instant {
    let now = xml::value(root, "CurrentTime")
        .and_then(timestamp)
        .unwrap_or_else(Utc::now);
    let lease = xml::value(root, "TerminationTime")
        .and_then(|term| {
            timestamp(term)
                .and_then(|term| term.signed_duration_since(now).to_std().ok())
                .or_else(|| duration(term))
        })
        .unwrap_or(Duration::from_secs(60));
    Instant::now()
        + (lease.min(Duration::from_secs(3600)).mul_f64(0.65)).max(Duration::from_millis(200))
}

fn duration(value: &str) -> Option<Duration> {
    let mut rest = value.strip_prefix("PT")?;
    let mut seconds = 0.0;
    for (unit, multiplier) in [('H', 3600.0), ('M', 60.0), ('S', 1.0)] {
        if let Some((amount, remaining)) = rest.split_once(unit) {
            seconds += amount.parse::<f64>().ok()? * multiplier;
            rest = remaining;
        }
    }
    if !rest.is_empty() {
        return None;
    }
    Duration::try_from_secs_f64(seconds).ok()
}

fn reference_header(node: Node<'_, '_>, top: bool) -> String {
    use std::fmt::Write;
    let qualified = |namespace: Option<&str>, name: &str| {
        namespace
            .and_then(|ns| node.lookup_prefix(ns))
            .map_or_else(|| name.into(), |prefix| format!("{prefix}:{name}"))
    };
    let name = qualified(node.tag_name().namespace(), node.tag_name().name());
    let mut output = format!("<{name}");
    for ns in node.namespaces() {
        let key = ns
            .name()
            .map_or_else(|| "xmlns".into(), |prefix| format!("xmlns:{prefix}"));
        let _ = write!(output, " {key}=\"{}\"", xml::escape(ns.uri()));
    }
    for attr in node.attributes() {
        let attr_name = qualified(attr.namespace(), attr.name());
        let _ = write!(output, " {attr_name}=\"{}\"", xml::escape(attr.value()));
    }
    if top && node.attribute((WSA, "IsReferenceParameter")).is_none() {
        let _ = write!(
            output,
            r#" xmlns:bridgewsa="{WSA}" bridgewsa:IsReferenceParameter="true""#
        );
    }
    output.push('>');
    for child in node.children() {
        if child.is_element() {
            output.push_str(&reference_header(child, false));
        } else if let Some(text) = child.text().filter(|_| child.is_text()) {
            output.push_str(&xml::escape(text));
        }
    }
    let _ = write!(output, "</{name}>");
    output
}

pub fn parse_notify(
    body: &str,
    camera_id: &str,
    received: DateTime<Utc>,
) -> Result<Vec<Observation>> {
    let doc = xml::document(body)?;
    let notify =
        xml::find(doc.root_element(), "Notify").ok_or_else(|| anyhow!("expected SOAP Notify"))?;
    Ok(read_messages(notify, camera_id, received))
}

pub(super) fn read_messages(
    root: Node<'_, '_>,
    id: &str,
    received: DateTime<Utc>,
) -> Vec<Observation> {
    root.descendants().filter(|n| n.has_tag_name("NotificationMessage")).filter_map(|notice| {
        let topic = xml::child(notice, "Topic");
        let topic_text = topic.and_then(xml::text).unwrap_or("onvif:unspecified");
        let wrapper = xml::child(notice, "Message")?;
        let message = wrapper.children().find(Node::is_element).unwrap_or(wrapper);
        let operation = message.attribute("PropertyOperation");
        let item = |section, names: &[&str]| {
            xml::child(message, section).into_iter().flat_map(|group| group.children())
                .filter(|node| node.has_tag_name("SimpleItem")).find_map(|node| {
                    let name = node.attribute("Name")?;
                    names.iter().any(|candidate| name.eq_ignore_ascii_case(candidate)).then(|| node.attribute("Value")).flatten()
                })
        };
        let event_type = kind(topic_text);
        let mut event = Event::new(id, event_type.clone(), "onvif", topic_text.into(), received);
        event.operation = operation.map(str::to_owned);
        event.camera_time = message.attribute("UtcTime").and_then(timestamp);
        event.active = item("Data", &["State", "IsMotion", "IsInside", "IsDetected", "Active", "LogicalState"]).and_then(boolean);
        event.rule_id = item("Source", &["Rule", "RuleId", "RuleName", "RuleToken"])
            .or_else(|| item("Key", &["Rule", "RuleId", "RuleName", "RuleToken"]))
            .or_else(|| item("Data", &["RuleId", "RuleToken"]))
            .filter(|value| !value.is_empty()).map(str::to_owned);
        event.objects = metadata::objects(message, "ONVIF event video source", None);
        event.vendor = json!({
            "message": xml::tree(message),
            "topic_dialect": topic.and_then(|node| node.attribute("Dialect")),
            "topic_namespace": topic.and_then(|node| topic_text.split_once(':').and_then(|(prefix, _)| node.lookup_namespace_uri(Some(prefix)))),
        });
        let property = (event.active.is_some() || operation.is_some()).then(|| {
            let mut identifiers: Vec<_> = ["Source", "Key"].into_iter()
                .flat_map(|name| xml::child(message, name)).flat_map(|node| node.children())
                .filter_map(|node| Some((node.attribute("Name")?, node.attribute("Value")?))).collect();
            identifiers.sort_unstable();
            format!("{:x}", md5::compute(format!("{topic_text}|{identifiers:?}")))
        });
        Some(Observation {
            event, property,
            initial: operation.is_some_and(|op| op.eq_ignore_ascii_case("Initialized")),
            deleted: operation.is_some_and(|op| op.eq_ignore_ascii_case("Deleted")),
        })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_initial_change_clear_and_undeclared_topic() {
        let received = Utc::now();
        let initial = parse_notify(
            include_str!("../../tests/fixtures/notify-initial.xml"),
            "front-door",
            received,
        )
        .unwrap();
        assert_eq!(initial.len(), 4);
        assert!(
            initial
                .iter()
                .all(|event| event.initial && event.property.is_some())
        );
        let motion = initial
            .iter()
            .find(|item| item.event.kind == "motion")
            .unwrap();
        assert_eq!(motion.event.rule_id.as_deref(), Some("000"));
        for (fixture, active) in [
            (include_str!("../../tests/fixtures/notify-active.xml"), true),
            (include_str!("../../tests/fixtures/notify-clear.xml"), false),
        ] {
            let events = parse_notify(fixture, "front-door", received).unwrap();
            assert_eq!(events.len(), 1);
            let person = &events[0];
            assert!(!person.initial);
            assert_eq!(person.event.kind, "person");
            assert_eq!(person.event.active, Some(active));
            assert_eq!(person.event.received_at, received);
            assert!(person.event.camera_time.is_some());
            assert!(person.event.rule_id.is_none() && person.event.objects.is_empty());
            assert_eq!(person.property, initial[0].property);
        }
        let events = parse_notify(
            include_str!("../../tests/fixtures/notify-vendor.xml"),
            "front-door",
            received,
        )
        .unwrap();
        let event = &events[0].event;
        assert!(event.kind.starts_with("vendor_parcelmoved_"));
        assert!(event.camera_time.is_none());
        assert!(event.vendor.to_string().contains("Geometry"));
        assert!(
            !event
                .vendor
                .to_string()
                .contains("test-secret-do-not-publish")
        );
        let object = &event.objects[0];
        assert_eq!(object.tracking_id.as_deref(), Some("42"));
        assert_eq!(object.confidence, Some(0.88));
        let bounds = object.bounding_box.as_ref().unwrap();
        assert_eq!(
            (bounds.left, bounds.top, bounds.right, bounds.bottom),
            (0.25, 0.25, 0.625, 0.625)
        );
    }
}
