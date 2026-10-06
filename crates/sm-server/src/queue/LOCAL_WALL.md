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
before admitting pending jobs after restart. Missing or changed state produces a
failed start, never a host execution fallback. Detaching removes only the live
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
