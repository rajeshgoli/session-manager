# Stage 4 Public Edge Proof Model

Status: owner security feedback incorporated after staged review convergence.

This artifact expands T32 in [threat_register.md](threat_register.md). It captures the owner-approved remote-access direction for the Rust cutover: Cloudflare or another public tunnel denies callers before traffic reaches the Session Manager origin unless the caller proves possession of an enrolled phone or registered node credential. Current Python behavior remains rollback/source evidence for the migration window, not the Rust target for public operational access.

## Target Boundary

The target shape is:

- Session Manager origin binds to loopback/private LAN where feasible and is not directly reachable from the internet.
- Public edge forwards only an explicit allowlist of routes needed for native mobile, proofed/signed app update, minimal browser auth shell if needed, email worker ingress, and registered-node fallback.
- Public edge verifies proof before forwarding operational data or control traffic.
- Origin also verifies a signed edge assertion, OAuth/session or SM device-bearer user auth for human/mobile/browser paths, and the route-local auth that already exists today. Cloudflare denial is treated as the first gate, not the only gate.
- Public unauthenticated responses must return no operational/session data. Auth/login/static shell responses may remain only if they do not disclose session state, secrets, node data, or attach metadata.
- Mobile enrollment happens through a trusted local/operator path. Remote first-use enrollment is not a default.
- Registered nodes prefer LAN `studio.local`. A node such as `macbook` may use public-edge fallback only when LAN reachability fails and the node proves possession of its configured credential.

## Operational Auth Order

For human/mobile/browser traffic, the intended order is:

1. Public edge proof: Cloudflare/public edge verifies the request comes from an enrolled device before forwarding. Without this proof, the origin should not see the request.
2. Origin edge assertion: Session Manager verifies that the request really came through the approved edge and was allowed for this route.
3. User authorization: Session Manager verifies Google OAuth/session or a signed SM device-bearer token tied to an allowlisted account. Device possession does not replace user authorization.
4. Route capability: high-risk actions still require route-local capability checks such as the current mobile attach-ticket plus first-frame terminal auth, or an owner-approved replacement direct signed WebSocket attach proof, plus session authority checks, hook secrets, or app-upload actor auth.

For registered node fallback traffic, OAuth is not the right second factor. The intended order is edge node proof, origin edge assertion, then the existing node-agent/node-token or route-local node authorization. Node fallback must remain LAN-first and route-allowlisted.

## Proof Material

| Caller | Preferred Proof | Enrollment / Rotation | Revocation |
| --- | --- | --- | --- |
| native mobile app | Non-exportable asymmetric device key or equivalent proof-of-possession credential; avoid long-lived bearer-only secrets when feasible. Per-request or per-session proof binds method, path, timestamp/nonce, and body hash where applicable. | Operator/trusted-LAN enrollment issues a device id/public key record and short enrollment window. Store private material in OS secure storage. | `sm list-devices` and `sm remove-device <id>` or equivalent API/CLI before relying on the boundary. Revocation must fail closed at edge and origin. |
| registered node fallback | Node-specific key/token distinct from hook secret where feasible; proof binds node id, route, timestamp/nonce, and control channel. | Configured or locally enrolled per node; LAN-first behavior remains preferred. Token/key rotation must be explicit and auditable. | Node removal/token rotation prevents public-edge fallback and origin acceptance for that node. |
| browser/watch diagnostics | Browser auth session or device proof before any operational data; public shell only may be unauthenticated and must not disclose session state. | OAuth/session compatibility applies only to retained local/auth/proofed diagnostics. Rust does not preserve unauthenticated public operational browser data. | Session logout/expiry and signing-key rotation are explicit cutover ledger events when required. |
| email worker | Worker secret and, where available, Cloudflare service identity or edge assertion for only the inbound email route. This is not phone proof and must not grant generic origin access. | Configured secret with fixed/default or explicitly allowlisted webhook route; authorized sender list; trusted session-id header allowed only after worker proof. | Rotate worker secret/service identity; remove webhook route from edge allowlist if compromised. |

## Attack Cases And Defenses

