//! Owner shell endpoints and the minute lifecycle pass. All tmux/ps work is blocking.
use super::*;
use crate::plain_terminals::{Limits, Store, Terminal};

pub(super) fn store(state: &AppState) -> Store {
    state.plain_terminal_store.clone()
}
pub(super) fn limits(state: &AppState) -> anyhow::Result<Limits> {
    let settings = state.session_store.owner_settings()?;
    let v = &settings["terminal_limits"];
    let mut limits = state.config.terminal_limits;
    if let Some(on) = v["plain_terminal_enabled"].as_bool() {
        limits.enabled = on;
    }
    if let Some(n) = v["max_plain_terminals"].as_u64() {
        limits.max_plain_terminals = n as usize;
    }
    if let Some(n) = v["idle_reap_minutes"].as_i64() {
        limits.idle_reap_minutes = n;
    }
    if let Some(n) = v["tunnel_linger_minutes"].as_i64() {
        limits.tunnel_linger_minutes = n;
    }
    Ok(limits.clamped())
}
fn denial(code: &str) -> ApiError {
    eprintln!("plain terminal denied: reason={code}");
    ApiError::Status {
        status: StatusCode::FORBIDDEN,
        detail: code.into(),
    }
}
pub(super) fn enabled(state: &AppState) -> Result<(), ApiError> {
    if !limits(state)?.enabled {
        return Err(denial("plain_disabled"));
    }
    Ok(())
}
pub(super) fn guard(state: &AppState, request: &Request) -> Result<String, ApiError> {
    enabled(state)?;
    if [
        handoff::SESSION_HEADER,
        "x-sm-session-id",
        "x-claude-session-id",
    ]
    .iter()
    .any(|h| request.headers().contains_key(*h))
        || crate::local_identity::current().is_some()
    {
        return Err(denial("not_owner"));
    }
    let peer = request_peer_addr(request).ok_or_else(|| denial("not_owner"))?;
    let owner = follows::owner_web_or_guard(
        state,
        request.headers(),
        peer,
        request.method().as_str(),
        request.uri(),
    )?;
    if let Some((_, user)) = request_actor_email(&state.config, request)
        .as_deref()
        .and_then(|email| mobile_terminal_visible_user(&state.config, email))
    {
        if !(user.owner || user.mobile_terminal_owner) {
            return Err(denial("not_owner"));
        }
    }
    Ok(owner)
}
pub(super) async fn body(request: Request) -> Result<Value, ApiError> {
    let bytes = to_bytes(request.into_body(), 16 * 1024)
        .await
        .map_err(|_| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Invalid terminal request".into(),
        })?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: "Invalid terminal JSON".into(),
    })
}

pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &request)?;
    Ok(Json(json!({"terminals":store(&state).list(false)?})))
}

pub(super) async fn create(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let owner = guard(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    if !state.config.rust_core.runtime_enabled {
        return Err(ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "Shell runtime is disabled".into(),
        });
    }
    let device = header_text(request.headers(), "x-sm-device-key-id");
    let via = if device.is_some() {
        "mobile"
    } else if request.headers().contains_key("origin") {
        "web"
    } else {
        "cli"
    };
    let payload = body(request).await?;
    board::blocking(&state, move |state| {
        create_locked(state, &payload, owner, device, via)
    })
    .await
}

