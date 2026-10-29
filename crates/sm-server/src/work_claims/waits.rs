//! Agent waits (#2477, `docs/specs/2477_agent_waits.html`): a seat records
//! with `sm await` that it is blocked on another seat or a ticket for a
//! named deliverable until an ETA. An open wait is an entry in the seat's
//! `waiting_on`, so Check C leaves the seat alone. A wait ends when the
//! awaited seat messages the waiter (`delivered`), the awaited ticket closes,
//! the awaited seat retires, the ETA passes, the waiter cancels, or the
//! waiter retires; the three sync-time endings message the waiter.
//!
//! A wait may also name a queue job (`queue:<job id>`), which ends it when the
//! job ends, or work outside sm (`<kind>:<id>`, e.g. `modal:ap-12`), which
//! only the ETA, a cancel or the waiter's retire ends (sm#2611).

use super::checks::{format_time, parse_time};
use super::*;

/// The furthest ETA a wait may carry.
pub const MAX_WAIT: Duration = Duration::from_secs(72 * 60 * 60);
/// The longest deliverable text, in characters.
pub const MAX_DELIVERABLE_CHARS: usize = 200;

pub fn init_schema(conn: &Connection) -> Result<()> {
    // Waits from before sm#2611 allowed only seats and tickets; SQLite cannot
    // widen a CHECK in place, so the table is rebuilt once.
    let old: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'agent_waits'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let migrate = old.is_some_and(|sql| !sql.contains("target_ref"));
    if migrate {
        conn.execute_batch(
            "DROP INDEX IF EXISTS agent_waits_open;
             ALTER TABLE agent_waits RENAME TO agent_waits_v1;",
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_waits (
            id TEXT PRIMARY KEY,
            waiter_session_id TEXT NOT NULL,
            target_kind TEXT NOT NULL CHECK (target_kind IN ('seat','ticket','job','external')),
            target_session_id TEXT,
            repo TEXT,
            number INTEGER,
            target_ref TEXT,                    -- job id, or the external reference
            deliverable TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            until_at TEXT NOT NULL,
            ended_at TEXT,
            end_reason TEXT CHECK (end_reason IN
                ('delivered','ticket_closed','target_retired','job_ended','expired',
                 'cancelled','waiter_retired')),
            end_message_id TEXT);
        CREATE UNIQUE INDEX IF NOT EXISTS agent_waits_open ON agent_waits
            (waiter_session_id, target_kind, IFNULL(target_session_id,''), IFNULL(repo,''),
             IFNULL(number,0), IFNULL(target_ref,''))
            WHERE ended_at IS NULL;",
    )?;
    if migrate {
        conn.execute_batch(
            "INSERT INTO agent_waits (id, waiter_session_id, target_kind, target_session_id,
                repo, number, deliverable, created_at, updated_at, until_at, ended_at,
                end_reason, end_message_id)
             SELECT id, waiter_session_id, target_kind, target_session_id, repo, number,
                deliverable, created_at, updated_at, until_at, ended_at, end_reason,
                end_message_id
               FROM agent_waits_v1;
             DROP TABLE agent_waits_v1;",
        )?;
    }
    Ok(())
}

