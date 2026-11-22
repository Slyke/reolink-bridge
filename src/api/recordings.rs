mod thumbnail;
pub(super) use thumbnail::get as preview;

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Extension, Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{NaiveDate, NaiveTime, Utc};
use futures_util::stream;
use serde::Deserialize;
use serde_json::json;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
    process::Command,
    sync::{Mutex, Semaphore},
    time::{Instant, timeout},
};

use super::{Api, error};
use crate::{
    http::CorrelationId,
    reolink::{
        recording_media,
        recordings::{self, Clip},
        wire::Device,
    },
};

#[derive(Debug)]
struct PreparationStage(&'static str);
impl std::fmt::Display for PreparationStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for PreparationStage {}

const SEARCH_TTL: Duration = Duration::from_secs(600);
const MAX_ENTRIES: usize = 8192;
const MAX_MEDIA_BYTES: u64 = 768 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Config {
    #[serde(default = "temporary_directory")]
    temporary_directory: Option<PathBuf>,
    #[serde(default)]
    cache: CacheConfig,
}

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct CacheConfig {
    max_bytes: u64,
    ttl_seconds: u64,
}
impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 2 * 1024 * 1024 * 1024,
            ttl_seconds: 600,
        }
    }
}
fn temporary_directory() -> Option<PathBuf> {
    Some(PathBuf::from("/tmp"))
}
impl Default for Config {
    fn default() -> Self {
        Self {
            temporary_directory: temporary_directory(),
            cache: CacheConfig::default(),
        }
    }
}

pub(super) struct Recordings {
    directory: Option<PathBuf>,
    cache_ttl: Duration,
    max_cache_bytes: u64,
    entries: Mutex<BTreeMap<String, Entry>>,
    thumbnails: Mutex<BTreeMap<String, thumbnail::Thumbnail>>,
    cover_unavailable: Mutex<BTreeMap<String, Instant>>,
    pub(super) thumbnail_store: Option<Arc<super::preview::Valkey>>,
    cameras: BTreeMap<String, Mutex<()>>,
    slots: Semaphore,
}
struct Entry {
    clip: Clip,
    expires: Instant,
    media: Option<CachedMedia>,
}
struct CachedMedia {
    file: Arc<Media>,
    accessed: Instant,
}
struct Media {
    metadata: recording_media::RecordingMetadata,
    _directory: tempfile::TempDir,
    path: PathBuf,
    size: u64,
}

impl Media {
    fn cached_bytes(&self) -> u64 {
        self.size + self.metadata.bytes
    }

    async fn detection_page(&self, page: usize) -> Result<Option<serde_json::Value>> {
        let Some((range, next)) = self.metadata.page(page) else {
            return Ok(None);
        };
        let mut file = File::open(self.path.with_file_name("detections.jsonl")).await?;
        file.seek(std::io::SeekFrom::Start(range.start)).await?;
        let mut raw = vec![0; (range.end - range.start) as usize];
        file.read_exact(&mut raw).await?;
        let frames = raw
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(serde_json::from_slice::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(
            json!({"source":"recording","frames":frames,"truncated":next.is_some(),"nextPage":next}),
        ))
    }
}