fn create_locked(
    state: &AppState,
    payload: &Value,
    owner: String,
    device: Option<String>,
    via: &str,
) -> Result<Response, ApiError> {
    let _gate = state
        .plain_terminal_gate
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    enabled(state)?;
    let store = store(state);
    let open = store.list(false)?;
    if open.len() >= limits(state)?.max_plain_terminals {
        eprintln!("plain terminal denied: reason=too_many_terminals");
        return Ok((StatusCode::CONFLICT, Json(json!({"error":"too_many_terminals","detail":"too_many_terminals","terminals":open}))).into_response());
    }
    let cwd = match payload.get("cwd") {
        Some(Value::String(cwd)) if !cwd.trim().is_empty() => expand_home(cwd),
        None | Some(Value::Null) => {
            let id = payload["from_session"]
                .as_str()
                .ok_or_else(|| ApiError::Status {
                    status: StatusCode::BAD_REQUEST,
                    detail: "cwd or from_session required".into(),
                })?;
            if let Some(t) = store.get(id)?.filter(|t| t.state == "live") {
                PathBuf::from(t.cwd)
            } else {
                PathBuf::from(
                    resolve_session_or_registry_role(state, id)?
                        .ok_or(ApiError::NotFound("Session not found"))?
                        .working_dir,
                )
            }
        }
        _ => {
            return Err(ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: "cwd must be a path".into(),
            })
        }
    };
    let cwd = cwd
        .canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "cwd must be an existing directory".into(),
        })?
        .to_string_lossy()
        .into_owned();
    let id = loop {
        let n = random_urlsafe_token(16);
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(n.as_bytes());
        let id = format!("term-{:02x}{:02x}", hash[0], hash[1]);
        if store.get(&id)?.is_none() {
            break id;
        }
    };
    let t = Terminal {
        id: id.clone(),
        cwd,
        name: payload["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        created_by_user: owner,
        created_by_device: device,
        created_via: via.into(),
        tmux_session: id,
        state: "live".into(),
        last_attach_at: None,
        last_input_at: None,
        busy_since: None,
        created_at: crate::plain_terminals::timestamp(),
        retired_at: None,
        retired_reason: None,
    };
    store.insert(&t)?;
    let log = expand_home(&state.config.paths.state_file)
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("logs")
        .join(format!("{}.log", t.id));
    let spec = crate::runtime::TmuxSessionSpec {
        session_id: t.id.clone(),
        session_credential: None,
        tmux_session: t.tmux_session.clone(),
        working_dir: t.cwd.clone(),
        log_file: log,
        provider: "shell".into(),
        initial_message: None,
        force_initial_prompt_stdin: false,
        claude_session_id: None,
        model: None,
        reasoning_effort: None,
    };
    if let Err(e) = state.runtime().create_session(&spec) {
        store.retire(&t.id, "tmux_gone")?;
        return Err(e.into());
    }
    if let Err(e) = ssh_transport::reconcile_locked(state, false) {
        eprintln!("shell SSH listener: {e:#}");
    }
    let push_state = state.clone();
    let notice = t.clone();
    // Push delivery never delays creation or holds the lifecycle lock.
    tokio::task::spawn_blocking(move || {
        if let Err(e) = push_opened(&push_state, &notice) {
            eprintln!("shell opened push: {e:#}");
        }
    });
    Ok((StatusCode::CREATED,Json(json!({"id":t.id,"tmux_session":t.tmux_session,"cwd":t.cwd,"tmux_socket_name":state.runtime().socket_name()}))).into_response())
}

pub(super) async fn retire(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    board::blocking(&state, move |state| {
        let _gate = state
            .plain_terminal_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        enabled(state)?;
        let t = store(state)
            .get(&id)?
            .ok_or(ApiError::NotFound("Terminal not found"))?;
        retire_locked(state, &t, "owner")?;
        ssh_transport::reconcile_locked(state, false)?;
        Ok(Json(json!({"id":id,"state":"retired"})))
    })
    .await
}

