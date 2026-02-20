# EM Handoff Doc (Session 14 → 15)

## First Steps

1. Read `~/.agent-os/personas/em.md` for EM protocol
2. `sm em` — one-shot preflight (sets name, context monitor, shows children)
3. Check active workstreams below before dispatching anything.
4. **You are in the session-manager repo.** All SM work is local.

## Agent Pool

| Agent | ID | Current Role | State |
|-------|-----|--------------|-------|
| engineer-267 | 8f4ab2af | Engineer (sm#267 PR #276 fixes) | Running — fixing 3 doc blockers |
| scout-269 | 117897d1 | Idle | Free to reuse |
| reviewer-240 | 0356d8c5 | Idle | Free to reuse (persistent — do NOT kill) |
| scout-271 | e19af4a4 | Idle | Done with sm#271, free to reuse |

**Codex reviewer is persistent — clear and reuse between reviews. Do NOT kill.**

## Active Workstreams

| Workstream | Agent | Status |
|-----------|-------|--------|
| sm#267 — docs update | engineer (8f4ab2af) | Fixing 3 architect blockers on PR #276 — PR ready soon |

## SM#267 — Current Architect Blockers (PR #276)

Engineer is fixing these right now:
1. README line ~154 Core Commands: `sm spawn` missing required `claude` provider arg
2. README line ~74 'Why This Exists': old spawn syntax (no provider + deprecated --wait)
3. em.md in .agent-os submodule (3408b53) missing `sm em` one-shot preflight — still shows old 3-step manual sequence. Must update submodule to commit that has sm em, or add it to em.md in .agent-os.

**When engineer reports done:** Dispatch architect for re-review.
**If architect finds ONLY hygiene issues:** Force merge directly (user directive).

## Priority Queue

| Priority | Item | Type | Status | Next Action |
|----------|------|------|--------|-------------|
| — | sm#267 | Docs | **In flight** PR #276 | Engineer fixing → architect re-review → merge |
| 0 | sm#271 | Bug | Spec done ✓ | Implement after sm#267 merges — HIGH PRI (affects EM→user paging) |
| 1 | sm#277 | Feature | Filed, no spec needed | Auto-register remind for EM-spawned agents. Simple behavioral change. After sm#271 |
| 2 | sm#269 | Feature | Spec done ✓ | task-complete registration (cancel remind noise). After sm#277 |
| 3 | sm#250 | Feature | Spec done ✓ | **ON HOLD — user input needed before implementing** |
| 4 | sm#240 | Bug | Partial spec | Phase 2 tmux fallback failure — telemetry needed. See `docs/working/232_sm_wait_false_idle_after_clear.md` |
| 5 | sm#243 | Bug | Pre-existing | test_monitor_loop timeout — watch, don't fix unless reoccurs |

## Completed This Session (Sessions 13-14)

| Item | Result |
|------|--------|
| sm#200 | Merged PR #268 — Telegram thread cleanup |
| sm#249 | Merged PR #270 — suppress remind during compaction |
| sm#256 | Merged PR #272 — directional notify-on-stop (is_em flag + PATCH endpoint) |
| sm#261 | Merged PR #273 — UTC fix in parent wake digest |
| sm#263 | Merged PR #274 — skip fence structural fix (3 bugs) |
| sm#255 | Merged PR #275 — redundant assignment + hardcoded path constant |
| sm#263 spec | Done — skip fence structural fix (3 bugs) |
| sm#271 spec | Done — Telegram thread cleanup (Fix A: ChildMonitor close, Fix B: EM topic inheritance, Fix C: cleanup endpoint) |
| sm#269 spec | Done — task-complete registration |
| sm#277 | Filed — auto-register remind for EM-spawned agents |
| feature/183 | Branch deleted — rejected approach |

## Spec Locations

- sm#271: `docs/specs/271_telegram_thread_cleanup.md` — **implement next**
- sm#269: `docs/specs/269_task_complete_registration.md`
- sm#250: `docs/specs/250_dual_artifact_handoff.md` — ON HOLD

## SM#250 — ON HOLD

Do NOT implement until user confirms direction. Comment posted to ticket. User said "let me think about 250."

## Operational Notes

**Post-merge always:**
```bash
git checkout main && git pull origin main
source venv/bin/activate && pip install -e . -q
launchctl stop com.claude.session-manager && sleep 2 && launchctl start com.claude.session-manager
```
Wait ~4s after restart before dispatching (service needs time to come up).

**Force merge rule:** Architect finds ONLY test/branch hygiene issues → force merge (user directive).

**sm spawn vs sm dispatch:** sm spawn does NOT register remind/notify-on-stop. For EM-spawned agents, manually send `sm context-monitor enable <id>` after spawning, or prefer sm dispatch on an existing agent. sm#277 will fix this automatically.

**Push all specs/working docs to main** after committing — user checks GitHub remotely.

**Dispatch template quirk:** Curly-brace vars (e.g. `{id}`) in `--task` string cause parse errors — use square brackets instead.

**Architect immediate-stop pattern:** If architect stops at 0m with stale status, re-dispatch — it received a stale message on clear. Second dispatch always runs properly.

**reviewer-240 remind noise:** Pre-sm#269 behavior — remind fires even when idle. Known issue, not a bug.

**Branch hygiene:** Always `git checkout main` before committing specs.

**Repo:** `/Users/rajesh/Desktop/automation/session-manager/` — PRs to **main**.
**Test command:** `source venv/bin/activate && python -m pytest tests/ -v`
**Dispatch templates:** `~/.sm/dispatch_templates.yaml`
**Key roles:** `engineer` (needs `--base_branch`), `architect`, `architect-merge`, `spec-reviewer` (needs `--scout_id`)

## Completed (Sessions 1-12)

See `docs/working/tmp_feb19_em_handoff_s12.md` for full history.
