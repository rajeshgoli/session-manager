# sm#325: launch rollout gates, hardening, and migration docs

## Scope

Implement operator-verifiable launch gates and publish migration/hardening guidance for codex-fork cutover.

## Implemented gates

Session Manager exposes launch gates via:

1. API: `GET /admin/codex-launch-gates`
2. CLI: `sm codex-rollout-gates` (`--json` supported)

Gate set:

1. `a0_event_schema_contract`
2. `launch_rollout_flags`
3. `launch_artifact_pin`
4. `launch_codex_app_drain`
5. `launch_provider_mapping_phase`

Each gate includes deterministic `ok` + `details` fields for automation.

## Hardening checks

The launch gate payload also includes:

1. `provider_counts` (including active `codex-app` session count)
2. rollout flags snapshot
3. codex-fork runtime pin metadata
4. provider mapping policy snapshot

Operators can rehearse rollback using `sm codex-fork-info` metadata:

1. capture current pin + schema
2. execute rollback command
3. re-run `sm codex-rollout-gates` to confirm stable fallback posture

## Migration documentation

README now includes:

1. codex-fork pin + rollback runbook
2. codex launch gate check list and interpretation guidance

## Acceptance mapping

1. Gate checks are wired and observable (`/admin/codex-launch-gates`, `sm codex-rollout-gates`).
2. Hardening path exists with provider-count and rollout snapshots.
3. Migration docs are published in README + this spec.