impl Recordings {
    pub(super) fn new(config: &Config, ids: impl Iterator<Item = String>) -> Result<Self> {
        let cache_ttl = Duration::from_secs(config.cache.ttl_seconds);
        ensure!(
            !cache_ttl.is_zero() && Instant::now().checked_add(cache_ttl).is_some(),
            "recording cache ttlSeconds must be positive and supported by the system clock"
        );
        ensure!(
            config.cache.max_bytes >= 32,
            "recording cache maxBytes must be at least 32"
        );
        Ok(Self {
            directory: config.temporary_directory.clone(),
            cache_ttl,
            max_cache_bytes: config.cache.max_bytes,
            entries: Mutex::default(),
            thumbnails: Mutex::default(),
            cover_unavailable: Mutex::default(),
            thumbnail_store: None,
            cameras: ids.map(|id| (id, Mutex::new(()))).collect(),
            slots: Semaphore::new(2),
        })
    }
    pub(super) fn available(&self) -> bool {
        let Some(directory) = &self.directory else {
            return false;
        };
        if directory.as_os_str().is_empty() {
            return false;
        }
        tempfile::Builder::new()
            .prefix(".reolink-recording-check-")
            .tempfile_in(directory)
            .and_then(|mut file| file.write_all(b"recording workspace check"))
            .is_ok()
    }
    pub(super) fn cleanup_interval(&self) -> Duration {
        self.cache_ttl.min(Duration::from_secs(60))
    }
    fn prune(&self, entries: &mut BTreeMap<String, Entry>, now: Instant) {
        entries.retain(|_, entry| {
            if entry
                .media
                .as_ref()
                .is_some_and(|cached| now.duration_since(cached.accessed) >= self.cache_ttl)
            {
                entry.media = None;
            }
            entry.expires > now || entry.media.is_some()
        });
    }
    fn trim_catalog(entries: &mut BTreeMap<String, Entry>, protected: impl Fn(&str) -> bool) {
        while entries.len() > MAX_ENTRIES {
            let oldest = entries
                .iter()
                .filter(|(key, _)| !protected(key))
                .min_by_key(|(_, entry)| {
                    (
                        entry.media.is_some(),
                        entry
                            .media
                            .as_ref()
                            .map_or(entry.expires, |cached| cached.accessed),
                    )
                })
                .map(|(key, _)| key.clone());
            let Some(key) = oldest else { break };
            entries.remove(&key);
        }
    }
    pub(super) async fn forget(&self, scope: &str, ids: &BTreeSet<&str>) {
        let mut entries = self.entries.lock().await;
        for id in ids {
            entries.remove(&format!("{scope}/{id}"));
        }
    }
    pub(super) async fn forget_camera(&self, scope: &str) {
        let prefix = format!("{scope}/");
        self.entries
            .lock()
            .await
            .retain(|key, _| !key.starts_with(&prefix));
    }

