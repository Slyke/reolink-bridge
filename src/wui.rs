mod credentials;
mod media;
mod preview;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, ensure};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
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
    config::{Camera, valid_id},
    http::{CorrelationId, authorized},
    hub::Hub,
    reolink::{
        control::{AiClass, Settings},
        wire::Device,
    },
};
use credentials::{
    Credential, EnrollmentMethod, EnrollmentRequest, Identifiers, Status, Target, Vault,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub service_token: String,
    pub admin_token: Option<String>,
    pub storage_directory: Option<PathBuf>,
    // Omission permits all cameras on this dedicated bridge. An explicit list limits scope.
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
}

pub struct Wui {
    pub config: Config,
    vault: Option<Mutex<Vault>>,
    configured: BTreeSet<String>,
    pub registered: BTreeSet<String>,
    active: BTreeSet<String>,
    devices: BTreeMap<String, Device>,
    previews: preview::Previews,
    hub: Option<Arc<Hub>>,
    media_slots: Arc<Semaphore>,
    settings_slots: Semaphore,
    stop: CancellationToken,
}

impl Wui {
    pub fn prepare(
        config: Config,
        cameras: &mut Vec<Camera>,
        legacy: &str,
        snapshot_slot: Arc<Semaphore>,
        stop: CancellationToken,
    ) -> Result<Self> {
        let safe = |token: &str| {
            (24..=256).contains(&token.len()) && token.bytes().all(|b| (33..=126).contains(&b))
        };
        ensure!(
            safe(&config.service_token) && config.service_token != legacy,
            "invalid or reused WUI service token"
        );
        ensure!(
            config.admin_token.as_ref().is_none_or(|token| safe(token)
                && token != legacy
                && token != &config.service_token),
            "invalid or reused WUI admin token"
        );
        ensure!(
            config.admin_token.is_some() == config.storage_directory.is_some(),
            "WUI admin requires a private vault"
        );
        ensure!(
            (config.targets.is_empty() && config.rotation_ids.is_empty())
                || config.admin_token.is_some(),
            "WUI credential management requires an admin token"
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
            "invalid WUI enrollment target"
        );
        ensure!(
            config
                .callback_destinations
                .iter()
                .all(|url| matches!(url.scheme(), "http" | "https")
                    && url.host().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none())
                && (config.callback_destinations.is_empty() || config.camera_ids.is_none()),
            "WUI callbacks require an explicit destination policy and a bridge-wide token"
        );
        ensure!(
            config
                .rotation_ids
                .iter()
                .all(|(camera, id)| configured.contains(camera) && id.valid()),
            "invalid WUI rotation binding"
        );
        ensure!(
            config
                .media
                .iter()
                .all(
                    |(id, profile)| (configured.contains(id) || target_ids.contains(id))
                        && profile.valid()
                ),
            "invalid WUI media profile"
        );

        ensure!(
            config.previews.cameras.iter().all(|(id, profile)| {
                (configured.contains(id) || target_ids.contains(id))
                    && config
                        .camera_ids
                        .as_ref()
                        .is_none_or(|scope| scope.contains(id))
                    && (500..=2_147_483_647).contains(&profile.interval_ms)
            }),
            "invalid WUI preview profile"
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
            "invalid WUI camera scope"
        );
        let active = cameras.iter().map(|cam| cam.id.clone()).collect();
        let devices = cameras
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
        let previews = preview::prepare(&config.previews, &devices, snapshot_slot, stop.clone())?;
        Ok(Self {
            config,
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
        self.hub = Some(hub);
        preview::start(&self.previews, workers);
        Arc::new(self)
    }

    fn allows(&self, id: &str) -> bool {
        self.config
            .camera_ids
            .as_ref()
            .is_none_or(|ids| ids.contains(id))
    }

    fn service(&self, headers: &HeaderMap) -> bool {
        authorized(headers, &self.config.service_token)
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
        self.service(headers) && self.config.commands && self.allows(camera)
    }
    pub fn session_access(&self, headers: &HeaderMap) -> bool {
        self.service(headers)
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

    pub fn callback_allowed(&self, destination: Option<&Url>) -> bool {
        !self.config.callback_destinations.is_empty()
            && destination.is_none_or(|url| self.config.callback_destinations.contains(url))
    }
}

pub fn router(wui: Arc<Wui>) -> Router {
    Router::new()
        .route("/api/wui/v1/contract", get(contract))
        .route("/api/wui/v1/inventory", get(inventory))
        .route("/api/wui/v1/cameras/{id}/settings/{group}", get(settings))
        .route("/api/wui/v1/cameras/{id}/media/{stream}", get(media::get))
        .route("/api/wui/v1/cameras/{id}/preview", get(preview::get))
        .route("/api/wui/v1/enrollment-targets", get(targets))
        .route("/api/wui/v1/enrollments", post(enroll))
        .route("/api/wui/v1/enrollments/{id}", get(enrollment_status))
        .route("/api/wui/v1/enrollments/{id}/resume", post(resume))
        .route(
            "/api/wui/v1/cameras/{id}/rotation",
            get(rotation_status).post(rotate),
        )
        .route(
            "/api/wui/v1/cameras/{id}/rotation/reconcile",
            post(reconcile_rotation),
        )
        .layer(DefaultBodyLimit::max(4096))
        .with_state(wui)
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
    State(wui): State<Arc<Wui>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !wui.service(&headers) {
        return error(StatusCode::UNAUTHORIZED, "WUI_SCOPE_DENIED", &correlation);
    }
    let profiles: Map<String, Value> = wui
        .config
        .media
        .iter()
        .filter(|(id, _)| {
            wui.active.contains(*id) && wui.devices.contains_key(*id) && wui.allows(id)
        })
        .map(|(id, profile)| (id.clone(), json!(profile.streams)))
        .collect();
    Json(json!({"version":1,"media":!profiles.is_empty(),"mediaCameras":profiles,
       "previewCameras":wui.previews.keys().collect::<Vec<_>>(),
       "enrollment":wui.vault.is_some(),"inventory":true,"settings":["leds","spotlight","motion","ai"],"alerts":["motion"],
       "correlation_id":correlation.0})).into_response()
}

async fn inventory(
    State(wui): State<Arc<Wui>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !wui.service(&headers) {
        return error(StatusCode::UNAUTHORIZED, "WUI_SCOPE_DENIED", &correlation);
    }
    Json(json!({"cameras":wui.registered.iter().filter(|id| wui.allows(id)).map(|id| json!({"id":id,"active":wui.active.contains(id)})).collect::<Vec<_>>(), "correlation_id":correlation.0})).into_response()
}

async fn settings(
    State(wui): State<Arc<Wui>>,
    Path((id, group)): Path<(String, String)>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !wui.service(&headers) || !wui.allows(&id) {
        return error(StatusCode::UNAUTHORIZED, "WUI_SCOPE_DENIED", &correlation);
    }
    let Some(device) = wui.devices.get(&id) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_SETTING_UNSUPPORTED",
            &correlation,
        );
    };
    let groups = match group.as_str() {
        "leds" => vec![Settings::Leds],
        "spotlight" => vec![Settings::Spotlight],
        "motion" => vec![Settings::Motion],
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
                "WUI_SETTING_UNSUPPORTED",
                &correlation,
            );
        }
    };
    let capabilities = wui.hub().snapshot(&id, "capabilities");
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
            "WUI_SETTING_UNSUPPORTED",
            &correlation,
        );
    }
    let Ok(_permit) = wui.settings_slots.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "WUI_READ_BUSY",
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
            "WUI_SETTING_READ_FAILED",
            &correlation,
        ),
    }
}

