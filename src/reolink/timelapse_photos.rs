use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
};

use anyhow::{Context, Result, anyhow, ensure};
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime};
use image::{DynamicImage, ImageFormat, ImageReader, codecs::jpeg::JpegEncoder};
use roxmltree::Node;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{
    recordings::{time_xml, wall_time},
    wire::Session,
};
use crate::xml;

const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_PHOTOS: usize = 20_000;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Photo {
    pub id: String,
    pub captured_at: Option<NaiveDateTime>,
    #[serde(skip)]
    pub file: String,
    #[serde(skip)]
    pub completed_at: Option<NaiveDateTime>,
    #[serde(skip)]
    pub format: String,
    #[serde(skip)]
    size: Option<usize>,
}

fn field<'a>(node: Node<'a, '_>, tag: &str) -> Option<&'a str> {
    xml::child(node, tag).and_then(xml::text)
}

fn folder(tag: &str, content: &str) -> String {
    format!(r#"<body><{tag} version="1.1">{content}</{tag}></body>"#)
}

fn identifier<'a>(node: Node<'a, '_>, tag: &str) -> Result<Option<&'a str>> {
    let value = field(node, tag);
    ensure!(
        value.is_none_or(|s| s.len() <= 512 && !s.contains(char::is_control)),
        "invalid time-lapse {tag}"
    );
    Ok(value)
}

// JPEG entries on some firmware carry the capture time only in the file name.
// Match the requested camera-local date and never use a parent task's start time.
fn filename_time(name: &str, day: NaiveDate) -> Option<NaiveDateTime> {
    let base = name.rsplit(['/', '\\']).next()?;
    for date_format in ["%Y%m%d", "%Y-%m-%d", "%Y_%m_%d"] {
        let date = day.format(date_format).to_string();
        for (pos, _) in base.match_indices(&date) {
            if pos > 0 && base.as_bytes()[pos - 1].is_ascii_digit() {
                continue;
            }
            let tail = base[pos + date.len()..].trim_start_matches(['T', '_', '-', ' ']);
            for (len, format) in [
                (6, "%H%M%S"),
                (8, "%H-%M-%S"),
                (8, "%H:%M:%S"),
                (8, "%H_%M_%S"),
            ] {
                if let Some(value) = tail.get(..len)
                    && !tail.as_bytes().get(len).is_some_and(u8::is_ascii_digit)
                    && let Ok(clock) = NaiveTime::parse_from_str(value, format)
                {
                    return Some(day.and_time(clock));
                }
            }
        }
    }
    None
}

fn page(
    raw: &str,
    scope: &str,
    task: &Task,
    day: NaiveDate,
    photos: &mut BTreeMap<String, Photo>,
) -> Result<(bool, usize)> {
    let doc = xml::document(raw)?;
    let root = xml::find(doc.root_element(), "timelapseFileSearch")
        .ok_or_else(|| anyhow!("missing time-lapse search result"))?;
    let mut returned = 0;
    for item in root.children().filter(|n| n.has_tag_name("item")) {
        returned += 1;
        let original = xml::child(item, "original")
            .ok_or_else(|| anyhow!("missing original time-lapse image"))?;
        let file = identifier(original, "id")?.context("time-lapse photo has no file ID")?;
        let key = format!(
            "{:x}",
            Sha256::digest(format!("{scope}\0{}\0{file}", task.id))
        );
        let captured_at = [original, item]
            .into_iter()
            .find_map(|node| xml::child(node, "startTime").and_then(wall_time))
            .or_else(|| field(original, "fileName").and_then(|name| filename_time(name, day)))
            .or_else(|| filename_time(file, day));
        let size = field(original, "size")
            .map(str::parse::<usize>)
            .transpose()?
            .filter(|n| *n > 0);
        photos.entry(key.clone()).or_insert(Photo {
            id: key,
            captured_at,
            file: file.to_owned(),
            completed_at: if task.format == "jpeg" {
                captured_at
            } else if task.closed {
                [original, item]
                    .into_iter()
                    .find_map(|node| xml::child(node, "endTime").and_then(wall_time))
            } else {
                None
            },
            format: task.format.clone(),
            size,
        });
        ensure!(
            photos.len() <= MAX_PHOTOS,
            "time-lapse gallery result limit exceeded"
        );
    }
    Ok((field(root, "finished") == Some("1"), returned))
}

struct Task {
    id: String,
    format: String,
    closed: bool,
    params: String,
}

async fn tasks(session: &mut Session, channel: u8) -> Result<Vec<Task>> {
    let target = format!("<channelId>{channel}</channelId>");
    let raw = session
        .command(321, Some(channel), &folder("timelapseTasks", &target))
        .await?;
    let doc = xml::document(&raw)?;
    let root = xml::find(doc.root_element(), "timelapseTasks")
        .context("camera time-lapse gallery unavailable")?;
    let uid = identifier(root, "uid")?
        .map(|u| format!("<uid>{}</uid>", xml::escape(u)))
        .unwrap_or_default();
    let mut result = Vec::new();
    for item in root.children().filter(|n| n.has_tag_name("item")) {
        let format = field(item, "taskType").unwrap_or("").to_ascii_lowercase();
        if !matches!(format.as_str(), "jpeg" | "mp4") {
            continue;
        }
        let id = identifier(item, "id")?
            .context("time-lapse task has no ID")?
            .to_owned();
        let params = format!(
            "{target}{uid}<id>{}</id><taskType>{format}</taskType>",
            xml::escape(&id)
        );
        result.push(Task {
            id,
            format,
            params,
            closed: field(item, "taskState")
                .is_some_and(|state| state.eq_ignore_ascii_case("idle")),
        });
        ensure!(result.len() <= 128, "too many time-lapse tasks");
    }
    Ok(result)
}

fn calendar_days(raw: &str, days: &mut BTreeSet<NaiveDate>) -> Result<()> {
    let doc = xml::document(raw)?;
    let root =
        xml::find(doc.root_element(), "timelapseDateTbl").context("missing time-lapse calendar")?;
    for item in root.children().filter(|n| n.has_tag_name("item")) {
        let year = field(item, "year")
            .context("missing calendar year")?
            .parse::<i32>()?;
        let table = field(item, "dateTable").context("missing time-lapse date table")?;
        let last =
            NaiveDate::from_ymd_opt(year, 12, 31).context("invalid time-lapse calendar year")?;
        ensure!(
            table.len() == 372 || table.len() == last.ordinal() as usize,
            "unknown time-lapse calendar format"
        );
        for (index, byte) in table.bytes().enumerate() {
            ensure!(
                matches!(byte, b'0' | b'1'),
                "invalid time-lapse calendar day"
            );
            if byte == b'1' {
                let n = index as u32;
                let date = if table.len() == 372 {
                    NaiveDate::from_ymd_opt(year, n / 31 + 1, n % 31 + 1)
                } else {
                    NaiveDate::from_yo_opt(year, n + 1)
                };
                days.insert(date.context("invalid capture day")?);
            }
        }
    }
    Ok(())
}

pub async fn dates(session: &mut Session, channel: u8) -> Result<BTreeSet<NaiveDate>> {
    let mut days = BTreeSet::new();
    for task in tasks(session, channel).await? {
        let raw = session
            .command(
                322,
                Some(channel),
                &folder("timelapseDateTbl", &task.params),
            )
            .await?;
        calendar_days(&raw, &mut days)?;
    }
    Ok(days)
}

pub async fn search(
    session: &mut Session,
    channel: u8,
    scope: &str,
    from: NaiveDateTime,
    to: NaiveDateTime,
) -> Result<Vec<Photo>> {
    search_files(session, channel, scope, from, to, false).await
}

pub async fn search_files(
    session: &mut Session,
    channel: u8,
    scope: &str,
    from: NaiveDateTime,
    to: NaiveDateTime,
    include_movies: bool,
) -> Result<Vec<Photo>> {
    let mut photos = BTreeMap::new();
    for task in tasks(session, channel).await? {
        if !include_movies && task.format != "jpeg" {
            continue;
        }
        let params = &task.params;
        let body = folder(
            "timelapseFileSearch",
            &format!(
                "{params}{}{}",
                time_xml("startTime", from),
                time_xml("endTime", to)
            ),
        );
        let raw = session.command(323, Some(channel), &body).await?;
        let doc = xml::document(&raw)?;
        let root = xml::find(doc.root_element(), "timelapseFileSearch")
            .ok_or_else(|| anyhow!("missing time-lapse search handle"))?;
        let handle: u32 = field(root, "handle")
            .ok_or_else(|| anyhow!("missing time-lapse search handle"))?
            .parse()?;
        let handle_fields = format!("{params}<handle>{handle}</handle>");
        let result = async {
            let expected = field(root, "totalNum")
                .map(str::parse::<usize>)
                .transpose()?;
            let mut raw = raw.clone();
            let initial_count = photos.len();
            let mut offset = 0;
            for index in 0..=MAX_PHOTOS {
                let before = photos.len();
                let (done, returned) = page(&raw, scope, &task, from.date(), &mut photos)?;
                offset += returned;
                ensure!(
                    offset <= MAX_PHOTOS,
                    "time-lapse gallery result limit exceeded"
                );
                if done || expected == Some(offset) {
                    ensure!(
                        expected.is_none_or(|n| n == photos.len() - initial_count),
                        "incomplete time-lapse gallery"
                    );
                    return Ok(());
                }
                ensure!(
                    index == 0 || photos.len() > before,
                    "time-lapse search stopped progressing"
                );
                // The Lumus repeats its first 16 files unless the next request includes the file offset.
                let next = folder(
                    "timelapseFileSearch",
                    &format!("{handle_fields}<fromWhere>{offset}</fromWhere>"),
                );
                raw = session.command(324, Some(channel), &next).await?;
            }
            anyhow::bail!("too many time-lapse search pages")
        }
        .await;
        let close = session
            .command(
                325,
                Some(channel),
                &folder("timelapseFileSearch", &handle_fields),
            )
            .await;
        result?;
        close?;
    }
    Ok(photos.into_values().collect())
}

pub async fn download(session: &mut Session, channel: u8, photo: &Photo) -> Result<Vec<u8>> {
    ensure!(
        photo.size.is_none_or(|n| n <= MAX_BYTES),
        "time-lapse image exceeds size limit"
    );
    let body = folder(
        "timelapseDownload",
        &format!(
            "<position>0</position><id>{}</id><taskType>jpeg</taskType>",
            xml::escape(&photo.file)
        ),
    );
    let extension =
        format!(r#"<Extension version="1.1"><channelId>{channel}</channelId></Extension>"#);
    let mut packet = session
        .exchange(327, channel + 1, &extension, &body, false)
        .await?;
    let sequence = packet.sequence;
    let mut jpeg = Vec::new();
    loop {
        if packet.command == 327 && packet.sequence == sequence {
            ensure!(
                matches!(packet.status, 200 | 201),
                "camera rejected time-lapse image transfer"
            );
            if let Some(bytes) = packet.binary(session) {
                ensure!(
                    bytes.len() <= MAX_BYTES - jpeg.len(),
                    "time-lapse image exceeds size limit"
                );
                jpeg.extend_from_slice(&bytes);
                ensure!(
                    photo.size.is_none_or(|n| jpeg.len() <= n),
                    "time-lapse image exceeded expected size"
                );
                if jpeg.ends_with(&[0xff, 0xd9]) && photo.size.is_none_or(|n| n == jpeg.len()) {
                    break;
                }
            } else if !jpeg.is_empty() && packet.payload_finished(session) {
                anyhow::bail!("incomplete time-lapse image transfer");
            }
        }
        packet = session.read().await?;
    }
    ensure!(jpeg.starts_with(&[0xff, 0xd8]), "invalid time-lapse JPEG");
    Ok(jpeg)
}

fn decode(jpeg: &[u8]) -> Result<DynamicImage> {
    let reader = || ImageReader::with_format(Cursor::new(jpeg), ImageFormat::Jpeg);
    let (w, h) = reader().into_dimensions()?;
    ensure!(
        w > 0 && h > 0 && u64::from(w) * u64::from(h) <= 32_000_000,
        "invalid time-lapse dimensions"
    );
    Ok(reader().decode()?)
}

pub fn thumbnail(jpeg: &[u8]) -> Result<Vec<u8>> {
    let frame = decode(jpeg)?;
    let scaled = if frame.width() > 480 || frame.height() > 270 {
        frame.thumbnail(480, 270)
    } else {
        frame
    };
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 80).encode_image(&scaled)?;
    Ok(out)
}

pub fn png(jpeg: Vec<u8>) -> Result<Vec<u8>> {
    let frame = decode(&jpeg)?;
    let mut out = Cursor::new(Vec::new());
    frame.write_to(&mut out, ImageFormat::Png)?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jpeg_filename_uses_the_capture_date_and_ignores_the_task_directory() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 17).unwrap();
        let expected = day.and_hms_opt(9, 15, 0);
        assert_eq!(filename_time("TL_20261117_091500.jpg", day), expected);
        assert_eq!(filename_time("2026-11-17T09-15-00.jpg", day), expected);
        assert_eq!(filename_time("/20261117_091500/000004.jpg", day), None);
        assert_eq!(filename_time("20261116_091500.jpg", day), None);
    }
}
