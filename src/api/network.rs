use super::*;
use crate::onvif::network::{Interface, Kind};
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
        Ok(raw) if raw.trim().is_empty() => Ok(Value::Null),
        Ok(raw) => parse(&raw),
        Err(e) if e.is::<Rejected>() => Ok(Value::Null),
        Err(e) => Err(e),
    }
}

fn link_type(raw: &str) -> Result<Value> {
    let doc = xml::document(raw)?;
    Ok(json!(
        xml::find(doc.root_element(), "LinkType").and_then(|n| field(n, "type"))
    ))
}

fn signal(raw: &str) -> Result<Value> {
    let doc = xml::document(raw)?;
    let strength = xml::find(doc.root_element(), "WifiSignal")
        .and_then(|n| field(n, "signal"))
        .and_then(|v| v.parse::<i16>().ok())
        .filter(|v| (-127..=100).contains(v));
    Ok(json!(strength))
}

#[derive(Default)]
struct Native {
    wifi: Value,
    link: Value,
    kind: Value,
    signal: Value,
    peer: Option<IpAddr>,
    incomplete: bool,
}

impl Native {
    fn active(&self) -> Option<Kind> {
        match self.kind.as_str()?.to_ascii_lowercase().as_str() {
            "lan" | "ethernet" | "wired" => Some(Kind::Ethernet),
            "wifi" | "wlan" | "wireless" => Some(Kind::Wifi),
            "pppoe"
                if self.wifi["ssid"].is_string() && self.signal.as_i64().is_some_and(|v| v < 0) =>
            {
                Some(Kind::Wifi)
            }
            _ => None,
        }
    }
    async fn read(device: &Device) -> Result<Self> {
        let mut session = device.connect().await?;
        let result = async {
            let mut native = Self {
                peer: session.peer_ip(),
                wifi: read_part(&mut session, 116, wifi).await?,
                link: read_part(&mut session, 76, link).await?,
                ..Default::default()
            };
            let transport = read_part(&mut session, 93, link_type).await;
            let strength = read_part(&mut session, 115, signal).await;
            native.incomplete = transport.is_err() || strength.is_err();
            native.kind = transport.unwrap_or_default();
            native.signal = strength.unwrap_or_default();
            Ok(native)
        }
        .await;
        session.logout().await;
        result
    }
}

fn wifi_control<'a>(native: &Native, interfaces: &'a [Interface]) -> Option<&'a Interface> {
    let peer = native.peer?;
    if native.active() != Some(Kind::Ethernet)
        || !interfaces.iter().any(|i| {
            i.kind == Kind::Ethernet && i.enabled == Some(true) && i.addresses.contains(&peer)
        })
    {
        return None;
    }
    let mut radios = interfaces.iter().filter(|i| i.kind == Kind::Wifi);
    let radio = radios.next()?;
    (radios.next().is_none()
        && radio.enabled.is_some()
        && !radio.addresses.contains(&peer)
        && interfaces.iter().filter(|i| i.token == radio.token).count() == 1)
        .then_some(radio)
}

