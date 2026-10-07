# Opencode transport foundation

This module supplies the HTTP, replay and persistence foundations for #1956.
It does not admit opencode session creation. #2044 owns wall-backed launch and
event projection; #2045 owns session-store delivery and lifecycle integration.

## Caller contract

For each pending row, hold the existing runtime input lock, then call
`RetainedQueueStore::bind_pending_provider_message` immediately before the
first HTTP attempt. SQLite assigns the message, part and conversation IDs in
one transaction; retries return the original binding. Do not substitute the
session's current conversation for the binding's stored conversation.

Call `Client::attempt_delivery` with that binding and the existing formatted
delivery text. It checks the provider first. Only HTTP 404 permits a POST;
HTTP 200 means accepted; other status codes and connection failures remain
unresolved. After POST it confirms by GET, bounded by the confirmation timeout.
`Accepted` authorizes the existing session-store completion transaction and
its side effects. `Unconfirmed` and errors keep the row pending with its IDs.
This module never marks a row delivered and never owns the retry sweep.

The caller enforces FIFO order and handoff holds. Rename rows call `rename`
instead of prompt delivery and complete only on success. Urgent messages use
ordinary delivery; `abort` is reserved for clear. Readiness queries HTTP and
does not inspect tmux. Busy conversations can accept delivery.

Before clear or handoff moves a pending row, GET the stored binding on the old
server. Accepted rows complete there; HTTP 404 permits
`clear_pending_provider_message_binding` with the exact expected binding.
The compare-and-clear cannot erase a newer identity. Unreachable servers and
other errors never authorize clearing. The later lifecycle ticket supplies
the clear refusal and handoff deferral deadlines from the approved contract.

`Client` connects only to IPv4 loopback, sends Basic authentication on every
request, disables environment proxy selection and redirects, and never prints
the password. Construct the event reader separately with its streaming timeout
and the same authentication. The client methods here are blocking; call them
on the existing blocking runtime workers.

## Persisted runtime and launch identities

`SessionRecord::opencode` stores the port, absolute state directory, provider
version and model endpoint. `RuntimeBinding::validate` checks their shape;
launch must still check the physical path, reserve the port and verify the
installed version and model. The optional binding and replay cursor are omitted
from existing providers' serialized records. Clear removes the replay cursor
while retaining the runtime binding.

Store reads normalize opencode reasoning effort to null. An opencode record
without a binding loads as stopped with the missing-binding error; both typed
and raw runtime decisions refuse to treat it as live. Explicit retirement
markers remain authoritative.

Call `SessionRuntimeLaunchRecord::ensure_opencode_brief_binding` only after
setting its conversation ID. Persist the updated launch record before any HTTP
attempt. Reopening a record reuses its message/part pair; a partial or invalid
pair is an error and cannot authorize generating replacement IDs. The helper
itself does not write the store or send the brief.

## Judge and launch integration

Copy `scripts/opencode/sm_judge.js` into the agent's immutable config folder at
each launch and restore. Use the production wall's returned environment:
`LOCAL_AGENT_ID`, `LOCAL_JUDGE_URL` (including `/decide`) and
`LOCAL_JUDGE_TOKEN`. The prototype's agent-ID-only request is insufficient for
the production judge. The authoritative service issues the URL and token;
do not derive the runtime endpoint from the config's default port.

Set `SM_JUDGE_PLUGIN_LOG` inside the writable `xdg/state` folder. The plugin
resolves paths against the checkout, checks all patch sources/destinations,
and denies a call unless the service explicitly allows it. Unknown tools,
including webfetch/websearch, reach the judge under their native names.

The rendered permissions allow webfetch/websearch under the owner's #1978
ruling. Launch must also set `OPENCODE_ENABLE_EXA=1`, use the wall's egress
environment, and keep question/task denied. The approved version is 1.17.9.
The fixed brief addendum is `docs/product/local_agent_addendum.md`.

## Verification

`events` (#2057) supplies conversation-scoped decoding, replay checkpoints and a
host-owned usage journal. It does not start a reader thread or mutate the
session store. #2075 supplies the durable runtime metadata; #2044 supplies
launch, recovery and reader integration.

The adapter reads the session's current conversation on every event and
replaces `Projection` after clear. It decodes on a clone, then commits the
checkpoint, activity and pending effects together before finishing external
writes. Receipts and the usage journal make those writes safe to retry.
Supply all host-generated user message IDs
(including the launch brief), so those messages cannot count as owner answers.

On connection, backfill message history before reconciling current activity.
Order by the provider's `time.created` and use recorded replayed-message IDs,
not an ID comparison with the cursor: a host ID allocated before a failed
delivery can be accepted after newer turns. Resolve assistant starts to their
parent user's cached prompt. Completed error responses (including aborted
responses without `finish`) close a prior turn when the next assistant has a
different parent user. Requests sharing a parent remain one turn, including
when tool-call finish metadata is delayed.
User submissions have no `time.completed`; metadata or text alone while the
provider is idle does not prove processing began. Backfill starts a turn only
on an assistant message or current busy/retry status. An unfinished assistant
holds the cursor, so subsequent reconnects revisit its growing parts. A completed model
request ending in `tool-calls` continues the same agent turn. It does not emit
a turn stop. A terminal assistant waits at the cursor while status remains
busy/retry; only idle or a later assistant proving a subsequent turn allows
that stop to replay. Live turn starts come from busy/retry status, after user text parts
have supplied the prompt; owner-answer effects also work while already busy.
Owner-reply effects wait for nonempty user text. Pending reply IDs survive a
checkpoint/reopen and hold the replay cursor until their text arrives. A
host-generated message retains that classification from either metadata or a
text part, even if a reconnect happens before the other event arrives and its
queue row is no longer in the generated-ID input.

