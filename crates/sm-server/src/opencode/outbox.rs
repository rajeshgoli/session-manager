//! Ordered delivery through the persisted queue. HTTP never holds the registry.
use super::*;
use crate::opencode::DeliveryOutcome;

impl SessionStore {
    pub(super) fn schedule_runtime_outbox(&self, id: &str, runtime: &TmuxRuntime) -> Result<()> {
        let key = (self.state_file.clone(), id.to_owned());
        if !scheduled_drains()
            .lock()
            .map_err(|_| anyhow::anyhow!("outbox scheduler poisoned"))?
            .insert(key.clone())
        {
            return Ok(());
        }
        let claim = ScheduledDrain(key);
        let store = self.clone();
        let runtime = runtime.clone();
        let id = id.to_owned();
        thread::Builder::new()
            .name(format!("sm-queued-delivery-{id}"))
            .spawn(move || {
                let _claim = claim;
                if let Err(error) = store.drain_runtime_pending_messages_for_session(&id, &runtime)
                {
                    eprintln!("immediate queue delivery {id} deferred: {error:#}");
                }
            })?;
        Ok(())
    }
    pub(super) fn is_opencode_session(&self, id: &str) -> Result<bool> {
        Ok(self
            .get_session(id)?
            .is_some_and(|session| session.provider == "opencode"))
    }

