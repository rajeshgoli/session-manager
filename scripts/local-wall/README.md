# Production local-agent sandbox

`wall_profile.sh` generates the macOS `sandbox-exec` profile used by a local
agent. It contains no provider-specific code. The launch/restore caller creates
the state directories outside the sandbox, generates the profile before launch,
and never grants the agent write access to the profile or launch configuration.
This is the profile primitive for #1974, implemented in #1976; it does not launch
agents or services. #1974 supplies runtime registration and #1956 supplies the
opencode provider.

`local_wall::LocalWallRuntime` composes the host preparation steps. Its Rust
configuration and agent registration come only from the host; they are not
deserializable tool or queue requests. `prepare` registers the agent with the
judge, the logged network proxy and the socket broker, then stages independent
ad-hoc signed tool copies, the adapter, the supervisor and the immutable profile
under its private configuration directory. Sources are embedded in the server
binary, so preparation does not depend on a developer checkout. The checkout
must have independent Git metadata; linked worktrees and shared object stores
are refused. Git identity is supplied through the cleared, host-controlled
environment alongside the private GitHub and Cargo configuration.

The returned `PreparedWall` exposes `spawn_provider` and `spawn_queue`. Both
accept only a registered tool name and command arguments. They use the prepared
environment and profile. Only the provider receives descriptor 198, the host's
already listening control socket. Providers use ordinary `socket`, `bind` and
`listen` calls on their assigned control port; the adapter duplicates that
socket. Queue children receive no provider descriptor. Drain child stdout and
stderr while waiting for commands with substantial output. Waiting or dropping
`WallChild` kills remaining children before releasing its launch authority.

Call `suspend` after all launched children have finished, cancel pending queue
work, and only then release durable network port registrations. Suspension
disables the judge and network registrations but preserves their assigned ports
for restoration. Before preparing a restored agent, finish every previous
launch and drop its old `PreparedWall`; a held host lock or live broker socket
causes refusal. A new preparation regenerates artifacts against the current
broker identity. A short private directory below `/private/tmp` holds only
host-owned broker aliases, allowing long production state paths without granting
agents general temporary-directory access. Runtime preparation is a library
primitive; provider activation and durable queue admission remain caller work.
Preparation rebuilds the staged executable directory from the current tool
registration, removing withdrawn tools and interrupted copies before launch.

Use `prepare_for_queue(agent, queue_state)` when the agent may submit queue work.
It stages and signs the host's zsh, excludes the queue service directory from the
wall, saves the host registration, and attaches a queue-only launcher sharing
the provider's socket service. Pause queue admission before preparing, detaching
or restoring these bindings. Preparation and `detach_queue` refuse running queue
rows; callers must also finish provider and direct launches before replacing
artifacts. `detach_queue` leaves pending commands and registrations intact.

On restore, the host validates the saved profile and shell hashes and compares
the current agent metadata, service ports, paths, tool source hashes and profile
generator against its saved preparation authority. A mismatch fails launch;
configuration changes require finishing/cancelling old work, suspending the wall
and calling `retire_queue` rather than rewriting pending commands. Retirement
refuses pending or running work and removes the saved registration; the host
removes immutable command inputs when retiring the agent. A matching restore retains
the original profile, including its launch-time listening-port deny list, and
regenerates the adapter for the live broker. The judge restores the credential
from the validated host registration through its private control socket, so
suspension or failed preparation does not invalidate the saved queue environment.
Missing or replaced artifacts fail before launch, with no host execution fallback.
An uncertain egress registration result triggers deactivation while preserving
its durable port assignments.

The live server owns `GenerationWalls`, the retained walls for one serving
process. It restores saved production registrations before queue recovery,
retries unavailable predecessor locks/services every five seconds, and makes the
retained walls available through `AppState::local_walls` for the future provider.
Shutdown stops preparation and launch admission and detaches queue launchers;
running children keep their service references until supervisor cleanup. A valid
pending command without a live launcher waits with `holding_reason=local_wall`
and leaves unrelated hosted jobs eligible. Invalid artifact or authority hashes
still cause failed starts. Recovery requires the host-authority hash pinned in
the durable manifest; legacy manually attached queue primitives do not become
production wall registrations automatically. Host authority is persisted before
the first queue manifest is published, so an interrupted first preparation can
retry without manual deletion.

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

