# Stage 2 Persistence Manifest

Generated: 2026-06-06T12:42:08-07:00

Provenance commands:

- `python3 - <<PY (sqlite_master schema extraction from local state DBs and JSON key inspection)`
- `rg -n "CREATE TABLE|CREATE INDEX|ALTER TABLE|sqlite3.connect|db_path|state_dir" src`

Reconciliation status: source-derived pass 3 for Stage 2 convergence review. Rows marked manual or supplemental are extracted from source patterns that are not directly represented by decorators, argparse metadata, or local SQLite files.
| store | path | exists locally | objects |
| --- | --- | --- | --- |
| message queue | /Users/rajesh/.local/share/claude-sessions/message_queue.db | yes | idx_codex_review_notify_active, idx_job_watch_target_active, idx_pending, codex_review_request_registrations, job_watch_registrations, message_queue, parent_wake_registrations, remind_registrations, scheduled_reminders |
| tool usage | /Users/rajesh/.local/share/claude-sessions/tool_usage.db | yes | idx_agent_id, idx_destructive, idx_hook_type, idx_project_name, idx_session, idx_tg_timestamp, idx_timestamp, idx_tool, idx_tool_use_id, sqlite_sequence, telegram_telemetry, tool_usage |
| response relay | /Users/rajesh/.local/share/claude-sessions/response_relay.db | yes | idx_assistant_outputs_relayed, idx_inbound_turns_active, assistant_outputs, inbound_turns |
| codex events | /Users/rajesh/.local/share/claude-sessions/codex_events.db | yes | idx_codex_assistant_relays_relayed_at, idx_codex_session_events_event_type, idx_codex_session_events_timestamp, idx_codex_session_events_ts, codex_assistant_relays, codex_fork_provider_cursors, codex_fork_provider_event_positions, codex_session_events |
| codex observability | /Users/rajesh/.local/share/claude-sessions/codex_observability.db | yes | idx_codex_tool_events_event, idx_codex_tool_events_provider_schema, idx_codex_tool_events_session_call, idx_codex_tool_events_session_created, idx_codex_tool_events_session_turn_call, idx_codex_tool_events_turn, idx_codex_turn_events_provider_schema, idx_codex_turn_events_session_created, idx_codex_turn_events_turn, codex_tool_events, codex_turn_events, sqlite_sequence |
| codex requests | /Users/rajesh/.local/share/claude-sessions/codex_requests.db | yes | idx_codex_pending_generation, idx_codex_pending_session_status, codex_pending_requests |
| queue runner | /Users/rajesh/.local/share/claude-sessions/queue-runner/queue_runner.db | yes | idx_queue_jobs_finished, idx_queue_jobs_notify_state, idx_queue_jobs_state_type_queued, queue_jobs, queue_resource_samples, sqlite_sequence |
| queue policy | /Users/rajesh/.local/share/claude-sessions/queue-runner/policy_runs.db | yes | idx_policy_results_policy_finished, idx_policy_results_policy_token, idx_policy_runs_policy_admitted, idx_policy_runs_policy_requested, idx_policy_runs_policy_token, idx_policy_runs_queue_job, queue_policy_results, queue_policy_runs |
| bug reports | /Users/rajesh/projects/session-manager/data/bug_reports.db | no | source-defined: bug_reports, bug_report_attachments, idx_bug_reports_created_at, idx_bug_reports_selected_session |

## Compatibility Classification Handoff

This table is the Stage 2 migration handoff for durable and runtime persistence. Any destructive reset, one-way migration, or intentional incompatibility in a `must preserve` or `migration-required` row requires the Stage 1 user-review path, with backup, rehearsal on copied real state, and rollback/downgrade behavior documented before implementation tickets are filed.

