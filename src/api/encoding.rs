use super::*;
use crate::{
    reolink::wire::{Rejected, Session},
    xml,
};
use roxmltree::Node;
use serde::Serialize;

use crate::reolink::wire::LiveStream as Stream;
impl Stream {
    fn element(self) -> &'static str {
        match self {
            Self::Main => "mainStream",
            Self::Sub => "subStream",
            Self::Third => "thirdStream",
        }
    }
}
#[derive(Default, Deserialize, Serialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Values {
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitrate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gop: Option<u32>,
}
impl Values {
    fn matches(&self, actual: &Self) -> bool {
        [
            (self.width, actual.width),
            (self.height, actual.height),
            (self.fps, actual.fps),
            (self.bitrate, actual.bitrate),
            (self.gop, actual.gop),
        ]
        .iter()
        .all(|(want, got)| want.is_none() || want == got)
    }
}
#[derive(Serialize, PartialEq, Clone)]
struct OptionSet {
    width: u32,
    height: u32,
    fps: Vec<u32>,
    bitrates: Vec<u32>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    stream: Stream,
    current: Values,
    options: Vec<OptionSet>,
    gop_range: Option<[u32; 2]>,
    bitrate_mode: Option<String>,
    encoder_profile: Option<String>,
}
#[derive(Serialize)]
struct Encoding {
    profiles: Vec<Profile>,
    #[serde(skip)]
    literal_frames: Option<u32>,
}
fn number(node: Node<'_, '_>, tag: &str) -> Option<u32> {
    xml::child(node, tag)?.text()?.trim().parse().ok()
}
fn items(node: Node<'_, '_>, tag: &str) -> Vec<u32> {
    xml::child(node, tag)
        .and_then(xml::text)
        .unwrap_or_default()
        .split(',')
        .take(128)
        .filter_map(|v| v.trim().parse().ok())
        .filter(|v| *v > 0)
        .collect()
}
fn parse(current: &str, tables: &str, channel: u8) -> Result<Encoding> {
    let current = xml::document(current)?;
    let root = xml::find(current.root_element(), "Compression")
        .ok_or_else(|| anyhow::anyhow!("missing encoder settings"))?;
    ensure!(
        number(root, "channelId").is_none_or(|v| v == u32::from(channel)),
        "encoder channel mismatch"
    );
    let tables = xml::document(tables)?;
    let mut profiles = Vec::new();
    for stream in [Stream::Main, Stream::Sub, Stream::Third] {
        let Some(raw) = xml::child(root, stream.element()) else {
            continue;
        };
        let gop = xml::child(raw, "gop");
        let current = Values {
            width: number(raw, "width").filter(|v| *v > 0),
            height: number(raw, "height").filter(|v| *v > 0),
            fps: number(raw, "frame").filter(|v| *v > 0),
            bitrate: number(raw, "bitRate").filter(|v| *v > 0),
            gop: gop.and_then(|g| number(g, "cur")).filter(|v| *v > 0),
        };
        if current == Values::default() {
            continue;
        }
        let mut options = Vec::new();
        for group in tables
            .descendants()
            .filter(|n| n.has_tag_name("StreamInfo"))
        {
            let mask = xml::child(group, "channelBits")
                .and_then(xml::text)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            if mask & 1u64.checked_shl(channel.into()).unwrap_or(0) == 0 {
                continue;
            }
            for table in group.children().filter(|n| n.has_tag_name("encodeTable")) {
                let kind = xml::value(table, "type");
                if kind != Some(stream.element())
                    && !(stream == Stream::Third && kind == Some("externStream"))
                {
                    continue;
                }
                let Some(resolution) = xml::child(table, "resolution") else {
                    continue;
                };
                let (Some(width), Some(height)) =
                    (number(resolution, "width"), number(resolution, "height"))
                else {
                    continue;
                };
                if width == 0 || height == 0 {
                    continue;
                }
                let settings = OptionSet {
                    width,
                    height,
                    fps: items(table, "framerateTable"),
                    bitrates: items(table, "bitrateTable"),
                };
                if !options.contains(&settings) {
                    options.push(settings);
                }
            }
        }
        profiles.push(Profile {
            stream,
            current,
            options,
            gop_range: gop
                .and_then(|g| Some([number(g, "min")?, number(g, "max")?]))
                .filter(|r| r[0] > 0 && r[0] <= r[1]),
            bitrate_mode: xml::value(raw, "encoderType")
                .filter(|s| matches!(*s, "cbr" | "vbr"))
                .map(str::to_owned),
            encoder_profile: xml::value(raw, "encoderProfile")
                .filter(|s| matches!(*s, "default" | "baseLine" | "high" | "main"))
                .map(str::to_owned),
        });
    }
    Ok(Encoding {
        profiles,
        literal_frames: number(root, "isNoTranslateFrame").filter(|v| *v <= 1),
    })
}
async fn read(session: &mut Session, channel: u8) -> Result<Encoding> {
    let raw = match session.command(56, Some(channel), "").await {
        Err(e) if e.is::<Rejected>() => {
            return Ok(Encoding {
                profiles: Vec::new(),
                literal_frames: None,
            });
        }
        result => result?,
    };
    let tables = match session.command(146, None, "").await {
        Err(e) if e.is::<Rejected>() => "<body/>".to_owned(),
        result => result?,
    };
    parse(&raw, &tables, channel)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    stream: Stream,
    settings: Values,
}
fn setting(
    change: &Change,
    profile: &Profile,
    channel: u8,
    literal: Option<u32>,
) -> Option<String> {
    let v = &change.settings;
    let c = &profile.current;
    if v == &Values::default() || v.width.is_some() != v.height.is_some() {
        return None;
    }
    if let Some(value) = v.gop {
        let [min, max] = profile.gop_range?;
        if c.gop.is_none() || !(min..=max).contains(&value) {
            return None;
        }
    }
    let switching = v.width.is_some() && (v.width != c.width || v.height != c.height);
    if v.width.is_some() || v.fps.is_some() || v.bitrate.is_some() {
        let width = v.width.or(c.width)?;
        let height = v.height.or(c.height)?;
        let allowed = profile.options.iter().any(|option| {
            let fps = v.fps.or(if switching { c.fps } else { None });
            let bitrate = v.bitrate.or(if switching { c.bitrate } else { None });
            option.width == width
                && option.height == height
                && fps.is_none_or(|f| c.fps.is_some() && option.fps.contains(&f))
                && bitrate.is_none_or(|b| c.bitrate.is_some() && option.bitrates.contains(&b))
        });
        if !allowed {
            return None;
        }
    }
    let tag = change.stream.element();
    let mut data = format!("<body><Compression version=\"1.1\"><channelId>{channel}</channelId>");
    if let Some(value) = literal.filter(|_| v.fps.is_some()) {
        data.push_str(&format!("<isNoTranslateFrame>{value}</isNoTranslateFrame>"))
    }
    data.push_str(&format!("<{tag}>"));
    for (key, value) in [
        ("width", v.width),
        ("height", v.height),
        ("frame", v.fps),
        ("bitRate", v.bitrate),
    ] {
        if let Some(n) = value {
            data.push_str(&format!("<{key}>{n}</{key}>"));
        }
    }
    if let Some(n) = v.gop {
        data.push_str(&format!("<gop><cur>{n}</cur></gop>"))
    }
    data.push_str(&format!("</{tag}></Compression></body>"));
    Some(data)
}

