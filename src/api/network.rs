use super::*;
use crate::reolink::wire::{Rejected, Session};
use crate::xml;
use std::net::IpAddr;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WifiChange {
    ssid: String,
    password: String,
    open: bool,
    confirmation: bool,
}

fn valid_ssid(name: &str) -> bool {
    (1..=32).contains(&name.len()) && !name.chars().any(char::is_control)
}

impl WifiChange {
    fn valid(&self) -> bool {
        self.confirmation
            && valid_ssid(&self.ssid)
            && if self.open {
                self.password.is_empty()
            } else {
                ((8..=63).contains(&self.password.len())
                    && !self.password.chars().any(char::is_control))
                    || (self.password.len() == 64
                        && self.password.bytes().all(|b| b.is_ascii_hexdigit()))
            }
    }

    fn body(&self) -> String {
        let (auth, encryption) = if self.open {
            ("open", "none")
        } else {
            ("detect", "detect")
        };
        format!(
            r#"<body><Wifi version="1.1"><mode>station</mode><authMode>{auth}</authMode><encryptType>{encryption}</encryptType><ssid>{}</ssid><key>{}</key></Wifi></body>"#,
            xml::escape(&self.ssid),
            xml::escape(&self.password)
        )
    }
}

fn field<'a>(root: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    xml::child(root, name).map(|node| node.text().unwrap_or(""))
}

