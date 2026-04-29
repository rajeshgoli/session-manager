# sm#680: rate-limited CI queue runs

## Scope

Add a CI-facing way to request a managed `sm queue` background run for a repository commit, while preventing push storms from flooding the local host.

Primary UX:

```bash
sm queue ci-run \
  --repo rajeshgoli/Fractal-Market-Simulator \
  --job full-pytest \
  --commit "$GITHUB_SHA" \
  --branch "$GITHUB_REF_NAME" \
  --min-interval 30m \
  --type tests \
  --cwd /Users/rajesh/worktrees/fractal-ci \
  --script-file .github/sm/full_pytest.zsh
```

Immediate response when admitted:

```text
Accepted CI queue run ciq_abc123 (full-pytest @ 1a2b3c4).
Queue job: job_def456
Log: /Users/rajesh/.local/share/claude-sessions/queue-runner/logs/job_def456.log
```

Immediate response when suppressed:

```text
Suppressed CI queue run for full-pytest @ 1a2b3c4: commit_already_admitted.
Last admitted: 2026-04-29T10:00:00-07:00
```

## Source Request

The user-defined rate-limit semantics are the source of truth:

> Only one job run per 15 minutes or unique commit id, whichever is less frequent.
>
> No more than one run every N minutes (say N=30) even if `sm queue` is free.
>
> No more than one run per commit even after 30 minutes.

Interpretation: admissions must pass both gates. The time gate makes runs no more frequent than one per configured interval. The commit gate makes each job key run at most once per commit. The composed policy is stricter than either gate alone.

## Problem

Consumer repos want CI to ask the local Session Manager host to run heavyweight validation after pushes. The existing `sm queue run` command can serialize local work, but it trusts every submitter. A CI workflow can trigger multiple times for one commit, for multiple branches, or for a rapid push series. Without a durable rate limit, those events can enqueue duplicate or excessive jobs and recreate the host contention that `sm queue` was introduced to prevent.

CI also needs durable results. If a validation starts failing, a consumer should be able to ask which commits had passing or failing local queue runs, then use that history to guide `git bisect` or decide which commit needs a rerun.

## Goals

1. Let CI request a queue run for a specific repo/job/commit.
2. Enforce per-job minimum interval and at-most-once-per-commit admission before creating the underlying queue job.
3. Persist gate decisions and run results across Session Manager restarts.
4. Expose lookup commands for one commit and for a commit range.
5. Keep the underlying execution model on the existing queue runner.
6. Make simultaneous duplicate submissions deterministic.

## Non-goals

1. Do not add a GitHub App, webhook server, or hosted CI service integration in v1.
2. Do not make Session Manager clone, fetch, or checkout repositories for the consumer.
3. Do not infer the commit SHA from git state; CI must pass it explicitly.
4. Do not auto-rerun suppressed commits later. Suppression is terminal for that request.
5. Do not provide distributed locking across multiple Session Manager hosts.
6. Do not replace `sm queue run`; this adds a rate-limited CI admission layer on top of it.

## CLI

Add a `sm queue ci-*` command family:

```bash
sm queue ci-run [options] -- COMMAND [ARG...]
sm queue ci-run [options] --script-file PATH
sm queue ci-status --repo REPO --job JOB --commit SHA [--json]
sm queue ci-history --repo REPO --job JOB [--branch BRANCH] [--limit N] [--json]
sm queue ci-range --repo REPO --job JOB --commits-file PATH [--json]
```

`ci-run` options:

```text
--repo OWNER/NAME              required; consumer repo identity
--job NAME                     required; stable job key within the repo
--commit SHA                   required; full or abbreviated commit SHA
--branch NAME                  optional; included in history/filtering, not in uniqueness by default
--min-interval DURATION        default from config, initially 30m
--type tests|perf|background   default: background for CI-triggered work
--label TEXT                   default: "<repo>:<job>@<short-sha>"
--cwd PATH                     required unless config supplies a job cwd
--timeout DURATION             passed to underlying queue job
--env KEY=VALUE                repeatable explicit environment additions/overrides
--notify SESSION_OR_ROLE       optional; default from config or maintainer
--dedupe-scope repo|branch     default: repo
```

Everything after `--` uses the existing `sm queue run` argv semantics. `--script-file` uses the existing stored-script execution model.

### Job Key

The canonical job key is:

```text
repo + job + dedupe_scope_value
```

For the default `--dedupe-scope repo`, `branch` is metadata only. A commit admitted for `rajeshgoli/Fractal-Market-Simulator/full-pytest` suppresses later submissions for the same commit on any branch.

For `--dedupe-scope branch`, the branch name becomes part of the key. This is useful for jobs where branch-specific environment or long-lived release branches should be tracked separately.

The spec recommends defaulting to `repo` because the user requirement says "unique commit id" and because commit-level dedupe across branch aliases prevents accidental duplicate runs when the same SHA appears on a PR branch and a merge queue branch.

