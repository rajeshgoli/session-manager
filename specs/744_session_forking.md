# SM-Owned Session Forking

Issue: #744

## Summary

Add a first-class Session Manager fork workflow so a user can fork a provider
conversation from the normal `sm watch` surface without losing either SM
identity:

```bash
sm watch
# select a session, press F to fork it, then press Enter to attach

sm fork <session> [--name NAME] [--attach] [--json]
sm fork --self [--name NAME] [--attach]
```

The primary v1 UX is: open `sm watch`, select a session, press `F`, get a new
SM session backed by a native provider fork, optionally rename it, then press
`Enter` to attach and keep working normally. The source session keeps its
original provider resume id, and restore now has two clean SM keys. Users should
not have to run provider-native `/fork` inside the source pane and then repair
SM state by hand.

## Live Investigation

On 2026-05-14, a disposable Codex-fork probe confirmed the reported failure
mode.

1. Created SM session `1f731576` with provider `codex-fork`.
2. Before fork, the session had `provider_resume_id =
   019e2869-e5bd-7072-89a8-7c2e525d569a`.
3. Driving a native provider fork in the same tmux pane produced a
   `codex_fork_thread/started` event whose payload contained:

   ```json
   {
     "thread": {
       "id": "019e286a-8646-7080-982a-9ef016d36b7c",
       "forkedFromId": "019e2869-e5bd-7072-89a8-7c2e525d569a"
     }
   }
   ```

4. The SM session id stayed `1f731576`, but `provider_resume_id` changed to
   `019e286a-8646-7080-982a-9ef016d36b7c`.
5. No separate live SM session represented the original provider thread.

A second probe showed that `sm send <session> '/fork'` is not a native slash
command path for Codex-fork. The message is wrapped as normal agent input and
the agent interprets it as work. A real fork command must use provider-specific
control, not generic `sm send` delivery.

## Problem

Provider-native fork commands mutate provider conversation state inside the
current runtime. Session Manager currently treats the runtime as the durable
identity and updates `provider_resume_id` in place when the provider reports a
new thread. That creates two problems:

1. The original SM id silently changes meaning from "the original thread" to
   "the forked thread".
2. The original provider thread can become reachable only through provider
   output or manual resume commands, not through a live or restorable SM
   session record.

This is especially confusing because SM ids are used for routing, Telegram
topics, watch rows, parent-child context, reminders, and restore.

## Goals

1. Add an SM-owned fork workflow that creates a new SM session for the fork and
   preserves the source session's provider resume id.
2. Support tmux-backed providers that expose native fork behavior: `claude`,
   `codex`, and `codex-fork`.
3. Preserve fork lineage in durable state so `sm watch`, `sm me`, restore, and
   future tooling can explain where a fork came from.
4. Avoid ordinary `sm send` for native fork commands.
5. Handle stopped/restorable source sessions when they have enough provider
   resume metadata.
6. Detect manual provider-native forks where possible and avoid discarding the
   pre-fork provider resume id.
7. Make `sm watch` the ergonomic primary surface for forking, attaching to the
   fork, and seeing both source/fork sessions.
8. Keep existing restore behavior intact.

## Non-Goals

1. Do not build an SM transcript browser or custom message-level fork picker in
   v1.
2. Do not support `codex-app` unless the app-server exposes a stable thread
   fork API. Headless fork support can be added later.
3. Do not guarantee that manual provider `/fork` inside an existing pane can
   preserve the original SM id. The supported path is an SM-owned fork action:
   watch `F` or `sm fork`.
4. Do not change parent-child orchestration semantics. Fork lineage should use
   dedicated fields, not overload `parent_session_id`.
5. Do not make fork creation depend on live display identity discovery or
   Telegram network calls.

## User Experience

### Primary Watch Flow

The main user-facing workflow should live in `sm watch`.

```bash
sm watch
```

In watch mode:

1. The user selects the source session.
2. The user presses `F`.
3. Session Manager creates a new fork session from the selected source.
4. The new fork row appears in watch as a normal SM session.
5. The fork row is selected automatically.
6. The user may rename it with existing watch/name affordances.
7. Pressing `Enter` attaches to the fork session, using the same attach behavior
   as any other tmux-backed row.

