mod audio;
use super::*;
use crate::reolink::wire::{Device, Rejected, Session};
use audio::Format;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::time::Instant;

const IDLE: Duration = Duration::from_secs(4);
type Slot = Arc<Mutex<Option<Speaker>>>;
pub(super) struct Speakers(BTreeMap<String, Slot>);
struct Speaker {
    id: Uuid,
    wire: Session,
    channel: u8,
    format: Format,
    next: u32,
    seq: u32,
    index: usize,
    expires: Instant,
    queued_until: Instant,
}
impl Speakers {
    pub(super) fn new(devices: &BTreeMap<String, Device>) -> Self {
        let mut physical = BTreeMap::new();
        Self(
            devices
                .iter()
                .map(|(id, d)| {
                    let key = (d.host.clone(), d.port, d.channel);
                    (
                        id.clone(),
                        physical
                            .entry(key)
                            .or_insert_with(|| Arc::new(Mutex::new(None)))
                            .clone(),
                    )
                })
                .collect(),
        )
    }
}
fn fail(status: StatusCode, key: &'static str, cid: &CorrelationId) -> Response {
    error(status, key, cid)
}
async fn finish(slot: &mut Option<Speaker>) {
    if let Some(mut live) = slot.take() {
        let _ = timeout(
            Duration::from_secs(2),
            live.wire.command(11, Some(live.channel), ""),
        )
        .await;
        live.wire.logout().await;
    }
}
pub(super) async fn capability(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    h: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&h) {
        return fail(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Some(d) = api.devices.get(&id) else {
        return fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_TALK_UNSUPPORTED",
            &cid,
        );
    };
    let Ok(_permit) = api.settings_slots.try_acquire() else {
        return fail(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let result = async {
        let mut s = d.connect().await?;
        let v = Format::read(&mut s, d.channel).await;
        s.logout().await;
        v
    }
    .await;
    match result {
        Ok(Some(f)) => Json(f.view()).into_response(),
        Ok(None) => Json(json!({"available":false})).into_response(),
        Err(_) => fail(StatusCode::BAD_GATEWAY, "API_TALK_FAILED", &cid),
    }
}
pub(super) async fn start(
    State(api): State<Arc<Api>>,
    Path((id, token)): Path<(String, Uuid)>,
    h: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&h) {
        return fail(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let (Some(d), Some(slot)) = (api.devices.get(&id), api.speakers.0.get(&id)) else {
        return fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_TALK_UNSUPPORTED",
            &cid,
        );
    };
    let mut active = slot.lock().await;
    if let Some(live) = active.as_ref() {
        return if live.id == token {
            Json(live.format.view()).into_response()
        } else {
            fail(StatusCode::CONFLICT, "API_TALK_BUSY", &cid)
        };
    }
    let Ok(mut wire) = d.connect().await else {
        return fail(StatusCode::BAD_GATEWAY, "API_TALK_FAILED", &cid);
    };
    let format = match Format::read(&mut wire, d.channel).await {
        Ok(Some(f)) => f,
        Ok(None) => {
            wire.logout().await;
            return fail(
                StatusCode::UNPROCESSABLE_ENTITY,
                "API_TALK_UNSUPPORTED",
                &cid,
            );
        }
        Err(_) => {
            wire.logout().await;
            return fail(StatusCode::BAD_GATEWAY, "API_TALK_FAILED", &cid);
        }
    };
    if let Err(e) = format.start(&mut wire, d.channel).await {
        wire.logout().await;
        return if e
            .downcast_ref::<Rejected>()
            .is_some_and(|r| r.status() == 422)
        {
            fail(StatusCode::CONFLICT, "API_TALK_BUSY", &cid)
        } else {
            fail(StatusCode::BAD_GATEWAY, "API_TALK_FAILED", &cid)
        };
    }
    let view = format.view();
    let now = Instant::now();
    let seq = wire.talk_sequence(d.channel);
    *active = Some(Speaker {
        id: token,
        wire,
        channel: d.channel,
        format,
        next: 0,
        seq,
        index: 0,
        expires: now + IDLE,
        queued_until: now,
    });
    let slot = slot.clone();
    let stop = api.stop.clone();
    tokio::spawn(async move {
        let mut expires = now + IDLE;
        loop {
            tokio::select! {_=tokio::time::sleep_until(expires)=>{},_=stop.cancelled()=>{}}
            let mut guard = slot.lock().await;
            let Some(current) = guard.as_ref().filter(|c| c.id == token) else {
                return;
            };
            if stop.is_cancelled() || current.expires <= Instant::now() {
                finish(&mut guard).await;
                return;
            }
            expires = current.expires;
        }
    });
    Json(view).into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Chunk {
    sequence: u32,
    pcm: String,
}
pub(super) async fn send(
    State(api): State<Arc<Api>>,
    Path((id, token)): Path<(String, Uuid)>,
    h: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    body: std::result::Result<Json<Chunk>, JsonRejection>,
) -> Response {
    if !api.authenticated(&h) {
        return fail(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Some(slot) = api.speakers.0.get(&id) else {
        return fail(StatusCode::NOT_FOUND, "API_TALK_EXPIRED", &cid);
    };
    let Ok(Json(chunk)) = body else {
        return fail(StatusCode::BAD_REQUEST, "API_TALK_INVALID", &cid);
    };
    let Ok(pcm) = STANDARD.decode(chunk.pcm) else {
        return fail(StatusCode::BAD_REQUEST, "API_TALK_INVALID", &cid);
    };
    let mut guard = slot.lock().await;
    let Some(s) = guard
        .as_mut()
        .filter(|s| s.id == token && s.expires > Instant::now())
    else {
        return fail(StatusCode::NOT_FOUND, "API_TALK_EXPIRED", &cid);
    };
    if chunk.sequence < s.next {
        return StatusCode::NO_CONTENT.into_response();
    }
    let block = s.format.samples * 2;
    if chunk.sequence != s.next
        || pcm.is_empty()
        || !pcm.len().is_multiple_of(block)
        || pcm.len() > block * 4
    {
        return fail(StatusCode::BAD_REQUEST, "API_TALK_INVALID", &cid);
    }
    let now = Instant::now();
    if s.queued_until > now + Duration::from_millis(350) {
        return fail(StatusCode::TOO_MANY_REQUESTS, "API_TALK_BUSY", &cid);
    }
    let encoded = audio::encode(&pcm, s.format.samples, &mut s.index);
    if s.wire.talk_audio(s.seq, s.channel, &encoded).await.is_err() {
        finish(&mut guard).await;
        return fail(StatusCode::BAD_GATEWAY, "API_TALK_FAILED", &cid);
    }
    s.next += 1;
    s.expires = now + IDLE;
    s.queued_until = s.queued_until.max(now)
        + Duration::from_secs_f64(pcm.len() as f64 / (2.0 * f64::from(s.format.rate)));
    StatusCode::NO_CONTENT.into_response()
}
pub(super) async fn stop(
    State(api): State<Arc<Api>>,
    Path((id, token)): Path<(String, Uuid)>,
    h: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&h) {
        return fail(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    if let Some(slot) = api.speakers.0.get(&id) {
        let mut guard = slot.lock().await;
        if guard.as_ref().is_some_and(|s| s.id == token) {
            finish(&mut guard).await;
        }
    }
    StatusCode::NO_CONTENT.into_response()
}
