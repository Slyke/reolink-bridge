use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use aes::{
    Aes128,
    cipher::{AsyncStreamCipher, KeyIvInit},
};
use anyhow::{Result, ensure};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub const PASSWORD: &str = "fixture-camera-password";
const NONCE: &str = "fixture-nonce";
const IV: &[u8; 16] = b"0123456789abcdef";

#[derive(Default)]
pub struct Camera {
    pub led: AtomicBool,
    pub writes: AtomicUsize,
    pub webhook: Mutex<Option<String>>,
}

impl Camera {
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let state = self.clone();
            tokio::spawn(async move {
                let _ = state.session(socket).await;
            });
        }
    }

    async fn session(&self, mut socket: TcpStream) -> Result<()> {
        let hash = format!("{:X}", md5::compute(format!("{NONCE}-{PASSWORD}")));
        let key: &[u8; 16] = hash.as_bytes()[..16].try_into()?;
        let mut logged_in = false;
        loop {
            let mut header = [0u8; 20];
            socket.read_exact(&mut header).await?;
            ensure!(&header[..4] == b"\xf0\xde\xbc\x0a");
            let command = u32::from_le_bytes(header[4..8].try_into()?);
            let size = u32::from_le_bytes(header[8..12].try_into()?) as usize;
            let legacy = header[18..20] == [0x14, 0x65];
            let offset = if legacy {
                0
            } else {
                socket.read_u32_le().await? as usize
            };
            let mut body = vec![0u8; size];
            socket.read_exact(&mut body).await?;
            let (status, mut reply) = if legacy {
                (
                    0xdd12u16,
                    format!("<body><Encryption><nonce>{NONCE}</nonce></Encryption></body>"),
                )
            } else if command == 1 {
                xor(&mut body, header[12]);
                let expected = format!("{:X}", md5::compute(format!("{PASSWORD}{NONCE}")));
                ensure!(std::str::from_utf8(&body)?.contains(&expected[..31]));
                logged_in = true;
                (200, String::new())
            } else {
                ensure!(logged_in);
                cfb_mode::Decryptor::<Aes128>::new(key.into(), IV.into())
                    .decrypt(&mut body[offset..]);
                self.handle(command, std::str::from_utf8(&body[offset..])?)?
            };
            header[16..18].copy_from_slice(&status.to_le_bytes());
            header[18..20].copy_from_slice(&[0x14, if legacy { 0x66 } else { 0x64 }]);
            if !reply.is_empty() {
                reply.insert_str(0, "<?xml version=\"1.0\"?>");
            }
            let mut reply = reply.into_bytes();
            if legacy {
                xor(&mut reply, header[12]);
            } else {
                cfb_mode::Encryptor::<Aes128>::new(key.into(), IV.into()).encrypt(&mut reply);
            }
            header[8..12].copy_from_slice(&(reply.len() as u32).to_le_bytes());
            socket.write_all(&header).await?;
            if !legacy {
                socket.write_u32_le(0).await?;
            }
            socket.write_all(&reply).await?;
        }
    }

    fn handle(&self, command: u32, body: &str) -> Result<(u16, String)> {
        let response = match command {
            2 | 93 => String::new(),
            80 => "<body><VersionInfo><name>Test Camera</name><itemNo>CAM-001</itemNo></VersionInfo></body>".into(),
            208 => {
                let state = if self.led.load(Ordering::SeqCst) { "open" } else { "close" };
                format!("<body><LedState><channelId>0</channelId><state>auto</state><lightState>{state}</lightState><untouched>19</untouched></LedState></body>")
            }
            209 => {
                let doc = roxmltree::Document::parse(body)?;
                ensure!(field(&doc, "state") == Some("auto") && field(&doc, "untouched") == Some("19"));
                self.led.store(field(&doc, "lightState") == Some("open"), Ordering::SeqCst);
                self.writes.fetch_add(1, Ordering::SeqCst);
                String::new()
            }
            806 => {
                let webhook = self.webhook.lock().unwrap();
                format!("<body><HaCfg><enable>{}</enable><url>{}</url></HaCfg></body>",
                    u8::from(webhook.is_some()), webhook.as_deref().unwrap_or(""))
            }
            807 => {
                let doc = roxmltree::Document::parse(body)?;
                ensure!(field(&doc, "enable") == Some("1"));
                *self.webhook.lock().unwrap() = field(&doc, "url").map(str::to_owned);
                String::new()
            }
            _ => return Ok((405, String::new())),
        };
        Ok((200, response))
    }
}

fn field<'a>(doc: &'a roxmltree::Document<'_>, name: &str) -> Option<&'a str> {
    doc.descendants()
        .find(|node| node.has_tag_name(name))?
        .text()
}

fn xor(body: &mut [u8], channel: u8) {
    let key = [0x1f, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0xff];
    for (index, byte) in body.iter_mut().enumerate() {
        *byte ^= key[(usize::from(channel) + index) % key.len()] ^ channel;
    }
}
