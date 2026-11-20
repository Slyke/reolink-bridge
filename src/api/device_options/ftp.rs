use super::{Problem, Result};
use crate::xml;
use roxmltree::Node;
use serde_json::{Map, Value, json};

const FIELDS: &[(&str, &str)] = &[
    ("server", "server"),
    ("port", "port"),
    ("username", "userName"),
    ("remoteDirectory", "remoteDir"),
    ("anonymous", "nonymous"),
    ("ftpsOnly", "onlyFtps"),
];

fn section<'a, 'b>(root: Node<'a, 'b>, channel: u8) -> Result<Node<'a, 'b>> {
    let ftp = xml::child(root, "Ftp").ok_or(Problem::Unsupported)?;
    if xml::child(ftp, "channelId")
        .is_some_and(|n| xml::text(n).and_then(|v| v.parse().ok()) != Some(channel))
        || xml::child(ftp, "server").is_none()
        || xml::child(ftp, "port").is_none()
    {
        return Err(Problem::Unsupported);
    }
    Ok(ftp)
}

fn password_limit(ftp: Node<'_, '_>) -> usize {
    xml::child(ftp, "pwdMaxLen")
        .and_then(xml::text)
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(127)
        .min(255)
}
fn masked(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b == b'*')
}
fn valid_host(value: &str) -> bool {
    if value.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    !value.is_empty()
        && value.len() <= 253
        && value.trim_end_matches('.').split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
pub(super) fn describe(root: Node<'_, '_>, channel: u8) -> Result<Value> {
    let ftp = section(root, channel)?;
    let mut fields = Map::new();
    for &(key, tag) in FIELDS {
        let Some(node) = xml::child(ftp, tag) else {
            continue;
        };
        let raw = node.text().unwrap_or("");
        let value = match key {
            "port" => json!(raw.trim().parse::<u16>().map_err(|_| Problem::Read)?),
            "anonymous" | "ftpsOnly" => match raw.trim() {
                "1" => json!(true),
                "0" => json!(false),
                _ => continue,
            },
            _ => json!(raw),
        };
        fields.insert(key.into(), value);
    }
    let mut result = json!({"fields":fields});
    if let Some(secret) = xml::child(ftp, "password") {
        result["passwordStored"] = json!(secret.text().is_some_and(|s| !s.is_empty()));
        result["passwordMaxLength"] = json!(password_limit(ftp));
    }
    Ok(result)
}

pub(super) fn edit(
    root: Node<'_, '_>,
    channel: u8,
    changes: &Map<String, Value>,
) -> Result<Option<String>> {
    if changes.is_empty() {
        return Err(Problem::Invalid);
    }
    let ftp = section(root, channel)?;
    let current = describe(root, channel)?;
    let destination_changed = ["server", "port"].iter().any(|key| {
        changes
            .get(*key)
            .is_some_and(|v| v != &current["fields"][key])
    });
    if destination_changed && current["passwordStored"] == true && !changes.contains_key("password")
    {
        return Err(Problem::Invalid);
    }
    let mut edits = Vec::new();
    for (key, value) in changes {
        let tag = if key == "password" {
            "password"
        } else {
            FIELDS
                .iter()
                .find(|(name, _)| *name == key)
                .ok_or(Problem::Invalid)?
                .1
        };
        let node = xml::child(ftp, tag).ok_or(Problem::Unsupported)?;
        let raw = match key.as_str() {
            "port" => value
                .as_u64()
                .filter(|v| (1..=65535).contains(v))
                .ok_or(Problem::Invalid)?
                .to_string(),
            "anonymous" | "ftpsOnly" if current["fields"].get(key).is_some() => {
                if value.as_bool().ok_or(Problem::Invalid)? {
                    "1"
                } else {
                    "0"
                }
                .into()
            }
            "server" | "username" | "remoteDirectory" | "password" => {
                let s = value.as_str().ok_or(Problem::Invalid)?;
                let max = if key == "password" {
                    password_limit(ftp)
                } else {
                    255
                };
                if s.len() > max
                    || s.chars().any(char::is_control)
                    || (key == "server" && !valid_host(s))
                {
                    return Err(Problem::Invalid);
                }
                s.into()
            }
            _ => return Err(Problem::Unsupported),
        };
        if (key == "password" && masked(node.text().unwrap_or("")))
            || node.text().unwrap_or("") != raw
        {
            edits.push((node, format!("<{tag}>{}</{tag}>", xml::escape(&raw))));
        }
    }
    if edits.is_empty() {
        return Ok(None);
    }
    for tag in ["pwdMaxLen", "supportTest"] {
        if let Some(node) = xml::child(ftp, tag) {
            edits.push((node, String::new()));
        }
    }
    if !changes.contains_key("password")
        && let Some(node) = xml::child(ftp, "password")
        && masked(node.text().unwrap_or(""))
    {
        edits.push((node, String::new()));
    }
    Ok(Some(xml::replace_nodes(root, edits)))
}

/// Some firmware returns only a password mask. Confirm an updated secret only when readback permits it.
pub(super) fn verify(
    raw: &str,
    channel: u8,
    result: &Value,
    changes: &Map<String, Value>,
) -> Result<bool> {
    if changes
        .iter()
        .any(|(key, v)| key != "password" && result["fields"].get(key) != Some(v))
    {
        return Err(Problem::Unverified);
    }
    let Some(password) = changes.get("password").and_then(Value::as_str) else {
        return Ok(true);
    };
    let doc = xml::document(raw).map_err(|_| Problem::Unverified)?;
    let ftp = section(doc.root_element(), channel)?;
    let seen = xml::child(ftp, "password")
        .ok_or(Problem::Unverified)?
        .text()
        .unwrap_or("");
    if masked(seen) || (!password.is_empty() && seen.is_empty()) {
        return Ok(false);
    }
    if seen != password {
        return Err(Problem::Unverified);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    const RAW: &str = r#"<body><Ftp version="1.1"><server/><port>21</port><userName/><password>******</password><pwdMaxLen>16</pwdMaxLen><supportTest>1</supportTest><remoteDir/><onlyFtps>0</onlyFtps><nonymous>0</nonymous><mode>2</mode><unknownSecret>hidden</unknownSecret></Ftp></body>"#;
    #[test]
    fn ftp_discloses_only_public_fields_and_preserves_upload_policy() {
        let doc = xml::document(RAW).unwrap();
        let root = doc.root_element();
        let view = describe(root, 0).unwrap();
        assert_eq!(
            view,
            json!({"fields":{"server":"","port":21,"username":"","remoteDirectory":"","anonymous":false,"ftpsOnly":false},"passwordStored":true,"passwordMaxLength":16})
        );
        let changes = json!({"username":"alice","remoteDirectory":"A & B","ftpsOnly":true});
        let body = edit(root, 0, changes.as_object().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            body.contains("<remoteDir>A &amp; B</remoteDir>")
                && body.contains("<mode>2</mode>")
                && body.contains("<unknownSecret>hidden</unknownSecret>")
        );
        assert!(
            !body.contains("password")
                && !body.contains("pwdMaxLen")
                && !body.contains("supportTest")
        );
        for invalid in [
            json!({"port":0}),
            json!({"server":"files.test"}),
            json!({"server":"ftp://host","password":""}),
            json!({"password":"too-long-for-this-camera"}),
            json!({"unknownSecret":"change"}),
        ] {
            assert!(edit(root, 0, invalid.as_object().unwrap()).is_err());
        }
        let changes = json!({"server":"files.test","password":"p<&"});
        let body = edit(root, 0, changes.as_object().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            body.contains("<server>files.test</server>")
                && body.contains("<password>p&lt;&amp;</password>")
        );
        let changes = json!({"password":"new"});
        assert!(!verify(RAW, 0, &view, changes.as_object().unwrap()).unwrap());
        assert!(
            verify(
                &RAW.replace("******", "new"),
                0,
                &view,
                changes.as_object().unwrap()
            )
            .unwrap()
        );
        let wrong = RAW.replace("<server/>", "<channelId>1</channelId><server/>");
        assert!(describe(xml::document(&wrong).unwrap().root_element(), 0).is_err());
    }
}
