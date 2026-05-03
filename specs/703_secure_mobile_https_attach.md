# Secure Mobile HTTPS Attach

Issue: #703

## Summary

Add a secure, SM-owned mobile attach transport that works off-LAN without relying on Termux `cloudflared access ssh`.

The recommended v1 shape is:

1. The Android app asks Session Manager for a short-lived attach ticket for one existing session.
2. The app proves possession of a registered device key, preserving the second authentication factor that SSH keys provide today.
3. The app opens an authenticated terminal stream over the existing HTTPS origin, `sm.rajeshgo.li`.
4. Session Manager bridges that stream to the selected tmux-backed session only.
5. The existing Termux SSH path remains available only as a temporary rollout fallback, then is removed from the app attach surface after the HTTPS path is verified.

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

This is not because Termux itself is bad. Termux is a capable terminal, and SSH key auth is a strong security model. The problem is operational shape: mobile attach currently depends on a separate Termux app, a separate Cloudflare SSH/websocket path, separate login/cache state, separate SSH configuration, and a second canonical remote-shell ingress path that Session Manager cannot fully audit, rate-limit, revoke, or health-check. Keeping both paths permanently means one will eventually drift, rust, and become either unreliable or a security liability.

The desired end state is one canonical hardened mobile attach path owned by SM: app OAuth plus registered device-key proof plus SM session authorization plus audited tmux bridging. Termux can remain during rollout as a rollback/fallback path, but should be removed from the app once the HTTPS path is verified.

## Goals

1. Make Android app attach work off-LAN through the existing Session Manager HTTPS origin.
2. Avoid exposing a generic shell, command execution endpoint, or arbitrary tmux target selector.
3. Require explicit, server-side authorization before any terminal bridge can start.
4. Use short-lived, single-use attach tickets so durable app credentials are not embedded in WebSocket URLs or terminal pages.
5. Audit terminal attach lifecycle events without logging sensitive terminal content by default.
6. Preserve existing desktop `sm attach` behavior.
7. Keep Termux SSH only as a temporary rollout fallback/copy-command path.
8. Preserve the current two-layer security model: authenticated app access plus a registered device key that proves the client is an approved device.
9. Decommission the Termux attach action after the HTTPS path ships and passes off-LAN verification.

## Non-Goals

1. Do not implement a general browser SSH gateway.
2. Do not allow clients to spawn arbitrary processes or run arbitrary shell commands.
3. Do not let users attach to sessions they cannot already see/control through the authenticated app API.
4. Do not rely on Cloudflare Access SSH, Termux, or LAN reachability for the primary mobile attach path.
5. Do not stream raw terminal input/output into normal application logs.
6. Do not support headless `codex-app` terminal attach in v1.
7. Do not replace tmux as the runtime/control plane for Claude, Codex, and codex-fork sessions.
8. Do not keep Termux SSH as a second long-term canonical mobile attach path after cutover.

## Threat Model

The design must assume these threats are realistic:

1. An unauthenticated internet client finds the public SM hostname and tries to open terminal WebSockets.
2. An authenticated but non-authorized account attempts to mint an attach ticket.
3. A valid attach ticket leaks through browser history, proxy logs, crash reports, app logs, or screenshots.
4. A valid app login token leaks without the approved device private key.
5. A client tampers with session ids, tmux session names, socket names, resize values, or terminal frames.
6. A malicious webpage tries to trigger attach through cookies or browser ambient credentials.
7. A stale Android app or compromised network path replays an old attach ticket.
8. A terminal bridge process outlives the mobile connection and leaves a shell attached.
9. High-volume failed connection attempts create a denial-of-service path on the SM event loop.

## Security Requirements

### Authorization

Attach is allowed only when all checks pass:

1. The caller is authenticated through the existing `/client` auth model.
2. The caller maps to a configured human user with explicit `interactive_shell_access: true`.
3. The requested session is visible to that user through existing client session APIs.
4. The session attach descriptor reports `attach_supported=true`.
5. The session is tmux-backed and has a server-derived tmux target.
6. The session is currently running or attachable according to cached SM state.
7. The caller proves possession of a registered device key for that human user.

Agents, email senders, Telegram senders, anonymous clients, and browser sessions without the explicit shell-access grant must not be able to mint attach tickets.

Recommended config shape:

```yaml
mobile_terminal:
  enabled: false
  allowed_users:
    rajesh:
      interactive_shell_access: true
      registered_device_keys:
        - id: pixel-8-pro
          public_key: "ssh-ed25519 AAAA..."
          label: "Rajesh Pixel"
          enabled: true
  ticket_ttl_seconds: 30
  auth_frame_timeout_seconds: 3
  max_attach_seconds: 3600
  max_concurrent_attaches_per_user: 1
  max_concurrent_attaches_per_session: 1
  max_concurrent_attaches_global: 4
  require_tls: true
```