    pub(super) async fn clean(&self) {
        self.prune(&mut *self.entries.lock().await, Instant::now());
        self.clean_thumbnails().await;
    }
    async fn retain(&self, camera: &str, clips: &[Clip]) {
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        self.prune(&mut entries, now);
        let mut refreshed = BTreeSet::new();
        for clip in clips {
            let key = format!("{camera}/{}", clip.id);
            refreshed.insert(key.clone());
            let entry = entries.entry(key).or_insert_with(|| Entry {
                clip: clip.clone(),
                expires: now + SEARCH_TTL,
                media: None,
            });
            entry.expires = now + SEARCH_TTL;
        }
        Self::trim_catalog(&mut entries, |key| refreshed.contains(key));
    }
    async fn lookup(&self, key: &str) -> Result<(Clip, Option<Arc<Media>>)> {
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        self.prune(&mut entries, now);
        let entry = entries
            .get_mut(key)
            .ok_or_else(|| anyhow!("recording search expired"))?;
        entry.expires = now + SEARCH_TTL;
        let media = entry.media.as_mut().map(|cached| {
            cached.accessed = now;
            cached.file.clone()
        });
        Ok((entry.clip.clone(), media))
    }
    async fn cache(&self, key: String, clip: Clip, media: Arc<Media>) -> Result<Arc<Media>> {
        ensure!(
            media.cached_bytes() <= self.max_cache_bytes,
            PreparationStage("cache-limit")
        );
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        self.prune(&mut entries, now);
        if let Some(entry) = entries.get_mut(&key) {
            entry.media = None;
        }
        let mut bytes = entries
            .values()
            .filter_map(|entry| entry.media.as_ref())
            .map(|cached| cached.file.cached_bytes())
            .sum::<u64>();
        while bytes > self.max_cache_bytes - media.cached_bytes() {
            let oldest = entries
                .iter()
                .filter_map(|(key, entry)| entry.media.as_ref().map(|cached| (key, cached)))
                .min_by_key(|(_, cached)| cached.accessed)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { break };
            if let Some(cached) = entries
                .get_mut(&oldest)
                .and_then(|entry| entry.media.take())
            {
                bytes -= cached.file.cached_bytes();
            }
        }
        entries.insert(
            key.clone(),
            Entry {
                clip,
                expires: now + SEARCH_TTL,
                media: Some(CachedMedia {
                    file: media.clone(),
                    accessed: now,
                }),
            },
        );
        Self::trim_catalog(&mut entries, |candidate| candidate == key);
        Ok(media)
    }
    async fn media(
        &self,
        camera: &str,
        id: &str,
        device: &Device,
        api: &Api,
    ) -> Result<Arc<Media>> {
        ensure!(
            id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid recording"
        );
        let key = format!("{}/{id}", api.recording_scope(camera));
        let _camera = self
            .cameras
            .get(camera)
            .ok_or_else(|| anyhow!("unknown camera"))?
            .lock()
            .await;
        let (clip, media) = self.lookup(&key).await?;
        if let Some(media) = media {
            return Ok(media);
        }
        let _slot = self.slots.try_acquire().context(PreparationStage("busy"))?;
        let parent = self
            .directory
            .as_ref()
            .ok_or_else(|| anyhow!("recording playback disabled"))?;
        let directory = tempfile::Builder::new()
            .prefix("reolink-recording-")
            .tempdir_in(parent)
            .context(PreparationStage("temporary-storage"))?;
        let input = directory.path().join("camera.ts");
        let path = directory.path().join("playback.mp4");
        let work = async {
            let mut output = File::create(&input)
                .await
                .context(PreparationStage("temporary-storage"))?;
            let mut detections = tokio::io::BufWriter::new(
                File::create(directory.path().join("detections.jsonl"))
                    .await
                    .context(PreparationStage("temporary-storage"))?,
            );
            let metadata =
                recording_media::download(device, &clip, &mut output, &mut detections, &api.stop)
                    .await
                    .context(PreparationStage("camera-download"))?;
            drop(output);
            let size = async {
                let copy = can_remux_video(&input).await;
                if !copy || !write_playback(&input, &path, true).await? {
                    ensure!(
                        write_playback(&input, &path, false).await?,
                        "recording conversion failed"
                    );
                }
                let size = tokio::fs::metadata(&path).await?.len();
                ensure!(
                    (32..=MAX_MEDIA_BYTES).contains(&size),
                    "invalid recording size"
                );
                Ok::<_, anyhow::Error>(size)
            }
            .await
            .context(PreparationStage("conversion"))?;
            tokio::fs::remove_file(&input)
                .await
                .context(PreparationStage("temporary-storage"))?;
            Ok::<_, anyhow::Error>((size, metadata))
        };
        let (size, metadata) = tokio::select! {
            _=api.stop.cancelled()=>return Err(anyhow!("shutdown")),
            result=timeout(Duration::from_secs(120),work)=>result.context(PreparationStage("timeout"))??,
        };
        let media = Arc::new(Media {
            metadata,
            _directory: directory,
            path,
            size,
        });
        self.cache(key, clip, media).await
    }
}

// The camera transfer already contains a timestamped video track. Copy ordinary
// browser-compatible H.264 so metadata and playback do not wait for a full transcode.
async fn can_remux_video(input: &std::path::Path) -> bool {
    let Ok(probe) = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,pix_fmt,has_b_frames",
            "-of",
            "json",
        ])
        .arg(input)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
    else {
        return false;
    };
    if !probe.status.success() {
        return false;
    }
    let Ok(info) = serde_json::from_slice::<serde_json::Value>(&probe.stdout) else {
        return false;
    };
    let stream = &info["streams"][0];
    stream["codec_name"] == "h264" && stream["pix_fmt"] == "yuv420p" && stream["has_b_frames"] == 0
}

