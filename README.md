# Session Manager

## Dispatch teams of Claude and Codex agents

**Agent swarms without babysitting.** Spawn real Claude and Codex sessions,
route work between them, keep every agent observable, and jump in from your
terminal or phone when the swarm needs a nudge.

Session Manager is built for the messy reality of running many agents at once:

- **Fast enough to stay out of the way**: migration baselines show roughly
  85-90% lower server memory and common read paths around 3x-20x faster than
  the previous Python service.
- **Ruggedized for real operations**: Rust owns the runtime, state stores have
  backup/restore and freeze/drain gates, and cutover evidence is executable
  instead of vibes.
- **Secure on the public edge**: the mobile app path is designed for
  Cloudflare Access mTLS first, then origin-side auth, then route-local shell
  proofs before terminal attach.
- **Remote when you need it**: the Android app is the on-the-go command center;
  email/human-recipient delivery remains the fallback for replies and alerts.

```
┌─────────────────────────────────────────────────────────────────┐
│                         ORCHESTRATOR                            │
│                    "Implement Epic #987"                        │
└──────────────┬────────────────┬────────────────┬───────────────┘
               │                │                │
         sm spawn          sm spawn          sm spawn
               │                │                │
               ▼                ▼                ▼
        ┌──────────┐     ┌──────────┐     ┌──────────┐
        │ Engineer │     │ Architect│     │  Scout   │
        │  Agent   │     │  Agent   │     │  Agent   │
        └────┬─────┘     └────┬─────┘     └────┬─────┘
             │                │                │
             └────────────────┼────────────────┘
                              │
                         sm send em
                        "done: PR #42"
                              │
              ┌───────────────┼───────────────┐
              ▼                               ▼
    ┌─────────────────┐             ┌─────────────────┐
    │ Orchestrator    │             │ Phone shows the │
    │ routes next     │             │ swarm in flight │
    └─────────────────┘             └─────────────────┘
```

---

## Agent Nirvana

**Let agents swarm loose on your problems while you do something better than
watching terminals.**

No more opaque subagents you cannot follow. Every agent is a full terminal
session with durable state and a real lifecycle.

- **Watch from anywhere** — the Android app shows live sessions, activity, app
  updates, analytics, and mobile terminal attach.
- **Jump in anytime** — `sm attach engineer` opens the actual tmux session.
- **Coordinate without polling** — `sm send`, parent wakes, stop notifications,
  and queue-backed delivery keep work moving while coordinators sleep.
- **Keep the fallback** — email/human-recipient delivery keeps a simple reply
  path when the app is not the right tool.
- **Real sessions** — not abstractions. Real tmux. Real Claude Code. Real
  Codex. `sm attach` and you are there.

```
Your Phone                           Your Agents
──────────                           ───────────
                                     EM: "Spawning engineer for #123"
[engineer spawned]             <────
                                     Engineer: working
                                     Engineer: "done: PR #456 created"
[engineer -> EM: done]         <────
                                     EM: "Routing to architect"
[architect review requested]   <────
                                     Architect: reviewing
[architect: approved]          <────
                                     EM: "Merging..."
[PR #456 merged]               <────

You: not babysitting terminals
```

---

## Why This Exists

**Problem:** agents burn context while waiting, subagents are hard to inspect,
and parallel work gets chaotic when every terminal is its own little island.

**Solution:** a central manager that lets agents go idle, wakes the right
session when something happens, and keeps the whole swarm visible. Spawn
workers, send them work, go to sleep, wake on signal, and attach only when you
need to intervene.

**Result:** complex multi-agent workflows with less token waste, lower operator
load, and a real control plane for agent work.

---

## What It Enables

### Agent Swarms

Spawn specialized agents that work in parallel. Engineer implements while
Architect reviews while Scout investigates. You keep the graph, not every token.

### Full Transparency

Every agent is a real Claude Code or Codex session. No black boxes. `sm attach`
to any session. `sm tail` when you want the recent trail. Android when you are
away from the desk.

### Async Orchestration

The Orchestrator/EM pattern: fan out known work, route tickets to the right
agents, collect results, and wake only on useful state changes. Never wait
synchronously unless you choose to.

### Named Maintainers

The Maintainer pattern is different: one named agent keeps infrastructure and
long-running project state healthy. It is the session you page when the system
itself needs attention.

### Remote Control

On the go? Use the app. Need a low-friction fallback? Use email/human delivery.
Need to debug? Attach from a terminal and take over the live pane.

