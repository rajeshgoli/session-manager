# Queue list scope disclosure

Issue: #1319

`sm queue list` already requests pending and running rows for the current
notify target. `--all` changes the projection to terminal-inclusive history
and, absent `--notify`, removes that target filter. The command previously
presented each result without saying which projection the user received.

This change adds human-output scope text only. The default query, running-row
semantics, JSON array shape, and `--all` behavior remain unchanged. The text
identifies the active notify target scope and points users to `--all`; it also
correctly describes `--all --notify` as per-target history rather than fleet
history.

## Required proof

- Unit coverage for default active-scope, filtered, per-target history, and
  fleet history labels.
- Existing CLI/API list projections retain pending and running behavior.

## Ticket classification

Single ticket.
