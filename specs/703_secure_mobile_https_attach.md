# Secure Mobile HTTPS Attach

Issue: #703

## Summary

Add a secure, SM-owned mobile attach transport that works off-LAN without relying on Termux `cloudflared access ssh`.

The recommended v1 shape is:

1. The Android app asks Session Manager for a short-lived attach ticket for one existing session.
2. The app opens an authenticated terminal stream over the existing HTTPS origin, `sm.rajeshgo.li`.
3. Session Manager bridges that stream to the selected tmux-backed session only.
4. The existing Termux SSH path remains available as an operator fallback, but is no longer the primary mobile attach path.

Security is the primary design constraint. This feature exposes an interactive shell path to the machine. It must be implemented as a narrowly authorized, audited, session-scoped terminal bridge, not as a general remote command API.

## Problem

The current Android attach path launches Termux and asks Termux to run an SSH command through Cloudflare:

```sh
ssh -o ProxyCommand='cloudflared access ssh --hostname %h' ...
```

That fails off-LAN when the phone cannot establish the Cloudflare SSH websocket:

```text
websocket: bad handshake
Connection closed by UNKNOWN port 65535
```

PR #702 added a direct LAN SSH fallback. That helps only when the phone can reach the Mac LAN listener. It does not solve the real off-LAN case.

The app already talks to Session Manager over HTTPS for watch, status, and session detail. Attach should use that same authenticated HTTPS path instead of requiring a second Cloudflare SSH stack inside Termux.

## Goals

1. Make Android app attach work off-LAN through the existing Session Manager HTTPS origin.
2. Avoid exposing a generic shell, command execution endpoint, or arbitrary tmux target selector.
3. Require explicit, server-side authorization before any terminal bridge can start.
4. Use short-lived, single-use attach tickets so durable app credentials are not embedded in WebSocket URLs or terminal pages.
5. Audit terminal attach lifecycle events without logging sensitive terminal content by default.
6. Preserve existing desktop `sm attach` behavior.
7. Keep Termux SSH as an optional fallback/copy-command path for operators.

## Non-Goals

1. Do not implement a general browser SSH gateway.
2. Do not allow clients to spawn arbitrary processes or run arbitrary shell commands.
3. Do not let users attach to sessions they cannot already see/control through the authenticated app API.
4. Do not rely on Cloudflare Access SSH, Termux, or LAN reachability for the primary mobile attach path.
5. Do not stream raw terminal input/output into normal application logs.
6. Do not support headless `codex-app` terminal attach in v1.
7. Do not replace tmux as the runtime/control plane for Claude, Codex, and codex-fork sessions.

## Threat Model

The design must assume these threats are realistic:

1. An unauthenticated internet client finds the public SM hostname and tries to open terminal WebSockets.
2. An authenticated but non-authorized account attempts to mint an attach ticket.
3. A valid attach ticket leaks through browser history, proxy logs, crash reports, app logs, or screenshots.
4. A client tampers with session ids, tmux session names, socket names, resize values, or terminal frames.
5. A malicious webpage tries to trigger attach through cookies or browser ambient credentials.
6. A stale Android app or compromised network path replays an old attach ticket.
7. A terminal bridge process outlives the mobile connection and leaves a shell attached.
8. High-volume failed connection attempts create a denial-of-service path on the SM event loop.

## Security Requirements

### Authorization

Attach is allowed only when all checks pass:

1. The caller is authenticated through the existing `/client` auth model.
2. The caller maps to a configured human user with explicit `interactive_shell_access: true`.
3. The requested session is visible to that user through existing client session APIs.
4. The session attach descriptor reports `attach_supported=true`.
5. The session is tmux-backed and has a server-derived tmux target.
6. The session is currently running or attachable according to cached SM state.

Agents, email senders, Telegram senders, anonymous clients, and browser sessions without the explicit shell-access grant must not be able to mint attach tickets.

Recommended config shape:

```yaml
mobile_terminal:
  enabled: false
  allowed_users:
    - rajesh
  ticket_ttl_seconds: 30
  auth_frame_timeout_seconds: 3
  max_attach_seconds: 14400
  max_concurrent_attaches_per_user: 2
  max_concurrent_attaches_global: 8
  require_tls: true
```

Default should be disabled unless the deployment config explicitly enables it.

### Attach Tickets

Attach tickets are the only way to open a mobile terminal stream.

