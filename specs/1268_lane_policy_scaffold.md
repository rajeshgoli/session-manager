# Session Manager lane policy scaffold

- **Issue:** [#1268](https://github.com/rajeshgoli/session-manager/issues/1268)
- **Status:** Draft for owner review
- **Scope:** Specification and execution plan only; no runtime implementation
- **Prerequisites:** #1264 merged and deployed; #1265 must merge before the
  policy evaluator is enforced

## Goal

Apply evolving lane policy without a persistent watchdog/policy seat and without
depending on orchestrators to remember `sm task`, context thresholds, rotation,
or model-tier rules.

## Lane 355 policy examples

- Orchestrator: Claude Opus/high, successor rotation around 35%, policy ceiling
  40% unless a durable scoped override exists.
- Spec owner: Claude Fable/high, same rotation profile.
- Watchdog: Codex Luna/high or xhigh.
- Routine bounded/mechanical fixes: Claude Sonnet/high.
- Reasonably complex implementation: Claude Opus/high.
- Initial one-task engineer: 65% ceiling only through the first provider turn;
  every follow-up/review/second turn uses the 40% ceiling.

## Empirical classifier results

Exact live Appendix SW spawn proposal, judged against the accidental full
Claude main-thread answer:

| Path | Latency | Measured input | Result |
|---|---:|---:|---|
| Luna cold, 25k tail | 16.32s | 40,243 | unsafe allow |
| Luna warm, 25k tail | 4.95s preload + 34.92s | 13,190 + 54,493 | unsafe allow |
| Luna cold, 50k tail | 66.50s | 68,201 | rewrite, materially incomplete |
| Luna cold, full available history | 74.83s | 114,414 | still incomplete |
| Sol cold, full available history | 33.76s | 115,837 | found conflict, obeyed stale policy |
| Accidental Claude main-thread turn | about 49s | 289,186 cache-read + 3,143 cache-create | complete current-context answer |
| Claude `/btw`, active foreground tool | about 4.2s | direct counters not exposed; completed prefix estimated near 79k | isolated success while tool remained live |
| Claude `/btw`, current-turn marker | about 8s | direct counters not exposed; completed prefix estimated near 81k | isolated success; active-turn-only marker unavailable |

The accidental Claude result is not valid evidence for true `/btw`. Controlled
isolated runs subsequently proved that `/btw` can execute concurrently with a
foreground shell tool, but sees only completed parent context. Issue #1265 must
still fix arbitrary busy-state safety and terminal request behavior before this
mechanism is enforced. Larger replay tails and a warm external classifier did
not provide a reliable authority boundary.

## Core design

### 1. Policy authority

The human-readable lane policy document is authoritative. Keep an append-only
version history and materialize only a small typed enforcement projection; do
not require every future policy concept to be added to a closed schema.
Separate:

- normative lane-wide clauses; and
- scoped rulings bound to a role, task, issue, transition, spawn request, or
  immutable prompt digest.

Each amendment records source evidence, approver identity, effective time,
explicit supersession, scope, enforcement class, and optional expiry/one-shot
consumption. Later text does not silently supersede active policy.

Agent-generated natural language creates a document change proposal. A
registered human approves/rejects the document diff in `sm watch`. The typed
projection contains stable SM primitives such as model/effort defaults,
context thresholds, rotation behavior, routing, and prompt guidance. A policy
concept SM cannot enforce remains advisory and is injected at the applicable
decision point; adding a new enforcement primitive can require development
without making the policy itself inexpressible.

Initially every lane-policy rule is overridable by the seat holder with a
durable, scoped reason. Non-overridable safety and authority properties are SM
invariants, not lane-policy clauses.

### 2. Stable seat identity

Model a named lane role as a stable seat key, for example `355-root`, whose
holder is a replaceable provider-session incarnation. The current holder need
not call `sm register`; policy and rotation transactions assign it.

Lane-owned routing targets the seat key and resolves atomically to its current
holder: sends, monitor notifications, email replies, queue/review wakes,
reminders, and ownership relationships. A provider session ID remains usable
for explicit diagnostics or consultation with an old incarnation. Historical
holder records remain durable.

### 3. Immutable spawn request

Issue #1264 supplies an immutable prompt artifact. A policy-enabled spawn
creates a durable request bound to:

- parent session incarnation and lane;
- prompt digest and launch-intent ID;
- policy version;
- requested name, vehicle, provider/model/effort, cwd, and node.

No child is allocated while policy is pending. A changed parent, prompt, policy,
or topology stales the request. The request ID is internal: from the calling
agent's perspective `sm spawn` is atomic and returns either the final child ID
or an actionable rejection/rewrite reason.

Measured on Claude Code 2.1.226: provider-native `/btw` can run as a genuine
parallel sidechain while the main turn has a live foreground shell tool. A
30-second `ping` process remained live; `/btw` returned in about 4.2 seconds;
the main transcript retained one uninterrupted tool-use/result chain and
contained no `/btw` text. This makes an atomic blocking `sm spawn` feasible.

The same experiment showed that the sidechain sees only context through the
last completed turn: a marker present solely in the current in-progress user
turn was reported unknown. The policy prompt must therefore include the frozen
spawn artifact and a bounded current-turn delta read from provider events or
the transcript. It does not need an arbitrary 25k/50k replay: `/btw` already
has the completed parent context, and SM supplies only what has not yet entered
the sidechain snapshot.

### 4. Intent extraction

Named roles can be classified deterministically. Generic work uses one true
provider-native parent `/btw` concurrently with the calling `sm spawn` tool
when that provider state is positively verified. Give it operation-relevant
active policy, the frozen spawn request, and the bounded current-turn delta.
It extracts evidence; it does not author policy.

Prefer provider-native `/btw` when it can execute safely and atomically. Keep
the evaluator replaceable; it is an evidence extractor, not policy authority.
Closed output vocabulary:

- lifecycle: `named_seat`, `ephemeral_task_worker`, `inline`, `no_spawn`
- work class: `orchestration`, `spec_authority`, `monitoring`,
  `routine_bounded`, `complex`, `unknown`
- turn class: `initial_task`, `followup`, `continuation`
- current facts with evidence and active-rule conflicts

Invalid, truncated, unavailable, or ambiguous extraction fails closed. No
fallback to ordinary main-thread delivery or arbitrary transcript-tail replay.

All spawned workers remain SM-managed. `ephemeral_task_worker` means a
short-lived SM child with automatic completion cleanup, not a Claude/Codex
private subagent.

### 5. Deterministic decision

The engine maps the closed classifier output to canonical provider/model/effort
and a pre-approved context profile. Precedence is explicit: scoped owner ruling,
lane clause, lane default, global default, then caller request. Conflicts at one
rank block instead of using timestamp or model judgment. Initially every lane
clause remains overridable with a durable scoped reason.

The calling `sm spawn` returns a child ID only after `allow` allocates and
launches it. `rewrite`/`block` returns exact amendments; terminal failure
creates no child. Internal durable request state remains observable for
recovery and diagnostics but is not the normal caller contract.

### 6. Evaluation telemetry

Every policy evaluation records one canonical, non-duplicated usage row keyed
by evaluation ID. Capture direct provider counters where available:

- input tokens;
- cache-read tokens;
- 5-minute and 1-hour cache-write tokens;
- output and reasoning tokens;
- provider model and effort;
- parent completed-turn leaf/context identity;
- policy version, prompt digest, and current-turn-delta digest plus byte/token
  sizes;
- start/end timestamps, latency, outcome, and rejection/rewrite class.

Also derive cache-hit share and context amplification (tokens consumed relative
to the frozen spawn brief), and retain whether several evaluations reused the
same completed parent context. Expose per-evaluation JSON/CSV and aggregates by
lane, seat, model, and policy version.

Usage attribution must identify source, confidence, method, and bounds. A
direct provider usage object is authoritative. Otherwise always emit a best
numeric estimate rather than `unknown`:

1. Estimate the sidechain's cache prefix from the target's latest completed
   provider turn. The concurrency experiment established that `/btw` excludes
   the active turn, so its cache-read size is anchored to that completed leaf's
   observed cache-read/cache-write counters.
2. Estimate injected policy, frozen brief, and current-turn delta separately
   from their exact byte counts and calibrated provider token ratios.
3. Snapshot account quota immediately before and after the evaluation and
   record all other observed seat-token activity in the same interval. Use the
   residual as a calibration signal, not as the sole estimator.
4. Calibrate estimates against every later evaluation for which direct usage is
   available, by provider/model/context-size band.

Persist `estimated`, `lower_bound`, `upper_bound`, `method`, and `confidence`
for every counter. User-facing values use `~` for estimates and expose the
interval on demand. If the transcript is unavailable, fall back to context
percentage times provider context capacity with a deliberately wider bound;
the telemetry surface still returns a number.

### 7. Runtime profile

Persist the accepted profile on the child and arm it automatically. Provider
Stop/turn events transition an initial-task engineer to follow-up after its
first turn, without `sm task`.

Context telemetry is boundary-based, so strict caps require headroom. Example:

- preflight at 32%;
- rotation target around 35%;
- hard state at first sample >=40%.

The context ceiling does not block the control plane. Handoff, context/status,
`sm what`, queue/review completion, monitoring, email replies, owner messages,
reparenting, cleanup, and completion of the current atomic task still flow.
At most it prevents starting new discretionary work while rotation is pending.
With stable seat routing, ordinary new work can queue briefly against the seat
and then deliver to the successor rather than being discarded.

At a rotation threshold, after a safe turn boundary, SM obtains a compact
handoff through isolated `/btw`, spawns the policy-defined successor, and
commits one policy-authorized rotation transaction. The approved policy is the
authority, so no per-rotation reparent approvals are required. Commit transfers
children, the stable seat, monitor routes, and lane-owned registrations;
predecessor becomes the successor's child and remains directly addressable by
its provider session ID. A durable scoped override may defer rotation.

## Execution plan

### Delivery topology and controls

- #1264 atomic prompt transport is merged and deployed.
- #1265 safe Claude `/btw` isolation is the remaining implementation
  prerequisite.
- First open a spec-only PR to `main`. No policy implementation starts before
  owner approval, except read-only spikes and fixture collection.
- After approval, create one epic branch. Every package uses its own worktree
  and targets the epic branch; a dependency must be merged, not merely in
  review, before its consumer starts.
- Each implementation agent owns its focused tests, full applicable gates,
  `docs/working/pr_review_process.md`, merge to the epic branch, and worktree
  cleanup. Only P1 findings require another Codex review round.
- Limit active implementation to four agents: at most two Sol, two Terra, and
  one Luna where the wave permits. More parallelism would increase merge and
  review overhead faster than it reduces wall time.
- The canonical maintainer owns contracts, dependency ordering, integration,
  shadow deployment, final epic review, and production rollout.

Agent tiers:

- **Luna/high:** read-only inventory, corpus extraction, mechanical fixtures,
  repetitive tests, docs, and quantitative telemetry analysis. Never owns an
  authority or lifecycle decision.
- **Terra/high:** bounded implementation against an approved contract: schemas,
  estimators, APIs/CLI, UI, deterministic mappings, and focused runtime work.
- **Sol/high:** cross-cutting identity, authority, provider concurrency,
  transactional rotation, restart recovery, and final integration.

### Wave 0 - prerequisites and specification

| ID | Owner | Work | Dependency | Output |
|---|---|---|---|---|
| 0A | Terra/high | Complete #1265, including busy/idle/restart isolation | #1264 | Merged and deployed safety prerequisite |
| 0B | Maintainer Sol/high | Convert this note into the normative spec: document authority, seat identity, atomic spawn, telemetry, overrides, recovery, and watch UX | 0A findings may amend it | Spec-only PR for owner approval |
| 0C | Terra/high | Read-only spike to locate direct Claude/Codex sidechain usage events and calibratable fallback inputs | none; parallel with 0B review | Evidence report, no production code |
| 0D | Luna/high | Build a sanitized corpus of 15-25 historical spawn decisions, including sparse briefs and scoped owner rulings | none; parallel with 0B review | Golden fixture inputs and expected decisions |

Owner checkpoint: approve the spec and fixture/ruling interpretation before an
epic branch is created.

### Wave 1 - independent foundations

Run these concurrently after the spec is approved. Their module and database
ownership must be fixed in the spec to keep overlap limited to registration
files.

| ID | Owner | Work | Dependency | Output |
|---|---|---|---|---|
| 1A | Maintainer Sol/high | Human-readable policy document history, scoped rulings, proposal/approval authority, and materialized enforceable effects | approved spec | Policy authority/store API |
| 1B | Sol/high | Stable seat identity, holder incarnations, historical ownership, atomic resolver, and migration compatibility | approved spec | Seat registry and resolver |
| 1C | Terra/high | Evaluation records, direct/estimated token counters, bounds/calibration, quota snapshots, JSON/CSV surfaces | 0C evidence | Telemetry API and CLI |
| 1D | Luna/high | Golden corpus harness, hostile policy fixtures, and restart/test scaffolding using frozen contracts | 0D corpus | Reusable test substrate |

### Wave 2 - spawn policy path

| ID | Owner | Work | Dependency | Parallelism |
|---|---|---|---|---|
| 2A | Terra/high | Pure deterministic decision engine for named seats, ephemeral workers, model/effort, context profiles, precedence, and scoped overrides | 1A, 1D | Starts first |
| 2B | Sol/high | Atomic `sm spawn` policy runner: caller binding, current-turn delta, native `/btw`, strict parsing, restart recovery, and no-child-on-reject | 0A, 1A, 1B, 1C, 2A | Critical path |
| 2C | Terra/high | `sm watch` policy document diff approval, decision explanation, and per-evaluation telemetry views | 1A, 1C | Parallel with 2A/2B behind API contracts |

Acceptance gate: shadow decisions over the golden corpus and disposable live
Claude/Codex sessions must produce no main-thread injection, no orphan child,
and one terminal telemetry row per attempted spawn.

### Wave 3 - runtime profiles and routing

| ID | Owner | Work | Dependency | Parallelism |
|---|---|---|---|---|
| 3A | Terra/high | Context profiles, initial-task to follow-up transitions, preflight/rotation/draining thresholds, and control-plane exceptions | 1A, 2A | Parallel |
| 3B | Terra/high | Route sends, monitors, email replies, queue/review wakes, reminders, and ownership through stable seat keys while preserving direct agent-ID addressing | 1B | Parallel |
| 3C | Luna/high | Inventory every persisted target/owner field and add mechanical routing/compatibility fixtures; report omissions to 3B | 1B | Parallel scout/test lane |

Sol/high maintainer performs the cross-subsystem integration review after 3A
and 3B merge; this is review/integration work, not a separate implementation
agent.

### Wave 4 - rotation and overrides

| ID | Owner | Work | Dependency | Parallelism |
|---|---|---|---|---|
| 4A | Sol/high | Policy-authorized rotation transaction: handoff, successor readiness, seat/children/routes transfer, predecessor attachment, idempotency, rollback, and restart recovery | 2B, 3A, 3B | Critical path |
| 4B | Terra/high | Scoped override command/API/watch UX, draining presentation, audit history, expiry and consumption semantics | 1A, 3A | Parallel with early 4A |
| 4C | Terra/high | Hostile and restart matrix for partial spawn, stale holder, duplicate commit, route delivery during transfer, and recovery after each transaction boundary | 4A contract; implementation follows incrementally | Test lane |

### Wave 5 - measured rollout

1. Maintainer deploys lane 355 in **shadow mode**. Deterministic named-seat
   checks and generic `/btw` classifications run and record telemetry, but do
   not reject or rewrite launches.
2. Collect 20-30 real spawn evaluations. Luna/high performs mechanical
   analysis of cache reads, latency, model mix, repeated-context amplification,
   and golden-decision agreement; Sol/high adjudicates only semantic misses.
3. Owner checkpoint: review decisions and telemetry. Choose optimizations from
   measured data rather than precommitting to summary caching or a cheaper
   evaluator.
4. Enable deterministic named-seat/model enforcement first, generic spawn
   classification second, context/draining third, and automatic rotation last.
   Every stage retains a durable seat-holder override reason.
5. Open one epic-to-main PR. The maintainer runs the full review protocol,
   deploys only after it exits, verifies Claude and Codex live paths, then
   removes all phase worktrees and retires all implementation seats.

### Expected critical path

`#1265 -> spec approval -> 1A/1B -> 2A -> 2B -> 3A/3B -> 4A -> shadow data -> owner enforcement approval`

Telemetry (1C), corpus/tests (1D), watch UX (2C), routing audit (3C), and
override UX (4B) run beside that path. This preserves wall-clock parallelism
without assigning Luna/Terra work that requires Sol-level authority judgment.