| store / artifact | compatibility classification | Rust handoff requirement | breaking-change / user-review trigger |
| --- | --- | --- | --- |
| message queue DB | must preserve / migrate | Preserve SQLite schema, ALTER history, queued messages, reminders, wait/watch registrations, codex-review registrations, and parent wake registrations, or drain them under an explicit cutover plan. | Dropping queued or scheduled delivery state, changing retry/notification semantics, or resetting registrations. |
| tool usage DB | security/audit persistence contract | Preserve or migrate `tool_usage` and `telegram_telemetry` history used by audit, telemetry, and usage-evidence workflows. | Resetting history, changing destructive-tool classification fields, or moving the DB without archival/compatibility guidance. |
| response relay DB | active-turn delivery contract | Preserve or migrate `inbound_turns` and `assistant_outputs`, or quiesce/drain active relays before Rust owns delivery. | Losing in-flight response relay state or marking relayed outputs differently across cutover. |
| codex events DB | must preserve / migrate | Preserve `codex_session_events`, assistant relay state, codex-fork provider cursors, and provider event positions so activity, SSE/events, recovery, and provider ingestion remain monotonic. | Resetting cursors/events in a way that duplicates, skips, or hides provider events or activity state. |
| codex observability DB | operator diagnostics / telemetry contract | Preserve or migrate retained tool/turn observability when feasible; if treated as rebuildable diagnostics, explicitly classify and archive/reset it in Stage 5. | Silently dropping observability history used by usage telemetry, debugging, or operator review. |
| codex requests DB | must preserve / migrate | Preserve pending structured requests, generation/status fields, and response metadata, or close/drain pending requests before cutover. | Losing pending approvals/reviews/requests or changing pending request API behavior. |
| queue runner DB | command-execution state contract | Preserve queued/running/finished job state, resource samples, notify state, and job identifiers, or drain/cancel jobs through an operator-approved cutover. | Resetting queued/running jobs, losing exit/status/log links, or changing notify state without operator review. |
| queue policy DB | queue-admission audit contract | Preserve policy run/result history coupled to queue jobs, or classify reset only after queue runner state is drained and documented. | Losing policy evidence for active/recent jobs or changing admission-token semantics. |
| bug reports DB | native app support contract | Preserve or migrate `bug_reports` and attachment blobs, including source-defined schema even when the local DB file is absent. | Disabling bug reports, deleting reports/attachments, or changing selected-session/device metadata without user review. |
| `sessions.json` state store | primary session compatibility contract | Preserve or migrate session records, provider ids, tmux names/socket names, role/maintainer/context fields, Telegram thread ids, and adoption/agent registries; support legacy path handling. | Destructive or one-way migration, loss of stopped/live session records, or inability to downgrade/rollback. |
| `telegram_topics.json` | external Telegram mapping contract | Preserve topic ids, chat/thread mappings, cleanup state, and title reconciliation inputs. | Resetting mappings, creating duplicate forum topics, or losing topic cleanup state. |
| app artifact files and metadata | mobile app distribution contract | Preserve `data/apps`/configured artifact layout, `latest.apk`, hashed APKs, metadata JSON, content-type/hash expectations, and public/private serving semantics. | Changing APK URLs, hash validation, metadata shape, or download authorization without Stage 4/5 review. |
| logs and timing logs | operator diagnostics / telemetry input | Preserve configured log paths and keep retained logs readable for usage evidence; Rust may start new logs only if archival and telemetry parsing are documented. | Silently moving or truncating retained logs that feed diagnostics or usage telemetry. |
| lock/worktree state | operator safety contract | Preserve lock semantics and dirty-worktree safety behavior; file format can remain private only if no external actor reads it directly and migration preserves effective locks. | Clearing locks, weakening dirty-worktree warnings, or allowing conflicting worktree operations. |
| codex-fork event/control artifacts | ephemeral runtime IPC contract | Treat `/tmp/claude-sessions/*.codex-fork.events.jsonl` and `*.control.sock` as live runtime artifacts: do not migrate as durable state, but quiesce, drain, or preserve coexistence behavior during cutover. | Deleting live IPC files, interrupting active codex-fork sessions, or changing control/event path conventions during coexistence. |
| runtime sockets, tmux sockets, and per-session logs | runtime compatibility contract | Preserve attach semantics, socket naming, and per-session log/transcript paths long enough for current clients, attach descriptors, and recovery flows to complete. | Renaming sockets/log paths or breaking attach/recovery for live or recently stopped sessions. |

