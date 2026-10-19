# Studio SSH — manual override and shell-driven access

## Goal

A phone/web toggle that exposes `ssh studio-away` (→ `studio-ssh.rajeshgo.li`) from any network,
key-only, over a dedicated cloudflared tunnel. The switch forces SSH always on; with it off,
live plain shells use a home listener and open the public tunnel only on an admitted away request.

## Frozen shared constants

| Thing | Value |
|---|---|
| Public hostname | `studio-ssh.rajeshgo.li` |
| cloudflared tunnel name | `studio-ssh` |
| Dedicated sshd bind | `127.0.0.1:22222` plus the private LAN IPv4 address while home access is needed |
| sshd LaunchAgent label | `com.rajesh.sm-studio-ssh-sshd` |
| tunnel LaunchAgent label | `com.rajesh.sm-studio-ssh-tunnel` |
| Plist path (each) | `~/Library/LaunchAgents/<label>.plist` |
| launchd domain | `gui/501` (uid 501) |
| cloudflared assets dir | `<repo>/.local/studio-ssh/cloudflared/` (`config.yml` + `<uuid>.json`) |
| sshd assets dir | `~/.local/share/session-manager/studio-ssh/` (`sshd_config`, host key, logs) |
| Device key file | `~/.config/session-manager/ssh/device_keys` (only registered unrevoked device keys) |
| server | Rust `sm-server`, axum 0.8, `127.0.0.1:8420` (crates/sm-server) — Python `src/` is LEGACY, do not touch |

## Frozen HTTP API (server ↔ Android ↔ web)

- `GET /admin/studio-ssh` → 200 JSON `StudioSshStatus`
- `POST /admin/studio-ssh` body `{ "enabled": true|false }` → 200 JSON `StudioSshStatus`

`StudioSshStatus` (exact field names):
```json
{
  "enabled": true,
  "status": "off",            // one of: "off" | "starting" | "on" | "error"
  "host": "studio-ssh.rajeshgo.li",
  "sshd_listening": false,
  "tunnel_running": false,
  "error": null               // string when status=="error", else null
}
```
Semantics: `enabled` = saved manual always-on switch, independent of automatic tunnel operation. `status` = observed:
`on` only when `sshd_listening && tunnel_running`; `starting` when enabled but not yet both up;
`off` when the manual switch is disabled (automatic services may still run); `error` with `error` message on failure. POST enable returns immediately after
kicking off enable() — it may return `starting`; clients poll GET.

Auth (both routes): reuse the EXACT preamble of `disable_mobile_terminal` in `http.rs` — Cloudflare
Access mobile-app JWT + public-edge assertion + `request_actor_email` + owner gate
(`mobile_terminal_owner` via the `mobile_terminal_user_can_disable`-style check). Loopback requests
(`is_local_bypass_request`) skip Access — required for local curl tests and the reconcile loop.

## Runtime behavior

The Studio SSH switch means **always on**. Session Manager saves its desired value in
`<sshd assets dir>/manual_enabled`; running an automatic tunnel does not turn the switch on.
On the first upgrade, an absent file adopts the old switch's observed state once.

| Situation | LAN listener | Loopback listener | Cloudflare tunnel |
|---|---|---|---|
| Switch off, no live shells | Off | Off | Off |
| Switch off, live shells, no away request | On | On | Off |
| Switch off, live shells, admitted away request | On | On | On until the last tunnel login ends plus the linger period |
| Switch on, shells enabled | On | On | On |
| Shell kill switch off, Studio SSH off | Off | Off | Off |
| Shell kill switch off, Studio SSH on | Off | On | On |

The LAN listener binds the private IPv4 address of the default-route interface, the same
address used for the direct web terminal. It never binds `0.0.0.0`. Address changes are
reconciled every 30 seconds. A config change sends SIGHUP to the dedicated sshd LaunchAgent,
so existing SSH connections survive listener reloads. Healthy services are not restarted.
Retiring the last shell closes the automatic tunnel immediately. Disabling shells retires
all shells, removes the LAN address and closes the automatic tunnel in the settings request;
the registered key file remains.

`terminal_limits.tunnel_linger_minutes` defaults to 15, with owner-editable range 1–120.
An away request that never produces a login expires after the same period. An active tunnel
login holds the tunnel open; a home login does not. A new away request renews the opening
period. Saved connection identities, log position and opening timestamps in `transport.json`
preserve this behavior across server restarts. Shell lifetime and idle cleanup remain as
specified in `docs/working/plain_terminal_memo.html`.

Implementation: `src/http/ssh_transport.rs`, `src/studio_ssh/transport.rs` and the independent
observer and policy tasks in `src/main.rs` (all source paths relative to `crates/sm-server`).

## Device keys and routes

The `device_ssh_keys` table in the usage database stores fingerprint, device id, public key,
registration timestamp and revocation timestamp. Each fingerprint belongs to one device.
Registration accepts a single valid Ed25519 public key without authorized_keys options.
Private keys never enter Session Manager. URL-encode fingerprints in endpoint paths, including
any `/` or `+` in their SHA256 representation.

| Owner endpoint | Request / response |
|---|---|
| `GET /devices/{id}/ssh-keys` | `{keys, import_candidates}`; key records include revoked keys. Candidates are populated only for `imported`. |
| `POST /devices/{id}/ssh-keys` | `{public_key}` → `{keys: [registered_key]}`. The device must be an active owner device, or the migration device `imported`. |
| `DELETE /devices/{id}/ssh-keys/{fingerprint}` | `{revoked: true}` after key removal and live connection teardown. Idempotent. |
| `POST /devices/imported/ssh-keys` | `{import_authorized_keys: true}` explicitly imports the previewed compatible keys under `imported`. |
| `POST /terminals/{id}/route` | `{where: "home" | "away"}` → `{ssh_host, ssh_port, ssh_user, tmux_session, tmux_socket_name, command}`. |

