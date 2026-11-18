use std::{
    collections::BTreeMap, io::Write, path::PathBuf, process::Stdio, sync::Arc, time::Duration,
};

use anyhow::{Result, anyhow, ensure};
use axum::{
    Extension, Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{NaiveDate, Utc};
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

const TTL: Duration = Duration::from_secs(600);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Config {
    #[serde(default = "temporary_directory")]
    temporary_directory: Option<PathBuf>,
}
fn temporary_directory() -> Option<PathBuf> {
    Some(PathBuf::from("/tmp"))
}
impl Default for Config {
    fn default() -> Self {
        Self {
            temporary_directory: temporary_directory(),
        }
    }
}

pub(super) struct Recordings {
    directory: Option<PathBuf>,
    entries: Mutex<BTreeMap<String, Entry>>,
    cameras: BTreeMap<String, Mutex<()>>,
    slots: Semaphore,
}
struct Entry {
    clip: Clip,
    expires: Instant,
    media: Option<Arc<Media>>,
}
struct Media {
    _directory: tempfile::TempDir,
    path: PathBuf,
    size: u64,
}

impl Recordings {
    pub(super) fn new(config: &Config, ids: impl Iterator<Item = String>) -> Self {
        Self {
            directory: config.temporary_directory.clone(),
            entries: Mutex::default(),
            cameras: ids.map(|id| (id, Mutex::new(()))).collect(),
            slots: Semaphore::new(2),
        }
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
    pub(super) async fn clean(&self) {
        self.entries
            .lock()
            .await
            .retain(|_, entry| entry.expires > Instant::now());
    }
    async fn retain(&self, camera: &str, clips: &[Clip]) {
        let mut entries = self.entries.lock().await;
        entries.retain(|_, entry| entry.expires > Instant::now());
        while entries.len() + clips.len() > 8192 {
            let oldest = entries
                .iter()
                .min_by_key(|(_, e)| e.expires)
                .map(|(k, _)| k.clone());
            let Some(key) = oldest else { break };
            entries.remove(&key);
        }
        for clip in clips {
            let entry = entries
                .entry(format!("{camera}/{}", clip.id))
                .or_insert_with(|| Entry {
                    clip: clip.clone(),
                    expires: Instant::now() + TTL,
                    media: None,
                });
            entry.expires = Instant::now() + TTL;
        }
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
        let key = format!("{camera}/{id}");
        let _camera = self
            .cameras
            .get(camera)
            .ok_or_else(|| anyhow!("unknown camera"))?
            .lock()
            .await;
        let clip = {
            let entries = self.entries.lock().await;
            let entry = entries
                .get(&key)
                .filter(|e| e.expires > Instant::now())
                .ok_or_else(|| anyhow!("recording search expired"))?;
            if let Some(media) = &entry.media {
                return Ok(media.clone());
            }
            entry.clip.clone()
        };
        let _slot = self.slots.try_acquire()?;
        let parent = self
            .directory
            .as_ref()
            .ok_or_else(|| anyhow!("recording playback disabled"))?;
        let directory = tempfile::Builder::new()
            .prefix("reolink-recording-")
            .tempdir_in(parent)?;
        let input = directory.path().join("camera.ts");
        let path = directory.path().join("playback.mp4");
        let work = async {
            let mut output = File::create(&input).await?;
            recording_media::download(device, &clip, &mut output, &api.stop).await?;
            drop(output);
            let result = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-threads",
                    "1",
                    "-filter_threads",
                    "1",
                    "-fflags",
                    "+genpts",
                ])
                .arg("-i")
                .arg(&input)
                .args([
                    "-map",
                    "0:v:0",
                    "-map",
                    "0:a:0?",
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
                    "-c:a",
                    "aac",
                    "-movflags",
                    "+faststart",
                    "-f",
                    "mp4",
                ])
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status()
                .await?;
            ensure!(result.success(), "recording conversion failed");
            let size = tokio::fs::metadata(&path).await?.len();
            ensure!(
                (32..=768 * 1024 * 1024).contains(&size),
                "invalid recording size"
            );
            tokio::fs::remove_file(&input).await?;
            Ok::<_, anyhow::Error>(size)
        };
        let size = tokio::select! {
            _=api.stop.cancelled()=>return Err(anyhow!("shutdown")),
            result=timeout(Duration::from_secs(120),work)=>result??,
        };
        let media = Arc::new(Media {
            _directory: directory,
            path,
            size,
        });
        let mut entries = self.entries.lock().await;
        entries.retain(|_, entry| entry.expires > Instant::now());
        let mut bytes = entries
            .values()
            .filter_map(|e| e.media.as_ref())
            .map(|m| m.size)
            .sum::<u64>();
        while bytes + size > 2 * 1024 * 1024 * 1024 {
            let oldest = entries
                .iter()
                .filter(|(_, e)| e.media.is_some())
                .min_by_key(|(_, e)| e.expires)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { break };
            if let Some(media) = entries.get_mut(&oldest).and_then(|e| e.media.take()) {
                bytes = bytes.saturating_sub(media.size);
            }
        }
        if let Some(entry) = entries.get_mut(&key) {
            entry.media = Some(media.clone());
            entry.expires = Instant::now() + TTL;
        }
        Ok(media)
    }
}

