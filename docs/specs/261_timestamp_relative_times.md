# sm#261: Fix negative relative timestamps in parent wake digest

**Issue**: https://github.com/rajeshgoli/session-manager/issues/261
**Observed**: `(-475m ago)` in "Recent activity" section of periodic parent wake messages
**Expected**: `(3m ago)` or similar small positive value

---

## Root Cause

**File**: `src/message_queue.py`
**Function**: `_assemble_parent_wake_digest` (line 1759)

```python
# BUG: datetime.now() returns naive LOCAL time
now = datetime.now()
for event in tool_events:
    ts_str = event.get("timestamp", "")
    ts = datetime.fromisoformat(ts_str.replace(" ", "T")) if ts_str else None
    age = f" ({int((now - ts).total_seconds() / 60)}m ago)" if ts else ""
```

SQLite's `CURRENT_TIMESTAMP` (used in the `tool_usage` table's `DEFAULT CURRENT_TIMESTAMP`) returns **UTC naive** strings (e.g. `"2026-02-20 01:12:11"`). On a machine in UTC-8 (PST), `datetime.now()` returns a value 8 hours *behind* the stored UTC timestamp. Subtracting: `LOCAL - UTC = -8h = -480 min`.

Verified with trace:
```
SQLite CURRENT_TIMESTAMP: 2026-02-20 01:12:11  (UTC)
datetime.now() (local):   2026-02-19 17:12:11  (UTC-8)
datetime.now() - ts = -28799.6s = -480.0m      ← BUG

datetime.utcnow() - ts = 0.4s = ~0m            ← CORRECT
```

The `int()` cast does not clamp to zero, so negative minutes propagate directly to output: `-475m ago`.

### Why -475 and not -480?

The tool was called ~5 minutes earlier in the session, so `UTC_now - tool_ts ≈ 5 min`. With the bug: `LOCAL_now - tool_ts = -480 + 5 = -475 min`.

---

## Scope: sm tail is NOT affected

`cmd_tail` in `src/cli/commands.py` (added in commit `05564c8`) was deliberately written with `datetime.utcnow()`:

```python
now_utc = datetime.utcnow()  # correctly matches SQLite UTC timestamps
```

The commit message explicitly notes: *"Relative time computed inline with datetime.utcnow() to correctly compare against UTC DB timestamps."*

The issue title mentions "sm tail" but the observed `-475m ago` output format (parentheses, `m ago` suffix) matches only the `_assemble_parent_wake_digest` code path. `sm tail` uses square-bracket format `[{elapsed} ago]`.

---

## Other datetime.now() calls in the same function — NOT affected

Lines 1721, 1733 in `_assemble_parent_wake_digest` also use `datetime.now()`:

```python
age_secs = (datetime.now() - child_session.agent_status_at).total_seconds()
elapsed_secs = (datetime.now() - reg.registered_at).total_seconds()
```

These compare against `agent_status_at` (set by `server.py:2243` via `datetime.now()`) and `reg.registered_at` (set via `datetime.now()`). Both are local time → `LOCAL - LOCAL = correct`. No bug here.

---

## Existing Tests Miss the Bug

`tests/unit/test_parent_wake.py` (line 411-412) passes timestamps as `datetime.now().isoformat()`:

```python
{"tool_name": "Bash", "timestamp": datetime.now().isoformat()},
```

This masks the bug because the test uses local time for both the tool timestamp and the `now` reference. The fix must also add a test that uses UTC timestamps (matching real SQLite output).

---

## Implementation Approach

### Single-line fix in `_assemble_parent_wake_digest`

**File**: `src/message_queue.py`, line 1759

Change:
```python
now = datetime.now()
```
To:
```python
from datetime import timezone
now = datetime.now(timezone.utc).replace(tzinfo=None)
```

`datetime.utcnow()` would also work and matches the pattern in `cmd_tail`, but it is deprecated in Python 3.12+ (deprecation warning confirmed on Python 3.14.2 in this project). The `datetime.now(timezone.utc).replace(tzinfo=None)` idiom produces an identical naive UTC datetime without triggering the deprecation warning.

Note: the `from datetime import timezone` import should be added to the existing `from datetime import datetime` import at the top of `message_queue.py`.

### Optional: Consistency fix in cmd_tail

`src/cli/commands.py`'s `_rel()` uses `datetime.utcnow()` which is also deprecated. A separate follow-up could update it to `datetime.now(timezone.utc).replace(tzinfo=None)` for consistency, but it is outside the scope of this fix.

