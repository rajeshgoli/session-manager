# Rust Port Resume Handoff

Status: handoff snapshot from 2026-06-16 after PR #1037 contract harness
read-budget fix, with issue #1038 adding an accelerated Rust canary evidence
path for Python-origin instability.

Use this file to resume the Rust cutover track without reconstructing state from
chat history. Binding scope still lives in [cutover_scope.md](cutover_scope.md),
release gates in [gate_matrix.md](gate_matrix.md), and executable contract
coverage in
[`scripts/rust_migration/contracts_manifest.json`](../../scripts/rust_migration/contracts_manifest.json).

## Current Repository State

- Branch: `main` after PR #1037.
- Latest merged commit before this docs refresh: `68adc3b` (`Merge pull
  request #1037`)
- Open PRs at handoff: none before this docs refresh PR.
- Dirty worktree at handoff: only pre-existing untracked
  `.claude/settings.local.json`, `certs/`, `data/`, and the local
  `config.yaml.shadow-backup-20260612T023248Z` created when enabling shadow
  mode.
- Stale session-manager review agents from the overnight run and PR #932 were retired.
- Unrelated non-session-manager agents were left alone.
- Live Python-authoritative Rust shadow mode is enabled in `config.yaml`, with
  Python serving on `:8420`, the Rust sidecar on `127.0.0.1:8421`, and the
  ledger at `~/.local/share/claude-sessions/rust_shadow.jsonl`.

## Completed Work

### Spec And Scope

- The Rust migration spec is converged and merged.
- Stage 5 artifacts define retained scope, removed surfaces, state ownership,
  rollout gates, workstreams, and cutover behavior.
- Owner-approved cutover decisions are incorporated:
  - keep useful stable core and native mobile app;
  - keep email/human fallback and inbound email;
  - remove Telegram, `sm dispatch`, `sm what`, standalone reminders,
    watch-job, policy/CI queue helpers, Termux attach, and public unauthenticated
    operational browser data;
  - require public-edge proof before public operational traffic reaches origin.

### Rust Implementation Track

Merged Rust slices cover:

- contract harness, fixture assertions, baseline runner, shadow mode, and MVP
  rehearsal tooling;
- core health/auth/bootstrap/session list/detail/output/events/SSE reads;
- core runtime slices for session/tmux/spawn/session graph/message queue/task
  complete/input batch/subagents and retained review routes;
- nodes list and node HTTP routes;
- queue jobs list/detail, CLI reads, pending submit, simple execute/cancel, and
  recovery;
- Codex review requests list/detail/create/cancel/recovery and retained
  `sm review` / PR review routes;
- tool-call projections for Claude and Codex fork;
- Codex events, pending requests, activity actions;
- mobile bootstrap/session support, attach-ticket proof, terminal WebSocket
  auth/bridge, runtime disable, device revoke/list CLI support;
- public-edge assertion gate and request-target binding;
- email/human fallback and inbound email validation;
- retired-surface fixtures for removed routes and commands.

### Documentation And Manifest

- [mvp_progress.md](mvp_progress.md) records the PR lineage through #1035.
- [cloudflare_access_cutover_evidence.md](cloudflare_access_cutover_evidence.md)
  records the current Cloudflare Access origin-gate behavior and remaining
  operator setup/smoke evidence.
- PR #880 added fixture-gated manifest checks for already-implemented detail
  endpoints:
  - `GET /queue-jobs/{queue_job_id}`
  - `GET /codex-review-requests/{codex_review_request_id}`
- PR #882 added validation-only manifest coverage for device-token and
  tmux-client hook error paths.
- PR #884 corrected the retained client-session fixture expectation for
  classic attach support.
- PR #886 added a synthetic Codex app read fixture.
- PR #888 added a synthetic app artifact metadata fixture.
- PR #890 added a synthetic queue-runner job fixture.
- PR #892 added the disposable mutating fixture workspace.
- PR #894 made the MVP rehearsal run both read-only and mutating Rust-core
  contracts in isolated sidecars.
- PR #896 updated this handoff after the first real-state rehearsal exposed a
  single `/events/state` shadow body mismatch.
- PR #898 reclassified `/events/state` shadow comparison as status-only because
  Python carries live tmux-client event state that a fresh Rust sidecar does not
  own.
