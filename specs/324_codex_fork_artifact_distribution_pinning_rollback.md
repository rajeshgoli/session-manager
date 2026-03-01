# sm#324: codex-fork artifact distribution, pinning, and rollback

## Scope

Implement launch-path operator controls for codex-fork artifact lifecycle:

1. Build/publish workflow for supported platforms.
2. Session Manager pinning to explicit fork release/ref.
3. Operator reporting for active fork + schema contract.
4. Rollback command path and runbook.

## Build + publish path

Use `scripts/codex_fork/release_artifacts.sh`:

1. Input: codex-fork checkout path, release label, immutable ref.
2. Build targets:
   - `aarch64-apple-darwin`
   - `x86_64-apple-darwin`
   - `x86_64-unknown-linux-gnu`
3. Output:
   - per-target tarballs
   - `manifest.json` containing release + immutable ref
4. Optional publish:
   - pass `<owner>/<repo>` to create/upload a GitHub release via `gh release create`.

## Pinning contract

`config.yaml` / `config.yaml.example` `codex_fork` keys:

1. `artifact_release`: release channel/tag identifier.
2. `artifact_ref`: immutable commit/tag pin consumed by operators.
3. `artifact_platforms`: expected platform matrix.
4. `rollback_provider`: emergency provider target.
5. `rollback_command`: operator rollback command.
6. `event_schema_version`: control/event contract version pin.

Session Manager now surfaces this via:

1. API: `GET /admin/codex-fork-runtime`
2. CLI: `sm codex-fork-info` (`--json` supported)

## Operator rollback path

1. Detect active pin/schema:
   - `sm codex-fork-info`
2. Execute rollback command:
   - default `sm codex`
3. If needed, repin to previous known-good release/ref in config and restart service.

## Acceptance mapping

1. Build/publish path implemented: `scripts/codex_fork/release_artifacts.sh`.
2. Explicit pinning fields implemented: `codex_fork.artifact_release`, `codex_fork.artifact_ref`.
3. Operator reporting implemented: `sm codex-fork-info`, `/admin/codex-fork-runtime`.
4. Rollback implementation path documented in README + this spec.
