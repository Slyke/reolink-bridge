use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
    time::sleep,
};

pub async fn fixture(listener: TcpListener) -> Result<()> {
    let output = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=blue:size=160x120:rate=16",
            "-frames:v",
            "1",
            "-c:v",
            "libx264",
            "-profile:v",
            "baseline",
            "-threads",
            "1",
            "-x264-params",
            "keyint=1:slices=1",
            "-f",
            "h264",
            "pipe:1",
        ])
        .output()
        .await?;
    ensure!(output.status.success(), "fixture encoder failed");
    let data = output.stdout;
    let mut prefix = Vec::new();
    let mut at = 0;
    while at + 3 <= data.len() {
        let size = if data[at..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[at..].starts_with(&[0, 0, 1]) {
            3
        } else {
            0
        };
        if size != 0 {
            prefix.push((at, at + size));
            at += size;
        } else {
            at += 1;
        }
    }
    let nal = |kind| {
        prefix.iter().enumerate().find_map(|(i, &(_, start))| {
            let end = prefix.get(i + 1).map_or(data.len(), |&(offset, _)| offset);
            (start < end && data[start] & 31 == kind).then_some(&data[start..end])
        })
    };
    let sps = nal(7).context("fixture SPS")?;
    let pps = nal(8).context("fixture PPS")?;
    let frame = nal(5).context("fixture IDR")?.to_vec();
    let session_description = format!(
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=Camera fixture\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\na=control:*\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=0\r\na=fmtp:96 packetization-mode=1;sprop-parameter-sets={},{}\r\n",
        STANDARD.encode(sps),
        STANDARD.encode(pps),
    );
    let url = format!("rtsp://{}/Preview_01_main/", listener.local_addr()?);
    loop {
        let (socket, _) = listener.accept().await?;
        let description = session_description.clone();
        let frame = frame.clone();
        let url = url.clone();
        tokio::spawn(async move {
            let _ = session(socket, description, frame, url).await;
        });
    }
}

async fn session(
    socket: TcpStream,
    description: String,
    frame: Vec<u8>,
    url: String,
) -> Result<()> {
    let (read, mut write) = socket.into_split();
    let mut buffered = BufReader::new(read);
    loop {
        let mut line = String::new();
        if buffered.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let method = line
            .split_whitespace()
            .next()
            .context("fixture request method")?
            .to_owned();
        let mut sequence = String::new();
        loop {
            line.clear();
            buffered.read_line(&mut line).await?;
            if line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("cseq")
            {
                sequence = value.trim().into();
            }
        }
        let body = match method.as_str() {
          "OPTIONS" => "Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n".into(),
          "DESCRIBE" => format!("Content-Type: application/sdp\r\nContent-Base: {url}\r\nContent-Length: {}\r\n\r\n{description}",description.len()),
          "SETUP" => "Session: 112233\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=12345678\r\n\r\n".into(),
          "PLAY" => format!("Session: 112233\r\nRange: npt=0.000-\r\nRTP-Info: url={url}trackID=0;seq=100;rtptime=0\r\n\r\n"),
          "TEARDOWN" => return Ok(()),
          _ => return Ok(()),
        };
        write
            .write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {sequence}\r\n{body}").as_bytes())
            .await?;
        if method == "PLAY" {
            for index in 0..200u16 {
                let size = u16::try_from(12 + frame.len())?;
                let mut packet = vec![b'$', 0];
                packet.extend_from_slice(&size.to_be_bytes());
                packet.extend_from_slice(&[0x80, 0xe0]);
                packet.extend_from_slice(&(100 + index).to_be_bytes());
                packet.extend_from_slice(&(u32::from(index) * 5625).to_be_bytes());
                packet.extend_from_slice(&0x12345678u32.to_be_bytes());
                packet.extend_from_slice(&frame);
                write.write_all(&packet).await?;
                sleep(Duration::from_millis(42)).await;
            }
            return Ok(());
        }
    }
}
