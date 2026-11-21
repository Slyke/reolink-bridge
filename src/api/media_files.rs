//! SD media inventory and exact-file deletion. Retention policy and scheduling belong to the caller.
use super::{Api, error, recordings::read_failure};
use crate::{
    http::CorrelationId,
    reolink::{recordings, timelapse_photos, wire::Session},
    xml,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Extension, Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum Kind {
    Video,
    TimeLapse,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Calendar {
    kind: Kind,
    month: Option<NaiveDate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    kind: Kind,
    date: NaiveDate,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Delete {
    source: String,
    kind: Kind,
    date: NaiveDate,
    files: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    video: bool,
    time_lapse: bool,
}

impl Capabilities {
    fn allows(&self, kind: Kind) -> bool {
        match kind {
            Kind::Video => self.video,
            Kind::TimeLapse => self.time_lapse,
        }
    }
}

fn support(raw: &str, channel: u8) -> Result<Capabilities> {
    let doc = xml::document(raw)?;
    let root = xml::find(doc.root_element(), "Support").context("missing camera capabilities")?;
    let number = |node, key| {
        xml::child(node, key)
            .and_then(xml::text)
            .and_then(|x| x.parse::<u32>().ok())
            .unwrap_or(0)
    };
    let item = root.descendants().find(|n| {
        n.has_tag_name("item")
            && xml::child(*n, "chnID")
                .or_else(|| xml::child(*n, "channelId"))
                .and_then(xml::text)
                .and_then(|v| v.parse::<u8>().ok())
                == Some(channel)
    });
    // The SDK's GetSupportDeleteRecordFiles reads bit 3 of replayVersion.
    Ok(Capabilities {
        video: number(root, "replayVersion") & 8 != 0,
        time_lapse: item.map_or_else(
            || number(root, "timelapse"),
            |item| number(item, "timelapse"),
        ) != 0,
    })
}

async fn features(session: &mut Session, channel: u8) -> Result<Capabilities> {
    support(&session.command(199, None, "").await?, channel)
}

pub(super) async fn source(session: &mut Session, channel: u8) -> Result<String> {
    let uid = recordings::uid(session).await?;
    Ok(format!("{:x}", Sha256::digest(format!("{uid}\0{channel}"))))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Format {
    Video,
    Jpeg,
    Mp4,
}
impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Jpeg => "jpeg",
            Self::Mp4 => "mp4",
        }
    }
}

struct File {
    native: String,
    format: Format,
    finished: NaiveDateTime,
    playback_ids: BTreeSet<String>,
}
#[derive(Default)]
struct Inventory {
    files: BTreeMap<String, File>,
    identities: BTreeMap<(Format, String), Option<String>>,
    skipped: usize,
}
impl Inventory {
    fn add(
        &mut self,
        source: &str,
        format: Format,
        native: Option<String>,
        finished: Option<NaiveDateTime>,
        playback_id: String,
    ) {
        let Some(native) =
            native.filter(|v| !v.is_empty() && v.len() <= 512 && !v.chars().any(char::is_control))
        else {
            self.skipped += 1;
            return;
        };
        let key = (format, native.clone());
        let candidate = finished.map(|time| {
            format!(
                "{:x}",
                Sha256::digest(format!("{source}\0{}\0{native}\0{time}", format.name()))
            )
        });
        // Streams and task listings may refer to one physical file. Withhold a file if
        // any alias reports a different completion time or still has no completion time.
        if let Some(previous) = self.identities.get_mut(&key) {
            if *previous != candidate {
                if let Some(old) = previous.take() {
                    self.files.remove(&old);
                    self.skipped += 1;
                }
            } else if let Some(file) = previous.as_ref().and_then(|id| self.files.get_mut(id)) {
                file.playback_ids.insert(playback_id);
            }
            return;
        }
        self.identities.insert(key, candidate.clone());
        if let Some((id, finished)) = candidate.zip(finished) {
            self.files.insert(
                id,
                File {
                    native,
                    format,
                    finished,
                    playback_ids: BTreeSet::from([playback_id]),
                },
            );
        } else {
            self.skipped += 1;
        }
    }
}

async fn inventory(
    s: &mut Session,
    channel: u8,
    scope: &str,
    source: &str,
    q: &Selection,
) -> Result<Inventory> {
    let from = q.date.and_time(NaiveTime::MIN);
    let to = q.date.and_hms_opt(23, 59, 59).unwrap();
    let mut result = Inventory::default();
    match q.kind {
        Kind::Video => {
            for stream in ["main", "sub"] {
                let (clips, truncated) = recordings::search(s, channel, from, to, stream).await?;
                ensure!(!truncated, "incomplete recording index");
                for clip in clips {
                    result.add(
                        source,
                        Format::Video,
                        clip.file_identity,
                        Some(clip.end),
                        clip.id,
                    );
                }
            }
        }
        Kind::TimeLapse => {
            for photo in timelapse_photos::search_files(s, channel, scope, from, to, true).await? {
                let format = if photo.format == "jpeg" {
                    Format::Jpeg
                } else {
                    Format::Mp4
                };
                result.add(
                    source,
                    format,
                    Some(photo.file),
                    photo.completed_at,
                    photo.id,
                );
            }
        }
    }
    Ok(result)
}

async fn video_dates(
    s: &mut Session,
    channel: u8,
    first: NaiveDate,
) -> Result<BTreeSet<NaiveDate>> {
    ensure!(first.day() == 1, "expected the first day of a month");
    let last = first
        .checked_add_months(Months::new(1))
        .and_then(|n| n.pred_opt())
        .context("invalid month")?;
    let body = format!(
        r#"<body><DayRecords version="1.1">{}{}<DayRecordList><DayRecord><channelId>{channel}</channelId></DayRecord></DayRecordList></DayRecords></body>"#,
        recordings::time_xml("startTime", first.and_time(NaiveTime::MIN)),
        recordings::time_xml("endTime", last.and_hms_opt(23, 59, 59).unwrap())
    );
    let raw = s.command(142, None, &body).await?;
    let doc = xml::document(&raw)?;
    let root = xml::find(doc.root_element(), "DayRecords").context("missing recording calendar")?;
    let mut days = BTreeSet::new();
    for row in root.descendants().filter(|n| n.has_tag_name("DayRecord")) {
        if xml::child(row, "channelId")
            .and_then(xml::text)
            .and_then(|id| id.parse::<u8>().ok())
            != Some(channel)
        {
            continue;
        }
        for item in row.descendants().filter(|n| n.has_tag_name("dayType")) {
            let index = xml::value(item, "index")
                .context("missing calendar day")?
                .parse::<u64>()?;
            let day = first
                .checked_add_days(Days::new(index))
                .filter(|d| d <= &last)
                .context("invalid recording calendar day")?;
            days.insert(day);
        }
    }
    Ok(days)
}

// Both operations take exact camera-issued identities. Neither accepts paths or a time-range delete.
fn native_delete(format: Format, ids: &[&str]) -> (u32, String) {
    let (cmd, root, tag) = match format {
        Format::Video => (303, "RecFilesDelete", "fileIdentity"),
        _ => (330, "timelapseFileDel", "id"),
    };
    let mut body = format!(r#"<body><{root} version="1.1">"#);
    if format != Format::Video {
        body.push_str(&format!("<taskType>{}</taskType>", format.name()));
    }
    for id in ids {
        body.push_str(&format!("<item><{tag}>{}</{tag}></item>", xml::escape(id)));
    }
    body.push_str(&format!("</{root}></body>"));
    (cmd, body)
}

fn digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'a'..=b'f'))
}

