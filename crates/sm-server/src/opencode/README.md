# Opencode transport foundation

This module implements #2043, the first implementation ticket under #1956.
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

- `scripts/test-rust-isolated.sh opencode -- --test-threads=1`: native-shaped
  IDs, approved config fixture, config loading, duplicate-append stub and
  lost-reply recovery, status/authentication errors, busy input, persistence,
  migration, concurrent assignment, compare-and-clear, and the real production
  judge's relative/traversal/symlink/move checks.
- `node --test scripts/opencode/sm_judge.test.mjs`: all tool translations,
  credentials, deny/error paths, patch endpoint checks and decision logs.

The full historical adversarial suite, launch/runtime immutability, usage
attribution and live acceptance remain attached to #1956's subsequent tickets.