File contents and directory listings are denied by default. The profile admits
the checkout, own state and system runtime/tool directories; it does not admit
the user's home, browser profiles, package-manager configuration or arbitrary
host data. Metadata remains available for path resolution; the filesystem root
directory itself is readable because the macOS program loader opens it before
loading the system library cache. This grants no access to its descendants. Repeat
`--read-only-dir` for each additional host-approved, credential-free toolchain
or dedicated log directory. Supply physical narrow directories, never a home,
state root, service-secret tree or their ancestors. The host must verify these
directories contain only immutable trusted tools or sanitized logs; do not
admit mutable shared dependency caches or directories containing credentials.
Missing tools must fail the launch or be staged into immutable own state;
never respond by admitting a broad host tree. The old credential-path denials
remain as extra protection, not an exhaustive credential inventory.

The owner policy in #1978 admits exactly `~/.config/gh/hosts.yml` for reads.
Other files and directory listings under `~/.config/gh` remain denied; the token
file remains unwritable. Profile generation rejects symlink components,
hard-linked credentials and overlaps with mutable checkout/state trees. The
host runs `scripts/local-wall/wall_profile.sh --prepare-gh "$home_dir" "$state_dir"`
before launch and sets `GH_CONFIG_DIR` to the returned directory. This prepares
a private `config.yml` containing only `version: 1` and a symlink to the approved host `hosts.yml`
under immutable `xdg/config/gh`. No host settings are copied. Do not export a
token. The judge still checks every gh command and git push, and credential
reads remain subject to its rules. Network requests use the assigned logged
proxy; this file grant does not permit direct internet access.

Set `CARGO_HOME` to `<state_dir>/xdg/cache/cargo`, a private mutable directory.
Prepare any required dependency caches there outside the wall before launch;
copy cache data without host configuration or credentials. Host Cargo binaries
are readable only when their physical tool directories are explicitly admitted
(typically the selected directory under `~/.rustup/toolchains`); the entire host Cargo home
stays unwritable. Its config
and credential files are unreadable. Do not allow writes to shared caches:
poisoned dependency source could execute during a later host build. This replaces
the prototype's writable `~/.cargo` grant to preserve the process boundary.
Put the selected toolchain's `bin` on `PATH` and invoke its Cargo/rustc directly,
so rustup does not need to read the host's settings. If the runtime uses rustup,
it must prepare a private `RUSTUP_HOME` with credential-free settings/toolchains.

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
  --broker-dir "$state_dir/tmp/broker" --contained-processes \
  --immutable-exec-dir "$host_staged_executables" \
  > "$state_dir/wall.sb.new"
