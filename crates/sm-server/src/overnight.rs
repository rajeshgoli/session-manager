//! Overnight mode (sm#2494): the owner is away or asleep, so agents should
//! work on their own. While it is on, `sm send <owner>` still files the
//! message in the app, but the sender is told at once that the owner is
//! away, and phone alerts wait. An agent that truly cannot continue runs
//! `sm blocked`, which files a "Blocked on you" message.
//!
//! Three sources turn it on, first match wins:
//! - the owner's moon button (`manual`), on until turned off;
//! - bedtime: inside the owner's bedtime window with no owner activity for
//!   `idle_minutes` (`bedtime`), off at the window's end or on activity;
//!   never without any recorded owner activity;
//! - the phone's Do Not Disturb, as the app reports it (`dnd`), with the
//!   same idle rule. The phone already silences alerts, so `dnd` holds none:
//!   the app keeps acknowledging pushes, which is how sm learns DND ended.
//!
//! Everything here but [`OvernightStore`] is pure.

use std::path::PathBuf;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Duration, OffsetDateTime, Time, UtcOffset};

use crate::owner_push::{format_ts, parse_ts};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Manual,
    Bedtime,
    Dnd,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Bedtime => "bedtime",
            Self::Dnd => "dnd",
        }
    }

    /// Whether phone alerts wait while this source holds overnight on.
    pub fn holds_alerts(self) -> bool {
        !matches!(self, Self::Dnd)
    }
}

/// Overnight is on: why, since when, and (bedtime) until when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Away {
    pub source: Source,
    pub since: String,
    pub until: Option<String>,
}

/// The owner's `overnight` settings, from the effective settings object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub follow_dnd: bool,
    pub bedtime_enabled: bool,
    pub bedtime_start: Time,
    pub bedtime_end: Time,
    pub idle_minutes: i64,
}

impl Settings {
    pub fn from_effective(settings: &Value) -> Self {
        let overnight = &settings["overnight"];
        let clock = |key: &str, fallback: Time| {
            overnight[key]
                .as_str()
                .and_then(parse_clock)
                .unwrap_or(fallback)
        };
        Self {
            follow_dnd: overnight["follow_dnd"].as_bool().unwrap_or(true),
            bedtime_enabled: overnight["bedtime_enabled"].as_bool().unwrap_or(true),
            bedtime_start: clock("bedtime_start", Time::from_hms(23, 0, 0).expect("time")),
            bedtime_end: clock("bedtime_end", Time::from_hms(7, 0, 0).expect("time")),
            idle_minutes: overnight["idle_minutes"].as_i64().unwrap_or(20),
        }
    }
}

/// `HH:MM`, 24-hour.
pub fn parse_clock(value: &str) -> Option<Time> {
    let (hours, minutes) = value.trim().split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    Time::from_hms(hours.parse().ok()?, minutes.parse().ok()?, 0).ok()
}

/// How long a Do Not Disturb report counts without a newer one.
pub const DND_REPORT_TTL: Duration = Duration::hours(12);

/// What sm remembers between evaluations. Stored as one JSON row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The moon button is on, since then.
    #[serde(default)]
    pub manual_since: Option<String>,
    /// The phone reported Do Not Disturb on, since then; `None` is off.
    #[serde(default)]
    pub dnd_since: Option<String>,
    #[serde(default)]
    pub dnd_reported_at: Option<String>,
    /// The owner turned overnight off during a bedtime episode: bedtime
    /// stays off until this time.
    #[serde(default)]
    pub bedtime_dismissed_until: Option<String>,
    /// The owner turned overnight off during this DND episode (its
    /// `dnd_since`): DND counts again once it turns off and on.
    #[serde(default)]
    pub dnd_dismissed: Option<String>,
    /// What the follow pass last acted on, to find transitions.
    #[serde(default)]
    pub seen: Option<Away>,
    /// The night's summary push, until the phone says it showed it.
    #[serde(default)]
    pub summary: Option<PendingSummary>,
}

