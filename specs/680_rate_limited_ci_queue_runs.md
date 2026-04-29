# sm#680: configurable rate-limited queue runs

## Scope

Extend `sm queue` with a generic configured admission policy for background queue submissions. The immediate consumer is CI, but the Session Manager feature should stay at the infrastructure level: SM admits or suppresses a queued command based on a named policy, not on project-specific concepts such as Fractal Market Simulator branches, tests, or GitHub workflow names.

Primary UX:

```bash
sm queue ci-run \
  --policy fractal-ci \
  --dedupe-token "$GITHUB_SHA" \
  --label "full-pytest@$GITHUB_SHA" \
  -- \
  bash -lc 'source venv/bin/activate && PYTHONPATH=. python -m pytest tests/ -q'
```

Immediate response when admitted:

```text
Accepted policy queue run qpol_abc123 (policy=fractal-ci token=1a2b3c4).
Queue job: job_def456
Log: /Users/rajesh/.local/share/claude-sessions/queue-runner/logs/job_def456.log
```

Immediate response when suppressed:

```text
Suppressed policy queue run for fractal-ci: time_gate.
Next admissible at: 2026-04-29T10:30:00-07:00
```

## Source Request

The original user-defined rate-limit intent was:

> No more than one run every N minutes (say N=30) even if `sm queue` is free.
>
> No more than one run per commit even after 30 minutes.

The generalized interpretation is: each policy can enable a time gate, a bounded dedupe-token gate, or both. When both are enabled, a request must pass both gates before SM creates the underlying queue job. This composes to make admissions rarer, not more frequent.

## Problem

`sm queue run` serializes local execution but accepts every valid request. CI and other automation can fire repeatedly during bursts: repeated workflow events for the same commit, many rapid pushes, or retries while the local host is busy. Without a durable admission policy, those requests can create a backlog of redundant background work.

Session Manager should offer a reusable infrastructure primitive: named policy plus queued command. Consumers configure the policy once in `config.yaml`, then pass a small amount of request metadata at submission time. SM applies the configured gates and records the outcome durably enough for later lookup.

## Goals

1. Add a generic `sm queue ci-run` admission wrapper over existing `sm queue run` execution.
2. Configure rate limiting by named policy in `config.yaml`.
3. Support one configurable minimum elapsed time between admitted runs per policy.
4. Support bounded dedupe by a caller-supplied token, such as a commit SHA.
5. Allow policies to enable time-only, token-only, or combined time+token gates.
6. Persist admission decisions and run results across Session Manager restarts.
7. Keep the underlying scheduling, resource gates, logs, and process management in the existing queue runner.
8. Make simultaneous submissions deterministic.

## Non-goals

1. Do not add project-specific fields such as `--repo`, `--branch`, or `--job` to the infrastructure primitive in v1.
2. Do not add a GitHub App, webhook server, or hosted CI integration.
3. Do not make Session Manager clone, fetch, checkout, or inspect git repositories.
4. Do not infer dedupe tokens from git state; callers must pass them explicitly if the policy requires them.
5. Do not maintain an infinite per-commit history solely for dedupe.
6. Do not auto-rerun suppressed requests later.
7. Do not provide distributed locking across multiple Session Manager hosts.
8. Do not replace `sm queue run`; this adds a policy gate in front of it.

## Configuration

Add `queue_runner.policies` to `config.yaml`:

```yaml
queue_runner:
  policies:
    fractal-ci:
      type: tests
      min_interval_seconds: 1800
      dedupe:
        mode: both              # none|time|token|both
        token_window: 10        # remember last K admitted tokens
      cwd: /Users/rajesh/worktrees/3175-epic-to-dev
      timeout_seconds: 1800
      retention:
        admitted_runs: 200
        suppressed_runs: 200
```

Policy fields:

| Field | Required | Meaning |
| --- | --- | --- |
| `type` | no | Underlying queue type: `tests`, `perf`, or `background`. Defaults to `background` for policy runs. |
| `min_interval_seconds` | required for `mode=time|both` | Minimum elapsed time between admitted runs for this policy. |
| `dedupe.mode` | no | `none`, `time`, `token`, or `both`. Defaults to `both` when `min_interval_seconds` and `token_window` are present; otherwise defaults to the gates that have enough config. |
| `dedupe.token_window` | required for `mode=token|both` | Number of recent admitted dedupe tokens to remember. Small values such as 10 are expected. |
| `cwd` | no | Default working directory for admitted commands. CLI may override. |
| `timeout_seconds` | no | Default timeout for the underlying queue job. CLI may override. |
| `retention.admitted_runs` | no | Number of admitted policy-run rows to retain per policy. |
| `retention.suppressed_runs` | no | Number of suppressed request rows to retain per policy. |

