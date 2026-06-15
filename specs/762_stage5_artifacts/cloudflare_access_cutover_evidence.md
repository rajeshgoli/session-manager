# Cloudflare Access Cutover Evidence

Status: implementation evidence after PR #1012 merged.

This artifact records the current Rust-side Cloudflare Access boundary for the
Rust cutover track. It complements the design in
[`../945_cloudflare_access_auth_model.md`](../945_cloudflare_access_auth_model.md)
and does not replace the rollout gates in [gate_matrix.md](gate_matrix.md).

## Merged Runtime Boundary

Rust `main` now includes the Cloudflare Access origin gate, read-only smoke
evidence runner, Rust mobile-device enrollment, Cloudflare mTLS CA automation,
and Android Camera-app enrollment flow through PR #1012:

| PR | Evidence added |
| --- | --- |
| #946 | Design artifact for the SM Cloudflare Access model. |
| #948 | Rust config parsing and Cloudflare Access JWT/audience/context classification. |
| #950 | Origin route gates, JWKS caching/refresh, public-host fail-closed behavior, app artifact gating, device-token exchange gating, and mobile device identity binding. |
| #996 | Read-only Cloudflare Access smoke runner for mobile app origin-gate, public-edge proof, SM-auth boundary, app artifact metadata, and browser edge-only notes. |
| #1000 | Android Cloudflare Access client-certificate storage and OkHttp/WebSocket client-certificate presentation. |
| #1002 | Initial Android QR enrollment support; superseded by the Camera-app deep-link flow in #1012 for scanning. |
| #1005 | Rust `sm enroll-device`, 15-minute mobile-device pairing listener, mobile device DB enrollment, CSR signing, and per-device Common Name policy sync. |
| #1007 | Cloudflare mTLS CA upload/association automation for the mobile app hostname, keeping the CA private key local. |
| #1010 | Android artifact version-code/version-name override support so published APK metadata matches the installed build. |
| #1012 | Android Camera-app QR handoff via `sm-enroll://enroll`, direct in-app certificate save, no camera permission, no in-app scanner, and no certificate material exposed in Settings. |

Current origin behavior:

- Cloudflare Access is disabled by default for existing local configs.
- If any Cloudflare Access app is enabled, public requests to unknown hosts fail
  closed instead of falling through to local/session auth.
- Enabled Access apps with missing host/audience config fail closed as
  incomplete config.
- Browser, mobile app, node fallback, and email worker hostnames are classified
  as separate Access applications.
- `/client/*`, `/auth/device/google`, `/apps/*`, and `/apk` require the
  `MobileApp` Access application class before route-specific SM auth.
- `MobileApp` Access assertions require a verified JWT/audience and an enrolled,
  enabled, non-revoked device Common Name.
- Actor-bearing mobile/app routes additionally require that Access device
  Common Name to be registered under the same mobile user resolved from the SM session or bearer actor.
- `/client/bootstrap` remains pre-SM-auth and proves only enrolled mobile
  device identity before returning native bootstrap metadata.
- Local loopback plus trusted local host still preserves local operator bypass.
- Non-mobile Access contexts are not accepted for native app routes.
- `sm enroll-device` is the intended pairing path for native app
  client-certificate setup. The pairing token is short-lived, the app submits
  its Android Keystore-backed CSR, Rust signs it with the local mobile-device
  CA, and the app stores the returned credential internally.
- When Cloudflare API configuration is present, enrollment ensures the mobile
  device CA is uploaded/associated with the app hostname and syncs the enrolled
  device key id as a Common Name include entry. Broad "any valid certificate"
  policy remains out of scope for cutover approval.

The merged Rust tests currently cover:

```bash
cargo test -p sm-server cloudflare_access -- --nocapture
cargo test -p sm-server --test read_only_http \
  device_google_auth_route_preserves_validation_and_config_errors -- --nocapture
cargo test -p sm-server
```

## Required Cloudflare Applications

