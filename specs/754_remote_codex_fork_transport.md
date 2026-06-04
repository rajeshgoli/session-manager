# Remote codex-fork Transport — Node-Routed Event Stream + AF_UNIX Control

Ticket: #754 (epic) · Follow-up to #752 (multi-node sessions)

## Summary

#752 ships remote session placement for **Claude only**: codex / codex-fork / codex-app are
rejected on a non-primary node. The reason is architectural — codex-fork's status and control ride
a **local IPC plane** (an event-stream JSONL the primary tails off its own disk, and a Unix-domain
control socket the primary `connect()`s locally) that does not cross the SSH-tmux transport.

This feature transports that plane across the node boundary so the single primary SM can place,
observe, control, and restore **codex-fork** sessions on a remote node at parity with Claude. The
primary stays the single brain — all event parsing, lifecycle reduction, observability storage, and
control logic remain on the primary; only the raw event bytes and control round-trips are bridged
from the node by a small **node-agent**.

Lifting the gate for codex-fork is what unlocks the single-master goal from the #752 discussion:
one SM that fully manages codex-fork sessions on either machine, including `sm restore`.

## Live Investigation (grounded code trace)

codex-fork's runtime is driven by two node-local files created by the codex binary at launch, plus
a primary-side monitor/controller. Verified against the code (no speculation):

- **Launch wires two local paths.** `_build_codex_fork_launch_spec()`
  (`session_manager.py:1260-1309`) appends `--event-stream <path>`, `--event-schema-version <v>`,
  and `--control-socket <path>` to the codex-fork argv. The paths come from
  `_codex_fork_event_stream_path()` → `log_dir/{id}.codex-fork.events.jsonl` (`:1214-1216`) and
  `_codex_fork_control_socket_path()` → `log_dir/{id}.codex-fork.control.sock` (`:1218-1220`). Both
  files are **deleted pre-launch** (`:1292-1298`); the codex binary **creates them on startup**.
- **Events OUT = a JSONL the primary tails locally.** `_monitor_codex_fork_event_stream()`
  (`session_manager.py:2680-2746`) seeks by per-session offset (`codex_fork_event_offsets`),
  polls every `codex_fork_event_poll_interval_seconds` (~0.5s), buffers partial lines
  (`codex_fork_event_buffers`), JSON-parses each line, normalizes the type
  (`_normalize_codex_fork_event_type`, `:1348-1371`), and ingests via `ingest_codex_fork_event`
  → `codex_event_store` (SQLite at `~/.local/share/claude-sessions/codex_events.db`), the lifecycle
  reducer, `_handle_codex_fork_turn_complete` (last-agent-message), Telegram relay, and
  `provider_resume_id` sync. Event types: `turn_started/complete/aborted`,
  `user_input_request/resolved`, `approval_request/resolved`, `error`, `thread_started`,
  `codex_fork_session_configured`, etc.
- **Control IN = a Unix-domain socket the primary connects locally.**
  `_codex_fork_control_roundtrip()` (`session_manager.py:5320-5339`) does
  `asyncio.open_unix_connection(socket_path)`, writes a JSON request + newline, reads one line back.
  Requests carry `{request_id, expected_epoch, command, **payload}` with epoch staleness retry
  (`:5341-5417`); commands include `get_epoch` and `set_thread_name`
  (`_rename_codex_fork_thread_via_control_socket`, `:4392`). Readiness =
  `_codex_fork_runtime_reachable()` (`:1378-1397`) which `connect()`s `socket.AF_UNIX` (`:1388`).
- **Restore relaunches with resume.** `restore_session()` codex-fork branch (`:6717-6753`) gets the
  resume id via `get_session_resume_id` → `_get_codex_resume_id_from_events` (reads
  `codex_events.db`, fallback persisted `session.provider_resume_id`), rebuilds the launch spec with
  `["resume", resume_id, ...]`, recreates tmux, resets offsets/buffers, restarts the event monitor.
