//! The health guard (B.4). Three counters, written each minute to
//! `guard_samples` in `utilization.db`, so the baseline survives a restart.
//! A counter whose last ten minutes exceed its threshold turns off every
//! switch turned on within the last hour.

use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::HOUR_MS;

pub const MINUTE_MS: i64 = 60_000;
pub const COUNTERS: [&str; 3] = ["owner_5xx", "delivery_failures", "panics"];
const RETENTION_MS: i64 = 8 * 24 * HOUR_MS;
const BASELINE_FROM_MS: i64 = 7 * 24 * HOUR_MS;
const BASELINE_UNTIL_MS: i64 = HOUR_MS;
const CURRENT_MINUTES: i64 = 10;
/// Under a day of 10-minute sums, the threshold is the floor.
const MIN_BASELINE_SUMS: usize = 1440;
const FLOOR: f64 = 3.0;
/// Switches turned on within this long are the guard's to turn off.
pub const SCOPE_MS: i64 = HOUR_MS;

/// The in-memory counts since the last minute boundary.
#[derive(Debug, Default)]
pub struct Counters {
    pub owner_5xx: AtomicU64,
    pub panics: AtomicU64,
}

/// Failed `sm send` deliveries happen in the session store, which has no
/// server state, so they count here.
static DELIVERY_FAILURES: AtomicU64 = AtomicU64::new(0);

pub fn record_delivery_failure() {
    DELIVERY_FAILURES.fetch_add(1, Ordering::Relaxed);
}

impl Counters {
    /// The counts since the last call, which resets them.
    pub fn take(&self) -> [i64; 3] {
        [
            self.owner_5xx.swap(0, Ordering::Relaxed),
            DELIVERY_FAILURES.swap(0, Ordering::Relaxed),
            self.panics.swap(0, Ordering::Relaxed),
        ]
        .map(|n| i64::try_from(n).unwrap_or(i64::MAX))
    }
}

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS guard_samples (
           minute_ms         INTEGER PRIMARY KEY,
           owner_5xx         INTEGER NOT NULL,
           delivery_failures INTEGER NOT NULL,
           panics            INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

pub fn open(db: &Path) -> Result<Connection> {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    init_schema(&conn)?;
    Ok(conn)
}

/// Writes one minute's counts (adding to a row a restart already wrote for
/// that minute) and prunes rows past eight days.
pub fn write_minute(conn: &Connection, minute_ms: i64, counts: [i64; 3]) -> Result<()> {
    conn.execute(
        "INSERT INTO guard_samples(minute_ms, owner_5xx, delivery_failures, panics)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(minute_ms) DO UPDATE SET
           owner_5xx = owner_5xx + excluded.owner_5xx,
           delivery_failures = delivery_failures + excluded.delivery_failures,
           panics = panics + excluded.panics",
        params![minute_ms, counts[0], counts[1], counts[2]],
    )?;
    conn.execute(
        "DELETE FROM guard_samples WHERE minute_ms < ?1",
        [minute_ms - RETENTION_MS],
    )?;
    Ok(())
}

/// One counter over its threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct Trip {
    pub counter: &'static str,
    pub value: i64,
    pub threshold: f64,
}

/// Each counter's current value and threshold at `now_minute`, the latest
/// written minute.
pub fn assess(conn: &Connection, now_minute: i64) -> Result<Vec<(&'static str, i64, f64)>> {
    let mut stmt = conn.prepare(
        "SELECT minute_ms, owner_5xx, delivery_failures, panics FROM guard_samples
         WHERE minute_ms >= ?1 AND minute_ms <= ?2",
    )?;
    let rows: BTreeMap<i64, [i64; 3]> = stmt
        .query_map(
            params![
                now_minute - BASELINE_FROM_MS - CURRENT_MINUTES * MINUTE_MS,
                now_minute
            ],
            |row| Ok((row.get(0)?, [row.get(1)?, row.get(2)?, row.get(3)?])),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (index, counter) in COUNTERS.iter().enumerate() {
        let current: i64 = (0..CURRENT_MINUTES)
            .filter_map(|back| rows.get(&(now_minute - back * MINUTE_MS)))
            .map(|row| row[index])
            .sum();
        let mut sums = Vec::new();
        let mut minute = now_minute - BASELINE_FROM_MS;
        while minute <= now_minute - BASELINE_UNTIL_MS {
            let window: Option<i64> = (0..CURRENT_MINUTES)
                .map(|back| rows.get(&(minute - back * MINUTE_MS)).map(|row| row[index]))
                .sum();
            if let Some(sum) = window {
                sums.push(sum);
            }
            minute += MINUTE_MS;
        }
        out.push((*counter, current, threshold(&mut sums)));
    }
    Ok(out)
}

/// `max(3, 2 × p95)` by nearest rank; 3 with under a day of sums.
pub fn threshold(sums: &mut [i64]) -> f64 {
    if sums.len() < MIN_BASELINE_SUMS {
        return FLOOR;
    }
    sums.sort_unstable();
    let rank = ((0.95 * sums.len() as f64).ceil() as usize).max(1);
    FLOOR.max(2.0 * sums[rank - 1] as f64)
}

pub fn trips(conn: &Connection, now_minute: i64) -> Result<Vec<Trip>> {
    Ok(assess(conn, now_minute)?
        .into_iter()
        .filter(|(_, value, threshold)| *value as f64 > *threshold)
        .map(|(counter, value, threshold)| Trip {
            counter,
            value,
            threshold,
        })
        .collect())
}
