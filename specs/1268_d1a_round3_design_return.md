# 1268 D1A round-3 design return

**PR:** #1297 at review head `beaf4e2b51aaf1254589cd3f12c8b33bd8f90224`  
**Tier:** R3 — persisted schema/lifecycle, authority bindings, atomic admission,
and restart/corruption behavior.  
**Disposition:** blocked at the three-round R3 cap; do not merge this PR.

## Review ledger

| Round | Requested/reviewed head | Scope | Result | Disposition |
|---|---|---|---|---|
| 1 | `220200f6129dded460b72ec36fd74c5613e69e85` | Broad schema/load authority | P1 schema-object drift; cross-request override rebinding; raw capacity claims | Batched in `8705533` |
| 2 | `8705533c343e64acf2209f01b5053f03398405d5` | Root-cause sibling paths | P1 same-request override rebinding; raw lifecycle mutation; unsigned active pointer | Batched in `c40c49d` |
| 3 | `beaf4e2b51aaf1254589cd3f12c8b33bd8f90224` | Enabled-path, rollback, retry, and lifecycle confirmation | Three reachable P1s | Cap reached; no further patching |

The current PR review protocol permits no fourth automatic review. The v1
schema/load work itself passed its hostile corpus, but the retained D1 monolith
also makes decision/lease behavior reachable. That has exceeded the bounded A
review surface.

## Round-3 P1 findings

1. An ordinary retry after a lease becomes committed, released, or expired
   falls through from the persisted terminal decision. It can then fail on
   uniqueness/capacity or create another child. The operative scaffold requires
   ordinary retries to reuse a terminal decision.
2. `mark_child_launched` treats a missing, released, or expired reservation as
   a successful no-op. D3 could therefore treat an unreserved child as launched.
3. Equal-rank rules that differ only in `overridable` are not conflicting. This
   is the explicitly deferred equal-rank enforcement-field check from D1B.

## Required split

Retain the D1A schema-frozen, canonical-loader artifact as its own bounded
package, but remove the decision-kernel and lease/override lifecycle paths from
its reachable surface. The follow-on D1B package owns all three findings:

- terminal-decision retry/idempotency across `active`, `committed`, `released`,
  and `expired` leases;
- strict `mark_child_launched` reservation state and D3 handoff; and
- equal-rank conflict comparison of `overridable` and every other
  enforcement-affecting field.

D1B must start from an explicit contract for retained decision reuse, then
exercise the retry/launch/release matrix under its own R3 review budget. D1A
does not gain a fourth review round or a silent decision/lifecycle repair.

## Evidence and gates

- Final Codex review: PR #1297 discussions `r3800596331`, `r3800596338`, and
  `r3800596345`.
- At the final reviewed head: `cargo test -p sm-server`, `cargo fmt --check`,
  and `git diff --check` passed before review.
- The focused hostile corpus covers v1 schema/version/truncation checks,
  JSON/row binding per persisted JSON-backed record, restart round trips, and
  lifecycle/pointer rebinding. It does not resolve the three semantic P1s
  above.