async fn write_playback(
    input: &std::path::Path,
    output: &std::path::Path,
    copy_video: bool,
) -> std::io::Result<bool> {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-fflags",
            "+genpts",
            "-i",
        ])
        .arg(input)
        .args(["-map", "0:v:0", "-map", "0:a:0?"]);
    if copy_video {
        command.args(["-c:v", "copy"]);
    } else {
        command.args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "23",
            "-pix_fmt",
            "yuv420p",
            "-threads",
            "1",
        ]);
    }
    Ok(command
        .args(["-c:a", "aac", "-movflags", "+faststart", "-f", "mp4"])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await?
        .success())
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct MetadataQuery {
    page: usize,
}

pub(super) async fn metadata(
    State(api): State<Arc<Api>>,
    Path((camera, id)): Path<(String, String)>,
    Query(query): Query<MetadataQuery>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(device) = api.devices.get(&camera) else {
        return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation);
    };
    if !api.recordings.available() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
    }
    if api
        .recordings
        .lookup(&format!("{}/{id}", api.recording_scope(&camera)))
        .await
        .is_err()
    {
        return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation);
    }
    let result = async {
        let media = api.recordings.media(&camera, &id, device, &api).await?;
        media.detection_page(query.page).await
    }
    .await;
    match result {
        Ok(Some(page)) => Json(page).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation),
        Err(cause) => {
            if let Some(hub) = &api.hub {
                hub.diagnostics.error(
                    "api::recordings::metadata",
                    "API_RECORDING_METADATA_FAILED",
                    Some(correlation.0.clone()),
                    json!({"cameraId":camera,"recordingId":id}),
                    Some(cause),
                );
            }
            error(
                StatusCode::BAD_GATEWAY,
                "API_RECORDING_METADATA_FAILED",
                &correlation,
            )
        }
    }
}

pub(super) fn read_failure(
    api: &Api,
    correlation: &CorrelationId,
    camera: &str,
    caller: &str,
    key: &'static str,
    cause: anyhow::Error,
) -> Response {
    let status = if cause.is::<tokio::time::error::Elapsed>() {
        StatusCode::GATEWAY_TIMEOUT
    } else {
        StatusCode::BAD_GATEWAY
    };
    if let Some(hub) = &api.hub {
        hub.diagnostics.error(
            caller,
            key,
            Some(correlation.0.clone()),
            json!({"cameraId":camera,"httpStatus":status.as_u16()}),
            Some(cause),
        );
    }
    error(status, key, correlation)
}

#[derive(Default, Deserialize)]
pub(super) struct StorageOptions {
    #[serde(default)]
    format: bool,
}

