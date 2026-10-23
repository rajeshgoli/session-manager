//! Overnight mode over HTTP (sm#2494): `GET`/`PUT /client/overnight` for
//! the moon button, `PUT /client/overnight/dnd` for the phone's Do Not
//! Disturb, and the follow-pass step that tells waiting agents, holds
//! alerts, and sends one summary when a held night ends on its own.

use super::*;
use crate::overnight::{self, Away, OvernightStore, Settings};
use crate::owner_push::{format_ts, parse_ts, PushError};

pub(super) fn store(state: &AppState) -> OvernightStore {
    OvernightStore::new(expand_home(&state.config.sm_send.db_path))
}

/// What overnight mode is at `now`.
pub(super) struct Now {
    pub away: Option<Away>,
    pub stored: overnight::State,
    pub last_activity: Option<OffsetDateTime>,
}

pub(super) fn evaluate(state: &AppState, now: OffsetDateTime) -> anyhow::Result<Now> {
    let settings = Settings::from_effective(&state.session_store.owner_settings()?);
    let stored = store(state).load()?;
    let last_activity = super::owner_events::store(state)
        .last_at_ms()?
        .and_then(|ms| OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok());
    let away = overnight::evaluate(
        now,
        overnight::local_offset,
        &settings,
        &stored,
        last_activity,
    );
    Ok(Now {
        away,
        stored,
        last_activity,
    })
}

/// Whether the owner is away now; `None` (and a log line) when sm cannot tell.
pub(super) fn current_away(state: &AppState) -> Option<Away> {
    match evaluate(state, OffsetDateTime::now_utc()) {
        Ok(now) => now.away,
        Err(error) => {
            eprintln!("overnight state unreadable: {error:#}");
            None
        }
    }
}

/// The human `sm send` reaches for the owner: the one named `owner_name`,
/// else the only one configured.
pub(super) fn owner_human(config: &AppConfig) -> Option<crate::email::HumanRecipient> {
    let bridge = EmailBridge::load(config).ok()?;
    if let Ok(Some(human)) = bridge.lookup_human(&config.owner_name) {
        return Some(human);
    }
    let mut humans = bridge.list_humans();
    (humans.len() == 1).then(|| humans.remove(0))
}

/// The identity the owner's phone registered its push tokens under: whose
/// alerts overnight holds and summarizes.
pub(super) fn owner_notice_user(state: &AppState) -> String {
    let owner = owner_human(&state.config).map(|human| human.name);
    follows::notice_user_id(state, owner.as_deref())
}

/// Agents blocked on the owner (names), and the owner's unread messages
/// since `since`.
fn waiting_counts(state: &AppState, since: &str) -> anyhow::Result<(Vec<String>, usize)> {
    let Some(owner) = owner_human(&state.config) else {
        return Ok((Vec::new(), 0));
    };
    let rows = super::messages::owner_message_store(state).for_obligations(since)?;
    let mut blocked = BTreeSet::new();
    let mut unread = 0;
    for (message, replied) in rows
        .into_iter()
        .filter(|(message, _)| message.human == owner.name)
    {
        if message.blocked && !replied && message.handled_at.is_none() {
            blocked.insert(message.sender_session_name.clone());
        }
        if message.created_at.as_str() >= since && message.first_viewed_at.is_none() {
            unread += 1;
        }
    }
    Ok((blocked.into_iter().collect(), unread))
}

fn status_json(state: &AppState, now: &Now, at: OffsetDateTime) -> anyhow::Result<Value> {
    let offset = overnight::local_offset(at);
    let settings = &state.session_store.owner_settings()?["overnight"];
    let (away, waiting) = match &now.away {
        Some(away) => {
            let (blocked, unread) = waiting_counts(state, &away.since)?;
            (
                json!({
                    "source": away.source.as_str(),
                    "since": away.since,
                    "since_label": overnight::clock_label(&away.since, offset),
                    "until": away.until,
                    "until_label": away.until.as_deref().map(|until| overnight::clock_label(until, offset)),
                    "holds_alerts": away.source.holds_alerts(),
                }),
                json!({"blocked": blocked, "unread": unread}),
            )
        }
        None => (Value::Null, Value::Null),
    };
    Ok(json!({
        "on": now.away.is_some(),
        "away": away,
        "waiting": waiting,
        "manual": now.stored.manual_since.is_some(),
        "dnd": now.stored.dnd_since.is_some(),
        "settings": settings,
    }))
}

async fn respond(state: Arc<AppState>) -> Result<Json<Value>, ApiError> {
    let value = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let at = OffsetDateTime::now_utc();
        let now = evaluate(&state, at)?;
        status_json(&state, &now, at)
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    Ok(Json(value))
}

/// `GET /client/overnight`.
pub(super) async fn get_overnight(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    board::owner_guard(&state, &headers, peer_addr, "GET", &uri, false)?;
    respond(state).await
}

#[derive(Debug, Deserialize)]
pub(super) struct Switch {
    on: bool,
}

