# Production local judge

`LocalJudgeRuntime::from_config(&AppConfig)` is the host interface. The caller
uses `register(session_id, &Registration)` **before launch** and `unregister`
on failed launch or final retirement. A registration contains name, ticket,
title, branch, absolute checkout, absolute temporary folder, parent, and sm URL.
This API works without a provider or an existing sm session record.

Registration returns the `/decide` URL and a private per-agent token. The
provider hook sends `X-Local-Agent: <sm id>` and `X-Local-Judge-Token: <token>`
with the unchanged hook JSON (`tool_name`, `tool_input`, `cwd`, `session_id`).
A token for another agent cannot authorize this agent's actions. The future
provider ticket installs this hook; this ticket changes no launch path.

Only `POST /decide` and `GET /health` exist on loopback TCP. Register,
unregister, registrations, health, and owner `allow` operations exist only on
the private Unix control socket. `allow_once(denial_id)` is an owner-only host
API; an app route added later must authenticate the owner and refuse a local
agent's gateway identity. Do not forward control requests from the sandbox.

The wall/gateway tickets must deny every local agent read/write/socket access
to the entire judge state directory, including copied Python sources, policy,
tokens, and control socket. Unix file permissions protect against other users;
the wall protects against sandboxed agents running as the owner's Unix user.
The service refuses a registration whose writable folders contain its state.

The sm daemon reconciles every five seconds when runtime is enabled. The judge
is detached from sm's process group, survives sm restart, and holds a lifetime
file lock. Concurrent starts share one process. Health carries a generation: a hash of the
embedded service, policy, startup configuration, and model URL. sm reuses a
healthy process only when that generation matches. For a mismatch it closes
admission, drains already admitted decisions, waits for the daemon's lifetime
lock to release, and starts the new generation against the same durable files.
New decisions during replacement fail closed. An unchanged sm restart keeps
the same judge process; a changed policy/configuration takes effect even while
agents remain registered. Registration retries if idle shutdown begins between
readiness and the registration request. If the judge crashes, sm starts
it again from durable registrations; no provider lookup is required. Explicit
unregister removes a registration. The judge exits after 60 seconds without
registrations, allowing a replacement launch to register during that interval.
No process is a queue job and no build output or temporary checkout is needed
by the installed daemon. Python 3 and its standard library are required.

Configuration (defaults shown):

```yaml
local_judge:
  root_dir: ~/.local/share/claude-sessions/local-judge
  port: 8431
  proxy_port: 8432
  timeout_seconds: 30
  python: python3
```

`root_dir` must be absolute after expanding `~/`.

The model endpoint comes from `local_host.base_url`. Policy and request settings
are lifted from the #1954 proof. Decisions fail closed on model unavailability,
a malformed answer, overload, logging failure, or the complete model-call
timeout. SSE (server-sent events, the model's streaming response format) works
with ordinary and chunked HTTP responses, including an answer completed while
the stream remains open. Relative tool paths resolve against the registered
checkout; symlinks resolve before containment checks. Caller `cwd` does not
replace this trusted path base.

Durable files under `root_dir`:

| File | Purpose |
| --- | --- |
| `agents.json` | Authoritative registrations and per-agent tokens; atomic replacement |
| `decisions.jsonl` | Proof decision fields, plus `cwd` and `action_key` |
| `allows.jsonl` | Proof owner-grant and consumption fields, plus `action_key` |
| `denial-ids.jsonl` | Durable reservations preventing concurrent/restart id reuse |
| `service.log` | Service diagnostics |
| `control.sock`, `startup.lock`, `service.lock` | Host controls and process ownership |
| `service-<hash>.py`, `policy-<hash>.md` | Installed sources independent of the checkout |

`action_key` is a SHA-256 hash of the tool name, complete tool input and caller
`cwd`, encoded as sorted compact JSON. A grant still carries the proof's
`session_id`, `tool`, and `command`. All must match. Consumption is appended and
synced under the daemon lock **before** replying allow. A repeated grant request
for the same denial never replenishes it. Changed write content, tool arguments,
agent identity, or `cwd` cannot consume the grant. A crash between consumption
and response conservatively spends the grant. A malformed durable log prevents
authorization rather than guessing at grant state.

Validation runs through `scripts/test-rust-isolated.sh local_judge`. The Python
suite replays the proof's recorded responses: 35 denied actions, 18 allowed
workflow actions, and five allowed look-alikes (words used only as data). It
also checks two registrations, private controls, forged tokens, relative paths,
symlinks, unavailable/malformed/timed-out model replies, chunked streams, twelve
concurrent identical retries with one success, and unused/consumed grants across
process restart. Rust tests check concurrent host startup, sm runtime
reconstruction retaining the same judge PID, daemon crash reconciliation, generation replacement preserving tokens, and an
idle-shutdown response precisely between readiness and registration.
The recorded replay tests service migration, not fresh model judgment accuracy.
An already running real model can be tested with `SM_JUDGE_TEST_MODEL_URL`,
without having the tests load model weights.
