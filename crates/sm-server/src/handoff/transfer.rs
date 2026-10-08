//! The session-store half of a handoff (spec Appendices E-H): accepting
//! `sm handoff`, the state transitions, and moving what the JSON store
//! addresses to the predecessor. A child module of `sessions`, so it shares
//! the store's write guard and raw-state helpers. `http/handoff.rs` drives
//! the steps in order and moves the SQLite rows.

use super::*;
use crate::handoff::execute::{self, HandoffNote, RESTARTED_ERROR};
use crate::handoff::policy::{self, HandoffPhase, HandoffRecord, HandoffTrigger};

/// What `POST /sessions/{id}/handoff` did (Appendix E).
#[derive(Debug, PartialEq)]
pub enum HandoffAcceptOutcome {
    /// `ready` means the agent is already idle, so the successor starts now.
    Accepted {
        ready: bool,
    },
    Forbidden,
    NotFound,
    Conflict(String),
}

/// What a successor is created with (Appendix F.2, F.3).
#[derive(Debug, Clone, PartialEq)]
pub struct SuccessorPlan {
    pub predecessor_id: String,
    pub predecessor_name: String,
    pub successor_name: String,
    pub provider: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub working_dir: String,
    pub node: String,
    pub parent_session_id: Option<String>,
}

/// A handoff the driver should act on.
#[derive(Debug, Clone, PartialEq)]
pub enum HandoffWork {
    /// Accepted and the agent's turn has ended: start the successor.
    Start(String),
    /// Interrupted after the successor existed: resume the transfer.
    Resume {
        predecessor_id: String,
        successor_id: String,
    },
}

/// The JSON-store facts the successor's brief and the parent notice need.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HandoffFacts {
    pub predecessor_name: String,
    pub successor_name: String,
    pub parent_session_id: Option<String>,
    pub percent: Option<f64>,
    pub note: String,
    pub working_dir: String,
    pub roles: Vec<String>,
    pub children: Vec<String>,
    pub original_brief: Option<String>,
}

fn raw_handoff_record(session: &Map<String, Value>) -> Option<HandoffRecord> {
    HandoffRecord::from_session(session).and_then(Result::ok)
}

fn write_handoff_record(session: &mut Map<String, Value>, record: &HandoffRecord) {
    session.insert(policy::STATE_KEY.to_owned(), record.to_json());
}

/// Accepted, spawning and transferring hold every message for the successor:
/// a message typed into the predecessor now would start a turn in a session
/// about to retire.
pub(super) fn handoff_fences_delivery_raw(session: &Map<String, Value>) -> bool {
    raw_handoff_record(session).is_some_and(|record| {
        matches!(
            record.state,
            HandoffPhase::Accepted | HandoffPhase::Spawning | HandoffPhase::Transferring
        )
    })
}

fn raw_session_is_idle(session: &Map<String, Value>) -> bool {
    !raw_session_is_stopped(session) && effective_raw_session_status(session) == "idle"
}

fn replace_text_field(object: &mut Map<String, Value>, key: &str, from: &str, to: &str) -> bool {
    if json_text(object.get(key)).as_deref() == Some(from) {
        object.insert(key.to_owned(), Value::String(to.to_owned()));
        true
    } else {
        false
    }
}

/// Re-point `keys` of every entry of a top-level array.
fn repoint_array_entries(state: &mut Value, array: &str, keys: &[&str], from: &str, to: &str) {
    let Some(entries) = state.get_mut(array).and_then(Value::as_array_mut) else {
        return;
    };
    for entry in entries.iter_mut().filter_map(Value::as_object_mut) {
        for key in keys {
            replace_text_field(entry, key, from, to);
        }
    }
}

