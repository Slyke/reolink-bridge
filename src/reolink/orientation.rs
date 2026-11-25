use anyhow::{Result, anyhow, ensure};
use roxmltree::Node;
use serde::Deserialize;
use serde_json::{Value, json};

use super::wire::Session;
use crate::{model::boolean, xml};

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Hflip,
    Vflip,
    Clockwise,
    Counterclockwise,
}

fn leaf<'a, 'b>(root: Node<'a, 'b>, names: &[&str]) -> Option<Node<'a, 'b>> {
    root.descendants()
        .filter(|node| node.is_element())
        .find_map(|node| {
            if !names
                .iter()
                .any(|name| node.tag_name().name().eq_ignore_ascii_case(name))
            {
                return None;
            }
            if !node.children().any(|n| n.is_element()) {
                return Some(node);
            }
            ["enable", "state", "angle", "value"]
                .into_iter()
                .find_map(|field| xml::child(node, field))
        })
}

fn angle_list(root: Node<'_, '_>, node: Node<'_, '_>) -> Vec<u16> {
    let range = root.descendants().filter(|n| n.is_element()).find(|n| {
        matches!(
            n.tag_name().name(),
            "rotationRange" | "RotateRange" | "rotationOptions" | "RotationOptions"
        )
    });
    let mut values: Vec<u16> = range
        .into_iter()
        .flat_map(|n| n.descendants())
        .filter(|n| n.is_element())
        .filter_map(xml::text)
        .filter_map(|s| s.parse().ok())
        .filter(|n| [0, 90, 180, 270].contains(n))
        .collect();
    if node
        .attribute("max")
        .and_then(|s| s.parse::<u16>().ok())
        .is_some_and(|n| n >= 270)
    {
        values = vec![0, 90, 180, 270];
    }
    values.sort_unstable();
    values.dedup();
    values
}

fn fields<'a, 'b>(
    root: Node<'a, 'b>,
) -> (
    Option<Node<'a, 'b>>,
    Option<Node<'a, 'b>>,
    Option<Node<'a, 'b>>,
) {
    (
        leaf(root, &["mirror", "mirroring", "hflip"]),
        leaf(root, &["flip", "vflip"]),
        leaf(root, &["rotate", "rotation"]),
    )
}

pub fn status(root: Node<'_, '_>) -> (Value, Value) {
    let (horizontal, vertical, rotation) = fields(root);
    let hflip = horizontal.and_then(xml::text).and_then(boolean);
    let vflip = vertical.and_then(xml::text).and_then(boolean);
    let angles = rotation.map(|n| angle_list(root, n)).unwrap_or_default();
    let current = rotation
        .and_then(xml::text)
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|n| angles.contains(n));
    (
        json!({"hflip":hflip,"vflip":vflip,"rotation":current}),
        json!({"hflip":hflip.is_some(),"vflip":vflip.is_some(),"rotation":current.is_some() && angles==[0,90,180,270],"angles":angles}),
    )
}

pub async fn inspect(session: &mut Session, channel: u8) -> Result<(Value, Value)> {
    let raw = session.command(26, Some(channel), "").await?;
    let doc = xml::document(&raw)?;
    let root = doc.root_element();
    let returned = xml::value(root, "channelId")
        .or_else(|| xml::value(root, "channel"))
        .ok_or_else(|| anyhow!("missing orientation channel"))?
        .parse::<u8>()?;
    ensure!(returned == channel, "wrong orientation channel");
    let (state, caps) = status(root);
    ensure!(
        caps["hflip"] == true || caps["vflip"] == true || caps["rotation"] == true,
        "camera reported no orientation controls"
    );
    Ok((state, caps))
}

pub async fn apply(
    session: &mut Session,
    channel: u8,
    action: Action,
) -> Result<(&'static str, Value, bool)> {
    let raw = session.command(26, Some(channel), "").await?;
    let doc = xml::document(&raw)?;
    let root = doc.root_element();
    ensure!(
        xml::value(root, "channelId")
            .or_else(|| xml::value(root, "channel"))
            .and_then(|s| s.parse::<u8>().ok())
            == Some(channel),
        "orientation channel mismatch"
    );
    let (horizontal, vertical, rotation) = fields(root);
    let (node, next) = match action {
        Action::Hflip | Action::Vflip => {
            let node = if action == Action::Hflip {
                horizontal
            } else {
                vertical
            }
            .ok_or_else(|| anyhow!("camera orientation field unsupported"))?;
            let current = xml::text(node).ok_or_else(|| anyhow!("orientation unavailable"))?;
            let enabled =
                !boolean(current).ok_or_else(|| anyhow!("orientation field is not boolean"))?;
            let next = match current {
                "true" | "false" => enabled.to_string(),
                "open" | "close" => if enabled { "open" } else { "close" }.into(),
                "on" | "off" => if enabled { "on" } else { "off" }.into(),
                _ => u8::from(enabled).to_string(),
            };
            (node, next)
        }
        Action::Clockwise | Action::Counterclockwise => {
            let node = rotation.ok_or_else(|| anyhow!("camera rotation unsupported"))?;
            ensure!(
                angle_list(root, node) == [0, 90, 180, 270],
                "quarter-turn rotation unsupported"
            );
            let current = xml::text(node)
                .and_then(|s| s.parse::<u16>().ok())
                .filter(|n| [0, 90, 180, 270].contains(n))
                .ok_or_else(|| anyhow!("invalid camera rotation"))?;
            let next = (current + if action == Action::Clockwise { 90 } else { 270 }) % 360;
            (node, next.to_string())
        }
    };
    let text = node
        .children()
        .find(Node::is_text)
        .ok_or_else(|| anyhow!("orientation field is empty"))?;
    let root_range = root.range();
    let mut body = raw[root_range.clone()].to_owned();
    let range = text.range();
    body.replace_range(
        range.start - root_range.start..range.end - root_range.start,
        &xml::escape(&next),
    );
    session.command(25, Some(channel), &body).await?;
    let readback = session.command(26, Some(channel), "").await?;
    let returned = xml::document(&readback)?;
    let returned_root = returned.root_element();
    ensure!(
        xml::value(returned_root, "channelId")
            .or_else(|| xml::value(returned_root, "channel"))
            .and_then(|s| s.parse::<u8>().ok())
            == Some(channel),
        "orientation readback channel mismatch"
    );
    let state = status(returned_root).0;
    let path: Vec<_> = node
        .ancestors()
        .filter(|n| n.is_element())
        .map(|n| n.tag_name().name())
        .collect();
    let found = path[..path.len() - 1]
        .iter()
        .rev()
        .try_fold(returned.root_element(), |node, tag| xml::child(node, tag));
    let verified = found.and_then(xml::text) == Some(next.as_str());
    Ok(("image", state, verified))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_quarter_turns_stay_unavailable() {
        let doc=xml::document("<body><VideoInput><channelId>0</channelId></VideoInput><InputAdvanceCfg><Mirror><enable>1</enable></Mirror><Flip><enable>0</enable></Flip><rotation>0</rotation></InputAdvanceCfg></body>").unwrap();
        let (state, caps) = status(doc.root_element());
        assert_eq!(state["hflip"], true);
        assert_eq!(state["vflip"], false);
        assert_eq!(caps["rotation"], false);
    }
}
