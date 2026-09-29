//! The activity recorder (sm#1676): turns and tool spans read from agent transcripts into
//! `activity.db`, for the Time view of Analytics.
//!
//! Every artifact bound in `usage.db`'s `seat_sessions` is read incrementally by byte offset.
//! Tool calls and turns still open at the offset are kept in `pending_json`, so a pair split
//! across two scans still joins. The offset, the pending state and the rows of one artifact
//! commit together, so a failed write retries from the old offset and never skips rows (#1658).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::usage_ledger::claude_main_transcript;

const RETENTION_MS: i64 = 90 * DAY_MS;
const PRUNE_INTERVAL_MS: i64 = DAY_MS;
const DAY_MS: i64 = 86_400_000;
/// A prompt opens the turn that starts at most this long after it.
const PROMPT_LEAD_MS: i64 = 5_000;
/// Open tool calls, items and turns older than this are dropped from the pending state.
const PENDING_HORIZON_MS: i64 = DAY_MS;
const RECENT_PROMPTS: usize = 32;
const LINE_HEAD_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivityScanSummary {
    pub artifacts_scanned: usize,
    pub turns: usize,
    pub spans: usize,
}

/// Owns `activity.db`; reads transcript bindings from `usage.db` read-only.
#[derive(Debug)]
pub struct ActivityRecorder {
    db_path: PathBuf,
    usage_db_path: PathBuf,
    last_prune_ms: Option<i64>,
}

impl ActivityRecorder {
    pub fn new(db_path: impl Into<PathBuf>, usage_db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            usage_db_path: usage_db_path.into(),
            last_prune_ms: None,
        }
    }

    pub fn scan(&mut self) -> Result<ActivityScanSummary> {
        self.scan_at(now_ms())
    }

    pub fn scan_at(&mut self, now_ms: i64) -> Result<ActivityScanSummary> {
        let artifacts = resolve_artifacts(load_bindings(&self.usage_db_path)?);
        let mut conn = open_for_write(&self.db_path)?;
        let cutoff_ms = now_ms - RETENTION_MS;
        let mut summary = ActivityScanSummary::default();
        let mut errors = Vec::new();
        for artifact in &artifacts {
            match scan_artifact(&mut conn, artifact, cutoff_ms) {
                Ok(Some((turns, spans))) => {
                    summary.artifacts_scanned += 1;
                    summary.turns += turns;
                    summary.spans += spans;
                }
                Ok(None) => {}
                Err(error) => errors.push(format!("{}: {error:#}", artifact.path.display())),
            }
        }
        if self
            .last_prune_ms
            .is_none_or(|at| now_ms - at >= PRUNE_INTERVAL_MS)
        {
            prune(&conn, cutoff_ms)?;
            self.last_prune_ms = Some(now_ms);
        }
        if !errors.is_empty() {
            bail!(
                "{} activity artifact scan(s) failed: {}",
                errors.len(),
                errors.join("; ")
            );
        }
        Ok(summary)
    }
}

fn open_for_write(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open activity db {}", path.display()))?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS activity_turns (
          seat_id TEXT NOT NULL, source_ref TEXT NOT NULL, provider TEXT NOT NULL,
          started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER NOT NULL,
          prompt_at_ms INTEGER,
          PRIMARY KEY (source_ref, started_at_ms));
        CREATE TABLE IF NOT EXISTS activity_spans (
          seat_id TEXT NOT NULL, source_ref TEXT NOT NULL,
          started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER NOT NULL,
          kind TEXT NOT NULL,
          tool TEXT,
          item_id TEXT NOT NULL,
          PRIMARY KEY (source_ref, item_id));
        CREATE INDEX IF NOT EXISTS activity_turns_seat ON activity_turns(seat_id, started_at_ms);
        CREATE INDEX IF NOT EXISTS activity_spans_seat ON activity_spans(seat_id, started_at_ms);
        CREATE TABLE IF NOT EXISTS activity_scan_offsets (artifact_path TEXT PRIMARY KEY,
          byte_offset INTEGER NOT NULL, mtime_ns INTEGER NOT NULL, pending_json TEXT,
          scanned_at TEXT NOT NULL);
        "#,
    )
    .context("failed to initialize activity db schema")?;
    Ok(conn)
}

