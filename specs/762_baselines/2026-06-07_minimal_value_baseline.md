# Minimal Value Baseline - 2026-06-07

Issue: #779

This is a minimal Rust migration value-gate baseline. It follows the owner
direction to avoid throwaway Python hardening/config variant work. The comparison
is current Python as operated versus the existing Rust read-only scaffold.

Raw machine-local JSON was written under `/tmp` and was not committed.

## Commands

Current Python attempt:

```bash
./venv/bin/python -m scripts.rust_migration.baseline \
  --target python \
  --base-url http://127.0.0.1:8420 \
  --repetitions 5 \
  --output /tmp/sm-python-min-baseline.json
```

Rust scaffold:

```bash
cargo run -p sm-server -- --port 8421 --config config.yaml

./venv/bin/python -m scripts.rust_migration.baseline \
  --target rust \
  --base-url http://127.0.0.1:8421 \
  --check-id http.health \
  --check-id http.health_detailed \
  --check-id http.auth_session \
  --check-id http.client_bootstrap \
  --check-id http.sessions \
  --check-id http.client_sessions \
  --check-id http.api_sessions_absent \
  --repetitions 5 \
  --output /tmp/sm-rust-min-baseline.json
```

## Python Hardening Waiver

Python hardening/config variant comparison is owner-waived for this cutover.
Python hardening is treated as throwaway work and should not block Rust migration
baseline or implementation tickets unless the owner later reopens that decision.

## Current Python Result

The live Python service did not complete the safe HTTP baseline. During the run,
the service stopped listening on `127.0.0.1:8420`; the baseline reported
connection-refused failures for 16 HTTP checks and 6 fixture-dependent skips.
CLI help checks still passed because they do not require the server.

Relevant daemon log evidence:

```text
2026-06-07 14:24:38,333 - __main__ - WARNING - Event loop did not respond within 10s
2026-06-07 14:24:38,334 - __main__ - ERROR - Event loop is frozen! Killing process for restart...
```

After launchd restarted the service, startup/restore work continued for minutes
and the daemon was still not listening on port 8420 when rechecked.

This means the current Python service is unstable under the retained real-state
environment used for the baseline attempt. The last successful committed safe
Python reference remains
`specs/762_baselines/2026-06-06_python_safe_baseline.md`, where the Python
service measured approximately 200.7 MiB RSS and completed 25 safe checks with
0 failures.

## Rust Scaffold Result

The Rust read-only scaffold completed the comparable implemented subset with
0 failures and 0 skips.

| Metric | Value |
| --- | ---: |
| Server PID | 65861 |
| RSS | 8.062 MiB |
| macOS physical footprint | 3.969 MiB |
| USS | unknown; not available from stdlib-safe runner |
| Elapsed baseline run | 0.599s |

Latency summary:

| Check | Count | Median ms | P95 ms | Max ms |
| --- | ---: | ---: | ---: | ---: |
| `http.health` | 5 | 0.248 | 8.500 | 10.563 |
| `http.health_detailed` | 5 | 0.253 | 0.447 | 0.481 |
| `http.auth_session` | 5 | 0.274 | 0.317 | 0.322 |
| `http.client_bootstrap` | 5 | 0.287 | 0.304 | 0.306 |
| `http.sessions` | 5 | 4.273 | 4.279 | 4.280 |
| `http.client_sessions` | 5 | 4.439 | 4.554 | 4.575 |
| `http.api_sessions_absent` | 5 | 0.274 | 0.302 | 0.304 |

## Interpretation

This is not a full replacement benchmark; the Rust server is still a read-only
scaffold. It is enough to justify continuing the Rust port:

- The scaffold is far smaller than the last successful Python RSS reference.
- The scaffold passes the implemented retained read-only subset.
- The current Python daemon showed event-loop watchdog restarts during the
  value-gate attempt, which strengthens the responsiveness/reliability case for
  moving the retained core out of the current Python runtime.

Do not spend Rust migration time on Python hardening variants. The next useful
work is more executable contract capture for retained high-priority surfaces,
especially native mobile/auth/public-edge proof and session lifecycle behavior.
