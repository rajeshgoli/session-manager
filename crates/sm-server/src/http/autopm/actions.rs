//! Auto PM's narrow issue/start/ask powers and durable external effects.
use super::*;
use crate::autopm::{ask, cycle};
use rusqlite::params;
use serde::Serialize;

// HTTP writes and the ticker share this lock; no SQLite transaction spans network I/O.
pub(super) static WORK: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub(super) struct Runtime<'a>(pub &'a AppState);
fn bad(gate: &str, text: &str) -> ApiError {
    api_error(autopm::Refusal::bad(gate, text).into())
}
fn refused(gate: &str, text: &str) -> ApiError {
    api_error(autopm::Refusal::gate(gate, text).into())
}
fn role_only(state: &AppState, request: &Request) -> Result<String, ApiError> {
    if role::actor(state, request)? != Actor::Autopm {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "requires the autopm role".into(),
        });
    }
    autopm_seat(state, &store(state)).ok_or_else(|| refused("role", "autopm role disappeared"))
}
pub(super) fn schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    ask::schema(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS autopm_filings (
        key TEXT PRIMARY KEY, payload TEXT NOT NULL, created_ms INTEGER NOT NULL,
        number INTEGER, labelled INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS autopm_starts (
        number INTEGER PRIMARY KEY, payload TEXT NOT NULL);",
    )?;
    Ok(())
}

fn gh(args: Vec<String>) -> anyhow::Result<()> {
    let out = gh_command_output(&args, Duration::from_secs(30)).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(out.status.success(), "{}", command_stderr(&out));
    Ok(())
}

/// The stable key is in both SQLite and GitHub's body, so a timeout after creation
/// reconciles the remote issue instead of creating another.
pub(super) fn file(
    state: &AppState,
    key: &str,
    repo: &str,
    title: &str,
    body: &str,
    label: &str,
) -> anyhow::Result<i64> {
    crate::owner_docs::validate_repo_slug(repo)?;
    let conn = store(state).open()?;
    schema(&conn)?;
    let payload = json!({"repo":repo,"title":title,"body":body,"label":label}).to_string();
    conn.execute(
        "INSERT OR IGNORE INTO autopm_filings(key,payload,created_ms) VALUES(?1,?2,?3)",
        params![key, payload, autopm::now_ms()],
    )?;
    let (saved, at, number, labelled): (String, i64, Option<i64>, bool) = conn.query_row(
        "SELECT payload,created_ms,number,labelled FROM autopm_filings WHERE key=?1",
        [key],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    anyhow::ensure!(
        saved == payload,
        "request id already belongs to different issue content"
    );
    let marker = format!("<!-- sm-autopm:{key} -->");
    let number = if let Some(n) = number {
        n
    } else {
        let mut found = None;
        for page in 1.. {
            let rows=gh_api_json(repo,&format!("issues?state=all&sort=created&direction=desc&per_page=100&page={page}&since={}",autopm::format_ms(at-autopm::DAY_MS)),false).map_err(anyhow::Error::msg)?;
            let rows = rows
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("invalid issue response"))?;
            if let Some(v) = rows.iter().find(|v| {
                v["pull_request"].is_null()
                    && v["body"].as_str().is_some_and(|b| b.contains(&marker))
            }) {
                found = v["number"].as_i64();
                break;
            }
            if rows.len() < 100 {
                break;
            }
        }
        let n = match found {
            Some(n) => n,
            None => {
                state
                    .board_source
                    .create_issue(repo, title, &format!("{body}\n\n{marker}"))
                    .map_err(anyhow::Error::msg)?
                    .0
            }
        };
        conn.execute(
            "UPDATE autopm_filings SET number=?1 WHERE key=?2",
            params![n, key],
        )?;
        n
    };
    if !labelled {
        gh([
            "label", "create", label, "--repo", repo, "--color", "5319e7", "--force",
        ]
        .map(str::to_owned)
        .to_vec())?;
        gh(vec![
            "issue".into(),
            "edit".into(),
            number.to_string(),
            "--repo".into(),
            repo.into(),
            "--add-label".into(),
            label.into(),
        ])?;
        conn.execute("UPDATE autopm_filings SET labelled=1 WHERE key=?1", [key])?;
    }
    Ok(number)
}

