# 1207 - Consent-based reparent and authority transfer

Status: implementation contract

Epic: #1207  
Incident and diagnosis: #1204  
Phases: #1209, #1208, #1211, #1210

## Summary

Session Manager derives destructive and supervisory authority from the durable
`parent_session_id` graph. A handoff can transfer responsibility to a successor,
but the Rust service has no working mutation surface that transfers that graph.
The Python `sm adopt` implementation remains in the repository, while the Rust
service only projects its legacy `adoption_proposals` records. The live Rust CLI
and API cannot create or decide them.

This epic adds two consent-based operations:

1. `reparent` moves one live session under a different live parent.
2. `reparent-tree` promotes one direct child to replace its parent as the
   orchestrator of the parent's live tree.

There is no generic inversion flag. Tree promotion is a named operation with a
fixed topology, consent set, and audit trail.

## User contract

### One-edge transfer

```text
sm reparent request <child> --to <new-parent>
sm reparent approve <request-id>
sm reparent reject <request-id>
sm reparent status [request-id]
sm adopt <child>
```

`sm adopt <child>` is exactly shorthand for requesting that `<child>` move to
the calling managed session. It does not bypass consent.

Only the current parent or proposed new parent may initiate. Creating the
request records the initiator's approval. The other party must explicitly run
`sm reparent approve`. If the child has no live current parent, a signed-in user
must supply the missing approval through `sm watch`.

A stopped or missing parent is treated as no live parent. It is recorded in the
expected topology for audit, but cannot approve.

### Tree promotion

```text
sm reparent-tree <source> --to <target>
sm reparent-tree <source> --to <target> --dry-run
```

`target` must be a live direct child of `source`. Given:

```text
grandparent -> source
source -> target
source -> child-a
source -> child-b
source -> stopped-child
```

the accepted transaction produces:

```text
grandparent -> target
target -> source
target -> child-a
target -> child-b
source -> stopped-child
```

The operation freezes the ordered set of live direct children at request
creation. Any change to the source parent, target edge, or frozen live-child set
makes the request stale. Stopped children preserve historical lineage.

Only the source, target, or source's live parent may initiate a tree request.
The source and target must approve. When source has a live parent, that
grandparent must approve because its own direct-child edge changes. A root
source needs no extra human approval: source explicitly approves its own
demotion. If source has a recorded but non-live parent, user approval replaces
the unavailable grandparent approval.

`--dry-run` creates no durable request. It prints the exact edge changes,
routing changes, required approvers, and any condition that would prevent a
request from being created.

## Authentication and consent

Agent decisions require both the managed caller ID and a server-issued,
per-runtime 256-bit credential. Session Manager injects the opaque credential as
`SM_SESSION_CREDENTIAL` when it launches or restores a seat, stores only its
SHA-256 verifier in session state, and rotates it on every restore. The Rust CLI
sends the credential in `X-SM-Session-Credential`; the server derives the actor
from the matching credential and rejects a payload whose
`requester_session_id` disagrees. `SESSION_MANAGER_ID` alone is not
authentication. An operator shell that merely sets another session ID cannot
impersonate an agent approval.

The same per-session credential gate applies to request creation. Credential
material is never returned by session/list APIs, written to logs, included in
notifications, or persisted in plaintext. Existing live sessions without a
verifier cannot approve after deployment until Session Manager rotates a
credential into that runtime; rollout must provide an explicit credential
rotation path and must not fall back to trusting an ID-only payload.

Session Manager agents share one operating-system account and filesystem. A
hostile same-UID process with arbitrary process-inspection capability is outside
this control's isolation boundary; protecting against that requires OS-level
per-seat identities. This credential still provides server-verifiable request
binding and prevents operator-shell and accidental cross-seat payload spoofing.

Human decisions are accepted only on an authenticated operator route used by
`sm watch`. Local-bypass watch is the supported operator-shell equivalent. A
human decision records the authenticated actor where available and the access
mode otherwise.

Required approvals are identities, not roles or aliases. Role reassignment or
friendly-name changes do not change an outstanding request's consent set.

Every required approver can reject. A rejection is terminal. Approvals and
rejections are idempotent only when the same actor repeats the same decision;
conflicting or unauthorized decisions fail.

Requests expire after 24 hours by default. Expiration and staleness are evaluated
under the session-state write lock before every read that claims actionability
and before every decision or pre-quiesce apply attempt. An `applying` request at
or beyond `routing_quiesced` never expires; recovery must finish its immutable
transaction.

