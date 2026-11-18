mod credentials;
mod media;
mod preview;
mod recordings;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, ensure};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::{
    config::{Callback, Camera, valid_id},
    http::{CorrelationId, authorized, edit_callbacks},
    hub::{CallbackEdit, Hub},
    reolink::{
        control::{AiClass, Settings},
        wire::Device,
    },
};
use credentials::{
    Credential, EnrollmentMethod, EnrollmentRequest, Identifiers, Status, Target, Vault,
};

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub service_token: Option<String>,
    pub admin_token: Option<String>,
    pub storage_directory: Option<PathBuf>,
    // An explicit list limits the scoped service token and credential operations.
    pub camera_ids: Option<BTreeSet<String>>,
    #[serde(default)]
    pub commands: bool,
    #[serde(default)]
    pub events: bool,
    #[serde(default)]
    pub callback_destinations: BTreeSet<Url>,
    #[serde(default)]
    targets: BTreeMap<String, Target>,
    #[serde(default)]
    rotation_ids: BTreeMap<String, Identifiers>,
    #[serde(default)]
    media: BTreeMap<String, media::Profile>,
    #[serde(default)]
    previews: preview::Config,
    #[serde(default)]
    recordings: recordings::Config,
}

pub struct Api {
    pub config: Config,
    http_token: String,
    vault: Option<Mutex<Vault>>,
    configured: BTreeSet<String>,
    pub registered: BTreeSet<String>,
    active: BTreeSet<String>,
    devices: BTreeMap<String, Device>,
    previews: preview::Previews,
    recordings: recordings::Recordings,
    hub: Option<Arc<Hub>>,
    media_slots: Arc<Semaphore>,
    settings_slots: Semaphore,
    stop: CancellationToken,
}

