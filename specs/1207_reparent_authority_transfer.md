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
sm reparent repair <request-id> --resume
sm reparent repair <request-id> --rollback-precommit
sm recredential <session>
sm recredential --all-live
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
verifier cannot approve after deployment until Session Manager relaunches that
runtime with a credential. The operator-authenticated `sm recredential` command
provides that path; it never falls back to trusting an ID-only payload.

Recredentialing is a durable, idle-only relaunch of the same seat and provider
thread, not an attempt to mutate a running process environment. Before stopping
anything, the server must prove the provider has resumable identity, persist a
rotation record, acquire the session input fence, and verify that the provider
turn is idle and its input queue is drained. Idle proof must be observed after
the rotation was requested (a fresh Claude prompt/Stop boundary or Codex
lifecycle event), not inferred from a stale projected status. Busy or unproven
seats remain `waiting_idle` and are retried by the provider Stop/event path.
The relaunch preserves the Session Manager ID, parent graph, task metadata, and
provider resume ID; it rotates the runtime credential and verifier. A crash
after the old runtime is stopped resumes the recorded relaunch on startup. If
resumability or readiness
cannot be established, the old runtime remains untouched and the rotation
fails closed. There is no force-active mode in this epic.

Startup completes every `relaunching` credential rotation synchronously before
HTTP/input delivery becomes ready. If the named tmux runtime exists, recovery
kills it and relaunches once from the frozen provider identity; this safely
handles a crash after launch but before the new verifier was persisted. Waiting
rotations start background idle-proof workers only after startup recovery.

Deployment does not silently relaunch the fleet. After the merged server is
healthy, the maintainer runs `sm recredential --all-live`; the command reports
which seats were rotated, waiting for idle, already credentialed, or failed.
Until a legacy seat completes this bootstrap, reparent commands fail with the
exact `sm recredential <session>` remediation. Newly created and normally
restored seats already receive credentials and need no bootstrap.

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
or beyond `json_routing_quiesced` never expires; recovery must finish its
immutable transaction.

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
                           stale | expired | failed | repaired
created_at
expires_at
decided_at
applied_at
failure_reason
topology_fingerprint
apply_stage                nullable | json_routing_quiesced |
                           routing_quiesced | authority_committed |
                           repair_rolled_back
apply_plan                 nullable while pending; immutable once applying
notification_intents[]     deterministic event/recipient delivery records
deferred_routing_intents[] idempotency key, operation payload, created_at,
                           replayed_at and resolved parent
repair_history[]           actor, action, prior failure, timestamp,
                           verified post-state fingerprint
```

`apply_plan` is versioned and contains the complete retry input rather than a
query that recovery would rerun:

```text
version
edge_changes[]             session ID, expected old parent, new parent
json_routing_changes[]     record kind/ID, expected old target, new target
queue_routing_changes[]    table/record ID, child ID, expected old target,
                           new target, prior active state, runtime task key
