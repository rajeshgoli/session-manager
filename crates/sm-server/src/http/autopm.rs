//! Auto PM over HTTP (sm#2174; spec 2170 Appendices B, C and F.2): the
//! switch, experiment and budget routes, the 👎 route, the review and app
//! deploy limits, the health guard's counters, and the minute ticker that
//! runs the guard, the budget upkeep and sm's own experiment moves.
//!
//! The owner and authenticated live autopm role holder may use the control routes.

use super::*;
pub(super) mod role;
use crate::autopm::budget::{self, ReviewGate, ReviewStep};
use crate::autopm::experiments::{self, Ctx, Git, Pull};
use crate::autopm::{self, guard, switches, Actor, AutopmStore, Effect, Fail, Paths, Settings};
use crate::work_claims::merge_holds::HoldActor;
use futures_util::FutureExt;
use rusqlite::OptionalExtension;

const EXPERIMENT_TICK: Duration = Duration::from_secs(300);
const OWNER_ONLY: &str = "requires the owner";

pub(super) fn store(state: &AppState) -> AutopmStore {
    let config = &state.config;
    AutopmStore::new(Paths {
        queue_db: expand_home(&config.sm_send.db_path),
        usage_db: expand_home(&config.usage.db_path),
        activity_db: expand_home(&config.activity.db_path),
        utilization_db: expand_home(&config.utilization.db_path),
        bugs_db: expand_home(&config.bug_reports.db_path),
    })
}

pub(super) fn settings(state: &AppState) -> Result<Settings, ApiError> {
    Ok(typed(state, &state.session_store.owner_settings()?))
}

/// The `autopm` setting, typed, with the configured owner name.
pub(super) fn typed(state: &AppState, owner_settings: &Value) -> Settings {
    Settings::from_owner(owner_settings).with_owner(&state.config.owner_name)
}

/// The commit the running server was built from (the restart script sets
/// `SM_BUILD_SHA`).
pub(super) fn build_sha() -> Option<&'static str> {
    option_env!("SM_BUILD_SHA").filter(|sha| !sha.is_empty())
}

/// GitHub and git for the gates, through the `gh` CLI.
pub(super) struct GhGit;

impl Git for GhGit {
    fn pull(&self, pr: i64) -> Result<Pull, String> {
        let v = gh_api_json(autopm::REPO, &format!("pulls/{pr}"), false)?;
        let merged = v["merged"].as_bool().unwrap_or(false);
        Ok(Pull {
            state: if merged {
                "merged".into()
            } else {
                v["state"].as_str().unwrap_or("closed").to_owned()
            },
            base: v["base"]["ref"].as_str().unwrap_or_default().to_owned(),
            head_sha: v["head"]["sha"].as_str().unwrap_or_default().to_owned(),
            merge_commit: merged
                .then(|| v["merge_commit_sha"].as_str().map(str::to_owned))
                .flatten(),
        })
    }

    fn file_at(&self, path: &str, commit: &str) -> Result<Option<String>, String> {
        let args = vec![
            "api".to_owned(),
            format!("repos/{}/contents/{path}?ref={commit}", autopm::REPO),
            "-H".to_owned(),
            "Accept: application/vnd.github.raw".to_owned(),
        ];
        let output = gh_command_output(&args, Duration::from_secs(30))?;
        if output.status.success() {
            return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
        }
        let stderr = command_stderr(&output);
        if stderr.contains("404") || stderr.contains("Not Found") {
            return Ok(None);
        }
        Err(stderr)
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, String> {
        if ancestor == descendant {
            return Ok(true);
        }
        let v = gh_api_json(
            autopm::REPO,
            &format!("compare/{ancestor}...{descendant}"),
            false,
        )?;
        Ok(matches!(v["status"].as_str(), Some("ahead" | "identical")))
    }

    fn close_issue(&self, number: i64, comment: &str) -> Result<(), String> {
        let args = [
            "issue",
            "close",
            &number.to_string(),
            "--repo",
            autopm::REPO,
            "--comment",
            comment,
        ]
        .map(str::to_owned)
        .to_vec();
        let output = gh_command_output(&args, Duration::from_secs(30))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_stderr(&output))
        }
    }
}

