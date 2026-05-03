# Turn-Bound Telegram Response Relay

Issue: #706

## Summary

Make Session Manager's automatic Telegram response relay durable, ordered, and bound to the current user turn. Telegram should never receive assistant text that predates the user message that triggered the current response, and it should not receive partial/cut-off text just because a Stop hook or idle marker fired before the final response was visible.

The recommended v1 shape is a provider-agnostic relay ledger keyed by session, inbound message/turn, and assistant output identity. Provider-specific collectors should feed completed assistant messages into this ledger; the Telegram notifier should send only messages that are proven to belong to the active turn and have not already been relayed.

## Problem

Telegram response relay can currently send stale, partial, or out-of-order assistant output. The common failure shape is:

1. A user sends an agent a message.
2. The agent starts answering.
3. The agent uses tools to inspect context.
4. Session Manager receives Stop/idle signals before the final assistant response is fully reflected in the transcript/event stream.
5. Telegram receives an older response, only the latter part of the new response, or a cut-off response.

The user then has to attach to the terminal or ask the agent to explicitly `sm send rajesh ...`. That fallback should not be required for ordinary response relay.

## Observed Incident

For session `3401-consultant`:

- SM delivered a user message at `2026-05-02 15:24:19`.
- The message text began with "This is way too complex of an explanation".
- Around `15:25:26`, the Claude Stop hook relayed a previous assistant response beginning with `D13`.
- Around `15:26:27`, the transcript showed the correct answer to the new user message beginning with "You're right".

The code path scans the Claude transcript backwards for the latest assistant text and compares it to volatile in-memory `last_claude_output`. That guard is not a durable turn boundary. After restore/restart, delayed transcript writes, or missed in-memory state, an older assistant message can look eligible and get sent.

## Goals

1. Bind every automatic Telegram response to the inbound user turn or SM-delivered message that triggered it.
2. Never relay assistant output older than the active inbound message.
3. Wait or defer when provider output is not yet complete instead of sending stale text.
4. Deduplicate responses durably across SM restarts and repeated hook events.
5. Support Claude, Codex, and codex-fork through one provider-agnostic relay contract.
6. Preserve existing Telegram formatting, chunking, and delivery telemetry.
7. Keep list/watch/readiness paths free of transcript scans and provider probes.

## Non-Goals

1. Do not make Telegram a full transcript mirror.
2. Do not stream every assistant token to Telegram in v1; send completed response chunks.
3. Do not expose chain-of-thought, raw tool output, or hidden provider events.
4. Do not solve explicit human-recipient commands here; that is specified in #707.
5. Do not depend on wall-clock sleeps alone. Short defer windows are acceptable, but the correctness gate must be turn identity or transcript/event ordering.

## Relay Model

Introduce a durable response relay ledger with two concepts:

### Inbound Turn

An inbound turn is a user/operator message delivered to an agent, including:

- `sm send <agent> ...`
- Telegram inbound-to-agent messages, if supported by the existing bot path
- provider-native user input detected by hooks, where an explicit SM message id is unavailable

Recommended fields:

| Field | Meaning |
| --- | --- |
| `session_id` | SM session id. |
| `inbound_id` | SM message id when available; otherwise provider turn id or generated id. |
| `source` | `sm-send`, `telegram`, `provider-hook`, or similar. |
| `delivered_at` | UTC timestamp when input reached the agent. |
| `transcript_offset` | Claude transcript byte offset or sequence marker when available. |
| `provider_turn_id` | Codex/Claude turn id when available. |
| `text_hash` | Hash of inbound text for diagnostics without indexing raw content. |

### Assistant Output

Assistant output is completed provider-visible assistant text associated with an inbound turn.

Recommended fields:

| Field | Meaning |
| --- | --- |
| `session_id` | SM session id. |
| `inbound_id` | The inbound turn this output answers. |
| `provider` | `claude`, `codex`, `codex-fork`, etc. |
| `provider_turn_id` | Provider turn id when available. |
| `assistant_message_id` | Provider message id or generated transcript sequence id. |
| `completed_at` | UTC timestamp or provider event time. |
| `text_hash` | Output hash for dedupe/debug. |
| `relayed_at` | UTC timestamp after notifier accepts the response event. |