## message queue: `/Users/rajesh/.local/share/claude-sessions/message_queue.db`

```sql
CREATE INDEX idx_codex_review_notify_active
            ON codex_review_request_registrations(notify_session_id, is_active);

CREATE INDEX idx_job_watch_target_active
            ON job_watch_registrations(target_session_id, is_active);

CREATE INDEX idx_pending
            ON message_queue(target_session_id, delivered_at)
            WHERE delivered_at IS NULL;

CREATE TABLE codex_review_request_registrations (
                id TEXT PRIMARY KEY,
                repo TEXT NOT NULL,
                pr_number INTEGER NOT NULL,
                requester_session_id TEXT,
                notify_session_id TEXT NOT NULL,
                steer TEXT,
                requested_at TIMESTAMP NOT NULL,
                latest_request_comment_id INTEGER,
                latest_request_comment_url TEXT,
                latest_request_posted_at TIMESTAMP,
                attempt_count INTEGER NOT NULL,
                next_retry_at TIMESTAMP,
                poll_interval_seconds INTEGER NOT NULL,
                retry_interval_seconds INTEGER NOT NULL,
                pickup_detected_at TIMESTAMP,
                pickup_source TEXT,
                review_landed_at TIMESTAMP,
                review_source TEXT,
                review_comment_id INTEGER,
                review_url TEXT,
                last_polled_at TIMESTAMP,
                last_error TEXT,
                state TEXT NOT NULL,
                is_active INTEGER DEFAULT 1
            );

CREATE TABLE job_watch_registrations (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL,
                label TEXT NOT NULL,
                pid INTEGER,
                file_path TEXT,
                progress_regex TEXT,
                done_regex TEXT,
                error_regex TEXT,
                exit_code_file TEXT,
                interval_seconds INTEGER NOT NULL,
                tail_lines INTEGER NOT NULL,
                tail_on_error INTEGER NOT NULL,
                notify_on_change INTEGER DEFAULT 1,
                created_at TIMESTAMP NOT NULL,
                file_start_offset INTEGER,
                last_file_offset INTEGER,
                last_polled_at TIMESTAMP,
                last_notified_at TIMESTAMP,
                last_progress_text TEXT,
                last_event TEXT,
                is_active INTEGER DEFAULT 1
            );

CREATE TABLE message_queue (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL,
                sender_session_id TEXT,
                sender_name TEXT,
                text TEXT NOT NULL,
                delivery_mode TEXT DEFAULT 'sequential',
                from_sm_send INTEGER DEFAULT 0,
                queued_at TIMESTAMP NOT NULL,
                timeout_at TIMESTAMP,
                notify_on_delivery INTEGER DEFAULT 0,
                notify_after_seconds INTEGER,
                notify_on_stop INTEGER DEFAULT 0,
                delivered_at TIMESTAMP,
                remind_soft_threshold INTEGER,
                remind_hard_threshold INTEGER,
                remind_cancel_on_reply_session_id TEXT,
                parent_session_id TEXT,
                message_category TEXT DEFAULT NULL,
                response_relay_source TEXT DEFAULT NULL
            );

CREATE TABLE parent_wake_registrations (
                id TEXT PRIMARY KEY,
                child_session_id TEXT NOT NULL UNIQUE,
                parent_session_id TEXT NOT NULL,
                period_seconds INTEGER NOT NULL,
                registered_at TIMESTAMP NOT NULL,
                last_wake_at TIMESTAMP,
                last_status_at_prev_wake TIMESTAMP,
                escalated INTEGER DEFAULT 0,
                is_active INTEGER DEFAULT 1
            );

CREATE TABLE remind_registrations (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL UNIQUE,
                soft_threshold_seconds INTEGER NOT NULL,
                hard_threshold_seconds INTEGER NOT NULL,
                registered_at TIMESTAMP NOT NULL,
                last_reset_at TIMESTAMP NOT NULL,
                cancel_on_reply_session_id TEXT,
                soft_fired INTEGER DEFAULT 0,
                is_active INTEGER DEFAULT 1
            , tracked_status_nudge_fired INTEGER DEFAULT 0, persistent_tracking INTEGER DEFAULT 0);

CREATE TABLE scheduled_reminders (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL,
                message TEXT NOT NULL,
                fire_at TIMESTAMP NOT NULL,
                task_type TEXT DEFAULT 'reminder',
                fired INTEGER DEFAULT 0,
                recurring_interval_seconds INTEGER,
                is_active INTEGER DEFAULT 1
            );

```

