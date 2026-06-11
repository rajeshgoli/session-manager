use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{json, Value};

const DEFAULT_PAYLOAD_PREVIEW_CHARS: usize = 1500;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexEventsResponse {
    pub events: Vec<CodexEventRow>,
    pub earliest_seq: Option<i64>,
    pub latest_seq: Option<i64>,
    pub next_seq: i64,
    pub history_gap: bool,
    pub gap_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexEventRow {
    pub session_id: String,
    pub seq: i64,
    pub timestamp: String,
    pub event_type: String,
    pub turn_id: Option<String>,
    pub payload_preview: Option<Value>,
    pub persisted: bool,
}

pub fn list_codex_events_from_path(
    db_path: &Path,
    session_id: &str,
    since_seq: Option<i64>,
    limit: usize,
) -> Result<CodexEventsResponse> {
    if !db_path.exists() {
        return Ok(empty_response(since_seq));
    }
    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(_) => return Ok(empty_response(since_seq)),
    };
    list_codex_events_conn(&conn, session_id, since_seq, limit)
}

fn list_codex_events_conn(
    conn: &Connection,
    session_id: &str,
    since_seq: Option<i64>,
    limit: usize,
) -> Result<CodexEventsResponse> {
    let limit = limit.clamp(1, 500);
    let (earliest_seq, latest_seq) = match conn.query_row(
        "SELECT MIN(seq), MAX(seq) FROM codex_session_events WHERE session_id = ?1",
        [session_id],
        |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
    ) {
        Ok(value) => value,
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            return Ok(empty_response(since_seq));
        }
        Err(_) => return Ok(empty_response(since_seq)),
    };

    let (Some(earliest_seq), Some(latest_seq)) = (earliest_seq, latest_seq) else {
        return Ok(empty_response(since_seq));
    };

    let mut history_gap = false;
    let mut gap_reason = None;
    let start_seq = match since_seq {
        None => earliest_seq.max(latest_seq.saturating_sub(limit as i64 - 1)),
        Some(seq) if seq < earliest_seq.checked_sub(1).unwrap_or(i64::MIN) => {
            history_gap = true;
            gap_reason = Some("retention".to_owned());
            earliest_seq
        }
        Some(seq) => next_seq_value(seq),
    };

    let mut statement = match conn.prepare(
        r#"
        SELECT seq, timestamp, event_type, turn_id, payload_preview_json
        FROM codex_session_events
        WHERE session_id = ?1 AND seq >= ?2
        ORDER BY seq ASC
        LIMIT ?3
        "#,
    ) {
        Ok(statement) => statement,
        Err(_) => return Ok(empty_response(since_seq)),
    };
    let rows = match statement.query_map((session_id, start_seq, limit as i64), |row| {
        let seq: i64 = row.get(0)?;
        let timestamp: String = row.get(1)?;
        let event_type: String = row.get(2)?;
        let turn_id: Option<String> = row.get(3)?;
        let payload_preview_json: Option<String> = row.get(4)?;
        Ok(CodexEventRow {
            session_id: session_id.to_owned(),
            seq,
            timestamp,
            event_type,
            turn_id,
            payload_preview: payload_preview_json.as_deref().map(parse_payload_preview),
            persisted: true,
        })
    }) {
        Ok(rows) => rows,
        Err(_) => return Ok(empty_response(since_seq)),
    };
    let events = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap_or_else(|_| Vec::new());

    let next_seq = events
        .last()
        .map(|event| next_seq_value(event.seq))
        .or_else(|| since_seq.map(next_seq_value))
        .unwrap_or(earliest_seq);

    Ok(CodexEventsResponse {
        events,
        earliest_seq: Some(earliest_seq),
        latest_seq: Some(latest_seq),
        next_seq,
        history_gap,
        gap_reason,
    })
}

fn empty_response(since_seq: Option<i64>) -> CodexEventsResponse {
    CodexEventsResponse {
        events: Vec::new(),
        earliest_seq: None,
        latest_seq: None,
        next_seq: since_seq.map(next_seq_value).unwrap_or(1),
        history_gap: false,
        gap_reason: None,
    }
}

fn next_seq_value(seq: i64) -> i64 {
    seq.checked_add(1).unwrap_or(i64::MAX)
}

fn parse_payload_preview(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(
        |_| json!({ "raw": raw.chars().take(DEFAULT_PAYLOAD_PREVIEW_CHARS).collect::<String>() }),
    )
}
