use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolCallRow {
    pub timestamp: Option<String>,
    pub tool_name: String,
    pub hook_type: String,
}

pub fn list_recent_tool_calls_from_path(
    db_path: &Path,
    session_id: &str,
    limit: usize,
) -> Result<Vec<ToolCallRow>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(_) => return Ok(Vec::new()),
    };
    list_recent_tool_calls_conn(&conn, session_id, limit)
}

fn list_recent_tool_calls_conn(
    conn: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<Vec<ToolCallRow>> {
    let mut statement = match conn.prepare(
        r#"
        SELECT timestamp, tool_name, hook_type
        FROM tool_usage
        WHERE session_id = ?1 AND hook_type = 'PreToolUse'
        ORDER BY id DESC
        LIMIT ?2
        "#,
    ) {
        Ok(statement) => statement,
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            return Ok(Vec::new());
        }
        Err(_) => return Ok(Vec::new()),
    };
    let rows = match statement.query_map((session_id, limit as i64), |row| {
        Ok(ToolCallRow {
            timestamp: row.get(0)?,
            tool_name: row.get(1)?,
            hook_type: row.get(2)?,
        })
    }) {
        Ok(rows) => rows,
        Err(_) => return Ok(Vec::new()),
    };
    Ok(rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap_or_else(|_| Vec::new()))
}

pub fn list_recent_codex_fork_tool_calls_from_path(
    db_path: &Path,
    session_id: &str,
    limit: usize,
) -> Result<Vec<ToolCallRow>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(_) => return Ok(Vec::new()),
    };
    list_recent_codex_fork_tool_calls_conn(&conn, session_id, limit)
}

fn list_recent_codex_fork_tool_calls_conn(
    conn: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<Vec<ToolCallRow>> {
    let scan_limit = limit.saturating_mul(4).min(500);
    let mut statement = match conn.prepare(
        r#"
        SELECT created_at, raw_payload_json
        FROM codex_tool_events
        WHERE session_id = ?1
        ORDER BY id DESC
        LIMIT ?2
        "#,
    ) {
        Ok(statement) => statement,
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            return Ok(Vec::new());
        }
        Err(_) => return Ok(Vec::new()),
    };
    let rows = match statement.query_map((session_id, scan_limit as i64), |row| {
        let timestamp: Option<String> = row.get(0)?;
        let raw_payload_json: Option<String> = row.get(1)?;
        Ok((timestamp, raw_payload_json))
    }) {
        Ok(rows) => rows,
        Err(_) => return Ok(Vec::new()),
    };

    let mut selected = Vec::new();
    for row in rows {
        let Ok((timestamp, raw_payload_json)) = row else {
            return Ok(Vec::new());
        };
        let Some(raw_payload_json) = raw_payload_json else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<Value>(&raw_payload_json) else {
            continue;
        };
        let Some(tool_name) = payload
            .as_object()
            .and_then(|payload| payload.get("tool_name"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        selected.push(ToolCallRow {
            timestamp,
            tool_name: tool_name.to_owned(),
            hook_type: "CodexForkToolCall".to_owned(),
        });
        if selected.len() >= limit {
            break;
        }
    }

    selected.reverse();
    Ok(selected)
}
