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

/// One press of the app's bug button (spec 1859 A2). The report is stored
/// before the issue is filed, so nothing typed is lost if GitHub refuses.
#[derive(Debug, Clone)]
pub struct CreateBugReport {
    pub report_text: String,
    pub reported_by: Option<String>,
    /// `web` or `android`.
    pub client: String,
    pub client_version: Option<String>,
    /// The page's human name, e.g. `Board`.
    pub page: String,
    pub route: Option<String>,
    pub page_data: Value,
    pub server_state: Value,
    pub screenshot_png: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct CreatedBugReport {
    pub id: String,
}

/// The issue a report was filed as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiledIssue {
    pub repo: String,
    pub number: i64,
    pub url: String,
}

/// A stored report as the owner page and `sm bug show` read it.
#[derive(Debug, Clone)]
pub struct StoredBugReport {
    pub id: String,
    pub created_at: String,
    pub reported_by: Option<String>,
    pub client: Option<String>,
    pub client_version: Option<String>,
    pub page: Option<String>,
    pub route: Option<String>,
    pub text: String,
    pub status: String,
    pub issue: Option<FiledIssue>,
    pub page_data: Value,
    pub server_facts: Value,
    pub has_screenshot: bool,
}

/// Columns the bug button added to the original table.
const ADDED_COLUMNS: [(&str, &str); 7] = [
    ("client", "TEXT"),
    ("client_version", "TEXT"),
    ("page", "TEXT"),
    ("page_data_json", "TEXT"),
    ("issue_repo", "TEXT"),
    ("issue_number", "INTEGER"),
    ("issue_url", "TEXT"),
];

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
        let page_data_json = compact_json(&report.page_data)?;
        let server_state_json = compact_json(&report.server_state)?;

        conn.execute("BEGIN IMMEDIATE", [])?;
        let result = (|| -> Result<()> {
            conn.execute(
                r#"
                INSERT INTO bug_reports (
                    id, created_at, reported_by, report_text, route,
                    include_debug_state, server_state_json, status,
                    client, client_version, page, page_data_json
                )
                VALUES (?, ?, ?, ?, ?, 1, ?, 'unfiled', ?, ?, ?, ?)
                "#,
                params![
                    id,
                    created_at,
                    report.reported_by,
                    report.report_text,
                    report.route,
                    server_state_json,
                    report.client,
                    report.client_version,
                    report.page,
                    page_data_json,
                ],
            )?;
            if let Some(png) = &report.screenshot_png {
                conn.execute(
                    "INSERT INTO bug_report_attachments (bug_report_id, kind, mime_type, payload)
                     VALUES (?, 'screenshot', 'image/png', ?)",
                    params![id, png],
                )?;
            }
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

    /// Records the issue the report was filed as.
    pub fn mark_filed(&self, bug_id: &str, issue: &FiledIssue) -> Result<()> {
        let conn = self.open()?;
        conn.execute(
            "UPDATE bug_reports
             SET status = 'filed', issue_repo = ?2, issue_number = ?3, issue_url = ?4
             WHERE id = ?1",
            params![bug_id, issue.repo, issue.number, issue.url],
        )?;
        Ok(())
    }

    pub fn report(&self, bug_id: &str) -> Result<Option<StoredBugReport>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = self.open()?;
        let report = conn
            .query_row(
                r#"
                SELECT id, created_at, reported_by, client, client_version, page, route,
                       report_text, status, issue_repo, issue_number, issue_url,
                       page_data_json, server_state_json,
                       EXISTS (SELECT 1 FROM bug_report_attachments
                               WHERE bug_report_id = bug_reports.id AND kind = 'screenshot')
                FROM bug_reports WHERE id = ?
                "#,
                [bug_id],
                |row| {
                    let issue = match (
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                    ) {
                        (Some(repo), Some(number), Some(url)) => {
                            Some(FiledIssue { repo, number, url })
                        }
                        _ => None,
                    };
                    let json = |text: Option<String>| {
                        text.and_then(|text| serde_json::from_str(&text).ok())
                            .unwrap_or(Value::Null)
                    };
                    Ok(StoredBugReport {
                        id: row.get(0)?,
                        created_at: row.get(1)?,
                        reported_by: row.get(2)?,
                        client: row.get(3)?,
                        client_version: row.get(4)?,
                        page: row.get(5)?,
                        route: row.get(6)?,
                        text: row.get(7)?,
                        status: row.get(8)?,
                        issue,
                        page_data: json(row.get(12)?),
                        server_facts: json(row.get(13)?),
                        has_screenshot: row.get(14)?,
                    })
                },
            )
            .optional()?;
        Ok(report)
    }

    pub fn screenshot(&self, bug_id: &str) -> Result<Option<Vec<u8>>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = self.open()?;
        let png = conn
            .query_row(
                "SELECT payload FROM bug_report_attachments
                 WHERE bug_report_id = ? AND kind = 'screenshot' ORDER BY id LIMIT 1",
                [bug_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(png)
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
        let existing = conn
            .prepare("SELECT name FROM pragma_table_info('bug_reports')")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for (column, kind) in ADDED_COLUMNS {
            if !existing.iter().any(|name| name == column) {
                conn.execute(
                    &format!("ALTER TABLE bug_reports ADD COLUMN {column} {kind}"),
                    [],
                )?;
            }
        }
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
