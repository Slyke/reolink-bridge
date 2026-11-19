use std::{collections::BTreeMap, io::Read, time::Duration};

use anyhow::{Result, bail, ensure};
use bytes::{Buf, BytesMut};
use chrono::Utc;
use serde_json::json;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::wire::{Device, le_u32};
use crate::{
    hub::Hub,
    model::{Bounds, Event, Object, Observation, reolink_timestamp},
};

const MAX_VIDEO_BUFFER: usize = 8 * 1024 * 1024;
const MAX_OVERLAY: u64 = 256 * 1024;

pub async fn run(device: &Device, id: &str, hub: &Hub, stop: &CancellationToken) -> Result<()> {
    let mut session = tokio::select! {
        _ = stop.cancelled() => return Ok(()),
        result = device.connect() => result?,
    };
    let first = session
        .preview(device.channel, super::recording_settings::Stream::Sub, true)
        .await?;
    let mut buffer = BytesMut::new();
    let mut video_dimensions = None;
    let mut ai_dimensions = None;
    let mut had_objects = false;
    let mut saw_overlay = false;
    let mut saw_frame = false;
    let result = async {
        let mut next = Some(first);
        loop {
            let packet = match next.take() {
                Some(packet) => packet,
                None => tokio::select! {
                    _ = stop.cancelled() => return Ok(()),
                    packet = timeout(Duration::from_secs(15), session.read()) => packet??,
                },
            };
            if packet.command != 3 {
                continue;
            }
            let Some(data) = packet.binary(&session) else { continue };
            ensure!(buffer.len() + data.len() <= MAX_VIDEO_BUFFER, "Baichuan media frame exceeds size limit");
            buffer.extend_from_slice(&data);
            while let Some((consumed, sample)) = media_sample(&buffer)? {
                match sample {
                    Media::Info(width, height) => video_dimensions = Some((width, height)),
                    Media::Audio { .. } => {}
                    Media::Video { iframe, ticks, extra, .. } => {
                        let overlay = Overlay::decode(extra, iframe);
                        if !saw_frame || (!saw_overlay && overlay.is_some()) {
                            saw_frame = true;
                            saw_overlay |= overlay.is_some();
                            hub.section(id, "capabilities", "coordinate_stream", json!({
                                "transport": "baichuan_substream", "connected": true, "overlay_observed": saw_overlay,
                                "video_dimensions": video_dimensions,
                            }));
                        }
                        if let Some(overlay) = overlay {
                            ai_dimensions = overlay.dimensions.or(ai_dimensions);
                            let objects = overlay.objects(ai_dimensions);
                            let present = !objects.is_empty();
                            if present || had_objects {
                                let received = Utc::now();
                                let mut event = Event::new(id, "objects".into(), "baichuan_metadata", "BcMedia/additionalHeader".into(), received);
                                event.camera_time = iframe.then(|| extra.get(..4)).flatten()
                                    .and_then(|time| reolink_timestamp(&le_u32(time, 0).to_string()));
                                event.channel = Some(device.channel);
                                event.objects = objects;
                                event.vendor = json!({
                                    "stream": "sub",
                                    "frame_microseconds": ticks,
                                    "video_dimensions": video_dimensions,
                                });
                                hub.ingest(Observation { event, property: None, initial: false, deleted: false });
                            }
                            had_objects = present;
                        }
                    }
                }
                buffer.advance(consumed);
            }
        }
    }.await;
    let _ = timeout(
        Duration::from_secs(2),
        session.preview(
            device.channel,
            super::recording_settings::Stream::Sub,
            false,
        ),
    )
    .await;
    session.logout().await;
    result
}

pub(super) fn recording_objects(
    extra: &[u8],
    iframe: bool,
    dimensions: &mut Option<(u32, u32)>,
) -> Option<Vec<Object>> {
    let overlay = Overlay::decode(extra, iframe)?;
    *dimensions = overlay.dimensions.or(*dimensions);
    Some(overlay.objects(*dimensions))
}