pub(super) fn attach(state: &AppState, number: i64, parent: i64) -> Result<(), ApiError> {
    crate::board::write_link(
        &board::board_store(state),
        state.board_source.as_ref(),
        &crate::board::LinkRequest {
            ticket: (autopm::REPO.into(), number),
            target: (autopm::REPO.into(), parent),
            kind: crate::board::model::EdgeKind::SubIssue,
            remove: false,
            actor: "sm:autopm".into(),
            actor_name: "Auto PM".into(),
        },
        time::OffsetDateTime::now_utc(),
    )?
    .map_err(|e| refused("board", &format!("{e:?}")))?;
    board::request_recompute(state);
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Issue {
    request_id: String,
    title: String,
    body: String,
    experiment_id: Option<String>,
    kind: Option<String>,
}

pub(in crate::http) async fn issue(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    role_only(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    let body: Issue = serde_json::from_value(json_body(request).await?)
        .map_err(|e| bad("body", &e.to_string()))?;
    blocking(move || {
        let _work = WORK
            .lock()
            .map_err(|_| anyhow::anyhow!("autopm lock poisoned"))?;
        let store = store(&state);
        let settings = settings(&state)?;
        budget::starts_allowed(&settings, false).map_err(api_error)?;
        if !super::super::messages::valid_submission_id(&body.request_id) {
            return Err(bad(
                "request_id",
                "use 8–64 letters, digits, hyphens or underscores",
            ));
        }
        if body.title.trim().is_empty()
            || body.title.chars().count() > 256
            || body.body.trim().is_empty()
        {
            return Err(bad(
                "issue",
                "title and body required; title max 256 characters",
            ));
        }
        if !body
            .body
            .lines()
            .any(|l| l.trim_start().starts_with("Tier:"))
        {
            return Err(bad("tier", "body needs a Tier: line"));
        }
        let context = ctx(&state, &store, &settings);
        match (body.kind.as_deref(), body.experiment_id.as_deref()) {
            (Some("fix"), None) => {
                experiments::gate_fix_issue(&body.body).map_err(|e| api_error(e.into()))?
            }
            (None | Some("experiment"), Some(id)) => {
                experiments::gate_issue(&context, id, &body.body).map_err(api_error)?
            }
            _ => {
                return Err(bad(
                    "kind",
                    "supply experiment_id, or kind=fix without an experiment",
                ))
            }
        }
        let cycle = cycle::current(&store)?
            .filter(|c| c.ready && c.ended_ms.is_none())
            .and_then(|c| c.ticket)
            .ok_or_else(|| refused("cycle", "no current cycle"))?;
        let number = file(
            &state,
            &format!("issue:{}", body.request_id),
            autopm::REPO,
            &body.title,
            &body.body,
            "autopm",
        )?;
        attach(&state, number, cycle)?;
        if let Some(id) = body.experiment_id {
            let x = experiments::get(&store.open()?, &id)?
                .ok_or_else(|| refused("experiment", "experiment disappeared"))?;
            let mut tickets = x.tickets();
            if !tickets.contains(&number) {
                tickets.push(number);
            }
            experiments::patch(
                &context,
                Actor::Autopm,
                &id,
                &json!({"tickets_json":tickets}),
            )
            .map_err(api_error)?;
        } else {
            store.record_fix(number, "fix", Some(cycle), None, autopm::now_ms())?;
        }
        Ok(Json(
            json!({"number":number,"repo":autopm::REPO,"cycle_ticket":cycle}),
        ))
    })
    .await
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Start {
    pub number: i64,
    pub experiment_id: Option<String>,
    pub provider: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub brief: Option<String>,
    pub points_est: f64,
}

pub(in crate::http) async fn start(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    role_only(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    let body: Start = serde_json::from_value(json_body(request).await?)
        .map_err(|e| bad("body", &e.to_string()))?;
    let _launch = state.board_auto_start_gate.lock().await;
    let work = state.clone();
    blocking(move || {
        let _work = WORK
            .lock()
            .map_err(|_| anyhow::anyhow!("autopm lock poisoned"))?;
        authorize(&work, &body)?;
        Ok(Json(json!({"number":body.number,"authorized":true})))
    })
    .await
}

/// Also used by the server's revert/cleanup producer. Caller serializes board starts.
pub(super) fn authorize(state: &AppState, body: &Start) -> Result<(), ApiError> {
    let config = crate::board::launch::Config {
        provider: body.provider.clone(),
        model: body.model.clone(),
        reasoning_effort: body.reasoning_effort.clone(),
        brief: body.brief.clone(),
    };
    if let Some(error) = config.validate() {
        return Err(bad("agent", error));
    }
    if !body.points_est.is_finite() || body.points_est <= 0.0 {
        return Err(bad("points_est", "must be positive"));
    }
    let store = store(state);
    let conn = store.open()?;
    schema(&conn)?;
    if !autopm::tickets_under_goal(&conn)?.contains(&body.number)
        || !autopm::ticket_open(&conn, body.number)?
    {
        return Err(refused("ticket", "ticket must be open and under #2170"));
    }
    if !autopm::live_holders(&conn, body.number)?.is_empty() {
        return Err(refused("claimed", "ticket is already claimed"));
    }
    let settings = settings(state)?;
    let context = ctx(state, &store, &settings);
    let fix = autopm::fix(&conn, body.number)?;
    let revert = fix
        .as_ref()
        .is_some_and(|f| f.kind == "revert" && f.cancelled_at_ms.is_none());
    if !revert {
        budget::starts_allowed(&settings, fix.as_ref().is_some_and(|f| f.kind == "cleanup"))
            .map_err(api_error)?;
    }
    let encoded = serde_json::to_string(body)?;
    let old: Option<String> = conn
        .query_row(
            "SELECT payload FROM autopm_starts WHERE number=?1",
            [body.number],
            |r| r.get(0),
        )
        .optional()?;
    if old.as_ref().is_some_and(|old| *old != encoded) {
        return Err(refused(
            "authorization",
            "ticket already has a different Auto PM authorization",
        ));
    }
    let provider = budget::provider_of_agent(&body.provider)
        .ok_or_else(|| bad("provider", "unsupported provider"))?;
    let window = budget::current(&conn, &store.paths.usage_db, provider)?;
    if !revert
        && window
            .as_ref()
            .is_some_and(|w| budget::skipped(&settings, w))
    {
        return Err(refused("skipped", "this provider window is skipped"));
    }
    // Admission and authorization are separate durable writes. Recover the gap
    // across providers, including suspended/rolled reservations, before admitting.
    let incompatible: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM autopm_reservations
         WHERE repo=?1 AND number=?2 AND released_at_ms IS NULL
         AND (provider != ?3 OR ABS(COALESCE(original_points,points)-?4) > ?5))",
        params![
            autopm::REPO,
            body.number,
            provider,
            body.points_est,
            f64::EPSILON
        ],
        |r| r.get(0),
    )?;
    if !revert && old.is_none() && incompatible {
        return Err(refused(
            "reservation",
            "retry must use the admitted provider and points estimate",
        ));
    }
    let reserved = budget::current(&conn, &store.paths.usage_db, provider)?
        .map(|w| budget::holds(&conn, provider, &w.window_start))
        .transpose()?
        .unwrap_or_default()
        .iter()
        .any(|r| r.number == body.number && r.repo == autopm::REPO && r.suspended_at_ms.is_none());
    // Validate the plan/fix even on retry; reservation recovery does not bypass its gate.
    if let Some(id) = body.experiment_id.as_deref() {
        let x = experiments::get(&conn, id)?
            .ok_or_else(|| refused("experiment", "unknown experiment"))?;
        if !x.tickets().contains(&body.number) {
            return Err(refused("ticket", "not an experiment build ticket"));
        }
        experiments::plan_merged(&context, &x).map_err(api_error)?;
        if !reserved {
            budget::admit_experiment_start(&context, id, body.number, provider, body.points_est)
                .map_err(api_error)?;
        }
    } else {
        if fix.as_ref().is_none_or(|f| f.cancelled_at_ms.is_some()) {
            return Err(refused("fix", "no active fix ticket"));
        }
        if !reserved || revert {
            budget::admit_fix_start(&context, body.number, provider, body.points_est)
                .map_err(api_error)?;
        }
    }
    conn.execute(
        "INSERT OR IGNORE INTO autopm_starts VALUES(?1,?2)",
        params![body.number, encoded],
    )?;
    let board = board::board_store(state);
    if !board.auto_starts()?.iter().any(|r| {
        r.number == body.number
            && r.repo == autopm::REPO
            && matches!(r.state.as_str(), "waiting" | "started")
    }) {
        board.authorize_auto_starts(
            &[crate::board::auto_start::Choice {
                repo: autopm::REPO.into(),
                number: body.number,
                agent_type: None,
                provider: body.provider.clone(),
                model: body.model.clone(),
                reasoning_effort: body.reasoning_effort.clone(),
                brief: body.brief.clone(),
            }],
            time::OffsetDateTime::now_utc(),
        )?;
    }
    board::request_recompute(state);
    Ok(())
}

pub(in crate::http) fn may_dispatch(
    state: &AppState,
    record: &crate::board::auto_start::Record,
) -> anyhow::Result<bool> {
    if record.repo != autopm::REPO {
        return Ok(true);
    }
    let store = store(state);
    let conn = store.open()?;
    schema(&conn)?;
    let ours: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM autopm_starts WHERE number=?1)",
        [record.number],
        |r| r.get(0),
    )?;
    if !ours {
        return Ok(true);
    }
    let fix = autopm::fix(&conn, record.number)?;
    if !super::recovery::current(state, record.number)? {
        return Ok(false);
    }
    if fix.as_ref().is_some_and(|f| f.cancelled_at_ms.is_some()) {
        return Ok(false);
    }
    if fix.as_ref().is_some_and(|f| f.kind == "revert") {
        return Ok(true);
    }
    let settings = typed(state, &state.session_store.owner_settings()?);
    if !settings.enabled
        || (settings.paused_at.is_some() && fix.is_none_or(|f| f.kind != "cleanup"))
    {
        return Ok(false);
    }
    let Some(provider) = budget::provider_of_agent(&record.provider) else {
        return Ok(false);
    };
    let Some(window) = budget::current(&conn, &store.paths.usage_db, provider)? else {
        return Ok(false);
    };
    if budget::skipped(&settings, &window) {
        return Ok(false);
    }
    Ok(budget::holds(&conn, provider, &window.window_start)?
        .iter()
        .any(|r| {
            r.repo == autopm::REPO && r.number == record.number && r.suspended_at_ms.is_none()
        }))
}

