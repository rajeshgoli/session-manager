# Local-agent queue execution

A verified gateway request captures `VerifiedLocalAgent` before leaving HTTP
request scope. `CreateQueueJob.local_submitter` carries this typed identity; JSON
fields, environment values, requester names, and notification recipients cannot
construct it. SQLite stores `local_agent_id` and `local_binding_json` separately
from the requester and notification recipient. Existing jobs with both columns
NULL retain the hosted execution path. A partially populated binding fails closed.

## Host composition

Before accepting local-agent queue work, the host must:

1. Stage a shell, profile, socket adapter, and native launch supervisor in the
   agent's immutable state. Generate the production profile with descendant
   confinement and the exact assigned gateway, egress, model, judge, and broker
   endpoints. Exclude the entire queue service directory from every agent wall.
2. Copy `/bin/zsh` into an immutable executable root and ad-hoc sign the copy
   (`codesign --force --sign -`). The Apple Silicon adapter builder includes
   arm64 and arm64e code so both ordinary tools and Apple's staged shell load it.
   Include the staged shell's directory in the adapter's immutable executable
   roots and the profile's corresponding executable/read permissions.
3. Call `queue::local_wall::register(state_dir, agent_id, WallSpec)`. Registration
   is host-only and immutable; identical registration is idempotent. The saved
   manifest contains physical checkout and agent-state paths, profile and shell
   hashes, assigned service ports, and the host-approved environment. Identity,
   gateway, and proxy environment values are derived from registration. Job
   environment values are discarded, including loader and requester overrides.
4. Call `queue::local_wall::attach` with a `LaunchBinding` for that exact agent and
   profile, sharing the agent's existing `AgentService`. Supply no provider
   control listener. This preserves the same socket-service authority for queued
   children without inheriting provider or host listeners.

Final runtime composition is owned by #1974. None of these host registration
operations is exposed through HTTP. The host is responsible for generating the
correct profile; registration verifies identity, paths and artifact integrity,
not the semantics of arbitrary sandbox profile text.

`local_wall::LocalWallRuntime::prepare_for_queue` supplies this composition:
it stages the signed shell, generates or validates the profile, registers the
host environment and attaches the shared socket service. Its host caller pauses
admission during preparation and restoration. `PreparedWall::detach_queue`
checks that no queue rows are running and removes the live launcher while
preserving pending command registrations. It does not cancel pending work.
The saved host authority pins agent metadata, source hashes, service assignments
and profile generator. Matching restores keep the original profile and saved
judge credential, while compiling a fresh adapter against the live broker.
Changed authority fails closed instead of changing an admitted job's fingerprint.
Production manifests also pin the host preparation authority file's hash.
The server restores those registrations before startup admission and retries
temporarily unavailable old-generation services. Valid work without its live
launcher stays pending with `local_wall`; it does not block hosted work. Invalid
saved authority follows the failed-start path. A server-generation shutdown stops
new launches and detaches bindings while preserving durable registrations.

## Durable provider ownership

`GenerationWalls::stage_provider(configuration, agent, ProviderLaunch,
installed_executable)` returns `HostLaunch`: the exact executable and arguments
to run in the host tmux serve window. This launcher owns the provider, broker
and queued process roots independently of sm's blue/green server slots.
For opencode, the host selects staged tool `opencode`, arguments `serve` plus
host-approved options, and extra settings in `ProviderLaunch`. Provider stdout
and stderr stream to tmux; stdin is closed. The ordinary provider environment
validation, immutable profile, tool staging and gated root registration apply.

Private queue state contains `local-wall-owners/<agent>/launch.json`, storing
the host configuration, agent registration, provider launch, egress client and
judge runtime. Staging is immutable and serialized across server processes.
The launcher is a content-named copy of the installed sm-server so an upgrade
cannot replace a live executable. It holds the agent preparation lock throughout
its lifetime. Queue state, launcher files and the host control socket are denied
to local agents by the production profile. Control also checks the kernel peer
user. None of this interface is exposed through HTTP.

`GenerationWalls::reconcile` reconnects with `OwnerClient` and compares the live
wall with the validated saved registration. `get_durable(agent)` returns that
host handle. Generation shutdown detaches its queue client, retaining provider
roots, broker audit token and test socket leases. An unavailable owner holds
valid jobs with `local_wall`; its marker forbids generation-owned fallback.
Server recovery does not start a provider.

Queue control transfers host-opened output descriptors, the saved command
binding and process ceiling. The owner revalidates the binding and supplies the
registered shell, cwd and environment. Each job retains a Unix connection in
sm; closure on process death or a lost launch reply kills/revokes only that job.
Provider lifetime does not depend on those connections. Queue status and
cancellation retain the existing durable lifecycle; no successful result is
invented after owner loss.

`OwnerClient::retire()` stops launch admission, kills/reaps provider and queued
descendants, revokes their roots, and unregisters egress/judge before replying.
Host SIGINT/SIGTERM and provider exit perform the same cleanup; unsuccessful
provider exit fails the launcher. The provider integration calls this on agent
retirement, cancels pending jobs, then removes queue registration/input files
and releases reserved ports. An explicit host restore can reuse the saved
launch configuration with identical immutable choices.

## Submission, execution, and restart

Submission requires a physical working directory inside the registered checkout.
The host stages the argv or script as a read-only command file in
`agent_state/queue-inputs/`, which must remain immutable in the profile. The durable
binding contains the submitting agent, a registration fingerprint, the command
path and hash, and the working directory. The host fsyncs the command and its
directory before committing the queue row.

Admission reloads and checks the saved registration and command. It launches the
staged shell through `LaunchBinding`, which registers the trusted supervisor
before allowing the sandboxed child to run. Both argv and script jobs use this
path. Output goes to host-opened log descriptors; input is closed. The existing
process limit and process-group cancellation apply. The sandbox protects host
files, other agents, and direct network endpoints while the socket adapter
provides authorized ordinary socket behavior.

Restore the saved profile/environment and attach the reconstructed socket service
before admitting pending jobs after restart. Missing or changed immutable state
produces a failed start; a valid registration waiting for its live launcher stays
pending. Neither path permits host execution fallback. Detaching removes only the live
binding. Dropping a running supervisor kills its descendants; a restart cannot
claim a successful exit without recorded evidence. The host removes immutable
queue input files when retiring the agent, after its queued/running work ends.

Local-agent cancellation compares the verified caller to the stored submitting
agent. `notify_target` may name another session but grants no cancellation rights.
Hosted owner operations keep their existing authority.

## Tests

- `http::local_agent::tests::queue_identity_survives_storage_and_notify_cannot_cancel`
  covers forged/empty requester values, ignored job environment, immutable
  registration, database reopen, and cancellation independent of notification.
- `queue::local_wall::tests` runs a real production profile and signed shell,
  verifies allowed checkout writes and socket use, denies host/profile writes,
  tests argv cancellation, and refuses missing/restored/changed launch bindings.
- `local_sockets::service::tests::launch_tests` exercises descendants, inherited
  descriptors, socket registration, and supervisor cleanup.
- `local_wall::tests::host_preparation_two_agents_restore_and_failed_launch_are_confined`
  keeps a real provider and its sockets alive across generation teardown and a
  separate recovery process; verifies unchanged broker, profile, adapter and
  manifest; admits recovered queue work; revokes an abandoned queue tree after
  an sm process crash; denies agent reads of launch configuration and host socket
  control; and verifies retirement kills provider descendants and disables egress.
