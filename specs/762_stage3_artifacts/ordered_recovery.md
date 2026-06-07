# Stage 3 Ordered Recovery Handoff

Status: converged after three sequential independent reviewer convergence signals.

These tables satisfy the Stage 3 requirement for ordered startup/recovery artifacts. They are compatibility baselines: Rust may improve transactionality or ordering only through Stage 4/5 review when external side effects can change.

## Server Restart / Startup Order

| Order | Step | Source / Tests | Compatibility Contract |
|-------|------|----------------|------------------------|
| 1 | Construct app dependencies and load persisted session state before `SessionManagerApp.start()` is called. | `src/main.py:220-349`; state load in `src/session_manager.py:1159-1218`. | Failed/corrupt state load must be loud and must not silently discard sessions. |
| 2 | Start infrastructure supervisor before the port preflight. | `src/main.py:904-910`. | Current behavior may repair Android attach sidecars even if another SM owns the port. Moving this after preflight is a Stage 4/5 hardening change. |
| 3 | Probe/bind configured host/port with `SO_REUSEADDR`; return before side-effectful services if already owned. | `src/main.py:912-927`. | On a doomed instance, do not start child monitor, message queue, SessionManager background tasks, Telegram, lifespan restore/reconcile, tmux hooks, or uvicorn. |
| 4 | Start child monitor. | `src/main.py:929-931`. | Child monitor starts only after preflight succeeds. |
| 5 | Start message queue manager and recover durable queue-side registrations. | `src/main.py:933-935`; `src/message_queue.py:1539-1558`. | Queue recovery precedes pending-message idle marking. |
| 6 | Start SessionManager background tasks, including codex-fork runtime maintenance. | `src/main.py:937-939`; `src/session_manager.py:5748-5762`. | Runtime maintenance starts after message queue recovery. |
| 7 | Start Telegram bot and load persisted session thread mappings. | `src/main.py:941-946`; `src/telegram_bot.py:262-271`. | Telegram mappings are hydrated before lifespan topic reconciliation. |
| 8 | Start uvicorn. Lifespan startup reconciles Telegram topics and restores output monitoring before serving lifespan-complete app state. | `src/main.py:948-969`; `src/main.py:733-746`. | Topic reconciliation and monitor restore are post-preflight server-owned work. |
| 9 | Install tmux client hooks only after uvicorn reports the listener started. | `src/main.py:748-754`, `src/main.py:964-972`. | Tmux hooks must not point at an unavailable server. |
| 10 | Shutdown cancels tmux-hook task, uvicorn-owned tasks, topic cleanup, bot, queue, monitor, maintenance tasks, and infra supervisor idempotently. | `src/main.py:969-1000`; subsystem stop methods. | Restart must not leak duplicate background tasks or stale hooks. |

## Session State Restore / Hydration Order

| Order | Step | Source / Tests | Compatibility Contract |
|-------|------|----------------|------------------------|
| 1 | Choose configured `sessions.json`; if default is missing or unreadable and legacy `/tmp/claude-sessions/sessions.json` exists, fall back to legacy path. | `src/session_manager.py:1159-1218`. | Existing installs preserve state across path migration. |
| 2 | Parse each session, drop retired legacy app-server Codex records, and hydrate `Session` models with backward-compatible enum/status mapping. | `src/session_manager.py:1218-1245`; `src/models.py:475-499`. | Unknown/old fields must not prevent current records from loading unless corrupt. |
| 3 | Backfill durable Telegram topic records for sessions with chat/thread ids. | `src/session_manager.py:1246-1255`. | Telegram routing survives restart even before bot reconciliation. |
| 4 | Codex app-server records restore without tmux; post-cutover retired app-server sessions are preserved as stopped retired records. | `src/session_manager.py:1256-1273`. | Do not run tmux checks on headless app-server sessions. |
| 5 | Stopped codex-fork sessions with reachable detached runtime are healed to idle, revive topic records, and set runtime owner. | `src/session_manager.py:1275-1294`; runtime reachability `src/session_manager.py:1834-1864`. | Detached runtime availability can revive stopped codex-fork state. |
| 6 | Other stopped sessions restore as records without live runtime checks. | `src/session_manager.py:1295-1300`. | Stopped records remain visible/restorable. |
| 7 | Live local tmux-backed sessions require tmux existence; missing local tmux collects orphaned topic cleanup candidates. | `src/session_manager.py:1302-1345`. | Dead local runtimes do not stay listed as live. |
| 8 | Remote-node tmux check failures preserve session state, mark node unreachable, and keep codex-fork runtime owner. | `src/session_manager.py:1303-1325`. | Node outages are overlays, not destructive state deletion. |
| 9 | Live local codex-fork sessions set runtime owner and later start event monitors from background-task startup. | `src/session_manager.py:1327-1339`; `src/session_manager.py:5748-5758`. | Event monitor recovery must not start before state hydration completes. |