## tool usage: `/Users/rajesh/.local/share/claude-sessions/tool_usage.db`

```sql
CREATE INDEX idx_agent_id ON tool_usage(agent_id);

CREATE INDEX idx_destructive ON tool_usage(is_destructive);

CREATE INDEX idx_hook_type ON tool_usage(hook_type);

CREATE INDEX idx_project_name ON tool_usage(project_name);

CREATE INDEX idx_session ON tool_usage(session_id);

CREATE INDEX idx_tg_timestamp ON telegram_telemetry(timestamp);

CREATE INDEX idx_timestamp ON tool_usage(timestamp);

CREATE INDEX idx_tool ON tool_usage(tool_name);

CREATE INDEX idx_tool_use_id ON tool_usage(tool_use_id);

CREATE TABLE sqlite_sequence(name,seq);

CREATE TABLE telegram_telemetry (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                    direction TEXT NOT NULL CHECK(direction IN ('in', 'out')),
                    session_id TEXT,
                    chat_id TEXT,
                    result TEXT
                );

CREATE TABLE tool_usage (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,

                    -- Session info (ours)
                    session_id TEXT,              -- current Python comment says CLAUDE_SESSION_MANAGER_ID; Rust canonical env should be SESSION_MANAGER_ID
                    session_name TEXT,
                    parent_session_id TEXT,

                    -- Session info (Claude's native)
                    claude_session_id TEXT,       -- Claude Code's internal session ID
                    tool_use_id TEXT,             -- For correlating PreToolUse/PostToolUse
                    cwd TEXT,                     -- Working directory at time of call
                    project_name TEXT,            -- Derived from cwd (last path component)
                    agent_id TEXT,                -- Subagent ID if this is a subagent call

                    -- Hook info
                    hook_type TEXT NOT NULL,      -- PreToolUse or PostToolUse

                    -- Tool info
                    tool_name TEXT NOT NULL,
                    tool_input TEXT,              -- JSON
                    tool_response TEXT,           -- JSON (PostToolUse only)

                    -- Derived fields
                    is_destructive BOOLEAN DEFAULT 0,
                    destructive_type TEXT,        -- e.g., "git_push_main", "rm_recursive"
                    is_sensitive_file BOOLEAN DEFAULT 0,
                    target_file TEXT,             -- For file operations
                    bash_command TEXT,            -- For Bash tool
                    exit_code INTEGER             -- For Bash PostToolUse
                );

```

## response relay: `/Users/rajesh/.local/share/claude-sessions/response_relay.db`