- PR #900 recorded the clean #898-era handoff.
- PR #902 added `shadow_report` observation evidence with blocker handling for
  no-data, malformed status, and invalid row shapes.
- PR #904 added the non-mutating shadow workflow planner.
- PR #906 added the safe `rust_shadow` config activation helper and backup path.
- PR #908 added `--since` and `--last-minutes` filtering to `shadow_report`.
- PR #910 made Rust contract CLI checks default to `target/debug/sm`.
- PR #912 added `--skip-fixture-checks` so broad live-state contract runs can
  skip synthetic fixture checks without suppressing ordinary live coverage.
- PR #914 refreshed this handoff after live shadow activation.
- PR #916 added shadow report coverage gates.
- PR #918 wired shadow observation coverage gates into the planner.
- PR #920 added mobile device CLI contract checks.
- PR #922 added the Rust CLI cutover scope audit.
- PR #924 added state ownership preflight.
- PR #926 added the state backup plan/copy tool.
- PR #928 added the freeze/drain plan ledger scaffold.
- PR #930 added backup verification and restore rehearsal.
- PR #932 made the MVP rehearsal run state preflight, backup, restore
  verification, disposable restore, and freeze/drain evidence by default.
- PR #934 refreshed the handoff after the state-gated rehearsal.
- PR #936 made live session detail shadow prediction status-only while keeping
  fixture-backed deterministic session detail checks body/assertion based.
- PR #938 made the MVP rehearsal run synthetic read-only fixture contracts in a
  dedicated sidecar.
- PR #940 refreshed the handoff after the post-#938 clean rehearsal.
- PR #942 added native mobile/sm-app read paths to rehearsal shadow probes and
  mobile route/pattern gates to the shadow observation planner/report.
- PR #944 refreshed the handoff after the post-#942 mobile shadow evidence.
- PR #946 added the Cloudflare Access auth model for browser, mobile app, node
  fallback, and email worker route classes.
- PR #948 added Rust Cloudflare Access config parsing and JWT/audience/context
  classification.
- PR #950 added Rust origin route gates for native mobile/app routes, app
  artifacts, `/auth/device/google`, JWKS cache refresh/TTL behavior, public-host
  fail-closed behavior, and mobile device Common Name actor binding.
- PR #952 recorded Cloudflare Access cutover evidence.
- PR #954 added Rust queue list/status CLI reads.
- PR #956 added pending queue job submission.
- PR #958 added simple queue job execute/cancel runtime.
- PR #960 added queue runtime recovery.
- PR #962 added node HTTP routes.
- PR #964 added Codex review request cancel.
- PR #966 wired `request-codex-review` list/status/cancel CLI surfaces.
- PR #968 added retained Codex review request creation.
- PR #970 wired `request-codex-review create`.
- PR #972 added active Codex review watch recovery.
- PR #974 wired retained `sm review`.
- PR #976 added retained `/reviews/pr`.
- PR #979 made PR review wait require actual review events before completion.
- PR #980 added retained session review runtime routes.
- PR #983 fixed session review wait timeout semantics and spawned review
  startup settle behavior.
- PR #984 added mutating fixture contracts for retained session review routes.
- PR #986 refreshed this handoff after the post-#984 review-route fixture line.
- PR #988 added a node restore mutating fixture contract.
- PR #990 added Rust CLI node restore fixture support.
- PR #992 added the stopped-service final backup gate.
- PR #994 integrated the final backup gate into MVP rehearsal.
- PR #996 added the Cloudflare Access mobile smoke evidence runner.
- PR #998 refreshed the handoff after the Cloudflare smoke runner.
- PR #1000 added Android Cloudflare client-certificate storage and native
  HTTP/WebSocket client-certificate presentation.
- PR #1002 added the first Android QR enrollment flow; PR #1012 later replaced
  the in-app scanner/manual certificate UI with Camera-app deep-link
  enrollment.
- PR #1005 added Rust `sm enroll-device`, mobile device DB enrollment, CSR
  signing, and per-device Common Name policy sync.
- PR #1007 automated Cloudflare mTLS CA upload and mobile app hostname
  association while keeping the CA private key local.