enum Request {
    Capabilities,
    Calendar(Calendar),
    List(Selection),
    Delete(Delete),
}

async fn run(
    api: &Api,
    camera: &str,
    channel: u8,
    s: &mut Session,
    request: Request,
) -> Result<(StatusCode, Value)> {
    let good = |value| Ok((StatusCode::OK, value));
    if let Request::Calendar(q) = request {
        let days = match (q.kind, q.month) {
            (Kind::Video, Some(month)) if month.day() == 1 => {
                video_dates(s, channel, month).await?
            }
            (Kind::TimeLapse, None) => timelapse_photos::dates(s, channel).await?,
            _ => return Ok((StatusCode::BAD_REQUEST, Value::Null)),
        };
        return good(json!({"days":days}));
    }
    let binding = source(s, channel).await?;
    let scope = api.recording_scope(camera);
    match request {
        Request::Capabilities => {
            let flags = features(s, channel).await?;
            let now = recordings::clock(s).await?;
            good(
                json!({"source":binding,"video":flags.video,"timeLapse":flags.time_lapse,"cameraTime":now}),
            )
        }
        Request::List(q) => {
            let found = inventory(s, channel, &scope, &binding, &q).await?;
            let files = found
                .files
                .iter()
                .map(|(id, f)| json!({"id":id,"completedAt":f.finished}))
                .collect::<Vec<_>>();
            good(json!({"source":binding,"files":files,"skipped":found.skipped}))
        }
        Request::Delete(q) => {
            if q.source != binding {
                return Ok((StatusCode::CONFLICT, Value::Null));
            }
            let flags = features(s, channel).await?;
            if !flags.allows(q.kind) {
                return Ok((StatusCode::UNPROCESSABLE_ENTITY, Value::Null));
            }
            let select = Selection {
                kind: q.kind,
                date: q.date,
            };
            let before = inventory(s, channel, &scope, &binding, &select).await?;
            let requested: BTreeSet<_> = q.files.iter().collect();
            let mut groups = BTreeMap::<Format, BTreeSet<&str>>::new();
            let mut present = Vec::new();
            let mut absent = Vec::new();
            for id in requested {
                if let Some(f) = before.files.get(id) {
                    groups.entry(f.format).or_default().insert(&f.native);
                    present.push(id);
                } else {
                    absent.push(id);
                }
            }
            if !present.is_empty() {
                // Expire cached references even when a later packet fails after the camera accepted earlier ones.
                let playback_ids = present
                    .iter()
                    .flat_map(|id| before.files[*id].playback_ids.iter().map(String::as_str))
                    .collect::<BTreeSet<_>>();
                api.photos.forget(&scope, &playback_ids).await;
                api.recordings.forget(&scope, &playback_ids).await;
                for (format, ids) in groups {
                    for batch in ids.into_iter().collect::<Vec<_>>().chunks(40) {
                        let (cmd, body) = native_delete(format, batch);
                        s.command(
                            cmd,
                            if format == Format::Video {
                                None
                            } else {
                                Some(channel)
                            },
                            &body,
                        )
                        .await?;
                    }
                }
                let after = inventory(s, channel, &scope, &binding, &select).await?;
                ensure!(
                    present.iter().all(|id| {
                        let file = &before.files[*id];
                        !after
                            .identities
                            .contains_key(&(file.format, file.native.clone()))
                    }),
                    "camera still lists a requested file"
                );
            }
            good(json!({"deleted":present,"absent":absent}))
        }
        Request::Calendar(_) => unreachable!(),
    }
}

