use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Path, Request as HttpRequest, State, WebSocketUpgrade,
        rejection::BytesRejection,
        ws::rejection::WebSocketUpgradeRejection,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::{sync::broadcast, time::timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    camera::ControlHandle,
    config::{Callback, Filter, valid_id},
    diagnostics::{BUILD_HASH, Diagnostics, SERVICE, VERSION},
    hub::{CallbackEdit, Hub, Update},
    onvif::events::{PushRoutes, parse_notify},
    reolink::{
        control::Control,
        events::{AlarmDecoder, parse_native},
    },
    xml,
};

pub struct App {
    pub hub: Arc<Hub>,
    pub push: PushRoutes,
    pub native: BTreeMap<String, NativeRoute>,
    pub controls: BTreeMap<String, ControlHandle>,
    pub camera_ids: BTreeSet<String>,
    pub ws_token: String,
    pub stop: CancellationToken,
}

pub struct NativeRoute {
    pub token: String,
    pub parser: NativeParser,
}

pub enum NativeParser {
    HaCfg(Mutex<AlarmDecoder>),
    Cgi(u8),
}

pub fn router(app: Arc<App>, ws_path: &str) -> Router {
    Router::new()
        .route("/livez", get(liveness))
        .route("/readyz", get(readiness))
        .route("/ingest/onvif/{id}/{key}", post(notify))
        .route("/ingest/reolink/{id}/{key}", post(native))
        .route(ws_path, get(upgrade))
        .layer(DefaultBodyLimit::max(xml::BODY_LIMIT))
        .layer(middleware::from_fn(correlate))
        .with_state(app)
}

const CORRELATION_HEADER: &str = "x-correlation-id";

#[derive(Clone)]
struct CorrelationId(String);

async fn correlate(mut request: HttpRequest, next: Next) -> Response {
    let id = request
        .headers()
        .get(CORRELATION_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() == 36)
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(Uuid::new_v4);
    let id = id.to_string();
    request.extensions_mut().insert(CorrelationId(id.clone()));
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        CORRELATION_HEADER,
        HeaderValue::from_str(&id).expect("valid UUID header"),
    );
    response
}

async fn liveness(axum::Extension(id): axum::Extension<CorrelationId>) -> Response {
    Json(probe("liveness", true, id)).into_response()
}

async fn readiness(
    State(app): State<Arc<App>>,
    axum::Extension(id): axum::Extension<CorrelationId>,
) -> Response {
    let (cameras, mqtt) = app.hub.readiness();
    let ok = cameras && mqtt;
    let mut payload = probe("readiness", ok, id);
    payload["checks"] = json!({"cameras":cameras,"mqtt":mqtt});
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(payload)).into_response()
}

fn probe(name: &str, ok: bool, id: CorrelationId) -> Value {
    json!({
        "ok": ok, "probe":name, "service":SERVICE,
        "version":VERSION, "buildHash":BUILD_HASH, "correlation_id": id.0,
    })
}

struct RequestErrors<'a> {
    diagnostics: &'a Diagnostics,
    caller: &'static str,
    correlation: String,
}

impl RequestErrors<'_> {
    fn reply(&self, key: &str, cause: Option<anyhow::Error>) -> Value {
        self.diagnostics.error(
            self.caller,
            key,
            Some(self.correlation.clone()),
            Value::Null,
            cause,
        )
    }

    fn http(&self, status: StatusCode, key: &str) -> Response {
        (status, Json(self.reply(key, None))).into_response()
    }
}

async fn notify(
    State(app): State<Arc<App>>,
    Path((id, key)): Path<(String, String)>,
    axum::Extension(correlation): axum::Extension<CorrelationId>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let errors = RequestErrors {
        diagnostics: &app.hub.diagnostics,
        caller: "http::notify",
        correlation: correlation.0,
    };
    let Some(destination) = app.push.receiver(&id, &key) else {
        return errors.http(StatusCode::NOT_FOUND, "ONVIF_NOTIFY_ROUTE_UNKNOWN");
    };
    let soap = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|kind| {
            ["application/soap+xml", "text/xml", "application/xml"]
                .iter()
                .any(|expected| kind.trim().eq_ignore_ascii_case(expected))
        });
    if !soap {
        return errors.http(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "ONVIF_NOTIFY_TYPE_INVALID",
        );
    }
    let body = match body {
        Ok(body) => body,
        Err(error) => return errors.http(error.status(), "ONVIF_NOTIFY_BODY_UNAVAILABLE"),
    };
    let Ok(body) = std::str::from_utf8(&body) else {
        return errors.http(StatusCode::BAD_REQUEST, "ONVIF_NOTIFY_ENCODING_INVALID");
    };
    let Ok(notifications) = parse_notify(body, &id, Utc::now()) else {
        return errors.http(StatusCode::BAD_REQUEST, "ONVIF_NOTIFY_INVALID");
    };
    match destination.try_send(notifications) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => errors.http(
            StatusCode::SERVICE_UNAVAILABLE,
            "ONVIF_NOTIFY_QUEUE_UNAVAILABLE",
        ),
    }
}

