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
| RSS | 205536 KiB / 200.719 MiB |
| USS | unknown; the safe stdlib runner does not use platform-specific USS tooling |

## Safe Contract Result

| Result | Count |
| --- | ---: |
| Passed | 25 |
| Failed | 0 |
| Skipped | 6 |

Skipped check:

| Check | Reason |
| --- | --- |
| `http.app_artifact_metadata` | fixture not supplied: `app_name` |
| `http.codex_pending_requests` | fixture not supplied: `codex_app_session_id` |
| `http.mobile_session_stop` | session id not supplied; this is mutating and requires an explicit test session plus opt-in |
| `http.session_output` | session id not supplied |
| `http.session_tool_calls` | session id not supplied |
| `http.tmux_client_hook_valid_event` | fixture not supplied: `tmux_session`; this is mutating and requires explicit opt-in |

## Latency Summary

Five repetitions against the live local Python service:

| Check | Count | Min ms | Median ms | P95 ms | Max ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| `http.health` | 5 | 3.117 | 5.908 | 39.954 | 45.969 |
| `http.health_detailed` | 5 | 130.414 | 237.996 | 357.051 | 368.909 |
| `http.auth_session` | 5 | 0.981 | 3.500 | 5.943 | 6.174 |
| `http.client_bootstrap` | 5 | 0.797 | 0.974 | 3.226 | 3.589 |
| `http.client_sessions` | 5 | 209.461 | 221.291 | 239.823 | 241.092 |
| `http.sessions` | 5 | 58.041 | 63.177 | 66.800 | 67.203 |
| `http.events_state` | 5 | 1.011 | 1.067 | 1.298 | 1.352 |
| `http.events_sse_hello` | 5 | 1.004 | 1.090 | 1.238 | 1.270 |
| `http.client_analytics_summary` | 5 | 59.427 | 62.858 | 110.050 | 121.542 |
| `http.nodes_list` | 5 | 1.067 | 1.119 | 51.897 | 64.577 |
| `http.queue_jobs_list` | 5 | 0.891 | 0.942 | 2.527 | 2.911 |
| `http.codex_review_requests_list` | 5 | 0.819 | 0.922 | 0.976 | 0.979 |
| `http.api_sessions_absent` | 5 | 0.746 | 0.772 | 0.875 | 0.893 |
| `http.email_inbound_validation_failure` | 5 | 1.016 | 1.033 | 1.154 | 1.177 |
| `http.device_google_validation_failure` | 5 | 0.828 | 0.919 | 1.910 | 2.135 |
| `http.tmux_client_hook_unsupported_event` | 5 | 0.764 | 0.862 | 0.900 | 0.904 |
| `cli.status_help` | 5 | 70.318 | 70.885 | 77.326 | 78.049 |
| `cli.send_help` | 5 | 66.711 | 70.513 | 76.797 | 77.480 |
| `cli.tail_raw_help` | 5 | 68.961 | 76.760 | 80.294 | 80.787 |
| `cli.retire_help` | 5 | 69.647 | 73.353 | 81.443 | 82.054 |
| `cli.email_help` | 5 | 68.297 | 74.570 | 83.218 | 84.642 |
| `cli.queue_list_help` | 5 | 68.879 | 72.114 | 76.429 | 76.438 |
| `cli.request_codex_review_help` | 5 | 70.181 | 74.817 | 83.003 | 85.033 |
| `cli.codex_help` | 5 | 70.331 | 76.448 | 76.976 | 77.086 |
| `cli.watch_help` | 5 | 69.894 | 73.144 | 76.229 | 76.777 |

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
