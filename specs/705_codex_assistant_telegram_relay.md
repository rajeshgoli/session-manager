# Codex Assistant Telegram Relay

Issue: #705

## Summary

Make Codex and codex-fork assistant replies first-class Telegram notifications. A tmux-backed Codex session with a Telegram topic should relay completed assistant turns to Telegram the same way Claude sessions do, without requiring the agent to explicitly run `sm send rajesh ...`.

The recommended v1 shape is to consume the provider-native Codex event stream, assemble completed `agentMessage` items by turn/message identity, and send the final assistant text through Session Manager's existing notifier path.

## Problem

Codex and codex-fork sessions can be managed by Session Manager and can have Telegram topics, but normal assistant replies are not consistently relayed to Telegram. In practice, the user has to ask codex-fork agents to run `sm send rajesh ...` to get a reliable Telegram-visible response. That makes Codex a second-class provider compared with Claude.

The observed live behavior is:

- The codex-fork event stream contains structured assistant output as `item/agentMessage/delta` and `item_completed` events.
- The `item_completed` event includes the completed assistant message text for an `agentMessage`.
- The current SM monitor only emits a Telegram response when a normalized `turn_complete` event includes a `last_agent_message` payload.
- In observed codex-fork rows, the `turn_complete` payload/output preview is empty, so no Telegram response notification is sent.

This is a relay contract mismatch, not a Telegram topic problem.

## Goals

1. Relay completed Codex and codex-fork assistant replies to Telegram automatically when the session has a Telegram topic.
2. Use structured Codex events as the source of truth, not tmux pane scraping.
3. Deduplicate notifications by stable provider identity such as `turnId` plus assistant message item id.
4. Preserve complete responses across tool calls and multi-item turns.
5. Reuse the existing Session Manager notification path for formatting, chunking, telemetry, and Telegram delivery.
6. Label provider output accurately in Telegram, e.g. `Codex` or `Codex-fork`, instead of using Claude-specific wording.
7. Keep relay work off hot read paths such as `/sessions`, `sm watch`, and `sm all`.

## Non-Goals

1. Do not add a new agent-facing command for this path. This is automatic provider output relay.
2. Do not rely on terminal screen scraping for Codex output.
3. Do not relay hidden chain-of-thought, raw debug events, or tool output unless Codex already exposes it as final assistant-visible text.
4. Do not solve all Telegram ordering/stale-message issues here. Turn-bound ordering across providers is specified separately in #706.
5. Do not require codex-fork changes unless the existing event stream cannot provide stable turn/message identity.

## Event Contract

The Codex monitor should treat these event shapes as assistant-output inputs:

| Event | Required fields | Use |
| --- | --- | --- |
| `item/agentMessage/delta` | `threadId`, `turnId`, message item id, `delta` | Optional fallback accumulation while a message streams. |
| `item_completed` where `item.type == "agentMessage"` | `threadId`, `turnId`, `item.id`, `item.text` | Preferred completed message source. |
| `turn_complete` | `threadId`, `turnId` | Completion boundary when available. |

Implementation should prefer completed `item.text` over accumulated deltas. Deltas are useful only if a completed message event is missing or incomplete.

Relay state should be keyed by:

```text
session_id + threadId + turnId + agentMessage item id
```

If `threadId` is unavailable, the implementation may key by `session_id + turnId + item id`, but it should keep the code structured so `threadId` can be added without a migration.

## Relay Semantics

For each Codex session:

1. The event monitor tails the existing codex-fork event stream.
2. For each assistant `agentMessage`, store the latest completed text in an in-memory accumulator and a durable relay ledger.
3. When the assistant message is completed and the turn is complete or idle-stable, enqueue one Telegram response notification.
4. Mark that message identity as relayed only after the notifier accepts the event for delivery.
5. If SM restarts, use the durable ledger to avoid re-sending already relayed message identities.

If a turn contains multiple final assistant messages, v1 may either:

- send each completed assistant message once, preserving provider order; or
- concatenate assistant messages for the same turn and send one response when `turn_complete` lands.

The recommended v1 behavior is to send one response per completed `agentMessage` item because it maps cleanly onto provider events and avoids waiting forever if `turn_complete` is missing.

## Durable State

Add a small durable relay ledger for Codex assistant notifications. It can live in the existing Codex event store or a separate SQLite table.

Recommended fields:

| Field | Meaning |
| --- | --- |
| `session_id` | SM session id. |
| `thread_id` | Codex thread id when present. |
| `turn_id` | Codex turn id. |
| `message_item_id` | Codex assistant message item id. |
| `text_hash` | Hash of relayed text for debug/dedupe without storing all content in indexes. |
| `relayed_at` | UTC timestamp when queued/sent to notifier. |
| `telegram_thread_id` | Topic id at relay time for diagnostics. |

The implementation may store bounded text preview for diagnostics, but should avoid introducing a new unbounded transcript store unless needed for #706.

## Telegram Formatting

The notifier should render provider labels from the session provider:

| Provider | Suggested label |
| --- | --- |
| `claude` | `Claude` |
| `codex` | `Codex` |
| `codex-fork` | `Codex-fork` |
| `codex-app` | `Codex-app` |

This spec only requires `codex` and `codex-fork` to relay tmux-backed assistant output. Headless `codex-app` relay can be added later if it exposes the same event contract.

## Failure Handling

- If Telegram delivery fails transiently, follow the existing notifier retry/telemetry behavior.
- If an event has no stable message id, do not send it unless a fallback id can be constructed from turn id plus sequence number.
- If completed text is empty, do not send an empty Telegram response.
- If event processing crashes on one malformed event, log the event id/path and continue tailing.
- If both completed text and accumulated deltas exist and differ, use completed text and log a bounded debug warning.

## Implementation Plan

1. Extend Codex event normalization to recognize assistant message deltas and completed `agentMessage` items.
2. Add a Codex assistant relay accumulator keyed by `session_id`, `threadId`, `turnId`, and item id.
3. Add a durable relay ledger to dedupe notifications across monitor restarts.
4. On completed assistant message, call the same notification path used by Claude response relay with provider-aware labels.
5. Preserve existing hook output storage where useful so `sm tail` and diagnostics can show the last assistant output consistently.
6. Add unit tests for delta accumulation, completed-message preference, dedupe after restart, empty-message suppression, provider label formatting, and malformed event tolerance.
7. Add an integration-style fixture test using a codex-fork event stream containing `item/agentMessage/delta`, `item_completed`, and `turn_complete` where `turn_complete` has no `last_agent_message`.

## Acceptance Criteria

1. A codex-fork session with a Telegram topic relays its completed assistant response without the agent running `sm send`.
2. A fixture matching the observed event stream sends the completed `item.text` even when `turn_complete.last_agent_message` is absent.
3. Restarting SM and replaying the same event file does not duplicate already relayed assistant messages.
4. Telegram output identifies Codex providers accurately.
5. Normal read surfaces remain fast because relay processing stays on monitor/background paths.
