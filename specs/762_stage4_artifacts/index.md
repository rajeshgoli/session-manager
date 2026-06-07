# Stage 4 Artifact Bundle

Status: converged after three sequential independent reviewer convergence signals; owner security feedback incorporated after convergence.

This bundle supports Stage 4 of [762_rust_migration_and_ruggedization.md](../762_rust_migration_and_ruggedization.md). Stage 4 turns the converged Stage 2 surface inventory and Stage 3 behavior handoff into threat-model, hardening, and cutover-decision requirements for the Rust migration.

| Artifact | Purpose |
| --- | --- |
| [threat_register.md](threat_register.md) | Threat scenarios, current controls, required Rust mitigations, residual risks, and Stage 5 cutover dispositions. |
| [route_local_secret_matrix.md](route_local_secret_matrix.md) | Missing/mismatch/reuse/logging/rotation behavior for route-local secrets and related auth tokens. |
| [hardening_backlog.md](hardening_backlog.md) | Hardening work, accepted cutover reductions, observability requirements, and Stage 5 handoff gates. |
| [public_edge_proof_model.md](public_edge_proof_model.md) | Owner-preferred Cloudflare/public-edge proof-of-possession boundary, residual attacks, fail-closed behavior, and mobile/node revocation requirements. |

## Handoff Rule

Stage 4 originally did not authorize breaking changes by itself. Stage 5 now records the owner-approved cutover scope and supersedes old candidate wording for the first Rust release.

Native mobile attach remains first-class and high priority. Generic public browser/watch operational data is removed from the Rust target. Email/human recipient delivery and inbound email stay retained as the fallback external channel after Telegram removal, with worker proof, sender allowlists, and explicit route allowlisting. Registered-node public fallback is accepted only with node proof-of-possession when LAN `studio.local` is unavailable.
