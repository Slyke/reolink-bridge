use super::{Problem, Result};
use crate::xml;
use roxmltree::Node;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Area {
    id: u8,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}
impl Area {
    fn valid(&self) -> bool {
        self.id < 8
            && self.x < WIDTH
            && self.y < HEIGHT
            && self.width > 0
            && self.height > 0
            && self.width <= WIDTH - self.x
            && self.height <= HEIGHT - self.y
    }
}
fn parts<'a, 'b>(root: Node<'a, 'b>, channel: u8) -> Result<(Node<'a, 'b>, Node<'a, 'b>)> {
    let section = xml::child(root, "Shelter").ok_or(Problem::Unsupported)?;
    if xml::child(section, "channelId")
        .and_then(xml::text)
        .and_then(|s| s.parse().ok())
        != Some(channel)
    {
        return Err(Problem::Unsupported);
    }
    Ok((
        section,
        xml::child(section, "shelterList").ok_or(Problem::Unsupported)?,
    ))
}
fn number(node: Node<'_, '_>, tag: &str) -> Option<u32> {
    let s = xml::child(node, tag).and_then(xml::text)?;
    s.parse()
        .ok()
        .or_else(|| s.parse::<i32>().ok().map(|n| n as u32))
}
// The client packs the position in the upper 16 bits and the reference image dimension in the lower 16.
fn coordinate(node: Node<'_, '_>, tag: &str, scale: u32) -> Result<u32> {
    let value = number(node, tag).ok_or(Problem::Unsupported)?;
    let unit = value & 65535;
    let base = if unit == 0 { scale } else { unit };
    Ok(((value >> 16) * scale + base / 2) / base)
}
fn area(node: Node<'_, '_>, id: u8) -> Result<Area> {
    let mut r = Area {
        id,
        x: coordinate(node, "topLeftX", WIDTH)?,
        y: coordinate(node, "topLeftY", HEIGHT)?,
        width: coordinate(node, "width", WIDTH)?,
        height: coordinate(node, "height", HEIGHT)?,
    };
    // Independent rounding of an edge and its width may be one unit over at the image border.
    if r.x.checked_add(r.width) == Some(WIDTH + 1) {
        r.width -= 1;
    }
    if r.y.checked_add(r.height) == Some(HEIGHT + 1) {
        r.height -= 1;
    }
    r.valid().then_some(r).ok_or(Problem::Unsupported)
}
fn slot(node: Node<'_, '_>) -> Result<u8> {
    number(node, "id")
        .filter(|v| *v < 8)
        .map(|v| v as u8)
        .ok_or(Problem::Unsupported)
}
fn active(node: Node<'_, '_>) -> bool {
    number(node, "enable") != Some(0)
}
fn areas(list: Node<'_, '_>) -> Result<Vec<Area>> {
    let mut values = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for node in list.children().filter(Node::is_element) {
        if !node.has_tag_name("Shelter") {
            return Err(Problem::Unsupported);
        }
        let id = slot(node)?;
        if !ids.insert(id) {
            return Err(Problem::Unsupported);
        }
        if active(node) {
            values.insert(id, area(node, id)?);
        }
    }
    Ok(values.into_values().collect())
}
pub(super) fn describe(root: Node<'_, '_>, channel: u8) -> Result<Value> {
    let (section, list) = parts(root, channel)?;
    let areas = areas(list)?;
    let enabled = number(section, "enable")
        .filter(|v| *v <= 1)
        .ok_or(Problem::Unsupported)?
        == 1;
    let max = number(section, "maxNum")
        .filter(|n| (1..=8).contains(n))
        .unwrap_or(4)
        .max(areas.len() as u32);
    let revision = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(enabled, &areas)).map_err(|_| Problem::Read)?)
    );
    Ok(json!({"areas":areas,"maxAreas":max,"revision":revision}))
}
fn element(name: &str, n: u32) -> String {
    format!("<{name}>{n}</{name}>")
}
fn geometry(a: Area) -> [(&'static str, u32); 4] {
    [
        ("topLeftX", (a.x << 16) | WIDTH),
        ("topLeftY", (a.y << 16) | HEIGHT),
        ("width", (a.width << 16) | WIDTH),
        ("height", (a.height << 16) | HEIGHT),
    ]
}
fn area_xml(a: Area) -> String {
    let geometry = geometry(a).map(|(k, n)| element(k, n)).join("");
    format!(
        "<Shelter><id>{}</id><enable>1</enable><layer>0</layer><color>0</color>{geometry}</Shelter>",
        a.id
    )
}
fn update_area(node: Node<'_, '_>, a: Area) -> Result<String> {
    let mut edits = Vec::new();
    for (tag, value) in geometry(a) {
        let child = xml::child(node, tag).ok_or(Problem::Unsupported)?;
        edits.push((child, element(tag, value)));
    }
    if let Some(child) = xml::child(node, "enable") {
        edits.push((child, element("enable", 1)));
    }
    Ok(xml::replace_nodes(node, edits))
}
pub(super) fn edit(
    root: Node<'_, '_>,
    channel: u8,
    fields: &Map<String, Value>,
) -> Result<Option<String>> {
    if fields.len() != 3 {
        return Err(Problem::Invalid);
    }
    let original = describe(root, channel)?;
    if fields.get("expectedRevision") != original.get("revision") {
        return Err(Problem::Changed);
    }
    let enabled = fields
        .get("maskEnabled")
        .and_then(Value::as_bool)
        .ok_or(Problem::Invalid)?;
    let input: Vec<Area> =
        serde_json::from_value(fields.get("maskAreas").ok_or(Problem::Invalid)?.clone())
            .map_err(|_| Problem::Invalid)?;
    if input.len() > original["maxAreas"].as_u64().unwrap_or(0) as usize
        || input.iter().any(|a| !a.valid())
        || (input.is_empty() && enabled)
    {
        return Err(Problem::Invalid);
    }
    let mut wanted: BTreeMap<_, _> = input.iter().map(|a| (a.id, *a)).collect();
    if wanted.len() != input.len() {
        return Err(Problem::Invalid);
    }
    let (section, list) = parts(root, channel)?;
    let mut edits = Vec::new();
    let flag = xml::child(section, "enable").ok_or(Problem::Unsupported)?;
    if number(section, "enable") != Some(u32::from(enabled)) {
        edits.push((flag, element("enable", enabled.into())));
    }
    let mut nodes = Vec::new();
    for node in list.children().filter(Node::is_element) {
        let id = slot(node)?;
        match wanted.remove(&id) {
            Some(next) if !active(node) || area(node, id)? != next => {
                nodes.push((node, update_area(node, next)?))
            }
            None if active(node) => nodes.push((node, String::new())),
            _ => {}
        }
    }
    if !nodes.is_empty() || !wanted.is_empty() {
        let mut xml = xml::replace_nodes(list, nodes);
        if !wanted.is_empty() {
            let added = wanted.into_values().map(area_xml).collect::<String>();
            if xml.ends_with("/>") {
                xml.truncate(xml.len() - 2);
                xml.push('>');
                xml.push_str(&added);
                xml.push_str("</shelterList>");
            } else {
                let end = xml.rfind("</").ok_or(Problem::Unsupported)?;
                xml.insert_str(end, &added);
            }
        }
        edits.push((list, xml));
    }
    Ok((!edits.is_empty()).then(|| xml::replace_nodes(root, edits)))
}
pub(super) fn verified(result: &Value, changes: &Map<String, Value>) -> bool {
    let Some(actual) = result["privacyMasks"]["areas"].as_array() else {
        return false;
    };
    let Some(wanted) = changes["maskAreas"].as_array() else {
        return false;
    };
    result["fields"]["maskEnabled"]["value"] == changes["maskEnabled"]
        && actual.len() == wanted.len()
        && wanted.iter().all(|a| actual.contains(a))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn draws_on_empty_list_preserves_unknown_settings_and_detects_concurrent_edits() {
        let source = r#"<body><Shelter version="1.1"><channelId>0</channelId><enable>0</enable><shelterList/><unchanged>3</unchanged></Shelter></body>"#;
        let doc = xml::document(source).unwrap();
        let root = doc.root_element();
        let rectangle = json!({"id":0,"x":512,"y":192,"width":300,"height":200});
        let fields = json!({"maskEnabled":true,"expectedRevision":describe(root,0).unwrap()["revision"],"maskAreas":[rectangle]});
        let raw = edit(root, 0, fields.as_object().unwrap()).unwrap().unwrap();
        assert!(
            raw.contains(&element("topLeftX", (512 << 16) | 1024))
                && raw.contains("<unchanged>3</unchanged>")
        );
        let saved = xml::document(&raw).unwrap();
        let saved = saved.root_element();
        let view = describe(saved, 0).unwrap();
        assert_eq!(view["areas"], json!([rectangle]));
        assert!(matches!(
            edit(saved, 0, fields.as_object().unwrap()),
            Err(Problem::Changed)
        ));
        let clear = json!({"maskEnabled":false,"expectedRevision":view["revision"],"maskAreas":[]});
        let cleared = edit(saved, 0, clear.as_object().unwrap()).unwrap().unwrap();
        assert!(cleared.contains("<shelterList></shelterList>"));
        assert!(describe(root, 1).is_err());
    }
}
