//! Format one explicitly selected SD card on a standalone camera.
use super::{Api, error, media_files, recordings::read_failure};
use crate::{
    http::CorrelationId,
    reolink::{
        recordings::{self, Card},
        wire::{Device, Rejected, Session},
    },
    xml,
};
use anyhow::Result;
use axum::{
    Extension, Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc};

fn writable_disk(raw: &str) -> Result<bool> {
    let doc = xml::document(raw)?;
    let module = xml::find(doc.root_element(), "AbilityInfo")
        .and_then(|n| xml::child(n, "disk"))
        .and_then(|n| xml::child(n, "subModule"));
    Ok(module
        .and_then(|n| xml::value(n, "abilityValue"))
        .is_some_and(|value| {
            value
                .split(|c: char| c == ',' || c.is_ascii_whitespace())
                .any(|token| matches!(token, "hddInit_rw" | "format_rw"))
        }))
}

pub(super) async fn identify(
    session: &mut Session,
    device: &Device,
    cards: &mut [Card],
) -> Result<()> {
    let extension = format!(
        r#"<Extension version="1.1"><userName>{}</userName><token>disk</token></Extension>"#,
        xml::escape(&device.username)
    );
    let reply = session.exchange(151, 250, &extension, "", false).await?;
    if !writable_disk(&session.decode(&reply)?)? {
        return Ok(());
    }
    // Older standalone cameras omit storageType. Do not infer an SD slot on a recorder or a hub.
    let version = session.command(80, None, "").await?;
    let doc = xml::document(&version)?;
    if !xml::find(doc.root_element(), "VersionInfo")
        .and_then(|n| xml::value(n, "hardwareVersion"))
        .is_some_and(|hw| hw.to_ascii_uppercase().starts_with("IPC_"))
    {
        return Ok(());
    }
    let source = media_files::source(session, device.channel).await?;
    let mut numbers = BTreeSet::new();
    let duplicates: BTreeSet<_> = cards
        .iter()
        .filter_map(|card| (!numbers.insert(card.number)).then_some(card.number))
        .collect();
    let single_slot = cards.len() == 1;
    for card in cards {
        if card.explicit_sd_slot
            && (card.reported_storage_type || single_slot)
            && !duplicates.contains(&card.number)
            && card.total_bytes.is_some_and(|size| size > 0)
        {
            let binding = format!(
                "{source}:{}:{}",
                card.number,
                card.total_bytes.unwrap_or_default()
            );
            card.format_source = Some(format!("{:x}", Sha256::digest(binding)));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    slot: u32,
    source: String,
    confirm: bool,
}

pub(super) async fn format_card(
    State(api): State<Arc<Api>>,
    Path(camera): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    body: std::result::Result<Json<Selection>, JsonRejection>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Ok(Json(request)) = body else {
        return error(StatusCode::BAD_REQUEST, "API_STORAGE_FORMAT_INVALID", &cid);
    };
    if !request.confirm
        || request.source.len() != 64
        || !request.source.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return error(StatusCode::BAD_REQUEST, "API_STORAGE_FORMAT_INVALID", &cid);
    }
    let Some(device) = api.devices.get(&camera) else {
        return error(StatusCode::NOT_FOUND, "API_MEDIA_UNSUPPORTED", &cid);
    };
    let Ok(_guard) = api.network_writes[&camera].try_lock() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };

    let prepare = async {
        let mut session = device.connect().await?;
        let mut cards = recordings::storage(&mut session).await?;
        identify(&mut session, device, &mut cards).await?;
        Ok::<_, anyhow::Error>((session, cards))
    };
    let (mut session, cards) = match prepare.await {
        Ok(value) => value,
        Err(cause) => {
            return read_failure(
                &api,
                &cid,
                &camera,
                "api::storage_format::identify",
                "API_STORAGE_FORMAT_CHECK_FAILED",
                cause,
            );
        }
    };
    let selected = cards
        .iter()
        .find(|card| card.number == request.slot)
        .and_then(|card| card.format_source.as_ref());
    if selected != Some(&request.source) {
        session.logout().await;
        return if selected.is_some() {
            error(StatusCode::CONFLICT, "API_CAMERA_IDENTITY_CHANGED", &cid)
        } else {
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "API_STORAGE_FORMAT_UNSUPPORTED",
                &cid,
            )
        };
    }

    // The camera can reboot before replying. Never replay a write after losing its response.
    let outcome = session.format_sd_card(request.slot).await;
    session.logout().await;
    if let Err(cause) = &outcome
        && cause.is::<Rejected>()
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_STORAGE_FORMAT_REJECTED",
            &cid,
        );
    }
    let scope = api.recording_scope(&camera);
    api.recordings.forget_camera(&scope).await;
    api.photos.forget_camera(&scope).await;
    match outcome {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({"outcome":"acknowledged","slot":request.slot,"correlation_id":cid.0})),
        )
            .into_response(),
        Err(cause) => read_failure(
            &api,
            &cid,
            &camera,
            "api::storage_format::format",
            "API_STORAGE_FORMAT_UNKNOWN",
            cause,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn require_write_permission_in_the_disk_module() {
        for (module, flags, allowed) in [
            ("disk", "hddCfg_ro, hddInit_rw", true),
            ("disk", "format_rw", true),
            ("disk", "hddInit_ro, hddCfg_rw", false),
            ("system", "format_rw", false),
        ] {
            let raw = format!(
                "<AbilityInfo><{module}><subModule><abilityValue>{flags}</abilityValue></subModule></{module}></AbilityInfo>"
            );
            assert_eq!(writable_disk(&raw).unwrap(), allowed);
        }
    }
}