```

The transition from `pending` to `applying` discovers these exact records once
under the state lock and atomically persists both the plan and a durable
topology/routing fence before quiescing anything. Recovery uses only this plan
plus expected-value checks.

While the fence covers a session, create/spawn with that session as parent,
restore of a stopped child, explicit retire/stop, and any parent-edge mutation
that could change the planned graph return `409` with the controlling request
ID. Observed provider/runtime death may still persist liveness truth; before
authority commit it forces the transaction to restore its exact old parent
edges and routing while preserving the observed stopped status, then end stale.
A frozen child that stopped therefore remains stopped under the old source. If
that restoration fails, the request enters the same durable quarantine as any
other post-quiesce failure. These checks apply to every
session in a tree plan's source/target/grandparent/frozen-child set, not only to
the edge rows being rewritten.

A route requested after planning must not escape the routing fence. The server
persists a `deferred_routing_intent` with an idempotency key and complete
operation payload instead of writing the live JSON/SQLite route. Direct
queue-table writes are not permitted. On `authority_committed`, each intent is
replayed once against the new canonical parent. If the transaction becomes
stale before commit or completes a pre-commit rollback, each intent is replayed
once against the unchanged old canonical parent before the fence is released.
Replay first idempotently upserts the target JSON/SQLite record under the
intent's key. After that record is confirmed, JSON records `replayed_at` and the
resolved parent. The final JSON replacement releases the fence only when every
intent is marked replayed. A crash between stores repeats the same keyed upsert,
so recovery cannot lose or duplicate the operation.
Parent-derived route reads and mutations, including task-complete fallback and
deactivation, use the same fence; it is not limited to creation.

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

Credential bootstrap uses a separate top-level
`session_credential_rotations` collection. Each record freezes the session ID,
provider, provider resume ID, original tmux/runtime identity, request actor,
status (`waiting_idle | relaunching | applied | failed`), requested event
cursor/timestamp, later idle proof, timestamps, and failure reason. It never
contains the plaintext credential. Only one active rotation may cover a seat,
and successful records are idempotent audit history.

## Validation

Request creation and apply both reject:

- empty, missing, stopped, or unsupported source/target sessions;
- self-parenting;
- a target already equal to the current parent;
- for single-edge requests, a new parent inside the subject's current
  descendant set;
- for tree requests, any cycle in the complete planned post-transaction graph;
- single-edge initiation by anyone except current or proposed parent;
- tree initiation by anyone except source, target, or source's live parent;
- tree promotion where target is not source's live direct child;
- duplicate active requests whose affected edge sets overlap;
- any request whose affected edge set overlaps a `failed` transaction that
  reached `json_routing_quiesced` or later and has not been explicitly repaired;
- a topology that changed after request creation.

Apply revalidates all identities, liveness, topology, consent, and cycle
conditions while holding the session-state write lock. Tree cycle validation
first applies every planned edge replacement to an in-memory graph and then
checks the resulting graph; it must not reject merely because the target is a
current direct child of the source.

## Routing and authority transaction

Changing only `parent_session_id` is incorrect. The following parent-derived
state must follow the new graph:

- `context_monitor_notify` only when its companion provenance marks the target
  as parent-derived;
- active parent-wake registrations in retained JSON and queue SQLite;
- pending parent-wake metadata attached to undelivered queue messages;
- child wait/completion monitors that captured a parent at spawn time;
- task-complete fallback routing;
- any additional old-parent-keyed state found by the phase audit.

Context-monitor state gains a companion provenance value,
`context_monitor_notify_source = explicit | parent_derived`. New monitor writes
must set it from the operation that chose the target. Existing records that
lack provenance are conservatively migrated as `explicit`, even when the target
happens to equal the old parent. Explicit notification recipients are never
retargeted; reparenting must not rewrite deliberate peer-to-peer messages.

Existing stop-notify `sender_session_id` values and provider
`subagents[].parent_session_id` values are explicit recipients or historical
provider lineage. They are preserved. Tool-usage parent fields are also
historical provenance and are never rewritten.

Parent-dependent runtime behavior should resolve the current parent at delivery
time where practical. Durable registrations that intentionally pin a parent are
retargeted during apply.

The JSON state file and queue SQLite cannot share an ACID transaction. Apply is
therefore a recoverable, fail-closed staged operation:

1. Under the session write lock, revalidate and atomically persist
   `status=applying`, the complete immutable plan, and its topology/routing
   fence. Parent edges remain unchanged. Every conflicting topology mutation
   checks this fence under the same lock.
2. Through the queue coordinator's routing fence, stop and remove each affected
   in-memory parent-wake task/registration. Under the session-state lock,
   atomically mark every affected retained JSON parent-wake registration
   inactive and persist `apply_stage=json_routing_quiesced`. Task-complete and
   all other fallback reads treat that record as inactive, so they cannot route
   to the old parent. Then, in one SQLite transaction, idempotently quiesce
   affected parent-derived wake registrations and pending wake metadata and
   persist `apply_stage=routing_quiesced` in JSON. A replay accepts either each
   plan entry's recorded pre-state or its exact already-quiesced state; any
   third state fails closed. On process restart, inactive JSON and SQLite rows
   cannot recreate the stopped runtime tasks.
3. In one atomic JSON replacement, update all parent edges and JSON-backed
   routing, and persist `apply_stage=authority_committed`. Dynamic
   task-complete and wait-monitor delivery now resolves this canonical edge.
   Deferred routing intents are now eligible to resolve against the new graph;
   topology mutations remain fenced until their replay is durable.
4. In one idempotent SQLite transaction, retarget and reactivate affected
   parent-derived routes. Under the state lock, retarget and reactivate the
   retained JSON registrations, then recreate their in-memory tasks with the
   new target under the routing fence, replay deferred routing intents against
   the new canonical parents, mark the JSON request `applied`, and release both
   fences. Replay accepts an already-correct runtime registration and never
   starts a duplicate task.

An interruption before step 3 exposes no new destructive authority and leaves
old-parent routing paused rather than misdirected. An interruption after step 3
has new authority with parent-derived routing still paused; dynamic delivery
uses the canonical new edge. Startup and the next request-store mutation resume
`applying` records from their immutable plan. A retry never recomputes a
different topology. If the frozen topology no longer matches before authority
commit, the server restores the exact old authority/routing state without
rewriting observed liveness, replays deferred routing against the unchanged old
parent, marks the request stale, and only then releases the fences. If that
restoration fails, the request remains failed and quarantined. After authority
commit, recovery completes the recorded transaction; it never silently rolls
authority back to a topology that may already have been observed.

On startup, all `applying` reparent records are recovered through their durable
stage before retained parent-wake tasks are recreated and before HTTP/input
readiness. In particular, a crash after an in-memory task was stopped but before
`json_routing_quiesced` cannot briefly recreate an old-parent task from the
still-active retained record.

A `failed` request at or beyond `json_routing_quiesced` remains a durable
quarantine, not a released terminal edge. Its complete affected edge set stays
reserved by the overlap fence, parent-derived route creation remains deferred
for those children, and no later request may include any reserved edge. Only an
authenticated operator repair action may either resume the immutable plan or
restore and verify its exact pre-commit state before clearing the quarantine.
Merely rejecting, expiring, deleting, or recreating the request cannot release
it.

The authenticated human repair route supports exactly two audited actions:

1. `resume` records a repair attempt, changes `failed` back to `applying`, and
   retries the same immutable plan from its persisted stage. It never replans.
2. `rollback_precommit` is accepted only from `json_routing_quiesced` or
   `routing_quiesced`, before `authority_committed`. Under the routing fence,
   the server restores every route to the apply plan's exact recorded
   pre-state, verifies all parent edges still match the old topology and all
   runtime/JSON/SQLite routes match that
   pre-state, replays deferred routing intents against the unchanged old parent,
   then atomically records `status=repaired` and
   `apply_stage=repair_rolled_back`. Any mismatch leaves the request failed and
   quarantined.

At or after `authority_committed`, only forward `resume` is available because
new authority may already have been observed. A successful retry ends
`applied`; a verified pre-commit rollback ends `repaired`. Those are the only
transitions that release the quarantine. Each attempt appends `repair_history`
with the authenticated human actor, action, prior failure, timestamp, and hash
of the verified resulting graph and routing state.

No network delivery occurs while holding the write lock. Approval and outcome
notification intents are persisted in the same JSON transition that creates
them, then reconciled to the queue after releasing the lock.

## Notification contract

Creation sends each missing agent approver an `important` message containing:

- request kind and ID;
- initiator;
- every edge that will change;
- expiry;
- exact approve and reject commands.

Each decision notifies the initiator and remaining approvers. Rejection,
expiration, staleness, failure, and completion send one terminal notification.
Every intent has a deterministic key formed from request ID, event, and
recipient. Queue insertion uses that key as its idempotency identity and marks
the intent enqueued only after the SQLite row exists. Startup and ordinary
request reconciliation retry unqueued intents. Notification delivery failure
does not undo durable request state, omit the prompt, or duplicate a terminal
event.

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
POST /reparent-requests/{request_id}/repair
POST /sessions/{session_id}/credential-rotation
GET  /session-credential-rotations
```

