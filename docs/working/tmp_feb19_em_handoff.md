# EM Handoff Doc (Session 9 → 10)

## First Steps

1. Read `~/.agent-os/personas/em.md` for EM protocol
2. `sm name em-session10`
3. `sm context-monitor enable`
4. `sm children` — verify agents are alive (IDs below may be stale; use what `sm children` returns)
5. **You are in the session-manager repo.** All SM work is local. App queue is reactive only.
6. Check active workstreams below before dispatching anything.

## Active Workstreams

| Workstream | Agent | Status |
|-----------|-------|--------|
| sm#232 PR #245 — false idle after clear | None | PR created, architect found 2 blocking issues (below). Needs engineer fix → re-review → merge. |
| sm#244 — direct delivery without idle detection | None | **Spec complete and reviewed.** Ready for implementation. Spec at `docs/working/244_sm_send_direct_delivery.md` (on branch `feature/232-fix-false-idle-after-clear`). |

## PR #245 Blocking Issues (sm#232)

Architect found 2 blocking issues, dispatch to engineer to fix. 

## Agent Pool
None -- you're going to be starting fresh. Spawn the minimum number of agents needed: 1 claude scout + 1 codex reviewer agent for spec loop and 2 claude (engineer + architect agents) for doc loop.

## Priority Queue

**Goal: drain the full queue.** User directive: don't stop to ask, keep working autonomously.

| Priority | Item | Type | Status | Next Action |
|----------|------|------|--------|-------------|
| 1 | sm#232 PR #245 | Code | 2 blocking issues from architect | Engineer fix → architect re-review → merge → reinstall |
| 2 | sm#244 | Code | **Spec complete** | Implement → review → merge |
| 3 | sm#230 | Code | Filed (Fix 4: empty transcript retry) | Implement |
| 4 | sm#225 | Epic | Sub-tickets filed (#235-#238) | Implement (A first, then B+D parallel, then C) |
| 5 | sm#234 | Enhancement | Filed (sm dispatch --clear) | Investigate or implement |
| 6 | sm#233 | Enhancement | Filed (sm em pre-flight command) | Investigate or implement |
| 7 | sm#241 | Bug | Filed (stale context notification after clear) | Investigate |
| 8 | sm#209 | Bug | Pre-existing test failure (canonical ticket) | Investigate |
| 9 | sm#200 | Investigation | Filed | Investigate |
| 10 | sm#192 | Investigation | Filed | Investigate |

## Completed This Session (Session 9)

| Item | Result |
|------|--------|
| sm#193 PR #239 | Merged to main. 2 review rounds. SM reinstalled. |
| sm#229 PR #242 | Merged to main. 2 review rounds. SM reinstalled. Fix incomplete — sm#244 filed. |
| sm#232 PR #245 | Created. 1 review round, 2 blocking issues pending fix. |
| sm#244 | Filed (sm#229 follow-up: direct delivery). Scout investigated, spec reviewed and converged. |
| sm#225 sub-tickets | Filed #235, #236, #237, #238. Epic #225 updated. |
| sm#240 | Filed — sm children stale state bug. |
| sm#241 | Filed — stale context notification after clear. |
| sm#243 | Filed by engineer — pre-existing test_issue_40 timeout. |

## sm#229 — DONE but INCOMPLETE (PR #242 merged)

Fix added `_check_stuck_delivery()` in monitor loop with tmux prompt detection. But queued messages still not delivering post-fix. Root cause: idle detection itself is unreliable. sm#244 proposes bypassing idle detection entirely — just paste into terminal.

**Spec:** `docs/working/229_sm_send_queued_message_delivery.md`

## sm#232 — PR #245 (1 review round, 2 blocking issues)

**Root cause:** `mark_session_idle()` unconditionally sets `is_idle=True` before the `stop_notify_skip_count` guard. Late `/clear` Stop hook overwrites correct active state.

**Fix:** Time-bounded skip fence (`skip_count_armed_at`, 8s TTL), `is_idle=True` moved after skip check, `session.status=IDLE` gated on `state.is_idle`. 6 files, 8 new tests, 808 pass.

**Spec:** `docs/working/232_sm_wait_false_idle_after_clear.md`

## sm#244 — Spec Complete, Ready for Implementation

**Approach:** Don't gate delivery on idle detection. Just paste message into tmux pane and hit enter (like urgent delivery but without ESC interrupt). Removes dependency on correct idle state tracking.

**Key design decisions (from review):**
- `paste_buffered_notify_sender_id` two-phase promotion for stop-notify (set at mid-turn paste; promotes on Task X's Stop hook; fires on Task Y's Stop hook)
- Drop `_pasted_session_ids`; `delivered_at` set immediately; ~0.1% false-idle window is negligible
- Remove `is_idle` guard from `_check_stale_input` so stale-input detection works regardless of idle state
- **Removes:** `_check_stuck_delivery`, idle gate in `_try_deliver_messages`, `_stuck_delivery_count`
- **Adds:** `paste_buffered_notify_sender_id` + `paste_buffered_notify_sender_name` fields in `SessionDeliveryState`

**Spec:** `docs/working/244_sm_send_direct_delivery.md` (on branch `feature/232-fix-false-idle-after-clear`, commit 075d61f)

## sm#193 — DONE (PR #239 merged)

Codex `queue_message` path: `mark_session_active` before `is_idle=True` + `_paused_sessions` guard + 2 regression tests.

## sm#225 Epic — Sub-Tickets Filed