## Admission Semantics

`sm queue ci-run` performs admission before creating an underlying queue job.

For one canonical job key and commit SHA:

1. Begin a SQLite write transaction.
2. If a prior admitted row exists for the same key and commit, suppress the request with reason `commit_already_admitted`.
3. If the most recent admitted row for the key is newer than `min_interval`, suppress the request with reason `time_gate`.
4. Otherwise create a CI run row with state `admitted`.
5. Create the underlying `queue_jobs` row in the same transaction boundary or with compensating failure handling.
6. Link the CI run row to the underlying queue job id.

Both gates must pass. "Whichever is less frequent" means the stricter composed policy wins.

Suppressed requests do not create queue jobs. They are still persisted as suppression rows for auditability and for diagnosing CI behavior.

## Concurrency

The CI gate store uses SQLite with a uniqueness constraint:

```sql
UNIQUE(job_key, commit_sha) WHERE decision = 'admitted'
```

Implementation should acquire admission with `BEGIN IMMEDIATE` so simultaneous requests serialize deterministically. If two requests for the same key/commit arrive at the same time, one creates the admitted row and queue job; the other returns `commit_already_admitted` and points to the already admitted run.

If two different commits arrive at the same time for the same key, the transaction that commits first wins the time gate. The later request sees the updated latest admitted timestamp and returns `time_gate`.

## Durable Storage

Use a new SQLite database under the queue runner state directory:

```text
~/.local/share/claude-sessions/queue-runner/ci_runs.db
```

Keeping this adjacent to `queue_runner.db` makes backup/inspection straightforward without growing the queue runner's hot scheduling table. The implementation may share a connection helper, but the data model should be separate.

### Tables

`ci_run_requests`:

```sql
id TEXT PRIMARY KEY,                 -- ciq_<hex>
job_key TEXT NOT NULL,
repo TEXT NOT NULL,
job_name TEXT NOT NULL,
dedupe_scope TEXT NOT NULL,          -- repo|branch
branch TEXT,
commit_sha TEXT NOT NULL,
requested_at TEXT NOT NULL,
decision TEXT NOT NULL,              -- admitted|suppressed
suppression_reason TEXT,             -- time_gate|commit_already_admitted
min_interval_seconds INTEGER NOT NULL,
queue_job_id TEXT,
notify_session_id TEXT,
label TEXT,
cwd TEXT,
command_json TEXT,
script_path TEXT,
metadata_json TEXT NOT NULL
```

Indexes:

```sql
CREATE UNIQUE INDEX idx_ci_runs_admitted_key_commit
  ON ci_run_requests(job_key, commit_sha)
  WHERE decision = 'admitted';
CREATE INDEX idx_ci_runs_key_requested ON ci_run_requests(job_key, requested_at);
CREATE INDEX idx_ci_runs_queue_job ON ci_run_requests(queue_job_id);
```

`ci_run_results`:

```sql
ci_run_id TEXT PRIMARY KEY,
queue_job_id TEXT NOT NULL,
job_key TEXT NOT NULL,
repo TEXT NOT NULL,
job_name TEXT NOT NULL,
branch TEXT,
commit_sha TEXT NOT NULL,
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
CREATE INDEX idx_ci_results_key_commit ON ci_run_results(job_key, commit_sha);
CREATE INDEX idx_ci_results_key_finished ON ci_run_results(job_key, finished_at);
CREATE INDEX idx_ci_results_repo_job ON ci_run_results(repo, job_name, finished_at);
```

## Result Recording

When a CI run is admitted, insert a result row with `status=pending` and the linked queue job id. The queue runner completion path updates that row when the underlying job reaches a terminal state.

If Session Manager restarts while the underlying queue job is running, existing queue recovery rules continue to own process recovery. The CI result projector reconciles from `queue_jobs` on startup and periodically while active jobs exist.

If the queue job is lost or cannot be reconciled after restart, mark the CI result `lost`. A lost result still counts as an admitted run for both gates unless the user chooses the alternate policy below.

### Crash Policy

Recommended policy: admission counts once the queue job is accepted, even if the process later crashes, times out, is cancelled, or is lost. This prevents a broken CI command from causing repeated local runs for the same commit.

Alternative policy for user feedback: only terminal `succeeded|failed|timed_out` count as "ran"; `cancelled|displaced|lost` may be manually rerun with an explicit override. This is more flexible but adds more state and operator judgment.

The implementation spec should default to the recommended policy unless user review asks for the alternate policy.

## Lookup API

### Single Commit

```bash
sm queue ci-status --repo rajeshgoli/Fractal-Market-Simulator --job full-pytest --commit 1a2b3c4
```

Output:

```text
full-pytest @ 1a2b3c4: failed exit=1 finished=2026-04-29T12:30:00-07:00
Queue job: job_def456
Log: /Users/rajesh/.local/share/claude-sessions/queue-runner/logs/job_def456.log
```

