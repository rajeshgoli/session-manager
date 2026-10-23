//! Runtime adapter for cycle effects. Only the minute ticker creates cycles.
use super::*;
use crate::autopm::cycle::{self, Actions, Cycle};
use rusqlite::params;

struct Runtime<'a>(&'a AppState);

fn gh(args: &[String]) -> anyhow::Result<String> {
    let out = gh_command_output(args, Duration::from_secs(30)).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(out.status.success(), "{}", command_stderr(&out));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    cycle::schema(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS autopm_cycle_seats (
        ticket INTEGER PRIMARY KEY, session_id TEXT NOT NULL UNIQUE, brief TEXT NOT NULL);",
    )?;
    Ok(())
}

pub(in crate::http) fn is_cycle(state: &AppState, repo: &str, ticket: i64) -> anyhow::Result<bool> {
    if repo != autopm::REPO {
        return Ok(false);
    }
    let conn = store(state).open()?;
    schema(&conn)?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM autopm_cycles WHERE ticket=?1 AND ready=1)",
        [ticket],
        |r| r.get(0),
    )?)
}

fn skipped(state: &AppState, settings: &Settings) -> anyhow::Result<bool> {
    let store = store(state);
    let conn = store.open()?;
    // Skip is expressed in the Claude quota window in the owner setting.
    Ok(budget::current(&conn, &store.paths.usage_db, "claude")?
        .is_some_and(|window| budget::skipped(settings, &window)))
}

pub(in crate::http) fn may_start(state: &AppState) -> anyhow::Result<bool> {
    let settings = settings(state).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    if !settings.enabled || settings.paused_at.is_some() || skipped(state, &settings)? {
        return Ok(false);
    }
    if autopm_seat(state, &store(state)).is_some() {
        return Ok(false);
    }
    if pending_seat(state)? {
        return Ok(false);
    }
    let owner = state.session_store.owner_settings()?;
    let provider = budget::provider_of_agent(
        owner["autopm"]["agent"]["provider"]
            .as_str()
            .unwrap_or("claude"),
    )
    .unwrap_or("claude");
    let store = store(state);
    Ok(budget::balance(
        &store.open()?,
        &store.paths.usage_db,
        &settings,
        provider,
        autopm::now_ms(),
    )?
    .is_some_and(|balance| balance.left(budget::PM) > 0.0))
}

