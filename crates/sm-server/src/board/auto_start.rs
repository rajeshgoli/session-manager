//! Persisted owner authorization to start a ticket when it becomes ready.

use anyhow::Result;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;

use super::model::{Key, TicketState};
use super::{BoardStore, Recomputed};
use crate::owner_push::format_ts;

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    pub repo: String,
    pub number: i64,
    pub agent_type: Option<String>,
    pub provider: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub brief: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub repo: String,
    pub number: i64,
    pub agent_type: Option<String>,
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub brief: Option<String>,
    pub state: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub session_id: Option<String>,
}

impl Record {
    pub fn key(&self) -> Key {
        (self.repo.clone(), self.number)
    }
    pub fn chip(&self) -> Value {
        json!({"agent_type":self.agent_type,"provider":self.provider,"model":self.model,
            "effort":self.effort,"state":self.state,"last_error":self.last_error,
            "attempts":self.attempts})
    }
}

/// The work one board recompute calls for. Closed and claimed tickets are
/// cancelled even while the owner has paused new starts.
pub fn plan(
    recomputed: &Recomputed,
    records: Vec<Record>,
    paused: bool,
) -> (Vec<Record>, Vec<(Key, &'static str)>) {
    let mut starts = Vec::new();
    let mut cancellations = Vec::new();
    for record in records
        .into_iter()
        .filter(|record| record.state == "waiting" || record.state == "failed")
    {
        let key = record.key();
        let facts = recomputed.board.facts.get(&key);
        let reason = match facts {
            Some(facts) if !facts.item.is_open() || facts.state == TicketState::Done => {
                Some("ticket closed")
            }
            Some(facts) if facts.holder.is_some() || facts.state == TicketState::InProgress => {
                Some("ticket claimed or started by hand")
            }
            None if recomputed
                .input
                .items
                .get(&key)
                .is_some_and(|item| !item.is_open()) =>
            {
                Some("ticket closed")
            }
            _ => None,
        };
        if let Some(reason) = reason {
            cancellations.push((key, reason));
            continue;
        }
        if paused || record.state != "waiting" || record.attempts >= 3 {
            continue;
        }
        if facts.is_some_and(|facts| {
            facts.state == TicketState::Ready
                && !facts.warnings.contains(&"stale")
                && !facts.warnings.contains(&"merged_not_closed")
        }) {
            starts.push(record);
        }
    }
    (starts, cancellations)
}

impl BoardStore {
    pub fn ticket_claimed(&self, key: &Key) -> Result<bool> {
        let Some(conn) = self.open_read()? else {
            return Ok(false);
        };
        Ok(conn
            .query_row(
                "SELECT 1 FROM work_claims WHERE repo=?1 AND number=?2
            AND kind='ticket' AND ended_at IS NULL LIMIT 1",
                params![key.0, key.1],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
    pub fn auto_starts(&self) -> Result<Vec<Record>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT repo, number, agent_type, provider, model, effort,
            brief, state, attempts, last_error, session_id FROM auto_starts",
        )?;
        let records = stmt
            .query_map([], |row| {
                Ok(Record {
                    repo: row.get(0)?,
                    number: row.get(1)?,
                    agent_type: row.get(2)?,
                    provider: row.get(3)?,
                    model: row.get(4)?,
                    effort: row.get(5)?,
                    brief: row.get(6)?,
                    state: row.get(7)?,
                    attempts: row.get(8)?,
                    last_error: row.get(9)?,
                    session_id: row.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }

    /// All choices in a lane are committed together.
    pub fn authorize_auto_starts(&self, choices: &[Choice], now: OffsetDateTime) -> Result<()> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ts = format_ts(now);
        for choice in choices {
            tx.execute(
                "INSERT INTO auto_starts
                (repo, number, agent_type, provider, model, effort, brief, state, attempts,
                 last_error, session_id, authorized_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'waiting', 0, NULL, NULL, ?8, ?8)
                ON CONFLICT(repo, number) DO UPDATE SET
                 agent_type=excluded.agent_type, provider=excluded.provider,
                 model=excluded.model, effort=excluded.effort, brief=excluded.brief,
                 state='waiting', attempts=0, last_error=NULL, session_id=NULL,
                 authorized_at=excluded.authorized_at, updated_at=excluded.updated_at",
                params![
                    choice.repo,
                    choice.number,
                    choice.agent_type,
                    choice.provider,
                    choice.model,
                    choice.reasoning_effort,
                    choice.brief,
                    ts
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn cancel_auto_start(&self, key: &Key, reason: &str, now: OffsetDateTime) -> Result<()> {
        self.open_write()?.execute(
            "UPDATE auto_starts SET state='cancelled',
            last_error=?3, updated_at=?4 WHERE repo=?1 AND number=?2 AND state IN ('waiting','failed')",
            params![key.0, key.1, reason, format_ts(now)],
        )?;
        Ok(())
    }

    pub fn auto_start_result(
        &self,
        key: &Key,
        result: std::result::Result<&str, &str>,
        now: OffsetDateTime,
    ) -> Result<i64> {
        let conn = self.open_write()?;
        match result {
            Ok(session_id) => {
                conn.execute(
                    "UPDATE auto_starts SET state='started', session_id=?3,
                    last_error=NULL, updated_at=?4 WHERE repo=?1 AND number=?2 AND state='waiting'",
                    params![key.0, key.1, session_id, format_ts(now)],
                )?;
                Ok(0)
            }
            Err(error) => {
                conn.execute(
                    "UPDATE auto_starts SET attempts=attempts+1,
                    state=CASE WHEN attempts+1 >= 3 THEN 'failed' ELSE 'waiting' END,
                    last_error=?3, updated_at=?4 WHERE repo=?1 AND number=?2 AND state='waiting'",
                    params![key.0, key.1, error, format_ts(now)],
                )?;
                Ok(conn.query_row(
                    "SELECT attempts FROM auto_starts WHERE repo=?1 AND number=?2",
                    params![key.0, key.1],
                    |row| row.get(0),
                )?)
            }
        }
    }

    pub fn ticket_tiers(&self) -> Result<std::collections::BTreeMap<Key, String>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Default::default());
        };
        let mut stmt =
            conn.prepare("SELECT repo, number, tier FROM board_items WHERE tier IS NOT NULL")?;
        let tiers = stmt
            .query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(tiers)
    }
}
