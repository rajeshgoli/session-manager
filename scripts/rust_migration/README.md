# Rust Migration Harness

These scripts are implementation artifacts for issue #762. They do not reopen
the migration spec; they turn the converged spec artifacts into executable
checks and minimal value-gate baselines.

## Contract Harness

Run the safe Python subset:

```bash
python -m scripts.rust_migration.contracts --target python --base-url http://127.0.0.1:8420
```

The harness is manifest driven. Checks that need a live server, a real session,
credentials, or mutating opt-in are reported as skipped when their preconditions
are not supplied. Retired surfaces are Rust-target checks; current Python may
still expose them during the migration window.

HTTP checks can assert more than status codes. Manifest rows may include:

- `request_headers` for host/proof/auth-boundary fixtures.
- `expected_body_contains_any` / `expected_body_contains_all` for text or SSE frames.
- `expected_json` JSON-pointer assertions with `type`, `equals`, `contains`, or
  `absent` checks for response-shape contracts.

Fixture values are supplied explicitly:

```bash
python -m scripts.rust_migration.contracts \
  --target python \
  --base-url http://127.0.0.1:8420 \
  --session-id <disposable-session-id> \
  --fixture app_name=session-manager-android \
  --fixture codex_app_session_id=<codex-app-session-id>
```

Mutating checks never run unless `--include-mutating` is present. Use only
disposable fixture sessions for checks such as retained mobile/API session stop.

Run the first Rust read-only server slice with the synthetic fixture config:

```bash
cargo run -p sm-server -- \
  --port 8421 \
  --config scripts/rust_migration/fixtures/read_only/config.yaml
```

Then, in another shell, run only the implemented Rust scaffold checks:

```bash
python -m scripts.rust_migration.contracts \
  --target rust \
  --base-url http://127.0.0.1:8421 \
  --check-id http.health \
  --check-id http.health_detailed \
  --check-id http.auth_session \
  --check-id http.client_bootstrap \
  --check-id http.events_state \
  --check-id http.events_sse_hello \
  --check-id http.sessions \
  --check-id http.client_sessions \
  --check-id http.api_sessions_absent \
  --check-id http.public_watch_operational_data_denied \
  --check-id http.scheduler_remind_retired \
  --check-id http.job_watches_retired \
  --check-id http.queue_policy_runs_retired
```

Fixture-backed checks assert concrete session projection and output behavior:

```bash
python -m scripts.rust_migration.contracts \
  --target rust \
  --base-url http://127.0.0.1:8421 \
  --session-id fixture001 \
  --check-id http.session_detail_fixture \
  --check-id http.client_session_detail_fixture \
  --check-id http.session_output_fixture \
  --check-id http.summary_provider_route_retired
```

Summary-route retirement must be checked with a real fixture session. The
harness first verifies `GET /sessions/{session_id}` returns 200 so a stale
fixture or resource-level 404 cannot masquerade as route retirement.

Omit `--config` to load `config.yaml`; the Rust scaffold also applies the same
default `.local/android-parity/values.env` overlay for the auth/bootstrap fields
that the Python server uses.

## Baseline Runner

Run a safe local Python baseline:

```bash
python -m scripts.rust_migration.baseline \
  --target python \
  --base-url http://127.0.0.1:8420 \
  --repetitions 5 \
  --output /tmp/sm-python-baseline.json
```

Run the comparable Rust scaffold subset while `sm-server` is listening:

```bash
python -m scripts.rust_migration.baseline \
  --target rust \
  --base-url http://127.0.0.1:8421 \
  --check-id http.health \
  --check-id http.health_detailed \
  --check-id http.auth_session \
  --check-id http.client_bootstrap \
  --check-id http.events_state \
  --check-id http.events_sse_hello \
  --check-id http.sessions \
  --check-id http.client_sessions \
  --check-id http.api_sessions_absent \
  --repetitions 5 \
  --output /tmp/sm-rust-baseline.json
```

The report records numeric data where it is safe to measure and marks missing
instrumentation or unsafe workloads explicitly. Python hardening/config variants
are owner-waived for the cutover value gate; compare current Python against the
Rust scaffold/prototype instead. Do not commit machine-local baseline reports if
they contain host-specific or private runtime details.
