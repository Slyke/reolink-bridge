mod clock;
mod credentials;
mod display;
mod encoding;
mod media;
mod media_files;
mod network;
mod photos;
mod preview;
mod recordings;
mod rtsp;
mod users;

use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::JsonRejection},
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
pub(crate) use credentials::factory_setup_pending;
use credentials::{
    Credential, EnrollmentMethod, EnrollmentRequest, Identifiers, Status, Target, Vault,
};

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub storage_directory: Option<PathBuf>,
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

impl Config {
    pub(crate) fn validate_initial_passwords(&self, input: &Value) -> Result<()> {
        for (name, target) in &self.targets {
            if !input["targets"][name]["initialPassword"].is_null() {
                let field = format!("api.targets.{name}.initialPassword");
                let Some(password) = &target.initial_password else {
                    anyhow::bail!("set the environment variable for {field}");
                };
                ensure!(
                    credentials::valid_new_password(password),
                    "{field} must contain 8 to 31 printable ASCII characters"
                );
            }
        }
        Ok(())
    }
}

pub struct Api {
    pub config: Config,
    http_token: String,
    vault: Option<Mutex<Vault>>,
    enrollment_cameras: BTreeMap<String, Target>,
    enrollment_gates: BTreeMap<String, CancellationToken>,
    pub registered: BTreeSet<String>,
    identities: BTreeMap<String, String>,
    inactive_identities: BTreeMap<String, crate::identity::HardwareId>,
    active: BTreeSet<String>,
    devices: BTreeMap<String, Device>,
    onvif: BTreeMap<String, crate::onvif::Onvif>,
    previews: preview::Previews,
    recordings: recordings::Recordings,
    photos: photos::Gallery,
    hub: Option<Arc<Hub>>,
    media_slots: Arc<Semaphore>,
    settings_slots: Semaphore,
    network_writes: BTreeMap<String, Mutex<()>>,
    stop: CancellationToken,
}

impl Api {
    pub fn prepare(
        mut config: Config,
        cameras: &mut Vec<Camera>,
        http_token: &str,
        snapshot_slot: Arc<Semaphore>,
        stop: CancellationToken,
    ) -> Result<Self> {
        ensure!(
            (config.targets.is_empty() && config.rotation_ids.is_empty())
                || config.storage_directory.is_some(),
            "camera provisioning and credential rotation require api.storageDirectory"
        );
        let configured: BTreeSet<_> = cameras.iter().map(|cam| cam.id.clone()).collect();
        let mut identities: BTreeMap<_, _> = cameras
            .iter()
            .map(|c| (c.id.clone(), c.identity().to_owned()))
            .collect();
        let stable_ids: BTreeSet<_> = identities.values().cloned().collect();
        let mut configured_aliases = BTreeMap::new();
        for (name, target) in &config.targets {
            for camera in cameras.iter().filter(|cam| {
                cam.channel == 0
                    && cam.baichuan_port == Some(target.port)
                    && cam.host.parse::<Ipv4Addr>().ok() == Some(target.host)
            }) {
                ensure!(
                    camera.username == "admin" && camera.identity() == target.camera_id,
                    "named enrollment target conflicts with a configured camera on the same endpoint"
                );
                configured_aliases.insert(name.clone(), camera.id.clone());
            }
        }
        let mut addresses = BTreeSet::new();
        let mut target_ids = BTreeSet::new();
        ensure!(
            config.targets.len() <= 64
                && config.targets.iter().all(|(id, target)| {
                    valid_id(id)
                        && valid_id(&target.camera_id)
                        && (configured_aliases.contains_key(id)
                            || (!configured.contains(&target.camera_id)
                                && !stable_ids.contains(&target.camera_id)))
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
        for (id, profile) in &config.media {
            ensure!(
                configured.contains(id) || target_ids.contains(id),
                "unknown camera in API media profile"
            );
            profile
                .validate()
                .with_context(|| format!("invalid API media profile for {id}"))?;
        }

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
            identities.extend(vault.configure(cameras, &config.targets)?);
            for stable in vault
                .registered()
                .filter(|id| target_ids.contains(*id) && !stable_ids.contains(*id))
            {
                ensure!(
                    registered.insert(stable.clone()),
                    "configured camera ID collides with a vaulted camera"
                );
                identities.insert(stable.clone(), stable.clone());
            }
        }
        ensure!(
            cameras.iter().all(|camera| camera.password.is_some()),
            "camera requires cameras[].password or a matching verified vault credential"
        );
        let active: BTreeSet<_> = cameras.iter().map(|cam| cam.id.clone()).collect();
        let inactive_identities = registered
            .difference(&active)
            .filter_map(|id| {
                let credential = vault.as_ref()?.credential(&identities[id])?;
                Some((
                    id.clone(),
                    crate::identity::HardwareId::serial(credential.identifiers.serial.as_deref()?)?,
                ))
            })
            .collect();
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
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut onvif = BTreeMap::new();
        for camera in cameras.iter() {
            if let (Some(port), Some(password)) = (camera.onvif_port, camera.password.as_ref()) {
                let client = crate::onvif::Onvif::new(
                    http.clone(),
                    &camera.host,
                    port,
                    camera.username.clone(),
                    password.clone(),
                )?;
                onvif.insert(camera.id.clone(), client);
            }
        }
        let enrollment_cameras: BTreeMap<_, _> = cameras
            .iter()
            .filter(|cam| vault.is_some() && configured.contains(&cam.id))
            .filter_map(|cam| {
                Some((
                    cam.id.clone(),
                    Target::from_camera(cam, config.rotation_ids.get(&cam.id))?,
                ))
            })
            .collect();
        for (alias, public_id) in configured_aliases {
            if let Some(camera) = enrollment_cameras.get(&public_id) {
                config.targets.insert(alias, camera.clone());
            } else {
                config.targets.remove(&alias);
            }
        }
        let enrollment_gates = enrollment_cameras
            .iter()
            .filter(|(_, target)| {
                vault
                    .as_ref()
                    .is_some_and(|v| v.credential(&target.camera_id).is_none())
            })
            .map(|(id, _)| (id.clone(), CancellationToken::new()))
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
            photos: photos::Gallery::default(),
            config,
            http_token: http_token.to_owned(),
            vault: vault.map(Mutex::new),
            enrollment_cameras,
            enrollment_gates,
            registered,
            identities,
            inactive_identities,
            active,
            network_writes: devices
                .keys()
                .chain(onvif.keys())
                .map(|id| (id.clone(), Mutex::new(())))
                .collect(),
            devices,
            onvif,
            previews,
            hub: None,
            media_slots: Arc::new(Semaphore::new(4)),
            settings_slots: Semaphore::new(8),
            stop,
        })
    }