/// A summary push out for acknowledgement. The night's held alerts stay
/// held until the phone shows it; with no acknowledgement within
/// [`SUMMARY_ACK_WINDOW`] they go out one by one after all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSummary {
    pub id: String,
    /// The night's start: what it held was created since then.
    pub since: String,
    pub sent_at: String,
}

pub const SUMMARY_ACK_WINDOW: Duration = Duration::minutes(15);

/// The bedtime window containing `now` in local time, as UTC instants.
fn bedtime_window(
    now: OffsetDateTime,
    offset: UtcOffset,
    start: Time,
    end: Time,
) -> Option<(OffsetDateTime, OffsetDateTime)> {
    if start == end {
        return None;
    }
    let local = now.to_offset(offset);
    let today = local.date();
    // Windows that could contain now: the one starting yesterday and today.
    for start_date in [today.previous_day()?, today] {
        let window_start = start_date.with_time(start).assume_offset(offset);
        let end_date = if end > start {
            start_date
        } else {
            start_date.next_day()?
        };
        let window_end = end_date.with_time(end).assume_offset(offset);
        if window_start <= now && now < window_end {
            return Some((window_start, window_end));
        }
    }
    None
}

/// Whether overnight is on at `now`, and why. `last_activity` is the
/// owner's latest action in any sm surface.
pub fn evaluate(
    now: OffsetDateTime,
    offset: UtcOffset,
    settings: &Settings,
    state: &State,
    last_activity: Option<OffsetDateTime>,
) -> Option<Away> {
    if let Some(since) = &state.manual_since {
        return Some(Away {
            source: Source::Manual,
            since: since.clone(),
            until: None,
        });
    }
    let idle_from = last_activity.map(|at| at + Duration::minutes(settings.idle_minutes));
    if idle_from.is_some_and(|from| now < from) {
        return None;
    }
    let after_idle = |at: OffsetDateTime| idle_from.map_or(at, |from| at.max(from));
    // Bedtime is a guess from the owner's habits; with no recorded
    // activity at all there is nothing to guess from.
    if settings.bedtime_enabled && last_activity.is_some() {
        let dismissed = state
            .bedtime_dismissed_until
            .as_deref()
            .and_then(parse_ts)
            .is_some_and(|until| now < until);
        if let Some((start, end)) =
            bedtime_window(now, offset, settings.bedtime_start, settings.bedtime_end)
                .filter(|_| !dismissed)
        {
            return Some(Away {
                source: Source::Bedtime,
                since: format_ts(after_idle(start)),
                until: Some(format_ts(end)),
            });
        }
    }
    // A report the phone has not renewed for half a day is stale: the app's
    // process may have died before Do Not Disturb turned off.
    let dnd_fresh = state
        .dnd_reported_at
        .as_deref()
        .and_then(parse_ts)
        .is_some_and(|at| now - at < DND_REPORT_TTL);
    if settings.follow_dnd && dnd_fresh {
        if let Some(dnd_since) = state
            .dnd_since
            .as_deref()
            .filter(|since| state.dnd_dismissed.as_deref() != Some(*since))
            .and_then(parse_ts)
        {
            return Some(Away {
                source: Source::Dnd,
                since: format_ts(after_idle(dnd_since)),
                until: None,
            });
        }
    }
    None
}

/// The state after the owner presses the moon button. Turning it off
/// during an automatic episode also dismisses that episode, so it does not
/// come straight back.
pub fn set_manual(
    state: &State,
    on: bool,
    now: OffsetDateTime,
    offset: UtcOffset,
    settings: &Settings,
) -> State {
    let mut next = state.clone();
    if on {
        next.manual_since = Some(state.manual_since.clone().unwrap_or_else(|| format_ts(now)));
        return next;
    }
    next.manual_since = None;
    if let Some((_, end)) =
        bedtime_window(now, offset, settings.bedtime_start, settings.bedtime_end)
    {
        next.bedtime_dismissed_until = Some(format_ts(end));
    }
    next.dnd_dismissed = state.dnd_since.clone();
    next
}

