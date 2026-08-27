# #1400 — Safe Spawn Permission Modes

## Decision

Session Manager will launch interactive Claude sessions with
`--permission-mode auto` and interactive Codex sessions with
`--approve-for-me`. These are the providers' supported automatic modes and
replace the current unsafe bypass flags.

`codex_fork` inherits Codex launch arguments when it does not declare its own
arguments, so it will also receive `--approve-for-me`. Its managed startup
update-disable argument remains unchanged.

The change does not alter `codex.app_server_args`; interactive Codex options
must not be passed to `codex app-server`.

## Verification

Configuration tests must prove the safe defaults are selected for Claude,
Codex, and the Codex-fork fallback. Existing configuration parsing tests cover
explicit operator overrides.

## Classification

Single ticket: this is a focused provider launch-default change with bounded
configuration and regression-test scope.