---

## Test Plan

### Failing test (add to `tests/unit/test_parent_wake.py`)

The test must be **deterministic across all host timezones** (including UTC, where the old buggy code also produces a non-negative result). The approach: patch `datetime` in `src.message_queue` with a subclass that hard-codes UTC-8 behavior for `now()` while still returning correct UTC for `now(timezone.utc)`.

```python
@pytest.mark.asyncio
async def test_digest_tool_timestamps_use_utc():
    """Recent activity ages must be positive regardless of host timezone.

    SQLite CURRENT_TIMESTAMP is UTC (naive). The digest must compare against
    UTC now — not local now — or results are negative on UTC-behind systems.

    The test is deterministic: it patches datetime.now in src.message_queue to
    simulate a UTC-8 machine regardless of actual host timezone.
    """
    import re
    from datetime import datetime, timedelta, timezone
    from unittest.mock import patch

    # Fixed reference point
    UTC_NOW = datetime(2026, 2, 20, 10, 0, 0)           # naive UTC
    UTC_8_NOW = UTC_NOW - timedelta(hours=8)             # naive "local" on UTC-8
    TOOL_TS = (UTC_NOW - timedelta(minutes=2)).strftime("%Y-%m-%d %H:%M:%S")  # SQLite format

    # Subclass that makes .now() behave like a UTC-8 machine
    class _FakeDatetime(datetime):
        @classmethod
        def now(cls, tz=None):
            if tz is None:
                return UTC_8_NOW          # simulates datetime.now() on UTC-8
            if tz == timezone.utc:
                return UTC_NOW.replace(tzinfo=timezone.utc)  # aware UTC
            return datetime.now(tz)       # fallback for other tzinfos

        @classmethod
        def fromisoformat(cls, s):
            return datetime.fromisoformat(s)  # delegate to real datetime

    mq = _make_mq()
    reg = ParentWakeRegistration(
        id="r1",
        child_session_id="child_tz",
        parent_session_id="parent_tz",
        period_seconds=600,
        registered_at=UTC_8_NOW - timedelta(minutes=5),
        last_wake_at=None,
        last_status_at_prev_wake=None,
        escalated=False,
        is_active=True,
    )
    mq._parent_wake_registrations["child_tz"] = reg

    child_session = MagicMock()
    child_session.friendly_name = "tz-test"
    child_session.agent_status_text = None
    child_session.agent_status_at = None
    mq.session_manager.get_session.return_value = child_session

    tool_events = [
        {"tool_name": "Bash", "target_file": None,
         "bash_command": "pytest tests/", "timestamp": TOOL_TS},
    ]

    with patch("src.message_queue.datetime", _FakeDatetime), \
         patch.object(mq, "_read_child_tail", return_value=tool_events):
        digest = await mq._assemble_parent_wake_digest("child_tz", reg)

    # Old code: datetime.now() → UTC_8_NOW = UTC - 8h → age = -478m  (FAIL)
    # New code: datetime.now(timezone.utc).replace(tzinfo=None) → UTC_NOW → age = 2m (PASS)
    match = re.search(r'\((-?\d+)m ago\)', digest)
    assert match, f"Expected '(Nm ago)' in digest:\n{digest}"
    age_minutes = int(match.group(1))

    assert age_minutes >= 0, (
        f"Age is negative ({age_minutes}m) — datetime.now() used instead of UTC now"
    )
    assert age_minutes <= 5, f"Age unexpectedly large: {age_minutes}m"
```

**Why this is deterministic**: `_FakeDatetime.now()` always returns `UTC_8_NOW` (UTC-8) regardless of host. On old (buggy) code, `age = int((UTC_8_NOW - UTC_2MIN_AGO_UTC).total_seconds() / 60) = -478` → assertion fails. On fixed code, `age = int((UTC_NOW - UTC_2MIN_AGO_UTC).total_seconds() / 60) = 2` → assertion passes. A UTC host does not change this because the mock overrides `datetime.now` at the module level.

### Manual verification

After fix, trigger a parent wake digest (via `sm dispatch`) on a non-UTC machine and confirm the "Recent activity:" section shows `(2m ago)` style values, not negative.

---

## Classification

**Single ticket** — one-line fix in `message_queue.py` plus one test in `test_parent_wake.py`. Well within single-agent scope.