fn wifi(raw: &str) -> Result<Value> {
    let doc = xml::document(raw)?;
    let root = xml::find(doc.root_element(), "Wifi")
        .ok_or_else(|| anyhow::anyhow!("missing Wi-Fi settings"))?;
    // Expose only the configured network name and names of nearby access points.
    // The device may also return both the current and fallback keys in this document.
    let nearby: BTreeSet<_> = xml::child(root, "udidList")
        .into_iter()
        .flat_map(|list| list.children())
        .filter(|n| n.has_tag_name("udid"))
        .filter_map(|n| field(n, "name"))
        .filter(|s| valid_ssid(s))
        .take(128)
        .collect();
    let ssid = field(root, "ssid").filter(|s| valid_ssid(s));
    Ok(
        json!({"ssid":ssid,"nearby":nearby,"configurable":field(root,"ssid").is_some() && field(root,"mode")!=Some("ap")}),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Readback {
    Settings,
    NetworkName,
    Unknown,
}

fn readback(raw: &str, change: &WifiChange) -> Readback {
    let Ok(doc) = xml::document(raw) else {
        return Readback::Unknown;
    };
    let Some(root) = xml::find(doc.root_element(), "Wifi") else {
        return Readback::Unknown;
    };
    if field(root, "ssid") != Some(&change.ssid) {
        return Readback::Unknown;
    }
    // Some firmware always suppresses the saved key, even on an authenticated read.
    let verified = if change.open {
        field(root, "authMode") == Some("open") && field(root, "encryptType") == Some("none")
    } else {
        field(root, "key") == Some(&change.password) && !change.password.bytes().all(|b| b == b'*')
    };
    if verified {
        Readback::Settings
    } else {
        Readback::NetworkName
    }
}

fn link(raw: &str) -> Result<Value> {
    let doc = xml::document(raw)?;
    let root = doc.root_element();
    let ip = xml::find(root, "Ip");
    let address = |name| {
        ip.and_then(|n| field(n, name))
            .and_then(|v| v.parse::<IpAddr>().ok())
    };
    Ok(
        json!({"address":address("ip"),"netmask":address("mask"),"gateway":address("gateway"),
        "dhcp":xml::find(root,"Dhcp").and_then(|n|field(n,"enable")).and_then(crate::model::boolean)}),
    )
}

async fn read_part(
    session: &mut Session,
    command: u32,
    parse: fn(&str) -> Result<Value>,
) -> Result<Value> {
    match session.command(command, None, "").await {
        Ok(raw) => parse(&raw),
        Err(e) if e.is::<Rejected>() => Ok(Value::Null),
        Err(e) => Err(e),
    }
}

pub(super) async fn get(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Some(device) = api.devices.get(&id) else {
        return error(StatusCode::NOT_FOUND, "API_NETWORK_UNSUPPORTED", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let read = async {
        let mut session = device.connect().await?;
        let wifi = read_part(&mut session, 116, wifi).await?;
        let link = read_part(&mut session, 76, link).await?;
        session.logout().await;
        Ok::<_, anyhow::Error>(json!({"wifi":wifi,"link":link}))
    };
    match timeout(Duration::from_secs(18), read).await {
        Ok(Ok(state)) => Json(state).into_response(),
        _ => error(StatusCode::BAD_GATEWAY, "API_NETWORK_READ_FAILED", &cid),
    }
}

pub(super) async fn set(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    body: Result<Json<WifiChange>, JsonRejection>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Ok(Json(change)) = body else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_NETWORK_INVALID",
            &cid,
        );
    };
    if !change.valid() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_NETWORK_INVALID",
            &cid,
        );
    }
    let Some(device) = api.devices.get(&id) else {
        return error(StatusCode::NOT_FOUND, "API_NETWORK_UNSUPPORTED", &cid);
    };
    let Ok(_guard) = api.network_writes[&id].try_lock() else {
        return error(StatusCode::CONFLICT, "API_NETWORK_BUSY", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let Ok(mut session) = device.connect().await else {
        return error(StatusCode::BAD_GATEWAY, "API_NETWORK_READ_FAILED", &cid);
    };
    match read_part(&mut session, 116, wifi).await {
        Ok(state) if state["configurable"] == true => (),
        Ok(_) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "API_NETWORK_UNSUPPORTED",
                &cid,
            );
        }
        Err(_) => return error(StatusCode::BAD_GATEWAY, "API_NETWORK_READ_FAILED", &cid),
    }
    // After dispatch the wireless interface may disconnect. Never automatically retry a write.
    match session.command(117, None, &change.body()).await {
        Err(e) if e.is::<Rejected>() => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_NETWORK_REJECTED",
            &cid,
        ),
        Err(_) => error(StatusCode::BAD_GATEWAY, "API_NETWORK_OUTCOME_UNKNOWN", &cid),
        Ok(_) => {
            let checked = session
                .command(116, None, "")
                .await
                .map_or(Readback::Unknown, |raw| readback(&raw, &change));
            let verified = checked == Readback::Settings;
            session.logout().await;
            if !verified {
                api.hub().diagnostics.error(
                    "api::network",
                    "API_NETWORK_UNVERIFIED",
                    Some(cid.0.clone()),
                    json!({"cameraId":id}),
                    None,
                );
            }
            Json(json!({"outcome":if verified {"confirmed"}else{"unverified"},"ssidVerified":checked != Readback::Unknown,"correlation_id":cid.0})).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wifi_credentials_are_write_only_and_xml_round_trips() {
        let change = WifiChange {
            ssid: " café & <lab> ".into(),
            password: "a&b<cd>123".into(),
            open: false,
            confirmation: true,
        };
        assert!(change.valid());
        assert_eq!(readback(&change.body(), &change), Readback::Settings);
        assert_eq!(
            readback(&change.body().replace("a&amp;b&lt;cd&gt;123", ""), &change),
            Readback::NetworkName
        );
        let read=wifi(r#"<body><Wifi><ssid>Test</ssid><key>hidden original</key><staticKey>hidden fallback</staticKey><udidList><udid><name>Other</name><key>hidden AP</key></udid></udidList></Wifi></body>"#).unwrap();
        assert_eq!(
            read,
            json!({"ssid":"Test","nearby":["Other"],"configurable":true})
        );
        assert_eq!(
            readback(
                "<body><Wifi><ssid>Test</ssid><key>********</key></Wifi></body>",
                &change
            ),
            Readback::Unknown
        );
        assert!(!valid_ssid(&"é".repeat(17)));
    }
}