fn prune(conn: &Connection, cutoff_ms: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM activity_turns WHERE ended_at_ms < ?1",
        [cutoff_ms],
    )?;
    conn.execute(
        "DELETE FROM activity_spans WHERE ended_at_ms < ?1",
        [cutoff_ms],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Claude,
    CodexRollout,
    CodexFork,
}

impl Provider {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::CodexRollout),
            "codex-fork" => Some(Self::CodexFork),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::CodexRollout => "codex",
            Self::CodexFork => "codex-fork",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Artifact {
    provider: Provider,
    path: PathBuf,
    seat_id: String,
}

struct Binding {
    seat_id: String,
    provider: String,
    provider_session_id: String,
    artifact_path: String,
}

fn load_bindings(usage_db_path: &Path) -> Result<Vec<Binding>> {
    if !usage_db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(usage_db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open usage db {}", usage_db_path.display()))?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    let has_table = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'seat_sessions'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_table {
        return Ok(Vec::new());
    }
    // Latest binding last, so it names the seat when two seats share one file.
    let mut statement = conn.prepare(
        r#"
        SELECT seat_id, provider, provider_session_id, artifact_path
        FROM seat_sessions
        WHERE artifact_path IS NOT NULL AND TRIM(artifact_path) != ''
        ORDER BY last_seen, seat_id
        "#,
    )?;
    let bindings = statement
        .query_map([], |row| {
            Ok(Binding {
                seat_id: row.get(0)?,
                provider: row.get(1)?,
                provider_session_id: row.get(2)?,
                artifact_path: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(bindings)
}

/// One artifact per file. A Claude binding that names a subagent file, or a path Claude Code
/// filed under another project folder, resolves to the session's main transcript; subagent
/// transcripts are not read, since the parent's Agent span covers their time.
fn resolve_artifacts(bindings: Vec<Binding>) -> Vec<Artifact> {
    let mut by_path = BTreeMap::<PathBuf, Artifact>::new();
    let mut transcripts_by_root = BTreeMap::new();
    for binding in bindings {
        let Some(provider) = Provider::parse(&binding.provider) else {
            continue;
        };
        let mut path = PathBuf::from(&binding.artifact_path);
        if provider == Provider::Claude {
            if let Some(main) = claude_main_transcript(
                &path,
                &binding.provider_session_id,
                &mut transcripts_by_root,
            ) {
                path = main;
            } else if path.parent().and_then(Path::file_name) == Some("subagents".as_ref()) {
                continue;
            }
        }
        by_path.insert(
            path.clone(),
            Artifact {
                provider,
                path,
                seat_id: binding.seat_id,
            },
        );
    }
    by_path.into_values().collect()
}

/// `Some((turns, spans))` written, or `None` when the file is unchanged or missing.
fn scan_artifact(
    conn: &mut Connection,
    artifact: &Artifact,
    cutoff_ms: i64,
) -> Result<Option<(usize, usize)>> {
    let metadata = match fs::metadata(&artifact.path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return Ok(None),
    };
    let mtime_ns = file_mtime_ns(&metadata);
    let file_len = metadata.len();
    let source_ref = artifact.path.to_string_lossy().into_owned();
    let saved = conn
        .query_row(
            "SELECT byte_offset, mtime_ns, pending_json FROM activity_scan_offsets
             WHERE artifact_path = ?1",
            [&source_ref],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let (offset, pending) = match saved {
        Some((offset, saved_mtime, _)) if offset == file_len && saved_mtime == mtime_ns => {
            return Ok(None)
        }
        // A file that shrank was rewritten; read it again from the start.
        Some((offset, _, pending)) if offset <= file_len => (
            offset,
            pending
                .as_deref()
                .map(serde_json::from_str::<Pending>)
                .transpose()
                .context("unreadable pending_json")?
                .unwrap_or_default(),
        ),
        _ => (0, Pending::default()),
    };

    let mut reader = BufReader::new(
        fs::File::open(&artifact.path)
            .with_context(|| format!("failed to open {}", artifact.path.display()))?,
    );
    reader.seek(SeekFrom::Start(offset))?;
    let mut parser = Parser::new(artifact.provider, pending);
    let mut next_offset = offset;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        // An unterminated tail is still being written; read it next time.
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        next_offset += read as u64;
        parser.line(&line);
    }
    let (turns, spans, pending) = parser.finish();

    let tx = conn.transaction()?;
    let mut turns_written = 0;
    let mut spans_written = 0;
    {
        let mut insert_turn = tx.prepare_cached(
            r#"
            INSERT INTO activity_turns
              (seat_id, source_ref, provider, started_at_ms, ended_at_ms, prompt_at_ms)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(source_ref, started_at_ms) DO UPDATE SET
              ended_at_ms = MAX(ended_at_ms, excluded.ended_at_ms),
              prompt_at_ms = COALESCE(prompt_at_ms, excluded.prompt_at_ms)
            "#,
        )?;
        for turn in turns.iter().filter(|turn| turn.ended_at_ms >= cutoff_ms) {
            turns_written += insert_turn.execute(params![
                artifact.seat_id,
                source_ref,
                artifact.provider.as_str(),
                turn.started_at_ms,
                turn.ended_at_ms,
                turn.prompt_at_ms,
            ])?;
        }
        let mut insert_span = tx.prepare_cached(
            r#"
            INSERT INTO activity_spans
              (seat_id, source_ref, started_at_ms, ended_at_ms, kind, tool, item_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(source_ref, item_id) DO UPDATE SET
              started_at_ms = MIN(started_at_ms, excluded.started_at_ms),
              ended_at_ms = MAX(ended_at_ms, excluded.ended_at_ms)
            "#,
        )?;
        for span in spans.iter().filter(|span| span.ended_at_ms >= cutoff_ms) {
            spans_written += insert_span.execute(params![
                artifact.seat_id,
                source_ref,
                span.started_at_ms,
                span.ended_at_ms,
                span.kind.as_str(),
                span.tool,
                span.item_id,
            ])?;
        }
    }
    tx.execute(
        r#"
        INSERT INTO activity_scan_offsets
          (artifact_path, byte_offset, mtime_ns, pending_json, scanned_at)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(artifact_path) DO UPDATE SET
          byte_offset = excluded.byte_offset,
          mtime_ns = excluded.mtime_ns,
          pending_json = excluded.pending_json,
          scanned_at = excluded.scanned_at
        "#,
        params![
            source_ref,
            next_offset,
            mtime_ns,
            serde_json::to_string(&pending)?,
            OffsetDateTime::now_utc().format(&Rfc3339)?,
        ],
    )?;
    tx.commit()?;
    Ok(Some((turns_written, spans_written)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Turn {
    started_at_ms: i64,
    ended_at_ms: i64,
    prompt_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    started_at_ms: i64,
    ended_at_ms: i64,
    kind: Kind,
    tool: Option<String>,
    item_id: String,
}

/// Appendix D.3 of the analytics spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Read,
    Edit,
    Git,
    Sm,
    Build,
    Shell,
    Subagent,
    Web,
    Approval,
    Other,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Git => "git",
            Self::Sm => "sm",
            Self::Build => "build",
            Self::Shell => "shell",
            Self::Subagent => "subagent",
            Self::Web => "web",
            Self::Approval => "approval",
            Self::Other => "other",
        }
    }
}

fn claude_tool_kind(name: &str, input: Option<&Value>) -> Kind {
    match name {
        "Read" | "Grep" | "Glob" | "LSP" => Kind::Read,
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => Kind::Edit,
        "Bash" => input
            .and_then(|input| input.get("command"))
            .and_then(Value::as_str)
            .map_or(Kind::Shell, shell_kind),
        "Agent" | "Task" => Kind::Subagent,
        "WebFetch" | "WebSearch" => Kind::Web,
        _ if name.starts_with("mcp__") => Kind::Web,
        _ => Kind::Other,
    }
}

/// Codex item types, camelCase (codex-fork) or PascalCase (rollout). `None`: not a span.
fn codex_item_kind(item: &Value) -> Option<Kind> {
    let item_type = item.get("type").and_then(Value::as_str)?;
    let kind = match lower_first(item_type).as_str() {
        "reasoning" | "agentMessage" | "userMessage" | "contextCompaction" => return None,
        "commandExecution" => item
            .get("command")
            .map_or(Kind::Shell, |command| match command {
                Value::String(command) => shell_kind(command),
                Value::Array(parts) => shell_kind(&shell_words_to_command(parts)),
                _ => Kind::Shell,
            }),
        "fileChange" => Kind::Edit,
        "mcpToolCall" | "webSearch" => Kind::Web,
        "extension" if item.get("kind").and_then(Value::as_str) == Some("web.search") => Kind::Web,
        "subAgentActivity" | "collabAgentToolCall" => Kind::Subagent,
        _ => Kind::Other,
    };
    Some(kind)
}

/// Rollout `custom_tool_call` and `function_call` names.
fn codex_call_kind(name: &str, namespace: Option<&str>, arguments: Option<&str>) -> Kind {
    match name {
        "apply_patch" => Kind::Edit,
        "exec" | "write_stdin" | "shell" => Kind::Shell,
        "exec_command" => arguments
            .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
            .and_then(|arguments| arguments.get("cmd").and_then(Value::as_str).map(shell_kind))
            .unwrap_or(Kind::Shell),
        "spawn_agent" | "wait_agent" | "send_message" | "followup_task" | "list_agents"
        | "close_agent" => Kind::Subagent,
        _ if namespace.is_some_and(|namespace| namespace.starts_with("mcp__")) => Kind::Web,
        _ => Kind::Other,
    }
}

fn shell_words_to_command(parts: &[Value]) -> String {
    let words = parts.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    // ["/bin/zsh", "-lc", "<script>"]: the script is the command.
    match words.as_slice() {
        [shell, "-lc" | "-c", script] if is_shell(shell) => (*script).to_owned(),
        _ => words.join(" "),
    }
}

fn is_shell(word: &str) -> bool {
    matches!(word.rsplit('/').next(), Some("zsh" | "bash" | "sh"))
}

/// Appendix D.3: strip a leading `/bin/zsh -lc` wrapper and its quotes, strip leading
/// `cd … &&` / `cd …;` segments, then classify by the first word.
fn shell_kind(command: &str) -> Kind {
    let mut rest = command.trim();
    let mut words = rest.splitn(3, char::is_whitespace);
    if let (Some(shell), Some("-lc" | "-c"), Some(script)) =
        (words.next(), words.next(), words.next())
    {
        if is_shell(shell) {
            let script = script.trim();
            rest = ['\'', '"']
                .iter()
                .find_map(|quote| {
                    script
                        .strip_prefix(*quote)
                        .and_then(|inner| inner.strip_suffix(*quote))
                })
                .unwrap_or(script)
                .trim();
        }
    }
    while let Some(after_cd) = rest.strip_prefix("cd ") {
        let and = after_cd.find("&&").map(|at| (at, 2));
        let semi = after_cd.find(';').map(|at| (at, 1));
        let Some((at, len)) = [and, semi].into_iter().flatten().min() else {
            break;
        };
        rest = after_cd[at + len..].trim_start();
    }
    let mut words = rest.split_whitespace();
    let Some(first) = words.next() else {
        return Kind::Shell;
    };
    match first {
        "cat" | "sed" | "rg" | "grep" | "ls" | "head" | "tail" | "nl" | "find" | "wc" => Kind::Read,
        "git" | "gh" => Kind::Git,
        "sm" => Kind::Sm,
        "cargo" | "gradle" | "./gradlew" | "npm" | "npx" | "pytest" | "make" => Kind::Build,
        "python" | "python3" if words.next() == Some("-m") && words.next() == Some("pytest") => {
            Kind::Build
        }
        _ if first.starts_with("scripts/test") || first.starts_with("./scripts/test") => {
            Kind::Build
        }
        _ => Kind::Shell,
    }
}

fn lower_first(value: &str) -> String {
    let mut chars = value.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OpenCall {
    started_at_ms: i64,
    tool: String,
    kind: Kind,
}

/// State carried between scans of one artifact.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Pending {
    /// Tool calls awaiting their result, by call id.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    open_calls: BTreeMap<String, OpenCall>,
    /// Codex turns started and not yet complete, by thread and turn id.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    open_turns: BTreeMap<String, i64>,
    /// codex-fork `item/started` times, by item id.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    started_items: BTreeMap<String, i64>,
    /// codex-fork threads that are not the seat's own work: side threads Codex starts for
    /// itself (catch-up summaries, titles) and subagent threads.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    ephemeral_threads: BTreeSet<String>,
    /// Recent non-meta prompt times, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    prompts: Vec<i64>,
    /// Claude: the prompt whose turn has no `turn_duration` line yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    open_prompt_ms: Option<i64>,
    /// Claude: the last assistant or tool-result line since `open_prompt_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_activity_ms: Option<i64>,
    /// Claude: end of the last turn written.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_turn_end_ms: Option<i64>,
}

struct Parser {
    provider: Provider,
    pending: Pending,
    turns: Vec<Turn>,
    spans: Vec<Span>,
    last_line_ms: i64,
}

impl Parser {
    fn new(provider: Provider, pending: Pending) -> Self {
        Self {
            provider,
            pending,
            turns: Vec::new(),
            spans: Vec::new(),
            last_line_ms: 0,
        }
    }

    fn line(&mut self, bytes: &[u8]) {
        if !self.wanted(bytes) {
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            return;
        };
        match self.provider {
            Provider::Claude => self.claude_line(&value),
            Provider::CodexRollout => self.rollout_line(&value),
            Provider::CodexFork => self.fork_line(&value),
        }
    }

    /// Cheap byte filter so the bulk of each file (streamed deltas, token counts) is never
    /// parsed as JSON. Every marker sits in a line's first few hundred bytes, ahead of any
    /// message body, so only the head is searched.
    fn wanted(&self, bytes: &[u8]) -> bool {
        let head = &bytes[..bytes.len().min(LINE_HEAD_BYTES)];
        let needles: &[&[u8]] = match self.provider {
            Provider::Claude => &[
                b"\"type\":\"user\"",
                b"\"role\":\"assistant\"",
                b"\"turn_duration\"",
            ],
            Provider::CodexRollout => &[
                b"\"task_started\"",
                b"\"task_complete\"",
                b"\"turn_aborted\"",
                b"\"item_completed\"",
                b"\"custom_tool_call",
                b"\"function_call",
                b"\"user_message\"",
                b"\"role\":\"user\"",
            ],
            Provider::CodexFork => &[
                b"\"event_type\":\"turn_",
                b"\"event_type\":\"item/started\"",
                b"\"event_type\":\"item/completed\"",
                b"\"event_type\":\"item_started\"",
                b"\"event_type\":\"item_completed\"",
                b"\"event_type\":\"item/autoApprovalReview/completed\"",
                b"\"event_type\":\"thread/started\"",
                b"\"event_type\":\"thread_started\"",
            ],
        };
        needles.iter().any(|needle| contains(head, needle))
    }

    fn finish(mut self) -> (Vec<Turn>, Vec<Span>, Pending) {
        let horizon = self.last_line_ms - PENDING_HORIZON_MS;
        self.pending
            .open_calls
            .retain(|_, call| call.started_at_ms >= horizon);
        self.pending.open_turns.retain(|_, start| *start >= horizon);
        self.pending
            .started_items
            .retain(|_, start| *start >= horizon);
        (self.turns, self.spans, self.pending)
    }

    fn note_prompt(&mut self, at_ms: i64) {
        self.pending.prompts.push(at_ms);
        let excess = self.pending.prompts.len().saturating_sub(RECENT_PROMPTS);
        self.pending.prompts.drain(..excess);
    }

    /// The last prompt at or before `start + 5 s`.
    fn prompt_for(&self, started_at_ms: i64) -> Option<i64> {
        self.pending
            .prompts
            .iter()
            .rev()
            .copied()
            .find(|prompt| *prompt <= started_at_ms + PROMPT_LEAD_MS)
    }

    fn push_turn(&mut self, started_at_ms: i64, ended_at_ms: i64, prompt_at_ms: Option<i64>) {
        if ended_at_ms < started_at_ms {
            return;
        }
        self.turns.push(Turn {
            started_at_ms,
            ended_at_ms,
            prompt_at_ms,
        });
    }

    fn close_call(&mut self, call_id: &str, ended_at_ms: i64) {
        if let Some(call) = self.pending.open_calls.remove(call_id) {
            self.spans.push(Span {
                started_at_ms: call.started_at_ms,
                ended_at_ms: ended_at_ms.max(call.started_at_ms),
                kind: call.kind,
                tool: Some(call.tool),
                item_id: call_id.to_owned(),
            });
        }
    }

    fn claude_line(&mut self, value: &Value) {
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let Some(at_ms) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ms)
        else {
            return;
        };
        self.last_line_ms = self.last_line_ms.max(at_ms);
        let content = value
            .get("message")
            .and_then(|message| message.get("content"));
        match value.get("type").and_then(Value::as_str) {
            Some("user") => {
                let blocks = content.and_then(Value::as_array);
                let mut had_result = false;
                for block in blocks.into_iter().flatten() {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        had_result = true;
                        if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                            self.close_call(id, at_ms);
                        }
                    }
                }
                if had_result {
                    self.pending.last_activity_ms = Some(at_ms);
                    return;
                }
                let is_meta = value.get("isMeta").and_then(Value::as_bool) == Some(true);
                let is_prompt = match content {
                    Some(Value::String(_)) => true,
                    Some(Value::Array(blocks)) => blocks
                        .iter()
                        .any(|block| block.get("type").and_then(Value::as_str) == Some("text")),
                    _ => false,
                };
                if is_meta || !is_prompt {
                    return;
                }
                self.close_claude_fallback_turn();
                self.pending.open_prompt_ms = Some(at_ms);
                self.pending.last_activity_ms = None;
                self.note_prompt(at_ms);
            }
            Some("assistant") => {
                for block in content.and_then(Value::as_array).into_iter().flatten() {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let (Some(id), Some(name)) = (
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    self.pending.open_calls.insert(
                        id.to_owned(),
                        OpenCall {
                            started_at_ms: at_ms,
                            tool: name.to_owned(),
                            kind: claude_tool_kind(name, block.get("input")),
                        },
                    );
                }
                self.pending.last_activity_ms = Some(at_ms);
            }
            Some("system")
                if value.get("subtype").and_then(Value::as_str) == Some("turn_duration") =>
            {
                let Some(duration_ms) = value.get("durationMs").and_then(Value::as_i64) else {
                    return;
                };
                // Turns in one transcript are sequential: a turn that absorbed a queued
                // message reports a duration reaching back into the previous turn.
                let started_at_ms =
                    (at_ms - duration_ms).max(self.pending.last_turn_end_ms.unwrap_or(i64::MIN));
                let prompt = self.prompt_for(started_at_ms);
                self.push_turn(started_at_ms, at_ms, prompt);
                self.pending.last_turn_end_ms = Some(at_ms);
                self.pending.open_prompt_ms = None;
                self.pending.last_activity_ms = None;
            }
            _ => {}
        }
    }

    /// A prompt whose turn wrote no `turn_duration` (interrupted or crashed) ends at the last
    /// assistant or tool-result line before the next prompt.
    fn close_claude_fallback_turn(&mut self) {
        if let (Some(prompt), Some(last)) =
            (self.pending.open_prompt_ms, self.pending.last_activity_ms)
        {
            self.push_turn(prompt, last, Some(prompt));
            self.pending.last_turn_end_ms = Some(last);
        }
        self.pending.open_prompt_ms = None;
        self.pending.last_activity_ms = None;
    }

    fn rollout_line(&mut self, value: &Value) {
        let Some(at_ms) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ms)
        else {
            return;
        };
        self.last_line_ms = self.last_line_ms.max(at_ms);
        let Some(payload) = value.get("payload") else {
            return;
        };
        let payload_type = payload.get("type").and_then(Value::as_str);
        match (value.get("type").and_then(Value::as_str), payload_type) {
            (Some("event_msg"), Some("task_started")) => {
                if let Some(turn) = payload.get("turn_id").and_then(Value::as_str) {
                    self.pending.open_turns.insert(turn.to_owned(), at_ms);
                }
            }
            (Some("event_msg"), Some("task_complete" | "turn_aborted")) => {
                if let Some(turn) = payload.get("turn_id").and_then(Value::as_str) {
                    self.close_codex_turn(turn, at_ms);
                }
            }
            (Some("event_msg"), Some("user_message")) => self.note_prompt(at_ms),
            (Some("event_msg"), Some("item_completed")) => {
                let Some(item) = payload.get("item") else {
                    return;
                };
                if item.get("type").and_then(Value::as_str) == Some("UserMessage") {
                    self.note_prompt(at_ms);
                    return;
                }
                let Some(kind) = codex_item_kind(item) else {
                    return;
                };
                let (Some(id), Some(end)) = (
                    item.get("id").and_then(Value::as_str),
                    payload.get("completed_at_ms").and_then(Value::as_i64),
                ) else {
                    return;
                };
                let start = payload
                    .get("started_at_ms")
                    .and_then(Value::as_i64)
                    .unwrap_or(end);
                self.push_item_span(id, start, end, kind, item);
            }
            (Some("response_item"), Some("message")) => {
                if payload.get("role").and_then(Value::as_str) == Some("user")
                    && !first_text(payload).is_some_and(|text| text.trim_start().starts_with('<'))
                {
                    self.note_prompt(at_ms);
                }
            }
            (Some("response_item"), Some("custom_tool_call" | "function_call")) => {
                let (Some(call_id), Some(name)) = (
                    payload.get("call_id").and_then(Value::as_str),
                    payload.get("name").and_then(Value::as_str),
                ) else {
                    return;
                };
                let kind = codex_call_kind(
                    name,
                    payload.get("namespace").and_then(Value::as_str),
                    payload.get("arguments").and_then(Value::as_str),
                );
                self.pending.open_calls.insert(
                    call_id.to_owned(),
                    OpenCall {
                        started_at_ms: at_ms,
                        tool: name.to_owned(),
                        kind,
                    },
                );
            }
            (Some("response_item"), Some("custom_tool_call_output" | "function_call_output")) => {
                if let Some(call_id) = payload.get("call_id").and_then(Value::as_str) {
                    self.close_call(call_id, at_ms);
                }
            }
            _ => {}
        }
    }

    fn close_codex_turn(&mut self, key: &str, ended_at_ms: i64) {
        if let Some(start) = self.pending.open_turns.remove(key) {
            let prompt = self.prompt_for(start);
            self.push_turn(start, ended_at_ms, prompt);
        }
    }

    fn push_item_span(&mut self, id: &str, start: i64, end: i64, kind: Kind, item: &Value) {
        let tool = item.get("type").and_then(Value::as_str).map(lower_first);
        self.spans.push(Span {
            started_at_ms: start.min(end),
            ended_at_ms: end,
            kind,
            tool,
            item_id: id.to_owned(),
        });
    }

    fn fork_line(&mut self, value: &Value) {
        let Some(at_ms) = value.get("ts").and_then(Value::as_str).and_then(parse_ms) else {
            return;
        };
        self.last_line_ms = self.last_line_ms.max(at_ms);
        let thread = value
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = value.get("payload").cloned().unwrap_or(Value::Null);
        let event_type = value.get("event_type").and_then(Value::as_str);
        if matches!(event_type, Some("thread/started" | "thread_started")) {
            let started = payload.get("thread");
            let field = |key: &str| started.and_then(|thread| thread.get(key));
            // Ephemeral side threads and subagent threads are not the seat's own turns; the
            // parent's subagent span covers a child's time.
            let side_thread = field("ephemeral").and_then(Value::as_bool) == Some(true)
                || field("parentThreadId").is_some_and(|parent| !parent.is_null())
                || matches!(
                    field("threadSource").and_then(Value::as_str),
                    Some("subagent" | "system" | "thread_title")
                );
            if side_thread {
                let id = started
                    .and_then(|thread| thread.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or(thread);
                self.pending.ephemeral_threads.insert(id.to_owned());
            }
            return;
        }
        if self.pending.ephemeral_threads.contains(thread) {
            return;
        }
        match event_type {
            Some("turn_started") => {
                if let Some(turn) = payload.get("turn_id").and_then(Value::as_str) {
                    self.pending
                        .open_turns
                        .insert(format!("{thread}/{turn}"), at_ms);
                }
            }
            Some("turn_complete" | "turn_aborted") => {
                if let Some(turn) = payload.get("turn_id").and_then(Value::as_str) {
                    self.close_codex_turn(&format!("{thread}/{turn}"), at_ms);
                }
            }
            Some("item/started" | "item_started") => {
                let Some(item) = payload.get("item") else {
                    return;
                };
                if codex_item_kind(item).is_none() {
                    return;
                }
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    let start = payload
                        .get("startedAtMs")
                        .and_then(Value::as_i64)
                        .unwrap_or(at_ms);
                    self.pending.started_items.insert(id.to_owned(), start);
                }
            }
            Some("item/completed" | "item_completed") => {
                let Some(item) = payload.get("item") else {
                    return;
                };
                if item.get("type").and_then(Value::as_str) == Some("userMessage") {
                    self.note_prompt(at_ms);
                    return;
                }
                let Some(kind) = codex_item_kind(item) else {
                    return;
                };
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    return;
                };
                let end = payload
                    .get("completedAtMs")
                    .and_then(Value::as_i64)
                    .unwrap_or(at_ms);
                let start = payload
                    .get("startedAtMs")
                    .and_then(Value::as_i64)
                    .or_else(|| self.pending.started_items.remove(id))
                    .or_else(|| {
                        item.get("durationMs")
                            .and_then(Value::as_i64)
                            .map(|duration| end - duration)
                    })
                    .unwrap_or(end);
                self.pending.started_items.remove(id);
                self.push_item_span(id, start, end, kind, item);
            }
            Some("item/autoApprovalReview/completed") => {
                let (Some(review), Some(start), Some(end)) = (
                    payload.get("reviewId").and_then(Value::as_str),
                    payload.get("startedAtMs").and_then(Value::as_i64),
                    payload.get("completedAtMs").and_then(Value::as_i64),
                ) else {
                    return;
                };
                self.spans.push(Span {
                    started_at_ms: start.min(end),
                    ended_at_ms: end,
                    kind: Kind::Approval,
                    tool: Some("autoApprovalReview".to_owned()),
                    item_id: format!("review:{review}"),
                });
            }
            _ => {}
        }
    }
}

fn first_text(payload: &Value) -> Option<&str> {
    payload
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|block| block.get("text").and_then(Value::as_str))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn parse_ms(value: &str) -> Option<i64> {
    let at = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()
}

fn now_ms() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn file_mtime_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| {
            i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