### Workspace Coordination

State, parent/child relationships, queue jobs, review requests, and runtime
recovery are durable. Agents can swarm the same codebase without everything
turning into terminal archaeology.

---

## Why The Rust Rewrite Matters

The migration baseline in
`.local/rust-mvp-rehearsals/20260612T-full-after-938/baseline/` measured:

| Metric | Previous Python service | Rust service | Rounded improvement |
| --- | ---: | ---: | ---: |
| RSS | 154.7 MiB | 19.8 MiB | about 87% lower |
| Physical footprint | 66.4 MiB | 6.7 MiB | about 90% lower |
| `/health` median | 4.17 ms | 0.28 ms | about 15x faster |
| `/client/bootstrap` median | 6.62 ms | 0.30 ms | about 20x faster |
| `/sessions` median | 25.75 ms | 7.97 ms | about 3x faster |
| `/client/sessions` median | 58.49 ms | 7.95 ms | about 7x faster |

Current live Rust server RSS is around 24 MiB on the maintainer machine. Exact
numbers depend on host load and retained state size, but the direction is not
subtle: the daemon is smaller, faster, and easier to reason about under load.

---

## Quick Start

Build the Rust service and CLI:

```bash
git clone https://github.com/rajeshgoli/session-manager
cd session-manager
cargo build -p sm-server --release
```

The server, CLI, and `sm watch` terminal dashboard are native Rust. No Python
environment is needed. Refresh the installed CLI with `./scripts/install-sm-cli.sh`.

In `sm watch`, jobs appear under their requesting agent with their friendly label,
state, and elapsed time. Pending rows also show their scheduler hold reason
(performance cooldown, test jobs, or an available slot); an unreported reason is
marked explicitly. Running jobs are green. Idle agents waiting for a queue
job or review result appear in cyan as **waiting**, with a clock beside their name.
A single visible job supplies its own detail row; multiple obligations get a compact
summary such as “Waiting for 2 jobs and 1 review”. Expand the agent for details of
reviews or delegated jobs; a single result without a visible job row keeps its wait age.
Expanded agents also show per-PR review counts tracked by sm (not all GitHub reviews).

Select a job with `j/k`. The first `Tab` opens a compact inline card with metadata
and six live output lines; a second `Tab` opens the full-screen live log. The log
fills the available terminal height and follows new output automatically, including
after a resize. `PgUp/PgDn` scrolls the fetched history; `End` resumes following.
The buffer holds at least 200 lines and grows with the terminal. `q` or `Esc`
returns to the dashboard, and closes an inline card before quitting.

`Tab` on an agent expands its activity and output; `J` opens its job browser.
The browser supports the same two-step job expansion, `t`/`Enter` for live output,
and `g` to switch between the agent's jobs and all jobs. Network reads run in the
background; unavailable data is marked explicitly. Finished jobs remain visible
while their browser or inline log is open.

`sm queue status`, `sm queue log`, and `sm queue cancel` accept a durable ID or an
exact, unique friendly label, for example `sm queue status 1374-demo-api-8974`.
Duplicate labels return an ambiguity error listing IDs; exact IDs take precedence.
Completion messages lead with the label and retain the ID for diagnostics.
New jobs have readable `label--job_id.log` hard-link aliases sharing the canonical
log's contents. Existing ID-based log paths continue to work.

Pending jobs show "Waiting to start — no log yet"; the tail starts automatically
when the job runs.

The dashboard retains session trees, filtering, send/rename/create/attach,
double-press retirement, reparent decisions/repair, and `--restore` browsing.
Use `PgUp/PgDn` to scroll expanded details and `?` for the keyboard reference.

Create local config from the example and adjust host/auth/state paths:

```bash
cp config.yaml.example config.yaml
vim config.yaml
```

Run the server directly:

```bash
target/release/sm-server --host 127.0.0.1 --port 8420 --config config.yaml
```

Or install/start the launchd-managed Rust service:

```bash
./scripts/restart-rust-server.sh
./scripts/rust-service-cutover.sh status
```

