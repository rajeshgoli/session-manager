//! Board alerts (appendix I, ticket #1682): after each recompute, one
//! "ready" notice per lane whose members just became Ready, and one "lane
//! done" notice per lane whose goal closed. Each is an owner notice, so the
//! follow worker pushes it and withdraws it once the owner opens the board.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use time::OffsetDateTime;

use super::model::{short_ref, Key, TicketState, WARN_MERGED_NOT_CLOSED};
use super::{insert_event, BoardStore, Recomputed, NOTICE_BOARD_LANE_DONE, NOTICE_BOARD_READY};
use crate::owner_push::{format_ts, NewNotice, OwnerPushStore};

/// How many ticket numbers a ready alert names before "+N more".
const READY_NAMED: usize = 4;

/// An alert a recompute decided on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub kind: &'static str,
    pub lane_id: i64,
    pub title: String,
    pub body: String,
    pub reader_path: String,
    /// Ready alerts: the tickets that became Ready, in row order.
    pub tickets: Vec<Key>,
}

/// The alerts one recompute calls for. `last_holder` names the session
/// whose claim on a ticket ended most recently.
pub fn decide(recomputed: &Recomputed, last_holder: &dyn Fn(&Key) -> Option<String>) -> Vec<Alert> {
    decide_excluding(recomputed, last_holder, &Default::default())
}

pub fn decide_excluding(
    recomputed: &Recomputed,
    last_holder: &dyn Fn(&Key) -> Option<String>,
    excluded: &std::collections::BTreeSet<Key>,
) -> Vec<Alert> {
    let board = &recomputed.board;
    let input = &recomputed.input;
    let ended_ranks: Vec<i64> = recomputed.ended.iter().map(|(lane, _)| lane.rank).collect();
    let mut alerts = Vec::new();
    for view in &board.lanes {
        let lane_id = view.lane.id;
        if recomputed.first_seen.contains(&lane_id)
            || recomputed.deferred.contains(&lane_id)
            || recomputed.ended.iter().any(|(lane, _)| lane.id == lane_id)
        {
            continue;
        }
        let Some(previous) = input.members.get(&lane_id) else {
            continue;
        };
        let listed: Vec<&Key> = view
            .rows
            .iter()
            .map(|row| &row.key)
            .filter(|key| {
                let facts = &board.facts[*key];
                facts.state == TicketState::Ready
                    && !excluded.contains(*key)
                    && previous
                        .get(*key)
                        .is_some_and(|member| member.state != TicketState::Ready)
                    && !input.stale.contains(&key.0)
                    && !facts.warnings.contains(&WARN_MERGED_NOT_CLOSED)
            })
            .collect();
        let base = &view.lane.goal.0;
        let mut explained = Vec::new();
        for first in listed {
            let from = previous[first].state;
            let cause = match from {
                TicketState::Done => format!("{} reopened", short_ref(first, base)),
                TicketState::InProgress | TicketState::NeedsYou => {
                    let retired = input
                        .holders
                        .get(first)
                        .and_then(|holders| holders.first())
                        .map(|holder| holder.name.clone());
                    let prs = &board.facts[first].prs;
                    let closed_pr = prs
                        .iter()
                        .filter(|pr| pr.state == "CLOSED")
                        .max_by_key(|pr| pr.number)
                        .filter(|_| !prs.iter().any(|pr| pr.state == "OPEN"));
                    match (retired, closed_pr) {
                        (Some(name), _) => format!("{name} let go of it"),
                        (None, Some(pr)) => format!("PR #{} closed", pr.number),
                        (None, None) => match last_holder(first) {
                            Some(name) => format!("{name} let go of it"),
                            None => "its agent let go of it".to_owned(),
                        },
                    }
                }
                TicketState::Blocked
                | TicketState::Ready
                | TicketState::CloseReady
                | TicketState::Standing => {
                    let closed: Vec<String> = board.facts[first]
                        .waits_on
                        .iter()
                        .filter(|blocker| {
                            board
                                .facts
                                .get(*blocker)
                                .is_some_and(|facts| facts.state == TicketState::Done)
                                && previous
                                    .get(*blocker)
                                    .is_some_and(|member| member.state != TicketState::Done)
                        })
                        .map(|blocker| short_ref(blocker, base))
                        .collect();
                    if closed.is_empty() {
                        if previous[first]
                            .waits_on
                            .iter()
                            .any(|blocker| !board.facts[first].waits_on.contains(blocker))
                        {
                            "a link was removed".to_owned()
                        } else {
                            // Recovery or an unexplained state change is not
                            // evidence that a dependency link was removed.
                            continue;
                        }
                    } else {
                        format!("{} closed", closed.join(", "))
                    }
                }
            };
            explained.push((first, cause));
        }
        let Some((_, cause)) = explained.first() else {
            continue;
        };
        let listed: Vec<&Key> = explained.iter().map(|(key, _)| *key).collect();
        let mut numbers: Vec<String> = listed
            .iter()
            .take(READY_NAMED)
            .map(|key| short_ref(key, base))
            .collect();
        if listed.len() > READY_NAMED {
            numbers.push(format!("+{} more", listed.len() - READY_NAMED));
        }
        let rank =
            view.lane.rank - ended_ranks.iter().filter(|r| **r < view.lane.rank).count() as i64;
        alerts.push(Alert {
            kind: NOTICE_BOARD_READY,
            lane_id,
            title: format!(
                "Ready in lane {rank}, {}",
                goal_title(recomputed, &view.lane.goal)
            ),
            body: format!("{} can start — {cause}", numbers.join(", ")),
            reader_path: format!("/board#lane-{lane_id}"),
            tickets: listed.into_iter().cloned().collect(),
        });
    }
    for (lane, _) in &recomputed.ended {
        let repo_short = lane.goal.0.rsplit('/').next().unwrap_or(&lane.goal.0);
        alerts.push(Alert {
            kind: NOTICE_BOARD_LANE_DONE,
            lane_id: lane.id,
            title: format!("Lane done: {}", goal_title(recomputed, &lane.goal)),
            body: format!("{repo_short}#{} closed · lanes below move up", lane.goal.1),
            reader_path: "/board".to_owned(),
            tickets: Vec::new(),
        });
    }
    alerts
}