Default should be disabled unless the deployment config explicitly enables it.

### Registered Device Key

The HTTPS attach path should replicate the current SSH model's second layer of protection. OAuth/app authentication proves the user account. A registered device key proves the request comes from an approved device, similar to the existing presigned SSH key requirement.

Recommended v1 behavior:

1. Each allowed human user has one or more configured device public keys.
2. The Android app stores the private key in Android Keystore when generated on-device, or imports a dedicated attach key through an operator-controlled setup flow.
3. The attach-ticket request includes a device key id, timestamp, nonce, and signature over the HTTP method, path, session id, user id, timestamp, and nonce.
4. The server verifies the signature against the configured public key before minting a ticket.
5. The ticket is bound to the verified device key id.
6. The WebSocket auth frame includes a second signature over the ticket id, session id, and a server/client nonce so a leaked ticket alone cannot open a shell.
7. Revoking a device key immediately prevents new tickets and rejects unconsumed tickets bound to that key.

The exact key format can be SSH `ed25519` public keys or another standard asymmetric format supported cleanly by the Android app and Python server. The security requirement is proof-of-possession of a configured device private key, not a bearer-only token.

### Attach Tickets

Attach tickets are the only way to open a mobile terminal stream.

Endpoint:

```http
POST /client/sessions/{session_id}/attach-ticket
```

Request authentication includes normal app auth plus device-key proof, for example:

```http
X-SM-Device-Key-Id: pixel-8-pro
X-SM-Device-Timestamp: 2026-05-03T00:00:00Z
X-SM-Device-Nonce: ...
X-SM-Device-Signature: ...
```

The signature covers the method, path, target session id, authenticated user id, timestamp, and nonce.

Response:

```json
{
  "ticket_id": "att_...",
  "ticket_secret": "...",
  "device_key_id": "pixel-8-pro",
  "ws_url": "wss://sm.rajeshgo.li/client/terminal",
  "expires_at": "2026-05-03T00:00:00Z"
}
```

Ticket rules:

1. `ticket_secret` is at least 256 bits of randomness and is returned only once.
2. Store only a keyed hash of the secret server-side.
3. TTL defaults to 30 seconds.
4. Tickets are single-use and are consumed atomically.
5. A ticket is bound to user id, registered device key id, session id, provider, tmux session, tmux socket, client id, and creation time.
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
  "ticket_secret": "...",
  "device_key_id": "pixel-8-pro",
  "nonce": "...",
  "signature": "..."
}
```

The server then:

1. Validates and atomically consumes the ticket.
2. Verifies the registered device-key signature and checks it matches the ticket-bound key id.
3. Re-runs authorization checks against current session state.
4. Starts the tmux bridge only after validation succeeds.
5. Closes the socket immediately on invalid auth, timeout, replay, or authorization failure.

Do not rely on browser cookies alone for WebSocket authorization. This prevents cross-site WebSocket abuse from ambient browser credentials.

### Transport Security

1. Production attach requires `wss://`.
2. Plain `ws://` is allowed only for localhost development when explicitly configured.
3. The server should validate `Origin` when present against configured app/web origins.
4. Missing `Origin` is acceptable for native OkHttp clients, but those clients still need a valid attach ticket.
5. Failed auth attempts are rate-limited by IP, user id when known, device key id when present, and global counters.

### Emergency Disable And Revocation

The emergency-control surface should match the threat model:

1. Lost or retired device: disable the registered device key from the local SM CLI/config path. This prevents new tickets and rejects unconsumed tickets for that key even if app login state remains valid.
2. Abuse detected while the operator is away from the Mac: expose an authenticated SM app/API control to disable mobile terminal attach globally or revoke the current device key.
3. Suspected server-side bug or active exploit: provide a config-level kill switch that disables ticket minting and WebSocket auth before any tmux bridge can start.

Recommended controls:

```bash
sm mobile-terminal disable
sm mobile-terminal device disable rajesh pixel-8-pro
```

The Android app should also expose an owner-only "Disable mobile terminal attach" action backed by the same API. All disable/revoke actions must audit who invoked them, which device/user was affected, and whether active bridges were terminated.

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
5. Registered device key id.
6. Remote address or coarse client fingerprint.
7. Result and reason.
8. Duration.
9. Input/output byte counts.

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

The app should make HTTPS terminal attach the primary action when the server advertises support. This terminal surface should live inside the SM Android app, behind the same app OAuth/auth boundary as watch/details. Do not expose a public browser terminal page as the normal v1 entry point.

End-state user flow:

1. The user opens the SM Android app.
2. The user navigates from watch or session details to an existing tmux-backed agent session.
3. The user taps `Attach`.
4. The app performs OAuth/session checks and registered device-key proof in the background.
5. The app opens an in-app terminal view for that agent's existing tmux session.
6. The terminal behaves like desktop `sm attach`: the user sees the live pane, can type, can send required control keys, can copy/paste, and can detach/back out.
7. On detach or back navigation, the app returns to the prior SM watch/details screen.

The normal end-state UX does not require Termux or another terminal app. Termux appears only during rollout as a temporary fallback action.

Attach sequence:

1. User taps a tmux-backed session in watch/details.
2. App signs the attach-ticket request with the registered device key.
3. App calls `POST /client/sessions/{id}/attach-ticket`.
4. App opens the terminal view and connects to `ws_url`.
5. App sends the auth frame, including a fresh device-key signature.
6. App renders output and forwards keyboard/resize input.
7. When the terminal disconnects, the app returns to the prior watch/details state.

### Android Terminal Renderer

Use a native Android terminal component unless the implementer proves it cannot meet the core requirements within reasonable scope. The native path is preferred because it keeps the terminal inside the app process, avoids a JavaScript bridge for shell input/output, and reduces the risk of exposing attach tickets to web content.

Renderer requirements:

1. Render ANSI terminal output correctly enough for Claude/Codex tmux sessions.
2. Support keyboard input, paste, resize, scrollback, and detach/back navigation.
3. Keep the ticket secret and device-key material out of URLs, logs, screenshots, persistent settings, and crash reports.
4. Expose no generic browser navigation or external web content in the terminal surface.
5. Return to watch/details when the attach ends.
6. Provide mobile-accessible controls for keys that the default Android keyboard cannot reliably emit.

### Mobile Control Keys

The in-app terminal must provide Termux-like control parity for the agent operations the user depends on. The default Android keyboard is not enough because it may not expose ESC, Ctrl, or tmux prefix sequences.

Minimum required controls:

| UI control | Wire behavior |
| --- | --- |
| `Esc` | Send ESC (`\x1b`) to the terminal. Used to interrupt/stop agent UI states. |
| `Detach` | Send tmux detach for the attached session, equivalent to `Ctrl-b d`, or call a server-side detach action that has the same effect. |
| `Ctrl` modifier | Allow at least `Ctrl-b` and common control chords needed by tmux/agent UIs. |
| `Paste` | Paste clipboard text safely into the terminal input stream. |
| `Copy` | Copy selected terminal text without sending unintended input. |

The app may implement these as a terminal accessory row, floating controls, or a command palette. The exact UI is an implementation detail; the requirement is that a mobile user can detach, send ESC, and copy/paste without installing Termux or switching keyboards.

The `Detach` control may translate to raw `Ctrl-b d` over the terminal stream, but a server-side detach frame is acceptable and may be safer if it can detach only the current bridge without sending extra input to the agent.

Implementation action required:

1. First evaluate a native Android terminal component against the renderer requirements.
2. If native is viable, use it for v1.
3. If native is not viable, document the specific blocker in the implementation PR and use a bundled local WebView renderer as the fallback.
4. Do not choose WebView solely for convenience if native is viable.

If WebView is used:

1. Terminal assets must be bundled in the app or served from SM with integrity/version control.
2. The ticket secret must be passed to the terminal renderer without writing it to logs, URLs, or persistent storage.
3. JavaScript interfaces must expose only the minimal terminal bridge API.
4. External web content must not be able to access attach tickets.

Termux attach remains available only as a rollout fallback such as "Open in Termux" or "Copy SSH fallback command" until the HTTPS path is verified. After cutover, remove the Termux attach action from the app and stop advertising Termux attach metadata. The goal is one canonical hardened mobile attach path, not two remote-shell paths with different auth, audit, and health behavior.

V1 should allow only one active mobile attach per user and one active mobile attach per session. To attach somewhere else, the app should detach the current terminal first, then mint a new ticket for the next session. This matches the mobile ergonomic model and avoids surprising simultaneous shell views.

## Server API Changes

During rollout, add bootstrap capability metadata:

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

When the feature is disabled or the user lacks shell access, `mobile_terminal.supported` should be false with a concise reason. `termux_attach_supported` is transitional metadata only; remove it from the app-facing attach surface after the HTTPS cutover is complete.

## Implementation Plan

