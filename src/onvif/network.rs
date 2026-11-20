use super::*;
use std::{collections::BTreeSet, net::IpAddr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Ethernet,
    Wifi,
}

pub(crate) struct Interface {
    pub token: String,
    pub kind: Kind,
    pub enabled: Option<bool>,
    pub addresses: BTreeSet<IpAddr>,
    pub dhcp: Option<bool>,
}

fn kind(node: Node<'_, '_>, token: &str) -> Option<Kind> {
    if xml::find(node, "Dot11").is_some() || xml::value(node, "InterfaceType") == Some("71") {
        return Some(Kind::Wifi);
    }
    let name = xml::child(node, "Info")
        .and_then(|i| xml::value(i, "Name"))
        .unwrap_or("");
    let names = [name.to_ascii_lowercase(), token.to_ascii_lowercase()];
    if names.iter().any(|n| {
        n.starts_with("wlan")
            || n.starts_with("wifi")
            || n.starts_with("wireless")
            || n.starts_with("wi-fi")
            || n.starts_with("wlp")
            || n.starts_with("ra0")
    }) {
        return Some(Kind::Wifi);
    }
    if xml::value(node, "InterfaceType") == Some("6")
        || names
            .iter()
            .any(|n| n.starts_with("eth") || n.starts_with("enp") || n.starts_with("lan"))
    {
        return Some(Kind::Ethernet);
    }
    None
}

fn interfaces(raw: &str) -> Result<Vec<Interface>> {
    let doc = xml::document(raw)?;
    let root = xml::find(doc.root_element(), "GetNetworkInterfacesResponse")
        .ok_or_else(|| anyhow!("missing network interfaces response"))?;
    Ok(root
        .children()
        .filter(|n| n.has_tag_name("NetworkInterfaces"))
        .filter_map(|n| {
            let token = n.attribute("token")?.trim();
            let kind = kind(n, token)?;
            if token.is_empty() || token.len() > 128 || token.chars().any(char::is_control) {
                return None;
            }
            let enabled = xml::child(n, "Enabled")
                .and_then(xml::text)
                .and_then(boolean);
            let mut addresses = BTreeSet::new();
            let mut dhcp = None;
            for version in ["IPv4", "IPv6"] {
                let Some(ip) = xml::child(n, version) else {
                    continue;
                };
                if xml::child(ip, "Enabled")
                    .and_then(xml::text)
                    .and_then(boolean)
                    == Some(false)
                {
                    continue;
                }
                let Some(config) = xml::child(ip, "Config") else {
                    continue;
                };
                if version == "IPv4" {
                    dhcp = xml::child(config, "DHCP")
                        .and_then(xml::text)
                        .and_then(boolean);
                }
                for item in config.children().filter(Node::is_element) {
                    if matches!(
                        item.tag_name().name(),
                        "Manual" | "FromDHCP" | "FromRA" | "LinkLocal"
                    ) && let Some(addr) = xml::child(item, "Address")
                        .and_then(xml::text)
                        .and_then(|v| v.parse::<IpAddr>().ok())
                        && !addr.is_unspecified()
                    {
                        addresses.insert(addr);
                    }
                }
            }
            Some(Interface {
                token: token.to_owned(),
                kind,
                enabled,
                addresses,
                dhcp,
            })
        })
        .collect())
}

