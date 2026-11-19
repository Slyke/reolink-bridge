use super::{
    credentials::{self, AccountEdit, UserAction},
    *,
};

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
        Ok(Ok((users, supported))) => Json(json!({"users":users,"writable":ready && supported,"manageAccounts":ready && supported,"requiresUid":!bound,"correlation_id":correlation.0})).into_response(),
        _ => error(StatusCode::BAD_GATEWAY, "API_CAMERA_USERS_UNAVAILABLE", &correlation),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    User,
    Administrator,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Operation {
    Add,
    Remove,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserRequest {
    id: Uuid,
    username: String,
    uid: Option<String>,
    confirmation: bool,
    action: Option<Operation>,
    role: Option<Role>,
    password: Option<String>,
}

pub(super) async fn change(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<UserRequest>,
) -> Response {
    edit(api, id, headers, correlation, body, true).await
}

pub(super) async fn manage(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(correlation): Extension<CorrelationId>,
    Json(body): Json<UserRequest>,
) -> Response {
    edit(api, id, headers, correlation, body, false).await
}

async fn edit(
    api: Arc<Api>,
    id: String,
    headers: HeaderMap,
    correlation: CorrelationId,
    body: UserRequest,
    password_only: bool,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &correlation);
    }
    let invalid = if password_only {
        "API_CAMERA_PASSWORD_INVALID"
    } else {
        "API_CAMERA_ACCOUNT_REJECTED"
    };
    let action = match (
        password_only,
        body.action,
        body.role,
        body.password.is_some(),
    ) {
        (true, None, None, true) => UserAction::Password,
        (false, Some(Operation::Add), Some(Role::User), true) => UserAction::AddUser,
        (false, Some(Operation::Add), Some(Role::Administrator), true) => {
            UserAction::AddAdministrator
        }
        (false, Some(Operation::Remove), None, false) => UserAction::Remove,
        _ => return error(StatusCode::UNPROCESSABLE_ENTITY, invalid, &correlation),
    };
    if !body.confirmation
        || body.id.get_version_num() != 4
        || body.username.trim().is_empty()
        || body.username.len() > 64
        || body.username.chars().any(char::is_control)
        || body
            .password
            .as_deref()
            .is_some_and(|p| !credentials::valid_new_password(p))
    {
        return error(StatusCode::UNPROCESSABLE_ENTITY, invalid, &correlation);
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
    // Key the fingerprint so the journal cannot be used to guess other accounts' passwords.
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ring::hmac;
    let key = hmac::Key::new(hmac::HMAC_SHA256, api.http_token.as_bytes());
    let input = serde_json::to_vec(&(body.id, &body.username, action, &body.password))
        .expect("serializable account intent");
    let edit = AccountEdit {
        id: body.id,
        username: &body.username,
        action,
        password: body.password.as_deref(),
        fingerprint: STANDARD.encode(hmac::sign(&key, &input).as_ref()),
    };
    let change = async {
        if action == UserAction::Password && body.username == operator.username {
            vault
                .rotate(stable, body.id, operator, body.password.clone())
                .await?;
            Ok(vault.rotation_progress(stable, Some(device)))
        } else {
            vault.edit_account(stable, &operator, &edit).await
        }
    };
    match timeout(Duration::from_secs(55), change).await {
        Ok(Ok(status)) => progress(status, None, &correlation),
        _ if vault.user_write_pending(stable, &edit) => {
            progress(Status::RecoveryRequired, None, &correlation)
        }
        Ok(Err(_)) if action != UserAction::Password => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "API_CAMERA_ACCOUNT_REJECTED",
            &correlation,
        ),
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
