//! Browser-only terminal tickets. The phone signature path remains separate.
use super::*;

/// Upload uses the same owner login and same-origin write check as minting a
/// terminal ticket. No caller-controlled filename or filesystem path is used.
pub(super) async fn upload_image(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &request)?;
    if !mobile_terminal_enabled(&state) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Terminal attach is disabled".into(),
        });
    }
    let session = resolve_session_or_registry_role(&state, &session_id)?
        .ok_or(ApiError::NotFound("Session not found"))?;
    if session.is_stopped() {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "Session is not running".into(),
        });
    }
    if attach_descriptor_payload(session.clone())["attach_supported"] != true {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Terminal attach is not supported".into(),
        });
    }
    // Bound the encoded request before JSON parsing or base64 allocation.
    let body = to_bytes(request.into_body(), 14 * 1024 * 1024)
        .await
        .map_err(|_| ApiError::Status {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            detail: "Image exceeds the 10 MiB limit".into(),
        })?;
    #[derive(Deserialize)]
    struct Image {
        mime: String,
        data: String,
    }
    let image: Image = serde_json::from_slice(&body).map_err(|_| ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: "Invalid image upload".into(),
    })?;
    let path = board::blocking(&state, move |state| {
        let bytes = STANDARD.decode(&image.data).map_err(|_| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "Invalid image encoding".into(),
        })?;
        state
            .session_store
            .save_terminal_image(&session.id, &image.mime, &bytes)
            .map_err(|error| ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: error.to_string(),
            })
    })
    .await?;
    Ok(Json(json!({"path": path})))
}

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

#[derive(Clone, Copy)]
pub(super) enum DirectRoute {
    Localhost,
    Lan,
}

impl DirectRoute {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Localhost => "localhost",
            Self::Lan => "lan",
        }
    }
}

pub(super) fn direct_route(state: &AppState, request: &Request) -> Option<DirectRoute> {
    let browser_host = state.config.cloudflare_access.browser.hostname.as_deref()?;
    let origin = header_text(request.headers(), "origin")?;
    if origin != format!("https://{browser_host}") {
        return None;
    }
    let host = header_text(request.headers(), "host")?;
    let localhost = ["localhost", "127.0.0.1"]
        .iter()
        .any(|name| host.eq_ignore_ascii_case(&format!("{name}:{}", state.listen_port)));
    if localhost && request_peer_addr(request).is_some_and(|peer| peer.ip().is_loopback()) {
        return Some(DirectRoute::Localhost);
    }
    let lan = &state.config.terminal_direct.lan;
    if lan.enabled
        && request.extensions().get::<DirectTerminalLan>().is_some()
        && host.eq_ignore_ascii_case(&format!("{}:{}", lan.hostname, lan.port))
    {
        return Some(DirectRoute::Lan);
    }
    None
}

pub(super) fn is_direct_host(state: &AppState, request: &Request) -> bool {
    if matches!(
        request_hostname(request.headers()).as_deref(),
        Some("localhost" | "127.0.0.1")
    ) {
        return true;
    }
    let lan = &state.config.terminal_direct.lan;
    lan.enabled
        && request.extensions().get::<DirectTerminalLan>().is_some()
        && request_hostname(request.headers()).as_deref() == Some(lan.hostname.as_str())
}

pub(super) fn direct_ticket_email(
    state: &AppState,
    frame: &MobileTerminalAuthFrame,
) -> Result<String, ApiError> {
    let tickets = state
        .mobile_terminal_tickets
        .lock()
        .map_err(|_| anyhow::anyhow!("Terminal ticket store unavailable"))?;
    let ticket = tickets
        .get(frame.ticket_id.as_deref().unwrap_or(""))
        .filter(|ticket| ticket.kind == TerminalTicketKind::Browser)
        .ok_or_else(|| denied("Browser ticket required for direct attach"))?;
    Ok(ticket.actor_email.clone())
}

pub(super) async fn probe(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if let Err(error) = super::ensure_identity_on_public_host(&state, &request) {
        return error.into_response();
    }
    let origin = state
        .config
        .cloudflare_access
        .browser
        .hostname
        .as_deref()
        .map(|host| format!("https://{host}"))
        .unwrap_or_default();
    (
        [
            ("access-control-allow-origin", origin),
            ("vary", "Origin".to_owned()),
            ("cache-control", "no-store".to_owned()),
        ],
        Json(json!({"instance": state.server_instance})),
    )
        .into_response()
}

pub(super) async fn probe_options(State(state): State<Arc<AppState>>) -> Response {
    let origin = state
        .config
        .cloudflare_access
        .browser
        .hostname
        .as_deref()
        .map(|host| format!("https://{host}"))
        .unwrap_or_default();
    (
        StatusCode::NO_CONTENT,
        [
            ("access-control-allow-origin", origin),
            ("access-control-allow-private-network", "true".to_owned()),
            ("access-control-allow-methods", "GET".to_owned()),
            ("vary", "Origin".to_owned()),
        ],
    )
        .into_response()
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
    let session = plain_terminals::browser_session(&state, &session_id)?
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
    let expires_at = now + Duration::from_secs(30);
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
    enforce_mobile_terminal_active_limits(&state, active.values().chain(pending.iter()), &ticket)?;
    ensure_mobile_terminal_ticket_runtime_enabled(&state)?;
    tickets.insert(ticket_id.clone(), ticket);
    let localhost = format!("localhost:{}", state.listen_port);
    let mut direct = vec![json!({
        "url": format!("ws://{localhost}/client/terminal"),
        "probe": format!("http://{localhost}/client/terminal/probe"),
    })];
    let lan = &state.config.terminal_direct.lan;
    if lan.enabled {
        let host = format!("{}:{}", lan.hostname, lan.port);
        direct.push(json!({
            "url": format!("wss://{host}/client/terminal"),
            "probe": format!("https://{host}/client/terminal/probe"),
        }));
    }
    // Relative to the authenticated browser origin, never the phone hostname.
    Ok(Json(json!({
        "ticket_id": ticket_id, "ticket_secret": ticket_secret,
        "ws_url": "/client/terminal", "expires_at": expires_at.format(&Rfc3339).unwrap(),
        "server_instance": state.server_instance, "direct": direct,
    })))
}