| # | Issue | Title | Depends On |
|---|-------|-------|-----------|
| A | #235 | Auto-remind in sm dispatch | sm#188 (done) |
| B | #236 | Remove --remind from sm send | #235 |
| C | #237 | Parent wake-up registration + digest | #235 |
| D | #238 | Default templates + sm setup | sm#187 (done) |

**Spec:** `docs/working/225_sm_dispatch_enhancements.md`

# Your primary queue: SM Bugs + Improvements

**Repo:** `/Users/rajesh/Desktop/automation/session-manager/` — PRs to **main** branch.
**Test command:** `source venv/bin/activate && python -m pytest tests/ -v`
**After merging SM fixes:** Always reinstall and restart:
```bash
cd /Users/rajesh/Desktop/automation/session-manager && source venv/bin/activate && pip install -e . -q
launchctl stop com.claude.session-manager && sleep 2 && launchctl start com.claude.session-manager
```

**Use `sm dispatch` for all dispatches.** Templates at `~/.sm/dispatch_templates.yaml`.

Key dispatch types:
- `--role engineer` — implement from spec (requires `--base_branch`)
- `--role fix-pr-review` — engineer fixes architect review comments on a PR (just point to PR, don't inline findings)
- `--role architect` — review a PR
- `--role architect-merge` — re-review and merge after fixes
- `--role scout` — investigate and write spec
- `--role spec-reviewer` — review a spec (codex)

## Repo Layout

| Repo | Path | PR target |
|------|------|-----------|
| session-manager | `/Users/rajesh/Desktop/automation/session-manager/` | main |
| agent-os | `~/.agent-os/` | push directly to main |

## Operational Lessons

**Dispatching (primary method):**
- **Use `sm dispatch` for ALL dispatches.**
- **`sm dispatch --role engineer` requires `--base_branch`.**
- **Don't inline findings in dispatch messages.** Point to GitHub or spec.
- **Exception: fix-pr-review can misfire with queued messages.** If architect's review was queued (sm#229 bug), engineer may re-report instead of fixing. Workaround: use manual `sm send` with fixes baked in explicitly.
- **`sm dispatch` does NOT clear.** Always `sm clear` first when switching roles.
- **Reset codex agents before any scout dispatch.** Fresh context for reviewer.

**Safety nets:**
- **Always re-arm `sm wait` after timeout.** #1 lesson.
- **False idle after clear (sm#232).** Every `sm clear` + `sm dispatch` triggers 2s false idle. Workaround: check `sm children`, re-arm.

**Trust but verify:**
- **`sm what` hallucinations are dangerous.** Never use for critical decisions.
- **`sm context` notifications may be stale after clear (sm#241).**
- **`sm context` notifications are NOT user interrupts.** Don't stop workflow.

**Compaction handling:**
- **Compaction ≠ death.** Wait at least one full cycle before acting.

**Agent management:**
- Names are labels. Any agent can play any role.
- **Codex: doc/spec review ONLY.** Never use codex for PR review.
- 4+ blocking items from architect → clear engineer, re-dispatch fresh with findings baked in.

**Parallelism rules:**
- Code changes: one at a time per repo/worktree.
- Investigations + specs: can run in parallel with code changes.
- Cross-repo code: can run in parallel.

**Communication:**
- Only act on `sm send` from agents. Ignore stop hooks.
- **sm send queued messages may not deliver (sm#229/sm#244).** If expecting a response, check GitHub directly or ask user.

**Workflow:**
- Don't stop to ask the user. Keep draining autonomously.
- Always spec first, even for small features.

**Post-merge:**
- Always `pip install -e .` + `launchctl stop/start com.claude.session-manager`.

**Timeouts:** Scout 600s, Engineer 600s, Architect 300s. Re-arm after every timeout.

**Pre-existing test failures:**
- test_monitor_loop_gives_up_after_max_retries (sm#209)
- test_issue_40 timeout (sm#243)

## Completed (Sessions 1-8)

| Item | Result |
|------|--------|
| sm#178 — sm send regressions | PR #179 merged |
| sm#180 — sm wait false timeout | PR #181 merged |
| sm#182 — Stop hook suppression | PR #198 merged |
| sm#183 — Non-urgent delivery interruption | PR #199 merged |
| sm#184 — Telegram notification delay | PR #195 merged |
| sm#185 — Codex bypass flag | PR #201 merged |
| sm#186 — pytest-timeout | PRs #194 + #1672 merged |
| sm#196 — sm handoff | PR #204 merged |
| sm#203 — context-aware handoff triggering | PR #205 merged |
| sm#187 — sm dispatch templates | PR #202 merged |
| sm#188 — sm remind | PR #214 merged |
| sm#206 — context monitor registration | PR #211 merged |
| sm#209 — test_monitor_loop | Filed (canonical) |
| sm#215 — Phase 3 false idle fix | PR #217 merged |
| sm#216 — Redundant idle suppression | PR #219 merged |
| sm#189 — sm tail | PR #220 merged |
| sm#190 — sm children enhancements | PR #221 merged |
| sm#210 — compaction bypass registration gate | PR #222 merged |
| sm#212 — context monitor child notifications | PR #223 merged |
| sm#224 — telegram notification v2 | PR #231 merged |
| sm#225 spec | Spec complete, sub-tickets filed |
| sm#226 — compaction notification wording | PR #227 merged |
| #1661 epic, #1668, #1646, #1389 | All merged to dev |
| #1535 | Closed (stale) |