    pub(super) fn send_opencode_input(
        &self,
        id: &str,
        request: SendCoreInputRequest,
        runtime: &TmuxRuntime,
    ) -> Result<Option<CoreInputResult>> {
        let queue = self
            .queue_store
            .as_ref()
            .context("opencode delivery requires the retained queue")?;
        let _clear = self.lock_clear_operation(id)?;
        let Some(record) = self.get_session(id)? else {
            return Ok(None);
        };
        let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        let _input = session_runtime.lock_session_input(&record.tmux_session)?;
        let _submission = self.lock_opencode_submission(id)?;
        let message_id = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let Some(session) = raw_session_object(&state, id) else {
                return Ok(None);
            };
            ensure_runtime_local_node(
                &json_text(session.get("node")).unwrap_or_else(default_node),
            )?;
            if raw_session_is_stopped(session)
                || session
                    .get("retirement_intent")
                    .is_some_and(|v| !v.is_null())
            {
                return Ok(Some(CoreInputResult {
                    ok: true,
                    session_id: id.into(),
                    delivered: false,
                    delivery_mode: request.delivery_mode,
                    notify_after_seconds: request.notify_after_seconds,
                    status: json_text(session.get("status")).unwrap_or_else(|| "stopped".into()),
                }));
            }
            super::opencode_clear::ensure_pending_clear_prompt(session, id, queue)?;
            let (text, sender) = format_send_input_text_raw(&state, &request);
            let metadata = queue_metadata_for_send_request(&state, id, &request, sender);
            let mode = normalized_delivery_mode(&request.delivery_mode);
            if request.parent_session_id.is_some() {
                if let Some(route) = active_reparent_route_request_for_session(&state, id)? {
                    persist_deferred_parent_input_intent(
                        &mut state,
                        &route,
                        &format!("parent-input:{}", generate_session_id()),
                        id,
                        &text,
                        &mode,
                        &metadata,
                    )?;
                    self.write_raw_json_value(&state)?;
                    return Ok(Some(CoreInputResult {
                        ok: true,
                        session_id: id.into(),
                        delivered: false,
                        delivery_mode: request.delivery_mode,
                        notify_after_seconds: request.notify_after_seconds,
                        status: self.get_session(id)?.context("session disappeared")?.status,
                    }));
                }
            }
            queue.enqueue_message_with_metadata(id, &text, &mode, metadata)?
        };
        self.drain_opencode_outbox_while_locked(id, runtime)?;
        Ok(Some(CoreInputResult {
            ok: true,
            session_id: id.into(),
            delivered: queue.message_delivered(&message_id)?,
            delivery_mode: request.delivery_mode,
            notify_after_seconds: request.notify_after_seconds,
            status: self.get_session(id)?.context("session disappeared")?.status,
        }))
    }

    /// Clear and restore use the same outer lock; retirement and handoff
    /// admission use the submission lock. Input locks also exclude native input.
    pub(crate) fn drain_opencode_outbox(&self, id: &str, runtime: &TmuxRuntime) -> Result<()> {
        let _clear = self.lock_clear_operation(id)?;
        let Some(record) = self.get_session(id)? else {
            return Ok(());
        };
        let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        let _input = session_runtime.lock_session_input(&record.tmux_session)?;
        let _submission = self.lock_opencode_submission(id)?;
        self.drain_opencode_outbox_while_locked(id, runtime)
    }

    pub(super) fn drain_opencode_outbox_while_locked(
        &self,
        id: &str,
        runtime: &TmuxRuntime,
    ) -> Result<()> {
        let Some(queue) = self.queue_store.as_ref() else {
            return Ok(());
        };
        loop {
            let Some(message) = queue.pending_messages_for_target(id, 1)?.into_iter().next() else {
                break;
            };
            let target = {
                let _guard = self.write_guard()?;
                let state = self.load_raw_json_value()?;
                opencode_delivery_target(&state, id, true)?
            };
            let Some(record) = target else { break };
            let driver = self.opencode_driver()?;
            let client = driver.client(
                record
                    .opencode
                    .as_ref()
                    .context("missing opencode binding")?,
            )?;
            let outcome = if message.message_category.as_deref() == Some("native_rename") {
                let Some(title) = extract_provider_native_rename_name(&message.text) else {
                    anyhow::bail!("invalid queued opencode rename")
                };
                client
                    .rename(
                        record
                            .provider_resume_id
                            .as_deref()
                            .context("missing conversation")?,
                        &title,
                    )
                    .map(|_| DeliveryOutcome::Accepted)
            } else {
                let binding = queue.bind_pending_provider_message(
                    id,
                    &message.id,
                    record
                        .provider_resume_id
                        .as_deref()
                        .context("missing conversation")?,
                )?;
                let text = {
                    let _guard = self.write_guard()?;
                    let state = self.load_raw_json_value()?;
                    let session = raw_session_object(&state, id).context("session disappeared")?;
                    text_with_task_reopen_notice(
                        session,
                        &message.delivery_text(OffsetDateTime::now_utc()),
                    )
                    .into_owned()
                };
                client.attempt_delivery(
                    &binding,
                    &text,
                    Duration::from_secs(driver.base_config().confirm_timeout_secs),
                )
            };
            match outcome {
                Ok(DeliveryOutcome::Accepted) => {}
                Ok(DeliveryOutcome::Unconfirmed) => break,
                Err(error) => {
                    eprintln!("opencode delivery {} retained: {error:#}", message.id);
                    break;
                }
            }
            self.complete_opencode_message(&record, &message, runtime, true)?;
        }
        Ok(())
    }

    fn complete_opencode_message(
        &self,
        record: &SessionRecord,
        message: &PendingMessage,
        runtime: &TmuxRuntime,
        mark_activity: bool,
    ) -> Result<()> {
        let queue = self
            .queue_store
            .as_ref()
            .context("opencode queue missing")?;
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let current = snapshot_from_raw_value(&state)?
            .sessions
            .into_iter()
            .find(|session| session.id == record.id)
            .context("opencode session disappeared")?;
        if current.opencode != record.opencode
            || current.provider_resume_id != record.provider_resume_id
            || current.session_credential_sha256 != record.session_credential_sha256
        {
            anyhow::bail!("opencode delivery binding changed before completion")
        }
        complete_runtime_message_delivery_with_sender_drain_raw(
            self, &mut state, runtime, queue, message, false,
        )?;
        if mark_activity
            && !current.is_stopped()
            && message.message_category.as_deref() != Some("native_rename")
        {
            let session = session_object_mut(ensure_sessions_array_mut(&mut state)?, &record.id)
                .context("session disappeared")?;
            mark_session_followup_activity(session, &now_rfc3339());
        }
        self.write_raw_json_value(&state)?;
        if message.notify_on_delivery {
            if let Some(sender) = message.sender_session_id.as_deref() {
                // An independent worker avoids nesting recipient locks and
                // also covers acknowledgements produced during ID resolution.
                self.schedule_runtime_outbox(sender, runtime)?;
            }
        }
        Ok(())
    }

    /// Must finish before clear or handoff changes a conversation/recipient.
    /// Unreachable rows retain their IDs and block the caller's transition.
    pub fn resolve_opencode_pending_bindings(&self, id: &str, runtime: &TmuxRuntime) -> Result<()> {
        let _clear = self.lock_clear_operation(id)?;
        let record = self.get_session(id)?.context("opencode session missing")?;
        let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        let _input = session_runtime.lock_session_input(&record.tmux_session)?;
        let _submission = self.lock_opencode_submission(id)?;
        self.resolve_opencode_pending_bindings_while_locked(&record, runtime)
    }

    /// The caller holds clear, input and submission locks through the later
    /// conversation/recipient change. Do not release them after resolution.
    pub(super) fn resolve_opencode_pending_bindings_while_locked(
        &self,
        record: &SessionRecord,
        runtime: &TmuxRuntime,
    ) -> Result<()> {
        let id = &record.id;
        let queue = self
            .queue_store
            .as_ref()
            .context("opencode queue missing")?;
        let driver = self.opencode_driver()?;
        let client = driver.client(
            record
                .opencode
                .as_ref()
                .context("missing opencode binding")?,
        )?;
        // Snapshot pending rows once; resolution never submits new prompts.
        for message in queue.pending_messages_for_target(id, i64::MAX as usize)? {
            if let Some(binding) = queue.pending_provider_message_binding(id, &message.id)? {
                if client
                    .message_exists(&binding)
                    .context("opencode server unreachable; pending messages unresolved")?
                {
                    self.complete_opencode_message(record, &message, runtime, false)?;
                } else if !queue.clear_pending_provider_message_binding(
                    id,
                    &message.id,
                    &binding,
                )? {
                    anyhow::bail!("opencode pending binding changed during resolution")
                }
            }
        }
        Ok(())
    }

    pub(super) fn retry_opencode_outboxes(&self) -> Result<usize> {
        let (Some(runtime), Some(queue)) =
            (self.delivery_runtime.as_ref(), self.queue_store.as_ref())
        else {
            return Ok(0);
        };
        let mut attempted = 0;
        let mut failures = Vec::new();
        for record in self
            .list_sessions(false)?
            .into_iter()
            .filter(|record| record.provider == "opencode")
        {
            if !queue.pending_messages_for_target(&record.id, 1)?.is_empty() {
                if let Err(error) = self.drain_opencode_outbox(&record.id, runtime) {
                    failures.push(format!("{}: {error:#}", record.id));
                }
                attempted += 1;
            }
        }
        if failures.is_empty() {
            Ok(attempted)
        } else {
            Err(anyhow::anyhow!(failures.join("; ")))
        }
    }
}

