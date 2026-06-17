# Rust Service Cutover Runbook

Status: issue #1040 implementation handoff.

This runbook is the first reviewed path for rotating production service
ownership from Python to Rust when Python-origin availability is the blocker.
It does not silently skip state safety. The cutover keeps the final backup,
freeze/drain ledger, Rust launchd ownership, and rollback commands explicit.

## Scope

- Rust owns the service port (`127.0.0.1:8420`) through launchd label
  `com.rajeshgoli.session-manager-rust`.
- Python launchd labels are stopped only by label, not by arbitrary port kill.
- Rust refuses to start when the target port is already occupied.
- Final backup is copied only after Python is stopped and the health probe is
  connection-refused for the configured hold window.
- The first canary may use the accelerated Rust evidence path from issue #1038
  instead of sustained Python-authoritative shadow, because Python is the
  unstable component.

## Build

Build the Rust server and CLI from the exact commit being deployed:

```bash
cargo build -p sm-server --release
```

The service helper defaults to `target/release/sm-server`. Use
`--binary target/debug/sm-server` only for a local dry run, not production.

## Pre-Cutover Review

Run the non-mutating service plan:

```bash
./scripts/rust-service-cutover.sh plan
```

Expected before stopping Python:

- Rust binary is executable.
- `config.yaml` is readable.
- known Python label `com.rajeshgoli.session-manager` may be loaded.
- port `8420` may be occupied by the current Python service.

Any unknown process on port `8420` is a blocker. Do not use `kill -9` as a
cutover primitive.

Optional pre-freeze safety backup while Python may still be running:

```bash
./venv/bin/python -m scripts.rust_migration.state_backup \
  --config config.yaml \
  --output-dir .local/rust-precutover-backup-$(date -u +%Y%m%dT%H%M%SZ) \
  --execute \
  --fail-on-blockers
```

This is not the rollback restore point. It is only a safety snapshot before the
stopped-origin final backup.

## Stop Python And Create Final Backup

Stop only known Python Session Manager launchd labels:

```bash
./scripts/rust-service-cutover.sh stop-python
```

Create the final stopped-origin backup and ledger:

```bash
./venv/bin/python -m scripts.rust_migration.final_backup \
  --config config.yaml \
  --output-dir .local/rust-final-backup-$(date -u +%Y%m%dT%H%M%SZ) \
  --ledger .local/rust-cutover-ledger.jsonl \
  --record-ledger \
  --execute \
  --fail-on-blockers
```

Do not start Rust if this command blocks. Fix the blocker or restart Python
with `./scripts/rust-service-cutover.sh rollback-python`.

## Start Rust

Write/load the Rust launchd plist and start Rust on the service port:

```bash
./scripts/rust-service-cutover.sh start-rust
./scripts/rust-service-cutover.sh status
```

The helper writes:

- plist: `~/Library/LaunchAgents/com.rajeshgoli.session-manager-rust.plist`
- stdout: `logs/rust-launchd.out.log`
- stderr: `logs/rust-launchd.err.log`

## Protected Public Tunnel

After Rust owns `127.0.0.1:8420`, the Cloudflare tunnel should forward the
protected app hostname directly to that launchd-managed Rust service. Do not
leave `sm-app.rajeshgo.li` pointed at a manually started `8421` sidecar, and do
not leave legacy `sm.rajeshgo.li` routed to origin.

Validate the local cloudflared ingress shape:

```bash
./venv/bin/python -m scripts.rust_migration.public_tunnel_preflight \
  --config .local/android-parity/cloudflared/config-http-only.yml \
  --fail-on-blockers
```

Expected post-cutover shape:

```yaml
ingress:
  - hostname: sm-app.rajeshgo.li
    service: http://127.0.0.1:8420
  - service: http_status:404
```

Then validate cloudflared syntax and restart the tunnel:

```bash
cloudflared tunnel --config .local/android-parity/cloudflared/config-http-only.yml ingress validate
launchctl kickstart -k gui/$(id -u)/com.rajesh.sm-android-tunnel
```

Public unauthenticated probes should show Cloudflare Access on the app host and
no origin route on the legacy host:

```bash
curl -sS -o /tmp/sm-app-health-public.txt -w 'sm-app %{http_code}\n' https://sm-app.rajeshgo.li/health
curl -sS -o /tmp/sm-legacy-health-public.txt -w 'legacy %{http_code}\n' https://sm.rajeshgo.li/health
```

Expected: `sm-app 403` from Cloudflare Access or another Cloudflare
before-origin denial, and `legacy 403` or `legacy 404`. A `200` health response
from either public hostname is a blocker because it means unauthenticated
traffic reached origin.

Collect the same checks as a single JSON evidence artifact:

```bash
./venv/bin/python -m scripts.rust_migration.live_canary_report \
  --output .local/rust-mvp-rehearsals/live-canary-$(date -u +%Y%m%dT%H%M%SZ).json \
  --fail-on-blockers
```

This command is non-mutating. It records Rust launchd ownership, local Rust
health/native-read checks, `sm status`, tunnel config shape, public Access
denial, legacy-host absence, and optional Cloudflare/mobile smoke evidence.

## First 15-Minute Smoke Checklist

Run these immediately after Rust is listening:

```bash
curl -sf http://127.0.0.1:8420/health
curl -sf http://127.0.0.1:8420/health/detailed
target/release/sm --api-url http://127.0.0.1:8420 status
target/release/sm --api-url http://127.0.0.1:8420 queue list
curl -sf http://127.0.0.1:8420/nodes
curl -sf http://127.0.0.1:8420/client/bootstrap
```

Then exercise one controlled session:

```bash
target/release/sm --api-url http://127.0.0.1:8420 spawn --name rust-cutover-smoke "echo rust cutover smoke && exit"
target/release/sm --api-url http://127.0.0.1:8420 status
```

App/operator checks:

- Android app opens against `sm-app.rajeshgo.li`.
- client certificate shows configured.
- Google sign-in/device auth succeeds.
- session list loads.
- app artifact update metadata/download works.
- request-status and analytics routes do not return login HTML.

Record any failure as a Rust canary bug. Do not restart Python unless the bug
blocks core command/session/mobile recovery.

## Rollback

For service rollback before incompatible Rust state changes:

```bash
./scripts/rust-service-cutover.sh rollback-python
./scripts/rust-service-cutover.sh status
```

If Rust has written bad state, stop Rust first:

```bash
./scripts/rust-service-cutover.sh stop-rust
```

Then use the final backup manifest as the audited restore source. The current
restore tool rehearses into a disposable root; live destructive restore remains
operator-reviewed and should not be automated silently:

```bash
./venv/bin/python -m scripts.rust_migration.state_restore \
  --manifest .local/rust-final-backup-<STAMP>/state-backup-manifest.json \
  --restore-dir /tmp/session-manager-restore-review-$(date -u +%Y%m%dT%H%M%SZ) \
  --execute-restore \
  --fail-on-blockers
```

After reviewing the restore contents, copy live state back only with explicit
operator approval.

## Stop Conditions

Stop the canary and rollback service ownership if:

- Rust cannot answer `/health` on port `8420`.
- `target/release/sm status` or `sm send` fails for retained core paths.
- mobile app auth/session list cannot recover quickly.
- app artifacts cannot be downloaded by the enrolled app.
- queue jobs or node status show ownership/corruption errors.
- any state backup/ledger blocker appears during final backup.

Non-critical retained-path bugs should be fixed forward in Rust while keeping
Python stopped, because Python is the current unstable component.
