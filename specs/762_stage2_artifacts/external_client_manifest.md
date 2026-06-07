# Stage 2 External Client Manifest

Generated: 2026-06-06T12:42:08-07:00

Provenance commands:

- `rg -n "/api/sessions|sm-terminal\.local|terminal\.html|xterm|client/sessions|attach-ticket|maintainer/ensure" web/sm-watch android-app src examples/cloudflare`
- `python3 - <<PY (regex scan of Android ApiService, watch UI constants/types, terminal assets, Telegram handlers, Cloudflare worker)`

Reconciliation status: source-derived pass 3 for Stage 2 convergence review. Rows marked manual or supplemental are extracted from source patterns that are not directly represented by decorators, argparse metadata, or local SQLite files.
## Android Retrofit endpoints

| method | path | function | return model | special headers | query params | body model |
| --- | --- | --- | --- | --- | --- | --- |
| GET | /client/bootstrap | getBootstrap | ClientBootstrapResponse | - | - | - |
| GET | /client/analytics/summary | getAnalyticsSummary | AnalyticsSummary | - | - | - |
| GET | /apps/{app}/meta.json | getAppArtifactMetadata | AppArtifactMetadata | - | - | - |
| GET | /auth/session | getAuthSession | AuthSessionResponse | - | - | - |
| POST | /auth/device/google | exchangeGoogleToken | DeviceGoogleAuthResponse | - | - | DeviceGoogleAuthRequest |
| GET | /client/sessions | getClientSessions | SessionListResponse | - | - | - |
| GET | /client/sessions/{session_id} | getClientSession | ClientSession | - | - | - |
| POST | /client/sessions/{session_id}/attach-ticket | createMobileAttachTicket | MobileAttachTicketResponse | X-SM-Device-Key-Id, X-SM-Device-Timestamp, X-SM-Device-Nonce, X-SM-Device-Signature | - | MobileAttachTicketRequest |
| POST | /client/request-status | requestStatus | RequestStatusResponse | - | - | - |
| POST | /maintainer/ensure | ensureMaintainer | EnsureMaintainerResponse | - | - | EnsureMaintainerRequest |
| GET | /sessions/{session_id}/output | getSessionOutput | OutputResponse | - | lines | - |
| GET | /sessions/{session_id}/tool-calls | getToolCalls | ToolCallsResponse | - | limit | - |
| GET | /sessions/{session_id}/activity-actions | getActivityActions | ActivityActionsResponse | - | limit | - |
| POST | /sessions/{session_id}/kill | killSession | KillSessionResponse | - | - | KillSessionRequest |

Android terminal WebView local asset contract:

| host/url | asset path |
| --- | --- |
| https://sm-terminal.local/terminal.html | android-app/app/src/main/assets/sm_terminal/terminal.html |
| /terminal.html | sm_terminal/terminal.html |
| /vendor/xterm.css | sm_terminal/vendor/xterm.css |
| /vendor/xterm.js | sm_terminal/vendor/xterm.js |
| /vendor/addon-fit.js | sm_terminal/vendor/addon-fit.js |

Watch UI API path fallbacks from `web/sm-watch/src/App.tsx`: `/client/sessions`, `/sessions`, `/api/sessions`, `/sessions/{id}/kill`, `/client/bug-reports`, `/maintainer/ensure`, `/watch/`.

`/api/sessions` is a watch-client probe only in the current source. `src/server.py` and `route_manifest.md` do not expose a current `/api/sessions` route. Preserving current Python behavior means preserving the fallback probe order and failure behavior, not silently adding a new server route. If Stage 5 chooses a Rust compatibility shim for `/api/sessions`, it must add route-manifest, route-auth-matrix, and fixture coverage.

Watch UI exported interfaces from `web/sm-watch/src/types.ts`: AdoptionProposal, ToolCallRow, ActivityActionRow, SessionDetail, AttachDescriptor, TermuxAttach, PrimaryAction, Session, EnsureMaintainerResponse, WatchSection, WatchSessionNode, WatchRepoRef, WatchRow

Cloudflare email worker headers: x-email-session-id, x-email-worker-secret. Payload fields observed: from_address, raw_email.

Telegram commands: /follow, /force, /help, /kill, /list, /message, /name, /new, /open, /password, /session, /start, /status, /stop, /subagents, /summary. Callback regexes: ^new_project:, ^follow:, ^perm:.

Reconciliation notes:

- Android `/maintainer/ensure` is a current server route consumed by the Android client.
- Watch fallback `/api/sessions` is an explicit client-side probe, but current `src/server.py` does not expose it as a server route.
- Native Android app and mobile terminal attach are first-class Rust targets per owner priority.
