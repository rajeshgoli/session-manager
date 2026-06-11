use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, macros::format_description, OffsetDateTime};

static BUG_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct BugReportStore {
    db_path: PathBuf,
    max_reports: usize,
}

#[derive(Debug, Clone)]
pub struct CreateBugReport {
    pub report_text: String,
    pub reported_by: Option<String>,
    pub selected_session_id: Option<String>,
    pub route: Option<String>,
    pub app_version: Option<String>,
    pub artifact_hash: Option<String>,
    pub include_debug_state: bool,
    pub client_state: Option<Value>,
    pub server_state: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct CreatedBugReport {
    pub id: String,
}

impl BugReportStore {
    pub fn new(db_path: PathBuf, max_reports: usize) -> Self {
        Self {
            db_path,
            max_reports: max_reports.max(1),
        }
    }

    pub fn create_report(&self, report: CreateBugReport) -> Result<CreatedBugReport> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create bug report dir {}", parent.display()))?;
        }
        let conn = self.open()?;
        let id = bug_id();
        let created_at = now_rfc3339();
        let client_state_json = report
            .include_debug_state
            .then(|| report.client_state.as_ref().map(compact_json))
            .flatten()
            .transpose()?;
        let server_state_json = report
            .include_debug_state
            .then(|| report.server_state.as_ref().map(compact_json))
            .flatten()
            .transpose()?;

        conn.execute("BEGIN IMMEDIATE", [])?;
        let result = (|| -> Result<()> {
            conn.execute(
                r#"
                INSERT INTO bug_reports (
                    id, created_at, reported_by, report_text, selected_session_id,
                    route, app_version, artifact_hash, include_debug_state,
                    client_state_json, server_state_json, status,
                    maintainer_delivery_result
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'new', NULL)
                "#,
                params![
                    id,
                    created_at,
                    report.reported_by,
                    report.report_text,
                    report.selected_session_id,
                    report.route,
                    report.app_version,
                    report.artifact_hash,
                    if report.include_debug_state { 1 } else { 0 },
                    client_state_json,
                    server_state_json,
                ],
            )?;
            self.prune_locked(&conn)?;
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute("COMMIT", [])?,
            Err(error) => {
                let _ = conn.execute("ROLLBACK", []);
                return Err(error);
            }
        };

        Ok(CreatedBugReport { id })
    }

    pub fn update_delivery_result(&self, bug_id: &str, result: &str) -> Result<()> {
        let conn = self.open()?;
        conn.execute(
            "UPDATE bug_reports SET maintainer_delivery_result = ?, status = 'submitted' WHERE id = ?",
            params![result, bug_id],
        )?;
        Ok(())
    }

    pub fn report_exists(&self, bug_id: &str) -> Result<bool> {
        let conn = self.open()?;
        let value = conn
            .query_row("SELECT 1 FROM bug_reports WHERE id = ?", [bug_id], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?;
        Ok(value.is_some())
    }

    fn open(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open bug report DB {}", self.db_path.display()))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA busy_timeout=5000;
            PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS bug_reports (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                reported_by TEXT,
                report_text TEXT NOT NULL,
                selected_session_id TEXT,
                route TEXT,
                app_version TEXT,
                artifact_hash TEXT,
                include_debug_state INTEGER NOT NULL,
                client_state_json TEXT,
                server_state_json TEXT,
                status TEXT NOT NULL DEFAULT 'new',
                maintainer_delivery_result TEXT
            );
            CREATE TABLE IF NOT EXISTS bug_report_attachments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bug_report_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                mime_type TEXT NOT NULL,
                payload BLOB NOT NULL,
                FOREIGN KEY (bug_report_id) REFERENCES bug_reports(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_bug_reports_created_at ON bug_reports(created_at, id);
            CREATE INDEX IF NOT EXISTS idx_bug_reports_selected_session ON bug_reports(selected_session_id, created_at);
            "#,
        )?;
        Ok(conn)
    }

    fn prune_locked(&self, conn: &Connection) -> Result<()> {
        let total: i64 =
            conn.query_row("SELECT COUNT(*) FROM bug_reports", [], |row| row.get(0))?;
        let excess = total - self.max_reports as i64;
        if excess <= 0 {
            return Ok(());
        }
        let doomed = conn
            .prepare(
                r#"
                SELECT id
                FROM bug_reports
                ORDER BY created_at ASC, id ASC
                LIMIT ?
                "#,
            )?
            .query_map([excess], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for id in doomed {
            conn.execute(
                "DELETE FROM bug_report_attachments WHERE bug_report_id = ?",
                [&id],
            )?;
            conn.execute("DELETE FROM bug_reports WHERE id = ?", [&id])?;
        }
        Ok(())
    }
}

fn compact_json(value: &Value) -> Result<String> {
    serde_json::to_string(value).context("failed to serialize bug report JSON payload")
}

fn bug_id() -> String {
    let now = OffsetDateTime::now_utc();
    let date = now
        .format(format_description!(
            "[year][month][day]-[hour][minute][second]"
        ))
        .unwrap_or_else(|_| "19700101-000000".to_owned());
    let counter = BUG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let suffix = format!(
        "{:06x}",
        (std::process::id() as u64 ^ counter) & 0x00ff_ffff
    );
    format!("BR-{date}-{suffix}")
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

pub fn bug_report_db_path(path: impl AsRef<Path>) -> PathBuf {
    path.as_ref().to_path_buf()
}