fn goal_title(recomputed: &Recomputed, goal: &Key) -> String {
    recomputed
        .input
        .items
        .get(goal)
        .map(|item| item.title.clone())
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| format!("{}#{}", goal.0, goal.1))
}

/// Decides the recompute's alerts and creates one owner notice for each,
/// recording a `push_sent` event (whose id the notice's subject carries) or,
/// when the notice can't be stored, `push_failed`.
pub fn send(
    store: &BoardStore,
    push: &OwnerPushStore,
    user_id: &str,
    recomputed: &Recomputed,
    now: OffsetDateTime,
) -> Result<Vec<Alert>> {
    send_excluding(store, push, user_id, recomputed, now, &Default::default())
}

pub fn send_excluding(
    store: &BoardStore,
    push: &OwnerPushStore,
    user_id: &str,
    recomputed: &Recomputed,
    now: OffsetDateTime,
    excluded: &std::collections::BTreeSet<Key>,
) -> Result<Vec<Alert>> {
    let alerts = decide_excluding(
        recomputed,
        &|key| store.last_holder_name(key).ok().flatten(),
        excluded,
    );
    if alerts.is_empty() {
        return Ok(alerts);
    }
    let ts = format_ts(now);
    let conn = store.open_write()?;
    for alert in &alerts {
        let detail = if alert.tickets.is_empty() {
            alert.title.clone()
        } else {
            tickets_detail(&alert.tickets)
        };
        let event_id = insert_event(
            &conn,
            &ts,
            "push_sent",
            Some(alert.lane_id),
            alert.tickets.first(),
            None,
            None,
            Some(&detail),
        )?;
        let notice = NewNotice {
            user_id: user_id.to_owned(),
            kind: alert.kind.to_owned(),
            session_id: "board".to_owned(),
            session_name: "sm board".to_owned(),
            subject_id: format!("board:{}:{event_id}", alert.lane_id),
            title: alert.title.clone(),
            body: alert.body.clone(),
            reader_path: alert.reader_path.clone(),
            blocking: false,
        };
        if let Err(error) = push.create_notice(&notice, now) {
            conn.execute(
                "UPDATE board_events SET kind = 'push_failed', detail = ?2 WHERE id = ?1",
                params![event_id, format!("{error:#}")],
            )?;
        }
    }
    Ok(alerts)
}

/// `owner/name#N owner/name#M`: the tickets a ready alert listed.
fn tickets_detail(tickets: &[Key]) -> String {
    tickets
        .iter()
        .map(|(repo, number)| format!("{repo}#{number}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The event id a board notice's subject (`board:{lane}:{event}`) carries.
pub fn subject_event(subject_id: &str) -> Option<(i64, i64)> {
    let mut parts = subject_id.strip_prefix("board:")?.split(':');
    let lane = parts.next()?.parse().ok()?;
    let event = parts.next()?.parse().ok()?;
    Some((lane, event))
}

/// Whether the owner opened the board after the notice was made: the
/// newest event when the owner looked is at least the notice's event, so
/// two in one second still order.
pub fn notice_opened(subject_id: &str, created_at: &str, seen: Option<&(String, i64)>) -> bool {
    let Some((seen_at, seen_event_id)) = seen else {
        return false;
    };
    match subject_event(subject_id) {
        Some((_, event_id)) => event_id <= *seen_event_id,
        None => seen_at.as_str() > created_at,
    }
}

impl BoardStore {
    /// The name of the session whose claim on the ticket ended last.
    pub fn last_holder_name(&self, key: &Key) -> Result<Option<String>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        if !super::table_exists(&conn, "work_claims")? {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                "SELECT IFNULL(session_name, session_id) FROM work_claims
                 WHERE repo = ?1 AND number = ?2 AND kind = 'ticket' AND reserved_at IS NULL
                 ORDER BY IFNULL(ended_at, claimed_at) DESC, claimed_at DESC LIMIT 1",
                params![key.0, key.1],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// `still_wanted` for a ready notice: its lane is active and one of the
    /// tickets it listed is still Ready (so no one holds it) as of the last
    /// recompute.
    pub fn ready_notice_wanted(&self, subject_id: &str) -> Result<bool> {
        let Some((lane_id, event_id)) = subject_event(subject_id) else {
            return Ok(false);
        };
        let Some(conn) = self.open_read()? else {
            return Ok(false);
        };
        let detail: Option<String> = conn
            .query_row(
                "SELECT detail FROM board_events WHERE id = ?1 AND lane_id = ?2",
                params![event_id, lane_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(detail) = detail else {
            return Ok(false);
        };
        for ticket in detail.split_whitespace() {
            let Some((repo, number)) = ticket.rsplit_once('#') else {
                continue;
            };
            let ready = conn
                .query_row(
                    "SELECT 1 FROM board_members m
                     JOIN board_lanes l ON l.id = m.lane_id AND l.ended_at IS NULL
                     WHERE m.lane_id = ?1 AND m.repo = ?2 AND m.number = ?3
                       AND m.state = 'ready'",
                    params![lane_id, repo, number.parse::<i64>().unwrap_or(-1)],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if ready {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
