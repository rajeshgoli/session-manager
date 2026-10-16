//! Live switches (B.1). The server keeps an in-memory copy of
//! `experiment_switches`, refreshed after every write, and code asks
//! [`is_on`]. A missing key reads as off, and every switch reads as off while
//! `autopm.enabled` is false.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwitchRow {
    pub key: String,
    pub experiment_id: String,
    pub on: bool,
    pub held: bool,
    pub changed_at_ms: i64,
    pub changed_by: String,
    pub reason: Option<String>,
}

/// The live copy. `version` rises on every switch write and on enable or
/// disable; it starts at the server's start time in ms, so a restart is a
/// new version to every client.
#[derive(Debug)]
pub struct Switches {
    rows: RwLock<BTreeMap<String, SwitchRow>>,
    enabled: AtomicBool,
    version: AtomicU64,
}

impl Switches {
    pub fn new(enabled: bool) -> Self {
        Self {
            rows: RwLock::new(BTreeMap::new()),
            enabled: AtomicBool::new(enabled),
            version: AtomicU64::new(u64::try_from(super::now_ms()).unwrap_or(0)),
        }
    }

    /// Re-reads every row, then raises the version.
    pub fn reload(&self, conn: &Connection) -> Result<()> {
        let rows = load(conn)?;
        *self.rows.write().unwrap_or_else(|e| e.into_inner()) =
            rows.into_iter().map(|row| (row.key.clone(), row)).collect();
        self.bump();
        Ok(())
    }

    pub fn bump(&self) {
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Records `autopm.enabled`; a change raises the version.
    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::SeqCst) != enabled {
            self.bump();
        }
    }

    pub fn is_on(&self, key: &str) -> bool {
        self.enabled()
            && self
                .rows
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(key)
                .is_some_and(|row| row.on)
    }

    /// `{key: bool}` as clients receive it: what [`Self::is_on`] reads.
    pub fn effective(&self) -> BTreeMap<String, bool> {
        let enabled = self.enabled();
        self.rows
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|row| (row.key.clone(), enabled && row.on))
            .collect()
    }

    pub fn row(&self, key: &str) -> Option<SwitchRow> {
        self.rows
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }
}

fn live() -> &'static RwLock<Option<Arc<Switches>>> {
    static LIVE: OnceLock<RwLock<Option<Arc<Switches>>>> = OnceLock::new();
    LIVE.get_or_init(|| RwLock::new(None))
}

/// Makes `switches` the copy [`is_on`] reads; the router installs its own.
pub fn install(switches: Arc<Switches>) {
    *live().write().unwrap_or_else(|e| e.into_inner()) = Some(switches);
}

/// Whether the switch `key` is on. Code behind an experiment asks this; off
/// must equal the behaviour before the change exactly.
pub fn is_on(key: &str) -> bool {
    live()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|switches| switches.is_on(key))
}

pub fn load(conn: &Connection) -> Result<Vec<SwitchRow>> {
    let mut stmt = conn.prepare(
        "SELECT key, experiment_id, on_, held, changed_at_ms, changed_by, reason
         FROM experiment_switches ORDER BY key",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SwitchRow {
                key: row.get(0)?,
                experiment_id: row.get(1)?,
                on: row.get(2)?,
                held: row.get(3)?,
                changed_at_ms: row.get(4)?,
                changed_by: row.get(5)?,
                reason: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, key: &str) -> Result<Option<SwitchRow>> {
    Ok(load(conn)?.into_iter().find(|row| row.key == key))
}

/// Writes one switch's value. `held` of `None` keeps the stored flag.
pub fn write(
    conn: &Connection,
    key: &str,
    on: bool,
    held: Option<bool>,
    changed_by: &str,
    reason: Option<&str>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE experiment_switches SET on_ = ?2, held = COALESCE(?3, held),
           changed_at_ms = ?4, changed_by = ?5, reason = ?6
         WHERE key = ?1",
        params![key, on, held, now, changed_by, reason],
    )?;
    Ok(())
}

/// A key is `autopm.` and a slug of lowercase letters, digits and dashes.
pub fn valid_key(key: &str) -> bool {
    key.strip_prefix("autopm.").is_some_and(|slug| {
        !slug.is_empty()
            && slug.len() <= 64
            && slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}
