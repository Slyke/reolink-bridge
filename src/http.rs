use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Path, Request as HttpRequest, State, WebSocketUpgrade,
        rejection::{BytesRejection, JsonRejection},
        ws::rejection::WebSocketUpgradeRejection,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::{sync::broadcast, time::timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    camera::ControlHandle,
    config::{Callback, Filter, valid_id},
    diagnostics::{Diagnostics, SERVICE, build_info},
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
    pub stop: CancellationToken,
    pub api: Arc<crate::api::Api>,
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
    let routes = Router::new()
        .route("/livez", get(liveness))
        .route("/readyz", get(readiness))
        .route("/ingest/onvif/{id}/{key}", post(notify))
        .route("/ingest/reolink/{id}/{key}", post(native))
        .route(
            "/api/cameras/{id}/command/exec",
            post(execute_camera).layer(DefaultBodyLimit::max(32 * 1024)),
        )
        .route(ws_path, get(upgrade))
        .layer(DefaultBodyLimit::max(xml::BODY_LIMIT))
        .with_state(app.clone());
    routes
        .merge(crate::api::router(app.api.clone()))
        .layer(middleware::from_fn(correlate))
}

const CORRELATION_HEADER: &str = "x-correlation-id";

#[derive(Clone)]
pub(crate) struct CorrelationId(pub(crate) String);

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
    let build = build_info();
    json!({
        "ok": ok, "probe":name, "service":SERVICE,
        "version":build.version, "buildHash":build.hash, "correlation_id": id.0,
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

pub(crate) fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .is_some_and(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("Bearer") && constant_match(token, expected)
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpCommand {
    id: String,
    command: Control,
}

async fn execute_camera(
    State(app): State<Arc<App>>,
    Path(camera_id): Path<String>,
    axum::Extension(correlation): axum::Extension<CorrelationId>,
    headers: HeaderMap,
    request: Result<Json<HttpCommand>, JsonRejection>,
) -> Response {
    let errors = RequestErrors {
        diagnostics: &app.hub.diagnostics,
        caller: "http::execute_camera",
        correlation: correlation.0,
    };
    if !app.api.command_access(&headers, &camera_id) {
        return errors.http(StatusCode::UNAUTHORIZED, "HTTP_CONTROL_AUTH_REQUIRED");
    }
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => return errors.http(rejection.status(), "HTTP_CONTROL_REQUEST_INVALID"),
    };
    if !valid_id(&request.id) {
        return errors.http(StatusCode::BAD_REQUEST, "HTTP_CONTROL_ID_INVALID");
    }
    let Some(handle) = app.controls.get(&camera_id) else {
        let (status, key) = if app.api.registered.contains(&camera_id) {
            (StatusCode::UNPROCESSABLE_ENTITY, "HTTP_CONTROL_DISABLED")
        } else {
            (StatusCode::NOT_FOUND, "HTTP_CONTROL_CAMERA_UNKNOWN")
        };
        return errors.http(status, key);
    };
    let mut reply = match handle.enqueue(
        request.command,
        Some(request.id.clone()),
        errors.correlation.clone(),
    ) {
        Ok(reply) => tokio::select! {
            _ = app.stop.cancelled() => errors.reply("HTTP_CONTROL_SHUTDOWN", None),
            result = reply => result.unwrap_or_else(|_| errors.reply("HTTP_CONTROL_WORKER_STOPPED", None)),
        },
        Err((key, error)) => errors.reply(key, Some(error)),
    };
    reply["id"] = request.id.into();
    reply["correlation_id"] = errors.correlation.into();
    let status = match reply["errorKey"].as_str() {
        _ if reply["ok"] == true => StatusCode::OK,
        Some(
            "CONTROL_ID_CONFLICT"
            | "PTZ_REQUEST_EXPIRED"
            | "PTZ_QUEUED_MOVE_CANCELLED"
            | "PTZ_MOVE_CANCELLED"
            | "PTZ_START_INTERRUPTED"
            | "PTZ_MOVE_INTERRUPTED",
        ) => StatusCode::CONFLICT,
        Some("PTZ_PRESET_UNKNOWN") => StatusCode::NOT_FOUND,
        Some(
            "CONTROL_VALIDATION_FAILED"
            | "CONTROL_TRANSPORT_DISABLED"
            | "PTZ_TRANSPORT_DISABLED"
            | "PTZ_UNSUPPORTED"
            | "PTZ_SPEED_UNSUPPORTED"
            | "PTZ_ZOOM_OUT_OF_RANGE",
        ) => StatusCode::UNPROCESSABLE_ENTITY,
        Some(
            "CONTROL_QUEUE_UNAVAILABLE"
            | "HTTP_CONTROL_SHUTDOWN"
            | "HTTP_CONTROL_WORKER_STOPPED"
            | "PTZ_CAPABILITIES_UNAVAILABLE",
        ) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, Json(reply)).into_response()
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
    let primary = app.api.primary(&headers);
    if !primary && !app.api.service(&headers) {
        return errors.http(StatusCode::UNAUTHORIZED, "WEBSOCKET_AUTH_REQUIRED");
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(error) => return errors.http(error.status(), "WEBSOCKET_UPGRADE_INVALID"),
    };
    upgrade
        .max_frame_size(32 * 1024)
        .max_message_size(32 * 1024)
        .on_upgrade(move |socket| websocket(socket, app, headers, primary))
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

async fn websocket(mut socket: WebSocket, app: Arc<App>, headers: HeaderMap, primary: bool) {
    let mut updates = app.hub.subscribe();
    let mut filter = None;
    let mut replies = FuturesUnordered::new();
    loop {
        let response = tokio::select! {
            _ = app.stop.cancelled() => return,
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Text(message))) => {
                        let Some(reply) = handle(&app, &message, &mut filter, &mut updates, &mut replies, &headers, primary).await else {
                            continue;
                        };
                        reply
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Binary(_))) => app.hub.diagnostics.error(
                        "http::websocket", "WEBSOCKET_TEXT_REQUIRED", None, Value::Null, None,
                    ),
                    _ => return,
                }
            }
            Some(reply) = replies.next(), if !replies.is_empty() => reply,
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

type PendingReplies<'a> = FuturesUnordered<BoxFuture<'a, Value>>;

async fn handle<'a>(
    app: &'a App,
    message: &str,
    filter: &mut Option<Filter>,
    updates: &mut broadcast::Receiver<Update>,
    replies: &mut PendingReplies<'a>,
    headers: &HeaderMap,
    primary: bool,
) -> Option<Value> {
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
            return Some(reply);
        }
    };
    let scoped = (!primary).then_some(app.api.as_ref());
    let mut operation = request.operation;
    if let Some(api) = scoped {
        let permitted = match &mut operation {
            Operation::Subscribe { filter } => api
                .event_filter(filter.clone())
                .map(|scope| *filter = scope)
                .is_some(),
            Operation::Snapshot => api.event_filter(Filter::default()).is_some(),
            Operation::CallbacksList | Operation::CallbacksRemove { .. } => {
                api.callback_allowed(false, None)
            }
            Operation::CallbacksAdd { callback } | Operation::CallbacksUpdate { callback } => {
                api.callback_allowed(false, Some(&callback.url))
            }
            Operation::Control { camera_id, .. } => api.command_access(headers, camera_id),
        };
        if !permitted {
            let mut reply = errors.reply("API_SCOPE_DENIED", None);
            complete_ws_reply(&mut reply, request.id, correlation);
            return Some(reply);
        }
    }
    let mut reply = match operation {
        Operation::Subscribe { filter: requested } => match requested.validate(&app.api.registered)
        {
            Ok(()) => {
                *updates = app.hub.subscribe();
                let snapshots = snapshots(&app.hub, &requested);
                *filter = Some(requested);
                json!({"ok":true,"snapshots":snapshots})
            }
            Err(error) => errors.reply("WEBSOCKET_FILTER_INVALID", Some(error)),
        },
        Operation::Snapshot => {
            let default = scoped
                .and_then(|api| api.event_filter(Filter::default()))
                .unwrap_or_default();
            json!({"ok":true,"snapshots":snapshots(&app.hub, filter.as_ref().unwrap_or(&default))})
        }
        Operation::CallbacksList => json!({"ok":true,"callbacks":app.hub.callbacks.list()}),
        Operation::CallbacksAdd { callback } => {
            edit_callbacks(&app.hub, CallbackEdit::Add(callback), correlation.clone()).await
        }
        Operation::CallbacksUpdate { callback } => {
            edit_callbacks(
                &app.hub,
                CallbackEdit::Update(callback),
                correlation.clone(),
            )
            .await
        }
        Operation::CallbacksRemove { callback_id } => {
            edit_callbacks(
                &app.hub,
                CallbackEdit::Remove(callback_id),
                correlation.clone(),
            )
            .await
        }
        Operation::Control { camera_id, command } => {
            if let Some(controller) = app.controls.get(&camera_id) {
                let urgent = command.ptz_action().is_some_and(|action| action.stop());
                let limit = if urgent { 20 } else { 16 };
                if replies.len() >= limit {
                    errors.reply("WEBSOCKET_CONTROL_REPLIES_FULL", None)
                } else {
                    let replay_id = command.ptz_action().map(|_| request.id.clone());
                    match controller.enqueue(command, replay_id, correlation.clone()) {
                        Ok(response) => {
                            let id = request.id;
                            replies.push(Box::pin(async move {
                                let mut reply = tokio::select! {
                                    _ = app.stop.cancelled() => errors.reply("WEBSOCKET_CONTROL_SHUTDOWN", None),
                                    result = response => result.unwrap_or_else(|_| errors.reply("WEBSOCKET_CONTROL_WORKER_STOPPED", None)),
                                };
                                complete_ws_reply(&mut reply, id, correlation);
                                reply
                            }));
                            return None;
                        }
                        Err((key, error)) => errors.reply(key, Some(error)),
                    }
                }
            } else if app.api.registered.contains(&camera_id) {
                errors.reply("WEBSOCKET_CONTROL_DISABLED", None)
            } else {
                errors.reply("WEBSOCKET_CAMERA_UNKNOWN", None)
            }
        }
    };
    complete_ws_reply(&mut reply, request.id, correlation);
    Some(reply)
}

fn complete_ws_reply(reply: &mut Value, id: String, correlation: String) {
    reply["id"] = id.into();
    if reply["correlation_id"].is_null() {
        reply["correlation_id"] = correlation.into();
    }
}

fn snapshots(hub: &Hub, filter: &Filter) -> Vec<Value> {
    hub.snapshots(filter)
        .iter()
        .map(|snapshot| json!(&**snapshot))
        .collect()
}

pub(crate) async fn edit_callbacks(hub: &Hub, edit: CallbackEdit, correlation: String) -> Value {
    let callbacks = hub.callbacks.clone();
    let request_id = correlation.clone();
    match tokio::task::spawn_blocking(move || callbacks.edit(edit, request_id)).await {
        Ok(reply) => reply,
        Err(_) => hub.diagnostics.error(
            "http::edit_callbacks",
            "CALLBACK_TASK_FAILED",
            Some(correlation),
            Value::Null,
            None,
        ),
    }
}