- PR #1010 added Android artifact version-code/version-name override support.
- PR #1012 added Android Camera-app QR handoff, `sm-enroll://enroll`, direct
  in-app credential save, local/private HTTP pairing fallback, and removed
  camera permission plus manual certificate UI.
- PR #1023 blocked `/auth/device/google` shadow successes while Rust still
  lacked native bearer issuance.
- PR #1025 implemented Rust native Google device auth success, including Google
  JWKS verification, mobile Access actor binding, public-edge gating, and
  zero-skew token temporal checks.
- PR #1027 is the post-#1025 smoke evidence slice; the first run records one
  mobile Access-denial boundary pass and the missing proof inputs blocking full
  smoke.
- PR #1029 clarified the post-#1025 smoke input status.
- PR #1031 made Android enrollment use a separate RSA mTLS credential.
- PR #1033 made Android Cloudflare mTLS use a software RSA key with protected
  local storage.
- PR #1035 allowed Android app artifacts to be read with cert-gated app auth
  while preserving Cloudflare edge and session-auth fallbacks.
- PR #1037 raised the contract harness default HTTP read budget so large live
  `/client/sessions` JSON responses are not truncated before assertion checks.
- Issue #1038 adds the accelerated Rust canary evidence mode for the case where
  Python-origin availability prevents sustained baseline/shadow evidence.
- Current contract manifest size:
  - `134` checks total
  - `73` `python_and_rust`
  - `61` `rust_only`
  - `93` read-only checks
  - `41` mutating checks

## Validation At Handoff

The latest clean full real-state MVP rehearsal remains:

```bash
.local/rust-mvp-rehearsals/20260612T-full-after-938/mvp-rehearsal-report.json
```

Observed results from that run:

- overall status: passed with zero blockers;
- Python health, explicit Rust sidecar reuse health, and isolated runtime smoke
  passed;
- state ownership gate passed:
  - `17` stores checked;
  - `13` existing stores copied into the backup;
  - `13` backup entries verified;
  - `13` backup entries restored into the disposable restore root;
  - freeze/drain ledger plan written;
- Rust live core sidecar contracts: `17` passed, `0` failed, `0` skipped;
- Rust synthetic read-only fixture contracts: `10` passed, `0` failed, `0`
  skipped;
- Rust mutating fixture contracts: `30` passed, `0` failed, `0` skipped;
- gap probes: `0` failed;
- Python and Rust baseline measurements completed;
- shadow read summary: `8` passed, `0` failed.

The earlier blocker was `GET /events/state` shadow comparison. Python and Rust
both returned status `200`, but the predicted Rust body hash did not match
Python because Python carries live tmux-client event state. PR #898 made that
shadow prediction status-only. PR #936 also made live session detail
status-only for shadow to avoid lifecycle TOCTOU noise. The clean run now
passes `/health`, `/health/detailed`, `/auth/session`, `/client/bootstrap`,
`/sessions`, `/client/sessions`, `/nodes`, and `/events/state`.

The same run measured the current Python process at `154.672 MiB` RSS and
`66.4 MiB` physical footprint, while the Rust sidecar measured `19.797 MiB`
RSS and `6.688 MiB` physical footprint.

The latest focused post-#984 fixture/state-gated rehearsal is:

```bash
/tmp/session-manager-pr981-rehearsal-ports/mvp-rehearsal-report.json
```

It used alternate ports because a Rust sidecar was already healthy on `8421`:

```bash
./venv/bin/python -m scripts.rust_migration.mvp_rehearsal \
  --output-dir /tmp/session-manager-pr981-rehearsal-ports \
  --rust-base-url http://127.0.0.1:18421 \
  --skip-python-health \
  --skip-baseline \
  --skip-shadow \
  --core-only
```

This was not a full cutover rehearsal because it skipped Python baseline and
shadow, but it remains the current fixture/state-gate evidence for the
post-#984 manifest:

- overall status: passed with zero blockers;
- state ownership gate: `17` stores checked, `13` existing, `13` copied, `13`
  verified, `13` restored, freeze/drain ledger written;
- Rust live core sidecar contracts: `17` passed, `0` failed, `0` skipped;
- Rust synthetic read-only fixture contracts: `14` passed, `0` failed, `0`
  skipped;
