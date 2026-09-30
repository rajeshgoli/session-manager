//! Review policy reads and writes for owner clients and authenticated sessions.
use super::*;
use crate::review::policy;

#[derive(Deserialize)]
pub(super) struct QueryPolicy {
    repo: Option<String>,
    pr: Option<i64>,
    ticket: Option<i64>,
    lane: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct PutPolicy {
    scope: String,
    repo: Option<String>,
    number: Option<i64>,
    reviewer: Option<Value>,
    session_id: Option<String>,
}

fn bad(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

fn forbidden(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::FORBIDDEN,
        detail: detail.into(),
    }
}

fn repo(value: Option<&str>) -> Result<String, ApiError> {
    let repo = value.ok_or_else(|| bad("repo is required"))?;
    let repo = crate::work_claims::canonical_repo(repo);
    crate::owner_docs::validate_repo_slug(&repo).map_err(|e| bad(e.to_string()))?;
    Ok(repo)
}

pub(super) async fn get_policy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<QueryPolicy>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer), "/review-policies")?;
    let db = expand_home(&state.config.sm_send.db_path);
    let selected = [query.pr, query.ticket, query.lane]
        .into_iter()
        .flatten()
        .count();
    if selected > 1 {
        return Err(bad("choose one of pr, ticket or lane"));
    }
    if [query.pr, query.ticket, query.lane]
        .into_iter()
        .flatten()
        .any(|n| n <= 0)
    {
        return Err(bad("number must be positive"));
    }
    if selected == 0 && query.repo.is_none() {
        let default = state.session_store.owner_settings()?["reviews"]["reviewer"].clone();
        return Ok(Json(json!({"policies":policy::list(&db)?,
            "default":{"reviewer":default,"fallback":crate::review::chain(&default).into_iter().skip(1).collect::<Vec<_>>(),"source":"default"}})));
    }
    let repo = repo(query.repo.as_deref())?;
    let default = state.session_store.owner_settings()?["reviews"]["reviewer"].clone();
    Ok(Json(policy::resolve(
        &db,
        &repo,
        query.pr,
        query.ticket,
        query.lane,
        &default,
    )?))
}

fn session_family(
    state: &AppState,
    id: &str,
) -> Result<std::collections::BTreeSet<String>, ApiError> {
    let mut family = std::collections::BTreeSet::new();
    let mut pending = vec![id.to_owned()];
    while let Some(id) = pending.pop() {
        if !family.insert(id.clone()) {
            continue;
        }
        let session = state
            .session_store
            .get_session(&id)?
            .ok_or_else(|| bad(format!("Session {id} not found")))?;
        pending.extend(session.predecessor_session_id);
        pending.extend(session.successor_session_id);
    }
    Ok(family)
}