The relay should only send an assistant output when it can prove the output belongs to the latest unrelayed inbound turn for that session.

## Provider Collection Rules

### Claude

Claude transcript collection should stop using "scan backwards and send latest assistant text" as the correctness rule.

Instead:

1. Record an inbound turn boundary when SM delivers input to Claude.
2. Capture a transcript offset, line number, or provider message sequence near that boundary when possible.
3. On Stop/idle hooks, scan only assistant transcript entries after that boundary.
4. If no assistant entry after the boundary exists yet, defer and retry from the same boundary.
5. If the newest assistant entry after the boundary still changes across retries, wait until it is stable or until the provider reports the turn complete.

If Claude transcript metadata exposes explicit message ids or turn ids, prefer those over timestamp/offset heuristics.

### Codex And Codex-Fork

Codex providers should use structured event identities from #705:

- `threadId`
- `turnId`
- assistant `agentMessage` item id

The `turnId` should bind assistant output to the inbound turn directly when available. If the provider emits assistant output without a matching inbound turn record, the relay may store it as orphaned diagnostic output but should not send it to Telegram unless an explicit safe association exists.

## Completion And Deferral

When a hook or monitor observes possible assistant output:

1. Identify the current inbound turn for the session.
2. Collect assistant output after that turn boundary.
3. If no eligible output exists, defer rather than sending older text.
4. If output exists but the provider is still active, store it as pending and wait for turn completion or idle-stable state.
5. Once complete, send exactly one notification per assistant output identity.
6. Mark the output relayed durably after notifier acceptance.

Recommended defer behavior:

- Use short retries for known transcript/event lag, e.g. 500ms, 1s, 2s.
- Keep a bounded pending queue so one stuck response cannot grow memory forever.
- Surface a warning in logs/telemetry if a pending inbound turn has no relayable assistant output after a configurable timeout.

## Ordering

Telegram response order should follow inbound turn order per session.

If turn N is pending and turn N+1 arrives:

- Do not send a late output for turn N after sending output for turn N+1 unless the output has a clear provider turn id and the notifier can preserve order.
- Recommended v1 behavior is conservative: expire or mark turn N as superseded if a newer inbound turn arrives before N has relayable output.
- Explicit `sm send rajesh ...` from the agent is not part of automatic response relay and should still deliver independently.

## Failure Handling

- If SM restarts, pending inbound turns and relayed output identities should be recoverable from durable state.
- If transcript parsing fails for one line, log the bounded error and continue from the next line.
- If Telegram delivery fails, leave the output unrelayed or retry according to existing notifier policy; do not mark it relayed prematurely.
- If output text exceeds Telegram chunk limits, keep current chunking but treat the chunk group as one assistant output for dedupe.
- If provider output cannot be associated safely, do not send it automatically.

## Implementation Plan

1. Add a provider-agnostic response relay ledger, likely SQLite-backed alongside existing message/codex stores.
2. Record inbound turn boundaries when SM delivers input to a session.
3. Refactor Claude Stop/idle response relay to use the stored inbound boundary instead of volatile `last_claude_output` comparison.
4. Add pending/deferred relay handling for transcript/event lag.
5. Feed Codex assistant output from the #705 event collector into the same ledger.
6. Update notifier integration so relayed output is marked durable only after notifier acceptance.
7. Add tests for stale-output suppression, deferred transcript lag, restart dedupe, chunk-group dedupe, multiple inbound turns, and provider-specific boundary mapping.
8. Add a regression fixture for the `3401-consultant` timeline: an older assistant message before the inbound turn and a correct assistant message after it.

## Acceptance Criteria

1. Given a transcript with an older assistant response before the latest inbound message, the relay does not send the older response.
2. Given delayed assistant output after a Stop hook, the relay defers and sends the correct post-boundary response once it appears.
3. Restarting SM between input delivery and response relay does not cause stale output to be sent.
4. Repeated hooks or monitor events do not duplicate Telegram responses.
5. Codex and Claude providers use the same durable relay semantics even though their collection mechanisms differ.
6. `sm watch`, `sm all`, `/sessions`, and health checks do not scan transcripts to satisfy relay state.
