# EM Handoff Doc (Session 13 → 14)

## First Steps

1. Read `~/.agent-os/personas/em.md` for EM protocol
2. `sm em` — one-shot preflight (sets name, context monitor, shows children)
3. Check active workstreams below before dispatching anything.
4. **You are in the session-manager repo.** All SM work is local.

## Agent Pool

| Agent | ID | Current Role | State |
|-------|-----|--------------|-------|
| engineer-256 | 8f4ab2af | Engineer (sm#256) | Running — implementing |
| scout-269 | 117897d1 | Scout (sm#269 spec) | Running — spec sent to reviewer, iterating |
| reviewer-240 | 0356d8c5 | Spec Reviewer | Running — reviewing sm#269, sent feedback to scout |
| scout-271 | e19af4a4 | Scout (sm#271) | Running — investigating Telegram thread clutter |

**Codex reviewer is persistent — clear and reuse between reviews. Do NOT kill.**

**scout-271 routing note:** scout-271 was told to send spec to EM (ce7bc28a). When it arrives, forward to reviewer-240 once sm#269 converges.

## Active Workstreams

| Workstream | Agent | Status |
|-----------|-------|--------|
| sm#256 — directional notify-on-stop | engineer (8f4ab2af) | Implementing — PR expected soon |
| sm#269 — task-complete registration | scout (117897d1) + reviewer (0356d8c5) | Spec in review loop |
| sm#271 — Telegram thread clutter | scout (e19af4a4) | Investigating |

## SM#250 — ON HOLD (User Input Required)

**Do NOT implement sm#250 until user confirms direction.**

- Comment posted to ticket: https://github.com/rajeshgoli/session-manager/issues/250#issuecomment-3931118788
- Spec is done at `docs/specs/250_dual_artifact_handoff.md`
- User said "let me think about 250" — check with user before dispatching engineer

## Priority Queue

**Goal: drain the full queue.** One code change at a time per repo.

| Priority | Item | Type | Status | Next Action |
|----------|------|------|--------|-------------|
| 0 | sm#271 | Bug | **In flight** | Telegram thread clutter — scout investigating, spec TBD. HIGH PRI: affects EM→user paging |
| 1 | sm#256 | Feature | **In flight** | Engineer implementing → architect review → merge |
| 2 | sm#261 | Bug | Spec done ✓ | Implement after sm#256 merges |
| 3 | sm#263 | Bug | Spec done ✓ | Implement after sm#261 merges |
| 4 | sm#255 | Bug | No spec needed | Simple cleanup (2 changes): (a) remove redundant child_id_short assignment, (b) replace hardcoded tool_usage.db path with named constant. Implement after sm#263 |
| 5 | sm#267 | Docs | No spec needed | Update README + agent-os docs. Implement after sm#255 |
| 6 | sm#250 | Feature | Spec done ✓ | **ON HOLD — user input needed** |
| 7 | sm#240 | Bug | Partial spec | Phase 2 tmux fallback failure — telemetry needed. See `docs/working/232_sm_wait_false_idle_after_clear.md` |
| 8 | sm#269 | Feature | **In flight** | Spec in review loop (scout + reviewer) |
| 9 | sm#243 | Bug | Pre-existing | test_monitor_loop timeout — watch, don't fix unless reoccurs |

## Completed This Session (Session 13)

| Item | Result |
|------|--------|
| sm#200 | Merged PR #268 — Telegram thread cleanup |
| sm#249 | Merged PR #270 — suppress remind during compaction |
| sm#263 | Spec done — skip fence structural fix (3 bugs) |
| feature/183 | Branch deleted — rejected approach, non-urgent delivery fixed elsewhere |

## Post-Merge Steps (always)

```bash
git checkout main && git pull origin main
source venv/bin/activate && pip install -e . -q
launchctl stop com.claude.session-manager && sleep 2 && launchctl start com.claude.session-manager
```

## Spec Locations

- sm#256: `docs/specs/256_directional_notify_on_stop.md`
- sm#261: `docs/specs/261_timestamp_relative_times.md`
- sm#263: `docs/specs/263_skip_fence_structural_fix.md`
- sm#250: `docs/specs/250_dual_artifact_handoff.md` (**on hold**)
- sm#269: `docs/specs/269_task_complete_registration.md` (in progress)

## Operational Notes

**Dispatch templates:** `~/.sm/dispatch_templates.yaml`
- `sm dispatch` with curly-braced vars (e.g. `{id}`) in `--task` will error — use square brackets instead
- Key roles: `engineer` (needs `--base_branch`), `architect`, `spec-reviewer` (needs `--scout_id`)

**Branch hygiene:**
- Always `git checkout main` before committing specs
- No orphaned branches remain (feature/183 deleted, feature/200 merged)

**Force merge rule:** If architect finds ONLY test/branch hygiene issues → force merge directly (user directive)

**Parallelism:** One code PR at a time in this repo. Specs can run in parallel with code.

**Pre-existing test failures:**
- test_issue_40 timeout (sm#243) — known, don't file again

## Repo Layout

**Repo:** `/Users/rajesh/Desktop/automation/session-manager/` — PRs to **main**.
**Test command:** `source venv/bin/activate && python -m pytest tests/ -v`

## Completed (Sessions 1-12)

See `docs/working/tmp_feb19_em_handoff_s12.md` for full history.