`Client::event_stream` authenticates with the same secret and refuses redirects
and proxies. Each connection is limited to twenty seconds, including healthy
streams: the caller reconnects with the specified backoff and backfills again.
This bounds a stalled read before checking whether the session has stopped.
`read_event` limits a frame to two MiB and discards incomplete frames on EOF.

Usage writes share the session-store writer lock across conversation changes.
`UsageJournal` synchronizes compact Claude-format JSON lines before returning,
adds reasoning to output tokens, and repairs an interrupted final line on
reopen. A malformed complete line is an error.

`SessionStore::apply_opencode_events` accepts either a live frame or a reconnect
snapshot. It atomically saves cached activity, the `Projection` checkpoint,
cursor, sequence and ordered pending effects in the session registry. SQL
history, tool logging and owner answers commit with receipts; usage lines
deduplicate by part ID. Failed writes retain the first unfinished effect.
`recover_opencode_effects` finishes those writes before replay, including for
stopped sessions. Old-conversation usage still reaches the journal after clear,
while its context sample cannot overwrite the new conversation's gauge.
Native message creation/completion times order history and owner answers;
an old prompt cannot answer a question created after it was typed.

The caller supplies the effective loaded-model configuration, tool-log database
and all persisted host-generated message IDs (including the initial brief).
A true result requests the usual handoff check after an applied stop. Read
`opencode_pending_stop_signal`, schedule the check, then acknowledge that exact
signal with `acknowledge_opencode_stop_signal`. The signal survives later write
failures and restart until acknowledged; a stale acknowledgement cannot clear
a newer stop. Context
measurements use the existing context-update path; provider capability and
usage-seat attribution remain separate integration work.

The serving generation starts one authenticated event reader per active primary
opencode session. It opens the stream before backfill, surrounds each history
GET with matching status observations, and fences the snapshot's conversation
under the registry write lock. Buffered status frames trigger another snapshot;
buffered complete-message frames cannot regress replayed text. Reconnects use
one, two, four and then five seconds of backoff. HTTP failures do not stop the
session. Shutdown prevents further application; the twenty-second body timeout
bounds a blocked stream. Live activity expires after sixty seconds without a
successful observation and never falls back to reading the viewer's pane.
Generated IDs include delivered queue rows and the persisted launch brief.
Owner answers wake the board after unlocking even when a later effect fails.
The supervisor scans the cached registry once for unfinished effects; stopped
sessions with completed recovery do not take the writer lock on every scan.
Tool-call history reads opencode's receipt-backed tool log independently of the
hosted usage setting. Launch/restore integration remains on #2044; HTTP outbox
delivery and public entry points remain on #2045.

- `scripts/test-rust-isolated.sh opencode -- --test-threads=1`: native-shaped
  IDs, approved config fixture, config loading, duplicate-append stub and
  lost-reply recovery, status/authentication errors, busy input, persistence,
  migration, concurrent assignment, compare-and-clear, and the real production
  judge's relative/traversal/symlink/move checks.
- `node --test scripts/opencode/sm_judge.test.mjs`: all tool translations,
  credentials, deny/error paths, patch endpoint checks and decision logs.
- Event tests cover missed complete turns, two disconnects during a growing
  assistant, repeated checkpoints, owner input while busy, live prompt text,
  conversation replacement, stream framing and interrupted usage writes. A
  permanent fixture preserves the native history from the #1966 proof.

`bash scripts/opencode/check.sh` runs both suites, Clippy and formatting.

## Host launch driver

`launch` (#2079, macOS) prepares private config, the judge plugin, an independent
plugin SDK copy and the persistent server password. SDK preparation installs
the judge plugin before running `opencode debug config`, so the pinned runtime
waits for its dependency installation before exiting. The installed-provider
check is available separately (requires opencode 1.17.9 and npm access):

```sh
scripts/test-rust-isolated.sh --lib production_opencode_prepares_its_real_plugin_sdk_outside_the_wall -- --ignored
```

It stages opencode through
the production durable wall owner; tmux runs that owner rather than a provider
outside the wall. The host serve script restarts at most five times within ten
minutes, and handles termination by stopping the owner. `serve.pid` identifies
that owner, which controls the provider and queued process trees.

The driver exposes separate server start and attach steps so the store can
commit the conversation and start its event reader between them. Authenticated
health/status polling bounds startup. Initial-brief retries use the persisted
binding and GET-before-POST contract. The caller supplies the configured brief
acknowledgement timeout.

Admission uses the shipped model host's `ready` record and loaded identifier;
rendered configuration uses its endpoint and context. The store must serialize
admission and include provisional launches when reserving seats and ports.
Only its authorized handoff path supplies the predecessor-seat exemption.
Wall host configuration, staged tools and credentials come from the host.
Preparation/restore must prove any previous provider exited before replacing
config or library files. Public creation routes remain disabled until their
session-store integration is complete.

The full historical adversarial suite, launch/runtime immutability, usage
attribution and live acceptance remain attached to #1956's subsequent tickets.