pub(super) async fn route(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &request)?;
    let device = ssh_transport::requesting_device(&state, &request)?;
    let payload = body(request).await?;
    let away = match payload["where"].as_str() {
        Some("away") => true,
        Some("home") => false,
        _ => {
            return Err(ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: "where must be home or away".into(),
            })
        }
    };
    let work_state = state.clone();
    let task = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let state = work_state;
        let _gate = state
            .plain_terminal_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        enabled(&state)?;
        let t = store(&state)
            .get(&id)?
            .filter(|t| t.state == "live")
            .ok_or(ApiError::NotFound("Terminal not found"))?;
        if away {
            let has_key = ssh_transport::keys(&state)
                .list()?
                .iter()
                .any(|k| Some(&k.device_id) == device.as_ref() && k.revoked_at.is_none());
            if !has_key {
                return Err(ApiError::Status {
                    status: StatusCode::CONFLICT,
                    detail: "no_ssh_key".into(),
                });
            }
            ssh_transport::reconcile_locked(&state, true).map_err(|e| ApiError::Status {
                status: StatusCode::SERVICE_UNAVAILABLE,
                detail: format!("away_unavailable: {e:#}"),
            })?;
        }
        let user = state
            .config
            .external_access
            .ssh_username
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .ok_or_else(|| ApiError::Status {
                status: StatusCode::SERVICE_UNAVAILABLE,
                detail: "SSH username is not configured".into(),
            })?;
        let host = if away {
            "studio-away"
        } else {
            "studio-lan.rajeshgo.li"
        };
        let port = state.config.external_access.studio_ssh.local_sshd_port;
        let target = if away {
            host.into()
        } else {
            format!("{user}@{host}")
        };
        let port_flag = if away {
            String::new()
        } else {
            format!(" -p {port}")
        };
        // Shells live on SM's tmux socket. Preserve that target on remote attach too.
        let runtime = state.runtime();
        let socket = runtime
            .socket_name()
            .map(|name| format!(" -L '{}'", name.replace('\'', "'\"'\"'")))
            .unwrap_or_default();
        Ok(
            json!({"ssh_host":host,"ssh_port":port,"ssh_user":user,"tmux_session":t.tmux_session,
            "tmux_socket_name":runtime.socket_name(),
            "lan_probe_url":format!("https://{}:{}/client/terminal/probe", state.config.terminal_direct.lan.hostname, state.config.terminal_direct.lan.port),
            "server_instance":state.server_instance,
            "command":format!("ssh -t{port_flag} {target} tmux{socket} attach-session -t {}",t.tmux_session)}),
        )
    });
    let ready = async {
        let response = task
            .await
            .map_err(|e| ApiError::from(anyhow::anyhow!(e)))??;
        if away {
            loop {
                let cfg = state.config.external_access.studio_ssh.clone();
                let status = tokio::task::spawn_blocking(move || studio_ssh::status(&cfg))
                    .await
                    .map_err(|e| ApiError::from(anyhow::anyhow!(e)))?;
                if status.sshd_listening && status.tunnel_running {
                    break;
                }
                enabled(&state)?;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        Ok::<_, ApiError>(Json(response))
    };
    tokio::time::timeout(Duration::from_secs(20), ready)
        .await
        .map_err(|_| ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "away_unavailable".into(),
        })?
}

fn push_opened(state: &AppState, t: &Terminal) -> anyhow::Result<()> {
    let Some(sender) = &state.push_sender else {
        return Ok(());
    };
    let store = follows::push_store(state);
    let notice = crate::owner_push::Notification {
        kind: "shell_opened".into(),
        title: "Shell opened".into(),
        body: format!(
            "Shell opened · {} · from {}",
            t.cwd,
            t.created_by_device.as_deref().unwrap_or("CLI")
        ),
        reader_path: Some(format!("/terminal/{}", t.id)),
    };
    let data = notice.data(None);
    for token in store.valid_tokens(&t.created_by_user)? {
        if let Err(error) = sender.send(&token.token, &data) {
            let invalid = matches!(error, crate::owner_push::PushError::InvalidToken(_))
                .then_some(OffsetDateTime::now_utc());
            store.record_token_error(&token, &error.to_string(), invalid)?;
        }
    }
    Ok(())
}

