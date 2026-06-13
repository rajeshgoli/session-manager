# Cloudflare Access Auth Model For SM

Status: draft for issue #945.

## Goal

Session Manager public access should move to a zero-trust boundary: no internet request reaches the SM origin unless Cloudflare Access has authenticated the caller first. The origin still performs SM authorization after Cloudflare. Cloudflare proof is the first gate, not the only gate.

This spec copies the neighboring `office-automate` Cloudflare Access model where it fits, then adapts it for SM's higher-risk surface: native mobile routes can expose live agents and shell attach.

## Decision

Use split Cloudflare Access applications and policies:

| Public entry | Cloudflare Access gate | SM origin gate | Purpose |
| --- | --- | --- | --- |
| `sm.rajeshgo.li` | Interactive Access login allowlisted to the owner email | Existing SM Google OAuth/session cookie | Browser/watch/admin pages and OAuth callback flow. |
| `sm-app.rajeshgo.li` | mTLS/service auth with per-device certificate Common Name allowlist | Existing Android Google ID-token exchange to SM `smat_` device bearer token, then route capability checks | Native Android app API, app update metadata/artifacts, mobile terminal attach flows. |
| node fallback hostname, or route-class equivalent | mTLS/service auth with per-node certificate Common Name allowlist | Node id/token/capability checks | Registered node fallback only when LAN `studio.local` is unavailable. |
| inbound email webhook hostname/path, or route-class equivalent | Worker/service identity plus existing worker secret | Authorized sender/session routing checks | Retained email fallback after Telegram removal. |

The native app must not rely on interactive Cloudflare redirects. It should never receive Cloudflare login HTML or a 302 as part of normal API operation.

Reuse the Office Automate implementation pattern and Cloudflare account plumbing where possible, but do not reuse the Office Automate Access application or a broad Office Automate device policy for SM. SM has different hostnames, endpoints, origin audience, and blast radius. The only safe reuse candidates are:

- Cloudflare account/zone/tunnel conventions.
- The uploaded device root CA, if sharing a CA across OA and SM is operationally acceptable.
- The Cloudflare API token shape, provided it has the required Access Apps and Policies write scope.
- The Rust policy-sync/JWT-validation code pattern.

SM should have its own Access applications and its own SM device/node Common Name allowlists. A physical phone may use the same root CA, but SM revocation must remove the phone from the SM allowlist independently of Office Automate.

## Required Cloudflare Setup

Create separate Access applications unless Cloudflare configuration proves that a single application can express the same host/path separation without policy ambiguity. Separate applications are the target because they make browser redirects and app service auth impossible to confuse.

| Access application | Public hostname | Policies | Notes |
| --- | --- | --- | --- |
| `sm-browser` | `sm.rajeshgo.li` | `Allow` policy with `Include: Email == <owner-email>`. No `Bypass`. | Interactive browser access only. Enable the chosen identity provider/email OTP. This app fronts `/`, `/watch`, `/auth/google/login`, `/auth/google/callback`, and browser diagnostics. |
| `sm-mobile-app` | `sm-app.rajeshgo.li` | `Service Auth` policy with `Include: Common Name` for each enrolled SM mobile device certificate. No broad `Valid Certificate` policy. No `Bypass`. Prefer 401/403 service-auth denial behavior, not browser redirects. | Native Android API only. This fronts `/client/*`, `/auth/device/google`, app artifact metadata/downloads used by the app, and mobile terminal routes. |
| `sm-node-fallback` | node fallback hostname, or exact route-class hostname | `Service Auth` policy with `Include: Common Name` for each enrolled SM node certificate. No broad `Valid Certificate`. No `Bypass`. | Only for LAN-first node fallback. A node certificate cannot authorize mobile/browser routes. |
| `sm-email-worker` | email webhook hostname/path if split out | `Service Auth` service token or equivalent worker identity, plus exact route allowlist. No generic app/node/browser access. | The origin still requires worker secret and authorized sender checks. |

The Cloudflare mTLS root CA must be associated with the SM app/node hostnames. The Common Name selector is the required selector for SM device and node access; a broad Valid Certificate selector is explicitly not sufficient because any cert signed by the CA would pass the edge.

Tunnel/DNS requirements:

- `cloudflared` routes only the exact SM hostnames to loopback/private SM origin.
- No wildcard public hostname, no private-network route, no Access `Bypass` policy, and no public path that forwards to origin without an Access decision.
- Final unmatched ingress route returns 404 or denies.
- Cloudflare Access AUD/JWT audience for each SM Access application is recorded in SM config and verified at origin.
- The origin remains loopback/private where feasible and denies public forwarded traffic without a valid Cloudflare/edge assertion.

## Why Not TOTP, SMS, Or Device-Code OAuth