```

Publish `wall.sb.new` as `wall.sb` only after exit status 0. Generation validates
all inputs before writing profile bytes; nevertheless the caller must check the
exit status. `--home` defaults to the current user's home; tests override it with
a fixture. Secret directories must exist before generation. Include every
directory containing judge registrations, allow records, gateway secrets,
proxy private keys and other host credentials.
The host must prepare the checkout/state without hard links to protected host
files or immutable state. File aliases already created outside the wall retain
their inode identity; the launch check must reject such shared inodes. Inside
the wall, hard links are allowed only from mutable files into writable paths.
Signals reach only processes inheriting that same sandbox, so commands can
manage their children without stopping the host services or other agents.

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
its `localhost` filter also accepts wildcard binds. The trusted host creates
and binds each socket to exactly `127.0.0.1` or `::1`. The socket service activates
test listening outside the wall when the adapter requests it; provider control
listeners are already listening before launch. The agent may accept and reply
on those prebound descriptors.
Ordinary bind/listen APIs need an immutable host-supplied adapter backed by the
trusted socket service in `sm_server::local_sockets`; launch composition remains
a separate #1974 prerequisite.
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
| Host Cargo home | No write; read only explicitly admitted tool directories |
| Other agents' state, including future siblings | No read or write |
| Own config, judge plugin, profile, launch files, server secret | Read; no write |
| GitHub `hosts.yml` | Read only, under owner policy #1978 |
| Other GitHub config, both standard Git credential stores, host global Git config, SSH/Claude/Codex/AWS credentials, keychains, sm config | No read |
| Judge/proxy service-secret directories | No read or write |
| Dedicated credential-free sm log directories | Read only when explicitly admitted |
| All other host file contents, including npm tokens and browser cookies | No read |
| Direct internet and DNS | No outbound connection |
| New TCP/UDP listeners on loopback, wildcard or LAN addresses | No bind/listen |
| Trusted prebound IPv4/IPv6 loopback listener descriptors | Accept and reply |
| Own sm gateway, egress proxy, model and judge | Loopback connection |
| Private Unix sockets in own `tmp/` | Connection |
| Every Unix socket outside own `tmp/`, including Docker, default tmux and sm-state sockets | No connection |
| Host Mach services, including keychain and application-launch brokers | No lookup or registration |
| Apple-event delegation | No send |
| Hard links from immutable state or host files into mutable paths | No link |
| Signals to host services or other sandboxes | Denied; own children permitted |
| Cross-sandbox process argument/environment queries | Denied |
| System-control mutations | Denied |
| Process information for host services or other sandboxes | Denied; own processes permitted |
| Listed CPU, memory, OS-version and runtime-limit kernel queries | Read |

The profile is a process-wide operating-system restriction. It does not decide
which GitHub action or sm route is permitted. The judge and gateway implement
those policies. Launch with a cleared environment: tokens or inherited open
connections cannot be removed by denying reads of their source files.

The native adapter is a shared library that replaces selected socket calls.
`build_adapter.py` compiles one signed library with the host's immutable Unix
endpoint, full kernel peer token, four admitted direct service ports, and optional
provider-control descriptor/port. It reads no environment value to choose an
endpoint, agent identity or control descriptor. Use `AgentService::peer_token()`
and `AgentService::endpoint()` from the trusted service when building it. Set
`peer0` through `peer7` to the eight unsigned words returned by `peer_token()`:

```sh
python3 scripts/local-wall/build_adapter.py \
  --endpoint "$broker_endpoint" \
  --peer-token "$peer0" "$peer1" "$peer2" "$peer3" "$peer4" "$peer5" "$peer6" "$peer7" \
  --direct-ports "$gateway_port" "$egress_port" "$model_port" "$judge_port" \
  --control-port "$agent_port" --control-fd "$inherited_control_fd" \
  --contained-spawns \
  --immutable-exec-dir "$host_staged_executables" \
  --immutable-exec-dir "$approved_python_installation" \
  --output "$state_dir/xdg/config/socket-adapter.dylib"
