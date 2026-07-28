# sm#1143: Restore `sm what` with provider-native `/btw`

## Status

Proposed. This is a specification-only change. Implementation must not begin until
the owner approves this document.

## Problem

The Rust cutover retired `sm what` because the old implementation captured a tmux
transcript and sent it to an unrelated Haiku process. That implementation was
lossy, exposed more transcript than necessary, and could describe stale output
instead of the target agent's current understanding.

Claude and Codex now provide `/btw`, which asks a small side agent a question
against the live conversation context. `sm what` should use that native capability
and relay the side agent's answer to the managed agent that asked.

The replacement cannot be built on `sm send`:

1. `sm send` wraps cross-session input as an ordinary message, so the target would
   receive `[Input from: ... via sm send] /btw ...` instead of a slash command.
2. `/btw` returns through provider UI state, not through Session Manager tools.
3. A generic child-completion or pane-tail relay includes terminal control
   sequences and unrelated screen content.
4. Codex remains in the side conversation until it is explicitly returned to the
   parent context.

## Observed Behavior

The design is based on live disposable-session probes, not code inspection alone.

### Claude

Submitting this literal line with tmux literal-key input and Enter:

```text
/btw Summarize what you have done so far in one sentence.
```

opened a result modal containing the question, answer, and this stable footer:

```text
↑/↓ to scroll · c to copy · f to fork · Esc to close
```

The answer was visible in the terminal but was not available through an SM
response API. `Escape` returned Claude to its main conversation.

### Stock Codex

Submitting the same literal line opened a side conversation. The displayed
question omitted the `/btw` prefix, and the side UI included:

```text
Side from main thread · Ctrl+C to return
```

The side answer was terminal-visible and no records appeared in the configured
Codex event stream. `Ctrl+C` was required to return to the main thread.

### codex-fork

Sending `/btw ...` through the existing `submit_message` control operation did
not invoke the slash command. It created an ordinary main-thread user turn and
consumed the main agent context.

The fork already implements `/btw` internally through its `StartSide` path, but
the control socket exposes no operation for that path. A dedicated operation is
required. Because the fork owns a structured control and event contract, SM must
not use tmux for codex-fork submission, result capture, or cleanup.

## Decision

Restore `sm what` as an asynchronous, requester-aware command:

```text
sm what <target> [prompt...]
sm btw <target> [prompt...]
```

`sm btw` is an exact alias. Both command names use the same API and all delivered
responses say `via sm what`.

When `prompt` is omitted, use:

```text
Summarize what you've done so far
```

The provider receives a literal native command equivalent to:

```text
/btw <prompt>
```

The quote characters used at a shell prompt are not sent to the provider.

The command returns after the request is durably accepted. The answer arrives
as input to the requesting managed session:

```text
[Input from <friendly_name> (<session_id>) via sm what] <summary>
```

`<friendly_name>` and `<session_id>` identify the target whose context was
summarized. The full stable session ID is used. If the summary is multiline,
subsequent lines are preserved after the first line.

## CLI Contract

### Target and requester

1. `<target>` uses normal exact ID, unique ID prefix, friendly-name, and role
   resolution.
2. The requester is read from `SESSION_MANAGER_ID`, with
   `CLAUDE_SESSION_MANAGER_ID` as the compatibility fallback.
3. The requester and target must both be live managed sessions.
4. Self-targeting is allowed. The response is not queued until provider cleanup
   has restored the main conversation.
5. An unmanaged shell is rejected with:

   ```text
   Error: sm what requires a managed requester session
   ```

An operator-only synchronous `--print` mode is not part of this version.

### Prompt

1. CLI arguments after `<target>` are joined with one space.
2. Leading and trailing whitespace is removed.
3. Empty prompts use the default prompt.
4. Newlines, NUL bytes, and terminal control characters are rejected.
5. The UTF-8 prompt is limited to 4 KiB.

These rules keep the native input to one slash-command line and prevent prompt
text from becoming a second terminal command.

### Acknowledgement

On acceptance:

```text
Requested context summary from <friendly_name> (<session_id>)
```

Immediate validation or delivery failures return nonzero and do not create a
request. Failures after acceptance are delivered to the requester through the
same internal response category:

```text
[sm what request <request_id> for <friendly_name> (<session_id>) failed] <reason>
```

## Request Lifecycle

Add a dedicated, durable `btw_requests` ledger. This is not modeled as an
ordinary `sm send` message.

Required fields:

| Field | Purpose |
|---|---|
| `request_id` | Globally unique correlation ID |
| `requester_session_id` | Destination for the answer |
| `target_session_id` | Session whose context is queried |
| `target_provider` | Provider adapter selected at acceptance |
| `prompt` | Validated `/btw` argument |
| `status` | `pending`, `running`, `completed`, `failed`, or `timed_out` |
| `provider_correlation` | Fork request/thread IDs or terminal capture marker |
| `created_at`, `started_at`, `finished_at` | Recovery and latency accounting |
| `result` or `error` | Bounded terminal outcome |
| `response_delivered_at` | One-shot requester relay guard |