fn ctx<'a>(state: &'a AppState, store: &'a AutopmStore, settings: &'a Settings) -> Ctx<'a> {
    Ctx {
        store,
        switches: &state.autopm_switches,
        git: state.autopm_git.as_ref(),
        settings,
        build_sha: build_sha(),
        now: autopm::now_ms(),
    }
}

fn api_error(fail: Fail) -> ApiError {
    match fail {
        Fail::Refused(refusal) => ApiError::Status {
            status: StatusCode::from_u16(refusal.status).unwrap_or(StatusCode::CONFLICT),
            detail: refusal.message(),
        },
        Fail::Internal(error) => ApiError::Internal(error),
    }
}

fn send(state: &AppState, session_id: &str, text: &str) -> anyhow::Result<()> {
    RetainedQueueStore::new(expand_home(&state.config.sm_send.db_path))
        .enqueue_message_with_metadata(
            session_id,
            text,
            "sequential",
            QueueMessageMetadata::default(),
        )?;
    if state.config.rust_core.runtime_enabled {
        let runtime = TmuxRuntime::from_app_config(&state.config);
        if let Err(error) = state
            .session_store
            .drain_runtime_pending_messages_for_session(session_id, &runtime)
        {
            eprintln!("autopm message to {session_id} not delivered yet: {error:#}");
        }
    }
    Ok(())
}

/// The live holder of the `autopm` role, recorded as an autopm seat.
pub(super) fn autopm_seat(state: &AppState, store: &AutopmStore) -> Option<String> {
    let holder = state
        .session_store
        .lookup_agent_registration(autopm::ROLE)
        .ok()
        .flatten()?
        .session_id;
    if let Err(error) = store.record_seat(&holder, autopm::now_ms()) {
        eprintln!("autopm seat {holder} not recorded: {error:#}");
    }
    Some(holder)
}

/// Carries out what a call left to do outside the database. Each failure is
/// logged; none undoes the call.
pub(super) fn run_effects(state: &AppState, effects: Vec<Effect>) {
    let store = store(state);
    for effect in effects {
        let result: Result<(), String> = match &effect {
            Effect::Message { session_id, text } => {
                send(state, session_id, text).map_err(|e| format!("{e:#}"))
            }
            Effect::WakeAutopm { text } => match autopm_seat(state, &store) {
                Some(seat) => send(state, &seat, text).map_err(|e| format!("{e:#}")),
                None => Ok(()),
            },
            Effect::MergeHold { pr, reason } => place_hold(state, &store, *pr, reason),
            Effect::CloseIssue { number, comment } => {
                state.autopm_git.close_issue(*number, comment)
            }
            Effect::CancelAutoStart { number, reason } => board::board_store(state)
                .cancel_auto_start(
                    &(autopm::REPO.to_owned(), *number),
                    reason,
                    time::OffsetDateTime::now_utc(),
                )
                .map_err(|e| format!("{e:#}")),
        };
        if let Err(error) = result {
            eprintln!("autopm effect {effect:?} failed: {error}");
        }
    }
}

/// One merge hold per PR, placed once: a closed PR is not asked again.
fn place_hold(state: &AppState, store: &AutopmStore, pr: i64, reason: &str) -> Result<(), String> {
    let conn = store.open().map_err(|e| format!("{e:#}"))?;
    let key = format!("hold:{pr}");
    let done: bool = conn
        .query_row(
            "SELECT 1 FROM autopm_notices WHERE key = ?1",
            [&key],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .is_some();
    if done {
        return Ok(());
    }
    let actor = HoldActor {
        session_id: None,
        name: "sm autopm".into(),
    };
    let result = claims::work_claim_store(state).place_merge_hold(
        autopm::REPO,
        pr,
        &actor,
        Some(reason),
        state.merge_hold_source.as_ref(),
        &[],
    );
    match result {
        Ok(_) | Err(crate::work_claims::merge_holds::HoldError::Conflict(_)) => {
            AutopmStore::first_notice(&conn, &key, autopm::now_ms()).map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(error) => Err(format!("{error:?}")),
    }
}

/// Owner-only, as `GET /autopm/events` is.
fn owner_only(state: &AppState, request: &Request) -> Result<(), ApiError> {
    ensure_owner_page_read_allowed(state, request)?;
    let local = is_request_local_bypass(state, request);
    if owner_events::from_agent(request.headers())
        || (!local
            && owner_events::owner_surface(state, request.headers(), request_peer_addr(request))
                .is_none())
    {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: OWNER_ONLY.into(),
        });
    }
    Ok(())
}

