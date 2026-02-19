# sm#209 — Fix test timeout in test_monitor_loop_gives_up_after_max_retries

## Problem

`test_monitor_loop_gives_up_after_max_retries` in
`tests/regression/test_issue_40_error_swallowing.py` reliably times out under
pytest-timeout=30s.

## Root Cause

`_monitor_loop()` (`src/message_queue.py:756`) calls `asyncio.sleep(retry_delay)`
between retries with no mock in the test. With `initial_retry_delay=1.0` and
exponential doubling, 5 retries consume:

```
1s + 2s + 4s + 8s + 16s = 31s real time
```

The test works around this by sleeping 35 seconds (`await asyncio.sleep(35)`) to
let retries exhaust naturally. That 35s wait exceeds the 30s pytest-timeout cap,
so the test is killed before the assertions run.

## Fix

Pass `config={"timeouts": {"message_queue": {"initial_retry_delay_seconds": 0.001}}}`
when constructing `MessageQueueManager` in the test so that the five real
retries complete in ~5 ms instead of 31 s. Then replace the 35 s wait with a
short polling loop using a real `asyncio.sleep`.

This approach is consistent with the existing pattern already used in this test
file (e.g. the retry-reset test uses tiny delays in config).

**Why not `patch("src.message_queue.asyncio.sleep")`**: that target patches the
`asyncio` module object's `sleep` attribute in place, so the test's own
`await asyncio.sleep(...)` poll calls are also patched. The monitor loop never
gets real event-loop yields, so `_monitor_loop` cannot progress to the give-up
state.

### Diff sketch — `tests/regression/test_issue_40_error_swallowing.py`

```python
# Before:
queue_mgr = MessageQueueManager(
    session_manager=mock_session_manager,
    db_path=str(tmp_path / "test.db"),
    config={"input_poll_interval": 0.05},
)
# ...
with caplog.at_level("ERROR"):
    await asyncio.sleep(35)   # real backoffs: 1+2+4+8+16 = 31s

# After:
queue_mgr = MessageQueueManager(
    session_manager=mock_session_manager,
    db_path=str(tmp_path / "test.db"),
    config={"timeouts": {"message_queue": {"initial_retry_delay_seconds": 0.001}}},
)
# ...
with caplog.at_level("ERROR"):
    # real retries complete in ~5ms; poll until loop gives up
    for _ in range(100):
        if "giving up" in caplog.text:
            break
        await asyncio.sleep(0.05)
```

`initial_retry_delay` is read from
`config["timeouts"]["message_queue"]["initial_retry_delay_seconds"]`
(see `message_queue.py:72`); it is not a direct constructor argument.

## Files

| File | Change |
|------|--------|
| `tests/regression/test_issue_40_error_swallowing.py` | Pass tiny `initial_retry_delay_seconds` via config; replace `await asyncio.sleep(35)` with short polling loop |

No production code changes required.

## Test Plan

1. Run the test before the fix and confirm it times out (or takes >30s).
2. Apply the fix.
3. Run the test — it must complete well within the 30s timeout.
4. Confirm both log assertions still pass:
   - `"failed 5 times, giving up"` in `caplog.text`
   - `"monitoring STOPPED"` in `caplog.text`
5. Run the full regression suite (`pytest tests/`) and confirm no new failures.

## Classification

Single ticket. One targeted test change, no production impact.