```sql
CREATE INDEX idx_assistant_outputs_relayed
                ON assistant_outputs(session_id, inbound_id, relayed_at);

CREATE INDEX idx_inbound_turns_active
                ON inbound_turns(session_id, delivered_at)
                WHERE superseded_at IS NULL;

CREATE TABLE assistant_outputs (
                    session_id TEXT NOT NULL,
                    inbound_id TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    provider_turn_id TEXT,
                    assistant_message_id TEXT NOT NULL,
                    completed_at TEXT NOT NULL,
                    text_hash TEXT NOT NULL,
                    text_preview TEXT,
                    relay_claimed_at TEXT,
                    relayed_at TEXT,
                    telegram_thread_id INTEGER,
                    PRIMARY KEY (session_id, inbound_id, provider, assistant_message_id)
                );

CREATE TABLE inbound_turns (
                    inbound_id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    source TEXT NOT NULL,
                    provider TEXT,
                    delivered_at TEXT NOT NULL,
                    transcript_path TEXT,
                    transcript_offset INTEGER,
                    provider_turn_id TEXT,
                    text_hash TEXT,
                    superseded_at TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

```

## codex events: `/Users/rajesh/.local/share/claude-sessions/codex_events.db`

```sql
CREATE INDEX idx_codex_assistant_relays_relayed_at
                ON codex_assistant_relays(relayed_at);

CREATE INDEX idx_codex_session_events_event_type ON codex_session_events(event_type);

CREATE INDEX idx_codex_session_events_timestamp ON codex_session_events(timestamp);

CREATE INDEX idx_codex_session_events_ts ON codex_session_events(session_id, timestamp);

CREATE TABLE codex_assistant_relays (
                    session_id TEXT NOT NULL,
                    thread_id TEXT NOT NULL DEFAULT '',
                    turn_id TEXT NOT NULL,
                    message_item_id TEXT NOT NULL,
                    text_hash TEXT NOT NULL,
                    relayed_at TEXT NOT NULL,
                    telegram_thread_id INTEGER,
                    text_preview TEXT,
                    PRIMARY KEY (session_id, thread_id, turn_id, message_item_id)
                );

CREATE TABLE codex_fork_provider_cursors (
                    session_id TEXT PRIMARY KEY,
                    session_epoch_json TEXT NOT NULL,
                    seq INTEGER NOT NULL,
                    updated_at TEXT NOT NULL
                );

CREATE TABLE codex_fork_provider_event_positions (
                    session_id TEXT NOT NULL,
                    session_epoch_json TEXT NOT NULL,
                    seq INTEGER NOT NULL,
                    applied_at TEXT NOT NULL,
                    PRIMARY KEY (session_id, session_epoch_json, seq)
                );

CREATE TABLE codex_session_events (
                    session_id TEXT NOT NULL,
                    seq INTEGER NOT NULL,
                    timestamp TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    turn_id TEXT,
                    payload_preview_json TEXT,
                    PRIMARY KEY (session_id, seq)
                );

```

## codex observability: `/Users/rajesh/.local/share/claude-sessions/codex_observability.db`