## Durable model

The session state gains a top-level `reparent_requests` array. Each record has:

```text
id
kind                       single | tree
subject_session_id         child for single, source for tree
target_parent_session_id   new parent for single, target for tree
expected_parent_session_id nullable
frozen_live_child_ids      [] for single
initiator_session_id
required_agent_approvals
required_human_approval    boolean
approvals[]                actor kind/id, decision, timestamp
status                     pending | applying | applied | rejected |
                           stale | expired | failed
created_at
expires_at
decided_at
applied_at
failure_reason
topology_fingerprint
apply_stage                nullable | routing_quiesced |
                           authority_committed
apply_plan                 nullable while pending; immutable once applying
```

`apply_plan` is versioned and contains the complete retry input rather than a
query that recovery would rerun:

```text
version
edge_changes[]             session ID, expected old parent, new parent
json_routing_changes[]     record kind/ID, expected old target, new target
queue_routing_changes[]    table/record ID, child ID, expected old target,
                           new target, prior active state
```

The transition from `pending` to `applying` discovers these exact records once
under the state lock and persists the plan before quiescing anything. Recovery
uses only this plan plus expected-value checks. A route created after planning
must derive its parent from the canonical graph and is not retroactively folded
into the in-flight plan.

The topology fingerprint is a canonical hash over operation kind, subject,
target, expected parent, and sorted frozen live children. Revalidation compares
both the fields and the hash; the hash is not a substitute for readable audit
data.

Legacy `adoption_proposals` are never auto-applied. On first mutation-aware
load, pending legacy records are projected as terminal `stale` reparent
requests with reason `legacy proposal requires a new consent request`, or are
left read-only while the new projection exposes the same reason. Either shape
must preserve the old data and prevent old one-party approvals from becoming
new authority.

## Validation

Request creation and apply both reject:

- empty, missing, stopped, or unsupported source/target sessions;
- self-parenting;
- a target already equal to the current parent;
- cycles, including a new parent inside the subject's descendant set;
- single-edge initiation by anyone except current or proposed parent;
- tree initiation by anyone except source, target, or source's live parent;
- tree promotion where target is not source's live direct child;
- duplicate active requests whose affected edge sets overlap;
- a topology that changed after request creation.

Apply revalidates all identities, liveness, topology, consent, and cycle
conditions while holding the session-state write lock.

## Routing and authority transaction

Changing only `parent_session_id` is incorrect. The following parent-derived
state must follow the new graph:

- `context_monitor_notify` when it points at the old parent;
- active parent-wake registrations in retained JSON and queue SQLite;
- pending parent-wake metadata attached to undelivered queue messages;
- child wait/completion monitors that captured a parent at spawn time;
- task-complete fallback routing;
- any additional old-parent-keyed state found by the phase audit.

Explicit notification recipients that happen to equal the old parent are not
retargeted unless their schema marks them as parent-derived. Reparenting must
not rewrite deliberate peer-to-peer messages.

Existing stop-notify `sender_session_id` values and provider
`subagents[].parent_session_id` values are explicit recipients or historical
provider lineage. They are preserved. Tool-usage parent fields are also
historical provenance and are never rewritten.

Parent-dependent runtime behavior should resolve the current parent at delivery
time where practical. Durable registrations that intentionally pin a parent are
retargeted during apply.

The JSON state file and queue SQLite cannot share an ACID transaction. Apply is
therefore a recoverable, fail-closed staged operation:

1. Under the session write lock, revalidate and persist `status=applying` with
   the complete immutable plan. Parent edges remain unchanged.
2. In one SQLite transaction, quiesce affected parent-derived wake registrations
   and pending wake metadata. Persist `apply_stage=routing_quiesced` in JSON.
3. In one atomic JSON replacement, update all parent edges and JSON-backed
   routing, and persist `apply_stage=authority_committed`. Dynamic
   task-complete and wait-monitor delivery now resolves this canonical edge.
4. In one idempotent SQLite transaction, retarget and reactivate affected
   parent-derived routes, then mark the JSON request `applied`.

An interruption before step 3 exposes no new destructive authority and leaves
old-parent routing paused rather than misdirected. An interruption after step 3
has new authority with parent-derived routing still paused; dynamic delivery
uses the canonical new edge. Startup and the next request-store mutation resume
`applying` records from their immutable plan. A retry never recomputes a
different topology. If the frozen topology no longer matches before routing is
quiesced, the request becomes stale. After quiescing, recovery completes the
recorded transaction or reports a durable `failed` state requiring operator
repair; it never silently rolls authority back to a topology that may already
have been observed.

