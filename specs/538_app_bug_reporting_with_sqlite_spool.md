# App Bug Reporting With SQLite Spool

Issue: #538

## Goal

Let the user submit a bug report directly from the SM app with minimal friction, capture enough state to make maintainer investigation effective, persist the report durably in a backend-owned SQLite spool with automatic eviction, and notify the maintainer immediately.

## Product constraints

- The report UX should be minimal: one freeform text box is sufficient.
- Optional debug-state capture should exist and default to enabled.
- Screenshot capture is useful but should not block v1.
- Reports should be stored in SQLite, not as file-per-report artifacts.
- Submission should notify maintainer automatically using the existing maintainer routing flow.

## Current state

- `web/sm-watch` already has authenticated session browsing, per-session actions, and mobile-oriented attach metadata.
- Session Manager already has durable local SQLite-backed components and maintainer routing via `sm send maintainer`.
- There is no app-facing bug report submission surface or backend spool for user-submitted bug reports.

## Target state

1. The app exposes a `Report bug` action that opens a lightweight submission UI.
2. The submission UI contains:
   - one multiline text field: `What went wrong?`
   - one checkbox, default on: `Include app debug state`
   - screenshot attachment support remains optional and can be omitted from v1
3. The backend accepts app bug reports through a dedicated authenticated endpoint.
4. Each report is stored in a SQLite spool with bounded retention and LRU-style eviction.
5. Maintainer receives an immediate routed message with the bug ID and a concise summary.
6. Maintainer can inspect the stored report data from a known local DB path without needing transient app state.

## Chosen architecture

### 1. One-step app submission UI

The app should expose a single entry point:

- global `Report bug` action from the app chrome
- optional session-scoped `Report bug` action when the user is looking at a specific session

The form should stay intentionally small:

- `report_text`: required freeform text
- `include_debug_state`: boolean, default `true`

Why:

- The maintainer workflow already assumes short human reports plus server-side investigation.
- More fields increase friction without meaningfully improving triage quality.
- The selected session and current route can be inferred from app context rather than asking the user to restate them.

### 2. Backend-owned SQLite spool

Persist reports in a dedicated local SQLite DB:

- `data/bug_reports.db`

Schema:

`bug_reports`
- `id TEXT PRIMARY KEY`
- `created_at TEXT NOT NULL`
- `report_text TEXT NOT NULL`
- `selected_session_id TEXT NULL`
- `route TEXT NULL`
- `app_version TEXT NULL`
- `artifact_hash TEXT NULL`
- `include_debug_state INTEGER NOT NULL`
- `client_state_json TEXT NULL`
- `server_state_json TEXT NULL`
- `status TEXT NOT NULL DEFAULT 'new'`
- `maintainer_delivery_result TEXT NULL`

`bug_report_attachments`
- `id INTEGER PRIMARY KEY`
- `bug_report_id TEXT NOT NULL`
- `kind TEXT NOT NULL`
- `mime_type TEXT NOT NULL`
- `payload BLOB NOT NULL`

Why SQLite:

- atomic writes
- simple concurrent reads/writes
- easy inspection/debugging from CLI and maintainer sessions
- avoids file-sprawl and per-report directory cleanup

### 3. LRU / bounded retention

Use size-bounded eviction after successful insert.

Initial policy:

- cap total report rows at `30`
- if row count exceeds cap, delete oldest rows first by `created_at`
- when attachments are present, delete their rows in the same transaction

This is effectively an oldest-first spool, which is good enough for v1 and matches the user request for LRU-style eviction without introducing a more complex “recently accessed” maintenance loop.

If later needed, add:

- byte-size cap
- “pin recent unsent failures”
- explicit maintainer archive/export flow

### 4. Debug-state capture

If `include_debug_state=true`, capture both client-visible and backend-visible state at submission time.

Client state payload:

- current route/view
- selected session id, if any
- expanded session ids
- current search/filter values
- current visible error/toast text
- last sync timestamp

Server state payload:

- current `/client/bootstrap`-equivalent external access/auth summary
- current session list snapshot
- selected session snapshot if `selected_session_id` is set
- compact health summary from `/health` or `/health/detailed`
- attach metadata if the selected session has mobile attach information

Why both:

- client state explains what the user was seeing
- server snapshot explains what Session Manager believed at the same moment

### 5. Maintainer notification

On successful insert, the backend should notify maintainer automatically.

Notification content should stay concise:

```text
[app bug] BR-20260407-abc123
report: attach goes blank, ctrl-c makes later attaches fail
session: f8b25fed
db: data/bug_reports.db
```

Implementation approach:

- use the same backend-side message delivery path already used for maintainer routing
- if maintainer is unavailable, keep the DB row with `maintainer_delivery_result=failed`

The notification is for triage only; the full state stays in SQLite.

## API contract

Add:

- `POST /client/bug-reports`

Request body:

- `report_text: string`
- `include_debug_state: boolean`
- `selected_session_id?: string`
- `client_state?: object`
- optional screenshot payload later

Response:

- `bug_id`
- `status`
- `maintainer_notified: boolean`

Auth:

- same authenticated app/browser session model as the rest of the `/client/*` surface

## App behavior

### Global report flow

Use for generic app bugs not tied to one session.

Captured automatically:

- current route
- global app filters/search state

### Session-scoped report flow

Use when the user reports an issue from a specific session row/detail panel.

Captured automatically:

- `selected_session_id`
- session snapshot
- attach metadata if present

This is especially useful for:

- Termux attach failures
- Telegram/thread mismatches
- stale state presentation bugs

## Screenshot support

Do not block v1 on screenshot capture.

Phase 1:

- text report + optional debug-state capture only

Phase 2:

- optional screenshot upload from app/native wrapper
- store screenshot bytes in `bug_report_attachments`

Reason:

- the state bundle is already enough for most maintainer investigations
- screenshot capture has more platform-specific complexity than the core reporting flow

## Operational workflow

1. User submits bug from app.
2. Backend stores report in `data/bug_reports.db`.
3. Backend evicts oldest rows if retention cap is exceeded.
4. Backend sends maintainer a short notification with bug id and summary.
5. Maintainer inspects the report row directly from SQLite and investigates against real state.

## Risks

- Debug-state capture can become too large if full session payloads are stored indiscriminately.
- Sensitive data may accidentally leak into stored JSON if we persist raw auth/session internals.
- Screenshot capture later will need explicit size limits and content-type validation.

## Mitigations

- Store compact server snapshots, not arbitrary full dumps of every backend object.
- Exclude secrets, cookies, tokens, and raw auth headers from persisted debug state.
- Add explicit per-report payload size limits.
- Keep screenshot support off until the text/state flow is proven useful.

## Recommendation

Implement the backend spool and the minimal app submission UI first. That gives immediate value with low UX friction and matches how maintainer already works today: terse reports plus strong backend investigation.

Ticket classification: single ticket