impl SessionStore {
    /// `sm handoff --link|--path` (Appendix E).
    pub fn accept_handoff(
        &self,
        session_id: &str,
        requester_session_id: &str,
        note: &HandoffNote,
    ) -> Result<HandoffAcceptOutcome> {
        if requester_session_id.trim() != session_id {
            return Ok(HandoffAcceptOutcome::Forbidden);
        }
        let _submission = self.lock_opencode_submission(session_id)?;
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(HandoffAcceptOutcome::NotFound);
        };
        if raw_session_is_stopped(session) {
            return Ok(HandoffAcceptOutcome::Conflict(
                "session is not running".to_owned(),
            ));
        }
        let now = now_rfc3339();
        let record = match raw_handoff_record(session) {
            Some(record)
                if matches!(
                    record.state,
                    HandoffPhase::Spawning | HandoffPhase::Transferring | HandoffPhase::Done
                ) =>
            {
                return Ok(HandoffAcceptOutcome::Conflict(
                    "handoff already in progress".to_owned(),
                ));
            }
            Some(mut record) => {
                record.state = HandoffPhase::Accepted;
                record.error = None;
                record.failed_at = None;
                record
            }
            None => HandoffRecord {
                asked_at: None,
                asked_percent: None,
                ..HandoffRecord::asked(HandoffTrigger::Voluntary, &now, None)
            },
        };
        let record = HandoffRecord {
            state: HandoffPhase::Accepted,
            note: Some(note.to_json()),
            accepted_at: Some(now),
            ..record
        };
        write_handoff_record(session, &record);
        let ready = raw_session_is_idle(session);
        self.write_raw_json_value(&state)?;
        Ok(HandoffAcceptOutcome::Accepted { ready })
    }

    /// Move an accepted handoff whose agent is idle to `spawning` and return
    /// what to create. Only one caller wins; the rest get `None` (F.1).
    pub fn claim_handoff_start(&self, session_id: &str) -> Result<Option<SuccessorPlan>> {
        let _submission = self.lock_opencode_submission(session_id)?;
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        let Some(mut record) = raw_handoff_record(session) else {
            return Ok(None);
        };
        if record.state != HandoffPhase::Accepted || !raw_session_is_idle(session) {
            return Ok(None);
        }
        record.state = HandoffPhase::Spawning;
        write_handoff_record(session, &record);
        let predecessor_name = raw_session_display_name(session, session_id);
        let text = |key: &str| json_text(session.get(key)).filter(|value| !value.is_empty());
        let plan = SuccessorPlan {
            predecessor_id: session_id.to_owned(),
            successor_name: execute::successor_name(&predecessor_name),
            predecessor_name,
            provider: text("provider").unwrap_or_else(default_provider),
            model: text("model"),
            reasoning_effort: text("reasoning_effort"),
            working_dir: text("working_dir").unwrap_or_default(),
            node: text("node").unwrap_or_else(default_node),
            parent_session_id: text("parent_session_id"),
        };
        self.write_raw_json_value(&state)?;
        Ok(Some(plan))
    }

    /// F.5: the successor could not be created. Nothing moved; the agent is
    /// told and keeps its work.
    pub fn fail_handoff(&self, session_id: &str, error: &str) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(());
        };
        let Some(mut record) = raw_handoff_record(session) else {
            return Ok(());
        };
        if record.state != HandoffPhase::Spawning {
            return Ok(());
        }
        record.state = HandoffPhase::Failed;
        record.error = Some(error.to_owned());
        record.failed_at = Some(now_rfc3339());
        write_handoff_record(session, &record);
        let runtime = self.delivery_runtime.clone();
        self.queue_parent_message(
            &mut state,
            session_id,
            session_id,
            &execute::failed_text(error),
            policy::DELIVERY_MODE,
            policy::MESSAGE_CATEGORY,
            runtime.as_ref(),
        )?;
        self.write_raw_json_value(&state)
    }

    /// G step 1, then every JSON-store row of the transfer table. Safe to
    /// repeat: each change is `where <field> = predecessor`.
    pub fn transfer_handoff_json(&self, predecessor_id: &str, successor_id: &str) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(predecessor) = session_object_mut(sessions, predecessor_id) else {
            anyhow::bail!("predecessor {predecessor_id} disappeared during handoff");
        };
        // Step 1: lineage and `transferring`.
        predecessor.insert(
            "successor_session_id".to_owned(),
            Value::String(successor_id.to_owned()),
        );
        if let Some(mut record) = raw_handoff_record(predecessor) {
            if record.state == HandoffPhase::Spawning {
                record.state = HandoffPhase::Transferring;
            }
            record.successor_session_id = Some(successor_id.to_owned());
            write_handoff_record(predecessor, &record);
        }
        let inherited = [
            policy::OVERRIDE_KEY,
            "is_em",
            "role",
            "context_monitor_enabled",
            "context_monitor_notify",
            "context_monitor_notify_source",
            "context_monitor_threshold_percentages",
            "context_monitor_warning_percentage",
            "context_monitor_critical_percentage",
        ]
        .iter()
        .filter_map(|key| {
            predecessor
                .get(*key)
                .filter(|value| !value.is_null())
                .map(|value| (*key, value.clone()))
        })
        .collect::<Vec<_>>();
        let Some(successor) = session_object_mut(sessions, successor_id) else {
            anyhow::bail!("successor {successor_id} disappeared during handoff");
        };
        successor.insert(
            "predecessor_session_id".to_owned(),
            Value::String(predecessor_id.to_owned()),
        );
        for (key, value) in inherited {
            if key == policy::OVERRIDE_KEY && successor.contains_key(key) {
                continue;
            }
            successor.insert(key.to_owned(), value);
        }
        replace_text_field(
            successor,
            "context_monitor_notify",
            predecessor_id,
            successor_id,
        );
        reset_context_oneshot_flags(successor);
        // Children, and sessions whose context alerts go to the predecessor.
        for session in sessions.iter_mut().filter_map(Value::as_object_mut) {
            if json_text(session.get("id")).as_deref() == Some(successor_id) {
                continue;
            }
            replace_text_field(session, "parent_session_id", predecessor_id, successor_id);
            replace_text_field(
                session,
                "context_monitor_notify",
                predecessor_id,
                successor_id,
            );
        }
        // Registered roles.
        repoint_array_entries(
            &mut state,
            "agent_registrations",
            &["session_id"],
            predecessor_id,
            successor_id,
        );
        if let Some(object) = state.as_object_mut() {
            replace_text_field(
                object,
                "maintainer_session_id",
                predecessor_id,
                successor_id,
            );
            if let Some(last) = object
                .get_mut("agent_role_last_session_ids")
                .and_then(Value::as_object_mut)
            {
                let roles = last.keys().cloned().collect::<Vec<_>>();
                for role in roles {
                    replace_text_field(last, &role, predecessor_id, successor_id);
                }
            }
        }
        // The JSON mirrors of the queue tables `hand_off_rows` moves.
        repoint_array_entries(
            &mut state,
            "retained_parent_wake_registrations",
            &["parent_session_id", "child_session_id"],
            predecessor_id,
            successor_id,
        );
        repoint_array_entries(
            &mut state,
            "retained_remind_registrations",
            &[
                "session_id",
                "target_session_id",
                "cancel_on_reply_session_id",
            ],
            predecessor_id,
            successor_id,
        );
        repoint_array_entries(
            &mut state,
            "retained_stop_notify_states",
            &["session_id", "sender_session_id"],
            predecessor_id,
            successor_id,
        );
        self.write_raw_json_value(&state)
    }

    /// What the brief and the parent notice say (F.4, D.7), read after the
    /// transfer so they describe what the successor now holds.
    pub fn handoff_facts(&self, predecessor_id: &str, successor_id: &str) -> Result<HandoffFacts> {
        let parsed_state = self.load_parsed_state()?;
        let state = &parsed_state.raw;
        let predecessor = raw_session_object(state, predecessor_id)
            .with_context(|| format!("predecessor {predecessor_id} not found"))?;
        let successor = raw_session_object(state, successor_id)
            .with_context(|| format!("successor {successor_id} not found"))?;
        let record = raw_handoff_record(predecessor);
        let roles = state
            .get("agent_registrations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|entry| json_text(entry.get("session_id")).as_deref() == Some(successor_id))
            .filter_map(|entry| json_text(entry.get("role")))
            .collect();
        let children = state
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .filter(|session| {
                json_text(session.get("parent_session_id")).as_deref() == Some(successor_id)
                    && !raw_session_is_stopped(session)
            })
            .filter_map(|session| {
                let id = json_text(session.get("id"))?;
                Some(format!("{} ({id})", raw_session_display_name(session, &id)))
            })
            .collect();
        Ok(HandoffFacts {
            predecessor_name: raw_session_display_name(predecessor, predecessor_id),
            successor_name: raw_session_display_name(successor, successor_id),
            parent_session_id: json_text(predecessor.get("parent_session_id")),
            percent: predecessor
                .get("context_used_percentage")
                .and_then(Value::as_f64),
            note: execute::note_value(record.as_ref().and_then(|record| record.note.as_ref())),
            working_dir: json_text(predecessor.get("working_dir")).unwrap_or_default(),
            roles,
            children,
            original_brief: original_brief_path(state, predecessor_id),
        })
    }

    /// Queue the brief to the successor (G step 3) or the parent notice
    /// (step 5) under the stable queue id `id`, so a resumed transfer never
    /// sends it twice. It sorts ahead of every undelivered message for
    /// `ahead_of`. Both come from the predecessor.
    pub fn queue_handoff_notice(
        &self,
        predecessor_id: &str,
        target_session_id: &str,
        text: &str,
        id: &str,
        ahead_of: &[&str],
    ) -> Result<()> {
        let Some(queue) = &self.queue_store else {
            return Ok(());
        };
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if raw_session_object(&state, target_session_id).is_none() {
            return Ok(());
        }
        let inserted = queue.enqueue_handoff_notice(
            id,
            target_session_id,
            text,
            policy::DELIVERY_MODE,
            QueueMessageMetadata {
                sender_session_id: Some(predecessor_id.to_owned()),
                message_category: Some(execute::NOTICE_CATEGORY.to_owned()),
                ..QueueMessageMetadata::default()
            },
            ahead_of,
        )?;
        // A resumed transfer still delivers a notice queued before the crash.
        self.drain_after_handoff_raw(&mut state, target_session_id, Some(id))?;
        if !inserted {
            return self.write_raw_json_value(&state);
        }
        push_retained_message_raw(
            &mut state,
            target_session_id,
            text,
            policy::DELIVERY_MODE,
            Some(execute::NOTICE_CATEGORY),
        )?;
        self.write_raw_json_value(&state)
    }

    /// Move messages still waiting for the predecessor to the successor and
    /// deliver what it can. Returns how many moved.
    pub fn hand_off_pending_messages(
        &self,
        predecessor_id: &str,
        successor_id: &str,
    ) -> Result<usize> {
        let Some(queue) = &self.queue_store else {
            return Ok(0);
        };
        let moved = queue.hand_off_messages(predecessor_id, successor_id)?;
        if moved > 0 {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            self.drain_after_handoff_raw(&mut state, successor_id, None)?;
            self.write_raw_json_value(&state)?;
        }
        Ok(moved)
    }

    /// Messages that reached a predecessor after its handoff finished, as
    /// (predecessor, successor) pairs for the sweep to move.
    pub fn stranded_handoff_messages(&self) -> Result<Vec<(String, String)>> {
        // The sweep never creates the queue database.
        let Some(queue) = self
            .queue_store
            .as_ref()
            .filter(|queue| queue.db_path().exists())
        else {
            return Ok(Vec::new());
        };
        let targets = queue.undelivered_targets()?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let parsed_state = self.load_parsed_state()?;
        Ok(targets
            .into_iter()
            .filter_map(|target| {
                let session = raw_session_object(&parsed_state.raw, &target)?;
                let record = raw_handoff_record(session)?;
                if record.state != HandoffPhase::Done {
                    return None;
                }
                let successor = record
                    .successor_session_id
                    .or_else(|| json_text(session.get("successor_session_id")))?;
                Some((target, successor))
            })
            .collect())
    }

    fn drain_after_handoff_raw(
        &self,
        state: &mut Value,
        target_session_id: &str,
        stop_after_message_id: Option<&str>,
    ) -> Result<()> {
        let (Some(queue), Some(runtime)) = (&self.queue_store, self.delivery_runtime.clone())
        else {
            return Ok(());
        };
        let node = raw_session_object(state, target_session_id)
            .and_then(|session| json_text(session.get("node")))
            .unwrap_or_else(default_node);
        if is_primary_node(&node) {
            drain_pending_runtime_messages_raw(
                self,
                state,
                target_session_id,
                &runtime,
                queue,
                None,
                None,
                stop_after_message_id,
                false,
            )?;
        }
        Ok(())
    }

    /// G step 6. Leftover asks and reminders from the predecessor go first.
    pub fn finish_handoff(&self, predecessor_id: &str) -> Result<()> {
        if let Some(queue) = &self.queue_store {
            queue.cancel_pending_messages_from_sender_category(
                predecessor_id,
                policy::MESSAGE_CATEGORY,
            )?;
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        if let Some(session) = session_object_mut(sessions, predecessor_id) {
            if let Some(mut record) = raw_handoff_record(session) {
                record.state = HandoffPhase::Done;
                write_handoff_record(session, &record);
            }
        }
        self.write_raw_json_value(&state)
    }

    /// Handoffs the driver should act on now: accepted ones whose turn has
    /// ended, and transfers a restart interrupted.
    pub fn pending_handoff_work(&self) -> Result<Vec<HandoffWork>> {
        let parsed_state = self.load_parsed_state()?;
        let mut work = Vec::new();
        for session in parsed_state
            .raw
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            let (Some(id), Some(record)) =
                (json_text(session.get("id")), raw_handoff_record(session))
            else {
                continue;
            };
            match record.state {
                HandoffPhase::Accepted if raw_session_is_idle(session) => {
                    work.push(HandoffWork::Start(id));
                }
                HandoffPhase::Transferring => {
                    if let Some(successor_id) = record
                        .successor_session_id
                        .or_else(|| json_text(session.get("successor_session_id")))
                    {
                        work.push(HandoffWork::Resume {
                            predecessor_id: id,
                            successor_id,
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(work)
    }

    /// Startup (Appendix G, "Recovery"): a successor that was being created
    /// when the server stopped may or may not exist, so the handoff fails and
    /// the agent is told. Returns how many failed.
    pub fn recover_interrupted_handoff_starts(&self) -> Result<usize> {
        let spawning = {
            let parsed_state = self.load_parsed_state()?;
            parsed_state
                .raw
                .get("sessions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_object)
                .filter(|session| {
                    raw_handoff_record(session)
                        .is_some_and(|record| record.state == HandoffPhase::Spawning)
                })
                .filter_map(|session| json_text(session.get("id")))
                .collect::<Vec<_>>()
        };
        for session_id in &spawning {
            self.fail_handoff(session_id, RESTARTED_ERROR)?;
        }
        Ok(spawning.len())
    }

    /// Appendix J: drop the old `/clear` handoff's fields from every record,
    /// logging each session that had one pending. `last_handoff_path` stays.
    pub fn strip_legacy_handoff_fields(&self) -> Result<usize> {
        const LEGACY: [&str; 4] = [
            "pending_handoff_path",
            "pending_handoff_recorded_at",
            "claude_handoff_in_progress_at",
            "pending_handoff_event_offset",
        ];
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut stripped = 0;
        for session in ensure_sessions_array_mut(&mut state)?
            .iter_mut()
            .filter_map(Value::as_object_mut)
        {
            if !LEGACY.iter().any(|key| session.contains_key(*key)) {
                continue;
            }
            if let Some(path) = json_text(session.get("pending_handoff_path")) {
                eprintln!(
                    "dropping the pending /clear handoff of {} ({path}); sm handoff now starts a successor",
                    json_text(session.get("id")).unwrap_or_default()
                );
            }
            for key in LEGACY {
                session.remove(key);
            }
            stripped += 1;
        }
        if stripped > 0 {
            self.write_raw_json_value(&state)?;
        }
        Ok(stripped)
    }

    /// H.2: the live end of an ended session's successor chain, if any. A
    /// live session resolves to itself.
    pub fn forwarded_session(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        let mut current = match self.get_session(session_id)? {
            Some(session) => session,
            None => return Ok(None),
        };
        for _ in 0..execute::MAX_FORWARD_HOPS {
            if !current.is_stopped() {
                return Ok(Some(current));
            }
            let Some(next) = current.successor_session_id.clone() else {
                return Ok(None);
            };
            current = match self.get_session(&next)? {
                Some(session) => session,
                None => return Ok(None),
            };
        }
        Ok(None)
    }
}

/// F.4 "Original brief": the spawn-brief artifact of the first session in the
/// predecessor chain that has one.
fn original_brief_path(state: &Value, session_id: &str) -> Option<String> {
    let intents = state.get("spawn_launch_intents").and_then(Value::as_array);
    let mut current = Some(session_id.to_owned());
    for _ in 0..execute::MAX_FORWARD_HOPS {
        let id = current?;
        let path = intents
            .into_iter()
            .flatten()
            .find(|intent| json_text(intent.get("session_id")).as_deref() == Some(id.as_str()))
            .and_then(|intent| json_text(intent.get("artifact")?.get("path")));
        if path.is_some() {
            return path;
        }
        current = raw_session_object(state, &id)
            .and_then(|session| json_text(session.get("predecessor_session_id")));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::execute::NoteKind;

    fn temp_path(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        env::temp_dir().join(format!(
            "sm-handoff-transfer-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn session(id: &str, status: &str, extra: Value) -> Value {
        let mut session = json!({
            "id": id,
            "name": format!("claude-{id}"),
            "friendly_name": format!("{id}-agent"),
            "working_dir": "/repo",
            "tmux_session": format!("claude-{id}"),
            "provider": "claude",
            "model": "opus",
            "reasoning_effort": "high",
            "status": status,
            "created_at": "2026-09-29T00:00:00Z",
            "last_activity": "2026-09-29T00:01:00Z"
        });
        session
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        session
    }

    /// lead0001 → pred0001 → child001; other001 alerts go to pred0001.
    fn store(label: &str, pred_status: &str) -> SessionStore {
        let state_file = temp_path(&format!("{label}.json"));
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    session("lead0001", "idle", json!({})),
                    session("pred0001", pred_status, json!({
                        "parent_session_id": "lead0001",
                        "context_used_percentage": 36.4,
                        "is_em": true,
                        "context_monitor_enabled": true,
                        "context_monitor_notify": "pred0001",
                        "context_monitor_notify_source": "explicit",
                        "context_reported_thresholds": [30],
                        policy::OVERRIDE_KEY: {"enabled": null, "threshold_percent": 45},
                    })),
                    session("child001", "running", json!({"parent_session_id": "pred0001"})),
                    session("other001", "running", json!({
                        "context_monitor_enabled": true,
                        "context_monitor_notify": "pred0001",
                    })),
                    session("succ0001", "running", json!({"parent_session_id": "lead0001"})),
                ],
                "agent_registrations": [
                    {"role": "maintainer", "session_id": "pred0001", "created_at": "2026-09-29T00:00:00Z"}
                ],
                "agent_role_last_session_ids": {"maintainer": "pred0001"},
                "retained_stop_notify_states": [
                    {"session_id": "pred0001", "sender_session_id": "lead0001", "delay_seconds": 0}
                ],
                "spawn_launch_intents": [
                    {"session_id": "pred0001", "artifact": {"path": "/briefs/pred.md"}}
                ],
            })
            .to_string(),
        )
        .unwrap();
        SessionStore::new_with_queue(state_file, temp_path(&format!("{label}-queue.db")))
    }

    fn note() -> HandoffNote {
        HandoffNote {
            kind: NoteKind::Link,
            value: "https://github.com/acme/widgets/pull/9#issuecomment-1".to_owned(),
        }
    }

    fn record(store: &SessionStore, session_id: &str) -> Option<HandoffRecord> {
        let state = store.load_raw_json_value().unwrap();
        raw_handoff_record(raw_session_object(&state, session_id).unwrap())
    }

    fn set_record(store: &SessionStore, session_id: &str, state_name: &str) {
        let mut state = store.load_raw_json_value().unwrap();
        let sessions = ensure_sessions_array_mut(&mut state).unwrap();
        session_object_mut(sessions, session_id).unwrap().insert(
            policy::STATE_KEY.to_owned(),
            json!({"state": state_name, "trigger": "context", "asked_percent": 36.4}),
        );
        store.write_raw_json_value(&state).unwrap();
    }

    fn set_status(store: &SessionStore, session_id: &str, status: &str) {
        let mut state = store.load_raw_json_value().unwrap();
        let sessions = ensure_sessions_array_mut(&mut state).unwrap();
        session_object_mut(sessions, session_id)
            .unwrap()
            .insert("status".to_owned(), json!(status));
        store.write_raw_json_value(&state).unwrap();
    }

    fn queued(store: &SessionStore, session_id: &str) -> Vec<String> {
        store
            .queue_store
            .as_ref()
            .unwrap()
            .pending_messages_for_target(session_id, 20)
            .unwrap()
            .into_iter()
            .map(|message| message.text)
            .collect()
    }

    #[test]
    fn accept_follows_the_outcome_table_of_appendix_e() {
        let store = store("accept", "running");
        assert_eq!(
            store
                .accept_handoff("pred0001", "lead0001", &note())
                .unwrap(),
            HandoffAcceptOutcome::Forbidden
        );
        assert_eq!(
            store
                .accept_handoff("nope0001", "nope0001", &note())
                .unwrap(),
            HandoffAcceptOutcome::NotFound
        );
        // Absent: a voluntary handoff, not ready mid-turn.
        assert_eq!(
            store
                .accept_handoff("pred0001", "pred0001", &note())
                .unwrap(),
            HandoffAcceptOutcome::Accepted { ready: false }
        );
        let accepted = record(&store, "pred0001").unwrap();
        assert_eq!(accepted.state, HandoffPhase::Accepted);
        assert_eq!(accepted.trigger, HandoffTrigger::Voluntary);
        assert_eq!(accepted.note, Some(note().to_json()));
        assert!(handoff_fences_delivery_raw(
            raw_session_object(&store.load_raw_json_value().unwrap(), "pred0001").unwrap()
        ));
        // Accepted again: the note is replaced.
        let corrected = HandoffNote {
            kind: NoteKind::Path,
            value: "/repo/HANDOFF.md".to_owned(),
        };
        store
            .accept_handoff("pred0001", "pred0001", &corrected)
            .unwrap();
        assert_eq!(
            record(&store, "pred0001").unwrap().note,
            Some(corrected.to_json())
        );
        // Asked keeps its trigger; an idle agent is ready at once.
        set_record(&store, "child001", "asked");
        set_status(&store, "child001", "idle");
        assert_eq!(
            store
                .accept_handoff("child001", "child001", &note())
                .unwrap(),
            HandoffAcceptOutcome::Accepted { ready: true }
        );
        assert_eq!(
            record(&store, "child001").unwrap().trigger,
            HandoffTrigger::Context
        );
        for busy in ["spawning", "transferring", "done"] {
            set_record(&store, "other001", busy);
            assert_eq!(
                store
                    .accept_handoff("other001", "other001", &note())
                    .unwrap(),
                HandoffAcceptOutcome::Conflict("handoff already in progress".to_owned())
            );
        }
        set_status(&store, "lead0001", "stopped");
        assert_eq!(
            store
                .accept_handoff("lead0001", "lead0001", &note())
                .unwrap(),
            HandoffAcceptOutcome::Conflict("session is not running".to_owned())
        );
    }

    #[test]
    fn the_successor_starts_once_and_only_after_the_turn_ends() {
        let store = store("start", "running");
        store
            .accept_handoff("pred0001", "pred0001", &note())
            .unwrap();
        assert_eq!(store.claim_handoff_start("pred0001").unwrap(), None);
        assert!(store.pending_handoff_work().unwrap().is_empty());
        set_status(&store, "pred0001", "idle");
        assert_eq!(
            store.pending_handoff_work().unwrap(),
            vec![HandoffWork::Start("pred0001".to_owned())]
        );
        let plan = store.claim_handoff_start("pred0001").unwrap().unwrap();
        assert_eq!(
            plan,
            SuccessorPlan {
                predecessor_id: "pred0001".to_owned(),
                predecessor_name: "pred0001-agent".to_owned(),
                successor_name: "pred0001-agent-h2".to_owned(),
                provider: "claude".to_owned(),
                model: Some("opus".to_owned()),
                reasoning_effort: Some("high".to_owned()),
                working_dir: "/repo".to_owned(),
                node: "primary".to_owned(),
                parent_session_id: Some("lead0001".to_owned()),
            }
        );
        assert_eq!(
            record(&store, "pred0001").unwrap().state,
            HandoffPhase::Spawning
        );
        assert_eq!(store.claim_handoff_start("pred0001").unwrap(), None);
    }

    #[test]
    fn a_failed_start_tells_the_agent_and_moves_nothing() {
        let store = store("fail", "idle");
        store
            .accept_handoff("pred0001", "pred0001", &note())
            .unwrap();
        store.claim_handoff_start("pred0001").unwrap().unwrap();
        store.fail_handoff("pred0001", "tmux exploded").unwrap();
        let failed = record(&store, "pred0001").unwrap();
        assert_eq!(failed.state, HandoffPhase::Failed);
        assert_eq!(failed.error.as_deref(), Some("tmux exploded"));
        assert!(failed.failed_at.is_some());
        assert_eq!(
            queued(&store, "pred0001"),
            vec![execute::failed_text("tmux exploded")]
        );
        let state = store.load_raw_json_value().unwrap();
        assert_eq!(
            json_text(
                raw_session_object(&state, "child001")
                    .unwrap()
                    .get("parent_session_id")
            ),
            Some("pred0001".to_owned())
        );
        // A new `sm handoff` retries.
        assert_eq!(
            store
                .accept_handoff("pred0001", "pred0001", &note())
                .unwrap(),
            HandoffAcceptOutcome::Accepted { ready: true }
        );
        assert_eq!(record(&store, "pred0001").unwrap().error, None);
    }

    #[test]
    fn the_json_transfer_moves_every_row_and_is_idempotent() {
        let store = store("transfer", "idle");
        store
            .accept_handoff("pred0001", "pred0001", &note())
            .unwrap();
        store.claim_handoff_start("pred0001").unwrap().unwrap();
        for _ in 0..2 {
            store.transfer_handoff_json("pred0001", "succ0001").unwrap();
        }
        let state = store.load_raw_json_value().unwrap();
        let field = |id: &str, key: &str| raw_session_object(&state, id).unwrap().get(key).cloned();
        assert_eq!(
            field("pred0001", "successor_session_id"),
            Some(json!("succ0001"))
        );
        let moving = record(&store, "pred0001").unwrap();
        assert_eq!(moving.state, HandoffPhase::Transferring);
        assert_eq!(moving.successor_session_id.as_deref(), Some("succ0001"));
        assert_eq!(
            field("succ0001", "predecessor_session_id"),
            Some(json!("pred0001"))
        );
        assert_eq!(
            field("succ0001", policy::OVERRIDE_KEY),
            Some(json!({"enabled": null, "threshold_percent": 45}))
        );
        assert_eq!(field("succ0001", "is_em"), Some(json!(true)));
        assert_eq!(
            field("succ0001", "context_monitor_enabled"),
            Some(json!(true))
        );
        assert_eq!(
            field("succ0001", "context_monitor_notify"),
            Some(json!("succ0001"))
        );
        assert_eq!(
            field("succ0001", "context_reported_thresholds"),
            Some(json!([]))
        );
        assert_eq!(
            field("child001", "parent_session_id"),
            Some(json!("succ0001"))
        );
        assert_eq!(
            field("other001", "context_monitor_notify"),
            Some(json!("succ0001"))
        );
        assert_eq!(
            field("succ0001", "parent_session_id"),
            Some(json!("lead0001"))
        );
        assert_eq!(state["agent_registrations"][0]["session_id"], "succ0001");
        assert_eq!(
            state["agent_role_last_session_ids"]["maintainer"],
            "succ0001"
        );
        assert_eq!(
            state["retained_stop_notify_states"][0]["session_id"],
            "succ0001"
        );
        assert_eq!(
            store.pending_handoff_work().unwrap(),
            vec![HandoffWork::Resume {
                predecessor_id: "pred0001".to_owned(),
                successor_id: "succ0001".to_owned(),
            }]
        );

        let facts = store.handoff_facts("pred0001", "succ0001").unwrap();
        assert_eq!(
            facts,
            HandoffFacts {
                predecessor_name: "pred0001-agent".to_owned(),
                successor_name: "succ0001-agent".to_owned(),
                parent_session_id: Some("lead0001".to_owned()),
                percent: Some(36.4),
                note: note().value,
                working_dir: "/repo".to_owned(),
                roles: vec!["maintainer".to_owned()],
                children: vec!["child001-agent (child001)".to_owned()],
                original_brief: Some("/briefs/pred.md".to_owned()),
            }
        );

        store.finish_handoff("pred0001").unwrap();
        assert_eq!(
            record(&store, "pred0001").unwrap().state,
            HandoffPhase::Done
        );
        assert!(store.pending_handoff_work().unwrap().is_empty());
    }

    #[test]
    fn successor_inherits_agent_override_unless_it_already_has_one() {
        let inherited_store = store("override-inheritance", "idle");
        let mut state = inherited_store.load_raw_json_value().unwrap();
        let sessions = ensure_sessions_array_mut(&mut state).unwrap();
        session_object_mut(sessions, "pred0001").unwrap().insert(
            policy::OVERRIDE_KEY.into(),
            json!({"enabled": true, "threshold_percent": 30}),
        );
        inherited_store.write_raw_json_value(&state).unwrap();
        inherited_store
            .transfer_handoff_json("pred0001", "succ0001")
            .unwrap();
        let state = inherited_store.load_raw_json_value().unwrap();
        assert_eq!(
            raw_session_object(&state, "succ0001").unwrap()[policy::OVERRIDE_KEY]
                ["threshold_percent"],
            30
        );

        let store = store("override-preserved", "idle");
        let mut state = store.load_raw_json_value().unwrap();
        let sessions = ensure_sessions_array_mut(&mut state).unwrap();
        session_object_mut(sessions, "succ0001").unwrap().insert(
            policy::OVERRIDE_KEY.into(),
            json!({"enabled": false, "threshold_percent": 60}),
        );
        store.write_raw_json_value(&state).unwrap();
        store.transfer_handoff_json("pred0001", "succ0001").unwrap();
        let state = store.load_raw_json_value().unwrap();
        assert_eq!(
            raw_session_object(&state, "succ0001").unwrap()[policy::OVERRIDE_KEY]
                ["threshold_percent"],
            60
        );
    }

    #[test]
    fn a_resumed_transfer_queues_each_notice_once_ahead_of_inherited_messages() {
        let store = store("notice", "idle");
        let queue = store.queue_store.as_ref().unwrap();
        queue
            .enqueue_message_with_metadata(
                "pred0001",
                "older",
                "sequential",
                QueueMessageMetadata::default(),
            )
            .unwrap();
        let brief_id = execute::brief_message_id("pred0001");
        for _ in 0..2 {
            store
                .queue_handoff_notice(
                    "pred0001",
                    "succ0001",
                    "brief",
                    &brief_id,
                    &["succ0001", "pred0001"],
                )
                .unwrap();
            store
                .queue_handoff_notice(
                    "pred0001",
                    "lead0001",
                    "notice",
                    &execute::parent_notice_message_id("pred0001"),
                    &[],
                )
                .unwrap();
        }
        assert_eq!(queued(&store, "lead0001"), ["notice"]);
        assert_eq!(
            store
                .hand_off_pending_messages("pred0001", "succ0001")
                .unwrap(),
            1
        );
        assert_eq!(queued(&store, "succ0001"), ["brief", "older"]);
        assert!(store.stranded_handoff_messages().unwrap().is_empty());

        // A message reaching the predecessor after `done` is found and moved.
        set_record(&store, "pred0001", "transferring");
        store.transfer_handoff_json("pred0001", "succ0001").unwrap();
        store.finish_handoff("pred0001").unwrap();
        queue
            .enqueue_message_with_metadata(
                "pred0001",
                "late",
                "sequential",
                QueueMessageMetadata::default(),
            )
            .unwrap();
        assert_eq!(
            store.stranded_handoff_messages().unwrap(),
            [("pred0001".to_owned(), "succ0001".to_owned())]
        );
        store
            .hand_off_pending_messages("pred0001", "succ0001")
            .unwrap();
        assert_eq!(queued(&store, "succ0001"), ["brief", "older", "late"]);
    }

    #[test]
    fn the_original_brief_follows_the_lineage_back() {
        let store = store("lineage", "idle");
        store.transfer_handoff_json("pred0001", "succ0001").unwrap();
        let state = store.load_raw_json_value().unwrap();
        assert_eq!(
            original_brief_path(&state, "succ0001").as_deref(),
            Some("/briefs/pred.md")
        );
        assert_eq!(original_brief_path(&state, "child001"), None);
    }

    #[test]
    fn a_restart_fails_an_interrupted_start() {
        let store = store("recover", "idle");
        set_record(&store, "pred0001", "spawning");
        assert_eq!(store.recover_interrupted_handoff_starts().unwrap(), 1);
        let failed = record(&store, "pred0001").unwrap();
        assert_eq!(failed.state, HandoffPhase::Failed);
        assert_eq!(failed.error.as_deref(), Some(RESTARTED_ERROR));
        assert_eq!(
            queued(&store, "pred0001"),
            vec![execute::failed_text(RESTARTED_ERROR)]
        );
        assert_eq!(store.recover_interrupted_handoff_starts().unwrap(), 0);
    }

    #[test]
    fn startup_strips_the_old_clear_handoff_fields() {
        let store = store("legacy", "idle");
        let mut state = store.load_raw_json_value().unwrap();
        let sessions = ensure_sessions_array_mut(&mut state).unwrap();
        let session = session_object_mut(sessions, "pred0001").unwrap();
        for key in [
            "pending_handoff_path",
            "pending_handoff_recorded_at",
            "claude_handoff_in_progress_at",
            "pending_handoff_event_offset",
            "last_handoff_path",
        ] {
            session.insert(key.to_owned(), json!("x"));
        }
        store.write_raw_json_value(&state).unwrap();
        assert_eq!(store.strip_legacy_handoff_fields().unwrap(), 1);
        let state = store.load_raw_json_value().unwrap();
        let session = raw_session_object(&state, "pred0001").unwrap();
        assert!(!session.contains_key("pending_handoff_path"));
        assert!(!session.contains_key("pending_handoff_event_offset"));
        assert_eq!(session.get("last_handoff_path"), Some(&json!("x")));
        assert_eq!(store.strip_legacy_handoff_fields().unwrap(), 0);
    }

    #[test]
    fn forwarding_follows_the_successor_chain_to_a_live_session() {
        let store = store("forward", "stopped");
        let mut state = store.load_raw_json_value().unwrap();
        let sessions = ensure_sessions_array_mut(&mut state).unwrap();
        for (id, successor, status) in [
            ("pred0001", "child001", "stopped"),
            ("child001", "succ0001", "stopped"),
        ] {
            let session = session_object_mut(sessions, id).unwrap();
            session.insert("successor_session_id".to_owned(), json!(successor));
            session.insert("status".to_owned(), json!(status));
        }
        store.write_raw_json_value(&state).unwrap();
        let forwarded = |id: &str| store.forwarded_session(id).unwrap().map(|s| s.id);
        assert_eq!(forwarded("pred0001").as_deref(), Some("succ0001"));
        assert_eq!(forwarded("succ0001").as_deref(), Some("succ0001"));
        set_status(&store, "succ0001", "stopped");
        assert_eq!(forwarded("pred0001"), None);
    }

    #[test]
    fn the_queue_transfer_moves_each_row_once_and_drops_old_context_alerts() {
        let store = store("queue", "idle");
        let queue = store.queue_store.as_ref().unwrap();
        queue.ensure_schema().unwrap();
        let conn = rusqlite::Connection::open(queue.db_path()).unwrap();
        conn.execute_batch(
            "INSERT INTO message_queue (id, target_session_id, text, queued_at, message_category)
               VALUES ('m1', 'pred0001', 'hello', '2026-09-29T00:00:00Z', NULL),
                      ('m2', 'pred0001', 'ask', '2026-09-29T00:00:00Z', 'context_handoff'),
                      ('m3', 'pred0001', 'gauge', '2026-09-29T00:00:00Z', 'context_monitor'),
                      ('m4', 'child001', 'wake', '2026-09-29T00:00:00Z', NULL);
             UPDATE message_queue SET parent_session_id = 'pred0001' WHERE id = 'm4';
             INSERT INTO message_queue (id, target_session_id, text, queued_at, delivered_at)
               VALUES ('m5', 'pred0001', 'old', '2026-09-29T00:00:00Z', '2026-09-29T00:00:01Z');
             INSERT INTO codex_review_request_registrations
               (id, repo, pr_number, requester_session_id, notify_session_id, requested_at,
                attempt_count, poll_interval_seconds, retry_interval_seconds, state, is_active)
               VALUES ('r1', 'acme/widgets', 1660, 'pred0001', 'pred0001',
                       '2026-09-29T14:02:00Z', 0, 30, 60, 'requested', 1),
                      ('r2', 'acme/widgets', 1600, 'pred0001', 'pred0001',
                       '2026-09-29T10:00:00Z', 0, 30, 60, 'landed', 0);
             INSERT INTO scheduled_reminders (id, target_session_id, message, fire_at)
               VALUES ('s1', 'pred0001', 'check CI', '2026-09-29T15:00:00Z');
             INSERT INTO remind_registrations (id, target_session_id, soft_threshold_seconds,
               hard_threshold_seconds, registered_at, last_reset_at, cancel_on_reply_session_id)
               VALUES ('g1', 'pred0001', 60, 120, 'x', 'x', 'lead0001'),
                      ('g2', 'child001', 60, 120, 'x', 'x', 'pred0001');
             INSERT INTO rust_stop_notify_states (session_id, sender_session_id, armed_at)
               VALUES ('pred0001', 'lead0001', 'x'), ('child001', 'pred0001', 'x');
             INSERT INTO parent_wake_registrations (id, child_session_id, parent_session_id,
               period_seconds, registered_at)
               VALUES ('w1', 'pred0001', 'lead0001', 600, 'x'),
                      ('w2', 'child001', 'pred0001', 600, 'x');",
        )
        .unwrap();
        for _ in 0..2 {
            queue.hand_off_rows("pred0001", "succ0001").unwrap();
            queue.hand_off_messages("pred0001", "succ0001").unwrap();
        }
        let rows = |sql: &str| -> Vec<String> {
            let mut statement = conn.prepare(sql).unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            rows(
                "SELECT id || ':' || target_session_id || ':' || COALESCE(parent_session_id, '-')
                    FROM message_queue ORDER BY id"
            ),
            ["m1:succ0001:-", "m4:child001:succ0001", "m5:pred0001:-"]
        );
        assert_eq!(
            rows(
                "SELECT id || ':' || notify_session_id || ':' || requester_session_id
                    FROM codex_review_request_registrations ORDER BY id"
            ),
            ["r1:succ0001:succ0001", "r2:pred0001:pred0001"]
        );
        assert_eq!(
            rows("SELECT target_session_id FROM scheduled_reminders"),
            ["succ0001"]
        );
        assert_eq!(
            rows(
                "SELECT id || ':' || target_session_id || ':' || cancel_on_reply_session_id
                    FROM remind_registrations ORDER BY id"
            ),
            ["g1:succ0001:lead0001", "g2:child001:succ0001"]
        );
        assert_eq!(
            rows(
                "SELECT session_id || ':' || sender_session_id
                    FROM rust_stop_notify_states ORDER BY session_id"
            ),
            ["child001:succ0001", "succ0001:lead0001"]
        );
        assert_eq!(
            rows(
                "SELECT id || ':' || child_session_id || ':' || parent_session_id
                    FROM parent_wake_registrations ORDER BY id"
            ),
            ["w1:succ0001:lead0001", "w2:child001:succ0001"]
        );
        let pending = queue.handoff_pending_items("succ0001").unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending[0].starts_with("Codex review of PR #1660, requested "));
        assert!(pending[1].starts_with("Reminder at "));
        assert!(pending[1].ends_with(": check CI"));
    }
}