/// The state after the phone reports its Do Not Disturb.
pub fn report_dnd(state: &State, on: bool, now: OffsetDateTime) -> State {
    let mut next = state.clone();
    next.dnd_reported_at = Some(format_ts(now));
    match (on, &state.dnd_since) {
        (true, None) => next.dnd_since = Some(format_ts(now)),
        (false, _) => {
            next.dnd_since = None;
            next.dnd_dismissed = None;
        }
        (true, Some(_)) => {}
    }
    next
}

/// "11:42 PM" in the server's local time.
pub fn clock_label(at: &str, offset: UtcOffset) -> String {
    let Some(at) = parse_ts(at) else {
        return String::new();
    };
    let local = at.to_offset(offset);
    let (hour, minute) = (local.hour(), local.minute());
    let suffix = if hour < 12 { "AM" } else { "PM" };
    let hour = match hour % 12 {
        0 => 12,
        other => other,
    };
    format!("{hour}:{minute:02} {suffix}")
}

/// Why the owner is away, as the agent is told it.
fn away_reason(owner: &str, away: &Away, offset: UtcOffset) -> String {
    let since = clock_label(&away.since, offset);
    match away.source {
        Source::Manual => format!("{owner} is away or asleep (overnight mode since {since})."),
        Source::Bedtime => {
            format!("{owner} is probably asleep (past bedtime, away since {since}).")
        }
        Source::Dnd => format!("{owner}'s phone is on Do Not Disturb (since {since})."),
    }
}

const BLOCKED_HOWTO: &str = "Work autonomously if a solution is possible. If you are truly blocked (you need a login, a physical device, or a decision only the owner can make), run `sm blocked 'why you are blocked'` and wait for the reply.";

/// What `sm send` prints after a message to an away owner.
pub fn send_autoreply(owner: &str, away: &Away, offset: UtcOffset) -> String {
    let alerts = if away.source.holds_alerts() {
        format!("Your message is saved in the sm app, but {owner} will not be alerted now.")
    } else {
        format!("Your message is saved in the sm app; {owner} will see it later.")
    };
    format!(
        "[sm autoreply] {} {alerts} {}",
        away_reason(owner, away, offset),
        BLOCKED_HOWTO.replace("the owner", owner)
    )
}

/// What an agent with an unanswered message hears when overnight turns on
/// after it sent.
pub fn late_autoreply(owner: &str, away: &Away, offset: UtcOffset) -> String {
    format!(
        "[sm autoreply] {} Your earlier message waits in the sm app. {}",
        away_reason(owner, away, offset),
        BLOCKED_HOWTO.replace("the owner", owner)
    )
}

/// What `sm blocked` prints while the owner is away.
pub fn blocked_autoreply(owner: &str, away: &Away, offset: UtcOffset) -> String {
    format!(
        "[sm autoreply] {} {owner} sees \"Blocked on you\" in the sm app when back. Wait for the reply; do not resend.",
        away_reason(owner, away, offset)
    )
}

const STATE_KEY: &str = "state";

/// The overnight row, in the retained queue DB beside owner messages.
#[derive(Debug, Clone)]
pub struct OvernightStore {
    db_path: PathBuf,
}

impl OvernightStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.db_path)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS owner_overnight (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        )?;
        Ok(conn)
    }

    pub fn load(&self) -> Result<State> {
        if !self.db_path.exists() {
            return Ok(State::default());
        }
        let value: Option<String> = self
            .open()?
            .query_row(
                "SELECT value FROM owner_overnight WHERE key = ?1",
                [STATE_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default())
    }

    /// Applies `change` to the stored state in one write transaction.
    pub fn update(&self, change: impl FnOnce(&State) -> State) -> Result<State> {
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: State = tx
            .query_row(
                "SELECT value FROM owner_overnight WHERE key = ?1",
                [STATE_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        let next = change(&current);
        if next != current {
            tx.execute(
                "INSERT INTO owner_overnight (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![STATE_KEY, serde_json::to_string(&next)?],
            )?;
        }
        tx.commit()?;
        Ok(next)
    }
}

/// The server's local offset at `at`; UTC when it cannot be read.
pub fn local_offset(at: OffsetDateTime) -> UtcOffset {
    UtcOffset::local_offset_at(at).unwrap_or(UtcOffset::UTC)
}

#[cfg(test)]
mod tests;
