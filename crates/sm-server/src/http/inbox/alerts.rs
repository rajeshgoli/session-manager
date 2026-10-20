//! sm's own alerts (sm#2414): the artifact and backup notices sm sends the
//! owner as `sm artifacts`. An alert needs the owner only when the owner
//! alone can act (a drive to reconnect or replace, deletions to confirm);
//! every alert an agent can work carries Assign to agent, which starts an
//! agent in the artifact root's repo briefed with the alert.

use super::*;
use crate::owner_settings::{AgentType, OwnerSettings};

/// The sender every sm alert carries.
pub(crate) const ALERT_SENDER: &str = "sm-artifacts";

/// Delivery-key prefixes of alerts no agent can act on: a drive to
/// reconnect or replace, and the all-clear after a backup recovers.
const NOT_AGENT_WORK: [&str; 3] = ["cold_missing:", "backup-failing:", "backup-recovered:"];

/// What an sm alert offers in its card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlertAction<'a> {
    /// Assign to agent.
    Assign,
    /// The agent the alert went to.
    Assigned(&'a str),
    /// Nothing: not an alert, owner-only, or its condition cleared.
    None,
}

/// The action an sm alert offers, from its delivery key and assignment.
pub(crate) fn alert_action<'a>(
    message: &OwnerMessage,
    key: Option<&str>,
    assigned_to: Option<&'a str>,
) -> AlertAction<'a> {
    if message.sender_session_id != ALERT_SENDER {
        return AlertAction::None;
    }
    if let Some(session_id) = assigned_to {
        return AlertAction::Assigned(session_id);
    }
    let cleared = matches!(
        message.handled_via.as_deref(),
        Some("resolved" | "superseded")
    );
    match key {
        Some(key) if !cleared && !NOT_AGENT_WORK.iter().any(|p| key.starts_with(p)) => {
            AlertAction::Assign
        }
        _ => AlertAction::None,
    }
}

/// The agent types the owner's settings offer, and the one preselected: the
/// type matching the New agent default, else the first.
pub(crate) fn agent_choices(state: &AppState) -> anyhow::Result<(Vec<AgentType>, Option<String>)> {
    let settings = OwnerSettings::from_effective(&state.session_store.owner_settings()?)?.new_agent;
    let defaults = settings.provider_defaults();
    let preferred = settings
        .agent_types
        .iter()
        .find(|t| {
            t.provider == settings.provider
                && Some(&t.model) == defaults.model.as_ref()
                && Some(&t.effort) == defaults.effort.as_ref()
        })
        .or(settings.agent_types.first())
        .map(|t| t.name.clone());
    Ok((settings.agent_types, preferred))
}

/// The brief the assigned agent starts with: the alert itself, then how to
/// work it inside the repo's rules and how to report back.
fn brief(owner: &str, message: &OwnerMessage) -> String {
    format!(
        "sm raised this alert for {owner} and {owner} assigned it to you.\n\n\
         ## {title}\n\n{body}\n\n---\n\n\
         Find out what is really happening before acting: `sm artifact disk`, \
         `sm artifact list` and `sm artifact show <name>` show sm's view, and each \
         artifact's own notes say what it is for and how long to keep it. Fix what \
         you can within this repo's rules. Do not delete an artifact {owner} has not \
         approved. When you are done, or when something needs a decision only \
         {owner} can make, send {owner} one message with `sm send {recipient}`: what \
         you found, what you did, and your recommendation.",
        title = message.title,
        body = message.body_markdown,
        recipient = owner.to_lowercase(),
    )
}

#[derive(Debug, Deserialize)]
struct AssignRequest {
    /// An agent type name from Settings › New agents; absent uses the
    /// New agent default.
    #[serde(default)]
    agent_type: Option<String>,
}