impl Api {
    pub fn prepare(
        config: Config,
        cameras: &mut Vec<Camera>,
        http_token: &str,
        snapshot_slot: Arc<Semaphore>,
        stop: CancellationToken,
    ) -> Result<Self> {
        let safe = |token: &str| {
            (24..=256).contains(&token.len()) && token.bytes().all(|b| (33..=126).contains(&b))
        };
        ensure!(
            config
                .service_token
                .as_ref()
                .is_none_or(|token| safe(token) && token != http_token),
            "invalid or reused API service token"
        );
        ensure!(
            config.admin_token.as_ref().is_none_or(|token| safe(token)
                && token != http_token
                && Some(token) != config.service_token.as_ref()),
            "invalid or reused API admin token"
        );
        ensure!(
            config.admin_token.is_some() == config.storage_directory.is_some(),
            "API admin requires a private vault"
        );
        ensure!(
            (config.targets.is_empty() && config.rotation_ids.is_empty())
                || config.admin_token.is_some(),
            "API credential management requires an admin token"
        );
        let configured: BTreeSet<_> = cameras.iter().map(|cam| cam.id.clone()).collect();
        let mut addresses = BTreeSet::new();
        let mut target_ids = BTreeSet::new();
        ensure!(
            config.targets.len() <= 64
                && config.targets.iter().all(|(id, target)| {
                    valid_id(id)
                        && valid_id(&target.camera_id)
                        && !configured.contains(&target.camera_id)
                        && target_ids.insert(target.camera_id.clone())
                        && target.port != 0
                        && (target.host.is_private()
                            || target.host.is_link_local()
                            || target.host.is_loopback())
                        && addresses.insert((target.host, target.port))
                        && target
                            .import_password
                            .as_ref()
                            .is_none_or(|value| !value.is_empty() && value.len() <= 256)
                }),
            "invalid API enrollment target"
        );
        ensure!(
            config
                .callback_destinations
                .iter()
                .all(|url| matches!(url.scheme(), "http" | "https")
                    && url.host().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none()
                    && url.port() != Some(0)),
            "invalid API callback destination"
        );
        ensure!(
            config
                .rotation_ids
                .iter()
                .all(|(camera, id)| configured.contains(camera) && id.valid()),
            "invalid API rotation binding"
        );
        ensure!(
            config
                .media
                .iter()
                .all(
                    |(id, profile)| (configured.contains(id) || target_ids.contains(id))
                        && profile.valid()
                ),
            "invalid API media profile"
        );

        ensure!(
            config.previews.cameras.iter().all(|(id, profile)| {
                (configured.contains(id) || target_ids.contains(id))
                    && (500..=2_147_483_647).contains(&profile.interval_ms)
            }),
            "invalid API preview profile"
        );

        let vault = config
            .storage_directory
            .as_ref()
            .map(|path| Vault::open(path))
            .transpose()?;
        let mut registered = configured.clone();
        if let Some(vault) = &vault {
            vault.configure(cameras)?;
            registered.extend(vault.registered().cloned());
        }
        ensure!(
            config
                .camera_ids
                .as_ref()
                .is_none_or(|cameras| !cameras.is_empty()
                    && cameras
                        .iter()
                        .all(|id| configured.contains(id) || target_ids.contains(id))),
            "invalid API camera scope"
        );
        let active = cameras.iter().map(|cam| cam.id.clone()).collect();
        let devices: BTreeMap<String, Device> = cameras
            .iter()
            .filter_map(|cam| {
                Some((
                    cam.id.clone(),
                    Device {
                        host: cam.host.clone(),
                        port: cam.baichuan_port?,
                        username: cam.username.clone(),
                        password: cam.password.clone()?,
                        channel: cam.channel,
                    },
                ))
            })
            .collect();
        let store = preview::store(&config.previews)?;
        let previews = preview::prepare(
            &config.previews,
            &devices,
            snapshot_slot,
            stop.clone(),
            store.clone(),
        )?;
        let mut recordings =
            recordings::Recordings::new(&config.recordings, devices.keys().cloned())?;
        recordings.thumbnail_store = store;
        Ok(Self {
            recordings,
            config,
            http_token: http_token.to_owned(),
            vault: vault.map(Mutex::new),
            configured,
            registered,
            active,
            devices,
            previews,
            hub: None,
            media_slots: Arc::new(Semaphore::new(4)),
            settings_slots: Semaphore::new(8),
            stop,
        })
    }

    pub fn attach(mut self, hub: Arc<Hub>, workers: &mut JoinSet<()>) -> Arc<Self> {
        self.hub = Some(hub.clone());
        preview::start(&self.previews, hub, workers);
        let api = Arc::new(self);
        let cleaning = api.clone();
        workers.spawn(async move {
            loop {
                tokio::select! {
                    _=cleaning.stop.cancelled()=>return,
                    _=tokio::time::sleep(cleaning.recordings.cleanup_interval())=>cleaning.recordings.clean().await,
                }
            }
        });
        api
    }

    fn allows(&self, id: &str) -> bool {
        self.config
            .camera_ids
            .as_ref()
            .is_none_or(|ids| ids.contains(id))
    }

    pub fn primary(&self, headers: &HeaderMap) -> bool {
        authorized(headers, &self.http_token)
    }

    pub fn service(&self, headers: &HeaderMap) -> bool {
        self.config
            .service_token
            .as_ref()
            .is_some_and(|token| authorized(headers, token))
    }

    fn read_access(&self, headers: &HeaderMap, camera: &str) -> bool {
        self.primary(headers) || (self.service(headers) && self.allows(camera))
    }
    fn admin(&self, headers: &HeaderMap) -> bool {
        self.config
            .admin_token
            .as_ref()
            .is_some_and(|token| authorized(headers, token))
    }
    fn hub(&self) -> &Hub {
        self.hub.as_ref().unwrap()
    }

    pub fn command_access(&self, headers: &HeaderMap, camera: &str) -> bool {
        self.primary(headers)
            || (self.config.commands && self.service(headers) && self.allows(camera))
    }