pub(super) async fn storage(
    State(api): State<Arc<Api>>,
    Path(camera): Path<String>,
    Query(options): Query<StorageOptions>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(device) = api.devices.get(&camera) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_READ_BUSY",
            &correlation,
        );
    };
    let read = || async {
        let mut session = device.connect().await?;
        match recordings::storage(&mut session).await {
            Ok(cards) => {
                let clock = recordings::clock(&mut session).await;
                Ok((cards, clock, session))
            }
            Err(cause) => {
                session.logout().await;
                Err(cause)
            }
        }
    };
    let result = timeout(Duration::from_secs(15), async {
        match read().await {
            Err(cause)
                if cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                    )
                }) =>
            {
                tokio::time::sleep(Duration::from_millis(200)).await;
                read().await
            }
            result => result,
        }
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    match result {
        Ok((mut cards, clock, mut session)) => {
            if options.format && cards.iter().any(|card| card.explicit_sd_slot) {
                let check = timeout(
                    Duration::from_secs(8),
                    super::storage_format::identify(&mut session, device, &mut cards),
                )
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
                if let Err(cause) = check
                    && let Some(hub) = &api.hub
                {
                    hub.diagnostics.error(
                        "api::storage_format::identify",
                        "API_STORAGE_FORMAT_CHECK_FAILED",
                        Some(correlation.0.clone()),
                        json!({"cameraId":camera}),
                        Some(cause),
                    );
                }
            }
            session.logout().await;
            let clock = match clock {
                Ok(time) => Some(time),
                Err(cause) => {
                    if let Some(hub) = &api.hub {
                        hub.diagnostics.error(
                            "api::recordings::clock",
                            "API_CAMERA_CLOCK_READ_FAILED",
                            Some(correlation.0.clone()),
                            json!({"cameraId":camera}),
                            Some(cause),
                        );
                    }
                    None
                }
            };
            Json(json!({"cards":cards,"playbackAvailable":api.recordings.available(),"cameraTime":clock,"refreshedAt":Utc::now(),"streams":["main","sub"],"correlation_id":correlation.0})).into_response()
        }
        Err(cause) => read_failure(
            &api,
            &correlation,
            &camera,
            "api::recordings::storage",
            "API_STORAGE_READ_FAILED",
            cause,
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Search {
    date: NaiveDate,
    from: Option<NaiveTime>,
    to: Option<NaiveTime>,
    #[serde(default = "substream")]
    stream: String,
}
fn substream() -> String {
    "sub".into()
}

pub(super) async fn list(
    State(api): State<Arc<Api>>,
    Path(camera): Path<String>,
    Query(query): Query<Search>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let start = query.date.and_time(query.from.unwrap_or(NaiveTime::MIN));
    let end = query.date.and_time(
        query
            .to
            .unwrap_or(NaiveTime::from_hms_opt(23, 59, 59).unwrap()),
    );
    if start > end || !matches!(query.stream.as_str(), "main" | "sub") {
        return error(
            StatusCode::BAD_REQUEST,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
    }
    let Some(device) = api.devices.get(&camera) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_READ_BUSY",
            &correlation,
        );
    };
    let result = timeout(Duration::from_secs(45), async {
        let mut session = device.connect().await?;
        let result =
            recordings::search(&mut session, device.channel, start, end, &query.stream).await;
        session.logout().await;
        result
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    match result {
        Ok((mut clips, truncated)) => {
            clips.sort_by_key(|clip| clip.start);
            api.recordings
                .retain(&api.recording_scope(&camera), &clips)
                .await;
            Json(json!({"recordings":clips,"truncated":truncated,"date":query.date,"stream":query.stream,"correlation_id":correlation.0})).into_response()
        }
        Err(cause) => read_failure(
            &api,
            &correlation,
            &camera,
            "api::recordings::list",
            "API_RECORDING_SEARCH_FAILED",
            cause,
        ),
    }
}

pub(super) async fn video(
    State(api): State<Arc<Api>>,
    Path((camera, id)): Path<(String, String)>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(device) = api.devices.get(&camera) else {
        return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation);
    };
    if !api.recordings.available() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
    }
    {
        let mut entries = api.recordings.entries.lock().await;
        api.recordings.prune(&mut entries, Instant::now());
        if !entries.contains_key(&format!("{}/{id}", api.recording_scope(&camera))) {
            return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation);
        }
    }
    let media = match api.recordings.media(&camera, &id, device, &api).await {
        Ok(media) => media,
        Err(failure) => {
            let stage = failure
                .downcast_ref::<PreparationStage>()
                .map_or("preparation", |stage| stage.0);
            if let Some(hub) = &api.hub {
                hub.diagnostics.error(
                    "api::recordings::video",
                    "API_RECORDING_PREPARE_FAILED",
                    Some(correlation.0.clone()),
                    json!({"cameraId":camera,"recordingId":id,"stage":stage}),
                    Some(failure),
                );
            }
            let status = match stage {
                "cache-limit" => StatusCode::PAYLOAD_TOO_LARGE,
                "busy" => StatusCode::SERVICE_UNAVAILABLE,
                "timeout" => StatusCode::GATEWAY_TIMEOUT,
                _ => StatusCode::BAD_GATEWAY,
            };
            return (status, Json(json!({"ok":false,"errorKey":"API_RECORDING_PREPARE_FAILED","stage":stage,"correlation_id":correlation.0}))).into_response();
        }
    };
    let requested = headers.get("range").and_then(|h| h.to_str().ok());
    let Some((start, end)) = byte_range(requested, media.size) else {
        return (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [("content-range", format!("bytes */{}", media.size))],
        )
            .into_response();
    };
    let Ok(mut file) = File::open(&media.path).await else {
        return error(
            StatusCode::BAD_GATEWAY,
            "API_MEDIA_START_FAILED",
            &correlation,
        );
    };
    if file.seek(SeekFrom::Start(start)).await.is_err() {
        return error(
            StatusCode::BAD_GATEWAY,
            "API_MEDIA_START_FAILED",
            &correlation,
        );
    }
    let length = end - start + 1;
    let size = media.size;
    let state = (file, length, media);
    let bytes = stream::try_unfold(state, |(mut file, left, media)| async move {
        if left == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        let mut buffer = vec![0; left.min(64 * 1024) as usize];
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buffer.truncate(read);
        Ok(Some((buffer, (file, left - read as u64, media))))
    });
    let mut response = Body::from_stream(bytes).into_response();
    *response.status_mut() = if requested.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let h = response.headers_mut();
    for (key, value) in [
        ("content-type", "video/mp4".into()),
        ("accept-ranges", "bytes".into()),
        ("content-length", length.to_string()),
        ("cache-control", "private, no-store".into()),
        ("x-content-type-options", "nosniff".into()),
    ] {
        h.insert(
            axum::http::HeaderName::from_static(key),
            value.parse().unwrap(),
        );
    }
    if requested.is_some() {
        h.insert(
            "content-range",
            format!("bytes {start}-{end}/{size}").parse().unwrap(),
        );
    }
    response
}

