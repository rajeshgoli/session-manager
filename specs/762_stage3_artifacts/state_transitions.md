# Stage 3 State Transition Handoff

Status: converged after three sequential independent reviewer convergence signals.

These tables satisfy the Stage 3 requirement for explicit state-transition artifacts. They focus on externally observable state and persisted fields that Rust must preserve or consciously migrate through Stage 4/5.

## SessionStatus

Source: `src/models.py:10-15`, hydration/status updates in `src/session_manager.py:1218-1345`, output-monitor status bridge in `src/main.py:886-902`, queue active/idle repair in `src/message_queue.py:1632-1788`.

| From | Trigger | To | External Contract |
|------|---------|----|-------------------|
| missing | create session succeeds | `running` | New tmux/provider runtime is visible as live only after validation and state persistence. |
| any non-stopped | output monitor completion / Stop hook idle | `idle` | Watch/mobile show waiting for input and queue delivery can resume. |
| `idle` or stale idle | PreToolUse/active input/queued active repair | `running` | Tool use or delivery clears stale idle so watch does not mislead operators. |
| non-stopped | kill / tmux death / provider terminal cleanup | `stopped` | Session remains listed/restorable with stopped metadata; queues/roles are cleaned. |
| `stopped` codex-fork | startup hydrate with reachable detached runtime | `idle` | Detached runtime can heal a stopped codex-fork record. |
| old persisted values | hydrate legacy status mapping | current enum value | Removed status values are mapped during load, not exposed as unknown states. |

## ActivityState

Source: enum `src/models.py:53-61`; projection behavior in `src/session_manager.py` activity helpers, output monitor `src/output_monitor.py`, mobile/watch response builders in `src/server.py`.

| Precedence | Condition | Activity Result | External Contract |
|------------|-----------|-----------------|-------------------|
| 1 | Session stopped/killed | `stopped` | Terminal state wins over any stale monitor/reducer evidence. |
| 2 | Remote node marked unreachable | `node-unreachable` | Node outage is visible without deleting session. |
| 3 | Provider waits for permission | `waiting_permission` | Permission prompts are distinguished from ordinary idle. |
| 4 | Completion/waiting-for-input evidence | `waiting_input` | Completed child/turn waits are not reported as active work. |
| 5 | Delivery state idle | `idle` | Queue/Stop hook idle state controls watch/mobile idle. |
| 6 | Output flowing or lifecycle running | `working` | Active tool/provider output wins over weak session status. |
| 7 | Recent Codex activity / prompt grace | `thinking` | Plain Codex avoids false idle immediately after activity. |
| 8 | Provider-specific no-tmux app-server state | `idle`, `thinking`, or `working` | Codex-app activity derives from callbacks/queue state, not tmux. |

## Delivery State

Source: `SessionDeliveryState` fields in `src/models.py:837-855`; state methods in `src/message_queue.py:1632-1788`, `src/message_queue.py:1913-2050`, delivery loop around `src/message_queue.py:4248-4560`.

| From | Trigger | To / Field Changes | External Contract |
|------|---------|--------------------|-------------------|
| absent | first delivery-state use | default state with `is_idle=False` | Missing state means not known idle. |
| any | `mark_session_idle` from Stop/completion | `is_idle=True`, `last_idle_at=now`; may arm stop notify/handoff/remind side effects | Queued messages can deliver only at a real turn boundary. |
| idle | `mark_session_active` / PreToolUse / urgent/send active repair | `is_idle=False`, session status running, delayed stop notify cancelled | Active work suppresses stale idle notifications. |
| idle with queued messages | delivery loop starts | `is_idle=False` during injection | Prevents concurrent delivery and duplicate turn-bound side effects. |
| pending user input stable past timeout | stale input save path | `saved_user_input` set, terminal input cleared, queue delivered, input restored | Human typed text is preserved around queue injection. |
| stop notify staged by paste-buffer | first genuine idle | paste-buffered notify promoted to stop notify | Spawn/EM stop notifications fire at the correct turn boundary. |

