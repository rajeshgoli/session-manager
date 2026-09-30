use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolCallRow {
    pub timestamp: Option<String>,
    pub tool_name: String,
    pub hook_type: String,
}

#[derive(Debug, Clone)]
pub struct ToolUsageEvent<'a> {
    pub session_id: Option<&'a str>,
    pub claude_session_id: Option<&'a str>,
    pub session_name: Option<&'a str>,
    pub parent_session_id: Option<&'a str>,
    pub hook_type: &'a str,
    pub tool_name: &'a str,
    pub tool_input: Option<&'a Value>,
    pub tool_response: Option<&'a Value>,
    pub tool_use_id: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub agent_id: Option<&'a str>,
}

pub fn log_tool_usage_to_path(db_path: &Path, event: ToolUsageEvent<'_>) -> Result<()> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create tool usage db directory {}",
                parent.display()
            )
        })?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open tool usage db {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000_i64)?;
    ensure_tool_usage_schema(&conn)?;

    let (is_destructive, destructive_type) = detect_destructive(event.tool_name, event.tool_input);
    let (is_sensitive_file, mut target_file) =
        detect_sensitive_file(event.tool_name, event.tool_input);
    let bash_command = json_object_string(event.tool_input, "command");
    let exit_code = json_object_i64(event.tool_response, "exitCode");
    if target_file.is_none() && matches!(event.tool_name, "Write" | "Edit" | "Read") {
        target_file = json_object_string(event.tool_input, "file_path");
    }
    let project_name = event.cwd.and_then(|cwd| {
        Path::new(cwd)
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
    });
    let tool_input_json = non_empty_json_payload(event.tool_input);
    let tool_response_json = non_empty_json_payload(event.tool_response);

    conn.execute(
        r#"
        INSERT INTO tool_usage (
            session_id, claude_session_id, session_name, parent_session_id,
            tool_use_id, cwd, project_name, agent_id,
            hook_type, tool_name, tool_input, tool_response,
            is_destructive, destructive_type, is_sensitive_file,
            target_file, bash_command, exit_code
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
        "#,
        params![
            event.session_id,
            event.claude_session_id,
            event.session_name,
            event.parent_session_id,
            event.tool_use_id,
            event.cwd,
            project_name,
            event.agent_id,
            event.hook_type,
            event.tool_name,
            tool_input_json,
            tool_response_json,
            is_destructive,
            destructive_type,
            is_sensitive_file,
            target_file,
            bash_command,
            exit_code,
        ],
    )?;
    Ok(())
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

fn ensure_tool_usage_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS tool_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
            session_id TEXT,
            session_name TEXT,
            parent_session_id TEXT,
            claude_session_id TEXT,
            tool_use_id TEXT,
            cwd TEXT,
            project_name TEXT,
            agent_id TEXT,
            hook_type TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            tool_input TEXT,
            tool_response TEXT,
            is_destructive BOOLEAN DEFAULT 0,
            destructive_type TEXT,
            is_sensitive_file BOOLEAN DEFAULT 0,
            target_file TEXT,
            bash_command TEXT,
            exit_code INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_session ON tool_usage(session_id);
        CREATE INDEX IF NOT EXISTS idx_tool ON tool_usage(tool_name);
        CREATE INDEX IF NOT EXISTS idx_destructive ON tool_usage(is_destructive);
        CREATE INDEX IF NOT EXISTS idx_timestamp ON tool_usage(timestamp);
        CREATE INDEX IF NOT EXISTS idx_hook_type ON tool_usage(hook_type);
        CREATE INDEX IF NOT EXISTS idx_tool_use_id ON tool_usage(tool_use_id);
        CREATE INDEX IF NOT EXISTS idx_agent_id ON tool_usage(agent_id);
        CREATE INDEX IF NOT EXISTS idx_project_name ON tool_usage(project_name);
        CREATE TABLE IF NOT EXISTS telegram_telemetry (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
            direction TEXT NOT NULL CHECK(direction IN ('in', 'out')),
            session_id TEXT,
            chat_id TEXT,
            result TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_tg_timestamp ON telegram_telemetry(timestamp);
        "#,
    )?;
    Ok(())
}

fn non_empty_json_payload(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Object(map)) if map.is_empty() => None,
        Some(Value::Null) | None => None,
        Some(value) => serde_json::to_string(value).ok(),
    }
}

fn json_object_string(value: Option<&Value>, key: &str) -> Option<String> {
    value
        .and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn json_object_i64(value: Option<&Value>, key: &str) -> Option<i64> {
    value
        .and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .and_then(Value::as_i64)
}

fn detect_destructive(tool_name: &str, tool_input: Option<&Value>) -> (bool, Option<String>) {
    let text = if tool_name == "Bash" {
        json_object_string(tool_input, "command")
    } else if matches!(tool_name, "Write" | "Edit" | "Read") {
        json_object_string(tool_input, "file_path")
    } else {
        None
    }
    .unwrap_or_default();
    let text = text.to_ascii_lowercase();
    let destructive_type = if text.contains("git push") && text.contains("main") {
        Some("git_push_main")
    } else if text.contains("git push") && text.contains("--force") {
        Some("git_push_force")
    } else if text.contains("git reset --hard") {
        Some("git_reset_hard")
    } else if text.contains("git branch -d") {
        Some("git_branch_delete")
    } else if text.contains("rm -rf /") {
        Some("rm_root")
    } else if text.contains("rm -rf") || text.contains("rm -r") {
        Some("rm_recursive")
    } else if text.contains("chmod 777") {
        Some("chmod_777")
    } else if text.contains("chown") {
        Some("chown")
    } else if text.contains("drop table") {
        Some("drop_table")
    } else if text.contains("drop database") {
        Some("drop_database")
    } else if text.contains("truncate") {
        Some("truncate")
    } else if text.contains("npm install -g") {
        Some("npm_global_install")
    } else if text.contains("pip install") {
        Some("pip_install")
    } else if text.contains("brew install") {
        Some("brew_install")
    } else if text.contains("sudo ") {
        Some("sudo")
    } else if text.contains(".env") {
        Some("env_file")
    } else if text.contains("credentials") {
        Some("credentials_file")
    } else if text.contains(".ssh") {
        Some("ssh_file")
    } else if text.contains("id_rsa") {
        Some("ssh_key")
    } else {
        None
    };
    match destructive_type {
        Some(value) => (true, Some(value.to_owned())),
        None => (false, None),
    }
}

fn detect_sensitive_file(tool_name: &str, tool_input: Option<&Value>) -> (bool, Option<String>) {
    let file_path = if matches!(tool_name, "Write" | "Edit" | "Read") {
        json_object_string(tool_input, "file_path")
    } else if tool_name == "Bash" {
        json_object_string(tool_input, "command")
    } else {
        None
    };
    let Some(file_path) = file_path else {
        return (false, None);
    };
    let lower = file_path.to_ascii_lowercase();
    let is_sensitive = [
        ".env",
        "credentials",
        "secrets",
        ".ssh/",
        "id_rsa",
        ".aws/",
        ".npmrc",
        ".pypirc",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    (is_sensitive, is_sensitive.then_some(file_path))
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
