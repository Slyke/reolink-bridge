use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use roxmltree::Node;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{recordings, wire::Session};
use crate::{model::boolean, xml};

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RecordingKind {
    Continuous,
    Motion,
    Person,
    Vehicle,
    Pet,
    Face,
    Package,
    Doorbell,
}

impl RecordingKind {
    fn vendor(value: &str) -> Option<Self> {
        match value
            .trim()
            .trim_end_matches(',')
            .to_ascii_lowercase()
            .as_str()
        {
            "normal" => Some(Self::Continuous),
            "md" => Some(Self::Motion),
            "people" => Some(Self::Person),
            "vehicle" => Some(Self::Vehicle),
            "dog_cat" => Some(Self::Pet),
            "face" => Some(Self::Face),
            "package" => Some(Self::Package),
            "visitor" => Some(Self::Doorbell),
            _ => None,
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleChange {
    pub enabled: Option<bool>,
    pub schedule: Option<HoursChange>,
}
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HoursChange {
    pub kind: RecordingKind,
    #[serde(default)]
    pub enable_hours: Vec<u8>,
    #[serde(default)]
    pub disable_hours: Vec<u8>,
}
impl ScheduleChange {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.enabled.is_some() || self.schedule.is_some(),
            "empty recording schedule change"
        );
        if let Some(change) = &self.schedule {
            let mut slots = BTreeSet::new();
            ensure!(
                change
                    .enable_hours
                    .iter()
                    .chain(&change.disable_hours)
                    .all(|h| *h < 168 && slots.insert(h))
                    && !slots.is_empty(),
                "invalid or repeated recording schedule hour"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CaptureFormat {
    Jpeg,
    Mp4,
}
impl CaptureFormat {
    fn tag(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Mp4 => "mp4",
        }
    }
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Main,
    Sub,
}
impl Stream {
    pub(super) fn tag(self) -> &'static str {
        match self {
            Self::Main => "mainStream",
            Self::Sub => "subStream",
        }
    }
}
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LapseChange {
    pub enabled: Option<bool>,
    pub interval_seconds: Option<u32>,
    pub format: Option<CaptureFormat>,
    pub stream: Option<Stream>,
    pub overwrite: Option<bool>,
}
impl LapseChange {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.enabled.is_some()
                || self.interval_seconds.is_some()
                || self.format.is_some()
                || self.stream.is_some()
                || self.overwrite.is_some(),
            "empty time-lapse change"
        );
        ensure!(
            self.interval_seconds
                .is_none_or(|n| (2..=21600).contains(&n)),
            "time-lapse interval must be 2–21600 seconds"
        );
        Ok(())
    }
}

fn text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    xml::child(node, name).and_then(xml::text)
}
fn boolean_field(node: Node<'_, '_>, name: &str) -> Option<bool> {
    boolean(text(node, name)?)
}
fn element<'i>(node: Node<'_, 'i>) -> &'i str {
    &node.document().input_text()[node.range()]
}

fn with_fields(root: Node<'_, '_>, fields: Vec<(&str, String)>) -> Result<String> {
    let mut replacements = Vec::new();
    let mut extra = String::new();
    for (name, content) in fields {
        let value = format!("<{name}>{content}</{name}>");
        if let Some(node) = xml::child(root, name) {
            replacements.push((node, value));
        } else {
            extra.push_str(&value);
        }
    }
    let mut patched = xml::replace_nodes(root, replacements);
    if !extra.is_empty() {
        if patched.ends_with("/>") {
            patched.truncate(patched.len() - 2);
            patched.push_str(&format!(">{extra}</{}>", root.tag_name().name()));
        } else {
            let end = patched
                .rfind("</")
                .context("invalid camera setting element")?;
            patched.insert_str(end, &extra);
        }
    }
    Ok(patched)
}

