use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, sleep_until, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

use super::wire::{Device, Session};
use crate::{config::PtzMode, xml};

const MIN_MOVE_MS: u16 = 50;
const MAX_MOVE_MS: u16 = 3000;
const MAX_SPEED: u8 = 64;

pub type Failure = (&'static str, anyhow::Error);

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Move {
        direction: Direction,
        duration_ms: u16,
        speed: Option<u8>,
    },
    Stop,
    Status,
    ListPresets,
    GotoPreset {
        preset_id: u32,
    },
    DeletePreset {
        preset_id: u8,
    },
    SavePreset {
        preset_id: u8,
        name: String,
        #[serde(default)]
        replace: bool,
    },
    SetZoom {
        position: i32,
    },
}

impl Action {
    pub fn validate(&self) -> Result<()> {
        if let Self::Move {
            duration_ms, speed, ..
        } = self
        {
            anyhow::ensure!(
                (MIN_MOVE_MS..=MAX_MOVE_MS).contains(duration_ms),
                "PTZ duration_ms must be {MIN_MOVE_MS}–{MAX_MOVE_MS}"
            );
            anyhow::ensure!(
                speed.is_none_or(|value| (1..=MAX_SPEED).contains(&value)),
                "PTZ speed must be 1–{MAX_SPEED}"
            );
        }
        if let Self::SavePreset { name, .. } = self {
            anyhow::ensure!(
                !name.trim().is_empty() && name.len() <= 32 && !name.chars().any(char::is_control),
                "invalid PTZ preset name"
            );
        }
        Ok(())
    }

    pub fn stop(&self) -> bool {
        matches!(self, Self::Stop)
    }

    pub fn moves_camera(&self) -> bool {
        matches!(
            self,
            Self::Move { .. } | Self::GotoPreset { .. } | Self::SetZoom { .. }
        )
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
    LeftUp,
    LeftDown,
    RightUp,
    RightDown,
    ZoomIn,
    ZoomOut,
}

impl Direction {
    fn native(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Up => "Up",
            Self::Down => "Down",
            Self::LeftUp => "LeftUp",
            Self::LeftDown => "LeftDown",
            Self::RightUp => "RightUp",
            Self::RightDown => "RightDown",
            Self::ZoomIn => "ZoomInc",
            Self::ZoomOut => "ZoomDec",
        }
    }

    fn supported(self, caps: &Capabilities) -> bool {
        match self {
            Self::Left | Self::Right => caps.pan,
            Self::Up | Self::Down => caps.tilt,
            Self::LeftUp | Self::LeftDown | Self::RightUp | Self::RightDown => caps.diagonal,
            Self::ZoomIn | Self::ZoomOut => caps.directional_zoom,
        }
    }
}

#[derive(Clone, Copy, Serialize)]
pub struct Range {
    min: i32,
    max: i32,
}

#[derive(Clone, Copy, Serialize)]
struct Zoom {
    position: i32,
    range: Range,
}

#[derive(Clone, Default, Serialize)]
pub struct Capabilities {
    pub discovery: &'static str,
    pub pan: bool,
    pub tilt: bool,
    pub diagonal: bool,
    pub directional_zoom: bool,
    pub presets: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoom_range: Option<Range>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_range: Option<Range>,
}

impl Capabilities {
    pub fn new(discovery: &'static str) -> Self {
        Self {
            discovery,
            ..Self::default()
        }
    }

    fn has_motors(&self) -> bool {
        self.pan || self.tilt || self.directional_zoom || self.zoom_range.is_some() || self.presets
    }

    pub fn snapshot(&self) -> Value {
        let mut value = json!(self);
        value["source"] = json!("baichuan");
        value["supported"] = json!(self.has_motors());
        value["save_presets"] = json!(self.presets);
        value["delete_presets"] = json!(self.presets);
        if self.pan || self.tilt || self.directional_zoom {
            value["duration_ms"] = json!({"min":MIN_MOVE_MS,"max":MAX_MOVE_MS});
        }
        value
    }