## Codex-Fork Lifecycle

Source: reducer reset `src/session_manager.py:540-551`, lifecycle setter `src/session_manager.py:1866-1900`, reducer `src/session_manager.py:1930-2045`, monitor processing `src/session_manager.py:3295-3402`.

| From | Trigger | To / Field Changes | External Contract |
|------|---------|--------------------|-------------------|
| absent/any | reset/restart/restore setup | reducer maps cleared | Old wait/turn state does not leak across runtime ownership changes. |
| idle | turn/task started event | running/working state; `turns_in_flight` updated | Watch/mobile show work in progress. |
| running | wait-for-permission/input event | wait state with kind/resume state | Permission/input waits are projected distinctly. |
| waiting | resume/turn continues | running or prior resume state | Resume clears wait state without losing turn tracking. |
| running/waiting | turn completed / assistant completed | idle if no turns remain | Activity returns idle only when real turn work is done. |
| running | interrupted abort without completed turn | remain running/waiting as reducer dictates | Spurious interrupted aborts do not falsely idle/stop the session. |
| any | shutdown-complete transport churn | no stopped transition by itself | Runtime transport restart is not a killed session. |
| any | monitor error | error lifecycle state | Control/event degradation becomes visible. |

## Codex Pending Requests

Source: DDL/startup `src/codex_request_ledger.py:41-89`, response `src/codex_request_ledger.py:228-255`, expiration `src/codex_request_ledger.py:480-497`, orphaning `src/codex_request_ledger.py:499-525`, listing route `src/server.py:5814-5837`.

| From | Trigger | To | External Contract |
|------|---------|----|-------------------|
| missing | provider registers request | `pending` | CLI/API can list/respond; provider waiter blocks. |
| `pending` | explicit accepted response | `resolved` | Provider receives resolved payload; duplicate responses are idempotent. |
| `pending` | timeout | `expired` | Timeout policy applies before optional late response handling. |
| `expired` | response allowed by policy | `resolved` | Late response can still resolve when allowed. |
| `pending` or `expired` | session kill/retire | `orphaned` with session-closed policy error | Provider waiters unblock on dead sessions. |
| `pending` or `expired` from older generation | ledger startup | `orphaned` with `server_restarted` | Restart/cutover does not leave waiters hanging forever. |

## Queue Runner Jobs

Source: states constants `src/queue_runner.py:20-21`, `QueueJob` fields `src/queue_runner.py:82-136`, create/admit/start/finish/recover around `src/queue_runner.py:541-603`, `src/queue_runner.py:884-945`, `src/queue_runner.py:1010-1066`, `src/queue_runner.py:1069-1114`, `src/queue_runner.py:1135-1165`.

| From | Trigger | To | External Contract |
|------|---------|----|-------------------|
| missing | create valid job | `pending` | Job id/files are durable before admission. |
| `pending` | admitted by scheduler | `running` | Wrapper starts in process group; started notification can fire. |
| `pending` | admission blocked | `pending` with holding reason | Operators see concurrency/memory/perf/test gate reason. |
| `running` | exit-code 0 | `succeeded` | Completion notification/log/exit code are durable. |
| `running` | nonzero/vanished process | `failed` | Failed jobs remain inspectable. |
| `running` | timeout | `timed_out` | Process group termination and timeout state survive restart. |
| `pending` or `running` | cancel | `cancelled` | Pending is not started; running process group is terminated. |
| `running` background | perf displacement | `displaced` | Perf jobs can displace eligible background jobs. |

## Reminders

Source: `RemindRegistration` fields `src/models.py:637-648`, recovery `src/message_queue.py:3140-3192`, registration/reset/cancel `src/message_queue.py:3195-3390`, loop behavior `src/message_queue.py:3668-3770`.