impl ask::Effects for Runtime<'_> {
    fn message(&self, key: &str, sender: &str, title: &str, body: &str) -> anyhow::Result<String> {
        let state = self.0;
        match super::super::messages::owner_message_store(state).create_once(
            crate::owner_messages::NewOwnerMessage {
                human: state.config.owner_name.clone(),
                sender_session_id: sender.into(),
                sender_session_name: "Auto PM".into(),
                title: title.into(),
                body_markdown: body.into(),
                blocking: false,
                blocked: false,
            },
            Some(key),
        )? {
            crate::owner_messages::CreateOwnerMessage::Created(message) => {
                super::super::follows::notice_new_message(state, &message);
                Ok(message.id)
            }
            crate::owner_messages::CreateOwnerMessage::UnreadCapReached => {
                anyhow::bail!("owner message unread cap reached")
            }
        }
    }
    fn clarify(&self, key: &str, text: &str) -> anyhow::Result<()> {
        if let Some(id) = autopm_seat(self.0, &store(self.0)) {
            cycles::once(self.0, key, &id, text)?;
        } else {
            anyhow::bail!("no autopm seat for clarification");
        }
        Ok(())
    }
    fn proposal(&self, x: &experiments::Experiment) -> anyhow::Result<(String, i64)> {
        let repo = x
            .target_repo
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("proposal has no target repo"))?;
        let body = format!(
            "Auto PM proposal from session-manager experiment {}. For {} to schedule.\n\n{}",
            x.id,
            self.0.config.owner_name,
            x.proposal_md.as_deref().unwrap_or_default()
        );
        let n = file(
            self.0,
            &format!("proposal:{}", x.id),
            repo,
            &x.title,
            &body,
            "autopm-proposal",
        )?;
        Ok((repo.clone(), n))
    }
}

