# #1324 Codex-fork restore acceptance

## Measured evidence

On 2026-08-18, historical root `d0d5f272` was restored with the durable
Codex resume identity `01a0028b-c5d3-75c1-8df6-4b40b2eb2d48` after its account
changed. The provider emitted structured `session_start`, then `session_end`
about 140 ms later, and the tmux/provider runtime was absent. The API still
projected it as running/idle. Terminal output is not evidence for this change.

The current durable state retains the historical record as stopped and its
successor is `9ffe4f64`. The short-lived event rows had already been pruned
from the bounded store when this investigation began.

Source trace at `origin/main` `414a704380b136cad9ec5a9b2b36e6c5a9798614`:
`SessionManager.restore_session()` treated successful
`tmux.create_session_with_command()` as the success boundary, then immediately
cleared stopped metadata and set `status=running` before the codex-fork event
monitor could observe provider acceptance or exit.

An isolated fake provider command confirmed that boundary against the unpatched
base: its tmux launch returned true, it wrote structured `session_start` and
`session_end`, and its tmux runtime was absent; `restore_session()` still
returned success and projected `running` / `working`. No terminal capture was
read. The patched equivalent rejects the same stream as stopped.

## Decision

For a codex-fork restore, persist a stopped, pending-acceptance marker before
launch. Admit the runtime only when the raw structured stream emits
`thread_started` for the exact durable resume id and tmux remains live through
the acceptance window. A `session_end`, provider error, missing/mismatched
identity, tmux disappearance, or timeout rejects the launch, tears down its
artifacts, and leaves the durable row stopped with an actionable reason. The
resume id is never rebound during this provisional phase.

On process restart, an unfinished acceptance marker is recovered as stopped,
not healed to idle/running, even if an ambiguous runtime artifact remains.

## Test contract

The isolated fake provider command writes structured JSONL and exits without
terminal text. Coverage includes account-bound rejection, immediate
`session_start`/`session_end`, missing thread start with tmux loss, mismatched
thread identity, valid live acceptance, restart recovery, and stopped
activity/attach projection. Remote restore coverage supplies the same matching
acceptance event through its bridge queue.

## Boundaries

This is distinct from #1322's recredential admission/wait behavior and only
cross-links their shared stale-runtime truthfulness family. No provider retry
or fresh thread creation is introduced. No production restore, restart,
deployment, or live probe is authorized by this work item.

Rust tests are not run: #1317 is currently draft and #1316 is its open issue;
the required final reviewed head and maintainer integration authorization have
not been supplied. Python isolated tests and static checks are allowed.

## Classification

Single ticket: the bounded Python restore admission, recovery, projection, and
remote-bridge test surface can be completed in one agent context.