pub(super) async fn get(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Some(device) = api.devices.get(&id) else {
        return error(StatusCode::NOT_FOUND, "API_ENCODING_UNSUPPORTED", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let read = || async {
        let mut session = device.connect().await?;
        let result = read(&mut session, device.channel).await;
        session.logout().await;
        result
    };
    match timeout(
        Duration::from_secs(20),
        device.read_retries.read("encoding", read),
    )
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        _ => error(StatusCode::BAD_GATEWAY, "API_ENCODING_READ_FAILED", &cid),
    }
}
pub(super) async fn set(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    body: Result<Json<Change>, JsonRejection>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Ok(Json(change)) = body else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ENCODING_INVALID",
            &cid,
        );
    };
    let Some(device) = api.devices.get(&id) else {
        return error(StatusCode::NOT_FOUND, "API_ENCODING_UNSUPPORTED", &cid);
    };
    let Ok(_guard) = api.network_writes[&id].try_lock() else {
        return error(StatusCode::CONFLICT, "API_DISPLAY_BUSY", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let work = async {
        let mut session = device
            .connect()
            .await
            .map_err(|_| "API_ENCODING_READ_FAILED")?;
        let result = async {
            let before = read(&mut session, device.channel)
                .await
                .map_err(|_| "API_ENCODING_READ_FAILED")?;
            let profile = before
                .profiles
                .iter()
                .find(|p| p.stream == change.stream)
                .ok_or("API_ENCODING_UNSUPPORTED")?;
            let xml = setting(&change, profile, device.channel, before.literal_frames)
                .ok_or("API_ENCODING_INVALID")?;
            if change.settings.matches(&profile.current) {
                return Ok(before);
            }
            session
                .command(57, Some(device.channel), &xml)
                .await
                .map_err(|e| {
                    if e.is::<Rejected>() {
                        "API_ENCODING_REJECTED"
                    } else {
                        "API_ENCODING_UNVERIFIED"
                    }
                })?;
            let after = read(&mut session, device.channel)
                .await
                .map_err(|_| "API_ENCODING_UNVERIFIED")?;
            if after
                .profiles
                .iter()
                .any(|p| p.stream == change.stream && change.settings.matches(&p.current))
            {
                Ok(after)
            } else {
                Err("API_ENCODING_UNVERIFIED")
            }
        }
        .await;
        session.logout().await;
        result
    };
    match timeout(Duration::from_secs(30), work)
        .await
        .unwrap_or(Err("API_ENCODING_UNVERIFIED"))
    {
        Ok(value) => {
            Json(json!({"outcome":"confirmed","profiles":value.profiles,"correlation_id":cid.0}))
                .into_response()
        }
        Err(key) => error(
            if matches!(
                key,
                "API_ENCODING_UNSUPPORTED" | "API_ENCODING_REJECTED" | "API_ENCODING_INVALID"
            ) {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_GATEWAY
            },
            key,
            &cid,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoder_options_are_scoped_to_the_channel_and_stream_and_writes_are_partial() {
        let raw = "<body><Compression><channelId>0</channelId><isNoTranslateFrame>1</isNoTranslateFrame><mainStream><width>2560</width><height>1440</height><frame>20</frame><bitRate>4096</bitRate><gop><min>1</min><max>2</max><cur>2</cur></gop></mainStream><subStream><width>640</width><height>360</height></subStream></Compression></body>";
        let table = "<encodeTable><type>mainStream</type><resolution><width>1920</width><height>1080</height></resolution><framerateTable>15,10</framerateTable><bitrateTable>2048,3072</bitrateTable></encodeTable>";
        let tables = format!(
            "<body><StreamInfo><channelBits>1</channelBits>{table}{table}</StreamInfo><StreamInfo><channelBits>2</channelBits>{}</StreamInfo></body>",
            table.replace("1920", "3840")
        );
        let enc = parse(raw, &tables, 0).unwrap();
        let main = &enc.profiles[0];
        assert_eq!(main.options.len(), 1);
        assert!(enc.profiles[1].options.is_empty());
        let with_third = raw.replace(
            "</subStream>",
            "</subStream><thirdStream><width>896</width><height>512</height></thirdStream>",
        );
        assert!(
            parse(&with_third, &tables, 0).unwrap().profiles[2]
                .options
                .is_empty()
        );
        let third_tables = tables.replace("mainStream", "externStream");
        assert_eq!(
            parse(&with_third, &third_tables, 0).unwrap().profiles[2]
                .options
                .len(),
            1
        );
        let mut change = Change {
            stream: Stream::Main,
            settings: Values {
                width: Some(1920),
                height: Some(1080),
                ..Default::default()
            },
        };
        assert!(setting(&change, main, 0, enc.literal_frames).is_none());
        change.settings.fps = Some(15);
        change.settings.bitrate = Some(3072);
        let xml = setting(&change, main, 0, enc.literal_frames).unwrap();
        assert!(xml.contains("<frame>15</frame>"));
        assert!(!xml.contains("subStream"));
        assert!(!xml.contains("encoderProfile"));
        assert!(!xml.contains("gop"));
        change.settings.gop = Some(3);
        assert!(setting(&change, main, 0, enc.literal_frames).is_none());
    }
}
