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
    pub import_password: Option<String>,
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
    pub method: EnrollmentMethod,
    pub confirmation: bool,
}

impl EnrollmentRequest {
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
    camera: String,
    host: String,
    port: u16,
    status: Status,
    staged: Option<Credential>,
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
    camera: String,
    username: String,
    status: Status,
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
        let data = if file.exists() {
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
        configured: &BTreeSet<String>,
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
        let target = targets
            .get(&request.target)
            .context("unknown private target")?;
        ensure!(
            self.data.jobs.len() < 256
                && !configured.contains(&target.camera_id)
                && !self.data.devices.contains_key(&target.camera_id)
                && !self
                    .data
                    .jobs
                    .values()
                    .any(|job| job.camera == target.camera_id),
            "target already registered"
        );
        let mut data = self.data.clone();
        data.jobs.insert(
            request.id,
            Job {
                camera: target.camera_id.clone(),
                host: target.host.to_string(),
                port: target.port,
                request,
                import_requested: false,
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
        let verified = self.data.devices.contains_key(&job.camera);
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
        import: bool,
    ) -> Result<()> {
        let mut data = self.data.clone();
        let job = data.jobs.get_mut(&id).context("unknown enrollment")?;
        if data.devices.contains_key(&job.camera) {
            return Ok(());
        }
        let target = targets
            .get(&job.request.target)
            .context("unknown private target")?;
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
        if !identified(&mut session, &old.identifiers).await {
            job.status = Status::IdentityMismatch;
            return self.save(data);
        }
        // Keep the verified serial available for recovery when a credential change pauses this camera.
        if old.identifiers.serial.is_none()
            && let Ok(raw) = session.command(80, None, "").await
            && let Ok(doc) = xml::document(&raw)
            && let Some(id) = ["serialNumber", "serialNo"].iter().find_map(|field| {
                xml::value(doc.root_element(), field).and_then(crate::identity::HardwareId::serial)
            })
        {
            old.identifiers.serial = Some(id.value);
        }
        if !factory {
            let _ = session.logout().await;
            data.devices.insert(job.camera.clone(), old);
            job.status = Status::Verified;
            return self.save(data);
        }
        let mut replacement = old.clone();
        replacement.password = generated_password();
        let Ok(payload) = change_payload(
            &mut session,
            &old.username,
            &old.username,
            &replacement.password,
            true,
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
            !self.data.spent_rotations.contains(&id) && self.data.spent_rotations.len() < 4096,
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
        let payload = change_payload(
            &mut session,
            &old.username,
            &old.username,
            &next.password,
            false,
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

    pub async fn change_other_user(
        &mut self,
        camera: &str,
        id: Uuid,
        operator: &Credential,
        username: &str,
        password: String,
    ) -> Result<Status> {
        ensure!(
            username != operator.username,
            "bridge login requires credential rotation"
        );
        let verify = Device {
            username: username.into(),
            password: password.clone(),
            ..operator.device()
        };
        if let Some(previous) = self.data.user_changes.get(&id) {
            ensure!(
                previous.camera == camera && previous.username == username,
                "operation ID conflict"
            );
            if previous.status == Status::Verified {
                return Ok(previous.status);
            }
            // An interrupted write may have reached the camera. Only re-check the supplied login.
        } else {
            ensure!(
                self.data.user_changes.len() < 4096,
                "device password operation limit reached"
            );
            let mut session = operator.device().connect().await?;
            ensure!(
                identified(&mut session, &operator.identifiers).await,
                "camera identity mismatch"
            );
            let payload =
                change_payload(&mut session, &operator.username, username, &password, false)
                    .await?;
            let mut data = self.data.clone();
            data.user_changes.insert(
                id,
                UserChange {
                    camera: camera.into(),
                    username: username.into(),
                    status: Status::RecoveryRequired,
                },
            );
            self.save(data)?;
            let _ = session.command(59, Some(0), &payload).await;
            drop(session);
        }
        let status = if let Ok(mut session) = verify.connect().await {
            session.logout().await;
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

    pub fn user_write_pending(&self, camera: &str, id: Uuid) -> bool {
        self.data
            .rotations
            .get(camera)
            .is_some_and(|op| op.id == id && op.staged.is_some())
            || self
                .data
                .user_changes
                .get(&id)
                .is_some_and(|op| op.camera == camera && op.status == Status::RecoveryRequired)
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
        if !matched(
            xml::value(doc.root_element(), "serialNumber")
                .or_else(|| xml::value(doc.root_element(), "serialNo")),
            serial,
        ) {
            return false;
        }
    }
    expected.valid()
}

async fn password_protocol(session: &mut Session) -> Result<bool> {
    let info = session.command(80, None, "").await?;
    let info = xml::document(&info)?;
    Ok(
        xml::value(info.root_element(), "hardwareVersion") == Some("IPC_NT1NO24MP")
            && xml::value(info.root_element(), "firmwareVersion") == Some("v3.1.0.6198_2604102075"),
    )
}

#[derive(Serialize)]
pub struct CameraUser {
    username: String,
    administrator: bool,
    bridge_login: bool,
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

async fn change_payload(
    session: &mut Session,
    operator: &str,
    target_name: &str,
    password: &str,
    factory: bool,
) -> Result<String> {
    ensure!(
        password_protocol(session).await?,
        "unsupported device password protocol"
    );
    let raw = session.user_list(operator).await?;
    password_payload(&raw, operator, target_name, password, factory)
}
fn password_payload(
    raw: &str,
    operator: &str,
    target_name: &str,
    password: &str,
    factory: bool,
) -> Result<String> {
    let doc = xml::document(raw)?;
    let users = user_nodes(&doc)?;
    ensure!(users.iter().any(|u| field(*u, "userName") == Some(operator) && field(*u, "userLevel") == Some("1")), "administrator required");
    ensure!(
        users
            .iter()
            .any(|u| field(*u, "userName") == Some(target_name)),
        "unknown device user"
    );
    ensure!(
        !factory || (users.len() == 1 && operator == "admin" && target_name == operator),
        "factory state not verified"
    );
    let mut output = String::from(r#"<body><UserList version="1.1">"#);
    for user in &users {
        let target = field(*user, "userName") == Some(target_name);
        output.push_str("<User>");
        for field in ["userName", "userLevel", "userId", "loginState"] {
            if let Some(value) = xml::child(*user, field).and_then(xml::text) {
                output.push_str(&format!("<{field}>{}</{field}>", xml::escape(value)));
            }
        }
        if target {
            output.push_str(&format!(
                "<userSetState>modify</userSetState><password>{}</password>",
                xml::escape(password)
            ));
        } else {
            output.push_str("<userSetState>none</userSetState>");
        }
        output.push_str("</User>");
    }
    output.push_str("</UserList></body>");
    Ok(output)
}