These endpoints use the shell owner guard: the public mobile path requires Access device
identity, public-edge assertion and owner authorization; the browser accepts its owner login
and matching Origin for writes. Local operator access follows the existing local guard.
Every endpoint returns 403 `plain_disabled` while shells are disabled. Agent identities are
refused. New routes appear in the owner-browser authorization regression tests.

The away route additionally requires an active SSH key for the authenticated certificate's
device. A public `x-sm-device-key-id` header cannot impersonate another device. Local operator
calls may supply that header. Without a key, return 409 `no_ssh_key` before opening anything.
The away call starts the tunnel and waits at most 20 seconds for sshd listening and the tunnel
agent running, then returns the command or 503 `away_unavailable`. Home never requests a tunnel.

Home command: `ssh -t -p 22222 rajesh@studio-lan.rajeshgo.li tmux -L 'session-manager' attach-session -t <shell>`.
Away command: `ssh -t studio-away tmux -L 'session-manager' attach-session -t <shell>`.
The actual configured tmux socket replaces `session-manager`; omit `-L` when no socket is
configured. The alias supplies away username, port and ProxyCommand.

sshd reads only `~/.config/session-manager/ssh/device_keys`, an atomic mode-0600 projection of
unrevoked database rows. Every line begins `restrict,pty`, permitting the terminal while
disallowing forwarding. The general `~/.ssh/authorized_keys` file grants no Studio SSH access.
The migration preview is read-only: setup offers the owner explicit import rather than
silently granting every old key access. Non-Ed25519 and option-bearing lines are excluded.
The three configurable paths are `external_access.studio_ssh.assets_dir`, `device_keys_file`
and `legacy_authorized_keys_file`; their defaults are the paths above and in the constants.

Implementation: `src/device_ssh.rs`, `src/http/ssh_transport.rs`, `src/http/plain_terminals.rs`.

## Connection accounting and revocation

The sshd LaunchAgent uses `sshd -D -E <assets>/sshd.err.log -f <assets>/sshd_config`.
`LogLevel VERBOSE` records accepted fingerprints and source addresses.
`LogVerbose auth.c:auth_log():*` adds the authentication process id to those log lines; plain
stderr logging alone does not include one. The observer reads appended complete lines once
per second, handles truncation and rotation, and records each login with its process start
time so a reused process id does not become a connection identity. Loopback sources count
as tunnel connections; other addresses count as home connections. Dead login processes end
their connections even if a disconnect log is missing. Shell-specific attach times come from
the native tmux client activity tracking used by shell cleanup, including SSH tmux clients.

An accepted tunnel login sends the owner push `SSH login over tunnel · <key comment>`.
Push delivery runs separately from listener control and revocation.

Deleting a key, or revoking its enrolled device through either existing device-revocation
endpoint, marks the corresponding keys revoked, projects the file, consumes pending login
records and terminates matching SSH login processes and their descendants. Other devices'
connections and the existing tmux server survive. The independent one-second observer also
terminates a login using an unavailable key, covering authentication races. Key mutation and
connection accounting never wait for launchctl or tunnel startup under the shell lifecycle
lock. The live connection must drop within five seconds.

Implementation: `src/studio_ssh/transport.rs`, `src/http/ssh_transport.rs`, and
`revoke_mobile_terminal_device` in `src/http.rs`.

## Installation and verification

`scripts/setup_studio_ssh.sh` provisions the dedicated tunnel, DNS, host key, sshd config and
LaunchAgents, leaving both agents disabled. `--dry-run` prints actions.
`--sshd-assets-only` writes only local sshd assets for tests.
`--local-only` upgrades local sshd assets and its plist without changing the tunnel plist,
DNS, tunnel credentials or the switch. Run that upgrade before deploying the server; reload a
loaded sshd LaunchAgent once so its arguments use `-E`. This one-time deployment reload may
disconnect existing SSH logins. Normal listener changes use SIGHUP and preserve them.
The setup output offers the key-import preview and explicit import endpoint.

The client alias remains:

```sshconfig
Host studio-away
    HostName studio-ssh.rajeshgo.li
    User rajesh
    Port 22222
    ProxyCommand /opt/homebrew/bin/cloudflared access ssh --hostname %h
```

Run Rust tests with `scripts/test-rust-isolated.sh` through `sm queue run`.
`tests/plain_terminal_ssh.rs` runs a private non-root sshd and proves key-only login, refusal
of unregistered/revoked keys, selective live teardown within five seconds, saved connection
re-adoption and automatic tunnel timing. `src/http/ssh_transport_tests.rs` covers migration,
file projection, certificate-device selection, missing-key denial, home commands, kill-switch
admission and the manual override. The browser-device regression in `src/http.rs` proves
device-wide key revocation. Existing `src/http/plain_terminal_tests.rs` covers shell cleanup
and relay restrictions. Test isolation uses private paths and never controls live LaunchAgents.

Live acceptance: with the manual switch off, open a shell and attach from a laptop with a
registered key using the returned home command. Request away, attach through `studio-away`,
revoke that key and verify refusal within five seconds. Close the last shell and verify the
tunnel exits; repeat with a one-minute linger and leave the shell open to verify timed stop.
Rebuild/restart only through `scripts/restart-rust-server.sh`; the repository AGENTS.md owns
the PR review, merge and deployment workflow.