Endpoint:

```http
POST /client/sessions/{session_id}/attach-ticket
```

Response:

```json
{
  "ticket_id": "att_...",
  "ticket_secret": "...",
  "ws_url": "wss://sm.rajeshgo.li/client/terminal",
  "expires_at": "2026-05-03T00:00:00Z"
}
```

Ticket rules:

1. `ticket_secret` is at least 256 bits of randomness and is returned only once.
2. Store only a keyed hash of the secret server-side.
3. TTL defaults to 30 seconds.
4. Tickets are single-use and are consumed atomically.
5. A ticket is bound to user id, session id, provider, tmux session, tmux socket, client id, and creation time.
6. Expired, consumed, revoked, or mismatched tickets fail closed.
7. Cleanup expired tickets in bounded background maintenance, not on the hot path.

The ticket secret must not be placed in the WebSocket URL. URLs are too likely to appear in logs. The client sends the ticket id and secret in the first WebSocket frame after connecting.

### WebSocket Authentication

Endpoint:

```http
GET /client/terminal
```

The server accepts the socket only into a pending-auth state. The client must send an auth frame within `auth_frame_timeout_seconds`:

```json
{
  "type": "auth",
  "ticket_id": "att_...",
  "ticket_secret": "..."
}
```

The server then:

1. Validates and atomically consumes the ticket.
2. Re-runs authorization checks against current session state.
3. Starts the tmux bridge only after validation succeeds.
4. Closes the socket immediately on invalid auth, timeout, replay, or authorization failure.

Do not rely on browser cookies alone for WebSocket authorization. This prevents cross-site WebSocket abuse from ambient browser credentials.

### Transport Security

1. Production attach requires `wss://`.
2. Plain `ws://` is allowed only for localhost development when explicitly configured.
3. The server should validate `Origin` when present against configured app/web origins.
4. Missing `Origin` is acceptable for native OkHttp clients, but those clients still need a valid attach ticket.
5. Failed auth attempts are rate-limited by IP, user id when known, and global counters.

### tmux Bridge Safety

The bridge must never construct a shell command from client input.

Allowed bridge shape:

1. Resolve the attach descriptor server-side from `session_id`.
2. Use only server-owned `tmux_session` and `tmux_socket_name` from that descriptor.
3. Validate names against conservative character allowlists before passing them to subprocess argv.
4. Spawn `tmux` with an argv list, not `shell=True`.
5. Run only an attach/control-mode command for the existing session.
6. Kill the bridge subprocess when the WebSocket closes, errors, or exceeds `max_attach_seconds`.

The client may send terminal input and resize frames after auth, but it may not choose a process, command, socket path, or tmux target.

### Auditing And Privacy

Audit these events:

1. Ticket minted.
2. Ticket consumed.
3. Attach started.
4. Attach ended.
5. Auth failed.
6. Attach denied by policy.
7. Bridge process exited unexpectedly.

Audit fields:

1. Timestamp.
2. User id.
3. Session id.
4. Provider.
5. Remote address or coarse client fingerprint.
6. Result and reason.
7. Duration.
8. Input/output byte counts.

Do not log raw terminal input or output by default. If a future debug mode captures content, it must be opt-in, time-bounded, visibly marked, and disabled in normal operation.

## Protocol

After successful auth, frames are minimal and explicit.

Client to server:

```json
{ "type": "input", "data": "..." }
{ "type": "resize", "cols": 120, "rows": 36 }
{ "type": "ping" }
```

Server to client:

```json
{ "type": "output", "data": "..." }
{ "type": "status", "state": "attached" }
{ "type": "error", "message": "session is no longer attachable" }
{ "type": "exit", "code": 0 }
```

Binary frames are acceptable for output/input if the implementation chooses them, but the protocol must still keep auth, resize, status, and error messages typed and testable.

Input and resize validation:

1. Cap input frame size.
2. Cap resize rows/cols to sane terminal limits.
3. Apply backpressure so slow mobile clients cannot grow unbounded memory.
4. Drop or close on malformed frames.

## Android UX

The app should make HTTPS terminal attach the primary action when the server advertises support:

1. User taps a tmux-backed session in watch/details.
2. App calls `POST /client/sessions/{id}/attach-ticket`.
3. App opens the terminal view and connects to `ws_url`.
4. App sends the auth frame.
5. App renders output and forwards keyboard/resize input.
6. When the terminal disconnects, the app returns to the prior watch/details state.