    pub fn event_filter(&self, mut filter: crate::config::Filter) -> Option<crate::config::Filter> {
        if !self.config.events || filter.cameras.iter().any(|id| !self.allows(id)) {
            return None;
        }
        if filter.cameras.is_empty() {
            let scoped: Vec<_> = self
                .registered
                .iter()
                .filter(|id| self.allows(id))
                .cloned()
                .collect();
            if scoped.is_empty() && !self.registered.is_empty() {
                return None;
            }
            filter.cameras = scoped;
        }
        Some(filter)
    }

    pub fn callback_allowed(&self, primary: bool, destination: Option<&Url>) -> bool {
        (primary || self.config.camera_ids.is_none())
            && !self.config.callback_destinations.is_empty()
            && destination.is_none_or(|url| self.config.callback_destinations.contains(url))
    }
}

pub fn router(api: Arc<Api>) -> Router {
    let callbacks = Router::new()
        .route("/api/callbacks", get(callbacks).post(add_callback))
        .route(
            "/api/callbacks/{id}",
            axum::routing::put(update_callback).delete(remove_callback),
        )
        .layer(DefaultBodyLimit::max(32 * 1024));
    Router::new()
        .route("/api/contract", get(contract))
        .route("/api/inventory", get(inventory))
        .route("/api/cameras/{id}/storage", get(recordings::storage))
        .route("/api/cameras/{id}/recordings", get(recordings::list))
        .route(
            "/api/cameras/{id}/recordings/{recording}/video",
            get(recordings::video),
        )
        .route(
            "/api/cameras/{id}/recordings/{recording}/preview",
            get(recordings::preview),
        )
        .route(
            "/api/cameras/{id}/recordings/{recording}/metadata",
            get(recordings::metadata),
        )
        .route("/api/cameras/{id}/settings/{group}", get(settings))
        .route("/api/cameras/{id}/media/{stream}", get(media::get))
        .route("/api/cameras/{id}/preview", get(preview::get))
        .route("/api/enrollment-targets", get(targets))
        .route("/api/enrollments", post(enroll))
        .route("/api/enrollments/{id}", get(enrollment_status))
        .route("/api/enrollments/{id}/resume", post(resume))
        .route(
            "/api/cameras/{id}/rotation",
            get(rotation_status).post(rotate),
        )
        .route(
            "/api/cameras/{id}/rotation/reconcile",
            post(reconcile_rotation),
        )
        .layer(DefaultBodyLimit::max(4096))
        .merge(callbacks)
        .with_state(api)
}

fn error(status: StatusCode, key: &'static str, correlation: &CorrelationId) -> Response {
    (
        status,
        Json(json!({"ok":false,"errorKey":key,"correlation_id":correlation.0})),
    )
        .into_response()
}
fn progress(status: Status, id: Option<String>, correlation: &CorrelationId) -> Response {
    Json(json!({"ok":true,"status":status,"camera_id":id,"correlation_id":correlation.0}))
        .into_response()
}

