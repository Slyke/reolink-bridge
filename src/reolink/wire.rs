use std::{collections::VecDeque, time::Duration};

use aes::{
    Aes128,
    cipher::{AsyncStreamCipher, KeyIvInit},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use bytes::{Buf, Bytes, BytesMut};
use chrono::{DateTime, Utc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

use crate::xml;

const MAGIC: [u8; 4] = [0xf0, 0xde, 0xbc, 0x0a];
const IV: &[u8; 16] = b"0123456789abcdef";
const XOR_KEY: [u8; 8] = [0x1f, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0xff];
const TIMEOUT: Duration = Duration::from_secs(4);
const MAX_PACKET: usize = 8 * 1024 * 1024;
const SNAPSHOT: u32 = 109;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(18);
const MAX_JPEG_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct Device {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub channel: u8,
}

#[derive(Debug)]
pub struct Rejected {
    command: u32,
    status: u16,
}
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Baichuan command {} returned status {}",
            self.command, self.status
        )
    }
}
impl std::error::Error for Rejected {}

pub struct Session {
    socket: TcpStream,
    buffer: BytesMut,
    sequence: u32,
    key: Option<[u8; 16]>,
    stream_sequence: Option<u32>,
    preview_type: u8,
    pub pushes: VecDeque<Packet>,
}

pub struct Packet {
    pub command: u32,
    pub(super) sequence: u32,
    pub(super) status: u16,
    offset: usize,
    body: Bytes,
}

impl Device {
    pub async fn capture_jpeg(&self) -> Result<(Vec<u8>, DateTime<Utc>)> {
        let mut session = self.connect().await?;
        let jpeg = session.snapshot_jpeg(self.channel).await?;
        let captured_at = Utc::now();
        session.logout().await;
        Ok((jpeg, captured_at))
    }

    pub async fn connect(&self) -> Result<Session> {
        let socket = timeout(
            TIMEOUT,
            TcpStream::connect((self.host.trim_matches(['[', ']']), self.port)),
        )
        .await??;
        let mut session = Session {
            socket,
            buffer: BytesMut::with_capacity(16 * 1024),
            sequence: 0,
            key: None,
            stream_sequence: None,
            preview_type: 0,
            pushes: VecDeque::new(),
        };
        let challenge = session.exchange(1, 250, "", "", true).await?;
        let challenge = session.decode(&challenge)?;
        let doc = xml::document(&challenge)?;
        let nonce = xml::value(doc.root_element(), "nonce")
            .ok_or_else(|| anyhow!("Baichuan login nonce missing"))?;
        let user = hash(&format!("{}{nonce}", self.username));
        let pass = hash(&format!("{}{nonce}", self.password));
        let auth = format!(
            r#"<body><LoginUser version="1.1"><userName>{user}</userName>
            <password>{pass}</password><userVer>1</userVer></LoginUser>
            <LoginNet version="1.1"><type>LAN</type><udpPort>0</udpPort></LoginNet></body>"#
        );
        let reply = session.exchange(1, 250, "", &auth, false).await?;
        ensure!(reply.status == 200, "Baichuan authentication failed");
        let key = hash(&format!("{nonce}-{}", self.password));
        session.key = Some(key.as_bytes()[..16].try_into().unwrap());
        Ok(session)
    }
}

