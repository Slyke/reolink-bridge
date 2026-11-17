use std::collections::BTreeMap;

use anyhow::{Result, anyhow, ensure};
use chrono::{NaiveDate, NaiveDateTime};
use roxmltree::Node;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::wire::Session;
use crate::xml;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub number: u32,
    pub kind: &'static str,
    pub mounted: bool,
    pub formatted: bool,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub id: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub stream: String,
    pub size_bytes: Option<u64>,
    pub kind: Option<String>,
}

pub async fn storage(session: &mut Session) -> Result<Vec<Card>> {
    let raw = session.command(102, None, "").await?;
    let doc = xml::document(&raw)?;
    let root = doc.root_element();
    ensure!(
        xml::find(root, "HddInfoList").is_some() || xml::find(root, "HddInfo").is_some(),
        "storage information unsupported"
    );
    root.descendants()
        .filter(|n| n.has_tag_name("HddInfo"))
        .map(|node| {
            let slot = number(node, "number").unwrap_or(0) as u32;
            let size = |g, m| {
                let gb = number(node, g);
                let mb = number(node, m);
                (gb.is_some() || mb.is_some())
                    .then(|| gb.unwrap_or(0) * 1_000_000_000 + mb.unwrap_or(0) * 1_000_000)
            };
            let total_bytes = size("capacityG", "capacityM");
            let free_bytes = size("remainSizeG", "remainSizeM");
            ensure!(
                free_bytes
                    .zip(total_bytes)
                    .is_none_or(|(free, total)| free <= total),
                "invalid storage sizes"
            );
            Ok(Card {
                number: slot,
                kind: if number(node, "storageType") == Some(1) {
                    "hdd"
                } else {
                    "sd"
                },
                mounted: number(node, "mount") == Some(1),
                formatted: number(node, "format") == Some(1),
                total_bytes,
                used_bytes: total_bytes
                    .zip(free_bytes)
                    .map(|(total, free)| total - free),
                free_bytes,
            })
        })
        .collect()
}

fn number(node: Node<'_, '_>, name: &str) -> Option<u64> {
    xml::value(node, name)?
        .parse()
        .ok()
        .filter(|n| *n <= 100_000_000)
}

pub fn wall_time(node: Node<'_, '_>) -> Option<NaiveDateTime> {
    let value = |tag| xml::value(node, tag)?.parse::<u32>().ok();
    NaiveDate::from_ymd_opt(value("year")? as i32, value("month")?, value("day")?)?.and_hms_opt(
        value("hour")?,
        value("minute")?,
        value("second")?,
    )
}

pub async fn clock(session: &mut Session) -> Option<NaiveDateTime> {
    let raw = session.command(104, None, "").await.ok()?;
    let doc = xml::document(&raw).ok()?;
    wall_time(doc.root_element())
}

pub fn time_xml(tag: &str, value: NaiveDateTime) -> String {
    format!("<{tag}>{}</{tag}>",value.format("<year>%Y</year><month>%m</month><day>%d</day><hour>%H</hour><minute>%M</minute><second>%S</second>"))
}

pub async fn uid(session: &mut Session) -> Result<String> {
    let raw = session.command(114, None, "").await?;
    let doc = xml::document(&raw)?;
    let uid = xml::value(doc.root_element(), "uid")
        .ok_or_else(|| anyhow!("device identifier unavailable"))?;
    ensure!(
        uid.len() <= 128
            && uid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid device identifier"
    );
    Ok(uid.into())
}

