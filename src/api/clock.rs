mod zone;

use self::zone::{Schedule, Zone};
use super::*;
use crate::reolink::{
    recordings::wall_time,
    wire::{Rejected, Session},
};
use crate::xml;
use chrono::NaiveDateTime;
use std::time::Instant;

const FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

fn parse_clock(value: &str) -> Option<NaiveDateTime> {
    let time = NaiveDateTime::parse_from_str(value, FORMAT).ok()?;
    (value.len() == 19 && time.format(FORMAT).to_string() == value).then_some(time)
}

async fn query(session: &mut Session, command: u32) -> Result<Option<String>> {
    match session.command(command, None, "").await {
        Err(err) if err.is::<Rejected>() => Ok(None),
        result => result.map(Some),
    }
}

fn enabled(node: roxmltree::Node<'_, '_>) -> Option<bool> {
    match xml::value(node, "enable") {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

#[derive(Default)]
struct Reading {
    time: Option<NaiveDateTime>,
    utc_offset: Option<i32>,
    dst_enabled: Option<bool>,
    schedule: Option<Schedule>,
    ntp: Option<bool>,
}

impl Reading {
    async fn read_general(&mut self, session: &mut Session) -> Result<()> {
        let Some(raw) = query(session, 104).await? else {
            self.time = None;
            self.utc_offset = None;
            return Ok(());
        };
        let doc = xml::document(&raw)?;
        let node = xml::find(doc.root_element(), "SystemGeneral");
        self.time = node.and_then(wall_time);
        // Baichuan expresses the zone in seconds west of UTC.
        self.utc_offset = node
            .and_then(|n| xml::value(n, "timeZone"))
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|s| (-50_400..=43_200).contains(s) && s % 60 == 0)
            .map(|s| -s / 60);
        Ok(())
    }
    async fn read(session: &mut Session) -> Result<Self> {
        let mut result = Self::default();
        result.read_general(session).await?;
        if result.time.is_none() {
            return Ok(result);
        }
        if let Some(raw) = query(session, 106).await? {
            let doc = xml::document(&raw)?;
            if let Some(node) = xml::find(doc.root_element(), "Dst") {
                result.dst_enabled = enabled(node);
                result.schedule = Schedule::read(node);
            }
        }
        if let Some(raw) = query(session, 38).await? {
            let doc = xml::document(&raw)?;
            result.ntp = xml::find(doc.root_element(), "Ntp").and_then(enabled);
        }
        Ok(result)
    }
    fn same_dst(&self, zone: &Zone) -> bool {
        self.dst_enabled == Some(zone.daylight_saving.is_some())
            && (zone.daylight_saving.is_none() || self.schedule == zone.daylight_saving)
    }
    fn matches(&self, change: &Change) -> bool {
        change.time_zone.as_ref().is_none_or(|zone| {
            self.utc_offset == Some(zone.utc_offset_minutes) && self.same_dst(zone)
        }) && change.ntp_enabled.is_none_or(|ntp| self.ntp == Some(ntp))
    }
    fn json(&self) -> Value {
        json!({"cameraTime":self.time.map(|v|v.format(FORMAT).to_string()),"settings":{
            "utcOffsetMinutes":self.utc_offset,"daylightSavingEnabled":self.dst_enabled,"ntpEnabled":self.ntp}})
    }
}

fn set_general(time: NaiveDateTime, zone: Option<&Zone>) -> String {
    let wall=time.format("<year>%Y</year><month>%m</month><day>%d</day><hour>%H</hour><minute>%M</minute><second>%S</second>");
    let offset = zone.map_or_else(String::new, |z| {
        format!("<timeZone>{}</timeZone>", -z.utc_offset_minutes * 60)
    });
    format!(r#"<body><SystemGeneral version="1.1">{offset}{wall}</SystemGeneral></body>"#)
}
fn set_ntp(enabled: bool) -> String {
    format!(
        r#"<body><Ntp version="1.1"><enable>{}</enable></Ntp></body>"#,
        u8::from(enabled)
    )
}
fn confirmed(actual: NaiveDateTime, requested: NaiveDateTime, elapsed: Duration) -> bool {
    (-1..=elapsed.as_secs() as i64 + 1).contains(&(actual - requested).num_seconds())
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
        return error(StatusCode::NOT_FOUND, "API_CLOCK_UNSUPPORTED", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let work = async {
        let mut session = device.connect().await?;
        let result = Reading::read(&mut session).await;
        session.logout().await;
        result
    };
    match timeout(Duration::from_secs(18), work).await {
        Ok(Ok(reading)) => Json(reading.json()).into_response(),
        _ => error(StatusCode::BAD_GATEWAY, "API_CLOCK_READ_FAILED", &cid),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Change {
    camera_time: String,
    time_zone: Option<Zone>,
    ntp_enabled: Option<bool>,
}

async fn apply(
    session: &mut Session,
    change: &Change,
    requested: NaiveDateTime,
) -> Result<(), &'static str> {
    let current = Reading::read(session)
        .await
        .map_err(|_| "API_CLOCK_READ_FAILED")?;
    let current_time = current.time.ok_or("API_CLOCK_UNSUPPORTED")?;
    if change.time_zone.is_some() && (current.utc_offset.is_none() || current.dst_enabled.is_none())
        || change.ntp_enabled.is_some() && current.ntp.is_none()
    {
        return Err("API_CLOCK_UNSUPPORTED");
    }
    let change_time = !confirmed(current_time, requested, Duration::ZERO);
    if change_time && change.time_zone.is_none() && change.ntp_enabled.or(current.ntp) == Some(true)
    {
        return Err("API_CLOCK_NTP_ACTIVE");
    }
    let change_ntp = change.ntp_enabled.is_some() && change.ntp_enabled != current.ntp;
    let mut updates = Vec::new();
    if change_ntp && change.ntp_enabled == Some(false) {
        updates.push((39, set_ntp(false)));
    }
    if let Some(zone) = change
        .time_zone
        .as_ref()
        .filter(|zone| !current.same_dst(zone))
    {
        updates.push((107, zone.dst_xml()));
    }
    if change_time
        || change.time_zone.as_ref().is_some_and(|zone| {
            current.utc_offset != Some(zone.utc_offset_minutes) || !current.same_dst(zone)
        })
    {
        updates.push((105, set_general(requested, change.time_zone.as_ref())));
    }
    if change_ntp && change.ntp_enabled == Some(true) {
        updates.push((39, set_ntp(true)));
    }
    for (index, (command, xml)) in updates.into_iter().enumerate() {
        session.command(command, None, &xml).await.map_err(|err| {
            if index == 0 && err.is::<Rejected>() {
                "API_CLOCK_REJECTED"
            } else {
                "API_CLOCK_UNVERIFIED"
            }
        })?;
    }
    Ok(())
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
        return error(StatusCode::UNPROCESSABLE_ENTITY, "API_CLOCK_INVALID", &cid);
    };
    let Some(requested) = parse_clock(&change.camera_time)
        .filter(|_| change.time_zone.as_ref().is_none_or(Zone::valid))
    else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "API_CLOCK_INVALID", &cid);
    };
    let Some(device) = api.devices.get(&id) else {
        return error(StatusCode::NOT_FOUND, "API_CLOCK_UNSUPPORTED", &cid);
    };
    let Ok(_guard) = api.network_writes[&id].try_lock() else {
        return error(StatusCode::CONFLICT, "API_DISPLAY_BUSY", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let work = async {
        let start = Instant::now();
        let mut writer = device
            .connect()
            .await
            .map_err(|_| "API_CLOCK_READ_FAILED")?;
        let result = apply(&mut writer, &change, requested).await;
        writer.logout().await;
        result?;
        let mut reader = device.connect().await.map_err(|_| "API_CLOCK_UNVERIFIED")?;
        let verification = async {
            let mut fresh = Reading::read(&mut reader).await.ok()?;
            let first = fresh.time?;
            if !fresh.matches(&change) || !confirmed(first, requested, start.elapsed()) {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(1100)).await;
            fresh.read_general(&mut reader).await.ok()?;
            let next = fresh.time?;
            (next > first && fresh.matches(&change) && confirmed(next, requested, start.elapsed()))
                .then_some(fresh)
        }
        .await;
        reader.logout().await;
        verification.ok_or("API_CLOCK_UNVERIFIED")
    };
    match timeout(Duration::from_secs(24), work)
        .await
        .unwrap_or(Err("API_CLOCK_UNVERIFIED"))
    {
        Ok(fresh) => {
            let mut response = fresh.json();
            response["outcome"] = json!("confirmed");
            response["correlation_id"] = json!(cid.0);
            Json(response).into_response()
        }
        Err(key) => error(
            if matches!(
                key,
                "API_CLOCK_UNSUPPORTED" | "API_CLOCK_REJECTED" | "API_CLOCK_NTP_ACTIVE"
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
    fn a_zone_change_requires_an_explicit_daylight_saving_choice() {
        assert!(serde_json::from_value::<Zone>(json!({"utcOffsetMinutes":-420})).is_err());
        assert!(
            serde_json::from_value::<Zone>(json!({"utcOffsetMinutes":-420,"daylightSaving":null}))
                .unwrap()
                .valid()
        );
    }
    #[test]
    fn time_and_zone_use_local_fields_and_seconds_west() {
        let requested = parse_clock("2028-02-29T23:59:59").unwrap();
        assert!(parse_clock("2027-02-29T01:01:01").is_none());
        let zone = Zone {
            utc_offset_minutes: -420,
            daylight_saving: None,
        };
        let raw = set_general(requested, Some(&zone));
        let doc = xml::document(&raw).unwrap();
        let node = xml::find(doc.root_element(), "SystemGeneral").unwrap();
        assert_eq!(wall_time(node), Some(requested));
        assert_eq!(xml::value(node, "timeZone"), Some("25200"));
        assert!(!set_general(requested, None).contains("timeZone"));
        assert!(confirmed(
            parse_clock("2028-03-01T00:00:00").unwrap(),
            requested,
            Duration::from_millis(500)
        ));
    }
}