    fn check(&self, action: &Action) -> std::result::Result<(), Failure> {
        // Stop remains possible when a previously responding camera's discovery is unavailable.
        if action.stop()
            && (self.has_motors() || matches!(self.discovery, "pending" | "unavailable"))
        {
            return Ok(());
        }
        if matches!(self.discovery, "pending" | "unavailable") {
            return Err((
                "PTZ_CAPABILITIES_UNAVAILABLE",
                anyhow!("camera PTZ capabilities are unavailable"),
            ));
        }
        let supported = match action {
            Action::Move { direction, .. } => direction.supported(self),
            Action::Stop => false,
            Action::Status => self.pan || self.tilt || self.zoom_range.is_some(),
            Action::ListPresets
            | Action::GotoPreset { .. }
            | Action::SavePreset { .. }
            | Action::DeletePreset { .. } => self.presets,
            Action::SetZoom { .. } => self.zoom_range.is_some(),
        };
        if !supported {
            return Err((
                "PTZ_UNSUPPORTED",
                anyhow!("camera did not report support for the requested PTZ operation"),
            ));
        }
        if matches!(action, Action::Move { speed: Some(_), .. }) && self.speed_range.is_none() {
            return Err((
                "PTZ_SPEED_UNSUPPORTED",
                anyhow!("use the camera's default motor speed"),
            ));
        }
        Ok(())
    }
}

pub async fn probe(session: &mut Session, channel: u8, mode: PtzMode) -> Capabilities {
    if mode == PtzMode::Disabled {
        return Capabilities::new("disabled");
    }
    let reply = session.command(199, None, "").await.ok();
    let doc = reply.as_deref().and_then(|body| xml::document(body).ok());
    let item = doc.as_ref().and_then(|doc| {
        doc.descendants().find(|node| {
            node.has_tag_name("item")
                && xml::child(*node, "chnID")
                    .and_then(xml::text)
                    .and_then(|id| id.parse().ok())
                    == Some(channel)
        })
    });
    let number = |name| {
        item.and_then(|node| xml::child(node, name))
            .and_then(xml::text)
            .and_then(|value| value.parse::<u32>().ok())
    };
    let ptz_type = number("ptzType");
    let control_bits = number("ptzControl");
    let (pan, tilt, optical_zoom) = match mode {
        PtzMode::Auto => (
            matches!(ptz_type, Some(2 | 3 | 5 | 6 | 7)),
            matches!(ptz_type, Some(2 | 3 | 5 | 6)),
            matches!(ptz_type, Some(1 | 2 | 5)),
        ),
        PtzMode::PanTilt => (true, true, false),
        PtzMode::PanTiltZoom => (true, true, true),
        PtzMode::Zoom => (false, false, true),
        PtzMode::Disabled => unreachable!(),
    };
    let digital_zoom = mode == PtzMode::Auto && control_bits.is_some_and(|bits| bits & 16 != 0);
    let zoom_range = if optical_zoom || (mode == PtzMode::Auto && ptz_type.is_none()) {
        read_zoom(session, channel)
            .await
            .ok()
            .map(|zoom| zoom.range)
    } else {
        None
    };
    let presets = if let Some(count) = number("ptzPreset") {
        count > 0
    } else {
        pan && read_presets(session, channel).await.is_ok()
    };
    let discovery = if mode != PtzMode::Auto {
        "override"
    } else if ptz_type.is_some_and(|n| n <= 7) || digital_zoom || zoom_range.is_some() {
        "detected"
    } else {
        "unavailable"
    };
    Capabilities {
        discovery,
        pan,
        tilt,
        presets,
        diagonal: pan && tilt && control_bits.is_none_or(|bits| bits & 2 == 0),
        directional_zoom: optical_zoom || digital_zoom || zoom_range.is_some(),
        speed_range: control_bits.filter(|bits| bits & 64 != 0).map(|_| Range {
            min: 1,
            max: MAX_SPEED.into(),
        }),
        zoom_range,
    }
}

