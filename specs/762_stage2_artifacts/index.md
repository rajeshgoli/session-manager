# Stage 2 Artifact Index

Generated: 2026-06-06T12:42:08-07:00

Provenance commands:

- `python3 - <<PY (generated all files in specs/762_stage2_artifacts)`

Reconciliation status: source-derived pass 6 for Stage 2 convergence review. Rows marked manual or supplemental are extracted from source patterns that are not directly represented by decorators, argparse metadata, or local SQLite files.
| artifact | path | status |
| --- | --- | --- |
| Route manifest | ./route_manifest.md | generated, linked, pass-2 reconciled |
| Route auth matrix | ./route_auth_matrix.md | generated, linked, pass-2 reconciled |
| CLI manifest | ./cli_manifest.md | generated and linked |
| Protocol manifest | ./protocol_manifest.md | generated, linked, pass-2 reconciled |
| External client manifest | ./external_client_manifest.md | generated, linked, pass-3 reconciled |
| Config manifest | ./config_manifest.md | generated, linked, pass-6 reconciled |
| Persistence manifest | ./persistence_manifest.md | generated, linked, pass-3 reconciled |
| Schema manifest | ./schema_manifest.md | generated, linked, pass-3 reconciled |
| Usage telemetry report | ./usage_telemetry_report.md | generated, linked, pass-3 reconciled |

Reconciliation summary:

- Route artifacts include 126 rows: 123 decorators, 2 dynamic registrations, 1 mounted static route. `/watch` rows are marked as mutually exclusive built-vs-missing runtime behavior, and inbound email rows distinguish default `/api/email-inbound` from configured aliases.
- CLI artifact includes 73 command/subcommand paths.
- Auth dimensions are applied in `route_auth_matrix.md`; other classification dimensions are applied to route rows and summarized for protocols/clients.
- Generated manifests still intentionally over-include raw code-read keys where automated extraction cannot distinguish config from payload fields. Verified server/local-env/nodes/client/source-defined config sections and the exact example-default summary are the Stage 2 config contracts; raw `.get()` and key-path rows are discovery evidence unless later promoted.
- Config manifest now records the YAML-plus-local-env overlay for `.local/android-parity/values.env`, including public host, Google auth, Android device auth, cloudflared SSH attach, and session-cookie-secret derivation behavior.
- Config manifest now records `nodes.*` remote-placement config as a security-sensitive trust-boundary contract and promotes source-defined non-example defaults for response relay, tool logging, mobile analytics, Codex session index/codex-fork IPC, Codex app-server metadata, infrastructure supervisor, watchdog, maintenance loops, and additional tmux/output-monitor timing.
- Persistence manifest now includes a per-store compatibility classification handoff for durable DBs, JSON state, app artifacts, logs, lock/worktree state, codex-fork IPC artifacts, runtime sockets, and tmux/log paths.
- Usage telemetry includes retained `RequestTimingMiddleware` server timing logs as threshold-biased positive route evidence, not complete access-log counts.
- Watch `/api/sessions` is classified as a client-side fallback probe; current `src/server.py` does not expose it as a server route.
- No generated artifact authorizes breaking changes. Candidate removals/consolidations remain Stage 4/5 and user-review decisions.