type DrainKey = (PathBuf, String);
fn scheduled_drains() -> &'static Mutex<BTreeSet<DrainKey>> {
    static DRAINS: std::sync::OnceLock<Mutex<BTreeSet<DrainKey>>> = std::sync::OnceLock::new();
    DRAINS.get_or_init(|| Mutex::new(BTreeSet::new()))
}
struct ScheduledDrain(DrainKey);
impl Drop for ScheduledDrain {
    fn drop(&mut self) {
        if let Ok(mut drains) = scheduled_drains().lock() {
            drains.remove(&self.0);
        }
    }
}

fn opencode_delivery_target(
    state: &Value,
    id: &str,
    require_active: bool,
) -> Result<Option<SessionRecord>> {
    let Some(raw) = raw_session_object(state, id) else {
        return Ok(None);
    };
    let record: SessionRecord = serde_json::from_value(Value::Object(raw.clone()))?;
    ensure_runtime_local_node(&record.node)?;
    if record.provider != "opencode" || record.provider_resume_id.is_none() {
        return Ok(None);
    }
    if require_active
        && (record.is_stopped()
            || handoff_fences_delivery_raw(raw)
            || raw
                .get("opencode_pending_clear")
                .is_some_and(|v| !v.is_null())
            || raw
                .get("opencode_clear_view_paused")
                .is_some_and(|v| v == &Value::Bool(true))
            || raw
                .get("opencode_pending_retire")
                .is_some_and(|v| !v.is_null())
            || raw.get("retirement_intent").is_some_and(|v| !v.is_null())
            || session_runtime_launch_records(state)?.iter().any(|launch| {
                launch.session_id == id
                    && matches!(
                        launch.status.as_str(),
                        "prepared" | "launching" | "teardown_pending"
                    )
            }))
    {
        return Ok(None);
    }
    Ok(Some(record))
}