/// What a wait is on: a seat by session id, a ticket, a queue job by id, or
/// work outside sm by the reference the waiter gave (`modal:ap-12`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitTarget {
    Seat(String),
    Ticket { repo: String, number: i64 },
    Job(String),
    External(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentWait {
    pub id: String,
    pub waiter_session_id: String,
    pub target_kind: String,
    pub target_session_id: Option<String>,
    pub repo: Option<String>,
    pub number: Option<i64>,
    pub target_ref: Option<String>,
    pub deliverable: String,
    pub created_at: String,
    pub updated_at: String,
    pub until_at: String,
}

impl AgentWait {
    pub fn target(&self) -> WaitTarget {
        let reference = || self.target_ref.clone().unwrap_or_default();
        match self.target_kind.as_str() {
            "job" => return WaitTarget::Job(reference()),
            "external" => return WaitTarget::External(reference()),
            _ => {}
        }
        match (&self.target_session_id, &self.repo, self.number) {
            (_, Some(repo), Some(number)) if self.target_kind == "ticket" => WaitTarget::Ticket {
                repo: repo.clone(),
                number,
            },
            (seat, _, _) => WaitTarget::Seat(seat.clone().unwrap_or_default()),
        }
    }

    fn matches(&self, target: &WaitTarget) -> bool {
        self.target() == *target
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEnd {
    Delivered,
    TicketClosed,
    TargetRetired,
    JobEnded,
    Expired,
    Cancelled,
    WaiterRetired,
}

impl WaitEnd {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::TicketClosed => "ticket_closed",
            Self::TargetRetired => "target_retired",
            Self::JobEnded => "job_ended",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::WaiterRetired => "waiter_retired",
        }
    }
}

/// The trimmed deliverable, or why it is refused.
pub fn validate_deliverable(text: &str) -> std::result::Result<String, String> {
    let text = text.trim();
    let chars = text.chars().count();
    if chars == 0 || chars > MAX_DELIVERABLE_CHARS || text.contains(['\n', '\r']) {
        return Err(format!(
            "for must be one line of 1 to {MAX_DELIVERABLE_CHARS} characters"
        ));
    }
    Ok(text.to_owned())
}

/// The ETA, if it is RFC 3339, later than `now` and within `MAX_WAIT`.
pub fn validate_until(
    until: &str,
    now: OffsetDateTime,
) -> std::result::Result<OffsetDateTime, String> {
    let refused = || "until must be in the future and within 72h".to_owned();
    let until = OffsetDateTime::parse(until.trim(), &Rfc3339).map_err(|_| refused())?;
    if until <= now || until - now > MAX_WAIT {
        return Err(refused());
    }
    Ok(until)
}

/// `Thu 22 Oct 05:30 UTC`.
pub fn eta_text(until: OffsetDateTime) -> String {
    let format = time::macros::format_description!(
        "[weekday repr:short] [day] [month repr:short] [hour]:[minute]"
    );
    until
        .to_offset(time::UtcOffset::UTC)
        .format(&format)
        .map(|text| format!("{text} UTC"))
        .unwrap_or_else(|_| format_time(until))
}

/// The wait's ETA as `eta_text` writes it.
pub fn until_text(wait: &AgentWait) -> String {
    parse_time(&wait.until_at).map_or_else(|| wait.until_at.clone(), eta_text)
}

/// `far-2171 (4c1d2e3f)`, or the bare short id when the seat has no name.
pub fn seat_label(sessions: &SessionDirectory, session_id: &str) -> String {
    let short: String = session_id.chars().take(8).collect();
    match sessions
        .get(session_id)
        .map(|session| session.name.as_str())
    {
        Some(name) if !name.is_empty() && name != session_id => format!("{name} ({short})"),
        _ => short,
    }
}

/// How the waiter's own messages name the target: the seat's name, or `#N`.
fn target_name(sessions: &SessionDirectory, wait: &AgentWait) -> String {
    match wait.target() {
        WaitTarget::Seat(id) => sessions
            .get(&id)
            .map(|session| session.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.chars().take(8).collect()),
        WaitTarget::Ticket { number, .. } => format!("#{number}"),
        WaitTarget::Job(id) | WaitTarget::External(id) => id,
    }
}

/// A queue job some open wait names, once it is no longer pending or
/// running (or the queue has no such job): `run_wait_endings` ends its waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedJob {
    /// `2611-check (job_071682aa6be6) succeeded`, or `job_x was not found`.
    pub text: String,
    /// The session the queue already woke with the job's result; its wait
    /// ends without a second message.
    pub woken: Option<String>,
}