#[derive(Serialize)]
struct Preset {
    id: u32,
    name: String,
}

async fn read_presets(session: &mut Session, channel: u8) -> Result<Vec<Preset>> {
    let body = session.command(190, Some(channel), "").await?;
    let doc = xml::document(&body)?;
    let presets = xml::find(doc.root_element(), "PtzPreset")
        .context("camera did not return a PTZ preset list")?;
    check_channel(presets, channel)?;
    Ok(presets
        .descendants()
        .filter(|node| node.has_tag_name("preset"))
        .filter_map(|node| {
            let id = xml::value(node, "id")?.parse().ok()?;
            let name = xml::value(node, "name")?.to_owned();
            if matches!(xml::value(node, "enable"), Some("0" | "false")) {
                return None;
            }
            Some(Preset { id, name })
        })
        .take(256)
        .collect())
}

async fn read_zoom(session: &mut Session, channel: u8) -> Result<Zoom> {
    let body = session.command(294, Some(channel), "").await?;
    let doc = xml::document(&body)?;
    let root = doc.root_element();
    check_channel(root, channel)?;
    let node = xml::find(root, "zoom").context("camera did not return a zoom range")?;
    let integer = |name| {
        xml::value(node, name)
            .and_then(|value| value.parse::<i32>().ok())
            .context("invalid camera zoom response")
    };
    let (min, max, position) = (integer("minPos")?, integer("maxPos")?, integer("curPos")?);
    anyhow::ensure!(
        min < max && (min..=max).contains(&position),
        "invalid camera zoom range"
    );
    Ok(Zoom {
        position,
        range: Range { min, max },
    })
}

fn check_channel(root: roxmltree::Node<'_, '_>, channel: u8) -> Result<()> {
    if let Some(reply) = xml::value(root, "channelId") {
        anyhow::ensure!(
            reply.parse::<u8>().ok() == Some(channel),
            "camera reported a different PTZ channel"
        );
    }
    Ok(())
}

pub struct Outcome {
    pub result: Value,
    pub verified: bool,
    pub status: Option<Value>,
}

pub async fn apply(
    device: &Device,
    capabilities: &Capabilities,
    action: &Action,
    shutdown: &CancellationToken,
    cancel_move: &CancellationToken,
) -> std::result::Result<Outcome, Failure> {
    capabilities.check(action)?;
    let dispatch = || apply_once(device, capabilities, action, shutdown, cancel_move);
    match action {
        Action::Status | Action::ListPresets => {
            device
                .read_retries
                .read_if(
                    if matches!(action, Action::Status) {
                        "ptz.status"
                    } else {
                        "ptz.presets"
                    },
                    dispatch,
                    |(_, error)| super::retry::transient(error),
                )
                .await
        }
        _ => dispatch().await,
    }
}

async fn apply_once(
    device: &Device,
    capabilities: &Capabilities,
    action: &Action,
    shutdown: &CancellationToken,
    cancel_move: &CancellationToken,
) -> std::result::Result<Outcome, Failure> {
    if let Action::Move {
        direction,
        duration_ms,
        speed,
    } = action
    {
        return timed_move(
            device,
            *direction,
            *duration_ms,
            *speed,
            shutdown,
            cancel_move,
        )
        .await;
    }
    let mut session = tokio::select! {
        biased;
        _ = interrupt(shutdown, cancel_move), if action.moves_camera() => {
            return Err(("PTZ_MOVE_CANCELLED", anyhow!("PTZ command cancelled before connecting")));
        }
        result = device.connect() => result.map_err(|error| ("PTZ_CONNECT_FAILED", error))?,
    };
    let result = if action.moves_camera() {
        tokio::select! {
            biased;
            _ = interrupt(shutdown, cancel_move) => Err(("PTZ_MOVE_INTERRUPTED", anyhow!("PTZ command was interrupted"))),
            result = execute(&mut session, device.channel, capabilities, action) => result,
        }
    } else {
        execute(&mut session, device.channel, capabilities, action).await
    };
    if action.moves_camera()
        && result.as_ref().is_err_and(|(key, _)| {
            matches!(
                *key,
                "PTZ_MOVE_INTERRUPTED" | "PTZ_PRESET_MOVE_FAILED" | "PTZ_ZOOM_WRITE_FAILED"
            )
        })
    {
        // A set-position command can reach the camera even if its acknowledgement is lost.
        drop(session);
        reconnect_stop(device)
            .await
            .map_err(|error| ("PTZ_INTERRUPTED_STOP_FAILED", error))?;
    } else {
        session.logout().await;
    }
    result
}