1. Add configuration parsing for `mobile_terminal`, default disabled.
2. Add an attach-ticket store with hashed secrets, atomic consume, TTL cleanup, and audit hooks.
3. Add registered device-key config and signature verification for ticket minting.
4. Add `POST /client/sessions/{session_id}/attach-ticket` with strict authorization.
5. Add `GET /client/terminal` WebSocket pending-auth flow with ticket and device-key proof validation.
6. Add a tmux bridge abstraction that uses server-derived attach descriptors and argv-only subprocess execution.
7. Add lifecycle cleanup so bridge processes are killed on disconnect, timeout, server shutdown, or session disappearance.
8. Add client payload metadata so Android can prefer mobile terminal attach when supported.
9. Add Android terminal attach UI using existing app auth to mint tickets and Android Keystore/device-key signing.
10. Keep existing Termux SSH attach as a rollout fallback only.
11. Add mobile-terminal disable and device-key revocation controls for CLI and app/API paths.
12. Rebuild and publish the Android APK artifact when the app change lands.
13. After HTTPS attach passes off-LAN verification, remove the Termux attach app action and stop advertising Termux attach metadata.

## Test Plan

Server tests:

1. Ticket mint denied when feature disabled.
2. Ticket mint denied for unauthenticated users.
3. Ticket mint denied for authenticated users without `interactive_shell_access`.
4. Ticket mint denied when the registered device-key signature is missing, invalid, stale, or revoked.
5. Ticket mint denied for non-attachable/headless sessions.
6. Ticket mint succeeds for an authorized user, registered device key, and tmux-backed session.
7. Ticket secret is not stored raw.
8. Expired ticket fails.
9. Replayed ticket fails.
10. Ticket for session A cannot attach to session B.
11. Ticket minted for device key A cannot be consumed by device key B.
12. WebSocket closes if auth frame is missing, late, malformed, unsigned, or invalid.
13. Bridge subprocess is not started until auth and device-key proof both succeed.
14. Bridge subprocess receives argv-only tmux command with server-derived target.
15. Malicious tmux names fail validation.
16. Disconnect kills the bridge subprocess.
17. Second simultaneous mobile attach by the same user/session is rejected or requires prior detach according to config.
18. Disabled global mobile-terminal config rejects ticket mint and WebSocket auth.
19. Disabled device key rejects ticket mint and unconsumed tickets bound to that key.
20. Audit events are written for success, deny, auth failure, device-key failure, disable/revoke, and abnormal exit.

Android tests:

1. App shows HTTPS terminal attach when `mobile_terminal.supported=true`.
2. App falls back to details/Termux when unsupported.
3. App signs ticket mint and WebSocket auth with the configured device key.
4. Ticket secret is not included in URLs or persisted settings.
5. Terminal view returns to watch/details after disconnect.
6. Starting a second attach first detaches or blocks the current mobile attach.
7. After cutover, Termux attach is not shown as a normal attach action.
8. Owner-only app disable control blocks new mobile terminal attaches.
9. Native renderer viability is documented in the implementation PR; if WebView is used, the PR documents the native blocker and WebView containment controls.
10. Terminal UI exposes mobile-accessible `Esc`, `Detach`, `Ctrl` modifier/chords, copy, and paste controls.
11. `Esc` sends ESC to the attached terminal.
12. `Detach` detaches from tmux and returns to watch/details without killing the agent.
13. Copy/paste works without leaking ticket secrets or sending unintended control input.

Manual verification:

1. Off-LAN Android attach works through `sm.rajeshgo.li`.
2. Invalid/expired ticket cannot attach.
3. Non-allowed account cannot mint a ticket.
4. OAuth-authenticated app without a registered device key cannot attach.
5. Existing desktop `sm attach` still works.
6. During rollout only, Termux fallback still works where Cloudflare/LAN SSH works.
7. After cutover, the SM app exposes only the HTTPS/device-key attach path for mobile attach.
8. On Android, the user can stop/interrupt with `Esc`, detach with the detach control or `Ctrl-b d` equivalent, and copy/paste without Termux.

## Rollout

1. Ship server feature disabled by default.
2. Enable only for the configured owner account after tests pass.
3. Verify off-LAN attach from Android.
4. Keep Termux SSH fallback visible only during the initial rollout.
5. Add dashboard/watch health text distinguishing HTTPS terminal attach from Termux attach.
6. After confidence, make HTTPS terminal attach the default app action.
7. Remove Termux attach from the app and public attach metadata after cutover so the old path does not rust into a latent vulnerability.

## Resolved Design Decisions

1. End-state mobile attach happens inside the SM Android app; the user does not normally use Termux or another terminal app.
2. Native Android terminal rendering is preferred for v1. WebView is a fallback only if the implementation PR documents a concrete native blocker and keeps assets local/contained.
3. Termux is transitional rollout fallback only and is removed from the app attach surface after HTTPS/device-key attach passes verification.

## Ticket Classification

Epic.

This is security-sensitive and crosses server auth, tmux process bridging, Android UI, app artifact publishing, and operational hardening. It should be implemented in staged PRs after this spec is approved:

1. Server ticket/auth/audit foundation.
2. Server WebSocket tmux bridge.
3. Android terminal client and artifact publish.
4. Hardening pass with off-LAN verification and documentation.