fn byte_range(header: Option<&str>, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let Some(header) = header else {
        return Some((0, size - 1));
    };
    let (start, end) = header.strip_prefix("bytes=")?.split_once('-')?;
    if end.contains(',') {
        return None;
    }
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(size);
        return (suffix > 0).then_some((size - suffix, size - 1));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (start <= end && start < size).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playback_requires_a_writable_configured_directory() {
        let defaults: Config = serde_json::from_value(json!({})).unwrap();
        assert_eq!(defaults.temporary_directory, Some(PathBuf::from("/tmp")));
        let directory = tempfile::tempdir().unwrap();
        let config: Config =
            serde_json::from_value(json!({"temporaryDirectory":directory.path()})).unwrap();
        let recordings = Recordings::new(&config, std::iter::empty()).unwrap();
        assert!(recordings.available());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let disabled: Config = serde_json::from_value(json!({"temporaryDirectory":null})).unwrap();
        assert!(
            !Recordings::new(&disabled, std::iter::empty())
                .unwrap()
                .available()
        );
        let missing: Config =
            serde_json::from_value(json!({"temporaryDirectory":directory.path().join("missing")}))
                .unwrap();
        assert!(
            !Recordings::new(&missing, std::iter::empty())
                .unwrap()
                .available()
        );
        let file = directory.path().join("file");
        std::fs::write(&file, b"test").unwrap();
        let invalid: Config = serde_json::from_value(json!({"temporaryDirectory":file})).unwrap();
        assert!(
            !Recordings::new(&invalid, std::iter::empty())
                .unwrap()
                .available()
        );
        let empty: Config = serde_json::from_value(json!({"temporaryDirectory":""})).unwrap();
        assert!(
            !Recordings::new(&empty, std::iter::empty())
                .unwrap()
                .available()
        );
    }

    fn clip(id: &str) -> Clip {
        let start = NaiveDate::from_ymd_opt(2026, 11, 17)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        Clip {
            file_identity: None,
            id: id.into(),
            start,
            end: start + chrono::Duration::seconds(10),
            stream: "sub".into(),
            size_bytes: None,
            kind: None,
        }
    }

    fn media(directory: &std::path::Path, size: u64) -> Arc<Media> {
        let directory = tempfile::tempdir_in(directory).unwrap();
        let path = directory.path().join("playback.mp4");
        std::fs::File::create(&path).unwrap().set_len(size).unwrap();
        Arc::new(Media {
            metadata: Default::default(),
            _directory: directory,
            path,
            size,
        })
    }

    #[test]
    fn cache_limits_have_defaults_and_reject_invalid_values() {
        let defaults = Recordings::new(&Config::default(), std::iter::empty()).unwrap();
        assert_eq!(defaults.cache_ttl, Duration::from_secs(600));
        assert_eq!(defaults.max_cache_bytes, 2_147_483_648);
        for (value, seconds, bytes) in [
            (json!({"cache":{"ttlSeconds":3600}}), 3600, 2_147_483_648),
            (json!({"cache":{"maxBytes":1_048_576}}), 600, 1_048_576),
        ] {
            let config: Config = serde_json::from_value(value).unwrap();
            let recordings = Recordings::new(&config, std::iter::empty()).unwrap();
            assert_eq!(recordings.cache_ttl, Duration::from_secs(seconds));
            assert_eq!(recordings.max_cache_bytes, bytes);
        }
        for value in [
            json!({"cache":{"ttlSeconds":0}}),
            json!({"cache":{"ttlSeconds":u64::MAX}}),
            json!({"cache":{"maxBytes":31}}),
        ] {
            let config: Config = serde_json::from_value(value).unwrap();
            assert!(Recordings::new(&config, std::iter::empty()).is_err());
        }
        for value in [
            json!({"cache":{"ttlSeconds":-1}}),
            json!({"cache":{"maxBytes":1.5}}),
            json!({"cache":{"ttl":600}}),
        ] {
            assert!(serde_json::from_value::<Config>(value).is_err());
        }
    }

    #[tokio::test]
    async fn cache_size_is_shared_and_evicts_least_recently_played_clips() {
        let directory = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(json!({"cache":{"maxBytes":128}})).unwrap();
        let recordings = Recordings::new(&config, std::iter::empty()).unwrap();
        let first = media(directory.path(), 64);
        let first_path = first.path.clone();
        recordings
            .cache("one/a".into(), clip("a"), first)
            .await
            .unwrap();
        let second = recordings
            .cache("two/b".into(), clip("b"), media(directory.path(), 64))
            .await
            .unwrap();
        let second_path = second.path.clone();
        {
            let mut entries = recordings.entries.lock().await;
            entries
                .get_mut("one/a")
                .unwrap()
                .media
                .as_mut()
                .unwrap()
                .accessed = Instant::now() - Duration::from_secs(30);
            entries
                .get_mut("two/b")
                .unwrap()
                .media
                .as_mut()
                .unwrap()
                .accessed = Instant::now() - Duration::from_secs(20);
        }
        recordings.lookup("one/a").await.unwrap();
        recordings
            .cache("three/c".into(), clip("c"), media(directory.path(), 64))
            .await
            .unwrap();
        let entries = recordings.entries.lock().await;
        assert!(entries["one/a"].media.is_some());
        assert!(entries["two/b"].media.is_none());
        assert!(entries["three/c"].media.is_some());
        assert_eq!(
            entries
                .values()
                .filter_map(|entry| entry.media.as_ref())
                .map(|cached| cached.file.size)
                .sum::<u64>(),
            128
        );
        assert!(first_path.exists());
        // A response already using an evicted clip can finish before its file is removed.
        assert!(second_path.exists());
        drop(second);
        assert!(!second_path.exists());
    }

    #[tokio::test]
    async fn a_full_cache_keeps_new_recording_search_results_playable() {
        let directory = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(&Config::default(), std::iter::empty()).unwrap();
        let file = media(directory.path(), 64);
        {
            let mut entries = recordings.entries.lock().await;
            for index in 0..MAX_ENTRIES {
                entries.insert(
                    format!("one/{index}"),
                    Entry {
                        clip: clip(&index.to_string()),
                        expires: Instant::now() + SEARCH_TTL,
                        media: Some(CachedMedia {
                            file: file.clone(),
                            accessed: Instant::now(),
                        }),
                    },
                );
            }
        }
        recordings.retain("two", &[clip("new")]).await;
        assert!(recordings.lookup("two/new").await.is_ok());
        assert_eq!(recordings.entries.lock().await.len(), MAX_ENTRIES);
    }

    #[tokio::test]
    async fn searching_does_not_keep_expired_video_cached() {
        let directory = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(json!({"cache":{"ttlSeconds":5}})).unwrap();
        let recordings = Recordings::new(&config, std::iter::empty()).unwrap();
        let file = media(directory.path(), 64);
        let path = file.path.clone();
        recordings
            .cache("one/a".into(), clip("a"), file)
            .await
            .unwrap();
        recordings
            .entries
            .lock()
            .await
            .get_mut("one/a")
            .unwrap()
            .media
            .as_mut()
            .unwrap()
            .accessed = Instant::now() - Duration::from_secs(6);
        recordings.retain("one", &[clip("a")]).await;
        assert!(!path.exists());
        assert!(recordings.lookup("one/a").await.unwrap().1.is_none());
    }

    #[tokio::test]
    async fn playback_renews_cache_expiry_independently_of_search_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(json!({"cache":{"ttlSeconds":1200}})).unwrap();
        let recordings = Recordings::new(&config, std::iter::empty()).unwrap();
        let file = media(directory.path(), 64);
        let path = file.path.clone();
        recordings
            .cache("one/a".into(), clip("a"), file)
            .await
            .unwrap();
        {
            let mut entries = recordings.entries.lock().await;
            let entry = entries.get_mut("one/a").unwrap();
            entry.expires = Instant::now() - Duration::from_secs(1);
            entry.media.as_mut().unwrap().accessed = Instant::now() - Duration::from_secs(1199);
        }
        let active = recordings.lookup("one/a").await.unwrap().1.unwrap();
        let mut entries = recordings.entries.lock().await;
        recordings.prune(&mut entries, Instant::now() + Duration::from_secs(2));
        assert!(entries["one/a"].media.is_some());
        recordings.prune(&mut entries, Instant::now() + Duration::from_secs(1201));
        assert!(!entries.contains_key("one/a"));
        assert!(path.exists());
        drop(active);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn oversized_clip_is_discarded_without_evicting_cached_clips() {
        let directory = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(json!({"cache":{"maxBytes":128}})).unwrap();
        let recordings = Recordings::new(&config, std::iter::empty()).unwrap();
        recordings
            .cache("one/a".into(), clip("a"), media(directory.path(), 128))
            .await
            .unwrap();
        let file = media(directory.path(), 129);
        let path = file.path.clone();
        assert!(
            recordings
                .cache("two/b".into(), clip("b"), file)
                .await
                .is_err()
        );
        assert!(!path.exists());
        let entries = recordings.entries.lock().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries["one/a"].media.as_ref().unwrap().file.size, 128);
    }

    #[test]
    fn ranges_follow_http_video_requests() {
        assert_eq!(byte_range(None, 100), Some((0, 99)));
        assert_eq!(byte_range(Some("bytes=20-39"), 100), Some((20, 39)));
        assert_eq!(byte_range(Some("bytes=-10"), 100), Some((90, 99)));
        assert_eq!(byte_range(Some("bytes=50-"), 100), Some((50, 99)));
        assert_eq!(byte_range(Some("bytes=100-"), 100), None);
        assert_eq!(byte_range(Some("bytes=0-1,4-5"), 100), None);
    }
}