/// `PUT /client/overnight` `{on}`: the moon button.
pub(super) async fn put_overnight(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Switch>,
) -> Result<Json<Value>, ApiError> {
    board::owner_guard(&state, &headers, peer_addr, "PUT", &uri, true)?;
    let work = state.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let now = OffsetDateTime::now_utc();
        let settings = Settings::from_effective(&work.session_store.owner_settings()?);
        store(&work).update(|current| {
            overnight::set_manual(current, body.on, now, overnight::local_offset, &settings)
        })?;
        Ok(())
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    respond(state).await
}

/// `PUT /client/overnight/dnd` `{on}`: the phone's Do Not Disturb.
pub(super) async fn put_dnd(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Switch>,
) -> Result<Json<Value>, ApiError> {
    board::owner_guard(&state, &headers, peer_addr, "PUT", &uri, true)?;
    let work = state.clone();
    tokio::task::spawn_blocking(move || {
        store(&work)
            .update(|current| overnight::report_dnd(current, body.on, OffsetDateTime::now_utc()))
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    respond(state).await
}

/// `POST /client/overnight/summary/{id}/ack`: the phone showed the night's
/// summary, so the alerts it stands for are released without a push each.
pub(super) async fn ack_summary(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // The same guard as a notice acknowledgement: the phone's push service sends it.
    follows::owner_guard(&state, &headers, peer_addr, "POST", &uri)?;
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let at = OffsetDateTime::now_utc();
        let mut released = None;
        store(&state).update(|current| match &current.summary {
            Some(summary) if summary.id == id => {
                released = Some(summary.notices.clone());
                overnight::State {
                    summary: None,
                    ..current.clone()
                }
            }
            _ => current.clone(),
        })?;
        // Only what the summary described.
        if let Some(notices) = released {
            follows::push_store(&state).release_held(&notices, at)?;
        }
        Ok(())
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    Ok(StatusCode::NO_CONTENT)
}

impl AppState {
    /// The follow pass's overnight step. Returns whether alerts wait now.
    /// On a new night it tells each agent with an unanswered message that
    /// the owner is away. When a night that held alerts ends, the message
    /// alerts that waited give way to one summary push: released silently
    /// when the owner is already back, or when the phone acknowledges it
    /// showed the summary. Otherwise (no phone, a failed or unshown push,
    /// nothing to summarize, or the follows the owner asked for) they go out
    /// one by one with their usual retries and email fallback.
    pub(super) fn overnight_tick(&self, at: OffsetDateTime) -> anyhow::Result<bool> {
        let now = evaluate(self, at)?;
        let push = follows::push_store(self);
        let mut holds = now
            .away
            .as_ref()
            .is_some_and(|away| away.source.holds_alerts());
        if let Some(summary) = &now.stored.summary {
            let sent = parse_ts(&summary.sent_at).unwrap_or(at);
            let owner_back = now.last_activity.is_some_and(|last| last > sent);
            if owner_back {
                push.release_held(&summary.notices, at)?;
            }
            if owner_back || at - sent >= overnight::SUMMARY_ACK_WINDOW {
                store(self).update(|current| overnight::State {
                    summary: None,
                    ..current.clone()
                })?;
            } else {
                holds = true;
            }
        }
        let seen = now.stored.seen.clone();
        if seen == now.away {
            return Ok(holds);
        }
        if let (None, Some(away)) = (&seen, &now.away) {
            self.tell_waiting_agents(away, at);
        }
        let held_before = seen.as_ref().filter(|away| away.source.holds_alerts());
        let mut pending = None;
        if let (Some(night), false) = (held_before, holds) {
            let since = parse_ts(&night.since).unwrap_or(at);
            let owner_back = now.last_activity.is_some_and(|last| last > since);
            // Read before the summary is counted, so it covers at least these.
            let held = push.held_message_notices(&owner_notice_user(self), since)?;
            if owner_back {
                push.release_held(&held, at)?;
            } else if !held.is_empty() {
                let summary = overnight::PendingSummary {
                    id: format!("ovs-{}", since.unix_timestamp()),
                    since: night.since.clone(),
                    sent_at: format_ts(at),
                    notices: held,
                };
                // Stored first, so a phone that acknowledges at once finds it.
                let stored = summary.clone();
                store(self).update(|current| overnight::State {
                    summary: Some(stored),
                    ..current.clone()
                })?;
                if self.send_overnight_summary(&night.since, &summary.id) {
                    pending = Some(summary);
                    holds = true;
                } else {
                    store(self).update(|current| overnight::State {
                        summary: current.summary.clone().filter(|kept| kept.id != summary.id),
                        ..current.clone()
                    })?;
                }
            }
        }
        let next = now.away.clone();
        let acknowledged = pending.is_some();
        store(self).update(|current| overnight::State {
            seen: next,
            ..current.clone()
        })?;
        // Acknowledged while it was being sent: the alerts are already released.
        if acknowledged && store(self).load()?.summary.is_none() {
            return Ok(now
                .away
                .as_ref()
                .is_some_and(|away| away.source.holds_alerts()));
        }
        Ok(holds)
    }

    /// One `[sm autoreply]` per agent waiting on an answer (a blocking
    /// message from before the night began); agents that sent during it
    /// were told at once.
    fn tell_waiting_agents(&self, away: &Away, at: OffsetDateTime) {
        let Some(human) = owner_human(&self.config) else {
            return;
        };
        // Older open blocking messages come back too, whatever their age.
        let window = format_ts(at - time::Duration::hours(24));
        let rows = match super::messages::owner_message_store(self).for_obligations(&window) {
            Ok(rows) => rows,
            Err(error) => {
                eprintln!("overnight: unanswered messages unreadable: {error:#}");
                return;
            }
        };
        let senders: BTreeSet<String> = rows
            .into_iter()
            .filter(|(message, replied)| {
                !replied
                    && message.blocking
                    && message.human == human.name
                    && message.handled_at.is_none()
                    && message.created_at < away.since
            })
            .map(|(message, _)| message.sender_session_id)
            .collect();
        let text =
            overnight::late_autoreply(&self.config.owner_name, away, overnight::local_offset(at));
        let queue = RetainedQueueStore::new(expand_home(&self.config.sm_send.db_path));
        for sender in senders {
            let live = self
                .session_store
                .get_session(&sender)
                .ok()
                .flatten()
                .is_some_and(|session| !super::messages::session_ended(&session));
            if !live {
                continue;
            }
            let id = format!("overnight-{}-{sender}", away.since);
            match queue.enqueue_message_once_with_metadata(
                &id,
                &sender,
                &text,
                "sequential",
                QueueMessageMetadata::default(),
            ) {
                Ok(()) => super::messages::deliver_now(self, &sender, "overnight autoreply"),
                Err(error) => eprintln!("overnight autoreply to {sender} failed: {error:#}"),
            }
        }
    }

    /// The morning push: what waited, in one notification that opens the
    /// Inbox; the phone acknowledges `id` once it shows it. Returns whether
    /// at least one phone took it.
    fn send_overnight_summary(&self, since: &str, id: &str) -> bool {
        let Some(sender) = self.push_sender.as_deref() else {
            return false;
        };
        let (blocked, unread) = match waiting_counts(self, since) {
            Ok(counts) => counts,
            Err(error) => {
                eprintln!("overnight summary counts failed: {error:#}");
                return false;
            }
        };
        if blocked.is_empty() && unread == 0 {
            return false;
        }
        let (title, body) = overnight_summary_text(&blocked, unread);
        let data = BTreeMap::from([
            ("kind".to_owned(), "overnight_summary".to_owned()),
            ("title".to_owned(), title),
            ("body".to_owned(), body),
            ("reader_path".to_owned(), "/inbox".to_owned()),
            ("summary_id".to_owned(), id.to_owned()),
        ]);
        let store = follows::push_store(self);
        let user = owner_notice_user(self);
        let tokens = match store.valid_tokens(&user) {
            Ok(tokens) => tokens,
            Err(error) => {
                eprintln!("overnight summary tokens unreadable: {error:#}");
                return false;
            }
        };
        let mut delivered = false;
        for token in tokens {
            match sender.send(&token.token, &data) {
                Ok(()) => delivered = true,
                Err(PushError::InvalidToken(detail) | PushError::Retryable(detail)) => {
                    eprintln!(
                        "overnight summary to {} failed: {detail}",
                        token.device_name
                    );
                }
            }
        }
        delivered
    }
}

/// The summary's title and body: blocked agents first, by name.
pub(super) fn overnight_summary_text(blocked: &[String], unread: usize) -> (String, String) {
    let messages = match unread {
        0 => None,
        1 => Some("1 message".to_owned()),
        n => Some(format!("{n} messages")),
    };
    let title = match (blocked.len(), &messages) {
        (0, Some(messages)) => format!("While you were away: {messages}"),
        (1, _) => "1 agent is blocked on you".to_owned(),
        (n, _) => format!("{n} agents are blocked on you"),
    };
    let body = if blocked.is_empty() {
        "Nothing is blocked; agents kept working.".to_owned()
    } else {
        let names = blocked.join(", ");
        match messages {
            Some(messages) => format!("{names} · {messages} waiting"),
            None => names,
        }
    };
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::overnight_summary_text;

    #[test]
    fn the_summary_leads_with_blocked_agents() {
        let (title, body) = overnight_summary_text(&["sm-2494".to_owned()], 3);
        assert_eq!(title, "1 agent is blocked on you");
        assert_eq!(body, "sm-2494 · 3 messages waiting");
        let (title, body) = overnight_summary_text(&[], 1);
        assert_eq!(title, "While you were away: 1 message");
        assert_eq!(body, "Nothing is blocked; agents kept working.");
    }
}
