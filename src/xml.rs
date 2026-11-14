use anyhow::{Result, anyhow};
use roxmltree::{Document, Node, ParsingOptions};
use serde_json::{Map, Value};

pub const BODY_LIMIT: usize = 512 * 1024;

pub fn document(input: &str) -> Result<Document<'_>> {
    Document::parse_with_options(
        input.trim_start_matches('\u{feff}').trim(),
        ParsingOptions {
            nodes_limit: 24_000,
            ..ParsingOptions::default()
        },
    )
    .map_err(|_| anyhow!("invalid XML response"))
}

pub fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children().find(|n| n.has_tag_name(name))
}

pub fn find<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.descendants().find(|n| n.has_tag_name(name))
}

pub fn text<'a>(node: Node<'a, '_>) -> Option<&'a str> {
    node.text().map(str::trim).filter(|value| !value.is_empty())
}

pub fn value<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    find(node, name).and_then(text)
}

pub fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn sensitive(name: &str) -> bool {
    let key: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    key.contains("password")
        || key.contains("passwd")
        || key.contains("passphrase")
        || key.contains("secret")
        || key.contains("credential")
        || key.contains("token")
        || key.contains("nonce")
        || key.contains("apikey")
        || key.contains("auth")
        || key.contains("cookie")
        || key.contains("sessionid")
        || matches!(
            key.as_str(),
            "username" | "loginuser" | "url" | "uri" | "xaddr" | "pwd" | "psk" | "pin"
        )
}

pub fn tree(node: Node<'_, '_>) -> Value {
    node_value(node, 0).unwrap_or(Value::Null)
}

fn node_value(node: Node<'_, '_>, depth: usize) -> Option<Value> {
    let name = node.tag_name().name();
    if depth > 24 || sensitive(name) || node.attribute("Name").is_some_and(sensitive) {
        return None;
    }
    let mut fields = Map::new();
    fields.insert("name".into(), name.into());
    if let Some(namespace) = node.tag_name().namespace() {
        fields.insert("namespace".into(), namespace.into());
    }
    let attributes: Map<_, _> = node
        .attributes()
        .filter(|attr| !sensitive(attr.name()))
        .map(|attr| (attr.name().to_owned(), attr.value().into()))
        .collect();
    if !attributes.is_empty() {
        fields.insert("attributes".into(), Value::Object(attributes));
    }
    if let Some(text) = node
        .children()
        .filter(Node::is_text)
        .filter_map(|n| n.text())
        .find(|value| !value.trim().is_empty())
    {
        fields.insert("text".into(), text.trim().into());
    }
    let children: Vec<_> = node
        .children()
        .filter(Node::is_element)
        .filter_map(|child| node_value(child, depth + 1))
        .collect();
    if !children.is_empty() {
        fields.insert("children".into(), children.into());
    }
    Some(Value::Object(fields))
}

// Keep credentials echoed by vendor extensions out of outward-facing camera data.
pub fn redact(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            if text.split_whitespace().any(|part| {
                if !part.contains("://") {
                    return false;
                }
                url::Url::parse(part).ok().is_some_and(|url| {
                    !url.username().is_empty()
                        || url.password().is_some()
                        || url.query_pairs().any(|(key, _)| sensitive(&key))
                })
            }) || secrets
                .iter()
                .any(|secret| text == secret || (secret.len() >= 4 && text.contains(secret)))
            {
                *text = "[redacted]".into();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redact(item, secrets)),
        Value::Object(fields) => fields.values_mut().for_each(|item| redact(item, secrets)),
        _ => {}
    }
}

pub fn strip_sensitive_fields(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(strip_sensitive_fields),
        Value::Object(fields) => {
            fields.retain(|key, _| !sensitive(key));
            fields.values_mut().for_each(strip_sensitive_fields);
        }
        _ => {}
    }
}
