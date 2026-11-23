pub mod events;
mod metadata;
pub(crate) mod network;

use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{SecondsFormat, Utc};
use digest_auth::AuthContext;
use reqwest::{Client, header};
use roxmltree::Node;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use url::{Position, Url};
use uuid::Uuid;

use crate::{
    model::{Rule, boolean},
    xml,
};

const DEV: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA: &str = "http://www.onvif.org/ver10/media/wsdl";
const ANALYTICS: &str = "http://www.onvif.org/ver20/analytics/wsdl";
const EVENTS: &str = "http://www.onvif.org/ver10/events/wsdl";
const WSN: &str = "http://docs.oasis-open.org/wsn/bw-2";
const WSA: &str = "http://www.w3.org/2005/08/addressing";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Onvif {
    client: Client,
    device: Url,
    username: String,
    password: String,
}

pub struct Discovery {
    pub info: Option<Value>,
    pub capabilities: Value,
    pub rules: Option<Vec<Rule>>,
}

struct Services {
    events: Url,
    media: Url,
    analytics: Url,
    capabilities: Option<Value>,
}

#[derive(Clone)]
pub struct MetadataStream {
    pub url: Url,
    pub reference_frame: String,
    pub dimensions: Option<(u32, u32)>,
}

struct Profile {
    token: String,
    dimensions: Option<(u32, u32)>,
    analytics_metadata: bool,
}

impl Onvif {
    pub fn new(
        client: Client,
        host: &str,
        port: u16,
        username: String,
        password: String,
    ) -> Result<Self> {
        let device = Url::parse(&format!("http://{host}:{port}/onvif/device_service"))
            .map_err(|_| anyhow!("invalid ONVIF endpoint"))?;
        Ok(Self {
            client,
            device,
            username,
            password,
        })
    }

    async fn call(&self, url: &Url, action: impl AsRef<str>, body: &str) -> Result<String> {
        self.request(url, action.as_ref(), body, "", REQUEST_TIMEOUT)
            .await
    }

