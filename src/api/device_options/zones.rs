use super::{Problem, Result};
use crate::xml;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use roxmltree::Node;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
pub(super) enum Zone {
    Motion,
    Ai {
        kind: &'static str,
        element: &'static str,
    },
}
impl Zone {
    pub fn named(group: &str) -> Option<Self> {
        let (kind, element) = match group {
            "motionZone" => return Some(Self::Motion),
            "personZone" => ("people", "personArea"),
            "vehicleZone" => ("vehicle", "vehicleArea"),
            "petZone" => ("dog_cat", "dogCatArea"),
            "faceZone" => ("face", "faceArea"),
            _ => return None,
        };
        Some(Self::Ai { kind, element })
    }
    pub fn commands(self) -> (u32, u32, &'static str) {
        match self {
            Self::Motion => (46, 47, "MD"),
            Self::Ai { .. } => (342, 345, "AiDetectCfg"),
        }
    }
    pub fn query(self, channel: u8) -> String {
        match self {
            Self::Motion => String::new(),
            Self::Ai { kind, .. } => format!(
                r#"<body><AiDetectCfg version="1.1"><chn>{channel}</chn><type>{kind}</type></AiDetectCfg></body>"#
            ),
        }
    }
    fn grid<'a, 'i>(self, root: Node<'a, 'i>, channel: u8) -> Result<Grid<'a, 'i>> {
        let section = xml::child(root, self.commands().2).ok_or(Problem::Unsupported)?;
        let (channel_tag, kind) = match self {
            Self::Motion => ("channelId", "motion"),
            Self::Ai { kind, .. } => {
                if xml::child(section, "type").and_then(xml::text) != Some(kind) {
                    return Err(Problem::Unsupported);
                }
                ("chn", kind)
            }
        };
        if number(section, channel_tag)? != usize::from(channel) {
            return Err(Problem::Unsupported);
        }
        let width = number(section, "width")?;
        let height = number(section, "height")?;
        let (stride, rows, node) = match self {
            Self::Motion => {
                let scope = xml::child(section, "scope").ok_or(Problem::Unsupported)?;
                (
                    number(scope, "columns")?,
                    number(scope, "rows")?,
                    xml::child(scope, "valueTable"),
                )
            }
            Self::Ai { .. } => (width, height, xml::child(section, "area")),
        };
        if width == 0
            || height == 0
            || !(width..=155).contains(&stride)
            || !(height..=100).contains(&rows)
        {
            return Err(Problem::Unsupported);
        }
        let node = node.ok_or(Problem::Unsupported)?;
        let encoded = xml::text(node).ok_or(Problem::Unsupported)?;
        let length = (stride * rows).div_ceil(8);
        if encoded.len() != length.div_ceil(3) * 4 {
            return Err(Problem::Unsupported);
        }
        let bytes = STANDARD.decode(encoded).map_err(|_| Problem::Unsupported)?;
        if bytes.len() != length {
            return Err(Problem::Unsupported);
        }
        let revision = format!(
            "{:x}",
            Sha256::digest(format!(
                "{channel}/{kind}/{width}/{height}/{stride}/{rows}/{encoded}"
            ))
        );
        Ok(Grid {
            width,
            height,
            stride,
            node,
            bytes,
            revision,
        })
    }
    pub fn describe(self, root: Node<'_, '_>, channel: u8) -> Result<Value> {
        let grid = self.grid(root, channel)?;
        Ok(
            json!({"fields":{},"zone":{"width":grid.width,"height":grid.height,"revision":grid.revision,"cells":grid.cells()}}),
        )
    }
    pub fn edit(
        self,
        root: Node<'_, '_>,
        channel: u8,
        fields: &Map<String, Value>,
    ) -> Result<Option<String>> {
        let mut grid = self.grid(root, channel)?;
        let revision = fields
            .get("expectedRevision")
            .and_then(Value::as_str)
            .ok_or(Problem::Invalid)?;
        let cells = fields
            .get("cells")
            .and_then(Value::as_str)
            .ok_or(Problem::Invalid)?;
        if fields.len() != 2
            || cells.len() != grid.width * grid.height
            || !cells.bytes().all(|v| v == b'0' || v == b'1')
        {
            return Err(Problem::Invalid);
        }
        if revision != grid.revision {
            return Err(Problem::ZoneChanged);
        }
        if cells == grid.cells() {
            return Ok(None);
        }
        for (i, value) in cells.bytes().enumerate() {
            let (index, bit) = grid.bit(i);
            if value == b'1' {
                grid.bytes[index] |= bit;
            } else {
                grid.bytes[index] &= !bit;
            }
        }
        let encoded = STANDARD.encode(&grid.bytes);
        Ok(Some(match self {
            Self::Motion => xml::replace_nodes(
                root,
                vec![(grid.node, format!("<valueTable>{encoded}</valueTable>"))],
            ),
            Self::Ai { element, .. } => format!(
                r#"<body><AlarmArea version="1.1"><chn>{channel}</chn><{element}><width>{}</width><height>{}</height><area>{encoded}</area></{element}></AlarmArea></body>"#,
                grid.width, grid.height
            ),
        }))
    }
}
fn number(node: Node<'_, '_>, tag: &str) -> Result<usize> {
    xml::child(node, tag)
        .and_then(xml::text)
        .and_then(|v| v.parse().ok())
        .ok_or(Problem::Unsupported)
}
struct Grid<'a, 'i> {
    width: usize,
    height: usize,
    stride: usize,
    node: Node<'a, 'i>,
    bytes: Vec<u8>,
    revision: String,
}
impl Grid<'_, '_> {
    // Baichuan packs rows most-significant bit first. The MD scope can include hidden padding.
    fn bit(&self, visible: usize) -> (usize, u8) {
        let offset = (visible / self.width) * self.stride + visible % self.width;
        (offset / 8, 0x80 >> (offset % 8))
    }
    fn cells(&self) -> String {
        (0..self.width * self.height)
            .map(|i| {
                let (index, bit) = self.bit(i);
                if self.bytes[index] & bit == 0 {
                    '0'
                } else {
                    '1'
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn motion_grid_keeps_padding_and_settings_and_rejects_stale_edits() {
        let raw = r#"<body><MD><channelId>0</channelId><width>3</width><height>2</height><enable>1</enable><scope><columns>8</columns><rows>3</rows><valueTable>peuB</valueTable></scope><sensInfoNew>keep</sensInfoNew></MD></body>"#;
        let doc = xml::document(raw).unwrap();
        let root = doc.root_element();
        let zone = Zone::Motion;
        let view = zone.describe(root, 0).unwrap();
        assert_eq!(view["zone"]["cells"], "101111");
        let mut change = json!({"expectedRevision":view["zone"]["revision"],"cells":"001110"});
        let result = zone
            .edit(root, 0, change.as_object().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            result,
            raw.replace("peuB", &STANDARD.encode([0x25, 0xcb, 0x81]))
        );
        let out = xml::document(&result).unwrap();
        assert!(matches!(
            zone.edit(out.root_element(), 0, change.as_object().unwrap()),
            Err(Problem::ZoneChanged)
        ));
        change["cells"] = json!("101111");
        assert!(
            zone.edit(root, 0, change.as_object().unwrap())
                .unwrap()
                .is_none()
        );
        change["cells"] = json!("010");
        assert!(matches!(
            zone.edit(root, 0, change.as_object().unwrap()),
            Err(Problem::Invalid)
        ));
        assert!(zone.describe(root, 1).is_err());
    }
    #[test]
    fn ai_zone_targets_one_class_and_keeps_partial_byte_padding() {
        let raw = r#"<body><AiDetectCfg><chn>1</chn><type>people</type><width>3</width><height>2</height><area>pQ==</area><sensitivity>30</sensitivity></AiDetectCfg></body>"#;
        let doc = xml::document(raw).unwrap();
        let root = doc.root_element();
        let zone = Zone::named("personZone").unwrap();
        let view = zone.describe(root, 1).unwrap();
        assert_eq!(view["zone"]["cells"], "101001");
        let changes = json!({"cells":"001000","expectedRevision":view["zone"]["revision"]});
        let write = zone
            .edit(root, 1, changes.as_object().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            write,
            r#"<body><AlarmArea version="1.1"><chn>1</chn><personArea><width>3</width><height>2</height><area>IQ==</area></personArea></AlarmArea></body>"#
        );
        assert!(Zone::named("petZone").unwrap().describe(root, 1).is_err());
    }
}
