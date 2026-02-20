# sm#249: Suppress remind during active compaction

## Problem

When an agent is mid-compaction, `sm remind` delivers its overdue message anyway, interrupting the compaction process and triggering a second compaction cycle.

Observed: engineer received a remind interrupt while compacting → second compaction fired immediately after the first completed.

## Root Cause

No compaction state is tracked server-side. The `Session` model has no `_is_compacting` flag. `PreCompact` hook notifies the server (compaction starting), but `SessionStart(compact)` hook only reads from the server — never signals completion. Both remind delivery paths (`_run_remind_task`, `_fire_reminder`) are blind to the compaction window.

## Fix

1. Add `Session._is_compacting: bool` runtime flag (not persisted, defaults `False`)
2. Set `_is_compacting = True` on `event: "compaction"` (PreCompact hook)
3. Clear `_is_compacting = False` + call `reset_remind()` on new `event: "compaction_complete"` — fired by updated `post_compact_recovery.sh` template in `scripts/install_context_hooks.sh`; timer reset here (not at PreCompact) so agent gets a fresh soft-threshold window exactly when it wakes
4. `_run_remind_task`: skip delivery iteration when `_is_compacting` is True
5. `_fire_reminder`: bounded wait (max 300s, poll every 5s) when `_is_compacting` is True; delivers after timeout to preserve one-shot guarantee

## Classification

Single ticket. Narrow scope: add compaction-state check to remind delivery path.
