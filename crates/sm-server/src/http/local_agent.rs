//! Local-agent route policy. Only explicit routes below accept stamped calls;
//! unclassified routes (including future routes) are closed to local agents.
use super::*;
use crate::local_egress::gateway::{AGENT_HEADER, SIGNATURE_HEADER};
use axum::extract::MatchedPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Policy {
    fields: &'static [&'static str],
    own_path: bool,
    reminder_query: bool,
}
const READ: Policy = Policy {
    fields: &[],
    own_path: false,
    reminder_query: false,
};
const REQUESTER: Policy = Policy {
    fields: &["requester_session_id"],
    ..READ
};
const SENDER: Policy = Policy {
    fields: &["sender_session_id"],
    ..READ
};
const SESSION: Policy = Policy {
    fields: &["session_id"],
    ..READ
};
const OWN: Policy = Policy {
    own_path: true,
    ..READ
};
const OWN_REQUESTER: Policy = Policy {
    own_path: true,
    ..REQUESTER
};

fn policy(method: &str, route: &str) -> Option<Policy> {
    let method = if method == "HEAD" { "GET" } else { method };
    Some(match (method, route) {
        (
            "GET",
            "/health"
            | "/sessions"
            | "/sessions/context-monitor"
            | "/sessions/{session_id}"
            | "/sessions/{session_id}/context"
            | "/sessions/{session_id}/usage"
            | "/sessions/{parent_session_id}/children"
            | "/sessions/{session_id}/root"
            | "/sessions/{session_id}/output"
            | "/sessions/{session_id}/tool-calls"
            | "/sessions/{session_id}/activity-actions"
            | "/sessions/{session_id}/codex-events"
            | "/sessions/{session_id}/codex-pending-requests"
            | "/sessions/{session_id}/last-turn"
            | "/sessions/{session_id}/subagents"
            | "/session-obligations"
            | "/registry"
            | "/registry/{role}"
            | "/humans"
            | "/humans/{identifier}"
            | "/nodes"
            | "/queue-jobs"
            | "/queue-jobs/{job_id}"
            | "/queue-jobs/{job_id}/log"
            | "/review-requests"
            | "/review-requests/{request_id}"
            | "/review-policies"
            | "/btw-requests/{request_id}"
            | "/reparent-requests"
            | "/reparent-requests/{request_id}"
            | "/docs"
            | "/docs/{doc_id}"
            | "/docs/{doc_id}/{*rest}"
            | "/claims"
            | "/board"
            | "/merge-holds"
            | "/history"
            | "/history/agents"
            | "/history/tickets"
            | "/t/{repo}/{number}"
            | "/bugs/{bug_id}"
            | "/bugs/{bug_id}/screenshot.png"
            | "/usage/accounts",
        ) => READ,
        (
            "POST",
            "/claims"
            | "/claims/release"
            | "/claims/worktree"
            | "/worktrees/keep"
            | "/worktrees/delete"
            | "/review-requests"
            | "/queue-jobs"
            | "/merge-holds"
            | "/merge-holds/release"
            | "/email/send"
            | "/humans/{identifier}/email"
            | "/sessions/{session_id}/what"
            | "/sessions/{session_id}/retire"
            | "/sessions/{session_id}/kill"
            | "/sessions/{session_id}/reparent-requests"
            | "/sessions/{session_id}/reparent-tree-requests"
            | "/reparent-requests/{request_id}/approve"
            | "/reparent-requests/{request_id}/reject",
        ) => REQUESTER,
        (
            "POST",
            "/sessions/{session_id}/input"
            | "/sessions/input-batch"
            | "/humans/{identifier}/messages",
        ) => SENDER,
        (
            "POST",
            "/docs" | "/board/links" | "/board/lanes" | "/review-requests/{request_id}/submit",
        )
        | ("PUT", "/review-policies") => SESSION,
        ("PATCH", "/sessions/{session_id}") | ("POST", "/sessions/{session_id}/agent-status") => {
            OWN
        }
        (
            "POST",
            "/sessions/{session_id}/task-complete"
            | "/sessions/{session_id}/turn-complete"
            | "/sessions/{session_id}/clear"
            | "/sessions/{session_id}/context-monitor"
            | "/sessions/{session_id}/handoff",
        ) => OWN_REQUESTER,
        ("POST", "/sessions/{session_id}/notify-on-stop") => Policy {
            fields: &["requester_session_id", "sender_session_id"],
            ..READ
        },
        ("POST", "/scheduler/remind") => Policy {
            reminder_query: true,
            ..READ
        },
        (
            "DELETE",
            "/scheduler/remind/{reminder_id}"
            | "/review-requests/{request_id}"
            | "/queue-jobs/{job_id}",
        )
        | ("POST", "/queue-jobs/{job_id}/cancel") => READ,
        _ => return None,
    })
}

pub(super) fn denied(detail: &str) -> ApiError {
    ApiError::Status {
        status: StatusCode::FORBIDDEN,
        detail: detail.into(),
    }
}

pub(super) async fn authenticate(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: axum::middleware::Next,
) -> Response {
    // A bare caller-supplied local-agent header has no effect on the legacy
    // request. A claimed service signature must verify, even with auth off.
    if !request.headers().contains_key(SIGNATURE_HEADER) {
        request.headers_mut().remove(AGENT_HEADER);
        return next.run(request).await;
    }
    match authenticate_request(&state, request).await {
        Ok((identity, request)) => crate::local_identity::scope(identity, next.run(request)).await,
        Err(error) => error.into_response(),
    }
}