async fn native(
    State(app): State<Arc<App>>,
    Path((id, key)): Path<(String, String)>,
    axum::Extension(correlation): axum::Extension<CorrelationId>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let errors = RequestErrors {
        diagnostics: &app.hub.diagnostics,
        caller: "http::native",
        correlation: correlation.0,
    };
    let Some(route) = app
        .native
        .get(&id)
        .filter(|route| constant_match(&key, &route.token))
    else {
        return errors.http(StatusCode::NOT_FOUND, "NATIVE_WEBHOOK_ROUTE_UNKNOWN");
    };
    let body = match body {
        Ok(body) => body,
        Err(error) => return errors.http(error.status(), "NATIVE_WEBHOOK_BODY_UNAVAILABLE"),
    };
    let received = Utc::now();
    let deliver = |result: anyhow::Result<Vec<crate::model::Observation>>| match result {
        Ok(events) => {
            for event in events {
                app.hub.ingest(event);
            }
            StatusCode::OK.into_response()
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(errors.reply("NATIVE_WEBHOOK_PAYLOAD_INVALID", Some(error))),
        )
            .into_response(),
    };
    match &route.parser {
        NativeParser::HaCfg(decoder) => {
            let mut decoder = decoder.lock().unwrap();
            deliver(parse_native(&mut decoder, &body, &id, received))
        }
        NativeParser::Cgi(channel) => deliver(crate::reolink::cgi::parse_notification(
            &body, &id, *channel, received,
        )),
    }
}

fn constant_match(value: &str, expected: &str) -> bool {
    bool::from(value.as_bytes().ct_eq(expected.as_bytes()))
}

async fn upgrade(
    State(app): State<Arc<App>>,
    axum::Extension(correlation): axum::Extension<CorrelationId>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let errors = RequestErrors {
        diagnostics: &app.hub.diagnostics,
        caller: "http::upgrade",
        correlation: correlation.0,
    };
    let auth = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '));
    if !auth.is_some_and(|(scheme, token)| {
        scheme.eq_ignore_ascii_case("Bearer") && constant_match(token, &app.ws_token)
    }) {
        return errors.http(StatusCode::UNAUTHORIZED, "WEBSOCKET_AUTH_REQUIRED");
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(error) => return errors.http(error.status(), "WEBSOCKET_UPGRADE_INVALID"),
    };
    upgrade
        .max_frame_size(32 * 1024)
        .max_message_size(32 * 1024)
        .on_upgrade(move |socket| websocket(socket, app))
        .into_response()
}

#[derive(Deserialize)]
struct Request {
    id: String,
    #[serde(flatten)]
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Subscribe {
        #[serde(flatten)]
        filter: Filter,
    },
    Snapshot,
    CallbacksList,
    CallbacksAdd {
        callback: Callback,
    },
    CallbacksUpdate {
        callback: Callback,
    },
    CallbacksRemove {
        callback_id: String,
    },
    Control {
        camera_id: String,
        command: Control,
    },
}

