use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const DEFAULT_USAGE_DB_PATH: &str = "~/.local/share/claude-sessions/usage.db";
const USAGE_DB_BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// Append-only mapping from an sm seat to every provider session identity it
/// has used. The rest of the usage ledger can consume this chain without
/// depending on the mutable current pointers in sessions.json.
#[derive(Debug, Clone)]
pub struct SeatSessionStore {
    db_path: PathBuf,
}

impl SeatSessionStore {
    pub fn for_state_file(state_file: &Path) -> Self {
        let default_state_file = expand_home("~/.local/share/claude-sessions/sessions.json");
        let db_path = if state_file == default_state_file {
            expand_home(DEFAULT_USAGE_DB_PATH)
        } else {
            // Alternate registries (including tests) get an isolated ledger.
            // The default production registry still uses the spec's usage.db.
            state_file.with_extension("usage.db")
        };
        Self { db_path }
    }

    pub fn append(
        &self,
        seat_id: &str,
        provider: &str,
        provider_session_id: &str,
        artifact_path: Option<&str>,
    ) -> Result<()> {
        let seat_id = required_text(seat_id, "seat_id")?;
        let provider = required_text(provider, "provider")?;
        let provider_session_id = required_text(provider_session_id, "provider_session_id")?;
        let artifact_path = artifact_path
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let connection = self.open()?;
        let observed_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("failed to format seat session timestamp")?;
        connection
            .execute(
                r#"
                INSERT OR IGNORE INTO seat_sessions (
                    seat_id,
                    provider,
                    provider_session_id,
                    artifact_path,
                    first_seen,
                    last_seen
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                "#,
                params![
                    seat_id,
                    provider,
                    provider_session_id,
                    artifact_path,
                    observed_at
                ],
            )
            .with_context(|| {
                format!(
                    "failed to append provider session {provider_session_id} for seat {seat_id}"
                )
            })?;
        Ok(())
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create usage database directory {}",
                    parent.display()
                )
            })?;
        }
        let connection = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open usage database {}", self.db_path.display()))?;
        connection
            .busy_timeout(USAGE_DB_BUSY_TIMEOUT)
            .context("failed to configure usage database busy timeout")?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable WAL mode for usage database")?;
        connection
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS seat_sessions (
                    seat_id             TEXT NOT NULL,
                    provider            TEXT NOT NULL,
                    provider_session_id TEXT NOT NULL,
                    artifact_path       TEXT,
                    first_seen          TEXT NOT NULL,
                    last_seen           TEXT NOT NULL,
                    PRIMARY KEY (seat_id, provider_session_id)
                );
                CREATE INDEX IF NOT EXISTS idx_seat_sessions_provider
                    ON seat_sessions(provider_session_id);
                "#,
            )
            .context("failed to initialize seat session schema")?;
        Ok(connection)
    }
}

fn required_text<'a>(value: &'a str, field: &str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    Ok(value)
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path));
    }
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    match std::env::var_os("HOME") {
        Some(home) => Path::new(&home).join(rest),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use rusqlite::Connection;

    use super::*;

    static TEST_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn append_keeps_distinct_provider_sessions_and_ignores_duplicates() {
        let state_file = test_path("append");
        let db_path = state_file.with_extension("usage.db");
        let store = SeatSessionStore::for_state_file(&state_file);

        store
            .append("seat-1", "claude", "provider-a", Some("/tmp/a.jsonl"))
            .unwrap();
        store
            .append("seat-1", "claude", "provider-b", Some("/tmp/b.jsonl"))
            .unwrap();
        store
            .append(
                "seat-1",
                "claude",
                "provider-a",
                Some("/tmp/replayed-a.jsonl"),
            )
            .unwrap();

        let connection = Connection::open(&db_path).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT provider_session_id, artifact_path FROM seat_sessions ORDER BY provider_session_id",
            )
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("provider-a".to_owned(), "/tmp/a.jsonl".to_owned()),
                ("provider-b".to_owned(), "/tmp/b.jsonl".to_owned()),
            ]
        );
    }

    fn test_path(label: &str) -> PathBuf {
        let counter = TEST_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "sm-seat-sessions-{label}-{}-{counter}.json",
            std::process::id()
        ))
    }
}