## Queue Recovery Order

| Order | Step | Source / Tests | Compatibility Contract |
|-------|------|----------------|------------------------|
| 1 | `MessageQueueManager.start()` returns once if already started, then recovers scheduled reminders. | `src/message_queue.py:1539-1545`; reminders `src/message_queue.py:3140-3192`. | One-shot/recurring scheduled reminders survive restart before tracking state is recovered. |
| 2 | Recover tracked remind registrations. | `src/message_queue.py:1546`; recovery `src/message_queue.py:3771-3825`. | Active tracking state, soft/hard flags, persistent tracking, and target-facing nudge flags are hydrated before queue delivery resumes. |
| 3 | Recover parent wake registrations. | `src/message_queue.py:1548`; recovery `src/message_queue.py:4133-4175`. | Parent wake digest state is restored before pending messages can change child activity. |
| 4 | Recover external job watches. | `src/message_queue.py:1550`; recovery `src/message_queue.py:806-849`. | Active watches restart only for existing target sessions; dead targets are skipped/marked inactive. |
| 5 | Recover Codex review requests. | `src/message_queue.py:1552`; recovery `src/message_queue.py:1400-1433`. | Active review watches restart after job-watch state exists; inactive history remains listable. |
| 6 | Recover pending messages by marking sessions with pending rows idle to trigger delivery. | `src/message_queue.py:1554-1558`; pending delivery `src/message_queue.py:1560-1584`. | Durable registrations exist before delivery can wake agents. |

## Codex-Fork Recovery Order

| Order | Step | Source / Tests | Compatibility Contract |
|-------|------|----------------|------------------------|
| 1 | Hydration sets `codex_fork_runtime_owner` for live/healed codex-fork sessions and preserves remote-node unreachable overlays. | `src/session_manager.py:1275-1294`, `src/session_manager.py:1323-1339`. | Runtime ownership is known before maintenance/monitoring. |
| 2 | Background startup prunes stale runtime artifacts and attempts runtime maintenance before starting monitors from EOF. | `src/session_manager.py:5748-5758`; maintenance `src/session_manager.py:5978-6065`. | Stale artifacts do not confuse monitor recovery; active detached runtimes get control/event repair. |
| 3 | Local event monitor reads from persisted/current offset, buffers partial lines, processes complete JSONL lines, and only removes monitor state on exit. | `src/session_manager.py:3330-3369`; event line processing `src/session_manager.py:3295-3315`. | No partial-line duplication; lifecycle reducer sees events in order. |
| 4 | Remote event monitor registers node-agent bridge; bridge failures mark node unreachable and retry; received `None` marks unreachable and restarts bridge loop. | `src/session_manager.py:3371-3402`. | Remote disconnects are recoverable overlays. |
| 5 | Runtime restart path sets lifecycle state, re-registers bridges, and restarts event monitor. | `src/session_manager.py:5867-5978`. | Restart does not imply stopped session unless provider/session cleanup says so. |

## Queue Runner Recovery Order

| Order | Step | Source / Tests | Compatibility Contract |
|-------|------|----------------|------------------------|
| 1 | `QueueRunner.start()` no-ops if disabled/started, then locks and inspects persisted jobs. | `src/queue_runner.py:514-518`. | Disabled queue runner must not mutate persisted jobs. |
| 2 | Running jobs with exit-code files are finished as succeeded/failed and notify only if completion notification was not already sent. | `src/queue_runner.py:519-521`; `src/queue_runner.py:1069-1078`. | Completed jobs are not double-run and do not double-notify. |
| 3 | Running jobs past timeout are terminated as `timed_out`. | `src/queue_runner.py:1079-1081`. | Timeouts remain enforced across restart. |
| 4 | Running jobs with live pids get recovery polling tasks. | `src/queue_runner.py:1082-1085`; poll loop `src/queue_runner.py:1088-1114`. | Live jobs continue to completion rather than being failed immediately. |
| 5 | Running jobs with vanished pids fail once, preserving notify-once behavior. | `src/queue_runner.py:1086`. | Vanished processes become visible failures. |
| 6 | Pending jobs clear stale holding reasons and re-enter admission. | `src/queue_runner.py:522-525`. | Holding reasons are recomputed, not trusted across restart. |
| 7 | Scheduler and resource sampler start after recovery classification. | `src/queue_runner.py:525-526`. | Admission/sampling sees recovered state, not stale pre-recovery rows. |
