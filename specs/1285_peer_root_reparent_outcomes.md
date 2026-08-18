# 1285 - Peer-root succession and truthful reparent outcomes

Status: implementation contract

Issue: #1285

## Incident evidence

On 2026-08-18, owner-started peer roots `459a100f` and `d7216556` could not
use `sm reparent-tree`: the successor was not a direct child. Seven manual
single-edge moves then raced. `ce188350bb5a` returned `500 reparent apply lease
disappeared` twice despite later reaching `applied`; `368eda611f75` was
advertised as pending, then returned a bare 404 after same-edge request
`f7d87ce301e2` won. The safe completion proof was committed child topology:
the outgoing root had no children and the successor had the expected child set.

## Contract

`sm reparent-tree --to <successor> <outgoing>` supports either the existing
direct-child promotion or a peer-root succession. The peer form is valid only
when both source and target are live roots. It freezes source's live children
and, after source/target consent, atomically makes the outgoing source and
every frozen live child direct children of the successor. The successor remains
a root; stopped children keep their historical parent. The persisted request
identifies this mode and restart revalidation requires the successor to remain
a root.

Apply drivers are serialized in-process in addition to the durable global
lease. A second approval/reconciliation must return the durable terminal state,
not mistake a lease released by the first completed driver for a failed apply.
Post-quiesce ambiguity remains durable, failed, and repair-gated.

If an already-advertised same-edge request loses to an applied request, it is
retained as terminal `superseded`, names `superseded_by_request_id`, and returns
an informative conflict on an attempted decision. It must never become a 404.
Completion consumers use committed topology, not notification delivery.

## Classification

Single ticket.