    async fn request(
        &self,
        url: &Url,
        action: &str,
        body: &str,
        references: &str,
        timeout: Duration,
    ) -> Result<String> {
        let nonce = Uuid::new_v4();
        let time = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let mut hash = Sha1::new();
        hash.update(nonce.as_bytes());
        hash.update(time.as_bytes());
        hash.update(self.password.as_bytes());
        let digest = STANDARD.encode(hash.finalize());
        let nonce = STANDARD.encode(nonce.as_bytes());
        let username = xml::escape(&self.username);
        let to = xml::escape(url.as_str());
        let request = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="{WSA}"
 xmlns:tds="{DEV}" xmlns:tev="{EVENTS}" xmlns:trt="{MEDIA}" xmlns:tan="{ANALYTICS}"
 xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2"
 xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"
 xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">
 <s:Header><wsa:Action>{action}</wsa:Action><wsa:To>{to}</wsa:To>
 <wsa:MessageID>urn:uuid:{message_id}</wsa:MessageID>
 <wsa:ReplyTo><wsa:Address>{WSA}/anonymous</wsa:Address></wsa:ReplyTo>{references}
 <wsse:Security><wsse:UsernameToken><wsse:Username>{username}</wsse:Username>
 <wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password>
 <wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce}</wsse:Nonce>
 <wsu:Created>{time}</wsu:Created></wsse:UsernameToken></wsse:Security>
 </s:Header><s:Body>{body}</s:Body></s:Envelope>"#,
            message_id = Uuid::new_v4()
        );
        let build = || {
            self.client
                .post(url.clone())
                .timeout(timeout)
                .header(
                    header::CONTENT_TYPE,
                    format!("application/soap+xml; charset=utf-8; action=\"{action}\""),
                )
                .body(request.clone())
        };
        let mut response = build()
            .send()
            .await
            .map_err(|_| anyhow!("ONVIF connection failed or timed out"))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get_all(header::WWW_AUTHENTICATE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .find_map(|value| digest_auth::parse(value).ok());
            let mut challenge =
                challenge.ok_or_else(|| anyhow!("ONVIF authentication rejected"))?;
            let context = AuthContext::new_post(
                self.username.as_str(),
                self.password.as_str(),
                &url[Position::BeforePath..Position::AfterQuery],
                Some(request.as_bytes()),
            );
            let auth = challenge
                .respond(&context)
                .map_err(|_| anyhow!("ONVIF HTTP Digest negotiation failed"))?;
            response = build()
                .header(header::AUTHORIZATION, auth.to_header_string())
                .send()
                .await
                .map_err(|_| anyhow!("ONVIF connection failed or timed out"))?;
        }
        let status = response.status();
        ensure!(status.is_success(), "ONVIF HTTP status {status}");
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("failed to read ONVIF response"))?
        {
            ensure!(
                body.len() + chunk.len() <= xml::BODY_LIMIT,
                "ONVIF response too large"
            );
            body.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(body).map_err(|_| anyhow!("ONVIF response is not UTF-8"))?;
        ensure!(
            xml::find(xml::document(&body)?.root_element(), "Fault").is_none(),
            "ONVIF SOAP fault"
        );
        Ok(body)
    }

    fn endpoint(&self, endpoint: Option<&str>, fallback: &str) -> Url {
        endpoint
            .and_then(|value| camera_url(&self.device, value, &["http", "https"]))
            .unwrap_or_else(|| self.device.join(fallback).expect("valid fallback endpoint"))
    }

    async fn services(&self) -> Services {
        let (cap, services) = tokio::join!(
            self.call(&self.device, format!("{DEV}/GetCapabilities"),
                "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>"),
            self.call(&self.device, format!("{DEV}/GetServices"),
                "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>")
        );
        let capabilities = cap
            .as_deref()
            .ok()
            .and_then(|body| xml::document(body).ok());
        let caps = capabilities
            .as_ref()
            .and_then(|doc| xml::find(doc.root_element(), "Capabilities"));
        let services = services
            .as_deref()
            .ok()
            .and_then(|body| xml::document(body).ok());
        let address = |namespace, name| {
            services
                .as_ref()
                .and_then(|doc| {
                    doc.descendants()
                        .filter(|node| node.has_tag_name("Service"))
                        .find(|node| xml::value(*node, "Namespace") == Some(namespace))
                        .and_then(|node| xml::value(node, "XAddr"))
                })
                .or_else(|| {
                    caps.and_then(|caps| xml::find(caps, name))
                        .and_then(|service| xml::value(service, "XAddr"))
                })
        };
        Services {
            events: self.endpoint(address(EVENTS, "Events"), "/onvif/event_service"),
            media: self.endpoint(address(MEDIA, "Media"), "/onvif/media_service"),
            analytics: self.endpoint(address(ANALYTICS, "Analytics"), "/onvif/analytics_service"),
            capabilities: caps.map(xml::tree),
        }
    }

    pub async fn hardware_id(&self) -> Option<crate::identity::HardwareId> {
        use crate::identity::HardwareId;
        let info = self
            .call(
                &self.device,
                format!("{DEV}/GetDeviceInformation"),
                "<tds:GetDeviceInformation/>",
            )
            .await
            .ok()?;
        let info = device_info(&info, &self.device)?;
        HardwareId::serial(info["serial_number"].as_str()?)
    }

    pub async fn discover(&self) -> Discovery {
        let (services, info) = tokio::join!(
            self.services(),
            self.call(
                &self.device,
                format!("{DEV}/GetDeviceInformation"),
                "<tds:GetDeviceInformation/>"
            )
        );
        let (topics, event_caps, profiles, analytics, metadata) = tokio::join!(
            self.call(
                &services.events,
                format!("{EVENTS}/EventPortType/GetEventPropertiesRequest"),
                "<tev:GetEventProperties/>"
            ),
            self.call(
                &services.events,
                format!("{EVENTS}/EventPortType/GetServiceCapabilitiesRequest"),
                "<tev:GetServiceCapabilities/>"
            ),
            self.call(
                &services.media,
                format!("{MEDIA}/GetProfiles"),
                "<trt:GetProfiles/>"
            ),
            self.call(
                &services.media,
                format!("{MEDIA}/GetVideoAnalyticsConfigurations"),
                "<trt:GetVideoAnalyticsConfigurations/>"
            ),
            self.call(
                &services.media,
                format!("{MEDIA}/GetMetadataConfigurations"),
                "<trt:GetMetadataConfigurations/>"
            )
        );
        let profile_doc = profiles
            .as_deref()
            .ok()
            .and_then(|xml| xml::document(xml).ok());
        let analytics_doc = analytics
            .as_deref()
            .ok()
            .and_then(|xml| xml::document(xml).ok());
        let tokens: std::collections::BTreeSet<_> = profile_doc
            .iter()
            .flat_map(|doc| doc.descendants())
            .filter(|n| n.has_tag_name("VideoAnalyticsConfiguration"))
            .filter_map(|n| n.attribute("token"))
            .chain(
                analytics_doc
                    .iter()
                    .flat_map(|doc| doc.descendants())
                    .filter(|n| n.has_tag_name("Configurations"))
                    .filter_map(|n| n.attribute("token")),
            )
            .take(32)
            .collect();
        let mut rules = None::<Vec<Rule>>;
        for token in tokens {
            let body = format!(
                "<tan:GetRules><tan:ConfigurationToken>{}</tan:ConfigurationToken></tan:GetRules>",
                xml::escape(token)
            );
            if let Ok(reply) = self
                .call(&services.analytics, &format!("{ANALYTICS}/GetRules"), &body)
                .await
                && let Ok(doc) = xml::document(&reply)
                && let Some(response) = xml::find(doc.root_element(), "GetRulesResponse")
            {
                rules.get_or_insert_with(Vec::new).extend(
                    response
                        .children()
                        .filter(|n| n.has_tag_name("Rule"))
                        .map(|node| read_rule(node, token)),
                );
            }
        }
        Discovery {
            info: info.ok().and_then(|body| device_info(&body, &self.device)),
            capabilities: json!({
                "device": services.capabilities,
                "events": event_caps.ok().and_then(|body| response_node(&body, "Capabilities")),
                "advertised_topics": topics.ok().and_then(|body| response_node(&body, "TopicSet")),
                "metadata": metadata.ok().and_then(|body| response_node(&body, "GetMetadataConfigurationsResponse")),
                "rules_read": rules.is_some(),
            }),
            rules,
        }
    }

    pub async fn metadata_stream(&self, configured: Option<Url>) -> Result<MetadataStream> {
        if let Some(url) = configured {
            return Ok(MetadataStream {
                url,
                reference_frame: "configured ONVIF RTSP stream".into(),
                dimensions: None,
            });
        }
        let services = self.services().await;
        let raw = self
            .call(
                &services.media,
                &format!("{MEDIA}/GetProfiles"),
                "<trt:GetProfiles/>",
            )
            .await?;
        let doc = xml::document(&raw)?;
        let profile = doc
            .descendants()
            .filter(|node| node.has_tag_name("Profiles"))
            .filter_map(|node| {
                let encoder = xml::find(node, "VideoEncoderConfiguration");
                let dimensions = encoder.and_then(|n| {
                    Some((
                        xml::value(n, "Width")?.parse().ok()?,
                        xml::value(n, "Height")?.parse().ok()?,
                    ))
                });
                let analytics_metadata = xml::find(node, "MetadataConfiguration")
                    .and_then(|n| xml::value(n, "Analytics"))
                    .and_then(boolean)
                    == Some(true);
                Some(Profile {
                    token: node.attribute("token")?.into(),
                    dimensions,
                    analytics_metadata,
                })
            })
            .filter(|profile| profile.analytics_metadata)
            .min_by_key(|profile| {
                profile
                    .dimensions
                    .map_or(u64::MAX, |(w, h)| u64::from(w) * u64::from(h))
            })
            .ok_or_else(|| anyhow!("camera advertises no ONVIF analytics metadata profile"))?;
        let body = format!(
            "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream>\
            <tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup>\
            <trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>",
            xml::escape(&profile.token)
        );
        let response = self
            .call(&services.media, &format!("{MEDIA}/GetStreamUri"), &body)
            .await?;
        let doc = xml::document(&response)?;
        let url = xml::value(doc.root_element(), "Uri")
            .and_then(|uri| camera_url(&self.device, uri, &["rtsp"]))
            .ok_or_else(|| anyhow!("camera did not return a usable RTSP URI"))?;
        Ok(MetadataStream {
            url,
            reference_frame: format!("ONVIF profile {}", profile.token),
            dimensions: profile.dimensions,
        })
    }

    pub async fn stream_metadata(
        &self,
        stream: MetadataStream,
        id: &str,
        hub: &crate::hub::Hub,
        stop: &tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        metadata::stream(stream, &self.username, &self.password, id, hub, stop).await
    }
}