`restart-rust-server.sh` builds, signs, installs, restarts, and verifies in the
one safe order, and registers launchd against an installed copy at
`.local/bin/sm-server` rather than `target/release/sm-server`. Do not call
`rust-service-cutover.sh start-rust` directly to bring the service up: its
default binary is cargo's output, so registering that way lets any later
`cargo build` replace the executable launchd is running, which is how the service
was taken down twice on 2026-07-27. See `specs/1134_rust_restart_procedure.md`.
The durable non-secret default lives in `config/rust-server-signing.env` and
must be the exact persistent certificate fingerprint from `security
find-identity -v -p codesigning`; the script refuses ad-hoc signing or an
implicit keychain default. During an intentional certificate rotation, supply a
validated replacement with both `SM_SIGN_IDENTITY=<40-hex-fingerprint>` and
the exact `SM_SIGN_DESIGNATED_REQUIREMENT` captured from disposable signing
proof, then update that tracked config before treating it as the new default.

If a deployment is already registered against `target/release/sm-server`, migrate
it once with:

```bash
./scripts/restart-rust-server.sh --adopt --allow-plist-change
```

Install the Rust CLI:

```bash
./scripts/install-sm-cli.sh
```

This installs `sm` to `.local/bin/sm`, beside the installed `sm-server`, and
`restart-rust-server.sh` refreshes it on every restart. Install it rather than
running cargo's output directly: `cargo clean` deletes the whole target
directory, so a `sm` that only exists at `target/release/sm` disappears with
it - along with every spawned session's ability to run `sm` at all. Nothing in
a cargo invocation writes `.local/bin`, so the installed copy survives a clean.

Put `.local/bin` on your `PATH` so `sm` resolves to the installed Rust CLI:

```bash
export PATH="/path/to/session-manager/.local/bin:$PATH"

sm status
sm spawn claude "say hello and exit" --name hello-agent
sm spawn codex --model gpt-5.6-terra --effort high "review this change"
sm spawn codex --prompt-file specs/1264_implementation_brief.md --name implementer
generate-brief | sm spawn claude --prompt-stdin --name researcher
sm all
```

---

## Core CLI

| Command | Purpose |
| --- | --- |
| `sm status` | Show your status and active sessions |
| `sm me` | Show the current session identity |
| `sm all` | List active sessions |
| `sm spawn <provider> <prompt> \| --prompt-file <path> \| --prompt-stdin [--model <model>] [--effort <level>]` | Start a managed agent. File/stdin briefs are accepted atomically, copied into private durable state, then delivered to the runtime as the verified accepted bytes. |
| `sm send <id> "<text>"` | Send input to an agent |
| `sm what <id> [prompt]` / `sm btw ...` | Ask an agent for a provider-native context summary |
| `sm wait <id> <seconds>` | Wait for a session state transition |
| `sm attach <id>` | Attach to the live tmux session |
| `sm tail <id>` | Show recent output/tool activity |
| `sm output <id>` | Print recent terminal output |
| `sm clear <id>` | Clear a session for a new task |
| `sm retire <id>` | Stop and retire a session |
| `sm restore <id>` | Restore a stopped/restorable session |
| `sm children` | List child agents |
| `sm task-complete` | Mark task completion and wake parent/maintainer |
| `sm turn-complete` | Mark a turn boundary |
| `sm queue list/status/run/cancel` | Manage retained queue jobs |
| `sm watch` | Agent dashboard with queue ages, hold reasons, and live job logs |
| `sm review` | Run local synchronous PR review flows |
| `sm request-codex-review` | Request async Codex review tracking |
| `sm enroll-device` | Enroll an Android app device certificate |
| `sm list-devices` | List enrolled mobile devices |
| `sm remove-device <id>` | Revoke an enrolled mobile device |

Message delivery modes:

```bash
sm send agent "message"              # Sequential: wait for idle
sm send agent "message" --important  # Queue behind current work
sm send agent "message" --urgent     # Interrupt immediately
```

For a multiline or large spawn brief, use `--prompt-file` or `--prompt-stdin`
instead of a temporary stand-by prompt followed by `sm send`. The three prompt
forms are mutually exclusive. The file/stdin content must be nonempty UTF-8;
Session Manager records its SHA-256 and preserves the accepted bytes in its
state directory before creating the child.

The old transcript summarizer and its `--lines`/`--deep` options remain retired.
`sm what` now delegates to the target provider's native `/btw` command. Use
`sm what <agent>` to summarize the agent's main conversation thread; an optional
prompt replaces that default. Use
`sm retire`, not legacy kill aliases.

---

## Android App

The Android app is the supported remote operator surface. It can:

- list sessions by repo and activity state;
- show health, analytics, and app update status;
- request session status updates;
- attach to mobile terminal sessions when configured;
- enroll and store a Cloudflare Access client certificate;
- authenticate with Google at the origin after client-certificate proof.

Publish a debug APK to the local artifact server:

```bash
cd android-app
SM_VERSION_CODE=1072 SM_VERSION_NAME=0.1.2 ./gradlew assembleDebug
cd ..
VERSION_CODE=1072 VERSION_NAME=0.1.2 ./scripts/deploy_android_app.sh
```

The app checks:

- `/apps/session-manager-android/meta.json`
- `/apps/session-manager-android/latest.apk`
- immutable `/apps/session-manager-android/{hash}.apk`

Device enrollment flow:

```bash
sm enroll-device
```

Scan the generated QR with the phone camera. The deep link opens the app, the
app submits a CSR, Session Manager issues a device certificate, and the device
stores it internally. The certificate is not displayed in the app UI.

---

## Public Access Model

The hardened public path is layered:

1. **Cloudflare Access mTLS** gates the app hostname before origin traffic is
   allowed.
2. **Origin auth** verifies Google/device identity for the Session Manager user.
3. **Route-local proofs** gate sensitive shell attach flows.
4. **Device revocation** removes a device from both Session Manager state and
   the Cloudflare Access certificate policy.

The browser/operator hostname and app hostname should be isolated in Cloudflare
Access policy. The app hostname should not expose unauthenticated origin
responses to the public internet.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    SESSION MANAGER                          │
│                                                             │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │ Rust HTTP   │  │ SQLite      │  │ tmux Runtime        │  │
│  │ API/CLI     │  │ state/queue │  │ Claude/Codex panes  │  │
│  └──────┬──────┘  └──────┬──────┘  └──────────┬──────────┘  │
│         │                │                    │             │
│         └────────────────┼────────────────────┘             │
│                          │                                  │
│         ┌────────────────┼────────────────┐                 │
│         ▼                ▼                ▼                 │
│  ┌────────────┐   ┌────────────┐   ┌────────────┐          │
│  │ Android    │   │ Email/     │   │ Cutover    │          │
│  │ app API    │   │ human send │   │ gates      │          │
│  └────────────┘   └────────────┘   └────────────┘          │
└─────────────────────────────────────────────────────────────┘
```

Key stores and surfaces:

- session state JSON;
- SQLite message queue;
- queue runner state;
- tool/audit log DB;
- Codex events, requests, and observability DBs;
- app artifact store;
- bug report store;
- Cloudflare/mobile device enrollment DB.

Cutover tooling lives under `scripts/rust_migration/` and records preflight,
backup, restore, freeze/drain, fixture, shadow, and canary evidence.

---

## API Reference

| Endpoint | Method | Purpose |
| --- | --- | --- |
| `/health` | GET | Health check |
| `/health/detailed` | GET | Detailed service state |
| `/sessions` | GET/POST | List/create sessions |
| `/sessions/{id}` | GET/PATCH | Read/update session metadata |
| `/sessions/{id}/input` | POST | Send input |
| `/sessions/{id}/output` | GET | Tail terminal output |
| `/sessions/{id}/attach-descriptor` | GET | Describe attach support |
| `/sessions/{id}/codex-events` | GET | Durable Codex lifecycle events |
| `/sessions/{id}/codex-pending-requests` | GET | Structured request state |
| `/sessions/{id}/activity-actions` | GET | Provider-neutral activity projection |
| `/client/bootstrap` | GET | Native app bootstrap |
| `/client/sessions` | GET | Native app session list |
| `/client/request-status` | POST | Ask live agents for status |
| `/auth/session` | GET | Auth/session status |
| `/auth/device/google` | POST | Native Google ID-token exchange |
| `/apps/{name}/meta.json` | GET | App artifact metadata |
| `/apps/{name}/latest.apk` | GET | Latest APK redirect/download |
| `/deploy/{name}` | POST | Local/authenticated app artifact upload |
| `/queue-jobs` | GET/POST | Queue job list/create |
| `/queue-jobs/{id-or-label}` | GET/DELETE | Queue job detail/cancel (unique exact label or ID) |
| `/session-obligations` | GET | Pending job/review results and sm-tracked PR history, keyed by session ID |
| `/codex-review-requests` | GET/POST | Codex review watch list/create |
| `/nodes` | GET | Node registry projection |

Full docs are available from the running service at `http://127.0.0.1:8420/docs`
when API docs are enabled.

---

