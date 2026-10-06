# Local-agent HTTPS proxy

The **egress proxy** is the host process through which a local agent reaches
public HTTPS servers. It forwards encrypted bytes; it never decrypts TLS or
adds credentials. Each agent has a dedicated loopback listener in 18700–18799.
Only the listener's saved registration determines the log's agent identity.

The host runtime calls `sm_server::local_egress::ServiceClient` before launching
an agent. No opencode provider or queue implementation is required:

```rust,ignore
let client = ServiceClient::production(installed_sm_server_path);
let registration = client.register_agent(session_id)?;
let proxy_port = registration.port;
let launch_environment = registration.environment();
```

Pass the installed `sm-server` executable, never a `target/` binary. Registration
starts the separate `com.rajeshgoli.sm-local-egress` launchd job if necessary.
It runs `sm-server --local-egress-service <directory>` independently of sm's
blue/green server processes. Restarts of sm leave existing proxy connections
running. A proxy crash restarts the service and binds every saved active port;
clients reconnect. The proxy exits successfully after 60 seconds without a
control request when no active registration remains. launchd restarts abnormal
exits, not that idle exit. Registration starts it again when needed.

`ServiceClient::new(directory, executable)` accepts an absolute host-owned,
mode-0700 directory. The production directory is
`~/.local/share/claude-sessions/local-egress/`. The host must exclude this entire
directory and its Unix control socket from every agent's wall. Registration
writes use atomic replacement and synchronize to disk. A file lock excludes
competing service instances. Agent IDs contain only letters, digits, `_` and
`-`, and are 1–128 bytes long.

| Host operation | Result |
| --- | --- |
| `register_agent(id)` | Idempotent active registration; smallest never-reserved available port for a new identity; same saved port for a suspended identity |
| `registration(id)` | Saved registration, including `active`; absent IDs return `None` |
| `unregister_agent(id)` | Stop its listener and connections; retain its port reservation for suspension/restore |
| `release_agent(id)` | Release an inactive reservation **only after every process using its wall exits**; reject an active registration |

The host must unregister before release. Keeping a reservation while a wall
exists prevents an old agent from reaching another identity on a recycled port.
If all 100 ports are reserved, registration fails; it never takes another
identity's reservation or silently changes the admitted port. After releasing
an exited wall, its former identity is a new registration on a later launch.
The launch caller owns process-exit verification and calls release only then.

Apply `Registration::environment()` to a cleared environment. It sets uppercase
and lowercase HTTP/HTTPS proxy variables to `http://127.0.0.1:<port>`, loopback
`NO_PROXY`, disables Cargo offline mode, disables host Git configuration, resets
Git's credential helpers and supplies only `!gh auth git-credential`. The `!`
means Git invokes a shell command. No token appears in these values. The launch
composition in #1974 supplies readable `~/.config/gh/hosts.yml`, private Cargo
and agent directories, judge rules and the wall's admitted proxy port. #1979
uses the same registration/environment for submitted queue jobs. Environment
composition must preserve the empty first credential helper and the gh helper
when adding any other Git configuration entries. Do not pass `--offline` to
Cargo. Clearing `GH_TOKEN`/`GITHUB_TOKEN` and other inherited credentials remains
the launch caller's responsibility.

The proxy accepts only bounded, well-formed HTTP/1.0 or HTTP/1.1 CONNECT headers
with an authority on port 443. It resolves outside the wall, rejects the entire
answer set if any address is non-public, then connects directly to a checked IP.
It blocks IPv4 private/loopback/link-local/unspecified, shared address space,
multicast/reserved/documentation addresses and IPv6 non-global, mapped private,
documentation and transition addresses. DNS failure, empty answers, request
errors and prohibited destinations return 403. Header and DNS/connect operations
have 15-second deadlines; tunnel reads/writes have five-minute idle deadlines.
A shared limit admits at most 256 simultaneous proxy workers.

`connections.jsonl` contains one JSON object per completed connection: `time`
(UTC RFC3339), `agent_id`, `host`, `resolved_address`, `port`, `bytes_to_host`,
`bytes_to_agent`, `duration_ms`, and `outcome`. Refused requests have null fields
when no valid authority/address is available. Outcomes are `allowed` or a
specific reason such as `non_public_address`, `port_not_443`, `connect_only`,
`malformed_request`, `dns_failed`, `connect_failed`, `registration_revoked` or
`tunnel_io_error`. Counts exclude proxy headers and include partial transfers.
Unregister drains workers and logs their revocation. A process crash can lose
completion records for connections still in flight; existing JSON lines and
registrations remain intact. No HTTP headers, request paths, payloads, tokens
or proxy credentials are logged.

## Verification

Run Rust tests through `scripts/test-rust-isolated.sh local_egress`. They cover
request validation, public-address classification, a fixture resolver returning
mixed public/private answers, refusal logging without request contents, byte
counts and half-close, registration restore, suspension, safe release and
stable environment values.

On macOS, explicitly run the live acceptance script after building `sm-server`:

```sh
python3 scripts/local-egress/test_proxy.py --binary target/debug/sm-server
```

Run builds/tests through `sm queue run`. The script uses a temporary service
process/directory, real gh authentication from its host-readable file, a fresh
Cargo cache, and an independently launched network-only sandbox. It creates
and deletes its own remote Git branch. It verifies `curl https://docs.rs`,
`gh pr view`, Git fetch/push, uncached Cargo fetch, agent A's inability to reach
B's proxy, blocked direct public TCP/DNS, 403 refusals, saved ports/log append
after a service crash, and attribution to both agents. It does not install a
launchd job or alter the live sm server. Full filesystem/judge/provider launch
composition is #1974's acceptance surface; this script deliberately exercises
the proxy and the network admission boundary independently of it.