| Attack / Failure | Defense | If Defense Fails |
| --- | --- | --- |
| Internet caller lacks device/node proof. | Cloudflare/public edge denies before origin. Response contains no operational data. | Origin rejects missing/invalid signed edge assertion and existing route-local auth still applies. Alert on unexpected origin public traffic without valid edge assertion. |
| Caller has device proof but no OAuth/allowlisted user auth. | Origin requires Google OAuth/session or SM device-bearer user auth after edge proof for human/mobile/browser routes. | Request is denied at origin. Device proof is logged as credential possession only, not user authorization. |
| Caller has OAuth/session auth but no enrolled-device proof. | Public edge denies before origin for operational routes. | If misforwarded, origin rejects missing edge assertion/device-proof context before returning operational data. |
| OAuth account is compromised but phone key is not. | Public edge still requires enrolled-device proof before origin. | Attacker cannot reach operational routes remotely; local/browser sessions remain governed by current OAuth/session controls and should be revocable/rotatable through Stage 5. |
| Phone key is stolen but Google/account auth is not. | Origin still requires OAuth/session or SM device-bearer auth tied to an allowlisted account, and route-local capability for shell operations. | Revoke the device with `sm remove-device <id>`; active attach tickets for that device are invalidated where feasible. |
| Cloudflare rule or tunnel route accidentally forwards too much. | Default-deny route allowlist; generated edge-route manifest; external black-box tests for denied paths; origin bound loopback/private where feasible. | Origin must reject public forwarded traffic without signed edge assertion and keep route-local auth checks. Public exposure kill switch disables forwarding. |
| Cloudflare account/tunnel credential is compromised. | Edge policy is not the sole trust root: origin verifies edge assertion/device/node proof; tunnel credential is least-privilege; config drift is audited. | Rotate edge/controller secrets and device/node credentials; disable public-edge forwarding; rely on LAN/local access. |
| Edge process is compromised. | Edge has a narrow route allowlist, no durable session state, no access to local SM stores, and forwards with an internal controller token only to allowed routes. | Origin route-local auth, device/node proof, and signed edge assertion limit blast radius. Rotate controller token and edge signing key; disable edge. |
| Origin becomes directly reachable, bypassing edge. | Bind origin to loopback/private address; firewall where feasible; origin rejects public-host/forwarded requests without edge assertion. | Treat as incident; public auth tests fail; kill switch or service config returns to loopback-only. |
| Device private key or token is stolen. | Prefer non-exportable device key; proof includes nonce/timestamp/path/body; short-lived derived sessions; audit by device id. | `sm remove-device <id>` revokes. Origin and edge deny revoked id; active mobile terminal tickets for that device are invalidated where feasible. |
| Node fallback credential is stolen. | Node id binding, route allowlist, replay protection, LAN-first behavior, and distinct node token. | Rotate/remove node token; disable public node fallback; active controls for that node fail closed. |
| Replay of proof headers or WebSocket auth. | Nonce cache or signed challenge, timestamp window, method/path/body binding, and TLS. WebSocket upgrade and first-frame tickets bind to device/node id. | Replayed proof is denied at edge and origin. On nonce-cache uncertainty, fail closed or require fresh challenge. |
| Attach-ticket flow is flaky or consumed before WebSocket succeeds. | Current Python uses a short-lived two-step ticket. Rust may either keep that model or replace it with a direct signed WebSocket attach handshake if it preserves user auth, device proof, session binding, quotas, revocation, audit, and fail-closed behavior. | Do not fall back to unauthenticated terminal access. Chosen attach auth must be fixture-locked with the native app. |
| Revocation cache is stale at edge. | Short revocation-cache TTL, origin re-checks revocation on forwarded requests, emergency edge deny-all switch. | Origin denial prevents accepted operation; stale-edge accept is logged as an incident. |
| Clock skew breaks valid devices. | Prefer challenge/nonce over timestamp-only proof; allow narrow skew with clear error. | Valid client fails closed and can re-enroll or sync time locally; no unauthenticated fallback to operational data. |
| Header spoofing or request smuggling. | Edge strips incoming `X-SM-Edge-*`/proof headers and injects its own signed assertion; origin trusts only edge-signed assertions and local source policy. | Origin rejects invalid assertion; malformed requests are not forwarded. |
| Public unauthenticated bootstrap leaks metadata. | Public shell/login/bootstrap fields are minimized and fixture-locked; no sessions, node metadata, attach descriptors, raw proxy commands, or secrets. | Treat field expansion as public-data change requiring review. |
| APK/app artifact route remains public. | Rust must require auth/proof or signed artifact metadata before public serving. | If upload path is compromised, public clients may install stale/malicious artifacts; kill public artifact serving and rotate artifacts. |
| Email worker route is exposed too broadly. | Edge allowlist permits only the fixed/default or explicitly configured inbound email path; origin requires worker proof and authorized sender before any session lookup or delivery. | Disable inbound email kill switch, rotate worker secret/service identity, remove route from edge allowlist, and audit accepted messages. |
| Telegram bypasses this boundary. | Telegram bot/control is removed from the first Rust release. | Python rollback may restore old Telegram behavior during the migration window; Rust must not reintroduce it without a later owner-approved issue. |
| Cloudflare outage or edge unavailable. | LAN/local access remains available; nodes prefer `studio.local`; mobile remote access fails closed. | No public operational fallback without proof. Operator uses local/LAN access or disables edge dependency after review. |
| Denial of service at edge. | Edge rejects before origin, rate-limits by credential/ip/path, bounds body/WebSocket frames, and exposes deny metrics. | Public remote access degrades but origin/local operation remains protected. |

