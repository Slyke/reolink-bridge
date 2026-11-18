use crate::model::Object;
use anyhow::{Result, anyhow, ensure};
use bytes::{Buf, BytesMut};
use serde::Serialize;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

use super::{
    metadata::{Media, media_sample, recording_objects},
    recordings::{Clip, time_xml, uid},
    wire::Device,
};

const LIMIT: usize = 512 * 1024 * 1024;

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingMetadata {
    pub frames: Vec<RecordingFrame>,
    pub truncated: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingFrame {
    pub offset_ms: u64,
    pub objects: Vec<Object>,
}

pub async fn download(
    device: &Device,
    clip: &Clip,
    output: &mut (impl AsyncWrite + Unpin),
    stop: &CancellationToken,
) -> Result<RecordingMetadata> {
    let mut session = device.connect().await?;
    let uid = uid(&mut session).await?;
    let channel = device.channel;
    let sub = u8::from(clip.stream == "sub");
    let body = format!(
        r#"<body><FileInfoList version="1.1"><FileInfo><uid>{uid}</uid><logicChnBitmap>255</logicChnBitmap><channelId>{channel}</channelId><supportSub>{sub}</supportSub><streamType>{}Stream</streamType>{}{}</FileInfo></FileInfoList></body>"#,
        clip.stream,
        time_xml("startTime", clip.start),
        time_xml("endTime", clip.end)
    );
    let first = session.recording_start(channel, &body).await?;
    let sequence = first.sequence;
    let mut packet = Some(first);
    let mut pending = BytesMut::new();
    let mut total = 0;
    let mut mux = None;
    let mut start = None;
    let mut last_ticks = 0;
    let mut audio_pts = 90_000u64;
    let mut frames = 0;
    let mut metadata = RecordingMetadata::default();
    let mut dimensions = None;
    let mut metadata_bytes = 0;
    let result = async {
        loop {
            let part = match packet.take() {
                Some(value) => value,
                None => tokio::select! {
                    _=stop.cancelled()=>return Err(anyhow!("recording cancelled")),
                    value=timeout(Duration::from_secs(20),session.read())=>value??,
                },
            };
            if part.command != 143 || part.sequence != sequence {
                continue;
            }
            // Cameras signal a completed download with 300 or 331, rather than a 200 empty packet.
            if matches!(part.status, 300 | 331) {
                break;
            }
            ensure!(
                matches!(part.status, 200 | 201),
                "recording transfer rejected"
            );
            if part.payload_finished(&session) {
                break;
            }
            let Some(data) = part.binary(&session) else {
                continue;
            };
            total += data.len();
            ensure!(
                total <= LIMIT && pending.len() + data.len() <= 16 * 1024 * 1024,
                "recording transfer exceeds size limit"
            );
            pending.extend_from_slice(&data);
            while let Some((consumed, sample)) = media_sample(&pending)? {
                match sample {
                    Media::Info(width, height) => dimensions = Some((width, height)),
                    Media::Video {
                        iframe,
                        ticks,
                        codec,
                        data,
                        extra,
                    } => {
                        if start.is_none() && !iframe {
                            pending.advance(consumed);
                            continue;
                        }
                        let base = *start.get_or_insert(ticks);
                        if frames > 0 && ticks.wrapping_sub(last_ticks) > 0x8000_0000 {
                            pending.advance(consumed);
                            continue;
                        }
                        if mux.is_none() {
                            let codec = match codec {
                                b"H264" => 0x1b,
                                b"H265" => 0x24,
                                _ => return Err(anyhow!("unsupported recording codec")),
                            };
                            let value = Mux::new(codec);
                            output.write_all(&value.header()).await?;
                            mux = Some(value);
                        }
                        let offset_ms = u64::from(ticks.wrapping_sub(base)) / 1000;
                        if !metadata.truncated
                            && metadata
                                .frames
                                .last()
                                .is_none_or(|frame| offset_ms >= frame.offset_ms + 200)
                            && let Some(mut objects) =
                                recording_objects(extra, iframe, &mut dimensions)
                        {
                            objects.truncate(32);
                            let frame = RecordingFrame { offset_ms, objects };
                            metadata_bytes += serde_json::to_vec(&frame)?.len();
                            if metadata.frames.len() >= 4096 || metadata_bytes > 4 * 1024 * 1024 {
                                metadata.truncated = true;
                            } else {
                                metadata.frames.push(frame);
                            }
                        }
                        let pts = 90_000 + u64::from(ticks.wrapping_sub(base)) * 9 / 100;
                        output
                            .write_all(&mux.as_mut().unwrap().pes(0x100, 0xe0, pts, data))
                            .await?;
                        last_ticks = ticks;
                        frames += 1;
                    }
                    Media::Audio { aac: true, data } if frames > 0 => {
                        let video_pts =
                            90_000 + u64::from(last_ticks.wrapping_sub(start.unwrap())) * 9 / 100;
                        if video_pts > audio_pts + 90_000 {
                            audio_pts = video_pts;
                        }
                        if data.len() >= 7 && data[0] == 0xff && data[1] & 0xf0 == 0xf0 {
                            let current = mux.as_mut().unwrap();
                            if !current.audio {
                                current.audio = true;
                                output.write_all(&current.header()).await?;
                            }
                            output
                                .write_all(&mux.as_mut().unwrap().pes(0x101, 0xc0, audio_pts, data))
                                .await?;
                            let rates = [
                                96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000,
                                12000, 11025, 8000, 7350, 16000, 16000, 16000,
                            ];
                            audio_pts += 1024 * 90_000 / rates[usize::from((data[2] >> 2) & 15)];
                        }
                    }
                    _ => {}
                }
                pending.advance(consumed);
            }
        }
        ensure!(
            frames > 0 && pending.is_empty(),
            "recording has no complete video"
        );
        output.flush().await?;
        Ok(metadata)
    }
    .await;
    let _ = session.command(144, Some(channel), "").await;
    session.logout().await;
    result
}

struct Mux {
    codec: u8,
    audio: bool,
    counters: [u8; 2],
}

impl Mux {
    fn new(codec: u8) -> Self {
        Self {
            codec,
            audio: false,
            counters: [0, 0],
        }
    }
    fn header(&self) -> Vec<u8> {
        let mut output = psi(0, 0, &[0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0]);
        let mut pmt = vec![
            0, 1, 0xc1, 0, 0, 0xe1, 0, 0xf0, 0, self.codec, 0xe1, 0, 0xf0, 0,
        ];
        if self.audio {
            pmt[2] = 0xc3;
            pmt.extend([0x0f, 0xe1, 1, 0xf0, 0]);
        }
        output.extend(psi(0x1000, 2, &pmt));
        output
    }
    fn pes(&mut self, pid: u16, kind: u8, pts: u64, data: &[u8]) -> Vec<u8> {
        let length = if kind == 0xe0 {
            0
        } else {
            (data.len() + 8) as u16
        };
        let mut payload = vec![
            0,
            0,
            1,
            kind,
            (length >> 8) as u8,
            length as u8,
            0x80,
            0x80,
            5,
        ];
        payload.extend([
            0x21 | ((pts >> 29) as u8 & 14),
            (pts >> 22) as u8,
            ((pts >> 14) as u8 & 0xfe) | 1,
            (pts >> 7) as u8,
            ((pts << 1) as u8 & 0xfe) | 1,
        ]);
        payload.extend_from_slice(data);
        let counter = &mut self.counters[usize::from(pid == 0x101)];
        let mut output = Vec::with_capacity(payload.len().div_ceil(184) * 188);
        for (index, chunk) in payload.chunks(184).enumerate() {
            let padding = 184 - chunk.len();
            output.extend([
                0x47,
                ((pid >> 8) as u8) | if index == 0 { 0x40 } else { 0 },
                pid as u8,
                (if padding > 0 { 0x30 } else { 0x10 }) | *counter,
            ]);
            *counter = (*counter + 1) & 15;
            if padding > 0 {
                output.push((padding - 1) as u8);
                if padding > 1 {
                    output.push(0);
                    output.extend(std::iter::repeat_n(0xff, padding - 2));
                }
            }
            output.extend_from_slice(chunk);
        }
        output
    }
}

fn psi(pid: u16, table: u8, body: &[u8]) -> Vec<u8> {
    let size = (body.len() + 4) as u16;
    let mut section = vec![table, 0xb0 | ((size >> 8) as u8), size as u8];
    section.extend_from_slice(body);
    let mut crc = 0xffff_ffffu32;
    for byte in &section {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c11db7
            } else {
                crc << 1
            };
        }
    }
    section.extend_from_slice(&crc.to_be_bytes());
    let mut packet = vec![0x47, 0x40 | ((pid >> 8) as u8), pid as u8, 0x10, 0];
    packet.extend(section);
    packet.resize(188, 0xff);
    packet
}

