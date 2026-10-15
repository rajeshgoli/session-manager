//! Owner activity log over HTTP (sm#2173; Auto PM spec, Appendix A): the
//! layer that records one row per owner request, the daily prune, and
//! `GET /autopm/events`.

use super::*;
use crate::owner_events::{self as log, EventQuery, OwnerEvent, OwnerEventStore};
use axum::extract::{FromRequestParts, MatchedPath, RawPathParams};

const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const OWNER_ONLY: &str = "autopm events are owner-only";

pub(super) fn store(state: &AppState) -> OwnerEventStore {
    OwnerEventStore::new(expand_home(&state.config.sm_send.db_path))
}

/// The surface and device of a request carrying owner auth (A.2), or `None`
/// for agents, loopback without owner auth, and everyone else.
fn owner_surface(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
) -> Option<(&'static str, Option<String>)> {
    let application = request_hostname(headers).and_then(|hostname| {
        cloudflare_access_application_for_host(&state.config.cloudflare_access, &hostname)
    });
    let assertion = header_text(headers, "cf-access-jwt-assertion");
    if application == Some(CloudflareAccessApplication::MobileApp) {
        if let Some(context) = assertion.as_deref().and_then(|assertion| {
            classify_cloudflare_access_assertion_cached(
                state,
                CloudflareAccessApplication::MobileApp,
                assertion,
            )
            .ok()
        }) {
            let device = context.identity.trim();
            return Some(("android", (!device.is_empty()).then(|| device.to_owned())));
        }
    }
    if device_auth_user(headers, &state.config).is_some() {
        return Some(("android", None));
    }
    if application == Some(CloudflareAccessApplication::Browser)
        && !is_local_bypass_request(headers, peer_addr, &state.config)
    {
        if let Some(context) = assertion.as_deref().and_then(|assertion| {
            classify_cloudflare_access_assertion_cached(
                state,
                CloudflareAccessApplication::Browser,
                assertion,
            )
            .ok()
        }) {
            if browser_owner_email(state, &context).is_ok_and(|email| email.is_some()) {
                return Some(("web", context.common_name.clone()));
            }
        }
    }
    browser_session_user(headers, &state.config)
        .filter(|user| allowlisted_google_email(&state.config, &user.email))
        .map(|_| ("web", None))
}

fn from_agent(headers: &HeaderMap) -> bool {
    crate::local_identity::current().is_some()
        || [handoff::SESSION_HEADER, "x-sm-session-id"]
            .into_iter()
            .any(|name| headers.contains_key(name))
}

/// Records each owner request after it is served. The row is written off
/// the request path, so a slow database never delays the response.
pub(super) async fn record(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(route) = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
    else {
        return next.run(request).await;
    };
    if from_agent(request.headers()) {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let params: Vec<String> = RawPathParams::from_request_parts(&mut parts, &())
        .await
        .map(|params| params.iter().map(|(_, value)| value.to_owned()).collect())
        .unwrap_or_default();
    let request = Request::from_parts(parts, body);
    let headers = request.headers().clone();
    let has_client_header = ["x-sm-screen", "x-sm-nav", "x-sm-bg"]
        .into_iter()
        .any(|name| headers.contains_key(name));
    let bg = header_text(&headers, "x-sm-bg");
    if log::is_background(&route, &params, bg.as_deref(), has_client_header) {
        return next.run(request).await;
    }
    let peer_addr = request_peer_addr(&request);
    let method = request.method().as_str().to_owned();
    let at_ms = log::now_ms();
    let started = Instant::now();
    let response = next.run(request).await;
    let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let status = i64::from(response.status().as_u16());
    tokio::task::spawn_blocking(move || {
        let Some((surface, device_id)) = owner_surface(&state, &headers, peer_addr) else {
            return;
        };
        let resolved = log::resolve(&method, &route, &params);
        let (nav, notice_id) = log::parse_nav(header_text(&headers, "x-sm-nav").as_deref());
        let event = OwnerEvent {
            at_ms,
            surface: surface.to_owned(),
            device_id,
            action: resolved.action,
            target_kind: resolved.target_kind.map(str::to_owned),
            target_id: resolved.target_id,
            screen: header_text(&headers, "x-sm-screen")
                .map(|screen| log::clip(&screen))
                .filter(|screen| !screen.is_empty()),
            nav,
            notice_id,
            status: Some(status),
            duration_ms: Some(duration_ms),
            ..OwnerEvent::default()
        };
        // Rows carrying the client's screen or nav headers are never coalesced.
        let coalesce = !["x-sm-screen", "x-sm-nav"]
            .into_iter()
            .any(|name| headers.contains_key(name));
        if let Err(error) = store(&state).record_unless_repeat(&event, coalesce) {
            eprintln!("owner event {method} {route} not recorded: {error:#}");
        }
    });
    response
}

/// Submitted app-terminal input (A.2), coalesced per session.
pub(super) fn record_terminal_input(state: &AppState, session_id: &str) {
    log::record_terminal_input(&expand_home(&state.config.sm_send.db_path), session_id);
}

/// Deletes rows past retention at startup and then daily (A.6).
pub(super) fn spawn_prune(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let store = store(&state);
            match tokio::task::spawn_blocking(move || store.prune(log::now_ms())).await {
                Ok(Err(error)) => eprintln!("owner event prune failed: {error:#}"),
                Err(error) => eprintln!("owner event prune failed: {error}"),
                Ok(Ok(_)) => {}
            }
            tokio::time::sleep(PRUNE_INTERVAL).await;
        }
    });
}

#[derive(Debug, Deserialize)]
pub(super) struct EventsQuery {
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    surface: Option<String>,
    action: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

fn bad_request(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

/// `GET /autopm/events` (A.7). Owner only until the autopm role exists.
pub(super) async fn list_events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EventsQuery>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    if from_agent(request.headers()) {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: OWNER_ONLY.into(),
        });
    }
    let limit = query.limit.unwrap_or(log::DEFAULT_LIMIT);
    if !(1..=log::MAX_LIMIT).contains(&limit) {
        return Err(bad_request(format!("limit must be 1-{}", log::MAX_LIMIT)));
    }
    let after = match query.cursor.as_deref().filter(|c| !c.is_empty()) {
        Some(cursor) => Some(
            log::parse_cursor(cursor).ok_or_else(|| bad_request("cursor must be <at_ms>:<id>"))?,
        ),
        None => None,
    };
    let surface = query.surface.filter(|s| !s.is_empty());
    if let Some(surface) = surface.as_deref() {
        if !log::SURFACES.contains(&surface) {
            return Err(bad_request(format!(
                "surface must be one of {}",
                log::SURFACES.join(", ")
            )));
        }
    }
    let event_query = EventQuery {
        since_ms: query.since_ms,
        until_ms: query.until_ms,
        surface,
        action: query.action.filter(|a| !a.is_empty()),
        limit,
        after,
    };
    let store = store(&state);
    let (events, next_cursor) = tokio::task::spawn_blocking(move || store.list(&event_query))
        .await
        .map_err(|error| ApiError::Internal(error.into()))??;
    Ok(Json(
        json!({ "events": events, "next_cursor": next_cursor }),
    ))
}

#[cfg(test)]
mod tests;