## Required Fixtures Before Adoption

- External unauthenticated request to each denied public route never reaches origin and receives no operational data.
- Origin rejects a public-host request without a valid edge assertion even if Cloudflare forwards it.
- Valid edge device proof without OAuth/session or SM device-bearer user auth is denied at origin.
- Valid OAuth/session or SM device-bearer auth without edge device proof is denied before origin, and denied at origin if misforwarded.
- Compromised-OAuth simulation without enrolled-device proof cannot reach operational mobile/watch/shell routes.
- Valid enrolled phone can reach native mobile bootstrap/session/attach-ticket flows; revoked phone cannot.
- Valid phone proof cannot be replayed with a different method/path/body or outside the nonce/timestamp window.
- `sm list-devices` shows enrolled device id/name/last seen/created/revoked status without private key material.
- `sm remove-device <id>` revokes future edge/origin access and invalidates active attach tickets where feasible.
- Node `macbook` uses LAN path when `studio.local` is reachable and public-edge fallback only after simulated LAN failure plus valid node proof.
- Revoked node proof and wrong-node proof are denied at edge and origin.
- Edge route allowlist denies non-mobile/non-node operational routes unless separately reviewed.
- Public browser/watch returns only auth shell or local/auth/proofed diagnostic data, matching the approved Stage 5 cutover scope.
- Email worker/inbound email route accepts only worker-proofed, authorized-sender requests; trusted session header is ignored unless worker proof is valid.
- Telegram command routes and bot-control paths are absent or return explicit retirement errors in Rust.

## Stage 5 Decisions

Stage 5 has decided:

- first Rust release uses proof-required public access by default for operational routes.
- edge forwarding is limited to owner-approved native mobile, minimal auth shell, proofed/signed app update, worker-proofed inbound email, and registered-node fallback routes.
- device inventory/revocation is required, with `sm list-devices` and `sm remove-device <id>` or equivalent commands/APIs.
- registered nodes may use public fallback for approved control streams only after LAN reachability fails and node proof succeeds.
- mobile terminal may keep the current two-step attach-ticket flow or move to direct signed WebSocket attach proof, provided the chosen design preserves user auth, device proof, session binding, quotas, revocation, audit, and fail-closed behavior.
- public app artifacts must require auth/proof or artifact signing.
- email/human recipient delivery and inbound email remain retained fallback surfaces after Telegram removal, with worker proof, authorized sender checks, and explicit route allowlisting.
- Telegram is deprecated in favor of the native app and is not ported to Rust.
- rollback to Python may restore old public behavior only during the migration window; Rust does not need to downgrade proof-required public access into unauthenticated Python-compatible public operational data.
