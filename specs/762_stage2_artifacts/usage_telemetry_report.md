# Stage 2 Usage Telemetry Report

Generated: 2026-06-06T12:42:08-07:00

Provenance commands:

- `python3 - <<PY (SQLite counts from local retained DBs and source telemetry scan)`
- `sqlite3 ~/.local/share/claude-sessions/tool_usage.db ".schema"`
- `rg -n "telemetry|tool_usage|queue_jobs|codex_session_events|telegram_telemetry" src`
- `python3 - <<PY (parse retained logs/*.log RequestTimingMiddleware method/path timing lines)`
- `route_manifest.md` and `cli_manifest.md` were joined into surface-level observation rows so telemetry cannot silently filter source-derived surfaces.

Reconciliation status: source-derived pass 3 for Stage 2 convergence review. Rows marked manual or supplemental are extracted from source patterns that are not directly represented by decorators, argparse metadata, or local SQLite files.

## Retained Telemetry Sources

| source | path | table | all retained count | last 30 days count | last observed | timestamp column |
| --- | --- | --- | --- | --- | --- | --- |
| tool usage | /Users/rajesh/.local/share/claude-sessions/tool_usage.db | tool_usage | 0 | 0 |  | timestamp |
| telegram telemetry | /Users/rajesh/.local/share/claude-sessions/tool_usage.db | telegram_telemetry | 6110 | 6110 | 2026-06-06 19:41:59 | timestamp |
| message queue | /Users/rajesh/.local/share/claude-sessions/message_queue.db | message_queue | 279 | 279 | 2026-06-06T12:40:49.605214 | queued_at |
| scheduled reminders | /Users/rajesh/.local/share/claude-sessions/message_queue.db | scheduled_reminders | 0 | 0 |  | fire_at |
| job watches | /Users/rajesh/.local/share/claude-sessions/message_queue.db | job_watch_registrations | 0 | 0 |  | created_at |
| codex review registrations | /Users/rajesh/.local/share/claude-sessions/message_queue.db | codex_review_request_registrations | 129 | 129 | 2026-06-06T19:38:12 | requested_at |
| codex events | /Users/rajesh/.local/share/claude-sessions/codex_events.db | codex_session_events | 95414 | 95414 | 2026-06-06T19:42:07.899420+00:00 | timestamp |
| codex observability tool events | /Users/rajesh/.local/share/claude-sessions/codex_observability.db | codex_tool_events | 0 | 0 |  | created_at |
| codex observability turn events | /Users/rajesh/.local/share/claude-sessions/codex_observability.db | codex_turn_events | 516 | 516 | 2026-06-06T19:40:51.976000+00:00 | created_at |
| codex pending requests | /Users/rajesh/.local/share/claude-sessions/codex_requests.db | codex_pending_requests | 0 | 0 |  | requested_at |
| queue jobs | /Users/rajesh/.local/share/claude-sessions/queue-runner/queue_runner.db | queue_jobs | 15 | 15 | 2026-06-06T11:34:34.591330 | queued_at |
| queue policy runs | /Users/rajesh/.local/share/claude-sessions/queue-runner/policy_runs.db | queue_policy_runs | 0 | 0 |  | requested_at |
| bug reports | /Users/rajesh/projects/session-manager/data/bug_reports.db | bug_reports | missing |  |  |  |
| server timing logs | logs/*.log | RequestTimingMiddleware method/path samples | 4981 | 4981 | 2026-06-06 13:37:19 | log timestamp |

Interpretation and guardrails:

- Direct CLI command usage is not comprehensively instrumented. Current `tool_usage` has no rows, so command usage is `unknown/insufficient instrumentation` unless another source proves use.
- Retained server timing logs are threshold-biased partial route samples from `RequestTimingMiddleware`, not complete access logs. Counts below prove observed use for slow/timed requests only; zero timing rows do not prove non-use.
- Telemetry is decision support only. It cannot filter source-derived surfaces, downgrade compatibility obligations, or authorize breaking changes.
- Native Android app and on-the-go mobile terminal attach remain first-class targets by owner priority even if retained endpoint-level telemetry is sparse.
- The generic public browser surface at `sm.rajeshgo.li` is lower product value except where it supports auth, app distribution, watch diagnostics, or the mobile attach flow.

## Server Timing Log Route Evidence

Source: `logs/*.log` lines emitted by `RequestTimingMiddleware` in `src/server.py`. These are partial, threshold-biased samples controlled by `timeouts.server.request_timing_threshold_seconds` and `timeouts.server.slow_request_threshold_seconds`; they are useful positive evidence but not complete access-log counts.

Retained source-server timing rows: 4981 across 89 exact method/path values.

Top exact method/path samples:

| method | exact path | retained timing rows |
| --- | --- | --- |
| GET | /sessions | 3056 |
| GET | /events/state | 1499 |
| GET | /nodes/macbook/restore-candidates | 121 |
| POST | /codex-review-requests | 69 |
| GET | /sessions/d504b8cf/summary | 25 |
| POST | /sessions | 22 |
| GET | /client/sessions | 21 |
| GET | /sessions/ee493d8e/summary | 17 |
| GET | /sessions/17c98275/summary | 12 |
| POST | /sessions/spawn | 9 |
| GET | /sessions/6e615374/summary | 8 |
| POST | /sessions/007c6275/input | 7 |
| POST | /sessions/1a3a46a9/input | 7 |
| PATCH | /sessions/1a3a46a9 | 6 |
| GET | /sessions/dc3f3e07/summary | 5 |

Route-pattern matches used to annotate the route table:

| method | route pattern | retained timing rows | last observed | latest example |
| --- | --- | --- | --- | --- |
| GET | /sessions | 3056 | 2026-06-06 13:37:19 | logs/launchd.err.log:22204 |
| GET | /events/state | 1499 | 2026-06-06 13:37:19 | logs/launchd.err.log:22205 |
| GET | /nodes/{node_id}/restore-candidates | 121 | 2026-06-04 22:04:12 | logs/launchd.err.log:8417 |
| GET | /sessions/{session_id}/summary | 72 | 2026-06-06 13:18:24 | logs/launchd.err.log:20565 |
| POST | /codex-review-requests | 69 | 2026-06-06 13:33:29 | logs/launchd.err.log:21877 |
| POST | /sessions/{session_id}/input | 34 | 2026-06-06 13:35:27 | logs/launchd.err.log:22054 |
| POST | /sessions/{target_session_id}/kill | 24 | 2026-06-06 12:37:18 | logs/launchd.err.log:17532 |
| POST | /sessions | 22 | 2026-06-05 15:35:51 | logs/log-20260605-110453-main.log:28694 |
| GET | /client/sessions | 21 | 2026-06-05 16:31:23 | logs/log-20260605-153709-revive-tmux-client.log:5483 |
| PATCH | /sessions/{session_id} | 17 | 2026-06-06 13:27:59 | logs/launchd.err.log:21374 |
| GET | /sessions/{session_id}/attach-descriptor | 14 | 2026-06-04 22:05:45 | logs/launchd.err.log:8470 |
| POST | /sessions/spawn | 9 | 2026-06-06 13:29:58 | logs/launchd.err.log:21545 |
| DELETE | /sessions/{session_id} | 6 | 2026-06-04 15:14:17 | logs/launchd.err.log:5344 |
| POST | /sessions/{session_id}/agent-status | 5 | 2026-06-05 14:47:00 | logs/log-20260605-110453-main.log:23262 |
| POST | /nodes/{node_id}/ping | 4 | 2026-06-04 19:44:39 | logs/launchd.err.log:6032 |
| POST | /nodes/{node_id}/restore-candidates/{session_id}/restore | 3 | 2026-06-04 21:45:33 | logs/launchd.err.log:8125 |
| GET | /apps/{app_name}/meta.json | 2 | 2026-06-05 16:29:23 | logs/log-20260605-153709-revive-tmux-client.log:5163 |
| POST | /auth/device/google | 1 | 2026-06-05 09:59:47 | logs/launchd.err.log:14064 |
| POST | /sessions/{session_id}/restore | 1 | 2026-06-05 15:33:41 | logs/log-20260605-110453-main.log:28458 |

## CLI Surface Observations

| command path | observed last 30 days | last observed | telemetry status | recommended priority | basis |
| --- | --- | --- | --- | --- | --- |
| adopt | unknown |  | unknown/insufficient instrumentation | unknown until contract tests/usage improve | CLI telemetry not directly instrumented |
| all | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| alone | unknown |  | unknown/insufficient instrumentation | compatibility shim or reviewed defer | operator support surface; telemetry unknown |
| attach | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| children | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| claude | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| clean | unknown |  | unknown/insufficient instrumentation | compatibility shim or reviewed defer | operator support surface; telemetry unknown |
| clear | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| codex | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| codex-2 | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| codex-app | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| codex-fork | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| codex-fork-info | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | Codex review/rollout workflow; retained codex review registrations/events evidence |
| codex-legacy | unknown |  | unknown/insufficient instrumentation | unknown until contract tests/usage improve | CLI telemetry not directly instrumented |
| codex-rollout-gates | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | Codex review/rollout workflow; retained codex review registrations/events evidence |
| codex-server | unknown |  | unknown/insufficient instrumentation | preserve removed-entrypoint error | compatibility requires exact retirement message/exit behavior |
| codex-tui | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| context-monitor | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| dispatch | unknown |  | unknown/insufficient instrumentation | compatibility shim or reviewed defer | operator support surface; telemetry unknown |
| em | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| email | unknown |  | unknown/insufficient instrumentation | first-class or reviewed defer | external delivery/reminder surface with side effects |
| fork | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| handoff | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| kill | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| lock | unknown |  | unknown/insufficient instrumentation | compatibility shim or reviewed defer | operator support surface; telemetry unknown |
| lookup | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| maintainer | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| me | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| name | unknown |  | unknown/insufficient instrumentation | unknown until contract tests/usage improve | CLI telemetry not directly instrumented |
| new | unknown |  | unknown/insufficient instrumentation | unknown until contract tests/usage improve | CLI telemetry not directly instrumented |
| node | unknown |  | unknown/insufficient instrumentation | compatibility shim or reviewed defer | operator support surface; telemetry unknown |
| node ping | unknown |  | unknown/insufficient instrumentation | compatibility shim or reviewed defer | operator support surface; telemetry unknown |
| nodes | unknown |  | unknown/insufficient instrumentation | compatibility shim or reviewed defer | operator support surface; telemetry unknown |
| others | unknown |  | unknown/insufficient instrumentation | unknown until contract tests/usage improve | CLI telemetry not directly instrumented |
| output | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| queue | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| queue cancel | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| queue ci-history | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| queue ci-run | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| queue ci-status | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| queue list | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| queue run | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| queue status | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| register | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| remind | unknown |  | unknown/insufficient instrumentation | first-class or reviewed defer | external delivery/reminder surface with side effects |
| request-codex-review | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | Codex review/rollout workflow; retained codex review registrations/events evidence |
| restore | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| retire | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| review | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | Codex review/rollout workflow; retained codex review registrations/events evidence |
| role | unknown |  | unknown/insufficient instrumentation | unknown until contract tests/usage improve | CLI telemetry not directly instrumented |
| roster | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| send | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| setup | unknown |  | unknown/insufficient instrumentation | compatibility shim or reviewed defer | operator support surface; telemetry unknown |
| spawn | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| status | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| subagent-start | unknown |  | unknown/insufficient instrumentation | unknown until contract tests/usage improve | CLI telemetry not directly instrumented |
| subagent-stop | unknown |  | unknown/insufficient instrumentation | unknown until contract tests/usage improve | CLI telemetry not directly instrumented |
| subagents | unknown |  | unknown/insufficient instrumentation | unknown until contract tests/usage improve | CLI telemetry not directly instrumented |
| tail | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| task | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| task-complete | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| telegram | unknown |  | unknown/insufficient instrumentation | first-class or reviewed defer | external delivery/reminder surface with side effects |
| turn-complete | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| unlock | unknown |  | unknown/insufficient instrumentation | compatibility shim or reviewed defer | operator support surface; telemetry unknown |
| unregister | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| wait | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| watch | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| watch-job | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| watch-job add | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| watch-job cancel | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| watch-job list | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | command execution / durable queue surface; retained queue_jobs evidence when applicable |
| what | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |
| who | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core operator/managed-agent workflow |

## HTTP/WebSocket Route Observations

| method | path | observed last 30 days | last observed | telemetry status | recommended priority | basis |
| --- | --- | --- | --- | --- | --- | --- |
| GET | /watch | unknown |  | unknown/insufficient instrumentation | preserve or reviewed narrower support | generic public browser lower owner value, but watch diagnostics/auth flow still externally visible |
| GET | /watch/{_path:path} | unknown |  | unknown/insufficient instrumentation | preserve or reviewed narrower support | generic public browser lower owner value, but watch diagnostics/auth flow still externally visible |
| GET | /watch | unknown |  | unknown/insufficient instrumentation | preserve or reviewed narrower support | generic public browser lower owner value, but watch diagnostics/auth flow still externally visible |
| MOUNT | /watch | unknown |  | unknown/insufficient instrumentation | preserve or reviewed narrower support | generic public browser lower owner value, but watch diagnostics/auth flow still externally visible |
| GET | / | unknown |  | unknown/insufficient instrumentation | preserve unless reviewed break | externally visible source-derived route |
| GET | /events/state | 1499 | 2026-06-06 13:37:19 | partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | protocol surface consumed by mobile/watch/node clients |
| GET | /events | unknown |  | unknown/insufficient instrumentation | first-class Rust port | protocol surface consumed by mobile/watch/node clients |
| POST | /hooks/tmux-client | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | managed-agent hook contract and telemetry/security audit path |
| GET | /auth/session | unknown |  | unknown/insufficient instrumentation | first-class Rust port | auth/mobile bootstrap support |
| GET | /client/bootstrap | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| GET | /client/analytics/summary | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| POST | /auth/device/google | 1 | 2026-06-05 09:59:47 | partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| POST | /deploy/{app_name} | unknown |  | unknown/insufficient instrumentation | preserve unless reviewed break | externally visible source-derived route |
| GET | /apps/{app_name}/latest.apk | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| GET | /apps/{app_name}/{artifact_hash}.apk | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| GET | /apps/{app_name}/meta.json | 2 | 2026-06-05 16:29:23 | partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| GET | /apk | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| GET | /auth/google/login | unknown |  | unknown/insufficient instrumentation | first-class Rust port | auth/mobile bootstrap support |
| GET | /auth/google/callback | unknown |  | unknown/insufficient instrumentation | first-class Rust port | auth/mobile bootstrap support |
| GET | /logged-out | unknown |  | unknown/insufficient instrumentation | preserve unless reviewed break | externally visible source-derived route |
| GET | /auth/logout | unknown |  | unknown/insufficient instrumentation | first-class Rust port | auth/mobile bootstrap support |
| GET | /health | unknown |  | unknown/insufficient instrumentation | preserve unless reviewed break | externally visible source-derived route |
| GET | /health/detailed | unknown |  | unknown/insufficient instrumentation | preserve unless reviewed break | externally visible source-derived route |
| POST | /sessions | 22 | 2026-06-05 15:35:51 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| POST | /sessions/create | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /sessions | 3056 | 2026-06-06 13:37:19 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| GET | /nodes | unknown |  | unknown/insufficient instrumentation | first-class or Stage 5 scoped release | remote node trust boundary |
| POST | /nodes/{node_id}/ping | 4 | 2026-06-04 19:44:39 | partial server timing log evidence; threshold-biased, not complete access log | first-class or Stage 5 scoped release | remote node trust boundary |
| GET | /nodes/{node_id}/restore-candidates | 121 | 2026-06-04 22:04:12 | partial server timing log evidence; threshold-biased, not complete access log | first-class or Stage 5 scoped release | remote node trust boundary |
| POST | /nodes/{node_id}/restore-candidates/{session_id}/restore | 3 | 2026-06-04 21:45:33 | partial server timing log evidence; threshold-biased, not complete access log | first-class or Stage 5 scoped release | remote node trust boundary |
| WEBSOCKET | /nodes/agent | unknown |  | unknown/insufficient instrumentation | first-class Rust port | protocol surface consumed by mobile/watch/node clients |
| GET | /client/sessions | 21 | 2026-06-05 16:31:23 | partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| POST | /client/request-status | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| POST | /client/bug-reports | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| POST | /client/sessions/{session_id}/attach-ticket | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| GET | /client/terminal | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| WEBSOCKET | /client/terminal | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| POST | /client/mobile-terminal/disable | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| GET | /sessions/{session_id}/attach-descriptor | 14 | 2026-06-04 22:05:45 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| GET | /sessions/context-monitor | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/fork | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id} | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /client/sessions/{session_id} | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| GET | /sessions/{session_id}/codex-events | indirect evidence only | 2026-06-06T19:42:07.899420+00:00 | indirect retained codex DB evidence, not per-route access log | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id}/activity-actions | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id}/codex-pending-requests | indirect evidence only | 2026-06-06T19:42:07.899420+00:00 | indirect retained codex DB evidence, not per-route access log | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/codex-requests/{request_id}/respond | indirect evidence only | 2026-06-06T19:42:07.899420+00:00 | indirect retained codex DB evidence, not per-route access log | first-class Rust port | core session/operator/agent API |
| PATCH | /sessions/{session_id} | 17 | 2026-06-06 13:27:59 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| PUT | /sessions/{session_id}/role | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| DELETE | /sessions/{session_id}/role | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| PUT | /sessions/{session_id}/maintainer | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| DELETE | /sessions/{session_id}/maintainer | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /maintainer/ensure | unknown |  | unknown/insufficient instrumentation | first-class Rust port | native mobile app/on-the-go attach or app distribution support |
| POST | /registry/{role}/ensure | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core session/operator/agent API |
| GET | /registry | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core session/operator/agent API |
| GET | /registry/{role} | unknown |  | unknown/insufficient instrumentation | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/registry | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| DELETE | /sessions/{session_id}/registry | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/context-monitor | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/notify-on-stop | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| PUT | /sessions/{session_id}/task | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/input | 34 | 2026-06-06 13:35:27 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| POST | /sessions/input-batch | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/key | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/clear | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/invalidate-cache | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| DELETE | /sessions/{session_id} | 6 | 2026-06-04 15:14:17 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/restore | 1 | 2026-06-05 15:33:41 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/open | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id}/output | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id}/tool-calls | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id}/last-message | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id}/summary | 72 | 2026-06-06 13:18:24 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/subagents | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/subagents/{agent_id}/stop | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id}/subagents | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /notify | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class or Stage 4 reviewed hardening | external delivery/email ingress side effects |
| GET | /humans | unknown |  | unknown/insufficient instrumentation | first-class or Stage 4 reviewed hardening | external delivery/email ingress side effects |
| GET | /humans/{identifier} | unknown |  | unknown/insufficient instrumentation | first-class or Stage 4 reviewed hardening | external delivery/email ingress side effects |
| POST | /humans/{identifier}/telegram | unknown |  | unknown/insufficient instrumentation | first-class or Stage 4 reviewed hardening | external delivery/email ingress side effects |
| POST | /humans/{identifier}/email | unknown |  | unknown/insufficient instrumentation | first-class or Stage 4 reviewed hardening | external delivery/email ingress side effects |
| POST | /email/send | unknown |  | unknown/insufficient instrumentation | first-class or Stage 4 reviewed hardening | external delivery/email ingress side effects |
| POST | /api/email-inbound | unknown |  | unknown/insufficient instrumentation | first-class or Stage 4 reviewed hardening | external delivery/email ingress side effects |
| POST | normalized `email_bridge.webhook_path` value when configured != /api/email-inbound | unknown |  | unknown/insufficient instrumentation | first-class or Stage 4 reviewed hardening | external delivery/email ingress side effects |
| POST | /hooks/claude | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | managed-agent hook contract and telemetry/security audit path |
| POST | /sessions/spawn | 9 | 2026-06-06 13:29:58 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id}/review-results | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/review | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/review | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /reviews/pr | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | Codex/admin/rollout externally visible support surface |
| GET | /sessions/{parent_session_id}/children | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /admin/rollout-flags | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | Codex/admin/rollout externally visible support surface |
| GET | /admin/codex-fork-runtime | indirect evidence only | 2026-06-06T19:42:07.899420+00:00 | indirect retained codex DB evidence, not per-route access log | first-class or compatibility shim | Codex/admin/rollout externally visible support surface |
| GET | /admin/codex-launch-gates | indirect evidence only | 2026-06-06T19:42:07.899420+00:00 | indirect retained codex DB evidence, not per-route access log | first-class or compatibility shim | Codex/admin/rollout externally visible support surface |
| POST | /sessions/{target_session_id}/kill | 24 | 2026-06-06 12:37:18 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/handoff | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{target_session_id}/adoption-proposals | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /adoption-proposals/{proposal_id}/accept | unknown |  | unknown/insufficient instrumentation | preserve unless reviewed break | externally visible source-derived route |
| POST | /adoption-proposals/{proposal_id}/reject | unknown |  | unknown/insufficient instrumentation | preserve unless reviewed break | externally visible source-derived route |
| POST | /sessions/{session_id}/task-complete | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{session_id}/turn-complete | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| GET | /sessions/{session_id}/send-queue | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /scheduler/remind | unknown |  | unknown/insufficient instrumentation | preserve unless reviewed break | externally visible source-derived route |
| DELETE | /scheduler/remind/{reminder_id} | unknown |  | unknown/insufficient instrumentation | preserve unless reviewed break | externally visible source-derived route |
| POST | /sessions/{session_id}/remind | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| DELETE | /sessions/{session_id}/remind | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /job-watches | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | durable command execution/job-watch surface |
| GET | /job-watches | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | durable command execution/job-watch surface |
| DELETE | /job-watches/{watch_id} | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | durable command execution/job-watch surface |
| POST | /queue-jobs | indirect evidence only | 2026-06-06T11:34:34.591330 | indirect queue DB evidence, not per-route access log | first-class or compatibility shim | durable command execution/job-watch surface |
| GET | /queue-jobs | indirect evidence only | 2026-06-06T11:34:34.591330 | indirect queue DB evidence, not per-route access log | first-class or compatibility shim | durable command execution/job-watch surface |
| GET | /queue-jobs/{job_id} | indirect evidence only | 2026-06-06T11:34:34.591330 | indirect queue DB evidence, not per-route access log | first-class or compatibility shim | durable command execution/job-watch surface |
| DELETE | /queue-jobs/{job_id} | indirect evidence only | 2026-06-06T11:34:34.591330 | indirect queue DB evidence, not per-route access log | first-class or compatibility shim | durable command execution/job-watch surface |
| POST | /queue-policy-runs | indirect evidence only | 2026-06-06T11:34:34.591330 | indirect queue DB evidence, not per-route access log | first-class or compatibility shim | durable command execution/job-watch surface |
| GET | /queue-policy-runs | indirect evidence only | 2026-06-06T11:34:34.591330 | indirect queue DB evidence, not per-route access log | first-class or compatibility shim | durable command execution/job-watch surface |
| GET | /queue-policy-runs/status | indirect evidence only | 2026-06-06T11:34:34.591330 | indirect queue DB evidence, not per-route access log | first-class or compatibility shim | durable command execution/job-watch surface |
| GET | /queue-policy-runs/{run_id} | indirect evidence only | 2026-06-06T11:34:34.591330 | indirect queue DB evidence, not per-route access log | first-class or compatibility shim | durable command execution/job-watch surface |
| POST | /codex-review-requests | 69 | 2026-06-06 13:33:29 | indirect retained codex DB evidence, not per-route access log; plus partial server timing log evidence | first-class or compatibility shim | Codex/admin/rollout externally visible support surface |
| GET | /codex-review-requests | indirect evidence only | 2026-06-06T19:42:07.899420+00:00 | indirect retained codex DB evidence, not per-route access log | first-class or compatibility shim | Codex/admin/rollout externally visible support surface |
| GET | /codex-review-requests/{request_id} | indirect evidence only | 2026-06-06T19:42:07.899420+00:00 | indirect retained codex DB evidence, not per-route access log | first-class or compatibility shim | Codex/admin/rollout externally visible support surface |
| DELETE | /codex-review-requests/{request_id} | indirect evidence only | 2026-06-06T19:42:07.899420+00:00 | indirect retained codex DB evidence, not per-route access log | first-class or compatibility shim | Codex/admin/rollout externally visible support surface |
| POST | /sessions/{session_id}/agent-status | 5 | 2026-06-05 14:47:00 | message/codex/hook state exists, partial server timing log evidence; threshold-biased, not complete access log | first-class Rust port | core session/operator/agent API |
| POST | /sessions/{target_session_id}/watch | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | core session/operator/agent API |
| POST | /hooks/tool-use | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | managed-agent hook contract and telemetry/security audit path |
| POST | /hooks/context-usage | indirect evidence possible | 2026-06-06T12:40:49.605214 | message/codex/hook state exists, no retained timing sample for this route | first-class Rust port | managed-agent hook contract and telemetry/security audit path |
| POST | /admin/cleanup-idle-topics | unknown |  | unknown/insufficient instrumentation | first-class or compatibility shim | Codex/admin/rollout externally visible support surface |

## Script And Hook Surface Observations

| script | observed last 30 days | telemetry status | recommended priority | basis |
| --- | --- | --- | --- | --- |
| hooks/log_tool_use.sh | unknown | unknown/insufficient instrumentation | first-class Rust-compatible hook contract | managed-agent lifecycle/tool/context hook ingress |
| hooks/log_tool_use.sh.bak | unknown | unknown/insufficient instrumentation | first-class Rust-compatible hook contract | managed-agent lifecycle/tool/context hook ingress |
| hooks/notify_server.sh | unknown | unknown/insufficient instrumentation | first-class Rust-compatible hook contract | managed-agent lifecycle/tool/context hook ingress |
| scripts/cleanup_duplicate_topics.py | unknown | unknown/insufficient instrumentation | Stage 5 rollout/cutover workstream | operator packaging/cutover surface |
| scripts/cleanup_orphan_forum_topics_mtproto.py | unknown | unknown/insufficient instrumentation | Stage 5 rollout/cutover workstream | operator packaging/cutover surface |
| scripts/codex_fork/release_artifacts.sh | unknown | unknown/insufficient instrumentation | Stage 5 rollout/cutover workstream | operator packaging/cutover surface |
| scripts/com.claude.session-manager.plist | unknown | unknown/insufficient instrumentation | compatibility shim or reviewed defer | direct operator script; telemetry not retained |
| scripts/deploy_android_app.sh | unknown | unknown/insufficient instrumentation | compatibility shim or reviewed defer | direct operator script; telemetry not retained |
| scripts/install-service.sh | unknown | unknown/insufficient instrumentation | Stage 5 rollout/cutover workstream | operator packaging/cutover surface |
| scripts/install_context_hooks.sh | unknown | unknown/insufficient instrumentation | Stage 5 rollout/cutover workstream | operator packaging/cutover surface |
| scripts/install_notify_server_hook.sh | unknown | unknown/insufficient instrumentation | Stage 5 rollout/cutover workstream | operator packaging/cutover surface |
| scripts/session-manager-wrapper.sh | unknown | unknown/insufficient instrumentation | compatibility shim or reviewed defer | direct operator script; telemetry not retained |
| scripts/test_clear_completed.py | unknown | unknown/insufficient instrumentation | compatibility shim or reviewed defer | direct operator script; telemetry not retained |

## Protocol And External Client Observations

| surface | observed last 30 days | telemetry status | recommended priority | basis |
| --- | --- | --- | --- | --- |
| native Android app Retrofit endpoints | unknown per endpoint | no retained per-client access log; owner priority confirms high value | first-class Rust port | high-priority mobile app/on-the-go attach workflow |
| mobile terminal WebSocket | unknown per connection | no retained WebSocket access log; mobile_terminal audit is log-only | first-class Rust port | owner-marked extremely useful attach workflow |
| Android terminal WebView `sm-terminal.local` assets | unknown | bundled client asset, not server-instrumented | first-class mobile client contract | required for in-app terminal rendering |
| watch UI `/client/sessions`, `/sessions`, `/api/sessions` fallbacks | unknown | no retained browser access log; `/api/sessions` is a client probe not a current server route | preserve current fallback behavior or add shim only through Stage 5 decision | diagnostics/support; generic public browser lower owner value |
| node-agent WebSocket | unknown | no retained connection telemetry | first-class or Stage 5 scoped release | remote node trust boundary and codex-fork IPC |
| SSE `/events` | unknown | no retained connection telemetry | first-class or compatibility shim | watch invalidation protocol |
| Cloudflare inbound email worker | unknown | external worker not instrumented locally | first-class or Stage 4 reviewed hardening | email ingress can restore sessions and inject input |
| Telegram bot commands/callbacks | 6110 telegram telemetry rows | family-level telemetry, not command-level | first-class or reviewed scope split | active retained Telegram usage |

## Persistent Surface Observations

| store/surface | observed last 30 days | last observed | telemetry status | recommended priority | basis |
| --- | --- | --- | --- | --- | --- |
| tool usage:tool_usage | 0 |  | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| telegram telemetry:telegram_telemetry | 6110 | 2026-06-06 19:41:59 | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| message queue:message_queue | 279 | 2026-06-06T12:40:49.605214 | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| scheduled reminders:scheduled_reminders | 0 |  | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| job watches:job_watch_registrations | 0 |  | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| codex review registrations:codex_review_request_registrations | 129 | 2026-06-06T19:38:12 | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| codex events:codex_session_events | 95414 | 2026-06-06T19:42:07.899420+00:00 | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| codex observability tool events:codex_tool_events | 0 |  | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| codex observability turn events:codex_turn_events | 516 | 2026-06-06T19:40:51.976000+00:00 | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| codex pending requests:codex_pending_requests | 0 |  | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| queue jobs:queue_jobs | 15 | 2026-06-06T11:34:34.591330 | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| queue policy runs:queue_policy_runs | 0 |  | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |
| bug reports:bug_reports | unknown |  | retained SQLite/JSON source table | preserve/migrate or explicit Stage 5 migration plan | state compatibility surface; local count is evidence only, not removal authority |

Initial port-priority recommendations:

| surface family | recommendation | basis |
| --- | --- | --- |
| native Android app + attach ticket + mobile terminal WebSocket | first-class Rust port | owner priority; high-value remote attach workflow |
| core `sm` CLI lifecycle/messaging/watch/attach/output | first-class Rust port | operator and managed-agent dependency |
| route/auth middleware behavior | first-class Rust port or deliberate Stage 4/5 break | security boundary |
| queue runner / policy runs | first-class or gated compatibility shim | command execution risk and existing state |
| generic public browser `sm.rajeshgo.li` watch use | candidate lower priority except diagnostics/auth/artifacts/mobile support | owner stated lower product value |
| low/unknown telemetry commands | unknown until instrumentation improves | missing telemetry is not non-use |
