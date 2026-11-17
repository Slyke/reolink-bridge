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
}

pub struct Vault {
    file: PathBuf,
    _lock: File,
    data: Data,
}

impl Vault {
    pub fn open(path: &Path) -> Result<Self> {
        let dir =
            fs::symlink_metadata(path).context("prepare a private WUI credential directory")?;
        ensure!(
            dir.is_dir() && dir.permissions().mode() & 0o077 == 0,
            "WUI credential directory must have mode 0700"
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path.join("vault.lock"))?;
        lock.try_lock()
            .map_err(|_| anyhow::anyhow!("WUI credential directory already in use"))?;
        let file = path.join("vault.json");
        let data = if file.exists() {
            let meta = fs::symlink_metadata(&file)?;
            ensure!(
                meta.is_file() && meta.permissions().mode() & 0o077 == 0 && meta.len() <= 1_000_000,
                "invalid WUI vault file"
            );
            serde_json::from_slice(&fs::read(&file)?)
                .map_err(|_| anyhow::anyhow!("invalid WUI vault data"))?
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
            .map_err(|_| anyhow::anyhow!("could not save WUI vault"))?;
        // Update memory immediately after rename. A sync failure must never authorize a write.
        self.data = data;
        File::open(directory)?.sync_all()?;
        Ok(())
    }

    pub fn configure(&self, cameras: &mut Vec<Camera>) -> Result<()> {
        let mut known: BTreeSet<_> = cameras.iter().map(|camera| camera.id.clone()).collect();
        for device in self.data.devices.values() {
            if let Some(camera) = cameras.iter_mut().find(|camera| camera.id == device.id) {
                ensure!(
                    device.matches(camera),
                    "WUI vault camera binding differs from configured device"
                );
                camera.password = Some(device.password.clone());
            } else if device.enrolled && known.insert(device.id.clone()) {
                cameras.push(device.camera()?);
            }
        }
        // A transport must not repeatedly try an old credential after an interrupted rotation.
        cameras.retain(|camera| {
            self.data
                .rotations
                .get(&camera.id)
                .is_none_or(|rotation| rotation.staged.is_none())
        });
        Ok(())
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
        active: &BTreeSet<String>,
    ) -> Option<(Status, Option<String>)> {
        let job = self.data.jobs.get(&id)?;
        let verified = self.data.devices.contains_key(&job.camera);
        let status = if verified {
            if active.contains(&job.camera) {
                Status::Active
            } else {
                Status::RestartRequired
            }
        } else {
            job.status
        };
        Some((status, verified.then(|| job.camera.clone())))
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
        let old = Credential {
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
        if !factory {
            let _ = session.logout().await;
            data.devices.insert(job.camera.clone(), old);
            job.status = Status::Verified;
            return self.save(data);
        }
        let Ok(payload) = change_payload(&mut session, &old.username, true).await else {
            job.status = Status::LocalSetupRequired;
            return self.save(data);
        };
        let mut replacement = old;
        replacement.password = generated_password();
        job.status = Status::InProgress;
        job.staged = Some(replacement.clone());
        self.save(data)?;
        let _ = session
            .command(
                59,
                Some(0),
                &payload.replace("{{WUI_PASSWORD}}", &replacement.password),
            )
            .await;
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

    pub fn rotation_progress(&self, camera: &str, devices: &BTreeMap<String, Device>) -> Status {
        self.data
            .rotations
            .get(camera)
            .map_or(Status::NotStarted, |rotation| {
                if rotation.status == Status::Verified
                    && self.data.devices.get(camera).is_some_and(|current| {
                        devices
                            .get(camera)
                            .is_none_or(|running| running.password != current.password)
                    })
                {
                    Status::RestartRequired
                } else {
                    rotation.status
                }
            })
    }

    pub async fn rotate(&mut self, camera: &str, id: Uuid, old: Credential) -> Result<()> {
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
        let payload = change_payload(&mut session, &old.username, false).await?;
        let mut next = old.clone();
        next.password = generated_password();
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
        let _ = session
            .command(
                59,
                Some(0),
                &payload.replace("{{WUI_PASSWORD}}", &next.password),
            )
            .await;
        drop(session);
        self.reconcile(camera).await
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

async fn change_payload(session: &mut Session, username: &str, factory: bool) -> Result<String> {
    let info = session.command(80, None, "").await?;
    let info = xml::document(&info)?;
    ensure!(
        xml::value(info.root_element(), "hardwareVersion") == Some("IPC_NT1NO24MP")
            && xml::value(info.root_element(), "firmwareVersion") == Some("v3.1.0.6198_2604102075"),
        "unsupported device password protocol"
    );
    let raw = session.user_list(username).await?;
    let doc = xml::document(&raw)?;
    let list = xml::find(doc.root_element(), "UserList").context("camera omitted local users")?;
    let users: Vec<_> = list
        .children()
        .filter(|node| node.has_tag_name("User"))
        .collect();
    ensure!(
        !users.is_empty() && users.len() <= 32,
        "invalid device user list"
    );
    let mut output = String::from(r#"<body><UserList version="1.1">"#);
    let mut names = BTreeSet::new();
    let mut admin = false;
    for user in &users {
        let name = xml::child(*user, "userName")
            .and_then(xml::text)
            .context("missing device user name")?;
        ensure!(
            !name.is_empty() && names.insert(name),
            "invalid device user"
        );
        let target = name == username;
        if target {
            admin = xml::child(*user, "userLevel").and_then(xml::text) == Some("1");
        }
        output.push_str("<User>");
        for field in ["userName", "userLevel", "userId", "loginState"] {
            if let Some(value) = xml::child(*user, field).and_then(xml::text) {
                output.push_str(&format!("<{field}>{}</{field}>", xml::escape(value)));
            }
        }
        output.push_str(if target {
            "<userSetState>modify</userSetState><password>{{WUI_PASSWORD}}</password>"
        } else {
            "<userSetState>none</userSetState>"
        });
        output.push_str("</User>");
    }
    ensure!(
        admin && (!factory || (users.len() == 1 && username == "admin")),
        "device admin state not verified"
    );
    output.push_str("</UserList></body>");
    Ok(output)
}