impl WorkClaimStore {
    /// Opens a wait, or updates the open one on the same target (its
    /// deliverable and ETA; `created_at` stays). Returns it and whether it is
    /// new.
    pub fn open_wait(
        &self,
        waiter: &str,
        target: &WaitTarget,
        deliverable: &str,
        until: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<(AgentWait, bool)> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stamp = format_time(now);
        let until = format_time(until);
        let existing = query_waits(
            &tx,
            "WHERE waiter_session_id = ?1 AND ended_at IS NULL",
            params![waiter],
        )?
        .into_iter()
        .find(|wait| wait.matches(target));
        let (id, created) = match existing {
            Some(wait) => {
                tx.execute(
                    "UPDATE agent_waits SET deliverable = ?2, until_at = ?3, updated_at = ?4
                      WHERE id = ?1",
                    params![wait.id, deliverable, until, stamp],
                )?;
                (wait.id, false)
            }
            None => {
                let id = random_wait_id();
                let (kind, seat, repo, number, reference) = match target {
                    WaitTarget::Seat(seat) => ("seat", Some(seat.as_str()), None, None, None),
                    WaitTarget::Ticket { repo, number } => {
                        insert_stub(&tx, repo, *number, WorkKind::Ticket)?;
                        ("ticket", None, Some(repo.as_str()), Some(*number), None)
                    }
                    WaitTarget::Job(id) => ("job", None, None, None, Some(id.as_str())),
                    WaitTarget::External(reference) => {
                        ("external", None, None, None, Some(reference.as_str()))
                    }
                };
                tx.execute(
                    "INSERT INTO agent_waits (id, waiter_session_id, target_kind,
                        target_session_id, repo, number, target_ref, deliverable, created_at,
                        updated_at, until_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
                    params![
                        id,
                        waiter,
                        kind,
                        seat,
                        repo,
                        number,
                        reference,
                        deliverable,
                        stamp,
                        until
                    ],
                )?;
                (id, true)
            }
        };
        let wait = query_waits(&tx, "WHERE id = ?1", params![id])?
            .pop()
            .context("wait vanished after write")?;
        let kind = if created {
            "wait.opened"
        } else {
            "wait.updated"
        };
        wait_event(&tx, kind, &wait, None, None, &stamp)?;
        tx.commit()?;
        Ok((wait, created))
    }

