# Stage 2 Protocol Manifest

Generated: 2026-06-06T12:42:08-07:00

Provenance commands:

- `rg -n "send_json|receive_json|frame_type|WEBSOCKET|StreamingResponse|WatchStaticFiles" src/server.py src/node_agent.py src/codex_fork_remote.py android-app web/sm-watch`
- `sed -n "5150,5615p" src/server.py`
- `sed -n "4726,4789p" src/server.py`
- `sed -n "253,464p" src/node_agent.py`
- `sed -n "151,286p" src/codex_fork_remote.py`

Reconciliation status: source-derived pass 2 for Stage 2 convergence review. Rows marked manual or supplemental are extracted from source patterns that are not directly represented by decorators, argparse metadata, or local SQLite files.

## Mobile Terminal WebSocket `/client/terminal`

First-class Rust target by owner priority. Current protocol is authenticated terminal I/O over WebSocket, separate from the lower-priority generic public browser watch surface.

| direction/kind | frame/path | concrete contract |
| --- | --- | --- |
| HTTP diagnostic | `GET /client/terminal` | Requires actor email and allowed interactive-shell user; returns 426 JSON `{"detail":"mobile terminal requires a WebSocket upgrade"}` with `Upgrade: websocket` and `Connection: Upgrade`. |
| connection open | WebSocket accept | Server accepts first, then checks runtime enabled flag and origin. Disabled sends `{"type":"error","message":"mobile terminal attach is disabled"}` and closes 1008. Invalid origin sends `invalid origin` and closes 1008. |
| client first frame | `auth` | Must arrive within `auth_frame_timeout_seconds` default 3s, min 1, max 30. Required fields: `ticket_id`, `ticket_secret`, `device_key_id`, `nonce`, `signature`. Non-auth first frame sends `First terminal frame must be auth` and closes 1008. Timeout sends `terminal auth timed out` and closes 1008. Generic failure sends `terminal auth failed` and closes 1008. |
| auth validation | ticket/device proof | Ticket must exist, be unexpired/unconsumed, match device id and secret hash, pass active attach quotas, map to still-allowed user/device, pass signed `SM-MOBILE-TERMINAL-WS-V1` message, and session must still be attachable. Failure messages include `Invalid terminal auth frame`, `Attach ticket is invalid or expired`, `Attach ticket device mismatch`, `Attach ticket secret mismatch`, `Attach ticket is expired or consumed`, `Too many active mobile attaches`, `Too many active mobile attaches for user`, `Session already has an active mobile attach`, `User is no longer allowed to attach`, `Device key is no longer registered`, `Session is no longer attachable`, or route-specific metadata reason. |
| bounds | config/constants | Input frame max 8192 chars. Rows 2-120, cols 10-300, defaults 24x80. Initial resize wait default 2.0s, min 0, max 10. History preload lines default 4000, min 0, max 20000. Max attach seconds default 3600, min 30, max 86400. |
| client frame | `resize` with `rows`, `cols` | Valid resize sets PTY size; notify path sends `{"type":"status","state":"resized","rows":...,"cols":...}`. Invalid size sends `ignored invalid resize`; PTY resize failure sends `failed to resize terminal`. |
| client frame | `input` with `data` | UTF-8 encoded to PTY. Oversized input sends `input frame too large` and stops attach loop. Write failure sends `failed to deliver terminal input` and stops attach loop. |
| client frame | `key` with `key` | Supported keys from `_mobile_terminal_key_bytes`: enter/return, tab, backspace, escape/esc, up/down/left/right, ctrl-c/d/z/l/a/e/u/k/w/b/f/p/n. Unsupported sends `unsupported key: <key>` and continues. Write failure sends `failed to deliver terminal key` and stops attach loop. |
| client frame | `ping` | Server sends `{"type":"status","state":"pong"}`. |
| client frame | `detach` | Sets stop event, closes normally in finally. |
| client frame | unknown | Server sends `{"type":"error","message":"unsupported terminal frame"}` and continues. |
| server output | `output` | History preload and live stream use `{"type":"output","mode":"history|stream","encoding":"base64","data":"..."}`. History normalizes LF to CRLF. |
| server status | `status` | `attached` includes `session_id`, `rows`, `cols`; resize and ping use `resized`/`pong`. Android client sets local status `authenticating` before first server status. |
| server exit | `exit` | Disabled active attach sends code 1008 reason `mobile_terminal_disabled`; max lifetime sends code 124 reason `max_attach_seconds`. Tmux read failure sends error `tmux session is no longer attachable`. Start failure sends `failed to attach tmux session`. |

## Node-Agent WebSocket `/nodes/agent`

