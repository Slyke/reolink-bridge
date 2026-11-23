use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    net::Ipv4Addr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::{
    config::{Camera, valid_id},
    reolink::wire::{Device, Session},
    xml,
};

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub camera_id: String,
    pub host: Ipv4Addr,
    #[serde(default = "port")]
    pub port: u16,
    pub initial_password: Option<String>,
    pub import_password: Option<String>,
    #[serde(skip)]
    binding: Option<Identifiers>,
}

pub(super) enum AddressError {
    Unknown,
    Ambiguous,
}

pub(super) fn camera_at(
    cameras: &BTreeMap<String, Target>,
    address: Ipv4Addr,
    port: Option<u16>,
) -> std::result::Result<(&str, &Target), AddressError> {
    let mut matches = cameras.iter().filter(|(_, camera)| {
        camera.host == address && port.is_none_or(|port| port == camera.port)
    });
    let (name, camera) = matches.next().ok_or(AddressError::Unknown)?;
    if matches.next().is_some() {
        return Err(AddressError::Ambiguous);
    }
    Ok((name, camera))
}

impl Target {
    pub(super) fn from_camera(camera: &Camera, binding: Option<&Identifiers>) -> Option<Self> {
        let host: Ipv4Addr = camera.host.parse().ok()?;
        if camera.username != "admin"
            || camera.channel != 0
            || !(host.is_private() || host.is_link_local() || host.is_loopback())
        {
            return None;
        }
        let password = camera.password.clone()?;
        Some(Self {
            host,
            port: camera.baichuan_port?,
            camera_id: camera.identity().to_owned(),
            initial_password: Some(password.clone()),
            import_password: Some(password),
            binding: binding.cloned(),
        })
    }

    pub(super) async fn identify(
        &self,
        method: EnrollmentMethod,
    ) -> Result<(Identifiers, Option<String>)> {
        let password = match method {
            EnrollmentMethod::Initialize => String::new(),
            EnrollmentMethod::Import => self
                .import_password
                .clone()
                .context("missing camera credential")?,
        };
        let device = Device {
            host: self.host.to_string(),
            port: self.port,
            channel: 0,
            username: "admin".into(),
            password,
        };
        let mut session = device.connect().await?;
        let result = reported_identity(&mut session).await;
        session.logout().await;
        result
    }
}

async fn reported_identity(session: &mut Session) -> Result<(Identifiers, Option<String>)> {
    let raw = session.command(114, None, "").await?;
    let doc = xml::document(&raw)?;
    let uid = xml::value(doc.root_element(), "uid")
        .and_then(crate::identity::HardwareId::uid)
        .context("camera omitted a valid Device UID")?;
    let mut identity = Identifiers {
        uid: Some(uid.value),
        serial: None,
    };
    let mut model = None;
    if let Ok(raw) = session.command(80, None, "").await
        && let Ok(doc) = xml::document(&raw)
    {
        let root = doc.root_element();
        identity.serial = crate::identity::native_serial(root).map(|id| id.value);
        model = xml::value(root, "name")
            .filter(|s| !s.is_empty() && s.len() <= 96 && !s.chars().any(char::is_control))
            .map(str::to_owned);
    }
    Ok((identity, model))
}

pub(crate) async fn factory_setup_pending(device: &Device) -> bool {
    if let Ok(mut session) = device.connect().await {
        session.logout().await;
        return false;
    }
    let mut factory = device.clone();
    factory.password.clear();
    let Ok(mut session) = factory.connect().await else {
        return false;
    };
    let identified = reported_identity(&mut session).await.is_ok();
    session.logout().await;
    identified
}