The app already uses Android Credential Manager to obtain a Google ID token and exchange it at `POST /auth/device/google`. That is the preferred second origin-auth factor after Cloudflare mTLS:

- It is app-native and does not require TV/device-code copy-paste.
- It reuses the same Google allowlist/audience model as browser auth.
- It avoids SMS travel and SIM-swap risks.
- It avoids introducing a local TOTP secret store and manual code UX.

TOTP can be considered later as an emergency operator fallback, but it is not the primary app auth. SMS and device-code OAuth are rejected for this cut.

## Request Flows

### Browser

```text
browser
  -> Cloudflare Access interactive login for owner email
  -> cloudflared exact-hostname route to loopback SM origin
  -> SM /auth/google/login and /auth/google/callback
  -> SM signed session cookie
  -> browser/watch/admin routes
```

Google's pages are not behind the SM Cloudflare app. The browser returns to `sm.rajeshgo.li` with an existing Cloudflare Access session cookie, or Cloudflare re-authenticates the browser before allowing the callback through.

### Android App

```text
Android app
  -> Cloudflare Access mTLS/service auth using enrolled client certificate
  -> cloudflared exact-hostname route to loopback SM origin
  -> SM verifies Cloudflare Access JWT/audience and enrolled device Common Name
  -> app calls POST /auth/device/google with Android Google ID token
  -> SM verifies Google token/audience/owner allowlist and returns smat_ bearer token
  -> app calls native routes with bearer token
  -> attach-ticket or terminal attach route capability checks still apply
```

Cloudflare client-certificate possession does not authorize shell access by itself. A request that has mTLS proof but lacks a valid SM user/device bearer token is denied at origin.

### Registered Node Fallback

```text
node
  -> try LAN/local studio.local path first
  -> only on LAN failure, use Cloudflare Access mTLS/service auth with node certificate
  -> SM verifies Cloudflare Access JWT/audience and enrolled node Common Name
  -> existing node-token/node-agent/capability checks
```

The node path is not browser OAuth and not phone auth. It is node possession proof plus node-specific origin authorization.

## Office Automate Pieces To Copy

Copy the pattern, not the names:

- `cloudflare_access` config shape: account id, access app id, JWT audience, device policy id, API token.
- Per-device Common Name allowlist in the Cloudflare Access Service Auth policy. Do not use a broad `Valid Certificate` selector.
- Device enrollment writes Cloudflare policy before local enrollment completes.
- Device revocation removes the Common Name from Cloudflare before marking the local device revoked.
- If Cloudflare policy sync fails, enrollment/revocation fails closed.
- Origin verifies `cf-access-jwt-assertion` issuer/JWKS/audience before accepting Cloudflare mTLS as device identity.
- Tunnel validation proves exact hostnames route only to loopback/private origin, has no wildcard/private-network route, has no bypass policy, and has final deny/404 behavior.
- Public validation includes unauthenticated probes that must be blocked by Cloudflare before origin.

Office Automate source anchors:

- `../office-automate/config.example.yaml` `cloudflare_access`.
- `../office-automate/rust/office-automate-server/src/cloudflare.rs` policy sync.
- `../office-automate/rust/office-automate-server/src/device.rs` register/revoke device lifecycle.
- `../office-automate/rust/office-automate-server/src/auth.rs` Cloudflare Access JWT validation.
- `../office-automate/rust/office-automate-server/src/http.rs` service-auth gate and trusted-network bypass protections.
- `../office-automate/docs/deployment/primary-host-launchd.md` device Common Name policy and cert storage notes.

## SM-Specific Adaptations

SM must add stricter origin checks than Office Automate because the Android app can reach live agents and terminal attach.

- Browser interactive Access and app mTLS Access are separate route classes. A native app route must not be protected only by browser Access.
- SM origin rejects public forwarded traffic without a valid edge assertion or Cloudflare Access assertion, even if Cloudflare is misconfigured and forwards it.
- Native app routes require both Cloudflare device proof and SM user/device auth.
- Shell/attach routes still require route-specific capability checks. The current attach-ticket/device-key proof may remain until a replacement signed WebSocket handshake is designed and fixture-locked.
- Public artifact routes for app updates are available only through the app mTLS entry or through a separately reviewed signed-artifact route.
- Email ingress is retained, but only as worker/service identity plus existing worker secret and authorized sender checks. It does not grant generic origin access.
- Telegram remains removed from the Rust target.

## Device Lifecycle

The Rust target needs first-class device inventory and revocation before relying on the public app boundary:

| Command / API | Requirement |
| --- | --- |
| `sm list-devices` | Shows device id, display name, certificate Common Name, owner email, created time, last seen time, revoked status, and source app/version where available. Never prints private key material. |
| `sm remove-device <id>` | Removes the Common Name from Cloudflare Access, marks the local device revoked, invalidates active mobile attach tickets where feasible, and records an audit event. If Cloudflare removal fails, the command fails closed unless an explicit local-only emergency flag is added later. |
| enrollment command/API | Runs only from trusted local/operator context, creates a device certificate/key package for the app, syncs Cloudflare Common Name allowlist first, then commits the local device record. |

