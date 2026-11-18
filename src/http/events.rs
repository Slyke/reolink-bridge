use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::stream::unfold;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;

use super::{App, CorrelationId, snapshots};
use crate::{config::Filter, hub::Update};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Options {
    #[serde(default)]
    cameras: String,
    #[serde(default)]
    events: String,
}

pub(super) async fn stream(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Query(options): Query<Options>,
) -> Response {
    let primary = app.api.primary(&headers);
    let denied = |status| {
        (
            status,
            Json(json!({"ok":false,"errorKey":"API_SCOPE_DENIED","correlation_id":correlation.0})),
        )
            .into_response()
    };
    if !primary && !app.api.service(&headers) {
        return denied(StatusCode::UNAUTHORIZED);
    }
    let list = |value: &str| {
        value
            .split(',')
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut filter = Filter {
        cameras: list(&options.cameras),
        events: list(&options.events),
    };
    if filter.cameras.len() > 128 || filter.events.len() > 128 {
        return denied(StatusCode::FORBIDDEN);
    }
    if !primary {
        let Some(scoped) = app.api.event_filter(filter) else {
            return denied(StatusCode::FORBIDDEN);
        };
        filter = scoped;
    }
    if filter.validate(&app.api.registered).is_err() {
        return denied(StatusCode::FORBIDDEN);
    }

    // Subscribe before taking the initial snapshot so updates remain queued during startup.
    let updates = app.hub.subscribe();
    let initial = Some(json!({"op":"ready","snapshots":snapshots(&app.hub, &filter)}));
    let stream = unfold(
        (app, updates, filter, initial),
        |(app, mut updates, filter, mut initial)| async move {
            let payload = if let Some(payload) = initial.take() {
                payload
            } else {
                loop {
                    let update = tokio::select! {
                        _ = app.stop.cancelled() => return None,
                        update = updates.recv() => update,
                    };
                    match update {
                        Ok(Update::Snapshot(snapshot)) if snapshot.matches(&filter) => {
                            break json!({"op":"snapshot","snapshot":&*snapshot});
                        }
                        Ok(Update::Event(event)) if event.matches(&filter) => {
                            break json!({"op":"event","event":event.payload});
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            updates = app.hub.subscribe();
                            break json!({"op":"gap","snapshots":snapshots(&app.hub, &filter)});
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
            };
            Some((
                Ok::<_, Infallible>(Event::default().data(payload.to_string())),
                (app, updates, filter, initial),
            ))
        },
    );
    (
        [
            ("cache-control", "no-store, private"),
            ("x-accel-buffering", "no"),
        ],
        Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(8))),
    )
        .into_response()
}