## Configuration

Start with `config.yaml.example`. Common sections:

```yaml
server:
  host: "127.0.0.1"
  port: 8420

paths:
  state_file: "~/.claude-sessions/state.json"
  app_artifacts_dir: "~/.local/share/claude-sessions/apps"

rust_core:
  runtime_enabled: true

auth:
  google:
    enabled: true
    allowlist_emails:
      - "you@example.com"

cloudflare_access:
  mobile_app:
    enabled: true
    hostname: "sm-app.example.com"

mobile_terminal:
  enabled: true
```

Local/private config stays in `config.yaml` and optional local env overlays; do
not commit secrets, Cloudflare tokens, Google client secrets, or device CA keys.

---

## Testing

Rust server and CLI:

```bash
cargo fmt --check
cargo test -p sm-server
```

Migration contract harness:

```bash
./venv/bin/python -m pytest tests/unit/test_rust_migration_contracts.py
./venv/bin/python -m scripts.rust_migration.contracts --target rust --base-url http://127.0.0.1:8420 --json
```

MVP rehearsal/cutover evidence:

```bash
./venv/bin/python -m scripts.rust_migration.mvp_rehearsal --output-dir .local/rust-mvp-rehearsals/$(date -u +%Y%m%dT%H%M%SZ)
./venv/bin/python -m scripts.rust_migration.live_canary_report --fail-on-blockers --json
```

Android app:

```bash
cd android-app
./gradlew testDebugUnitTest assembleDebug
```

---

## Operator Notes

- Prefer `sm status`, `sm all`, `sm tail`, and the Android app for live state.
- Use `sm what <agent>` for a bounded context summary without disturbing the
  target's main conversation.
- Use `sm retire` for lifecycle stop; avoid legacy kill terminology.
- Keep app updates and Cloudflare mobile device policy changes auditable through
  Session Manager commands.
- If the public app path fails, check Cloudflare Access mTLS first, then origin
  auth, then route-local attach proof.

### Usage ledger

`usage.db` stores minute-level `seat_tokens` facts. Do not sum rows across
`window_kind`: a token can belong to more than one quota window. For correct
current unscoped per-seat totals, use the `current_seat_token_totals` view,
which selects the latest observed unscoped window and filters facts by that
window's timestamp range. Scoped model pools remain available through `sm
usage`'s model-aware report:

```bash
sqlite3 ~/.local/share/claude-sessions/usage.db \
  'SELECT seat_id, window_kind, SUM(input_tokens + output_tokens + reasoning_tokens + cache_write_5m + cache_write_1h + cache_read_tokens) AS tokens FROM current_seat_token_totals GROUP BY seat_id, window_kind;'
```

---

## Requirements

- macOS with tmux
- Rust toolchain
- Claude Code and/or Codex CLI
- Android app optional but recommended for mobile operation
- Cloudflare Access optional for public mobile access, strongly recommended for
  exposed app/browser hostnames

---

## License

MIT

---

**Built for the age of AI agents.** When one agent is not enough, let the swarm
work while you stay in control.

### Waiting-state API for desktop clients

`GET /session-obligations` is a protected, read-only snapshot with `schema_version: 1`
and a `sessions` array. Join each entry's `session_id` to `/sessions`. Each contains:

- `waiting_on`: pending/running queue jobs and active review watches, with `kind`
  (`queue_job` or `review`), `id`, friendly `label`, `state`, `since`, and
  `requester_session_id`. Reviews also include `repo`, `pr_number`, `last_polled_at`,
  and `last_error`.
- `waiting_since`: oldest outstanding request timestamp, or null.
- `review_history`: per-PR `repo`, `pr_number`, `scope: "sm_tracked"`,
  `request_count`, `requested_by_agent`, `landed_count`, and
  `landed_requested_by_agent`. Landed reviews deduplicate by review URL.

Decorate only an **idle** agent with nonempty `waiting_on` as waiting. Keep the
underlying `activity_state` unchanged. Obligations belong to the notification
recipient; history attributes requests to the requesting agent. A successful
snapshot omitting an agent means no tracked obligations or history. Preserve a
stale/unknown indication on request failure rather than treating failure as empty.
An obligation ends when the job finishes or the review watch becomes inactive;
this API does not represent unread completion messages. It makes no GitHub calls.
For individual review records and timestamps, use
`GET /codex-review-requests?include_inactive=true&repo=OWNER/REPO&pr_number=N`.
