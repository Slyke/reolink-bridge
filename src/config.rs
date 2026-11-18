use std::{
    collections::BTreeSet,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use styleguide_logger::config::resolve_environment_references;
use url::Url;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mqtt: Mqtt,
    pub http: Http,
    websocket: Option<WebSocket>,
    pub cameras: Vec<Camera>,
    #[serde(default)]
    pub callbacks: Vec<Callback>,
    #[serde(alias = "wui")]
    pub api: Option<crate::api::Config>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mqtt {
    pub url: Url,
    #[serde(alias = "prefix")]
    pub base_topic: String,
    #[serde(default = "client_id")]
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    password_env: Option<String>,
    pub tls: Option<MqttTls>,
    #[serde(default = "default_snapshot_payload_bytes")]
    pub snapshot_max_payload_bytes: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MqttTls {
    pub ca_file: Option<PathBuf>,
}

pub const MIN_SNAPSHOT_PAYLOAD_BYTES: usize = 4096;
pub const MAX_SNAPSHOT_PAYLOAD_BYTES: usize = 24 * 1024 * 1024;

fn default_snapshot_payload_bytes() -> usize {
    1_000_000
}

fn client_id() -> String {
    "reolink-bridge".into()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Http {
    pub listen: SocketAddr,
    pub advertise_base_url: Option<Url>,
    pub token: Option<String>,
    pub websocket: Option<WebSocket>,
    pub tls: Option<HttpTls>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpTls {
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    #[serde(default)]
    pub server_names: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebSocket {
    #[serde(default = "websocket_path")]
    pub path: String,
    pub token: Option<String>,
    token_env: Option<String>,
}

fn websocket_path() -> String {
    "/ws".into()
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Camera {
    pub id: String,
    pub stable_id: Option<String>,
    pub host: String,
    pub username: String,
    pub password: Option<String>,
    password_env: Option<String>,
    #[serde(default = "onvif_port")]
    pub onvif_port: Option<u16>,
    #[serde(default = "baichuan_port")]
    pub baichuan_port: Option<u16>,
    #[serde(default)]
    pub event_transport: EventTransport,
    #[serde(default)]
    pub channel: u8,
    #[serde(default)]
    pub coordinates: Coordinates,
    pub native_webhook: Option<NativeWebhook>,
    #[serde(default)]
    pub ptz: PtzMode,
}

impl Camera {
    pub fn identity(&self) -> &str {
        self.stable_id.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PtzMode {
    #[default]
    Auto,
    Disabled,
    PanTilt,
    PanTiltZoom,
    Zoom,
}

fn onvif_port() -> Option<u16> {
    Some(8000)
}

fn baichuan_port() -> Option<u16> {
    Some(9000)
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventTransport {
    #[default]
    Auto,
    OnvifPush,
    OnvifPull,
    Baichuan,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Coordinates {
    #[serde(default)]
    pub allow_video_stream: bool,
    pub rtsp_url: Option<Url>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeWebhook {
    pub token: Option<String>,
    token_env: Option<String>,
    #[serde(default)]
    pub protocol: WebhookProtocol,
    pub cgi_base_url: Option<Url>,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebhookProtocol {
    #[default]
    HaCfg,
    Cgi,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Callback {
    pub id: String,
    pub url: Url,
    #[serde(flatten)]
    pub filter: Filter,
}

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Filter {
    #[serde(default)]
    pub cameras: Vec<String>,
    #[serde(default)]
    pub events: Vec<String>,
}

impl Filter {
    pub fn camera(&self, id: &str) -> bool {
        self.cameras.is_empty() || self.cameras.iter().any(|item| item == id)
    }

    pub fn event(&self, kind: &str, vendor_topic: &str) -> bool {
        self.events.is_empty()
            || self
                .events
                .iter()
                .any(|item| item == kind || item == vendor_topic)
    }

    pub fn validate(&self, cameras: &BTreeSet<String>) -> Result<()> {
        ensure!(
            self.cameras.iter().all(|id| cameras.contains(id)),
            "filter contains an unknown camera"
        );
        ensure!(
            self.events.iter().all(|event| !event.is_empty()
                && event.len() <= 256
                && !event.chars().any(char::is_control)),
            "invalid event filter"
        );
        Ok(())
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

fn safe_url(url: &Url, schemes: &[&str]) -> bool {
    schemes.contains(&url.scheme())
        && url.host().is_some()
        && url.port() != Some(0)
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

pub fn validate_callbacks(callbacks: &[Callback], cameras: &BTreeSet<String>) -> Result<()> {
    ensure!(callbacks.len() <= 64, "too many callbacks");
    let mut ids = BTreeSet::new();
    for callback in callbacks {
        ensure!(
            valid_id(&callback.id) && ids.insert(&callback.id),
            "invalid or duplicate callback ID"
        );
        ensure!(
            safe_url(&callback.url, &["http", "https"]),
            "callback URL must be HTTP(S) with no credentials or fragment"
        );
        callback.filter.validate(cameras)?;
    }
    Ok(())
}

pub fn document(raw: &str) -> Result<Value> {
    json5::from_str(raw).map_err(|_| anyhow::anyhow!("invalid JSON5 configuration syntax"))
}

pub fn parse(mut document: Value, environment: &impl Fn(&str) -> Option<String>) -> Result<Config> {
    // Logging settings resolve separately. This lets the shared logger apply its defaults
    // and legacy environment overrides once, including for structured startup failures.
    if let Some(fields) = document.as_object_mut() {
        fields.remove("logging");
        // Old manifests may still reference retired credentials. Only http.token authenticates clients.
        for name in ["api", "wui"] {
            if let Some(api) = fields.get_mut(name).and_then(Value::as_object_mut) {
                for retired in [
                    "serviceToken",
                    "adminToken",
                    "cameraIds",
                    "commands",
                    "events",
                ] {
                    api.remove(retired);
                }
            }
        }
    }
    let raw = document.clone();
    resolve_environment_references(&mut document, environment);
    let mut config: Config = serde_json::from_value(document).map_err(|_| {
        anyhow::anyhow!(
            "configuration fields do not match the expected types after environment expansion"
        )
    })?;
    ensure!(
        safe_url(&config.mqtt.url, &["mqtt", "mqtts"])
            && matches!(config.mqtt.url.path(), "" | "/")
            && config.mqtt.url.query().is_none(),
        "MQTT URL must contain only the scheme, host and optional port"
    );
    ensure!(
        valid_id(&config.mqtt.client_id)
            && config.mqtt.base_topic.len() <= 160
            && config.mqtt.base_topic.split('/').all(valid_id),
        "invalid MQTT baseTopic or clientId"
    );
    ensure!(
        (MIN_SNAPSHOT_PAYLOAD_BYTES..=MAX_SNAPSHOT_PAYLOAD_BYTES)
            .contains(&config.mqtt.snapshot_max_payload_bytes),
        "mqtt.snapshotMaxPayloadBytes must be {MIN_SNAPSHOT_PAYLOAD_BYTES}–{MAX_SNAPSHOT_PAYLOAD_BYTES}"
    );
    let mqtt_password = resolve_secret(
        config.mqtt.password.take(),
        config.mqtt.password_env.take(),
        &raw["mqtt"]["password"],
        "mqtt.password",
        environment,
    )?;
    ensure!(
        config.mqtt.username.is_some() == mqtt_password.is_some()
            && config
                .mqtt
                .username
                .as_ref()
                .is_none_or(|value| !value.is_empty() && !value.chars().any(char::is_control)),
        "MQTT authentication requires a username and password"
    );
    config.mqtt.password = mqtt_password;
    ensure!(
        config.mqtt.tls.is_none() || config.mqtt.url.scheme() == "mqtts",
        "mqtt.tls requires an mqtts URL"
    );
    config
        .http
        .configure_auth(config.websocket.take(), &raw, environment)?;
    if let Some(tls) = &config.http.tls {
        ensure!(
            tls.cert_file.is_some() == tls.key_file.is_some(),
            "HTTPS requires both certFile and keyFile"
        );
        ensure!(
            tls.cert_file.is_none() || tls.server_names.is_empty(),
            "serverNames applies only to generated HTTPS certificates"
        );
        ensure!(
            tls.server_names.len() <= 32
                && tls.server_names.iter().all(|name| {
                    !name.is_empty()
                        && name.len() <= 253
                        && name
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b".:-".contains(&c))
                }),
            "invalid HTTPS certificate names"
        );
    }
    if let Some(url) = &config.http.advertise_base_url {
        ensure!(
            safe_url(url, &["http", "https"]) && url.query().is_none(),
            "invalid advertised HTTP URL"
        );
    }
    ensure!(
        !config.cameras.is_empty() || config.api.is_some(),
        "configure at least one camera"
    );
    let mut ids = BTreeSet::new();
    let mut identities = BTreeSet::new();
    for (index, camera) in config.cameras.iter_mut().enumerate() {
        ensure!(
            valid_id(&camera.id) && ids.insert(camera.id.clone()),
            "invalid or duplicate camera ID"
        );
        ensure!(
            valid_id(camera.identity()) && identities.insert(camera.identity().to_owned()),
            "invalid or duplicate camera stableId"
        );
        // A verified credential from this bridge can supply the password when a provisioned
        // camera moves into the static configuration. Explicit secret references must still resolve.
        if !raw["cameras"][index]["password"].is_null()
            || camera.password_env.is_some()
            || config
                .api
                .as_ref()
                .and_then(|api| api.storage_directory.as_ref())
                .is_none()
        {
            camera.password = Some(required_secret(
                camera.password.take(),
                camera.password_env.take(),
                &raw["cameras"][index]["password"],
                "cameras[].password",
                environment,
            )?);
        }
        ensure!(
            camera.channel < 100
                && !camera.username.is_empty()
                && !camera.username.chars().any(char::is_control),
            "invalid camera channel or username"
        );
        ensure!(
            camera.onvif_port != Some(0) && camera.baichuan_port != Some(0),
            "invalid camera port"
        );
        ensure!(
            matches!(camera.ptz, PtzMode::Auto | PtzMode::Disabled)
                || camera.baichuan_port.is_some(),
            "a PTZ override requires baichuanPort"
        );
        let host =
            url::Host::parse(&camera.host).map_err(|_| anyhow::anyhow!("invalid camera host"))?;
        ensure!(
            host.to_string() == camera.host || host.to_string().eq_ignore_ascii_case(&camera.host),
            "camera host must be a hostname or IP address"
        );
        match camera.event_transport {
            EventTransport::Auto => ensure!(
                camera.onvif_port.is_some() || camera.baichuan_port.is_some(),
                "camera needs an event transport"
            ),
            EventTransport::OnvifPush => ensure!(
                camera.onvif_port.is_some() && config.http.advertise_base_url.is_some(),
                "ONVIF push requires onvifPort and advertiseBaseUrl"
            ),
            EventTransport::OnvifPull => {
                ensure!(camera.onvif_port.is_some(), "ONVIF pull requires onvifPort")
            }
            EventTransport::Baichuan => ensure!(
                camera.baichuan_port.is_some(),
                "Baichuan requires baichuanPort"
            ),
        }
        if let Some(webhook) = &mut camera.native_webhook {
            let token = required_secret(
                webhook.token.take(),
                webhook.token_env.take(),
                &raw["cameras"][index]["nativeWebhook"]["token"],
                "cameras[].nativeWebhook.token",
                environment,
            )?;
            ensure!(
                valid_id(&token) && token.len() >= 24,
                "native webhook requires a URL-safe token of at least 24 characters"
            );
            webhook.token = Some(token);
            ensure!(
                config.http.advertise_base_url.is_some(),
                "native webhook requires advertiseBaseUrl"
            );
            match webhook.protocol {
                WebhookProtocol::HaCfg => ensure!(
                    camera.baichuan_port.is_some() && webhook.cgi_base_url.is_none(),
                    "HaCfg webhook requires baichuanPort"
                ),
                WebhookProtocol::Cgi => {
                    let url = webhook
                        .cgi_base_url
                        .as_ref()
                        .context("CGI webhook requires cgiBaseUrl")?;
                    ensure!(
                        safe_url(url, &["http", "https"])
                            && url.path() == "/"
                            && url.query().is_none()
                            && url
                                .host_str()
                                .and_then(|name| url::Host::parse(name).ok())
                                .as_ref()
                                == Some(&host),
                        "CGI base URL must be a camera HTTP(S) origin without credentials"
                    );
                }
            }
        }
        if let Some(url) = &camera.coordinates.rtsp_url {
            ensure!(
                safe_url(url, &["rtsp"])
                    && url.host_str().and_then(|name| url::Host::parse(name).ok()) == Some(host),
                "RTSP URL must use the configured camera host and omit credentials"
            );
        }
    }
    validate_callbacks(&config.callbacks, &ids)?;
    Ok(config)
}

impl Http {
    fn configure_auth(
        &mut self,
        legacy: Option<WebSocket>,
        raw: &Value,
        environment: &impl Fn(&str) -> Option<String>,
    ) -> Result<()> {
        ensure!(
            legacy.is_none() || self.websocket.is_none(),
            "configure WebSocket under http.websocket or at the top level"
        );
        let legacy_path = legacy.is_some();
        let mut ws = self
            .websocket
            .take()
            .or(legacy)
            .unwrap_or_else(|| WebSocket {
                path: websocket_path(),
                token: None,
                token_env: None,
            });
        let ws_raw = if legacy_path {
            &raw["websocket"]
        } else {
            &raw["http"]["websocket"]
        };
        let path = &ws.path;
        ensure!(
            path.starts_with('/')
                && path[1..].split('/').all(valid_id)
                && !path.starts_with("/ingest/")
                && !path.starts_with("/api/")
                && !matches!(path.as_str(), "/livez" | "/readyz"),
            "invalid or reserved WebSocket path"
        );
        ensure!(
            raw["http"]["token"].is_null() || (ws_raw["token"].is_null() && ws.token_env.is_none()),
            "configure a single bearer token for HTTP and WebSocket"
        );
        let token = if !raw["http"]["token"].is_null() {
            required_secret(
                self.token.take(),
                None,
                &raw["http"]["token"],
                "http.token",
                environment,
            )?
        } else {
            required_secret(
                ws.token.take(),
                ws.token_env.take(),
                &ws_raw["token"],
                "http.token",
                environment,
            )?
        };
        ensure!(
            token.bytes().all(|byte| byte.is_ascii_graphic()),
            "HTTP bearer token must be printable ASCII without spaces"
        );
        self.token = Some(token);
        self.websocket = Some(ws);
        Ok(())
    }
}

fn required_secret(
    value: Option<String>,
    legacy: Option<String>,
    raw: &Value,
    field: &str,
    environment: &impl Fn(&str) -> Option<String>,
) -> Result<String> {
    resolve_secret(value, legacy, raw, field, environment)?
        .with_context(|| format!("configure {field}"))
}

fn resolve_secret(
    value: Option<String>,
    legacy: Option<String>,
    raw: &Value,
    field: &str,
    environment: &impl Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    ensure!(
        legacy.is_none() || raw.is_null(),
        "configure either {field} or its legacy environment alias"
    );
    let value = if let Some(name) = legacy {
        ensure!(
            environment_name(&name),
            "invalid environment variable for {field}"
        );
        Some(
            environment(&name)
                .with_context(|| format!("set the environment variable for {field}"))?,
        )
    } else {
        if value.is_none() && !raw.is_null() {
            anyhow::bail!("set the environment variable for {field}");
        }
        value
    };
    ensure!(
        value.as_deref().is_none_or(|secret| !secret.is_empty()),
        "{field} must be non-empty"
    );
    Ok(value)
}

pub fn environment_reference(value: &str) -> bool {
    value
        .strip_prefix("$")
        .and_then(|rest| rest.strip_prefix('{'))
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(environment_name)
}

fn environment_name(name: &str) -> bool {
    let mut chars = name.bytes();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && chars.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub fn advertised(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}{path}", base.path().trim_end_matches('/')));
    url
}

pub fn write_callbacks(
    path: &Path,
    raw: &str,
    document: Value,
) -> Result<(String, Option<std::io::Error>)> {
    let output = serde_json::to_string_pretty(&document)? + "\n";
    let parent = path
        .parent()
        .context("configuration has no parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(std::fs::metadata(path)?.permissions())?;
    file.write_all(output.as_bytes())?;
    file.as_file().sync_all()?;
    ensure!(
        std::fs::read_to_string(path)? == raw,
        "configuration changed on disk; restart the bridge"
    );
    file.persist(path).map_err(|error| error.error)?;
    // After the rename, the file is visible even if syncing its directory fails.
    #[cfg(unix)]
    let sync_error = std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .err();
    #[cfg(not(unix))]
    let sync_error = None;
    Ok((output, sync_error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_validates_and_callback_edit_preserves_environment_references() {
        let raw = include_str!("../bridge.example.json5");
        let env = |name: &str| match name {
            "CAMERA_BRIDGE_WS_TOKEN" => Some("fixture-ws-token".into()),
            "CAMERA_PASSWORD" => Some("fixture-password".into()),
            _ => None,
        };
        let document = document(raw).unwrap();
        let config = parse(document.clone(), &env).unwrap();
        assert_eq!(
            config.cameras[0].password.as_deref(),
            Some("fixture-password")
        );
        assert!(parse(document.clone(), &|_| None).is_err());
        assert!(parse(document.clone(), &|_| Some(String::new())).is_err());
        let mut previous = document.clone();
        let mqtt = previous["mqtt"].as_object_mut().unwrap();
        let topic = mqtt.remove("baseTopic").unwrap();
        mqtt.insert("prefix".into(), topic);
        let camera = previous["cameras"][0].as_object_mut().unwrap();
        camera.remove("password");
        camera.insert("passwordEnv".into(), "CAMERA_PASSWORD".into());
        assert_eq!(parse(previous, &env).unwrap().mqtt.base_topic, "cameras");

        let mut invalid = document.clone();
        for malformed in ["", "cameras/#", "cameras/+", "cameras//outdoor"] {
            invalid["mqtt"]["baseTopic"] = malformed.into();
            assert!(parse(invalid.clone(), &env).is_err());
        }

        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("bridge.json5");
        std::fs::write(&file, raw).unwrap();
        let mut edited = document.clone();
        edited["callbacks"] = serde_json::json!([]);
        write_callbacks(&file, raw, edited.clone()).unwrap();
        let output = std::fs::read_to_string(&file).unwrap();
        let reread = parse(super::document(&output).unwrap(), &env).unwrap();
        assert!(reread.callbacks.is_empty());
        assert_eq!(reread.cameras[0].id, config.cameras[0].id);
        assert!(output.contains("${CAMERA_PASSWORD}"));
        assert!(!output.contains("fixture-password"));
        assert!(write_callbacks(&file, raw, document).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), output);
    }
}