This is the primary surface because it matches the operator's normal flow:
watch sessions, choose one, fork it, then enter the new session and continue
working.

The watch action should show a short status/flash message with both ids:

```text
Forked maintainer (5a116fe5) -> maintainer-fork-a1b2c3d4 (8c2f901a)
```

If the fork fails, the watch flash should show the same concise error the CLI
would return, without changing selection away from the source session.

`sm watch --restore` should also benefit indirectly: after a fork, restore has
two clean SM records/keys, one for the source and one for the fork. No special
restore-mode fork action is required in v1.

### Current Session

If the caller is inside an SM-managed session:

```bash
sm fork --self
sm fork --self --name spike-from-review --attach
```

`--self` resolves to `CLAUDE_SESSION_MANAGER_ID`. The command should fail
clearly if it cannot determine the current SM session.

### Target Session

Operators can fork any resolvable session:

```bash
sm fork maintainer --name maintainer-fork
sm fork 1f731576 --attach
```

Target resolution should follow the same session/alias resolution used by
`sm attach` and `sm restore`, with ambiguity errors instead of arbitrary
matches.

### Output

Text output should make both identities visible:

```text
Forked maintainer (5a116fe5)
Original provider thread: 019e2869-e5bd-7072-89a8-7c2e525d569a
Fork session: maintainer-fork (8c2f901a)
Fork provider thread: 019e286a-8646-7080-982a-9ef016d36b7c
```

JSON output should include at least:

```json
{
  "source_session_id": "5a116fe5",
  "source_provider_resume_id": "019e2869-e5bd-7072-89a8-7c2e525d569a",
  "fork_session_id": "8c2f901a",
  "fork_provider_resume_id": "019e286a-8646-7080-982a-9ef016d36b7c",
  "provider": "codex-fork"
}
```

### Attach

`--attach` should attach to the fork session after the provider fork is
confirmed. Without `--attach`, the command should leave both source and fork
addressable through normal SM commands.

For the primary watch flow, attach is not a fork option. The fork row should
become selected after creation; the existing `Enter` action attaches to it.

## Recommended Design

`sm fork` should create a new SM runtime and run the provider-native fork flow
there, seeded from the source provider resume id. It should not send `/fork`
into the source pane.

For tmux-backed providers:

1. Resolve the source session and snapshot its provider resume id.
2. Allocate a new `Session` with a new SM id, same provider, same working
   directory, same model, and optional user-provided friendly name.
3. Store fork lineage on the new session before launch.
4. Start the new provider runtime by resuming the source provider thread.
5. Drive the provider-native fork command in the new runtime using a
   provider-specific adapter.
6. Wait for provider evidence that a new fork thread exists.
7. Bind the new SM session to the fork provider resume id.
8. Leave the source session unchanged.

This gives the provider a native fork while ensuring the forked runtime is
already owned by a distinct SM id.

## Data Model

Add durable lineage fields to `Session`:

```python
forked_from_session_id: Optional[str]
forked_from_provider_resume_id: Optional[str]
forked_provider_resume_id: Optional[str]
forked_at: Optional[datetime]
forked_by_session_id: Optional[str]
```

Field meanings:

- `forked_from_session_id`: SM session that was forked.
- `forked_from_provider_resume_id`: provider resume id captured before fork.
- `forked_provider_resume_id`: provider resume id produced by the fork. This is
  usually the same as `provider_resume_id` on the fork session, but keeping it
  explicit makes lineage stable if restore metadata changes later.
- `forked_at`: when SM confirmed the fork.
- `forked_by_session_id`: caller session id, if known.

Do not use `parent_session_id` for this. Parent-child relationships mean
orchestration ownership today and drive other behavior.

## Provider Adapters

Introduce a provider-facing fork adapter layer, for example:

```python
class ProviderForkResult(TypedDict):
    provider_resume_id: str
    forked_from_provider_resume_id: str

async def fork_provider_session(source: Session, fork: Session) -> ProviderForkResult:
    ...
```

The exact shape can differ, but implementation should keep provider-specific
details out of CLI command code.

### Codex-Fork

Observed behavior gives a concrete v1 path:

- launch the fork session with `codex resume <source_resume_id>` through the
  existing codex-fork launch builder;
- drive native fork control in that new tmux runtime, not through `sm send`;
- watch the existing codex-fork event stream for `thread/started` with
  `thread.forkedFromId == source_resume_id`;
- use `thread.id` as the fork session `provider_resume_id`;
- keep the source session `provider_resume_id` unchanged.

The event reducer currently updates `provider_resume_id` whenever event
`session_id` changes. That behavior must become fork-aware so a fork operation
does not silently rewrite the wrong session.

### Codex

Plain `codex` should follow the same shape if its local event/session metadata
can expose the new thread id. If the implementation cannot reliably observe the
fork result for plain `codex`, v1 may return a clear unsupported-provider error
for `codex` and document the gap in the PR.

### Claude

Claude should use the provider-native fork command in a new SM-owned tmux
runtime seeded by the source transcript/resume id. The implementation must
verify how Claude exposes the forked transcript path or resume id before
declaring support.

If Claude only exposes the fork result through a transcript path, the fork
session should use that transcript stem as `provider_resume_id`, matching the
restore model already used for Claude sessions.

### Codex-App

`codex-app` is out of scope unless a stable headless thread fork API exists.
The command should fail clearly:

```text
Session forking is not supported for provider=codex-app yet.
```

## API

Add a backend endpoint:

```http
POST /sessions/{session_id}/fork
```

Request body:

```json
{
  "name": "optional-friendly-name",
  "attach": false,
  "fork_point": "current"
}
```

Response body:

```json
{
  "source_session": { "...": "existing session response shape" },
  "fork_session": { "...": "existing session response shape" },
  "source_provider_resume_id": "019e2869-e5bd-7072-89a8-7c2e525d569a",
  "fork_provider_resume_id": "019e286a-8646-7080-982a-9ef016d36b7c"
}
```

`fork_point` is reserved for future expansion. V1 may accept only `current` and
return `400` for any other value.

## Manual Native Fork Detection

The canonical path is `sm fork`, but users can still run provider `/fork`
manually inside a pane. When provider events make this detectable, SM should
avoid losing the old resume id.

For Codex-fork, if a session receives `thread/started` with `forkedFromId` and
there is no pending SM fork operation for that session:

1. Preserve the previous `provider_resume_id` in durable fork history or a
   stopped restorable snapshot.
2. Update the current session only after recording the fork relationship.
3. Surface enough state in logs/watch details for a maintainer to understand
   that this was an in-place manual fork.

V1 does not need to remap the running tmux process to a different SM id after a
manual fork. That is risky because the provider runtime, event stream path, and
environment already carry the original SM id. The main requirement is: do not
discard the original provider resume id silently.

## Command And CLI Changes

Add:

```bash
sm fork [session] [--self] [--name NAME] [--attach] [--json]
```

Rules:

1. Either `session` or `--self` is required.
2. `--self` and `session` together should fail.
3. If `--name` is omitted, derive a non-conflicting name from the source
   effective display name, for example `<source>-fork-<shortid>`.
4. If the source has no provider resume id or transcript path, fail clearly.
5. If the provider is unsupported, fail clearly.
6. `--attach` attaches only after the fork session has a confirmed provider
   resume id.

## Watch, Restore, And Display

`sm watch` is the primary fork surface in v1. Add an `F` key binding in normal
watch mode:

| Key | Action |
| --- | --- |
| `F` | Fork the selected session and select the new fork row. |
| `Enter` | Attach to the selected session, including a newly created fork. |

The watch footer/help should advertise `F` only when the selected row is a
forkable session, or always show it with a clear failure message for unsupported
rows. The implementation can choose the simpler display, but pressing `F` on a
non-session row or unsupported provider must fail clearly.

`sm all` does not need a new default column in v1. Session detail surfaces
should include fork lineage where space allows:

```text
Forked from: maintainer (5a116fe5)
Original provider thread: 019e2869-e5bd-7072-89a8-7c2e525d569a
```

Restore should work normally because the fork session has its own
`provider_resume_id`. The source session should remain restorable or live under
its original `provider_resume_id`. This is the important restore invariant:
after a fork, source and fork are two clean SM keys that can be independently
restored.