async fn targets(
    State(wui): State<Arc<Wui>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !wui.admin(&headers) {
        return error(StatusCode::UNAUTHORIZED, "WUI_ADMIN_REQUIRED", &correlation);
    }
    Json(json!({"targets":wui.config.targets.keys().collect::<Vec<_>>(),"correlation_id":correlation.0})).into_response()
}

async fn enroll(
    State(wui): State<Arc<Wui>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<EnrollmentRequest>,
) -> Response {
    if !wui.admin(&headers) {
        return error(StatusCode::UNAUTHORIZED, "WUI_ADMIN_REQUIRED", &correlation);
    }
    let Some(vault) = &wui.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_ENROLLMENT_DISABLED",
            &correlation,
        );
    };
    let Ok(mut vault) = vault.try_lock() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "WUI_VAULT_BUSY",
            &correlation,
        );
    };
    let id = body.id;
    if vault
        .start(body, &wui.config.targets, &wui.configured)
        .is_err()
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_ENROLLMENT_INVALID",
            &correlation,
        );
    }
    finish_enrollment(&wui, &mut vault, id, false, &correlation).await
}
async fn enrollment_status(
    State(wui): State<Arc<Wui>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !wui.admin(&headers) {
        return error(StatusCode::UNAUTHORIZED, "WUI_ADMIN_REQUIRED", &correlation);
    }
    let Some(vault) = &wui.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_ENROLLMENT_DISABLED",
            &correlation,
        );
    };
    let Ok(vault) = vault.try_lock() else {
        return progress(Status::InProgress, None, &correlation);
    };
    match vault.progress(id, &wui.active) {
        Some((state, camera)) => progress(state, camera, &correlation),
        None => error(
            StatusCode::NOT_FOUND,
            "WUI_ENROLLMENT_UNKNOWN",
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
    State(wui): State<Arc<Wui>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<Resume>,
) -> Response {
    if !wui.admin(&headers) {
        return error(StatusCode::UNAUTHORIZED, "WUI_ADMIN_REQUIRED", &correlation);
    }
    let Some(vault) = &wui.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_ENROLLMENT_DISABLED",
            &correlation,
        );
    };
    let Ok(mut vault) = vault.try_lock() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "WUI_VAULT_BUSY",
            &correlation,
        );
    };
    finish_enrollment(
        &wui,
        &mut vault,
        id,
        body.method == Some(EnrollmentMethod::Import),
        &correlation,
    )
    .await
}
async fn finish_enrollment(
    wui: &Wui,
    vault: &mut Vault,
    id: Uuid,
    import: bool,
    correlation: &CorrelationId,
) -> Response {
    if vault.progress(id, &wui.active).is_none() {
        return error(StatusCode::NOT_FOUND, "WUI_ENROLLMENT_UNKNOWN", correlation);
    }
    if !matches!(
        timeout(
            Duration::from_secs(53),
            vault.resume(id, &wui.config.targets, import)
        )
        .await,
        Ok(Ok(()))
    ) {
        return error(
            StatusCode::BAD_GATEWAY,
            "WUI_ENROLLMENT_INTERRUPTED",
            correlation,
        );
    }
    let (status, camera) = vault.progress(id, &wui.active).unwrap();
    progress(status, camera, correlation)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotate {
    id: Uuid,
    confirmation: bool,
}

async fn rotation_status(
    State(wui): State<Arc<Wui>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !wui.admin(&headers) || !wui.allows(&id) {
        return error(StatusCode::UNAUTHORIZED, "WUI_ADMIN_REQUIRED", &correlation);
    }
    let Some(vault) = &wui.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_ROTATION_DISABLED",
            &correlation,
        );
    };
    let Ok(vault) = vault.try_lock() else {
        return progress(Status::InProgress, None, &correlation);
    };
    progress(
        vault.rotation_progress(&id, &wui.devices),
        None,
        &correlation,
    )
}

