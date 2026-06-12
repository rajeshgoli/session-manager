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

For broad live-state checks with a real session id, skip synthetic fixture
contracts so fixture-specific names/output do not create false failures:

```bash
python -m scripts.rust_migration.contracts \
  --target rust \
  --base-url http://127.0.0.1:8421 \
  --session-id <real-session-id> \
  --skip-fixture-checks
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

For `--target rust`, CLI checks default to `target/debug/sm` so retired or
Rust-only CLI contracts do not accidentally exercise the Python `sm` on PATH.
Pass `--sm-binary <path>` when testing another Rust CLI build.

Audit the retained/retired CLI cutover scope without running commands:

```bash
python -m scripts.rust_migration.cli_cutover_audit --fail-on-gaps
```

This verifies that `contracts_manifest.json` still contains the owner-approved
retained Python/Rust CLI help checks, retained Rust-only mobile device checks,
and retired Rust-only checks with the expected target, classification, safety,
and command tuple. It is a manifest-quality gate; use `contracts` for runtime
CLI execution.

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

## State Ownership Preflight

Before implementing freeze/final-backup/rollback, generate a non-mutating view
of the must-preserve stores the cutover tool will need to account for:

```bash
python -m scripts.rust_migration.state_preflight --config config.yaml --fail-on-blockers
```

The report resolves retained store paths from config and documented defaults,
including server config, shared client config, session state, message queue,
response relay, tool audit, Codex events/requests/observability, queue-runner
state, bug reports, app artifacts, Telegram archive data, email bridge config,
and logs. It records existence, kind, readability/copyability, file hashes, and
directory sizes without writing to live state. Missing optional stores are
warnings; missing required session state or unreadable/wrong-kind paths are
blockers for cutover tooling.

Plan a copyable backup manifest without writing:

```bash
python -m scripts.rust_migration.state_backup \
  --config config.yaml \
  --output-dir /tmp/sm-rust-state-backup-plan
```

Copy the planned stores only after reviewing the dry-run output:

```bash
python -m scripts.rust_migration.state_backup \
  --config config.yaml \
  --output-dir /tmp/sm-rust-state-backup-$(date -u +%Y%m%dT%H%M%SZ) \
  --execute \
  --fail-on-blockers
```

Execution creates a fresh output directory, copies existing copyable stores, and
writes `state-backup-manifest.json` inside it. Missing optional stores remain
warnings. Preflight blockers, top-level symlink stores, and pre-existing backup
roots block execution. Directory copies do not follow symlink children.

Verify an executed backup manifest without writing:

```bash
python -m scripts.rust_migration.state_restore \
  --manifest /tmp/sm-rust-state-backup-20260612T000000Z/state-backup-manifest.json \
  --fail-on-blockers
```

Rehearse restore only into a fresh disposable root after reviewing verification
output:

```bash
python -m scripts.rust_migration.state_restore \
  --manifest /tmp/sm-rust-state-backup-20260612T000000Z/state-backup-manifest.json \
  --restore-dir /tmp/sm-rust-state-restore-rehearsal-$(date -u +%Y%m%dT%H%M%SZ) \
  --execute-restore \
  --fail-on-blockers
```

Restore rehearsal copies backup contents under `stores/<store_id>` in the
restore root and writes `state-restore-report.json`. It never restores into
live Session Manager paths. Existing restore roots, symlinks, unsafe roots, and
restore roots nested inside the backup or source stores block execution.

Plan the write-freeze and active-writer drain coverage without activating a
freeze:

```bash
python -m scripts.rust_migration.freeze_drain_plan \
  --config config.yaml \
  --fail-on-blockers
```

The report maps Stage 5 writer families to the stores they protect, the current
evidence source, and the freeze/drain action that must exist before final
backup. It explicitly reports `freeze_active=false` and
`rust_ownership_active=false`; it is planning evidence only.

To append a plan-only ledger entry, provide an explicit ledger path:

```bash
python -m scripts.rust_migration.freeze_drain_plan \
  --config config.yaml \
  --record-plan \
  --ledger /tmp/sm-rust-migration-ledger.jsonl \
  --fail-on-blockers
```

Ledger writes are rejected when the output path is a directory, symlink, or has
a missing/non-directory parent. The entry records the planned coverage only; it
does not claim write admission is frozen, writers are drained, or Rust owns any
store.

## Shadow Comparison Mode

Shadow mode lets Python stay authoritative while Rust observes bounded
request/response envelopes and reports whether it would match, mismatch, or
currently lacks a side-effect-free implementation. It is disabled by default.

Start Rust on the shadow port:

```bash
cargo run -p sm-server -- \
  --port 8421 \
  --config scripts/rust_migration/fixtures/read_only/config.yaml
