//! Open-work checks (sm#1452 appendix F, ticket #1486): the three moments
//! sm tells an agent, in facts only, that it looks finished but its ticket
//! or PR is still open.
//!
//! - Check A: a PR merged while a linked ticket is still open.
//! - Check B: at `sm task-complete`, the agent holds open work.
//! - Check C: idle for `idle_nudge_minutes` with nothing pending.
//!
//! Every message is an `important` queue message in the `work_claim`
//! category and writes a `claim.nudge` event on each item it names. What the
//! agent does about it is policy, written in AGENTS.md, never here.

use super::*;

/// GitHub closes a merged PR's linked tickets asynchronously, so Check A
/// waits this long after the merge before reading their state.
pub const MERGE_SETTLE: Duration = Duration::from_secs(60);

/// Check C's view of one session, from its record and the session feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleSession {
    pub session_id: String,
    /// The persisted `last_activity` (RFC 3339).
    pub last_activity: String,
    /// Its `waiting_on` in the session feed is not empty: a queue job, Codex
    /// review or owner review is outstanding.
    pub waiting: bool,
}

impl WorkClaimStore {
    /// Check A, after a sync pass: every PR with `merge_check = pending`
    /// whose merge is at least `MERGE_SETTLE` old and whose linked tickets
    /// are all settled. `answered` holds the items GitHub answered for in
    /// this pass (found or not found). Returns the sessions sent a message.
    pub fn run_check_a(
        &self,
        answered: &BTreeSet<(String, i64)>,
        sessions: &SessionDirectory,
        now: OffsetDateTime,
    ) -> Result<Vec<String>> {
        let Some(mut conn) = self.open_existing()? else {
            return Ok(Vec::new());
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = query_items(
            &tx,
            "WHERE kind = 'pr' AND merge_check = 'pending' ORDER BY repo, number",
            [],
        )?;
        let mut notified = Vec::new();
        for pr in pending {
            let merged_at = pr.merged_at.as_deref().and_then(parse_time);
            if merged_at.is_some_and(|merged| now - merged < MERGE_SETTLE) {
                continue;
            }
            let Some(open) = settled_open_tickets(&tx, &pr, answered)? else {
                continue;
            };
            if !open.is_empty() {
                check_a_messages(&tx, &pr, &open, merged_at, sessions, now, &mut notified)?;
            }
            tx.execute(
                "UPDATE work_items SET merge_check = 'done' WHERE repo = ?1 AND number = ?2",
                params![pr.repo, pr.number],
            )?;
        }
        tx.commit()?;
        Ok(notified)
    }

    /// Check B, step 1, at task-complete: marks the session's active claims
    /// due. Returns how many were marked (0: nothing to check).
    pub fn mark_check_b_due(&self, session_id: &str, now: OffsetDateTime) -> Result<usize> {
        let Some(conn) = self.open_existing()? else {
            return Ok(0);
        };
        Ok(conn.execute(
            "UPDATE work_claims SET check_b_due_at = ?2
              WHERE session_id = ?1 AND ended_at IS NULL AND reserved_at IS NULL",
            params![session_id, format_time(now)],
        )?)
    }

    /// Sessions with a Check B still due: processed at server start and on
    /// every sync pass, so a crash or a failed check only delays it.
    pub fn sessions_due_check_b(&self) -> Result<Vec<String>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(
            "SELECT DISTINCT session_id FROM work_claims
              WHERE check_b_due_at IS NOT NULL ORDER BY session_id",
        )?;
        let rows = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Check B, step 2: fetches the session's due, still-active items fresh
    /// and, if any is open, sends one message listing them. Clears the due
    /// marks in the same transaction. A retired session is cleared without
    /// a message. `Err` (GitHub unreachable included) leaves the marks due
    /// for the next sync pass. Returns the sessions sent a message.
    pub fn run_check_b(
        &self,
        session_id: &str,
        sessions: &SessionDirectory,
        source: &dyn WorkItemSource,
        now: OffsetDateTime,
    ) -> Result<Vec<String>> {
        let due = match self.open_read()? {
            Some(conn) => query_claims(
                &conn,
                "WHERE session_id = ?1 AND check_b_due_at IS NOT NULL",
                params![session_id],
            )?,
            None => return Ok(Vec::new()),
        };
        if due.is_empty() {
            return Ok(Vec::new());
        }
        let retired = sessions
            .get(session_id)
            .is_none_or(|session| session.state == HolderState::Retired);
        if !retired {
            let mut by_repo = BTreeMap::<String, Vec<i64>>::new();
            for claim in due.iter().filter(|claim| claim.ended_at.is_none()) {
                let numbers = by_repo.entry(claim.repo.clone()).or_default();
                if !numbers.contains(&claim.number) {
                    numbers.push(claim.number);
                }
            }
            for (repo, numbers) in by_repo {
                for chunk in numbers.chunks(MAX_ALIASES_PER_QUERY) {
                    let fetched = source.fetch(&repo, chunk);
                    self.record_fetch(&repo, chunk, &fetched)?;
                    if let Err(error) = fetched {
                        bail!("Check B for {session_id} deferred: {repo}: {error}");
                    }
                }
            }
        }
        let due_ids: BTreeSet<String> = due.into_iter().map(|claim| claim.id).collect();
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Re-read inside the transaction: another runner may have finished
        // this check already, and only the claims fetched above count.
        let still_due: Vec<WorkClaim> = query_claims(
            &tx,
            "WHERE session_id = ?1 AND check_b_due_at IS NOT NULL",
            params![session_id],
        )?
        .into_iter()
        .filter(|claim| due_ids.contains(&claim.id))
        .collect();
        let mut notified = Vec::new();
        if !retired {
            let named = open_claims(&tx, still_due.iter())?;
            if !named.is_empty() {
                let text = format!(
                    "[sm claim] At task-complete you hold: {}.",
                    held_list(&named)
                );
                nudge(&tx, "B", session_id, &named, &text, now, &mut notified)?;
                set_nudged_idle(&tx, &named, now)?;
            }
        }
        for claim in &still_due {
            tx.execute(
                "UPDATE work_claims SET check_b_due_at = NULL WHERE id = ?1",
                params![claim.id],
            )?;
        }
        tx.commit()?;
        Ok(notified)
    }

    /// Check C, after a sync pass: one message to each idle session that has
    /// waited on nothing for at least `idle` since its last activity and
    /// holds an armed claim on an open item. A claim is armed until it is
    /// named, and re-arms once the session's activity lands more than
    /// `idle` after that. Returns the sessions sent a message.
    pub fn run_check_c(
        &self,
        candidates: &[IdleSession],
        sessions: &SessionDirectory,
        idle: Duration,
        now: OffsetDateTime,
    ) -> Result<Vec<String>> {
        let Some(mut conn) = self.open_existing()? else {
            return Ok(Vec::new());
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut notified = Vec::new();
        for candidate in candidates {
            let is_idle = sessions
                .get(&candidate.session_id)
                .is_some_and(|session| session.state == HolderState::Idle);
            let Some(last_activity) = parse_time(&candidate.last_activity) else {
                continue;
            };
            let idle_for = now - last_activity;
            if !is_idle || candidate.waiting || idle_for < idle {
                continue;
            }
            let claims = query_claims(
                &tx,
                "WHERE session_id = ?1 AND ended_at IS NULL AND reserved_at IS NULL",
                params![candidate.session_id],
            )?;
            let armed = claims.iter().filter(|claim| {
                claim
                    .nudged_idle_at
                    .as_deref()
                    .and_then(parse_time)
                    .is_none_or(|nudged| last_activity - nudged > idle)
            });
            let named = open_claims(&tx, armed)?;
            if named.is_empty() {
                continue;
            }
            let text = format!(
                "[sm claim] Idle {}m, nothing pending. You hold: {}.",
                idle_for.whole_minutes(),
                held_list(&named)
            );
            nudge(
                &tx,
                "C",
                &candidate.session_id,
                &named,
                &text,
                now,
                &mut notified,
            )?;
            set_nudged_idle(&tx, &named, now)?;
        }
        tx.commit()?;
        Ok(notified)
    }
}

/// The open tickets linked to `pr`, or `None` while any linked ticket is
/// unsettled: not answered by GitHub in this pass and not already recorded
/// closed by an earlier fetch. A ticket GitHub reports missing is settled
/// and not open.
fn settled_open_tickets(
    conn: &Connection,
    pr: &WorkItem,
    answered: &BTreeSet<(String, i64)>,
) -> Result<Option<Vec<WorkItem>>> {
    let mut open = Vec::new();
    for number in linked_tickets(conn, &pr.repo, pr.number)? {
        let Some(ticket) = get_item(conn, &pr.repo, number)? else {
            return Ok(None);
        };
        if answered.contains(&(pr.repo.clone(), number)) {
            match ticket.sync_error.as_deref() {
                None if ticket.state == "open" && ticket.kind == "ticket" => open.push(ticket),
                None | Some("not found") => {}
                Some(_) => return Ok(None),
            }
        } else if ticket.synced_at.is_none() || ticket.state == "open" {
            return Ok(None);
        }
    }
    Ok(Some(open))
}

/// Check A's recipients and messages for one merged PR: the live or
/// dormant holders of each open ticket, and the PR's holders whose claim
/// ended with the merge; one message per session naming its tickets. With
/// no recipient the event is still written, with a null message id.
fn check_a_messages(
    conn: &Connection,
    pr: &WorkItem,
    open: &[WorkItem],
    merged_at: Option<OffsetDateTime>,
    sessions: &SessionDirectory,
    now: OffsetDateTime,
    notified: &mut Vec<String>,
) -> Result<()> {
    let reachable = |session_id: &str| {
        sessions
            .get(session_id)
            .is_some_and(|session| session.state != HolderState::Retired)
    };
    let mut recipients = BTreeMap::<String, BTreeSet<i64>>::new();
    for ticket in open {
        for claim in query_claims(
            conn,
            "WHERE repo = ?1 AND number = ?2 AND ended_at IS NULL AND reserved_at IS NULL",
            params![ticket.repo, ticket.number],
        )? {
            if reachable(&claim.session_id) {
                recipients
                    .entry(claim.session_id)
                    .or_default()
                    .insert(ticket.number);
            }
        }
    }
    for claim in query_claims(
        conn,
        "WHERE repo = ?1 AND number = ?2 AND end_reason = 'merged'",
        params![pr.repo, pr.number],
    )? {
        if reachable(&claim.session_id) {
            recipients
                .entry(claim.session_id)
                .or_default()
                .extend(open.iter().map(|ticket| ticket.number));
        }
    }
    let age = merged_at.map(|merged| now - merged);
    let text_for = |numbers: &BTreeSet<i64>| {
        let tickets: Vec<&WorkItem> = open
            .iter()
            .filter(|ticket| numbers.contains(&ticket.number))
            .collect();
        check_a_text(pr.number, age, &tickets)
    };
    let items_for = |numbers: &BTreeSet<i64>| {
        let mut items = vec![(WorkKind::Pr, pr.number)];
        items.extend(numbers.iter().map(|number| (WorkKind::Ticket, *number)));
        items
    };
    let stamp = format_time(now);
    if recipients.is_empty() {
        let all: BTreeSet<i64> = open.iter().map(|ticket| ticket.number).collect();
        let text = text_for(&all);
        for (kind, number) in items_for(&all) {
            nudge_event(conn, "A", None, None, &pr.repo, kind, number, &text, &stamp)?;
        }
        return Ok(());
    }
    for (session_id, numbers) in recipients {
        let text = text_for(&numbers);
        let message_id =
            crate::queue::enqueue_important_in_conn(conn, &session_id, &text, MESSAGE_CATEGORY)?;
        for (kind, number) in items_for(&numbers) {
            nudge_event(
                conn,
                "A",
                Some(&session_id),
                Some(&message_id),
                &pr.repo,
                kind,
                number,
                &text,
                &stamp,
            )?;
        }
        if !notified.contains(&session_id) {
            notified.push(session_id);
        }
    }
    Ok(())
}

/// `[sm claim] PR #1460 merged 3m ago. Linked ticket #1449 "…" is open.`
fn check_a_text(pr: i64, age: Option<time::Duration>, tickets: &[&WorkItem]) -> String {
    let merged = match age {
        Some(age) => format!("merged {} ago", format_age(age)),
        None => "merged".to_owned(),
    };
    let list = tickets
        .iter()
        .map(|ticket| format!("#{} \"{}\"", ticket.number, ticket.title))
        .collect::<Vec<_>>()
        .join(", ");
    if tickets.len() == 1 {
        format!("[sm claim] PR #{pr} {merged}. Linked ticket {list} is open.")
    } else {
        format!("[sm claim] PR #{pr} {merged}. Linked tickets {list} are open.")
    }
}

/// `3m`, `5h`, `2d`.
fn format_age(age: time::Duration) -> String {
    let minutes = age.whole_minutes().max(0);
    match minutes {
        0..=59 => format!("{minutes}m"),
        60..=2879 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

/// Of `claims`, those still active on an open, fetched item: tickets
/// first, then in claim order.
fn open_claims<'a>(
    conn: &Connection,
    claims: impl Iterator<Item = &'a WorkClaim>,
) -> Result<Vec<WorkClaim>> {
    let mut named = Vec::new();
    for claim in claims {
        if claim.ended_at.is_some() || claim.reserved_at.is_some() {
            continue;
        }
        let open = get_item(conn, &claim.repo, claim.number)?
            .is_some_and(|item| item.synced_at.is_some() && item.state == "open");
        if open {
            named.push(claim.clone());
        }
    }
    named.sort_by(|a, b| {
        (a.kind() != WorkKind::Ticket, &a.claimed_at, &a.id).cmp(&(
            b.kind() != WorkKind::Ticket,
            &b.claimed_at,
            &b.id,
        ))
    });
    Ok(named)
}

/// `ticket #1452 (open), PR #1470 (open, not merged)`.
fn held_list(claims: &[WorkClaim]) -> String {
    claims
        .iter()
        .map(|claim| match claim.kind() {
            WorkKind::Ticket => format!("ticket #{} (open)", claim.number),
            WorkKind::Pr => format!("PR #{} (open, not merged)", claim.number),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Enqueues `text` to `session_id` and writes a `claim.nudge` event on
/// every claimed item it names.
fn nudge(
    conn: &Connection,
    check: &str,
    session_id: &str,
    named: &[WorkClaim],
    text: &str,
    now: OffsetDateTime,
    notified: &mut Vec<String>,
) -> Result<()> {
    let message_id =
        crate::queue::enqueue_important_in_conn(conn, session_id, text, MESSAGE_CATEGORY)?;
    let stamp = format_time(now);
    for claim in named {
        nudge_event(
            conn,
            check,
            Some(session_id),
            Some(&message_id),
            &claim.repo,
            claim.kind(),
            claim.number,
            text,
            &stamp,
        )?;
    }
    if !notified.iter().any(|existing| existing == session_id) {
        notified.push(session_id.to_owned());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn nudge_event(
    conn: &Connection,
    check: &str,
    session_id: Option<&str>,
    message_id: Option<&str>,
    repo: &str,
    kind: WorkKind,
    number: i64,
    text: &str,
    now: &str,
) -> Result<()> {
    let (ticket, pr) = item_keys(conn, repo, kind, number)?;
    write_event(
        conn,
        "claim.nudge",
        session_id,
        Some(repo),
        ticket,
        pr,
        json!({"check": check, "message_id": message_id, "text": text}),
        now,
    )
}

fn set_nudged_idle(conn: &Connection, claims: &[WorkClaim], now: OffsetDateTime) -> Result<()> {
    let stamp = format_time(now);
    for claim in claims {
        conn.execute(
            "UPDATE work_claims SET nudged_idle_at = ?2 WHERE id = ?1",
            params![claim.id, stamp],
        )?;
    }
    Ok(())
}

fn query_items(
    conn: &Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<WorkItem>> {
    let mut statement = conn.prepare(&format!("SELECT {ITEM_COLUMNS} FROM work_items {filter}"))?;
    let rows = statement
        .query_map(params, item_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn format_time(at: OffsetDateTime) -> String {
    at.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// RFC 3339, or the naive `YYYY-MM-DDTHH:MM:SS[.f]` older session records
/// carry, read as UTC.
pub(super) fn parse_time(value: &str) -> Option<OffsetDateTime> {
    let value = value.trim();
    if let Ok(parsed) = OffsetDateTime::parse(value, &Rfc3339) {
        return Some(parsed);
    }
    OffsetDateTime::parse(&format!("{value}Z"), &Rfc3339).ok()
}