- Rust mutating fixture contracts: `35` passed, `0` failed, `0` skipped.

PRs #986-#1037 added more cutover tooling and evidence gates after this focused
run, including node restore fixtures, stopped-origin final backup, rehearsal
final-backup integration, the Cloudflare Access mobile smoke runner, Rust
mobile-device enrollment, Cloudflare mTLS CA automation, Android Camera-app
enrollment, Android mTLS fixes, native device auth, and app artifact auth
fixes, plus the larger live HTTP read budget. The latest post-#1035 rehearsal
below is the current evidence snapshot.

The latest post-#1035 full rehearsal attempts are blocked, not cutover evidence:

```bash
.local/rust-mvp-rehearsals/20260616T215701Z-post-1035/mvp-rehearsal-report.json
.local/rust-mvp-rehearsals/20260616T220122Z-post-1035/mvp-rehearsal-report.json
.local/rust-mvp-rehearsals/20260616T221117Z-post-1035/mvp-rehearsal-report.json
```

The first run exposed a contract-harness limit: live `/client/sessions`
responses were valid JSON but exceeded the old 64 KiB default read budget, so
the harness truncated the body before JSON assertions. This refresh raises the
default contract HTTP read budget and adds a regression test for large JSON
responses.

The latest run is the clearer current snapshot. It started with Python
`/health` passing, and the state/Rust-side gates passed:

- state ownership gate: `17` stores checked, `14` copied, `14` verified, `14`
  restored, freeze/drain ledger written;
- Rust live core sidecar contracts: `17` passed, `0` failed, `0` skipped;
- Rust synthetic read-only fixture contracts: `14` passed, `0` failed, `0`
  skipped;
- Rust mutating fixture contracts: `37` passed, `0` failed, `0` skipped;
- Rust baseline: passed, `19.359 MiB` RSS and `8.125 MiB` physical footprint.

The same run blocked because the Python authoritative service stopped accepting
connections during `python_baseline`, after the first health probe. The
`shadow_mobile_path_resolution` step therefore skipped with connection refused,
and `shadow_read_summary` failed all `9` requested read probes. Launchd logs
around the run show the Python watchdog reporting an event-loop freeze and
killing the process for restart; after that, port `8420` stopped accepting
connections. Do not treat the post-#1035 rehearsal as a clean MVP cutover
artifact until Python-origin availability is stable for the baseline/shadow
steps, or until the accelerated Rust canary evidence path records passing spot
checks plus Rust/state/Cloudflare gates.

## Accelerated Rust Canary Evidence Path

Issue #1038 adds an explicit path for rotating faster when Python-origin
availability is the blocker. This is not a silent shadow skip. The report must
show:

- `python_canary_spot_checks` passed;
- state preflight/backup/restore/freeze-drain gate passed;
- Rust live core, synthetic read-only fixture, and mutating fixture contracts
  passed;
- Rust baseline passed;
- `cloudflare_access_smoke_report` passed from a smoke report that used real
  Cloudflare Access, public-edge, and SM auth proof inputs;
- `python_baseline` and `shadow_read_summary` recorded as skipped because
  `--rust-canary-cutover` was intentionally selected.

Command shape:

```bash
./venv/bin/python -m scripts.rust_migration.cloudflare_access_smoke \
  --base-url http://127.0.0.1:8421 \
  --mobile-host sm-app.rajeshgo.li \
  --browser-host sm.rajeshgo.li \
  --mobile-access-jwt-env CF_MOBILE_ACCESS_JWT \
  --browser-access-jwt-env CF_BROWSER_ACCESS_JWT \
  --public-edge-secret-env SM_PUBLIC_EDGE_SECRET \
  --bearer-token-env SM_DEVICE_BEARER_TOKEN \
  --output .local/rust-mvp-rehearsals/cloudflare-smoke.json \
  --json \
  --fail-on-blockers

./venv/bin/python -m scripts.rust_migration.mvp_rehearsal \
  --rust-canary-cutover \
  --cloudflare-smoke-report .local/rust-mvp-rehearsals/cloudflare-smoke.json
```

A missing, blocked, or synthetic Cloudflare smoke report keeps the canary run
blocked. Use this path only after the operator accepts that sustained Python
shadow is being replaced because Python is the unstable component.

## Live Observation

