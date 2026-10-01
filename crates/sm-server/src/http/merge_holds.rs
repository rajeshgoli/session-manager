use super::*;
use crate::work_claims::merge_holds::{HoldActor, HoldError, HoldPr, MergeHoldSource};

pub(super) struct GhMergeHoldSource;
impl MergeHoldSource for GhMergeHoldSource {
    fn pull_request(&self, repo: &str, pr: i64) -> Result<HoldPr, String> {
        let args = vec![
            "pr".into(),
            "view".into(),
            pr.to_string(),
            "--repo".into(),
            repo.into(),
            "--json".into(),
            "state,isDraft,id,headRefOid".into(),
        ];
        let output =
            gh_command_output(&args, Duration::from_secs(30)).map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(command_stderr(&output));
        }
        let v: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
        Ok(HoldPr {
            node_id: v["id"].as_str().ok_or("missing PR id")?.into(),
            state: v["state"]
                .as_str()
                .ok_or("missing PR state")?
                .to_ascii_lowercase(),
            is_draft: v["isDraft"].as_bool().ok_or("missing draft state")?,
        })
    }
    fn set_draft(&self, node_id: &str, draft: bool) -> Result<(), String> {
        let op = if draft {
            "convertPullRequestToDraft"
        } else {
            "markPullRequestReadyForReview"
        };
        docs::gh_graphql(&format!("mutation($id:ID!) {{ {op}(input:{{pullRequestId:$id}}) {{ pullRequest {{ id }} }} }}"),json!({"id":node_id}),false)?;
        Ok(())
    }
}
fn error(e: HoldError) -> ApiError {
    let (status, detail) = match e {
        HoldError::NotFound(s) => (StatusCode::NOT_FOUND, s),
        HoldError::Conflict(s) => (StatusCode::CONFLICT, s),
        HoldError::Forbidden(s) => (StatusCode::FORBIDDEN, s),
        HoldError::Github(s) => (StatusCode::BAD_GATEWAY, s),
        HoldError::Store(e) => return ApiError::from(e),
    };
    ApiError::Status { status, detail }
}
#[derive(Default, Deserialize)]
pub(super) struct ListQuery {
    repo: Option<String>,
}
pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let repo = query.repo.map(|r| crate::work_claims::canonical_repo(&r));
    Ok(
        Json(json!({"holds":claims::work_claim_store(&state).merge_holds(repo.as_deref())?}))
            .into_response(),
    )
}
#[derive(Deserialize)]
pub(super) struct HoldRequest {
    pub repo: String,
    pub pr: i64,
    pub reason: Option<String>,
    pub requester_session_id: Option<String>,
}
fn actor(state: &AppState, id: Option<&str>) -> Result<HoldActor, ApiError> {
    match id {
        None => Ok(HoldActor {
            session_id: None,
            name: state.config.owner_name.clone(),
        }),
        Some(id) => {
            let session = state
                .session_store
                .get_session(id)?
                .ok_or(ApiError::NotFound("Session not found"))?;
            if messages::session_ended(&session) {
                return Err(ApiError::Status {
                    status: StatusCode::CONFLICT,
                    detail: "Session is retired".into(),
                });
            }
            Ok(HoldActor {
                session_id: Some(id.into()),
                name: claims::session_info(&session).name,
            })
        }
    }
}
pub(super) fn recipients(state: &AppState, repo: &str, pr: i64) -> anyhow::Result<Vec<String>> {
    let mut targets = BTreeSet::new();
    for claim in claims::work_claim_store(state).claims_for_item(repo, pr)? {
        if claim.ended_at.is_none()
            && state
                .session_store
                .get_session(&claim.session_id)?
                .is_some_and(|s| !s.is_stopped() && !messages::session_ended(&s))
        {
            targets.insert(claim.session_id);
        }
    }
    if targets.is_empty() {
        for summary in
            crate::owner_docs::OwnerDocStore::new(expand_home(&state.config.sm_send.db_path))
                .summaries(None, false)?
        {
            let doc = summary.doc;
            if doc.repo.eq_ignore_ascii_case(repo) && doc.pr_number == Some(pr) {
                // An auto-retired author is not woken for a hold; its chain is.
                let id = docs::review_wake_recipient(state, &doc).and_then(|id| {
                    match state.session_store.get_session(&id).ok().flatten() {
                        Some(session) if messages::restores(&session) => {
                            messages::live_recipient(state, &id).map(|session| session.id)
                        }
                        _ => Some(id),
                    }
                });
                if let Some(id) = id {
                    targets.insert(id);
                }
            }
        }
    }
    Ok(targets.into_iter().collect())
}
pub(super) async fn change(
    state: Arc<AppState>,
    payload: HoldRequest,
    release: bool,
    silent: bool,
) -> Result<Value, ApiError> {
    ensure_core_writes_enabled(&state)?;
    let repo = crate::work_claims::canonical_repo(&payload.repo);
    crate::owner_docs::validate_repo_slug(&repo).map_err(|e| ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: e.to_string(),
    })?;
    if payload.pr <= 0 {
        return Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "PR number must be positive".into(),
        });
    }
    let actor = actor(&state, payload.requester_session_id.as_deref())?;
    tokio::task::spawn_blocking(move || {
        let targets = if silent {
            vec![]
        } else {
            recipients(&state, &repo, payload.pr)?
        };
        let store = claims::work_claim_store(&state);
        let result = if release {
            store.release_merge_hold(
                &repo,
                payload.pr,
                &actor,
                &state.config.owner_name,
                state.merge_hold_source.as_ref(),
                &targets,
            )
        } else {
            store.place_merge_hold(
                &repo,
                payload.pr,
                &actor,
                payload.reason.as_deref(),
                state.merge_hold_source.as_ref(),
                &targets,
            )
        }
        .map_err(error)?;
        claims::deliver_claim_notices(&state, &result.notified);
        Ok::<_, ApiError>(serde_json::to_value(result)?)
    })
    .await
    .map_err(|e| ApiError::from(anyhow::anyhow!(e)))?
}
pub(super) async fn place(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<HoldRequest>,
) -> Result<Response, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer), "/merge-holds")?;
    Ok(Json(change(state, payload, false, false).await?).into_response())
}
pub(super) async fn release(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<HoldRequest>,
) -> Result<Response, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer), "/merge-holds/release")?;
    Ok(Json(change(state, payload, true, false).await?).into_response())
}
pub(super) fn sync(state: &AppState) -> anyhow::Result<()> {
    let store = claims::work_claim_store(state);
    for hold in store.merge_holds(None)? {
        match store.sync_merge_hold(
            &hold.id,
            state.merge_hold_source.as_ref(),
            &recipients(state, &hold.repo, hold.pr)?,
        ) {
            Ok((notified, _)) => claims::deliver_claim_notices(state, &notified),
            Err(e) => eprintln!("merge hold sync failed: {e:?}"),
        }
    }
    for hold in store.pending_merge_hold_notices()? {
        use crate::owner_messages::{CreateOwnerMessage, NewOwnerMessage};
        let result = messages::owner_message_store(state).create_once(
            NewOwnerMessage {
                human: state.config.owner_name.clone(),
                sender_session_id: "sm".into(),
                sender_session_name: "Session Manager".into(),
                title: format!("PR #{} merged while held", hold.pr),
                body_markdown: hold.merged_message(),
                blocking: false,
            },
            Some(&format!("merge-hold:{}", hold.id)),
        )?;
        if let CreateOwnerMessage::Created(message) = result {
            follows::notice_new_message(state, &message);
            store.finish_merge_hold_notice(&hold.id)?;
        }
    }
    Ok(())
}
