mod fields;
mod ftp;

use super::*;
use crate::{
    reolink::wire::{Rejected, Session},
    xml,
};
use fields::Group;
use serde_json::Map;

#[derive(Debug)]
enum Problem {
    Invalid,
    Unsupported,
    Read,
    Rejected,
    Unverified,
}
type Result<T> = std::result::Result<T, Problem>;
impl Problem {
    fn response(self, cid: &CorrelationId) -> Response {
        let (status, key) = match self {
            Self::Invalid => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "API_DEVICE_OPTIONS_INVALID",
            ),
            Self::Unsupported => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "API_DEVICE_OPTIONS_UNSUPPORTED",
            ),
            Self::Read => (StatusCode::BAD_GATEWAY, "API_DEVICE_OPTIONS_READ_FAILED"),
            Self::Rejected => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "API_DEVICE_OPTIONS_REJECTED",
            ),
            Self::Unverified => (StatusCode::BAD_GATEWAY, "API_DEVICE_OPTIONS_UNVERIFIED"),
        };
        error(status, key, cid)
    }
}
async fn read(session: &mut Session, group: &Group, channel: u8) -> Result<String> {
    session
        .command(group.get, group.wire_channel(channel), "")
        .await
        .map_err(|e| {
            if e.downcast_ref::<Rejected>()
                .is_some_and(|r| matches!(r.status(), 400 | 404 | 405 | 422 | 501))
            {
                Problem::Unsupported
            } else {
                Problem::Read
            }
        })
}
fn report(group: &Group, raw: &str, channel: u8) -> Result<Value> {
    let doc = xml::document(raw).map_err(|_| Problem::Read)?;
    group.describe(doc.root_element(), channel)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    fields: Map<String, Value>,
}

pub(super) async fn get(
    State(api): State<Arc<Api>>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let (Some(device), Some(group)) = (api.devices.get(&id), Group::named(&name)) else {
        return Problem::Unsupported.response(&cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let work = async {
        let mut session = device.connect().await.map_err(|_| Problem::Read)?;
        let raw = read(&mut session, &group, device.channel).await;
        session.logout().await;
        report(&group, &raw?, device.channel)
    };
    match timeout(Duration::from_secs(18), work)
        .await
        .unwrap_or(Err(Problem::Read))
    {
        Ok(data) => Json(data).into_response(),
        Err(problem) => problem.response(&cid),
    }
}
pub(super) async fn set(
    State(api): State<Arc<Api>>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    body: std::result::Result<Json<Change>, JsonRejection>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Ok(Json(change)) = body else {
        return Problem::Invalid.response(&cid);
    };
    let (Some(device), Some(group)) = (api.devices.get(&id), Group::named(&name)) else {
        return Problem::Unsupported.response(&cid);
    };
    // The FTP destination belongs to the device, including when multiple routes address its channels.
    let lock_id = if group.device_wide() {
        api.devices
            .iter()
            .find(|(_, d)| d.host == device.host && d.port == device.port)
            .map_or(&id, |(key, _)| key)
    } else {
        &id
    };
    let Ok(_guard) = api.network_writes[lock_id].try_lock() else {
        return error(StatusCode::CONFLICT, "API_DISPLAY_BUSY", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let channel = device.channel;
    let work = async {
        let mut session = device.connect().await.map_err(|_| Problem::Read)?;
        let result = async {
            let raw = read(&mut session, &group, channel).await?;
            let doc = xml::document(&raw).map_err(|_| Problem::Read)?;
            let Some(body) = group.edit(doc.root_element(), channel, &change.fields)? else {
                return report(&group, &raw, channel);
            };
            session
                .command(group.set, group.wire_channel(channel), &body)
                .await
                .map_err(|e| {
                    if e.is::<Rejected>() {
                        Problem::Rejected
                    } else {
                        Problem::Unverified
                    }
                })?;
            let raw = read(&mut session, &group, channel)
                .await
                .map_err(|_| Problem::Unverified)?;
            let mut result = report(&group, &raw, channel).map_err(|_| Problem::Unverified)?;
            if group.device_wide() {
                if !ftp::verify(&raw, channel, &result, &change.fields)? {
                    result["outcome"] = json!("unverified");
                }
            } else if !change.fields.iter().all(|(key, value)| {
                result["fields"]
                    .get(key)
                    .and_then(|field| field.get("value"))
                    == Some(value)
            }) {
                return Err(Problem::Unverified);
            }
            Ok(result)
        }
        .await;
        session.logout().await;
        result
    };
    match timeout(Duration::from_secs(26), work)
        .await
        .unwrap_or(Err(Problem::Unverified))
    {
        Ok(mut data) => {
            if data.get("outcome").is_none() {
                data["outcome"] = json!("confirmed");
            }
            data["correlation_id"] = json!(cid.0);
            Json(data).into_response()
        }
        Err(problem) => problem.response(&cid),
    }
}
