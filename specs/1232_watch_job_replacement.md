# sm#1232: replace stale `watch-job` guidance

## Decision

Keep `sm watch-job` retired. The Rust queue runner is the supported durable
execution and completion-wake path:

```bash
sm queue run --type tests --label <label> --cwd <worktree> -- <command>
```

The current managed session is the default notify target. Callers outside a
managed session pass `--notify <session>`. When the job reaches a terminal
state, Session Manager queues an `[sm queue]` message containing the state,
exit code when available, runtime, queue time, and log path. Command output
stays in that log so the receiving agent can inspect it selectively.
The caller goes idle after submission; it does not poll or add another watch.
Terminal jobs retain a notification-pending marker until that message is
durably queued. A service-owned retry loop reattempts unnotified completions,
including across restarts, using an idempotent message ID.

Processes started outside `sm queue run` cannot be registered after launch.
Work that needs durable Session Manager supervision must be submitted through
the queue from the start.

## Compatibility

`sm watch-job ...` remains a nonzero retired command, but its error names the
exact queue replacement and automatic completion wake. This preserves the
owner-approved Rust cutover boundary without leaving callers at a dead end.

## Verification

Live smoke job `job_d3d77198345e` completed with exit code 0 and woke its notify
target with the expected `[sm queue]` terminal message. CLI coverage locks the
replacement guidance for every `watch-job` invocation, including `add --help`.
Recovery coverage proves that marked terminal jobs are retried exactly once and
that historical terminal rows are not replayed during migration.

## Classification

Single ticket.