- **The gate.** `_provider_node_rejection(provider, node)` (`session_manager.py:497-500`):
  `node != primary and provider != "claude"` → reject. Enforced at create (`:2789`), restore
  (`:6718`), and the public `validate_create_node_provider` (`:512`).
- **#752 transport surface.** `NodeRunner` (`src/node_runner.py`): `run`/`run_async`,
  `attach_command` (adds `-tt`), `ping`, SSH ControlMaster options (`control_path`,
  `ConnectTimeout`, optional `ProxyCommand`), and `NodeConfig` (ssh, control_path, api_url,
  hook_base_url, hook_secret, projects_root). tmux/pipe-pane already route through this; the codex
  IPC plane does not.

Why it cannot ride the existing transport: a Unix-domain socket is a filesystem object on one host
— you cannot `connect()` to a remote host's socket — and the JSONL is read from the primary's own
filesystem. SSH-tmux carries the pane and pipe-pane logs, not these two channels.

## Problem

A remote codex-fork session would **spawn** (tmux over SSH works) but the primary would get **no
events** (lifecycle/status/turn-complete/last-message/approvals/fork all come from the JSONL it
can't read) and could **send no control** (the AF_UNIX socket it can't reach). That is a
half-managed session, so #752 rejects it outright. Same logic blocks remote restore.

## Goals

1. Place a **codex-fork** session on a registered remote node; the codex-fork runtime runs on the
   node, the primary observes and controls it at parity with a local codex-fork session.
2. Full **event parity**: lifecycle reduction, turn-complete/last-agent-message, approvals, errors,
   fork lineage, `provider_resume_id` sync, and `codex_events.db` ingestion all work for remote
   sessions.
3. Full **control parity**: `get_epoch`, `set_thread_name`, and the rest of the control round-trip
   protocol (incl. epoch staleness retry) work against a remote runtime.
4. **Restore** a stopped remote codex-fork session (resume) onto its node.
5. The **primary stays the single brain** — parsing, reducers, lifecycle state, observability DB,
   Telegram relay all remain on the primary; the node bridges only raw event bytes and control
   round-trips.
