use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use roxmltree::Node;
use serde_json::{Value, json};
use tokio::time::{Instant, interval_at};
use tokio_util::sync::CancellationToken;

use super::wire::{Device, Packet, Session};
use crate::{
    hub::Hub,
    model::{Event, Observation, boolean, kind, reolink_timestamp},
    xml,
};

pub struct AlarmDecoder {
    channel: u8,
    source: &'static str,
    states: BTreeMap<(String, Option<String>), bool>,
    ai_types: BTreeSet<String>,
    status_types: BTreeSet<String>,
    rules: BTreeSet<(String, String)>,
    first: bool,
}

impl AlarmDecoder {
    pub fn new(channel: u8, source: &'static str, ai_types: BTreeSet<String>) -> Self {
        Self {
            channel,
            source,
            states: BTreeMap::new(),
            ai_types,
            status_types: BTreeSet::new(),
            rules: BTreeSet::new(),
            first: true,
        }
    }

    pub fn decode(
        &mut self,
        body: &str,
        id: &str,
        received: DateTime<Utc>,
    ) -> Result<Vec<Observation>> {
        let doc = xml::document(body)?;
        let mut observations = Vec::new();
        for alarm in doc
            .descendants()
            .filter(|node| node.has_tag_name("AlarmEvent"))
        {
            if alarm_channel(alarm) != Some(self.channel) {
                continue;
            }
            let previous = observations.len();
            let status = xml::child(alarm, "status").map(|n| tokens(xml::text(n).unwrap_or("")));
            let ai = xml::child(alarm, "AItype").map(|n| tokens(xml::text(n).unwrap_or("")));
            if status.is_some() || ai.is_some() {
                let motion = status
                    .as_ref()
                    .is_some_and(|set| set.contains("MD") || set.contains("motion"))
                    || ai.as_ref().is_some_and(|set| set.contains("other"));
                self.state(
                    alarm,
                    "motion",
                    motion,
                    None,
                    id,
                    received,
                    &mut observations,
                );
            }
            if let Some(status) = status {
                self.status_types.extend(
                    status
                        .iter()
                        .filter(|v| !matches!(v.as_str(), "MD" | "motion"))
                        .cloned(),
                );
                for token in self.status_types.clone() {
                    self.state(
                        alarm,
                        &token,
                        status.contains(&token),
                        None,
                        id,
                        received,
                        &mut observations,
                    );
                }
            }
            if let Some(ai) = ai {
                self.ai_types
                    .extend(ai.iter().filter(|v| v.as_str() != "other").cloned());
                for token in self.ai_types.clone() {
                    self.state(
                        alarm,
                        &token,
                        ai.contains(&token),
                        None,
                        id,
                        received,
                        &mut observations,
                    );
                }
            }
            if let Some(active) = xml::value(alarm, "recording").and_then(boolean) {
                self.state(
                    alarm,
                    "recording",
                    active,
                    None,
                    id,
                    received,
                    &mut observations,
                );
            }
            if let Some(list) = xml::child(alarm, "smartAiTypeList") {
                let mut active_rules = BTreeSet::new();
                for item in list
                    .descendants()
                    .filter(|node| node.has_tag_name("smartAiType"))
                {
                    let Some(category) = xml::child(item, "type").and_then(xml::text) else {
                        continue;
                    };
                    for location in item
                        .children()
                        .filter(|node| node.has_tag_name("subList"))
                        .filter_map(|entry| {
                            xml::value(entry, "index").and_then(|n| n.parse::<u32>().ok())
                        })
                    {
                        active_rules
                            .insert((category.to_owned(), format!("{category}:{location}")));
                    }
                    if let Some(bitmap) = xml::child(item, "index")
                        .and_then(xml::text)
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        for bit in 0..64 {
                            if bitmap & (1 << bit) != 0 {
                                active_rules
                                    .insert((category.to_owned(), format!("{category}:{bit}")));
                            }
                        }
                    }
                }
                self.rules.extend(active_rules.iter().cloned());
                for (category, rule_id) in self.rules.clone() {
                    let active = active_rules.contains(&(category.clone(), rule_id.clone()));
                    self.state(
                        alarm,
                        &category,
                        active,
                        Some(rule_id),
                        id,
                        received,
                        &mut observations,
                    );
                }
            }
            if observations.len() == previous
                && (status_or_ai_missing(alarm)
                    || xml::child(alarm, "smartAiTypeList").is_some_and(|list| {
                        list.children().any(|child| child.is_element()) && self.rules.is_empty()
                    }))
            {
                let event = alarm_event(alarm, "AlarmEvent", self.source, id, received);
                observations.push(Observation {
                    event,
                    property: None,
                    initial: self.initial(false),
                    deleted: false,
                });
            }
            self.first = false;
        }