No network delivery occurs while holding the write lock. Approval and outcome
notifications are queued after the durable state transition.

## Notification contract

Creation sends each missing agent approver an `important` message containing:

- request kind and ID;
- initiator;
- every edge that will change;
- expiry;
- exact approve and reject commands.

Each decision notifies the initiator and remaining approvers. Rejection,
expiration, staleness, failure, and completion send one terminal notification.
Notification delivery failure does not undo durable request state and can be
retried through the existing queue.

## HTTP API

```text
POST /sessions/{subject}/reparent-requests
POST /sessions/{source}/reparent-tree-requests
GET  /reparent-requests
GET  /reparent-requests/{request_id}
POST /reparent-requests/{request_id}/approve
POST /reparent-requests/{request_id}/reject
POST /reparent-requests/{request_id}/human-approve
POST /reparent-requests/{request_id}/human-reject
```

Creation payloads carry `requester_session_id`; decision payloads carry the same
for agent routes. Human routes ignore agent IDs and use request authentication.
List defaults to requests involving the caller or requiring human action.
Operator watch can list all pending human-gated requests.

HTTP status conventions:

- `200`: idempotent read/decision or dry-run;
- `201`: request created;
- `400`: malformed or structurally invalid operation;
- `403`: caller is not an authorized initiator/approver;
- `404`: session or request not found;
- `409`: stale topology, conflicting request, terminal decision, or cycle;
- `410`: expired request;
- `503`: durable state or routing store unavailable.

## CLI and watch UX

The Rust CLI owns all non-watch commands. It resolves aliases through the
existing API and always forwards the managed caller identity.

`sm reparent status` prints compact request rows by default and a complete edge
and approval plan for one ID. Terminal states include their reason.

The retained Python `sm watch` TUI replaces its legacy adoption projection with
pending reparent requests. `A` and `X` remain approve/reject, but only operate on
human-gated requests. The detail line shows source, target, operation, age,
expiry, and missing approvals. Multiple requests are selectable; watch never
approves an arbitrary first item without showing which request is selected.

## Role and history boundaries

Reparenting does not transfer service roles, aliases, provider sessions,
transcripts, account identity, working directory, task text, or stopped-child
history. Those are properties of seats, not parent authority.

Usage ancestry follows the new live graph for future reports. Historical usage
events retain the parent/seat metadata recorded when they occurred.

## Phases

### Phase 0 - #1209

Add the durable request schema, legacy migration/projection, topology planning,
consent state machine, expiry/staleness handling, read/decision HTTP API, and
focused tests. No request may apply yet.

### Phase 1 - #1208

Add single-edge apply, routing reconciliation, fail-closed recovery, and tests
that direct-child retire/kill authority and supervision follow the new edge.

### Phase 2 - #1211

Add tree-promotion planning/apply, dry-run, frozen child-set validation,
grandparent consent, stopped-child preservation, and recovery tests.

### Phase 3 - #1210

Add Rust CLI commands, actionable notifications, retained `sm watch` human UX,
and end-to-end tests.

Every phase PR targets `epic/1207-reparent-authority` and follows
`docs/working/pr_review_process.md`. After all phases merge, one reviewed epic
PR targets `main`. Partial phases are not deployed.

## Acceptance gates

- All consent combinations, unauthorized actors, repeated decisions, expiry,
  staleness, self-parenting, and cycles have deterministic tests.
- A regular reparent changes exactly one edge.
- A tree promotion produces exactly the documented live graph and preserves
  stopped lineage.
- Parent-derived routing and direct-child destructive authority follow the new
  graph immediately after apply.
- Pending and `applying` requests recover correctly after restart at every
  durable stage.
- Legacy pending adoption records cannot auto-apply.
- Rust CLI parser/dispatch and Python watch interaction tests pass.
- Full Rust and retained Python test suites pass, or any demonstrably preexisting
  failure is filed and referenced under the maintainer workflow.
- The final epic PR passes Codex review under the P1 exit criterion, merges to
  `main`, and is deployed only with `scripts/restart-rust-server.sh`.

## Classification

Epic. The work is split across four dependent tickets because the durable state
machine, cross-store authority mutation, tree transaction, and operator UX each
require separate reviewable proofs.