impl Session {
    pub async fn command(&mut self, id: u32, channel: Option<u8>, body: &str) -> Result<String> {
        let extension = channel.map(|channel| {
            format!(r#"<Extension version="1.1"><channelId>{channel}</channelId></Extension>"#)
        });
        let reply = self
            .exchange(
                id,
                channel.map_or(250, |channel| channel + 1),
                extension.as_deref().unwrap_or(""),
                body,
                false,
            )
            .await?;
        self.decode(&reply)
    }

    pub async fn cover_start(&mut self, body: &str) -> Result<Packet> {
        self.exchange(298, 250, "", body, false).await
    }

    pub async fn recording_start(&mut self, channel: u8, body: &str) -> Result<Packet> {
        let extension =
            format!(r#"<Extension version="1.1"><channelId>{channel}</channelId></Extension>"#);
        self.exchange(143, channel + 1, &extension, body, false)
            .await
    }

    pub async fn user_list(&mut self, username: &str) -> Result<String> {
        let extension = format!(
            r#"<Extension version="1.1"><userName>{}</userName></Extension>"#,
            xml::escape(username)
        );
        let reply = self.exchange(58, 250, &extension, "", false).await?;
        self.decode(&reply)
    }

    pub async fn subscribe(&mut self) -> Result<()> {
        self.exchange(31, 251, "", "", false).await?;
        Ok(())
    }

    pub async fn preview(
        &mut self,
        channel: u8,
        quality: super::recording_settings::Stream,
        start: bool,
    ) -> Result<Packet> {
        let command = if start { 3 } else { 4 };
        let extension =
            format!(r#"<Extension version="1.1"><channelId>{channel}</channelId></Extension>"#);
        self.preview_type = u8::from(matches!(quality, super::recording_settings::Stream::Sub));
        let handle = u16::from(self.preview_type) << 8;
        let stream = if start {
            format!("<streamType>{}</streamType>", quality.tag())
        } else {
            String::new()
        };
        let body = format!(
            r#"<body><Preview version="1.0"><channelId>{channel}</channelId><handle>{handle}</handle>{stream}</Preview></body>"#
        );
        self.exchange(command, channel, &extension, &body, false)
            .await
    }

    pub async fn snapshot_jpeg(&mut self, channel: u8) -> Result<Vec<u8>> {
        let extension =
            format!(r#"<Extension version="1.1"><channelId>{channel}</channelId></Extension>"#);
        let request = format!(
            r#"<body><Snap version="1.1"><channelId>{channel}</channelId><logicChannel>0</logicChannel><time>0</time><fullFrame>0</fullFrame><streamType>main</streamType></Snap></body>"#
        );
        let deadline = tokio::time::Instant::now() + SNAPSHOT_TIMEOUT;
        let mut packet = self
            .exchange_with_timeout(
                SNAPSHOT,
                channel + 1,
                &extension,
                &request,
                false,
                SNAPSHOT_TIMEOUT,
            )
            .await?;
        let sequence = packet.sequence;
        let body = self.decode(&packet)?;
        let xml = xml::document(&body)?;
        let header = xml.root_element();
        if let Some(reported) = xml::value(header, "channelId") {
            ensure!(
                reported.parse().ok() == Some(channel),
                "camera reported a different snapshot channel"
            );
        }
        let size = xml::value(header, "pictureSize")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|size| (4..=MAX_JPEG_BYTES).contains(size))
            .context("invalid camera JPEG size")?;
        let mut jpeg = Vec::with_capacity(size);
        loop {
            if let Some(data) = packet.binary(self) {
                ensure!(
                    jpeg.len() + data.len() <= size,
                    "snapshot exceeded the announced JPEG size"
                );
                jpeg.extend_from_slice(&data);
                if jpeg.len() == size {
                    ensure!(jpeg.starts_with(&[0xff, 0xd8]), "snapshot is not JPEG");
                    return Ok(jpeg);
                }
            }
            loop {
                packet = tokio::time::timeout_at(deadline, self.read())
                    .await
                    .context("Baichuan JPEG transfer timed out")??;
                if packet.command == SNAPSHOT && packet.sequence == sequence {
                    ensure!(
                        matches!(packet.status, 200 | 201) && packet.offset < packet.body.len(),
                        "snapshot ended before delivering the announced JPEG size"
                    );
                    break;
                }
            }
        }
    }

    pub async fn logout(&mut self) {
        let _ = timeout(
            Duration::from_secs(2),
            self.command(
                2,
                None,
                r#"<body><LoginUser version="1.1"><userVer>1</userVer></LoginUser></body>"#,
            ),
        )
        .await;
    }

    pub async fn exchange(
        &mut self,
        command: u32,
        channel: u8,
        extension: &str,
        body: &str,
        challenge: bool,
    ) -> Result<Packet> {
        self.exchange_with_timeout(command, channel, extension, body, challenge, TIMEOUT)
            .await
    }

    async fn exchange_with_timeout(
        &mut self,
        command: u32,
        channel: u8,
        extension: &str,
        body: &str,
        challenge: bool,
        deadline: Duration,
    ) -> Result<Packet> {
        timeout(deadline, async {
            self.sequence = (self.sequence + 1) & 0x00ff_ffff;
            let sequence = match command {
                3 => {
                    let sequence = (self.sequence & 0xffff) << 16
                        | u32::from(self.preview_type) << 8
                        | u32::from(channel);
                    self.stream_sequence = Some(sequence);
                    sequence
                }
                4 => self
                    .stream_sequence
                    .ok_or_else(|| anyhow!("no active Baichuan stream"))?,
                _ => self.sequence << 8 | u32::from(channel),
            };
            let extension = self.encode(extension, channel);
            let body = self.encode(body, channel);
            let mut frame = Vec::with_capacity(24 + extension.len() + body.len());
            frame.extend_from_slice(&MAGIC);
            frame.extend_from_slice(&command.to_le_bytes());
            frame.extend_from_slice(&((extension.len() + body.len()) as u32).to_le_bytes());
            frame.extend_from_slice(&sequence.to_le_bytes());
            if challenge {
                frame.extend_from_slice(&[0x12, 0xdc, 0x14, 0x65]);
            } else {
                frame.extend_from_slice(&[0, 0, 0x14, 0x64]);
                frame.extend_from_slice(&(extension.len() as u32).to_le_bytes());
            }
            frame.extend(extension);
            frame.extend(body);
            self.socket.write_all(&frame).await?;
            loop {
                let packet = self.read().await?;
                if packet.command == command && packet.sequence == sequence {
                    ensure!(
                        matches!(packet.status, 200 | 201)
                            || (packet.status == 300 && !matches!(command, 18 | 19 | 295))
                            || (challenge && packet.status & 0xff00 == 0xdd00),
                        Rejected {
                            command,
                            status: packet.status
                        }
                    );
                    return Ok(packet);
                }
                if matches!(packet.command, 33 | 291) && self.pushes.len() < 32 {
                    self.pushes.push_back(packet);
                }
            }
        })
        .await?
    }

    pub async fn read(&mut self) -> Result<Packet> {
        loop {
            if let Some(packet) = Packet::take(&mut self.buffer)? {
                return Ok(packet);
            }
            if self.socket.read_buf(&mut self.buffer).await? == 0 {
                bail!("Baichuan connection closed");
            }
        }
    }

    fn encode(&self, xml: &str, channel: u8) -> Vec<u8> {
        if xml.is_empty() {
            return Vec::new();
        }
        let mut bytes = Vec::with_capacity(38 + xml.len());
        bytes.extend_from_slice(br#"<?xml version="1.0" encoding="UTF-8"?>"#);
        bytes.extend_from_slice(xml.as_bytes());
        if let Some(key) = &self.key {
            cfb_mode::Encryptor::<Aes128>::new(key.into(), IV.into()).encrypt(&mut bytes);
        } else {
            xor(&mut bytes, channel);
        }
        bytes
    }

    pub fn decode(&self, packet: &Packet) -> Result<String> {
        if packet.body.is_empty() {
            return Ok(String::new());
        }
        if packet.offset != 0
            && let Ok(body) = self.decode_xml(&packet.body[packet.offset..], packet.sequence as u8)
        {
            return Ok(body);
        }
        let end = if packet.offset == 0 {
            packet.body.len()
        } else {
            packet.offset
        };
        self.decode_xml(&packet.body[..end], packet.sequence as u8)
    }

    pub fn decode_xml(&self, data: &[u8], channel: u8) -> Result<String> {
        let parse = |data: &[u8]| {
            std::str::from_utf8(data)
                .ok()
                .filter(|text| text.trim_start().starts_with('<') && text.trim_end().ends_with('>'))
                .map(str::to_owned)
        };
        if let Some(xml) = parse(data) {
            return Ok(xml);
        }
        let mut bytes = data.to_vec();
        if let Some(key) = &self.key {
            cfb_mode::Decryptor::<Aes128>::new(key.into(), IV.into()).decrypt(&mut bytes);
            if let Some(xml) = parse(&bytes) {
                return Ok(xml);
            }
        }
        bytes.copy_from_slice(data);
        xor(&mut bytes, channel);
        parse(&bytes).ok_or_else(|| anyhow!("cannot decode Baichuan XML"))
    }

    pub fn decrypt_binary(&self, bytes: &mut [u8]) {
        if let Some(key) = &self.key {
            cfb_mode::Decryptor::<Aes128>::new(key.into(), IV.into()).decrypt(bytes);
        }
    }
}

impl Packet {
    pub fn payload_finished(&self, session: &Session) -> bool {
        self.body.is_empty()
            || (self.offset == self.body.len()
                && self.offset > 0
                && session
                    .decode_xml(&self.body, self.sequence as u8)
                    .ok()
                    .and_then(|raw| {
                        let doc = xml::document(&raw).ok()?;
                        xml::value(doc.root_element(), "binaryData").map(|_| true)
                    })
                    .unwrap_or(false))
    }
    fn take(buffer: &mut BytesMut) -> Result<Option<Self>> {
        if buffer.len() < 20 {
            return Ok(None);
        }
        ensure!(buffer[..4] == MAGIC, "invalid Baichuan packet marker");
        let command = le_u32(buffer, 4);
        let length = le_u32(buffer, 8) as usize;
        let sequence = le_u32(buffer, 12);
        let status = u16::from_le_bytes(buffer[16..18].try_into().unwrap());
        let header = match &buffer[18..20] {
            [0x14, 0x65 | 0x66] => 20,
            [0x14, 0x64] | [0, 0] => 24,
            _ => bail!("unsupported Baichuan packet header"),
        };
        ensure!(length <= MAX_PACKET, "Baichuan packet exceeds size limit");
        if buffer.len() < header + length {
            return Ok(None);
        }
        let offset = if header == 24 {
            le_u32(buffer, 20) as usize
        } else {
            0
        };
        ensure!(offset <= length, "invalid Baichuan packet offset");
        let mut body = buffer.split_to(header + length).freeze();
        body.advance(header);
        Ok(Some(Self {
            command,
            sequence,
            status,
            offset,
            body,
        }))
    }

    pub fn binary(&self, session: &Session) -> Option<Bytes> {
        if self.offset == self.body.len() || !matches!(self.status, 200 | 201) {
            return None;
        }
        let payload = self.body.slice(self.offset..);
        if self.offset == 0
            && payload.len() < 4096
            && session.decode_xml(&payload, self.sequence as u8).is_ok()
        {
            return None;
        }
        // Cmd 3 continuations may omit the binaryData flag. Its offset still
        // separates the clear video tail from any encrypted XML extension.
        let encrypted = session
            .decode_xml(&self.body[..self.offset], self.sequence as u8)
            .ok()
            .and_then(|extension| {
                let doc = xml::document(&extension).ok()?;
                doc.descendants()
                    .find(|n| {
                        n.is_element() && n.tag_name().name().eq_ignore_ascii_case("encryptLen")
                    })
                    .and_then(xml::text)?
                    .parse::<usize>()
                    .ok()
            })
            .unwrap_or(0);
        if encrypted == 0 {
            return Some(payload);
        }
        let mut payload = payload.to_vec();
        session.decrypt_binary(payload.get_mut(..encrypted)?);
        Some(payload.into())
    }
}

pub fn le_u32(buf: &[u8], start: usize) -> u32 {
    u32::from_le_bytes(buf[start..start + 4].try_into().unwrap())
}

fn xor(bytes: &mut [u8], channel: u8) {
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte ^= XOR_KEY[(usize::from(channel) + index) % XOR_KEY.len()] ^ channel;
    }
}

fn hash(text: &str) -> String {
    let digest = format!("{:X}", md5::compute(text));
    digest[..31].into()
}