Notification routing is not policy configuration. The runtime submitter owns the notification target: if a live managed agent submits the run, SM records that session id and wakes it on start/completion while it remains routable. If CI or another non-agent process submits the run, no default wake target is inferred.

The policy name is an infrastructure key. `fractal-ci` is just an operator-defined name, not a built-in project concept.

## CLI

Add a policy-run command family under `sm queue`:

```bash
sm queue ci-run --policy POLICY [options] -- COMMAND [ARG...]
sm queue ci-run --policy POLICY [options] --script-file PATH
sm queue ci-status --policy POLICY [--id RUN_ID | --dedupe-token TOKEN] [--json]
sm queue ci-history --policy POLICY [--limit N] [--include-suppressed] [--json]
```

`ci-run` options:

```text
--policy NAME                  required; configured policy key
--dedupe-token TEXT            required when policy dedupe mode includes token
--label TEXT                   default: "<policy>@<short-token>" when token is present
--cwd PATH                     override policy cwd
--timeout DURATION             override policy timeout
--type tests|perf|background   override policy type only if policy allows overrides
--env KEY=VALUE                repeatable explicit environment additions/overrides
--metadata KEY=VALUE           repeatable opaque caller metadata for lookup/debug
```

Everything after `--` keeps existing `sm queue run` argv semantics. `--script-file` keeps the existing stored-script execution model.

The command name is `ci-run` because the first expected caller is CI, but the semantics are generic policy-based queue admission. If the name feels too CI-specific during review, the implementation can use `sm queue policy-run` with `ci-run` as an alias.

## Admission Semantics

`sm queue ci-run` performs admission before creating an underlying queue job.

For one policy:

1. Load the policy config.
2. Validate required request fields. If token dedupe is enabled, `--dedupe-token` is required.
3. Begin a SQLite write transaction with `BEGIN IMMEDIATE`.
4. Evaluate the configured gates against admitted runs for that policy.
5. If any enabled gate fails, persist a suppressed request row and return `decision=suppressed` without creating a queue job.
6. If all enabled gates pass, persist an admitted policy-run row and create one underlying `queue_jobs` row.
7. Link the policy-run row to the queue job id and return `decision=admitted`.

### Time Gate

If `dedupe.mode` is `time` or `both`, compare the latest admitted run for the policy to `min_interval_seconds`.

A request is suppressed with reason `time_gate` when:

```text
now - latest_admitted_at < min_interval_seconds
```

The response includes `next_admissible_at`.

### Token Gate

If `dedupe.mode` is `token` or `both`, compare `--dedupe-token` to the last `token_window` admitted tokens for the policy.

A request is suppressed with reason `dedupe_token` when the token appears in that bounded recent-token set.

This deliberately does not enforce infinite commit uniqueness. It prevents duplicate retry storms and recent repeated commits while keeping state bounded. If a consumer needs stronger long-term lookup, it can use result history retention separately from admission dedupe.

### Combined Gate

If `dedupe.mode` is `both`, both gates must pass. A new token inside the time window is suppressed by `time_gate`; an old token outside the time window but still inside the recent-token window is suppressed by `dedupe_token`.

If multiple gates fail, return the most actionable reason in this order:

1. `dedupe_token`
2. `time_gate`

The response may include all failed gates in `failed_gates` for JSON callers.

## Queue Interaction

Admitted policy runs create normal queue jobs with policy-derived defaults. They then follow existing queue behavior:

1. They may remain pending until queue capacity/resource gates pass.
2. They may be displaced only if their underlying queue type can be displaced.
3. They reuse existing logs, wrapper scripts, exit-code files, and completion notifications.
4. They recover through the existing queue runner restart path.

This means the policy layer controls admission frequency, not exact start time. The time gate is based on admission time, not process start time, because admission is the point where SM promises that a run will occur unless the queue job later fails, is cancelled, or is displaced.

