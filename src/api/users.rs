use super::{credentials, *};

pub(super) async fn list(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let Some(device) = api.devices.get(&id) else {
        return error(
            StatusCode::NOT_FOUND,
            "API_ROTATION_UNSUPPORTED",
            &correlation,
        );
    };
    let Some(stable) = api.vault_key(&id) else {
        return error(
            StatusCode::NOT_FOUND,
            "API_ROTATION_UNSUPPORTED",
            &correlation,
        );
    };
    let (ready, bound) = if let Some(vault) = &api.vault {
        let Ok(vault) = vault.try_lock() else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "API_VAULT_BUSY",
                &correlation,
            );
        };
        let status = vault.rotation_progress(stable, Some(device));
        let ready = !matches!(
            status,
            Status::RecoveryRequired | Status::InProgress | Status::RestartRequired
        );
        (
            ready,
            vault.credential(stable).is_some() || api.config.rotation_ids.contains_key(&id),
        )
    } else {
        (false, false)
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API_READ_BUSY",
            &correlation,
        );
    };
    match timeout(Duration::from_secs(18), credentials::list_users(device)).await {
        Ok(Ok((users, supported))) => Json(json!({"users":users,"writable":ready && supported,"requiresUid":!bound,"correlation_id":correlation.0})).into_response(),
        _ => error(StatusCode::BAD_GATEWAY, "API_CAMERA_USERS_UNAVAILABLE", &correlation),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PasswordChange {
    id: Uuid,
    username: String,
    password: String,
    uid: Option<String>,
    confirmation: bool,
}

pub(super) async fn change(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<PasswordChange>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    if !body.confirmation
        || body.id.get_version_num() != 4
        || body.username.is_empty()
        || body.username.len() > 64
        || body.username.chars().any(char::is_control)
        || !(8..=31).contains(&body.password.len())
        || !body.password.bytes().all(|b| (0x20..=0x7e).contains(&b))
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_CAMERA_PASSWORD_INVALID",
            &correlation,
        );
    }
    let (Some(stable), Some(device)) = (api.vault_key(&id), api.devices.get(&id)) else {
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
    if matches!(
        vault.rotation_progress(stable, Some(device)),
        Status::RestartRequired | Status::InProgress | Status::RecoveryRequired
    ) {
        return error(StatusCode::CONFLICT, "API_VAULT_BUSY", &correlation);
    }
    let identifiers = api
        .config
        .rotation_ids
        .get(&id)
        .cloned()
        .unwrap_or(Identifiers {
            uid: body.uid,
            serial: None,
        });
    let operator = vault.credential(stable).unwrap_or_else(|| Credential {
        id: stable.into(),
        host: device.host.clone(),
        port: device.port,
        channel: device.channel,
        username: device.username.clone(),
        password: device.password.clone(),
        identifiers,
        enrolled: false,
    });
    if !operator.identifiers.valid() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ROTATION_UNSUPPORTED",
            &correlation,
        );
    }
    let change = async {
        if body.username == operator.username {
            vault
                .rotate(stable, body.id, operator, Some(body.password))
                .await?;
            Ok(vault.rotation_progress(stable, Some(device)))
        } else {
            vault
                .change_other_user(stable, body.id, &operator, &body.username, body.password)
                .await
        }
    };
    match timeout(Duration::from_secs(55), change).await {
        Ok(Ok(status)) => progress(status, None, &correlation),
        Ok(Err(_)) if vault.user_write_pending(stable, body.id) => {
            progress(Status::RecoveryRequired, None, &correlation)
        }
        Ok(Err(_)) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_ROTATION_UNSUPPORTED",
            &correlation,
        ),
        Err(_) => error(
            StatusCode::GATEWAY_TIMEOUT,
            "API_ROTATION_RECONCILE_FAILED",
            &correlation,
        ),
    }
}
