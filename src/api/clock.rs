use super::*;
use crate::reolink::{
    recordings::wall_time,
    wire::{Rejected, Session},
};
use chrono::NaiveDateTime;
use std::time::Instant;

const FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

fn parse_clock(value: &str) -> Option<NaiveDateTime> {
    let time = NaiveDateTime::parse_from_str(value, FORMAT).ok()?;
    (value.len() == 19 && time.format(FORMAT).to_string() == value).then_some(time)
}

async fn read(session: &mut Session) -> Result<Option<NaiveDateTime>> {
    let raw = match session.command(104, None, "").await {
        Err(err) if err.is::<Rejected>() => return Ok(None),
        result => result?,
    };
    let doc = crate::xml::document(&raw)?;
    Ok(crate::xml::find(doc.root_element(), "SystemGeneral").and_then(wall_time))
}

fn setting(time: NaiveDateTime) -> String {
    time.format(r#"<body><SystemGeneral version="1.1"><year>%Y</year><month>%m</month><day>%d</day><hour>%H</hour><minute>%M</minute><second>%S</second></SystemGeneral></body>"#).to_string()
}

fn confirmed(actual: NaiveDateTime, requested: NaiveDateTime, elapsed: Duration) -> bool {
    (0..=elapsed.as_secs() as i64 + 1).contains(&(actual - requested).num_seconds())
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
        let result = read(&mut session).await;
        session.logout().await;
        result
    };
    match timeout(Duration::from_secs(18), work).await {
        Ok(Ok(time)) => {
            Json(json!({"cameraTime":time.map(|v|v.format(FORMAT).to_string())})).into_response()
        }
        _ => error(StatusCode::BAD_GATEWAY, "API_CLOCK_READ_FAILED", &cid),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Change {
    camera_time: String,
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
    let Some(requested) = body
        .ok()
        .and_then(|Json(value)| parse_clock(&value.camera_time))
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
        let mut session = device
            .connect()
            .await
            .map_err(|_| "API_CLOCK_READ_FAILED")?;
        let result = async {
            let current = read(&mut session)
                .await
                .map_err(|_| "API_CLOCK_READ_FAILED")?
                .ok_or("API_CLOCK_UNSUPPORTED")?;
            if current == requested {
                return Ok(current);
            }
            let start = Instant::now();
            session
                .command(105, None, &setting(requested))
                .await
                .map_err(|e| {
                    if e.is::<Rejected>() {
                        "API_CLOCK_REJECTED"
                    } else {
                        "API_CLOCK_UNVERIFIED"
                    }
                })?;
            read(&mut session)
                .await
                .ok()
                .flatten()
                .filter(|v| confirmed(*v, requested, start.elapsed()))
                .ok_or("API_CLOCK_UNVERIFIED")
        }
        .await;
        session.logout().await;
        result
    };
    match timeout(Duration::from_secs(24),work).await.unwrap_or(Err("API_CLOCK_UNVERIFIED")) {
        Ok(time) => Json(json!({"outcome":"confirmed","cameraTime":time.format(FORMAT).to_string(),"correlation_id":cid.0})).into_response(),
        Err(key) => error(if matches!(key,"API_CLOCK_UNSUPPORTED"|"API_CLOCK_REJECTED") {StatusCode::UNPROCESSABLE_ENTITY} else {StatusCode::BAD_GATEWAY},key,&cid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_change_only_sets_the_wall_time_and_verifies_rollover() {
        let requested = parse_clock("2028-02-29T23:59:59").unwrap();
        assert!(parse_clock("2027-02-29T01:01:01").is_none());
        assert!(parse_clock("2028-02-29T09:05:00Z").is_none());
        let xml = setting(requested);
        let doc = crate::xml::document(&xml).unwrap();
        let node = crate::xml::find(doc.root_element(), "SystemGeneral").unwrap();
        assert_eq!(wall_time(node), Some(requested));
        assert_eq!(node.children().filter(|n| n.is_element()).count(), 6);
        assert!(confirmed(
            parse_clock("2028-03-01T00:00:00").unwrap(),
            requested,
            Duration::from_millis(500)
        ));
        assert!(!confirmed(
            requested - chrono::Duration::hours(1),
            requested,
            Duration::from_secs(2)
        ));
    }
}