fn holds_ticket(
    db: &std::path::Path,
    family: &std::collections::BTreeSet<String>,
    repo: &str,
    number: i64,
) -> anyhow::Result<bool> {
    let conn = rusqlite::Connection::open(db)?;
    let mut stmt=conn.prepare("SELECT session_id FROM work_claims WHERE ended_at IS NULL AND \
        ((kind='ticket' AND repo=?1 AND number=?2) OR \
         (kind='pr' AND ((repo=?1 AND number IN \
            (SELECT pr_number FROM work_links WHERE repo=?1 AND ticket_number=?2)) OR \
            (repo,number) IN (SELECT pr_repo,pr_number FROM board_prs WHERE repo=?1 AND issue_number=?2))))")?;
    let ids = stmt.query_map(rusqlite::params![repo, number], |r| r.get::<_, String>(0))?;
    for id in ids {
        if family.contains(&id?) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) async fn put_policy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<PutPolicy>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer), "/review-policies")?;
    ensure_core_writes_enabled(&state)?;
    let caller = header_text(&headers, handoff::SESSION_HEADER);
    if caller.as_deref() != body.session_id.as_deref() {
        return Err(forbidden("Session identity does not match the request"));
    }
    let repo = if body.scope == "default" {
        body.repo
            .as_deref()
            .map(|value| repo(Some(value)))
            .transpose()?
            .unwrap_or_default()
    } else {
        repo(body.repo.as_deref())?
    };
    let number = match body.scope.as_str() {
        "repo" => 0,
        "lane" | "ticket" => body
            .number
            .filter(|n| *n > 0)
            .ok_or_else(|| bad("number must be positive"))?,
        "default" => 0,
        _ => return Err(bad("scope must be default, repo, lane or ticket")),
    };
    if let Some(value) = &body.reviewer {
        policy::validate(&body.scope, value).map_err(bad)?;
        if value["kind"] == "paired" {
            return Err(bad(
                "Paired reviewers are available after the paired-reviewer feature ships.",
            ));
        }
    }
    if let Some(id) = caller.as_deref() {
        let credential = reparent_session_credential(&headers)?;
        if !state
            .session_store
            .session_credential_matches(id, &credential)?
        {
            return Err(forbidden("Session credential does not match the caller"));
        }
        if body.scope != "ticket" {
            return Err(forbidden(format!(
                "Only {} sets default, repo and lane review policies.",
                state.config.owner_name
            )));
        }
        let family = session_family(&state, id)?;
        let db = expand_home(&state.config.sm_send.db_path);
        policy::ensure_schema(&db)?;
        if holds_ticket(&db, &family, &repo, number)? {
            return Err(forbidden(format!(
                "You hold #{number}; an author cannot choose its own reviewer."
            )));
        }
    } else {
        board::owner_guard(&state, &headers, peer, "PUT", &uri, true)?;
    }
    if body.scope == "default" {
        let reviewer = body
            .reviewer
            .unwrap_or_else(|| json!({"kind":"github_codex"}));
        let settings = state
            .session_store
            .update_owner_settings(&json!({"reviews":{"reviewer":reviewer}}), |_| Ok(Ok(())))?
            .map_err(bad)?;
        return Ok(Json(json!({"reviewer":settings["reviews"]["reviewer"],
            "fallback":crate::review::chain(&settings["reviews"]["reviewer"]).into_iter().skip(1).collect::<Vec<_>>(),"source":"default"})));
    }
    let name = match caller.as_deref() {
        Some(id) => state
            .session_store
            .get_session(id)?
            .map(|s| claims::session_info(&s).name)
            .unwrap_or_else(|| id.to_owned()),
        None => state.config.owner_name.clone(),
    };
    let db = expand_home(&state.config.sm_send.db_path);
    let now = now_rfc3339();
    let saved = policy::set(
        &db,
        policy::PolicyChange {
            scope: &body.scope,
            repo: &repo,
            number,
            reviewer: body.reviewer.as_ref(),
            session_id: caller.as_deref(),
            name: &name,
            now: &now,
        },
    )?;
    Ok(Json(json!({"policy":saved})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request},
    };
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    #[tokio::test]
    async fn author_and_agent_permissions_apply_to_ticket_and_pr_claims() {
        let dir = std::env::temp_dir().join(format!(
            "sm-policy-http-{}-{}",
            std::process::id(),
            random_urlsafe_token(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state_file = dir.join("sessions.json");
        let author_hash = format!("{:x}", Sha256::digest(b"author-secret"));
        let planner_hash = format!("{:x}", Sha256::digest(b"planner-secret"));
        std::fs::write(&state_file,serde_json::to_vec(&json!({"sessions":[
            {"id":"author","name":"author","working_dir":"/repo","tmux_session":"author","provider":"codex-fork","status":"running","created_at":"2026-09-30T00:00:00","last_activity":"2026-09-30T00:00:00","session_credential_sha256":author_hash},
            {"id":"planner","name":"planner","working_dir":"/repo","tmux_session":"planner","provider":"codex-fork","status":"running","created_at":"2026-09-30T00:00:00","last_activity":"2026-09-30T00:00:00","session_credential_sha256":planner_hash}
        ]})).unwrap()).unwrap();
        let db = dir.join("queue.db");
        policy::ensure_schema(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "INSERT INTO work_claims(id,repo,number,kind,session_id,source,claimed_at) VALUES
            ('c1','example/repo',7,'ticket','author','test','now'),
            ('c2','example/repo',13,'pr','author','test','now');
            INSERT INTO work_links(repo,pr_number,ticket_number,source,created_at) VALUES
            ('example/repo',13,8,'test','now');",
        )
        .unwrap();
        let mut config = AppConfig::default();
        config.paths.state_file = state_file.display().to_string();
        config.sm_send.db_path = db.display().to_string();
        config.rust_core.fixture_writes_enabled = true;
        let app = router(AppState::new(config));
        let send = |session: Option<&str>, credential: Option<&str>, scope: &str, number: i64| {
            let mut builder = Request::builder()
                .method(Method::PUT)
                .uri("/review-policies")
                .header("host", "testserver")
                .header("content-type", "application/json");
            if let Some(session) = session {
                builder = builder.header("x-sm-session", session);
            }
            if let Some(credential) = credential {
                builder = builder.header("x-sm-session-credential", credential);
            }
            let body = json!({"scope":scope,"repo":"example/repo","number":number,
                "reviewer":{"kind":"github_codex"},"session_id":session});
            let mut request = builder.body(Body::from(body.to_string())).unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4200))));
            request
        };
        assert_eq!(
            app.clone()
                .oneshot(send(Some("author"), Some("author-secret"), "ticket", 7))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(send(Some("author"), Some("author-secret"), "ticket", 8))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(send(Some("planner"), Some("planner-secret"), "repo", 0))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(send(Some("planner"), Some("planner-secret"), "ticket", 8))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(send(Some("planner"), Some("author-secret"), "ticket", 8))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.oneshot(send(None, None, "repo", 0))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