        for status in doc.descendants().filter(|node| {
            node.is_element()
                && !node.has_tag_name("AlarmEvent")
                && node
                    .parent()
                    .is_some_and(|parent| parent.tag_name().name().ends_with("EventList"))
        }) {
            if alarm_channel(status) != Some(self.channel) {
                continue;
            }
            let event = alarm_event(status, status.tag_name().name(), self.source, id, received);
            let key = format!("{}:{}", event.vendor_topic, self.channel);
            observations.push(Observation {
                event,
                property: Some(key),
                initial: self.initial(false),
                deleted: false,
            });
        }
        Ok(observations)
    }

    fn initial(&self, active: bool) -> bool {
        self.first && (self.source == "baichuan" || !active)
    }

    #[allow(clippy::too_many_arguments)]
    fn state(
        &mut self,
        node: Node<'_, '_>,
        token: &str,
        active: bool,
        rule_id: Option<String>,
        id: &str,
        received: DateTime<Utc>,
        out: &mut Vec<Observation>,
    ) {
        let topic = format!("AlarmEvent/{token}");
        let normalized = kind(&topic);
        let key = (normalized.clone(), rule_id.clone());
        if self.states.insert(key, active) == Some(active) {
            return;
        }
        let mut event = alarm_event(node, &topic, self.source, id, received);
        event.kind = normalized;
        event.active = Some(active);
        event.rule_id = rule_id;
        let initial = self.initial(active);
        event.operation = Some(if initial { "Initialized" } else { "Changed" }.into());
        let property = Some(format!(
            "{}:{}",
            self.channel,
            event.rule_id.as_deref().unwrap_or(token)
        ));
        out.push(Observation {
            event,
            property,
            initial,
            deleted: false,
        });
    }
}

fn status_or_ai_missing(alarm: Node<'_, '_>) -> bool {
    xml::child(alarm, "status").is_none()
        && xml::child(alarm, "AItype").is_none()
        && xml::child(alarm, "recording").is_none()
}

fn alarm_event(
    node: Node<'_, '_>,
    topic: &str,
    source: &'static str,
    id: &str,
    received: DateTime<Utc>,
) -> Event {
    let mut event = Event::new(id, kind(topic), source, topic.into(), received);
    event.camera_time = xml::value(node, "timeStamp").and_then(reolink_timestamp);
    event.channel = alarm_channel(node);
    event.vendor = xml::tree(node);
    event
}

fn alarm_channel(node: Node<'_, '_>) -> Option<u8> {
    xml::value(node, "channelId")
        .or_else(|| xml::value(node, "channel"))?
        .parse()
        .ok()
}

pub(super) fn tokens(value: &str) -> BTreeSet<String> {
    value
        .split([',', ';', ' '])
        .map(str::trim)
        .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case("none"))
        .take(64)
        .map(str::to_owned)
        .collect()
}

pub async fn ai_types(session: &mut Session, channel: u8) -> (BTreeSet<String>, Value) {
    let Ok(reply) = session.command(299, Some(channel), "").await else {
        return (BTreeSet::new(), Value::Null);
    };
    let Ok(doc) = xml::document(&reply) else {
        return (BTreeSet::new(), Value::Null);
    };
    let node = xml::find(doc.root_element(), "AiCfg").unwrap_or_else(|| doc.root_element());
    (
        xml::value(node, "detectType")
            .map(tokens)
            .unwrap_or_default(),
        xml::tree(node),
    )
}

pub async fn run(device: &Device, id: &str, hub: &Hub, stop: &CancellationToken) -> Result<()> {
    let (mut session, known) = tokio::select! {
        _ = stop.cancelled() => return Ok(()),
        result = async {
            let mut session = device.connect().await?;
            let (known, _) = ai_types(&mut session, device.channel).await;
            session.subscribe().await?;
            Ok::<_, anyhow::Error>((session, known))
        } => result?,
    };
    let mut decoder = AlarmDecoder::new(device.channel, "baichuan", known);
    let interval = Duration::from_secs(15);
    let mut ping = interval_at(Instant::now() + interval, interval);
    hub.availability(id, Some("baichuan"), "subscribed");
    hub.diagnostics.log(
        "reolink::events::run",
        "BAICHUAN_EVENTS_ACTIVE",
        "info",
        None,
        json!({"cameraId":id}),
    );
    let result = async {
        loop {
            while let Some(packet) = session.pushes.pop_front() {
                ingest(&session, packet, &mut decoder, id, hub);
            }
            tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                _ = ping.tick() => { session.command(93, None, "").await?; }
                packet = session.read() => ingest(&session, packet?, &mut decoder, id, hub),
            }
        }
    }
    .await;
    session.logout().await;
    result
}