    pub fn enrollment_gate(&self, id: &str) -> Option<CancellationToken> {
        self.enrollment_gates.get(id).cloned()
    }

    fn enrollment_progress(&self, vault: &Vault, id: Uuid) -> Option<(Status, Option<String>)> {
        vault.progress(id, |stable| {
            self.active
                .iter()
                .find(|public| {
                    self.identities[*public] == stable
                        && self.devices.get(*public).is_some_and(|device| {
                            vault
                                .credential(stable)
                                .is_some_and(|credential| credential.password == device.password)
                        })
                })
                .cloned()
        })
    }

    pub fn camera_identity(&self, id: &str) -> Option<String> {
        self.hardware(id).map(|v| v.value)
    }

    fn hardware(&self, id: &str) -> Option<crate::identity::HardwareId> {
        self.hub
            .as_ref()?
            .identity(id)
            .or_else(|| self.inactive_identities.get(id).cloned())
    }

    fn recording_scope(&self, camera: &str) -> String {
        format!(
            "{camera}/{}",
            self.camera_identity(camera).unwrap_or_default()
        )
    }

    fn vault_key(&self, id: &str) -> Option<&str> {
        self.identities.get(id).map(String::as_str)
    }

    pub fn attach(mut self, hub: Arc<Hub>, workers: &mut JoinSet<()>) -> Arc<Self> {
        self.hub = Some(hub.clone());
        if let Some(store) = &self.recordings.thumbnail_store {
            let _ = store.diagnostics.set(hub.diagnostics.clone());
        }
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

    pub fn authenticated(&self, headers: &HeaderMap) -> bool {
        authorized(headers, &self.http_token)
    }

    fn hub(&self) -> &Hub {
        self.hub.as_ref().unwrap()
    }

    fn callback_allowed(&self, destination: Option<&Url>) -> bool {
        !self.config.callback_destinations.is_empty()
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
        .route(
            "/api/cameras/{id}/encoding",
            get(encoding::get).put(encoding::set),
        )
        .route("/api/cameras/{id}/clock", get(clock::get).put(clock::set))
        .route(
            "/api/cameras/{id}/network/wifi",
            axum::routing::put(network::switch),
        )
        .route(
            "/api/cameras/{id}/display",
            get(display::get).put(display::set),
        )
        .route(
            "/api/cameras/{id}/users",
            get(users::list).post(users::manage),
        )
        .route(
            "/api/cameras/{id}/network",
            get(network::get).put(network::set),
        )
        .route(
            "/api/cameras/{id}/network/rtsp",
            get(rtsp::get).put(rtsp::set),
        )
        .route("/api/cameras/{id}/users/password", post(users::change))
        .route("/api/cameras/{id}/time-lapse/photos", get(photos::list))
        .route(
            "/api/cameras/{id}/time-lapse/photos/{photo}",
            get(photos::image),
        )
        .route(
            "/api/cameras/{id}/media/capabilities",
            get(media_files::capabilities),
        )
        .route("/api/cameras/{id}/media/days", get(media_files::calendar))
        .route("/api/cameras/{id}/media/files", get(media_files::list))
        .route(
            "/api/cameras/{id}/media/delete",
            post(media_files::delete).layer(DefaultBodyLimit::max(80 * 1024)),
        )
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
        .route(
            "/api/enrollment-targets/identify",
            get(identify_enrollment_camera),
        )
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
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let profiles: Map<String, Value> = api
        .config
        .media
        .iter()
        .filter(|(id, _)| api.active.contains(*id) && api.devices.contains_key(*id))
        .map(|(id, profile)| (id.clone(), json!(profile.streams)))
        .collect();
    let recordings = if api.recordings.available() {
        api.devices.keys().collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    Json(json!({"version":1,"eventStream":true,"media":!profiles.is_empty(),"mediaCameras":profiles,
       "displayCameras":api.devices.keys().collect::<Vec<_>>(),
       "encodingCameras":api.devices.keys().collect::<Vec<_>>(),
       "clockCameras":api.devices.keys().collect::<Vec<_>>(),"rtspSettings":true,"networkCameras":api.devices.keys().chain(api.onvif.keys()).collect::<BTreeSet<_>>(),
       "userCameras":api.devices.keys().collect::<Vec<_>>(),
       "storageCameras":api.devices.keys().collect::<Vec<_>>(),
       "mediaFileCameras":api.devices.keys().collect::<Vec<_>>(),
       "timeLapsePhotoCameras":api.devices.keys().collect::<Vec<_>>(),
       "recordingPreviewCameras":api.devices.keys().collect::<Vec<_>>(),
       "recordingMetadataCameras":recordings,
       "recordingsCameras":recordings,
       "previewNotifications":true,
       "previewCameras":api.previews.keys().collect::<Vec<_>>(),
       "enrollment":api.vault.is_some(),"inventory":true,"callbacks":api.callback_allowed(None),"settings":["leds","spotlight","motion","ai","image","recording_schedule","time_lapse"],"alerts":["motion"],
       "correlation_id":correlation.0})).into_response()
}

async fn inventory(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    Json(json!({"cameras":api.registered.iter().map(|id| {
        let hardware = api.hardware(id);
        json!({"id":id,"stableId":hardware.as_ref().map(|v| &v.value),"identitySource":hardware.as_ref().map(|v| v.source),
            "previousId":api.identities[id],"active":api.active.contains(id)})
    }).collect::<Vec<_>>(), "correlation_id":correlation.0})).into_response()
}

async fn callbacks(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    if !api.callback_allowed(None) {
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
    if !api.authenticated(headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", correlation);
    }
    let destination = match &edit {
        CallbackEdit::Add(callback) | CallbackEdit::Update(callback) => Some(&callback.url),
        CallbackEdit::Remove(_) => None,
    };
    if !api.callback_allowed(destination) {
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
    if !api.authenticated(&headers) {
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
        "recording_schedule" => vec![Settings::RecordingSchedule],
        "time_lapse" => vec![Settings::TimeLapse],
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
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    Json(json!({"addressLookup":api.vault.is_some(),"enableRtsp":api.vault.is_some(),"targets":api.config.targets.keys().collect::<Vec<_>>(),"correlation_id":correlation.0})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentifyCamera {
    address: Ipv4Addr,
    port: Option<u16>,
    method: EnrollmentMethod,
}

async fn identify_enrollment_camera(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Query(query): Query<IdentifyCamera>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    if api.vault.is_none() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ENROLLMENT_DISABLED",
            &correlation,
        );
    }
    let (name, target) =
        match credentials::camera_at(&api.enrollment_cameras, query.address, query.port) {
            Ok(camera) => camera,
            Err(credentials::AddressError::Unknown) => {
                return error(
                    StatusCode::NOT_FOUND,
                    "API_ENROLLMENT_ADDRESS_UNKNOWN",
                    &correlation,
                );
            }
            Err(credentials::AddressError::Ambiguous) => {
                return error(
                    StatusCode::CONFLICT,
                    "API_ENROLLMENT_ADDRESS_AMBIGUOUS",
                    &correlation,
                );
            }
        };
    if query.method == EnrollmentMethod::Initialize
        && !target
            .initial_password
            .as_deref()
            .is_some_and(credentials::valid_new_password)
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ENROLLMENT_PASSWORD_INVALID",
            &correlation,
        );
    }
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_VAULT_BUSY",
            &correlation,
        );
    };
    match timeout(Duration::from_secs(15), target.identify(query.method)).await {
        Ok(Ok((identity, model))) => Json(json!({"target": name, "address":target.host, "port":target.port,
            "uid":identity.uid,"serial":identity.serial,"model":model,"correlation_id":correlation.0})).into_response(),
        _ => error(StatusCode::BAD_GATEWAY, "API_ENROLLMENT_IDENTIFY_FAILED", &correlation),
    }
}

async fn enroll(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<EnrollmentRequest>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
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
        .start(body, &api.config.targets, &api.enrollment_cameras)
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
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
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
    match api.enrollment_progress(&vault, id) {
        Some((state, camera)) => {
            enrollment_response(state, camera, vault.enrollment_rtsp(id), &correlation)
        }
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
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
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
    if api.enrollment_progress(vault, id).is_none() {
        return error(StatusCode::NOT_FOUND, "API_ENROLLMENT_UNKNOWN", correlation);
    }
    if !matches!(
        timeout(
            Duration::from_secs(53),
            vault.resume(id, &api.config.targets, &api.enrollment_cameras, import)
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
    match vault.configure_rtsp(id).await {
        Ok(Some(key)) => {
            api.hub().diagnostics.error(
                "api::enrollment",
                key,
                Some(correlation.0.clone()),
                json!({"enrollmentId":id}),
                None,
            );
        }
        Ok(None) => (),
        Err(_) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "API_ENROLLMENT_INTERRUPTED",
                correlation,
            );
        }
    }
    let (status, camera) = api.enrollment_progress(vault, id).unwrap();
    if status == Status::Active
        && let Some(name) = camera.as_deref()
        && let Some(gate) = api.enrollment_gates.get(name)
        && !gate.is_cancelled()
    {
        if let Some(credential) = api.vault_key(name).and_then(|key| vault.credential(key))
            && let Some(serial) = credential
                .identifiers
                .serial
                .as_deref()
                .and_then(crate::identity::HardwareId::serial)
        {
            api.hub().identify(name, Some(serial));
        }
        gate.cancel();
    }
    enrollment_response(status, camera, vault.enrollment_rtsp(id), correlation)
}

fn enrollment_response(
    status: Status,
    camera: Option<String>,
    rtsp: Option<credentials::EnrollmentRtsp>,
    cid: &CorrelationId,
) -> Response {
    Json(json!({"ok":true,"status":status,"camera_id":camera,"rtsp":rtsp,"correlation_id":cid.0}))
        .into_response()
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
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(stable) = api.vault_key(&id) else {
        return error(
            StatusCode::NOT_FOUND,
            "API_ROTATION_UNSUPPORTED",
            &correlation,
        );
    };
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
        vault.rotation_progress(stable, api.devices.get(&id)),
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
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    if !body.confirmation || body.id.get_version_num() != 4 {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_CONFIRM_REQUIRED",
            &correlation,
        );
    }
    let Some(stable) = api.vault_key(&id) else {
        return error(
            StatusCode::NOT_FOUND,
            "API_ROTATION_UNSUPPORTED",
            &correlation,
        );
    };
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
    if vault.rotation_progress(stable, api.devices.get(&id)) == Status::RestartRequired {
        return progress(Status::RestartRequired, None, &correlation);
    }
    let old = vault.credential(stable).or_else(|| {
        let device = api.devices.get(&id)?;
        Some(Credential {
            id: stable.to_owned(),
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
        timeout(
            Duration::from_secs(53),
            vault.rotate(stable, body.id, old, None)
        )
        .await,
        Ok(Ok(()))
    ) {
        let status = vault.rotation_progress(stable, api.devices.get(&id));
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
        vault.rotation_progress(stable, api.devices.get(&id)),
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
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    if !body.confirmation || body.id.get_version_num() != 4 {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_CONFIRM_REQUIRED",
            &correlation,
        );
    }
    let Some(stable) = api.vault_key(&id) else {
        return error(
            StatusCode::NOT_FOUND,
            "API_ROTATION_UNSUPPORTED",
            &correlation,
        );
    };
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
        timeout(Duration::from_secs(33), vault.reconcile(stable)).await,
        Ok(Ok(()))
    ) {
        return error(
            StatusCode::BAD_GATEWAY,
            "API_ROTATION_RECONCILE_FAILED",
            &correlation,
        );
    }
    progress(
        vault.rotation_progress(stable, api.devices.get(&id)),
        None,
        &correlation,
    )
}
