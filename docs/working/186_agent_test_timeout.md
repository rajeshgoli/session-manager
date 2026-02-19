# sm#186: Agent Test Runs Exceed Shell Timeout

**Issue:** [sm#186](https://github.com/rajeshgoli/session-manager/issues/186)
**Status:** Investigation complete

---

## Problem Statement

Engineers dispatched to run tests get stuck for 10+ minutes. The EM then has to manually nudge them to skip tests and create the PR. This wastes agent tokens and EM attention on every engineer dispatch.

## Root Cause Analysis

Three factors combine to create the problem:

### 1. Engineer persona instructs full-suite, verbose runs

The engineer persona (`personas/engineer.md`) hardcodes:

```
7. **Test** — `python -m pytest tests/ -v` (STOP if tests fail)
```

And in Task Completion Protocol:

```
1. **Test**: `python -m pytest tests/ -v`
```

This runs **every test**, verbose, with no timeout. The `-v` flag produces one line per test, generating massive output for large suites.

### 2. Claude Code Bash tool has a 2-minute default timeout

Per the issue report and observed agent behavior, the Claude Code Bash tool defaults to a 120-second (2-minute) timeout. Agents can request up to 600s (10 min) via the `timeout` parameter, but the engineer persona doesn't instruct this. When the timeout fires mid-run, the agent receives a truncated/error result and doesn't understand the test run was interrupted — it may retry, re-run, or stall.

### 3. No per-test timeout guard

Neither repo configures `pytest-timeout` or equivalent. A single hanging test (e.g., one that accidentally opens a socket, waits for input, or hits a slow external resource) can block the entire suite indefinitely.

## Empirical Data

Measured on local machine (`-q --tb=no`), runtime varies significantly with system load:

| Repo | Tests | Unloaded | Under multi-agent load |
|------|-------|----------|----------------------|
| fractal-market-simulator | 2462 (2401 run, 61 skipped) | ~60s | ~138s |
| session-manager | 560 | ~78s | ~78s (CPU-light tests) |

**Fractal exceeds the 2-minute budget under realistic multi-agent conditions.** The 60s→138s increase (2.3x) occurs because fractal tests are CPU-intensive (data loading, numerical computation). Session-manager tests are I/O-mock-heavy and less affected by load.

Additional factors that increase runtime:
- The `-v` flag adds output overhead proportional to test count
- Any new slow test pushes the suite further over the threshold
- Fractal's `pytest_sessionfinish` hook spawns a background backtest — while non-blocking, it adds process startup overhead

The issue reports 10+ minute stalls, which suggests agents are also **retrying** after timeout. Each retry burns another 2-minute window, and the agent may retry 3-5 times before giving up or being nudged.

## Existing Safeguards

- **Fractal conftest.py** already has a `@pytest.mark.slow` marker with `--run-slow` opt-in. This is a good pattern but only covers explicitly-marked tests.
- **No `pytest-timeout`** configured in either repo. Fractal's `pytest.ini` has a commented-out `timeout = 60` line, confirming intent but never enabled. Session-manager's `pyproject.toml` has no timeout setting.
- **No CI test pipeline** — fractal only has a `protect-main.yml` workflow that blocks PRs to main. Session-manager has no CI workflows at all. Neither repo runs tests automatically.

## Proposed Solution

A two-layer fix: **persona guidance** (immediate) + **pytest-timeout guard** (defense-in-depth).

### Layer 1: Update engineer persona test instructions

**File:** `.agent-os/personas/engineer.md` (separate copies in each repo, same content)

Change the test step from:

```
7. **Test** — `python -m pytest tests/ -v` (STOP if tests fail)
```

To:

```
7. **Test** — Run targeted tests first, then full suite if time permits:
   - Identify test files relevant to your changes (same module name, or grep for imports)
   - Run targeted: `python -m pytest tests/<path>/test_<relevant>.py -v` (or use `-k <pattern>` for cross-file filtering)
   - If targeted tests pass AND full suite fits in budget: `python -m pytest tests/ -q --timeout=120`
   - If full suite would exceed shell budget, skip it — note in PR that full suite was not run
   - STOP if any tests fail
```

And update the Task Completion Protocol similarly:

```
1. **Test**: Run relevant test file(s): `python -m pytest tests/<path>/test_<relevant>.py -v` (or `-k <pattern>`)
   - If tests fail: STOP. Fix failures before proceeding.
   - Full suite run is optional locally. Note in the PR if only targeted tests were run.
```

**Note:** Neither repo currently has CI test automation, so skipping the full suite locally means no automated regression check exists. This is an accepted tradeoff until CI is added — the alternative (agents stuck in retry loops) is worse.

### Layer 2: Add pytest-timeout to both repos

**fractal-market-simulator** — uses `pytest.ini` (not pyproject.toml). Uncomment and set the existing timeout line:

```ini
# In pytest.ini:
timeout = 30
```

Also add `pytest-timeout` to `requirements.txt`.

**session-manager** — uses `pyproject.toml`. Extend existing `[tool.pytest.ini_options]`:

```toml
[tool.pytest.ini_options]
asyncio_mode = "auto"
testpaths = ["tests"]
timeout = 30
```

Also add `pytest-timeout>=2.0` to `[project.optional-dependencies].dev`.

This sets a **per-test** timeout of 30 seconds. Any individual test taking longer than 30s is almost certainly hanging. Tests that legitimately need more time can use `@pytest.mark.timeout(120)` to override.

### Layer 3 (optional): Update EM dispatch checklist

In `personas/em.md`, change:

```
- Always: "Run tests when done."
```

To:

```
- Always: "Run targeted tests for your changes. Full suite is optional locally."
```

This matches the engineer persona change and prevents the EM from issuing contradictory "run all tests" instructions.

## What This Does NOT Change

- **No CI pipeline added.** That's a separate initiative (and the issue doesn't request it).
- **No changes to `sm wait` timeouts.** The 600s EM fallback timeout is for agent hangs, not test hangs.
- **No changes to Claude Code's Bash timeout.** That's an upstream setting we don't control.

## Test Plan

1. **Verify pytest-timeout works:** After adding `pytest-timeout` to both repos, run `python -m pytest tests/ -q` and confirm all tests still pass (none take >30s normally).
2. **Verify timeout catches hangs:** Create a throwaway test with `time.sleep(60)`, confirm it fails with a timeout error after 30s.
3. **Verify persona guidance:** Dispatch an engineer with the updated persona and observe whether they run targeted tests instead of the full suite.
4. **Regression check:** Ensure the `@pytest.mark.slow` mechanism in fractal's conftest.py still works correctly alongside `pytest-timeout`.

## Ticket Classification

Single ticket, but requires **Director** involvement for persona file edits (engineer persona explicitly forbids modifying persona files — see `engineer.md` line 170: "Modify persona files (escalate to Director)").

Recommended split:
- **Engineer** handles: pytest-timeout dependency + config in both repos (requirements.txt for fractal, pyproject.toml for SM)
- **Director** handles: persona file edits (engineer.md test instructions, em.md dispatch checklist) in both repos

No epic needed — two small changes that can be coordinated in one session.
