use anyhow::{Result, bail, ensure};
use bytes::{Buf, Bytes, BytesMut};

use super::{
    metadata::{Media, media_sample},
    wire::{Device, LiveStream, Packet, Session},
};

pub struct LiveVideo {
    session: Session,
    first: Option<Packet>,
    buffer: BytesMut,
    awaiting_keyframe: bool,
}

impl LiveVideo {
    pub async fn open(device: &Device, stream: LiveStream) -> Result<Self> {
        let mut session = device.connect().await?;
        let first = session.preview(device.channel, stream, true).await?;
        Ok(Self {
            session,
            first: Some(first),
            buffer: BytesMut::new(),
            awaiting_keyframe: true,
        })
    }

    pub async fn next_frame(&mut self) -> Result<(&'static str, Bytes)> {
        loop {
            while let Some((length, sample)) = media_sample(&self.buffer)? {
                let frame = match sample {
                    Media::Info(width, height) => {
                        ensure!(
                            width <= 4096 && height <= 2160,
                            "video profile exceeds relay limits"
                        );
                        None
                    }
                    Media::Video {
                        iframe,
                        codec,
                        data,
                        ..
                    } if !self.awaiting_keyframe || iframe => {
                        let format = match codec {
                            b"H264" => "h264",
                            b"H265" => "hevc",
                            _ => bail!("unsupported Baichuan video codec"),
                        };
                        self.awaiting_keyframe = false;
                        Some((format, Bytes::copy_from_slice(data)))
                    }
                    _ => None,
                };
                self.buffer.advance(length);
                if let Some(frame) = frame {
                    return Ok(frame);
                }
            }
            let packet = match self.first.take() {
                Some(packet) => packet,
                None => self.session.read().await?,
            };
            if packet.command == 3
                && let Some(bytes) = packet.binary(&self.session)
            {
                ensure!(
                    self.buffer.len() + bytes.len() <= 16 * 1024 * 1024,
                    "oversize Baichuan video buffer"
                );
                self.buffer.extend_from_slice(&bytes);
            }
        }
    }
}
