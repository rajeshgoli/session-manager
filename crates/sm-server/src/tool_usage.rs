use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

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
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}