/// `POST /inbox/alerts/{message_id}/assign`: start an agent for an sm alert.
/// Returns `{session_id, name, thread_path}`.
pub(crate) async fn post_assign(
    State(state): State<Arc<AppState>>,
    Path(message_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let path = format!("/inbox/alerts/{message_id}/assign");
    ensure_inbox_write_allowed(&state, &headers, peer_addr, &path)?;
    let payload: AssignRequest = docs::parse_json_body(&body)?;
    let _guard = state.owner_message_lock.lock().await;
    let store = owner_message_store(&state);
    let message = store
        .get(&message_id)?
        .ok_or(ApiError::NotFound("Message not found"))?;
    let keys = store.delivery_keys()?;
    let assignments = store.assignments()?;
    match alert_action(
        &message,
        keys.get(&message.id).map(String::as_str),
        assignments.get(&message.id).map(String::as_str),
    ) {
        AlertAction::Assign => {}
        AlertAction::Assigned(session_id) => {
            return Err(conflict(format!(
                "Already with {}",
                state
                    .session_store
                    .get_session(session_id)?
                    .map(session_display_name)
                    .unwrap_or_else(|| session_id.to_owned())
            )))
        }
        AlertAction::None => return Err(conflict("This alert has nothing for an agent to do")),
    }
    let working_dir = state
        .managed_artifacts
        .as_ref()
        .and_then(|svc| {
            svc.config
                .roots
                .iter()
                .map(|root| expand_home(&root.repo_checkout))
                .find(|path| path.is_dir())
        })
        .ok_or_else(|| {
            conflict("No repo checkout is configured for artifacts; start the agent yourself.")
        })?;
    let (types, preferred) = agent_choices(&state)?;
    let chosen = payload.agent_type.or(preferred);
    let agent = match chosen {
        Some(name) => Some(
            types
                .into_iter()
                .find(|t| t.name == name)
                .ok_or_else(|| bad_request(format!("Unknown agent type {name}")))?,
        ),
        None => None,
    };
    let request = CreateCoreSessionRequest {
        id: None,
        name: None,
        working_dir: Some(working_dir.to_string_lossy().into_owned()),
        provider: agent.as_ref().map(|t| t.provider.clone()),
        parent_session_id: None,
        node: None,
        initial_message: Some(brief(&state.config.owner_name, &message)),
        model: agent.as_ref().map(|t| t.model.clone()),
        reasoning_effort: agent.as_ref().map(|t| t.effort.clone()),
        wait: None,
        max_wait_seconds: None,
        spawn_prompt_source: Some(SpawnBriefSource {
            kind: "positional".to_owned(),
            path: None,
        }),
        spawn_brief: None,
        started_by_sm: true,
    };
    let session = super::super::create_session_from_request(state.clone(), request).await?;
    store.assign(&message.id, &session.id)?;
    super::super::board::request_recompute(&state);
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "session_id": session.id,
            "name": session_display_name(session.clone()),
            "thread_path": agent_thread_path(&session.id),
        })),
    ))
}

/// Whether an alert under `prefix` is with an agent that is still working
/// it (its condition has not cleared), so sm does not send the same alert
/// again meanwhile.
pub(crate) fn with_live_agent(state: &AppState, prefix: &str) -> anyhow::Result<bool> {
    let store = owner_message_store(state);
    let assignments = store.assignments()?;
    for (message_id, key) in store.delivery_keys()? {
        if !key.starts_with(prefix) {
            continue;
        }
        let Some(session_id) = assignments.get(&message_id) else {
            continue;
        };
        let open = store
            .get(&message_id)?
            .is_some_and(|message| message.handled_via.as_deref() == Some("agent"));
        if open
            && state
                .session_store
                .get_session(session_id)?
                .is_some_and(|session| !session_ended(&session))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(handled_via: Option<&str>) -> OwnerMessage {
        OwnerMessage {
            id: "msg_00000001".into(),
            human: "rajesh".into(),
            sender_session_id: ALERT_SENDER.into(),
            sender_session_name: "sm artifacts".into(),
            title: "Artifacts need disk space".into(),
            body_markdown: "Free 10 GB.".into(),
            blocking: false,
            created_at: "2026-10-20T00:00:00Z".into(),
            first_viewed_at: None,
            handled_at: handled_via.map(|_| "2026-10-20T00:10:00Z".into()),
            handled_via: handled_via.map(Into::into),
        }
    }

    #[test]
    fn only_agent_work_offers_assign_until_it_clears_or_is_taken() {
        let open = alert(None);
        assert_eq!(
            alert_action(&open, Some("pressure_pending:7:1"), None),
            AlertAction::Assign
        );
        // Done in the Inbox is not a cleared condition: the owner may still assign it.
        assert_eq!(
            alert_action(&alert(Some("inbox")), Some("pressure_pending:7:1"), None),
            AlertAction::Assign
        );
        for via in ["resolved", "superseded"] {
            assert_eq!(
                alert_action(&alert(Some(via)), Some("pressure_pending:7:1"), None),
                AlertAction::None
            );
        }
        for key in [
            "cold_missing:/a:1",
            "backup-failing:mini:1:0",
            "backup-recovered:3",
        ] {
            assert_eq!(alert_action(&open, Some(key), None), AlertAction::None);
        }
        assert_eq!(
            alert_action(
                &alert(Some("agent")),
                Some("pressure_pending:7:1"),
                Some("abc")
            ),
            AlertAction::Assigned("abc")
        );
        let mut agent_message = open.clone();
        agent_message.sender_session_id = "abc12345".into();
        assert_eq!(
            alert_action(&agent_message, Some("k"), None),
            AlertAction::None
        );
        assert_eq!(alert_action(&open, None, None), AlertAction::None);
    }

    #[test]
    fn brief_carries_the_alert_and_the_report_back() {
        let text = brief("Rajesh", &alert(None));
        assert!(text.contains("## Artifacts need disk space\n\nFree 10 GB."));
        assert!(text.contains("`sm send rajesh`"));
        assert!(text.contains("Do not delete an artifact Rajesh has not approved"));
    }
}
