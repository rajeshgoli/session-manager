//! Atomically commit the conversation checkpoint and cached session activity.
//! Retained effects bridge the JSON registry to SQL history and the usage file.
//! They are drained in order; a failed write keeps the head for safe recovery.
use super::*;
use crate::opencode::{
    events::{Activity, Effect, Projection, UsageJournal},
    OpencodeConfig,
};

pub enum OpencodeEventInput<'a> {
    Live(&'a Value),
    Backfill {
        messages: &'a [Value],
        activity: Activity,
    },
}

/// Retained until the caller schedules the handoff check and acknowledges this
/// exact signal. An older acknowledgement cannot erase a newer applied stop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpencodeStopSignal {
    pub key: String,
    pub conversation: String,
}

fn pending_stop(session: &Map<String, Value>) -> Result<Option<OpencodeStopSignal>> {
    if raw_session_is_stopped(session) {
        return Ok(None);
    }
    Ok(session
        .get("opencode_stop_signal")
        .cloned()
        .map(serde_json::from_value::<OpencodeStopSignal>)
        .transpose()?
        .filter(|signal| {
            session.get("provider_resume_id").and_then(Value::as_str) == Some(&signal.conversation)
        }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingEffect {
    key: String,
    conversation: String,
    at: String,
    turn_started: Option<String>,
    effect: Effect,
    model_id: String,
    context_window: u64,
    working_dir: String,
    state_dir: PathBuf,
    tool_db: PathBuf,
}

impl SessionStore {
    /// Apply a live frame or reconnect snapshot. Callers supply all persisted
    /// host-generated message IDs, including the brief, before decoding owner
    /// input. A true result requests the same handoff check as an applied Stop;
    /// read and acknowledge the retained signal only after scheduling that check.
    pub fn apply_opencode_events(
        &self,
        session_id: &str,
        input: OpencodeEventInput<'_>,
        config: &OpencodeConfig,
        generated: &BTreeSet<String>,
        tool_db: &Path,
    ) -> Result<bool> {
        config.validate()?;
        let guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let (mut stop_applied, mut owner_wake) =
            self.drain_opencode_effects(&mut state, session_id)?;
        let session = raw_session_object(&state, session_id).context("unknown opencode session")?;
        if json_text(session.get("provider")).as_deref() != Some("opencode")
            || raw_session_is_stopped(session)
        {
            drop(guard);
            self.wake_opencode_owner(owner_wake);
            return Ok(stop_applied);
        }
        let conversation = json_text(session.get("provider_resume_id"))
            .context("opencode session missing conversation")?;
        let binding: crate::opencode::RuntimeBinding = serde_json::from_value(
            session
                .get("opencode")
                .cloned()
                .context("missing binding")?,
        )?;
        binding.validate()?;
        let mut projection = session
            .get("opencode_event_projection")
            .cloned()
            .map(serde_json::from_value::<Projection>)
            .transpose()?
            .filter(|projection| projection.conversation_id == conversation)
            .unwrap_or(Projection::new(&conversation, Activity::Idle)?);
        let effects = match input {
            OpencodeEventInput::Live(event) => projection.live(event, generated)?,
            OpencodeEventInput::Backfill { messages, activity } => {
                projection.backfill(messages, activity, generated)?
            }
        };
        // Even a replay producing no effects can advance a completed cursor.
        let now = OffsetDateTime::now_utc();
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let session = session_object_mut(sessions, session_id).context("session disappeared")?;
        let working_dir = json_text(session.get("working_dir")).unwrap_or_default();
        let mut sequence = session
            .get("opencode_effect_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut pending = Vec::new();
        for effect in effects {
            sequence = sequence
                .checked_add(1)
                .context("opencode effect sequence exhausted")?;
            let observed_at = now + time::Duration::nanoseconds(i64::try_from(pending.len())?);
            let native_time = match &effect {
                Effect::TurnStart { message_id, .. } => message_id
                    .as_deref()
                    .and_then(|id| projection.message_time_ms(id, false)),
                Effect::TurnStop { message_id, .. } => message_id
                    .as_deref()
                    .and_then(|id| projection.message_time_ms(id, true)),
                Effect::OwnerPrompt { message_id, .. } => {
                    projection.message_time_ms(message_id, false)
                }
                _ => None,
            };
            let at = native_time
                .and_then(|ms| {
                    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
                })
                .unwrap_or(observed_at)
                .format(&Rfc3339)?;
            session.insert("last_activity".into(), json!(observed_at.format(&Rfc3339)?));
            match &effect {
                Effect::TurnStart { .. } => {
                    session.insert("status".into(), json!("running"));
                    session.insert("activity_hook_at".into(), json!(at));
                    session.insert("activity_turn_start_hook_at".into(), json!(at));
                    session.insert("agent_task_completed_at".into(), Value::Null);
                }
                Effect::TurnStop { text, .. } => {
                    increment_completed_turns(session);
                    session.insert("status".into(), json!("idle"));
                    session.insert("activity_hook_at".into(), json!(at));
                    session.insert("agent_status_text".into(), Value::Null);
                    session.insert("agent_status_at".into(), Value::Null);
                    session.insert("last_action_summary".into(), json!(text));
                }
                Effect::Tool { name, .. } => {
                    session.insert("status".into(), json!("running"));
                    session.insert("activity_hook_at".into(), json!(at));
                    session.insert("last_tool_name".into(), json!(name));
                    session.insert("last_tool_call".into(), json!(at));
                    session.insert("agent_task_completed_at".into(), Value::Null);
                }
                Effect::Title(title) => {
                    if session.get("native_title").and_then(Value::as_str) != Some(title) {
                        session.insert(
                            "native_title_updated_at_ns".into(),
                            json!(observed_at.unix_timestamp_nanos()),
                        );
                    }
                    session.insert("native_title".into(), json!(title));
                }
                Effect::Error(error) => {
                    session.insert("last_provider_error".into(), json!(error));
                }
                _ => {}
            }
            if matches!(
                effect,
                Effect::TurnStop { .. }
                    | Effect::OwnerPrompt { .. }
                    | Effect::Tool { .. }
                    | Effect::Usage(_)
                    | Effect::Compacted
            ) {
                pending.push(PendingEffect {
                    key: format!("opencode:{session_id}:{conversation}:{sequence}"),
                    conversation: conversation.clone(),
                    at,
                    turn_started: json_text(session.get("activity_turn_start_hook_at")),
                    effect,
                    model_id: config.model_id.clone(),
                    context_window: config.context_window,
                    working_dir: working_dir.clone(),
                    state_dir: PathBuf::from(&binding.state_dir),
                    tool_db: tool_db.to_path_buf(),
                });
            }
        }
        session.insert("provider_event_cursor".into(), json!(projection.cursor));
        session.insert(
            "opencode_event_projection".into(),
            serde_json::to_value(projection)?,
        );
        session.insert("opencode_effect_sequence".into(), json!(sequence));
        session.insert(
            "opencode_pending_effects".into(),
            serde_json::to_value(pending)?,
        );
        self.write_raw_json_value(&state)?;
        let (stop, wake) = self.drain_opencode_effects(&mut state, session_id)?;
        stop_applied |= stop;
        owner_wake |= wake;
        drop(guard);
        self.wake_opencode_owner(owner_wake);
        Ok(stop_applied)
    }

    /// Run at reader startup, including for a session retired after its final
    /// frame. History/usage still commit; old-conversation context never does.
    pub fn recover_opencode_effects(&self, session_id: &str) -> Result<bool> {
        let guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let (stop, wake) = self.drain_opencode_effects(&mut state, session_id)?;
        drop(guard);
        self.wake_opencode_owner(wake);
        Ok(stop)
    }

    pub fn opencode_pending_stop_signal(
        &self,
        session_id: &str,
    ) -> Result<Option<OpencodeStopSignal>> {
        let state = self.load_parsed_state()?;
        raw_session_object(&state.raw, session_id)
            .map(pending_stop)
            .transpose()
            .map(Option::flatten)
    }

    pub fn acknowledge_opencode_stop_signal(
        &self,
        session_id: &str,
        expected: &OpencodeStopSignal,
    ) -> Result<bool> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let Some(session) = session_object_mut(ensure_sessions_array_mut(&mut state)?, session_id)
        else {
            return Ok(false);
        };
        let stored = session
            .get("opencode_stop_signal")
            .cloned()
            .map(serde_json::from_value::<OpencodeStopSignal>)
            .transpose()?;
        if stored.as_ref() != Some(expected) {
            return Ok(false);
        }
        session.remove("opencode_stop_signal");
        self.write_raw_json_value(&state)?;
        Ok(true)
    }

    fn wake_opencode_owner(&self, needed: bool) {
        if needed {
            if let Some(wake) = &self.owner_answered_wake {
                (wake.0)();
            }
        }
    }

    fn drain_opencode_effects(&self, state: &mut Value, session_id: &str) -> Result<(bool, bool)> {
        let mut stop = raw_session_object(state, session_id)
            .map(pending_stop)
            .transpose()?
            .flatten()
            .is_some();
        let mut owner_wake = false;
        while let Some(session) = raw_session_object(state, session_id) {
            let Some(raw) = session
                .get("opencode_pending_effects")
                .and_then(Value::as_array)
                .and_then(|rows| rows.first())
            else {
                break;
            };
            let pending: PendingEffect = serde_json::from_value(raw.clone())?;
            let current = !raw_session_is_stopped(session)
                && session.get("provider_resume_id").and_then(Value::as_str)
                    == Some(&pending.conversation);
            let at = OffsetDateTime::parse(&pending.at, &Rfc3339)?;
            match &pending.effect {
                Effect::TurnStop { text, .. } => {
                    let store = self
                        .turn_message_store()
                        .context("opencode history requires retained queue")?;
                    let timing = pending
                        .turn_started
                        .as_deref()
                        .map(|start| OffsetDateTime::parse(start, &Rfc3339))
                        .transpose()?
                        .map_or(
                            crate::turn_messages::ReplyTiming::AtMessage,
                            crate::turn_messages::ReplyTiming::Started,
                        );
                    store.record_turn_with_receipt(
                        session_id,
                        "opencode",
                        at,
                        timing,
                        text,
                        Some(&pending.key),
                    )?;
                    stop |= current;
                }
                Effect::OwnerPrompt { .. } => {
                    let queue = self
                        .queue_store
                        .as_ref()
                        .context("opencode owner input requires retained queue")?;
                    crate::owner_messages::OwnerMessageStore::new(queue.db_path().to_path_buf())
                        .answer_session_with_receipt(
                            session_id,
                            "opencode_prompt",
                            &pending.at,
                            &pending.key,
                        )?;
                    // Repeating this wake is harmless, including after a crash
                    // between the SQL commit and the registry acknowledgement.
                    owner_wake = true;
                }
                Effect::Tool {
                    part_id,
                    call_id,
                    name,
                    input,
                } => {
                    crate::tool_usage::log_tool_usage_with_receipt(
                        &pending.tool_db,
                        crate::tool_usage::ToolUsageEvent {
                            session_id: Some(session_id),
                            claude_session_id: Some(&pending.conversation),
                            session_name: session.get("friendly_name").and_then(Value::as_str),
                            parent_session_id: session
                                .get("parent_session_id")
                                .and_then(Value::as_str),
                            hook_type: "PreToolUse",
                            tool_name: name,
                            tool_input: Some(input),
                            tool_response: None,
                            tool_use_id: Some(if call_id.is_empty() { part_id } else { call_id }),
                            cwd: Some(&pending.working_dir),
                            agent_id: Some(session_id),
                        },
                        Some(&pending.key),
                    )?;
                }
                Effect::Usage(usage) => {
                    UsageJournal::open(&pending.state_dir.join("usage.jsonl"))?.append(
                        usage,
                        &pending.working_dir,
                        &pending.model_id,
                        &pending.at,
                    )?;
                    if current {
                        let total = i64::try_from(usage.total_input()?)?;
                        let window = i64::try_from(pending.context_window)?;
                        let event = ContextUsageEvent {
                            session_id: session_id.into(),
                            used_percentage: Some(total as f64 / window as f64 * 100.0),
                            total_input_tokens: Some(total),
                            context_window_tokens: Some(window),
                            model_id: Some(pending.model_id.clone()),
                            emitted_at: Some(pending.at.clone()),
                            ..Default::default()
                        };
                        self.apply_context_usage_update(
                            state,
                            session_id,
                            &event,
                            self.delivery_runtime.as_ref(),
                        )?;
                    }
                }
                Effect::Compacted if current => {
                    let session = session_object_mut(ensure_sessions_array_mut(state)?, session_id)
                        .context("missing session")?;
                    session.insert("context_compaction_active".into(), json!(false));
                    clear_context_snapshot(session);
                }
                _ => {}
            }
            let session = session_object_mut(ensure_sessions_array_mut(state)?, session_id)
                .context("missing session")?;
            if current && matches!(pending.effect, Effect::TurnStop { .. }) {
                session.insert(
                    "opencode_stop_signal".into(),
                    serde_json::to_value(OpencodeStopSignal {
                        key: pending.key,
                        conversation: pending.conversation,
                    })?,
                );
            }
            session
                .get_mut("opencode_pending_effects")
                .and_then(Value::as_array_mut)
                .context("missing pending opencode effects")?
                .remove(0);
            self.write_raw_json_value(state)?;
        }
        Ok((stop, owner_wake))
    }
}

#[cfg(test)]
#[path = "events_store_tests.rs"]
mod tests;