fn pending_seat(state: &AppState) -> anyhow::Result<bool> {
    let conn = store(state).open()?;
    schema(&conn)?;
    let mut stmt = conn.prepare("SELECT session_id FROM autopm_cycle_seats")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        if state
            .session_store
            .get_session(&id)?
            .is_some_and(|s| !s.is_stopped())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Persist before spawning. The ticker can authorize and deliver after a server restart.
pub(in crate::http) fn prepare(
    state: &AppState,
    ticket: i64,
    id: &str,
    brief: &str,
) -> anyhow::Result<()> {
    let conn = store(state).open()?;
    schema(&conn)?;
    conn.execute(
        "INSERT INTO autopm_cycle_seats(ticket,session_id,brief) VALUES(?1,?2,?3)
        ON CONFLICT(ticket) DO UPDATE SET session_id=excluded.session_id,brief=excluded.brief",
        params![ticket, id, brief],
    )?;
    Ok(())
}

pub(in crate::http) fn deliver(state: &AppState, ticket: i64) -> anyhow::Result<bool> {
    let conn = store(state).open()?;
    schema(&conn)?;
    let pending: Option<(String, String)> = conn
        .query_row(
            "SELECT session_id,brief FROM autopm_cycle_seats WHERE ticket=?1",
            [ticket],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((id, brief)) = pending else {
        return Ok(false);
    };
    let Some(session) = state.session_store.get_session(&id)? else {
        return Ok(false);
    };
    let settings = typed(state, &state.session_store.owner_settings()?);
    if !settings.enabled || settings.paused_at.is_some() || skipped(state, &settings)? {
        return Ok(true);
    }
    let key = format!("autopm-brief-{id}");
    if RetainedQueueStore::new(expand_home(&state.config.sm_send.db_path))
        .message_delivered(&key)?
    {
        // A historical stopped seat must not satisfy start() for a new cycle.
        return Ok(!session.is_stopped());
    }
    if super::super::auto_retire::retired_autopm_needs_successor(state, &session) {
        // The cycle wake below remains durably addressed to the retired
        // holder; the message-ready path creates its successor before it is
        // delivered.
        once(state, &key, &id, &brief)?;
        return Ok(true);
    }
    if super::super::messages::restores(&session) {
        let Some(session) = super::super::auto_retire::ready_for_message(state, session) else {
            return Ok(false);
        };
        state.session_store.authorize_autopm_seat(&session.id)?;
        store(state).record_seat(&session.id, autopm::now_ms())?;
        once(state, &key, &id, &brief)?;
        return Ok(true);
    }
    if session.is_stopped() {
        return Ok(false);
    }
    let Some(session) = super::super::auto_retire::ready_for_message(state, session) else {
        return Ok(false);
    };
    let id = session.id;
    state.session_store.authorize_autopm_seat(&id)?;
    store(state).record_seat(&id, autopm::now_ms())?;
    once(state, &key, &id, &brief)?;
    Ok(true)
}

pub(super) fn once(state: &AppState, key: &str, target: &str, text: &str) -> anyhow::Result<()> {
    // A delivered once-keyed wake must never reopen a retired seat. An
    // undelivered wake still needs its existing queue target made ready.
    let conn = store(state).open()?;
    let initialized: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2)",
        ["table", "message_queue"],
        |r| r.get(0),
    )?;
    let existing: Option<(String, bool)> = if initialized {
        conn.query_row(
            "SELECT target_session_id, delivered_at IS NOT NULL FROM message_queue WHERE id=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    } else {
        None
    };
    if existing.as_ref().is_some_and(|(_, delivered)| *delivered) {
        return Ok(());
    }
    let target = existing.as_ref().map_or(target, |(id, _)| id.as_str());
    let target = match state.session_store.get_session(target)? {
        Some(session)
            if super::super::auto_retire::retired_autopm_needs_successor(state, &session) =>
        {
            session.id
        }
        Some(session) => super::super::auto_retire::ready_for_message(state, session)
            .map(|session| session.id)
            .unwrap_or_else(|| target.to_owned()),
        None => target.to_owned(),
    };
    if existing.is_none() {
        RetainedQueueStore::new(expand_home(&state.config.sm_send.db_path))
            .enqueue_message_once_with_metadata(
                key,
                &target,
                text,
                "sequential",
                QueueMessageMetadata::default(),
            )?;
    }
    if state.config.rust_core.runtime_enabled {
        let runtime = TmuxRuntime::from_app_config(&state.config);
        state
            .session_store
            .drain_runtime_pending_messages_for_session(&target, &runtime)?;
    }
    Ok(())
}

impl Actions for Runtime<'_> {
    fn lane_and_policy(&self) -> anyhow::Result<()> {
        let state = self.0;
        let board = board::board_store(state);
        let goal = (autopm::REPO.to_owned(), autopm::GOAL);
        let lane = match board.active_lanes()?.into_iter().find(|l| l.goal == goal) {
            Some(lane) => lane.id,
            None => board
                .add_lane(
                    &goal,
                    "sm:autopm-enable",
                    "Auto PM",
                    time::OffsetDateTime::now_utc(),
                )?
                .map_err(|e| anyhow::anyhow!("{e:?}"))?,
        };
        crate::review::policy::set(
            &expand_home(&state.config.sm_send.db_path),
            crate::review::policy::PolicyChange {
                scope: "lane",
                repo: autopm::REPO,
                number: lane,
                reviewer: Some(&json!({"kind":"codex","model":"gpt-6-sol","effort":"medium"})),
                session_id: None,
                name: &state.config.owner_name,
                now: &now_rfc3339(),
            },
        )?;
        board::request_recompute(state);
        Ok(())
    }

    fn issue(&self, cycle: &Cycle) -> anyhow::Result<i64> {
        let title = format!("Auto PM cycle {}", cycle.week);
        let marker = format!("<!-- sm-autopm-cycle:{} -->", cycle.week);
        // REST listing avoids GitHub search's indexing delay. Include closed issues.
        let since = autopm::format_ms(cycle.opened_ms - autopm::DAY_MS);
        for page in 1.. {
            let rows=gh_api_json(autopm::REPO,&format!("issues?state=all&sort=created&direction=desc&per_page=100&page={page}&since={since}"),false)
                .map_err(anyhow::Error::msg)?;
            let rows = rows
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("invalid GitHub issue list"))?;
            if let Some(issue) = rows.iter().find(|v| {
                v["pull_request"].is_null()
                    && v["body"].as_str().is_some_and(|b| b.contains(&marker))
            }) {
                return issue["number"]
                    .as_i64()
                    .ok_or_else(|| anyhow::anyhow!("issue without number"));
            }
            if rows.len() < 100 {
                break;
            }
        }
        let body=format!("Tier: Top\n\nGoals: pick or continue up to three experiments; measure those due; publish the readout.\n\nPart of #2170.\n\n{marker}");
        let (number, _) = self
            .0
            .board_source
            .create_issue(autopm::REPO, &title, &body)
            .map_err(anyhow::Error::msg)?;
        Ok(number)
    }

    fn attach(&self, ticket: i64) -> anyhow::Result<()> {
        gh(&[
            "label",
            "create",
            "autopm",
            "--repo",
            autopm::REPO,
            "--color",
            "5319e7",
            "--description",
            "Auto PM cycles and experiments",
            "--force",
        ]
        .map(str::to_owned))?;
        gh(&[
            "issue".into(),
            "edit".into(),
            ticket.to_string(),
            "--repo".into(),
            autopm::REPO.into(),
            "--add-label".into(),
            "autopm".into(),
        ])?;
        crate::board::write_link(
            &board::board_store(self.0),
            self.0.board_source.as_ref(),
            &crate::board::LinkRequest {
                ticket: (autopm::REPO.into(), ticket),
                target: (autopm::REPO.into(), autopm::GOAL),
                kind: crate::board::model::EdgeKind::SubIssue,
                remove: false,
                actor: "sm:autopm".into(),
                actor_name: "Auto PM".into(),
            },
            time::OffsetDateTime::now_utc(),
        )?
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(())
    }

    fn rollover(&self, old: i64, new: i64) -> anyhow::Result<()> {
        let issue = gh_api_json(autopm::REPO, &format!("issues/{old}"), false)
            .map_err(anyhow::Error::msg)?;
        if issue["state"] == "open" {
            self.0
                .autopm_git
                .close_issue(old, &format!("rolled over to #{new}"))
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    fn wake(&self, key: &str, text: &str) -> anyhow::Result<()> {
        if let Some(id) = autopm_seat(self.0, &store(self.0)) {
            once(self.0, &format!("autopm-{key}"), &id, text)?;
        }
        Ok(())
    }

    fn start(&self, cycle: &Cycle) -> anyhow::Result<()> {
        let state = self.0;
        if autopm_seat(state, &store(state)).is_some() {
            return Ok(());
        }
        let ticket = cycle.ticket.expect("cycle filed");
        let conn = store(state).open()?;
        schema(&conn)?;
        let mut stmt = conn.prepare("SELECT ticket FROM autopm_cycle_seats")?;
        let tickets = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for launched in tickets {
            if deliver(state, launched)? {
                return Ok(());
            }
        }
        if deliver(state, ticket)? {
            return Ok(());
        }
        let board = board::board_store(state);
        if board.auto_starts()?.iter().any(|r| {
            r.number == ticket
                && r.repo == autopm::REPO
                && matches!(r.state.as_str(), "waiting" | "failed")
        }) {
            return Ok(());
        }
        if board.ticket_claimed(&(autopm::REPO.into(), ticket))? {
            return Ok(());
        }
        let owner = state.session_store.owner_settings()?;
        let agent = &owner["autopm"]["agent"];
        board.authorize_auto_starts(
            &[crate::board::auto_start::Choice {
                repo: autopm::REPO.into(),
                number: ticket,
                agent_type: None,
                provider: agent["provider"].as_str().unwrap_or("claude").into(),
                model: agent["model"].as_str().map(str::to_owned),
                reasoning_effort: agent["effort"].as_str().map(str::to_owned),
                brief: Some(format!(
                    "{}\n\nCurrent cycle: #{} ({}).",
                    include_str!("../../../../../docs/autopm/brief.md"),
                    ticket,
                    cycle.week
                )),
            }],
            time::OffsetDateTime::now_utc(),
        )?;
        board::request_recompute(state);
        Ok(())
    }

    fn pause(&self, at: &str) -> anyhow::Result<()> {
        self.0
            .session_store
            .update_owner_settings(&json!({"autopm":{"paused_at":at}}), |_| Ok(Ok(())))?
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    fn pause_notice(&self, at: &str, weeks: &[String]) -> anyhow::Result<()> {
        let state = self.0;
        let links=weeks.iter().rev().take(4).rev().map(|week|format!("[{week}](https://sm.rajeshgo.li/docs/session-manager/docs/autopm/readouts/{week}.html)")).collect::<Vec<_>>().join(", ");
        let message = crate::owner_messages::NewOwnerMessage {
            human: state.config.owner_name.clone(),
            sender_session_id: "sm:autopm".into(),
            sender_session_name: "Auto PM".into(),
            title: "Auto PM paused".into(),
            body_markdown: format!("Auto PM paused: four cycles, nothing kept. Readouts: {links}"),
            blocking: false,
            blocked: false,
        };
        match super::super::messages::owner_message_store(state)
            .create_once(message, Some(&format!("autopm-pause:{at}")))?
        {
            crate::owner_messages::CreateOwnerMessage::Created(message) => {
                super::super::follows::notice_new_message(state, &message)
            }
            crate::owner_messages::CreateOwnerMessage::UnreadCapReached => {
                anyhow::bail!("Auto PM pause notice unread cap")
            }
        }
        Ok(())
    }
}

/// Capture short enable/pause transitions even if both happen between minute ticks.
pub(super) fn settings_changed(
    state: &AppState,
    before: &Settings,
    after: &Settings,
) -> anyhow::Result<()> {
    let conn = store(state).open()?;
    schema(&conn)?;
    if before.enabled != after.enabled {
        conn.execute("UPDATE autopm_cycle_control SET enabled=0 WHERE id=1", [])?;
    }
    if before.paused_at.is_some() && after.paused_at.is_none() {
        conn.execute(
            "UPDATE autopm_cycle_control SET pause_seen=NULL,score_since_ms=?1 WHERE id=1",
            [autopm::now_ms()],
        )?;
    }
    board::request_recompute(state);
    Ok(())
}

pub(in crate::http) fn tick(state: &AppState, now: i64) -> anyhow::Result<()> {
    if !state.config.rust_core.runtime_enabled {
        return Ok(());
    }
    let settings = typed(state, &state.session_store.owner_settings()?);
    let at = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000)?;
    let offset = time::UtcOffset::local_offset_at(at)?;
    cycle::tick(
        &store(state),
        &settings,
        now,
        offset,
        skipped(state, &settings)?,
        &Runtime(state),
    )
}

#[cfg(test)]
mod tests {
    use super::super::tests::role_state;
    use super::*;

    fn registered_pm() -> AppState {
        let state = role_state();
        state.session_store.authorize_autopm_seat("pm").unwrap();
        state
            .session_store
            .register_agent_role(
                "pm",
                crate::sessions::RoleRegistrationRequest {
                    requester_session_id: "pm".into(),
                    role: autopm::ROLE.into(),
                },
            )
            .unwrap();
        state
    }

    fn retire_pm(state: &AppState) {
        let path = &state.config.paths.state_file;
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let pm = raw["sessions"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|session| session["id"] == "pm")
            .unwrap();
        pm["status"] = json!("stopped");
        pm["completion_status"] = json!("retired");
        pm["completed_at"] = json!("2026-10-16T00:00:00Z");
        pm["terminal_provenance"] = json!({
            "cause":"explicit_retire", "observed_at":"2026-10-16T00:00:00Z",
            "authority":"server_lifecycle", "source":"auto_retire"
        });
        std::fs::write(path, raw.to_string()).unwrap();
        assert!(state
            .session_store
            .get_session("pm")
            .unwrap()
            .unwrap()
            .auto_retired());
    }

    #[test]
    fn repeated_cycle_tick_does_not_restore_a_delivered_wake() {
        let state = registered_pm();
        let now = 1_760_659_200_000;
        let week = cycle::due_week(now, time::UtcOffset::UTC, true);
        let key = format!("autopm-cycle:{week}");
        let actions = Runtime(&state);
        actions
            .wake(&format!("cycle:{week}"), "already sent")
            .unwrap();
        let queue = RetainedQueueStore::new(expand_home(&state.config.sm_send.db_path));
        queue.mark_delivered(&key).unwrap();
        let conn = store(&state).open().unwrap();
        schema(&conn).unwrap();
        conn.execute("UPDATE autopm_cycle_control SET enabled=1", [])
            .unwrap();
        conn.execute(
            "INSERT INTO autopm_cycles(week,ticket,opened_ms,ready) VALUES(?1,3001,?2,1)",
            params![week, now],
        )
        .unwrap();
        state
            .session_store
            .update_owner_settings(&json!({"autopm":{"enabled":true}}), |_| Ok(Ok(())))
            .unwrap()
            .unwrap();
        retire_pm(&state);
        let settings = typed(&state, &state.session_store.owner_settings().unwrap());
        for _ in 0..2 {
            cycle::tick(
                &store(&state),
                &settings,
                now,
                time::UtcOffset::UTC,
                false,
                &actions,
            )
            .unwrap();
        }
        assert!(state
            .session_store
            .get_session("pm")
            .unwrap()
            .unwrap()
            .auto_retired());
        assert!(state
            .session_store
            .get_session("pm")
            .unwrap()
            .unwrap()
            .successor_session_id
            .is_none());
    }

    #[test]
    fn new_and_undelivered_once_wakes_restore_the_retired_holder() {
        for already_queued in [false, true] {
            let state = registered_pm();
            if already_queued {
                once(&state, "autopm-test", "pm", "wake").unwrap();
            }
            retire_pm(&state);
            once(&state, "autopm-test", "pm", "wake").unwrap();
            assert!(!state
                .session_store
                .get_session("pm")
                .unwrap()
                .unwrap()
                .is_stopped());
            let conn = store(&state).open().unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM message_queue WHERE id='autopm-test' AND target_session_id='pm'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
        }
    }

    #[test]
    fn durable_brief_authorizes_a_restored_cycle_seat_before_delivery() {
        let state = role_state();
        prepare(&state, 3001, "pm", "register autopm").unwrap();
        state
            .session_store
            .update_owner_settings(&json!({"autopm":{"enabled":true}}), |_| Ok(Ok(())))
            .unwrap()
            .unwrap();
        retire_pm(&state);
        assert!(deliver(&state, 3001).unwrap());
        assert!(state.session_store.is_authorized_autopm_seat("pm").unwrap());
        assert_eq!(
            store(&state)
                .open()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM message_queue WHERE id='autopm-brief-pm'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn delivered_brief_for_stopped_historical_seat_does_not_satisfy_start() {
        let state = role_state();
        prepare(&state, 3001, "pm", "old brief").unwrap();
        state
            .session_store
            .update_owner_settings(&json!({"autopm":{"enabled":true}}), |_| Ok(Ok(())))
            .unwrap()
            .unwrap();
        assert!(deliver(&state, 3001).unwrap());
        let queue = RetainedQueueStore::new(expand_home(&state.config.sm_send.db_path));
        queue.mark_delivered("autopm-brief-pm").unwrap();
        let path = &state.config.paths.state_file;
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let pm = raw["sessions"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|session| session["id"] == "pm")
            .unwrap();
        pm["status"] = json!("stopped");
        std::fs::write(path, raw.to_string()).unwrap();
        assert!(!deliver(&state, 3001).unwrap());
        assert!(state
            .session_store
            .get_session("pm")
            .unwrap()
            .unwrap()
            .is_stopped());
    }

    #[test]
    fn runtime_enable_policy_and_durable_brief_recovery() {
        let state = role_state();
        let actions = Runtime(&state);
        actions.lane_and_policy().unwrap();
        actions.lane_and_policy().unwrap();
        let lanes = board::board_store(&state).active_lanes().unwrap();
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].added_by, "sm:autopm-enable");
        let policies =
            crate::review::policy::list(&expand_home(&state.config.sm_send.db_path)).unwrap();
        assert_eq!(
            policies[0]["reviewer"],
            json!({"kind":"codex","model":"gpt-6-sol","effort":"medium"})
        );
        assert_eq!(policies[0]["number"], lanes[0].id);
        prepare(&state, 3001, "pm", "register autopm; read the brief").unwrap();
        // A disabled seat is never authorized or sent its brief.
        assert!(deliver(&state, 3001).unwrap());
        assert!(!state.session_store.is_authorized_autopm_seat("pm").unwrap());
        state
            .session_store
            .update_owner_settings(&json!({"autopm":{"enabled":true}}), |_| Ok(Ok(())))
            .unwrap()
            .unwrap();
        assert!(deliver(&state, 3001).unwrap());
        assert!(state.session_store.is_authorized_autopm_seat("pm").unwrap());
        // Restart recovery calls the same durable intent, without a duplicate brief.
        let restarted = AppState::new(state.config.clone());
        assert!(deliver(&restarted, 3001).unwrap());
        let count: i64 = store(&state)
            .open()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM message_queue WHERE id='autopm-brief-pm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert!(
            !may_start(&state).unwrap(),
            "no quota sample means no paid start"
        );
    }

    #[test]
    fn pause_notice_is_once_and_settings_resume_clears_old_score() {
        let state = role_state();
        let actions = Runtime(&state);
        actions
            .pause_notice("2026-10-15T00:00:00Z", &["2026-W42".into()])
            .unwrap();
        actions
            .pause_notice("2026-10-15T00:00:00Z", &["2026-W42".into()])
            .unwrap();
        let conn = store(&state).open().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM owner_messages WHERE sender_session_id='sm:autopm'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let mut before = typed(&state, &state.session_store.owner_settings().unwrap());
        before.paused_at = Some("2026-10-15T00:00:00Z".into());
        let mut after = before.clone();
        after.paused_at = None;
        after.enabled = true;
        settings_changed(&state, &before, &after).unwrap();
        let (seen, since): (Option<String>, i64) = conn
            .query_row(
                "SELECT pause_seen,score_since_ms FROM autopm_cycle_control",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(seen.is_none());
        assert!(since > 0);
    }
}