```

Enable Python shadowing in local config only:

```yaml
rust_shadow:
  enabled: true
  endpoint: "http://127.0.0.1:8421/__shadow/http"
  # Required when Rust is not reached over loopback, optional for localhost.
  secret: "local-dev-shared-secret"
  ledger_path: "~/.local/share/claude-sessions/rust_shadow.jsonl"
  timeout_seconds: 0.5
  max_body_bytes: 65536
```

Generate the non-destructive local plan before touching the running service:

```bash
./venv/bin/python -m scripts.rust_migration.shadow_observation \
  --config config.yaml \
  --ledger ~/.local/share/claude-sessions/rust_shadow.jsonl \
  --report-last-minutes 60 \
  --report-min-rows 1000 \
  --mobile-sm-app-profile \
  --report-require-route 'GET /sessions' \
  --report-require-route 'GET /events/state' \
  --report-min-route-rows 'GET /sessions=100' \
  --report-min-route-rows 'GET /events/state=100' \
  --fail-on-blockers
```

The planner checks that the Rust sidecar config and `cargo` are available,
warns when Python is not healthy, blocks a fresh sidecar plan if the Rust port
is already healthy, and prints the exact sidecar command, local Python
`rust_shadow` config snippet, and ledger report command. It never edits
`config.yaml`, restarts Session Manager, or starts a process. Use
`--reuse-rust-sidecar` when intentionally observing against an already-running
Rust sidecar. `--report-*` options are copied into the generated
`shadow_report` command so the operator-reviewed plan and the final observation
gate use the same window and coverage thresholds. `--mobile-sm-app-profile`
adds native app read coverage gates for `/auth/session`, `/client/bootstrap`,
`/client/sessions`, `/client/analytics/summary`, `/client/sessions/{id}`, and
`/sessions/{id}/attach-descriptor`; exercise the sm app during that observation
window or the gate should block.

After reviewing the planner output, prepare the local Python config with the
dry-run-first activation helper:

```bash
./venv/bin/python -m scripts.rust_migration.shadow_config \
  --config config.yaml \
  --endpoint http://127.0.0.1:8421/__shadow/http \
  --ledger ~/.local/share/claude-sessions/rust_shadow.jsonl
```

By default this only prints a unified diff. Re-run with `--write` after
reviewing the diff to replace or append only the top-level `rust_shadow`
section. A timestamped `.shadow-backup-*` copy is created before any write.
The helper does not restart Session Manager or start Rust; restart Python
deliberately after accepting the local config change.

The ledger is JSONL. Each row includes method/path, redacted query metadata,
Python status and body hash, Rust comparison result, Rust support status,
latency, and any shadow transport error. Raw request and response bodies are not
forwarded or written to the ledger. Cookies, bearer tokens, worker secrets, hook
secrets, device signatures, and sensitive query values such as OAuth
`code`/`state` are omitted or redacted before the Rust envelope and ledger are
written. Shadow mode preserves only query values currently needed for
side-effect-free comparisons, such as `/sessions?include_stopped=...` and
`/sessions/{id}/output?lines=...`.

The Rust shadow endpoint accepts loopback callers by default only when no
`rust_shadow.secret` is configured. If Rust is behind a proxy, tunnel, or bound
on a non-local interface, configure the same `rust_shadow.secret` for Python and
Rust. Once configured, the secret is required even from loopback so local proxies
cannot bypass it.

Current Rust shadow support is intentionally side-effect-free:

- stable read-only routes can return `comparison: "match"` or a mismatch.
- volatile reads such as detailed health may return `status_match`.
- retained writes such as session create/input/kill return
  `support_status: "unsupported_retained_write"` and `would_write: false` until
  native Rust runtime ownership is implemented.

For a cutover trial, run shadow mode for the agreed observation window and treat
unexplained mismatches on retained core surfaces as blockers before Rust becomes
the writer.

Summarize the ledger during or after the observation window:

```bash
./venv/bin/python -m scripts.rust_migration.shadow_report \
  --ledger ~/.local/share/claude-sessions/rust_shadow.jsonl \
  --fail-on-blockers
```

Use `--json` to produce automation-friendly output with route summaries,
comparison counts, support-status counts, and a blocker list. Blockers include
status/body mismatches, shadow transport errors, non-JSON shadow responses, and
invalid ledger rows. `not_compared` rows are summarized but are not blockers by
default because retained writes may be intentionally observed without Rust side
effects before cutover.

For a bounded observation window, filter valid rows by timestamp:

```bash
./venv/bin/python -m scripts.rust_migration.shadow_report \
  --ledger ~/.local/share/claude-sessions/rust_shadow.jsonl \
  --last-minutes 60 \
  --fail-on-blockers
