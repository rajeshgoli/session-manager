# Rust Migration Harness

These scripts are implementation artifacts for issue #762. They do not reopen
the migration spec; they turn the converged spec artifacts into executable
checks and measurable Python baselines.

## Contract Harness

Run the safe Python subset:

```bash
python -m scripts.rust_migration.contracts --target python --base-url http://127.0.0.1:8420
```

The harness is manifest driven. Checks that need a live server, a real session,
credentials, or mutating opt-in are reported as skipped when their preconditions
are not supplied. Retired surfaces are Rust-target checks; current Python may
still expose them during the migration window.

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

Run the first Rust read-only server slice:

```bash
cargo run -p sm-server -- --port 8421 --config /tmp/sm-rust-missing-config.yaml
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
  --check-id http.sessions \
  --check-id http.client_sessions \
  --check-id http.api_sessions_absent
```

Omit `--config` to load `config.yaml`; the Rust scaffold also applies the same
default `.local/android-parity/values.env` overlay for the auth/bootstrap fields
that the Python server uses.

## Baseline Runner

Run a safe local baseline:

```bash
python -m scripts.rust_migration.baseline --base-url http://127.0.0.1:8420 --repetitions 5 --output /tmp/sm-python-baseline.json
```

The report records numeric data where it is safe to measure and marks missing
instrumentation or unsafe workloads explicitly. Do not commit machine-local
baseline reports if they contain host-specific or private runtime details.
