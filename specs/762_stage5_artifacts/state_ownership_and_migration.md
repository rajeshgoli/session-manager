# Stage 5 State Ownership And Migration

Status: converged after three sequential independent reviewer convergence signals.

## Ownership Rules

- One active writer owns each durable store at a time.
- Python remains the owner until Rust passes preflight and cutover begins.
- Rust rehearsal uses copied real state, never the live stores.
- Final rollback state is captured only after a write-admission freeze and active-writer drain. A pre-freeze safety backup is not sufficient as the default rollback restore point.
- Write-freeze coverage must be generated from this whole table and the Stage 2 persistence manifest. Stores are not exempt from freeze/drain just because they are audit, telemetry, observability, or support diagnostics.
- Dual-write is rejected by default. It requires a separate design, contract tests, and crash-window analysis.
- One-way or destructive migrations require explicit user review, backup rehearsal, and downgrade behavior.

## Store Handoff Table

| Store / Artifact | First Rust Release Behavior | Backup / Rehearsal | Rollback / Downgrade |
| --- | --- | --- | --- |
| `sessions.json` and legacy session state | Must read/write retained session fields compatibly, including stopped records, provider ids, tmux names, role/maintainer/context/mobile state, and node mapping. Deprecated Telegram topic fields are archive/rollback-only. | Copy real state; validate hydrate output against Python `/sessions` and `/client/sessions` fixtures for retained fields. | Restore backup if Rust writes incompatible retained state; no lossy rewrite of archive fields without a migration ledger entry. |
| tmux sessions, sockets, logs, attach descriptors | Preserve names, sockets, provider mapping, dead-pane cleanup, attach descriptors, and output/tail behavior. Termux command metadata is deprecated and excluded from the Rust target. | Rehearse against copied state and a test tmux namespace where possible. | Existing tmux sessions must remain attachable by Python after rollback; rollback may restore deprecated Termux metadata only as Python behavior. |
| message queue DB | Must preserve retained messages, parent wakes, notify-on-stop delivery, and Codex review request registrations. Scheduled reminders, remind registrations, and job watches are archive/rollback-only and are not active Rust features. | Copy DB; run recovery and queue-depth checks for retained delivery; record any deprecated rows in the migration ledger. | Restore DB atomically if Rust claims/updates messages incorrectly. No mixed ownership. |
| tool usage DB and Telegram telemetry | Must preserve audit schema and destructive/sensitive classification visibility. Telegram telemetry rows are archive/rollback-only. | Freeze or journal tool audit inserts; drain asynchronous logger tasks; copy DB; compare `/tool-calls` queries. Archive Telegram telemetry without creating a Rust writer. | Rust audit additions may be retained only if Python can ignore them or backup restore is used. Rollback must not silently drop accepted audit writes after the final restore point. |
| response relay DB | Must preserve inbound turns, assistant outputs, claim/dedupe/release, and accepted-only mark-relayed semantics. | Copy DB; replay relay fixtures from Stage 3. | Restore backup if relay state diverges; avoid duplicate external notifications during rollback. |
| email/human delivery config and inbound email state | Must preserve retained email/human recipient config, authorized senders, worker secret/header names, trusted-session-header rules, and email response-relay source semantics. | Freeze or journal inbound email admission and outbound email/human delivery attempts; run fake-notifier and worker-payload fixtures against copied config/state. | Rollback must not lose accepted inbound email deliveries or duplicate outbound notifications without operator-visible replay/discard notes. |
| Codex events and observability DBs | Must preserve event ids, provider cursors, positions, assistant relays, reducer state, and observability rows. | Copy DBs; validate cursor monotonicity and replay/skip fixtures. | Cursor reset or event discard requires user review; rollback must not replay provider events unexpectedly. |
| Codex requests DB | Must preserve pending/orphaned/resolved states and process-generation startup orphaning. | Copy DB; validate include-orphaned API and provider waiter behavior. | Before rollback, drain or restore pending requests so Python does not hang on Rust-owned state. |
| queue runner DBs and per-job directories | Must preserve retained queue job states, resource samples, submitted scripts, logs, cancellation, and notification state. Policy runs/results and CI-helper state are archive/rollback-only. | Copy queue-runner root; run dry-run list/status/admission checks for retained narrow queue. | Active jobs should be drained before cutover; rollback cannot safely adopt Rust-started jobs without explicit design. |
| bug reports DB and attachments | Must preserve DDL, attachment paths, metadata, selected-session fields, create/prune behavior, and delivery-result updates. | Freeze or journal native app bug-report admission, prune, maintainer notification delivery-result updates, and attachment writes; copy DB and attachment root; validate report list/detail fixtures. | Restore backup if schema/path layout changes. Accepted bug reports after the restore point must be replayed from the journal or explicitly discarded with operator acknowledgement. |
| Telegram topics JSON | Archive/rollback-only. Rust does not run Telegram topic reconciliation or bot control. | Copy JSON for rollback and historical attribution only. | Restore mapping before Python rollback if Telegram is re-enabled by Python. |
| app artifacts and metadata | Must preserve latest APK, immutable hash APKs, metadata JSON, and uploaded_by attribution. Rust must add auth/proof or signing before public serving; legacy `/apk` may remain only as a proof/auth-gated alias. | Copy artifact root; validate hash/meta/latest fixtures plus proof/auth/signed serving behavior. | Restore artifact root atomically if Rust upload occurs before rollback. |
| config, `client.yaml`, local-env overlay, retained email config, retired config files | Must preserve YAML defaults, client env precedence, `.local/android-parity/values.env` overlay, auth enablement, node config, `config/email_send.yaml`, email bridge worker settings, human recipients, and secret redaction for retained keys. Telegram, dispatch, remind, Termux, and queue-policy config are parsed only enough to reject/ignore safely and preserve rollback files. | Run Rust config preflight against current files; no writes in rehearsal. | Do not rewrite config during cutover unless backed up and owner-approved. |
| node registry and runtime caches | Must preserve `primary`, node ids, SSH/proxy/control paths, hook/node secrets, restore inventory cache behavior, and `/nodes` redaction. | Rehearse node listing and remote inventory against configured nodes where safe. | Node agents must reconnect cleanly to Python after rollback; stale Rust control sockets must be cleaned. |
| codex-fork runtime artifacts | Must preserve event/control socket semantics while Rust owns runtime; old orphaned artifacts need cleanup policy. | Rehearse with copied logs/runtime paths when possible. | Stop Rust and remove Rust-owned sockets before Python resumes. |
| locks and worktree state | Must preserve auto-lock acquisition, conflict text, Stop-hook release, worktree-add tracking, and dirty-worktree prompt state. | Rehearse with disposable worktrees and copied lock state. | Rollback must not drop active locks or dirty-worktree warnings. |
| Python CLI and compatibility shims | HTTP-only commands may remain as clients. Direct read-only SQLite/file commands may remain only against compatible schemas and read-only handles. Direct writers such as lock/worktree commands must either route through Rust, remain explicitly CLI-owned with no Rust writer, or have a retirement gate. | CLI manifest fixtures must classify every retained Python command as HTTP-only, read-only local, CLI-owned writer, Rust-routed writer, or retired. | Rollback must restore the same CLI behavior and must not leave lock/worktree state split between Python and Rust. |
| launchd plist, wrapper, service logs | Must preserve service name, port, health checks, log paths, restart behavior, and rollback entrypoint. | Install into a staging label/path first where possible. | Restore Python plist/wrapper and verify `/health` and mobile bootstrap. |