pub(super) enum Media<'a> {
    Info(u32, u32),
    Video {
        iframe: bool,
        ticks: u32,
        extra: &'a [u8],
        codec: &'a [u8],
        data: &'a [u8],
    },
    Audio {
        aac: bool,
        data: &'a [u8],
    },
}

pub(super) fn media_sample(data: &[u8]) -> Result<Option<(usize, Media<'_>)>> {
    if data.len() < 8 {
        return Ok(None);
    }
    match le_u32(data, 0) {
        0x31303031 | 0x32303031 => {
            ensure!(le_u32(data, 4) == 32, "invalid Baichuan media info length");
            if data.len() >= 32 {
                return Ok(Some((32, Media::Info(le_u32(data, 8), le_u32(data, 12)))));
            }
        }
        magic @ (0x63643030..=0x63643039 | 0x63643130..=0x63643139) => {
            if data.len() < 24 {
                return Ok(None);
            }
            let payload = le_u32(data, 8) as usize;
            let extra = le_u32(data, 12) as usize;
            ensure!(
                payload <= MAX_VIDEO_BUFFER && extra <= u16::MAX as usize,
                "invalid Baichuan media frame length"
            );
            let end = 24 + extra + payload.next_multiple_of(8);
            if data.len() >= end {
                return Ok(Some((
                    end,
                    Media::Video {
                        iframe: magic <= 0x63643039,
                        ticks: le_u32(data, 16),
                        extra: &data[24..24 + extra],
                        codec: &data[4..8],
                        data: &data[24 + extra..24 + extra + payload],
                    },
                )));
            }
        }
        0x62773530 | 0x62773130 => {
            let length = u16::from_le_bytes([data[4], data[5]]);
            ensure!(
                data[4..6] == data[6..8],
                "invalid Baichuan media audio length"
            );
            let end = 8 + usize::from(length).next_multiple_of(8);
            if data.len() >= end {
                return Ok(Some((
                    end,
                    Media::Audio {
                        aac: le_u32(data, 0) == 0x62773530,
                        data: &data[8..8 + usize::from(length)],
                    },
                )));
            }
        }
        _ => bail!("unrecognized Baichuan media frame"),
    }
    Ok(None)
}

#[derive(Default)]
struct Overlay {
    dimensions: Option<(u32, u32)>,
    boxes: BTreeMap<[u16; 4], (u8, u16)>,
}

impl Overlay {
    fn decode(extra: &[u8], iframe: bool) -> Option<Self> {
        let data = extra.get(if iframe { 8 } else { 0 }..)?;
        let marker = data.get(..8)?;
        if marker[0] != 0xff || marker[2..] != [0, 1, 11, 0, 1, 8] {
            return None;
        }
        let mut overlay = Self::default();
        overlay.read_tlvs(data, &[], 0);
        Some(overlay)
    }