Lifecycle:

1. Resolve and validate requester and target.
2. In one transaction, reject another nonterminal request for the same target
   and insert the new request.
3. Return the CLI acknowledgement.
4. Acquire the target's provider-native input lock.
5. Submit the request through the provider adapter.
6. Observe the correlated result.
7. Restore the target's normal provider UI state.
8. Persist the terminal outcome.
9. Queue exactly one preformatted response for the requester.
10. Mark `response_delivered_at` only after queue insertion succeeds.

One target may have only one active `sm what` request. Different targets may run
concurrently. A duplicate request fails with HTTP `409` and names the active
request ID.

The main target task may continue while `/btw` runs. `sm what` must not arm
notify-on-stop, response-relay, reminder, or child-completion behavior, and must
not rewrite the target's main-task lifecycle state.

## Provider Contracts

### Claude

Claude has no structured external slash-command control path, so its adapter may
use tmux for this operation.

Submission:

1. Verify the pane is reachable and is not already in a modal, paste, approval,
   or unrelated side-command state.
2. Capture a bounded pre-submission pane snapshot.
3. Send the complete `/btw <prompt>` line with tmux literal-key input.
4. Send Enter as a separate key operation.
5. Never route the line through `sm send` or generic queue formatting.

Completion and cleanup:

1. Locate the new `/btw` result region after the baseline snapshot.
2. Require the Claude result footer before extracting an answer.
3. Strip ANSI/control sequences with the existing terminal parser.
4. Exclude the question, separators, footer, and any baseline content.
5. Send `Escape` after the result is captured.
6. Verify that the result modal is gone before marking the request completed.

### Stock Codex

Stock Codex also requires a tmux adapter because its side result is not present
in the configured event stream.

Submission follows the same literal-line and separate-Enter rules as Claude.

Completion and cleanup:

1. Correlate the newly displayed question with the submitted prompt.
2. Require the `Side from main thread` state and a completed side answer.
3. Extract only assistant answer rows after the correlated question.
4. Strip ANSI/control sequences and exclude headers, status lines, composer
   content, and footer text.
5. Send `Ctrl+C` after capture.
6. Verify that `Side from main thread` is gone and the parent context is active
   before marking the request completed.

Leaving Codex in the side context is a failed request even when an answer was
captured. Cleanup is retried until the request timeout.

### codex-fork

codex-fork must use its Unix control socket and JSONL event stream. There is no
tmux fallback.

Extend the fork control protocol with:

```json
{
  "request_id": "<request_id>",
  "expected_epoch": "<epoch>",
  "command": "submit_btw",
  "prompt": "<prompt>"
}
```

The immediate response acknowledges acceptance or returns a typed error.

`submit_btw` must:

1. Use the same side-boundary prompt, fork configuration, and model behavior as
   interactive `/btw`.
2. Fork a transient side thread from the current main thread.
3. Submit the prompt to that side thread without changing the TUI's displayed
   or active thread.
4. Preserve an in-progress main turn.
5. Emit correlated structured lifecycle records:
   - `btw_started`
   - `btw_completed` with the final answer
   - `btw_failed` with a typed error
6. Close and discard the transient side thread after the terminal event.
7. Apply the existing control-socket epoch, idempotency-cache, size, and
   permissions rules.

SM consumes the structured result by `request_id`; it does not parse terminal
rows or infer completion from generic turn events.

## Terminal Adapter Safety

Claude and stock Codex terminal adapters are necessarily provider-version
sensitive. They must be isolated behind a common provider interface and covered
by fixture tests.

Required safeguards:

1. Capture physical terminal rows without tmux join-wrapped-lines behavior.
2. Bound every capture by rows and bytes.
3. Use a terminal/ANSI parser rather than regular expressions alone.
4. Require provider-specific start and completion markers.
5. Never return baseline rows or screen chrome as the summary.
6. Reject ambiguous screens instead of typing into an unknown UI state.
7. Use one lock for all SM-owned native keyboard operations on a session.
8. Verify cleanup from a fresh pane capture.

Default timeout is 60 seconds from provider submission. The extracted answer is
limited to 16 KiB after ANSI stripping. Truncation is explicit:

```text
...[sm what output truncated]
```

## API Contract

Add:

```text
POST /sessions/{target}/btw
```

Request:

```json
{
  "requester_session_id": "<managed-session-id>",
  "prompt": "Summarize what you've done so far"
}
```

Accepted response (`202`):