```sql
CREATE INDEX idx_codex_tool_events_event ON codex_tool_events(session_id, event_type, created_at);

CREATE INDEX idx_codex_tool_events_provider_schema ON codex_tool_events(provider, schema_version, created_at, id);

CREATE INDEX idx_codex_tool_events_session_call ON codex_tool_events(session_id, item_id, created_at, id);

CREATE INDEX idx_codex_tool_events_session_created ON codex_tool_events(session_id, created_at, id);

CREATE INDEX idx_codex_tool_events_session_turn_call ON codex_tool_events(session_id, turn_id, item_id, created_at, id);

CREATE INDEX idx_codex_tool_events_turn ON codex_tool_events(turn_id, created_at);

CREATE INDEX idx_codex_turn_events_provider_schema ON codex_turn_events(provider, schema_version, created_at, id);

CREATE INDEX idx_codex_turn_events_session_created ON codex_turn_events(session_id, created_at, id);

CREATE INDEX idx_codex_turn_events_turn ON codex_turn_events(turn_id, created_at);

CREATE TABLE codex_tool_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    thread_id TEXT,
                    turn_id TEXT,
                    item_id TEXT,
                    request_id TEXT,
                    event_type TEXT NOT NULL,
                    item_type TEXT,
                    phase TEXT,
                    command TEXT,
                    cwd TEXT,
                    exit_code INTEGER,
                    file_path TEXT,
                    diff_summary TEXT,
                    approval_decision TEXT,
                    latency_ms INTEGER,
                    final_status TEXT,
                    error_code TEXT,
                    error_message TEXT,
                    raw_payload_json TEXT,
                    provider TEXT NOT NULL DEFAULT 'codex-app',
                    schema_version INTEGER,
                    created_at TEXT NOT NULL
                );

CREATE TABLE codex_turn_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    thread_id TEXT,
                    turn_id TEXT,
                    event_type TEXT NOT NULL,
                    status TEXT,
                    delta_chars INTEGER,
                    output_preview TEXT,
                    error_code TEXT,
                    error_message TEXT,
                    raw_payload_json TEXT,
                    provider TEXT NOT NULL DEFAULT 'codex-app',
                    schema_version INTEGER,
                    created_at TEXT NOT NULL
                );

CREATE TABLE sqlite_sequence(name,seq);

```

## codex requests: `/Users/rajesh/.local/share/claude-sessions/codex_requests.db`

```sql
CREATE INDEX idx_codex_pending_generation ON codex_pending_requests(process_generation, status);

CREATE INDEX idx_codex_pending_session_status ON codex_pending_requests(session_id, status, requested_at);

CREATE TABLE codex_pending_requests (
                    request_id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    process_generation TEXT NOT NULL,
                    rpc_request_id INTEGER,
                    thread_id TEXT,
                    turn_id TEXT,
                    item_id TEXT,
                    request_type TEXT NOT NULL,
                    request_method TEXT NOT NULL,
                    requested_at TEXT NOT NULL,
                    expires_at TEXT,
                    status TEXT NOT NULL,
                    request_payload_json TEXT,
                    resolved_payload_json TEXT,
                    resolved_at TEXT,
                    resolution_source TEXT,
                    error_code TEXT,
                    error_message TEXT
                );

```

## queue runner: `/Users/rajesh/.local/share/claude-sessions/queue-runner/queue_runner.db`

```sql
CREATE INDEX idx_queue_jobs_finished ON queue_jobs(finished_at);

CREATE INDEX idx_queue_jobs_notify_state ON queue_jobs(notify_session_id, state);

CREATE INDEX idx_queue_jobs_state_type_queued ON queue_jobs(state, type, queued_at);

CREATE TABLE queue_jobs (
                    id TEXT PRIMARY KEY,
                    type TEXT NOT NULL,
                    label TEXT NOT NULL,
                    requester_session_id TEXT,
                    notify_session_id TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    argv_json TEXT,
                    script_path TEXT,
                    env_json TEXT NOT NULL,
                    timeout_seconds INTEGER NOT NULL,
                    state TEXT NOT NULL,
                    holding_reason TEXT,
                    queued_at TEXT NOT NULL,
                    started_at TEXT,
                    finished_at TEXT,
                    pid INTEGER,
                    process_group_id INTEGER,
                    exit_code INTEGER,
                    log_path TEXT,
                    exit_code_path TEXT,
                    wrapper_path TEXT,
                    queued_notified_at TEXT,
                    started_notified_at TEXT,
                    completion_notified_at TEXT
                );

CREATE TABLE queue_resource_samples (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    sampled_at TEXT NOT NULL,
                    pending_by_type_json TEXT NOT NULL,
                    running_by_type_json TEXT NOT NULL,
                    total_running INTEGER NOT NULL,
                    memory_json TEXT NOT NULL,
                    cpu_json TEXT NOT NULL,
                    gpu_json TEXT
                );

CREATE TABLE sqlite_sequence(name,seq);

```

