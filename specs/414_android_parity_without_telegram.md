# Android Parity Without Telegram

Issue: #414

## Goal

Make session-manager fully usable from Android without relying on Telegram as the primary interface.

## Current state

- `web/sm-watch` renders a React watch dashboard and opens Telegram deep links on session tap.
- Session-manager is effectively local-first: `http://localhost:8420/watch` and local API access.
- Remote terminal attachment is not modeled as a first-class mobile workflow.

## Target state

1. A public HTTPS origin exposes the watch/dashboard and API over a `cloudflared` tunnel.
2. The public origin uses Google OAuth with an explicit single-user email allowlist for the initial rollout.
3. A native Android app provides parity with the current watch dashboard using a real Android UI.
4. Tapping a session in Android opens a real terminal attach flow through Termux/tmux instead of Telegram.
5. Deployment-specific hostnames, email addresses, client IDs, secrets, and tunnel identifiers stay local and gitignored.

## Chosen architecture

### 1. Public ingress

Use a locally managed Cloudflare Tunnel to expose session-manager externally.

- One HTTPS hostname routes to the session-manager HTTP server.
- One SSH hostname routes to the local SSH service for tmux attachment.
- The tunnel configuration is stored locally in a gitignored directory.

Why:
- Outbound-only origin connectivity, no inbound port exposure.
- Clean separation between browser/API traffic and terminal attach traffic.
- SSH-over-hostname is a supported Cloudflare Tunnel pattern and fits Termux better than inventing a browser terminal protocol first.

### 2. Authentication model

Use Google OAuth in session-manager itself for the browser/API origin.

- Browser dashboard: standard OAuth redirect flow.
- Android app: Google sign-in via Android Credential Manager, backend verifies Google ID token and issues a session-manager auth session/token.
- Server enforces an explicit email allowlist; initial rollout is one user.

Why this over putting Access in front of the app origin:
- The Android app needs native API access, not just browser cookies.
- Owning auth at the session-manager layer gives one identity model for web and Android.
- The public hostname still sits behind Cloudflare Tunnel, but app auth remains under session-manager control.

### 3. Terminal attach model

Use Termux plus SSH-over-tunnel for real tmux attachment.

Flow:
- Android app calls into Termux via `RUN_COMMAND` intent.
- Termux runs a local helper command/script.
- That helper uses `cloudflared access ssh --hostname <ssh-hostname>` as the SSH transport.
- SSH lands on the server and attaches to the requested tmux session.

Why:
- Keeps terminal behavior native and battle-tested.
- Avoids building an in-app terminal emulator + PTY streaming protocol as the first version.
- Preserves tmux as the source of truth instead of inventing a second remote shell abstraction.

## Work breakdown

### Ticket #415: access-protected public ingress

Deliverables:
- public tunnel for the HTTP origin
- local-only tunnel config templates and service instructions
- server config support for public base URL / OAuth callback URL / email allowlist
- login/session handling for the browser dashboard

### Ticket #416: Android-friendly watch/API surface

Deliverables:
- API contract suitable for mobile consumption
- no Telegram-only click-through assumptions in server/UI surface
- attach metadata endpoint or equivalent returned in session payloads
- stable auth/session contract for native app use

### Ticket #417: native Android app

Deliverables:
- Android app module in repo
- Jetpack Compose watch UI matching current hierarchy/detail/status surface
- authenticated session fetch / refresh / actions
- no WebView/TWA wrapper as the main implementation

### Ticket #418: Termux/tmux attach integration

Deliverables:
- Android app -> Termux handoff
- local Termux helper command/script contract
- SSH/tmux attach workflow against the tunnel-exposed SSH hostname
- graceful handling for unauthenticated/misconfigured Termux state

## Local-only data that must not go in GitHub

The following stay only in gitignored local files:
- real public hostname(s)
- actual allowlisted email(s)
- Google OAuth client IDs/secrets
- Cloudflare account/zone/tunnel identifiers
- local SSH username/host mapping details
- Android signing fingerprints tied to the private rollout

## Exact inputs needed later

### Cloudflare / tunnel

- Cloudflare zone with control of the public hostname
- browser authorization for `cloudflared tunnel login`
- chosen tunnel name
- whether SSH should use the same parent domain or a dedicated subdomain

### Google OAuth

For browser/backend OAuth:
- Google Cloud project (existing or new)
- OAuth consent screen configured
- Web OAuth client
- authorized redirect URI for session-manager callback on the public origin

For Android sign-in:
- Android application ID/package name
- signing certificate SHA-256 fingerprint for the sideloaded app build you will actually install
- Android OAuth client in the same Google project
- Web client ID available to Android for backend-audience ID tokens

### Local server / SSH

- local SSH daemon available for tmux attach
- SSH user/account that should own tmux attachment from Android
- confirmation that Termux on the device can install `cloudflared` and `openssh`

## Sequencing

1. Build the generic spec and ticket split.
2. Create gitignored local deployment files.
3. Bootstrap the tunnel locally.
4. Add Google OAuth to session-manager for browser/API.
5. Add mobile-oriented attach metadata/API support.
6. Build the Android app.
7. Wire Android -> Termux -> SSH -> tmux attach.

## Risks

- Native Android auth and browser auth must share one backend identity model cleanly.
- Termux handoff depends on Android permission and installed-package state.
- Sideloaded app signing identity must be stable enough for Google Android OAuth configuration.
- A future in-app terminal may still be desirable, but it should not block v1 parity.

## Recommendation

Proceed with the chosen architecture above. It gives the fastest path to usable Android parity while keeping the hardest problem (terminal attach) delegated to a tool that already solves it well.

Ticket classification: epic