## Concurrency

SQLite admission uses a transaction-level lock:

1. `BEGIN IMMEDIATE` serializes writers.
2. Gate evaluation and insertion happen in the same transaction.
3. Simultaneous requests for the same policy observe a deterministic order.

For duplicate token submissions, exactly one can be admitted within the recent-token window. Later requests persist as suppressed rows.

For different tokens submitted simultaneously under a time-gated policy, the first committed request wins; later requests see the updated latest admitted timestamp and suppress on `time_gate`.

## Durable Storage

Use a new SQLite database under the queue runner state directory:

```text
~/.local/share/claude-sessions/queue-runner/policy_runs.db
```

Keeping policy admission state adjacent to `queue_runner.db` makes local inspection and backup simple while keeping policy-specific tables out of the queue runner's hot scheduling table.

### Tables

`queue_policy_runs`:

```sql
id TEXT PRIMARY KEY,                 -- qpol_<hex>
policy TEXT NOT NULL,
decision TEXT NOT NULL,              -- admitted|suppressed
suppression_reason TEXT,             -- time_gate|dedupe_token|invalid_policy|queue_create_failed
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
```

Indexes:

```sql
CREATE INDEX idx_policy_runs_policy_requested ON queue_policy_runs(policy, requested_at);
CREATE INDEX idx_policy_runs_policy_admitted ON queue_policy_runs(policy, admitted_at);
CREATE INDEX idx_policy_runs_policy_token ON queue_policy_runs(policy, dedupe_token, admitted_at);
CREATE INDEX idx_policy_runs_queue_job ON queue_policy_runs(queue_job_id);
```

`queue_policy_results`:

```sql
policy_run_id TEXT PRIMARY KEY,
queue_job_id TEXT NOT NULL,
policy TEXT NOT NULL,
dedupe_token TEXT,
status TEXT NOT NULL,                -- pending|running|succeeded|failed|timed_out|cancelled|displaced|lost
exit_code INTEGER,
queued_at TEXT NOT NULL,
started_at TEXT,
finished_at TEXT,
log_path TEXT,
artifact_json TEXT NOT NULL,
updated_at TEXT NOT NULL
```

Indexes:

```sql
CREATE INDEX idx_policy_results_policy_token ON queue_policy_results(policy, dedupe_token);
CREATE INDEX idx_policy_results_policy_finished ON queue_policy_results(policy, finished_at);
```

## Result Recording

When a policy run is admitted, insert a result row with `status=pending` and the linked queue job id. The queue runner completion path updates that row when the underlying job reaches a terminal state.

If Session Manager restarts while the underlying queue job is running, existing queue recovery rules continue to own process recovery. A lightweight policy-result reconciler reads linked `queue_jobs` rows after startup and periodically while active policy jobs exist.

If the queue job is lost or cannot be reconciled after restart, mark the policy result `lost`.

## Crash And Cancellation Policy

Recommended policy: an admitted run consumes the time gate and token slot once the queue job is accepted, even if the process later fails, times out, is cancelled, displaced, or lost. This prevents broken commands or flapping CI from repeatedly hammering the host.

If a user needs a rerun, that should be a separate explicit override feature later, for example:

```bash
sm queue ci-run --policy fractal-ci --dedupe-token SHA --force --reason "rerun after infra fix" -- ...
```

`--force` is out of scope for v1 because it needs explicit audit semantics.

## Lookup API

### Single Run Or Token

```bash
sm queue ci-status --policy fractal-ci --dedupe-token 1a2b3c4
sm queue ci-status --policy fractal-ci --id qpol_abc123
```

Output:

```text
fractal-ci token=1a2b3c4: failed exit=1 finished=2026-04-29T12:30:00-07:00
Queue job: job_def456
Log: /Users/rajesh/.local/share/claude-sessions/queue-runner/logs/job_def456.log
```

If no admitted result exists for a token, return exit code `1` and print `No policy result for <policy> token <token>`.

### History

```bash
sm queue ci-history --policy fractal-ci --limit 50 --include-suppressed
```

Returns recent admitted policy runs and optionally suppressed requests. `--json` returns structured rows for dashboards or scripts.

### Range-Like Lookup

Session Manager should stay generic and should not run `git` for v1. For commit-oriented consumers, range analysis is caller-driven:

```bash
git rev-list --reverse "$GOOD_SHA..$BAD_SHA" |
  xargs -I{} sm queue ci-status --policy fractal-ci --dedupe-token {}
```

A convenience batch command can be added without git awareness:

```bash
sm queue ci-history --policy fractal-ci --tokens-file commits.txt --json
```

This reports known result rows and marks missing tokens as `unknown`.

## API

Add HTTP endpoints parallel to the CLI:

```text
POST /queue-policy-runs
GET  /queue-policy-runs
GET  /queue-policy-runs/{id}
GET  /queue-policy-runs/status?policy=...&dedupe_token=...
```

`POST /queue-policy-runs` returns:

```json
{
  "id": "qpol_abc123",
  "policy": "fractal-ci",
  "decision": "admitted",
  "suppression_reason": null,
  "failed_gates": [],
  "queue_job_id": "job_def456",
  "dedupe_token": "1a2b3c4...",
  "log_path": "..."
}
```

Suppressed requests return HTTP 200 with `decision=suppressed`, not an error. Invalid input returns 400. SM unavailability remains a client transport error.

## Retention

Retention is per policy and count-based by default, because the dedupe token window is intentionally bounded:

1. Keep the latest `retention.admitted_runs` admitted rows per policy. Default: 200.
2. Keep the latest `retention.suppressed_runs` suppressed rows per policy. Default: 200.
3. Keep at least `dedupe.token_window` admitted rows regardless of retention settings.
4. Do not delete queue logs independently in this feature; store log paths and rely on the queue runner's log retention policy.
5. If a log path is missing during lookup, report the result row and show `log_missing=true`.

Retention pruning must run in a background maintenance task, not on startup before uvicorn binds.

## Notifications

For admitted runs, reuse existing queue notifications for start/completion, but include policy metadata:

```text
[sm queue policy] fractal-ci token=1a2b3c4 completed: failed exit=1. Queue job: job_def456. Log: ...
```

Suppressed requests do not notify by default. Automation receives the suppression response synchronously. For admitted runs, the notification target is captured from the submitting managed agent when there is one. If the submitter is not a live managed agent, the run still records durable status and logs, but no wake is attempted.

## Failure Modes

1. Missing policy: reject with 400 and do not persist a run row unless audit logging for invalid policy requests is explicitly enabled later.
2. Missing dedupe token for token-gated policy: reject with 400 before gate evaluation.
3. Duplicate recent token: return `suppressed/dedupe_token` and include the prior policy run id.
4. Time window active: return `suppressed/time_gate` and include `next_admissible_at`.
5. Queue job creation fails after policy admission row: mark the policy row `suppressed/queue_create_failed` or `lost` and return 500. Implementation should prefer one transaction boundary or compensating update.
6. SM restarts before job starts: pending queue job and policy result recover from durable storage.
7. SM restarts while job runs: queue runner recovers via PID/exit-code file; policy result reconciler updates from queue job state.
8. Log deleted: lookup still returns result metadata with `log_missing=true`.

## Out Of Scope

1. GitHub Actions workflow templates for consumer repos.
2. GitHub Checks API publishing.
3. Auto-checkout, worktree management, or git range computation.
4. Manual force-rerun command.
5. Web UI or `sm watch` policy-run panels.
6. Cross-host dedupe.
7. Infinite dedupe history.

## Acceptance Criteria

1. `ci-run --policy P` admits the first request when all enabled gates pass and creates one underlying queue job.
2. With `mode=time`, a second request before `min_interval_seconds` is suppressed by `time_gate`.
3. With `mode=token`, a request whose token appears in the last `token_window` admitted tokens is suppressed by `dedupe_token`.
4. With `mode=both`, both gates are enforced.
5. Simultaneous requests for one policy produce deterministic admitted/suppressed decisions.
6. Queue completion updates the durable policy result row.
7. `ci-status` can look up one policy run by id or dedupe token.
8. `ci-history` can show recent admitted and suppressed rows.
9. Retention never prunes below the configured dedupe token window.
10. Startup does not run unbounded retention or result scans before the API binds.

## Ticket Classification

Single implementation ticket. The feature is a generic policy-admission wrapper over the existing queue runner with bounded storage and focused CLI/API additions. Consumer repo workflow changes are separate downstream tickets.
