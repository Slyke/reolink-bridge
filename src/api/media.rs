use std::{collections::BTreeSet, future::Future, process::Stdio, sync::Arc, time::Duration};

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
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::OwnedSemaphorePermit,
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};
use url::Url;

use super::{Api, error};
use crate::{
    http::CorrelationId,
    reolink::{
        live_video::LiveVideo,
        wire::{Device, LiveStream},
    },
};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    transport: VideoTransport,
    pub port: u16,
    pub streams: Vec<LiveStream>,
}

#[derive(Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum VideoTransport {
    #[default]
    Auto,
    Rtsp,
    Baichuan,
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.port != 0, "media port must be positive");
        ensure!(!self.streams.is_empty(), "media profile requires a stream");
        ensure!(
            self.streams.iter().collect::<BTreeSet<_>>().len() == self.streams.len(),
            "duplicate media stream"
        );
        ensure!(
            !(matches!(self.transport, VideoTransport::Rtsp)
                && self.streams.contains(&LiveStream::Third)),
            "the third live stream requires auto or baichuan transport"
        );
        Ok(())
    }
}

struct Relay {
    output: ChildStdout,
    _ffmpeg: Child,
    feed: JoinHandle<Result<()>>,
    _slot: Arc<OwnedSemaphorePermit>,
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
            if self.feed.is_finished() {
                (&mut self.feed).await??;
            }
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
    let Some((profile, quality)) = api.config.media.get(&id).and_then(|profile| {
        profile
            .streams
            .iter()
            .copied()
            .find(|q| q.id() == stream)
            .map(|q| (profile, q))
    }) else {
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
    let slot = Arc::new(slot);
    let open = |transport| {
        let slot = slot.clone();
        let stop = api.stop.clone();
        async move {
            let mut relay = match transport {
                VideoTransport::Rtsp => {
                    rtsp_relay(device, profile, quality.id(), slot, stop).await?
                }
                VideoTransport::Baichuan => native_relay(device, quality, slot, stop).await?,
                VideoTransport::Auto => unreachable!(),
            };
            let first = relay
                .sample()
                .await?
                .context("decoder exited before its first frame")?;
            Ok::<_, anyhow::Error>((relay, first))
        }
    };
    // Reolink defines RTSP paths for main and sub. The third encoder has a native preview handle.
    let transport = if quality == LiveStream::Third {
        VideoTransport::Baichuan
    } else {
        profile.transport
    };
    let result = match transport {
        VideoTransport::Auto => {
            match timeout(Duration::from_secs(8), open(VideoTransport::Rtsp))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
            {
                Ok(value) => Ok(value),
                Err(rtsp_failure) => {
                    timeout(Duration::from_secs(14), open(VideoTransport::Baichuan))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|r| r)
                        .with_context(|| {
                            format!("RTSP failed ({rtsp_failure}); Baichuan fallback failed")
                        })
                }
            }
        }
        transport => timeout(Duration::from_secs(20), open(transport))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r),
    };
    let (relay, first) = match result {
        Ok(value) => value,
        Err(failure) => {
            if let Some(hub) = &api.hub {
                hub.diagnostics.error(
                    "api::media::start",
                    "API_MEDIA_START_FAILED",
                    Some(correlation.0.clone()),
                    serde_json::json!({"cameraId":id,"stream":stream}),
                    Some(failure),
                );
            }
            return error(
                StatusCode::BAD_GATEWAY,
                "API_MEDIA_START_FAILED",
                &correlation,
            );
        }
    };
    let hub = api.hub.clone();
    let context = serde_json::json!({"cameraId":id,"stream":stream});
    let body = stream::once(async { Ok::<_, std::io::Error>(first) }).chain(stream::unfold(
        (relay, hub, context, correlation.0),
        |(mut relay, hub, context, cid)| async move {
            let failure = match relay.sample().await {
                Ok(Some(bytes)) => {
                    return Some((Ok::<_, std::io::Error>(bytes), (relay, hub, context, cid)));
                }
                _ if relay.stop.is_cancelled() || Instant::now() >= relay.deadline => return None,
                Ok(None) => anyhow!("the upstream video decoder ended"),
                Err(cause) => cause,
            };
            if let Some(hub) = hub {
                hub.diagnostics.error(
                    "api::media::stream",
                    "API_MEDIA_STREAM_ENDED",
                    Some(cid),
                    context,
                    Some(failure),
                );
            }
            None
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

async fn rtsp_relay(
    device: &Device,
    profile: &Profile,
    stream: &str,
    slot: Arc<OwnedSemaphorePermit>,
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
        .position(|stream| {
            matches!(
                stream.encoding_name().to_ascii_lowercase().as_str(),
                "h264" | "h265" | "hevc"
            )
        })
        .context("no supported video track")?;
    let format = if session.streams()[index]
        .encoding_name()
        .eq_ignore_ascii_case("h264")
    {
        "h264"
    } else {
        "hevc"
    };
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
    transcode(format, slot, stop, move |mut input| async move {
        let mut need_keyframe = true;
        while let Some(item) = timeout(Duration::from_secs(8), session.next()).await? {
            let CodecItem::VideoFrame(frame) = item.map_err(|_| anyhow!("invalid video frame"))?
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
        Ok(())
    })
}

async fn native_relay(
    device: &Device,
    quality: LiveStream,
    slot: Arc<OwnedSemaphorePermit>,
    stop: CancellationToken,
) -> Result<Relay> {
    let mut video = LiveVideo::open(device, quality)
        .await
        .context("open Baichuan preview")?;
    let (format, first) = video.next_frame().await.context("read Baichuan keyframe")?;
    transcode(format, slot, stop, move |mut input| async move {
        input.write_all(&first).await?;
        loop {
            let (codec, data) = timeout(Duration::from_secs(8), video.next_frame()).await??;
            ensure!(codec == format, "Baichuan codec changed");
            timeout(Duration::from_secs(6), input.write_all(&data)).await??;
        }
    })
}

fn transcode<F>(
    format: &str,
    slot: Arc<OwnedSemaphorePermit>,
    stop: CancellationToken,
    feed: impl FnOnce(ChildStdin) -> F,
) -> Result<Relay>
where
    F: Future<Output = Result<()>> + Send + 'static,
{
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
            format,
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
    let input = ffmpeg.stdin.take().context("FFmpeg input unavailable")?;
    let output = ffmpeg.stdout.take().context("FFmpeg output unavailable")?;
    let feed = tokio::spawn(feed(input));
    Ok(Relay {
        output,
        _ffmpeg: ffmpeg,
        feed,
        _slot: slot,
        deadline: Instant::now() + Duration::from_secs(125),
        stop,
    })
}

#[cfg(test)]
mod tests {
    use super::Profile;
    use serde_json::json;

    #[test]
    fn validates_live_streams_for_each_transport() {
        let profile = |transport, streams| {
            serde_json::from_value::<Profile>(json!({
                "transport": transport, "port": 554, "streams": streams
            }))
        };
        assert!(
            profile("auto", vec!["main", "sub", "third"])
                .unwrap()
                .validate()
                .is_ok()
        );
        assert!(
            profile("baichuan", vec!["third"])
                .unwrap()
                .validate()
                .is_ok()
        );
        assert!(
            profile("rtsp", vec!["main", "sub"])
                .unwrap()
                .validate()
                .is_ok()
        );
        assert!(profile("rtsp", vec!["third"]).unwrap().validate().is_err());
        assert!(
            profile("auto", vec!["main", "third", "third"])
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(profile("auto", vec!["other"]).is_err());
    }
}