Live shadow mode is currently active. The most recent clean passive report used:

```bash
./venv/bin/python -m scripts.rust_migration.shadow_report \
  --ledger ~/.local/share/claude-sessions/rust_shadow.jsonl \
  --last-minutes 30 \
  --fail-on-blockers \
  --json
```

Observed after #996 at `2026-06-14T23:36Z`:

- status: passed;
- rows: `2,238`;
- blockers: `0`;
- invalid rows: `0`;
- routes observed: `GET /events/state` (`861` status matches) and
  `GET /sessions` (`1,377` status matches).
- mobile route/pattern gates from PR #942 were not satisfied by this passive
  window; they require native app traffic or explicit operator exercise.
- this is not Cloudflare/public-edge evidence; the local shell did not have
  `CF_MOBILE_ACCESS_JWT`, `CF_BROWSER_ACCESS_JWT`,
  `SM_PUBLIC_EDGE_SECRET`, `SM_DEVICE_BEARER_TOKEN`, or `SM_COOKIE` set, and
  `--mobile-host` / `--browser-host` were not supplied for a real smoke run.

Post-#1025, the Rust sidecar was rebuilt and restarted from current `main` on
`127.0.0.1:8421`. The Cloudflare/mobile smoke runner was executed with
`--mobile-host sm-app.rajeshgo.li` and `--browser-host sm.rajeshgo.li`; output
was recorded at:

```text
.local/rust-mvp-rehearsals/post-1025-mobile-auth-smoke-blocked.json
```

That run passed `mobile.bootstrap_requires_access`, proving the Rust origin
denies mobile bootstrap requests on the app host when no Cloudflare Access
assertion is present. It remains blocked because the shell did not have
`CF_MOBILE_ACCESS_JWT`, `CF_BROWSER_ACCESS_JWT`, `SM_PUBLIC_EDGE_SECRET`,
`SM_DEVICE_BEARER_TOKEN`, or `SM_COOKIE` set. Summary: `1` passed, `5`
blocked, `7` skipped. This is partial boundary evidence only.

No newer clean full passive shadow or full Cloudflare/mobile smoke artifact has
been recorded in this handoff. PR #1012 published Android artifact `cbb61798`
(`versionCode=1013`, `versionName=0.1.0-enroll-ui-cleanup`) and operator
testing confirmed the app reached "Client certificate saved" after Camera-app
deep-link enrollment. After PR #1035, operator testing also confirmed Android
app update artifact access works through the certificate-gated app path. The
full Access smoke runner and sustained shadow window still need recorded
artifacts.

The broad live Rust contract run now uses the fixture filter:

```bash
./venv/bin/python -m scripts.rust_migration.contracts \
  --target rust \
  --base-url http://127.0.0.1:8421 \
  --session-id 007c6275 \
  --skip-fixture-checks \
  --json
```

Fixture-filtered live Rust contract result:

- `71` passed;
- `3` skipped because they are mutating checks without `--include-mutating`;
- `0` failed.

Transient observation note: while retiring the PR #912 review session, one
`GET /sessions/765b4805` body mismatch was recorded and then the same route
matched on the next observation. Treat this as session-state drift unless it
recurs in a later sustained window.

## Useful Local Fixtures Found

These were valid at handoff and are useful for continuing fixture-backed
contract coverage:

| Fixture | Value | Source |
| --- | --- | --- |
| `queue_job_id` | `job_5a66488c1b6b` | `~/.local/share/claude-sessions/queue-runner/queue_runner.db` |
| `codex_review_request_id` | `255618bcc483` | `~/.local/share/claude-sessions/message_queue.db` |

Historical missing or not-found local fixtures from the earlier #880 handoff:

- no active `provider=codex-app` sessions in `GET /sessions`;
- no app artifact `meta.json` found in the checked default local artifact paths.

Those gaps are now covered by synthetic read-only fixtures from PR #886 and
PR #888 for Rust-side fixture execution. Real live-state fixtures are still
needed for final cutover evidence.

## What Remains

### Near-Term Work

1. Collect either the normal Python-authoritative shadow window or the
   accelerated Rust canary evidence report.
   - Normal path: keep shadow mode running for retained reads and triage
     unexplained mismatches.
   - Canary path: run `--rust-canary-cutover` with a passed
     `--cloudflare-smoke-report` and treat any blocker as cutover-blocking.