If no admitted result exists, return exit code `1` and print `No CI queue result for <job> @ <commit>`.

### History

```bash
sm queue ci-history --repo rajeshgoli/Fractal-Market-Simulator --job full-pytest --limit 50
```

Returns recent admitted runs and suppressed requests for that key. `--json` returns structured rows for dashboards or scripts.

### Commit Range

Session Manager should not run `git` for v1. The range command consumes an ordered commit list from the caller:

```bash
git rev-list --reverse "$GOOD_SHA..$BAD_SHA" |
  sm queue ci-range --repo rajeshgoli/Fractal-Market-Simulator --job full-pytest --commits-file -
```

This avoids making SM depend on local repo checkout freshness. The command reports known result rows and marks missing commits as `unknown`.

## API

Add HTTP endpoints parallel to the CLI:

```text
POST /queue-ci-runs
GET  /queue-ci-runs
GET  /queue-ci-runs/{id}
GET  /queue-ci-runs/status?repo=...&job=...&commit=...
POST /queue-ci-runs/range
```

`POST /queue-ci-runs` returns:

```json
{
  "id": "ciq_abc123",
  "decision": "admitted",
  "suppression_reason": null,
  "queue_job_id": "job_def456",
  "job_key": "rajeshgoli/Fractal-Market-Simulator:full-pytest",
  "commit_sha": "1a2b3c4...",
  "log_path": "..."
}
```

Suppressed requests return HTTP 200 with `decision=suppressed`, not an error. Invalid input returns 400. SM unavailability remains a client transport error.

## Configuration

Add optional per-job defaults:

```yaml
queue_runner:
  ci_jobs:
    "rajeshgoli/Fractal-Market-Simulator:full-pytest":
      min_interval_seconds: 1800
      type: tests
      cwd: /Users/rajesh/worktrees/fractal-ci
      notify: maintainer
      dedupe_scope: repo
      retention_days: 180
```

CLI flags override config for one request. If no config exists, callers must pass required execution fields explicitly.

The commit gate is always enabled and cannot be disabled in v1.

## Retention

Default retention:

1. Keep admitted result rows for 180 days.
2. Keep suppressed request rows for 30 days.
3. Do not delete queue logs independently in this feature; store log paths and rely on the queue runner's log retention policy.
4. If a log path is missing during lookup, report the result row and show `log_missing=true`.

Retention pruning must run in a background maintenance task, not on startup before uvicorn binds.

## Notifications

For admitted runs, reuse existing queue notifications for start/completion, but include CI metadata:

```text
[sm queue ci] full-pytest @ 1a2b3c4 completed: failed exit=1. Queue job: job_def456. Log: ...
```

Suppressed requests do not notify by default. CI receives the suppression response synchronously. A future operator setting can mirror suppressions to a session if that proves useful.

## Failure Modes

1. Duplicate same-commit request: return `suppressed/commit_already_admitted` and include the prior CI run id.
2. Different commit inside interval: return `suppressed/time_gate` and include `next_admissible_at`.
3. Queue job creation fails after CI admission row: mark CI row `suppressed/internal_queue_create_failed` or `lost` and return 500. The implementation should prefer one transaction boundary where possible.
4. SM restarts before job starts: pending queue job and CI row recover from durable storage.
5. SM restarts while job runs: queue runner recovers via PID/exit-code file; CI result projector reconciles from queue job state.
6. Log deleted: lookup still returns result metadata with `log_missing=true`.
7. Consumer passes invalid SHA/job/repo: reject with 400 before gate evaluation.

## Out Of Scope

1. GitHub Actions workflow templates for consumer repos.
2. GitHub Checks API publishing.
3. Auto-checkout or worktree management.
4. Manual force-rerun command. If needed, add `sm queue ci-rerun --commit SHA --reason TEXT` later with explicit audit fields.
5. Web UI or `sm watch` queue panels for CI history.
6. Cross-host dedupe.

## Acceptance Criteria

1. `ci-run` admits the first request for a job key/commit and creates one underlying queue job.
2. A second request for the same job key/commit is suppressed, even after the time interval.
3. A request for a different commit before the interval expires is suppressed by the time gate.
4. A request for a different commit after the interval expires is admitted.
5. Simultaneous duplicate requests produce exactly one admitted row.
6. Queue completion updates the durable CI result row.
7. `ci-status` can look up one commit result.
8. `ci-history` can show recent admitted and suppressed rows.
9. `ci-range --commits-file -` reports known and unknown commits in caller-provided order.
10. Startup does not run unbounded retention or result scans before the API binds.

## Ticket Classification

Single implementation ticket. The feature spans CLI/API/storage and queue runner integration, but it is cohesive and can be implemented by one maintainer agent with focused tests. Consumer repo workflow changes are separate downstream tickets.