fn wireless_status(raw: &str) -> Result<Value> {
    let doc = xml::document(raw)?;
    let status = xml::find(doc.root_element(), "GetDot11StatusResponse")
        .and_then(|r| xml::child(r, "Status"))
        .ok_or_else(|| anyhow!("missing Wi-Fi status"))?;
    // ONVIF represents the SSID as up to 32 octets of hexBinary.
    let ssid = xml::value(status, "SSID")
        .filter(|s| {
            s.len() <= 64 && s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .and_then(|s| {
            let octets: Option<Vec<_>> = (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
                .collect();
            String::from_utf8(octets?).ok()
        })
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control));
    let signal = xml::value(status, "SignalStrength")
        .filter(|s| matches!(*s, "None" | "Very Bad" | "Bad" | "Good" | "Very Good"));
    let bssid = xml::value(status, "BSSID").and_then(crate::identity::HardwareId::mac);
    let associated = if signal == Some("None") {
        Some(false)
    } else if bssid.is_some() || ssid.is_some() {
        Some(true)
    } else {
        None
    };
    Ok(json!({"associated":associated,"ssid":ssid,"signal":signal}))
}

impl Onvif {
    pub(crate) async fn wireless_status(&self, interface: &Interface) -> Result<Value> {
        let request = format!(
            "<tds:GetDot11Status><tds:InterfaceToken>{}</tds:InterfaceToken></tds:GetDot11Status>",
            xml::escape(&interface.token)
        );
        let raw = self
            .call(&self.device, format!("{DEV}/GetDot11Status"), &request)
            .await?;
        wireless_status(&raw)
    }
    pub(crate) async fn interfaces(&self) -> Result<Vec<Interface>> {
        let body = self
            .call(
                &self.device,
                format!("{DEV}/GetNetworkInterfaces"),
                "<tds:GetNetworkInterfaces/>",
            )
            .await?;
        interfaces(&body)
    }
    pub(crate) async fn set_wifi_interface(&self, wifi: &Interface, enabled: bool) -> Result<bool> {
        ensure!(wifi.kind == Kind::Wifi, "wireless interface required");
        let request = format!(
            "<tds:SetNetworkInterfaces><tds:InterfaceToken>{}</tds:InterfaceToken><tds:NetworkInterface><tt:Enabled>{enabled}</tt:Enabled></tds:NetworkInterface></tds:SetNetworkInterfaces>",
            xml::escape(&wifi.token)
        );
        let body = self
            .call(
                &self.device,
                format!("{DEV}/SetNetworkInterfaces"),
                &request,
            )
            .await?;
        let doc = xml::document(&body)?;
        xml::value(doc.root_element(), "RebootNeeded")
            .and_then(boolean)
            .ok_or_else(|| anyhow!("interface change was not verified"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distinguishes_associated_network_from_stored_credentials() {
        let value=wireless_status("<GetDot11StatusResponse><Status><SSID>54657374</SSID><SignalStrength>Good</SignalStrength><Key>hidden</Key></Status></GetDot11StatusResponse>").unwrap();
        assert_eq!(
            value,
            json!({"associated":true,"ssid":"Test","signal":"Good"})
        );
        let empty=wireless_status("<GetDot11StatusResponse><Status><SSID/><SignalStrength>None</SignalStrength></Status></GetDot11StatusResponse>").unwrap();
        assert_eq!(empty["associated"], false);
    }
    #[tokio::test]
    async fn wireless_toggle_sends_only_the_interface_token_and_enabled_flag() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = |body: String| async move {
            let doc = xml::document(&body).unwrap();
            let op = xml::find(doc.root_element(), "SetNetworkInterfaces").unwrap();
            assert_eq!(xml::value(op, "InterfaceToken"), Some("radio&1"));
            let config = xml::child(op, "NetworkInterface").unwrap();
            assert_eq!(config.children().filter(Node::is_element).count(), 1);
            assert_eq!(xml::value(config, "Enabled"), Some("false"));
            "<SetNetworkInterfacesResponse><RebootNeeded>false</RebootNeeded></SetNetworkInterfacesResponse>"
        };
        let app = axum::Router::new().route("/onvif/device_service", axum::routing::post(handler));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = Onvif::new(
            Client::new(),
            "127.0.0.1",
            port,
            "fixture".into(),
            "fixture".into(),
        )
        .unwrap();
        let radio = Interface {
            token: "radio&1".into(),
            kind: Kind::Wifi,
            enabled: Some(true),
            addresses: BTreeSet::new(),
            dhcp: None,
        };
        assert!(!client.set_wifi_interface(&radio, false).await.unwrap());
        task.abort();
    }
    #[test]
    fn reads_addresses_per_interface_without_including_wifi_credentials() {
        let raw = r#"<GetNetworkInterfacesResponse><NetworkInterfaces token="eth0"><Enabled>true</Enabled><IPv4><Config><DHCP>true</DHCP><FromDHCP><Address>192.0.2.4</Address></FromDHCP></Config></IPv4></NetworkInterfaces><NetworkInterfaces token="radio"><Info><Name>wlan0</Name></Info><Enabled>false</Enabled><IPv6><Config><FromRA><Address>2001:db8::4</Address></FromRA></Config></IPv6><Extension><Dot11><PSK>secret</PSK></Dot11></Extension></NetworkInterfaces></GetNetworkInterfacesResponse>"#;
        let result = interfaces(raw).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, Kind::Ethernet);
        assert_eq!(result[0].dhcp, Some(true));
        assert!(result[0].addresses.contains(&"192.0.2.4".parse().unwrap()));
        assert_eq!(result[1].kind, Kind::Wifi);
        assert_eq!(result[1].enabled, Some(false));
        assert!(
            result[1]
                .addresses
                .contains(&"2001:db8::4".parse().unwrap())
        );
    }
}
