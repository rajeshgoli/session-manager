//! Owner device-key endpoints and shell SSH transport integration.
use super::*;
use crate::{device_ssh, studio_ssh::transport};

pub(super) fn keys(state: &AppState) -> device_ssh::Store {
    state.device_ssh_store.clone()
}

/// Caller holds plain_terminal_gate before ssh_transport; all lifecycle mutations share this order.
pub(super) fn reconcile_locked(state: &AppState, request_away: bool) -> anyhow::Result<()> {
    let cfg = &state.config.external_access.studio_ssh;
    let configured = expand_home(&cfg.assets_dir).join("sshd_config").exists();
    if !configured && !request_away && !state.studio_ssh_enabled.load(Ordering::SeqCst) {
        return Ok(());
    }
    {
        let _keys = state.ssh_key_gate.lock().unwrap_or_else(|p| p.into_inner());
        keys(state).project(&expand_home(&cfg.device_keys_file))?;
    }
    let mut runtime = state
        .ssh_transport
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if request_away {
        runtime.requested_at = Some(now);
    }
    let limits = plain_terminals::limits(state)?;
    let manual = state.studio_ssh_enabled.load(Ordering::SeqCst);
    let shells = limits.enabled && !plain_terminals::store(state).list(false)?.is_empty();
    // Kill switch removes the LAN listener even when the manual public override stays on.
    let lan = limits.enabled && (shells || manual);
    let tunnel = runtime.tunnel_needed(now, manual, shells, limits.tunnel_linger_minutes);
    let policy = (lan, tunnel);
    let repair = runtime.last_policy != Some(policy) || now - runtime.last_repair >= 30;
    runtime.save(cfg)?;
    drop(runtime);
    if repair {
        transport::apply(cfg, manual, lan, tunnel)?;
        let mut runtime = state
            .ssh_transport
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        runtime.last_policy = Some(policy);
        runtime.last_repair = now;
    }
    Ok(())
}

/// This pass never waits for launchctl or the shell lifecycle lock, including during startup.
pub(super) fn observe(state: &AppState) -> anyhow::Result<()> {
    let cfg = &state.config.external_access.studio_ssh;
    if !expand_home(&cfg.assets_dir).join("sshd_config").exists() {
        return Ok(());
    }
    let key_gate = state.ssh_key_gate.lock().unwrap_or_else(|p| p.into_inner());
    let mut runtime = state
        .ssh_transport
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let notices = runtime.read_log(cfg, now)?;
    keys(state).project(&expand_home(&cfg.device_keys_file))?;
    let key_list = keys(state).list()?;
    runtime.revoke(&key_list)?;
    runtime.save(cfg)?;
    drop(runtime);
    drop(key_gate);
    push_logins(state, &key_list, notices);
    Ok(())
}

fn push_logins(
    state: &AppState,
    key_list: &[device_ssh::Key],
    notices: Vec<transport::Connection>,
) {
    // Push work must never delay revocation or listener/tunnel policy.
    for login in notices {
        let comment = key_list
            .iter()
            .find(|k| k.fingerprint == login.fingerprint)
            .map(|k| {
                k.public_key
                    .splitn(3, ' ')
                    .nth(2)
                    .unwrap_or(&k.device_id)
                    .to_owned()
            })
            .unwrap_or(login.fingerprint);
        let Some(sender) = state.push_sender.clone() else {
            continue;
        };
        let push_store = follows::push_store(state);
        let owner = follows::follow_owner_id(&state.config, None);
        tokio::task::spawn_blocking(move || {
            let notice = crate::owner_push::Notification {
                kind: "ssh_tunnel_login".into(),
                title: "SSH login over tunnel".into(),
                body: format!("SSH login over tunnel · {comment}"),
                reader_path: None,
            };
            for token in push_store.valid_tokens(&owner).unwrap_or_default() {
                let _ = sender.send(&token.token, &notice.data(None));
            }
        });
    }
}

pub(super) fn reconcile(state: &AppState) -> anyhow::Result<()> {
    let _gate = state
        .plain_terminal_gate
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    reconcile_locked(state, false)
}

