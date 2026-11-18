use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Result, ensure};
use axum::{
    Extension,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use redis::{AsyncConnectionConfig, FromRedisValue, aio::MultiplexedConnection};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex, Semaphore, watch},
    task::JoinSet,
    time::{Instant, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{Api, error};
use crate::{diagnostics::Diagnostics, http::CorrelationId, hub::Hub, reolink::wire::Device};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(35);
const CACHE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_JPEG_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Config {
    #[serde(default)]
    pub(super) cameras: BTreeMap<String, Profile>,
    #[serde(default)]
    cache: CacheConfig,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Profile {
    #[serde(default = "default_interval_ms")]
    pub(super) interval_ms: u64,
}

fn default_interval_ms() -> u64 {
    5_000
}

#[derive(Default, Deserialize)]
#[serde(tag = "backend", rename_all = "camelCase", deny_unknown_fields)]
enum CacheConfig {
    #[default]
    Memory,
    Valkey {
        url: Url,
        #[serde(rename = "keyPrefix", default = "default_key_prefix")]
        key_prefix: String,
    },
}

fn default_key_prefix() -> String {
    "reolink-bridge".into()
}

pub(super) type Previews = BTreeMap<String, Arc<Preview>>;

pub(super) fn store(config: &Config) -> Result<Option<Arc<Valkey>>> {
    Ok(match &config.cache {
        CacheConfig::Memory => None,
        CacheConfig::Valkey { url, key_prefix } => {
            ensure!(
                matches!(url.scheme(), "redis" | "rediss") && url.host().is_some(),
                "preview Valkey URL requires a Redis scheme and host"
            );
            ensure!(
                url.query().is_none() && url.fragment().is_none(),
                "preview Valkey URL contains a query or fragment"
            );
            ensure!(
                !key_prefix.is_empty()
                    && key_prefix.len() <= 80
                    && key_prefix
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b":_-".contains(&b)),
                "invalid preview Valkey key prefix"
            );
            let client = redis::Client::open(url.as_str())
                .map_err(|_| anyhow::anyhow!("invalid preview Valkey URL"))?;
            Some(Arc::new(Valkey {
                client,
                prefix: key_prefix.clone(),
                connection: Mutex::new(None),
                diagnostics: OnceLock::new(),
            }))
        }
    })
}

pub(super) fn prepare(
    config: &Config,
    devices: &BTreeMap<String, Device>,
    slots: Arc<Semaphore>,
    stop: CancellationToken,
    store: Option<Arc<Valkey>>,
) -> Result<Previews> {
    Ok(config
        .cameras
        .iter()
        .filter_map(|(id, profile)| {
            let device = devices.get(id)?.clone();
            let origin = Sha256::digest(format!(
                "{}:{}:{}",
                device.host, device.port, device.channel
            ));
            Some((
                id.clone(),
                Arc::new(Preview {
                    device,
                    camera_id: id.clone(),
                    hub: OnceLock::new(),
                    interval: Duration::from_millis(profile.interval_ms),
                    cache_id: format!("{id}:{origin:x}"),
                    store: store.clone(),
                    slots: slots.clone(),
                    stop: stop.clone(),
                    state: watch::channel(StateOfPreview::default()).0,
                    in_flight: Mutex::new(()),
                }),
            ))
        })
        .collect())
}

pub(super) fn start(previews: &Previews, hub: Arc<Hub>, workers: &mut JoinSet<()>) {
    for (id, preview) in previews {
        let _ = preview.hub.set(hub.clone());
        workers.spawn(preview.clone().run(id.clone(), hub.clone()));
    }
}

#[derive(Clone)]
struct Frame {
    jpeg: Bytes,
    captured_at: DateTime<Utc>,
    etag: String,
}

impl Frame {
    fn new(jpeg: Vec<u8>, captured_at: DateTime<Utc>) -> Option<Self> {
        if jpeg.len() > MAX_JPEG_BYTES
            || !jpeg.starts_with(&[0xff, 0xd8])
            || !jpeg.ends_with(&[0xff, 0xd9])
        {
            return None;
        }
        Some(Self {
            etag: format!("\"{:x}\"", Sha256::digest(&jpeg)),
            jpeg: Bytes::from(jpeg),
            captured_at,
        })
    }
}

#[derive(Clone, Default)]
struct StateOfPreview {
    attempt: u64,
    finished_at: Option<Instant>,
    succeeded: bool,
    frame: Option<Frame>,
}

pub(super) struct Preview {
    device: Device,
    camera_id: String,
    hub: OnceLock<Arc<Hub>>,
    interval: Duration,
    cache_id: String,
    store: Option<Arc<Valkey>>,
    slots: Arc<Semaphore>,
    stop: CancellationToken,
    state: watch::Sender<StateOfPreview>,
    in_flight: Mutex<()>,
}

impl Preview {
    async fn run(self: Arc<Self>, id: String, hub: Arc<Hub>) {
        if let Some(store) = &self.store {
            tokio::select! {
                _ = self.stop.cancelled() => return,
                frame = store.load(&self.cache_id) => {
                    if let Some(frame) = frame {
                        self.state.send_if_modified(|state| {
                            if state.frame.is_some() {
                                return false;
                            }
                            state.frame = Some(frame);
                            true
                        });
                    }
                }
            }
        }
        let mut updates = self.state.subscribe();
        loop {
            if let Some(frame) = &updates.borrow().frame {
                hub.section(
                    &id,
                    "state/preview",
                    "preview",
                    json!({
                        "etag":frame.etag, "captured_at":frame.captured_at,
                    }),
                );
            }
            let (next, previous) = {
                let state = updates.borrow_and_update();
                (
                    state
                        .finished_at
                        .map_or_else(Instant::now, |finished| finished + self.interval),
                    state.attempt,
                )
            };
            tokio::select! {
                _ = self.stop.cancelled() => return,
                _ = updates.changed() => {},
                _ = sleep_until(next) => { self.refresh(previous).await; }
            }
        }
    }

    async fn refresh(&self, previous: u64) -> Option<Frame> {
        let _lock = tokio::select! {
            _ = self.stop.cancelled() => return None,
            lock = self.in_flight.lock() => lock,
        };
        {
            let current = self.state.borrow();
            if current.attempt != previous {
                return current.succeeded.then(|| current.frame.clone()).flatten();
            }
        }
        let frame = tokio::select! {
            _ = self.stop.cancelled() => return None,
            result = timeout(CAPTURE_TIMEOUT, async {
                let _slot = self.slots.acquire().await?;
                self.device.capture_jpeg().await
            }) => {
                let result = result.map_err(anyhow::Error::from).and_then(|result| result)
                    .and_then(|(jpeg,captured_at)| Frame::new(jpeg,captured_at).ok_or_else(|| anyhow::anyhow!("invalid preview JPEG")));
                match result {
                    Ok(frame) => Some(frame),
                    Err(cause) => {
                        if let Some(hub) = self.hub.get() {
                            hub.diagnostics.error("api::preview::capture","API_PREVIEW_CAPTURE_FAILED",None,json!({"cameraId":self.camera_id}),Some(cause));
                        }
                        None
                    }
                }
            },
        };
        let finished = Instant::now();
        self.state.send_modify(|state| {
            state.attempt = state.attempt.wrapping_add(1);
            state.finished_at = Some(finished);
            state.succeeded = frame.is_some();
            if let Some(frame) = &frame {
                state.frame = Some(frame.clone());
            }
        });
        if let (Some(store), Some(frame)) = (&self.store, &frame) {
            tokio::select! {
                _ = self.stop.cancelled() => return None,
                _ = store.save(&self.cache_id, self.interval, frame) => {}
            }
        }
        frame
    }
}

pub(super) struct Valkey {
    client: redis::Client,
    prefix: String,
    connection: Mutex<Option<MultiplexedConnection>>,
    pub(super) diagnostics: OnceLock<Diagnostics>,
}

impl Valkey {
    pub(super) async fn recording(
        &self,
        id: &str,
        jpeg: Option<&Bytes>,
        ttl: Duration,
    ) -> Option<Bytes> {
        let key = format!("{}:recording-preview:{id}", self.prefix);
        if let Some(jpeg) = jpeg {
            let _: Option<()> = self
                .command(
                    redis::cmd("SET")
                        .arg(key)
                        .arg(jpeg.as_ref())
                        .arg("PX")
                        .arg(ttl.as_millis().max(1) as u64),
                )
                .await;
            return None;
        }
        let value: Vec<u8> = self
            .command(redis::cmd("GETRANGE").arg(key).arg(0).arg(512 * 1024))
            .await?;
        (value.len() <= 512 * 1024
            && value.starts_with(&[0xff, 0xd8])
            && value.ends_with(&[0xff, 0xd9]))
        .then(|| Bytes::from(value))
    }

    fn key(&self, id: &str) -> String {
        format!("{}:preview:{id}", self.prefix)
    }

    async fn command<T: FromRedisValue>(&self, cmd: &redis::Cmd) -> Option<T> {
        let result = timeout(CACHE_TIMEOUT, async {
            let mut connection = self.connection.lock().await;
            if connection.is_none() {
                let config = AsyncConnectionConfig::new().set_response_timeout(CACHE_TIMEOUT);
                *connection = Some(
                    self.client
                        .get_multiplexed_async_connection_with_config(&config)
                        .await?,
                );
            }
            let result = cmd.query_async(connection.as_mut().unwrap()).await;
            if result.is_err() {
                *connection = None;
            }
            result
        })
        .await;
        let kind = match result {
            Ok(Ok(value)) => return Some(value),
            Ok(Err(error)) => format!("{:?}", error.kind()),
            Err(_) => "timeout".into(),
        };
        if let Some(diagnostics) = self.diagnostics.get() {
            // Do not log Redis URLs, credentials, commands, or raw error messages.
            diagnostics.error(
                "api::preview::cache",
                "API_PREVIEW_CACHE_FAILED",
                None,
                json!({"backend":"valkey","failure":kind}),
                None,
            );
        }
        None
    }

    async fn load(&self, id: &str) -> Option<Frame> {
        let value: Vec<u8> = self.command(redis::cmd("GET").arg(self.key(id))).await?;
        let (stamp, jpeg) = value.split_at_checked(8)?;
        let captured_at =
            DateTime::from_timestamp_millis(i64::from_be_bytes(stamp.try_into().ok()?))?;
        Frame::new(jpeg.to_vec(), captured_at)
    }

    async fn save(&self, id: &str, interval: Duration, frame: &Frame) {
        let mut value = Vec::with_capacity(frame.jpeg.len() + 8);
        value.extend_from_slice(&frame.captured_at.timestamp_millis().to_be_bytes());
        value.extend_from_slice(&frame.jpeg);
        // Allow a warm restart and let unused camera keys expire.
        let ttl = (interval * 2).max(Duration::from_secs(86_400));
        let _: Option<()> = self
            .command(
                redis::cmd("SET")
                    .arg(self.key(id))
                    .arg(value)
                    .arg("PX")
                    .arg(ttl.as_millis() as u64),
            )
            .await;
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreviewQuery {
    #[serde(default)]
    refresh: bool,
}

pub(super) async fn get(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    Query(query): Query<PreviewQuery>,
    method: Method,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.read_access(&headers, &id) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(preview) = api.previews.get(&id) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
    };
    let frame = if query.refresh && method == Method::GET {
        let previous = preview.state.borrow().attempt;
        preview.refresh(previous).await
    } else {
        preview.state.borrow().frame.clone()
    };
    let Some(frame) = frame else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_PREVIEW_UNAVAILABLE",
            &correlation,
        );
    };
    let unchanged = headers
        .get_all("if-none-match")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == frame.etag);
    let (status, body) = if unchanged {
        (StatusCode::NOT_MODIFIED, Body::empty())
    } else {
        (StatusCode::OK, Body::from(frame.jpeg))
    };
    (
        status,
        [
            ("content-type", "image/jpeg".to_owned()),
            ("cache-control", "private, no-store".to_owned()),
            ("x-content-type-options", "nosniff".to_owned()),
            ("etag", frame.etag),
            ("x-preview-captured-at", frame.captured_at.to_rfc3339()),
        ],
        body,
    )
        .into_response()
}