async fn contract(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    let primary = api.primary(&headers);
    if !primary && !api.service(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let profiles: Map<String, Value> = api
        .config
        .media
        .iter()
        .filter(|(id, _)| {
            api.active.contains(*id) && api.devices.contains_key(*id) && (primary || api.allows(id))
        })
        .map(|(id, profile)| (id.clone(), json!(profile.streams)))
        .collect();
    let recordings_available = api.recordings.available();
    Json(json!({"version":1,"eventStream":primary || api.config.events,"media":!profiles.is_empty(),"mediaCameras":profiles,
       "storageCameras":api.devices.keys().filter(|id| primary || api.allows(id)).collect::<Vec<_>>(),
       "recordingPreviewCameras":api.devices.keys().filter(|id| primary || api.allows(id)).collect::<Vec<_>>(),
       "recordingMetadataCameras":api.devices.keys().filter(|id| recordings_available && (primary || api.allows(id))).collect::<Vec<_>>(),
       "recordingsCameras":api.devices.keys().filter(|id| recordings_available && (primary || api.allows(id))).collect::<Vec<_>>(),
       "previewNotifications":true,
       "previewCameras":api.previews.keys().filter(|id| primary || api.allows(id)).collect::<Vec<_>>(),
       "enrollment":api.vault.is_some(),"inventory":true,"callbacks":api.callback_allowed(primary, None),"settings":["leds","spotlight","motion","ai","image"],"alerts":["motion"],
       "correlation_id":correlation.0})).into_response()
}

async fn inventory(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    let primary = api.primary(&headers);
    if !primary && !api.service(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    Json(json!({"cameras":api.registered.iter().filter(|id| primary || api.allows(id)).map(|id| json!({"id":id,"active":api.active.contains(id)})).collect::<Vec<_>>(), "correlation_id":correlation.0})).into_response()
}

async fn callbacks(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    let primary = api.primary(&headers);
    if !primary && !api.service(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    if !api.callback_allowed(primary, None) {
        return error(
            StatusCode::FORBIDDEN,
            "CALLBACK_DESTINATION_DENIED",
            &correlation,
        );
    }
    Json(json!({
        "ok": true,
        "callbacks": api.hub().callbacks.list(),
        "correlation_id": correlation.0
    }))
    .into_response()
}

async fn add_callback(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    body: Result<Json<Callback>, JsonRejection>,
) -> Response {
    let Ok(Json(callback)) = body else {
        return error(
            StatusCode::BAD_REQUEST,
            "CALLBACK_CONFIG_INVALID",
            &correlation,
        );
    };
    change_callback(&api, &headers, &correlation, CallbackEdit::Add(callback)).await
}

async fn update_callback(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    body: Result<Json<Callback>, JsonRejection>,
) -> Response {
    let Ok(Json(callback)) = body else {
        return error(
            StatusCode::BAD_REQUEST,
            "CALLBACK_CONFIG_INVALID",
            &correlation,
        );
    };
    if callback.id != id {
        return error(
            StatusCode::BAD_REQUEST,
            "CALLBACK_CONFIG_INVALID",
            &correlation,
        );
    }
    change_callback(&api, &headers, &correlation, CallbackEdit::Update(callback)).await
}

async fn remove_callback(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    change_callback(&api, &headers, &correlation, CallbackEdit::Remove(id)).await
}

async fn change_callback(
    api: &Api,
    headers: &HeaderMap,
    correlation: &CorrelationId,
    edit: CallbackEdit,
) -> Response {
    let primary = api.primary(headers);
    if !primary && !api.service(headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", correlation);
    }
    let destination = match &edit {
        CallbackEdit::Add(callback) | CallbackEdit::Update(callback) => Some(&callback.url),
        CallbackEdit::Remove(_) => None,
    };
    if !api.callback_allowed(primary, destination) {
        return error(
            StatusCode::FORBIDDEN,
            "CALLBACK_DESTINATION_DENIED",
            correlation,
        );
    }
    let mut reply = edit_callbacks(api.hub(), edit, correlation.0.clone()).await;
    reply["correlation_id"] = json!(correlation.0);
    let status = if reply["ok"] == true {
        StatusCode::OK
    } else {
        match reply["errorKey"].as_str() {
            Some("CALLBACK_CONFIG_INVALID") => StatusCode::UNPROCESSABLE_ENTITY,
            Some("CALLBACK_UPDATE_MISSING" | "CALLBACK_REMOVE_MISSING") => StatusCode::NOT_FOUND,
            Some("CALLBACK_CONFIG_CHANGED") => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    };
    (status, Json(reply)).into_response()
}

async fn settings(
    State(api): State<Arc<Api>>,
    Path((id, group)): Path<(String, String)>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.read_access(&headers, &id) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(device) = api.devices.get(&id) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_SETTING_UNSUPPORTED",
            &correlation,
        );
    };
    let groups = match group.as_str() {
        "leds" => vec![Settings::Leds],
        "spotlight" => vec![Settings::Spotlight],
        "motion" => vec![Settings::Motion],
        "image" => vec![Settings::Image],
        "ai" => [
            AiClass::Person,
            AiClass::Vehicle,
            AiClass::Pet,
            AiClass::Face,
            AiClass::Package,
        ]
        .into_iter()
        .map(Settings::Ai)
        .collect(),
        _ => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "API_SETTING_UNSUPPORTED",
                &correlation,
            );
        }
    };
    let capabilities = api.hub().snapshot(&id, "capabilities");
    let groups: Vec<_> = groups
        .into_iter()
        .filter(|group| {
            capabilities
                .as_ref()
                .is_some_and(|caps| caps.payload["baichuan"]["controls"][group.key()].is_object())
        })
        .collect();
    if groups.is_empty() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_SETTING_UNSUPPORTED",
            &correlation,
        );
    }
    let Ok(_permit) = api.settings_slots.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_READ_BUSY",
            &correlation,
        );
    };
    let values = timeout(Duration::from_secs(15), async {
        let mut session = device.connect().await?;
        let mut values = Map::new();
        for group in groups {
            let (mut value, _) = group
                .inspect(&mut session, device.channel)
                .await
                .ok_or_else(|| anyhow::anyhow!("settings read incomplete"))?;
            value.as_object_mut().unwrap().remove("vendor");
            values.insert(group.key().into(), value);
        }
        session.logout().await;
        Ok::<_, anyhow::Error>(values)
    })
    .await;
    match values {
        Ok(Ok(values)) => {
            Json(json!({"values":values,"correlation_id":correlation.0})).into_response()
        }
        _ => error(
            StatusCode::BAD_GATEWAY,
            "API_SETTING_READ_FAILED",
            &correlation,
        ),
    }
}