fn camera_url(base: &Url, value: &str, schemes: &[&str]) -> Option<Url> {
    let mut url = base.join(value).ok()?;
    if !schemes.contains(&url.scheme()) || url.port() == Some(0) {
        return None;
    }
    url.set_host(base.host_str()).ok()?;
    url.set_password(None).ok()?;
    url.set_username("").ok()?;
    url.set_fragment(None);
    Some(url)
}

fn device_info(body: &str, endpoint: &Url) -> Option<Value> {
    let doc = xml::document(body).ok()?;
    let info = xml::find(doc.root_element(), "GetDeviceInformationResponse")?;
    let manufacturer = xml::value(info, "Manufacturer");
    let reolink = manufacturer.is_some_and(|name| name.to_ascii_lowercase().contains("reolink"));
    let address = endpoint
        .host_str()
        .and_then(|host| host.parse::<std::net::Ipv4Addr>().ok());
    // The manufacturer's raw response is retained under vendor for diagnostics.
    let serial = xml::value(info, "SerialNumber")
        .and_then(crate::identity::HardwareId::serial)
        .filter(|id| !(reolink && id.value.bytes().all(|b| b.is_ascii_digit())))
        .filter(|id| !address.is_some_and(|ip| id.value == ip.to_string().replace('.', "")));
    Some(json!({
        "manufacturer": manufacturer,
        "model": xml::value(info, "Model"),
        "firmware": xml::value(info, "FirmwareVersion"),
        "serial_number": serial.map(|id| id.value),
        "hardware": xml::value(info, "HardwareId"),
        "vendor": xml::tree(info),
    }))
}

