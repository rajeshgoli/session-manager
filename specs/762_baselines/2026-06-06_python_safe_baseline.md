# Python Safe Baseline - 2026-06-06

This is the first safe, read-only baseline for the Rust migration value gate.
It was generated with:

```bash
./venv/bin/python -m scripts.rust_migration.baseline \
  --base-url http://127.0.0.1:8420 \
  --repetitions 5 \
  --output /tmp/sm-python-baseline-safe.json
```

The raw machine-local JSON was not committed. This artifact keeps aggregate
numbers only.

## Environment

| Field | Value |
| --- | --- |
| Target | current Python service |
| Host OS | Darwin |
| Machine | arm64 |
| Python | 3.14.5 |
| Server PID | 3778 |
| RSS | 203920 KiB / 199.141 MiB |
| USS | unknown; the safe stdlib runner does not use platform-specific USS tooling |

## Safe Contract Result

| Result | Count |
| --- | ---: |
| Passed | 11 |
| Failed | 0 |
| Skipped | 1 |

Skipped check:

| Check | Reason |
| --- | --- |
| `http.mobile_session_stop` | session id not supplied; this is mutating and requires an explicit test session plus opt-in |

## Latency Summary

Five repetitions against the live local Python service:

| Check | Count | Min ms | Median ms | P95 ms | Max ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| `http.health` | 5 | 1.914 | 3.858 | 11.487 | 13.246 |
| `http.health_detailed` | 5 | 168.111 | 444.307 | 645.935 | 691.031 |
| `http.auth_session` | 5 | 1.045 | 1.232 | 53.694 | 66.707 |
| `http.client_bootstrap` | 5 | 0.830 | 0.870 | 0.926 | 0.938 |
| `http.client_sessions` | 5 | 201.032 | 206.959 | 231.113 | 231.235 |
| `http.sessions` | 5 | 62.043 | 67.372 | 123.851 | 133.677 |
| `http.events_state` | 5 | 1.052 | 1.352 | 53.733 | 66.066 |
| `cli.status_help` | 5 | 76.641 | 80.109 | 86.398 | 87.400 |
| `cli.send_help` | 5 | 72.990 | 76.096 | 81.040 | 81.555 |
| `cli.tail_raw_help` | 5 | 76.732 | 79.434 | 87.567 | 88.552 |
| `cli.retire_help` | 5 | 77.814 | 78.537 | 81.323 | 81.463 |

## Python Hardening Variants

These variants remain unmeasured in this safe pass because they require a
controlled config copy, restart rehearsal, or compatibility fixture comparison:

| Variant | Status | Reason |
| --- | --- | --- |
| disable already-unused integrations by config | not measured | requires controlled config copy and restart rehearsal |
| reduce retained event/log scan windows where compatible | not measured | requires retained-state workload and compatibility fixture comparison |
| defer startup background work not needed for first response | not measured | requires startup harness and controlled service restart |
| remove retired surfaces while isolating optional retained integrations | not measured | requires feature-gated Python patch or Rust prototype comparison |
| reduce logging verbosity or request timing thresholds where compatible | not measured | requires log/telemetry compatibility comparison |

## Remaining Baseline Work

- Capture USS with a platform-specific tool or approved dependency.
- Add mutating test-session workloads for mobile/session-stop, attach-ticket mint,
  `sm send`, queue wake delivery, and retained hook paths.
- Add startup/restore measurements from copied real state.
- Add provider/Codex event ingestion, node reconnect, SSE stream, and email/human
  fake-notifier baselines.
- Run the same report against feasible Python hardening/config variants.
