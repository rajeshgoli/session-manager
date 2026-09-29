//! Handoff policy persistence and the tipping-point asks (spec Appendices B,
//! C and I.1). A child module of `sessions`, so it shares the session
//! store's write guard and raw-state helpers.

use super::*;
use crate::handoff::policy::{
    self, ask_text, claims_text, effective_policy, reminder_text, AskReason, EffectivePolicy,
    HandoffDefaults, HandoffOverride, HandoffPhase, HandoffRecord, HandoffTrigger, PolicyUpdate,
    SessionRef,
};

/// What `PUT /sessions/{id}/handoff-policy` did.
#[derive(Debug)]
pub enum HandoffPolicyOutcome {
    /// The effective policy object of Appendix I.2.
    Updated(Value),
    NotFound,
    Conflict(String),
}

/// A review request that may be a tipping point (Appendix C.3).
#[derive(Debug, Clone, Copy)]
pub enum ReviewAsk<'a> {
    Codex {
        pr_number: i64,
    },
    Doc {
        doc_title: &'a str,
        owner_name: &'a str,
    },
}

/// A session's handoff inputs, read from the raw store.
struct RawHandoffContext {
    defaults: HandoffDefaults,
    policy: EffectivePolicy,
    /// `Some(Err)` is a record this build cannot parse; it counts as present.
    record: Option<std::result::Result<HandoffRecord, ()>>,
    used_percentage: Option<f64>,
    stopped: bool,
}

fn raw_handoff_context(state: &Value, session_id: &str) -> Option<RawHandoffContext> {
    let session = raw_session_object(state, session_id)?;
    let defaults = HandoffDefaults::from_stored(state.get(policy::DEFAULTS_KEY));
    let provider = raw_session_provider(session);
    let override_ = HandoffOverride::from_stored(session.get(policy::OVERRIDE_KEY));
    let policy = effective_policy(
        &defaults,
        &provider,
        override_.as_ref(),
        provider_has_measured_context_gauge(&provider),
    );
    Some(RawHandoffContext {
        defaults,
        policy,
        record: HandoffRecord::from_session(session),
        used_percentage: session
            .get("context_used_percentage")
            .and_then(Value::as_f64),
        stopped: raw_session_is_stopped(session),
    })
}

fn raw_session_provider(session: &Map<String, Value>) -> String {
    json_text(session.get("provider"))
        .filter(|provider| !provider.trim().is_empty())
        .unwrap_or_else(|| "claude".to_owned())
}

fn set_handoff_record(
    state: &mut Value,
    session_id: &str,
    record: Option<&HandoffRecord>,
) -> Result<()> {
    let sessions = ensure_sessions_array_mut(state)?;
    if let Some(session) = session_object_mut(sessions, session_id) {
        match record {
            Some(record) => {
                session.insert(policy::STATE_KEY.to_owned(), record.to_json());
            }
            None => {
                session.remove(policy::STATE_KEY);
            }
        }
    }
    Ok(())
}

/// A session as handoff views name it: its display name, else its id.
pub(super) fn handoff_session_ref(session: &SessionRecord) -> SessionRef {
    SessionRef {
        id: session.id.clone(),
        name: session
            .cached_display_name()
            .unwrap_or_else(|| non_empty_or(session.name.clone(), &session.id)),
    }
}

/// The session JSON `handoff` object (Appendix I.2).
pub(super) fn handoff_view_for_record(
    session: &SessionRecord,
    defaults: &HandoffDefaults,
    session_refs: &BTreeMap<String, SessionRef>,
) -> Value {
    let provider = non_empty_or(session.provider.clone(), "claude");
    let override_ = HandoffOverride::from_stored(session.handoff_policy_override.as_ref());
    let policy = effective_policy(
        defaults,
        &provider,
        override_.as_ref(),
        provider_has_measured_context_gauge(&provider),
    );
    let record = session
        .handoff
        .as_ref()
        .filter(|value| !value.is_null())
        .and_then(|value| serde_json::from_value::<HandoffRecord>(value.clone()).ok());
    let lookup = |id: Option<&String>| {
        id.map(|id| {
            session_refs.get(id).cloned().unwrap_or_else(|| SessionRef {
                id: id.clone(),
                name: id.clone(),
            })
        })
    };
    let successor = lookup(
        session.successor_session_id.as_ref().or(record
            .as_ref()
            .and_then(|record| record.successor_session_id.as_ref())),
    );
    let predecessor = lookup(session.predecessor_session_id.as_ref());
    policy::view_json(
        &policy,
        record.as_ref(),
        successor.as_ref(),
        predecessor.as_ref(),
    )
}

