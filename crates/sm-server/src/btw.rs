use std::path::PathBuf;

use anyhow::{Context, Result};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub const DEFAULT_BTW_PROMPT: &str = "Summarize what you have done so far in the main conversation thread. Do not summarize this /btw side conversation.";
pub const MAX_BTW_PROMPT_BYTES: usize = 4 * 1024;
pub const MAX_BTW_RESULT_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BtwRequestRecord {
    pub request_id: String,
    pub requester_session_id: Option<String>,
    pub target_session_id: String,
    pub target_provider: String,
    pub delivery_mode: String,
    pub prompt: String,
    pub status: String,
    pub provider_correlation: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub response_delivered_at: Option<String>,
    pub response_undeliverable_at: Option<String>,
}

#[derive(Debug)]
pub enum CreateBtwRequestError {
    Active(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CreateBtwRequestError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

#[derive(Debug, Clone)]
pub struct BtwStore {
    db_path: PathBuf,
}

impl BtwStore {
    pub fn new(db_path: PathBuf) -> Result<Self> {
        let store = Self { db_path };
        let _ = store.open()?;
        Ok(store)
    }

    pub fn create(
        &self,
        requester_session_id: Option<&str>,
        target_session_id: &str,
        target_provider: &str,
        delivery_mode: &str,
        prompt: &str,
    ) -> std::result::Result<BtwRequestRecord, CreateBtwRequestError> {
        let mut conn = self.open().map_err(CreateBtwRequestError::Other)?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(anyhow::Error::from)
            .map_err(CreateBtwRequestError::Other)?;
        let active = transaction
            .query_row(
                r#"
                SELECT request_id
                FROM btw_requests
                WHERE target_session_id = ?1
                  AND status IN ('pending', 'running')
                ORDER BY created_at ASC
                LIMIT 1
                "#,
                [target_session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(anyhow::Error::from)
            .map_err(CreateBtwRequestError::Other)?;
        if let Some(request_id) = active {
            return Err(CreateBtwRequestError::Active(request_id));
        }

        let request_id = new_request_id();
        let created_at = now_rfc3339();
        transaction
            .execute(
                r#"
                INSERT INTO btw_requests (
                    request_id,
                    requester_session_id,
                    target_session_id,
                    target_provider,
                    delivery_mode,
                    prompt,
                    status,
                    created_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)
                "#,
                params![
                    request_id,
                    requester_session_id,
                    target_session_id,
                    target_provider,
                    delivery_mode,
                    prompt,
                    created_at,
                ],
            )
            .map_err(anyhow::Error::from)
            .map_err(CreateBtwRequestError::Other)?;
        transaction
            .commit()
            .map_err(anyhow::Error::from)
            .map_err(CreateBtwRequestError::Other)?;
        self.get(&request_id)
            .map_err(CreateBtwRequestError::Other)?
            .ok_or_else(|| {
                CreateBtwRequestError::Other(anyhow::anyhow!("created btw request disappeared"))
            })
    }

    pub fn get(&self, request_id: &str) -> Result<Option<BtwRequestRecord>> {
        self.with_connection(|conn| {
            conn.query_row(
                r#"
                SELECT
                    request_id,
                    requester_session_id,
                    target_session_id,
                    target_provider,
                    delivery_mode,
                    prompt,
                    status,
                    provider_correlation,
                    created_at,
                    started_at,
                    finished_at,
                    result,
                    error,
                    response_delivered_at,
                    response_undeliverable_at
                FROM btw_requests
                WHERE request_id = ?1
                "#,
                [request_id],
                record_from_row,
            )
            .optional()
            .map_err(anyhow::Error::from)
        })
    }

    pub fn active_for_target(&self, target_session_id: &str) -> Result<Option<BtwRequestRecord>> {
        self.with_connection(|conn| {
            conn.query_row(
                r#"
                SELECT
                    request_id,
                    requester_session_id,
                    target_session_id,
                    target_provider,
                    delivery_mode,
                    prompt,
                    status,
                    provider_correlation,
                    created_at,
                    started_at,
                    finished_at,
                    result,
                    error,
                    response_delivered_at,
                    response_undeliverable_at
                FROM btw_requests
                WHERE target_session_id = ?1
                  AND status IN ('pending', 'running')
                ORDER BY created_at ASC
                LIMIT 1
                "#,
                [target_session_id],
                record_from_row,
            )
            .optional()
            .map_err(Into::into)
        })
    }

    pub fn list_recoverable(&self) -> Result<Vec<BtwRequestRecord>> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                r#"
                SELECT
                    request_id,
                    requester_session_id,
                    target_session_id,
                    target_provider,
                    delivery_mode,
                    prompt,
                    status,
                    provider_correlation,
                    created_at,
                    started_at,
                    finished_at,
                    result,
                    error,
                    response_delivered_at,
                    response_undeliverable_at
                FROM btw_requests
                WHERE status IN ('pending', 'running')
                   OR (
                       delivery_mode = 'session'
                       AND response_delivered_at IS NULL
                       AND response_undeliverable_at IS NULL
                       AND status IN ('completed', 'failed', 'timed_out')
                   )
                ORDER BY created_at ASC
                "#,
            )?;
            let records = statement
                .query_map([], record_from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(records)
        })
    }

    pub fn mark_running(&self, request_id: &str, provider_correlation: Option<&str>) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                r#"
                UPDATE btw_requests
                SET status = 'running',
                    provider_correlation = ?2,
                    started_at = ?3
                WHERE request_id = ?1 AND status = 'pending'
                "#,
                params![request_id, provider_correlation, now_rfc3339()],
            )?;
            Ok(())
        })
    }

    pub fn set_provider_correlation(&self, request_id: &str, correlation: &str) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                r#"
                UPDATE btw_requests
                SET provider_correlation = ?2
                WHERE request_id = ?1 AND status = 'running'
                "#,
                params![request_id, correlation],
            )?;
            Ok(())
        })
    }

    pub fn complete(&self, request_id: &str, result: &str) -> Result<()> {
        let result = bound_utf8(result.trim(), MAX_BTW_RESULT_BYTES);
        self.finish(request_id, "completed", Some(&result), None)
    }

    pub fn fail(&self, request_id: &str, error: &str) -> Result<()> {
        let error = bound_utf8(error.trim(), MAX_BTW_RESULT_BYTES);
        self.finish(request_id, "failed", None, Some(&error))
    }

    pub fn time_out(&self, request_id: &str, error: &str) -> Result<()> {
        let error = bound_utf8(error.trim(), MAX_BTW_RESULT_BYTES);
        self.finish(request_id, "timed_out", None, Some(&error))
    }

    pub fn mark_response_delivered(&self, request_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                r#"
                UPDATE btw_requests
                SET response_delivered_at = ?2
                WHERE request_id = ?1 AND response_delivered_at IS NULL
                "#,
                params![request_id, now_rfc3339()],
            )?;
            Ok(())
        })
    }

    pub fn mark_response_undeliverable(&self, request_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                r#"
                UPDATE btw_requests
                SET response_undeliverable_at = ?2
                WHERE request_id = ?1
                  AND response_delivered_at IS NULL
                  AND response_undeliverable_at IS NULL
                "#,
                params![request_id, now_rfc3339()],
            )?;
            Ok(())
        })
    }

    pub fn fail_for_session(&self, session_id: &str, error: &str) -> Result<Vec<BtwRequestRecord>> {
        let error = bound_utf8(error.trim(), MAX_BTW_RESULT_BYTES);
        let mut conn = self.open()?;
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let request_ids = {
            let mut statement = transaction.prepare(
                r#"
                SELECT request_id
                FROM btw_requests
                WHERE status IN ('pending', 'running')
                  AND (
                      target_session_id = ?1
                      OR requester_session_id = ?1
                  )
                ORDER BY created_at ASC
                "#,
            )?;
            let request_ids = statement
                .query_map([session_id], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            request_ids
        };
        let now = now_rfc3339();
        transaction.execute(
            r#"
            UPDATE btw_requests
            SET status = 'failed',
                result = NULL,
                error = ?2,
                finished_at = ?3,
                response_undeliverable_at = CASE
                    WHEN requester_session_id = ?1 THEN ?3
                    ELSE response_undeliverable_at
                END
            WHERE status IN ('pending', 'running')
              AND (
                  target_session_id = ?1
                  OR requester_session_id = ?1
              )
            "#,
            params![session_id, error, now],
        )?;
        transaction.commit()?;
        request_ids
            .into_iter()
            .filter_map(|request_id| self.get(&request_id).transpose())
            .collect()
    }

    fn finish(
        &self,
        request_id: &str,
        status: &str,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        self.with_connection(|conn| {
            conn.execute(
                r#"
                UPDATE btw_requests
                SET status = ?2,
                    result = ?3,
                    error = ?4,
                    finished_at = ?5
                WHERE request_id = ?1 AND status IN ('pending', 'running')
                "#,
                params![request_id, status, result, error, now_rfc3339()],
            )?;
            Ok(())
        })
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open {}", self.db_path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        init_schema(&conn)?;
        Ok(conn)
    }

    fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.open()?;
        f(&conn)
    }
}

