use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use roxmltree::{Document, Node};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::timeout;

use super::{
    events::spotlight_report,
    ptz,
    wire::{Device, Session},
};
use crate::{model::boolean, xml};

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "setting", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Spotlight {
        enabled: bool,
        #[serde(default = "light_duration")]
        duration_seconds: u16,
    },
    SpotlightBrightness {
        brightness: u8,
    },
    StatusLed {
        enabled: bool,
    },
    IrLed {
        mode: IrMode,
    },
    MotionDetection {
        enabled: Option<bool>,
        sensitivity: Option<u8>,
    },
    AiDetection {
        class: AiClass,
        sensitivity: Option<u8>,
        delay_seconds: Option<u8>,
    },
    RecordingSchedule(super::recording_settings::ScheduleChange),
    TimeLapse(super::recording_settings::LapseChange),
    ImageOrientation {
        action: super::orientation::Action,
    },
    Ptz {
        action: ptz::Action,
    },
}

fn light_duration() -> u16 {
    60
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IrMode {
    Auto,
    Off,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AiClass {
    Person,
    Vehicle,
    Pet,
    Face,
    Package,
}

impl AiClass {
    pub fn from_vendor(value: &str) -> Option<Self> {
        match value {
            "people" => Some(Self::Person),
            "vehicle" => Some(Self::Vehicle),
            "dog_cat" => Some(Self::Pet),
            "face" => Some(Self::Face),
            "package" => Some(Self::Package),
            _ => None,
        }
    }

    fn vendor(self) -> &'static str {
        match self {
            Self::Person => "people",
            Self::Vehicle => "vehicle",
            Self::Pet => "dog_cat",
            Self::Face => "face",
            Self::Package => "package",
        }
    }

    pub fn topic(self) -> &'static str {
        match self {
            Self::Person => "ai_person",
            Self::Vehicle => "ai_vehicle",
            Self::Pet => "ai_pet",
            Self::Face => "ai_face",
            Self::Package => "ai_package",
        }
    }
}

