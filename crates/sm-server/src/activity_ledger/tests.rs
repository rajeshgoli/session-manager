use std::io::Write;

use serde_json::json;

use super::*;

/// 2026-09-29T05:00:00Z.
const T0: i64 = 1_790_658_000_000;
const NOW: i64 = T0 + DAY_MS;

struct Fixture {
    root: PathBuf,
    recorder: ActivityRecorder,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "sm-activity-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        let usage = root.join("usage.db");
        Connection::open(&usage)
            .unwrap()
            .execute_batch(
                "CREATE TABLE seat_sessions (seat_id TEXT NOT NULL, provider TEXT NOT NULL,
                   provider_session_id TEXT NOT NULL, artifact_path TEXT,
                   first_seen TEXT NOT NULL, last_seen TEXT NOT NULL,
                   PRIMARY KEY (seat_id, provider_session_id));",
            )
            .unwrap();
        let recorder = ActivityRecorder::new(root.join("activity.db"), usage);
        Self { root, recorder }
    }

    /// Binds a new transcript file to `seat` and returns its path.
    fn bind(&self, seat: &str, provider: &str, session: &str, file: &str) -> PathBuf {
        let path = self.root.join(file);
        fs::File::create(&path).unwrap();
        Connection::open(self.root.join("usage.db"))
            .unwrap()
            .execute(
                "INSERT INTO seat_sessions VALUES (?1, ?2, ?3, ?4, 'x', 'x')",
                params![seat, provider, session, path.to_string_lossy()],
            )
            .unwrap();
        path
    }

    fn scan(&mut self) -> ActivityScanSummary {
        self.recorder.scan_at(NOW).unwrap()
    }

    fn db(&self) -> Connection {
        Connection::open(self.root.join("activity.db")).unwrap()
    }

    /// `(seat, start, end, prompt)` relative to T0, in ms.
    fn turns(&self) -> Vec<(String, i64, i64, Option<i64>)> {
        self.db()
            .prepare(
                "SELECT seat_id, started_at_ms, ended_at_ms, prompt_at_ms FROM activity_turns
                 ORDER BY started_at_ms",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, i64>(1)? - T0,
                    row.get::<_, i64>(2)? - T0,
                    row.get::<_, Option<i64>>(3)?.map(|at| at - T0),
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// `(item_id, start, end, kind, tool)` relative to T0, in ms.
    fn spans(&self) -> Vec<(String, i64, i64, String, Option<String>)> {
        self.db()
            .prepare(
                "SELECT item_id, started_at_ms, ended_at_ms, kind, tool FROM activity_spans
                 ORDER BY started_at_ms, item_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, i64>(1)? - T0,
                    row.get::<_, i64>(2)? - T0,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn offset(&self, path: &Path) -> Option<u64> {
        self.db()
            .query_row(
                "SELECT byte_offset FROM activity_scan_offsets WHERE artifact_path = ?1",
                [path.to_string_lossy()],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn append(path: &Path, lines: &[Value]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

/// RFC 3339 for T0 + `offset_ms`.
fn ts(offset_ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(T0 + offset_ms) * 1_000_000)
        .unwrap()
        .format(&Rfc3339)
        .unwrap()
}

fn claude_prompt(at: i64, text: &str) -> Value {
    json!({"type": "user", "isSidechain": false, "timestamp": ts(at),
           "message": {"role": "user", "content": text}})
}

fn claude_meta(at: i64) -> Value {
    json!({"type": "user", "isMeta": true, "timestamp": ts(at),
           "message": {"role": "user", "content": "<system-reminder>x</system-reminder>"}})
}

fn claude_tool_use(at: i64, id: &str, name: &str, input: Value) -> Value {
    json!({"type": "assistant", "timestamp": ts(at), "message": {"role": "assistant",
           "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}})
}

fn claude_tool_result(at: i64, id: &str) -> Value {
    json!({"type": "user", "timestamp": ts(at), "message": {"role": "user",
           "content": [{"type": "tool_result", "tool_use_id": id, "content": "ok"}]}})
}

fn claude_text(at: i64) -> Value {
    json!({"type": "assistant", "timestamp": ts(at), "message": {"role": "assistant",
           "content": [{"type": "text", "text": "done"}]}})
}

fn claude_turn_duration(at: i64, duration_ms: i64) -> Value {
    json!({"type": "system", "subtype": "turn_duration", "durationMs": duration_ms,
           "timestamp": ts(at)})
}

#[test]
fn claude_turns_come_from_turn_duration_with_an_interrupted_turn_fallback() {
    let mut fixture = Fixture::new("claude-turns");
    let path = fixture.bind("seat-a", "claude", "s1", "s1.jsonl");
    append(
        &path,
        &[
            // Turn 1: prompt at 0, turn_duration at 60 s says it ran 59.9 s.
            claude_prompt(0, "You own 1676"),
            claude_meta(10),
            claude_text(30_000),
            claude_turn_duration(60_000, 59_900),
            // Turn 2 absorbed a queued message: its duration reaches back past turn 1's end,
            // so it starts where turn 1 ended.
            claude_turn_duration(120_000, 119_000),
            // Turn 3 is interrupted: no turn_duration before the next prompt.
            claude_prompt(200_000, "look at this"),
            claude_text(210_000),
            claude_tool_use(215_000, "toolu_x", "Read", json!({})),
            claude_tool_result(220_000, "toolu_x"),
            // Turn 4 opens and completes normally.
            claude_prompt(300_000, "go on"),
            claude_text(305_000),
            claude_turn_duration(306_000, 6_000),
            // A sidechain line never opens or ends a turn.
            json!({"type": "user", "isSidechain": true, "timestamp": ts(400_000),
                   "message": {"role": "user", "content": "subagent prompt"}}),
        ],
    );
    fixture.scan();
    assert_eq!(
        fixture.turns(),
        vec![
            ("seat-a".into(), 100, 60_000, Some(0)),
            ("seat-a".into(), 60_000, 120_000, Some(0)),
            ("seat-a".into(), 200_000, 220_000, Some(200_000)),
            ("seat-a".into(), 300_000, 306_000, Some(300_000)),
        ]
    );
    assert_eq!(fixture.scan(), ActivityScanSummary::default());
    // Readers open the database read-only between scans.
    let reader = Connection::open_with_flags(
        fixture.root.join("activity.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let turns: i64 = reader
        .query_row("SELECT COUNT(*) FROM activity_turns", [], |row| row.get(0))
        .unwrap();
    assert_eq!(turns, 4);
}

#[test]
fn claude_tool_pairs_join_across_two_scans() {
    let mut fixture = Fixture::new("claude-split");
    let path = fixture.bind("seat-a", "claude", "s1", "s1.jsonl");
    append(
        &path,
        &[
            claude_prompt(0, "build it"),
            claude_tool_use(
                1_000,
                "toolu_1",
                "Bash",
                json!({"command": "cargo test -p x"}),
            ),
            claude_tool_result(4_000, "toolu_1"),
            claude_tool_use(5_000, "toolu_2", "Agent", json!({"prompt": "scout"})),
        ],
    );
    let first = fixture.scan();
    assert_eq!((first.turns, first.spans), (0, 1));
    // The unterminated half of a line is left for the next scan.
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    let result = claude_tool_result(65_000, "toolu_2").to_string();
    let (head, tail) = result.split_at(20);
    write!(file, "{head}").unwrap();
    fixture.scan();
    writeln!(file, "{tail}").unwrap();
    append(
        &path,
        &[
            claude_tool_use(
                66_000,
                "toolu_3",
                "mcp__claude-in-chrome__navigate",
                json!({}),
            ),
            claude_tool_result(67_000, "toolu_3"),
            claude_turn_duration(70_000, 70_000),
        ],
    );
    fixture.scan();
    assert_eq!(
        fixture.spans(),
        vec![
            (
                "toolu_1".into(),
                1_000,
                4_000,
                "build".into(),
                Some("Bash".into())
            ),
            (
                "toolu_2".into(),
                5_000,
                65_000,
                "subagent".into(),
                Some("Agent".into())
            ),
            (
                "toolu_3".into(),
                66_000,
                67_000,
                "web".into(),
                Some("mcp__claude-in-chrome__navigate".into())
            ),
        ]
    );
    assert_eq!(fixture.turns().len(), 1);
    assert_eq!(
        fixture.offset(&path),
        Some(fs::metadata(&path).unwrap().len())
    );
    assert_eq!(fixture.scan(), ActivityScanSummary::default());
}

fn rollout(at: i64, kind: &str, payload: Value) -> Value {
    json!({"timestamp": ts(at), "type": kind, "payload": payload})
}

#[test]
fn codex_rollout_turns_items_and_custom_tool_calls() {
    let mut fixture = Fixture::new("rollout");
    let path = fixture.bind("seat-c", "codex", "t1", "rollout-t1.jsonl");
    append(
        &path,
        &[
            rollout(
                0,
                "event_msg",
                json!({"type": "task_started", "turn_id": "u1"}),
            ),
            rollout(
                100,
                "response_item",
                json!({"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": "<environment_context>"}]}),
            ),
            rollout(
                200,
                "event_msg",
                json!({"type": "item_completed", "turn_id": "u1",
                       "item": {"type": "UserMessage", "id": "m1"}}),
            ),
            rollout(
                1_000,
                "response_item",
                json!({"type": "custom_tool_call", "call_id": "call_exec", "name": "exec",
                       "input": "text(await tools.exec_command({cmd:\"gh pr view\"}))"}),
            ),
            rollout(
                3_000,
                "event_msg",
                json!({"type": "item_completed", "turn_id": "u1",
                       "started_at_ms": T0 + 1_200, "completed_at_ms": T0 + 2_900,
                       "item": {"type": "CommandExecution", "id": "exec-1",
                                "command": ["/bin/zsh", "-lc", "gh pr view 12"]}}),
            ),
            rollout(
                3_100,
                "response_item",
                json!({"type": "custom_tool_call_output", "call_id": "call_exec"}),
            ),
            rollout(
                4_000,
                "response_item",
                json!({"type": "custom_tool_call", "call_id": "call_patch",
                       "name": "apply_patch", "input": "*** Begin Patch"}),
            ),
            rollout(
                4_500,
                "response_item",
                json!({"type": "custom_tool_call_output", "call_id": "call_patch"}),
            ),
            rollout(
                5_000,
                "event_msg",
                json!({"type": "item_completed", "started_at_ms": T0 + 4_800,
                       "completed_at_ms": T0 + 5_000,
                       "item": {"type": "Reasoning", "id": "rs_1"}}),
            ),
            rollout(
                6_000,
                "event_msg",
                json!({"type": "item_completed", "started_at_ms": T0 + 5_500,
                       "completed_at_ms": T0 + 6_000,
                       "item": {"type": "Extension", "kind": "web.search", "id": "ws-1"}}),
            ),
            rollout(
                9_000,
                "event_msg",
                json!({"type": "task_complete", "turn_id": "u1"}),
            ),
        ],
    );
    let summary = fixture.scan();
    assert_eq!((summary.turns, summary.spans), (1, 4));
    assert_eq!(
        fixture.turns(),
        vec![("seat-c".into(), 0, 9_000, Some(200))]
    );
    assert_eq!(
        fixture.spans(),
        vec![
            (
                "call_exec".into(),
                1_000,
                3_100,
                "shell".into(),
                Some("exec".into())
            ),
            (
                "exec-1".into(),
                1_200,
                2_900,
                "git".into(),
                Some("commandExecution".into())
            ),
            (
                "call_patch".into(),
                4_000,
                4_500,
                "edit".into(),
                Some("apply_patch".into())
            ),
            (
                "ws-1".into(),
                5_500,
                6_000,
                "web".into(),
                Some("extension".into())
            ),
        ]
    );
    assert_eq!(fixture.scan(), ActivityScanSummary::default());
}

fn fork(at: i64, thread: &str, event_type: &str, payload: Value) -> Value {
    json!({"schema_version": 2, "ts": ts(at), "session_id": thread, "event_type": event_type,
           "payload": payload})
}

#[test]
fn codex_fork_turns_items_approval_reviews_and_duration_fallback() {
    let mut fixture = Fixture::new("fork");
    let path = fixture.bind("seat-f", "codex-fork", "th", "s.codex-fork.events.jsonl");
    let item = |at: i64, event: &str, payload: Value| fork(at, "th", event, payload);
    append(
        &path,
        &[
            fork(
                0,
                "th",
                "thread/started",
                json!({"thread": {"id": "th", "ephemeral": false}}),
            ),
            item(1_000, "turn_started", json!({"turn_id": "u1"})),
            item(
                1_100,
                "item/completed",
                json!({"completedAtMs": T0 + 1_100,
                       "item": {"type": "userMessage", "id": "m1"}}),
            ),
            // Paired with item/started: the start time comes from there.
            item(
                2_000,
                "item/started",
                json!({"startedAtMs": T0 + 2_000,
                       "item": {"type": "commandExecution", "id": "exec-a"}}),
            ),
            item(
                6_000,
                "item/completed",
                json!({"completedAtMs": T0 + 6_000,
                       "item": {"type": "commandExecution", "id": "exec-a", "durationMs": 0,
                                "command": "/bin/zsh -lc 'cd /repo && cargo build'"}}),
            ),
            // No item/started: completedAtMs − durationMs.
            item(
                9_000,
                "item/completed",
                json!({"completedAtMs": T0 + 9_000,
                       "item": {"type": "mcpToolCall", "id": "mcp-1", "durationMs": 2_000}}),
            ),
            item(
                10_000,
                "item/completed",
                json!({"completedAtMs": T0 + 10_000,
                       "item": {"type": "agentMessage", "id": "msg-1"}}),
            ),
            item(
                13_000,
                "item/autoApprovalReview/completed",
                json!({"startedAtMs": T0 + 11_000, "completedAtMs": T0 + 13_000,
                       "reviewId": "r1", "targetItemId": "exec-b"}),
            ),
            item(
                15_000,
                "item/completed",
                json!({"completedAtMs": T0 + 15_000,
                       "item": {"type": "subAgentActivity", "id": "call_sub"}}),
            ),
            // An ephemeral side thread (a catch-up summary) is not the agent's work.
            fork(
                16_000,
                "eph",
                "thread/started",
                json!({"thread": {"id": "eph", "ephemeral": true}}),
            ),
            fork(16_100, "eph", "turn_started", json!({"turn_id": "e1"})),
            fork(
                16_500,
                "eph",
                "item/completed",
                json!({"completedAtMs": T0 + 16_500,
                       "item": {"type": "commandExecution", "id": "exec-e", "durationMs": 10}}),
            ),
            fork(17_000, "eph", "turn_complete", json!({"turn_id": "e1"})),
            // A subagent thread's work belongs to the parent's subagent span.
            fork(
                17_500,
                "child",
                "thread/started",
                json!({"thread": {"id": "child", "parentThreadId": "th",
                                  "threadSource": "subagent", "ephemeral": false}}),
            ),
            fork(17_600, "child", "turn_started", json!({"turn_id": "c1"})),
            fork(
                17_800,
                "child",
                "item/completed",
                json!({"completedAtMs": T0 + 17_800,
                       "item": {"type": "commandExecution", "id": "exec-c", "durationMs": 10}}),
            ),
            fork(18_000, "child", "turn_complete", json!({"turn_id": "c1"})),
            item(20_000, "turn_complete", json!({"turn_id": "u1"})),
        ],
    );
    fixture.scan();
    assert_eq!(
        fixture.turns(),
        vec![("seat-f".into(), 1_000, 20_000, Some(1_100))]
    );
    assert_eq!(
        fixture.spans(),
        vec![
            (
                "exec-a".into(),
                2_000,
                6_000,
                "build".into(),
                Some("commandExecution".into())
            ),
            (
                "mcp-1".into(),
                7_000,
                9_000,
                "web".into(),
                Some("mcpToolCall".into())
            ),
            (
                "review:r1".into(),
                11_000,
                13_000,
                "approval".into(),
                Some("autoApprovalReview".into())
            ),
            (
                "call_sub".into(),
                15_000,
                15_000,
                "subagent".into(),
                Some("subAgentActivity".into())
            ),
        ]
    );
    assert_eq!(fixture.scan(), ActivityScanSummary::default());
}

#[test]
fn shell_commands_classify_by_first_word_after_wrappers() {
    let cases = [
        ("cat src/main.rs", Kind::Read),
        ("rg -n foo", Kind::Read),
        ("sed -n 1,20p x", Kind::Read),
        ("ls", Kind::Read),
        ("wc -l x", Kind::Read),
        ("git status", Kind::Git),
        ("gh pr view 12", Kind::Git),
        ("sm send rajesh", Kind::Sm),
        ("cargo build -p sm-server", Kind::Build),
        ("./gradlew assembleDebug", Kind::Build),
        ("npx tsc", Kind::Build),
        ("make", Kind::Build),
        ("python -m pytest tests", Kind::Build),
        ("python3 -m pytest", Kind::Build),
        ("python script.py", Kind::Shell),
        ("scripts/test-rust-isolated.sh", Kind::Build),
        ("./scripts/test-rust-isolated.sh", Kind::Build),
        ("/bin/zsh -lc 'git log -1'", Kind::Git),
        ("/bin/zsh -lc \"sm me\"", Kind::Sm),
        ("bash -lc 'cd /tmp && cargo test'", Kind::Build),
        ("cd ~/worktrees/x && git diff", Kind::Git),
        ("cd /a; cd /b && sm status 'x'", Kind::Sm),
        ("echo hi", Kind::Shell),
        ("", Kind::Shell),
    ];
    for (command, kind) in cases {
        assert_eq!(shell_kind(command), kind, "{command}");
    }
    assert_eq!(
        shell_words_to_command(&[json!("/bin/zsh"), json!("-lc"), json!("gh pr list")]),
        "gh pr list"
    );
    assert_eq!(claude_tool_kind("Grep", None), Kind::Read);
    assert_eq!(claude_tool_kind("MultiEdit", None), Kind::Edit);
    assert_eq!(claude_tool_kind("Task", None), Kind::Subagent);
    assert_eq!(claude_tool_kind("WebSearch", None), Kind::Web);
    assert_eq!(
        claude_tool_kind("Bash", Some(&json!({"command": "sm me"}))),
        Kind::Sm
    );
    assert_eq!(claude_tool_kind("TodoWrite", None), Kind::Other);
    assert_eq!(
        codex_call_kind("exec_command", None, Some(r#"{"cmd":"rg x"}"#)),
        Kind::Read
    );
    assert_eq!(
        codex_call_kind("js", Some("mcp__cua_repl"), None),
        Kind::Web
    );
    assert_eq!(codex_call_kind("wait_agent", None, None), Kind::Subagent);
}

#[test]
fn offset_holds_on_write_failure_and_old_rows_expire_after_90_days() {
    let mut fixture = Fixture::new("failure");
    let path = fixture.bind("seat-a", "claude", "s1", "s1.jsonl");
    append(
        &path,
        &[
            claude_prompt(0, "go"),
            claude_tool_use(1_000, "toolu_1", "Read", json!({})),
            claude_tool_result(2_000, "toolu_1"),
            claude_turn_duration(3_000, 3_000),
        ],
    );
    // First scan creates the schema; then make span writes fail.
    fixture.scan();
    let db = fixture.db();
    db.execute_batch(
        "DELETE FROM activity_turns; DELETE FROM activity_spans;
         DELETE FROM activity_scan_offsets;
         CREATE TRIGGER fail_spans BEFORE INSERT ON activity_spans
         BEGIN SELECT RAISE(ABORT, 'disk says no'); END;",
    )
    .unwrap();
    let error = fixture.recorder.scan_at(NOW).unwrap_err();
    assert!(format!("{error:#}").contains("disk says no"), "{error:#}");
    assert_eq!(fixture.offset(&path), None);
    assert!(fixture.turns().is_empty());

    db.execute_batch("DROP TRIGGER fail_spans;").unwrap();
    fixture.scan();
    assert_eq!(fixture.turns().len(), 1);
    assert_eq!(fixture.spans().len(), 1);

    // 90 days after the turn ended, the next daily prune removes it; rows already past the
    // horizon are never written.
    let later = T0 + 3_000 + RETENTION_MS + 1;
    fixture.recorder.scan_at(later).unwrap();
    assert!(fixture.turns().is_empty());
    assert!(fixture.spans().is_empty());

    let late = fixture.bind("seat-b", "claude", "s2", "s2.jsonl");
    append(
        &late,
        &[claude_prompt(0, "old"), claude_turn_duration(2_000, 2_000)],
    );
    fixture.recorder.scan_at(later).unwrap();
    assert!(fixture.turns().is_empty());
    assert_eq!(
        fixture.offset(&late),
        Some(fs::metadata(&late).unwrap().len())
    );
}

#[test]
fn a_subagent_binding_reads_the_main_transcript_once() {
    let mut fixture = Fixture::new("subagent");
    let main = fixture.bind("seat-a", "claude", "s1", "s1.jsonl");
    fs::create_dir_all(fixture.root.join("s1/subagents")).unwrap();
    fixture.bind("seat-a2", "claude", "s1", "s1/subagents/agent-1.jsonl");
    append(
        &main,
        &[claude_prompt(0, "go"), claude_turn_duration(1_000, 1_000)],
    );
    let artifacts = resolve_artifacts(load_bindings(&fixture.root.join("usage.db")).unwrap());
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].path, main);
    assert_eq!(fixture.scan().turns, 1);
}