## queue policy: `/Users/rajesh/.local/share/claude-sessions/queue-runner/policy_runs.db`

```sql
CREATE INDEX idx_policy_results_policy_finished ON queue_policy_results(policy, finished_at);

CREATE INDEX idx_policy_results_policy_token ON queue_policy_results(policy, dedupe_token);

CREATE INDEX idx_policy_runs_policy_admitted ON queue_policy_runs(policy, admitted_at);

CREATE INDEX idx_policy_runs_policy_requested ON queue_policy_runs(policy, requested_at);

CREATE INDEX idx_policy_runs_policy_token ON queue_policy_runs(policy, dedupe_token, admitted_at);

CREATE INDEX idx_policy_runs_queue_job ON queue_policy_runs(queue_job_id);

CREATE TABLE queue_policy_results (
                    policy_run_id TEXT PRIMARY KEY,
                    queue_job_id TEXT NOT NULL,
                    policy TEXT NOT NULL,
                    dedupe_token TEXT,
                    status TEXT NOT NULL,
                    exit_code INTEGER,
                    queued_at TEXT NOT NULL,
                    started_at TEXT,
                    finished_at TEXT,
                    log_path TEXT,
                    artifact_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

CREATE TABLE queue_policy_runs (
                    id TEXT PRIMARY KEY,
                    policy TEXT NOT NULL,
                    decision TEXT NOT NULL,
                    suppression_reason TEXT,
                    failed_gates_json TEXT NOT NULL,
                    dedupe_token TEXT,
                    requested_at TEXT NOT NULL,
                    admitted_at TEXT,
                    queue_job_id TEXT,
                    notify_session_id TEXT,
                    label TEXT,
                    cwd TEXT,
                    queue_type TEXT,
                    command_json TEXT,
                    script_path TEXT,
                    metadata_json TEXT NOT NULL
                );

```


## bug reports: `/Users/rajesh/projects/session-manager/data/bug_reports.db` (source-defined, local DB missing)

Local DB absence is not evidence that this store is optional. `src/bug_report_store.py` defines the schema that Rust must read/write or migrate if the native app bug-report path remains enabled.

```sql
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

CREATE INDEX IF NOT EXISTS idx_bug_reports_created_at
    ON bug_reports(created_at, id);

CREATE INDEX IF NOT EXISTS idx_bug_reports_selected_session
    ON bug_reports(selected_session_id, created_at);
```

Compatibility classification: native app support contract. The first Rust release either preserves this spool format, migrates it with backup/rollback coverage, or disables bug-report submission only through the Stage 1 user-review path.

## JSON store `/Users/rajesh/.local/share/claude-sessions/sessions.json`

Top-level keys: adoption_proposals, agent_registrations, agent_role_last_session_ids, em_topic, maintainer_session_id, sessions

Session record keys observed: agent_status_at, agent_status_text, agent_task_completed_at, auto_bootstrapped_role, cleanup_prompted, codex_thread_id, completed_at, completion_message, completion_status, context_monitor_enabled, context_monitor_notify, created_at, current_task, display_identity_synced_at_ns, display_identity_synced_chat_id, display_identity_synced_name, display_identity_synced_thread_id, error_message, forked_at, forked_by_session_id, forked_from_provider_resume_id, forked_from_session_id, forked_provider_resume_id, friendly_name, friendly_name_is_explicit, friendly_name_updated_at_ns, git_remote_url, id, is_em, last_activity, last_handoff_path, last_tool_call, last_tool_name, log_file, model, name, native_title, native_title_source_mtime_ns, native_title_updated_at_ns, node, parent_session_id, provider, provider_resume_id, recovery_count, review_config, role, spawn_prompt, spawned_at, status, stopped_at, subagents, telegram_chat_id, telegram_thread_id, tmux_session, tmux_socket_name, tokens_used, tools_used, touched_repos, transcript_path, working_dir, worktrees