fn port() -> u16 {
    9000
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identifiers {
    pub uid: Option<String>,
    pub serial: Option<String>,
}

impl Identifiers {
    pub(super) fn hardware(&self) -> Option<crate::identity::HardwareId> {
        use crate::identity::HardwareId;
        let serial = self.serial.as_deref().and_then(HardwareId::serial);
        if let Some(uid) = self.uid.as_deref() {
            return Some(HardwareId::uid(uid)?.with_serial(serial));
        }
        serial
    }

    fn matches(&self, reported: &Identifiers) -> bool {
        [(&self.uid, &reported.uid), (&self.serial, &reported.serial)]
            .into_iter()
            .all(|(expected, actual)| {
                expected.as_ref().is_none_or(|expected| {
                    actual
                        .as_ref()
                        .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
                })
            })
    }

    pub fn valid(&self) -> bool {
        (self.uid.is_some() || self.serial.is_some())
            && self.uid.as_ref().is_none_or(|id| {
                (8..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
            })
            && self
                .serial
                .as_ref()
                .is_none_or(|id| (4..=64).contains(&id.len()) && valid_id(id))
    }
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentMethod {
    Initialize,
    Import,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentRequest {
    pub id: Uuid,
    pub uid: Option<String>,
    pub serial: Option<String>,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Ipv4Addr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub method: EnrollmentMethod,
    #[serde(default, rename = "enableRtsp")]
    pub enable_rtsp: bool,
    pub confirmation: bool,
}

impl EnrollmentRequest {
    fn resolve<'a>(
        &self,
        named: &'a BTreeMap<String, Target>,
        cameras: &'a BTreeMap<String, Target>,
    ) -> Result<&'a Target> {
        match (self.address, self.port) {
            (Some(address), Some(port)) => camera_at(cameras, address, Some(port))
                .ok()
                .filter(|(name, _)| *name == self.target)
                .map(|(_, camera)| camera),
            (None, None) => named.get(&self.target),
            _ => None,
        }
        .context("unknown enrollment target")
    }

    fn identifiers(&self) -> Identifiers {
        Identifiers {
            uid: self.uid.clone(),
            serial: self.serial.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Credential {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub channel: u8,
    pub username: String,
    pub password: String,
    pub identifiers: Identifiers,
    pub enrolled: bool,
}

impl Credential {
    pub fn device(&self) -> Device {
        Device {
            host: self.host.clone(),
            port: self.port,
            channel: self.channel,
            username: self.username.clone(),
            password: self.password.clone(),
        }
    }
    fn matches(&self, camera: &Camera) -> bool {
        self.host == camera.host
            && self.port == camera.baichuan_port.unwrap_or(0)
            && self.channel == camera.channel
            && self.username == camera.username
    }
    fn camera(&self) -> Result<Camera> {
        Ok(serde_json::from_value(serde_json::json!({
            "id": self.id, "host": self.host, "username": self.username, "password": self.password,
            "baichuanPort": self.port, "onvifPort": null, "channel": self.channel, "eventTransport": "baichuan"
        }))?)
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Job {
    request: EnrollmentRequest,
    #[serde(default)]
    import_requested: bool,
    #[serde(default)]
    superseded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rtsp: Option<EnrollmentRtsp>,
    camera: String,
    host: String,
    port: u16,
    status: Status,
    staged: Option<Credential>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum EnrollmentRtsp {
    Enabled,
    Unsupported,
    Failed,
    Unverified,
}

#[derive(Serialize, Deserialize, Clone)]
struct Rotation {
    id: Uuid,
    status: Status,
    staged: Option<(Credential, Credential)>,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Waiting,
    IdentityMismatch,
    LocalSetupRequired,
    RecoveryRequired,
    Verified,
    RestartRequired,
    Active,
    PreviousCredentialValid,
    InProgress,
    NotStarted,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Data {
    devices: BTreeMap<String, Credential>,
    jobs: BTreeMap<Uuid, Job>,
    rotations: BTreeMap<String, Rotation>,
    #[serde(default)]
    spent_rotations: BTreeSet<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    user_changes: BTreeMap<Uuid, UserChange>,
}

#[derive(Serialize, Deserialize, Clone)]
struct UserChange {
    #[serde(default)]
    action: UserAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    camera: String,
    username: String,
    status: Status,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum UserAction {
    #[default]
    Password,
    AddUser,
    AddAdministrator,
    Remove,
}

impl UserAction {
    fn new_level(self) -> Option<&'static str> {
        match self {
            Self::AddUser => Some("0"),
            Self::AddAdministrator => Some("1"),
            _ => None,
        }
    }
}

pub(super) struct AccountEdit<'a> {
    pub id: Uuid,
    pub username: &'a str,
    pub action: UserAction,
    pub password: Option<&'a str>,
    pub fingerprint: String,
}

impl UserChange {
    fn matches(&self, camera: &str, edit: &AccountEdit<'_>) -> bool {
        self.camera == camera
            && self.username == edit.username
            && self.action == edit.action
            && self
                .fingerprint
                .as_ref()
                .is_none_or(|old| old == &edit.fingerprint)
    }
}

pub struct Vault {
    file: PathBuf,
    _lock: File,
    data: Data,
}

impl Vault {
    pub fn open(path: &Path) -> Result<Self> {
        let dir =
            fs::symlink_metadata(path).context("prepare a private API credential directory")?;
        ensure!(
            dir.is_dir() && dir.permissions().mode() & 0o077 == 0,
            "API credential directory must have mode 0700"
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path.join("vault.lock"))?;
        lock.try_lock()
            .map_err(|_| anyhow::anyhow!("API credential directory already in use"))?;
        let file = path.join("vault.json");
        let mut data: Data = if file.exists() {
            let meta = fs::symlink_metadata(&file)?;
            ensure!(
                meta.is_file() && meta.permissions().mode() & 0o077 == 0 && meta.len() <= 1_000_000,
                "invalid API vault file"
            );
            serde_json::from_slice(&fs::read(&file)?)
                .map_err(|_| anyhow::anyhow!("invalid API vault data"))?
        } else {
            Data::default()
        };
        // Older enrollments automatically copied the decimal VersionInfo field alongside the UID.
        // Explicit rotationIds and new enrollment requests still have to match every supplied field.
        for credential in data.devices.values_mut().chain(
            data.rotations
                .values_mut()
                .filter_map(|r| r.staged.as_mut())
                .flat_map(|(old, new)| [old, new]),
        ) {
            if credential.enrolled
                && credential
                    .identifiers
                    .uid
                    .as_deref()
                    .and_then(crate::identity::HardwareId::uid)
                    .is_some()
                && credential
                    .identifiers
                    .serial
                    .as_ref()
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            {
                credential.identifiers.serial = None;
            }
        }
        Ok(Self {
            file,
            _lock: lock,
            data,
        })
    }

    fn save(&mut self, data: Data) -> Result<()> {
        let directory = self.file.parent().expect("validated vault path");
        let mut temp = NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut temp, &data)?;
        temp.flush()?;
        ensure!(
            temp.as_file().metadata()?.len() <= 1_000_000,
            "credential journal is full"
        );
        temp.as_file().sync_all()?;
        temp.persist(&self.file)
            .map_err(|_| anyhow::anyhow!("could not save API vault"))?;
        // Update memory immediately after rename. A sync failure must never authorize a write.
        self.data = data;
        File::open(directory)?.sync_all()?;
        Ok(())
    }

    pub fn configure(
        &self,
        cameras: &mut Vec<Camera>,
        targets: &BTreeMap<String, Target>,
    ) -> Result<BTreeMap<String, String>> {
        for device in self.data.devices.values() {
            if let Some(camera) = cameras.iter_mut().find(|camera| {
                camera.identity() == device.id
                    || (camera.stable_id.is_none()
                        && camera.password.is_none()
                        && device.matches(camera))
            }) {
                ensure!(
                    device.matches(camera),
                    "API vault camera binding differs from configured device"
                );
                camera.stable_id = Some(device.id.clone());
                camera.password = Some(device.password.clone());
            } else if device.enrolled
                && let Some(target) = targets
                    .values()
                    .find(|target| target.camera_id == device.id)
            {
                ensure!(
                    target.host.to_string() == device.host && target.port == device.port,
                    "API vault camera binding differs from enrollment target"
                );
                ensure!(
                    !cameras.iter().any(|c| c.id == device.id
                        || (c.host == device.host
                            && c.baichuan_port == Some(device.port)
                            && c.channel == device.channel)),
                    "configured camera ID collides with an enrolled camera"
                );
                cameras.push(device.camera()?);
            }
        }
        // A transport must not repeatedly try an old credential after an interrupted rotation.
        let keys = cameras
            .iter()
            .map(|camera| (camera.id.clone(), camera.identity().to_owned()))
            .collect();
        cameras.retain(|camera| {
            self.data
                .rotations
                .get(camera.identity())
                .is_none_or(|rotation| rotation.staged.is_none())
        });
        Ok(keys)
    }

    pub fn start(
        &mut self,
        request: EnrollmentRequest,
        targets: &BTreeMap<String, Target>,
        cameras: &BTreeMap<String, Target>,
    ) -> Result<()> {
        ensure!(
            request.confirmation
                && request.id.get_version_num() == 4
                && request.identifiers().valid()
                && (request.method != EnrollmentMethod::Initialize || request.uid.is_some()),
            "invalid enrollment"
        );
        if let Some(existing) = self.data.jobs.get(&request.id) {
            ensure!(request == existing.request, "enrollment conflict");
            return Ok(());
        }
        let target = request.resolve(targets, cameras)?;
        if request.method == EnrollmentMethod::Initialize
            && let Some(password) = &target.initial_password
        {
            ensure!(valid_new_password(password), "invalid first admin password");
        }
        ensure!(
            self.data.jobs.len() < 256 && !self.data.devices.contains_key(&target.camera_id),
            "target already registered"
        );
        let mut data = self.data.clone();
        for job in data
            .jobs
            .values_mut()
            .filter(|job| job.camera == target.camera_id && !job.superseded)
        {
            ensure!(
                request.address.is_some()
                    && job.staged.is_none()
                    && matches!(
                        job.status,
                        Status::IdentityMismatch | Status::LocalSetupRequired | Status::Waiting
                    ),
                "camera already has a pending enrollment"
            );
            job.superseded = true;
        }
        data.jobs.insert(
            request.id,
            Job {
                camera: target.camera_id.clone(),
                host: target.host.to_string(),
                port: target.port,
                request,
                import_requested: false,
                superseded: false,
                rtsp: None,
                status: Status::Waiting,
                staged: None,
            },
        );
        self.save(data)
    }

    pub fn progress(
        &self,
        id: Uuid,
        public_id: impl FnOnce(&str) -> Option<String>,
    ) -> Option<(Status, Option<String>)> {
        let job = self.data.jobs.get(&id)?;
        let verified =
            job.status == Status::Verified && self.data.devices.contains_key(&job.camera);
        let active = public_id(&job.camera);
        let status = if verified {
            if active.is_some() {
                Status::Active
            } else {
                Status::RestartRequired
            }
        } else {
            job.status
        };
        Some((
            status,
            verified.then(|| active.unwrap_or_else(|| job.camera.clone())),
        ))
    }

    pub async fn resume(
        &mut self,
        id: Uuid,
        targets: &BTreeMap<String, Target>,
        cameras: &BTreeMap<String, Target>,
        import: bool,
    ) -> Result<()> {
        let mut data = self.data.clone();
        let job = data.jobs.get_mut(&id).context("unknown enrollment")?;
        if job.superseded || data.devices.contains_key(&job.camera) {
            return Ok(());
        }
        let target = job.request.resolve(targets, cameras)?;
        ensure!(
            job.host == target.host.to_string()
                && job.port == target.port
                && job.camera == target.camera_id,
            "enrollment target changed"
        );
        if let Some(staged) = job.staged.clone() {
            // A request can resume after the write or after a crash just before it. Never issue it again.
            if authenticates(&staged).await {
                data.devices.insert(job.camera.clone(), staged);
                job.staged = None;
                job.status = Status::Verified;
            } else {
                job.status = Status::RecoveryRequired;
            }
            return self.save(data);
        }
        job.import_requested |= import;
        let factory = job.request.method == EnrollmentMethod::Initialize && !job.import_requested;
        let Some(password) = (if factory {
            Some(String::new())
        } else {
            target.import_password.clone()
        }) else {
            job.status = Status::LocalSetupRequired;
            return self.save(data);
        };
        let mut old = Credential {
            id: job.camera.clone(),
            host: job.host.clone(),
            port: job.port,
            channel: 0,
            username: "admin".into(),
            password,
            identifiers: job.request.identifiers(),
            enrolled: true,
        };
        let Ok(mut session) = old.device().connect().await else {
            job.status = if factory {
                Status::LocalSetupRequired
            } else {
                Status::Waiting
            };
            return self.save(data);
        };
        let binding = target
            .binding
            .as_ref()
            .filter(|binding| !binding.matches(&old.identifiers));
        for expected in std::iter::once(&old.identifiers).chain(binding) {
            if !identified(&mut session, expected).await {
                job.status = Status::IdentityMismatch;
                return self.save(data);
            }
        }
        if let Ok((identity, _)) = reported_identity(&mut session).await {
            old.identifiers.uid = identity.uid;
            old.identifiers.serial = old.identifiers.serial.or(identity.serial);
        }
        if !factory {
            let _ = session.logout().await;
            data.devices.insert(job.camera.clone(), old);
            job.status = Status::Verified;
            return self.save(data);
        }
        let mut replacement = old.clone();
        replacement.password = target
            .initial_password
            .clone()
            .unwrap_or_else(generated_password);
        ensure!(
            valid_new_password(&replacement.password),
            "invalid first admin password"
        );
        let Ok(payload) = account_payload(
            &mut session,
            &old.username,
            &old.username,
            UserAction::Password,
            Some(&replacement.password),
            AccountUse::FirstPassword,
        )
        .await
        else {
            job.status = Status::LocalSetupRequired;
            return self.save(data);
        };
        job.status = Status::InProgress;
        job.staged = Some(replacement.clone());
        self.save(data)?;
        let _ = session.command(59, Some(0), &payload).await;
        drop(session);
        let mut data = self.data.clone();
        let job = data.jobs.get_mut(&id).unwrap();
        if authenticates(&replacement).await {
            data.devices.insert(job.camera.clone(), replacement);
            job.staged = None;
            job.status = Status::Verified;
        } else {
            job.status = Status::RecoveryRequired;
        }
        self.save(data)
    }

    pub(super) fn enrollment_rtsp(&self, id: Uuid) -> Option<EnrollmentRtsp> {
        self.data.jobs.get(&id).and_then(|job| job.rtsp)
    }

    pub(super) async fn configure_rtsp(&mut self, id: Uuid) -> Result<Option<&'static str>> {
        use super::rtsp::Problem;
        let Some(job) = self.data.jobs.get(&id) else {
            return Ok(None);
        };
        if job.superseded
            || !job.request.enable_rtsp
            || job.rtsp.is_some()
            || job.status != Status::Verified
        {
            return Ok(None);
        }
        let credential = self
            .data
            .devices
            .get(&job.camera)
            .context("missing verified enrollment")?
            .clone();
        // Record the attempt first so a resumed enrollment cannot undo a later manual change.
        let mut data = self.data.clone();
        data.jobs.get_mut(&id).unwrap().rtsp = Some(EnrollmentRtsp::Unverified);
        self.save(data)?;
        let operation = async {
            let mut session = credential
                .device()
                .connect()
                .await
                .map_err(|_| Problem::Read)?;
            let result = if identified(&mut session, &credential.identifiers).await {
                super::rtsp::apply(&mut session, true).await.map(|_| ())
            } else {
                Err(Problem::Read)
            };
            session.logout().await;
            result
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(18), operation)
            .await
            .unwrap_or(Err(Problem::Unverified));
        let outcome = match result {
            Ok(()) => EnrollmentRtsp::Enabled,
            Err(Problem::Unsupported) => EnrollmentRtsp::Unsupported,
            Err(Problem::Unverified) => EnrollmentRtsp::Unverified,
            Err(_) => EnrollmentRtsp::Failed,
        };
        let mut data = self.data.clone();
        data.jobs.get_mut(&id).unwrap().rtsp = Some(outcome);
        self.save(data)?;
        Ok(result.err().map(Problem::key))
    }

    pub fn rotation_progress(&self, camera: &str, running: Option<&Device>) -> Status {
        self.data
            .rotations
            .get(camera)
            .map_or(Status::NotStarted, |rotation| {
                if rotation.status == Status::Verified
                    && self.data.devices.get(camera).is_some_and(|current| {
                        running.is_none_or(|running| running.password != current.password)
                    })
                {
                    Status::RestartRequired
                } else {
                    rotation.status
                }
            })
    }

    pub async fn rotate(
        &mut self,
        camera: &str,
        id: Uuid,
        old: Credential,
        password: Option<String>,
    ) -> Result<()> {
        if let Some(rotation) = self.data.rotations.get(camera) {
            if rotation.id == id {
                return Ok(());
            }
            ensure!(
                rotation.staged.is_none(),
                "reconcile staged credential first"
            );
        }
        ensure!(
            !self.data.spent_rotations.contains(&id)
                && !self.data.user_changes.contains_key(&id)
                && self.data.spent_rotations.len() < 4096,
            "retired or over-limit rotation ID"
        );
        let mut data = self.data.clone();
        // Rotation is gated to an authenticated, verified binding and an explicitly supported password protocol.
        let mut session = old.device().connect().await?;
        ensure!(
            identified(&mut session, &old.identifiers).await,
            "rotation identity mismatch"
        );
        let mut next = old.clone();
        next.password = password.unwrap_or_else(generated_password);
        let payload = account_payload(
            &mut session,
            &old.username,
            &old.username,
            UserAction::Password,
            Some(&next.password),
            AccountUse::Management,
        )
        .await?;
        data.rotations.insert(
            camera.into(),
            Rotation {
                id,
                status: Status::InProgress,
                staged: Some((old, next.clone())),
            },
        );
        data.spent_rotations.insert(id);
        self.save(data)?;
        let _ = session.command(59, Some(0), &payload).await;
        drop(session);
        self.reconcile(camera).await
    }

    pub(super) async fn edit_account(
        &mut self,
        camera: &str,
        operator: &Credential,
        edit: &AccountEdit<'_>,
    ) -> Result<Status> {
        let (id, username, action, password) = (edit.id, edit.username, edit.action, edit.password);
        ensure!(
            !username.eq_ignore_ascii_case(&operator.username),
            "bridge login requires credential rotation"
        );
        ensure!(
            !self.data.spent_rotations.contains(&id),
            "operation ID conflict"
        );
        if let Some(previous) = self.data.user_changes.get(&id) {
            ensure!(previous.matches(camera, edit), "operation ID conflict");
            if previous.status == Status::Verified {
                return Ok(previous.status);
            }
            // A disconnected write may have succeeded. Replays only perform readback.
        } else {
            ensure!(
                self.data.user_changes.len() < 4096,
                "device account operation limit reached"
            );
            let mut session = operator.device().connect().await?;
            ensure!(
                identified(&mut session, &operator.identifiers).await,
                "camera identity mismatch"
            );
            let payload = account_payload(
                &mut session,
                &operator.username,
                username,
                action,
                password,
                AccountUse::Management,
            )
            .await?;
            let mut data = self.data.clone();
            data.user_changes.insert(
                id,
                UserChange {
                    camera: camera.into(),
                    username: username.into(),
                    fingerprint: Some(edit.fingerprint.clone()),
                    action,
                    status: Status::RecoveryRequired,
                },
            );
            self.save(data)?;
            let _ = session.command(59, Some(0), &payload).await;
        }
        let verified = verify_account(operator, username, action, password)
            .await
            .unwrap_or(false);
        let status = if verified {
            Status::Verified
        } else {
            Status::RecoveryRequired
        };
        let mut data = self.data.clone();
        data.user_changes.get_mut(&id).unwrap().status = status;
        self.save(data)?;
        Ok(status)
    }

    pub async fn reconcile(&mut self, camera: &str) -> Result<()> {
        let mut data = self.data.clone();
        let Some(rotation) = data.rotations.get_mut(camera) else {
            return Ok(());
        };
        let Some((old, next)) = rotation.staged.clone() else {
            return Ok(());
        };
        if authenticates(&next).await {
            data.devices.insert(camera.into(), next);
            rotation.staged = None;
            rotation.status = Status::Verified;
        } else if authenticates(&old).await {
            rotation.staged = None;
            rotation.status = Status::PreviousCredentialValid;
        } else {
            rotation.status = Status::RecoveryRequired;
        }
        self.save(data)
    }

    pub(super) fn user_write_pending(&self, camera: &str, edit: &AccountEdit<'_>) -> bool {
        self.data
            .rotations
            .get(camera)
            .is_some_and(|op| op.id == edit.id && op.staged.is_some())
            || self
                .data
                .user_changes
                .get(&edit.id)
                .is_some_and(|op| op.matches(camera, edit) && op.status == Status::RecoveryRequired)
    }

    pub fn registered(&self) -> impl Iterator<Item = &String> {
        self.data.devices.keys()
    }

    pub fn credential(&self, camera: &str) -> Option<Credential> {
        self.data.devices.get(camera).cloned()
    }
}

fn generated_password() -> String {
    format!("Wui1{}", &Uuid::new_v4().simple().to_string()[..26])
}

async fn authenticates(credential: &Credential) -> bool {
    let Ok(mut session) = credential.device().connect().await else {
        return false;
    };
    let matched = identified(&mut session, &credential.identifiers).await;
    let _ = session.logout().await;
    matched
}

async fn identified(session: &mut Session, expected: &Identifiers) -> bool {
    let matched = |actual: Option<&str>, expected: &str| {
        actual.is_some_and(|value| value.eq_ignore_ascii_case(expected))
    };
    if let Some(uid) = &expected.uid {
        let Ok(raw) = session.command(114, None, "").await else {
            return false;
        };
        let Ok(doc) = xml::document(&raw) else {
            return false;
        };
        if !matched(xml::value(doc.root_element(), "uid"), uid) {
            return false;
        }
    }
    if let Some(serial) = &expected.serial {
        let Ok(raw) = session.command(80, None, "").await else {
            return false;
        };
        let Ok(doc) = xml::document(&raw) else {
            return false;
        };
        if !crate::identity::native_serial(doc.root_element())
            .is_some_and(|id| id.value.eq_ignore_ascii_case(serial))
        {
            return false;
        }
    }
    expected.valid()
}

pub(super) fn valid_new_password(password: &str) -> bool {
    (8..=31).contains(&password.len()) && password.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccountUse {
    FirstPassword,
    Management,
}

async fn password_protocol(session: &mut Session) -> Result<bool> {
    let info = session.command(80, None, "").await?;
    let info = xml::document(&info)?;
    let root = info.root_element();
    Ok(
        match (
            xml::value(root, "hardwareVersion"),
            xml::value(root, "firmwareVersion"),
        ) {
            (Some("IPC_NT1NO24MP"), Some("v3.1.0.6198_2604102075")) => true,
            (Some("IPC_NT1NA45MP"), Some("v3.1.0.4417_2412122130")) => {
                xml::value(root, "itemNo") == Some("E Series E330")
            }
            _ => false,
        },
    )
}

#[derive(Serialize)]
pub struct CameraUser {
    username: String,
    pub(super) administrator: bool,
    pub(super) bridge_login: bool,
}

fn user_nodes<'a, 'i>(doc: &'a roxmltree::Document<'i>) -> Result<Vec<roxmltree::Node<'a, 'i>>> {
    let list = xml::find(doc.root_element(), "UserList").context("camera omitted local users")?;
    let users: Vec<_> = list
        .children()
        .filter(|node| node.has_tag_name("User"))
        .collect();
    ensure!(
        !users.is_empty() && users.len() <= 32,
        "invalid device user list"
    );
    let mut names = BTreeSet::new();
    for user in &users {
        let name = field(*user, "userName").context("missing device username")?;
        ensure!(
            !name.is_empty()
                && name.len() <= 64
                && !name.chars().any(char::is_control)
                && names.insert(name),
            "invalid device username"
        );
    }
    Ok(users)
}

fn field<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    xml::child(node, name).and_then(xml::text)
}

pub async fn list_users(device: &Device) -> Result<(Vec<CameraUser>, bool)> {
    let mut session = device.connect().await?;
    let raw = session.user_list(&device.username).await?;
    let doc = xml::document(&raw)?;
    let users: Vec<_> = user_nodes(&doc)?
        .into_iter()
        .map(|node| {
            let username = field(node, "userName").unwrap().to_owned();
            CameraUser {
                bridge_login: username == device.username,
                administrator: field(node, "userLevel") == Some("1"),
                username,
            }
        })
        .collect();
    let writable = users.iter().any(|u| u.bridge_login && u.administrator)
        && password_protocol(&mut session).await.unwrap_or(false);
    session.logout().await;
    Ok((users, writable))
}

async fn verify_account(
    operator: &Credential,
    username: &str,
    action: UserAction,
    password: Option<&str>,
) -> Result<bool> {
    if action != UserAction::Password {
        let mut session = operator.device().connect().await?;
        ensure!(
            identified(&mut session, &operator.identifiers).await,
            "camera identity mismatch"
        );
        let raw = session.user_list(&operator.username).await?;
        session.logout().await;
        let doc = xml::document(&raw)?;
        let users = user_nodes(&doc)?;
        let user = users
            .into_iter()
            .find(|u| field(*u, "userName") == Some(username));
        if action == UserAction::Remove {
            return Ok(user.is_none());
        }
        if user.and_then(|u| field(u, "userLevel")) != action.new_level() {
            return Ok(false);
        }
    }
    let device = Device {
        username: username.into(),
        password: password.context("missing proposed password")?.into(),
        ..operator.device()
    };
    let mut session = device.connect().await?;
    session.logout().await;
    Ok(true)
}

async fn account_payload(
    session: &mut Session,
    operator: &str,
    target_name: &str,
    action: UserAction,
    password: Option<&str>,
    purpose: AccountUse,
) -> Result<String> {
    ensure!(
        password_protocol(session).await?,
        "unsupported device account protocol"
    );
    let raw = session.user_list(operator).await?;
    let doc = xml::document(&raw)?;
    let users = user_nodes(&doc)?;
    ensure!(users.iter().any(|u| field(*u, "userName") == Some(operator) && field(*u, "userLevel") == Some("1")), "administrator required");
    let primary = operator.eq_ignore_ascii_case("admin");
    let target = users
        .iter()
        .find(|u| field(**u, "userName") == Some(target_name));
    match action {
        UserAction::AddUser | UserAction::AddAdministrator => {
            ensure!(
                primary || action == UserAction::AddUser,
                "built-in administrator required"
            );
            ensure!(
                users.len() < 32
                    && !users.iter().any(|u| field(*u, "userName")
                        .is_some_and(|name| name.eq_ignore_ascii_case(target_name))),
                "account already exists or user list full"
            );
        }
        UserAction::Remove => {
            ensure!(
                !target_name.eq_ignore_ascii_case("admin")
                    && !target_name.eq_ignore_ascii_case(operator),
                "protected camera account"
            );
            let target = target.context("unknown device user")?;
            ensure!(
                primary || field(*target, "userLevel") == Some("0"),
                "built-in administrator required"
            );
        }
        UserAction::Password => {
            ensure!(target.is_some(), "unknown device user");
            ensure!(
                purpose != AccountUse::FirstPassword
                    || (users.len() == 1 && operator == "admin" && target_name == operator),
                "factory state not verified"
            );
        }
    }
    let mut output = String::from(r#"<body><UserList version="1.1">"#);
    for user in &users {
        output.push_str("<User>");
        for key in ["userName", "userLevel", "userId", "loginState"] {
            if let Some(value) = field(*user, key) {
                output.push_str(&format!("<{key}>{}</{key}>", xml::escape(value)));
            }
        }
        if field(*user, "userName") != Some(target_name) {
            output.push_str("<userSetState>none</userSetState>");
        } else if action == UserAction::Remove {
            output.push_str("<userSetState>delete</userSetState>");
        } else {
            output.push_str(&format!(
                "<userSetState>modify</userSetState><password>{}</password>",
                xml::escape(password.context("missing password")?)
            ));
        }
        output.push_str("</User>");
    }
    if let Some(level) = action.new_level() {
        output.push_str(&format!("<User><userName>{}</userName><userLevel>{level}</userLevel><userSetState>add</userSetState><password>{}</password></User>",
            xml::escape(target_name), xml::escape(password.context("missing password")?)));
    }
    output.push_str("</UserList></body>");
    Ok(output)
}
