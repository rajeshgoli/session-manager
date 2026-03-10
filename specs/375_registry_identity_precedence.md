# sm#375: registry identity precedence over friendly names

## Scope

Make the agent registry the canonical identity layer for system surfaces.

## Design

1. Registry roles are authoritative for routing and display.
2. Effective display precedence is:
   - primary registry role
   - `friendly_name`
   - internal session name / ID
3. Reserved canonical aliases like `maintainer` cannot be claimed as free-form friendly names by unrelated sessions.
4. Once a session has a registry role, conflicting `friendly_name` updates are rejected instead of creating two different visible identities.
5. System-generated labels should prefer the canonical registry identity:
   - `sm send` sender labels
   - API session payloads
   - registry/children/watch surfaces
   - Telegram/notification headers
   - queue/watch/compaction notices

## Implementation choice

Preserve the underlying user-provided `friendly_name` in state, but never let it override a canonical registry identity. This avoids discarding a previous human label when a role is registered, while still making the registry the only visible identity until the role is removed.

## Acceptance mapping

1. Registry alias wins in user-visible/system-generated labels.
2. Reserved aliases are rejected for unrelated friendly-name updates.
3. Registry-owned sessions reject conflicting friendly-name changes.
4. Telegram/status-bar sync uses the canonical display name after alias changes.

## Classification

Single ticket.
