use super::{Problem, Result};
use crate::xml;
use roxmltree::Node;
use serde_json::{Map, Value, json};

#[derive(Clone, Copy)]
enum Kind {
    Switch,
    Number(i32, i32),
    ReportedRange,
    Choice(&'static [&'static str]),
    Flicker,
}
use Kind::*;
type Field = (&'static str, &'static str, Kind);

pub(super) struct Group {
    pub get: u32,
    pub set: u32,
    root: &'static str,
    fields: &'static [Field],
}

impl Group {
    pub fn device_wide(&self) -> bool {
        self.get == 68
    }
    pub fn wire_channel(&self, channel: u8) -> Option<u8> {
        (!self.device_wide()).then_some(channel)
    }
    pub fn named(name: &str) -> Option<Self> {
        let (get, set, root, fields): (_, _, _, &[Field]) = match name {
            "picture" => (
                26,
                25,
                "VideoInput",
                &[
                    ("brightness", "VideoInput/bright", Number(0, 255)),
                    ("contrast", "VideoInput/contrast", Number(0, 255)),
                    ("saturation", "VideoInput/saturation", Number(0, 255)),
                    ("hue", "VideoInput/hue", Number(0, 255)),
                    ("sharpness", "VideoInput/sharpen", Number(0, 255)),
                    (
                        "dayNight",
                        "InputAdvanceCfg/DayNight/mode",
                        Choice(&["auto", "color", "blackAndWhite"]),
                    ),
                    (
                        "antiFlicker",
                        "InputAdvanceCfg/PowerLineFrequency/mode",
                        Flicker,
                    ),
                    (
                        "exposure",
                        "InputAdvanceCfg/Exposure/mode",
                        Choice(&["auto", "lownoise", "anti-smearing", "manual"]),
                    ),
                    (
                        "whiteBalance",
                        "InputAdvanceCfg/Scene/mode",
                        Choice(&["auto", "manual"]),
                    ),
                    (
                        "redGain",
                        "InputAdvanceCfg/Scene/Redgain/cur",
                        ReportedRange,
                    ),
                    (
                        "blueGain",
                        "InputAdvanceCfg/Scene/Bluegain/cur",
                        ReportedRange,
                    ),
                    ("hdr", "InputAdvanceCfg/hdrSwitch", Choice(&["0", "1", "2"])),
                    ("noiseReduction", "InputAdvanceCfg/nr3d/enable", Switch),
                    (
                        "backlightCompensation",
                        "InputAdvanceCfg/BLC/enable",
                        Switch,
                    ),
                    (
                        "dayTone",
                        "InputAdvanceCfg/bdDayColor/mode",
                        Choice(&["auto", "manual"]),
                    ),
                    (
                        "dayHighlight",
                        "InputAdvanceCfg/bdDayColor/bright/cur",
                        ReportedRange,
                    ),
                    (
                        "dayShadow",
                        "InputAdvanceCfg/bdDayColor/dark/cur",
                        ReportedRange,
                    ),
                    (
                        "nightTone",
                        "InputAdvanceCfg/bdNight/mode",
                        Choice(&["auto", "manual"]),
                    ),
                    (
                        "nightHighlight",
                        "InputAdvanceCfg/bdNight/bright/cur",
                        ReportedRange,
                    ),
                    (
                        "nightShadow",
                        "InputAdvanceCfg/bdNight/dark/cur",
                        ReportedRange,
                    ),
                ],
            ),
            "threshold" => (
                296,
                297,
                "DayNightThreshold",
                &[(
                    "switchThreshold",
                    "DayNightThreshold/thresholdval/cur",
                    ReportedRange,
                )],
            ),
            "sound" => (
                264,
                265,
                "audioCfg",
                &[
                    ("speakerVolume", "audioCfg/volume", Number(0, 100)),
                    ("talkVolume", "audioCfg/talkAndReplyVolume", Number(0, 100)),
                    ("doorbellVolume", "audioCfg/visitorVolume", Number(0, 100)),
                ],
            ),
            "recordingAudio" => (
                56,
                57,
                "Compression",
                &[
                    ("mainAudio", "Compression/mainStream/audio", Switch),
                    ("subAudio", "Compression/subStream/audio", Switch),
                    ("thirdAudio", "Compression/thirdStream/audio", Switch),
                ],
            ),
            "privacy" => (
                52,
                53,
                "Shelter",
                &[("maskEnabled", "Shelter/enable", Switch)],
            ),
            "tracking" => (
                299,
                300,
                "AiCfg",
                &[("autoTracking", "AiCfg/smartTrack", Switch)],
            ),
            "siren" => (
                232,
                231,
                "AudioTask",
                &[("sirenOnEvent", "AudioTask/enable", Switch)],
            ),
            "push" => (
                219,
                218,
                "PushTask",
                &[("pushEnabled", "PushTask/enable", Switch)],
            ),
            "email" => (
                217,
                216,
                "EmailTask",
                &[("emailEnabled", "EmailTask/enable", Switch)],
            ),
            "ftpDestination" => (68, 69, "Ftp", &[]),
            "ftp" => (
                70,
                71,
                "FtpTask",
                &[("ftpEnabled", "FtpTask/enable", Switch)],
            ),
            _ => return None,
        };
        Some(Self {
            get,
            set,
            root,
            fields,
        })
    }
    fn channel(&self, root: Node<'_, '_>, channel: u8) -> Result<()> {
        let matches = |section: Node<'_, '_>| {
            xml::child(section, "channelId")
                .and_then(xml::text)
                .and_then(|v| v.parse::<u8>().ok())
                == Some(channel)
        };
        if !xml::child(root, self.root).is_some_and(matches)
            || (self.get == 26
                && xml::child(root, "InputAdvanceCfg").is_some_and(|advanced| {
                    xml::child(advanced, "channelId").is_some() && !matches(advanced)
                }))
        {
            return Err(Problem::Unsupported);
        }
        Ok(())
    }
    pub fn describe(&self, root: Node<'_, '_>, channel: u8) -> Result<Value> {
        if self.device_wide() {
            return super::ftp::describe(root, channel);
        }
        self.channel(root, channel)?;
        let mut fields = Map::new();
        for &(key, path, kind) in self.fields {
            if let Some(description) = at(root, path).and_then(|node| kind.describe(node)) {
                fields.insert(key.into(), description);
            }
        }
        let mut result = json!({"fields":fields});
        if self.get == 52 {
            let zones = at(root, "Shelter/shelterList").map(|list| {
                list.children()
                    .filter(|node| node.has_tag_name("Shelter"))
                    .count()
            });
            if let Some(count) = zones {
                result["privacyZones"] = json!(count);
            }
        }
        Ok(result)
    }
    pub fn edit(
        &self,
        root: Node<'_, '_>,
        channel: u8,
        fields: &Map<String, Value>,
    ) -> Result<Option<String>> {
        if self.device_wide() {
            return super::ftp::edit(root, channel, fields);
        }
        if fields.is_empty() {
            return Err(Problem::Invalid);
        }
        let current = self.describe(root, channel)?;
        let mut edits = Vec::new();
        for (key, value) in fields {
            let &(_, path, kind) = self
                .fields
                .iter()
                .find(|(k, ..)| k == key)
                .ok_or(Problem::Invalid)?;
            let spec = current["fields"].get(key).ok_or(Problem::Unsupported)?;
            if spec["value"] == *value {
                continue;
            }
            if key == "maskEnabled"
                && value == true
                && current["privacyZones"].as_u64().unwrap_or(0) == 0
            {
                return Err(Problem::Unsupported);
            }
            let node = at(root, path).ok_or(Problem::Unsupported)?;
            kind.write(node, spec, value, &mut edits)?;
        }
        Ok((!edits.is_empty()).then(|| xml::replace_nodes(root, edits)))
    }
}
fn at<'a, 'b>(root: Node<'a, 'b>, path: &str) -> Option<Node<'a, 'b>> {
    path.split('/').try_fold(root, xml::child)
}
fn scalar<'a, 'b>(
    edits: &mut Vec<(Node<'a, 'b>, String)>,
    node: Node<'a, 'b>,
    value: &str,
) -> Result<()> {
    let text = node
        .children()
        .find(Node::is_text)
        .ok_or(Problem::Unsupported)?;
    edits.push((text, xml::escape(value)));
    Ok(())
}
impl Kind {
    fn describe(self, node: Node<'_, '_>) -> Option<Value> {
        let raw = xml::text(node)?;
        let parent = node.parent()?;
        match self {
            Switch => match raw {
                "0" | "1" => Some(json!({"value":raw=="1"})),
                _ => None,
            },
            Number(_, _) | ReportedRange => {
                let bounds = match self {
                    Number(min, max) => (min, max),
                    _ => (
                        xml::child(parent, "min")
                            .and_then(xml::text)?
                            .parse::<i32>()
                            .ok()?,
                        xml::child(parent, "max")
                            .and_then(xml::text)?
                            .parse::<i32>()
                            .ok()?,
                    ),
                };
                let value = raw.parse::<i32>().ok()?;
                (bounds.0 <= value && value <= bounds.1)
                    .then(|| json!({"value":value,"min":bounds.0,"max":bounds.1}))
            }
            Choice(choices) => {
                let advertised = xml::child(parent, "modeList").and_then(xml::text);
                let valid: Vec<_> = choices
                    .iter()
                    .copied()
                    .filter(|choice| {
                        advertised.is_none_or(|list| list.split(',').any(|v| v.trim() == *choice))
                    })
                    .collect();
                valid
                    .contains(&raw)
                    .then(|| json!({"value":raw,"choices":valid}))
            }
            Flicker => {
                let enabled = xml::child(parent, "enable").and_then(xml::text)?;
                let value = match enabled {
                    "0" => "off",
                    "1" if ["50hz", "60hz", "outdoor"].contains(&raw) => raw,
                    _ => return None,
                };
                Some(json!({"value":value,"choices":["off","50hz","60hz","outdoor"]}))
            }
        }
    }
    fn write<'a, 'b>(
        self,
        node: Node<'a, 'b>,
        spec: &Value,
        value: &Value,
        edits: &mut Vec<(Node<'a, 'b>, String)>,
    ) -> Result<()> {
        match self {
            Switch => scalar(
                edits,
                node,
                if value.as_bool().ok_or(Problem::Invalid)? {
                    "1"
                } else {
                    "0"
                },
            ),
            Number(_, _) | ReportedRange => {
                let n = value
                    .as_i64()
                    .filter(|v| {
                        spec["min"].as_i64().is_some_and(|min| *v >= min)
                            && spec["max"].as_i64().is_some_and(|max| *v <= max)
                    })
                    .ok_or(Problem::Invalid)?;
                scalar(edits, node, &n.to_string())
            }
            Choice(_) | Flicker => {
                let choice = value
                    .as_str()
                    .filter(|_| {
                        spec["choices"]
                            .as_array()
                            .is_some_and(|choices| choices.contains(value))
                    })
                    .ok_or(Problem::Invalid)?;
                if matches!(self, Flicker) {
                    let enable = node
                        .parent()
                        .and_then(|p| xml::child(p, "enable"))
                        .ok_or(Problem::Unsupported)?;
                    scalar(edits, enable, if choice == "off" { "0" } else { "1" })?;
                    if choice == "off" {
                        return Ok(());
                    }
                }
                scalar(edits, node, choice)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_edits_preserve_unknown_fields_and_obey_reported_ranges() {
        let raw = "<body><VideoInput><channelId>0</channelId><bright scale=\"native\">128</bright><future>keep</future></VideoInput><InputAdvanceCfg><PowerLineFrequency><mode>50hz</mode><enable>0</enable></PowerLineFrequency><Scene><modeList>auto</modeList><mode>auto</mode><Redgain><cur>40</cur><min>10</min><max>50</max></Redgain></Scene></InputAdvanceCfg></body>";
        let doc = xml::document(raw).unwrap();
        let root = doc.root_element();
        let group = Group::named("picture").unwrap();
        let fields = |v: Value| v.as_object().unwrap().clone();
        assert!(group.describe(root, 1).is_err());
        assert!(group.edit(root, 0, &fields(json!({"redGain":51}))).is_err());
        assert!(
            group
                .edit(root, 0, &fields(json!({"whiteBalance":"manual"})))
                .is_err()
        );
        assert!(group.edit(root, 0, &fields(json!({"hue":42}))).is_err());
        let body = group
            .edit(
                root,
                0,
                &fields(json!({"brightness":129,"antiFlicker":"60hz"})),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            body,
            raw.replace(">128<", ">129<")
                .replace(">50hz<", ">60hz<")
                .replace("<enable>0", "<enable>1")
        );
    }
    #[test]
    fn a_single_stream_and_existing_privacy_regions_are_independent() {
        let raw = "<body><Compression><channelId>0</channelId><mainStream><audio>1</audio></mainStream><subStream><audio>1</audio></subStream></Compression></body>";
        let doc = xml::document(raw).unwrap();
        let group = Group::named("recordingAudio").unwrap();
        let fields = json!({"subAudio":false});
        assert_eq!(
            group
                .edit(doc.root_element(), 0, fields.as_object().unwrap())
                .unwrap()
                .unwrap(),
            raw.replace("<subStream><audio>1", "<subStream><audio>0")
        );
        let empty = "<body><Shelter><channelId>0</channelId><enable>0</enable><shelterList/></Shelter></body>";
        let doc = xml::document(empty).unwrap();
        assert!(
            Group::named("privacy")
                .unwrap()
                .edit(
                    doc.root_element(),
                    0,
                    json!({"maskEnabled":true}).as_object().unwrap()
                )
                .is_err()
        );
    }
}