async fn execute(
    session: &mut Session,
    channel: u8,
    capabilities: &Capabilities,
    action: &Action,
) -> std::result::Result<Outcome, Failure> {
    let result = |result: Value, verified| Outcome {
        result,
        verified,
        status: None,
    };
    match action {
        Action::Stop => {
            direction(session, channel, "Stop", None)
                .await
                .map_err(|error| ("PTZ_STOP_FAILED", error))?;
            Ok(result(json!({"action":"stop", "stopped":true}), false))
        }
        Action::Status => {
            let status = read_status(session, channel, capabilities)
                .await
                .map_err(|error| ("PTZ_STATUS_FAILED", error))?;
            Ok(Outcome {
                result: status.clone(),
                verified: true,
                status: Some(status),
            })
        }
        Action::ListPresets => {
            let presets = read_presets(session, channel)
                .await
                .map_err(|error| ("PTZ_PRESETS_FAILED", error))?;
            Ok(result(json!({"presets":presets}), true))
        }
        Action::SavePreset {
            preset_id,
            name,
            replace,
        } => {
            if !replace
                && read_presets(session, channel)
                    .await
                    .map_err(|e| ("PTZ_PRESET_LOOKUP_FAILED", e))?
                    .iter()
                    .any(|p| p.id == u32::from(*preset_id))
            {
                return Err((
                    "PTZ_PRESET_OCCUPIED",
                    anyhow!("preset number is already in use"),
                ));
            }
            let xml = format!(
                r#"<body><PtzPreset version="1.1"><channelId>{channel}</channelId><presetList><preset><id>{preset_id}</id><name>{}</name><command>setPos</command></preset></presetList></PtzPreset></body>"#,
                xml::escape(name)
            );
            session
                .command(19, Some(channel), &xml)
                .await
                .map_err(|error| ("PTZ_PRESET_SAVE_FAILED", error))?;
            let presets = read_presets(session, channel)
                .await
                .map_err(|error| ("PTZ_PRESETS_FAILED", error))?;
            let listed = presets
                .iter()
                .any(|preset| preset.id == u32::from(*preset_id) && preset.name == *name);
            let mut saved = json!({"action":"save_preset","preset_id":preset_id,"presets":presets});
            if listed && let Ok(position) = read_status(session, channel, capabilities).await {
                saved["position"] = position;
            }
            Ok(result(saved, listed))
        }
        Action::DeletePreset { preset_id } => {
            let before = read_presets(session, channel)
                .await
                .map_err(|e| ("PTZ_PRESET_LOOKUP_FAILED", e))?;
            let id = u32::from(*preset_id);
            if !before.iter().any(|preset| preset.id == id) {
                return Ok(result(
                    json!({"action":"delete_preset","preset_id":id,"presets":before}),
                    true,
                ));
            }
            let payload = format!(
                r#"<body><PtzPreset version="1.1"><channelId>{channel}</channelId><presetList><preset><id>{id}</id><command>delPos</command></preset></presetList></PtzPreset></body>"#
            );
            session
                .command(19, Some(channel), &payload)
                .await
                .map_err(|e| ("PTZ_PRESET_DELETE_FAILED", e))?;
            let presets = read_presets(session, channel)
                .await
                .map_err(|e| ("PTZ_PRESETS_FAILED", e))?;
            let removed = presets.iter().all(|preset| preset.id != id);
            Ok(result(
                json!({"action":"delete_preset","preset_id":id,"presets":presets}),
                removed,
            ))
        }
        Action::GotoPreset { preset_id } => {
            let presets = read_presets(session, channel)
                .await
                .map_err(|error| ("PTZ_PRESET_LOOKUP_FAILED", error))?;
            if !presets.iter().any(|preset| preset.id == *preset_id) {
                return Err((
                    "PTZ_PRESET_UNKNOWN",
                    anyhow!("preset ID is not present on the camera"),
                ));
            }
            let xml = format!(
                r#"<body><PtzPreset version="1.1"><channelId>{channel}</channelId><presetList><preset><id>{preset_id}</id><command>toPos</command></preset></presetList></PtzPreset></body>"#
            );
            session
                .command(19, Some(channel), &xml)
                .await
                .map_err(|error| ("PTZ_PRESET_MOVE_FAILED", error))?;
            Ok(result(
                json!({"action":"goto_preset","preset_id":preset_id}),
                false,
            ))
        }
        Action::SetZoom { position } => {
            let current = read_zoom(session, channel)
                .await
                .map_err(|error| ("PTZ_ZOOM_RANGE_FAILED", error))?;
            if !(current.range.min..=current.range.max).contains(position) {
                return Err((
                    "PTZ_ZOOM_OUT_OF_RANGE",
                    anyhow!(
                        "position must be {}–{}",
                        current.range.min,
                        current.range.max
                    ),
                ));
            }
            if current.position != *position {
                let xml = format!(
                    r#"<body><StartZoomFocus version="1.1"><channelId>{channel}</channelId><command>ZoomPos</command><movePos>{position}</movePos></StartZoomFocus></body>"#
                );
                session
                    .command(295, Some(channel), &xml)
                    .await
                    .map_err(|error| ("PTZ_ZOOM_WRITE_FAILED", error))?;
            }
            let observed = if current.position == *position {
                Some(current)
            } else {
                zoom_readback(session, channel, *position).await
            };
            let state = observed.map(|zoom| json!({"zoom":zoom,"received_at":chrono::Utc::now()}));
            Ok(Outcome {
                result: json!({"action":"set_zoom","requested":position,"zoom":observed}),
                verified: observed.is_some_and(|zoom| zoom.position == *position),
                status: state,
            })
        }
        Action::Move { .. } => unreachable!(),
    }
}