    /// Every open wait, oldest first.
    pub fn open_waits(&self) -> Result<Vec<AgentWait>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        if !table_exists(&conn, "agent_waits")? {
            return Ok(Vec::new());
        }
        query_waits(&conn, "WHERE ended_at IS NULL ORDER BY created_at, id", [])
    }

    /// Every wait open at some instant after `from`, oldest first, with
    /// when it ended (`None` while open): the board's ticket clock (#2611).
    pub fn waits_since(&self, from: OffsetDateTime) -> Result<Vec<(AgentWait, Option<String>)>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        if !table_exists(&conn, "agent_waits")? {
            return Ok(Vec::new());
        }
        let filter = "WHERE ended_at IS NULL OR ended_at > ?1 ORDER BY created_at, id";
        let from = format_time(from);
        let ended: BTreeMap<String, Option<String>> = conn
            .prepare(&format!("SELECT id, ended_at FROM agent_waits {filter}"))?
            .query_map(params![from], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(query_waits(&conn, filter, params![from])?
            .into_iter()
            .map(|wait| {
                let at = ended.get(&wait.id).cloned().flatten();
                (wait, at)
            })
            .collect())
    }

    /// Ends the waiter's open wait on `target`, or all of them. Returns how
    /// many ended.
    pub fn cancel_waits(
        &self,
        waiter: &str,
        target: Option<&WaitTarget>,
        now: OffsetDateTime,
    ) -> Result<usize> {
        self.end_waiter_waits(waiter, target, WaitEnd::Cancelled, now)
    }

    fn end_waiter_waits(
        &self,
        waiter: &str,
        target: Option<&WaitTarget>,
        reason: WaitEnd,
        now: OffsetDateTime,
    ) -> Result<usize> {
        let Some(mut conn) = self.open_existing()? else {
            return Ok(0);
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stamp = format_time(now);
        let waits: Vec<AgentWait> = query_waits(
            &tx,
            "WHERE waiter_session_id = ?1 AND ended_at IS NULL",
            params![waiter],
        )?
        .into_iter()
        .filter(|wait| target.is_none_or(|target| wait.matches(target)))
        .collect();
        for wait in &waits {
            end_wait(&tx, wait, reason, None, &stamp)?;
        }
        tx.commit()?;
        Ok(waits.len())
    }

    /// An `sm send` from `sender` reached `recipient`: ends the recipient's
    /// waits on the sender, and on tickets the sender holds. Returns how many
    /// ended.
    pub fn deliver_waits(
        &self,
        sender: &str,
        recipient: &str,
        now: OffsetDateTime,
    ) -> Result<usize> {
        if sender == recipient {
            return Ok(0);
        }
        let Some(mut conn) = self.open_existing()? else {
            return Ok(0);
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stamp = format_time(now);
        let mut ended = 0;
        for wait in query_waits(
            &tx,
            "WHERE waiter_session_id = ?1 AND ended_at IS NULL",
            params![recipient],
        )? {
            let from_target = match wait.target() {
                WaitTarget::Seat(seat) => seat == sender,
                WaitTarget::Ticket { repo, number } => tx
                    .query_row(
                        "SELECT 1 FROM work_claims WHERE repo = ?1 AND number = ?2
                            AND session_id = ?3 AND ended_at IS NULL AND reserved_at IS NULL",
                        params![repo, number, sender],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some(),
                WaitTarget::Job(_) | WaitTarget::External(_) => false,
            };
            if from_target {
                end_wait(&tx, &wait, WaitEnd::Delivered, None, &stamp)?;
                ended += 1;
            }
        }
        tx.commit()?;
        Ok(ended)
    }

    /// After a sync pass, before Check C: ends waits whose waiter retired,
    /// whose ticket GitHub reported closed or missing in this pass
    /// (`answered`, as for Check A) or whose cached state is already closed,
    /// whose seat retired, whose queue job ended (`jobs`), or whose ETA
    /// passed, in that order of precedence; all but the first message the
    /// waiter, except a job's end to the session the queue already woke.
    /// Returns the sessions sent a message.
    pub fn run_wait_endings(
        &self,
        answered: &BTreeSet<(String, i64)>,
        jobs: &BTreeMap<String, EndedJob>,
        sessions: &SessionDirectory,
        now: OffsetDateTime,
    ) -> Result<Vec<String>> {
        let Some(mut conn) = self.open_existing()? else {
            return Ok(Vec::new());
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stamp = format_time(now);
        let retired = |id: &str| {
            sessions
                .get(id)
                .is_none_or(|session| session.state == HolderState::Retired)
        };
        let mut notified = Vec::new();
        for wait in query_waits(&tx, "WHERE ended_at IS NULL ORDER BY created_at, id", [])? {
            if retired(&wait.waiter_session_id) {
                end_wait(&tx, &wait, WaitEnd::WaiterRetired, None, &stamp)?;
                continue;
            }
            let deliverable = &wait.deliverable;
            // A ticket counts when GitHub answered for it in this pass, or when
            // its cached state is final: the sync never re-polls a closed item.
            let ticket_settled = match wait.target() {
                WaitTarget::Ticket { repo, number } => {
                    answered.contains(&(repo.clone(), number))
                        || get_item(&tx, &repo, number)?.is_some_and(|item| {
                            item.synced_at.is_some()
                                && item.sync_error.is_none()
                                && item.state != "open"
                        })
                }
                _ => false,
            };
            let mut ending = match wait.target() {
                WaitTarget::Ticket { repo, number } if ticket_settled => {
                    closed_text(&tx, &repo, number, deliverable)?
                        .map(|text| (WaitEnd::TicketClosed, text))
                }
                WaitTarget::Seat(seat) if retired(&seat) => Some((
                    WaitEnd::TargetRetired,
                    format!(
                        "[sm await] {} retired without messaging you. Your wait on it for \"{deliverable}\" ended.",
                        seat_label(sessions, &seat)
                    ),
                )),
                WaitTarget::Job(id) => jobs.get(&id).map(|job| {
                    let text = (job.woken.as_deref() != Some(&wait.waiter_session_id)).then(|| {
                        format!(
                            "[sm await] Queue job {}. Your wait on it for \"{deliverable}\" ended.",
                            job.text
                        )
                    });
                    (WaitEnd::JobEnded, text.unwrap_or_default())
                }),
                _ => None,
            };
            if ending.is_none() && parse_time(&wait.until_at).is_some_and(|until| until <= now) {
                ending = Some((
                    WaitEnd::Expired,
                    format!(
                        "[sm await] Your wait on {} for \"{deliverable}\" passed its ETA, {}, and ended.",
                        target_name(sessions, &wait),
                        until_text(&wait)
                    ),
                ));
            }
            let Some((reason, text)) = ending else {
                continue;
            };
            if text.is_empty() {
                end_wait(&tx, &wait, reason, None, &stamp)?;
                continue;
            }
            let message_id = crate::queue::enqueue_important_in_conn(
                &tx,
                &wait.waiter_session_id,
                &text,
                MESSAGE_CATEGORY,
            )?;
            end_wait(&tx, &wait, reason, Some(&message_id), &stamp)?;
            if !notified.contains(&wait.waiter_session_id) {
                notified.push(wait.waiter_session_id.clone());
            }
        }
        tx.commit()?;
        Ok(notified)
    }

    /// At `sm task-complete`: tells the session who still waits on it,
    /// directly or through a ticket it claims. The waits stay open. Returns
    /// the sessions sent a message.
    pub fn notify_awaited_at_task_complete(
        &self,
        session_id: &str,
        sessions: &SessionDirectory,
    ) -> Result<Vec<String>> {
        let Some(conn) = self.open_existing()? else {
            return Ok(Vec::new());
        };
        let held: BTreeSet<(String, i64)> = query_claims(
            &conn,
            "WHERE session_id = ?1 AND ended_at IS NULL AND reserved_at IS NULL",
            params![session_id],
        )?
        .into_iter()
        .map(|claim| (claim.repo, claim.number))
        .collect();
        let lines: Vec<String> =
            query_waits(&conn, "WHERE ended_at IS NULL ORDER BY created_at, id", [])?
                .into_iter()
                .filter(|wait| {
                    wait.waiter_session_id != session_id
                        && sessions
                            .get(&wait.waiter_session_id)
                            .is_some_and(|session| session.state != HolderState::Retired)
                        && match wait.target() {
                            WaitTarget::Seat(seat) => seat == session_id,
                            WaitTarget::Ticket { repo, number } => held.contains(&(repo, number)),
                            WaitTarget::Job(_) | WaitTarget::External(_) => false,
                        }
                })
                .map(|wait| {
                    format!(
                        "{} waits on you for \"{}\", ETA {}",
                        seat_label(sessions, &wait.waiter_session_id),
                        wait.deliverable,
                        until_text(&wait)
                    )
                })
                .collect();
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let text = format!("[sm await] At task-complete, {}.", lines.join("; "));
        crate::queue::enqueue_important_in_conn(&conn, session_id, &text, MESSAGE_CATEGORY)?;
        Ok(vec![session_id.to_owned()])
    }
}

/// The waiter's message when this pass found the awaited ticket closed or
/// missing on GitHub.
fn closed_text(
    conn: &Connection,
    repo: &str,
    number: i64,
    deliverable: &str,
) -> Result<Option<String>> {
    let Some(item) = get_item(conn, repo, number)? else {
        return Ok(None);
    };
    Ok(match item.sync_error.as_deref() {
        Some("not found") => Some(format!(
            "[sm await] #{number} was not found on GitHub. Your wait on it for \"{deliverable}\" ended."
        )),
        None if item.state != "open" => {
            let titled = if item.title.is_empty() {
                format!("#{number}")
            } else {
                format!("#{number} \"{}\"", item.title)
            };
            Some(format!(
                "[sm await] {titled} closed. Your wait on it for \"{deliverable}\" ended."
            ))
        }
        _ => None,
    })
}

/// Retire or kill: the session's own waits end silently.
pub(super) fn end_waits_of_waiter(conn: &Connection, waiter: &str, now: &str) -> Result<()> {
    for wait in query_waits(
        conn,
        "WHERE waiter_session_id = ?1 AND ended_at IS NULL",
        params![waiter],
    )? {
        end_wait(conn, &wait, WaitEnd::WaiterRetired, None, now)?;
    }
    Ok(())
}

/// Restore, or the handoff that follows an Auto PM retire: reopens the
/// waiter's waits that its retire ended, those ended `waiter_retired` at or
/// after `retired_at`, unless the same wait is open again. A reopened wait
/// past its ETA ends as `expired` at the next sync pass.
pub(super) fn reopen_retired_waits(
    conn: &Connection,
    waiter: &str,
    retired_at: OffsetDateTime,
    now: &str,
) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT id, ended_at FROM agent_waits
          WHERE waiter_session_id = ?1 AND end_reason = 'waiter_retired'",
    )?;
    let ended: Vec<(String, String)> = statement
        .query_map(params![waiter], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, ended_at) in ended {
        if parse_time(&ended_at).is_none_or(|ended| ended < retired_at) {
            continue;
        }
        let reopened = conn.execute(
            "UPDATE OR IGNORE agent_waits
                SET ended_at = NULL, end_reason = NULL, end_message_id = NULL
              WHERE id = ?1",
            params![id],
        )?;
        if reopened == 1 {
            if let Some(wait) = query_waits(conn, "WHERE id = ?1", params![id])?.pop() {
                wait_event(conn, "wait.reopened", &wait, None, None, now)?;
            }
        }
    }
    Ok(())
}

fn end_wait(
    conn: &Connection,
    wait: &AgentWait,
    reason: WaitEnd,
    message_id: Option<&str>,
    now: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE agent_waits SET ended_at = ?2, end_reason = ?3, end_message_id = ?4 WHERE id = ?1",
        params![wait.id, now, reason.as_str(), message_id],
    )?;
    wait_event(conn, "wait.ended", wait, Some(reason), message_id, now)
}

