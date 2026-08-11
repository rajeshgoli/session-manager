# Usage transcript fixture corpus

These redacted JSONL streams cover one Claude parent session, its subagent
replay, an in-file compaction marker, a resume under a second Claude session
ID, and a Codex model family.

The subagent repeats `msg-shared` with a different `requestId`, marks it as a
sidechain, and inflates `cache_read_input_tokens` from 80 to 800. C.3 keeps the
non-sidechain parent copy, so the accepted Claude totals are input 350, output
80, cache creation 15, cache read 135, for 580 tokens. The resumed Opus message
is included in those Claude totals.

Codex normalizes 120 input tokens with 90 cached to 30 uncached input, plus 30
output tokens, for 150 tokens. The corpus therefore has four accepted ledger
messages and nine aliases: one exact and one loose alias per ledger row, plus
the rejected replay's exact alias pointing at the preferred parent row. Total
accepted billable tokens are 730. The compaction marker and inflated sidechain
replay are not ledger rows. See `expected.json` for the hand-computed answer.
