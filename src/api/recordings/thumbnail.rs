use std::{process::Stdio, sync::Arc, time::Duration};

use anyhow::{Result, anyhow, ensure};
use axum::{
    Extension,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::{Instant, timeout},
};

use super::{Api, Recordings, SEARCH_TTL, error};
use crate::{
    http::CorrelationId,
    reolink::{recording_media, wire::Device},
};

const MAX_JPEG: usize = 512 * 1024;
const CACHE_BYTES: usize = 32 * 1024 * 1024;

pub(super) struct Thumbnail {
    jpeg: Bytes,
    accessed: Instant,
}

impl Recordings {
    pub(super) async fn clean_thumbnails(&self) {
        let now = Instant::now();
        let ttl = self.cache_ttl.min(SEARCH_TTL);
        self.thumbnails
            .lock()
            .await
            .retain(|_, item| now.duration_since(item.accessed) < ttl);
    }

    async fn thumbnail(&self, camera: &str, id: &str, device: &Device) -> Result<Bytes> {
        let key = format!("{camera}/{id}");
        let _camera = self
            .cameras
            .get(camera)
            .ok_or_else(|| anyhow!("unknown camera"))?
            .lock()
            .await;
        self.clean_thumbnails().await;
        {
            let mut cache = self.thumbnails.lock().await;
            if let Some(item) = cache.get_mut(&key) {
                item.accessed = Instant::now();
                return Ok(item.jpeg.clone());
            }
        }
        let clip = {
            let mut entries = self.entries.lock().await;
            self.prune(&mut entries, Instant::now());
            entries
                .get(&key)
                .ok_or_else(|| anyhow!("recording search expired"))?
                .clip
                .clone()
        };
        let _slot = self.slots.try_acquire()?;
        let (codec, data) = recording_media::cover(device, &clip).await?;
        let mut child = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-threads",
                "1",
                "-filter_threads",
                "1",
                "-f",
                codec,
                "-i",
                "pipe:0",
                "-frames:v",
                "1",
                "-vf",
                "scale=480:-2",
                "-f",
                "image2pipe",
                "-vcodec",
                "mjpeg",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let write = async {
            input.write_all(&data).await?;
            drop(input);
            Ok::<_, std::io::Error>(())
        };
        let read = async {
            let mut jpeg = Vec::new();
            output
                .take(MAX_JPEG as u64 + 1)
                .read_to_end(&mut jpeg)
                .await?;
            Ok::<_, std::io::Error>(jpeg)
        };
        let (_, jpeg, status) = tokio::try_join!(write, read, child.wait())?;
        ensure!(
            status.success()
                && jpeg.len() <= MAX_JPEG
                && jpeg.starts_with(&[0xff, 0xd8])
                && jpeg.ends_with(&[0xff, 0xd9]),
            "recording preview conversion failed"
        );
        let jpeg = Bytes::from(jpeg);
        let mut cache = self.thumbnails.lock().await;
        let mut size: usize = cache.values().map(|item| item.jpeg.len()).sum();
        while size + jpeg.len() > CACHE_BYTES || cache.len() >= 128 {
            let oldest = cache
                .iter()
                .min_by_key(|(_, item)| item.accessed)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { break };
            if let Some(item) = cache.remove(&oldest) {
                size -= item.jpeg.len();
            }
        }
        cache.insert(
            key,
            Thumbnail {
                jpeg: jpeg.clone(),
                accessed: Instant::now(),
            },
        );
        Ok(jpeg)
    }
}

pub(in crate::api) async fn get(
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
    {
        let mut entries = api.recordings.entries.lock().await;
        api.recordings.prune(&mut entries, Instant::now());
        if !entries.contains_key(&format!("{camera}/{id}")) {
            return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation);
        }
    }
    let result = tokio::select! {
        _ = api.stop.cancelled() => return error(StatusCode::SERVICE_UNAVAILABLE,"API_MEDIA_START_FAILED",&correlation),
        result = timeout(Duration::from_secs(35),api.recordings.thumbnail(&camera,&id,device)) => result,
    };
    match result {
        Ok(Ok(jpeg)) => (
            [
                ("content-type", "image/jpeg"),
                ("cache-control", "private, no-store"),
                ("x-content-type-options", "nosniff"),
            ],
            jpeg,
        )
            .into_response(),
        _ => error(
            StatusCode::BAD_GATEWAY,
            "API_MEDIA_START_FAILED",
            &correlation,
        ),
    }
}
