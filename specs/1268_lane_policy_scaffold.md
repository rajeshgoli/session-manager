# Session Manager lane policy scaffold

- **Issue:** [#1268](https://github.com/rajeshgoli/session-manager/issues/1268)
- **Status:** Draft for owner review
- **Scope:** Specification and execution plan only; no runtime implementation
- **Prerequisites:** #1264 and #1265 merged, deployed, and live-verified

## Goal

Apply evolving lane policy without a persistent watchdog/policy seat and without
depending on orchestrators to remember `sm task`, context thresholds, rotation,
or model-tier rules.

## Lane 355 policy examples

- Orchestrator: Claude Opus/high, successor rotation around 35%, policy ceiling
  40% unless a durable scoped override exists.
- Spec owner: Claude Fable/high, same rotation profile.
- Watchdog and keeper: Codex Luna/high or xhigh. They do not rotate for context;
  long-running Codex-fork holders use normal provider-native autocompaction.
- Routine bounded/mechanical fixes: Claude Sonnet/high.
- Reasonably complex implementation: Claude Opus/high.
- Initial one-task engineer: 65% ceiling only through the first provider turn;
  every follow-up/review/second turn uses the 40% ceiling.
- Named orchestrator home: `/Users/rajesh/projects/fractal-algo-rust` even when
  a bounded task temporarily moves its holder into an SM-owned worktree.

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
foreground shell tool, but sees only completed parent context. Issue #1265 and
follow-up #1271 now provide verified idle-composer admission, durable recovery
stages, and fail-closed ambiguous-state handling. Larger replay tails and a
warm external classifier did not provide a reliable authority boundary.

### Live omitted-tier fixture

Session `355-617-death-1` (`aa6c1120`) was spawned for an engineer task without
an explicit tier and began on the Claude default. Direct provider usage records
show 96 Fable messages with about 19.8M cache-read tokens before later Opus
activity; the projected `seat_meta.model` simultaneously reported `sonnet`.
This is a binding hostile fixture for both admission and attestation:

- policy must classify the frozen task as routine/Sonnet or complex/Opus from
  evidence and pass an explicit canonical model/effort to the launch adapter;
- a policy-governed engineer spawn may never inherit the provider default merely
  because the caller omitted `--model`;
- synchronous launch validation rejects an absent, unsupported, or noncanonical
  model before child allocation; and
- provider-event attestation, not the projected roster model field or agent
  self-report, verifies what actually ran and stops a mismatch before the child
  accepts further work.

The fixture records avoided wrong-tier tokens as the benefit. It also records
any bootstrap/attestation tokens consumed, so preventing a 19.8M-token default
mistake is compared against the policy gate's actual cost rather than asserted.

## Role-context review results

Read-only Claude-native forks of the current lane-355 orchestrator, Fable spec
owner, and Opus engineer reviewed this draft with their existing role context.
The originals and lane topology were unchanged, and all review forks were
retired afterward. Their independently recurring findings are incorporated:

- durable artifacts and third-party tools must route by stable seat key rather
  than cache holder IDs;
- rotation must preserve imperative next actions, negative results, evidence
  freshness, delivered-but-unacted events, and a directed retirement probe;
- cleanup ownership includes non-repository state and launched processes, with
  coverage-counted verification rather than remembered lists;
- non-rotating Codex seats still need durable state snapshots and explicit
  required-provider outage behavior; and
- policy needs refusal, recusal, reclassification, conflict escalation, live
  model attestation, and continuously measured rotation cost.

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

Normative clauses have stable lane-scoped IDs; rulings and supersession records
bind those IDs, never line numbers or prose snippets. Superseded text remains
readable in place with a forward pointer to the replacing clause and amendment.
An open-to-settled status change is invalid unless the same amendment names the
external settling record; a policy document cannot cite itself as authority for
its own settlement.

Agent-generated natural language creates a document change proposal. A
registered human approves/rejects the document diff through an operator-only
`sm watch` channel that managed sessions cannot invoke; shared GitHub author
credentials are not human identity evidence. The typed
projection contains stable SM primitives such as model/effort defaults,
context thresholds, rotation behavior, routing, and prompt guidance. A policy
concept SM cannot enforce remains advisory and is injected at the applicable
decision point; adding a new enforcement primitive can require development
without making the policy itself inexpressible.

Initially every lane-policy rule is overridable by the seat holder with a
durable, scoped reason. Non-overridable safety and authority properties are SM
invariants, not lane-policy clauses.

An override that benefits the holder issuing it, including deferring its own
rotation or expanding its own authority, is explicitly flagged for owner audit.

### 2. Stable seat identity

Model a named lane role as a stable seat key, for example `355-root`, whose
holder is a replaceable provider-session incarnation. The current holder need
not call `sm register`; policy and rotation transactions assign it.

Seat keys are lane-prefixed and globally unambiguous; bare role keys such as
`spec-owner` are invalid. Durable lane artifacts store the seat key rather than
copying a current holder ID. `sm seat resolve 355-root` is the stable read-time
resolver for scripts and third-party tools; a holder ID may accompany the key
as point-in-time evidence but is never authoritative routing state.

Lane-owned routing targets the seat key and resolves atomically to its current
holder: sends, monitor notifications, email replies, queue/review wakes,
reminders, and ownership relationships. A provider session ID remains usable
for explicit diagnostics or consultation with an old incarnation. Historical
holder records remain durable.

Resolution against a missing, dead, or not-yet-committed holder fails loudly,
queues the operation against the seat generation, and escalates to the policy
approver; it never falls back to a historical holder or drops the event. Every
seat with active routing targets has a holder-liveness alarm. Lifecycle history
distinguishes at least `incoming`, `live`, `stopped-but-restorable`, `retired`,
and `unretirable` rather than collapsing every non-live state into retired.

Addressing is explicit:

- `355-root` resolves to the current holder at operation acceptance time;
- `355-root@previous` resolves to the immediately preceding incarnation and is
  frozen to its agent ID before delivery;
- an exact agent ID addresses that specific incarnation, regardless of which
  seat it formerly held; and
- `sm seat history 355-root` lists holder agent IDs, acquisition/release times,
  and current lifecycle state so any older predecessor can be selected.

Seat-relative selectors are conveniences, not mutable aliases carried inside a
queued message. The accepted message stores the resolved seat generation and
agent ID, preventing a later rotation from silently changing its recipient.

### 3. Immutable spawn request

Issue #1264 supplies an immutable prompt artifact. A policy-enabled spawn
creates a durable request bound to:

- authoritative parent seat key, resolved parent incarnation, seat generation,
  and lane;
- prompt digest and launch-intent ID;
- policy version;
- requested name, vehicle, provider/model/effort, cwd, and node.

No child is allocated while policy is pending. A changed parent, prompt, policy,
or topology stales the request. The request ID is internal: from the calling
agent's perspective `sm spawn` is atomic and returns either the final child ID
or an actionable rejection/rewrite reason.

The parent incarnation is point-in-time launch evidence. Routing, ownership,
and later lifecycle decisions derive from the parent seat key and generation,
so an immutable brief cannot freeze a soon-to-retire holder as durable parent
authority.

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

Invalid, truncated, unavailable, ambiguous, or internally conflicting evidence
fails closed. Contradictory facts that each parse successfully produce
`conflicting_evidence`, not a plausible merged classification. No fallback to
ordinary main-thread delivery or arbitrary transcript-tail replay.

All spawned workers remain SM-managed. `ephemeral_task_worker` means a
short-lived SM child with automatic completion cleanup, not a Claude/Codex
private subagent.

Evaluation is demand-driven. Named roles, explicit task types, and spawn
requests whose frozen fields satisfy deterministic clauses do not invoke
`/btw`; they record the deterministic decision and zero evaluator tokens.
Generic or conflicting intent invokes at most one evaluator for the immutable
request. SM injects only applicable clause IDs/text and the bounded current-turn
delta, reuses the provider's completed-context cache, and never resubmits the
whole policy document merely because another spawn occurs.

#### Ephemeral worker closeout

An ephemeral worker receives a durable lifecycle and cleanup manifest when SM
creates it. The manifest distinguishes resources SM owns from paths or branches
the worker merely borrows, and records:

- repository identity, working directory, and owned worktree path;
- base/head commits and local/remote branch ownership;
- linked issue IDs, PR ID/head/base, and the exact closing-reference contract;
- current review request and whether a successor is expected to handle it;
- repository and non-repository durable artifacts that must be retained, with
  owning seat key, migration target, and retention policy;
- launched processes, process groups, PID files, ports, and external watches
  owned by the incarnation; and
- cleanup owner, state, and completed actions.

At provider Stop, task-complete, PR/review wake, and context-rotation boundaries,
SM deterministically checks known Git/GitHub/session facts. If task state is
still semantically ambiguous, SM invokes isolated `/btw` on the worker with the
manifest and applicable policy, asking for a structured closeout claim: work
complete, awaiting review, awaiting external input, or follow-up required, plus
the claimed cleanup actions. The claim is evidence, not authority; SM verifies
each action before performing it.

Automatic cleanup requires all applicable checks:

- the worktree is SM-owned and `git status --porcelain` is empty;
- no unpushed commit or unretained artifact would become unreachable;
- the recorded PR is merged/closed, or the no-PR completion contract is met;
- only the recorded owned local/remote branch is selected for deletion; and
- issue closure is observed through the recorded PR closing reference or an
  explicit policy-authorized action, never inferred from a branch name.

Process cleanup derives ownership from durable launch records and kernel-held
identity such as PID-file/FD evidence, never argv matching or a remembered
partial list. A process or watch addressed to a retiring holder is stopped or
retargeted to its seat key before holder retirement commits. Fleet sweeps record
the expected population, items visited, items accepted, and errors; a zero from
an incomplete sweep is not clean evidence.

Dirty, shared, unpushed, ambiguous, or still-in-review state fails closed. SM
keeps the worker and resources, records the exact blocker, and routes the
follow-up to that worker or a policy-selected successor. If review is pending,
the task identity and cleanup manifest transfer to the successor so cleanup is
part of completing the task, not a separate fleet-wide cleanup job. Once facts
verify clean completion, SM removes owned worktree/branches and retires the
ephemeral incarnation under policy authority; the worker need not remember a
self-retirement command.

#### Workspace anchors and relocation

Policy may assign a stable seat or retained worker a durable workspace anchor
and allow task-scoped worktrees as temporary locations. Before deleting an
owned worktree that still contains a restorable session, SM performs one
provider-neutral relocation transaction:

1. verify that the policy-selected anchor is absolute, exists, belongs to the
   expected repository, satisfies declared capabilities (remote/ref/read-only
   or writable state), and is not inside the cleanup target;
2. move or resume the provider session at that anchor (`/cd` may be one
   provider adapter, but is not itself completion evidence);
3. observe the provider's effective cwd and atomically update the durable SM
   `working_dir` to the same canonical path;
4. verify that a restore plan no longer references the worktree; and only then
5. authorize normal worktree cleanup.

Provider cwd and SM metadata must agree before cleanup. Failure, ambiguity, or
an unavailable anchor leaves the worktree intact and reports the exact blocker.
Anchor behavioral constraints, such as “never change git state in the owner's
live checkout,” are injected at spawn and relocation.
This generic primitive lets a later policy amendment change orchestrator homes
or pre-cleanup destinations without adding a new SM field or code path for each
directory convention. A fully retired ephemeral worker needs no relocation;
one retained for review, consultation, or restoration does.

### 5. Deterministic decision

The engine maps the closed classifier output to canonical provider/model/effort
and a pre-approved context profile. Precedence is explicit: scoped owner ruling,
lane clause, lane default, global default, then caller request. Conflicts at one
rank block instead of using timestamp or model judgment. Initially every lane
clause remains overridable with a durable scoped reason.

A same-rank conflict automatically escalates to the registered human approver
with both stable clause IDs and verbatim text; a seat benefiting from either
interpretation cannot settle it. Evaluator outage also fails closed, but the
human approver may authorize one frozen spawn request directly as a durable
one-shot override. There is no undocumented agent fallback.

Policy decisions are bidirectional. A managed worker may issue a durable
`policy-refuse` for unsound instructions or `policy-recuse` for a conflict of
interest; both preserve the rejected assignment, reason, evidence, and required
escalation. New discretionary assignments to an existing seat pass the same
policy gate as spawn. If work class changes, SM records a reclassification and
either updates a compatible runtime profile, rotates/replaces the holder, or
rejects the assignment with actionable amendments. Spawn-time classification
does not silently govern unrelated later work forever.

Policy conditions that track external state are derived at evaluation time
where possible. A pinned baseline records its derivation evidence, verification
time, and mandatory revalidation trigger; passing a stale startup-only baseline
is not current policy evidence.

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

Every enforceable or injected policy requirement has a measurement contract
before it enters the active canary. The contract binds a stable requirement ID
to:

- the intervention event and its linked operation/seat/rotation trace;
- the incremental cost boundary, including input, cache read, 5-minute and
  1-hour cache write, output/reasoning tokens, dollar estimate, and latency;
- a matched pre-policy baseline, earlier rollout cohort, or owner-approved
  holdback when withholding the requirement is safe;
- immediate machine-observable benefit metrics and the horizon over which they
  are evaluated; and
- a later Luna/high benefit review with evidence, confidence, and the resulting
  keep/change/remove recommendation.

Safety and authority invariants are never disabled merely to manufacture a
control. Where no safe holdback exists, compare against matched historical
operations and report the resulting wider confidence interval. Cost counters
always receive a best estimate with bounds; an absent observed workflow benefit
is recorded as `no_observed_benefit`, not silently omitted.

One logical workflow is decomposed into linked, non-overlapping spans so shared
provider counters are not charged twice. For example, a rotation records the
outgoing `/btw` handoff summary, successor launch/orientation, directed
predecessor probe, predecessor answer, and any addendum processing separately.
Asking a 400k-context predecessor after already producing a `/btw` summary must
therefore show exactly how much additional input/cache-read/cache-write/output
and latency the probe consumed.

The initial requirement-effect ledger includes:

| Requirement family | Incremental cost boundary | Immediate benefit evidence |
|---|---|---|
| Spawn intent and model/vehicle policy | classifier plus injected rewrite and any retry | corrected provider/model/vehicle, prevented invalid launch, retry/rework rate |
| Stable seats and holder liveness | resolution/alarm/queued-delivery operations | stale-holder deliveries prevented, manual re-points avoided, eventual delivery success |
| Context profiles and rotation thresholds | preflight, handoff, orientation, probe, and retirement spans | quota/tokens per completed unit of work, threshold overrides, rotation failures, time to productive successor |
| Handoff summary and directed probe | summary and probe spans reported separately | novel actionable items, corrections to the summary, ruled-out paths preserved, successor actions attributable to each item |
| Codex native-compaction snapshots | each snapshot and restore/reconstruction attempt | successful restore, reconstruction time avoided, state-loss incidents |
| Cleanup manifests and workspace relocation | closeout classification, verification, relocation, and cleanup spans | owned resources removed, ambiguous cleanup blocked, separate cleanup jobs avoided, successful later restore |
| Refusal, recusal, conflicts, overrides, and reclassification | decision/escalation plus replacement work | accepted corrections, conflicted work avoided, owner reversals, reassignment/rework avoided |
| Model attestation and provider-outage disposition | attestation or failed launch path | mis-tiered launches caught, hidden provider failures surfaced, unstaffed intervals made explicit |
| Coverage-counted sweeps and baseline revalidation | sweep/revalidation runtime and any evaluator use | partial sweeps detected, stale baselines rejected, false-clean reports prevented |

Per-requirement reports show the baseline and intervention side by side and
derive benefit per million incremental tokens, benefit per dollar, and elapsed
time saved or added where those units are meaningful. They also classify the
observed tradeoff as `lower_cost`, `better_execution`, `both`, or `neither`;
aggregate token savings cannot conceal a workflow regression, and a workflow
improvement cannot conceal unbounded recurring token cost.

Benefit attribution distinguishes direct observation from inference. A novel
probe item is not automatically valuable: Luna/high classifies whether it was
already present in the handoff, whether the successor acted on it, and whether
it changed an outcome. The review may use later agent-role forks after the trial
to assess workflow effects that counters cannot establish, but those reviews
receive frozen artifacts and do not rewrite the measured event record.

Rotation telemetry is first-class: trigger context, handoff/probe/successor
tokens, latency, failure/override, generations per hour, and time to the next
rotation. This is required to test whether a 35% target actually costs less
than a later threshold. Automated corpus/fleet analyses also emit expected,
visited, accepted, refused, and error counts.

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
   residual as a calibration signal, not as the sole estimator. When concurrent
   unattributed activity dominates the interval, mark that residual component
   low-confidence/no-signal instead of calibrating against fleet noise.
4. Calibrate estimates against every later evaluation for which direct usage is
   available, by provider/model/context-size band.

Persist `estimated`, `lower_bound`, `upper_bound`, `method`, and `confidence`
for every counter. User-facing values use `~` for estimates and expose the
interval on demand. If the transcript is unavailable, fall back to context
percentage times provider context capacity with a deliberately wider bound;
the telemetry surface still returns a number.

The always-numeric rule applies only to token/cost counters. Operational facts
such as identity, liveness, authority, gate state, and horizon remain fail-closed
and may explicitly report unavailable. A refused policy operation still records
the numeric tokens consumed while reaching refusal.

### 7. Runtime profile

Persist the accepted profile on the child and arm it automatically. Provider
Stop/turn events transition an initial-task engineer to follow-up after its
first turn, without `sm task`.

Context behavior is role- and provider-specific. A `watchdog` or `keeper`
running on the required Codex Luna high/xhigh profile uses
`provider_native_compaction`: SM records context and liveness telemetry but
does not arm successor preflight, draining, or the Claude 35/40% rotation
thresholds. Codex-fork remains one long-running holder and autocompacts through
its normal harness. This exception is bound to both role and accepted provider;
it must not exempt an accidentally mis-tiered Claude holder from Claude policy.

Every required-provider role declares its unavailable disposition: `block`,
`run_unstaffed_recorded`, or an explicit substitute profile. Lane 355 watchdog
and keeper seats use `run_unstaffed_recorded`; they never silently substitute a
Claude seat during Codex outage. After launch, SM verifies actual provider,
model, and effort from provider-side events/usage or launch evidence rather than
agent self-report. Mismatch alarms and fails profile activation.

Non-rotating `provider_native_compaction` seats periodically externalize a
policy-defined durable state artifact and refresh it before planned shutdown.
Native compaction is not a substitute for recoverable seat state if the process
dies. Snapshot cadence, ownership, hash, and last verified time are visible.

Context telemetry is boundary-based, so strict caps require headroom. Example:

- preflight at 32%;
- rotation target around 35%;
- hard state at first sample >=40%.

The 35% target is an economic control for repeated reads of a large Claude
context, not a claim that the provider is near its context-capacity limit.
Rotation telemetry tests that cost assumption rather than treating it as fixed
capacity doctrine.

The context ceiling does not block the control plane. Handoff, context/status,
`sm what`, queue/review completion, monitoring, email replies, owner messages,
reparenting, cleanup, and completion of the current atomic task still flow.
At most it prevents starting new discretionary work while rotation is pending.
With stable seat routing, ordinary new work can queue briefly against the seat
and then deliver to the successor rather than being discarded.

`sm me` and `sm context` expose the holder's current sampled percentage,
sample time, freshness, active profile, and next threshold. A cached percentage
is labeled cached/stale and is never presented as a heartbeat.

At a rotation threshold, after a safe turn boundary, SM obtains a compact
handoff through isolated `/btw`, spawns the policy-defined successor, and
commits one policy-authorized rotation transaction. The approved policy is the
authority, so no per-rotation reparent approvals are required. Commit transfers
children, the stable seat, monitor routes, and lane-owned registrations;
predecessor becomes the successor's child and remains directly addressable by
its provider session ID. A durable scoped override may defer rotation.

The handoff prompt is assembled by SM, not authored solely by the policy
document. A fixed versioned prompt template supplies the structured output
contract. SM adds the applicable rotation/role clauses from the approved policy
document, current seat/child/routing/task facts, the completed-context identity,
and the bounded active-turn delta. `/btw` summarizes the main thread against
those requirements into an immutable handoff artifact. Policy controls what the
handoff must preserve; SM supplies machine facts and verifies identifiers.

The artifact is an imperative successor continuation brief, not merely a state
snapshot. Its required semantic sections are:

- the seat's objective and the rationale behind current decisions;
- completed work, in-flight work, and the exact ordered next actions;
- acceptance gates, invariants, scope exclusions, and decisions the successor
  must not silently reverse;
- blockers, deadlines, pending reviews/messages/owner obligations, and cleanup
  or retirement duties; and
- commands, paths, IDs, and targets needed to perform the next actions when
  those details exist in the source context;
- attempted approaches and ruled-out paths; and
- an evidence grade for each external-state claim: first-hand measured or
  inherited, with verified head/value/time where applicable.

The policy document specifies which obligations are required for the role.
`/btw` extracts the contextual why and continuation instructions from the
outgoing thread, while SM supplies and verifies machine-owned identifiers and
topology. Missing required sections fail the handoff before successor launch.

The generated artifact supersedes the handwritten predecessor state file for
this transition; policy must not require both. After the immutable artifact is
pinned, late findings go to a hash-linked append-only addendum rather than
rewriting the artifact.

Summarization alone cannot recover facts the outgoing holder does not recognize
as noteworthy. Each role policy therefore carries a short named-direction
retirement probe (for example: failed approaches, tool limitations, pending
human obligations, stale baselines, cleanup, and corrections to the artifact).
After orientation, the successor asks that probe directly by predecessor agent
ID; the predecessor answers in a bounded list. The answer becomes an addendum,
and rotation does not reach retirement-ready state until it is received or a
durable override records why it was skipped. Delivered-but-unacted events are
probe obligations distinct from queued-undelivered seat events.

Policy may declare an external transition-window predicate with earliest start
and must-complete-by conditions. A closed window defers rotation; “safe turn
boundary” alone is not sufficient when a zero-holder interval must fit between
detector ticks or another external cadence.

#### Rotation message streams

The outgoing and incoming incarnations receive different visible messages
because identity and behavior changes must not be implicit:

1. At preflight, the outgoing holder receives a control message stating the
   measured context, target threshold, that new discretionary work will pause,
   and the exact scoped-override command. It may finish its current atomic work.
2. At the next safe boundary, SM runs the handoff `/btw` invisibly. Its prompt
   and answer do not enter the outgoing main thread; this avoids polluting the
   context merely to extract state.
3. SM starts the successor in provisional `incoming` state with one immutable
   initial brief containing the target seat, predecessor agent ID, handoff
   artifact, policy version, current children, pending routed events, open
   review/cleanup manifests, and an instruction to verify identity with
   `sm me`. It cannot accept seat-routed discretionary work before commit.
4. Provider readiness and completion of that first orientation turn are
   observed by SM hooks; the successor does not need to remember an approval or
   ready command. Failure leaves the predecessor holding the seat.
5. The atomic commit changes the seat generation and routes, reparents the
   frozen child set, and attaches the predecessor beneath the successor.
6. The successor receives a visible commit message naming the seat and
   generation it now holds, the predecessor ID, transferred responsibilities,
   and the complete before/after ownership edge list. During orientation it
   independently re-derives that edge list from SM state. Queued seat work is
   then released to it.
7. The predecessor receives a direct agent-ID message stating that rotation
   completed, naming the successor and its new consult-only/cleanup obligations.

Visible control messages are used only where an agent's identity, authority, or
required behavior changes. Hidden `/btw` is used for state extraction. The owner
is notified only on failure, an override, or a policy-defined escalation; normal
policy-authorized rotation requires no human or per-agent approval.

`incoming` is available to any seat transition, including a subordinate that
must warm up off-pattern before becoming discoverable. Releasing a seat key and
retiring its former holder are separate ordered operations: routes commit to
the successor first; owned processes are stopped/retargeted; then retirement is
requested. Retirement completion requires policy-defined absence evidence,
including genuinely separated samples when the lane requires them. A stopped
provider process or a success notification is not verified absence.

## Execution plan

### Fast vertical slice and self-dogfood

The 2-to-4-day estimate below is full-epic completion, not time to first useful
trial. The first implementation objective is a narrow vertical slice that can
govern this epic's own subsequent spawns on the same day.

The slice reuses #1264 immutable prompt transport and #1265 isolated `/btw` and
contains only:

- an approved policy version plus the minimal materialized clauses needed for
  spawn provider/model/effort and budget/concurrency decisions;
- deterministic classification for explicit/named work, with `/btw` only for a
  genuinely ambiguous frozen request;
- atomic allow/rewrite/block before child allocation;
- actual provider/model/effort attestation after launch;
- linked decision, token, wall-time, child-lifecycle, forecast, and breaker
  evidence; and
- read-only owner/maintainer inspection surfaces.

Stable cross-lane seats, generalized routing, cleanup/relocation, context
profiles, and rotation are not prerequisites for this first slice. They remain
in the full plan and are added to the live canary only after their own review.
This avoids paying for the complete identity and lifecycle substrate before the
spawn-policy hypothesis produces evidence.

#### Bootstrap authority

The slice cannot authorize its own creation. PR #1269 plus the owner-approved
0F budget is the immutable bootstrap authority. Work performed before the slice
deploys may be imported as `bootstrap_observation` evidence, but SM must not
claim it was policy-enforced retroactively.

After the slice is reviewed and deployed, the next implementation spawn from
the canonical maintainer is the first active dogfood event. Capability N may
govern capability N+1 only after N has passed its own tests/review/deployment;
no capability supplies evidence for its own approval. Initially the canary is
bound to this maintainer incarnation and lane `sm-policy-1268`. Once stable-seat
identity lands, that binding migrates to the durable `maintainer` seat without
changing historical event ownership.

Every later package spawn is then atomic and active: the policy either returns
the child ID or an actionable rewrite/block. The maintainer may use a durable
scoped override, but the override and its forecast impact are part of the same
event. This dogfoods model selection, concurrency, budget, and intent handling
without exposing lane 355 to an unproven implementation.

D4 deploys an exact reviewed commit behind a canary scope that matches only the
bound maintainer lane/incarnation. The evidence record includes source commit,
built binary hash/signing identity, configuration digest, service restart and
health result, and prior rollback target. A canary fault restores the prior
binary/configuration without deleting the failed decision or usage evidence.

#### Inspectable evidence

The dogfood event chain is append-only and links:

1. frozen request, caller incarnation/seat, prompt digest, policy version, and
   applicable stable clause IDs;
2. extraction evidence, deterministic or `/btw` path, allow/rewrite/block,
   latency, and any override;
3. allocated child and provider-side model/effort attestation;
4. per-span input/cache-read/cache-write/output/reasoning tokens, dollar/quota
   estimate, queue wait, and active/elapsed wall time;
5. child completion, review rounds/findings, merge or retained-work outcome,
   cleanup state, and forecast delta; and
6. immediate machine benefit evidence followed by the Luna/high
   keep/change/remove assessment at its declared horizon.

Machine observations are visible immediately and labeled provisional until the
child reaches a terminal lifecycle point. Benefit judgments are separate
append-only records; they never rewrite the measured operation.

The initial read surfaces are deliberately small and reuse the existing CLI/API
rather than waiting for a new dashboard:

- `sm policy status --lane sm-policy-1268` shows active policy, canary scope,
  package state, actual/forecast tokens and time, and breaker state;
- `sm policy explain <decision-id>` shows the immutable request, evidence,
  clauses, decision, actual launch, and override;
- `sm policy events --lane sm-policy-1268 --json` exposes the append-only event
  stream for independent inspection;
- `sm policy trial --lane sm-policy-1268 --csv` emits per-requirement cost and
  benefit comparisons; and
- `sm watch` projects the same status, decisions requiring owner action, and
  warning/breaker events.

The registered owner can inspect every event and approve policy changes or hard
breaker continuation through the operator channel. The named maintainer can
read and explain all evidence for its own epic and issue ordinary scoped
overrides, but cannot impersonate owner approval or rewrite measured history.

#### Slice schedule and breaker

These are slices of the existing 1A/1C/2A/2B packages, not additional packages
or throwaway implementations:

| ID | Owner | Work | Dependency | Target |
|---|---|---|---|---|
| D0 | Maintainer Sol/high | Freeze policy-kernel, event, and admission interfaces plus dogfood fixtures, including `aa6c1120` omitted-tier/misreport evidence | approved spec/0F | 30-60 min |
| D1 | Terra/high | Minimal policy store/projection and deterministic decision kernel | D0 | 1.5-3 h; parallel |
| D2 | Terra/high | Append-only evidence ledger, forecast/breaker rows, and read-only CLI/API | D0 | 1.5-3 h; parallel |
| D3 | Sol/high | Atomic spawn admission and provider attestation using D0 interfaces | D0; integrates D1/D2 | 2.5-5 h; parallel start |
| D4 | Maintainer Sol/high | Integrate, run review protocol, deploy, and execute first governed spawn | D1-D3 | 1-2 h |

Target first dogfood is 4-8 elapsed hours, central estimate 6 hours, using at
most two Terra and one Sol implementation seat plus the maintainer. The slice's
incremental envelope is 100M-220M Codex tokens and no more than two bounded
Claude live probes. It is included inside the full-epic envelope below.

At 6 elapsed hours or 75% of the token envelope, SM/maintainer publishes a
same-day estimate-to-complete. At 8 hours, 220M tokens, or two failed integration
attempts without a governable spawn, the slice breaker fires: stop dispatch,
preserve evidence, and either simplify the slice or amend the spec with owner
approval. The response is not to continue toward the full 2-to-4-day build
without first obtaining trial evidence.

#### Economy controls

- Use the deterministic path before any model call. An immutable request gets
  at most one `/btw`; retries reuse its terminal decision unless a bound input
  changed and therefore created a new request.
- Select policy clauses by stable ID and scope. Do not inject unchanged policy
  history, unrelated lane clauses, or an arbitrary transcript tail.
- Reuse the existing SQLite/event, HTTP, CLI, `sm watch`, #1264 transport, and
  #1265 sidechain infrastructure in D0-D4. A new dashboard, generalized rule
  language, and cross-lane migration are outside the first slice.
- Freeze D0 interfaces before parallel dispatch so D1-D3 integrate rather than
  reimplement one another. An interface change invalidating two active packages
  fires the slice re-plan trigger.
- Treat the detailed rows below as 16 work items, not 16 mandatory PRs. Closely
  owned work with identical dependencies may share one bounded package; target
  10-12 reviewed merge packages for the full epic, including D1-D3.
- Run focused gates while iterating, full applicable package gates once at the
  exact review head, and the full integration matrix once at the epic head.
  A changed head reruns the gates affected by its delta; unchanged full suites
  are not repeated for ceremony.
- Do not repeat full role-context forks on a cadence. Luna/high reads frozen
  event/handoff artifacts first and requests a role fork only when a declared
  benefit question cannot be answered from machine evidence.

### Epic budget and completion forecast

The policy epic itself is governed by the same cost/execution tradeoff it adds
to SM. Before implementation dispatch, the maintainer publishes an immutable
baseline containing:

- the package DAG, tier and owner for each package, concurrency assumptions,
  critical path, and expected review iterations;
- projected agent-active, queue-wait, review-wait, maintainer-integration, and
  elapsed wall time;
- projected input, cache-read, cache-write, output/reasoning, dollar, and quota
  consumption by provider/model/tier; and
- explicit contingency and the evidence range used for each estimate.

The forecast is recomputed after every completed package and at every wave
boundary using actual child and maintainer telemetry. Original and revised
baselines remain visible; reforecasting never erases an overrun.

#### Initial planning baseline (2026-08-17)

The closest measured work from this maintainer tree is:

- three merged/deployed Terra packages (#1264, #1252, and #1265) took 20.7 to
  63.6 agent-minutes each, consumed 6.0M to 31.2M directly attributed tokens
  each, and together used 51.3M tokens over about 105 minutes of elapsed time
  with overlap;
- this maintainer recorded 67.2M directly attributed Sol tokens during the
  same policy/prerequisite work interval; its broader current-window total is
  2.858B and is retained only as an upper-scale historical proxy, not charged
  entirely to this epic; and
- the orchestrator role-review fork recorded 3.6M direct Claude tokens. The two
  other native forks currently lose exact fork-seat attribution; their session
  counters and the measured fork ratio produce an 8M to 15M best estimate for
  all three reviews rather than an `unknown` value. This attribution defect is
  part of telemetry work, not a reason to omit the cost.

Completed policy/prerequisite work is therefore 118.5M directly attributed
Codex tokens plus an estimated 8M to 15M Claude tokens. The initial total-epic
projection, including that sunk work, is 0.57B to 0.97B Codex tokens and 18M to
55M Claude tokens. Breakers compare both total-epic and remaining-work views so
already consumed budget cannot disappear at the implementation checkpoint.

The approved plan has 16 implementation work items: 4 Sol, 10 Terra, and 2
Luna, grouped into approximately 10-12 merge packages plus maintainer
integration and final review. Based on the observed package distribution and
added complexity, the initial remaining-work envelope is:

- **Codex tokens:** 0.45B to 0.85B, central estimate 0.62B;
- **Codex quota equivalent:** about 0.7 to 1.3 points using this maintainer's
  current 2.858B-token/4.3-point calibration, revised as tier-specific evidence
  arrives;
- **Claude live-path/review tokens:** 10M to 40M cache-weighted tokens, with no
  repeat full-role fork unless its benefit review requires one;
- **first active dogfood:** central 6 hours, bounded at 8 hours and 220M Codex
  tokens as specified above;
- **active critical-path work:** 15 to 24 hours; and
- **elapsed completion:** 2 to 4 calendar days with the stated four-agent cap,
  prompt owner checkpoints, and no external outage: approximately 2026-08-19
  through 2026-08-21 PDT if approved on 2026-08-17.

Owner approval time and external service outages are excluded from active-work
efficiency but remain explicit additions to the elapsed-date forecast. Each
forecast includes a projected completion timestamp, not only duration.

The current Codex account reading is 66% consumed with 34 points nominally
free, but its low-confidence fleet projection reaches about 100.9% at reset.
Before every wave, SM compares the epic's upper estimate-to-complete plus 25%
reserve against free quota after the measured fleet forecast. Insufficient
headroom blocks new dispatch and proposes rescheduling or a cheaper valid tier;
nominal account free space alone is not authorization to spend it.

#### Breakers and variance response

SM evaluates both token and elapsed-time forecasts continuously:

1. **Warning:** at 75% of either baseline, or whenever actual plus estimate-to-
   complete exceeds 110%, surface the variance and revised completion forecast
   in `sm watch`. Existing work may finish, but no new discretionary package is
   added without recording the forecast impact.
2. **Breaker:** when actual plus estimate-to-complete exceeds 125% of either
   approved envelope, or a critical-path package exceeds twice its estimate
   without a usable artifact, stop new implementation dispatch. Control-plane,
   cleanup, evidence preservation, and completion of the current atomic action
   continue. Resumption requires an owner-approved rebaseline or scope change.
3. **Hard overrun:** at 150% of either original envelope, the default action is
   to pause the epic even if a prior rebaseline would permit more. The owner
   must approve a new bounded phase with an explicit benefit case; an ordinary
   seat-holder override is insufficient.

Every breaker produces a variance report assigning the miss to one or more of:
estimate error, scope growth, architecture/specification error, agent/package
shape, review churn, telemetry failure, or external wait. The response follows
the cause:

- an invalid architecture or policy assumption amends this spec and repeats the
  owner checkpoint before dependent work resumes;
- scope growth is split, deferred, or added as a separately approved bounded
  phase rather than silently absorbed;
- an agent/package-shape miss changes tier, divides the package, or reduces
  concurrency without rewriting an otherwise valid contract;
- telemetry failure pauses the affected trial comparison until attribution is
  repaired; and
- external wait pauses the active-work clock but continues to move and report
  the calendar completion forecast.

For active-canary requirements, a cost overrun with demonstrated workflow
benefit triggers simplification or narrower scope. A requirement with high cost
and `no_observed_benefit` is disabled or removed by policy amendment. A workflow
regression is rolled back even when it saves tokens. This keeps project
completion bounded without treating either cost or execution quality as the
sole objective.

### Delivery topology and controls

- #1264 atomic prompt transport is merged and deployed.
- #1265 safe Claude `/btw` isolation is merged, deployed, and live-verified.
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
  active canary deployment, final epic review, and production rollout.

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
| 0E | Maintainer-owned Claude forks | Fork the latest lane orchestrator, Fable spec owner, and Opus engineer as read-only children; ask each to review policy adherence, operational pain, and token reduction from its lived role | 0B draft | Role-context findings folded into owner review; originals unchanged |
| 0F | Maintainer Sol/high | Publish the immutable token/wall-time baseline, completion timestamp, breaker thresholds, and current quota fit from maintainer-tree telemetry | 0B, current telemetry | Owner-approved epic execution envelope |

Owner checkpoint: approve the spec, fixture/ruling interpretation, and bounded
epic execution envelope before an epic branch is created.

### Wave 1 - independent foundations

Run these concurrently after the spec is approved. Their module and database
ownership must be fixed in the spec to keep overlap limited to registration
files.

| ID | Owner | Work | Dependency | Output |
|---|---|---|---|---|
| 1A | Maintainer Sol/high | Extend D1 into complete human-readable policy history, stable clause IDs, scoped rulings, operator-only approval, conflicts, and materialized enforceable effects | D1 dogfood evidence | Complete policy authority/store API |
| 1B | Sol/high | Lane-scoped seat identity, holder incarnations, dead-holder behavior, durable-artifact resolver, historical lifecycle, and migration compatibility | approved spec | Seat registry and resolver |
| 1C | Terra/high | Extend D2 into the complete requirement-effect ledger, linked spans, epic estimate-to-complete/breakers, evaluation/rotation records, calibrated counters, quota snapshots, JSON/CSV surfaces | D2 dogfood evidence, 0C, 0F | Complete telemetry API and CLI |
| 1D | Luna/high | Golden corpus harness, hostile/conflicting policy fixtures, coverage-counted sweeps, and restart/test scaffolding using frozen contracts | 0D corpus | Reusable test substrate |

### Wave 2 - spawn policy path

| ID | Owner | Work | Dependency | Parallelism |
|---|---|---|---|---|
| 2A | Terra/high | Extend D1 decision kernel for all named seats, ephemeral workers, role/provider context profiles (including Codex native-compaction seats), precedence, and scoped overrides | D1, 1A, 1D | Uses live dogfood findings |
| 2B | Sol/high | Extend D3 atomic `sm spawn` runner to stable-seat caller binding, generic current-turn delta, native `/btw`, strict parsing, and restart recovery | D3, 1A, 1B, 1C, 2A | Critical path generalization |
| 2C | Terra/high | `sm watch` policy document diff approval, decision explanation, and per-evaluation telemetry views | 1A, 1C | Parallel with 2A/2B behind API contracts |
| 2D | Terra/high | Existing-seat assignment gate, refusal/recusal, work reclassification, and one-shot evaluator-outage override surfaces | 1A, 2A, 2B | Parallel after 2B contract lands |

Acceptance gate: active overridable decisions over the golden corpus and
disposable live Claude/Codex sessions must produce no main-thread injection, no
orphan child, one terminal telemetry row per attempted spawn, and a complete
cost/benefit measurement contract for every enabled requirement.

### Wave 3 - runtime profiles and routing

| ID | Owner | Work | Dependency | Parallelism |
|---|---|---|---|---|
| 3A | Terra/high | Context profiles, initial-task to follow-up transitions, preflight/rotation/draining thresholds, and control-plane exceptions | 1A, 2A | Parallel |
| 3B | Terra/high | Route sends, monitors, email replies, queue/review wakes, reminders, and ownership through stable seat keys while preserving direct agent-ID addressing | 1B | Parallel |
| 3C | Luna/high | Inventory every persisted target/owner field and add mechanical routing/compatibility fixtures; report omissions to 3B | 1B | Parallel scout/test lane |
| 3D | Terra/high | Ephemeral-worker lifecycle and verified cleanup manifests, including external state/process ownership, review-pending succession, and fail-closed Git/GitHub checks | 1B, 2A, 2B | Parallel after contracts land |
| 3E | Terra/high | Workspace anchors and provider-neutral relocation: provider cwd, durable SM metadata, restore plan, and worktree cleanup commit or rollback together | 1B, 3D | Parallel with late 3A/3B |

Sol/high maintainer performs the cross-subsystem integration review after 3A
and 3B merge; this is review/integration work, not a separate implementation
agent.

### Wave 4 - rotation and overrides

| ID | Owner | Work | Dependency | Parallelism |
|---|---|---|---|---|
| 4A | Sol/high | Policy-authorized rotation transaction: evidence-graded handoff, named-direction probe/addendum, transition windows, successor readiness, seat/children/routes transfer, verified retirement, idempotency, rollback, and restart recovery | 2B, 3A, 3B | Critical path |
| 4B | Terra/high | Scoped override command/API/watch UX, draining presentation, audit history, expiry and consumption semantics | 1A, 3A | Parallel with early 4A |
| 4C | Terra/high | Hostile and restart matrix for partial spawn, stale holder, duplicate commit, route delivery during transfer, and recovery after each transaction boundary | 4A contract; implementation follows incrementally | Test lane |

### Continuous measured rollout

This begins with D4 before the full Wave 1/2 generalization and continues through
the later waves; it is not a final phase that waits for stable seats or rotation.

1. D4 enables the first end-to-end slice for `sm-policy-1268`, this maintainer's
   own implementation epic, as an **active, overridable canary**. The caller
   receives the real allow/rewrite/block result, and every subsequent package
   contributes evidence while building the remaining capabilities. Every policy
   result remains overridable by the seat holder with a durable scoped reason.
2. After at least three governed package spawns produce complete decision,
   launch-attestation, token, lifecycle, and forecast rows with no P1 admission
   defect, enable the proven capability subset for lane 355. This is a promotion
   of reviewed deployed code, not a second implementation or a shadow run.
   Lane-declared P0 artifacts and incident scopes are excluded from automatic
   gating or mutation; during a declared incident the canary fails closed and
   escalates rather than converting unavailable evidence into allow.
3. Add capabilities to the live canaries as their dependencies land:
   deterministic named-seat/model rules first, generic spawn classification
   second, context/draining third, workspace relocation/cleanup when available,
   and automatic rotation last. Do not wait for the complete epic before using
   an independently safe primitive. This capability order is binding; a later
   capability cannot be enabled before its listed dependencies and live gates.
4. Every attempted operation records the actual decision, override, resulting
   provider/model/effort, agent behavior, latency, token estimates or direct
   counters, and requirement-effect rows. Each newly enabled requirement must
   produce both an incremental-cost comparison and its declared workflow-benefit
   evidence. There is no separate counterfactual-only or shadow execution path.
5. Luna/high analyzes telemetry continuously. An initial 20-30-event sample is
   a useful optimization checkpoint, not an admission gate for unrelated work:
   report cache reads, latency, model mix, repeated-context amplification,
   overrides, policy/actual disagreement, golden-decision agreement, and a
   per-requirement keep/change/remove recommendation as data arrives. For
   handoffs, report summary and directed-probe costs separately and classify the
   probe's incremental findings and downstream actions. Sol/high adjudicates
   only semantic misses.
6. The maintainer also publishes actual versus forecast epic tokens, active
   work, elapsed time, completion date, and estimate-to-complete after each
   package. Warning/breaker state is visible in the same owner view as policy
   benefit telemetry.
7. Owner reviews decisions and telemetry on a rolling basis. A bad rule can be
   overridden immediately, amended conversationally, and re-approved without
   disabling already sound enforcement elsewhere in the lane.
8. Open one epic-to-main PR. The maintainer runs the full review protocol,
   deploys only after it exits, verifies Claude and Codex live paths, then
   removes all phase worktrees and retires all implementation seats.

### Expected critical path

`#1265 -> spec and 0F approval -> D0 -> D1/D2/D3 -> D4 own-epic canary -> 1A/1B/1C/1D -> 2A/2B -> lane-355 promotion -> 3A/3B/3D/3E -> 4A`

Telemetry (1C), corpus/tests (1D), watch UX (2C), routing audit (3C), and
override UX (4B) run beside that path. This preserves wall-clock parallelism
without assigning Luna/Terra work that requires Sol-level authority judgment.