#[derive(Clone, Copy)]
pub enum Group {
    Schedule,
    Lapse,
}
impl Group {
    fn ids(self) -> (u32, u32) {
        match self {
            Self::Schedule => (81, 82),
            Self::Lapse => (319, 320),
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Schedule => "recording_schedule",
            Self::Lapse => "time_lapse",
        }
    }
    fn root<'a, 'i>(self, doc: &'a roxmltree::Document<'i>, channel: u8) -> Result<Node<'a, 'i>> {
        let tag = match self {
            Self::Schedule => "Record",
            Self::Lapse => "timelapseCfg",
        };
        let node = xml::child(doc.root_element(), tag).context("camera setting not reported")?;
        ensure!(
            text(node, "channelId").and_then(|s| s.parse::<u8>().ok()) == Some(channel),
            "camera setting channel mismatch"
        );
        Ok(node)
    }
    async fn read(self, session: &mut Session, channel: u8) -> Result<String> {
        session.command(self.ids().0, Some(channel), "").await
    }
    fn state(self, node: Node<'_, '_>) -> Result<(Value, Value)> {
        match self {
            Self::Schedule => {
                let enabled =
                    boolean_field(node, "enable").context("recording switch not reported")?;
                let tables: BTreeMap<_, _> = schedule_nodes(node)?
                    .into_iter()
                    .map(|(kind, table)| (kind, xml::text(table).unwrap()))
                    .collect();
                Ok((
                    json!({"enabled":enabled,"schedules":tables}),
                    json!({"enabled":true,"kinds":tables.keys().collect::<Vec<_>>()}),
                ))
            }
            Self::Lapse => {
                let item = xml::child(node, "item");
                let overwrite = boolean_field(node, "overwrite");
                ensure!(
                    overwrite.is_some() || item.is_some(),
                    "time-lapse configuration not reported"
                );
                let value = |key| item.and_then(|i| text(i, key));
                let enabled = match item {
                    Some(i) => boolean_field(i, "enable").context("invalid time-lapse switch")?,
                    None => false,
                };
                let stream = match value("streamType") {
                    Some("mainStream") => Some("main"),
                    Some("subStream") => Some("sub"),
                    other => other,
                };
                Ok((
                    json!({"enabled":enabled,"configured":value("id").is_some() || enabled,
                    "interval_seconds":value("interval").and_then(|v|v.parse::<u32>().ok()),"format":value("taskType"),"stream":stream,"overwrite":overwrite}),
                    json!({"configure":true,"overwrite":overwrite.is_some()}),
                ))
            }
        }
    }
    pub async fn inspect(self, session: &mut Session, channel: u8) -> Option<(Value, Value)> {
        let raw = self.read(session, channel).await.ok()?;
        let doc = xml::document(&raw).ok()?;
        self.state(self.root(&doc, channel).ok()?).ok()
    }
}

fn schedule_nodes<'a, 'i>(node: Node<'a, 'i>) -> Result<BTreeMap<RecordingKind, Node<'a, 'i>>> {
    let mut tables = BTreeMap::new();
    let list = xml::child(node, "typeScheduleList").context("recording timetable not reported")?;
    for item in list.children().filter(|n| n.has_tag_name("item")) {
        if xml::child(item, "index").is_some() {
            continue;
        }
        let Some(kind) = text(item, "type").and_then(RecordingKind::vendor) else {
            continue;
        };
        let table = xml::child(item, "valueTable").context("missing hourly timetable")?;
        let value = xml::text(table).context("empty hourly timetable")?;
        ensure!(
            value.len() == 168
                && value.bytes().all(|v| matches!(v, b'0' | b'1'))
                && tables.insert(kind, table).is_none(),
            "unsupported hourly timetable"
        );
    }
    Ok(tables)
}
impl ScheduleChange {
    fn patch(&self, node: Node<'_, '_>) -> Result<String> {
        let mut edits = Vec::new();
        if let Some(enabled) = self.enabled {
            let field = xml::child(node, "enable").context("recording switch unavailable")?;
            edits.push((field, format!("<enable>{}</enable>", u8::from(enabled))));
        }
        if let Some(change) = &self.schedule {
            let table = schedule_nodes(node)?
                .get(&change.kind)
                .copied()
                .context("recording type unavailable")?;
            let mut bytes = xml::text(table).unwrap().as_bytes().to_vec();
            for hour in &change.enable_hours {
                bytes[*hour as usize] = b'1'
            }
            for hour in &change.disable_hours {
                bytes[*hour as usize] = b'0'
            }
            let hours = String::from_utf8(bytes)?;
            edits.push((table, format!("<valueTable>{hours}</valueTable>")));
        }
        Ok(xml::replace_nodes(node, edits))
    }
}