fn wait_event(
    conn: &Connection,
    kind: &str,
    wait: &AgentWait,
    reason: Option<WaitEnd>,
    message_id: Option<&str>,
    now: &str,
) -> Result<()> {
    let mut payload = json!({
        "wait_id": wait.id, "target_kind": wait.target_kind,
        "target_session_id": wait.target_session_id, "repo": wait.repo, "number": wait.number,
        "target_ref": wait.target_ref,
        "for": wait.deliverable, "until": wait.until_at,
    });
    if let Some(reason) = reason {
        payload["reason"] = json!(reason.as_str());
        payload["message_id"] = json!(message_id);
    }
    write_event(
        conn,
        kind,
        Some(&wait.waiter_session_id),
        wait.repo.as_deref(),
        wait.number,
        None,
        payload,
        now,
    )
}

fn query_waits(
    conn: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<AgentWait>> {
    let mut statement = conn.prepare(&format!(
        "SELECT id, waiter_session_id, target_kind, target_session_id, repo, number,
                deliverable, created_at, updated_at, until_at, target_ref
           FROM agent_waits {filter}"
    ))?;
    let rows = statement
        .query_map(params, |row| {
            Ok(AgentWait {
                id: row.get(0)?,
                waiter_session_id: row.get(1)?,
                target_kind: row.get(2)?,
                target_session_id: row.get(3)?,
                repo: row.get(4)?,
                number: row.get(5)?,
                deliverable: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
                until_at: row.get(9)?,
                target_ref: row.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn random_wait_id() -> String {
    let mut bytes = [0u8; 6];
    OsRng.fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("wait_{hex}")
}

#[cfg(test)]
mod tests;
