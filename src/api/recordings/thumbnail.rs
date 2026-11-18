use std::{process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Extension,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::{Instant, timeout},
};

use super::{Api, Media, Recordings, SEARCH_TTL, error};
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

    async fn thumbnail(
        &self,
        camera: &str,
        id: &str,
        offset_ms: u64,
        device: &Device,
        api: &Api,
        correlation: &CorrelationId,
    ) -> Result<Bytes> {
        let catalog_key = format!("{camera}/{id}");
        let _camera = self
            .cameras
            .get(camera)
            .ok_or_else(|| anyhow!("unknown camera"))?
            .lock()
            .await;
        let (mut clip, media) = self.lookup(&catalog_key).await?;
        // One-second buckets make neighbouring timeline positions share the same frame.
        let offset_ms = (offset_ms / 1000 * 1000).min(
            (clip.end - clip.start)
                .num_milliseconds()
                .saturating_sub(1000)
                .max(0) as u64,
        );
        let origin = Sha256::digest(format!(
            "{}:{}:{}",
            device.host, device.port, device.channel
        ));
        let stable = api
            .camera_identity(camera)
            .context("unknown camera identity")?;
        let key = format!("{stable}:{origin:x}:{id}:{offset_ms}");
        self.clean_thumbnails().await;
        {
            let mut cache = self.thumbnails.lock().await;
            if let Some(item) = cache.get_mut(&key) {
                item.accessed = Instant::now();
                return Ok(item.jpeg.clone());
            }
        }
        let ttl = self.cache_ttl.min(SEARCH_TTL);
        if let Some(store) = &self.thumbnail_store
            && let Some(jpeg) = store.recording(&key, None, ttl).await
        {
            self.cache_thumbnail(key, jpeg.clone()).await;
            return Ok(jpeg);
        }
        let _slot = self.slots.try_acquire()?;
        let jpeg = if let Some(media) = media {
            convert(None, Some(media), offset_ms).await?
        } else {
            clip.start += chrono::Duration::milliseconds(offset_ms as i64);
            // CoverPreview is optional. The ordinary read-only recording download works
            // on older models too; stop it as soon as the first keyframe arrives.
            let until = self.cover_unavailable.lock().await.get(camera).copied();
            let jpeg = if until.is_none_or(|until| Instant::now() >= until) {
                let cover = timeout(Duration::from_secs(5), async {
                    let frame = recording_media::cover(device, &clip).await?;
                    convert(Some(frame), None, 0).await
                })
                .await;
                match cover {
                    Ok(Ok(jpeg)) => Some(jpeg),
                    failure => {
                        self.cover_unavailable
                            .lock()
                            .await
                            .insert(camera.into(), Instant::now() + Duration::from_secs(300));
                        if let Some(hub) = &api.hub {
                            let cause = match failure {
                                Ok(Err(error)) => error,
                                _ => anyhow!("CoverPreview command timed out"),
                            };
                            hub.diagnostics.error(
                                "api::recordings::preview",
                                "API_RECORDING_PREVIEW_FALLBACK",
                                Some(correlation.0.clone()),
                                json!({"cameraId":camera,"recordingId":id,"stage":"cover-preview"}),
                                Some(cause),
                            );
                        }
                        None
                    }
                }
            } else {
                None
            };
            match jpeg {
                Some(jpeg) => jpeg,
                None => {
                    let frame = recording_media::download_frame(device, &clip).await?;
                    convert(Some(frame), None, 0).await?
                }
            }
        };
        self.cache_thumbnail(key.clone(), jpeg.clone()).await;
        if let Some(store) = &self.thumbnail_store {
            store.recording(&key, Some(&jpeg), ttl).await;
        }
        Ok(jpeg)
    }

    async fn cache_thumbnail(&self, key: String, jpeg: Bytes) {
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
                jpeg,
                accessed: Instant::now(),
            },
        );
    }
}

async fn convert(
    frame: Option<(&str, Vec<u8>)>,
    media: Option<Arc<Media>>,
    offset_ms: u64,
) -> Result<Bytes> {
    let mut command = Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-nostdin",
        "-threads",
        "1",
        "-filter_threads",
        "1",
    ]);
    if let Some((codec, _)) = &frame {
        command.args(["-f", codec, "-i", "pipe:0"]);
    }
    if let Some(media) = &media {
        command
            .args(["-ss", &format!("{:.3}", offset_ms as f64 / 1000.0)])
            .arg("-i")
            .arg(&media.path);
    }
    let mut child = command
        .args([
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
        .stdin(if frame.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let input = child.stdin.take();
    let output = child.stdout.take().unwrap();
    let write = async {
        if let Some(mut input) = input {
            input.write_all(&frame.unwrap().1).await?;
        }
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
    Ok(Bytes::from(jpeg))
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::api) struct PreviewQuery {
    offset_ms: u64,
}

pub(in crate::api) async fn get(
    State(api): State<Arc<Api>>,
    Path((camera, id)): Path<(String, String)>,
    Query(query): Query<PreviewQuery>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    if query.offset_ms > 86_400_000 {
        return error(
            StatusCode::BAD_REQUEST,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
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
        result = timeout(Duration::from_secs(35),api.recordings.thumbnail(&camera,&id,query.offset_ms,device,&api,&correlation)) => result,
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
        failure => {
            if let Some(hub) = &api.hub {
                let failure = match failure {
                    Ok(Err(error)) => error,
                    _ => anyhow!("recording preview timed out"),
                };
                hub.diagnostics.error(
                    "api::recordings::preview",
                    "API_RECORDING_PREVIEW_FAILED",
                    Some(correlation.0.clone()),
                    json!({"cameraId":camera,"recordingId":id,"stage":"preview"}),
                    Some(failure),
                );
            }
            error(
                StatusCode::BAD_GATEWAY,
                "API_MEDIA_START_FAILED",
                &correlation,
            )
        }
    }
}