/// Public requests derive the device from its authenticated certificate identity, never a header.
pub(super) fn requesting_device(
    state: &AppState,
    request: &Request,
) -> Result<Option<String>, ApiError> {
    if is_request_local_bypass(state, request) {
        return Ok(header_text(request.headers(), "x-sm-device-key-id"));
    }
    let Some(application) = request_cloudflare_access_application(state, request) else {
        return Ok(None);
    };
    let assertion = header_text(request.headers(), "cf-access-jwt-assertion").ok_or_else(|| {
        ApiError::Status {
            status: StatusCode::UNAUTHORIZED,
            detail: "Device certificate is required".into(),
        }
    })?;
    let context = classify_cloudflare_access_assertion_cached(state, application, &assertion)
        .map_err(cloudflare_access_error)?;
    // Browser assertions carry the device's certificate name separately from the owner email.
    if application == CloudflareAccessApplication::Browser {
        return Ok(browser_device_common_name(&context));
    }
    ensure_cloudflare_access_context_allowed(state, &context)?;
    Ok(Some(context.identity))
}

fn browser_device_common_name(context: &CloudflareAccessContext) -> Option<String> {
    context.common_name.clone()
}

fn validate_device(state: &AppState, id: &str) -> Result<(), ApiError> {
    if id == "imported" {
        return Ok(());
    }
    for (user_id, user) in &state.config.mobile_terminal.allowed_users {
        if (user.owner || user.mobile_terminal_owner)
            && mobile_terminal_user_device_exists(state, user_id, id)
            && !mobile_terminal_device_revoked(state, user_id, id)?
        {
            return Ok(());
        }
    }
    Err(ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: "SSH keys require an active owner device".into(),
    })
}

pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    plain_terminals::guard(&state, &request)?;
    board::blocking(&state, move |state| {
        let keys: Vec<_> = keys(state)
            .list()?
            .into_iter()
            .filter(|k| k.device_id == id)
            .collect();
        let migration = if id == "imported" {
            transport::migration_candidates(&expand_home(
                &state
                    .config
                    .external_access
                    .studio_ssh
                    .legacy_authorized_keys_file,
            ))?
        } else {
            Vec::new()
        };
        Ok(Json(json!({"keys":keys,"import_candidates":migration})))
    })
    .await
}

pub(super) async fn add(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    plain_terminals::guard(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    let payload = plain_terminals::body(request).await?;
    board::blocking(&state, move |state| {
        let _gate = state.ssh_key_gate.lock().unwrap_or_else(|p| p.into_inner());
        plain_terminals::enabled(state)?;
        validate_device(state, &id)?;
        let public_keys = if id == "imported" && payload["import_authorized_keys"] == true {
            transport::migration_candidates(&expand_home(
                &state
                    .config
                    .external_access
                    .studio_ssh
                    .legacy_authorized_keys_file,
            ))?
        } else {
            vec![payload["public_key"]
                .as_str()
                .ok_or_else(|| ApiError::Status {
                    status: StatusCode::BAD_REQUEST,
                    detail: "public_key is required".into(),
                })?
                .to_owned()]
        };
        let mut added = Vec::new();
        for public_key in public_keys {
            added.push(
                keys(state)
                    .add(&id, &public_key)
                    .map_err(|e| ApiError::Status {
                        status: StatusCode::BAD_REQUEST,
                        detail: e.to_string(),
                    })?,
            );
        }
        keys(state).project(&expand_home(
            &state.config.external_access.studio_ssh.device_keys_file,
        ))?;
        Ok(Json(json!({"keys":added})))
    })
    .await
}

pub(super) fn revoke_device_locked(
    state: &AppState,
    id: &str,
    fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    keys(state).revoke(id, fingerprint)?;
    keys(state).project(&expand_home(
        &state.config.external_access.studio_ssh.device_keys_file,
    ))?;
    let cfg = &state.config.external_access.studio_ssh;
    let mut runtime = state
        .ssh_transport
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    // Consume pending accepted logins before teardown; the second-by-second pass catches races.
    let notices = runtime.read_log(cfg, OffsetDateTime::now_utc().unix_timestamp())?;
    let key_list = keys(state).list()?;
    runtime.revoke(&key_list)?;
    runtime.save(cfg)?;
    drop(runtime);
    push_logins(state, &key_list, notices);
    Ok(())
}
pub(super) async fn remove(
    State(state): State<Arc<AppState>>,
    Path((id, fingerprint)): Path<(String, String)>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    plain_terminals::guard(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    board::blocking(&state, move |state| {
        let _gate = state.ssh_key_gate.lock().unwrap_or_else(|p| p.into_inner());
        plain_terminals::enabled(state)?;
        revoke_device_locked(state, &id, Some(&fingerprint))?;
        Ok(Json(json!({"revoked":true})))
    })
    .await
}
