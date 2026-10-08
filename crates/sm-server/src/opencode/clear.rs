//! A persisted conversation switch, followed by repeatable host-side completion.
use super::*;
use crate::opencode::events::Activity;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingClear {
    token: String,
    old_conversation: String,
    conversation: String,
    prompt: Option<String>,
    message_id: String,
}

impl SessionStore {
    pub(super) fn clear_opencode_session(
        &self,
        id: &str,
        request: ClearSessionRequest,
        runtime: &TmuxRuntime,
    ) -> Result<CoreClearOutcome> {
        let _clear = self.lock_clear_operation(id)?;
        let Some(record) = self.get_session(id)? else {
            return Ok(CoreClearOutcome::NotFound);
        };
        let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        let _input = session_runtime.lock_session_input(&record.tmux_session)?;
        let _submission = self.lock_opencode_submission(id)?;
        let record = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let Some(raw) = raw_session_object(&state, id) else {
                return Ok(CoreClearOutcome::NotFound);
            };
            if let Some(detail) =
                clear_authorization_error(raw, request.requester_session_id.as_deref())
            {
                return Ok(CoreClearOutcome::Unauthorized(detail));
            }
            if raw_session_is_stopped(raw) {
                return Ok(CoreClearOutcome::NotRunning);
            }
            ensure_runtime_local_node(&json_text(raw.get("node")).unwrap_or_else(default_node))?;
            if handoff_fences_delivery_raw(raw)
                || raw.get("retirement_intent").is_some_and(|v| !v.is_null())
                || session_runtime_launch_records(&state)?.iter().any(|l| {
                    l.session_id == id
                        && matches!(
                            l.status.as_str(),
                            "prepared" | "launching" | "teardown_pending"
                        )
                })
            {
                return Ok(CoreClearOutcome::Conflict(
                    "Opencode lifecycle change is in progress".into(),
                ));
            }
            if raw
                .get("opencode_pending_clear")
                .is_some_and(|v| !v.is_null())
            {
                return Ok(CoreClearOutcome::Conflict(
                    "Previous Opencode clear is still completing; retry shortly".into(),
                ));
            }
            serde_json::from_value::<SessionRecord>(Value::Object(raw.clone()))?
        };
        let driver = self.opencode_driver()?;
        let client = driver.client(
            record
                .opencode
                .as_ref()
                .context("missing opencode binding")?,
        )?;
        let old = record
            .provider_resume_id
            .as_deref()
            .context("missing conversation")?;
        let idle = || -> Result<bool> {
            Ok(client
                .status()?
                .get(old)
                .map(Activity::from_value)
                .transpose()?
                .unwrap_or(Activity::Idle)
                == Activity::Idle)
        };
        let stopped = (|| -> Result<()> {
            if idle()? {
                return Ok(());
            }
            client.abort(old)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while !idle()? {
                if Instant::now() >= deadline {
                    anyhow::bail!("conversation is still busy")
                }
                thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        })();
        if stopped.is_err() {
            return Ok(CoreClearOutcome::Conflict(
                "opencode conversation did not stop; clear not applied".into(),
            ));
        }
        if self
            .resolve_opencode_pending_bindings_while_locked(&record, runtime)
            .is_err()
        {
            return Ok(CoreClearOutcome::Conflict(
                "opencode server unreachable; pending messages unresolved".into(),
            ));
        }
        let conversation =
            client.create_conversation(record.friendly_name.as_deref().unwrap_or(id))?;
        let pending = PendingClear {
            token: generate_session_id(),
            old_conversation: old.into(),
            conversation,
            prompt: request
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned),
            message_id: format!("opencode-clear:{}", generate_session_id()),
        };
        {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let raw = session_object_mut(ensure_sessions_array_mut(&mut state)?, id)
                .context("session disappeared")?;
            if raw_session_is_stopped(raw)
                || json_text(raw.get("provider_resume_id")).as_deref() != Some(old)
                || json_text(raw.get("session_credential_sha256"))
                    != record.session_credential_sha256
            {
                anyhow::bail!("Opencode binding changed before clear commit")
            }
            raw.insert("provider_resume_id".into(), json!(pending.conversation));
            reset_session_after_clear(raw, &now_rfc3339());
            raw.insert("status".into(), json!("idle"));
            raw.insert(
                "opencode_pending_clear".into(),
                serde_json::to_value(&pending)?,
            );
            self.write_raw_json_value(&state)?;
        }
        if let Err(error) = self.finish_opencode_clear_while_locked(id, runtime) {
            eprintln!("opencode clear {id} retained for recovery: {error:#}");
            return Ok(CoreClearOutcome::Conflict(
                "Opencode conversation switched; clear completion pending recovery".into(),
            ));
        }
        self.drain_opencode_outbox_while_locked(id, runtime)?;
        Ok(CoreClearOutcome::Cleared(CoreClearResult {
            status: "cleared".into(),
            session_id: id.into(),
        }))
    }

    fn finish_opencode_clear_while_locked(&self, id: &str, runtime: &TmuxRuntime) -> Result<()> {
        let (record, pending) = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let raw = raw_session_object(&state, id).context("session disappeared")?;
            let Some(value) = raw.get("opencode_pending_clear").filter(|v| !v.is_null()) else {
                return Ok(());
            };
            if raw_session_is_stopped(raw)
                || handoff_fences_delivery_raw(raw)
                || raw.get("retirement_intent").is_some_and(|v| !v.is_null())
                || session_runtime_launch_records(&state)?.iter().any(|l| {
                    l.session_id == id
                        && matches!(
                            l.status.as_str(),
                            "prepared" | "launching" | "teardown_pending"
                        )
                })
            {
                anyhow::bail!("Opencode lifecycle change blocks clear completion")
            }
            (
                serde_json::from_value::<SessionRecord>(Value::Object(raw.clone()))?,
                serde_json::from_value::<PendingClear>(value.clone())?,
            )
        };
        if record.provider_resume_id.as_deref() != Some(&pending.conversation) {
            anyhow::bail!("Opencode clear conversation changed")
        }
        let artifact = PathBuf::from(
            &record
                .opencode
                .as_ref()
                .context("missing opencode binding")?
                .state_dir,
        )
        .join("usage.jsonl");
        for conversation in [&pending.old_conversation, &pending.conversation] {
            self.seat_session_store.append(
                id,
                "opencode",
                conversation,
                Some(&artifact.to_string_lossy()),
            )?;
        }
        self.cancel_context_monitor_alerts(id)?;
        enqueue_clear_prompt(
            &pending,
            id,
            self.queue_store
                .as_ref()
                .context("Opencode clear requires the retained queue")?,
        )?;
        self.opencode_driver()?.replace_attach(&record, runtime)?;
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let raw = session_object_mut(ensure_sessions_array_mut(&mut state)?, id)
            .context("session disappeared")?;
        if raw
            .get("opencode_pending_clear")
            .and_then(|v| v["token"].as_str())
            != Some(&pending.token)
            || json_text(raw.get("provider_resume_id")).as_deref() != Some(&pending.conversation)
        {
            anyhow::bail!("Opencode clear changed before completion")
        }
        raw.remove("opencode_pending_clear");
        self.write_raw_json_value(&state)
    }

    pub(crate) fn recover_opencode_clears(&self, runtime: &TmuxRuntime) -> Result<()> {
        let ids: Vec<String> = {
            let state = self.load_raw_json_value()?;
            snapshot_from_raw_value(&state)?
                .sessions
                .into_iter()
                .filter(|s| s.provider == "opencode" && !s.is_stopped())
                .filter(|s| {
                    raw_session_object(&state, &s.id).is_some_and(|raw| {
                        raw.get("opencode_pending_clear")
                            .is_some_and(|v| !v.is_null())
                    })
                })
                .map(|s| s.id)
                .collect()
        };
        for id in ids {
            let _clear = self.lock_clear_operation(&id)?;
            let Some(record) = self.get_session(&id)? else {
                continue;
            };
            let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
            let _input = session_runtime.lock_session_input(&record.tmux_session)?;
            let _submission = self.lock_opencode_submission(&id)?;
            if let Err(error) = self.finish_opencode_clear_while_locked(&id, runtime) {
                eprintln!("opencode clear recovery {id} deferred: {error:#}");
                continue;
            }
            self.drain_opencode_outbox_while_locked(&id, runtime)?;
        }
        Ok(())
    }
}

pub(super) fn ensure_pending_clear_prompt(
    raw: &Map<String, Value>,
    id: &str,
    queue: &RetainedQueueStore,
) -> Result<()> {
    if let Some(value) = raw.get("opencode_pending_clear").filter(|v| !v.is_null()) {
        enqueue_clear_prompt(
            &serde_json::from_value::<PendingClear>(value.clone())?,
            id,
            queue,
        )?;
    }
    Ok(())
}

fn enqueue_clear_prompt(
    pending: &PendingClear,
    id: &str,
    queue: &RetainedQueueStore,
) -> Result<()> {
    if let Some(prompt) = &pending.prompt {
        queue.enqueue_message_once_with_metadata(
            &pending.message_id,
            id,
            prompt,
            "sequential",
            QueueMessageMetadata::default(),
        )?;
    }
    Ok(())
}