Use separate Cloudflare Access applications unless an explicit owner-reviewed
Cloudflare config proves equivalent host/path separation.

| Access application | Hostname | Required policy | Origin expectation |
| --- | --- | --- | --- |
| `sm-browser` | `sm.rajeshgo.li` | Interactive Access login allowlisted to the owner email. No bypass policy. | Existing SM Google OAuth/session cookie remains required for operational browser data. |
| `sm-mobile-app` | `sm-app.rajeshgo.li` | Service Auth or mTLS policy with Common Name include entries for enrolled SM mobile devices. No broad Valid Certificate policy. | Rust verifies Access JWT/audience, enrolled device Common Name, then SM Google/device bearer auth and route capabilities. |
| `sm-node-fallback` | node fallback hostname | Service Auth or mTLS policy with Common Name include entries for registered SM nodes. No broad Valid Certificate policy. | Rust must verify node Access context plus node credentials/capabilities when node fallback is ported. |
| `sm-email-worker` | email ingress hostname/path if split out | Worker/service identity or service token, exact route allowlist, no generic app/node/browser access. | Rust/Python still require route-local worker secret, trusted session header handling, authorized sender, and delivery checks. |

Cloudflare tunnel requirements:

- route only exact SM hostnames to loopback/private origin;
- do not expose wildcard DNS, private-network catchalls, or Access bypass;
- strip any incoming spoofed `CF-Access-*` / edge assertion headers before
  injecting trusted headers;
- deny or 404 unmatched ingress;
- record each Access application audience in SM config.

## Remaining Cutover Evidence

The Rust origin gate and smoke runner are merged, but public cutover still
needs operator-side Cloudflare and native-app evidence:

| Evidence | Required before public mobile cutover |
| --- | --- |
| Cloudflare policy setup | Prove each Access app exists with the expected hostname, audience, and no bypass/broad-certificate policy. |
| Mobile device enrollment | Prove an enrolled phone certificate Common Name appears in the `sm-mobile-app` policy and in SM mobile device config. |
| Revoked-device denial | Prove a removed/revoked device Common Name is denied by Cloudflare or origin and cannot use `/client/*`, `/apps/*`, `/apk`, or `/auth/device/google`. |
| Native app smoke | Exercise `/client/bootstrap`, `/auth/device/google`, `/client/sessions`, `/client/sessions/{id}`, attach ticket, WebSocket auth, request-status, analytics, bug report, and app artifact metadata/download through the app hostname. |
| Browser smoke | Exercise browser Access owner email login, SM Google OAuth callback, `/auth/session`, and authenticated/proofed watch diagnostics through the browser hostname. |
| Node fallback smoke | Once node fallback is ported, prove LAN-first behavior and Cloudflare node certificate fallback for a registered node. |
| Shadow/rehearsal | Record a clean shadow/rehearsal window that includes native app traffic or an explicit operator-driven mobile route exercise. |

The smoke runner requires real deployment inputs. The local shell used for the
post-#1012 handoff refresh did not have `CF_MOBILE_ACCESS_JWT`,
`CF_BROWSER_ACCESS_JWT`, `SM_PUBLIC_EDGE_SECRET`, `SM_DEVICE_BEARER_TOKEN`,
or `SM_COOKIE` set, and `--mobile-host` / `--browser-host` were not supplied,
so no passing Cloudflare/mobile smoke evidence was produced there. Missing
required mobile inputs are blockers by design.

The Android artifact published during PR #1012 is versionCode `1013`,
versionName `0.1.0-enroll-ui-cleanup`, artifact hash `cbb61798`. It supports
Camera-app enrollment handoff and direct internal credential storage, but that
published artifact is not by itself full Cloudflare Access cutover evidence.

Do not treat the Cloudflare Access design as complete cutover evidence until
these setup and smoke checks are recorded. The revoked-device denial and native
app smoke checks are release gates, not optional diagnostics.
