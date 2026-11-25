use super::{Api, error, media_files, recordings::read_failure};
use crate::{
    http::CorrelationId,
    reolink::wire::{Device, Rejected, Session},
    xml,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Extension, Json,
    body::{Body, to_bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;

const MAX_PACKAGE: usize = 128 * 1024 * 1024;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Firmware {
    model: String,
    hardware: String,
    installed: String,
    source: String,
    can_install: bool,
    update_available: Option<bool>,
    progress: Option<Progress>,
}

#[derive(Serialize)]
struct Progress {
    state: String,
    percent: Option<u8>,
}

fn writable(raw: &str) -> Result<bool> {
    let doc = xml::document(raw)?;
    Ok(xml::find(doc.root_element(), "AbilityInfo")
        .and_then(|n| xml::child(n, "system"))
        .and_then(|n| xml::child(n, "subModule"))
        .and_then(|n| xml::value(n, "abilityValue"))
        .is_some_and(|v| {
            v.split(|c: char| c == ',' || c.is_ascii_whitespace())
                .any(|s| s == "upgrade_rw")
        }))
}

// The status and online check are optional on cameras that accept local firmware packages.
async fn optional(session: &mut Session, cmd: u32, extension: &str) -> Result<Option<String>> {
    match session.exchange(cmd, 250, extension, "", false).await {
        Ok(reply) => Ok(Some(session.decode(&reply)?)),
        Err(err)
            if err
                .downcast_ref::<Rejected>()
                .is_some_and(|e| matches!(e.status(), 400 | 403 | 404 | 501)) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn offer(raw: &str) -> Result<bool> {
    let doc = xml::document(raw)?;
    match xml::value(doc.root_element(), "hasNewFirmware") {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        _ => anyhow::bail!("camera did not report a firmware offer"),
    }
}
fn progress(raw: &str) -> Result<Progress> {
    let doc = xml::document(raw)?;
    let root = doc.root_element();
    let state = xml::value(root, "state").context("missing firmware upgrade state")?;
    ensure!(
        matches!(
            state,
            "none" | "downloading" | "updating" | "stopped" | "timeout" | "finish" | "imgerror"
        ),
        "unknown firmware upgrade state"
    );
    let number = |name| xml::value(root, name).and_then(|s| s.parse::<u64>().ok());
    let percent = number("downloadSize")
        .zip(number("packetSize"))
        .filter(|(_, size)| *size > 0 && state == "downloading")
        .map(|(n, size)| (n.saturating_mul(100) / size).min(100) as u8);
    Ok(Progress {
        state: state.to_owned(),
        percent,
    })
}

async fn inspect(session: &mut Session, device: &Device, check: bool) -> Result<Firmware> {
    let raw = session.command(80, None, "").await?;
    let (model, hardware, installed) = {
        let doc = xml::document(&raw)?;
        let node = xml::find(doc.root_element(), "VersionInfo")
            .context("missing camera version information")?;
        let field = |tag| {
            xml::value(node, tag)
                .filter(|s| s.len() <= 160)
                .map(str::to_owned)
                .context("missing camera firmware identity")
        };
        (
            field("type").or_else(|_| field("name"))?,
            field("hardwareVersion")?,
            field("firmwareVersion")?,
        )
    };
    let identity = media_files::source(session, device.channel).await?;
    let source = format!(
        "{:x}",
        Sha256::digest(format!("{identity}\0{hardware}\0{installed}"))
    );
    let ext = format!(
        r#"<Extension version="1.1"><userName>{}</userName><token>system</token></Extension>"#,
        xml::escape(&device.username)
    );
    let can_install = (hardware.starts_with("IPC_") || hardware.starts_with("BIPC_"))
        && optional(session, 151, &ext)
            .await?
            .as_deref()
            .map(writable)
            .transpose()?
            .unwrap_or(false);
    let state = optional(session, 227, "")
        .await?
        .as_deref()
        .map(progress)
        .transpose()?;
    let update_available = if check {
        optional(session, 197, "")
            .await?
            .as_deref()
            .map(offer)
            .transpose()?
    } else {
        None
    };
    Ok(Firmware {
        model,
        hardware,
        installed,
        source,
        can_install,
        update_available,
        progress: state,
    })
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct Options {
    #[serde(default)]
    check: bool,
}

pub(super) async fn get(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    Query(opts): Query<Options>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Some(device) = api.devices.get(&id) else {
        return error(StatusCode::NOT_FOUND, "API_FIRMWARE_UNSUPPORTED", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    match device
        .read_retries
        .read("firmware.read", || async {
            let mut session = device.connect().await?;
            let result = inspect(&mut session, device, opts.check).await;
            session.logout().await;
            result
        })
        .await
    {
        Ok(view) => Json(view).into_response(),
        Err(err) => read_failure(
            &api,
            &cid,
            &id,
            "api::firmware::get",
            "API_FIRMWARE_READ_FAILED",
            err,
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    source: String,
    confirm: bool,
    file: Option<String>,
}

fn package_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    name.len() <= 180
        && !name.starts_with('.')
        && (lower.ends_with(".pak") || lower.ends_with(".paks"))
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" ._()-+".contains(&b))
}

pub(super) async fn install(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    Query(selection): Query<Selection>,
    body: Body,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let invalid = || error(StatusCode::BAD_REQUEST, "API_FIRMWARE_INVALID", &cid);
    if !selection.confirm
        || selection.source.len() != 64
        || !selection.source.bytes().all(|b| b.is_ascii_hexdigit())
        || selection.file.as_deref().is_some_and(|s| !package_name(s))
    {
        return invalid();
    }
    let Some(device) = api.devices.get(&id) else {
        return error(StatusCode::NOT_FOUND, "API_FIRMWARE_UNSUPPORTED", &cid);
    };
    let Ok(_guard) = api.network_writes[&id].try_lock() else {
        return error(StatusCode::CONFLICT, "API_READ_BUSY", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let local = selection.file.is_some();
    if local
        && headers.get("content-type").and_then(|s| s.to_str().ok())
            != Some("application/octet-stream")
    {
        return invalid();
    }
    let bytes = match timeout(
        Duration::from_secs(180),
        to_bytes(body, if local { MAX_PACKAGE } else { 0 }),
    )
    .await
    {
        Ok(Ok(data)) if !local || !data.is_empty() => data,
        _ => return invalid(),
    };
    let preparation = async {
        let mut session = device.connect().await?;
        let view = inspect(&mut session, device, !local).await?;
        Ok::<_, anyhow::Error>((session, view))
    }
    .await;
    let (mut session, view) = match preparation {
        Ok(value) => value,
        Err(err) => {
            return read_failure(
                &api,
                &cid,
                &id,
                "api::firmware::prepare",
                "API_FIRMWARE_READ_FAILED",
                err,
            );
        }
    };
    if selection.source != view.source {
        return error(StatusCode::CONFLICT, "API_CAMERA_IDENTITY_CHANGED", &cid);
    }
    if !view.can_install {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_FIRMWARE_UNSUPPORTED",
            &cid,
        );
    }
    if view
        .progress
        .as_ref()
        .is_some_and(|p| matches!(p.state.as_str(), "downloading" | "updating"))
    {
        return error(StatusCode::CONFLICT, "API_READ_BUSY", &cid);
    }
    let result = if let Some(name) = selection.file {
        timeout(
            Duration::from_secs(240),
            session.upload_firmware(&name, &bytes),
        )
        .await
        .map_err(anyhow::Error::from)
        .and_then(|v| v)
    } else {
        if view.update_available != Some(true) {
            return error(StatusCode::CONFLICT, "API_FIRMWARE_NO_OFFER", &cid);
        }
        session.start_firmware_update().await
    };
    match result {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({"outcome":"acknowledged","correlation_id":cid.0})),
        )
            .into_response(),
        Err(err) if err.is::<Rejected>() => {
            if let Some(hub) = &api.hub {
                hub.diagnostics.error(
                    "api::firmware::install",
                    "API_FIRMWARE_REJECTED",
                    Some(cid.0.clone()),
                    json!({"cameraId":id}),
                    Some(err),
                );
            }
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "API_FIRMWARE_REJECTED",
                &cid,
            )
        }
        Err(err) => read_failure(
            &api,
            &cid,
            &id,
            "api::firmware::install",
            "API_FIRMWARE_UNKNOWN",
            err,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offers_are_explicit() {
        assert!(!offer("<body><hasNewFirmware>0</hasNewFirmware></body>").unwrap());
        assert!(offer("<body/>").is_err());
    }
    #[test]
    fn package_and_permission_checks() {
        assert!(package_name("IPC_NT1NA45MP.4417.paks"));
        for name in ["../other.pak", "file.zip", "a/b.pak"] {
            assert!(!package_name(name))
        }
        assert!(writable("<AbilityInfo><system><subModule><abilityValue>version_ro, upgrade_rw</abilityValue></subModule></system></AbilityInfo>").unwrap());
        assert!(!writable("<AbilityInfo><disk><subModule><abilityValue>upgrade_rw</abilityValue></subModule></disk></AbilityInfo>").unwrap());
    }
}
