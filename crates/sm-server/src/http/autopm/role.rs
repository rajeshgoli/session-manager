//! Authentication for the Auto PM role; the registry remains the single holder authority.
use super::*;

fn forbidden() -> ApiError {
    ApiError::Status {
        status: StatusCode::FORBIDDEN,
        detail: "requires the authenticated live autopm role holder".into(),
    }
}

/// Both CLI identity headers are accepted, but conflicting identities never are.
fn identity(headers: &HeaderMap) -> Result<String, ApiError> {
    let session = header_text(headers, "x-sm-session");
    let legacy = header_text(headers, "x-sm-session-id");
    if session
        .as_ref()
        .zip(legacy.as_ref())
        .is_some_and(|(a, b)| a != b)
    {
        return Err(forbidden());
    }
    session
        .or(legacy)
        .filter(|s| !s.is_empty())
        .ok_or_else(forbidden)
}

pub(super) fn credential(
    state: &AppState,
    headers: &HeaderMap,
    expected: &str,
) -> Result<(), ApiError> {
    if identity(headers)? != expected {
        return Err(forbidden());
    }
    let credential = header_text(headers, "x-sm-session-credential").ok_or_else(forbidden)?;
    if !state
        .session_store
        .session_credential_matches(expected, &credential)?
    {
        return Err(forbidden());
    }
    Ok(())
}

/// Register and unregister are self-directed and authenticated for the reserved role.
pub(in crate::http) fn registry_guard(
    state: &AppState,
    headers: &HeaderMap,
    session: &str,
    role: &str,
) -> Result<(), ApiError> {
    if crate::sessions::normalize_role(role) == autopm::ROLE {
        credential(state, headers, session)?;
    }
    Ok(())
}

pub(in crate::http) fn actor(state: &AppState, request: &Request) -> Result<Actor, ApiError> {
    if !owner_events::from_agent(request.headers()) {
        owner_only(state, request)?;
        return Ok(Actor::Owner);
    }
    ensure_session_allowed_from_parts(
        &state.config,
        request.headers(),
        request_peer_addr(request),
        request.uri().path(),
    )?;
    let session = identity(request.headers())?;
    credential(state, request.headers(), &session)?;
    if !state.session_store.is_authorized_autopm_seat(&session)?
        || state
            .session_store
            .lookup_agent_registration(autopm::ROLE)?
            .is_none_or(|holder| holder.session_id != session)
    {
        return Err(forbidden());
    }
    Ok(Actor::Autopm)
}