impl SessionStore {
    /// The default policy, starting values filled in (`GET /handoff-defaults`).
    pub fn handoff_defaults(&self) -> Result<HandoffDefaults> {
        let state = self.load_raw_json_value()?;
        Ok(HandoffDefaults::from_stored(
            state.get(policy::DEFAULTS_KEY),
        ))
    }

    /// Merge `patch` into the default policy, then re-run the withdrawal and
    /// threshold checks for every running session (Appendix B). The inner
    /// error is a validation message naming the field.
    pub fn update_handoff_defaults(
        &self,
        patch: &Value,
    ) -> Result<std::result::Result<HandoffDefaults, String>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let current = HandoffDefaults::from_stored(state.get(policy::DEFAULTS_KEY));
        let mut next = match current.merged(patch) {
            Ok(next) => next,
            Err(error) => return Ok(Err(error)),
        };
        next.updated_at = Some(now_rfc3339());
        let session_ids = ensure_sessions_array_mut(&mut state)?
            .iter()
            .filter_map(|session| json_text(session.get("id")))
            .collect::<Vec<_>>();
        let was_enabled = session_ids
            .iter()
            .map(|id| raw_handoff_context(&state, id).is_some_and(|context| context.policy.enabled))
            .collect::<Vec<_>>();
        state
            .as_object_mut()
            .context("session state is not a JSON object")?
            .insert(policy::DEFAULTS_KEY.to_owned(), next.to_json());
        let runtime = self.delivery_runtime.clone();
        for (session_id, was_enabled) in session_ids.iter().zip(was_enabled) {
            self.recheck_handoff_policy(&mut state, session_id, was_enabled, runtime.as_ref())?;
        }
        self.write_raw_json_value(&state)?;
        Ok(Ok(next))
    }

    /// The effective policy object of one session (Appendix I.2).
    pub fn handoff_policy_view(&self, session_id: &str) -> Result<Option<Value>> {
        Ok(self
            .get_session(session_id)?
            .and_then(|session| session.handoff_view))
    }

    /// Apply an owner's policy change or Hand off now (Appendices I.1, C.4).
    pub fn update_handoff_policy(
        &self,
        session_id: &str,
        update: &PolicyUpdate,
        owner_name: &str,
    ) -> Result<HandoffPolicyOutcome> {
        {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let Some(context) = raw_handoff_context(&state, session_id) else {
                return Ok(HandoffPolicyOutcome::NotFound);
            };
            // Validate Hand off now before changing anything, so a refused
            // request leaves the override as it was.
            if update.ask_now {
                if context.stopped {
                    return Ok(HandoffPolicyOutcome::Conflict(
                        "session is not running".to_owned(),
                    ));
                }
                match &context.record {
                    None => {}
                    Some(Ok(record)) => {
                        return Ok(HandoffPolicyOutcome::Conflict(format!(
                            "handoff already {}",
                            record.state.as_str()
                        )));
                    }
                    Some(Err(())) => {
                        return Ok(HandoffPolicyOutcome::Conflict(
                            "handoff state is unreadable".to_owned(),
                        ));
                    }
                }
            }
            let runtime = self.delivery_runtime.clone();
            if update.changes_override() {
                let current = raw_session_object(&state, session_id).and_then(|session| {
                    HandoffOverride::from_stored(session.get(policy::OVERRIDE_KEY))
                });
                let next = update.apply(current.as_ref(), &now_rfc3339());
                let sessions = ensure_sessions_array_mut(&mut state)?;
                if let Some(session) = session_object_mut(sessions, session_id) {
                    match next {
                        Some(next) => {
                            session.insert(policy::OVERRIDE_KEY.to_owned(), next.to_json());
                        }
                        None => {
                            session.remove(policy::OVERRIDE_KEY);
                        }
                    }
                }
                // Hand off now in the same request is the owner's ask; a
                // threshold recheck must not claim the absent state first
                // and record it as a context ask. With ask_now the state was
                // validated absent above, so there is nothing to withdraw.
                if !update.ask_now {
                    // Explicitly setting it off always counts as switching it off.
                    let was_enabled =
                        context.policy.enabled || update.enabled == policy::FieldChange::Set(false);
                    self.recheck_handoff_policy(
                        &mut state,
                        session_id,
                        was_enabled,
                        runtime.as_ref(),
                    )?;
                }
            }
            if update.ask_now {
                self.ask_handoff(
                    &mut state,
                    session_id,
                    &AskReason::Owner { owner_name },
                    runtime.as_ref(),
                )?;
            }
            self.write_raw_json_value(&state)?;
        }
        Ok(match self.handoff_policy_view(session_id)? {
            Some(view) => HandoffPolicyOutcome::Updated(view),
            None => HandoffPolicyOutcome::NotFound,
        })
    }

    /// Decide whether a successful review request is a tipping point
    /// (Appendix C.3). Returns the D.1 text for the command to print; it is
    /// not queued, so the agent never sees it twice.
    pub fn review_handoff_ask(
        &self,
        session_id: &str,
        ask: ReviewAsk<'_>,
    ) -> Result<Option<String>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let Some(context) = raw_handoff_context(&state, session_id) else {
            return Ok(None);
        };
        let switch = match ask {
            ReviewAsk::Codex { .. } => context.defaults.ask_on_codex_review,
            ReviewAsk::Doc { .. } => context.defaults.ask_on_doc_review,
        };
        if context.stopped || !context.policy.enabled || !switch || context.record.is_some() {
            return Ok(None);
        }
        let percent = if context.policy.has_gauge {
            match context.used_percentage {
                Some(used) if used >= context.defaults.review_floor_percent => Some(used),
                // Below the floor, or a gauge provider with no reading yet.
                _ => return Ok(None),
            }
        } else {
            None
        };
        let reason = match ask {
            ReviewAsk::Codex { pr_number } => AskReason::ReviewRequest { percent, pr_number },
            ReviewAsk::Doc {
                doc_title,
                owner_name,
            } => AskReason::DocReview {
                percent,
                owner_name,
                doc_title,
            },
        };
        let record = HandoffRecord::asked(reason.trigger(), &now_rfc3339(), percent);
        set_handoff_record(&mut state, session_id, Some(&record))?;
        self.write_raw_json_value(&state)?;
        Ok(Some(ask_text(
            &reason,
            &self.handoff_claims_text(session_id),
        )))
    }

    /// Apply the threshold ask and the reminder to an accepted context sample
    /// (Appendices C.1 and C.2). The caller writes `state` when this returns
    /// true.
    pub(super) fn evaluate_handoff_sample(
        &self,
        state: &mut Value,
        session_id: &str,
        used_percentage: f64,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<bool> {
        let Some(context) = raw_handoff_context(state, session_id) else {
            return Ok(false);
        };
        if context.stopped || !context.policy.enabled {
            return Ok(false);
        }
        match context.record {
            None if used_percentage >= context.policy.threshold_percent => {
                self.ask_handoff(
                    state,
                    session_id,
                    &AskReason::Context {
                        percent: used_percentage,
                    },
                    runtime,
                )?;
                Ok(true)
            }
            Some(Ok(mut record))
                if record.state == HandoffPhase::Asked
                    && record.reminded_at.is_none()
                    && context
                        .policy
                        .reminder_percent
                        .is_some_and(|reminder| used_percentage >= reminder) =>
            {
                record.reminded_at = Some(now_rfc3339());
                let asked_percent = record
                    .asked_percent
                    .unwrap_or(context.policy.threshold_percent);
                set_handoff_record(state, session_id, Some(&record))?;
                self.queue_handoff_message(
                    state,
                    session_id,
                    &reminder_text(used_percentage, asked_percent),
                    runtime,
                )?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// After a policy change: withdraw an ask the new policy no longer
    /// supports (C.5), then ask a session already past a lowered threshold
    /// (C.1). Returns true when `state` changed.
    ///
    /// Switching the policy off withdraws any ask. A change that leaves an
    /// already-off policy off does not, so editing the defaults never undoes
    /// Hand off now on an agent whose policy was off to begin with.
    fn recheck_handoff_policy(
        &self,
        state: &mut Value,
        session_id: &str,
        was_enabled: bool,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<bool> {
        let Some(context) = raw_handoff_context(state, session_id) else {
            return Ok(false);
        };
        if context.stopped {
            return Ok(false);
        }
        match context.record {
            Some(Ok(record)) if record.state == HandoffPhase::Asked => {
                let below_threshold = record.trigger == HandoffTrigger::Context
                    && context
                        .used_percentage
                        .is_some_and(|used| used < context.policy.threshold_percent);
                let switched_off = was_enabled && !context.policy.enabled;
                if !switched_off && !below_threshold {
                    return Ok(false);
                }
                if let Some(queue) = &self.queue_store {
                    queue.cancel_pending_messages_from_sender_category(
                        session_id,
                        policy::MESSAGE_CATEGORY,
                    )?;
                }
                set_handoff_record(state, session_id, None)?;
                self.queue_handoff_message(state, session_id, policy::WITHDRAWN_TEXT, runtime)?;
                Ok(true)
            }
            None if context.policy.enabled && context.policy.has_gauge => {
                match context.used_percentage {
                    Some(used) if used >= context.policy.threshold_percent => {
                        self.ask_handoff(
                            state,
                            session_id,
                            &AskReason::Context { percent: used },
                            runtime,
                        )?;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            _ => Ok(false),
        }
    }

    /// Record state `asked` and queue D.1.
    fn ask_handoff(
        &self,
        state: &mut Value,
        session_id: &str,
        reason: &AskReason<'_>,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<()> {
        let record = HandoffRecord::asked(reason.trigger(), &now_rfc3339(), reason.percent());
        set_handoff_record(state, session_id, Some(&record))?;
        let text = ask_text(reason, &self.handoff_claims_text(session_id));
        self.queue_handoff_message(state, session_id, &text, runtime)
    }

    /// Asks, reminders and withdrawals come from the session itself, like
    /// context alerts, so a completed handoff can cancel leftovers by category.
    fn queue_handoff_message(
        &self,
        state: &mut Value,
        session_id: &str,
        text: &str,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<()> {
        self.queue_parent_message(
            state,
            session_id,
            session_id,
            text,
            policy::DELIVERY_MODE,
            policy::MESSAGE_CATEGORY,
            runtime,
        )
    }

    /// `<claims>` of D.1: the session's active claims in claim order.
    pub fn handoff_claims_text(&self, session_id: &str) -> String {
        let Some(queue) = &self.queue_store else {
            return claims_text(&[]);
        };
        let store = crate::work_claims::WorkClaimStore::new(queue.db_path().to_path_buf());
        let claims = match store.claims_for_session(session_id, true) {
            Ok(claims) => claims,
            Err(error) => {
                eprintln!("reading claims of {session_id} for a handoff ask failed: {error:#}");
                Vec::new()
            }
        };
        let items = claims
            .iter()
            .map(|view| (view.claim.kind().noun(), view.claim.number))
            .collect::<Vec<_>>();
        claims_text(&items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_claims::{ClaimSource, HolderState, SessionDirectory, SessionInfo};

    const ASK_TAIL: &str = " Stop at a logical point. sm will move your claims (none) and pending wakes to a fresh agent. Post what the next agent needs in your PR, ticket, or a file, then run `sm handoff --link <url>` or `sm handoff --path <file>` and end your turn.";

    fn temp_path(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        env::temp_dir().join(format!(
            "sm-handoff-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn session(id: &str, provider: &str, extra: Value) -> Value {
        let mut session = json!({
            "id": id,
            "name": format!("{provider}-{id}"),
            "working_dir": "/repo",
            "tmux_session": format!("{provider}-{id}"),
            "provider": provider,
            "status": "running",
            "created_at": "2026-09-29T00:00:00Z",
            "last_activity": "2026-09-29T00:01:00Z"
        });
        if let (Some(session), Some(extra)) = (session.as_object_mut(), extra.as_object()) {
            session.extend(extra.clone());
        }
        session
    }

    /// agent001 (claude), fork0001 (codex-fork), app00001 (codex-app).
    fn store(label: &str) -> SessionStore {
        store_with(label, json!({}))
    }

    fn store_with(label: &str, agent_extra: Value) -> SessionStore {
        let state_file = temp_path(&format!("{label}.json"));
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    session("agent001", "claude", agent_extra),
                    session("fork0001", "codex-fork", json!({})),
                    session("app00001", "codex-app", json!({})),
                ],
            })
            .to_string(),
        )
        .unwrap();
        SessionStore::new_with_queue(state_file, temp_path(&format!("{label}-queue.db")))
    }

    fn sample(store: &SessionStore, session_id: &str, percent: f64) {
        store
            .apply_context_usage_event(
                &ContextUsageEvent {
                    session_id: session_id.to_owned(),
                    used_percentage: Some(percent),
                    total_input_tokens: Some((percent * 1000.0) as i64),
                    ..ContextUsageEvent::default()
                },
                None,
            )
            .unwrap();
    }

    fn queued(store: &SessionStore, session_id: &str) -> Vec<String> {
        store
            .queue_store
            .as_ref()
            .unwrap()
            .pending_messages_for_target_by_category(session_id, policy::MESSAGE_CATEGORY, 20)
            .unwrap()
            .into_iter()
            .map(|message| message.text)
            .collect()
    }

    fn record(store: &SessionStore, session_id: &str) -> Option<HandoffRecord> {
        store
            .get_session(session_id)
            .unwrap()
            .unwrap()
            .handoff
            .map(|value| serde_json::from_value(value).unwrap())
    }

    fn update(store: &SessionStore, session_id: &str, body: Value) -> HandoffPolicyOutcome {
        store
            .update_handoff_policy(session_id, &PolicyUpdate::parse(&body).unwrap(), "Rajesh")
            .unwrap()
    }

    fn display(store: &SessionStore, session_id: &str) -> String {
        store.handoff_policy_view(session_id).unwrap().unwrap()["display"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn threshold_asks_once_reporting_the_actual_sample_without_context_monitor() {
        let store = store("threshold");
        sample(&store, "agent001", 30.0);
        assert!(queued(&store, "agent001").is_empty());
        assert_eq!(display(&store, "agent001"), "hands off at 35%");

        // A jump over the threshold asks once and reports where it landed.
        sample(&store, "agent001", 41.2);
        sample(&store, "agent001", 44.0);
        assert_eq!(
            queued(&store, "agent001"),
            vec![format!(
                "[sm context management] Your context is at 41%.{ASK_TAIL}"
            )]
        );
        let asked = record(&store, "agent001").unwrap();
        assert_eq!(asked.state, HandoffPhase::Asked);
        assert_eq!(asked.trigger, HandoffTrigger::Context);
        assert_eq!(asked.asked_percent, Some(41.2));
        assert!(display(&store, "agent001").starts_with("asked "));
    }

    #[test]
    fn stale_samples_never_ask() {
        let store = store_with(
            "stale",
            json!({"context_sampled_at": "2026-09-29T12:00:00Z", "context_used_percentage": 10.0}),
        );
        store
            .apply_context_usage_event(
                &ContextUsageEvent {
                    session_id: "agent001".to_owned(),
                    used_percentage: Some(60.0),
                    emitted_at: Some("2026-09-29T11:00:00Z".to_owned()),
                    ..ContextUsageEvent::default()
                },
                None,
            )
            .unwrap();
        assert!(queued(&store, "agent001").is_empty());
        assert!(record(&store, "agent001").is_none());
    }

    #[test]
    fn providers_off_by_default_are_never_asked_until_switched_on() {
        let store = store("providers");
        sample(&store, "fork0001", 60.0);
        assert!(queued(&store, "fork0001").is_empty());
        assert_eq!(display(&store, "fork0001"), "handoff off");

        // Switching it on asks immediately: it is already past 35%.
        update(&store, "fork0001", json!({"enabled": true}));
        assert_eq!(
            queued(&store, "fork0001"),
            vec![format!(
                "[sm context management] Your context is at 60%.{ASK_TAIL}"
            )]
        );
    }

    #[test]
    fn reminder_fires_once_then_shows_overdue() {
        let store = store("reminder");
        sample(&store, "agent001", 36.0);
        sample(&store, "agent001", 50.4);
        sample(&store, "agent001", 70.0);
        let messages = queued(&store, "agent001");
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[1],
            "[sm context management] Reminder: your context is at 50% and sm asked you to hand off at 36%. Finish the current step, post your handoff note, and run `sm handoff --link <url>` or `sm handoff --path <file>`."
        );
        assert!(record(&store, "agent001").unwrap().reminded_at.is_some());
        assert_eq!(display(&store, "agent001"), "handoff overdue");
    }

    #[test]
    fn no_reminder_when_threshold_is_at_or_above_it() {
        let store = store("noreminder");
        update(&store, "agent001", json!({"threshold_percent": 55}));
        sample(&store, "agent001", 56.0);
        sample(&store, "agent001", 80.0);
        assert_eq!(queued(&store, "agent001").len(), 1);
    }

    #[test]
    fn review_requests_ask_above_the_floor_only() {
        // Below the floor, then no reading, then above.
        let store = store("review");
        store
            .update_handoff_defaults(&json!({"threshold_percent": 90}))
            .unwrap()
            .unwrap();
        let codex = ReviewAsk::Codex { pr_number: 1660 };
        sample(&store, "agent001", 15.0);
        assert_eq!(store.review_handoff_ask("agent001", codex).unwrap(), None);
        sample(&store, "agent001", 24.0);
        assert_eq!(
            store.review_handoff_ask("agent001", codex).unwrap().unwrap(),
            format!("[sm context management] Your context is at 24% and you just requested a review of PR #1660, a good point to hand off.{ASK_TAIL}")
        );
        let asked = record(&store, "agent001").unwrap();
        assert_eq!(asked.trigger, HandoffTrigger::ReviewRequest);
        // A second request, of either kind, does nothing new.
        assert_eq!(store.review_handoff_ask("agent001", codex).unwrap(), None);
        let doc = ReviewAsk::Doc {
            doc_title: "Context handoff",
            owner_name: "Rajesh",
        };
        assert_eq!(store.review_handoff_ask("agent001", doc).unwrap(), None);
        // Printed by the command, not also queued.
        assert!(queued(&store, "agent001").is_empty());
    }

    #[test]
    fn gauge_provider_without_a_reading_is_not_asked() {
        let store = store("noreading");
        let codex = ReviewAsk::Codex { pr_number: 7 };
        assert_eq!(store.review_handoff_ask("agent001", codex).unwrap(), None);
    }

    #[test]
    fn review_switches_gate_each_command() {
        let store = store("switches");
        store
            .update_handoff_defaults(
                &json!({"ask_on_codex_review": false, "threshold_percent": 90}),
            )
            .unwrap()
            .unwrap();
        sample(&store, "agent001", 30.0);
        let codex = ReviewAsk::Codex { pr_number: 7 };
        assert_eq!(store.review_handoff_ask("agent001", codex).unwrap(), None);
        let doc = ReviewAsk::Doc {
            doc_title: "Context handoff",
            owner_name: "Rajesh",
        };
        assert_eq!(
            store.review_handoff_ask("agent001", doc).unwrap().unwrap(),
            format!("[sm context management] Your context is at 30% and you just asked Rajesh to review Context handoff, a good point to hand off. His review will wake your successor.{ASK_TAIL}")
        );
        assert_eq!(
            record(&store, "agent001").unwrap().trigger,
            HandoffTrigger::DocReview
        );
    }

    #[test]
    fn codex_app_has_no_gauge_so_the_floor_is_ignored() {
        let store = store("codexapp");
        let codex = ReviewAsk::Codex { pr_number: 1660 };
        assert_eq!(store.review_handoff_ask("app00001", codex).unwrap(), None);
        update(&store, "app00001", json!({"enabled": true}));
        assert_eq!(
            store.review_handoff_ask("app00001", codex).unwrap().unwrap(),
            format!("[sm context management] You just requested a review of PR #1660, a good point to hand off.{ASK_TAIL}")
        );
        let view = store.handoff_policy_view("app00001").unwrap().unwrap();
        assert_eq!(view["has_gauge"], json!(false));
        assert_eq!(view["source"], json!("override"));
    }

    #[test]
    fn ask_now_queues_the_owner_ask_once_whatever_the_policy() {
        let store = store("asknow");
        let HandoffPolicyOutcome::Updated(view) =
            update(&store, "fork0001", json!({"ask_now": true}))
        else {
            panic!("expected update");
        };
        assert_eq!(view["state"], json!("asked"));
        assert_eq!(
            queued(&store, "fork0001"),
            vec![format!(
                "[sm context management] Rajesh asked you to hand off.{ASK_TAIL}"
            )]
        );
        match update(&store, "fork0001", json!({"ask_now": true})) {
            HandoffPolicyOutcome::Conflict(detail) => assert_eq!(detail, "handoff already asked"),
            other => panic!("expected conflict, got {other:?}"),
        }
        assert!(matches!(
            update(&store, "missing1", json!({"ask_now": true})),
            HandoffPolicyOutcome::NotFound
        ));
    }

    #[test]
    fn ask_now_with_a_lowered_threshold_stays_an_owner_ask() {
        let store = store("asknowlower");
        sample(&store, "agent001", 30.0);
        update(
            &store,
            "agent001",
            json!({"ask_now": true, "threshold_percent": 25}),
        );
        assert_eq!(
            record(&store, "agent001").unwrap().trigger,
            HandoffTrigger::Owner
        );
        assert_eq!(
            queued(&store, "agent001"),
            vec![format!(
                "[sm context management] Rajesh asked you to hand off.{ASK_TAIL}"
            )]
        );
        // Raising the threshold later does not withdraw the owner's ask.
        update(&store, "agent001", json!({"threshold_percent": 60}));
        assert!(record(&store, "agent001").is_some());
    }

    #[test]
    fn raising_the_threshold_withdraws_a_context_ask_and_cancels_it() {
        let store = store("withdraw");
        sample(&store, "agent001", 36.0);
        assert_eq!(queued(&store, "agent001").len(), 1);
        update(&store, "agent001", json!({"threshold_percent": 45}));
        assert!(record(&store, "agent001").is_none());
        // The undelivered ask is cancelled; only the withdrawal remains.
        assert_eq!(
            queued(&store, "agent001"),
            vec![policy::WITHDRAWN_TEXT.to_owned()]
        );
        assert_eq!(display(&store, "agent001"), "hands off at 45%");
    }

    #[test]
    fn non_context_asks_withdraw_only_when_switched_off() {
        let store = store("withdrawowner");
        sample(&store, "agent001", 10.0);
        update(&store, "agent001", json!({"ask_now": true}));
        update(&store, "agent001", json!({"threshold_percent": 80}));
        assert_eq!(
            record(&store, "agent001").unwrap().trigger,
            HandoffTrigger::Owner
        );
        update(&store, "agent001", json!({"enabled": false}));
        assert!(record(&store, "agent001").is_none());
        assert_eq!(display(&store, "agent001"), "handoff off");

        // The default policy switching a provider off withdraws too.
        // Editing the defaults never undoes Hand off now on an agent whose
        // policy was already off; switching its provider off from on does.
        update(&store, "fork0001", json!({"ask_now": true}));
        store
            .update_handoff_defaults(&json!({"threshold_percent": 40}))
            .unwrap()
            .unwrap();
        assert!(
            record(&store, "fork0001").is_some(),
            "already off: owner ask stands"
        );
        store
            .update_handoff_defaults(&json!({"providers": {"codex-fork": true}}))
            .unwrap()
            .unwrap();
        store
            .update_handoff_defaults(&json!({"providers": {"codex-fork": false}}))
            .unwrap()
            .unwrap();
        assert!(record(&store, "fork0001").is_none());

        // An explicit off always withdraws, even when it was already off.
        update(&store, "fork0001", json!({"ask_now": true}));
        update(&store, "fork0001", json!({"enabled": false}));
        assert!(record(&store, "fork0001").is_none());
    }

    #[test]
    fn defaults_start_filled_merge_and_reask_below_a_lowered_threshold() {
        let store = store("defaults");
        let defaults = store.handoff_defaults().unwrap();
        assert_eq!(defaults, HandoffDefaults::default());
        sample(&store, "agent001", 30.0);
        assert!(queued(&store, "agent001").is_empty());

        let merged = store
            .update_handoff_defaults(&json!({"threshold_percent": 25}))
            .unwrap()
            .unwrap();
        assert_eq!(merged.threshold_percent, 25.0);
        assert!(merged.updated_at.is_some());
        assert_eq!(queued(&store, "agent001").len(), 1);

        // Raising it again withdraws.
        store
            .update_handoff_defaults(&json!({"threshold_percent": 40}))
            .unwrap()
            .unwrap();
        assert!(record(&store, "agent001").is_none());

        assert_eq!(
            store
                .update_handoff_defaults(&json!({"threshold_percent": 0}))
                .unwrap()
                .unwrap_err(),
            "threshold_percent must be a number in (0, 100]"
        );
        let stored = store.load_raw_json_value().unwrap()[policy::DEFAULTS_KEY].clone();
        assert_eq!(stored["threshold_percent"], json!(40));
        assert_eq!(stored["reminder_percent"], json!(50));
    }

    #[test]
    fn stopped_sessions_are_never_asked() {
        let store = store_with("stopped", json!({"status": "stopped"}));
        sample(&store, "agent001", 60.0);
        assert!(queued(&store, "agent001").is_empty());
        assert!(matches!(
            update(&store, "agent001", json!({"ask_now": true})),
            HandoffPolicyOutcome::Conflict(_)
        ));
    }

    #[test]
    fn ask_names_active_claims_in_claim_order() {
        let store = store("claims");
        let claims = crate::work_claims::WorkClaimStore::new(
            store.queue_store.as_ref().unwrap().db_path().to_path_buf(),
        );
        claims.ensure_schema().unwrap();
        let info = SessionInfo {
            id: "agent001".to_owned(),
            name: "claude-agent001".to_owned(),
            parent_session_id: None,
            state: HolderState::Working,
            stopped_at: None,
        };
        let directory = SessionDirectory::new([info.clone()]);
        for pr in [1660, 1661] {
            claims
                .claim_implicit(
                    "acme/widgets",
                    pr,
                    &info,
                    ClaimSource::CodexReview,
                    &directory,
                )
                .unwrap();
        }
        sample(&store, "agent001", 40.0);
        assert!(queued(&store, "agent001")[0]
            .contains("sm will move your claims (PR #1660, PR #1661) and pending wakes"));
    }

    #[test]
    fn session_json_carries_context_percent_and_the_handoff_view() {
        let store = store("json");
        sample(&store, "agent001", 28.0);
        let session = store.get_session("agent001").unwrap().unwrap();
        let response = serde_json::to_value(SessionResponse::from(session)).unwrap();
        assert_eq!(response["context_percent"], json!(28.0));
        assert_eq!(
            response["handoff"],
            json!({
                "enabled": true, "threshold_percent": 35, "source": "default",
                "has_gauge": true, "display": "hands off at 35%", "state": null,
                "successor": null, "predecessor": null
            })
        );
    }
}
