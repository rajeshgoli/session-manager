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

## Baseline Runner

Run a safe local baseline:

```bash
python -m scripts.rust_migration.baseline --base-url http://127.0.0.1:8420 --repetitions 5 --output /tmp/sm-python-baseline.json
```

The report records numeric data where it is safe to measure and marks missing
instrumentation or unsafe workloads explicitly. Do not commit machine-local
baseline reports if they contain host-specific or private runtime details.