pub fn validate_prompt(prompt: Option<&str>) -> Result<String> {
    let prompt = prompt.unwrap_or(DEFAULT_BTW_PROMPT).trim();
    let prompt = if prompt.is_empty() {
        DEFAULT_BTW_PROMPT
    } else {
        prompt
    };
    if prompt.len() > MAX_BTW_PROMPT_BYTES {
        anyhow::bail!("prompt exceeds {MAX_BTW_PROMPT_BYTES} UTF-8 bytes");
    }
    if prompt
        .chars()
        .any(|ch| ch == '\n' || ch == '\r' || ch == '\0' || ch.is_control())
    {
        anyhow::bail!("prompt must be a single line without control characters");
    }
    Ok(prompt.to_owned())
}

pub fn codex_btw_event(line: &str, request_id: &str) -> Option<Result<String, String>> {
    let event: Value = serde_json::from_str(line.trim()).ok()?;
    let event_type = event.get("event_type")?.as_str()?;
    let payload = event.get("payload")?.as_object()?;
    if payload.get("request_id")?.as_str()? != request_id {
        return None;
    }
    match event_type {
        "btw_completed" => Some(Ok(payload
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())),
        "btw_failed" => Some(Err(payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("provider_failed")
            .to_owned())),
        _ => None,
    }
}