```json
{
  "request_id": "<request-id>",
  "status": "pending",
  "target_session_id": "<resolved-id>",
  "target_friendly_name": "<name>"
}
```

The server resolves `requester_session_id` to a live session before accepting the
request. As with existing requester-aware SM endpoints, the loopback API remains
inside the same-user local trust boundary; this ticket does not add a new
authentication scheme.

Add an internal status read for recovery and tests:

```text
GET /btw-requests/{request_id}
```

It returns bounded request metadata and status for local recovery and diagnostics.

## Requester Delivery

The answer uses the existing durable message queue transport with a new
`message_category` of `btw_response`, but it bypasses cross-session `sm send`
formatting.

The queue payload is already fully formatted as:

```text
[Input from <friendly_name> (<session_id>) via sm what] <summary>
```

Delivery invariants:

1. Do not prepend `[Input from: ... via sm send]`.
2. Do not interpret a leading slash in the summary.
3. Do not set `response_relay_source`.
4. Do not trigger target stop/completion notifications.
5. Deduplicate by `request_id`, including across server restart.
6. If the requester is retired before completion, mark the response undeliverable
   and retain the terminal request record for normal retention cleanup.

## Restart Recovery

On server startup:

1. Resume nonterminal codex-fork requests from the event sequence stored in
   `provider_correlation`.
2. For Claude and stock Codex, inspect the target pane for the correlated provider
   state and continue capture/cleanup when unambiguous.
3. If the pane or correlation state is gone, fail the request rather than
   resubmitting `/btw`.
4. Requeue terminal responses whose `response_delivered_at` is null.
5. Never deliver a response twice.

Session teardown fails any nonterminal request targeting or requested by that
session and performs best-effort provider cleanup before the runtime disappears.

## Compatibility and Removal

This feature is an owner-approved replacement for the command name, not a
revival of the retired implementation.

Remove or keep retired:

1. The Python transcript-to-Haiku summary endpoint.
2. `sm what --lines`.
3. `sm what --deep`.
4. Any shell-out to an external summarizer.
5. Any fallback that sends `/btw` as an ordinary user message.

Existing scripts that call the retired flags receive an explicit migration error:

```text
Error: --lines and --deep were removed; use sm what <target> [prompt...]
```

Supported targets are `claude`, `codex`, and `codex-fork`. Retired
`codex-app` sessions remain unsupported.

## Testing

### CLI

1. Parse default and custom prompts for both command names.
2. Prove `sm btw` and `sm what` issue identical API requests.
3. Validate requester, target, self-target, prompt bounds, and retired flags.
4. Verify exact acknowledgement and exit codes.

### Server and persistence

1. Atomic one-active-request-per-target enforcement.
2. State transitions and timeout behavior.
3. Restart recovery for every nonterminal state.
4. One-shot response insertion and restart deduplication.
5. Requester or target teardown during each lifecycle stage.
6. Exact response envelope without `sm send` wrapping.
7. No notify-on-stop, reminder, response-relay, or completion side effects.

### Provider adapters

1. Claude fixtures for complete, streaming, wrapped, ANSI-heavy, ambiguous, and
   timed-out result modals.
2. Stock Codex fixtures for complete, streaming, wrapped, ANSI-heavy, ambiguous,
   and timed-out side conversations.
3. Cleanup verification tests for Claude `Escape` and Codex `Ctrl+C`.
4. Literal input tests proving Enter is sent separately and `/btw` is the first
   bytes of provider input.
5. codex-fork control protocol tests for epoch validation, duplicate request IDs,
   active main turns, structured result/error events, hidden-thread behavior, and
   transient thread cleanup.

### Live acceptance

Run disposable sessions for all three supported providers:

1. Ask the default question while the target is idle.
2. Ask a custom question while the target has an active main task.
3. Verify the target returns to or remains in its main context.
4. Verify the requester receives exactly one clean response.
5. Restart SM during an in-flight request and verify recovery.
6. Confirm no terminal control bytes appear in requester input.

## Rollout

The CLI command remains rejected until the backend request ledger and the
selected provider adapter are available. Provider capability is explicit; no
provider silently falls back to ordinary message delivery.

Ship in this order:

1. #1144: codex-fork `submit_btw` control and event contract.
2. #1145: SM request ledger, API, response delivery, and codex-fork adapter.
3. #1146: Claude and stock Codex terminal adapters, CLI restoration, and
   end-to-end acceptance.

## Ticket Classification

**Epic.** The work crosses the codex fork control/event contract, durable SM
orchestration, two terminal-state parsers, requester delivery, CLI compatibility,
and restart recovery. One agent cannot implement and validate the full surface
without context compaction.

Implementation must be split into the three ordered tickets in the Rollout
section. Issue #1143 remains the epic and approval gate; no implementation ticket
starts until this specification is approved.
