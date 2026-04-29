# sm#670: managed local queue runner

## Scope

Add a first-class Session Manager queue for local commands that can contend for shared machine resources, such as test suites, benchmarks, and replay sweeps.

Primary UX:

```bash
sm queue run --type tests -- python -m pytest tests/unit -q
```

Immediate response:

```text
Accepted queue job job_abc123 (tests).
State: running
Log: /Users/rajesh/.local/share/claude-sessions/queue-runner/logs/job_abc123.log
```

Terminal wake to the requester:

```text
[sm queue] job_abc123 completed: succeeded in 2m14s. Log: ...
```

## Problem

The host machine is a Mac laptop with 18 GB RAM and 12 cores. Multiple agents routinely run local resource-intensive operations in parallel:

1. full Python test suites
2. performance benchmarks
3. replay sweeps
4. fixture/corpus generation

That creates three concrete failures:

1. Performance metrics are polluted when benchmarks run under unrelated load.
2. Memory pressure builds and all agents lose time to swap and UI latency.
3. Degenerate OOM can destabilize the whole machine and kill every in-flight session.

Agents currently have no reliable view of other agents' local workload. Memory rules like "do not run more than one replay" are advisory and fail under sprint fan-out. Session Manager already owns agent coordination, durable background wakeups, and operator-visible state, so it should own local queue admission too.

## Current Behavior

Verified current repo behavior:

1. `sm watch-job` can watch an already-started external process and wake a session, but it does not arbitrate when a process may start.
2. `sm request-codex-review` provides the right durable request/ack/background-notify shape, but it is GitHub-specific.
3. There is no `sm submit-task`, `sm queue`, or equivalent managed local execution queue today.

The new feature should reuse the durable request and notification patterns from `request-codex-review`, and the PID/log/exit-code lessons from `watch-job`, but it must add admission control before the process starts.

## Goals

1. Give agents one standard way to submit local queued commands instead of running resource-intensive work directly.
2. Queue or run submitted jobs based on declared workload type and host resource gates.
3. Return immediately with a job id and log path so agents can continue or tail the log directly.
4. Wake the requester when a delayed job starts and when any job completes.
5. Preserve running jobs across Session Manager restarts where the OS process is still alive.
6. Expose queue state to operators and agents via CLI/API.

## Non-goals

1. Do not implement cross-host scheduling.
2. Do not sandbox CPU/memory with cgroups or macOS job objects.
3. Do not infer task type from arbitrary command text.
4. Do not mutate task environment for determinism. If an agent wants `PYTHONHASHSEED=0`, it should pass it explicitly.
5. Do not make synchronous waiting the primary workflow. Agents should rely on the completion wakeup or tail the returned log path.
6. Do not manage parallelism inside the submitted command. Queue admission bounds across-job contention only; intra-job contention such as multiple `@pytest.mark.perf` tests inside one `pytest -n 4` invocation remains the test author's responsibility, typically through project-level serial markers or test runner configuration.

## CLI

Add one command group:

```bash
sm queue run [options] -- COMMAND [ARG...]
sm queue run [options] --script-file PATH
sm queue run [options] --script-file -
sm queue list [--type TYPE] [--state pending|running|succeeded|failed|cancelled|timed_out|displaced|done] [--all] [--json]
sm queue status <job-id> [--json]
sm queue cancel <job-id>
```

`sm queue run` options:

```text
--type tests|perf|background   default: tests
--label TEXT                   human-readable label shown in list/watch
--cwd PATH                     default: caller working directory
--timeout DURATION             examples: 90s, 10m, 2h
--env KEY=VALUE                repeatable explicit environment additions/overrides
--notify SESSION_OR_ROLE       default: current managed session
```

For `sm queue list`, `--state done` means all terminal states.

### Command ergonomics

The common case must be as simple as running the command directly:

```bash
sm queue run --type tests -- python -m pytest tests/regression/test_issue_667.py -q
```

Everything after `--` is captured as an argv vector and executed without shell interpolation. This avoids quote/backtick hazards for normal test commands.