impl LapseChange {
    fn needs_start(&self, node: Node<'_, '_>) -> bool {
        self.enabled == Some(true)
            && !xml::child(node, "item").is_some_and(|i| {
                text(i, "id").is_some() || boolean_field(i, "enable") == Some(true)
            })
    }
    fn patch(&self, node: Node<'_, '_>, now: Option<chrono::NaiveDateTime>) -> Result<String> {
        let item = xml::child(node, "item");
        let mut top = Vec::new();
        let supports_overwrite = boolean_field(node, "overwrite").is_some();
        if self.overwrite.is_some() || (now.is_some() && supports_overwrite) {
            ensure!(supports_overwrite, "overwrite unavailable");
            top.push((
                "overwrite",
                u8::from(self.overwrite.unwrap_or(true)).to_string(),
            ));
        }
        let mut fields = Vec::new();
        if let Some(v) = self.enabled {
            fields.push(("enable", u8::from(v).to_string()))
        }
        if let Some(v) = self.interval_seconds {
            fields.push(("interval", v.to_string()))
        }
        if let Some(v) = self.format {
            fields.push(("taskType", v.tag().into()))
        }
        if let Some(v) = self.stream {
            fields.push(("streamType", v.tag().into()))
        }
        if let Some(start) = now {
            // Reolink identifies a new task by epoch milliseconds. Its date fields use camera-local time.
            let created = chrono::Utc::now();
            let start_time = start.format("<year>%Y</year><month>%-m</month><day>%-d</day><hour>%-H</hour><minute>%-M</minute>").to_string();
            fields.extend([
                ("id", created.timestamp_millis().to_string()),
                (
                    "properties",
                    format!("v1_{}_Customize", created.timestamp()),
                ),
                ("startTime", start_time.clone()),
                ("endTime", start_time),
                ("neverEnd", "1".into()),
            ]);
        }
        let adjusting_capture =
            self.format.is_some() || self.stream.is_some() || self.interval_seconds.is_some();
        if now.is_some()
            || (adjusting_capture
                && item.and_then(|i| boolean_field(i, "buseThumbnail")) == Some(true))
        {
            let reported = |name| item.and_then(|i| text(i, name));
            let format = self
                .format
                .map(CaptureFormat::tag)
                .or_else(|| reported("taskType"))
                .unwrap_or("jpeg");
            let stream = self
                .stream
                .map(Stream::tag)
                .or_else(|| reported("streamType"))
                .unwrap_or("mainStream");
            let interval = self
                .interval_seconds
                .or_else(|| reported("interval")?.parse().ok())
                .unwrap_or(10);
            // A Fluent JPEG already is the small image. Separate thumbnails also count toward the camera's daily file limit.
            let file_limit = text(node, "maxFileNum").and_then(|v| v.parse::<u32>().ok());
            let thumbnail = format == "jpeg"
                && stream != "subStream"
                && interval > 0
                && file_limit.is_some_and(|limit| 86_400u32.div_ceil(interval) * 2 <= limit);
            fields.push(("buseThumbnail", u8::from(thumbnail).to_string()));
        }
        if !fields.is_empty() {
            let empty = xml::document(
                "<item><enable>0</enable><interval>10</interval><frameRate>15</frameRate><taskType>jpeg</taskType><streamType>mainStream</streamType><buseThumbnail>1</buseThumbnail></item>",
            )?;
            let mut updated = with_fields(item.unwrap_or_else(|| empty.root_element()), fields)?;
            if now.is_some() {
                let doc = xml::document(&updated)?;
                let i = doc.root_element();
                // Preserve an existing daily schedule. For a new blank task enable its first full-day window.
                if !i
                    .children()
                    .any(|d| d.has_tag_name("duration") && boolean_field(d, "valid") == Some(true))
                {
                    let window = "<valid>1</valid><startTime><hour>0</hour><minute>0</minute><second>0</second></startTime><endTime><hour>23</hour><minute>59</minute><second>59</second></endTime>";
                    updated = with_fields(i, vec![("duration", window.into())])?;
                }
            }
            if let Some(i) = item {
                let patched = xml::replace_nodes(node, vec![(i, updated)]);
                let doc = xml::document(&patched)?;
                return with_fields(doc.root_element(), top);
            }
            let doc = xml::document(&updated)?;
            let root = doc.root_element();
            top.push(("item", root.children().map(element).collect()));
        }
        with_fields(node, top)
    }
}