Telegram topic creation for the fork should follow normal session creation
behavior. Slow Telegram sync must not block fork correctness.

## Implementation Plan

1. Add fork lineage fields to `Session` serialization/deserialization.
2. Add `SessionManager.fork_session(source_id, name=None, fork_point="current",
   forked_by_session_id=None)`.
3. Add a provider adapter for `codex-fork` first, based on the observed
   `thread/started` event with `forkedFromId`.
4. Add backend `POST /sessions/{id}/fork`.
5. Add CLI parsing and client method for `sm fork`.
6. Add an `F` action to normal `sm watch` that calls the fork API for the
   selected row, refreshes the list, selects the new fork row, and lets existing
   `Enter` attach behavior take over.
7. Make Codex-fork event ingestion fork-aware so pending SM fork operations
   bind the new thread to the new SM session and manual forks preserve the old
   resume id.
8. Add Claude and plain Codex adapters only after live verification proves how
   to observe the fork result. If either provider is not reliable in the same
   PR, leave a clear unsupported-provider error and update the issue.
9. Add watch/detail display of fork lineage if the existing detail payload has
   room. Do not add blocking live probes.

## Tests

Add focused tests around the state transition, not just CLI parsing:

1. `fork_session` creates a new SM session and does not mutate the source
   `provider_resume_id`.
2. The fork session persists lineage fields and its own
   `provider_resume_id`.
3. Codex-fork `thread/started` with matching `forkedFromId` completes a pending
   fork operation.
4. Codex-fork `thread/started` without a pending operation records manual fork
   lineage before updating the current session.
5. Unsupported providers return a clear error.
6. Missing source resume metadata returns a clear error.
7. CLI `sm fork --self`, `sm fork <id> --name ...`, `--json`, and invalid
   argument combinations behave correctly.
8. Watch `F` calls the fork API for the selected row, selects the new fork row
   on success, and shows clear errors for unsupported rows.
9. Restore still uses the correct resume id for both source and fork sessions.

Live manual verification for the implementation PR should include one
Codex-fork session and record the source/fork provider ids before and after.

## Edge Cases

- Source session is running: `sm fork` should not inject commands into the
  source pane.
- Source session is stopped: `sm fork` may still work if restore metadata is
  present; the source should remain stopped.
- Source session is busy: the new fork runtime is independent, so this should
  not interrupt source work.
- Duplicate friendly names: use existing name collision behavior and append
  the new short id when deriving a default fork name.
- Fork command times out: stop/retire the partially created fork session or mark
  it failed with a clear error; do not mutate the source.
- Service restart during pending fork: on restart, pending fork state should
  either be recovered from durable fields or marked failed. Do not leave a
  running fork runtime without a visible SM session.
- Provider emits a fork event where `forkedFromId` does not match the source
  snapshot: fail the operation and keep the source unchanged.

## Acceptance Criteria

1. `sm fork <session>` creates a new SM session for a provider-native fork.
2. The source session retains its original SM id and original provider resume
   id.
3. The fork session has its own SM id, provider resume id, tmux session, and
   optional friendly name.
4. In `sm watch`, selecting a forkable row and pressing `F` creates the fork
   session, refreshes the list, and selects the fork row.
5. Pressing `Enter` on that selected fork row attaches to it through the
   existing watch attach path.
6. Fork lineage is durable and visible in session detail output.
7. Generic `sm send` is not used to drive native fork commands.
8. Codex-fork support is backed by event evidence, specifically
   `thread/started.thread.forkedFromId`.
9. Unsupported providers or missing resume metadata fail with explicit messages.
10. Manual native forks do not silently discard the pre-fork provider resume id
   when the provider emits detectable fork metadata.
11. Existing create, send, watch, and restore behavior remains unchanged for
   non-fork sessions.
12. Tests cover state preservation, lineage persistence, provider event
    handling, watch `F` behavior, CLI behavior, and restore resume ids.

## Ticket Classification

Single implementation ticket if the first implementation is scoped to
Codex-fork plus clear unsupported-provider errors for providers that cannot be
verified. If Claude and plain Codex both require separate provider-specific
reverse engineering, split those adapters into follow-up tickets after the
Codex-fork core lands.