fn ingest(session: &Session, packet: Packet, decoder: &mut AlarmDecoder, id: &str, hub: &Hub) {
    let received = Utc::now();
    if packet.command == 33
        && let Ok(body) = session.decode(&packet)
        && let Ok(messages) = decoder.decode(&body, id, received)
    {
        for message in messages {
            hub.ingest(message);
        }
    } else if packet.command == 291
        && let Ok(body) = session.decode(&packet)
        && let Some(active) = spotlight_report(&body, decoder.channel)
    {
        hub.section(
            id,
            "state/settings",
            "spotlight_report",
            json!({"enabled":active,"received_at":received}),
        );
    }
}

pub(super) fn spotlight_report(body: &str, channel: u8) -> Option<bool> {
    let doc = xml::document(body).ok()?;
    doc.descendants()
        .filter(|node| node.has_tag_name("FloodlightStatus"))
        .find(|node| alarm_channel(*node) == Some(channel))
        .and_then(|node| xml::value(node, "status"))
        .and_then(boolean)
}

pub fn parse_native(
    decoder: &mut AlarmDecoder,
    raw: &[u8],
    id: &str,
    received: DateTime<Utc>,
) -> Result<Vec<Observation>> {
    let mut payload: Value =
        serde_json::from_slice(raw).map_err(|_| anyhow!("invalid Reolink webhook body"))?;
    if payload["cmd"].as_u64() == Some(33) {
        let body = payload["xml"]
            .as_str()
            .ok_or_else(|| anyhow!("Reolink webhook is missing alarm XML"))?;
        return decoder.decode(body, id, received);
    }
    let data = payload
        .get_mut("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| anyhow!("Reolink webhook is missing data"))?;
    let event_name = data["event"]
        .as_str()
        .ok_or_else(|| anyhow!("Reolink webhook is missing an event"))?
        .to_owned();
    let types = match event_name.as_str() {
        "wake" => match data["reason"].as_str() {
            Some("pir") => vec!["awake".into(), "motion".into()],
            Some("doorbell") => vec!["awake".into(), "doorbell".into()],
            _ => vec!["awake".into()],
        },
        "sleep" => vec!["sleeping".into()],
        "test" => vec![],
        _ => vec![kind(&event_name)],
    };
    let camera_time = data["timeStamp"].as_str().and_then(reolink_timestamp);
    xml::strip_sensitive_fields(data);
    Ok(types
        .into_iter()
        .map(|event_type| {
            let mut event = Event::new(
                id,
                event_type,
                "reolink_webhook",
                format!("HaCfg/{event_name}"),
                received,
            );
            event.camera_time = camera_time;
            event.channel = Some(decoder.channel);
            event.vendor = data.clone();
            Observation {
                event,
                property: None,
                initial: false,
                deleted: false,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_baichuan_alarm_states_produce_edges_after_the_first_packet() {
        let mut decoder = AlarmDecoder::new(0, "baichuan", tokens("people,vehicle,dog_cat"));
        let idle = include_str!("../../tests/fixtures/baichuan-idle.xml");
        let active = include_str!("../../tests/fixtures/baichuan-active.xml");
        let init = decoder.decode(idle, "front-door", Utc::now()).unwrap();
        assert!(
            init.iter()
                .all(|item| item.initial && item.event.active == Some(false))
        );
        assert!(init.iter().any(|item| item.event.kind == "pet"));
        let raised = decoder.decode(active, "front-door", Utc::now()).unwrap();
        assert!(
            raised
                .iter()
                .all(|item| !item.initial && item.event.active == Some(true))
        );
        assert!(
            raised
                .iter()
                .any(|item| item.event.kind == "person" && item.event.rule_id.is_none())
        );
        assert!(
            raised
                .iter()
                .any(|item| item.event.rule_id.as_deref() == Some("intrusion:2"))
        );
        assert!(
            raised
                .iter()
                .any(|item| item.event.kind.starts_with("vendor_future_ai_"))
        );
        assert!(
            decoder
                .decode(active, "front-door", Utc::now())
                .unwrap()
                .is_empty()
        );
        let cleared = decoder.decode(idle, "front-door", Utc::now()).unwrap();
        assert_eq!(raised.len(), cleared.len());
        assert!(
            cleared
                .iter()
                .all(|item| !item.initial && item.event.active == Some(false))
        );
    }

    #[test]
    fn native_wake_events_omit_authentication_fields() {
        let mut decoder = AlarmDecoder::new(0, "reolink_webhook", BTreeSet::new());
        let events = parse_native(
            &mut decoder,
            br#"{"data":{"event":"wake","reason":"pir","authToken":"test-secret-do-not-publish"}}"#,
            "front-door",
            Utc::now(),
        )
        .unwrap();
        assert_eq!(events[1].event.kind, "motion");
        assert!(
            !events[1]
                .event
                .vendor
                .to_string()
                .contains("test-secret-do-not-publish")
        );
    }
}