async fn read_status(
    session: &mut Session,
    channel: u8,
    capabilities: &Capabilities,
) -> Result<Value> {
    let mut status = json!({});
    let mut failure = None;
    if capabilities.pan || capabilities.tilt {
        match session.command(433, Some(channel), "").await {
            Ok(body) => {
                if let Ok(doc) = xml::document(&body)
                    && check_channel(doc.root_element(), channel).is_ok()
                {
                    for (key, tag) in [("pan", "pPos"), ("tilt", "tPos")] {
                        if let Some(position) = xml::value(doc.root_element(), tag)
                            .and_then(|value| value.parse::<i32>().ok())
                        {
                            status[key] = position.into();
                        }
                    }
                }
            }
            Err(error) => failure = Some(error),
        }
    }
    if capabilities.zoom_range.is_some() {
        match read_zoom(session, channel).await {
            Ok(zoom) => status["zoom"] = json!(zoom),
            Err(error) if failure.is_none() || super::retry::transient(&error) => {
                failure = Some(error)
            }
            Err(_) => {}
        }
    }
    if status == json!({}) {
        return Err(failure.unwrap_or_else(|| anyhow!("camera did not return PTZ position")));
    }
    status["received_at"] = json!(chrono::Utc::now());
    Ok(status)
}

async fn zoom_readback(session: &mut Session, channel: u8, target: i32) -> Option<Zoom> {
    let mut observed = None;
    let _ = timeout(Duration::from_secs(3), async {
        loop {
            sleep(Duration::from_millis(250)).await;
            match read_zoom(session, channel).await {
                Ok(zoom) => {
                    observed = Some(zoom);
                    if zoom.position == target {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
    .await;
    observed
}

async fn timed_move(
    device: &Device,
    movement: Direction,
    duration_ms: u16,
    speed: Option<u8>,
    shutdown: &CancellationToken,
    cancel_move: &CancellationToken,
) -> std::result::Result<Outcome, Failure> {
    let mut session = tokio::select! {
        biased;
        _ = interrupt(shutdown, cancel_move) => return Err(("PTZ_MOVE_CANCELLED", anyhow!("PTZ move cancelled before it started"))),
        result = device.connect() => result.map_err(|error| ("PTZ_MOVE_CONNECT_FAILED", error))?,
    };
    let started = Instant::now();
    let deadline = started + Duration::from_millis(u64::from(duration_ms));
    let start = tokio::select! {
        biased;
        _ = interrupt(shutdown, cancel_move) => Err(("PTZ_START_INTERRUPTED", anyhow!("PTZ move interrupted while awaiting camera acknowledgement"))),
        result = timeout_at(deadline.min(started + Duration::from_millis(850)), direction(&mut session, device.channel, movement.native(), speed)) => {
            result.map_err(anyhow::Error::from).and_then(|result| result)
                .map_err(|error| ("PTZ_MOVE_START_FAILED", error))
        },
    };
    if start.is_ok() {
        tokio::select! {
            _ = sleep_until(deadline) => {},
            _ = interrupt(shutdown, cancel_move) => {},
        }
    }
    // Session::read retains partial frames, and exchanges match by sequence. A pending
    // start reply cannot be mistaken for the subsequent Stop acknowledgement.
    if matches!(
        timeout(
            Duration::from_secs(2),
            direction(&mut session, device.channel, "Stop", None)
        )
        .await,
        Ok(Ok(()))
    ) {
        session.logout().await;
    } else {
        drop(session);
        reconnect_stop(device)
            .await
            .map_err(|error| ("PTZ_TIMED_STOP_FAILED", error))?;
    }
    // An interrupted acknowledgement is an expected release once Stop was acknowledged.
    if !matches!(start, Err(("PTZ_START_INTERRUPTED", _))) {
        start?;
    }
    Ok(Outcome {
        result: json!({
            "action":"move","direction":movement,"duration_ms":duration_ms,
            "stopped":true,"interrupted":cancel_move.is_cancelled() || shutdown.is_cancelled(),
        }),
        verified: false,
        status: None,
    })
}

async fn direction(
    session: &mut Session,
    channel: u8,
    command: &str,
    speed: Option<u8>,
) -> Result<()> {
    let speed = speed
        .map(|value| format!("<speed>{value}</speed>"))
        .unwrap_or_default();
    let xml = format!(
        r#"<body><PtzControl version="1.1"><channelId>{channel}</channelId><command>{command}</command>{speed}</PtzControl></body>"#
    );
    session.command(18, Some(channel), &xml).await?;
    Ok(())
}

async fn interrupt(shutdown: &CancellationToken, cancel_move: &CancellationToken) {
    tokio::select! {
        _ = shutdown.cancelled() => {},
        _ = cancel_move.cancelled() => {},
    }
}

async fn reconnect_stop(device: &Device) -> Result<()> {
    let mut session = timeout(Duration::from_secs(6), async {
        let mut session = device.connect().await?;
        direction(&mut session, device.channel, "Stop", None).await?;
        Ok::<_, anyhow::Error>(session)
    })
    .await
    .context("PTZ Stop retry timed out")??;
    session.logout().await;
    Ok(())
}