async fn rotate(
    State(wui): State<Arc<Wui>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<Rotate>,
) -> Response {
    if !wui.admin(&headers) || !wui.allows(&id) {
        return error(StatusCode::UNAUTHORIZED, "WUI_ADMIN_REQUIRED", &correlation);
    }
    if !body.confirmation || body.id.get_version_num() != 4 {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_CONFIRM_REQUIRED",
            &correlation,
        );
    }
    let Some(vault) = &wui.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_ROTATION_DISABLED",
            &correlation,
        );
    };
    let Ok(mut vault) = vault.try_lock() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "WUI_VAULT_BUSY",
            &correlation,
        );
    };
    if vault.rotation_progress(&id, &wui.devices) == Status::RestartRequired {
        return progress(Status::RestartRequired, None, &correlation);
    }
    let old = vault.credential(&id).or_else(|| {
        let device = wui.devices.get(&id)?;
        Some(Credential {
            id: id.clone(),
            host: device.host.clone(),
            port: device.port,
            channel: device.channel,
            username: device.username.clone(),
            password: device.password.clone(),
            identifiers: wui.config.rotation_ids.get(&id)?.clone(),
            enrolled: false,
        })
    });
    let Some(old) = old else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_ROTATION_UNSUPPORTED",
            &correlation,
        );
    };
    if !matches!(
        timeout(Duration::from_secs(53), vault.rotate(&id, body.id, old)).await,
        Ok(Ok(()))
    ) {
        let status = vault.rotation_progress(&id, &wui.devices);
        if matches!(status, Status::InProgress | Status::RecoveryRequired) {
            return progress(status, None, &correlation);
        }
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_ROTATION_UNSUPPORTED",
            &correlation,
        );
    }
    progress(
        vault.rotation_progress(&id, &wui.devices),
        None,
        &correlation,
    )
}

async fn reconcile_rotation(
    State(wui): State<Arc<Wui>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<Rotate>,
) -> Response {
    if !wui.admin(&headers) || !wui.allows(&id) {
        return error(StatusCode::UNAUTHORIZED, "WUI_ADMIN_REQUIRED", &correlation);
    }
    if !body.confirmation || body.id.get_version_num() != 4 {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_CONFIRM_REQUIRED",
            &correlation,
        );
    }
    let Some(vault) = &wui.vault else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "WUI_ROTATION_DISABLED",
            &correlation,
        );
    };
    let Ok(mut vault) = vault.try_lock() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "WUI_VAULT_BUSY",
            &correlation,
        );
    };
    if !matches!(
        timeout(Duration::from_secs(33), vault.reconcile(&id)).await,
        Ok(Ok(()))
    ) {
        return error(
            StatusCode::BAD_GATEWAY,
            "WUI_ROTATION_RECONCILE_FAILED",
            &correlation,
        );
    }
    progress(
        vault.rotation_progress(&id, &wui.devices),
        None,
        &correlation,
    )
}
