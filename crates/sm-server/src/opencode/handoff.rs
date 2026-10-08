//! Resolve the old conversation before successor creation or any work transfer.
use super::*;
use crate::handoff::policy::{HandoffPhase, HandoffRecord};

const BLOCKED: &str = "handoff blocked: unresolved deliveries to an unreachable conversation";
const DEFERRED: &str = "handoff deferred: unresolved Opencode deliveries";

impl SessionStore {
    /// A failed transfer can leave an empty successor after a crash. Keep its
    /// durable ID until verified teardown, and serialize against a fresh ask.
    pub(crate) fn cleanup_failed_opencode_handoff(
        &self,
        id: &str,
        successor_id: &str,
        runtime: &TmuxRuntime,
    ) -> Result<()> {
        let _clear = self.lock_clear_operation(id)?;
        let _submission = self.lock_opencode_submission(id)?;
        let state = self.load_raw_json_value()?;
        let Some(raw) = raw_session_object(&state, id) else {
            return Ok(());
        };
        let Some(mut handoff) = HandoffRecord::from_session(raw).and_then(Result::ok) else {
            return Ok(());
        };
        if json_text(raw.get("provider")).as_deref() != Some("opencode")
            || json_text(raw.get("successor_session_id")).is_some()
            || handoff.state != HandoffPhase::Failed
            || handoff.successor_session_id.as_deref() != Some(successor_id)
            || id == successor_id
        {
            return Ok(());
        }
        if let Some(successor) = self.get_session(successor_id)? {
            if successor.provider != "opencode" || successor.predecessor_session_id.is_some() {
                anyhow::bail!("failed handoff successor has acquired transfer lineage")
            }
            match self.retire_core_session_with_runtime_authorized(
                successor_id,
                RetireAuthority::handoff(id),
                None,
                runtime,
            )? {
                CoreRetireOutcome::Retired(_) | CoreRetireOutcome::NotFound => {}
                other => anyhow::bail!("retiring failed handoff successor: {other:?}"),
            }
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let raw = session_object_mut(ensure_sessions_array_mut(&mut state)?, id)
            .context("handoff predecessor disappeared")?;
        handoff.successor_session_id = None;
        raw.insert("handoff".into(), handoff.to_json());
        self.write_raw_json_value(&state)
    }

    pub(crate) fn opencode_handoff_ready(&self, id: &str, runtime: &TmuxRuntime) -> Result<bool> {
        if !self.is_opencode_session(id)? {
            return Ok(true);
        }
        let _clear = self.lock_clear_operation(id)?;
        let record = self
            .get_session(id)?
            .context("Opencode handoff predecessor disappeared")?;
        let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        let _input = session_runtime.lock_session_input(&record.tmux_session)?;
        let _submission = self.lock_opencode_submission(id)?;
        let now = OffsetDateTime::now_utc();
        let blocked_at = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let raw = raw_session_object(&state, id).context("session disappeared")?;
            let Some(handoff) = HandoffRecord::from_session(raw).and_then(Result::ok) else {
                return Ok(false);
            };
            if !matches!(
                handoff.state,
                HandoffPhase::Accepted | HandoffPhase::Spawning | HandoffPhase::Transferring
            ) || [
                "opencode_pending_clear",
                "opencode_pending_retire",
                "retirement_intent",
            ]
            .iter()
            .any(|key| raw.get(*key).is_some_and(|v| !v.is_null()))
                || raw.get("opencode_clear_view_paused") == Some(&Value::Bool(true))
            {
                return Ok(false);
            }
            let parse = |key: &str| -> Result<Option<OffsetDateTime>> {
                json_text(raw.get(key))
                    .map(|value| OffsetDateTime::parse(&value, &Rfc3339).map_err(Into::into))
                    .transpose()
            };
            let blocked = parse("opencode_handoff_delivery_blocked_at")?;
            if parse("opencode_handoff_delivery_last_attempt_at")?
                .is_some_and(|last| now - last < time::Duration::seconds(5))
            {
                return Ok(false);
            }
            blocked
        };
        let resolution = self.resolve_opencode_pending_bindings_while_locked(&record, runtime);
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let raw = session_object_mut(ensure_sessions_array_mut(&mut state)?, id)
            .context("session disappeared")?;
        let mut handoff = HandoffRecord::from_session(raw)
            .and_then(Result::ok)
            .context("handoff disappeared")?;
        if resolution.is_ok() {
            raw.remove("opencode_handoff_delivery_blocked_at");
            raw.remove("opencode_handoff_delivery_last_attempt_at");
            if handoff.error.as_deref() == Some(DEFERRED) {
                handoff.error = None;
                raw.insert("handoff".into(), handoff.to_json());
            }
            self.write_raw_json_value(&state)?;
            return Ok(true);
        }
        let first = blocked_at.unwrap_or(now);
        raw.insert(
            "opencode_handoff_delivery_blocked_at".into(),
            json!(first.format(&Rfc3339)?),
        );
        raw.insert(
            "opencode_handoff_delivery_last_attempt_at".into(),
            json!(now.format(&Rfc3339)?),
        );
        if now - first >= time::Duration::minutes(10) {
            handoff.state = HandoffPhase::Failed;
            handoff.failed_at = Some(now.format(&Rfc3339)?);
            handoff.error = Some(BLOCKED.into());
            raw.insert("agent_status_text".into(), json!(BLOCKED));
        } else {
            handoff.error = Some(DEFERRED.into());
        }
        raw.insert("handoff".into(), handoff.to_json());
        self.write_raw_json_value(&state)?;
        Ok(false)
    }

    pub(crate) fn opencode_http_ready(&self, id: &str) -> Result<bool> {
        let record = self
            .get_session(id)?
            .context("Opencode session disappeared")?;
        if record.provider != "opencode" || record.is_stopped() {
            return Ok(false);
        }
        self.opencode_driver()?
            .client(
                record
                    .opencode
                    .as_ref()
                    .context("Opencode binding missing")?,
            )?
            .ready()
    }

    pub(crate) fn with_opencode_handoff_transfer<T>(
        &self,
        id: &str,
        runtime: &TmuxRuntime,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if !self.is_opencode_session(id)? {
            return operation();
        }
        let _clear = self.lock_clear_operation(id)?;
        let record = self
            .get_session(id)?
            .context("Opencode predecessor disappeared")?;
        let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        let _input = session_runtime.lock_session_input(&record.tmux_session)?;
        let _submission = self.lock_opencode_submission(id)?;
        self.resolve_opencode_pending_bindings_while_locked(&record, runtime)?;
        operation()
    }
}