pub async fn cover(device: &Device, clip: &Clip) -> Result<(&'static str, Vec<u8>)> {
    frame(device, clip, true).await
}

/// Read only the first keyframe, then stop the download. Older cameras lack CoverPreview.
pub async fn download_frame(device: &Device, clip: &Clip) -> Result<(&'static str, Vec<u8>)> {
    frame(device, clip, false).await
}

async fn frame(device: &Device, clip: &Clip, cover: bool) -> Result<(&'static str, Vec<u8>)> {
    let mut session = device.connect().await?;
    let uid = uid(&mut session).await?;
    let end = clip.end.min(clip.start + chrono::Duration::seconds(10));
    let body = format!(
        r#"<body><CoverPreview version="1.1"><channelId>{}</channelId><uid>{uid}</uid><desc>0</desc><streamType>{}Stream</streamType>{}{}<frameList><frameNo>1</frameNo></frameList></CoverPreview></body>"#,
        device.channel,
        clip.stream,
        time_xml("startTime", clip.start),
        time_xml("endTime", end)
    );
    let first = if cover {
        session.cover_start(&body).await?
    } else {
        let sub = u8::from(clip.stream == "sub");
        let body = format!(
            r#"<body><FileInfoList version="1.1"><FileInfo><uid>{uid}</uid><logicChnBitmap>255</logicChnBitmap><channelId>{}</channelId><supportSub>{sub}</supportSub><streamType>{}Stream</streamType>{}{}</FileInfo></FileInfoList></body>"#,
            device.channel,
            clip.stream,
            time_xml("startTime", clip.start),
            time_xml("endTime", end)
        );
        session.recording_start(device.channel, &body).await?
    };
    let command = if cover { 298 } else { 143 };
    let sequence = first.sequence;
    let mut next = Some(first);
    let mut buffer = BytesMut::new();
    let mut total = 0;
    let result = async {
        loop {
            let packet = match next.take() {
                Some(packet) => packet,
                None => session.read().await?,
            };
            if packet.command != command || packet.sequence != sequence {
                continue;
            }
            ensure!(
                matches!(packet.status, 200 | 201),
                "recording frame command {command} returned status {}",
                packet.status
            );
            ensure!(
                !packet.payload_finished(&session),
                "recording frame command {command} ended without a frame"
            );
            let Some(data) = packet.binary(&session) else {
                continue;
            };
            total += data.len();
            ensure!(
                total <= 16 * 1024 * 1024,
                "recording preview exceeds size limit"
            );
            buffer.extend_from_slice(&data);
            while let Some((consumed, sample)) = media_sample(&buffer)? {
                if let Media::Video {
                    iframe: true,
                    codec,
                    data,
                    ..
                } = sample
                {
                    let codec = match codec {
                        b"H264" => "h264",
                        b"H265" => "hevc",
                        _ => return Err(anyhow!("unsupported recording preview codec")),
                    };
                    return Ok((codec, data.to_vec()));
                }
                buffer.advance(consumed);
            }
        }
    }
    .await;
    if !cover {
        let _ = session.command(144, Some(device.channel), "").await;
    }
    session.logout().await;
    result
}
