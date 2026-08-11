use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, Transaction};
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
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    pub(crate) fn default_db_path() -> PathBuf {
        expand_home(DEFAULT_USAGE_DB_PATH)
    }

    pub fn for_state_file(state_file: &Path) -> Self {
        let default_state_file = expand_home("~/.local/share/claude-sessions/sessions.json");
        let db_path = if state_file == default_state_file {
            Self::default_db_path()
        } else {
            // Alternate registries (including tests) get an isolated ledger.
            // The default production registry still uses the spec's usage.db.
            state_file.with_extension("usage.db")
        };
        Self::new(db_path)
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

        let mut connection = self.open()?;
        let transaction = connection
            .transaction()
            .context("failed to start seat session append")?;
        let observed_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("failed to format seat session timestamp")?;
        append_identity(
            &transaction,
            seat_id,
            provider,
            provider_session_id,
            artifact_path,
            &observed_at,
        )
        .with_context(|| {
            format!("failed to append provider session {provider_session_id} for seat {seat_id}")
        })?;
        transaction
            .commit()
            .context("failed to commit seat session append")?;
        Ok(())
    }

    pub fn append_batch(&self, sessions: &[SeatSessionIdentity]) -> Result<()> {
        if sessions.is_empty() {
            return Ok(());
        }
        let mut connection = self.open()?;
        let transaction = connection
            .transaction()
            .context("failed to start seat session reconciliation")?;
        let observed_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("failed to format seat session timestamp")?;
        for session in sessions {
            append_identity(
                &transaction,
                &session.seat_id,
                &session.provider,
                &session.provider_session_id,
                session.artifact_path.as_deref(),
                &observed_at,
            )
            .with_context(|| {
                format!(
                    "failed to reconcile provider session {} for seat {}",
                    session.provider_session_id, session.seat_id
                )
            })?;
            if session.artifact_path.is_some() {
                transaction
                    .execute(
                        r#"
                        UPDATE seat_sessions
                        SET artifact_path = ?4
                        WHERE seat_id = ?1
                          AND provider = ?2
                          AND provider_session_id = ?3
                          AND artifact_path IS NULL
                        "#,
                        params![
                            session.seat_id,
                            session.provider,
                            session.provider_session_id,
                            session.artifact_path
                        ],
                    )
                    .with_context(|| {
                        format!(
                            "failed to repair artifact path for provider session {} on seat {}",
                            session.provider_session_id, session.seat_id
                        )
                    })?;
            }
        }
        transaction
            .commit()
            .context("failed to commit seat session reconciliation")?;
        Ok(())
    }

    pub fn claimed_provider_sessions(&self) -> Result<BTreeSet<(String, String)>> {
        let connection = self.open()?;
        let mut statement = connection
            .prepare("SELECT provider, provider_session_id FROM seat_sessions")
            .context("failed to prepare seat session claim query")?;
        let claims = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .context("failed to query claimed provider sessions")?
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .context("failed to read claimed provider sessions")?;
        Ok(claims)
    }

    pub fn provider_sessions_missing_artifacts(&self, provider: &str) -> Result<BTreeSet<String>> {
        let connection = self.open()?;
        let mut statement = connection
            .prepare(
                r#"
                SELECT DISTINCT provider_session_id
                FROM seat_sessions
                WHERE provider = ?1
                  AND (artifact_path IS NULL OR TRIM(artifact_path) = '')
                "#,
            )
            .context("failed to prepare missing usage artifact query")?;
        let sessions = statement
            .query_map([provider], |row| row.get(0))
            .context("failed to query missing usage artifacts")?
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .context("failed to read missing usage artifacts")?;
        Ok(sessions)
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

fn append_identity(
    transaction: &Transaction<'_>,
    seat_id: &str,
    provider: &str,
    provider_session_id: &str,
    artifact_path: Option<&str>,
    observed_at: &str,
) -> rusqlite::Result<()> {
    if seat_id != "unassigned" {
        transaction.execute(
            r#"
            DELETE FROM seat_sessions
            WHERE seat_id = 'unassigned'
              AND provider = ?1
              AND provider_session_id = ?2
            "#,
            params![provider, provider_session_id],
        )?;
    }
    transaction.execute(
        r#"
        INSERT OR IGNORE INTO seat_sessions (
            seat_id,
            provider,
            provider_session_id,
            artifact_path,
            first_seen,
            last_seen
        )
        SELECT ?1, ?2, ?3, ?4, ?5, ?5
        WHERE ?1 != 'unassigned'
           OR NOT EXISTS (
                SELECT 1
                FROM seat_sessions
                WHERE seat_id != 'unassigned'
                  AND provider = ?2
                  AND provider_session_id = ?3
           )
        "#,
        params![
            seat_id,
            provider,
            provider_session_id,
            artifact_path,
            observed_at
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatSessionIdentity {
    pub seat_id: String,
    pub provider: String,
    pub provider_session_id: String,
    pub artifact_path: Option<String>,
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

    #[test]
    fn append_batch_repairs_only_missing_artifact_paths() {
        let state_file = test_path("batch-artifact-repair");
        let db_path = state_file.with_extension("usage.db");
        let store = SeatSessionStore::for_state_file(&state_file);

        store
            .append("seat-1", "codex-fork", "missing-path", None)
            .unwrap();
        store
            .append(
                "seat-1",
                "codex-fork",
                "existing-path",
                Some("/tmp/original.jsonl"),
            )
            .unwrap();
        store
            .append_batch(&[
                SeatSessionIdentity {
                    seat_id: "seat-1".to_owned(),
                    provider: "codex-fork".to_owned(),
                    provider_session_id: "missing-path".to_owned(),
                    artifact_path: Some("/tmp/recovered.jsonl".to_owned()),
                },
                SeatSessionIdentity {
                    seat_id: "seat-1".to_owned(),
                    provider: "codex-fork".to_owned(),
                    provider_session_id: "existing-path".to_owned(),
                    artifact_path: Some("/tmp/replayed.jsonl".to_owned()),
                },
            ])
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
                ("existing-path".to_owned(), "/tmp/original.jsonl".to_owned(),),
                ("missing-path".to_owned(), "/tmp/recovered.jsonl".to_owned(),),
            ]
        );
    }

    #[test]
    fn definitive_binding_replaces_and_blocks_provisional_unassigned_rows() {
        let state_file = test_path("provisional-binding");
        let db_path = state_file.with_extension("usage.db");
        let store = SeatSessionStore::for_state_file(&state_file);
        let provisional = SeatSessionIdentity {
            seat_id: "unassigned".to_owned(),
            provider: "claude".to_owned(),
            provider_session_id: "provider-a".to_owned(),
            artifact_path: Some("/tmp/provisional.jsonl".to_owned()),
        };

        store.append_batch(&[provisional.clone()]).unwrap();
        store
            .append(
                "seat-1",
                "claude",
                "provider-a",
                Some("/tmp/definitive.jsonl"),
            )
            .unwrap();
        store.append_batch(&[provisional]).unwrap();

        let connection = Connection::open(&db_path).unwrap();
        let rows = connection
            .prepare(
                "SELECT seat_id, artifact_path FROM seat_sessions WHERE provider_session_id = 'provider-a'",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![("seat-1".to_owned(), "/tmp/definitive.jsonl".to_owned())]
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
