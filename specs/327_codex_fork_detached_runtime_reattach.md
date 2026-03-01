# sm#327: codex-fork detached runtime + reattach

## Scope

Post-launch codex-fork runtime/reattach architecture for operator attach workflows.

## Implemented architecture

### Detached runtime ownership model

`SessionManager` now maintains codex-fork runtime ownership metadata:

1. `codex_fork_runtime_owner[session_id]`
2. owner populated on session create/restore
3. owner removed on session kill

Runtime remains detached from any one terminal and is referenced by stable runtime ID:

1. `runtime_id = codex-fork:<session_id>`

### Attach protocol

New API:

1. `GET /sessions/{id}/attach-descriptor`

Descriptor includes:

1. provider attach support
2. attach transport (`tmux`)
3. runtime mode (`detached_runtime` for codex-fork)
4. runtime ID/owner
5. lifecycle state/cause snapshot
6. control socket and event stream paths

CLI `sm attach` now consumes this descriptor and reattaches to the same live runtime without starting a new session.

### Reliability coverage

Added tests verify:

1. codex-fork attach descriptors preserve waiting lifecycle states
2. headless codex-app sessions are explicitly non-attachable
3. `sm attach` uses detached runtime descriptors for codex-fork reattach path

## Acceptance mapping

1. Detached ownership model: runtime owner + stable runtime ID metadata.
2. Reattach protocol: attach descriptor API + CLI attach integration.
3. Reliability checks: active/waiting reattach metadata tests.
