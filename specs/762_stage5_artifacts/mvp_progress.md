# Rust MVP Progress Snapshot

Status: implementation snapshot after PR #1039 accelerated Rust canary evidence
mode, with issue #1040 adding Rust service cutover tooling/runbook.
The last clean full real-state MVP rehearsal remains the post-#938 run; the
post-#1035 rehearsal proves state gates, live core contracts, read-only
fixtures, mutating fixtures, and Rust baseline on isolated sidecars, but blocks
on Python-origin availability during Python baseline and shadow. PRs #986-#1039
added node restore fixtures, stopped-origin final backup gates, rehearsal
final-backup integration, Cloudflare Access smoke evidence tooling, Rust
mobile-device enrollment, Cloudflare mTLS CA automation, the Android Camera-app
enrollment handoff, and Rust native Google device-auth bearer issuance, plus
Android mTLS, artifact read fixes, and a larger live HTTP read budget. Public
cutover still needs full Cloudflare policy/proof inputs and real mobile/browser
smoke evidence. A stable Python-authoritative shadow window is preferred, but
the explicit accelerated canary path may replace it when Python availability is
the unstable component.

This file is a handoff aid for the Rust cutover implementation track. It does
not change retained or removed scope. Binding scope remains
[cutover_scope.md](cutover_scope.md), release gates remain
[gate_matrix.md](gate_matrix.md), and executable surface coverage remains
[`scripts/rust_migration/contracts_manifest.json`](../../scripts/rust_migration/contracts_manifest.json).

## Current Merge Point

`main` includes the Rust MVP implementation line through:

| PR | Slice |
| --- | --- |
| #773 | read-only session list scaffold |
| #776-#782 | contract fixture expansion and minimal value baseline |
| #784-#788 | read-only session contracts, watch/SSE contracts, and shadow mode |
| #790 | shadow secret redaction |
| #792-#818 | core session/tmux/spawn/session-graph/message-queue/task-complete/input-batch/subagent slices |
| #822-#824 | Codex-fork runtime and control slices |
| #826 | MVP sidecar rehearsal harness |
| #828-#834 | nodes read API, analytics summary, Codex review request list, queue jobs list |
| #836 | live rehearsal and shadow integration |
| #838-#840 | mobile support and email/human fallback |
| #842-#856 | mobile attach tickets, terminal WebSocket auth/bridge, disable/revoke/device CLI, public-edge assertion gate |
| #858-#862 | queue job detail and public-edge request-target follow-up |
| #864-#876 | Codex review detail, tool-call projections, Codex events, pending requests, activity actions, and manifest coverage |
| #878 | progress snapshot for resuming the Rust MVP track |
| #880 | fixture-gated manifest checks for queue job and Codex review request detail endpoints |
| #882 | validation-only manifest coverage for device-token and tmux-client hook error paths |
| #884 | client-session fixture assertion correction for classic attach support |
| #886 | synthetic Codex app read fixture for retained Codex read checks |
| #888 | synthetic app artifact metadata fixture |
| #890 | synthetic queue-runner job fixture for list/detail checks |
| #892 | disposable mutating fixture workspace for Rust-core CLI and HTTP checks |
| #894 | MVP rehearsal gate runs isolated read-only and mutating Rust-core contracts |
| #896 | handoff update after the first real-state rehearsal |
| #898 | `/events/state` shadow comparison moved to status-only, clearing the rehearsal blocker |
| #900 | clean #898-era handoff update |
| #902 | shadow observation report with blocker handling |
| #904 | non-mutating shadow workflow planner |
| #906 | safe `rust_shadow` config activation helper |
| #908 | shadow report `--since` and `--last-minutes` filters |
| #910 | Rust contract CLI checks default to `target/debug/sm` |
| #912 | `--skip-fixture-checks` for broad live-state contract runs |
| #914 | handoff refresh after live shadow activation |
| #916 | shadow report coverage gates |
| #918 | shadow observation planner carries coverage gates |
| #920 | mobile device CLI contract checks |
| #922 | Rust CLI cutover scope audit |
| #924 | state ownership preflight |
| #926 | state backup plan and copy tool |
| #928 | freeze/drain plan ledger scaffold |
| #930 | backup verification and restore rehearsal |
| #932 | MVP rehearsal runs state preflight, backup, restore, and freeze/drain evidence |
| #934 | handoff update after the state-gated rehearsal |
| #936 | live session detail shadow comparison moved to status-only |
| #938 | MVP rehearsal runs synthetic read-only fixture contracts in a dedicated sidecar |
| #940 | handoff update after the post-#938 clean rehearsal |
| #942 | mobile/sm-app routes added to active rehearsal shadow probes and passive shadow coverage gates |
| #944 | handoff update after the post-#942 mobile shadow evidence |
| #946 | Cloudflare Access auth model for browser, mobile app, node fallback, and email worker route classes |
| #948 | Rust Cloudflare Access config parsing and JWT/audience/context classification |
| #950 | Rust origin route gates for mobile/app surfaces, app artifacts, device-token exchange, JWKS cache behavior, and mobile device actor binding |
| #952 | Cloudflare Access cutover evidence snapshot |
| #954 | Rust queue list/status CLI reads |
| #956 | Pending queue job submission |
| #958 | Simple queue job execute/cancel runtime |
| #960 | Queue runtime recovery |
| #962 | Node HTTP routes |
| #964 | Codex review request cancel |
| #966 | `request-codex-review` CLI list/status/cancel wiring |
| #968 | Retained Codex review request creation |
| #970 | `request-codex-review create` CLI wiring |
| #972 | Active Codex review watch recovery |
| #974 | Retained `sm review` CLI wiring |
| #976 | Retained `/reviews/pr` PR review route |
| #979 | PR review wait observes actual review events before completion |
| #980 | Retained session review runtime routes |
| #983 | Follow-up session review wait timeout and spawned-child startup settle fixes |
| #984 | Review-route mutating fixture contracts |
| #986 | Handoff refresh after the post-#984 review-route fixture line |
| #988 | Node restore mutating fixture contract |
| #990 | Rust CLI node restore fixture support |
| #992 | Stopped-service final backup gate |
| #994 | Final backup gate integrated into MVP rehearsal |
| #996 | Cloudflare Access mobile smoke evidence runner |
| #998 | Handoff refresh after Cloudflare smoke runner |
| #1000 | Android Cloudflare client-certificate storage and native HTTP/WebSocket client-certificate presentation |
| #1002 | Initial Android QR enrollment support; later replaced by the Camera-app deep-link flow |
| #1005 | Rust `sm enroll-device`, mobile device DB enrollment, CSR signing, and per-device Common Name sync |
| #1007 | Cloudflare mTLS CA upload and mobile app hostname association automation |
| #1010 | Android artifact version-code/version-name override support |
| #1012 | Android Camera-app QR handoff, `sm-enroll://enroll` deep link, direct in-app credential save, and no camera permission/manual cert UI |
| #1023 | Block unported `/auth/device/google` shadow successes until Rust can issue native device bearers |
| #1025 | Rust native Google device-auth success with Google JWKS verification, mobile Access actor binding, public-edge gate, and zero-skew token temporal checks |
| #1027 | Post-#1025 mobile auth smoke boundary evidence; records mobile Access denial and missing proof inputs |
| #1029 | Clarify post-#1025 smoke input status |
| #1031 | Android enrollment uses separate RSA mTLS credentials |
| #1033 | Android Cloudflare mTLS uses a software RSA key with protected local storage |
| #1035 | Android app artifacts can be read with cert-gated app auth while preserving edge/session fallbacks |
| #1037 | Contract harness HTTP reads default to a larger budget for live `/client/sessions` payloads |
| #1039 | Accelerated Rust canary evidence mode for Python-origin instability |
| issue #1040 | Rust service launchd cutover tooling and first-canary runbook |

## Implemented Capability Groups

The Rust sidecar now has executable coverage for:

| Group | Current state |
| --- | --- |
| Harness and baselines | Contract manifest, fixture assertions, minimal value baseline runner, shadow comparison, MVP rehearsal, synthetic read-only fixture sidecar, disposable mutating fixtures, Rust CLI build gating, state preflight/backup/restore, stopped-origin final backup, freeze/drain evidence gates, Cloudflare Access smoke evidence runner, mobile-aware shadow observation gates, and accelerated Rust canary evidence mode exist. |
| Retired-surface checks | Retired public/watch/summary/remind/job-watch/queue-policy checks are represented as Rust-target absence or denial fixtures. |
| Core reads | Health, auth session, bootstrap, session list/detail, client session list/detail, output, events state, SSE hello, nodes list, queue jobs list/detail, Codex review requests list/detail, and tool/audit read projections are implemented. |
| Core runtime | Session/tmux/spawn/session-graph/message-queue/task-complete/input-batch/subagent and retained review-route slices are merged, with shadow and contract fixtures covering the early cutover path. |
| Codex retained reads | Codex event stream, pending request ledger reads, activity actions, review detail/list, and Claude/Codex tool-call projections are implemented and covered by manifest checks. |
| Mobile | Native bootstrap/session support, attach-ticket proofing, terminal WebSocket auth/bridge, runtime disable, device revoke/list CLI support, public-edge assertion validation, Android client-certificate storage, Rust `sm enroll-device`, Camera-app deep-link enrollment, and Rust Google device-auth bearer issuance are merged. |
| Cloudflare Access origin gate | Config parsing, JWT/audience classification, host/app separation, public-host fail-closed behavior, mobile/app route gating, app artifact gating, device-token exchange, Google JWKS cache refresh/TTL behavior, mobile Access CN actor binding, Cloudflare mTLS CA upload/hostname association, per-device Common Name policy sync, and read-only smoke evidence collection are merged. |
| External fallback | Email/human fallback delivery and inbound email validation path are retained in the Rust track. |
| Queue and nodes | Queue list/detail, CLI list/status, pending submit, simple execute/cancel, queue recovery, queue fixtures, node reads, and node HTTP routes are implemented; remaining work is final live/recovery cutover evidence and any retained node-agent remote-control gaps. |