async fn json_body(request: Request) -> Result<Value, ApiError> {
    let bytes = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|e| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: format!("unreadable body: {e}"),
        })?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: format!("body must be JSON: {e}"),
    })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError::Internal(error.into()))?
}

/// `GET /autopm/switches`.
pub(super) async fn list_switches(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    role::actor(&state, &request)?;
    blocking(move || {
        let rows = switches::load(&store(&state).open()?)?;
        Ok(Json(json!({
            "enabled": state.autopm_switches.enabled(),
            "version": state.autopm_switches.version(),
            "switches": rows,
        })))
    })
    .await
}

/// `PUT /autopm/switches/{key} {on, reason}` (B.1).
pub(super) async fn put_switch(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let actor = role::actor(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    let body = json_body(request).await?;
    let on = body["on"].as_bool().ok_or_else(|| ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: "on must be true or false".into(),
    })?;
    let reason = body["reason"].as_str().map(str::to_owned);
    blocking(move || {
        let store = store(&state);
        let settings = settings(&state)?;
        let (row, effects) = experiments::set_switch(
            &ctx(&state, &store, &settings),
            actor,
            &key,
            on,
            reason.as_deref(),
        )
        .map_err(api_error)?;
        run_effects(&state, effects);
        Ok(Json(
            json!({"switch": row, "version": state.autopm_switches.version()}),
        ))
    })
    .await
}

/// `GET /autopm/experiments`.
pub(super) async fn list_experiments(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    role::actor(&state, &request)?;
    blocking(move || {
        let experiments =
            experiments::list(&store(&state).open()?).map_err(|e| ApiError::Internal(e.into()))?;
        Ok(Json(json!({ "experiments": experiments })))
    })
    .await
}

/// `GET /autopm/experiments/{id}`: the record, its measurements and votes.
pub(super) async fn get_experiment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    role::actor(&state, &request)?;
    blocking(move || {
        let conn = store(&state).open()?;
        let experiment = experiments::get(&conn, &id)
            .map_err(|e| ApiError::Internal(e.into()))?
            .ok_or(ApiError::NotFound("experiment not found"))?;
        let mut stmt = conn
            .prepare(
                "SELECT attempt, at_ms, status, error, result_json, decision, superseded_at_ms
                 FROM autopm_measurements WHERE experiment_id = ?1 ORDER BY attempt",
            )
            .map_err(|e| ApiError::Internal(e.into()))?;
        let measurements = stmt
            .query_map([&id], |row| {
                Ok(json!({
                    "attempt": row.get::<_, i64>(0)?,
                    "at_ms": row.get::<_, i64>(1)?,
                    "status": row.get::<_, String>(2)?,
                    "error": row.get::<_, Option<String>>(3)?,
                    "result": row.get::<_, Option<String>>(4)?
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok()),
                    "decision": row.get::<_, Option<String>>(5)?,
                    "superseded_at_ms": row.get::<_, Option<i64>>(6)?,
                }))
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|e| ApiError::Internal(e.into()))?;
        Ok(Json(
            json!({ "experiment": experiment, "measurements": measurements }),
        ))
    })
    .await
}

