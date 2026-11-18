use std::{process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Extension,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use retina::{
    client::{Credentials, PlayOptions, Session, SessionOptions, SetupOptions, Transport},
    codec::{CodecItem, FrameFormat, ParametersRef},
};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdout, Command},
    sync::OwnedSemaphorePermit,
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};
use url::Url;

use super::{Api, error};
use crate::{http::CorrelationId, reolink::wire::Device};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    pub port: u16,
    pub streams: Vec<String>,
}

impl Profile {
    pub fn valid(&self) -> bool {
        self.port != 0
            && !self.streams.is_empty()
            && self.streams.len() <= 2
            && self
                .streams
                .iter()
                .all(|name| matches!(name.as_str(), "main" | "sub"))
            && (self.streams.len() == 1 || self.streams[0] != self.streams[1])
    }
}

struct Relay {
    output: ChildStdout,
    _ffmpeg: Child,
    feed: JoinHandle<()>,
    _slot: OwnedSemaphorePermit,
    deadline: Instant,
    stop: tokio_util::sync::CancellationToken,
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.feed.abort();
    }
}
impl Relay {
    async fn sample(&mut self) -> Result<Option<Bytes>> {
        let mut bytes = vec![0; 32_768];
        let deadline = (Instant::now() + Duration::from_secs(9)).min(self.deadline);
        let count = tokio::select! {
            _ = self.stop.cancelled() => return Ok(None),
            result = timeout_at(deadline,self.output.read(&mut bytes)) => result??,
        };
        if count == 0 {
            return Ok(None);
        }
        bytes.truncate(count);
        Ok(Some(Bytes::from(bytes)))
    }
}

pub async fn get(
    State(api): State<Arc<Api>>,
    Path((id, stream)): Path<(String, String)>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(profile) = api
        .config
        .media
        .get(&id)
        .filter(|profile| profile.streams.contains(&stream))
    else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
    };
    let Some(device) = api.devices.get(&id) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
    };
    let Ok(slot) = api.media_slots.clone().try_acquire_owned() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "API_MEDIA_BUSY",
            &correlation,
        );
    };
    let Ok(Ok(mut relay)) = timeout(
        Duration::from_secs(12),
        start(device, profile, &stream, slot, api.stop.clone()),
    )
    .await
    else {
        return error(
            StatusCode::BAD_GATEWAY,
            "API_MEDIA_START_FAILED",
            &correlation,
        );
    };
    let Ok(Some(first)) = relay.sample().await else {
        return error(
            StatusCode::BAD_GATEWAY,
            "API_MEDIA_START_FAILED",
            &correlation,
        );
    };
    let body = stream::once(async { Ok::<_, std::io::Error>(first) }).chain(stream::unfold(
        relay,
        |mut relay| async {
            relay
                .sample()
                .await
                .ok()
                .flatten()
                .map(|bytes| (Ok::<_, std::io::Error>(bytes), relay))
        },
    ));
    (
        [
            (
                "content-type",
                "multipart/x-mixed-replace; boundary=reolink-frame",
            ),
            ("cache-control", "private, no-store"),
            ("x-accel-buffering", "no"),
            ("x-content-type-options", "nosniff"),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

async fn start(
    device: &Device,
    profile: &Profile,
    stream: &str,
    slot: OwnedSemaphorePermit,
    stop: tokio_util::sync::CancellationToken,
) -> Result<Relay> {
    // Credentials are held by the private RTSP client, never in FFmpeg arguments or environment.
    let url = Url::parse(&format!(
        "rtsp://{}:{}/Preview_{:02}_{stream}",
        device.host,
        profile.port,
        device.channel + 1
    ))?;
    let options = SessionOptions::default().creds(Some(Credentials {
        username: device.username.clone(),
        password: device.password.clone(),
    }));
    let mut session = Session::describe(url, options)
        .await
        .map_err(|_| anyhow!("RTSP description failed"))?;
    let index = session
        .streams()
        .iter()
        .position(|stream| stream.encoding_name().eq_ignore_ascii_case("h264"))
        .context("no supported video track")?;
    session
        .setup(
            index,
            SetupOptions::default()
                .transport(Transport::Tcp(Default::default()))
                .frame_format(FrameFormat::SIMPLE),
        )
        .await
        .map_err(|_| anyhow!("RTSP setup failed"))?;
    let mut session = session
        .play(PlayOptions::default())
        .await
        .map_err(|_| anyhow!("RTSP play failed"))?
        .demuxed()
        .map_err(|_| anyhow!("RTSP demux failed"))?;
    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-probesize",
            "32768",
            "-analyzeduration",
            "1000000",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-an",
            "-vf",
            "fps=8,scale=w=min(iw\\,1280):h=-2",
            "-threads",
            "1",
            "-c:v",
            "mjpeg",
            "-q:v",
            "6",
            "-f",
            "mpjpeg",
            "-boundary_tag",
            "reolink-frame",
            "-flush_packets",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = ffmpeg.stdin.take().context("FFmpeg input unavailable")?;
    let output = ffmpeg.stdout.take().context("FFmpeg output unavailable")?;
    let feed = tokio::spawn(async move {
        let _ = async {
            let mut need_keyframe = true;
            while let Some(item) = timeout(Duration::from_secs(8), session.next()).await? {
                let CodecItem::VideoFrame(frame) =
                    item.map_err(|_| anyhow!("invalid video frame"))?
                else {
                    continue;
                };
                ensure!(
                    frame.data().len() <= 8 * 1024 * 1024,
                    "oversize video frame"
                );
                if frame.loss() != 0 {
                    need_keyframe = true;
                }
                if frame.is_random_access_point() {
                    let Some(ParametersRef::Video(video)) = session.streams()[index].parameters()
                    else {
                        continue;
                    };
                    let (width, height) = video.pixel_dimensions();
                    ensure!(
                        width <= 4096 && height <= 2160 && video.extra_data().len() <= 65_536,
                        "video profile exceeds relay limits"
                    );
                    need_keyframe = false;
                }
                if !need_keyframe {
                    timeout(Duration::from_secs(6), input.write_all(frame.data())).await??;
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
    });
    Ok(Relay {
        output,
        _ffmpeg: ffmpeg,
        feed,
        _slot: slot,
        deadline: Instant::now() + Duration::from_secs(125),
        stop,
    })
}