pub fn extract_stock_codex_answer(snapshot: &str, prompt: &str) -> Option<String> {
    let lines = snapshot.lines().map(str::trim).collect::<Vec<_>>();
    let prompt_index = stock_codex_prompt_end(&lines, prompt)?;
    let mut answer = Vec::new();
    for line in lines.into_iter().skip(prompt_index + 1) {
        if line.contains("Side from main thread")
            || line.contains("Ctrl+C to return")
            || line.starts_with("tokens used")
            || line.starts_with("context left")
        {
            continue;
        }
        let cleaned = line
            .trim_matches(|ch: char| matches!(ch, '│' | '╭' | '╮' | '╰' | '╯' | '─'))
            .trim();
        if cleaned.is_empty()
            || cleaned.starts_with('›')
            || cleaned.starts_with('>')
            || cleaned.starts_with("esc ")
        {
            continue;
        }
        answer.push(cleaned);
    }
    let answer = answer.join("\n").trim().to_owned();
    (!answer.is_empty()).then_some(answer)
}

fn stock_codex_prompt_end(lines: &[&str], prompt: &str) -> Option<usize> {
    if let Some(index) = lines
        .iter()
        .rposition(|line| *line == prompt || line.ends_with(prompt))
    {
        return Some(index);
    }
    let prompt = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    for start in (0..lines.len()).rev() {
        let mut candidate = String::new();
        for (index, line) in lines.iter().enumerate().skip(start) {
            let part = line
                .trim_matches(|ch: char| matches!(ch, '│' | '╭' | '╮' | '╰' | '╯' | '─'))
                .trim()
                .trim_start_matches(['›', '>'])
                .trim();
            if part.is_empty() {
                continue;
            }
            if !candidate.is_empty() {
                candidate.push(' ');
            }
            candidate.push_str(part);
            let normalized = candidate.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized == prompt {
                return Some(index);
            }
            if !prompt.starts_with(&normalized) {
                break;
            }
        }
    }
    None
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS btw_requests (
            request_id TEXT PRIMARY KEY,
            requester_session_id TEXT,
            target_session_id TEXT NOT NULL,
            target_provider TEXT NOT NULL,
            delivery_mode TEXT NOT NULL,
            prompt TEXT NOT NULL,
            status TEXT NOT NULL,
            provider_correlation TEXT,
            created_at TEXT NOT NULL,
            started_at TEXT,
            finished_at TEXT,
            result TEXT,
            error TEXT,
            response_delivered_at TEXT,
            response_undeliverable_at TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_btw_one_active_per_target
            ON btw_requests(target_session_id)
            WHERE status IN ('pending', 'running');
        CREATE INDEX IF NOT EXISTS idx_btw_created_at
            ON btw_requests(created_at);
        "#,
    )?;
    ensure_column(conn, "btw_requests", "response_undeliverable_at", "TEXT")?;
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == column);
    if !exists {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BtwRequestRecord> {
    Ok(BtwRequestRecord {
        request_id: row.get(0)?,
        requester_session_id: row.get(1)?,
        target_session_id: row.get(2)?,
        target_provider: row.get(3)?,
        delivery_mode: row.get(4)?,
        prompt: row.get(5)?,
        status: row.get(6)?,
        provider_correlation: row.get(7)?,
        created_at: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
        result: row.get(11)?,
        error: row.get(12)?,
        response_delivered_at: row.get(13)?,
        response_undeliverable_at: row.get(14)?,
    })
}

fn new_request_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let mut value = String::with_capacity(36);
    value.push_str("btw-");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn bound_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (BtwStore, PathBuf) {
        let path = std::env::temp_dir().join(format!("sm-btw-test-{}.db", new_request_id()));
        (BtwStore::new(path.clone()).unwrap(), path)
    }

    #[test]
    fn prompt_validation_applies_default_and_rejects_multiline() {
        assert_eq!(validate_prompt(None).unwrap(), DEFAULT_BTW_PROMPT);
        assert!(DEFAULT_BTW_PROMPT.contains("main conversation thread"));
        assert!(DEFAULT_BTW_PROMPT.contains("Do not summarize this /btw side conversation"));
        assert_eq!(
            validate_prompt(Some("Summarize the current blocker")).unwrap(),
            "Summarize the current blocker"
        );
        assert!(validate_prompt(Some("one\ntwo")).is_err());
        assert!(validate_prompt(Some(&"x".repeat(MAX_BTW_PROMPT_BYTES + 1))).is_err());
    }

    #[test]
    fn store_enforces_one_active_request_per_target() {
        let (store, path) = test_store();
        let first = store
            .create(None, "target-1", "codex-fork", "poll", "summary")
            .unwrap();
        assert!(matches!(
            store.create(None, "target-1", "codex-fork", "poll", "again"),
            Err(CreateBtwRequestError::Active(request_id)) if request_id == first.request_id
        ));
        store.complete(&first.request_id, "done").unwrap();
        assert!(store
            .create(None, "target-1", "codex-fork", "poll", "again")
            .is_ok());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recovery_includes_active_and_undelivered_session_requests() {
        let (store, path) = test_store();
        let active = store
            .create(None, "target-active", "codex-fork", "poll", "summary")
            .unwrap();
        let session = store
            .create(
                Some("requester"),
                "target-session",
                "codex-fork",
                "session",
                "summary",
            )
            .unwrap();
        store.complete(&session.request_id, "done").unwrap();
        let undeliverable = store
            .create(
                Some("retired-requester"),
                "target-undeliverable",
                "codex-fork",
                "session",
                "summary",
            )
            .unwrap();
        store.complete(&undeliverable.request_id, "done").unwrap();
        let poll = store
            .create(None, "target-poll", "codex-fork", "poll", "summary")
            .unwrap();
        store.complete(&poll.request_id, "done").unwrap();

        let recoverable = store
            .list_recoverable()
            .unwrap()
            .into_iter()
            .map(|request| request.request_id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            recoverable,
            std::collections::BTreeSet::from([
                active.request_id.clone(),
                session.request_id.clone(),
                undeliverable.request_id.clone(),
            ])
        );

        store.mark_response_delivered(&session.request_id).unwrap();
        store
            .mark_response_undeliverable(&undeliverable.request_id)
            .unwrap();
        let recoverable = store.list_recoverable().unwrap();
        assert_eq!(recoverable.len(), 1);
        assert_eq!(recoverable[0].request_id, active.request_id);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn session_teardown_fails_related_requests_and_suppresses_retired_requester_delivery() {
        let (store, path) = test_store();
        let requested = store
            .create(
                Some("retired"),
                "target-a",
                "codex-fork",
                "session",
                "summary",
            )
            .unwrap();
        let targeted = store
            .create(
                Some("requester-b"),
                "retired",
                "codex-fork",
                "session",
                "summary",
            )
            .unwrap();

        let affected = store
            .fail_for_session("retired", "session retired")
            .unwrap();
        assert_eq!(affected.len(), 2);
        let requested = store.get(&requested.request_id).unwrap().unwrap();
        assert_eq!(requested.status, "failed");
        assert!(requested.response_undeliverable_at.is_some());
        let targeted = store.get(&targeted.request_id).unwrap().unwrap();
        assert_eq!(targeted.status, "failed");
        assert!(targeted.response_undeliverable_at.is_none());
        assert!(store.list_recoverable().unwrap().contains(&targeted));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parses_only_correlated_codex_events() {
        let completed =
            r#"{"event_type":"btw_completed","payload":{"request_id":"req-1","result":"done"}}"#;
        assert_eq!(
            codex_btw_event(completed, "req-1"),
            Some(Ok("done".to_owned()))
        );
        assert_eq!(codex_btw_event(completed, "req-2"), None);
    }

    #[test]
    fn extracts_stock_codex_answer_without_side_chrome() {
        let snapshot = "\
Question\n\
Summarize now\n\
The implementation is complete.\n\
Tests are passing.\n\
Side from main thread · Ctrl+C to return\n";
        assert_eq!(
            extract_stock_codex_answer(snapshot, "Summarize now").as_deref(),
            Some("The implementation is complete.\nTests are passing.")
        );
    }

    #[test]
    fn extracts_stock_codex_answer_after_wrapped_prompt() {
        let snapshot = "\
› Summarize the implementation and\n\
  current verification status\n\
The implementation is complete.\n\
Side from main thread · Ctrl+C to return\n";
        assert_eq!(
            extract_stock_codex_answer(
                snapshot,
                "Summarize the implementation and current verification status",
            )
            .as_deref(),
            Some("The implementation is complete.")
        );
    }
}
