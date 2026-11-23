use crate::xml;
use roxmltree::Node;
use serde_json::{Value, json};

const MODES: &[(&str, &str)] = &[
    ("auto", "auto"),
    ("lownoise", "gainFist"),
    ("anti-smearing", "shutterFirst"),
    ("manual", "manual"),
];

fn mode(value: &str) -> Option<(&'static str, &'static str)> {
    MODES.iter().copied().find(|(name, native)| {
        value.eq_ignore_ascii_case(name)
            || value.eq_ignore_ascii_case(native)
            || (*name == "lownoise" && value.eq_ignore_ascii_case("gainFirst"))
    })
}

fn advertised<'a>(node: Node<'a, '_>) -> Option<&'a str> {
    xml::child(node.parent()?, "modeList").and_then(xml::text)
}

pub(super) fn describe(node: Node<'_, '_>) -> Option<Value> {
    let (value, _) = mode(xml::text(node)?)?;
    let listed = advertised(node);
    let choices: Vec<_> = MODES
        .iter()
        .map(|&(name, _)| name)
        .filter(|&name| {
            listed.is_none_or(|list| {
                list.split(',')
                    .any(|s| mode(s.trim()).is_some_and(|m| m.0 == name))
            })
        })
        .collect();
    choices
        .contains(&value)
        .then(|| json!({"value":value,"choices":choices}))
}

pub(super) fn wire_value<'a>(node: Node<'a, '_>, name: &str, legacy: bool) -> Option<&'a str> {
    let (api, native) = MODES.iter().copied().find(|m| m.0 == name)?;
    if let Some(list) = advertised(node) {
        return list
            .split(',')
            .map(str::trim)
            .find(|s| mode(s).is_some_and(|m| m.0 == api));
    }
    let current = xml::text(node)?;
    let named_mode = ["lownoise", "anti-smearing"]
        .iter()
        .any(|v| current.eq_ignore_ascii_case(v));
    Some(if legacy || named_mode { api } else { native })
}

pub(super) fn ambiguous(node: Node<'_, '_>, requested: &str) -> bool {
    matches!(requested, "lownoise" | "anti-smearing")
        && advertised(node).is_none()
        && xml::text(node)
            .and_then(mode)
            .is_some_and(|(name, _)| matches!(name, "auto" | "manual"))
}
