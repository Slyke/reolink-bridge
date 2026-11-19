use super::*;
use crate::reolink::wire::{Rejected, Session};
use crate::{model, xml};
use serde::Serialize;

#[derive(Serialize, Debug, PartialEq)]
pub(super) struct RtspSettings {
    pub enabled: bool,
    port: Option<u16>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Problem {
    Unsupported,
    Read,
    Rejected,
    Unverified,
}

impl Problem {
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Unsupported => "API_RTSP_UNSUPPORTED",
            Self::Read => "API_NETWORK_READ_FAILED",
            Self::Rejected => "API_RTSP_REJECTED",
            Self::Unverified => "API_RTSP_UNVERIFIED",
        }
    }
    fn response(self, cid: &CorrelationId) -> Response {
        let status = match self {
            Self::Unsupported | Self::Rejected => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Read | Self::Unverified => StatusCode::BAD_GATEWAY,
        };
        error(status, self.key(), cid)
    }
}

fn parse(raw: &str) -> Result<RtspSettings, Problem> {
    let doc = xml::document(raw).map_err(|_| Problem::Read)?;
    let root = xml::find(doc.root_element(), "RtspPort").ok_or(Problem::Unsupported)?;
    let field = |a, b| {
        xml::child(root, a)
            .or_else(|| xml::child(root, b))
            .and_then(|node| node.text())
    };
    let enabled = field("enable", "rtspEnable")
        .ok_or(Problem::Unsupported)
        .and_then(|v| model::boolean(v).ok_or(Problem::Read))?;
    let port = field("rtspPort", "port")
        .map(str::parse::<u16>)
        .transpose()
        .map_err(|_| Problem::Read)?;
    Ok(RtspSettings {
        enabled,
        port: port.filter(|v| *v > 0),
    })
}

async fn read(session: &mut Session) -> Result<RtspSettings, Problem> {
    let raw = session.command(37, None, "").await.map_err(|e| {
        if e.is::<Rejected>() {
            Problem::Unsupported
        } else {
            Problem::Read
        }
    })?;
    parse(&raw)
}

pub(super) async fn apply(session: &mut Session, enabled: bool) -> Result<RtspSettings, Problem> {
    let current = read(session).await?;
    if current.enabled == enabled {
        return Ok(current);
    }
    // Set only the RTSP switch. Other services and the camera's configured port stay intact.
    let body = format!(
        r#"<body><RtspPort version="1.1"><enable>{}</enable></RtspPort></body>"#,
        u8::from(enabled)
    );
    session.command(36, None, &body).await.map_err(|e| {
        if e.is::<Rejected>() {
            Problem::Rejected
        } else {
            Problem::Unverified
        }
    })?;
    let state = read(session).await.map_err(|_| Problem::Unverified)?;
    if state.enabled != enabled {
        return Err(Problem::Unverified);
    }
    Ok(state)
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
        return Problem::Unsupported.response(&cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let result = async {
        let mut session = device.connect().await.map_err(|_| Problem::Read)?;
        let state = read(&mut session).await;
        session.logout().await;
        state
    };
    match timeout(Duration::from_secs(18), result)
        .await
        .unwrap_or(Err(Problem::Read))
    {
        Ok(state) => Json(json!({"supported":true,"enabled":state.enabled,"port":state.port}))
            .into_response(),
        Err(Problem::Unsupported) => Json(json!({"supported":false})).into_response(),
        Err(problem) => problem.response(&cid),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    enabled: bool,
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
        return error(StatusCode::UNPROCESSABLE_ENTITY, "API_RTSP_INVALID", &cid);
    };
    let Some(device) = api.devices.get(&id) else {
        return Problem::Unsupported.response(&cid);
    };
    let Ok(_guard) = api.network_writes[&id].try_lock() else {
        return error(StatusCode::CONFLICT, "API_NETWORK_BUSY", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let Ok(mut session) = device.connect().await else {
        return Problem::Read.response(&cid);
    };
    let result = timeout(Duration::from_secs(18), apply(&mut session, change.enabled))
        .await
        .unwrap_or(Err(Problem::Unverified));
    session.logout().await;
    match result {
        Ok(state) => Json(json!({"outcome":"confirmed","enabled":state.enabled,"port":state.port,"correlation_id":cid.0})).into_response(),
        Err(problem) => problem.response(&cid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rtsp_is_independent_of_other_ports_and_missing_switches() {
        let raw = "<body><HttpPort><enable>1</enable></HttpPort><RtspPort><rtspPort>8554</rtspPort><enable>0</enable></RtspPort></body>";
        assert_eq!(
            parse(raw).unwrap(),
            RtspSettings {
                enabled: false,
                port: Some(8554)
            }
        );
        assert!(matches!(
            parse("<body><RtspPort><port>554</port></RtspPort></body>"),
            Err(Problem::Unsupported)
        ));
        assert!(matches!(
            parse("<body><RtspPort><enable>maybe</enable></RtspPort></body>"),
            Err(Problem::Read)
        ));
    }
}