pub async fn search(
    session: &mut Session,
    channel: u8,
    day: NaiveDate,
    stream: &str,
) -> Result<(Vec<Clip>, bool)> {
    ensure!(
        matches!(stream, "main" | "sub"),
        "unsupported recording stream"
    );
    let uid = uid(session).await?;
    let start = day.and_hms_opt(0, 0, 0).unwrap();
    let end = day.and_hms_opt(23, 59, 59).unwrap();
    let body = format!(
        r#"<body><FileInfoList version="1.1"><FileInfo><uid>{}</uid><channelId>{channel}</channelId><logicChnBitmap>255</logicChnBitmap><streamType>{stream}Stream</streamType><recordType>manual, sched, io, md, people, face, vehicle, dog_cat, visitor, other, package</recordType>{}{}</FileInfo></FileInfoList></body>"#,
        xml::escape(&uid),
        time_xml("startTime", start),
        time_xml("endTime", end)
    );
    // The file-search header uses the host channel. The XML carries the camera channel.
    let raw = session.command(14, None, &body).await?;
    let doc = xml::document(&raw)?;
    let handle: u64 = xml::value(doc.root_element(), "handle")
        .ok_or_else(|| anyhow!("recording search unsupported"))?
        .parse()?;
    let page = format!(
        r#"<body><FileInfoList version="1.1"><FileInfo><channelId>{channel}</channelId><uid>{}</uid><handle>{handle}</handle></FileInfo></FileInfoList></body>"#,
        xml::escape(&uid)
    );
    let result = async {
        let mut clips = BTreeMap::new();
        for _ in 0..128 {
            let raw = session.command(15, None, &page).await?;
            let doc = xml::document(&raw)?;
            let mut count = 0;
            for node in doc
                .descendants()
                .filter(|n| n.has_tag_name("FileInfo") || n.has_tag_name("File"))
            {
                let Some(clip) = parse_clip(node, stream, day) else {
                    continue;
                };
                clips.insert(clip.id.clone(), clip);
                count += 1;
                if clips.len() >= 4096 {
                    return Ok((clips.into_values().collect(), true));
                }
            }
            let finished = xml::value(doc.root_element(), "bFinished")
                .or_else(|| xml::value(doc.root_element(), "finished"));
            if finished == Some("1") || count == 0 || (finished.is_none() && count < 40) {
                return Ok((clips.into_values().collect(), false));
            }
        }
        Ok((clips.into_values().collect(), true))
    }
    .await;
    let _ = session.command(16, None, &page).await;
    result
}

fn parse_clip(node: Node<'_, '_>, stream: &str, day: NaiveDate) -> Option<Clip> {
    let get = |key| xml::child(node, key).and_then(xml::text);
    let name = get("name")
        .or_else(|| get("fileName"))
        .or_else(|| get("Id"))?;
    if name.len() > 1024 {
        return None;
    }
    let mut start = xml::child(node, "startTime").and_then(wall_time);
    let mut end = xml::child(node, "endTime").and_then(wall_time);
    // Some firmwares encode both times only in RecM/RecS filenames.
    if start.is_none() || end.is_none() {
        let parts: Vec<_> = name.rsplit('/').next()?.split('_').collect();
        if let Some((index, date)) = parts
            .iter()
            .enumerate()
            .find(|(_, s)| s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()))
        {
            let stamp = |offset| {
                NaiveDateTime::parse_from_str(
                    &format!("{date}{}", parts.get(index + offset)?),
                    "%Y%m%d%H%M%S",
                )
                .ok()
            };
            start = start.or_else(|| stamp(1));
            end = end.or_else(|| stamp(2));
        }
    }
    let (start, end) = (start?, end?);
    if end <= start || start.date() != day || (end - start).num_hours() > 24 {
        return None;
    }
    let id = format!(
        "{:x}",
        Sha256::digest(format!("{stream}\0{name}\0{start}\0{end}"))
    );
    Some(Clip {
        id,
        start,
        end,
        stream: stream.into(),
        size_bytes: get("size")
            .or_else(|| get("fileSize"))
            .and_then(|s| s.parse().ok()),
        kind: get("type")
            .or_else(|| get("recordType"))
            .or_else(|| get("alarmType"))
            .filter(|s| s.len() <= 120)
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filename_times_and_clock_are_validated() {
        let raw = "<FileInfo><name>/record/RecM01_20261117_120000_120050_ABC.mp4</name><size>500</size><type>md</type></FileInfo>";
        let doc = xml::document(raw).unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 11, 17).unwrap();
        let clip = parse_clip(doc.root_element(), "main", day).unwrap();
        assert_eq!((clip.end - clip.start).num_seconds(), 50);
        assert_eq!(clip.size_bytes, Some(500));
        assert_eq!(clip.id.len(), 64);
    }
}