| direction/kind | frame/path | concrete contract |
| --- | --- | --- |
| connection open | server accept | If session manager missing, sends `{"type":"error","message":"Session manager not configured"}` and closes 1011. |
| client first frame | `hello` | Required within 5s. Missing/wrong type sends `First node-agent frame must be hello` and closes 1008. Timeout sends `Node-agent hello timed out` and closes 1008. |
| hello fields | `node_id`, `secret` | `node_id` must be non-empty and not `primary`; otherwise `Invalid node id` close 1008. Expected node-agent secret must be configured; otherwise `Node-agent secret not configured` close 1008. Bad supplied secret sends `Invalid node-agent secret` close 1008. |
| server frame | `hello_ok` | Payload `{"type":"hello_ok","node_id":...}` confirms accepted node agent. |
| server frame | `register` | Includes `session_id`, `event_stream_path`, `control_socket_path`, optional `cursor`. Node resolves paths under node `log_dir`; missing fields or path violations return `register_failed` with `error`. |
| client frame | `registered` | Includes `session_id`, `event_stream_path`, `control_socket_path`; completes pending registration future. |
| client frame | `register_failed` | Includes `session_id`, `error`; primary converts to `RuntimeError(error)` for pending registration. |
| server frame | `unregister` | Includes `session_id`; node stops tail registration if present. |
| client frame | `event` | Requires `session_id` and string `line`; malformed event is skipped. Valid line enters remote event queue. |
| client frame | `event_gap` | Logged as warning with node id/session/frame; no close or direct client-visible error. |
| server frame | `control` | Includes `request_id`, `session_id`, provider control `frame`, and timeout. Node returns `control_result` ok with `line`, or ok false `error` dict. Error codes include `not_registered`, `not_ready`, and `control_failed`; timeout message is `control socket timed out after {timeout:.1f}s`. |
| server frame | `restore_inventory` | Includes `request_id`; node returns `restore_inventory_result` with `ok`, `node_id`, `state_file`, `sessions`, or `error`. Missing request_id is ignored. |
| unknown frames | any | Node client ignores unknown primary frames; primary logs and ignores unknown node-agent frames. `runtime_ready` and `pong` are accepted no-ops. |
| bridge failure | exception | Server attempts error `Node-agent bridge failed` and closes 1011. Disconnect fails pending control futures with `Node-agent <node_id> disconnected`. |

## SSE `/events` And Snapshot `/events/state`

| path | concrete contract |
| --- | --- |
| `GET /events/state` | JSON snapshot: `tmux_client_event_version` integer and `last_tmux_client_event` value from session manager or fallback `{version:0,last_event:null}`. |
| `GET /events` | `StreamingResponse` media type `text/event-stream`; headers `Cache-Control: no-cache` and `X-Accel-Buffering: no`. First frame is event `hello` with current `/events/state` payload. Each queued event uses its `type` field as SSE event name, default `message`. Idle timeout is 15.0s, yielding keepalive comment `<colon-space> keepalive` followed by two LF bytes. Subscriber queue size is 32. |

## Static Watch And App Artifacts

| surface | concrete contract |
| --- | --- |
| `/watch` when dist missing | `GET /watch` and `GET /watch/{_path:path}` return 503 JSON `{"error":"sm-watch frontend is not built. Build with: (cd web/sm-watch && npm install && npm run build)"}`. |
| `/watch` when dist exists | `GET /watch` redirects to `/watch/`; `WatchStaticFiles(directory=dist, html=True)` mounts `/watch` with SPA fallback. HTML or index responses with 200/304 get `Cache-Control: no-store, max-age=0, must-revalidate` and `Pragma: no-cache`. |
| `/apps/{app}/latest.apk` | Public Google-auth-exempt artifact route. Valid app name regex `^[a-z0-9][a-z0-9-]*$`; missing/invalid metadata returns 404 `Artifact not found`. Success redirects 302 to hashed APK with `Cache-Control: no-cache`. |
| `/apps/{app}/{artifact_hash}.apk` | Public Google-auth-exempt immutable APK route. App name regex as above, hash regex `^[0-9a-f]{8}$`; missing invalid returns 404 `Artifact not found`. Success `FileResponse` media type `application/vnd.android.package-archive`, filename `{app}.apk`, header `Cache-Control: public, max-age=31536000, immutable`. |
| `/apps/{app}/meta.json` | Public Google-auth-exempt metadata route. Invalid app returns 404 `Artifact metadata not found`; unreadable metadata returns 500 `Artifact metadata unreadable`; response model fields in schema manifest. |
| `/apk` | Public Google-auth-exempt legacy alias; redirects 302 to `/apps/session-manager-android/latest.apk`. |