## Current Live Shadow And Contract State

Live Python-authoritative Rust shadow mode is active in the local config:

- Python origin: `:8420`;
- Rust sidecar: `127.0.0.1:8421`;
- shadow ledger: `~/.local/share/claude-sessions/rust_shadow.jsonl`;
- config backup created by the activation helper:
  `config.yaml.shadow-backup-20260612T023248Z`.

The latest clean passive shadow report after #996 used
`--last-minutes 30 --fail-on-blockers --json` and returned:

| Metric | Result |
| --- | ---: |
| Status | passed |
| Rows | 2,238 |
| Blockers | 0 |
| `GET /events/state` | 861 status matches |
| `GET /sessions` | 1,377 status matches |
| Invalid rows | 0 |

This passive window did not satisfy the new mobile route/pattern coverage gates
from PR #942; those gates need real native app traffic or explicit operator
exercise. It also does not include Cloudflare Access proof inputs, so it is not
public-edge cutover evidence.

After PR #1025, the Rust sidecar was rebuilt and restarted from current `main`
on `127.0.0.1:8421`, then the Cloudflare/mobile smoke runner was executed with
`--mobile-host sm-app.rajeshgo.li` and `--browser-host sm.rajeshgo.li`. The
blocked evidence file is:

```text
.local/rust-mvp-rehearsals/post-1025-mobile-auth-smoke-blocked.json
```

That run passed `mobile.bootstrap_requires_access`, proving the Rust origin
denies mobile bootstrap requests on the app host when no Cloudflare Access
assertion is present. It remains blocked because the shell did not provide the
Access JWTs, public-edge HMAC secret, or SM bearer/cookie needed for success
checks. Summary: `1` passed, `5` blocked, `7` skipped. This is partial boundary
evidence only, not full mobile auth smoke evidence.

The local shell used for the post-#1025 smoke run did not have the
Cloudflare/mobile proof inputs set. The hostnames were supplied:

| Input | Status |
| --- | --- |
| `CF_MOBILE_ACCESS_JWT` | unset |
| `CF_BROWSER_ACCESS_JWT` | unset |
| `SM_PUBLIC_EDGE_SECRET` | unset |
| `SM_DEVICE_BEARER_TOKEN` | unset |
| `SM_COOKIE` | unset |
| `--mobile-host` | `sm-app.rajeshgo.li` |
| `--browser-host` | `sm.rajeshgo.li` |

Run the Cloudflare Access smoke runner after secret inputs are supplied through
environment variables or secret files. Missing required mobile inputs are
blockers by design and should not be recorded as passing evidence.

The fixture-filtered broad live Rust contract run now passes without synthetic
fixture false failures:

| Metric | Result |
| --- | ---: |
| Passed | 71 |
| Skipped | 3 |
| Failed | 0 |

The skipped checks are mutating checks without `--include-mutating`, which is
the expected safety behavior for a live-state read run.

Current executable manifest size after PR #1035:

| Metric | Count |
| --- | ---: |
| Total checks | 134 |
| `python_and_rust` checks | 73 |
| `rust_only` checks | 61 |
| Read-only checks | 93 |
| Mutating checks | 41 |

The latest focused isolated-sidecar rehearsal before the post-#1035 run is:

```bash
/tmp/session-manager-pr981-rehearsal-ports/mvp-rehearsal-report.json
```

It was run with `--skip-python-health --skip-baseline --skip-shadow --core-only`
and `--rust-base-url http://127.0.0.1:18421` to avoid an existing local sidecar
on port `8421`. Results:

| Area | Result |
| --- | --- |
| Overall status | Passed with 0 blockers |
| State ownership gate | Passed: 17 stores checked, 13 existing, 13 copied, 13 verified, 13 restored, freeze/drain ledger written |
| Rust live core sidecar contracts | 17 passed, 0 failed, 0 skipped |
| Rust synthetic read-only fixture contracts | 14 passed, 0 failed, 0 skipped |
| Rust mutating fixture contracts | 35 passed, 0 failed, 0 skipped |

This is not a full cutover rehearsal because it skips Python baseline and
shadow. It remains useful historical fixture/state-gate evidence for the
post-#984 contract set; newer post-#1035 evidence is below.

## Latest Real-State Rehearsal

Report:
`.local/rust-mvp-rehearsals/20260612T-full-after-938/mvp-rehearsal-report.json`

Summary:

| Area | Result |
| --- | --- |
| Overall status | Passed with 0 blockers |
| Python health | Passed |
| State ownership gate | Passed: 17 stores checked, 13 existing, 13 copied, 13 verified, 13 restored, freeze/drain ledger written |
| Rust sidecar health | Passed using explicit `--reuse-rust-sidecar` because port 8421 was already healthy |
| Isolated runtime smoke | Passed |
| Rust live core sidecar contracts | 17 passed, 0 failed, 0 skipped |
| Rust synthetic read-only fixture contracts | 10 passed, 0 failed, 0 skipped |
| Rust mutating fixture contracts | 30 passed, 0 failed, 0 skipped |
| Gap probes | 0 failed |
| Python baseline | Passed |
| Rust baseline | Passed |
| Shadow read summary | 8 passed, 0 failed |

The earlier blocker was a body mismatch for `GET /events/state` in shadow
comparison. PR #898 reclassified that route as status-only because Python
carries live tmux-client event state that a fresh Rust sidecar does not own.
After PR #936, live session detail is also status-only for shadow comparison to
avoid lifecycle TOCTOU noise while fixture-backed deterministic session detail
checks remain body/assertion based. The clean run now passes the shadow summary
for `/health`, `/health/detailed`, `/auth/session`, `/client/bootstrap`,
`/sessions`, `/client/sessions`, `/nodes`, and `/events/state`.

Measured baseline snapshot from the same run:

| Metric | Python | Rust |
| --- | ---: | ---: |
| RSS | 154.672 MiB | 19.797 MiB |
| Physical footprint | 66.4 MiB | 6.688 MiB |
| `GET /health` median | 4.167 ms | 0.275 ms |
| `GET /health/detailed` median | 1078.139 ms | 0.294 ms |
| `GET /auth/session` median | 18.825 ms | 0.268 ms |
| `GET /client/bootstrap` median | 6.622 ms | 0.298 ms |
| `GET /events/state` median | 5.017 ms | 0.288 ms |
| `GET /sessions` median | 25.746 ms | 7.972 ms |
| `GET /client/sessions` median | 58.486 ms | 7.946 ms |

## Latest Post-1035 Rehearsal Attempt

Reports:

- `.local/rust-mvp-rehearsals/20260616T215701Z-post-1035/mvp-rehearsal-report.json`
- `.local/rust-mvp-rehearsals/20260616T220122Z-post-1035/mvp-rehearsal-report.json`
- `.local/rust-mvp-rehearsals/20260616T221117Z-post-1035/mvp-rehearsal-report.json`

The latest run is the current snapshot. It includes the contract harness fix
that raised the default HTTP read budget above the old 64 KiB limit; before that
fix, live `/client/sessions` responses were valid JSON but the harness truncated
them before JSON assertion checks.

| Area | Result |
| --- | --- |
| Overall status | Blocked with 2 blockers |
| Python health | Passed at start |
| State ownership gate | Passed: 17 stores checked, 14 existing, 14 copied, 14 verified, 14 restored, freeze/drain ledger written |
| Rust sidecar health | Passed using fresh sidecar |
| Isolated runtime smoke | Passed |
| Rust live core sidecar contracts | 17 passed, 0 failed, 0 skipped |
| Rust synthetic read-only fixture contracts | 14 passed, 0 failed, 0 skipped |
| Rust mutating fixture contracts | 37 passed, 0 failed, 0 skipped |
| Gap probes | 0 failed |
| Python baseline | Failed: Python origin refused connections after the first health probe |
| Rust baseline | Passed: 19.359 MiB RSS, 8.125 MiB physical footprint |
| Mobile shadow path resolution | Skipped: Python `/sessions` was unavailable |
| Shadow read summary | Failed: 9 requested read probes failed with connection refused |

