use super::{Api, error, recordings::read_failure};
use crate::{
    http::CorrelationId,
    reolink::timelapse_photos::{self, Photo},
};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{NaiveDate, NaiveTime};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Mutex, Semaphore},
    time::{Instant, timeout},
};

#[derive(Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Window {
    date: NaiveDate,
    from: Option<NaiveTime>,
    to: Option<NaiveTime>,
}

struct Manifest {
    scope: String,
    window: Window,
    photos: BTreeMap<String, Photo>,
    expires: Instant,
}

pub(super) struct Gallery {
    manifests: Mutex<VecDeque<Manifest>>,
    slots: Semaphore,
}
impl Default for Gallery {
    fn default() -> Self {
        Self {
            manifests: Mutex::default(),
            slots: Semaphore::new(2),
        }
    }
}

pub(super) async fn list(
    State(api): State<Arc<Api>>,
    Path(camera): Path<String>,
    Query(window): Query<Window>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(device) = api.devices.get(&camera) else {
        return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation);
    };
    let from = window.date.and_time(window.from.unwrap_or(NaiveTime::MIN));
    let to = window.date.and_time(
        window
            .to
            .unwrap_or(NaiveTime::from_hms_opt(23, 59, 59).unwrap()),
    );
    if from > to {
        return error(
            StatusCode::BAD_REQUEST,
            "API_MEDIA_UNSUPPORTED",
            &correlation,
        );
    }
    let Ok(_slot) = api.photos.slots.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_READ_BUSY",
            &correlation,
        );
    };
    let scope = api.recording_scope(&camera);
    let result = timeout(Duration::from_secs(60), async {
        let mut session = device.connect().await?;
        let result = timelapse_photos::search(&mut session, device.channel, &scope, from, to).await;
        session.logout().await;
        result
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|r| r);
    match result {
        Ok(photos) => {
            let response = Json(json!({"photos":photos})).into_response();
            let mut cache = api.photos.manifests.lock().await;
            cache.retain(|m| {
                m.expires > Instant::now() && !(m.scope == scope && m.window == window)
            });
            if cache.len() >= 8 {
                cache.pop_front();
            }
            cache.push_back(Manifest {
                scope,
                window,
                photos: photos.into_iter().map(|p| (p.id.clone(), p)).collect(),
                expires: Instant::now() + Duration::from_secs(600),
            });
            response
        }
        Err(cause) => read_failure(
            &api,
            &correlation,
            &camera,
            "api::photos::list",
            "API_RECORDING_SEARCH_FAILED",
            cause,
        ),
    }
}

pub(super) async fn image(
    State(api): State<Arc<Api>>,
    Path((camera, id)): Path<(String, String)>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(device) = api.devices.get(&camera) else {
        return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation);
    };
    let scope = api.recording_scope(&camera);
    let photo = api
        .photos
        .manifests
        .lock()
        .await
        .iter()
        .rev()
        .filter(|m| m.scope == scope && m.expires > Instant::now())
        .find_map(|m| m.photos.get(&id).cloned());
    let Some(photo) = photo else {
        return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &correlation);
    };
    let Ok(_slot) = api.photos.slots.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_READ_BUSY",
            &correlation,
        );
    };
    let result = timeout(Duration::from_secs(60), async {
        let mut session = device.connect().await?;
        let result = timelapse_photos::download(&mut session, device.channel, &photo).await;
        session.logout().await;
        let jpeg = result?;
        tokio::task::spawn_blocking(move || timelapse_photos::png(jpeg)).await?
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|r| r);
    match result {
        Ok(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "private, no-store"),
            ],
            png,
        )
            .into_response(),
        Err(cause) => read_failure(
            &api,
            &correlation,
            &camera,
            "api::photos::image",
            "API_RECORDING_PREPARE_FAILED",
            cause,
        ),
    }
}