2. Continue expanding fixture/live evidence as new retained rows land.
   - The MVP rehearsal already runs the current synthetic read-only and
     mutating fixture sets, including review-route fixtures.
   - Final live mobile/device fixture evidence is still needed.
3. Re-run the CLI cutover audit whenever retained or retired CLI surface
   changes.
   - PR #922 added the audit; use it as a regression gate rather than an open
     discovery task.
4. Complete final state ownership and migration tooling.
   - Initial preflight, backup, restore, and freeze/drain evidence tools are
     merged and exercised by the MVP rehearsal.
   - Remaining work is the live write-admission freeze/journal gate before
     final backup.
   - Prove rollback restores or accounts for every accepted write after the
     restore point.
5. Complete Cloudflare Access deployment integration.
   - Configure the browser, mobile app, node fallback, and email worker Access
     apps/policies from [cloudflare_access_cutover_evidence.md](cloudflare_access_cutover_evidence.md).
   - Prove app-host requests require an enrolled mobile device certificate,
     then SM Google/device bearer auth at origin.
   - Prove revoked-device denial, browser OAuth callback behavior, and later
     node fallback proof.
6. Finish retained node-control and narrow queue writer evidence.
   - Basic node HTTP routes and narrow queue writer/recovery are implemented.
   - Remaining work is final live/recovery cutover evidence, audit/rollback
     accounting, and any retained node-agent remote-control gaps.
7. Exercise service packaging and cutover.
   - launchd wrapper, non-destructive port ownership, health checks, rollback.
8. Run final native mobile smoke checks.
   - bootstrap, session list/detail, attach, request-status, analytics, bug
     reports, app artifacts.
   - PR #996 added the read-only Cloudflare Access smoke runner.
   - PRs #1005, #1007, and #1012 added the mobile enrollment path needed to
     generate app credentials for that smoke.
   - Passing evidence still requires the real Cloudflare Access JWTs,
     public-edge secret, SM auth token or cookie, and mobile app traffic.

### Cutover Stop Conditions

Do not start Rust writer ownership or MVP cutover until:

- retained manifest checks pass for Python and Rust on the selected fixture set;
- either shadow mode shows no unexplained retained-core mismatches for the
  agreed observation window, or the accelerated Rust canary report records
  passing `python_canary_spot_checks`, Rust/state/fixture gates, Rust baseline,
  and Cloudflare/mobile smoke;
- final backup happens after write admission is frozen or journaled;
- public traffic has proof-of-possession before origin plus origin
  auth/capability checks;
- Cloudflare Access browser/mobile/node/email route classes are configured with
  no bypass/broad-certificate policy and verified against Rust origin gates;
- mobile attach, request-status, bug reports, and app artifacts pass smoke
  checks;
- rollback restores or explicitly journals every accepted write after the
  restore point.

## Recommended Resume Point

Continue with Cloudflare Access deployment evidence, then produce either normal
shadow evidence or the accelerated Rust canary report:

1. Provide the smoke runner proof inputs for the already configured Cloudflare
   Access apps: mobile/browser Access JWTs, public-edge HMAC secret, and SM
   bearer/cookie.
2. Configure or verify any remaining Cloudflare Access apps/policies described in
   [cloudflare_access_cutover_evidence.md](cloudflare_access_cutover_evidence.md).
3. Exercise the native app route class through the app hostname so mobile gates
   collect real traffic and smoke evidence.
4. If Python remains stable, keep Python-authoritative Rust shadow running for
   retained reads, let it observe real traffic for the agreed window, and
   summarize the ledger by route/comparison/mismatch.
5. If Python remains the unstable component, run `mvp_rehearsal
   --rust-canary-cutover --cloudflare-smoke-report <passed-smoke-json>` and use
   that report as the cutover-candidate evidence artifact.
6. Convert any unexplained retained-surface mismatch or canary blocker into a
   bounded Rust slice.

This is the highest-signal next step because the Rust origin gate is merged and
the broad Python service is now the limiting factor. Do not do broad Python
hardening; prove the Rust canary path or fix bounded Rust/Cloudflare/mobile
blockers.