async fn handle(
    api: Arc<Api>,
    camera: String,
    headers: HeaderMap,
    cid: CorrelationId,
    request: Request,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Some(device) = api.devices.get(&camera) else {
        return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &cid);
    };
    let deleting = matches!(request, Request::Delete(_));
    if let Request::Delete(q) = &request {
        if !digest(&q.source) || q.files.len() > 1024 || q.files.iter().any(|id| !digest(id)) {
            return error(StatusCode::BAD_REQUEST, "API_MEDIA_FILES_INVALID", &cid);
        }
        if q.files.is_empty() {
            return Json(json!({"deleted":[],"absent":[]})).into_response();
        }
    }
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let _guard = if deleting {
        match api.network_writes[&camera].try_lock() {
            Ok(g) => Some(g),
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid),
        }
    } else {
        None
    };
    let work = async {
        let mut session = device.connect().await?;
        let result = run(&api, &camera, device.channel, &mut session, request).await;
        session.logout().await;
        result
    };
    let result = timeout(Duration::from_secs(120), work)
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
    match result {
        Ok((StatusCode::OK, value)) => Json(value).into_response(),
        Ok((status, _)) => error(
            status,
            match status {
                StatusCode::CONFLICT => "API_CAMERA_IDENTITY_CHANGED",
                StatusCode::UNPROCESSABLE_ENTITY => "API_MEDIA_DELETE_UNSUPPORTED",
                _ => "API_MEDIA_FILES_INVALID",
            },
            &cid,
        ),
        Err(cause) => read_failure(
            &api,
            &cid,
            &camera,
            "api::media_files",
            if deleting {
                "API_MEDIA_DELETE_FAILED"
            } else {
                "API_RECORDING_SEARCH_FAILED"
            },
            cause,
        ),
    }
}