pub(in crate::http) async fn ask(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let sender = role_only(&state, &request)?;
    ensure_core_writes_enabled(&state)?;
    let body = json_body(request).await?;
    let decisions: Vec<ask::Decision> = serde_json::from_value(body["decisions"].clone())
        .map_err(|e| bad("decisions", &e.to_string()))?;
    blocking(move || {
        let _work = WORK
            .lock()
            .map_err(|_| anyhow::anyhow!("autopm lock poisoned"))?;
        ask::validate(&decisions).map_err(api_error)?;
        let store = store(&state);
        let settings = settings(&state)?;
        budget::starts_allowed(&settings, false).map_err(api_error)?;
        let cycle = cycle::current(&store)?
            .filter(|c| c.ready && c.ended_ms.is_none())
            .and_then(|c| c.ticket)
            .ok_or_else(|| refused("cycle", "no current cycle"))?;
        let id = ask::send(
            &ctx(&state, &store, &settings),
            cycle,
            &sender,
            &decisions,
            &Runtime(&state),
        )
        .map_err(api_error)?;
        Ok(Json(json!({"message_id":id})))
    })
    .await
}

pub(super) fn tick(state: &AppState) -> anyhow::Result<()> {
    let _work = WORK
        .lock()
        .map_err(|_| anyhow::anyhow!("autopm lock poisoned"))?;
    let store = store(state);
    let conn = store.open()?;
    schema(&conn)?;
    let settings = typed(state, &state.session_store.owner_settings()?);
    let context = ctx(state, &store, &settings);
    let effects = Runtime(state);
    // Complete an accepted ask whose durable message write was interrupted.
    let pending = {
        let mut s = conn
            .prepare("SELECT cycle,sender,decisions FROM autopm_asks WHERE message_id IS NULL")?;
        let rows = s
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (cycle, sender, encoded) in pending {
        ask::send(
            &context,
            cycle,
            &sender,
            &serde_json::from_str::<Vec<ask::Decision>>(&encoded)?,
            &effects,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let ids = {
        let mut s =
            conn.prepare("SELECT message_id FROM autopm_asks WHERE message_id IS NOT NULL")?;
        let rows = s
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for id in ids {
        for reply in super::super::messages::owner_message_store(state).replies(&id)? {
            let mut text = reply.body;
            for comment in reply.comments {
                text.push('\n');
                text.push_str(&comment.body);
            }
            ask::reply(
                &context,
                &id,
                &reply.id,
                &text,
                autopm::parse_ms(&reply.created_at).unwrap_or(context.now),
                &effects,
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
    }
    ask::apply(&context, &effects).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::role_state;
    use super::*;

    fn setup() -> AppState {
        let state = role_state();
        state
            .session_store
            .update_owner_settings(&json!({"autopm":{"enabled":true}}), |_| Ok(Ok(())))
            .unwrap()
            .unwrap();
        let conn = store(&state).open().unwrap();
        crate::board::init_board_schema(&conn).unwrap();
        crate::work_claims::init_work_claims_schema(&conn).unwrap();
        for n in [3001, 3002] {
            conn.execute("INSERT INTO board_items(repo,number,title,url,state,first_seen_at,synced_at) VALUES(?1,?2,'fix','url','OPEN','now','now')",params![autopm::REPO,n]).unwrap();
            conn.execute("INSERT INTO board_edges(waiter_repo,waiter_number,blocker_repo,blocker_number,kind,first_seen_at,source) VALUES(?1,2170,?1,?2,'sub_issue','now','test')",params![autopm::REPO,n]).unwrap();
            store(&state)
                .record_fix(n, "fix", Some(3000), None, autopm::now_ms())
                .unwrap();
        }
        let usage = rusqlite::Connection::open(&state.config.usage.db_path).unwrap();
        usage.execute_batch("CREATE TABLE accounts(account_key TEXT,provider TEXT,last_seen TEXT);
            CREATE TABLE burn_samples(id INTEGER PRIMARY KEY,account_key TEXT,window_kind TEXT,window_scope TEXT,window_start TEXT,resets_at TEXT,percent REAL,observed_at TEXT);
            CREATE TABLE seat_tokens(seat_id TEXT,account_key TEXT,window_kind TEXT,bucket_ts TEXT,model TEXT,input_tokens INTEGER,output_tokens INTEGER,cache_write_5m INTEGER,cache_write_1h INTEGER,cache_read_tokens INTEGER);
            INSERT INTO accounts VALUES('test','codex','now');").unwrap();
        let now = autopm::now_ms();
        usage
            .execute(
                "INSERT INTO burn_samples VALUES(1,'test','codex_10080',NULL,?1,?2,0,?3)",
                params![
                    autopm::format_ms(now - autopm::DAY_MS),
                    autopm::format_ms(now + 6 * autopm::DAY_MS),
                    autopm::format_ms(now)
                ],
            )
            .unwrap();
        budget::on_enable(
            &store(&state),
            &typed(&state, &state.session_store.owner_settings().unwrap()),
            now,
        )
        .unwrap();
        state
    }
    fn choice(number: i64, points: f64) -> Start {
        Start {
            number,
            experiment_id: None,
            provider: "codex-fork".into(),
            model: Some("gpt-6-sol".into()),
            reasoning_effort: Some("medium".into()),
            brief: None,
            points_est: points,
        }
    }

    #[test]
    fn admission_retry_and_dispatch_recheck_skip_pause_and_cleanup_cancellation() {
        let state = setup();
        assert!(authorize(&state, &choice(3001, 999.0)).is_err());
        let count: i64 = store(&state)
            .open()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM autopm_starts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "rejected estimates must not poison a later corrected request"
        );
        authorize(&state, &choice(3001, 2.0)).unwrap();
        authorize(&state, &choice(3001, 2.0)).unwrap();
        let conn = store(&state).open().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM autopm_reservations WHERE number=3001",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let record = board::board_store(&state).auto_starts().unwrap().remove(0);
        assert!(may_dispatch(&state, &record).unwrap());
        state
            .session_store
            .update_owner_settings(
                &json!({"autopm":{"skip_window_starting":autopm::format_ms(autopm::now_ms())}}),
                |_| Ok(Ok(())),
            )
            .unwrap()
            .unwrap();
        assert!(!may_dispatch(&state, &record).unwrap());
        assert!(authorize(&state, &choice(3001, 2.0)).is_err());
        state.session_store.update_owner_settings(&json!({"autopm":{"skip_window_starting":null,"paused_at":autopm::format_ms(autopm::now_ms())}}), |_|Ok(Ok(()))).unwrap().unwrap();
        assert!(!may_dispatch(&state, &record).unwrap());
        conn.execute(
            "UPDATE autopm_fixes SET kind='cleanup' WHERE number=3001",
            [],
        )
        .unwrap();
        assert!(may_dispatch(&state, &record).unwrap());
        conn.execute(
            "UPDATE autopm_fixes SET cancelled_at_ms=1 WHERE number=3001",
            [],
        )
        .unwrap();
        assert!(!may_dispatch(&state, &record).unwrap());
        // Revert recovery uses the same authorization function but no budget or pause gate.
        conn.execute(
            "UPDATE autopm_fixes SET kind='revert' WHERE number=3002",
            [],
        )
        .unwrap();
        state
            .session_store
            .update_owner_settings(&json!({"autopm":{"enabled":false}}), |_| Ok(Ok(())))
            .unwrap()
            .unwrap();
        authorize(&state, &choice(3002, 1.0)).unwrap();
        let revert = board::board_store(&state)
            .auto_starts()
            .unwrap()
            .into_iter()
            .find(|r| r.number == 3002)
            .unwrap();
        assert!(may_dispatch(&state, &revert).unwrap());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM autopm_reservations WHERE number=3002",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn admission_crash_rejects_changed_provider_or_estimate() {
        let state = setup();
        let store = store(&state);
        let settings = settings(&state).unwrap();
        budget::admit_fix_start(&ctx(&state, &store, &settings), 3001, "codex", 2.0).unwrap();
        // Crash before autopm_starts exists: no saved authorization to compare.
        let mut retry = choice(3001, 2.0);
        retry.provider = "claude".into();
        retry.model = Some("opus".into());
        retry.reasoning_effort = Some("high".into());
        let error = authorize(&state, &retry).unwrap_err();
        assert!(format!("{error:?}").contains("admitted provider"));
        assert!(authorize(&state, &choice(3001, 3.0)).is_err());
        authorize(&state, &choice(3001, 2.0)).unwrap();
        let count: i64 = store
            .open()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM autopm_reservations WHERE number=3001",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn filing_retry_returns_recorded_target_issue_without_another_remote_effect() {
        let state = setup();
        let conn = store(&state).open().unwrap();
        schema(&conn).unwrap();
        let payload=json!({"repo":"owner/other","title":"A proposal","body":"Evidence","label":"autopm-proposal"}).to_string();
        conn.execute(
            "INSERT INTO autopm_filings VALUES('proposal:test',?1,1,42,1)",
            [payload],
        )
        .unwrap();
        assert_eq!(
            file(
                &state,
                "proposal:test",
                "owner/other",
                "A proposal",
                "Evidence",
                "autopm-proposal"
            )
            .unwrap(),
            42
        );
        assert!(file(
            &state,
            "proposal:test",
            "owner/other",
            "Changed",
            "Evidence",
            "autopm-proposal"
        )
        .is_err());
    }
}