For multiline shell or commands that intentionally need shell features, agents use a script file or stdin:

```bash
sm queue run --type perf --script-file - <<'EOF'
set -euo pipefail
python scripts/run_benchmark.py --case baseline
python scripts/run_benchmark.py --case candidate
EOF
```

`--script-file -` stores the submitted script content in the job directory and runs it with `/bin/zsh`. The stored script becomes part of the durable job record for audit/debug.

Submission exits with code `0` when SM accepts the job, even if the command later fails. Submission exits non-zero only for invalid input or SM unavailability. The managed command's exit code is reported by `sm queue status` and the completion notification.

## Workload Types

V1 ships three built-in workload types. Their defaults are configurable in `config.yaml`, but the type names are stable CLI/API values.

| Type | Max concurrent | Can displace | Can be displaced | Default timeout | Choose this when |
| --- | ---: | --- | --- | --- | --- |
| `tests` | 2 | no | no | 15m | Output is content-deterministic and multiple instances can run safely. |
| `perf` | 1 | `background` only | no | 45m | Output is wall-time-derived, such as benchmarks or latency measurements, and needs a quiet measurement window. |
| `background` | 2 | no | yes | 60m | Work is long-running, content-producing, and safe to cancel/retry, such as corpus prep or fixture generation. |

Unknown types are rejected in v1. If operators need more classes later, add config-defined custom types as a follow-up after the core queue is stable.

The choice criterion is the output contract, not how long the command runs. A slow parity/hash/equality harness still belongs in `tests` if its pass/fail result is content-deterministic. Use `perf` only when the reported result is itself derived from runtime, latency, throughput, or another measurement that would be polluted by concurrent load.

## Scheduling Rules

Jobs start when all of these are true:

1. The job is at the head of its type queue.
2. The type's max-concurrent cap has a free slot.
3. The global max-running cap has a free slot.
4. Global resource gates pass.
5. A `perf` job is not blocked by cooldown.

Within each type, ordering is FIFO by accepted timestamp.

Cross-type admission order:

1. `perf`
2. `tests`
3. `background`

This gives clean benchmark windows priority while still bounding test starvation with the cooldown rule below.

## Displacement

When a `perf` job is ready but all runnable capacity is occupied by `background` jobs, SM may displace the oldest running `background` job.

Displacement behavior:

1. Send SIGTERM to the process group.
2. Wait 10 seconds.
3. Send SIGKILL to the process group if it is still alive.
4. Mark the job terminal state as `displaced`.
5. Notify the requester that the job was displaced and must be resubmitted manually if still needed.

V1 does not automatically resubmit displaced jobs. Manual resubmission is clearer and avoids surprising repeated resource churn from work that may not be safe to restart.

## Resource Gates

V1 has two gates:

1. Memory preflight.
2. Perf cooldown.

Memory preflight reads macOS memory pressure/free memory before starting a job. Default policy:

```yaml
queue_runner:
  memory:
    min_free_bytes: 2147483648
    retry_interval_seconds: 10
```

The 2 GB default is a conservative first-pass guardrail, not a measured optimum: it is roughly 11% of the host's 18 GB RAM and leaves room for macOS, active agents, browser/Telegram clients, and filesystem cache before admitting another queued command. The value is configurable and should be tuned after the observability samples below show real workload profiles.

If the gate fails, the job remains pending with holding reason `memory_pressure`. SM does not fail the job just because memory is low; it waits until memory recovers or the job is cancelled.

Perf cooldown protects measurement quality:

```yaml
queue_runner:
  perf_cooldown_seconds: 30
```

Before starting `perf`, require a quiet window after the most recent `tests` or `perf` completion. After `perf` completes, hold new `tests` and `perf` starts for the same cooldown.

If `perf` demand is continuous and `tests` jobs are pending, after one completed `perf` job SM must admit at least one `tests` job before the next `perf` job. This prevents benchmark traffic from starving normal validation.

## Execution Model

SM starts jobs itself after admission, rather than asking the agent to start a process and register a watch.

Implementation shape:

1. Persist a job record at submission time.
2. Allocate a job directory under `~/.local/share/claude-sessions/queue-runner/<job-id>/`.
3. Write command metadata and any submitted script file into that directory.
4. At run start, launch a wrapper process detached from the SM request handler.
5. Redirect stdout and stderr to `logs/<job-id>.log`.
6. Write `pid`, `started_at`, and process group id.
7. Have the wrapper write an exit-code file before exiting.
8. Run process waiting and completion handling in a background task, not on the FastAPI event loop.

The process must be detached enough that an SM restart does not kill it. The job is still managed by SM through persisted PID/process-group metadata after restart.

## State Machine

```text
accepted -> pending -> running -> succeeded
                            \-> failed
                            \-> timed_out
                            \-> cancelled
                            \-> displaced
```

`accepted` is internal and should normally become `pending` or `running` before the CLI response returns.

Terminal state mapping:

1. `succeeded`: process exit code `0`.
2. `failed`: process exit code non-zero, or restart recovery finds a dead process with no exit file.
3. `timed_out`: SM terminated the process group after timeout.
4. `cancelled`: user cancelled the job.
5. `displaced`: SM preempted a `background` job for a `perf` job.

## Notifications

SM sends messages to the notify target using existing `sm send` delivery machinery.

Queued notification, only if the job does not start immediately:

```text
[sm queue] job_abc123 queued: tests, position 3, holding on concurrency_cap. Log: ...
```

Started notification, only if the job was previously queued:

```text
[sm queue] job_abc123 started: tests, pid 12345. Log: ...
```

Completion notification, always:

```text
[sm queue] job_abc123 completed: failed exit=1 runtime=3m12s queue=48s. Log: ...
stderr tail:
...
```

Completion notification includes:

1. job id
2. label
3. type
4. terminal state
5. exit code if present
6. queue duration
7. runtime
8. log path
9. stderr tail capped at 8 KB

## API

Add REST endpoints:

```text
POST   /queue-jobs
GET    /queue-jobs
GET    /queue-jobs/{job_id}
DELETE /queue-jobs/{job_id}
```

`POST /queue-jobs` accepts:

```json
{
  "type": "tests",
  "label": "unit regression",
  "argv": ["python", "-m", "pytest", "tests/unit", "-q"],
  "script": null,
  "cwd": "/Users/rajesh/Desktop/automation/session-manager",
  "env": {"PYTHONPATH": "."},
  "notify_target": "maintainer",
  "requester_session_id": "9b134c6e",
  "timeout_seconds": 900
}
```

Exactly one of `argv` or `script` is required.

The server resolves `notify_target` through the same live session / registry role resolution used by `request-codex-review`.

## Persistence

Use a dedicated SQLite table in the existing message-queue database or a small adjacent SQLite database under the same state directory. The implementation choice should favor bounded startup behavior and avoid large startup scans.

Persist:

1. job id
2. requester session id
3. notify session id
4. type
5. label
6. argv or script path
7. cwd
8. explicit environment overrides
9. timeout
10. state
11. holding reason
12. queued/started/finished timestamps
13. pid/process group id
14. exit code
15. log path
16. terminal summary

Indexes:

1. `(state, type, queued_at)` for scheduler admission.
2. `(notify_session_id, state)` for list/status queries.
3. `(finished_at)` for retention cleanup.

## Restart Recovery

On SM startup:

1. Load non-terminal jobs.
2. For `pending` jobs, put them back into the scheduler.
3. For `running` jobs with a live PID/process group, resume waiting in a background task.
4. For `running` jobs with an exit-code file, mark terminal and send completion if not already sent.
5. For `running` jobs with no live process and no exit-code file, mark `failed` with reason `lost_process`.

Startup recovery must not block API readiness on long filesystem scans. Bound recovery to the job table and known job directories.

## Log Management

Logs live under:

```text
~/.local/share/claude-sessions/queue-runner/logs/<job-id>.log
```

The CLI returns this path immediately. Agents that want live output should tail it directly:

```bash
tail -f ~/.local/share/claude-sessions/queue-runner/logs/job_abc123.log
```