fn summary(native: &Native, interfaces: &[Interface]) -> Value {
    let active = native.active();
    let address = native.link["address"]
        .as_str()
        .and_then(|v| v.parse::<IpAddr>().ok())
        .filter(|v| !v.is_unspecified());
    let control = wifi_control(native, interfaces).is_some();
    let describe = |kind| {
        // Some wireless-only firmware calls its single logical interface eth0.
        let logical_wifi = interfaces.len() == 1
            && active == Some(Kind::Wifi)
            && address.is_some_and(|ip| interfaces[0].addresses.contains(&ip));
        let reported: Vec<_> = interfaces
            .iter()
            .filter(|i| {
                if logical_wifi {
                    kind == Kind::Wifi
                } else {
                    i.kind == kind
                }
            })
            .collect();
        let current = active == Some(kind);
        if reported.is_empty() && !current && !(kind == Kind::Wifi && native.wifi.is_object()) {
            return Value::Null;
        }
        let mut ips: BTreeSet<_> = reported
            .iter()
            .flat_map(|i| i.addresses.iter().copied())
            .collect();
        if current {
            ips.extend(address);
        }
        let dhcp = reported.iter().find_map(|i| i.dhcp).or_else(|| {
            if current {
                native.link["dhcp"].as_bool()
            } else {
                None
            }
        });
        let enabled = if current {
            Some(true)
        } else {
            reported.iter().filter_map(|i| i.enabled).max()
        };
        json!({"addresses":ips,"active":active.map(|k|k==kind),"enabled":enabled,"dhcp":dhcp})
    };
    json!({"link":native.link,"wifi":native.wifi,"interfaces":{"ethernet":describe(Kind::Ethernet),"wifi":describe(Kind::Wifi)},
        "wifiSignal":native.signal,"wifiSwitchAvailable":control})
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
    let device = api.devices.get(&id);
    let onvif = api.onvif.get(&id);
    if device.is_none() && onvif.is_none() {
        return error(StatusCode::NOT_FOUND, "API_NETWORK_UNSUPPORTED", &cid);
    }
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let (native, interfaces) = tokio::join!(
        async {
            match device {
                Some(d) => timeout(Duration::from_secs(17), Native::read(d))
                    .await
                    .ok()
                    .and_then(Result::ok),
                None => None,
            }
        },
        async {
            match onvif {
                Some(client) => timeout(Duration::from_secs(5), client.interfaces())
                    .await
                    .ok()
                    .and_then(Result::ok),
                None => None,
            }
        }
    );
    if native.is_none() && interfaces.is_none() {
        return error(StatusCode::BAD_GATEWAY, "API_NETWORK_READ_FAILED", &cid);
    }
    let native_incomplete = device.is_some() && native.as_ref().is_none_or(|n| n.incomplete);
    if native_incomplete || onvif.is_some() && interfaces.is_none() {
        api.hub().diagnostics.error(
            "api::network",
            "API_NETWORK_PARTIAL",
            Some(cid.0.clone()),
            json!({"cameraId":id,"missing":if native_incomplete {"baichuan"}else{"onvif"}}),
            None,
        );
    }
    let interfaces = interfaces.unwrap_or_default();
    let mut status = summary(&native.unwrap_or_default(), &interfaces);
    let mut radios = interfaces
        .iter()
        .filter(|i| i.kind == Kind::Wifi && i.enabled != Some(false));
    if let (Some(client), Some(radio), None) = (onvif, radios.next(), radios.next()) {
        match timeout(Duration::from_secs(3), client.wireless_status(radio)).await {
            Ok(Ok(wifi)) => status["wifiStatus"] = wifi,
            _ => {
                api.hub().diagnostics.error(
                    "api::network",
                    "API_NETWORK_PARTIAL",
                    Some(cid.0.clone()),
                    json!({"cameraId":id,"missing":"wifiStatus"}),
                    None,
                );
            }
        }
    }
    Json(status).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Switch {
    enabled: bool,
    confirmation: bool,
}

pub(super) async fn switch(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    body: Result<Json<Switch>, JsonRejection>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Ok(Json(Switch {
        enabled,
        confirmation: true,
    })) = body
    else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_NETWORK_INVALID",
            &cid,
        );
    };
    let (Some(device), Some(onvif)) = (api.devices.get(&id), api.onvif.get(&id)) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_NETWORK_UNSUPPORTED",
            &cid,
        );
    };
    let Ok(_guard) = api.network_writes[&id].try_lock() else {
        return error(StatusCode::CONFLICT, "API_NETWORK_BUSY", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    // Use fresh readings on every change. A cached LAN indication could outlive the cable connection.
    let preflight = async { tokio::try_join!(Native::read(device), onvif.interfaces()) };
    let Ok(Ok((native, interfaces))) = timeout(Duration::from_secs(17), preflight).await else {
        return error(StatusCode::BAD_GATEWAY, "API_NETWORK_READ_FAILED", &cid);
    };
    let Some(radio) = wifi_control(&native, &interfaces) else {
        return error(StatusCode::CONFLICT, "API_NETWORK_WIRED_REQUIRED", &cid);
    };
    if radio.enabled != Some(enabled) {
        match onvif.set_wifi_interface(radio, enabled).await {
            Ok(false) => (),
            Ok(true) => return error(StatusCode::CONFLICT, "API_WIFI_REBOOT_REQUIRED", &cid),
            Err(_) => return error(StatusCode::BAD_GATEWAY, "API_NETWORK_OUTCOME_UNKNOWN", &cid),
        }
        if !onvif.interfaces().await.is_ok_and(|after| {
            after.iter().any(|i| {
                i.token == radio.token && i.kind == Kind::Wifi && i.enabled == Some(enabled)
            })
        }) {
            return error(StatusCode::BAD_GATEWAY, "API_NETWORK_OUTCOME_UNKNOWN", &cid);
        }
    }
    Json(json!({"enabled":enabled,"outcome":"confirmed","correlation_id":cid.0})).into_response()
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
    fn switch_requires_a_distinct_radio_and_a_fresh_wired_path() {
        let peer = "192.0.2.3".parse::<IpAddr>().unwrap();
        let mut native = Native {
            peer: Some(peer),
            kind: json!("LAN"),
            link: json!({"address":peer}),
            ..Default::default()
        };
        let mut ifaces = [
            Interface {
                token: "eth0".into(),
                kind: Kind::Ethernet,
                enabled: Some(true),
                addresses: [peer].into(),
                dhcp: Some(true),
            },
            Interface {
                token: "wlan0".into(),
                kind: Kind::Wifi,
                enabled: Some(true),
                addresses: Default::default(),
                dhcp: None,
            },
        ];
        assert!(wifi_control(&native, &ifaces).is_some());
        ifaces[1].addresses.insert(peer);
        assert!(wifi_control(&native, &ifaces).is_none());
        native.kind = json!("PPPOE");
        native.signal = json!(-45);
        native.wifi = json!({"ssid":"test"});
        assert!(wifi_control(&native, &ifaces).is_none());
        let result = summary(&native, &ifaces[..1]);
        assert_eq!(result["interfaces"]["ethernet"], Value::Null);
        assert_eq!(result["interfaces"]["wifi"]["addresses"][0], "192.0.2.3");
        assert_eq!(result["wifiSignal"], -45);
    }

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
