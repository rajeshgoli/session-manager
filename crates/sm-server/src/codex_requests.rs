use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexPendingRequestsResponse {
    pub requests: Vec<CodexPendingRequestRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexPendingRequestRow {
    pub request_id: String,
    pub session_id: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub request_type: String,
    pub request_method: String,
    pub status: String,
    pub requested_at: String,
    pub expires_at: Option<String>,
    pub resolved_payload: Option<Value>,
    pub resolved_at: Option<String>,
    pub resolution_source: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

pub fn list_codex_pending_requests_from_path(
    db_path: &Path,
    session_id: &str,
    include_orphaned: bool,
) -> Result<CodexPendingRequestsResponse> {
    if !db_path.exists() {
        return Ok(CodexPendingRequestsResponse {
            requests: Vec::new(),
        });
    }
    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(_) => {
            return Ok(CodexPendingRequestsResponse {
                requests: Vec::new(),
            })
        }
    };
    list_codex_pending_requests_conn(&conn, session_id, include_orphaned)
}

fn list_codex_pending_requests_conn(
    conn: &Connection,
    session_id: &str,
    include_orphaned: bool,
) -> Result<CodexPendingRequestsResponse> {
    let status_clause = if include_orphaned {
        "status IN ('pending', 'orphaned')"
    } else {
        "status = 'pending'"
    };
    let query = format!(
        r#"
        SELECT request_id, session_id, thread_id, turn_id, item_id,
               request_type, request_method, status, requested_at, expires_at,
               resolved_payload_json, resolved_at, resolution_source, error_code, error_message
        FROM codex_pending_requests
        WHERE session_id = ?1 AND {status_clause}
        ORDER BY requested_at ASC
        "#
    );
    let mut statement = match conn.prepare(&query) {
        Ok(statement) => statement,
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            return Ok(CodexPendingRequestsResponse {
                requests: Vec::new(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    let rows = statement.query_map(params![session_id], codex_pending_request_row_from_sql)?;
    Ok(CodexPendingRequestsResponse {
        requests: rows.collect::<std::result::Result<Vec<_>, _>>()?,
    })
}

fn codex_pending_request_row_from_sql(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<CodexPendingRequestRow> {
    let resolved_payload_json: Option<String> = row.get(10)?;
    let resolved_payload = match resolved_payload_json {
        Some(value) => Some(serde_json::from_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?),
        None => None,
    };
    Ok(CodexPendingRequestRow {
        request_id: row.get(0)?,
        session_id: row.get(1)?,
        thread_id: row.get(2)?,
        turn_id: row.get(3)?,
        item_id: row.get(4)?,
        request_type: row.get(5)?,
        request_method: row.get(6)?,
        status: row.get(7)?,
        requested_at: row.get(8)?,
        expires_at: row.get(9)?,
        resolved_payload,
        resolved_at: row.get(11)?,
        resolution_source: row.get(12)?,
        error_code: row.get(13)?,
        error_message: row.get(14)?,
    })
}