pub(super) async fn storage(
    State(api): State<Arc<Api>>,
    Path(camera): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.read_access(&headers, &camera) {
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
    let result = timeout(Duration::from_secs(15), async {
        let mut session = device.connect().await?;
        let cards = recordings::storage(&mut session).await;
        let clock = recordings::clock(&mut session).await;
        session.logout().await;
        Ok::<_, anyhow::Error>((cards?, clock))
    })
    .await;
    match result {
        Ok(Ok((cards,clock)))=>Json(json!({"cards":cards,"playbackAvailable":api.recordings.available(),"cameraTime":clock,"refreshedAt":Utc::now(),"streams":["main","sub"],"correlation_id":correlation.0})).into_response(),
        _=>error(StatusCode::UNPROCESSABLE_ENTITY,"API_SETTING_READ_FAILED",&correlation),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Search {
    date: NaiveDate,
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
    if !api.read_access(&headers, &camera) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    if !matches!(query.stream.as_str(), "main" | "sub") {
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
            recordings::search(&mut session, device.channel, query.date, &query.stream).await;
        session.logout().await;
        result
    })
    .await;
    match result {
        Ok(Ok((mut clips, truncated))) => {
            clips.sort_by_key(|clip| clip.start);
            api.recordings.retain(&camera, &clips).await;
            Json(json!({"recordings":clips,"truncated":truncated,"date":query.date,"stream":query.stream,"correlation_id":correlation.0})).into_response()
        }
        _ => error(
            StatusCode::BAD_GATEWAY,
            "API_SETTING_READ_FAILED",
            &correlation,
        ),
    }
}

pub(super) async fn video(
    State(api): State<Arc<Api>>,
    Path((camera, id)): Path<(String, String)>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.read_access(&headers, &camera) {
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
        let entries = api.recordings.entries.lock().await;
        if !entries
            .get(&format!("{camera}/{id}"))
            .is_some_and(|e| e.expires > Instant::now())
        {
            return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation);
        }
    }
    let Ok(media) = api.recordings.media(&camera, &id, device, &api).await else {
        return error(
            StatusCode::BAD_GATEWAY,
            "API_MEDIA_START_FAILED",
            &correlation,
        );
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
        let recordings = Recordings::new(&config, std::iter::empty());
        assert!(recordings.available());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let disabled: Config = serde_json::from_value(json!({"temporaryDirectory":null})).unwrap();
        assert!(!Recordings::new(&disabled, std::iter::empty()).available());
        let missing: Config =
            serde_json::from_value(json!({"temporaryDirectory":directory.path().join("missing")}))
                .unwrap();
        assert!(!Recordings::new(&missing, std::iter::empty()).available());
        let file = directory.path().join("file");
        std::fs::write(&file, b"test").unwrap();
        let invalid: Config = serde_json::from_value(json!({"temporaryDirectory":file})).unwrap();
        assert!(!Recordings::new(&invalid, std::iter::empty()).available());
        let empty: Config = serde_json::from_value(json!({"temporaryDirectory":""})).unwrap();
        assert!(!Recordings::new(&empty, std::iter::empty()).available());
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