fn response_node(body: &str, name: &str) -> Option<Value> {
    xml::find(xml::document(body).ok()?.root_element(), name).map(xml::tree)
}

fn read_rule(node: Node<'_, '_>, token: &str) -> Rule {
    let parameters = xml::child(node, "Parameters");
    let items = parameters
        .into_iter()
        .flat_map(|node| node.children())
        .filter(Node::is_element);
    let enabled = node.attribute("enabled").and_then(boolean).or_else(|| {
        items.clone().find_map(|item| {
            let name = item.attribute("Name")?;
            (name.eq_ignore_ascii_case("Enabled") || name.eq_ignore_ascii_case("Enable"))
                .then(|| item.attribute("Value").and_then(boolean))
                .flatten()
        })
    });
    let geometry = items
        .filter(|node| node.has_tag_name("ElementItem"))
        .map(xml::tree)
        .filter(|item| !item.is_null())
        .collect();
    let name = node.attribute("Name").map(str::to_owned);
    Rule {
        configuration_id: Some(token.to_owned()),
        source: "onvif",
        id: node
            .attribute("token")
            .or(node.attribute("Id"))
            .map(str::to_owned)
            .or_else(|| name.clone()),
        kind: node.attribute("Type").unwrap_or("unknown").into(),
        name,
        enabled,
        geometry,
        parameters: parameters.map(xml::tree).unwrap_or(Value::Null),
    }
}