This is not a Rust contract regression: the Rust/state/fixture portions passed.
The blocker is Python-authoritative origin availability during the baseline and
shadow steps. Logs around the latest run show the Python watchdog reporting an
event-loop freeze and killing the process for restart; after the kill, port
`8420` stopped accepting connections. The normal cutover-evidence path still
needs Python to remain up through baseline and shadow, including the mobile read
probes from PR #942. If Python-origin availability remains the blocker, use the
accelerated Rust canary path instead of broad Python hardening.

## Accelerated Rust Canary Evidence Path

Issue #1038 adds an explicit owner-approved canary mode for the case where Rust
passes state/fixture/core gates but Python cannot stay healthy long enough to be
the sustained shadow authority. The mode is intentionally narrower than the
normal full rehearsal and should be identified as such in reports.

Required command shape:

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

Blocking evidence in this path:

- `python_canary_spot_checks` for a short authority sanity set;
- state preflight, backup, restore rehearsal, and freeze/drain ledger;
- Rust live core contracts and synthetic read-only/mutating fixture contracts;
- Rust baseline;
- `cloudflare_access_smoke_report` from a passed smoke report using real
  Access/public-edge/SM auth inputs.

The mode intentionally skips sustained `python_baseline` and
`shadow_read_summary`; those steps must be recorded as skipped, not silently
omitted. A canary report with missing or blocked Cloudflare smoke is not
cutover-candidate evidence.

## Near-Term Remaining Work

These are the next practical buckets before an MVP cutover trial:

| Bucket | Why it remains |
| --- | --- |
| Shadow/canary observation window | Python-authoritative shadow mode is enabled and a short clean window has been recorded, but mobile gates from PR #942 still need real app/operator traffic. Prefer a longer agreed shadow window when Python stays healthy; otherwise use `--rust-canary-cutover` with Python spot checks plus Rust/state/Cloudflare gates and triage any blockers before Rust becomes the writer. |
| Full fixture manifest execution | The MVP rehearsal now runs the current synthetic read-only and mutating fixture sets, including review-route fixtures. Remaining work is final live mobile/device fixture evidence and any additional retained fixture rows added by later slices. |
| CLI cutover audit | Verify every retained CLI command in [cutover_scope.md](cutover_scope.md) is native Rust or intentionally routed, and every removed command is absent or explicitly retired. |
| State ownership and migration tooling | Initial preflight, backup, restore, and freeze/drain evidence tools are merged and exercised by the rehearsal. Remaining work is live write-admission freeze/journal ownership and rollback accounting from [state_ownership_and_migration.md](state_ownership_and_migration.md). |
| Cloudflare Access deployment integration | Pair the merged Rust origin gate with the actual Cloudflare Access apps, mTLS/service-auth policies, device Common Name enrollment/revocation, app-host native auth smoke, browser OAuth smoke, and later node fallback tests. Current evidence lives in [cloudflare_access_cutover_evidence.md](cloudflare_access_cutover_evidence.md). |
| Node and queue writer completion | Basic node HTTP routes and narrow queue writer/recovery are implemented. Remaining work is final live/recovery cutover evidence, audit/rollback accounting, and any retained node-agent remote-control gaps. |
| Service packaging and rollback | Exercise launchd/service cutover, non-destructive port ownership, health checks, rollback, and operator diagnostics. |
| Final native mobile smoke | Run bootstrap/session/attach/request-status/analytics/bug-report/app-artifact smoke checks against Rust using real mobile assumptions. |
| Python-origin availability during evidence runs | Post-#1035 full rehearsal is blocked because Python watchdog-restarted during baseline/shadow. This does not call for broad Python hardening. Either collect the normal stable Python-authoritative window, or use the accelerated Rust canary evidence path with spot checks and Cloudflare/mobile smoke. |

## Stop Conditions For MVP Cutover

Do not start an MVP cutover until:

- retained manifest checks pass for Python and Rust on the selected fixture set;
- either shadow mode shows no unexplained mismatches on retained core reads for
  the agreed observation window, or an accelerated Rust canary report records
  passing `python_canary_spot_checks`, Rust/state/fixture gates, Rust baseline,
  and Cloudflare/mobile smoke;
- final backup happens after write admission is frozen or journaled;
- public traffic has proof-of-possession before origin plus origin auth/capability checks;
- Cloudflare Access browser/mobile/node/email route classes are configured with no bypass/broad-certificate policy and verified against Rust origin gates;
- mobile attach, request-status, bug reports, and app artifacts pass smoke checks;
- rollback restores or explicitly journals every accepted write after the restore point.