6. **Node liveness**: a remote codex-fork on an unreachable node surfaces `node-unreachable`
   (reusing #752 semantics), not silent death; reconnect resumes cleanly.
7. Lift `_provider_node_rejection` for **codex-fork** specifically. Default (primary) behavior is
   byte-for-byte unchanged.

## Non-Goals

- **codex-app remote** — it has its own app-server plane; out of scope here unless it reduces to
  the same bridge (separate evaluation). The gate stays for codex-app.
- Offline / multi-master operation (separate from transport).
- Changing the Claude remote path or the tmux/pipe-pane transport.
- Moving event parsing/reduction onto the node — the node-agent is a dumb bridge by design.

## Recommended Design

**A per-node "node-agent" bridges codex-fork's local IPC to the primary over a single authenticated
WebSocket.** The node-agent owns the node-local event JSONL and control socket; the primary keeps
all logic behind a transport interface.

### Topology — one node-initiated WebSocket

The node-agent **dials the primary** and holds one authenticated WebSocket (outbound from the node,
exactly like the hook model — so **no inbound port on the node beyond sshd**). That single socket
carries both directions:

- **events** node → primary (the agent pushes raw JSONL lines as they appear);
- **control RPCs** primary → node (the primary sends a control request frame; the agent performs
  the local AF_UNIX round-trip and returns the response on the same socket).

Auth reuses the per-node shared secret (`nodes.<id>.hook_secret`, or a dedicated `node_token`) sent
on connect; the channel rides the trusted path (LAN/tunnel) like `/hooks/*`.

### Primary side — a transport interface

Introduce `CodexForkTransport` with two implementations selected by `session.node`:

- `LocalTransport` (primary node): today's behavior — read the local JSONL with offsets, connect the
  local AF_UNIX socket. Behavior-neutral.
- `RemoteTransport` (remote node): subscribe to the node-agent's event push for this session;
  send control round-trips as RPCs over the node-agent WS.

`_monitor_codex_fork_event_stream` and `_codex_fork_control_roundtrip` swap their raw I/O to the
transport. **Everything downstream is unchanged** — normalization, `ingest_codex_fork_event`, the
reducer, turn-complete, Telegram, `codex_events.db`, epoch management all stay on the primary and
operate on bytes the transport delivers. This is the key property: the bridge is transparent;
parity is structural, not re-implemented.

### Node-agent responsibilities (deliberately minimal)

- Watch the node-local event JSONL for each active remote codex-fork session (it owns the offset +
  partial-line buffer **locally**, so a primary restart resumes by replaying from a primary-tracked
  last-seq, not a byte offset the primary no longer holds — see Edge Cases). Push raw lines.
- Accept control RPCs, perform `asyncio.open_unix_connection` against the local control socket, relay
  the one-line response. The agent never parses or interprets — it is a byte/line pipe.
- Report socket/file readiness (mirror `_codex_fork_runtime_reachable`) so the primary knows when the
  codex binary has created the socket post-launch.

### Launch & restore

codex-fork is spawned on the node via the existing `NodeRunner`/tmux path with `--event-stream` /
`--control-socket` at **node-local** paths (under the node's `log_dir`). The primary tells the
node-agent the session id + paths to bridge. Restore is identical: relaunch on the node with the
resume id (discovered on the primary from `codex_events.db` / `provider_resume_id`, both of which
live on the primary and are fed by the streamed events — so resume-id discovery is already portable),
and the node-agent re-tails / reconnects.

### Lifting the gate

`_provider_node_rejection` is relaxed to allow `codex-fork` on non-primary nodes **once a healthy
node-agent transport exists for that node**; codex-app stays rejected. The gate becomes
provider-and-capability aware rather than Claude-only.

## Control Protocol Over the Bridge

The existing request frame `{request_id, expected_epoch, command, **payload}` and one-line JSON
response are tunneled verbatim as a WS RPC: the primary sends `{type: "control", session_id, frame}`
and the agent replies `{type: "control_result", request_id, line}`. Epoch logic stays entirely on
the primary (`:5341-5417`); the agent is transparent, so `get_epoch`, `set_thread_name`, and stale-
epoch retry behave identically. Timeouts map to the same `RuntimeError` surfaces.

## Data Model / Config

- No new `Session` field — `session.node` already exists (#752) and selects the transport.
- Node-agent connection auth: reuse `nodes.<id>.hook_secret` or add `nodes.<id>.node_token`.
- Per-session bridge registration is runtime state on the primary (which node-agent serves which
  session), not persisted in `sessions.json`.
- Primary tracks a per-session **last applied event seq** (already implied by `codex_event_store`
  seq) so reconnect/restart resumes without a node-side byte offset.

## Security

- The control channel can drive a live codex-fork runtime (rename, and any future steer/input/
  interrupt), so the node-agent WS **must** be authenticated (per-node secret/token) and ride the
  trusted path (LAN or authenticated tunnel); reject unauthenticated connects.
- Validate `session_id` ownership on control RPCs (the agent only operates sockets for sessions the
  primary has registered to it) to prevent a compromised primary-side bug from poking arbitrary
  sockets.
- The node-agent only opens sockets/files under the node's `log_dir`; never arbitrary paths from the
  wire.

## Alternatives Considered

- **SSH Unix-socket forwarding + `ssh tail -F`** (no node-agent). Forward the control socket with
  OpenSSH `-L`/StreamLocalBind and stream the JSONL via a persistent `ssh tail -F`. Rejected: the
  control protocol is bidirectional request/response with epoch state, which maps poorly onto a raw
  forwarded socket across reconnects; the socket is created *after* launch (forward must wait/retry);
  and offset/partial-line semantics across reconnects are fragile. It also still needs a supervisor
  to manage the two SSH channels per session — at which point a node-agent is cleaner.
- **Per-session helper over SSH** instead of a per-node daemon. Rejected: N management surfaces vs
  one; the per-node agent amortizes the connection and reuses one WS for all sessions on the node.
- **Move parsing/reduction to the node.** Rejected: that splits the brain and duplicates the reducer/
  observability logic; keeping the node dumb preserves the single-master invariant.

## Implementation Plan (sub-tickets)

1. **`CodexForkTransport` abstraction** — extract the raw event-read and control-socket I/O in
   `_monitor_codex_fork_event_stream` and `_codex_fork_control_roundtrip` behind an interface;
   `LocalTransport` reproduces today exactly. Behavior-neutral on the primary.
2. **Node-agent service** — node-local daemon (ships in the SM package the node already installs):
   tails the event JSONL and pushes lines; accepts control RPCs and relays the AF_UNIX round-trip;
   reports runtime readiness. Single authenticated WebSocket dialed to the primary.
3. **`RemoteTransport` + wiring** — implement the primary-side transport over the node-agent WS;
   select transport by `session.node`; manage node-agent lifecycle/health per node and per session.
4. **Remote spawn + restore + gate** — spawn/restore codex-fork on a node end-to-end; relax
   `_provider_node_rejection` for codex-fork (keep codex-app); validate resume-id flow.
5. **Liveness / reconnect** — `node-unreachable` for remote codex-fork; resume from last-seq on
   reconnect and on primary restart; readiness wait for the post-launch socket.
6. **Attach** — `sm attach` for a remote codex-fork (detached-runtime descriptor over the node,
   building on #327's attach descriptor which already carries `control_socket_path`/
   `event_stream_path`/lifecycle).

## Tests

- Unit: `CodexForkTransport` selection by node; control-frame tunneling round-trip incl. stale-epoch
  retry; event-line buffering across a simulated reconnect.
- Integration (local fake-remote via `ssh localhost` + a node-agent): spawn a remote codex-fork;
  assert lifecycle/turn-complete/last-message/approval events reach the primary's reducer and
  `codex_events.db`; `set_thread_name`/`get_epoch` succeed; restore resumes on the node.
- Liveness: kill the node-agent → control surfaces unreachable and lifecycle shows `node-unreachable`
  (no `_handle_session_died`); restart → events resume from last seq, no duplicates/gaps.
- Regression: primary-local codex-fork unchanged (LocalTransport); codex-app on a node still rejected.

## Edge Cases

- **Socket created post-launch** — the agent retries readiness until the codex binary creates the
  control socket; control RPCs before readiness return a clear `not-ready` error, not a hang.
- **Primary restart mid-session** — primary resumes from its last persisted event seq
  (`codex_events.db`), asking the agent to replay from there; dedupe by seq so no double-ingest.
- **Partial JSON across a reconnect** — line buffering lives on the node-agent (local to the file),
  so a primary/WS reconnect never splits a line.
- **Fork lineage** — `thread_started`/`codex_fork_session_configured` events must arrive in order so
  `provider_resume_id` and fork detection stay correct; the agent preserves file order.
- **Node-agent crash vs node down** — distinguish (agent gone but ssh up = restart agent; node
  unreachable = `node-unreachable`).

## Acceptance Criteria

- `sm spawn codex --node <node>` (and `sm codex --node <node>`) runs the codex-fork runtime on the
  node; `sm all` shows `node=<node>`, provider codex-fork.
- The primary's lifecycle/status, last-agent-message, approvals, and `codex_events.db` reflect the
  remote runtime identically to a local one.
- Control round-trips (`set_thread_name`, `get_epoch`, stale-epoch retry) succeed against the remote
  runtime.
- `sm restore <id>` resumes a stopped remote codex-fork on its node.
- A remote codex-fork on a downed node reports `node-unreachable`, and recovers (events resume, no
  duplicates) on reconnect.
- codex-app on a non-primary node is still rejected; primary-local codex-fork is byte-for-byte
  unchanged and existing tests pass.

## Ticket Classification

**Epic.** Spans a transport abstraction, a new node-agent service with an authenticated bidirectional
channel, primary-side rewiring of the event monitor and control round-trip, spawn/restore/gate
changes, liveness/reconnect, and attach. Sub-tickets follow the six-step plan; step 1 lands
behavior-neutral. File sub-tickets referencing this spec before implementation.
