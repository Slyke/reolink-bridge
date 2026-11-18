use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
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
    pub mirror: AtomicBool,
    pub flip: AtomicBool,
    pub rotation: AtomicI32,
    pub recording: Option<Vec<u8>>,
    pub recording_downloads: AtomicUsize,
    pub recording_end_status: AtomicUsize,
    pub recording_previews: AtomicUsize,
    pub reject_read_command: AtomicUsize,
    pub reject_recording_cover: AtomicBool,
    pub writes: AtomicUsize,
    pub webhook: Mutex<Option<String>>,
    pub ptz: AtomicBool,
    pub zoom: AtomicI32,
    pub motions: Mutex<Vec<(String, Option<u32>)>>,
    pub fail_next_stop: AtomicBool,
    pub jpeg: Option<Vec<u8>>,
    pub snapshot_requests: Mutex<Vec<tokio::time::Instant>>,
    pub snapshot_delay: std::time::Duration,
    pub reject_snapshots: AtomicBool,
    pub password: Mutex<Option<String>>,
    pub credential_writes: AtomicUsize,
    pub block_after_write: AtomicBool,
    pub block_login: AtomicBool,
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
        let password = self
            .password
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| PASSWORD.into());
        let hash = format!("{:X}", md5::compute(format!("{NONCE}-{password}")));
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
                let expected = format!("{:X}", md5::compute(format!("{password}{NONCE}")));
                ensure!(
                    !self.block_login.load(Ordering::SeqCst)
                        && std::str::from_utf8(&body)?.contains(&expected[..31])
                );
                logged_in = true;
                (200, String::new())
            } else {
                ensure!(logged_in);
                cfb_mode::Decryptor::<Aes128>::new(key.into(), IV.into())
                    .decrypt(&mut body[offset..]);
                if command == 59 {
                    let raw = roxmltree::Document::parse(std::str::from_utf8(&body[offset..])?)?;
                    ensure!(
                        field(&raw, "userName") == Some("admin")
                            && field(&raw, "userSetState") == Some("modify")
                    );
                    *self.password.lock().unwrap() =
                        Some(field(&raw, "password").unwrap().to_owned());
                    self.credential_writes.fetch_add(1, Ordering::SeqCst);
                    if self.block_after_write.swap(false, Ordering::SeqCst) {
                        self.block_login.store(true, Ordering::SeqCst);
                    }
                    // The device can apply the write and close the connection without replying.
                    return Ok(());
                }
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
            if matches!(command, 143 | 298) && status == 200 {
                if command == 143 {
                    self.recording_downloads.fetch_add(1, Ordering::SeqCst);
                } else {
                    self.recording_previews.fetch_add(1, Ordering::SeqCst);
                }
                for chunk in self.recording.as_deref().unwrap().chunks(16 * 1024) {
                    header[8..12].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
                    socket.write_all(&header).await?;
                    socket.write_u32_le(0).await?;
                    socket.write_all(chunk).await?;
                }
                header[8..12].copy_from_slice(&0u32.to_le_bytes());
                let status = self.recording_end_status.load(Ordering::SeqCst);
                header[16..18].copy_from_slice(
                    &(if status == 0 { 300u16 } else { status as u16 }).to_le_bytes(),
                );
                socket.write_all(&header).await?;
                socket.write_u32_le(0).await?;
            }
            if command == 109 && status == 200 {
                self.snapshot_requests
                    .lock()
                    .unwrap()
                    .push(tokio::time::Instant::now());
                tokio::time::sleep(self.snapshot_delay).await;
                for chunk in self.jpeg.as_deref().unwrap().chunks(64 * 1024) {
                    let mut extension = format!(
                        r#"<?xml version="1.0"?><Extension><binaryData>1</binaryData><encryptLen>{}</encryptLen></Extension>"#,
                        chunk.len().min(16),
                    ).into_bytes();
                    let mut payload = chunk.to_vec();
                    let encrypt_len = payload.len().min(16);
                    cfb_mode::Encryptor::<Aes128>::new(key.into(), IV.into())
                        .encrypt(&mut extension);
                    cfb_mode::Encryptor::<Aes128>::new(key.into(), IV.into())
                        .encrypt(&mut payload[..encrypt_len]);
                    header[8..12]
                        .copy_from_slice(&((extension.len() + payload.len()) as u32).to_le_bytes());
                    socket.write_all(&header).await?;
                    socket.write_u32_le(extension.len() as u32).await?;
                    socket.write_all(&extension).await?;
                    socket.write_all(&payload).await?;
                }
            }
        }
    }

    fn handle(&self, command: u32, body: &str) -> Result<(u16, String)> {
        if self.reject_read_command.load(Ordering::SeqCst) == command as usize {
            return Ok((405, String::new()));
        }
        let response = match command {
            2 | 16 | 31 | 93 | 144 => String::new(),
            102=>"<body><HddInfoList><HddInfo><number>0</number><storageType>2</storageType><mount>1</mount><format>1</format><capacity>63</capacity><capacityM>500</capacityM><remainSize>23</remainSize><remainSizeM>800</remainSizeM></HddInfo></HddInfoList></body>".into(),
            104=>"<body><SystemGeneral><year>2026</year><month>11</month><day>17</day><hour>12</hour><minute>30</minute><second>00</second></SystemGeneral></body>".into(),
            14=> {
                ensure!(body.contains("<streamType>subStream</streamType>") || body.contains("<streamType>mainStream</streamType>"));
                "<body><FileInfoList><FileInfo><handle>42</handle></FileInfo></FileInfoList></body>".into()
            }
            15=>"<body><FileInfoList><bFinished>1</bFinished><FileInfo><name>RecS01_20261117_120000_120002_AAA.mp4</name><size>18000</size><type>md</type></FileInfo></FileInfoList></body>".into(),
            298 => {
                if self.reject_recording_cover.load(Ordering::SeqCst) { return Ok((405,String::new())); }
                if self.recording.is_none() {return Ok((405,String::new()))}
                let doc=roxmltree::Document::parse(body)?;
                ensure!(field(&doc,"channelId")==Some("0") && field(&doc,"frameNo")==Some("1"));
                ensure!(field(&doc,"streamType")==Some("subStream"));
                "<body><CoverPreview/></body>".into()
            }
            143=> {
                if self.recording.is_none() {return Ok((405,String::new()))}
                ensure!(body.contains("<streamType>subStream</streamType>"));
                "<body><FileInfoList/></body>".into()
            }
            26=>format!("<body><VideoInput><channelId>0</channelId><bright>128</bright></VideoInput><InputAdvanceCfg><Mirror><enable>{}</enable></Mirror><Flip><enable>{}</enable></Flip><rotationRange><angle>0</angle><angle>90</angle><angle>180</angle><angle>270</angle></rotationRange><rotation>{}</rotation><untouched>19</untouched></InputAdvanceCfg></body>",
                u8::from(self.mirror.load(Ordering::SeqCst)),u8::from(self.flip.load(Ordering::SeqCst)),self.rotation.load(Ordering::SeqCst)),
            25=>{
                let doc=roxmltree::Document::parse(body)?;
                ensure!(field(&doc,"untouched")==Some("19") && field(&doc,"bright")==Some("128"));
                let value=|tag|doc.descendants().find(|n|n.has_tag_name(tag)).and_then(|n|n.descendants().find(|n|n.has_tag_name("enable"))).and_then(|n|n.text());
                self.mirror.store(value("Mirror")==Some("1"),Ordering::SeqCst);
                self.flip.store(value("Flip")==Some("1"),Ordering::SeqCst);
                self.rotation.store(field(&doc,"rotation").unwrap().parse()?,Ordering::SeqCst);
                self.writes.fetch_add(1,Ordering::SeqCst);
                String::new()
            }
            109 => {
                let Some(jpeg) = self.jpeg.as_ref().filter(|_| !self.reject_snapshots.load(Ordering::SeqCst)) else {
                    return Ok((405, String::new()));
                };
                let doc = roxmltree::Document::parse(body)?;
                ensure!(field(&doc, "channelId") == Some("0"));
                ensure!(field(&doc, "streamType") == Some("main"));
                format!("<body><Snap><channelId>0</channelId><pictureSize>{}</pictureSize></Snap></body>", jpeg.len())
            }
            199 => {
                let kind = if self.ptz.load(Ordering::SeqCst) { 2 } else { 0 };
                let control = if kind == 2 { 64 } else { 0 };
                let presets = if kind == 2 { 1 } else { 0 };
                format!("<body><Support><item><chnID>0</chnID><ptzType>{kind}</ptzType><ptzControl>{control}</ptzControl><ptzPreset>{presets}</ptzPreset></item><item><chnID>1</chnID><ptzType>0</ptzType><ptzControl>0</ptzControl></item></Support></body>")
            }
            18 | 19 | 190 | 294 | 295 | 433 if !self.ptz.load(Ordering::SeqCst) => {
                return Ok((405, String::new()));
            }
            18 => {
                let doc = roxmltree::Document::parse(body)?;
                ensure!(field(&doc, "channelId") == Some("0"));
                let command = field(&doc, "command").unwrap();
                let speed = field(&doc, "speed").and_then(|value| value.parse().ok());
                self.motions.lock().unwrap().push((command.into(), speed));
                if command == "Stop" && self.fail_next_stop.swap(false, Ordering::SeqCst) {
                    return Ok((405, String::new()));
                }
                String::new()
            }
            19 => {
                let doc = roxmltree::Document::parse(body)?;
                ensure!(field(&doc, "channelId") == Some("0"));
                ensure!(field(&doc, "command") == Some("toPos"));
                let id = field(&doc, "id").unwrap().parse()?;
                self.motions.lock().unwrap().push(("toPos".into(), Some(id)));
                String::new()
            }
            190 => "<body><PtzPreset version=\"1.1\"><channelId>0</channelId><presetList><preset><id>1</id><name>Fixture entrance</name></preset><preset><id>2</id><name>Unused</name><enable>0</enable></preset></presetList></PtzPreset></body>".into(),
            294 => {
                let zoom = self.zoom.load(Ordering::SeqCst);
                format!("<body><ZoomFocus><channelId>0</channelId><zoom><minPos>0</minPos><maxPos>27</maxPos><curPos>{zoom}</curPos></zoom></ZoomFocus></body>")
            }
            295 => {
                let doc = roxmltree::Document::parse(body)?;
                ensure!(field(&doc, "channelId") == Some("0"));
                ensure!(field(&doc, "command") == Some("ZoomPos"));
                let zoom = field(&doc, "movePos").unwrap().parse()?;
                self.zoom.store(zoom, Ordering::SeqCst);
                String::new()
            }
            433 => "<body><PtzCurPos><channelId>0</channelId><pPos>725</pPos><tPos>110</tPos></PtzCurPos></body>".into(),
            58 => r#"<body><UserList><User><userName>admin</userName><userLevel>1</userLevel><userId>1</userId><loginState>1</loginState></User></UserList></body>"#.into(),
            80 => "<body><VersionInfo><name>Test Camera</name><itemNo>CAM-001</itemNo><hardwareVersion>IPC_NT1NO24MP</hardwareVersion><firmwareVersion>v3.1.0.6198_2604102075</firmwareVersion><serialNumber>FIXTURE-SERIAL</serialNumber></VersionInfo></body>".into(),
            114 => "<body><Uid><uid>952700FIXTURE0001</uid></Uid></body>".into(),
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
