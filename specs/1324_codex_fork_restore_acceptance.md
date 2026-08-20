# #1324 Codex-fork restore acceptance

## Problem

The Rust restore lifecycle treated a successful tmux launch as a successful
Codex-fork resume. A provider that rejected the saved, account-bound root
thread could exit immediately while the durable session was already projected
as running.

## Approach

Keep a Codex-fork restore stopped and its runtime-launch record `launching`
until the newly created event stream publishes a root `thread_started` event
whose ID exactly matches the durable `provider_resume_id`. After that event,
require the tmux session to remain live through the configured runtime settle
window before applying the restore.

`session_end`, `shutdown`, a mismatched root thread, a missing root-thread
event, or missing tmux runtime fails the launch. The failure keeps the stored
resume identity, leaves the session stopped, and records an actionable reason.
Interrupted Codex-fork restore acceptance is ambiguous after process restart,
so recovery fails it conservatively rather than replaying the launch.

## Coverage

- exact valid root-thread acceptance and liveness window;
- immediate provider exit;
- `session_start` followed by `session_end` without `thread_started`;
- mismatched and absent root-thread identities;
- tmux disappearance;
- restart recovery; and
- stopped durable projection with an actionable error reason.

## Classification

Single ticket: the Rust restore lifecycle, durable launch state, and focused
hostile tests are bounded work that one agent can complete without a separate
epic.
