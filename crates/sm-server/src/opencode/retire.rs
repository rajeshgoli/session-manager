//! Authorized, durable teardown. Capacity is released only after owner proof.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
struct PendingRetire {
    token: String,
    record: SessionRecord,
    provenance: TerminalProvenance,
}

impl SessionStore {
    pub(super) fn retire_opencode_session(
        &self,
        id: &str,
        authority: RetireAuthority,
        credential: Option<&str>,
        runtime: &TmuxRuntime,
        if_finished_idle: bool,
        live_activity_blocks_retire: &dyn Fn(&SessionRecord) -> bool,
    ) -> Result<CoreRetireOutcome> {
        let _clear = self.lock_clear_operation(id)?;
        let Some(record) = self.get_session(id)? else {
            return Ok(CoreRetireOutcome::NotFound);
        };
        let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        let _input = session_runtime.lock_session_input(&record.tmux_session)?;
        let _submission = self.lock_opencode_submission(id)?;
        {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            ensure_session_not_reparent_fenced(&state, id)?;
            let sessions = snapshot_from_raw_value(&state)?.sessions;
            if !authority.is_authorized(&sessions, credential) {
                return Ok(CoreRetireOutcome::Forbidden);
            }
            let Some(raw) = raw_session_object(&state, id) else {
                return Ok(CoreRetireOutcome::NotFound);
            };
            if !authority.can_retire(raw) {
                return Ok(authority.rejection_outcome(raw));
            }
            if raw_session_is_stopped(raw)
                && completion_status_is_retired(json_text(raw.get("completion_status")).as_deref())
                && raw
                    .get("terminal_provenance")
                    .and_then(|v| v["tmux_disposition"].as_str())
                    == Some("verified_opencode_owner_teardown")
                && raw
                    .get("opencode_pending_retire")
                    .is_none_or(|v| v.is_null())
            {
                return self.retire_stopped_session(state, id, &authority);
            }
            let record = sessions
                .into_iter()
                .find(|s| s.id == id)
                .context("session disappeared")?;
            if !is_primary_node(&record.node) {
                return Ok(CoreRetireOutcome::UnsupportedNode(record.node));
            }
            // An earlier authorized request is already committed. A retry
            // resumes its proof rather than applying a new idle precondition.
            if raw
                .get("opencode_pending_retire")
                .is_none_or(|v| v.is_null())
            {
                if if_finished_idle
                    && (!raw_session_is_finished_idle(raw) || live_activity_blocks_retire(&record))
                {
                    return Ok(CoreRetireOutcome::PreconditionFailed);
                }
                let provenance = if record.is_stopped()
                    && completion_status_is_retired(record.completion_status.as_deref())
                {
                    record
                        .terminal_provenance
                        .clone()
                        .unwrap_or_else(|| authority.terminal_provenance(&now_rfc3339(), None))
                } else {
                    authority.terminal_provenance(&now_rfc3339(), None)
                };
                let pending = PendingRetire {
                    token: generate_session_id(),
                    record,
                    provenance: provenance.clone(),
                };
                let raw = session_object_mut(ensure_sessions_array_mut(&mut state)?, id)
                    .context("session disappeared")?;
                raw.insert(
                    "retirement_intent".into(),
                    serde_json::to_value(provenance)?,
                );
                raw.insert(
                    "opencode_pending_retire".into(),
                    serde_json::to_value(pending)?,
                );
                self.write_raw_json_value(&state)?;
            }
        }
        self.finish_opencode_retire_while_locked(id, runtime)?;
        Ok(CoreRetireOutcome::Retired(retire_result(id)))
    }

