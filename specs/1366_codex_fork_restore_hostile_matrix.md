# #1324 Codex-fork restore hostile closure matrix

This document is the closure ledger for #1324. It records the behavior that
must remain true on current `main`; it does not define new shared lifecycle
semantics.

## Durable outcomes

| Boundary | Hostile outcome | Session projection | Launch | Credential/root | Runtime and retry |
| --- | --- | --- | --- | --- | --- |
| Explicit request, before teardown | exact target killed | stopped if the replacement launch fails | failed | replacement credential committed only after kill; durable root unchanged | old runtime gone; explicit retry remains possible |
| Explicit request, before teardown | authoritative target absence | replacement may launch | launching/applied or failed with provider result | replacement credential committed only after authoritative absence; root unchanged until exact acceptance | no old runtime; no prefix target is touched |
| Explicit request, before teardown | missing socket, permission, execution, or other transport failure | error, with terminal fields cleared | failed | old credential and durable root retained | launch nothing; ordinary retry fenced |
| Startup, pending authorized restore before credential commit | killed or authoritative absence | stopped with explicit manual-restore reason | failed | old credential/root retained | never replay provider; require explicit restore |
| Startup, pending authorized restore after credential commit/provider launch | killed or authoritative absence | stopped with explicit manual-restore reason | failed | replacement credential/root retained | exact teardown; never replay provider; require explicit restore |
| Startup, either credential boundary | unavailable runtime or inconclusive teardown | error, with terminal fields cleared | failed | currently persisted credential/root retained | never replay provider; ordinary retry fenced |
| Provider acceptance | exact durable root; `shutdown_complete` before/after | continue to settle | launching | replacement credential and durable root correspond | runtime must still pass settle |
| Provider acceptance | wrong root, missing/timeout, `session_end`, or `shutdown` | stopped after confirmed teardown; error if teardown is inconclusive | failed | replacement credential and durable root retained for the launched runtime | checked exact teardown; no false running state |
| Post-acceptance settle | exact target live through deadline | running | applied | replacement credential and exact durable root | attach descriptor supported |
| Post-acceptance settle | authoritative disappearance | stopped | failed | replacement credential/root retained | attach descriptor unsupported for stopped lifecycle |
| Post-acceptance settle | transport ambiguity, including unlinked live socket | error, with terminal fields cleared | failed | replacement credential/root retained | possibly-live runtime preserved; attach target remains structurally known but transport is unavailable until recovery |

## Executable evidence

The supported runner is `./scripts/test-rust-isolated.sh`. The closure run must
include the following groups from current `main`:

- `runtime::tests::restore_teardown_*`: no existence preflight, authoritative
  absence only, and transport ambiguity.
- `runtime::tests::real_tmux_restore_teardown_never_kills_a_prefix_match`:
  destructive targets use `=<session>`.
- `runtime::tests::real_tmux_missing_socket_is_inconclusive_both_cold_and_live`
  and `real_tmux_default_socket_is_inconclusive_both_cold_and_unlinked_live`:
  a missing pathname is not absence evidence.
- `runtime::tests::real_tmux_configured_socket_has_one_neutral_anchor_before_agents`:
  fixture and production named servers retain a neutral server argv.
- `sessions::tests::codex_fork_restore_request_*`: credential ordering,
  provider-launch fencing, exact teardown, and request retry behavior.
- `sessions::tests::codex_fork_restore_recovery_*`: both credential boundaries,
  killed/absent/inconclusive/disabled-runtime outcomes, preserved durable
  identity, and no provider replay.
- `sessions::tests::codex_fork_restore_root_acceptance_*`: exact identity,
  mismatch, timeout, trust handling, terminal events, and
  `shutdown_complete` churn.
- `sessions::tests::codex_fork_restore_projects_running_only_after_exact_root_acceptance`,
  `codex_fork_restore_rejects_mismatched_root_and_tears_down_runtime`, and
  `codex_fork_restore_rejection_fences_inconclusive_teardown_with_live_credential`:
  isolated real-tmux acceptance and rejection projections.
- `runtime::tests::restore_liveness_probe_is_exact_and_tri_state` and
  `sessions::tests::codex_fork_restore_liveness_requires_live_through_the_settle_window`:
  exact tri-state settle proof.
- `sessions::tests::codex_fork_restore_rejects_runtime_that_exits_during_settle`
  and `codex_fork_restore_fences_transport_ambiguity_during_settle`: isolated
  real-tmux settle failures.
- `read_only_http::runtime_core_lifecycle_uses_codex_fork_launch_config`:
  successful HTTP restore, status projection, provider arguments, fresh
  artifacts, and live attach target.

## Non-regression boundary

Generic `session_exists`, create, recredential, Claude/classic-Codex restore,
and unrelated lifecycle behavior are intentionally outside #1324. Known
full-suite fixture/concurrency debt is tracked separately and is not converted
into restore behavior: reminder schema setup, queue-authority stale-socket
timing (#1311), completion delivery concurrency (#1367 and #1378),
context-monitor path lifetime (#1369), and historical materialization timing
(#1379).
