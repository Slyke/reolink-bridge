use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Serialize)]
pub struct Event {
    pub camera_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub source: &'static str,
    pub vendor_topic: String,
    pub received_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_time: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<Object>,
    pub vendor: Value,
}

impl Event {
    pub fn new(
        camera_id: &str,
        kind: String,
        source: &'static str,
        topic: String,
        received_at: DateTime<Utc>,
    ) -> Self {
        Self {
            camera_id: camera_id.into(),
            kind,
            source,
            vendor_topic: topic,
            received_at,
            camera_time: None,
            active: None,
            operation: None,
            rule_id: None,
            channel: None,
            objects: Vec::new(),
            vendor: Value::Null,
        }
    }
}

#[derive(Clone, Default, Serialize)]
pub struct Object {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracking_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounding_box: Option<Bounds>,
}

#[derive(Clone, Serialize)]
pub struct Bounds {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub coordinate_space: &'static str,
    pub reference_frame: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_height: Option<u32>,
}

#[derive(Serialize)]
pub struct Rule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration_id: Option<String>,
    pub source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub geometry: Vec<Value>,
    pub parameters: Value,
}

pub struct Observation {
    pub event: Event,
    pub property: Option<String>,
    pub initial: bool,
    pub deleted: bool,
}

pub fn boolean(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "open" => Some(true),
        "false" | "0" | "off" | "close" | "closed" => Some(false),
        _ => None,
    }
}

pub fn timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|time| time.to_utc())
}

pub fn reolink_timestamp(value: &str) -> Option<DateTime<Utc>> {
    timestamp(value).or_else(|| {
        let value = value.trim().parse::<i64>().ok()?;
        match value {
            946_684_800..=4_102_444_800 => DateTime::from_timestamp(value, 0),
            946_684_800_000..=4_102_444_800_000 => DateTime::from_timestamp_millis(value),
            946_684_800_000_000..=4_102_444_800_000_000 => DateTime::from_timestamp_micros(value),
            _ => None,
        }
    })
}

pub fn kind(topic: &str) -> String {
    let leaf = topic.rsplit(['/', ':']).next().unwrap_or(topic);
    let leaf = leaf.to_ascii_lowercase();
    let normalized = match leaf.as_str() {
        "motion" | "motiondetect" | "motionsensor" | "md" | "other" => "motion",
        "people" | "person" | "peopledetect" | "persondetect" => "person",
        "vehicle" | "vehicledetect" => "vehicle",
        "dog_cat" | "dogcatdetect" | "pet" | "petdetect" | "animal" => "pet",
        "visitor" | "doorbell" => "doorbell",
        "face" | "facedetect" => "face",
        "package" | "packagedetect" => "package",
        "tamper" | "tamperdetect" => "tamper",
        "recording" => "recording",
        "intrusion" => "intrusion",
        "crossline" => "crossline",
        "loitering" => "loitering",
        "legacy" => "legacy",
        "loss" => "loss",
        "daynightevent" => "day_night",
        _ => {
            let label: String = leaf
                .chars()
                .take(40)
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let hash = format!("{:x}", md5::compute(topic));
            return format!("vendor_{}_{}", label.trim_matches('_'), &hash[..12]);
        }
    };
    normalized.into()
}