```

Use `--since 2026-06-12T02:00:00Z` when the window start is known exactly.
Invalid JSON rows, invalid row shapes, and rows with missing or malformed
`observed_at` timestamps remain blockers under a filter because they cannot be
assigned safely to an observation window.

For an automation-grade observation gate, require both total volume and route
coverage. Quote route arguments because they contain spaces:

```bash
./venv/bin/python -m scripts.rust_migration.shadow_report \
  --ledger ~/.local/share/claude-sessions/rust_shadow.jsonl \
  --last-minutes 60 \
  --min-rows 1000 \
  --require-route 'GET /sessions' \
  --require-route 'GET /events/state' \
  --require-route 'GET /client/analytics/summary' \
  --require-route-pattern 'GET /client/sessions/*' \
  --require-route-pattern 'GET /sessions/*/attach-descriptor' \
  --min-route-rows 'GET /sessions=100' \
  --min-route-rows 'GET /events/state=100' \
  --fail-on-blockers
```

Coverage gate failures are reported as blockers (`insufficient_rows`,
`missing_required_route`, `insufficient_route_rows`,
`missing_required_route_pattern`, or `insufficient_route_pattern_rows`) so the
same `--fail-on-blockers` automation path handles mismatches and insufficient
observation evidence.

Passive shadow observation is read-only. Native app side-effect flows such as
attach-ticket minting, request-status prompts, bug-report submission, and device
revocation/list changes should be covered by disposable fixture or smoke
rehearsal gates, not by executing Rust against live state from shadow mode.

## MVP Sidecar Rehearsal

The MVP rehearsal gate starts Rust as a sidecar, keeps Python authoritative, and
produces a single JSON report with pass/fail steps, blockers, shadow-read
comparisons, and baseline artifacts. It is the quickest way to answer whether
the current Rust core is cutover-shaped enough for the next MVP slice.

Live rehearsal against the current local config:

```bash
python -m scripts.rust_migration.mvp_rehearsal
```

By default this:

- checks Python `/health` on `http://127.0.0.1:8420`;
- runs the state ownership gate: preflight, backup copy, backup verification,
  disposable restore rehearsal, and freeze/drain plan ledger under the report
  directory;
- starts Rust `sm-server` on `http://127.0.0.1:8421` with `config.yaml`;
- runs `scripts/rust-mvp-smoke.sh` for isolated mutating runtime coverage;
- runs core read/retired-surface contracts against Rust;
- starts a separate Rust sidecar with
  `scripts/rust_migration/fixtures/read_only/config.yaml` and runs synthetic
  read-only fixture contracts for stable session, Codex, app artifact, and queue
  read surfaces;
- probes retained MVP gaps when any remain;
- posts shadow-style read comparisons to Rust `POST /__shadow/http`;
- writes Python and Rust baseline JSON under the report directory.

The rehearsal uses exact body hashes only for stable sidecar-readable payloads.
Live session list projections such as `/sessions` and `/client/sessions` include
runtime-derived activity and attach metadata that the read-only sidecar does not
own yet. `/nodes` includes live node-agent connectivity that the sidecar does
not own yet. `/client/bootstrap` can advertise Python-only mobile-terminal
attach support until Rust owns the terminal/ticket routes. The shadow summary
treats those paths as status-only until Rust owns the corresponding runtime
state. The active rehearsal shadow summary also probes
`/client/analytics/summary` and, when Python has at least one live session,
`/client/sessions/{id}` plus `/sessions/{id}/attach-descriptor`, so native app
read prediction is exercised before a longer passive mobile observation window
is collected.

Reports are written under `.local/rust-mvp-rehearsals/<timestamp>/` unless
`--output-dir` is supplied. `.local/` is ignored; do not commit host-local
reports. The state gate writes detailed artifacts under
`<report-dir>/state-gate/`, including `state-backup-manifest.json`,
`state-restore-report.json`, and `freeze-drain-ledger.jsonl`. If this gate
blocks, the rehearsal exits before Rust sidecar startup because the run cannot
serve as cutover evidence.

Useful variants:

```bash
# Reuse an already-running Rust sidecar.
python -m scripts.rust_migration.mvp_rehearsal --reuse-rust-sidecar

# Validate live core reads plus the synthetic read-only fixture without slower gates.
python -m scripts.rust_migration.mvp_rehearsal \
  --skip-python-health \
  --skip-state-gate \
  --skip-smoke \
  --skip-baseline \
  --skip-shadow \
  --skip-mutating-contracts \
  --reuse-rust-sidecar

# Start fresh sidecars for the core and read-only fixture checks only.
python -m scripts.rust_migration.mvp_rehearsal \
  --skip-python-health \
  --skip-state-gate \
  --skip-smoke \
  --skip-baseline \
  --skip-shadow \
  --skip-mutating-contracts \
  --core-only

# Produce a report even when known MVP gaps remain.
python -m scripts.rust_migration.mvp_rehearsal --allow-blockers
```

Exit code is non-zero when blockers are present unless `--allow-blockers` is
used. A blocked report is expected while retained MVP gap probes are still
unimplemented; use the blocker list to choose the next Rust slice.
