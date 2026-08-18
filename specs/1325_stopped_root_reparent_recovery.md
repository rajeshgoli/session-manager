# 1325 - Stopped-root reparent recovery

Issue: #1325 (regression in #1285)

## Measured incident

`9ffe4f64` dry-ran `sm reparent-tree d0d5f272 --to 9ffe4f64` at
`2026-08-18T16:23:09.823Z`. It exited 0 while projecting a target-to-root
edge, source-to-target edge, and exactly five live source children. It also
reported that the source was stopped and target did not meet the old direct
child/peer-root topology. The matching apply failed at
`2026-08-18T16:23:22.247Z` with HTTP 400 `source session d0d5f272 is stopped`.
No edge changed.

Durable state showed stopped root `d0d5f272`; five live children
`77de8fd5`, `37b926be`, `a2242bc3`, `07664488`, and `f4e86f39`; stopped child
`711debab`; and live successor `9ffe4f64` under maintainer `031de889`.

## Contract

The persisted `stopped_root_recovery` mode applies only to a durably stopped
root, a live consent-capable successor, and a live consent-capable current
successor parent that is the durable `maintainer` registration. The target must
have no children. Its credential starts the request; its current maintainer
parent gives the second ordinary credential-bound approval. There is no new
synchronous human gate: the durable maintainer registration and its recorded
approval are the recovery authority.

The immutable plan binds both old parents and exactly the frozen live source
children:

1. successor: current maintainer parent to root;
2. stopped source: root to successor;
3. every frozen live child: source to successor.

Stopped historical children do not move. A changed parent, source terminal
state, target liveness, target child set, frozen source-child set, missing
maintainer binding, pending overlap, or fingerprint mismatch fails closed
before authority commit. Preview returns the same rejection class as apply;
it never emits a successful actionable plan with blockers.

The existing durable apply plan and stage machine retain all-or-nothing routing
quiesce/rollback, restart recovery, and idempotent terminal-state recovery.

## Verification gate

Hostile isolated coverage is added for exact preview/apply parity and
post-state, stopped-child exclusion, changed child set/zero partial edges,
overlapping pending request, live source, terminal target, target children,
and restart continuation. Rust tests are deliberately not executed until PR
#1317 has final reviewed isolation acceptance and the maintainer provides the
integration handoff.

## Classification

Single ticket.