impl Control {
    pub fn ptz_action(&self) -> Option<&ptz::Action> {
        match self {
            Self::Ptz { action } => Some(action),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Spotlight {
                duration_seconds, ..
            } => ensure!(
                (1..=180).contains(duration_seconds),
                "duration_seconds must be 1–180"
            ),
            Self::SpotlightBrightness { brightness } => {
                ensure!(*brightness <= 100, "brightness must be 0–100")
            }
            Self::MotionDetection {
                enabled,
                sensitivity,
            } => {
                ensure!(
                    enabled.is_some() || sensitivity.is_some(),
                    "send enabled or sensitivity"
                );
                ensure!(
                    sensitivity.is_none_or(|n| (1..=50).contains(&n)),
                    "motion sensitivity must be 1–50"
                );
            }
            Self::AiDetection {
                sensitivity,
                delay_seconds,
                ..
            } => {
                ensure!(
                    sensitivity.is_some() || delay_seconds.is_some(),
                    "send sensitivity or delay_seconds"
                );
                ensure!(
                    sensitivity.is_none_or(|n| n <= 100),
                    "AI sensitivity must be 0–100"
                );
                ensure!(
                    delay_seconds.is_none_or(|n| n <= 8),
                    "delay_seconds must be 0–8"
                );
            }
            Self::RecordingSchedule(change) => change.validate()?,
            Self::TimeLapse(change) => change.validate()?,
            Self::Ptz { action } => action.validate()?,
            Self::StatusLed { .. } | Self::IrLed { .. } | Self::ImageOrientation { .. } => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub enum Settings {
    Leds,
    Spotlight,
    Motion,
    Ai(AiClass),
    Image,
    RecordingSchedule,
    TimeLapse,
}

impl Settings {
    pub fn key(self) -> &'static str {
        match self {
            Self::Leds => "leds",
            Self::Spotlight => "spotlight",
            Self::Motion => "motion_detection",
            Self::Image => "image",
            Self::TimeLapse => "time_lapse",
            Self::RecordingSchedule => "recording_schedule",
            Self::Ai(class) => class.topic(),
        }
    }

    async fn read(self, session: &mut Session, channel: u8) -> Result<String> {
        let (id, query) = match self {
            Self::TimeLapse | Self::RecordingSchedule => {
                unreachable!("recording settings use their native codec")
            }
            Self::Leds => (208, String::new()),
            Self::Spotlight => (289, String::new()),
            Self::Motion => (46, String::new()),
            Self::Image => (26, String::new()),
            Self::Ai(class) => (
                342,
                format!(
                    r#"<body><AiDetectCfg version="1.1"><chn>{channel}</chn><type>{}</type></AiDetectCfg></body>"#,
                    class.vendor()
                ),
            ),
        };
        session.command(id, Some(channel), &query).await
    }

    fn root<'a, 'input>(self, doc: &'a Document<'input>, channel: u8) -> Result<Node<'a, 'input>> {
        let (tag, channel_tag) = match self {
            Self::TimeLapse | Self::RecordingSchedule => {
                unreachable!("recording settings use their native codec")
            }
            Self::Leds => ("LedState", "channelId"),
            Self::Spotlight => ("FloodlightTask", "channel"),
            Self::Motion => ("MD", "channelId"),
            Self::Image => ("VideoInput", "channelId"),
            Self::Ai(_) => ("AiDetectCfg", "chn"),
        };
        let node = xml::child(doc.root_element(), tag)
            .ok_or_else(|| anyhow!("camera did not expose the requested setting"))?;
        ensure!(
            xml::child(node, channel_tag)
                .and_then(xml::text)
                .and_then(|value| value.parse().ok())
                == Some(channel),
            "camera reported a different channel"
        );
        if let Self::Ai(class) = self {
            ensure!(
                xml::child(node, "type").and_then(xml::text) == Some(class.vendor()),
                "camera reported a different AI class"
            );
        }
        Ok(node)
    }

    fn status(self, node: Node<'_, '_>) -> Value {
        let get = |name| xml::child(node, name).and_then(xml::text);
        let num = |name| get(name).and_then(|value| value.parse::<u8>().ok());
        let mut result = match self {
            Self::TimeLapse | Self::RecordingSchedule => {
                unreachable!("recording settings use their native codec")
            }
            Self::Leds => json!({
                "status_led": get("lightState").and_then(boolean),
                "ir_mode": match get("state") { Some("auto") => Some("auto"), Some("close") => Some("off"), _ => None },
            }),
            Self::Spotlight => json!({
                "enabled": get("status").and_then(boolean),
                "brightness": num("brightness_cur"),
                "brightness_min": num("brightness_min"),
                "brightness_max": num("brightness_max"),
                "mode": num("alarmMode"),
            }),
            Self::Motion => json!({
                "enabled": get("enable").and_then(boolean),
                "sensitivity": xml::child(node, "sensInfoNew").and_then(|n| xml::value(n, "sensitivityDefault"))
                    .and_then(|s| s.parse::<u8>().ok()).filter(|n| (1..=50).contains(n)).map(|n| 51 - n),
            }),
            Self::Image => super::orientation::status(node).0,
            Self::Ai(_) => json!({
                "sensitivity": num("sensitivity"),
                "delay_seconds": get("stayTime").and_then(|s| s.parse::<f64>().ok()),
            }),
        };
        result["vendor"] = xml::tree(node);
        result
    }

    pub async fn inspect(self, session: &mut Session, channel: u8) -> Result<(Value, Value)> {
        use super::recording_settings::Group;
        match self {
            Self::RecordingSchedule => return Group::Schedule.inspect(session, channel).await,
            Self::TimeLapse => return Group::Lapse.inspect(session, channel).await,
            _ => {}
        }
        if matches!(self, Self::Image) {
            return super::orientation::inspect(session, channel).await;
        }
        let raw = self.read(session, channel).await?;
        let doc = xml::document(&raw)?;
        let node = self.root(&doc, channel)?;
        let status = self.status(node);
        let supported = |field| !status[field].is_null();
        let controls = match self {
            Self::TimeLapse | Self::RecordingSchedule => {
                unreachable!("recording settings use their native codec")
            }
            Self::Leds => {
                json!({"status_led": supported("status_led"), "ir_led": supported("ir_mode")})
            }
            Self::Spotlight => json!({
                "manual": true, "brightness": supported("brightness"),
                "brightness_min": status["brightness_min"], "brightness_max": status["brightness_max"],
            }),
            Self::Motion => {
                json!({"enabled": supported("enabled"), "sensitivity": supported("sensitivity")})
            }
            Self::Image => super::orientation::status(node).1,
            Self::Ai(_) => {
                json!({"sensitivity": supported("sensitivity"), "delay_seconds": supported("delay_seconds")})
            }
        };
        Ok((status, controls))
    }
}

pub async fn apply(device: &Device, command: &Control) -> Result<(&'static str, Value, bool)> {
    command.validate()?;
    let mut session = device.connect().await?;
    let result = apply_connected(&mut session, device.channel, command).await;
    session.logout().await;
    result
}

async fn apply_connected(
    session: &mut Session,
    channel: u8,
    command: &Control,
) -> Result<(&'static str, Value, bool)> {
    if matches!(
        command,
        Control::RecordingSchedule(_) | Control::TimeLapse(_)
    ) {
        return super::recording_settings::apply(session, channel, command).await;
    }
    if let Control::ImageOrientation { action } = command {
        return super::orientation::apply(session, channel, *action).await;
    }
    if let Control::Spotlight {
        enabled,
        duration_seconds,
    } = command
    {
        let raw = Settings::Spotlight.read(session, channel).await?;
        let doc = xml::document(&raw)?;
        Settings::Spotlight.root(&doc, channel)?;
        // An event subscription can provide the physical state on models whose settings omit it.
        let _ = session.subscribe().await;
        session.pushes.clear();
        let value = u8::from(*enabled);
        let body = format!(
            r#"<body><FloodlightManual version="1.1"><channelId>{channel}</channelId>
            <status>{value}</status><duration>{duration_seconds}</duration></FloodlightManual></body>"#
        );
        session.command(288, Some(channel), &body).await?;
        let report = timeout(Duration::from_millis(1500), async {
            loop {
                let packet = match session.pushes.pop_front() {
                    Some(packet) => packet,
                    None => session.read().await.ok()?,
                };
                if packet.command == 291
                    && let Some(state) = spotlight_report(&session.decode(&packet).ok()?, channel)
                {
                    return Some(state);
                }
            }
        })
        .await
        .ok()
        .flatten();
        let verified = report == Some(*enabled);
        return Ok((
            "spotlight_report",
            json!({"enabled": report, "received_at": chrono::Utc::now()}),
            verified,
        ));
    }

    let (settings, setter, edits) = match command {
        Control::SpotlightBrightness { brightness } => (
            Settings::Spotlight,
            290,
            vec![(vec!["brightness_cur"], brightness.to_string())],
        ),
        Control::StatusLed { enabled } => (
            Settings::Leds,
            209,
            vec![(
                vec!["lightState"],
                if *enabled { "open" } else { "close" }.into(),
            )],
        ),
        Control::IrLed { mode } => (
            Settings::Leds,
            209,
            vec![(
                vec!["state"],
                if *mode == IrMode::Auto {
                    "auto"
                } else {
                    "close"
                }
                .into(),
            )],
        ),
        Control::MotionDetection {
            enabled,
            sensitivity,
        } => {
            let mut edits = Vec::new();
            if let Some(value) = enabled {
                edits.push((vec!["enable"], u8::from(*value).to_string()));
            }
            if let Some(value) = sensitivity {
                edits.push((
                    vec!["sensInfoNew", "sensitivityDefault"],
                    (51 - value).to_string(),
                ));
            }
            (Settings::Motion, 47, edits)
        }
        Control::AiDetection {
            class,
            sensitivity,
            delay_seconds,
        } => {
            let mut edits = Vec::new();
            if let Some(value) = sensitivity {
                edits.push((vec!["sensitivity"], value.to_string()));
            }
            if let Some(value) = delay_seconds {
                edits.push((vec!["stayTime"], value.to_string()));
            }
            (Settings::Ai(*class), 343, edits)
        }
        Control::Spotlight { .. }
        | Control::ImageOrientation { .. }
        | Control::RecordingSchedule(_)
        | Control::TimeLapse(_) => unreachable!(),
        Control::Ptz { .. } => anyhow::bail!("PTZ uses the PTZ command worker"),
    };
    let raw = settings.read(session, channel).await?;
    let doc = xml::document(&raw)?;
    let node = settings.root(&doc, channel)?;
    if let Control::SpotlightBrightness { brightness } = command {
        let numeric = |field, fallback| {
            xml::value(node, field)
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(fallback)
        };
        ensure!(
            (numeric("brightness_min", 0)..=numeric("brightness_max", 100)).contains(brightness),
            "brightness is outside the camera's reported range"
        );
    }
    let body = edit_xml(&doc, node, &edits)?;
    session.command(setter, Some(channel), &body).await?;
    let raw = settings.read(session, channel).await?;
    let doc = xml::document(&raw)?;
    let node = settings.root(&doc, channel)?;
    let verified = edits.iter().all(|(path, expected)| {
        field(node, path).and_then(xml::text).is_some_and(|value| {
            value == expected
                || value
                    .parse::<f64>()
                    .ok()
                    .zip(expected.parse::<f64>().ok())
                    .is_some_and(|(a, b)| a == b)
        })
    });
    Ok((settings.key(), settings.status(node), verified))
}

fn field<'a, 'input>(node: Node<'a, 'input>, path: &[&str]) -> Option<Node<'a, 'input>> {
    path.iter()
        .try_fold(node, |node, name| xml::child(node, name))
}

fn edit_xml(
    doc: &Document<'_>,
    node: Node<'_, '_>,
    edits: &[(Vec<&str>, String)],
) -> Result<String> {
    let root = doc.root_element().range();
    let mut body = doc.input_text()[root.clone()].to_owned();
    let mut patches = Vec::new();
    for (path, value) in edits {
        let text = field(node, path)
            .and_then(|n| n.children().find(Node::is_text))
            .ok_or_else(|| anyhow!("camera did not expose the requested setting field"))?;
        let range = text.range();
        patches.push((
            range.start - root.start..range.end - root.start,
            xml::escape(value),
        ));
    }
    patches.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    for (range, replacement) in patches {
        body.replace_range(range, &replacement);
    }
    Ok(body)
}