async fn websocket(mut socket: WebSocket, app: Arc<App>) {
    let mut updates = app.hub.subscribe();
    let mut filter = None;
    loop {
        let response = tokio::select! {
            _ = app.stop.cancelled() => return,
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Text(message))) => handle(&app, &message, &mut filter, &mut updates).await,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Binary(_))) => app.hub.diagnostics.error(
                        "http::websocket", "WEBSOCKET_TEXT_REQUIRED", None, Value::Null, None,
                    ),
                    _ => return,
                }
            }
            update = updates.recv(), if filter.is_some() => {
                let filter = filter.as_ref().unwrap();
                match update {
                    Ok(Update::Snapshot(snapshot)) if snapshot.matches(filter) => json!({"op":"snapshot","snapshot":&*snapshot}),
                    Ok(Update::Event(event)) if event.matches(filter) => json!({"op":"event","event":event.payload}),
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        updates = app.hub.subscribe();
                        json!({"op":"gap","snapshots":snapshots(&app.hub, filter)})
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        };
        let sent = timeout(
            Duration::from_secs(5),
            socket.send(Message::Text(response.to_string().into())),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            app.hub.diagnostics.error(
                "http::websocket",
                "WEBSOCKET_SEND_FAILED",
                None,
                Value::Null,
                None,
            );
            return;
        }
    }
}

async fn handle(
    app: &App,
    message: &str,
    filter: &mut Option<Filter>,
    updates: &mut broadcast::Receiver<Update>,
) -> Value {
    let correlation = Uuid::new_v4().to_string();
    let errors = RequestErrors {
        diagnostics: &app.hub.diagnostics,
        caller: "http::handle",
        correlation: correlation.clone(),
    };
    let request = match serde_json::from_str::<Request>(message) {
        Ok(request) if valid_id(&request.id) => Ok(request),
        Ok(_) => Err("WEBSOCKET_ID_INVALID"),
        Err(_) => Err("WEBSOCKET_REQUEST_INVALID"),
    };
    let request = match request {
        Ok(request) => request,
        Err(key) => {
            let mut reply = errors.reply(key, None);
            reply["id"] = Value::Null;
            return reply;
        }
    };
    let mut reply = match request.operation {
        Operation::Subscribe { filter: requested } => match requested.validate(&app.camera_ids) {
            Ok(()) => {
                *updates = app.hub.subscribe();
                let snapshots = snapshots(&app.hub, &requested);
                *filter = Some(requested);
                json!({"ok":true,"snapshots":snapshots})
            }
            Err(error) => errors.reply("WEBSOCKET_FILTER_INVALID", Some(error)),
        },
        Operation::Snapshot => {
            json!({"ok":true,"snapshots":snapshots(&app.hub, filter.as_ref().unwrap_or(&Filter::default()))})
        }
        Operation::CallbacksList => json!({"ok":true,"callbacks":app.hub.callbacks.list()}),
        Operation::CallbacksAdd { callback } => {
            edit(app, CallbackEdit::Add(callback), correlation.clone()).await
        }
        Operation::CallbacksUpdate { callback } => {
            edit(app, CallbackEdit::Update(callback), correlation.clone()).await
        }
        Operation::CallbacksRemove { callback_id } => {
            edit(app, CallbackEdit::Remove(callback_id), correlation.clone()).await
        }
        Operation::Control { camera_id, command } => {
            if let Some(controller) = app.controls.get(&camera_id) {
                match controller.enqueue(command, None, correlation.clone()) {
                    Ok(response) => tokio::select! {
                        _ = app.stop.cancelled() => errors.reply("WEBSOCKET_CONTROL_SHUTDOWN", None),
                        result = response => result.unwrap_or_else(|_| errors.reply("WEBSOCKET_CONTROL_WORKER_STOPPED", None)),
                    },
                    Err((key, error)) => errors.reply(key, Some(error)),
                }
            } else if app.camera_ids.contains(&camera_id) {
                errors.reply("WEBSOCKET_CONTROL_DISABLED", None)
            } else {
                errors.reply("WEBSOCKET_CAMERA_UNKNOWN", None)
            }
        }
    };
    reply["id"] = request.id.into();
    if reply["correlation_id"].is_null() {
        reply["correlation_id"] = correlation.into();
    }
    reply
}

fn snapshots(hub: &Hub, filter: &Filter) -> Vec<Value> {
    hub.snapshots(filter)
        .iter()
        .map(|snapshot| json!(&**snapshot))
        .collect()
}

async fn edit(app: &App, edit: CallbackEdit, correlation: String) -> Value {
    let callbacks = app.hub.callbacks.clone();
    let request_id = correlation.clone();
    match tokio::task::spawn_blocking(move || callbacks.edit(edit, request_id)).await {
        Ok(reply) => reply,
        Err(_) => app.hub.diagnostics.error(
            "http::edit",
            "WEBSOCKET_CALLBACK_TASK_FAILED",
            Some(correlation),
            Value::Null,
            None,
        ),
    }
}