fn retire_locked(state: &AppState, t: &Terminal, reason: &str) -> anyhow::Result<()> {
    if t.state != "live" {
        return Ok(());
    }
    crate::plain_terminals::retire_runtime(&state.runtime(), t)?;
    store(state).retire(&t.id, reason)?;
    if let Ok(active) = state.mobile_terminal_active_attaches.lock() {
        for attach in active.values().filter(|a| a.session_id == t.id) {
            attach.stop.store(true, Ordering::SeqCst);
        }
    }
    Ok(())
}
pub(super) fn retire_all_locked(state: &AppState) -> anyhow::Result<()> {
    for t in store(state).list(false)? {
        retire_locked(state, &t, "kill_switch")?;
    }
    ssh_transport::reconcile_locked(state, false)?;
    Ok(())
}

pub(super) fn reconcile(state: &AppState) -> anyhow::Result<()> {
    reconcile_at(state, OffsetDateTime::now_utc())
}

pub(super) fn reconcile_at(state: &AppState, now: OffsetDateTime) -> anyhow::Result<()> {
    if !state.config.rust_core.runtime_enabled {
        return Ok(());
    }
    let _gate = state
        .plain_terminal_gate
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if !limits(state)?.enabled {
        return retire_all_locked(state);
    }
    let store = store(state);
    let runtime = state.runtime();
    for t in store.list(false)? {
        let exists = match crate::plain_terminals::exists(&runtime, &t) {
            Ok(exists) => exists,
            Err(error) => {
                eprintln!("shell {} presence inconclusive: {error:#}", t.id);
                continue;
            }
        };
        if !exists {
            store.retire(&t.id, "tmux_gone")?;
            continue;
        }
        match crate::plain_terminals::observe(&runtime, &t) {
            Ok(o) => {
                store.observe(&t.id, o.busy, o.attached)?;
                if let Some(at) = &o.last_attach_at {
                    store.attached_at(&t.id, at)?;
                }
                if let Some(at) = &o.last_input_at {
                    store.input_at(&t.id, at)?;
                }
                state
                    .plain_terminal_commands
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(t.id.clone(), o.command.clone());
                let current = store.get(&t.id)?.expect("existing terminal");
                if o.idle_expired(now, limits(state)?.idle_reap_minutes)
                    && current.idle_expired(now, limits(state)?.idle_reap_minutes)
                {
                    // Recheck immediately before teardown; a build or attach may have started.
                    if let Ok(fresh) = crate::plain_terminals::observe(&runtime, &t) {
                        if fresh.idle_expired(now, limits(state)?.idle_reap_minutes) {
                            retire_locked(state, &t, "idle")?;
                        }
                    }
                }
            }
            Err(error) => eprintln!("shell {} inspection inconclusive: {error:#}", t.id),
        }
    }
    ssh_transport::reconcile_locked(state, false)?;
    Ok(())
}

pub(super) fn browser_session(
    state: &AppState,
    id: &str,
) -> Result<Option<SessionRecord>, ApiError> {
    let Some(t) = store(state).get(id)? else {
        return resolve_session_or_registry_role(state, id);
    };
    enabled(state)?;
    Ok(Some(serde_json::from_value(json!({
        "id":t.id,"name":t.id,"working_dir":t.cwd,"tmux_session":t.tmux_session,
        "tmux_socket_name":state.runtime().socket_name(),"node":state.config.nodes.default_node,
        "provider":"shell","status":if t.state=="live" {"running"} else {"stopped"},
        "created_at":t.created_at,"last_activity":t.last_input_at.unwrap_or(t.created_at)
    }))?))
}

pub(super) fn relay_allowed(
    state: &AppState,
    ticket: &MobileTerminalTicket,
    home: bool,
) -> Result<(), ApiError> {
    if ticket.provider != "shell" {
        return Ok(());
    }
    enabled(state)?;
    if !home || ticket.kind != TerminalTicketKind::Browser {
        return Err(denial("shell_requires_home_route"));
    }
    let t = store(state)
        .get(&ticket.session_id)?
        .filter(|t| t.state == "live")
        .ok_or(ApiError::NotFound("Terminal not found"))?;
    if t.tmux_session != ticket.tmux_session {
        return Err(denial("shell_target_changed"));
    }
    Ok(())
}