Creation payloads carry `requester_session_id`; decision payloads carry the same
for agent routes. Human routes ignore agent IDs and use request authentication.
List defaults to requests involving the caller or requiring human action.
Operator watch can list all pending human-gated requests.
The repair route is operator-authenticated only and accepts
`action=resume|rollback_precommit`; an agent session credential cannot invoke
it. `sm watch` exposes the same actions with the stage-specific safety text.
Credential-rotation routes are also operator-authenticated only. The create
route returns the durable rotation state; repeated calls return the existing
active or already-applied record rather than scheduling a second relaunch.

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
focused tests. Add runtime credential issue/verification plus the durable
idle-only credential-rotation API and recovery path. No reparent request may
apply yet.

### Phase 1 - #1208

Add single-edge apply, routing reconciliation, fail-closed recovery, and tests
that direct-child retire/kill authority and supervision follow the new edge.
This phase also implements durable repair transitions and their authenticated
HTTP route; Phase 3 adds their CLI/watch UX.

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
- Conflicting spawn/restore/retire/topology mutations are fenced until commit;
  an observed pre-commit liveness change restores the old graph instead of
  applying a stale frozen child set.
- Parent-derived routing and direct-child destructive authority follow the new
  graph immediately after apply.
- Pending and `applying` requests recover correctly after restart at every
  durable stage.
- Post-quiesce failures retain their overlap/routing fence; only a successful
  immutable-plan retry or verified pre-commit rollback releases it.
- Deferred parent-derived route requests replay exactly once against the new
  parent after commit or the old parent after stale/rollback release.
- A pre-deployment live seat is recredentialed only after it becomes idle, keeps
  its seat and provider-thread identity, and recovers a crash after stop without
  accepting input under two runtimes.
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