async fn targets(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.admin(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_ADMIN_REQUIRED", &correlation);
    }
    Json(json!({"targets":api.config.targets.keys().collect::<Vec<_>>(),"correlation_id":correlation.0})).into_response()
}

async fn enroll(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<EnrollmentRequest>,
) -> Response {
    if !api.admin(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_ADMIN_REQUIRED", &correlation);
    }
    let Some(vault) = &api.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ENROLLMENT_DISABLED",
            &correlation,
        );
    };
    let Ok(mut vault) = vault.try_lock() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_VAULT_BUSY",
            &correlation,
        );
    };
    let id = body.id;
    if vault
        .start(body, &api.config.targets, &api.configured)
        .is_err()
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ENROLLMENT_INVALID",
            &correlation,
        );
    }
    finish_enrollment(&api, &mut vault, id, false, &correlation).await
}
async fn enrollment_status(
    State(api): State<Arc<Api>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.admin(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_ADMIN_REQUIRED", &correlation);
    }
    let Some(vault) = &api.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ENROLLMENT_DISABLED",
            &correlation,
        );
    };
    let Ok(vault) = vault.try_lock() else {
        return progress(Status::InProgress, None, &correlation);
    };
    match vault.progress(id, &api.active) {
        Some((state, camera)) => progress(state, camera, &correlation),
        None => error(
            StatusCode::NOT_FOUND,
            "API_ENROLLMENT_UNKNOWN",
            &correlation,
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resume {
    method: Option<EnrollmentMethod>,
}

async fn resume(
    State(api): State<Arc<Api>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<Resume>,
) -> Response {
    if !api.admin(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_ADMIN_REQUIRED", &correlation);
    }
    let Some(vault) = &api.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ENROLLMENT_DISABLED",
            &correlation,
        );
    };
    let Ok(mut vault) = vault.try_lock() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_VAULT_BUSY",
            &correlation,
        );
    };
    finish_enrollment(
        &api,
        &mut vault,
        id,
        body.method == Some(EnrollmentMethod::Import),
        &correlation,
    )
    .await
}
async fn finish_enrollment(
    api: &Api,
    vault: &mut Vault,
    id: Uuid,
    import: bool,
    correlation: &CorrelationId,
) -> Response {
    if vault.progress(id, &api.active).is_none() {
        return error(StatusCode::NOT_FOUND, "API_ENROLLMENT_UNKNOWN", correlation);
    }
    if !matches!(
        timeout(
            Duration::from_secs(53),
            vault.resume(id, &api.config.targets, import)
        )
        .await,
        Ok(Ok(()))
    ) {
        return error(
            StatusCode::BAD_GATEWAY,
            "API_ENROLLMENT_INTERRUPTED",
            correlation,
        );
    }
    let (status, camera) = vault.progress(id, &api.active).unwrap();
    progress(status, camera, correlation)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotate {
    id: Uuid,
    confirmation: bool,
}

async fn rotation_status(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.admin(&headers) || !api.allows(&id) {
        return error(StatusCode::UNAUTHORIZED, "API_ADMIN_REQUIRED", &correlation);
    }
    let Some(vault) = &api.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ROTATION_DISABLED",
            &correlation,
        );
    };
    let Ok(vault) = vault.try_lock() else {
        return progress(Status::InProgress, None, &correlation);
    };
    progress(
        vault.rotation_progress(&id, &api.devices),
        None,
        &correlation,
    )
}