pub(super) async fn capabilities(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    handle(api, id, headers, cid, Request::Capabilities).await
}
pub(super) async fn calendar(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    Query(q): Query<Calendar>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    handle(api, id, headers, cid, Request::Calendar(q)).await
}
pub(super) async fn list(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    Query(q): Query<Selection>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    handle(api, id, headers, cid, Request::List(q)).await
}
pub(super) async fn delete(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    body: std::result::Result<Json<Delete>, JsonRejection>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    match body {
        Ok(Json(q)) => handle(api, id, headers, cid, Request::Delete(q)).await,
        Err(_) => error(StatusCode::BAD_REQUEST, "API_MEDIA_FILES_INVALID", &cid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ambiguous_completion_protects_every_alias() {
        let mut index = Inventory::default();
        let complete = NaiveDate::from_ymd_opt(2026, 11, 17)
            .unwrap()
            .and_hms_opt(9, 0, 0);
        for alias in ["main", "sub"] {
            index.add(
                "camera",
                Format::Video,
                Some("physical".into()),
                complete,
                alias.into(),
            );
        }
        assert_eq!(index.files.len(), 1);
        assert_eq!(index.files.values().next().unwrap().playback_ids.len(), 2);
        index.add(
            "camera",
            Format::Video,
            Some("physical".into()),
            None,
            "active".into(),
        );
        index.add(
            "camera",
            Format::Video,
            Some("physical".into()),
            complete,
            "again".into(),
        );
        assert!(index.files.is_empty());
        assert_eq!(index.skipped, 1);
    }

    #[test]
    fn exact_file_commands_and_capabilities() {
        let (cmd, v) = native_delete(Format::Video, &["exact&file", "second"]);
        assert_eq!(cmd, 303);
        assert!(v.contains("<item><fileIdentity>exact&amp;file</fileIdentity></item><item><fileIdentity>second</fileIdentity></item>"));
        let (cmd, p) = native_delete(Format::Jpeg, &["image"]);
        assert_eq!(cmd, 330);
        assert!(p.contains("<taskType>jpeg</taskType><item><id>image</id></item>"));
        assert!(
            native_delete(Format::Mp4, &["movie"])
                .1
                .contains("<taskType>mp4</taskType>")
        );
        let raw = "<Support><replayVersion>1047</replayVersion><item><channelId>0</channelId><timelapse>3</timelapse></item></Support>";
        let flags = support(raw, 0).unwrap();
        assert!(!flags.video && flags.time_lapse);
        assert!(support(&raw.replace("1047", "1055"), 0).unwrap().video);
    }
}