    fn read_tlvs(&mut self, mut data: &[u8], context: &[u8], depth: u8) {
        if depth >= 10 {
            return;
        }
        while let Some(header) = data.get(..3) {
            let tag = header[0];
            let len = usize::from(u16::from_le_bytes([header[1], header[2]]));
            let Some(value) = data.get(3..3 + len) else {
                return;
            };
            if tag == 0 {
                return;
            }
            data = &data[3 + len..];
            match (context, tag, len) {
                ([255, 2], 2, _) if value.starts_with(&[0x04, 0x22, 0x4d, 0x18]) => {
                    let mut inner = Vec::new();
                    let decoder = lz4_flex::frame::FrameDecoder::new(value);
                    if decoder
                        .take(MAX_OVERLAY + 1)
                        .read_to_end(&mut inner)
                        .is_ok()
                        && inner.len() as u64 <= MAX_OVERLAY
                    {
                        self.read_tlvs(&inner, &[], depth + 1);
                    }
                }
                (_, 3, 4) => {
                    let (w, h) = (u16_at(value, 0), u16_at(value, 2));
                    if (64..=8192).contains(&w) && (64..=8192).contains(&h) {
                        self.dimensions = Some((w.into(), h.into()));
                    }
                }
                ([class @ (1..=3), 3 | 5], 4, 10 | 13 | 14) | ([class @ 3, 3 | 5], 2, 10) => {
                    let rect = [
                        u16_at(value, 0),
                        u16_at(value, 2),
                        u16_at(value, 4),
                        u16_at(value, 6),
                    ];
                    if rect[0] < rect[2] && rect[1] < rect[3] && self.boxes.len() < 256 {
                        let confidence = u16_at(value, 8);
                        self.boxes
                            .entry(rect)
                            .and_modify(|current| {
                                if specificity(*class) > specificity(current.0) {
                                    *current = (*class, confidence);
                                }
                            })
                            .or_insert((*class, confidence));
                    }
                }
                (_, _, 0) => {}
                ([], _, _) => self.read_tlvs(value, &[tag], depth + 1),
                ([class], _, _) => self.read_tlvs(value, &[*class, tag], depth + 1),
                _ => {}
            }
        }
    }

    fn objects(&self, dimensions: Option<(u32, u32)>) -> Vec<Object> {
        self.boxes
            .iter()
            .filter_map(|(&[x1, y1, x2, y2], &(class, conf))| {
                let (sx, sy, coordinate_space) = if let Some((w, h)) = dimensions {
                    if u32::from(x2) > w || u32::from(y2) > h {
                        return None;
                    }
                    (f64::from(w), f64::from(h), "normalized_top_left")
                } else {
                    (1.0, 1.0, "pixels_top_left")
                };
                let class = match class {
                    1 => "person",
                    2 => "vehicle",
                    _ => "pet",
                };
                Some(Object {
                    tracking_id: None,
                    class: Some(class.into()),
                    confidence: (conf <= 100).then(|| f64::from(conf) / 100.0),
                    bounding_box: Some(Bounds {
                        left: f64::from(x1) / sx,
                        top: f64::from(y1) / sy,
                        right: f64::from(x2) / sx,
                        bottom: f64::from(y2) / sy,
                        coordinate_space,
                        reference_frame: "Reolink AI processing frame (substream)".into(),
                        frame_width: dimensions.map(|(w, _)| w),
                        frame_height: dimensions.map(|(_, h)| h),
                    }),
                })
            })
            .collect()
    }
}

fn specificity(class: u8) -> u8 {
    match class {
        3 => 3,
        1 => 2,
        _ => 1,
    }
}

fn u16_at(data: &[u8], index: usize) -> u16 {
    u16::from_le_bytes([data[index], data[index + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tlv(kind: u8, value: &[u8]) -> Vec<u8> {
        let mut record = vec![kind];
        record.extend((value.len() as u16).to_le_bytes());
        record.extend(value);
        record
    }

    #[test]
    fn decodes_camera_boxes_from_compressed_frame_metadata() {
        let dimensions: Vec<_> = [896u16, 480]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        let box_data: Vec<_> = [100u16, 50, 450, 250, 92]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut raw = tlv(3, &dimensions);
        raw.extend(tlv(1, &tlv(3, &tlv(4, &box_data))));
        let mut lz4 = lz4_flex::frame::FrameEncoder::new(Vec::new());
        lz4.write_all(&raw).unwrap();
        let compressed = lz4.finish().unwrap();
        let mut payload = tlv(1, &[1, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        payload.extend(tlv(2, &tlv(2, &compressed)));
        let overlay = Overlay::decode(&tlv(255, &payload), false).unwrap();
        assert_eq!(overlay.dimensions, Some((896, 480)));
        let objects = overlay.objects(overlay.dimensions);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].class.as_deref(), Some("person"));
        assert_eq!(objects[0].confidence, Some(0.92));
        assert_eq!(objects[0].bounding_box.as_ref().unwrap().top, 50.0 / 480.0);
    }
}