async fn authenticate_request(
    state: &AppState,
    request: Request,
) -> Result<(crate::local_egress::gateway::VerifiedLocalAgent, Request), ApiError> {
    let peer = request_peer_addr(&request).ok_or_else(|| denied("gateway peer is missing"))?;
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 8 * 1024 * 1024)
        .await
        .map_err(|_| ApiError::Status {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            detail: "gateway request exceeds limit".into(),
        })?;
    let verifier = state.local_agent_verifier.clone();
    let headers = parts.headers.clone();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let signed_body = body.clone();
    let identity = tokio::task::spawn_blocking(move || {
        verifier.verify(peer, &headers, &method, &uri, &signed_body)
    })
    .await
    .map_err(|e| ApiError::Internal(e.into()))??
    .ok_or_else(|| denied("invalid gateway signature or registration"))?;
    let session = state
        .session_store
        .get_session(identity.agent_id())?
        .filter(|s| !s.is_stopped())
        .ok_or_else(|| denied("gateway agent is not a live session"))?;
    let mut request = Request::from_parts(parts, Body::from(body));
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str())
        .unwrap_or("");
    let selected = policy(request.method().as_str(), route)
        .ok_or_else(|| denied("route is not available to local agents"))?;
    let body = to_bytes(
        std::mem::replace(request.body_mut(), Body::empty()),
        8 * 1024 * 1024,
    )
    .await
    .map_err(|_| denied("invalid gateway body"))?;
    let body = bind_caller(selected, &mut request, body, &session.id)?;
    *request.body_mut() = Body::from(body);
    // Defense in depth for signed requests arriving directly: neither a valid
    // owner cookie nor forwarding metadata can promote a stamped caller.
    let remove: Vec<_> = request
        .headers()
        .keys()
        .filter(|name| {
            let n = name.as_str();
            n.starts_with("x-sm-")
                || n.starts_with("x-forwarded-")
                || n.starts_with("cf-")
                || matches!(n, "authorization" | "cookie" | "forwarded" | "origin")
        })
        .cloned()
        .collect();
    for name in remove {
        request.headers_mut().remove(name);
    }
    let id = axum::http::HeaderValue::from_str(&session.id)
        .map_err(|_| denied("invalid registered agent id"))?;
    request.headers_mut().insert("x-sm-session-id", id.clone());
    request.headers_mut().insert(handoff::SESSION_HEADER, id);
    request.headers_mut().remove("content-length");
    request.extensions_mut().insert(identity.clone());
    Ok((identity, request))
}

fn bind_caller(
    selected: Policy,
    request: &mut Request,
    body: Bytes,
    agent: &str,
) -> Result<Bytes, ApiError> {
    if selected.own_path {
        let path_agent = request.uri().path().split('/').nth(2).unwrap_or("");
        // IDs are canonical ASCII gateway registration IDs. Reject encoded or
        // aliased paths rather than letting a decoder change the self target.
        if path_agent != agent {
            return Err(denied("local agents may update only their own session"));
        }
    }
    if selected.reminder_query {
        // Preserve nonidentity query bytes and duplicate-field validation in
        // the existing extractor; replace only the session_id query argument.
        let mut pairs: Vec<_> = request
            .uri()
            .query()
            .unwrap_or("")
            .split('&')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        // Refuse encoded keys to avoid a second interpretation after binding.
        // Values (including message text) retain their ordinary form encoding.
        pairs.retain(|p| p.split('=').next() != Some("session_id"));
        if pairs
            .iter()
            .any(|p| p.split('=').next().unwrap_or("").contains('%'))
        {
            return Err(denied("encoded reminder query keys are not supported"));
        }
        pairs.push(format!("session_id={agent}"));
        let target = format!("{}?{}", request.uri().path(), pairs.join("&"));
        *request.uri_mut() = target
            .parse()
            .map_err(|_| denied("invalid reminder query"))?;
    }
    if selected.fields.is_empty() && !(selected.own_path && request.method() == "PATCH") {
        return Ok(body);
    }
    let mut value: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(|_| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "invalid JSON request".into(),
        })?
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| denied("gateway request must be a JSON object"))?;
    if selected.own_path && request.method() == "PATCH" && object.contains_key("is_em") {
        return Err(denied("local agents cannot change management authority"));
    }
    for field in selected.fields {
        object.insert((*field).into(), json!(agent));
    }
    // These are sender-related capabilities, not message recipients. Never
    // allow a forged parent edge or cancel another session's reminders.
    if selected == SENDER && request.uri().path().starts_with("/sessions/") {
        object.remove("parent_session_id");
        if object.contains_key("remind_cancel_on_reply_session_id") {
            object.insert("remind_cancel_on_reply_session_id".into(), json!(agent));
        }
    }
    Ok(Bytes::from(serde_json::to_vec(&value)?))
}

/// Call before any queue action; #2009 replaces this closed boundary with
/// persistence and enforcement of the verified submitting agent's wall.
pub(super) fn require_queue_confinement() -> Result<(), ApiError> {
    if crate::local_identity::current().is_some() {
        return Err(ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "local-agent queue confinement is not installed".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
