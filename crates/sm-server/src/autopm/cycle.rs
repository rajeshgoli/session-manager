//! Durable weekly cycle bookkeeping. External operations are retryable by cycle key.
use super::{AutopmStore, Settings};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use time::{Date, OffsetDateTime, UtcOffset};

#[derive(Debug, Clone)]
pub struct Cycle {
    pub week: String,
    pub ticket: Option<i64>,
    pub opened_ms: i64,
    pub ended_ms: Option<i64>,
    pub ready: bool,
}

pub fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS autopm_cycles (
        week TEXT PRIMARY KEY, ticket INTEGER UNIQUE, opened_ms INTEGER NOT NULL,
        ended_ms INTEGER, ready INTEGER NOT NULL DEFAULT 0, rolled INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS autopm_cycle_control (
        id INTEGER PRIMARY KEY CHECK(id=1), enabled INTEGER NOT NULL DEFAULT 0,
        pause_seen TEXT, score_since_ms INTEGER NOT NULL DEFAULT 0);
        INSERT OR IGNORE INTO autopm_cycle_control(id) VALUES(1);",
    )?;
    Ok(())
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Cycle> {
    Ok(Cycle {
        week: r.get(0)?,
        ticket: r.get(1)?,
        opened_ms: r.get(2)?,
        ended_ms: r.get(3)?,
        ready: r.get(4)?,
    })
}

pub fn current(store: &AutopmStore) -> Result<Option<Cycle>> {
    let conn = store.open()?;
    schema(&conn)?;
    Ok(conn.query_row("SELECT week,ticket,opened_ms,ended_ms,ready FROM autopm_cycles ORDER BY opened_ms DESC LIMIT 1", [], row).optional()?)
}

fn week(date: Date) -> String {
    let (year, week, _) = date.to_iso_week_date();
    format!("{year}-W{week:02}")
}

/// The host uses the owner's local timezone, as the existing budget/deploy week does.
/// Passing an offset makes boundary and daylight-saving cases deterministic in tests.
pub fn due_week(now: i64, offset: UtcOffset, first: bool) -> String {
    let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .to_offset(offset);
    let before = at.weekday() == time::Weekday::Monday && (at.hour(), at.minute()) < (9, 7);
    week(if before && !first {
        at.date() - time::Duration::days(7)
    } else {
        at.date()
    })
}

pub trait Actions {
    fn lane_and_policy(&self) -> Result<()>;
    /// Must reconcile a prior successful remote create before creating an issue.
    fn issue(&self, cycle: &Cycle) -> Result<i64>;
    fn attach(&self, ticket: i64) -> Result<()>;
    fn rollover(&self, old: i64, new: i64) -> Result<()>;
    fn wake(&self, key: &str, text: &str) -> Result<()>;
    fn start(&self, cycle: &Cycle) -> Result<()>;
    fn pause(&self, at: &str) -> Result<()>;
    fn pause_notice(&self, at: &str, weeks: &[String]) -> Result<()>;
}