The Android private key/cert should live in Android secure storage / platform TLS client certificate storage where practical. If app-side certificate installation UX is too constrained, the implementation ticket must document the actual Android storage and TLS client-cert path before shipping.

## Origin Enforcement

At origin, Rust should classify incoming requests into one of these contexts:

- `local_loopback`: local operator/dev path; existing local bypass rules may apply only on loopback plus trusted local host checks.
- `browser_access`: valid Cloudflare Access user/browser session for the browser hostname. Still requires SM Google OAuth/session for operational data.
- `mobile_device_access`: valid Cloudflare mTLS/service assertion with enrolled, non-revoked device Common Name. Still requires SM device bearer token and route capabilities.
- `node_access`: valid Cloudflare mTLS/service assertion with enrolled, non-revoked node Common Name. Still requires node authorization.
- `worker_access`: valid worker/service proof for the inbound email route only.
- `invalid_public`: public/forwarded traffic without the required Cloudflare proof. Deny without operational data.

Origin must not trust raw client-supplied Cloudflare headers unless the request also comes through the approved edge path and the assertion verifies. The edge should strip incoming `CF-Access-*` and `X-SM-Edge-*` style headers before injecting its own.

## Route Classes

| Class | Examples | Cloudflare requirement | Origin requirement |
| --- | --- | --- | --- |
| Browser auth shell | `/`, `/watch`, `/auth/google/login`, `/auth/google/callback`, `/auth/session` | Browser Access email allowlist | SM Google OAuth/session before operational data. |
| Native app bootstrap/auth | `/client/bootstrap`, `/auth/device/google`, `/client/sessions`, `/client/sessions/{id}` | App mTLS enrolled device | Google ID token exchange or bearer token, owner allowlist. |
| Native mobile terminal | `/client/sessions/{id}/attach-ticket`, `/client/terminal`, `/client/mobile-terminal/*` | App mTLS enrolled device | Bearer token, mobile capability, attach ticket/WebSocket auth, quotas, revocation. |
| App artifacts | `/apps/*`, `/apk` | App mTLS enrolled device unless a signed-artifact exception is reviewed | Metadata/hash compatibility, no public unauthenticated artifact serving by default. |
| Node fallback | `/nodes/*`, node-agent/control routes | Node mTLS enrolled node | Node token/capability and LAN-first fallback logic. |
| Email worker | configured inbound email path | Worker/service identity and route allowlist | Worker secret, authorized sender, session routing checks. |
| Retired/public-denied | Telegram, retired CLI/API surfaces, non-allowlisted routes | Denied before origin | Origin denies if reached. |

## Validation Gates

Before cutover to this model:

- Unauthenticated public probes to browser, app, node, artifact, email, and retired paths are blocked by Cloudflare before origin.
- App-host requests without a client certificate never reach origin.
- App-host requests with an unknown/revoked certificate are denied by Cloudflare or origin.
- Browser-host requests without the owner Access identity cannot reach SM OAuth or watch routes.
- A valid app certificate without SM bearer auth gets origin 401/403 JSON, not operational data.
- A valid SM bearer token without app certificate cannot reach the origin remotely.
- Google ID-token exchange works through the app mTLS hostname without Cloudflare interactive redirects.
- `/auth/google/callback` works through the browser Access hostname.
- `sm list-devices` and `sm remove-device <id>` pass against a test Cloudflare policy and local inventory.
- Cloudflare policy sync failure fails enrollment/revocation closed.
- Cloudflared config validation proves exact hostnames, loopback/private origin, no wildcard DNS, no private-network route, no bypass policy, and final deny/404 route.
- Shadow/rehearsal evidence includes denied-public probes plus successful browser OAuth and Android native auth smoke checks.

## Implementation Slices

1. Add SM Cloudflare Access config and validation evidence model, copied from Office Automate patterns.
2. Add Cloudflare Access JWT/audience validation at origin and request context classification.
3. Add device certificate enrollment/list/remove/revocation with Cloudflare Common Name policy sync.
4. Add Android app TLS client certificate support and base URL separation for app hostname.
5. Wire native app routes to require app mTLS context plus existing SM bearer auth.
6. Add browser hostname validation and public OAuth callback/watch smoke checks.
7. Add node certificate inventory/revocation and LAN-first Cloudflare fallback.
8. Add edge/tunnel black-box validation and cutover rehearsal gates.

## Ticket Classification

Issue #945 is a single-ticket design artifact. Runtime implementation is an epic and should be filed as separate implementation tickets from the slices above after this spec is accepted.
