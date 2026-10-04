# Production local-agent sandbox

`wall_profile.sh` generates the macOS `sandbox-exec` profile used by a local
agent. It contains no provider-specific code. The launch/restore caller creates
the state directories outside the sandbox, generates the profile before launch,
and never grants the agent write access to the profile or launch configuration.
This is the profile primitive for #1974, implemented in #1976; it does not launch
agents or services. #1974 supplies runtime registration and #1956 supplies the
opencode provider.

The input state layout is:

```text
<state-root>/<agent-id>/
  xdg/config/                    readable, host-writable only
  xdg/data/ xdg/cache/ xdg/state/ readable and agent-writable
  tmp/                           readable and agent-writable
  wall.sb server.secret ...      readable, host-writable only
```

All paths must be absolute physical directories. Each agent state directory must
be a direct child of the state root. The generator resolves aliases
such as `/var` and `/tmp` to their physical paths. The checkout must be inside
the supplied home directory and must not overlap the agent state root. Mutable
state directories must not be symlinks. A service-secret directory must not
overlap the checkout, cargo directory or agent state. The host controls all
arguments; never forward agent-provided profile paths or port lists.

Create `tmp/` and choose a private temporary path with:

```sh
scripts/local-wall/wall_profile.sh --prepare-tmp "$state_dir"
```

The returned absolute path ends in `/`. Use it for both `TMPDIR` and
`TMUX_TMPDIR`. Its length is at least `getconf DARWIN_USER_TEMP_DIR`'s path length,
so a Unix socket path which fails outside the sandbox also fails inside it. This
does not grant write access to the general macOS temporary directory. Fail the
launch if this command fails.

Set `CARGO_HOME` to `<state_dir>/xdg/cache/cargo`, a private mutable directory.
Prepare any required dependency caches there outside the wall before launch;
copy cache data without host configuration or credentials. Host Cargo binaries
remain readable, but the entire host Cargo home stays unwritable. Its config
and credential files are unreadable. Do not allow writes to shared caches:
poisoned dependency source could execute during a later host build. This replaces
the prototype's writable `~/.cargo` grant to preserve the process boundary.

Set `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1` in the cleared launch
environment. Host `.gitconfig` and `.config/git` are unreadable because they may
hold credentials or authentication settings. Install the agent's Git identity
in its checkout config and pass the proxy's credential helper through the
host-controlled Git environment, rather than relying on host configuration.

Generate the profile with host-owned values, for example:

```sh
scripts/local-wall/wall_profile.sh \
  --checkout "$checkout" \
  --state-root "$state_root" --state-dir "$state_dir" --tmp-dir "$agent_tmp" \
  --agent-port 18500 --agent-port-range 18500-18599 \
  --gateway-port 18600 --gateway-port-range 18600-18699 \
  --egress-port 18700 --egress-port-range 18700-18799 \
  --model-port 8000 --judge-port 8441 \
  --service-state-dir "$judge_state" --service-state-dir "$proxy_state" \
  > "$state_dir/wall.sb.new"
```

Publish `wall.sb.new` as `wall.sb` only after exit status 0. Generation validates
all inputs before writing profile bytes; nevertheless the caller must check the
exit status. `--home` defaults to the current user's home; tests override it with
a fixture. Secret directories must exist before generation. Include every
directory containing judge registrations, allow records, gateway secrets,
proxy private keys and GitHub credentials.

The server, gateway and egress ranges default to the values above and must be
disjoint. The caller reserves these entire ranges for their respective services.
Every server port, including the agent's own server, is blocked outbound. Only
the current agent's gateway and egress ports are reachable in those ranges. The
model/judge ports must be distinct, outside these ranges and outside sm's
8420/8443. Raw TCP connections reach only these four admitted service ports;
other host services remain unreachable even when launched after the profile.
The generator also blocks every other TCP listener observed at launch,
plus sm and LM Studio's fixed ports; admitted services override the fixed
8000/1234–1236 denials. `lsof` errors, diagnostics or malformed socket records
fail the generation. The wall denies every new IP listener, including loopback
and wildcard binds. macOS cannot express an inbound rule restricted to loopback:
its `localhost` filter also accepts wildcard binds. The trusted host must create,
bind to exactly `127.0.0.1` or `::1`, and listen on each socket before passing it
into the wall. The agent may accept and reply on those prebound descriptors.
Ordinary bind/listen APIs need an immutable host-supplied adapter backed by the
trusted listener broker; creating that broker is a separate #1974 prerequisite.
Test-client connections to broker-allocated test listeners likewise use trusted
connected descriptors. The broker may connect only to that agent's registered
test listeners; it must refuse provider control ports, privileged service ports,
other agents' listeners and arbitrary destinations.
Do not relax the profile when it or the adapter is absent. Outbound connections
to admitted services need no inbound-listener grant. Private Unix listeners
remain usable without a TCP broker.

Gateway/egress port reservations protect agents launched later: a wall generated
before a successor launches still cannot reach that successor's agent, gateway
or proxy ports. The runtime must not recycle a reserved gateway/egress endpoint
for another identity while an earlier agent's wall still admits that endpoint.
The runtime must preserve the same physical state-root path and secret-directory
paths for the lifetime of every launched wall, including across sm restarts.

| Resource | Agent access |
| --- | --- |
| Own checkout, private Cargo home, mutable state, `/dev` | Write |
| Host Cargo home | No write; config and credentials unreadable |
| Other agents' state, including future siblings | No read or write |
| Own config, judge plugin, profile, launch files, server secret | Read; no write |
| GitHub config, both standard Git credential stores, host global Git config, SSH/Claude/Codex/AWS credentials, keychains, sm config | No read |
| Judge/proxy service-secret directories | No read or write |
| sm logs outside protected state directories | Read |
| Direct internet and DNS | No outbound connection |
| New TCP/UDP listeners on loopback, wildcard or LAN addresses | No bind/listen |
| Trusted prebound IPv4/IPv6 loopback listener descriptors | Accept and reply |
| Own sm gateway, egress proxy, model and judge | Loopback connection |
| Private Unix sockets in own `tmp/` | Connection |
| Every Unix socket outside own `tmp/`, including Docker, default tmux and sm-state sockets | No connection |
| Host Mach services, including keychain and application-launch brokers | No lookup or registration |
| Apple-event delegation | No send |

The profile is a process-wide operating-system restriction. It does not decide
which GitHub action or sm route is permitted. The judge and gateway implement
those policies. Launch with a cleared environment: tokens or inherited open
connections cannot be removed by denying reads of their source files.

Run fixture validation through the durable queue:

```sh
sm queue run --type tests --label local-wall-fixtures --cwd "$worktree" \
  -- python3 scripts/local-wall/test_wall.py
```

The tests use temporary fake credentials and loopback listeners. On macOS they
invoke the real `sandbox-exec`; other operating systems skip that enforcement
test and can only validate profile construction. The full launched-agent,
authenticated GitHub and queue-job checks belong to #1974 and #1956.
