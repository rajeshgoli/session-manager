use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexActivityActionsResponse {
    pub actions: Vec<CodexActivityAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexActivityAction {
    pub source_provider: String,
    pub action_kind: String,
    pub summary_text: String,
    pub status: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
}

#[derive(Debug, Clone)]
struct CodexToolEventRow {
    session_id: String,
    turn_id: Option<String>,
    item_id: Option<String>,
    event_type: String,
    item_type: Option<String>,
    command: Option<String>,
    file_path: Option<String>,
    approval_decision: Option<String>,
    latency_ms: Option<i64>,
    final_status: Option<String>,
    error_message: Option<String>,
    created_at: Option<String>,
}

pub fn list_codex_activity_actions_from_path(
    db_path: &Path,
    session_id: &str,
    limit: usize,
) -> Result<CodexActivityActionsResponse> {
    if !db_path.exists() {
        return Ok(CodexActivityActionsResponse {
            actions: Vec::new(),
        });
    }
    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(_) => {
            return Ok(CodexActivityActionsResponse {
                actions: Vec::new(),
            })
        }
    };
    list_codex_activity_actions_conn(&conn, session_id, limit)
}

fn list_codex_activity_actions_conn(
    conn: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<CodexActivityActionsResponse> {
    let limit = limit.clamp(1, 200);
    let mut statement = match conn.prepare(
        r#"
        SELECT session_id, turn_id, item_id, event_type, item_type,
               command, file_path, approval_decision, latency_ms, final_status,
               error_message, created_at
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
            return Ok(CodexActivityActionsResponse {
                actions: Vec::new(),
            });
        }
        Err(_) => {
            return Ok(CodexActivityActionsResponse {
                actions: Vec::new(),
            })
        }
    };
    let rows = match statement.query_map(params![session_id, limit as i64], |row| {
        Ok(CodexToolEventRow {
            session_id: row.get(0)?,
            turn_id: row.get(1)?,
            item_id: row.get(2)?,
            event_type: row.get(3)?,
            item_type: row.get(4)?,
            command: row.get(5)?,
            file_path: row.get(6)?,
            approval_decision: row.get(7)?,
            latency_ms: row.get(8)?,
            final_status: row.get(9)?,
            error_message: row.get(10)?,
            created_at: row.get(11)?,
        })
    }) {
        Ok(rows) => rows,
        Err(_) => {
            return Ok(CodexActivityActionsResponse {
                actions: Vec::new(),
            })
        }
    };
    let mut rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    rows.reverse();
    Ok(CodexActivityActionsResponse {
        actions: rows.iter().map(project_row).collect(),
    })
}

fn project_row(row: &CodexToolEventRow) -> CodexActivityAction {
    CodexActivityAction {
        source_provider: "codex-app".to_owned(),
        action_kind: action_kind(&row.event_type, row.item_type.as_deref()).to_owned(),
        summary_text: summary_text(row),
        status: status(&row.event_type, row.final_status.as_deref()),
        started_at: derive_started_at(row.created_at.as_deref(), row.latency_ms),
        ended_at: if is_terminal_event(&row.event_type) {
            row.created_at.clone()
        } else {
            None
        },
        session_id: row.session_id.clone(),
        turn_id: row.turn_id.clone(),
        item_id: row.item_id.clone(),
    }
}

fn derive_started_at(created_at: Option<&str>, latency_ms: Option<i64>) -> Option<String> {
    let created_at = created_at?;
    let Some(latency_ms) = latency_ms.filter(|value| *value != 0) else {
        return Some(created_at.to_owned());
    };
    let Ok(ended_at) = OffsetDateTime::parse(created_at, &Rfc3339) else {
        return Some(created_at.to_owned());
    };
    Some(format_python_datetime(
        ended_at - Duration::milliseconds(latency_ms),
    ))
}

fn format_python_datetime(value: OffsetDateTime) -> String {
    let offset_seconds = value.offset().whole_seconds();
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let offset_abs = offset_seconds.unsigned_abs();
    let offset_hours = offset_abs / 3600;
    let offset_minutes = (offset_abs % 3600) / 60;
    let base = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    );
    let fraction = if value.nanosecond() == 0 {
        String::new()
    } else {
        format!(".{:06}", value.nanosecond() / 1_000)
    };
    format!("{base}{fraction}{sign}{offset_hours:02}:{offset_minutes:02}")
}

fn action_kind(event_type: &str, item_type: Option<&str>) -> &'static str {
    if matches!(event_type, "request_approval" | "approval_decision") {
        return "approval";
    }
    if matches!(event_type, "request_user_input" | "user_input_submitted") {
        return "user_input";
    }
    match item_type {
        Some("commandExecution") => "command",
        Some("fileChange") => "file_change",
        _ => "tool",
    }
}

fn status(event_type: &str, final_status: Option<&str>) -> String {
    if matches!(event_type, "request_approval" | "request_user_input") {
        return "pending".to_owned();
    }
    if matches!(
        event_type,
        "completed" | "failed" | "interrupted" | "cancelled" | "timeout"
    ) {
        return final_status.unwrap_or(event_type).to_owned();
    }
    if matches!(event_type, "approval_decision" | "user_input_submitted") {
        return "completed".to_owned();
    }
    "running".to_owned()
}

fn summary_text(row: &CodexToolEventRow) -> String {
    let item_type = row.item_type.as_deref().unwrap_or("tool");
    let command = row.command.as_deref();
    let file_path = row.file_path.as_deref();
    match row.event_type.as_str() {
        "request_approval" => format!("Approval requested ({item_type})"),
        "approval_decision" => row
            .approval_decision
            .as_deref()
            .map(|decision| format!("Approval decision: {decision}"))
            .unwrap_or_else(|| "Approval decision submitted".to_owned()),
        "request_user_input" => "User input requested".to_owned(),
        "user_input_submitted" => "User input submitted".to_owned(),
        "started" => {
            if let Some(command) = command {
                return format!("Started: {}", truncate_chars(command, 80));
            }
            if let Some(file_path) = file_path {
                return format!("Started file change: {file_path}");
            }
            format!("Started {item_type}")
        }
        "output_delta" => {
            if let Some(command) = command {
                return format!("Output update: {}", truncate_chars(command, 60));
            }
            if let Some(file_path) = file_path {
                return format!("File update: {file_path}");
            }
            "Output update".to_owned()
        }
        "completed" | "failed" | "interrupted" | "cancelled" | "timeout" => {
            let target = command.or(file_path).unwrap_or(item_type);
            if row.event_type == "failed" {
                if let Some(error_message) = row.error_message.as_deref() {
                    return format!("Failed {target}: {}", truncate_chars(error_message, 120));
                }
            }
            format!("{} {target}", capitalize_ascii(&row.event_type))
        }
        event_type => format!("{event_type} ({item_type})"),
    }
}

fn is_terminal_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "completed"
            | "failed"
            | "interrupted"
            | "cancelled"
            | "timeout"
            | "approval_decision"
            | "user_input_submitted"
    )
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn capitalize_ascii(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
}