/// Counting cycles have at least one terminal measurement. Empty cycles do not reset a run.
pub fn losing_run(conn: &Connection, since: i64) -> Result<Vec<String>> {
    let mut stmt=conn.prepare("SELECT c.week,
        SUM(CASE WHEN m.decision='keep' THEN 1 ELSE 0 END)
        FROM autopm_cycles c JOIN autopm_measurements m ON m.at_ms>=c.opened_ms AND m.at_ms<c.ended_ms
        WHERE c.ended_ms IS NOT NULL AND c.ended_ms>?1 AND m.status='ok'
        AND m.decision IN ('keep','revert')
        GROUP BY c.week,c.opened_ms ORDER BY c.opened_ms")?;
    let mut run = Vec::new();
    for item in stmt.query_map([since], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (week, kept) = item?;
        if kept > 0 {
            run.clear();
        } else {
            run.push(week);
        }
    }
    Ok(run)
}

/// One serialized upkeep pass. Call only from the Auto PM ticker.
pub fn tick(
    store: &AutopmStore,
    settings: &Settings,
    now: i64,
    offset: UtcOffset,
    skipped: bool,
    actions: &dyn Actions,
) -> Result<()> {
    let conn = store.open()?;
    schema(&conn)?;
    if !settings.enabled {
        conn.execute("UPDATE autopm_cycle_control SET enabled=0 WHERE id=1", [])?;
        return Ok(());
    }
    let (enabled, seen, since): (bool, Option<String>, i64) = conn.query_row(
        "SELECT enabled,pause_seen,score_since_ms FROM autopm_cycle_control WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if !enabled {
        actions.lane_and_policy()?;
        conn.execute("UPDATE autopm_cycle_control SET enabled=1 WHERE id=1", [])?;
    }
    if let Some(paused) = settings.paused_at.as_deref() {
        conn.execute(
            "UPDATE autopm_cycle_control SET pause_seen=?1 WHERE id=1",
            [paused],
        )?;
        let active: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM autopm_experiments WHERE state IN ('live','measuring'))",
            [],
            |r| r.get(0),
        )?;
        if !active {
            let weeks = losing_run(&conn, since)?;
            actions.wake(
                &format!("pause:{paused}"),
                "[sm autopm] paused: publish a final readout and go idle",
            )?;
            actions.pause_notice(paused, &weeks)?;
        }
        return Ok(());
    }
    let since = if seen.is_some() {
        conn.execute(
            "UPDATE autopm_cycle_control SET pause_seen=NULL,score_since_ms=?1 WHERE id=1",
            [now],
        )?;
        now
    } else {
        since
    };
    if skipped {
        return Ok(());
    }
    let old = current(store)?;
    let due = due_week(now, offset, old.is_none());
    // Recover a first enable before Monday 09:07 without going back a week.
    let due = old
        .as_ref()
        .filter(|c| c.week > due)
        .map_or(due.clone(), |c| c.week.clone());
    if old
        .as_ref()
        .is_some_and(|c| c.week != due && c.ended_ms.is_none())
    {
        conn.execute(
            "UPDATE autopm_cycles SET ended_ms=?1 WHERE week=?2",
            params![now, old.as_ref().unwrap().week],
        )?;
    }
    let losses = losing_run(&conn, since)?;
    if losses.len() >= 4 {
        let at = super::format_ms(now);
        actions.pause(&at)?;
        conn.execute(
            "UPDATE autopm_cycle_control SET pause_seen=?1 WHERE id=1",
            [&at],
        )?;
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO autopm_cycles(week,opened_ms) VALUES(?1,?2)",
        params![due, now],
    )?;
    let mut cycle = current(store)?.expect("inserted cycle");
    if cycle.ticket.is_none() {
        let ticket = actions.issue(&cycle)?;
        conn.execute(
            "UPDATE autopm_cycles SET ticket=?1 WHERE week=?2",
            params![ticket, cycle.week],
        )?;
        cycle.ticket = Some(ticket);
    }
    let ticket = cycle.ticket.expect("filed cycle");
    if !cycle.ready {
        actions.attach(ticket)?;
        conn.execute(
            "UPDATE autopm_cycles SET ready=1 WHERE week=?1",
            [&cycle.week],
        )?;
        cycle.ready = true;
    }
    let pending = {
        let mut stmt=conn.prepare("SELECT week,ticket FROM autopm_cycles WHERE week<>?1 AND rolled=0 AND ticket IS NOT NULL")?;
        let rows = stmt
            .query_map([&cycle.week], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (week, old_ticket) in pending {
        actions.rollover(old_ticket, ticket)?;
        conn.execute("UPDATE autopm_cycles SET rolled=1 WHERE week=?1", [week])?;
    }
    actions.start(&cycle)?;
    actions.wake(
        &format!("cycle:{}", cycle.week),
        &format!("[sm autopm] cycle {} opened: #{ticket}", cycle.week),
    )?;
    Ok(())
}