## Migration Tool Requirements

The Rust migration tool must support:

- `preflight`: validate config, paths, schemas, permissions, port/service ownership, and auth policy without writing.
- `rehearse`: run migrations on copied state and emit a compatibility report.
- `safety-backup`: copy all must-preserve stores before write freeze for disaster recovery and record hashes/sizes.
- `freeze`: put Python into write-admission freeze, blocking or journaling new accepted writes while allowing read-only diagnostics.
- `drain`: stop or drain active writers and report residual active work by family.
- `final-backup`: copy all must-preserve stores after freeze/drain, record hashes/sizes, and prove no accepted live writes happened after the restore point except explicit journaled writes.
- `cutover`: claim ownership only after preflight, rehearsal, safety backup, freeze, drain, final backup, and operator confirmation gates pass.
- `rollback`: restore the previous owner and verify Python-compatible health.
- `status`: report current owner, backup root, migration generation, last gate passed, and rollback availability.

The tool must write a durable migration ledger. The ledger is itself a compatibility and audit artifact for cutover but should not be required by Python after rollback. It must record write-freeze start/end, blocked/journaled writes, active-writer drain result, final backup hashes, ownership handoff, rollback restore point, and whether any accepted write must be replayed or discarded by explicit operator decision.

## Downgrade Rules

- Backward-compatible additive schema changes are allowed only if Python ignores them safely.
- Renames, deletions, normalized enum changes, cursor resets, and token/signing-secret rotation are not downgrade-compatible unless a tested down-migration exists.
- If a store cannot be downgraded, the cutover plan must require explicit user review and a longer stabilization window before removing backups.
- Process-local state such as mobile attach tickets may be lost on restart because that is current behavior. Browser sessions and native device bearer tokens must not be invalidated without user approval.
