use std::collections::{BTreeMap, BTreeSet};
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
    pub gallery_jpeg: Vec<u8>,
    pub empty_gallery_uid: AtomicBool,
    pub deleted_media: Mutex<BTreeSet<String>>,
    pub video_delete_disabled: AtomicBool,
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
    pub saved_presets: Mutex<BTreeMap<u32, String>>,
    pub live_qualities: Mutex<Vec<(u8, u16)>>,
    pub fail_next_stop: AtomicBool,
    pub jpeg: Option<Vec<u8>>,
    pub snapshot_requests: Mutex<Vec<tokio::time::Instant>>,
    pub snapshot_delay: std::time::Duration,
    pub reject_snapshots: AtomicBool,
    pub version_info: Mutex<Option<String>>,
    pub password: Mutex<Option<String>>,
    pub users: Mutex<BTreeMap<String, (bool, String)>>,
    pub schedule_xml: Mutex<Option<String>>,
    pub rtsp_enabled: AtomicBool,
    pub rtsp_writes: AtomicUsize,
    pub wifi_xml: Mutex<Option<String>>,
    pub encoder_fps: Mutex<Option<u32>>,
    pub clock_xml: Mutex<Option<String>>,
    pub display_xml: Mutex<Option<String>>,
    pub reject_display: AtomicBool,
    pub time_lapse_xml: Mutex<Option<String>>,
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
        let mut key = [0u8; 16];
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
                let raw = std::str::from_utf8(&body)?;
                let matches_user = |name: &str| {
                    let hash = format!("{:X}", md5::compute(format!("{name}{NONCE}")));
                    raw.contains(&hash[..31])
                };
                let login_password = if matches_user("admin") {
                    password.clone()
                } else {
                    self.users
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(name, _)| matches_user(name))
                        .map(|(_, (_, password))| password.clone())
                        .ok_or_else(|| anyhow::anyhow!("unknown fixture user"))?
                };
                let expected = format!("{:X}", md5::compute(format!("{login_password}{NONCE}")));
                let hash = format!("{:X}", md5::compute(format!("{NONCE}-{login_password}")));
                key.copy_from_slice(&hash.as_bytes()[..16]);
                ensure!(
                    !self.block_login.load(Ordering::SeqCst)
                        && std::str::from_utf8(&body)?.contains(&expected[..31])
                );
                logged_in = true;
                (200, String::new())
            } else {
                ensure!(logged_in);
                cfb_mode::Decryptor::<Aes128>::new((&key).into(), IV.into())
                    .decrypt(&mut body[offset..]);
                if command == 59 {
                    let raw = roxmltree::Document::parse(std::str::from_utf8(&body[offset..])?)?;
                    let mut changed = 0;
                    for user in raw.descendants().filter(|u| u.has_tag_name("User")) {
                        let field = |name| {
                            user.children()
                                .find(|f| f.has_tag_name(name))
                                .and_then(|f| f.text())
                        };
                        let name = field("userName").unwrap();
                        let mut users = self.users.lock().unwrap();
                        match field("userSetState") {
                            Some("none") => {
                                ensure!(field("password").is_none());
                                continue;
                            }
                            Some("modify") if name == "admin" => {
                                *self.password.lock().unwrap() =
                                    Some(field("password").unwrap().into())
                            }
                            Some("modify") => {
                                users.get_mut(name).unwrap().1 = field("password").unwrap().into()
                            }
                            Some("add") => {
                                let admin = match field("userLevel") {
                                    Some("1") => true,
                                    Some("0") => false,
                                    _ => anyhow::bail!("unknown level"),
                                };
                                ensure!(
                                    name != "admin"
                                        && users
                                            .insert(
                                                name.into(),
                                                (admin, field("password").unwrap().into())
                                            )
                                            .is_none()
                                );
                            }
                            Some("delete") => {
                                ensure!(
                                    name != "admin"
                                        && field("password").is_none()
                                        && users.remove(name).is_some()
                                );
                            }
                            _ => anyhow::bail!("unexpected user operation"),
                        }
                        changed += 1;
                    }
                    ensure!(changed == 1);
                    self.credential_writes.fetch_add(1, Ordering::SeqCst);
                    if self.block_after_write.swap(false, Ordering::SeqCst) {
                        self.block_login.store(true, Ordering::SeqCst);
                    }
                    // The device can apply the write and close the connection without replying.
                    return Ok(());
                }
                if command == 318 && header[12] == 2 {
                    (200, "<body><ChnTypeInfo><serialNumber>FIXTURE-CHANNEL-1</serialNumber></ChnTypeInfo></body>".into())
                } else {
                    self.handle(command, std::str::from_utf8(&body[offset..])?)?
                }
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
                cfb_mode::Encryptor::<Aes128>::new((&key).into(), IV.into()).encrypt(&mut reply);
            }
            header[8..12].copy_from_slice(&(reply.len() as u32).to_le_bytes());
            socket.write_all(&header).await?;
            if !legacy {
                socket.write_u32_le(0).await?;
            }
            socket.write_all(&reply).await?;
            if command == 3 && status == 200 {
                let request = roxmltree::Document::parse(std::str::from_utf8(&body[offset..])?)?;
                let handle = field(&request, "handle")
                    .ok_or_else(|| anyhow::anyhow!("missing preview handle"))?
                    .parse()?;
                self.live_qualities
                    .lock()
                    .unwrap()
                    .push((header[13], handle));
                for _ in 0..6 {
                    for chunk in self.recording.as_deref().unwrap().chunks(1100) {
                        header[8..12].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
                        socket.write_all(&header).await?;
                        socket.write_u32_le(0).await?;
                        socket.write_all(chunk).await?;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
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
            if matches!(command, 109 | 327) && status == 200 {
                let binary = if command == 327 {
                    self.gallery_jpeg.as_slice()
                } else {
                    self.snapshot_requests
                        .lock()
                        .unwrap()
                        .push(tokio::time::Instant::now());
                    tokio::time::sleep(self.snapshot_delay).await;
                    self.jpeg.as_deref().unwrap()
                };
                for chunk in binary.chunks(if command == 327 { 67 } else { 64 * 1024 }) {
                    let mut extension = format!(
                        r#"<?xml version="1.0"?><Extension><binaryData>1</binaryData><encryptLen>{}</encryptLen></Extension>"#,
                        chunk.len().min(16),
                    ).into_bytes();
                    let mut payload = chunk.to_vec();
                    let encrypt_len = payload.len().min(16);
                    cfb_mode::Encryptor::<Aes128>::new((&key).into(), IV.into())
                        .encrypt(&mut extension);
                    cfb_mode::Encryptor::<Aes128>::new((&key).into(), IV.into())
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
        if matches!(command, 323..=325) {
            let doc = roxmltree::Document::parse(body)?;
            let uid = doc.descendants().find(|n| n.has_tag_name("uid"));
            if self.empty_gallery_uid.load(Ordering::SeqCst) {
                ensure!(
                    uid.is_none(),
                    "standalone SD search should omit the blank storage UID"
                );
            } else {
                ensure!(
                    uid.and_then(|n| n.text()) == Some("disk-1"),
                    "missing camera storage UID"
                );
            }
        }
        let response = match command {
            76 => "<body><Ip><ip>192.0.2.7</ip><mask>255.255.255.0</mask><gateway>192.0.2.1</gateway></Ip><Dhcp><enable>1</enable></Dhcp></body>".into(),
            37 => format!("<body><HttpPort><enable>0</enable></HttpPort><RtspPort><rtspPort>8554</rtspPort><enable>{}</enable></RtspPort></body>", u8::from(self.rtsp_enabled.load(Ordering::SeqCst))),
            36 => {
                let doc = roxmltree::Document::parse(body.trim_end_matches('\0'))?;
                let root = doc.root_element();
                ensure!(root.children().filter(|node|node.is_element()).count() == 1, "unexpected network changes");
                let service = root.first_element_child().unwrap();
                ensure!(service.has_tag_name("RtspPort") && service.children().filter(|node|node.is_element()).count() == 1, "unexpected RTSP changes");
                let flag = service.first_element_child().unwrap();
                ensure!(flag.has_tag_name("enable"), "unexpected RTSP field");
                self.rtsp_enabled.store(flag.text() == Some("1"), Ordering::SeqCst);
                self.rtsp_writes.fetch_add(1, Ordering::SeqCst);
                String::new()
            }
            116 => self.wifi_xml.lock().unwrap().clone().unwrap_or_else(|| "<body><Wifi><ssid>Fixture network</ssid><key>fixture-old-wifi</key><udidList><udid><name>Other network</name></udid></udidList></Wifi></body>".into()),
            117 => {
                let doc = roxmltree::Document::parse(body)?;
                *self.wifi_xml.lock().unwrap() = Some(body[doc.root_element().range()].to_owned());
                String::new()
            }
            44 => self.display_xml.lock().unwrap().clone().unwrap_or_else(|| r#"<body><OsdChannelName version="1.1"><channelId>0</channelId><name>Fixture overlay</name><enable>1</enable><topLeftX>65536</topLeftX><topLeftY>65536</topLeftY><enWatermark>1</enWatermark><enBgcolor>0</enBgcolor></OsdChannelName><OsdDatetime version="1.1"><channelId>0</channelId><enable>1</enable><topLeftX>-32768</topLeftX><topLeftY>1</topLeftY><width>0</width><height>0</height><language>English</language></OsdDatetime></body>"#.into()),
            45 => {
                let doc = roxmltree::Document::parse(body)?;
                let root = doc.root_element();
                let complete = |tag: &str, fields: &[&str]| root.children().find(|n| n.has_tag_name(tag))
                    .is_some_and(|group| fields.iter().all(|f| group.children().any(|n| n.has_tag_name(*f))));
                if !complete("OsdChannelName", &["channelId","name","enable","topLeftX","topLeftY","enWatermark","enBgcolor"])
                    || !complete("OsdDatetime", &["channelId","enable","topLeftX","topLeftY","width","height","language"]) {
                    return Ok((400, String::new()));
                }
                if self.reject_display.swap(false, Ordering::SeqCst) {
                    return Ok((403, String::new()));
                }
                *self.display_xml.lock().unwrap() = Some(body[root.range()].to_owned());
                String::new()
            }
            3 if self.recording.is_some() => {
                let doc = roxmltree::Document::parse(body)?;
                ensure!(matches!((field(&doc,"streamType"),field(&doc,"handle")), (Some("mainStream"),Some("0")) | (Some("subStream"),Some("256")) | (Some("externStream"),Some("1024"))));
                String::new()
            }
            2 | 4 | 16 | 31 | 93 | 144 => String::new(),
            102=>"<body><HddInfoList><HddInfo><number>0</number><storageType>2</storageType><mount>1</mount><format>1</format><capacity>63</capacity><capacityM>500</capacityM><remainSize>23</remainSize><remainSizeM>800</remainSizeM></HddInfo></HddInfoList></body>".into(),
            56=>format!("<body><Compression><channelId>0</channelId><mainStream><width>1920</width><height>1080</height><frame>{}</frame><bitRate>2048</bitRate></mainStream><subStream><width>640</width><height>360</height><frame>4</frame></subStream></Compression></body>",self.encoder_fps.lock().unwrap().unwrap_or(20)),
            146=>"<body><StreamInfoList><StreamInfo><channelBits>1</channelBits><encodeTable><type>mainStream</type><resolution><width>1920</width><height>1080</height></resolution><framerateTable>20,15</framerateTable><bitrateTable>2048,4096</bitrateTable></encodeTable></StreamInfo></StreamInfoList></body>".into(),
            57=>{
                let doc=roxmltree::Document::parse(body)?;
                let main=doc.descendants().find(|n|n.has_tag_name("mainStream")).unwrap();
                ensure!(main.children().filter(|n|n.is_element()).count()==1 && field(&doc,"subStream").is_none());
                *self.encoder_fps.lock().unwrap()=Some(field(&doc,"frame").unwrap().parse()?);
                String::new()
            }
            104=>self.clock_xml.lock().unwrap().clone().unwrap_or_else(||"<body><SystemGeneral><year>2026</year><month>11</month><day>17</day><hour>12</hour><minute>30</minute><second>00</second></SystemGeneral></body>".into()),
            105=>{
                let doc=roxmltree::Document::parse(body)?;
                let general=doc.descendants().find(|n|n.has_tag_name("SystemGeneral")).unwrap();
                ensure!(general.children().filter(|n|n.is_element()).count()==6);
                for name in ["year","month","day","hour","minute","second"] {ensure!(field(&doc,name).is_some());}
                *self.clock_xml.lock().unwrap()=Some(body[doc.root_element().range()].to_owned());
                String::new()
            }
            14=> {
                ensure!(body.contains("<streamType>subStream</streamType>") || body.contains("<streamType>mainStream</streamType>"));
                "<body><FileInfoList><FileInfo><handle>42</handle></FileInfo></FileInfoList></body>".into()
            }
            15 => {
                let item = if self.deleted_media.lock().unwrap().contains("fixture-video") { "" } else {
                    "<FileInfo><Id>fixture-video</Id><name>RecS01_20261117_120000_120002_AAA.mp4</name><size>18000</size><type>md</type></FileInfo>"
                };
                format!("<body><FileInfoList><bFinished>1</bFinished>{item}</FileInfoList></body>")
            }
            142 => {
                let doc = roxmltree::Document::parse(body)?;
                let day = if field(&doc, "month")==Some("11") && field(&doc, "year")==Some("2026") {
                    "<dayType><index>16</index></dayType>"
                } else { "" };
                format!("<body><DayRecords><DayRecordList><DayRecord><channelId>0</channelId>{day}</DayRecord></DayRecordList></DayRecords></body>")
            }
            321 => {
                let uid = if self.empty_gallery_uid.load(Ordering::SeqCst) { "" } else { "disk-1" };
                format!("<body><timelapseTasks><uid>{uid}</uid><item><id>task</id><taskType>jpeg</taskType></item><item><id>movie</id><taskType>mp4</taskType><taskState>IDLE</taskState></item><item><id>running</id><taskType>mp4</taskType><taskState>RUNNING</taskState></item></timelapseTasks></body>")
            }
            322 => {
                let mut days="0".repeat(372);
                days.replace_range(326..327,"1");
                format!("<body><timelapseDateTbl><item><year>2026</year><dateTable>{days}</dateTable></item></timelapseDateTbl></body>")
            }
            323 => {
                ensure!(body.contains("<endTime>"));
                let deleted=self.deleted_media.lock().unwrap();
                let (handle,total) = if body.contains("<taskType>jpeg</taskType>") && body.contains("<id>task</id>") {
                    (3,(0..17).filter(|i| !deleted.contains(&format!("private-image-{i}"))).count())
                } else if body.contains("<taskType>mp4</taskType>") && body.contains("<id>movie</id>") {
                    (4,usize::from(!deleted.contains("private-movie")))
                } else {
                    ensure!(body.contains("<taskType>mp4</taskType>") && body.contains("<id>running</id>"));
                    (5,1)
                };
                format!("<body><timelapseFileSearch><handle>{handle}</handle><totalNum>{total}</totalNum><finished>0</finished></timelapseFileSearch></body>")
            }
            324 => {
                let doc = roxmltree::Document::parse(body)?;
                let clock = |second| format!("<year>2026</year><month>11</month><day>17</day><hour>9</hour><minute>15</minute><second>{second}</second>");
                let (items,done)=match field(&doc,"handle") {
                    Some("3") => {
                        let deleted=self.deleted_media.lock().unwrap();
                        let remaining:Vec<_>=(0..17).filter(|i| !deleted.contains(&format!("private-image-{i}"))).collect();
                        let start = field(&doc, "fromWhere").unwrap_or("0").parse::<usize>()?;
                        ensure!(start <= remaining.len());
                        let end = (start + 16).min(remaining.len());
                        let items=remaining[start..end].iter().map(|second| format!(
                            "<item><startTime>{}</startTime><original><id>private-image-{second}</id><size>{}</size></original><thumbnail><id>ignore-thumb</id></thumbnail></item>",clock(*second),self.gallery_jpeg.len()
                        )).collect::<String>();
                        (items,end==remaining.len())
                    }
                    Some(handle @ ("4"|"5")) => {
                        let file=if handle=="4" {"private-movie"} else {"running-movie"};
                        (format!("<item><startTime>{}</startTime><endTime>{}</endTime><original><id>{file}</id></original></item>",clock(0),clock(40)),true)
                    }
                    _=>anyhow::bail!("unexpected gallery handle"),
                };
                format!("<body><timelapseFileSearch><finished>{}</finished><totalNum>0</totalNum>{items}</timelapseFileSearch></body>", u8::from(done))
            }
            325 => {ensure!(["3","4","5"].iter().any(|h|body.contains(&format!("<handle>{h}</handle>"))));String::new()}
            303 | 330 => {
                let doc=roxmltree::Document::parse(body)?;
                let root=doc.root_element().first_element_child().unwrap();
                ensure!(root.tag_name().name()==if command==303 {"RecFilesDelete"} else {"timelapseFileDel"});
                let tag=if command==303 {"fileIdentity"} else {"id"};
                let ids:Vec<_>=root.children().filter(|n|n.has_tag_name("item")).map(|item| {
                    let id=item.first_element_child().unwrap();
                    ensure!(id.has_tag_name(tag) && id.next_sibling().is_none());
                    let value=id.text().unwrap_or("");
                    ensure!(match (command,field(&doc,"taskType")) {
                        (303,None)=>value=="fixture-video",
                        (330,Some("jpeg"))=>value.starts_with("private-image-"),
                        (330,Some("mp4"))=>value=="private-movie",
                        _=>false,
                    });
                    Ok(value.to_owned())
                }).collect::<Result<Vec<_>>>()?;
                ensure!((1..=40).contains(&ids.len()));
                self.deleted_media.lock().unwrap().extend(ids);
                String::new()
            }
            327 => {
                ensure!(body.contains("<id>private-image-0</id>") && body.contains("<position>0</position>"));
                "<body><timelapseDownload><position>0</position></timelapseDownload></body>".into()
            }
            298 => {
                if self.reject_recording_cover.load(Ordering::SeqCst) { return Ok((405,String::new())); }
                if self.recording.is_none() {return Ok((405,String::new()))}
                let doc=roxmltree::Document::parse(body)?;
                ensure!(field(&doc,"channelId")==Some("0") && field(&doc,"frameNo")==Some("1"));
                ensure!(field(&doc,"streamType")==Some("subStream"));
                "<body><CoverPreview/></body>".into()
            }
            143=> {
                if self.reject_recording_cover.load(Ordering::SeqCst) && body.contains("<second>01</second></startTime>") { return Ok((300,String::new())); }
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
                let replay=if self.video_delete_disabled.load(Ordering::SeqCst) { 0 } else { 8 };
                format!("<body><Support><replayVersion>{replay}</replayVersion><item><chnID>0</chnID><timelapse>3</timelapse><ptzType>{kind}</ptzType><ptzControl>{control}</ptzControl><ptzPreset>{presets}</ptzPreset></item><item><chnID>1</chnID><ptzType>0</ptzType><ptzControl>0</ptzControl></item></Support></body>")
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
                let id = field(&doc, "id").unwrap().parse()?;
                match field(&doc,"command") {
                    Some("setPos") => { self.saved_presets.lock().unwrap().insert(id,field(&doc,"name").unwrap().to_owned()); }
                    Some("delPos") => { self.saved_presets.lock().unwrap().remove(&id); }
                    Some("toPos") => self.motions.lock().unwrap().push(("toPos".into(),Some(id))),
                    _ => anyhow::bail!("unexpected fixture preset command"),
                }
                String::new()
            }
            190 => {
                let mut presets = self.saved_presets.lock().unwrap().clone();
                presets.entry(1).or_insert_with(||"Fixture entrance".into());
                let list: String = presets.into_iter().map(|(id,name)|format!("<preset><id>{id}</id><name>{}</name></preset>",name.replace('&',"&amp;").replace('<',"&lt;"))).collect();
                format!("<body><PtzPreset><channelId>0</channelId><presetList>{list}<preset><id>2</id><name>Unused</name><enable>0</enable></preset></presetList></PtzPreset></body>")
            }
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
            58 => {
                let mut list = String::from("<body><UserList version=\"1.1\"><User><userName>admin</userName><userLevel>1</userLevel><userId>1</userId><loginState>1</loginState><password>fixture-hidden</password></User>");
                for (index,(name,(admin,_))) in self.users.lock().unwrap().iter().enumerate() {
                    let name = name.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
                    list.push_str(&format!("<User><userName>{name}</userName><userLevel>{}</userLevel><userId>{}</userId></User>", u8::from(*admin), index + 2));
                }
                list + "</UserList></body>"
            },
            81 => self.schedule_xml.lock().unwrap().clone().unwrap_or_else(||format!("<body><Record><channelId>0</channelId><enable>1</enable><typeScheduleList><item><type>Normal,</type><valueTable>{}</valueTable></item></typeScheduleList><vendor>keep</vendor></Record></body>","0".repeat(168))),
            319 => self.time_lapse_xml.lock().unwrap().clone().unwrap_or_else(||include_str!("../fixtures/lumus-empty-time-lapse.xml").into()),
            82 | 320 => {
                let target = if command == 82 { &self.schedule_xml } else { &self.time_lapse_xml };
                let doc = roxmltree::Document::parse(body)?;
                if command == 320 && field(&doc, "enable") == Some("1") {
                    if field(&doc, "streamType") == Some("subStream") && field(&doc, "buseThumbnail") == Some("1") {
                        return Ok((400,String::new()));
                    }
                    if target.lock().unwrap().is_none() {
                        let millis = field(&doc, "id").and_then(|v| v.parse::<u64>().ok());
                        if !millis.is_some_and(|n| field(&doc,"properties") == Some(format!("v1_{}_Customize",n/1000).as_str())) {
                            return Ok((400,String::new()));
                        }
                    }
                }
                *target.lock().unwrap() = Some(body[doc.root_element().range()].to_owned());
                self.writes.fetch_add(1,Ordering::SeqCst);
                String::new()
            }
            80 => self.version_info.lock().unwrap().clone().unwrap_or_else(|| "<body><VersionInfo><name>Test Camera</name><itemNo>CAM-001</itemNo><hardwareVersion>IPC_NT1NO24MP</hardwareVersion><firmwareVersion>v3.1.0.6198_2604102075</firmwareVersion><serialNumber>FIXTURE-SERIAL</serialNumber></VersionInfo></body>".into()),
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
