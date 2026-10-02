//! Web launch setup. Existing phone launch routes and defaults are unchanged.
use super::board::blocking;
use super::*;
use crate::board::launch::{Config, Preview, Reply};
use crate::owner_settings::OwnerSettings;

fn reply((status, value): Reply) -> (StatusCode, Json<Value>) {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(value),
    )
}
fn sync(state: &AppState) -> anyhow::Result<()> {
    let store = board::board_store(state);
    let (current, _) = store.board(&board::outside(state)?, time::OffsetDateTime::now_utc())?;
    let settings = OwnerSettings::from_effective(&state.session_store.owner_settings()?)?;
    store.sync_launch_preferences(&current, &settings.new_agent)
}
pub(super) fn decorate(state: &AppState, payload: &mut Value) -> anyhow::Result<()> {
    sync(state)?;
    let store = board::board_store(state);
    let db = expand_home(&state.config.sm_send.db_path);
    let active = RetainedQueueStore::list_active_codex_review_requests_from_path(&expand_home(
        &state.config.codex_requests.db_path,
    ))?;
    let docs = crate::owner_docs::OwnerDocStore::new(db);
    let mut owner_reviews = Vec::new();
    for summary in docs.summaries(None, false)? {
        if summary.state != crate::owner_docs::OwnerDocState::ReviewRequested {
            continue;
        }
        if let Some(publish) = docs.publishes(&summary.doc.id)?.pop() {
            if let Some(pr) = publish.pr_number.or(summary.doc.pr_number) {
                owner_reviews.push((summary.doc.repo,pr,json!({"id":format!("owner:{}",summary.doc.id),"by":"you","number":pr,"reviewer_label":"Your review","since":publish.published_at,"state":"waiting"})));
            }
        }
    }
    let reviews_for = |ticket: &mut Value| {
        let prs = ticket["prs"].as_array().cloned().unwrap_or_default();
        let mut reviews = Vec::new();
        for pr in prs.iter().filter(|pr| {
            pr["state"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("open"))
        }) {
            for r in active
                .iter()
                .filter(|r| pr["repo"] == r.repo && pr["number"] == r.pr_number)
            {
                reviews.push(json!({"id":r.id,"by":"agent","number":r.pr_number,"reviewer_label":r.reviewer_label,"state":r.state,"since":r.step_started_at.as_deref().unwrap_or(&r.requested_at)}));
            }
            for (repo, number, value) in &owner_reviews {
                if pr["repo"] == *repo && pr["number"] == *number {
                    reviews.push(value.clone());
                }
            }
        }
        let mut seen = BTreeSet::new();
        reviews.retain(|r| seen.insert(r["id"].as_str().unwrap_or_default().to_owned()));
        ticket["reviews"] = json!(reviews);
    };
    for lane in payload["lanes"].as_array_mut().into_iter().flatten() {
        lane["launch_default"] = store.launch_default(lane["id"].as_i64().unwrap_or(0))?;
        for ticket in lane["tickets"].as_array_mut().into_iter().flatten() {
            decorate_ticket(&store, ticket)?;
            reviews_for(ticket);
        }
    }
    for group in payload["other"].as_array_mut().into_iter().flatten() {
        for ticket in group["tickets"].as_array_mut().into_iter().flatten() {
            decorate_ticket(&store, ticket)?;
            reviews_for(ticket);
        }
    }
    Ok(())
}
fn decorate_ticket(store: &crate::board::BoardStore, ticket: &mut Value) -> anyhow::Result<()> {
    let key = (
        ticket["repo"].as_str().unwrap_or_default().to_owned(),
        ticket["number"].as_i64().unwrap_or(0),
    );
    ticket["launch_preference"] = store.launch_preference(&key)?;
    Ok(())
}
pub(super) async fn preview(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Preview>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    owner_web_guard(&state, &headers, Some(peer), "POST")?;
    board::owner_write_guard(&state, &headers, peer, "POST", &uri)?;
    ensure_core_writes_enabled(&state)?;
    let _launch = state.board_auto_start_gate.lock().await;
    let result = blocking(&state, move |state| {
        let _board = state
            .board_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        sync(state)?;
        Ok(board::board_store(state).preview_launch(
            &board::outside(state)?,
            body,
            state.session_store.owner_settings()?["new_agent"].clone(),
            &random_urlsafe_token(32),
            time::OffsetDateTime::now_utc(),
        )?)
    })
    .await?;
    Ok(reply(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Commit {
    request_id: String,
    token: String,
}
pub(super) async fn commit(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Commit>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    owner_web_guard(&state, &headers, Some(peer), "PUT")?;
    board::owner_write_guard(&state, &headers, peer, "PUT", &uri)?;
    ensure_core_writes_enabled(&state)?;
    let _launch = state.board_auto_start_gate.lock().await;
    let result = blocking(&state, move |state| {
        let _board = state
            .board_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        let result = board::board_store(state).commit_launch(
            &board::outside(state)?,
            state.session_store.owner_settings()?["new_agent"].clone(),
            &body.request_id,
            &body.token,
            time::OffsetDateTime::now_utc(),
        )?;
        if result.0 == 200 && result.1["preferences_count"].as_u64().unwrap_or(0) > 0 {
            board::request_recompute(state);
        }
        Ok(result)
    })
    .await?;
    Ok(reply(result))
}
#[derive(Deserialize)]
pub(super) struct DefaultKey {
    lane_id: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DefaultWrite {
    lane_id: i64,
    expected_revision: i64,
    config: Option<Config>,
}
pub(super) async fn get_default(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Query(key): Query<DefaultKey>,
) -> Result<Json<Value>, ApiError> {
    board::owner_guard(&state, &headers, peer, "GET", &uri, false)?;
    Ok(Json(
        blocking(&state, move |state| {
            Ok(board::board_store(state).launch_default(key.lane_id)?)
        })
        .await?,
    ))
}
pub(super) async fn put_default(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<DefaultWrite>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    owner_web_guard(&state, &headers, Some(peer), "PUT")?;
    board::owner_write_guard(&state, &headers, peer, "PUT", &uri)?;
    ensure_core_writes_enabled(&state)?;
    let _launch = state.board_auto_start_gate.lock().await;
    Ok(reply(
        blocking(&state, move |state| {
            let _board = state
                .board_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
            sync(state)?;
            Ok(board::board_store(state).set_launch_default(
                body.lane_id,
                body.expected_revision,
                body.config,
            )?)
        })
        .await?,
    ))
}
