# EM Handoff Doc (Session 11 → 12)

## First Steps

1. Read `~/.agent-os/personas/em.md` for EM protocol
2. `sm em` — one-shot preflight (sets name, context monitor, shows children) — NEW this session
3. Check active workstreams below before dispatching anything.
4. **You are in the session-manager repo.** All SM work is local.

## Agent Pool

| Agent | ID | Current Role | State |
|-------|-----|--------------|-------|
| engineer-sm200 | 8f4ab2af | Engineer (sm#200) | Running |
| scout-250 | 117897d1 | Scout (sm#250) | Running |
| reviewer-240 | 0356d8c5 | Codex Reviewer | Idle, standing by |

**Codex reviewer is persistent — clear and reuse between reviews. Do NOT kill.**

## Active Workstreams

| Workstream | Agent | Status |
|-----------|-------|--------|
| sm#200 — Telegram thread cleanup | engineer (8f4ab2af) | In flight. When done: architect review → merge → reinstall → next item. |
| sm#250 — Dual-artifact handoff investigation | scout (117897d1) | In flight. Experimenting with tmux Ctrl+O/E capture. Reports to EM (check sm me for ID). |

## Priority Queue

**Goal: drain the full queue.** User directive: don't stop to ask, keep working autonomously.

| Priority | Item | Type | Status | Next Action |
|----------|------|------|--------|-------------|
| 1 | sm#200 | Code | **In flight** | Architect review → merge → reinstall |
| 2 | sm#249 | Bug | Filed | Investigate/implement (suppress remind during compaction) |
| 3 | sm#256 | Feature | Filed | Implement (directional notify-on-stop, unblocked — sm#233 merged) |
| 4 | sm#258 | Bug | Filed | Implement (suppress sm wait noise if sm send arrived within 30s) |
| 5 | sm#267 | Docs | Filed | Scout task: audit all session 11 PRs, update README + agent-os docs |
| 6 | sm#250 | Investigation | **In flight** | Dual-artifact handoff — experiment tmux capture |
| 7 | sm#263 | Investigation | Queued | Structural skip fence fix (hook identity) — after sm#250 |
| 8 | sm#262 | Bug | Filed | Spurious sm wait fix — LOW priority (sm wait removed from workflow) |

## Completed This Session (Session 11)

| Item | Result |
|------|--------|
| sm#237 PR #253 | Merged — parent wake-up registration + digest |
| sm#225 epic | Closed — all sub-issues complete (#235, #236, #237, #238) |
| sm#234 PR #257 | Merged — sm dispatch auto-clear + --no-clear opt-out |
| sm#233 PR #259 | Merged — sm em one-shot preflight command |
| sm#241 PR #264 | Merged — message_category column + cancel stale context notifications |
| sm#209 PR #266 | Merged — fix monitor loop test timeout (923 tests passing) |
| sm#240 investigation | Complete — spec at specs/240_sm_wait_spurious_fires.md; root cause: skip fence race from sm#234 auto-clear |
| em.md updated | dispatch-and-chill workflow: no sm wait, sm tail/children/status as primary observability |
| em.md pushed | agent-os commit 3408b53 |
| Filed | #256 (directional notify-on-stop), #258 (suppress sm wait after sm send), #261 (timestamp bug in sm remind/tail), #262 (spurious sm wait fix), #263 (structural skip fence), #267 (doc update) |
| Codex reviewer | Added to persistent pool (clear+reuse, never kill) |

## Operational Lessons

**Dispatching:**
- **Use `sm dispatch` for ALL dispatches.** Requires `--base_branch` for engineer role.
- **sm dispatch now auto-clears** (sm#234 merged, PR #257). Use `--no-clear` for follow-up dispatches.
- **Dispatch and go idle.** sm remind (210s soft / 420s hard) + notify-on-stop pages you. No sm wait needed.

**Observability (in order):**
1. `sm children` — state, last tool, status message. Use first.
2. `sm status <id>` — focused single-agent view.
3. `sm tail <id>` — last N tool actions with timestamps. Add `--raw` for full tmux pane output.
4. `sm what <id>` — haiku summary. Last resort only.

**Codex reviewer:**
- Keep persistent in pool — clear and reuse between reviews. Do NOT kill after each review.
- If stuck: clear and re-dispatch with spec file path baked in directly.
- Codex = spec review only. Not for PR review. Cannot do investigations.

**Parallelism:**
- Code: one at a time per repo. Sequential only.
- Spec/investigation: one scout at a time. Sequential.
- Code + investigation: parallel fine.

**Spurious sm wait fires:**
- Root cause identified (sm#262): sm#234 auto-clear introduced skip fence race.
- LOW priority to fix — sm wait removed from EM workflow, so no real impact.
- Re-arm if you ever use sm wait: check sm children first, 0s/2s fires = spurious.

**Communication:**
- Only act on `sm send` from agents. Ignore stop hooks.
- sm remind fires are informational — no action unless 3+ fires with no progress.
- sm wait suppressed after sm send: pending fix in sm#258.

**Post-merge always:**
```bash
git checkout main && git pull origin main
source venv/bin/activate && pip install -e . -q
launchctl stop com.claude.session-manager && sleep 2 && launchctl start com.claude.session-manager
```

**Pre-existing test failures:**
- test_issue_40 timeout (sm#243)
- Any new ones: file immediately, don't ignore.

## Specs Ready for Implementation

| Issue | Spec File | Summary |
|-------|-----------|---------|
| sm#200 | `docs/working/200_telegram_thread_cleanup.md` | try-and-fallback Telegram notification on clear/kill (**in flight**) |
| sm#262 | `specs/240_sm_wait_spurious_fires.md` | Phase 4b tmux prompt check in `_watch_for_idle` (low priority) |

## Repo Layout & Commands

**Repo:** `/Users/rajesh/Desktop/automation/session-manager/` — PRs to **main**.
**Test command:** `source venv/bin/activate && python -m pytest tests/ -v`
**Dispatch templates:** `~/.sm/dispatch_templates.yaml`

Key dispatch roles: `engineer` (needs `--base_branch`), `architect`, `architect-merge`, `fix-pr-review`, `scout`

## Completed (Sessions 1-10)

See `docs/working/tmp_feb19_em_handoff.md` for full history.