    fn finish_opencode_retire_while_locked(&self, id: &str, runtime: &TmuxRuntime) -> Result<()> {
        let pending: PendingRetire = {
            let state = self.load_raw_json_value()?;
            let raw = raw_session_object(&state, id).context("session disappeared")?;
            let Some(value) = raw.get("opencode_pending_retire").filter(|v| !v.is_null()) else {
                return Ok(());
            };
            serde_json::from_value(value.clone())?
        };
        if let Err(error) = self.opencode_driver()?.stop(&pending.record, runtime) {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let raw = session_object_mut(ensure_sessions_array_mut(&mut state)?, id)
                .context("session disappeared")?;
            raw.insert(
                "error_message".into(),
                json!(format!("opencode server did not exit: {error:#}")),
            );
            self.write_raw_json_value(&state)?;
            anyhow::bail!("opencode server did not exit; retirement retained for verified recovery: {error:#}")
        }
        {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let raw = raw_session_object(&state, id).context("session disappeared")?;
            let current: SessionRecord = serde_json::from_value(Value::Object(raw.clone()))?;
            if raw
                .get("opencode_pending_retire")
                .and_then(|v| v["token"].as_str())
                != Some(&pending.token)
                || current.session_credential_sha256 != pending.record.session_credential_sha256
                || current.opencode != pending.record.opencode
                || current.provider_resume_id != pending.record.provider_resume_id
                || current.tmux_socket_name != pending.record.tmux_socket_name
                || current.tmux_session != pending.record.tmux_session
            {
                anyhow::bail!("Opencode retirement binding changed before finalization")
            }
            finalize_active_credential_rotations_for_terminal_session(&mut state, id)?;
            let mut launches = session_runtime_launch_records(&state)?;
            for launch in launches.iter_mut().filter(|l| {
                l.session_id == id
                    && matches!(
                        l.status.as_str(),
                        "prepared" | "launching" | "teardown_pending"
                    )
            }) {
                launch.status = "failed".into();
                launch.failure_reason =
                    Some("Opencode runtime retired with verified teardown".into());
            }
            store_session_runtime_launch_records(&mut state, &launches)?;
            let raw = session_object_mut(ensure_sessions_array_mut(&mut state)?, id)
                .context("session disappeared")?;
            let was_running = !raw_session_is_stopped(raw);
            let already_retired =
                completion_status_is_retired(json_text(raw.get("completion_status")).as_deref());
            let recipient_name = raw_session_display_name(raw, id);
            let now = now_rfc3339();
            let mut provenance = pending.provenance;
            provenance.tmux_disposition = Some("verified_opencode_owner_teardown".into());
            raw.insert("status".into(), json!("stopped"));
            if already_retired {
                raw.insert(
                    "terminal_provenance".into(),
                    serde_json::to_value(provenance)?,
                );
            } else {
                mark_session_retired(raw, &now, provenance);
                raw.insert("stopped_at".into(), json!(now));
                raw.insert("last_activity".into(), json!(now));
            }
            raw.insert("retirement_intent".into(), Value::Null);
            raw.remove("opencode_pending_retire");
            if json_text(raw.get("error_message"))
                .is_some_and(|s| s.starts_with("opencode server did not exit:"))
            {
                raw.remove("error_message");
            }
            if was_running {
                complete_stop_notify_after_stop_raw(
                    self,
                    &mut state,
                    Some(runtime),
                    id,
                    &recipient_name,
                )?;
            }
            self.write_raw_json_value(&state)?;
        }
        self.end_work_claims_after_retire(id);
        Ok(())
    }

    pub(crate) fn recover_opencode_retirements(&self, runtime: &TmuxRuntime) -> Result<()> {
        let state = self.load_raw_json_value()?;
        let ids: Vec<_> = snapshot_from_raw_value(&state)?
            .sessions
            .into_iter()
            .filter(|s| s.provider == "opencode" && is_primary_node(&s.node))
            .filter(|s| {
                raw_session_object(&state, &s.id).is_some_and(|r| {
                    r.get("opencode_pending_retire")
                        .is_some_and(|v| !v.is_null())
                })
            })
            .map(|s| s.id)
            .collect();
        for id in ids {
            let _clear = self.lock_clear_operation(&id)?;
            let Some(record) = self.get_session(&id)? else {
                continue;
            };
            let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
            let _input = session_runtime.lock_session_input(&record.tmux_session)?;
            let _submission = self.lock_opencode_submission(&id)?;
            if let Err(error) = self.finish_opencode_retire_while_locked(&id, runtime) {
                eprintln!("Opencode retirement recovery {id} deferred: {error:#}");
            }
        }
        Ok(())
    }
}