```

Omit both control arguments for an adapter without a provider control capability.
The host stages this library in agent-readable, agent-write-denied state and
loads it with `DYLD_INSERT_LIBRARIES` in a cleared launch environment. The host
must also make the broker endpoint directory and its short alias agent-write-denied
while keeping the endpoint inside the permitted private Unix socket subtree.
Pass `--broker-dir` to exclude the endpoint directory from tmp writes, and repeat
`--immutable-exec-dir` for every root compiled into the adapter. The profile
protects their contents and containing directories against replacement.
Application TMPDIR must be outside the broker directory. Mode 0500 on the library and
0700 on the endpoint directory do not replace sandbox write denials, because
the host and agent use the same operating-system user.

The adapter validates received descriptor type and exact address before replacing
the application's descriptor. Bind and listen remain separate operations. Dup,
dup2 and fcntl duplication retain the lease; close releases that descriptor's
copy. Forked processes obtain independent authenticated control connections via
the service's retain operation, which only references an existing same-agent
listener and never allocates another port. Nonblocking and close-on-exec flags
survive replacement. TCP service connections go directly only to the four
compiled service ports; own test connections use the trusted service. Unix
socket calls retain their ordinary behavior under the wall's own restrictions.
The service limits each agent to 16 live test listeners and 32 control connections;
the adapter tracks up to 1024 application descriptor copies.

Exec and posix_spawn recover inherited listeners and bound sockets from kernel
socket state. The adapter restores its compiled absolute loader path even with
a cleared or forged environment. Recovery opens a fresh authenticated retain
connection per listener before closing inherited control streams; duplicated
application descriptors share the recovered lease. Connected sockets retain
their ordinary lifetime and are not mistaken for listeners. Final listener
close waits for the host to drop that process's lease before acknowledging it.
Fork and ordinary spawn return only after the child has recovered, so a parent
may immediately close its copy. Spawn forwards ordered close, dup2, open,
inherit and working-directory actions, including close-on-exec defaults.
Python's process-replacement spawn mode follows the exec recovery path.

The adapter supports native-architecture unsigned and ad-hoc signed Mach-O
executables. Inherited adapted sockets require a canonical executable path below
a compiled `--immutable-exec-dir`. The host must stage these directories before
launch and deny agent writes to their files, entries and ancestors; no writable
ancestor may move or replace a trusted root. Reject hard links to mutable files.
These checks apply to same-user writable tool installations as well as own
state. Canonical execution prevents a mutable symlink from substituting another
image between validation and execution. With no roots configured, inheritance
is refused. Agents can execute mutable programs when no adapted socket survives;
the host must stage a program in immutable state before it inherits a listener.
It refuses surviving adapted descriptors for platform, restricted,
hardened, library-validated or unsupported images and suspended spawn. Such
executables may run when no adapted descriptors survive. Relative spawn paths
resolve after ordered directory actions. The adapter mirrors up to 64 live file-action objects and
256 actions per object; it does not inspect libc's private representation.
An internal `SM_WALL_RECOVERY_FD` value identifies only a parent completion
socket, verified through its kernel parent identity and an expected marker.
It never chooses broker identity, endpoint, agent attribution or socket rights.
Caller loader and completion overrides are replaced; other environment values
are forwarded. Two-second recovery waits fail closed if the child does not
confirm initialization. `local_sockets::launch::LaunchBinding` provides host
launch composition, proved with native fixtures, Rust/Python inheritance,
pinned opencode's Bun HTTP server, and sm's HTTP/queue socket fixtures under the
production profile. Direct kernel TCP calls stay constrained by the wall;
the caller must never widen it to restore adaptation.

The macOS Rust tests under `local_sockets` compile native fixtures with warnings
as errors and exercise the real service, forged peer identity, malformed replies
and descriptor cleanup, provider-control refusal, ordinary IPv4/IPv6 socket calls,
duplication, fork, concurrency, flags, all exec entry points, spawn file actions,
cleared/forged environments, unsupported-image refusal, immediate port reuse,
and ordinary Rust/Python programs inheriting listeners.
The socket fixture also proves raw bind succeeds outside its sandbox and fails
inside it. The production launch fixture covers fresh registrations, forged or
absent requester variables, cancellation cleanup and hard-link rejection.

Compile `native/launch_supervisor.c` with clang's `-std=c11 -Wall -Wextra -Werror`
flags into immutable host-owned state. Build the adapter with control descriptor
198 (`launch::CONTROL_FD`). Construct `LaunchBinding` with the shared
`AgentService`, an optional exact-loopback control listener, profile, adapter and
supervisor. The three host files must have no hard-link aliases. The host must
also reject mutable aliases into trusted executable roots before launch.
Pass an explicit host environment allowlist and checkout to `spawn`; drain
its piped stdout/stderr while it runs. Caller loader overrides are replaced.
Unrelated host descriptors and environment are not inherited.

The trusted supervisor waits for host registration before forking the sandboxed
application and remains its ancestry root across exec. A private completion
socket returns the application's exit status. Completion or cancellation kills
the private process group and revokes registration. The supervisor's PID stays
reserved until cleanup. Drop the service at shutdown and create a new service
after host restart. Queue jobs share the agent's binding with fresh process
registrations; requester environment never selects socket authority. Pass `Some`
control listener only for the provider and `None` for queue commands. The latter
receive no provider descriptor, even when they share the provider's adapter.
The application starts with the normal default SIGPIPE disposition.

Production launch requires a profile generated with `--contained-processes`
and an adapter built with `--contained-spawns`. `LaunchBinding` rejects a profile
without the required syscall denial. The kernel denies `setsid`, `setpgid` and
raw `posix_spawn`, including calls that bypass the adapter. The adapter implements
supported `posix_spawn` calls using kernel fork followed by ordered file actions
and exec. It supports signal defaults/masks, reset IDs, close-on-exec defaults
and process replacement. Requests for a new process group or session fail with
EPERM; other unsupported flags fail with ENOTSUP. Tools that bypass the adapter
for spawning also fail closed. The standalone file/network profile without
`--contained-processes` does not promise descendant cleanup and cannot be used
with `LaunchBinding`.
For the exact host-staged `opencode` executable, preparation compiles an
attached-spawn compatibility setting into the immutable adapter. Opencode
1.17.9 requests detached execution for ordinary Git and Bash commands; the
adapter removes those session/group flags and runs the command in the existing
launch group. The adapter maps the provider's SIGTERM/SIGKILL cancellation of
that command to its descendants, binding the command PID to its kernel start
time. It stops each parent before discovering and signalling its children;
both group-first and direct-child cancellation use this path. Unrelated tools
keep running. Traversal errors fail cancellation instead of reporting success
for only the immediate child. This setting is selected by the physical
executable path, never argv, environment or a queue request. Other executables
retain the EPERM refusal. Kernel denials still prevent detachment through raw
syscalls or an adapter bypass, and verified shutdown still kills the entire
original launch group.
Contained launches allow listing integer process IDs for this traversal;
per-process metadata stays restricted to the same sandbox, outside process
environments remain denied, and signals cannot cross the sandbox boundary.
The provider config selects the wall's immutable, ad-hoc signed `queue-zsh`
shell. Platform shells can discard loader settings; the staged shell retains
the adapter across tool execution and into staged programs. The native fixture
checks actual Git/project initialization, conversation creation, shell execution,
group-first and direct-child cancellation of children and grandchildren while
the provider remains alive, and the unchanged refusal in another image.
Actual provider launch and stamped queue dispatch integration remains #1974.

Run fixture validation through the durable queue:

```sh
sm queue run --type tests --label local-wall-fixtures --cwd "$worktree" \
  -- python3 scripts/local-wall/test_wall.py
```

The tests use temporary fake credentials and loopback listeners. On macOS they
invoke the real `sandbox-exec`; other operating systems skip that enforcement
test and can only validate profile construction. The full launched-agent,
authenticated GitHub and queue-job checks belong to #1974 and #1956.
Host provider code may call `PreparedWall::spawn_provider_with_environment` with
additional settings such as its HTTP authentication password and behavior flags.
The API validates environment names/values and reserves existing wall settings
case-insensitively, proxy names, loader settings, Git/GitHub/Cargo configuration,
private state, identity and remote-model credential prefixes. `SM_SESSION_CREDENTIAL`
and `SM_JUDGE_PLUGIN_LOG` are permitted host additions for managed session delivery
and logging. No request handler or durable queue submission accepts these settings;
queue launches continue to use the registered wall environment exclusively.

Preparation writes the trusted agent name and `<id>@local-agent.invalid` into an
independent checkout's local Git configuration without reading global/system
configuration or expanding includes. Existing repository settings are preserved.
Shared or aliased Git metadata/configuration and an existing configuration lock
are refused. Filesystem-only fixtures may omit `.git`; production providers supply
an independent checkout.