No `sm queue log` command is required for v1. Direct file access is simpler for local agents and avoids adding a streaming API before there is evidence it is needed.

Retention defaults:

1. `succeeded`: 24 hours
2. `failed` / `timed_out`: 7 days
3. `cancelled` / `displaced`: 1 hour
4. total log cap: 1 GB, oldest logs evicted first

Retention runs in a background maintenance task and must not run synchronously on startup.

## Config

Add:

```yaml
queue_runner:
  enabled: true
  state_dir: "~/.local/share/claude-sessions/queue-runner"
  max_running_jobs: 2
  perf_cooldown_seconds: 30
  cancel_grace_seconds: 10
  memory:
    min_free_bytes: 2147483648
    retry_interval_seconds: 10
  types:
    tests:
      max_concurrent: 2
      default_timeout_seconds: 900
    perf:
      max_concurrent: 1
      default_timeout_seconds: 2700
    background:
      max_concurrent: 2
      default_timeout_seconds: 3600
```

## Operator Visibility

`sm queue list` default output shows pending and running jobs for the current session. `--all` shows all sessions.

Columns:

```text
ID  Type  State  Notify  Label  Queued  Started  Runtime  Holding  Log
```

`sm watch` integration is optional for v1 implementation, but the data model should make it easy to add a compact queue summary later.

## Resource Observability

While at least one queue job is pending or running, SM should sample host load periodically for later analysis. This is not used for scheduling decisions in v1 except for the memory preflight gate; it is for post hoc reconstruction of contention.

Default sampling:

```yaml
queue_runner:
  resource_sampling:
    enabled: true
    interval_seconds: 15
```

Each sample records:

1. timestamp
2. pending job count by type
3. running job count by type
4. total running queue jobs
5. memory free / active / wired / compressed from macOS-native counters
6. CPU load average and process CPU percentage for queue job process groups when available
7. GPU load when a non-blocking macOS source is available; otherwise `null`

Sampling must run in a background task and must not block job admission, `/health`, or list/status APIs. Missing CPU/GPU fields are acceptable; the important v1 guarantee is a durable time series that correlates queue occupancy with memory and CPU pressure.

## Security and Safety

1. This feature executes arbitrary local commands from already-authorized local agents. It does not introduce remote command execution.
2. `cwd` must be absolute after CLI normalization.
3. Environment capture is explicit and small. The CLI passes `PATH`, `PYTHONPATH`, `VIRTUAL_ENV`, and repeated `--env` values; it does not copy the entire agent environment.
4. Cancellation targets the process group, not just the wrapper PID.
5. The scheduler and process reaper must not block the FastAPI event loop.

## Acceptance Criteria

1. `sm queue run --type tests -- python -m pytest ...` accepts a job and returns immediately with id, state, and log path.
2. If slots are free and gates pass, the job starts without agent involvement.
3. If slots are full or gates fail, the job remains pending and the requester is notified only if it did not start immediately.
4. Completion always wakes the requester with terminal state, exit code, runtime, queue time, and log path.
5. `sm queue list/status/cancel` work for pending and running jobs.
6. Running jobs survive SM restart when their OS process survives.
7. Restart recovery completes without delaying `/health` readiness on large historical logs.
8. Tests cover CLI parsing for argv and stdin script modes, scheduler admission, cancellation, timeout, restart recovery, and completion notifications.
9. Tests cover that resource sampling starts when the queue becomes non-empty, stops when it drains, and tolerates unavailable optional metrics.

## Deferred

1. Custom workload types beyond the three built-ins.
2. `sm queue log` streaming convenience command.
3. `sm wait-task` or any blocking wait command.
4. Automatic retry/resubmit for displaced or failed jobs.
5. CPU/GPU load gates.
6. `sm watch` full queue panel.
7. Persona or memory-rule edits that tell all agents to use the queue by default.

## Classification

Single implementation ticket after spec approval.

The v1 scope is broad but cohesive: one queue manager, one CLI/API group, one scheduler, and one process wrapper. The deferred items keep it small enough for one maintainer implementation PR without requiring an epic split.