pub async fn apply(
    session: &mut Session,
    channel: u8,
    command: &super::control::Control,
) -> Result<(&'static str, Value, bool)> {
    use super::control::Control;
    let group = match command {
        Control::RecordingSchedule(_) => Group::Schedule,
        Control::TimeLapse(_) => Group::Lapse,
        _ => unreachable!(),
    };
    let raw = group.read(session, channel).await?;
    let doc = xml::document(&raw)?;
    let node = group.root(&doc, channel)?;
    group.state(node)?;
    let new_task = matches!(command, Control::TimeLapse(change) if change.needs_start(node));
    let result = match command {
        Control::RecordingSchedule(change) => change.patch(node)?,
        Control::TimeLapse(change) => {
            let now = if new_task {
                Some(recordings::clock(session).await?)
            } else {
                None
            };
            change.patch(node, now)?
        }
        _ => unreachable!(),
    };
    let expected_doc = xml::document(&result)?;
    let expected = group.state(expected_doc.root_element())?.0;
    if result != element(node) {
        session
            .command(
                group.ids().1,
                Some(channel),
                &format!("<body>{result}</body>"),
            )
            .await?;
    }
    let readback = group.read(session, channel).await?;
    let doc = xml::document(&readback)?;
    let actual_root = group.root(&doc, channel)?;
    let actual = group.state(actual_root)?.0;
    let verified = match command {
        Control::RecordingSchedule(_) => expected == actual,
        Control::TimeLapse(_) if new_task => {
            expected == actual
                && xml::child(actual_root, "item")
                    .and_then(|i| text(i, "id"))
                    .is_some()
        }
        Control::TimeLapse(change) => [
            ("enabled", change.enabled.is_some()),
            ("interval_seconds", change.interval_seconds.is_some()),
            ("format", change.format.is_some()),
            ("stream", change.stream.is_some()),
            ("overwrite", change.overwrite.is_some()),
        ]
        .iter()
        .all(|(k, set)| !set || expected[*k] == actual[*k]),
        _ => unreachable!(),
    };
    Ok((group.key(), actual, verified))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tasks_honor_opt_out_and_edits_preserve_the_existing_task() {
        let start = chrono::NaiveDate::from_ymd_opt(2026, 11, 20)
            .unwrap()
            .and_hms_opt(19, 14, 30)
            .unwrap();
        let empty = xml::document("<timelapseCfg><channelId>0</channelId><overwrite>1</overwrite><maxFileNum>10000</maxFileNum></timelapseCfg>").unwrap();
        let create: LapseChange = serde_json::from_value(json!({"enabled":true,"overwrite":false,"interval_seconds":30,"format":"jpeg","stream":"main"})).unwrap();
        let created = create.patch(empty.root_element(), Some(start)).unwrap();
        let saved = xml::document(&created).unwrap();
        let root = saved.root_element();
        assert_eq!(boolean_field(root, "overwrite"), Some(false));
        let item = xml::child(root, "item").unwrap();
        assert_eq!(boolean_field(item, "buseThumbnail"), Some(true));
        let edit: LapseChange = serde_json::from_value(json!({"stream":"sub"})).unwrap();
        assert!(!edit.needs_start(root));
        let edited = edit.patch(root, None).unwrap();
        let after = xml::document(&edited).unwrap();
        let next = xml::child(after.root_element(), "item").unwrap();
        assert_eq!(boolean_field(next, "buseThumbnail"), Some(false));
        assert_eq!(
            boolean_field(after.root_element(), "overwrite"),
            Some(false)
        );
        for tag in ["id", "properties", "startTime", "endTime", "duration"] {
            assert_eq!(
                element(xml::child(next, tag).unwrap()),
                element(xml::child(item, tag).unwrap())
            );
        }
    }
}
