//! Browser-only terminal tickets. The phone signature path remains separate.
use super::*;

pub(super) fn denied(detail: &str) -> ApiError {
    ApiError::Status {
        status: StatusCode::UNAUTHORIZED,
        detail: detail.to_owned(),
    }
}

/// Treat an upgrade as a write: it grants interactive control of a session.
pub(super) fn authorize(state: &AppState, request: &Request) -> Result<String, ApiError> {
    owner_web_guard(state, request.headers(), request_peer_addr(request), "POST")?
        .ok_or_else(|| denied("Browser Access login required"))
}

pub(super) async fn create_ticket(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let actor_email = authorize(&state, &request)?;
    if !mobile_terminal_enabled(&state) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Terminal attach is disabled".into(),
        });
    }
    ensure_mobile_terminal_ticket_runtime_enabled(&state)?;
    let session = resolve_session_or_registry_role(&state, &session_id)?
        .ok_or(ApiError::NotFound("Session not found"))?;
    if session.is_stopped() {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "Session is not running".into(),
        });
    }
    let attach = attach_descriptor_payload(session.clone());
    if attach["attach_supported"] != true {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: attach["message"]
                .as_str()
                .unwrap_or("Attach not supported")
                .to_owned(),
        });
    }
    let tmux_session = attach["tmux_session"].as_str().unwrap_or("").to_owned();
    let tmux_socket_name = attach["tmux_socket_name"].as_str().map(str::to_owned);
    validate_mobile_terminal_tmux_target(&tmux_session, tmux_socket_name.as_deref())?;
    let now = OffsetDateTime::now_utc();
    let expires_at = now + Duration::from_secs(60);
    let ticket_id = format!("att_{}", random_urlsafe_token(18));
    let ticket_secret = random_urlsafe_token(40);
    let ticket = MobileTerminalTicket {
        kind: TerminalTicketKind::Browser,
        ticket_id: ticket_id.clone(),
        secret_hash: mobile_terminal_secret_hash(&state.mobile_terminal_secret, &ticket_secret)?,
        user_id: mobile_terminal_visible_user(&state.config, &actor_email)
            .map(|(id, _)| id.to_owned())
            .unwrap_or_else(|| format!("browser:{actor_email}")),
        actor_email,
        session_id: session.id,
        provider: session.provider,
        node: session.node,
        tmux_session,
        tmux_socket_name,
        device_key_id: "browser".into(),
        created_at_unix: now.unix_timestamp(),
        expires_at_unix: expires_at.unix_timestamp(),
    };
    let mut tickets = state
        .mobile_terminal_tickets
        .lock()
        .map_err(|_| anyhow::anyhow!("Terminal ticket store unavailable"))?;
    cleanup_mobile_terminal_tickets(&mut tickets, now.unix_timestamp());
    // A retry replaces only this browser user's pending ticket for this session.
    tickets.retain(|_, t| {
        !(t.kind == TerminalTicketKind::Browser
            && t.actor_email == ticket.actor_email
            && t.session_id == ticket.session_id)
    });
    let active = state
        .mobile_terminal_active_attaches
        .lock()
        .map_err(|_| anyhow::anyhow!("Terminal attach store unavailable"))?;
    let pending = tickets
        .values()
        .map(|t| MobileTerminalActiveAttach {
            user_id: t.user_id.clone(),
            session_id: t.session_id.clone(),
            provider: t.provider.clone(),
            device_key_id: t.device_key_id.clone(),
            started_at_unix: t.created_at_unix,
            stop: Arc::new(AtomicBool::new(false)),
        })
        .collect::<Vec<_>>();
    enforce_mobile_terminal_active_limits(
        &state.config,
        active.values().chain(pending.iter()),
        &ticket,
    )?;
    ensure_mobile_terminal_ticket_runtime_enabled(&state)?;
    tickets.insert(ticket_id.clone(), ticket);
    // Relative to the authenticated browser origin, never the phone hostname.
    Ok(Json(json!({
        "ticket_id": ticket_id, "ticket_secret": ticket_secret,
        "ws_url": "/client/terminal", "expires_at": expires_at.format(&Rfc3339).unwrap(),
    })))
}
