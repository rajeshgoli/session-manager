//! Owner controls for context handoff (sm#1651, Appendix I.1):
//! `GET/PUT /handoff-defaults` and `GET/PUT /sessions/{id}/handoff-policy`.

use super::*;
use crate::handoff::policy::PolicyUpdate;
use crate::sessions::{HandoffPolicyOutcome, ReviewAsk};

/// Header the `sm` client sends from inside a managed session.
pub(super) const SESSION_HEADER: &str = "x-sm-session";
const OWNER_ONLY: &str = "handoff policy is owner-only";

fn status(status: StatusCode, detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status,
        detail: detail.into(),
    }
}

/// Owner only: the usual session gate, then refuse anything that identifies
/// itself as an agent, then require a browser's `Origin` to match the host.
fn ensure_owner(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    path: &str,
    body: Option<&Value>,
) -> Result<(), ApiError> {
    ensure_session_allowed_from_parts(&state.config, headers, Some(peer_addr), path)?;
    let from_agent = header_text(headers, SESSION_HEADER).is_some()
        || body.is_some_and(|body| body.get("requester_session_id").is_some());
    if from_agent {
        return Err(status(StatusCode::FORBIDDEN, OWNER_ONLY));
    }
    if let Some(origin) = header_text(headers, "origin") {
        let host =
            header_text(headers, "x-forwarded-host").or_else(|| header_text(headers, "host"));
        if !origin_matches_host(&origin, host.as_deref()) {
            return Err(status(StatusCode::FORBIDDEN, "Origin does not match host"));
        }
    }
    Ok(())
}

/// `https://sm.example.com` matches host `sm.example.com`. Non-browser
/// clients (the app, sm watch) send no Origin and skip this check.
fn origin_matches_host(origin: &str, host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Some((_, authority)) = origin.split_once("://") else {
        return false;
    };
    authority
        .trim_end_matches('/')
        .eq_ignore_ascii_case(host.trim())
}

/// Appendix C.3: a successful review request may ask its requester to hand
/// off. The ask rides in the response for the CLI to print; a failure to
/// evaluate it never fails the review request.
pub(super) fn add_review_handoff_ask(
    state: &AppState,
    requester_session_id: Option<&str>,
    ask: ReviewAsk<'_>,
    response: &mut Value,
) {
    let Some(requester) = requester_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    match state.session_store.review_handoff_ask(requester, ask) {
        Ok(Some(text)) => response["handoff_ask"] = json!(text),
        Ok(None) => {}
        Err(error) => eprintln!("handoff ask for {requester} failed: {error:#}"),
    }
}

pub(super) async fn get_handoff_defaults(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(&state, &headers, peer_addr, "/handoff-defaults", None)?;
    Ok(Json(state.session_store.handoff_defaults()?.to_json()))
}

pub(super) async fn put_handoff_defaults(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(
        &state,
        &headers,
        peer_addr,
        "/handoff-defaults",
        Some(&body),
    )?;
    ensure_core_writes_enabled(&state)?;
    match state.session_store.update_handoff_defaults(&body)? {
        Ok(defaults) => Ok(Json(defaults.to_json())),
        Err(detail) => Err(status(StatusCode::BAD_REQUEST, detail)),
    }
}

pub(super) async fn get_handoff_policy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(
        &state,
        &headers,
        peer_addr,
        "/sessions/handoff-policy",
        None,
    )?;
    state
        .session_store
        .handoff_policy_view(&session_id)?
        .map(Json)
        .ok_or(ApiError::NotFound("Session not found"))
}

pub(super) async fn put_handoff_policy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    ensure_owner(
        &state,
        &headers,
        peer_addr,
        "/sessions/handoff-policy",
        Some(&body),
    )?;
    ensure_core_writes_enabled(&state)?;
    let update =
        PolicyUpdate::parse(&body).map_err(|detail| status(StatusCode::BAD_REQUEST, detail))?;
    match state.session_store.update_handoff_policy(
        &session_id,
        &update,
        &state.config.owner_name,
    )? {
        HandoffPolicyOutcome::Updated(view) => Ok(Json(view)),
        HandoffPolicyOutcome::NotFound => Err(ApiError::NotFound("Session not found")),
        HandoffPolicyOutcome::Conflict(detail) => Err(status(StatusCode::CONFLICT, detail)),
    }
}

#[cfg(test)]
mod tests {
    use super::origin_matches_host;

    #[test]
    fn origin_must_match_host() {
        assert!(origin_matches_host(
            "https://sm.rajeshgo.li",
            Some("sm.rajeshgo.li")
        ));
        assert!(origin_matches_host(
            "http://localhost:8420",
            Some("localhost:8420")
        ));
        assert!(!origin_matches_host(
            "https://evil.example",
            Some("sm.rajeshgo.li")
        ));
        assert!(!origin_matches_host("null", Some("sm.rajeshgo.li")));
        assert!(!origin_matches_host("https://sm.rajeshgo.li", None));
    }
}