The Android implementation can use a native terminal component or a local WebView terminal renderer. If WebView is used:

1. Terminal assets must be bundled in the app or served from SM with integrity/version control.
2. The ticket secret must be passed to the terminal renderer without writing it to logs, URLs, or persistent storage.
3. JavaScript interfaces must expose only the minimal terminal bridge API.
4. External web content must not be able to access attach tickets.

Termux attach remains available as a secondary action such as "Open in Termux" or "Copy SSH fallback command".

## Server API Changes

Add bootstrap capability metadata:

```json
{
  "external_access": {
    "mobile_terminal_supported": true,
    "termux_attach_supported": true
  }
}
```

Add session action metadata:

```json
{
  "primary_action": {
    "type": "mobile_terminal",
    "label": "Attach"
  },
  "mobile_terminal": {
    "supported": true,
    "reason": null
  }
}
```

When the feature is disabled or the user lacks shell access, `mobile_terminal.supported` should be false with a concise reason, and Termux fallback metadata can remain as-is.

## Implementation Plan

1. Add configuration parsing for `mobile_terminal`, default disabled.
2. Add an attach-ticket store with hashed secrets, atomic consume, TTL cleanup, and audit hooks.
3. Add `POST /client/sessions/{session_id}/attach-ticket` with strict authorization.
4. Add `GET /client/terminal` WebSocket pending-auth flow.
5. Add a tmux bridge abstraction that uses server-derived attach descriptors and argv-only subprocess execution.
6. Add lifecycle cleanup so bridge processes are killed on disconnect, timeout, server shutdown, or session disappearance.
7. Add client payload metadata so Android can prefer mobile terminal attach when supported.
8. Add Android terminal attach UI using existing app auth to mint tickets.
9. Keep existing Termux SSH attach as fallback.
10. Rebuild and publish the Android APK artifact when the app change lands.

## Test Plan

Server tests:

1. Ticket mint denied when feature disabled.
2. Ticket mint denied for unauthenticated users.
3. Ticket mint denied for authenticated users without `interactive_shell_access`.
4. Ticket mint denied for non-attachable/headless sessions.
5. Ticket mint succeeds for an authorized user and tmux-backed session.
6. Ticket secret is not stored raw.
7. Expired ticket fails.
8. Replayed ticket fails.
9. Ticket for session A cannot attach to session B.
10. WebSocket closes if auth frame is missing, late, malformed, or invalid.
11. Bridge subprocess is not started until auth succeeds.
12. Bridge subprocess receives argv-only tmux command with server-derived target.
13. Malicious tmux names fail validation.
14. Disconnect kills the bridge subprocess.
15. Audit events are written for success, deny, auth failure, and abnormal exit.

Android tests:

1. App shows HTTPS terminal attach when `mobile_terminal.supported=true`.
2. App falls back to details/Termux when unsupported.
3. Ticket secret is not included in URLs or persisted settings.
4. Terminal view returns to watch/details after disconnect.

Manual verification:

1. Off-LAN Android attach works through `sm.rajeshgo.li`.
2. Invalid/expired ticket cannot attach.
3. Non-allowed account cannot mint a ticket.
4. Existing desktop `sm attach` still works.
5. Termux fallback still works where Cloudflare/LAN SSH works.

## Rollout

1. Ship server feature disabled by default.
2. Enable only for the configured owner account after tests pass.
3. Verify off-LAN attach from Android.
4. Keep Termux SSH fallback visible during the initial rollout.
5. Add dashboard/watch health text distinguishing HTTPS terminal attach from Termux attach.
6. After confidence, make HTTPS terminal attach the default app action.

## Open Decisions For PR Review

1. Terminal renderer: native Android terminal component vs bundled local WebView renderer.
2. Maximum attach duration default.
3. Whether concurrent attaches to the same session should be allowed or limited to one mobile attach at a time.
4. Whether operator emergency disable should be config-only or also exposed as a CLI/API switch.

## Ticket Classification

Epic.

This is security-sensitive and crosses server auth, tmux process bridging, Android UI, app artifact publishing, and operational hardening. It should be implemented in staged PRs after this spec is approved:

1. Server ticket/auth/audit foundation.
2. Server WebSocket tmux bridge.
3. Android terminal client and artifact publish.
4. Hardening pass with off-LAN verification and documentation.