| From | Trigger | To / Field Changes | External Contract |
|------|---------|--------------------|-------------------|
| missing | track/remind registration | active row with reset timestamps and thresholds | Tracking survives restart. |
| active, before soft | status/reset/reply depending mode | `last_reset_at` updated; soft/nudge flags cleared | Agent progress postpones reminders. |
| active, lead window | target-facing status nudge | `tracked_status_nudge_fired=True` | Target is nudged before owner escalation. |
| active, soft threshold | owner/target soft message | `soft_fired=True` | Soft reminders send once per cycle. |
| active, hard threshold | urgent overdue message | `soft_fired=False`, nudge flag reset, cycle continues | Hard reminders interrupt and reset cadence. |
| active | cancel/reply non-persistent/target stopped | inactive/removed | Completed or stopped work stops reminders. |

## Parent Wakes

Source: `ParentWakeRegistration` fields `src/models.py:651-667`, register/cancel `src/message_queue.py:3845-3896`, loop and digest `src/message_queue.py:3959-4075`, recovery `src/message_queue.py:4133-4175`.

| From | Trigger | To / Field Changes | External Contract |
|------|---------|--------------------|-------------------|
| missing | dispatch/track child work | active registration | Parent/EM gets durable progress wakeups. |
| active, first wake | digest sent | `last_wake_at` and `last_status_at_prev_wake` set | Parent sees status/tool digest. |
| active, no status progress | escalation | `escalated=True`, period shortened | No-progress children wake parent more often. |
| active, child progress/status change | normal cadence retained | last status checkpoint updated | Status updates suppress no-progress escalation. |
| active | child completed/stopped/cancel | inactive/removed | Parent wake stops when child is no longer active. |

## Telegram Topic Records

Source: `TelegramTopicRecord` fields `src/models.py:97-144`, startup reconciliation `src/main.py:570-628`, stale cleanup `src/main.py:629-730`, title sync `src/main.py:780-880`, topic create callback `src/main.py:539-568`, routing in `src/notifier.py:156-214`.

| From | Trigger | To / Field Changes | External Contract |
|------|---------|--------------------|-------------------|
| missing | session create/follow/topic ensure succeeds | active record with chat/thread/session | Telegram messages route to session topic. |
| active record, session lacks thread | startup reconciliation reuses durable record | session thread restored; bot mapping registered | Topic routing survives state partial loss. |
| active topic with no routable session | startup/stale cleanup | deleted marker / session thread cleared | Orphaned forum topics are cleaned without deleting live topics. |
| active topic title stale | title sync succeeds | synced name/chat/thread/timestamp updated | Topic names converge to current display name. |
| active topic title request stale/fails | retry/backoff or skip | no state change unless success | Stale rename requests do not overwrite newer names. |

## Bug Report Status

Source: DDL/store `src/bug_report_store.py:40-76`, create/prune `src/bug_report_store.py:88-156`, delivery update `src/bug_report_store.py:157-166`, route side effects in `src/server.py`.

| From | Trigger | To | External Contract |
|------|---------|----|-------------------|
| missing | app bug report accepted | `new` | Report id/created fields are returned and spool row exists. |
| `new` | maintainer notification result stored | `submitted` | Delivery result is persisted even if notification failed/succeeded with details. |
| any old rows | max report pruning | deleted with attachments | Bounded spool prevents unbounded local growth. |

## App Artifact Baseline Fixture Caveat

The app artifact behavior is source-derived from `src/server.py:2868-2900`, `src/server.py:3827-3900`, and `src/server.py:3921-3963`. The cited tests in `tests/unit/test_app_artifact_server.py:42-129` are intended baseline fixtures, but in the current declared dependency environment they require the multipart form parser dependency used by `request.form()`. Until `python-multipart` or an equivalent test harness dependency is declared, those tests can fail before reaching artifact assertions. This is a fixture-readiness gap, not permission to weaken the artifact compatibility contract.