/// `POST /autopm/experiments` (B.2, B.3).
pub(super) async fn create_experiment(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let actor = role::actor(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    let body = json_body(request).await?;
    blocking(move || {
        let store = store(&state);
        let settings = settings(&state)?;
        let experiment = experiments::create(&ctx(&state, &store, &settings), actor, &body)
            .map_err(api_error)?;
        Ok((
            StatusCode::CREATED,
            Json(json!({ "experiment": experiment })),
        ))
    })
    .await
}

/// `PATCH /autopm/experiments/{id}`.
pub(super) async fn patch_experiment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let actor = role::actor(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    let body = json_body(request).await?;
    blocking(move || {
        let store = store(&state);
        let settings = settings(&state)?;
        let (experiment, effects) =
            experiments::patch(&ctx(&state, &store, &settings), actor, &id, &body)
                .map_err(api_error)?;
        run_effects(&state, effects);
        Ok(Json(json!({ "experiment": experiment })))
    })
    .await
}

/// `POST /autopm/experiments/{id}/live`: an approved switchless change.
pub(super) async fn experiment_live(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let actor = role::actor(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    blocking(move || {
        let store = store(&state);
        let settings = settings(&state)?;
        let experiment =
            experiments::go_live_switchless(&ctx(&state, &store, &settings), actor, &id)
                .map_err(api_error)?;
        Ok(Json(json!({ "experiment": experiment })))
    })
    .await
}

/// `GET /autopm/budget` (C.3).
pub(super) async fn get_budget(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    role::actor(&state, &request)?;
    blocking(move || {
        let store = store(&state);
        let settings = settings(&state)?;
        Ok(Json(budget::status(&store, &settings, autopm::now_ms())?))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct Feedback {
    experiment_id: String,
    vote: String,
    note: Option<String>,
}

/// `POST /client/autopm/feedback {experiment_id, vote, note?}` (F.3).
pub(super) async fn feedback(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Feedback>,
) -> Result<Json<Value>, ApiError> {
    board::owner_guard(&state, &headers, peer_addr, "POST", &uri, true)?;
    ensure_core_writes_enabled(&state)?;
    let up = match body.vote.as_str() {
        "up" => true,
        "down" => false,
        _ => {
            return Err(ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail: "vote must be up or down".into(),
            })
        }
    };
    blocking(move || {
        let store = store(&state);
        let settings = settings(&state)?;
        let (experiment, effects) = experiments::vote(
            &ctx(&state, &store, &settings),
            &body.experiment_id,
            up,
            body.note.as_deref().filter(|n| !n.trim().is_empty()),
        )
        .map_err(api_error)?;
        run_effects(&state, effects);
        Ok(Json(json!({ "experiment": experiment })))
    })
    .await
}

/// Applies an owner settings change to the auto PM: the kill switch (B.5),
/// enable (C.4 standing reservations), and skip (C.1).
pub(super) fn on_settings_change(state: &AppState, before: &Settings, after: &Settings) {
    state.autopm_switches.set_enabled(after.enabled);
    let store = store(state);
    let now = autopm::now_ms();
    let mut effects = Vec::new();
    if before.enabled && !after.enabled {
        match experiments::disable(&ctx(state, &store, after)) {
            Ok(more) => effects.extend(more),
            Err(fail) => eprintln!("autopm kill switch failed: {:?}", api_error(fail)),
        }
    }
    if !before.enabled && after.enabled {
        match budget::on_enable(&store, after, now) {
            Ok(more) => effects.extend(more),
            Err(error) => eprintln!("autopm enable failed: {error:#}"),
        }
    }
    if before.skip_window_starting != after.skip_window_starting {
        match budget::apply_skip(&store, after, now) {
            Ok(more) => effects.extend(more),
            Err(error) => eprintln!("autopm skip failed: {error:#}"),
        }
    }
    run_effects(state, effects);
}

/// `x-sm-switches` on every response (B.1).
pub(super) async fn stamp_switches(
    State(state): State<Arc<AppState>>,
    mut response: Response,
) -> Response {
    if let Ok(value) =
        axum::http::HeaderValue::from_str(&state.autopm_switches.version().to_string())
    {
        response.headers_mut().insert("x-sm-switches", value);
    }
    response
}

/// Counts the guard's owner 5xx responses and caught handler panics (B.4).
/// A panic answers 500 instead of dropping the connection.
pub(super) async fn count_errors(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let headers = request.headers().clone();
    let peer_addr = request_peer_addr(&request);
    let response = match std::panic::AssertUnwindSafe(next.run(request))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            state
                .autopm_counters
                .panics
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    if response.status().is_server_error() && !owner_events::from_agent(&headers) {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            if owner_events::owner_surface(&state, &headers, peer_addr).is_some() {
                state
                    .autopm_counters
                    .owner_5xx
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    response
}

/// The provider and later providers of a review step.
fn step_provider(step: &Value) -> String {
    match step["kind"].as_str() {
        Some("github_codex") => "codex".into(),
        Some("paired") => step["provider"].as_str().unwrap_or("codex").into(),
        Some(kind) => kind.into(),
        None => "codex".into(),
    }
}

/// C.4's review rules for one step of `r`. Settles finished slots first,
/// so their spend is charged before the next step is weighed.
pub(super) fn review_gate(
    state: &AppState,
    r: &CodexReviewRequestRegistration,
    step: &Value,
) -> ReviewGate {
    let result = (|| -> anyhow::Result<ReviewGate> {
        let settings = typed(state, &state.session_store.owner_settings()?);
        if !settings.enabled {
            return Ok(ReviewGate::Proceed);
        }
        let store = store(state);
        settle_review_slots(state, &store)?;
        let chain: Vec<Value> =
            serde_json::from_str(r.chain_json.as_deref().unwrap_or("[]")).unwrap_or_default();
        let later: Vec<String> = chain
            .iter()
            .skip(usize::try_from(r.step_index).unwrap_or(0) + 1)
            .map(step_provider)
            .collect();
        let provider = step_provider(step);
        budget::review_gate(
            &store,
            &settings,
            &ReviewStep {
                request_id: &r.id,
                round: r.round,
                step_index: r.step_index,
                pr: r.pr_number,
                repo: &r.repo,
                provider: &provider,
                later: &later,
            },
            autopm::now_ms(),
        )
    })();
    match result {
        Ok(ReviewGate::Wait(Some(text))) => {
            let author = r
                .requester_session_id
                .clone()
                .unwrap_or_else(|| r.notify_session_id.clone());
            if let Err(error) = send(state, &author, &text) {
                eprintln!("autopm review notice to {author} failed: {error:#}");
            }
            ReviewGate::Wait(None)
        }
        Ok(gate) => gate,
        Err(error) => {
            eprintln!("autopm review gate for {} failed: {error:#}", r.id);
            ReviewGate::Proceed
        }
    }
}

/// Points a finished review step spent, from what it ran (C.4).
fn step_points(
    state: &AppState,
    slot: &budget::Slot,
    r: Option<&CodexReviewRequestRegistration>,
) -> f64 {
    let Some(r) = r else {
        return 0.0;
    };
    let chain: Vec<Value> =
        serde_json::from_str(r.chain_json.as_deref().unwrap_or("[]")).unwrap_or_default();
    let Some(step) = chain.get(usize::try_from(slot.step_index).unwrap_or(usize::MAX)) else {
        return 0.0;
    };
    let model = step["model"].as_str().unwrap_or_default();
    match step["kind"].as_str() {
        Some("github_codex") => crate::quota_rates::CLOUD_REVIEW_PERCENT,
        Some("paired") => r
            .reviewer_session_id
            .as_ref()
            .and_then(|reviewer| {
                budget::seat_points(
                    &expand_home(&state.config.usage.db_path),
                    &slot.provider,
                    &BTreeSet::from([reviewer.clone()]),
                    slot.started_at_ms,
                    autopm::now_ms() + 1,
                )
                .ok()
            })
            .map_or(0.0, |points| points.values().sum()),
        Some(kind) => {
            let dir = expand_home(&state.config.sm_send.db_path)
                .parent()
                .map(|parent| {
                    parent
                        .join("reviews")
                        .join(&r.id)
                        .join(format!("r{}-s{}", slot.round, slot.step_index))
                });
            let tokens = dir.and_then(|dir| {
                if kind == "codex" {
                    std::fs::read_to_string(dir.join("events.jsonl"))
                        .ok()
                        .map(|text| codex_run_tokens(&text))
                } else {
                    std::fs::read_to_string(dir.join("result.json"))
                        .ok()
                        .map(|text| claude_run_tokens(&text))
                }
            });
            tokens.map_or(0.0, |tokens| budget::run_points(kind, model, &tokens))
        }
        None => 0.0,
    }
}

/// Token use of a `codex exec --json` run: every `turn.completed` usage.
pub(super) fn codex_run_tokens(events: &str) -> crate::quota_rates::Tokens {
    let mut tokens = crate::quota_rates::Tokens::default();
    for line in events.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event["type"] != "turn.completed" {
            continue;
        }
        let usage = &event["usage"];
        let cached = usage["cached_input_tokens"].as_i64().unwrap_or(0);
        tokens.input += usage["input_tokens"].as_i64().unwrap_or(0) - cached;
        tokens.cache_read += cached;
        tokens.output += usage["output_tokens"].as_i64().unwrap_or(0);
    }
    tokens
}

/// Token use of a `claude -p --output-format json` run.
pub(super) fn claude_run_tokens(result: &str) -> crate::quota_rates::Tokens {
    let usage = serde_json::from_str::<Value>(result).unwrap_or_default()["usage"].clone();
    crate::quota_rates::Tokens {
        input: usage["input_tokens"].as_i64().unwrap_or(0),
        output: usage["output_tokens"].as_i64().unwrap_or(0),
        cache_write_5m: usage["cache_creation_input_tokens"].as_i64().unwrap_or(0),
        cache_write_1h: 0,
        cache_read: usage["cache_read_input_tokens"].as_i64().unwrap_or(0),
    }
}

/// Ends every slot whose step is over and charges what it spent.
fn settle_review_slots(state: &AppState, store: &AutopmStore) -> anyhow::Result<()> {
    let db = expand_home(&state.config.sm_send.db_path);
    for slot in budget::slots(&store.open()?)? {
        let r = RetainedQueueStore::get_codex_review_request_from_path(&db, &slot.request_id)?;
        let current = r.as_ref().is_some_and(|r| {
            r.is_active && r.round == slot.round && r.step_index == slot.step_index
        });
        if !current {
            let points = step_points(state, &slot, r.as_ref());
            budget::finish_slot(store, &slot, points, autopm::now_ms())?;
        }
    }
    Ok(())
}

/// A counted autopm app deploy's claimed weekly slot (B.3). Dropping it
/// before [`DeployClaim::finish`] frees the slot, so a deploy that fails
/// partway does not use up the week.
pub(super) struct DeployClaim {
    store: AutopmStore,
    slot: autopm::DeploySlot,
    finished: bool,
}

impl DeployClaim {
    /// A counted deploy must name its version, or its experiment could
    /// never see the app installed.
    pub(super) fn check_version(&self, version_name: Option<&str>) -> Result<(), ApiError> {
        if version_name.is_some_and(|v| experiments::version_at_least(v, "0") == Some(true)) {
            return Ok(());
        }
        Err(ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "version_name: an autopm app deploy needs a dotted numeric version_name".into(),
        })
    }

    pub(super) fn finish(mut self, hash: &str, version_name: &str) {
        self.finished = true;
        if let Err(error) = self.store.finish_app_deploy(&self.slot, hash, version_name) {
            eprintln!(
                "autopm app deploy for #{} not recorded: {error:#}",
                self.slot.ticket
            );
        }
    }
}

impl Drop for DeployClaim {
    fn drop(&mut self) {
        if !self.finished {
            if let Err(error) = self.store.release_app_deploy(&self.slot) {
                eprintln!("autopm app deploy slot not released: {error:#}");
            }
        }
    }
}

/// The Android deploy cap (B.3) for a deploy from an agent's shell.
pub(super) fn app_deploy_cap(
    state: &AppState,
    app_name: &str,
    headers: &HeaderMap,
) -> Result<Option<DeployClaim>, ApiError> {
    if app_name != "session-manager-android" {
        return Ok(None);
    }
    let Some(session) = header_text(headers, "x-sm-session-id") else {
        return Ok(None);
    };
    let settings = settings(state)?;
    let store = store(state);
    Ok(store
        .claim_app_deploy(&settings, &session, autopm::now_ms())
        .map_err(api_error)?
        .map(|slot| DeployClaim {
            store,
            slot,
            finished: false,
        }))
}

/// The owner devices' reported app versions.
fn owner_app_versions(state: &AppState) -> Vec<String> {
    let path = crate::owner_push::push_db_path(&state.config);
    let Ok(Some(conn)) = autopm::open_read_only(&path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT DISTINCT app_version FROM push_tokens") else {
        return Vec::new();
    };
    stmt.query_map([], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .unwrap_or_default()
}

fn agent_provider(state: &AppState, session_id: &str) -> Option<String> {
    let session = state.session_store.get_session(session_id).ok()??;
    budget::provider_of_agent(&session.provider).map(str::to_owned)
}

/// One minute of upkeep: the guard's sample and evaluation, the budget, and
/// (every five minutes) sm's own experiment moves.
pub(super) fn minute(
    state: &AppState,
    minute_ms: i64,
    experiments_due: bool,
) -> anyhow::Result<()> {
    let store = store(state);
    let settings = typed(state, &state.session_store.owner_settings()?);
    state.autopm_switches.set_enabled(settings.enabled);
    let mut effects = Vec::new();
    let guard_db = guard::open(&store.paths.utilization_db)?;
    guard::write_minute(&guard_db, minute_ms, state.autopm_counters.take())?;
    if settings.enabled {
        let trips = guard::trips(&guard_db, minute_ms)?;
        if !trips.is_empty() {
            effects.extend(
                experiments::guard_trip(&ctx(state, &store, &settings), &trips)
                    .map_err(|fail| anyhow::anyhow!("{:?}", api_error(fail)))?,
            );
        }
    }
    autopm_seat(state, &store);
    effects.extend(budget::tick(
        &store,
        &settings,
        autopm::now_ms(),
        &|session| agent_provider(state, session),
    )?);
    settle_review_slots(state, &store)?;
    if experiments_due {
        let context = ctx(state, &store, &settings);
        effects.extend(
            experiments::tick(&context, &owner_app_versions(state))
                .map_err(|fail| anyhow::anyhow!("{:?}", api_error(fail)))?,
        );
        effects.extend(
            experiments::cancelled_cleanup_prs(&store.open()?, &settings.owner)
                .map_err(|fail| anyhow::anyhow!("{:?}", api_error(fail)))?,
        );
    }
    run_effects(state, effects);
    Ok(())
}

/// Runs [`minute`] at each minute boundary.
pub(super) fn spawn_ticker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut last_experiments = std::time::Instant::now()
            .checked_sub(EXPERIMENT_TICK)
            .unwrap_or_else(std::time::Instant::now);
        loop {
            let now = autopm::now_ms();
            let next = (now / guard::MINUTE_MS + 1) * guard::MINUTE_MS;
            tokio::time::sleep(Duration::from_millis(
                u64::try_from(next - now).unwrap_or(0),
            ))
            .await;
            if state.shutdown().is_stopped() {
                return;
            }
            let due = last_experiments.elapsed() >= EXPERIMENT_TICK;
            if due {
                last_experiments = std::time::Instant::now();
            }
            let state = state.clone();
            let minute_ms = next - guard::MINUTE_MS;
            match tokio::task::spawn_blocking(move || minute(&state, minute_ms, due)).await {
                Ok(Err(error)) => eprintln!("autopm minute failed: {error:#}"),
                Err(error) => eprintln!("autopm minute failed: {error}"),
                Ok(Ok(())) => {}
            }
        }
    });
}

#[cfg(test)]
mod tests;