## JSON store `/Users/rajesh/.local/share/claude-sessions/telegram_topics.json`

Top-level keys: topics

## Source ALTER / migration references

| file | line | statement/ref |
| --- | --- | --- |
| src/message_queue.py | 216 | cursor.execute("PRAGMA table_info(message_queue)") |
| src/message_queue.py | 219 | cursor.execute("ALTER TABLE message_queue ADD COLUMN notify_on_stop INTEGER DEFAULT 0") |
| src/message_queue.py | 222 | cursor.execute("ALTER TABLE message_queue ADD COLUMN from_sm_send INTEGER DEFAULT 0") |
| src/message_queue.py | 225 | cursor.execute("ALTER TABLE message_queue ADD COLUMN remind_soft_threshold INTEGER") |
| src/message_queue.py | 228 | cursor.execute("ALTER TABLE message_queue ADD COLUMN remind_hard_threshold INTEGER") |
| src/message_queue.py | 231 | cursor.execute("ALTER TABLE message_queue ADD COLUMN remind_cancel_on_reply_session_id TEXT") |
| src/message_queue.py | 234 | cursor.execute("ALTER TABLE message_queue ADD COLUMN parent_session_id TEXT") |
| src/message_queue.py | 237 | cursor.execute("ALTER TABLE message_queue ADD COLUMN message_category TEXT DEFAULT NULL") |
| src/message_queue.py | 240 | cursor.execute("ALTER TABLE message_queue ADD COLUMN response_relay_source TEXT DEFAULT NULL") |
| src/message_queue.py | 242 | cursor.execute("PRAGMA table_info(scheduled_reminders)") |
| src/message_queue.py | 245 | cursor.execute("ALTER TABLE scheduled_reminders ADD COLUMN recurring_interval_seconds INTEGER") |
| src/message_queue.py | 248 | cursor.execute("ALTER TABLE scheduled_reminders ADD COLUMN is_active INTEGER DEFAULT 1") |
| src/message_queue.py | 272 | cursor.execute("PRAGMA table_info(remind_registrations)") |
| src/message_queue.py | 275 | cursor.execute("ALTER TABLE remind_registrations ADD COLUMN cancel_on_reply_session_id TEXT") |
| src/message_queue.py | 278 | cursor.execute("ALTER TABLE remind_registrations ADD COLUMN tracked_status_nudge_fired INTEGER DEFAULT 0") |
| src/message_queue.py | 281 | cursor.execute("ALTER TABLE remind_registrations ADD COLUMN persistent_tracking INTEGER DEFAULT 0") |
| src/message_queue.py | 329 | cursor.execute("PRAGMA table_info(job_watch_registrations)") |
| src/message_queue.py | 332 | cursor.execute("ALTER TABLE job_watch_registrations ADD COLUMN file_start_offset INTEGER") |
| src/message_queue.py | 335 | cursor.execute("ALTER TABLE job_watch_registrations ADD COLUMN last_file_offset INTEGER") |
| src/queue_runner.py | 260 | row[1] for row in conn.execute("PRAGMA table_info(queue_jobs)").fetchall() |
| src/queue_runner.py | 264 | conn.execute(f"ALTER TABLE queue_jobs ADD COLUMN {column} TEXT") |
| src/codex_observability_logger.py | 167 | cursor.execute(f"PRAGMA table_info({table})") |
| src/codex_observability_logger.py | 171 | cursor.execute(f"ALTER TABLE {table} ADD COLUMN {column} {definition}") |

Runtime artifact notes:

- Queue runner uses per-job directories under `~/.local/share/claude-sessions/queue-runner/job_*` with `run.zsh`, optional `submitted.zsh`, `exit.code`, and logs under `logs/`.
- codex-fork runtime exposes `/tmp/claude-sessions/*.codex-fork.events.jsonl` and `*.control.sock` when live.