async fn rotate(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<Rotate>,
) -> Response {
    if !api.admin(&headers) || !api.allows(&id) {
        return error(StatusCode::UNAUTHORIZED, "API_ADMIN_REQUIRED", &correlation);
    }
    if !body.confirmation || body.id.get_version_num() != 4 {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_CONFIRM_REQUIRED",
            &correlation,
        );
    }
    let Some(vault) = &api.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ROTATION_DISABLED",
            &correlation,
        );
    };
    let Ok(mut vault) = vault.try_lock() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_VAULT_BUSY",
            &correlation,
        );
    };
    if vault.rotation_progress(&id, &api.devices) == Status::RestartRequired {
        return progress(Status::RestartRequired, None, &correlation);
    }
    let old = vault.credential(&id).or_else(|| {
        let device = api.devices.get(&id)?;
        Some(Credential {
            id: id.clone(),
            host: device.host.clone(),
            port: device.port,
            channel: device.channel,
            username: device.username.clone(),
            password: device.password.clone(),
            identifiers: api.config.rotation_ids.get(&id)?.clone(),
            enrolled: false,
        })
    });
    let Some(old) = old else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ROTATION_UNSUPPORTED",
            &correlation,
        );
    };
    if !matches!(
        timeout(Duration::from_secs(53), vault.rotate(&id, body.id, old)).await,
        Ok(Ok(()))
    ) {
        let status = vault.rotation_progress(&id, &api.devices);
        if matches!(status, Status::InProgress | Status::RecoveryRequired) {
            return progress(status, None, &correlation);
        }
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ROTATION_UNSUPPORTED",
            &correlation,
        );
    }
    progress(
        vault.rotation_progress(&id, &api.devices),
        None,
        &correlation,
    )
}

async fn reconcile_rotation(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<Rotate>,
) -> Response {
    if !api.admin(&headers) || !api.allows(&id) {
        return error(StatusCode::UNAUTHORIZED, "API_ADMIN_REQUIRED", &correlation);
    }
    if !body.confirmation || body.id.get_version_num() != 4 {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_CONFIRM_REQUIRED",
            &correlation,
        );
    }
    let Some(vault) = &api.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ROTATION_DISABLED",
            &correlation,
        );
    };
    let Ok(mut vault) = vault.try_lock() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_VAULT_BUSY",
            &correlation,
        );
    };
    if !matches!(
        timeout(Duration::from_secs(33), vault.reconcile(&id)).await,
        Ok(Ok(()))
    ) {
        return error(
            StatusCode::BAD_GATEWAY,
            "API_ROTATION_RECONCILE_FAILED",
            &correlation,
        );
    }
    progress(
        vault.rotation_progress(&id, &api.devices),
        None,
        &correlation,
    )
}
