# Local egress and sm gateways

`ServiceClient` is the host-only interface to the separately supervised
`--local-egress-service` process. It runs outside every local-agent sandbox.
#2007 adds gateway transport; #2008 supplies HTTP route authorization and #2009
supplies queue confinement. #1974 must compose all three before enabling local
agents. A gateway alone is not an authorization boundary on an old sm server.

## Register and restore

1. Construct `ServiceClient` with the private service directory and an installed
   sm-server executable containing gateway support. Do not use a Cargo target
   executable. An already running older service must be upgraded before calling
   the new interface; an unknown registration request fails rather than silently
   creating an unstamped gateway.
2. Call `register_gateway(agent_id, sm_loopback_address)`. This also registers
   egress. The address is host configuration, never agent input. Gateways bind
   IPv4 loopback on 18600–18699; egress remains on 18700–18799. Existing
   `register_agent` callers retain their egress-only behavior.
3. Use `registration.environment()` in the cleared launch environment. It adds
   `SM_API_URL` for the gateway alongside the existing proxy/Git settings.
4. Build the wall admitting only the assigned gateway/egress ports, model and
   judge. Exclude this entire service directory from every wall, including its
   control socket, registrations and signing key. Preserve its physical path
   across restart. The service's Unix control socket is host-only, not an agent
   registration API.
5. `unregister_agent` suspends both listeners, terminates connections, and marks
   the registration inactive. Ports remain reserved. Re-registration restores
   the same ports and upstream. `release_agent` frees reservations only after
   unregister; the host must first establish that **all** processes using the
   old wall, including queue descendants, have exited.

The existing `registrations.json` format gains an optional `gateway` object
with `port` and `upstream`. Old records without it remain egress-only. Active
listeners and gateway ports restore from this file when the service restarts.
The upstream cannot change until release; a busy reserved port fails closed.
The server's own restart does not stop this separately supervised service.

## Verify before route dispatch

`ServiceClient::stamp_verifier()` returns a verifier rooted at the same private
service directory. On a blocking worker, call `verify` with the actual peer,
original method, original URI (including query), headers and exact body bytes.
Only `Some(VerifiedLocalAgent)` establishes agent authority. Verify **before**
rewriting caller fields, and retain/reconstruct the body for the route handler.
Do not treat the local-agent header, loopback peer or missing credentials as
proof of a local agent or as permission to use an owner-only route.

`None` establishes no stamped identity. A bare or forged identity header must
never confer authority. An I/O error must fail closed. Middleware must reject a
request that asserts a gateway signature but cannot verify it, rather than let
it fall into the localhost owner bypass; this also protects against missing or
invalid service state during deployment/recovery. Strip the wire stamp before
ordinary route dispatch and carry only the verified identity in a request
extension. Route policy remains responsible for choosing caller fields rather
than target/recipient fields and for rejecting owner-only operations.

The service stores a random 32-byte key in private `gateway.key`; the key never
travels over HTTP. `X-SM-Local-Agent` contains the registered id and
`X-SM-Gateway-Signature` contains `timestamp:base64url(HMAC-SHA256)`. The signature
covers length-prefixed protocol version, agent id, Unix timestamp, method,
path/query, and SHA-256 of the body. It expires after 120 seconds (5 seconds of
future clock tolerance). Verification also checks an active gateway registration
on every request. This authenticates requests; it does not provide exactly-once
execution or prevent repetition of an identical request within that interval.

## Transport limits

The gateway accepts origin-form GET, HEAD, POST, PUT, PATCH, DELETE and OPTIONS.
It forwards to the configured address only. It rejects CONNECT, TRACE,
absolute-form URLs and Upgrade, and refuses upstream redirects/protocol upgrades
while preserving 304 cache responses. It permits only ordinary content/cache
metadata headers, dropping caller identity, credentials, cookies, forwarded
headers and unknown headers. Upstream cookies/credential headers and trailers
are not returned. Caller JSON is unchanged until the route-policy layer.

Requests are buffered up to 8 MiB for signing. Responses stream with a 64 MiB
limit and 120-second idle timeout. Request processing has a 120-second deadline;
a client connection has a 300-second lifetime and no keep-alive. All gateways
share a 128-connection limit. Suspension cancels active transport futures.

Tests: `scripts/test-rust-isolated.sh local_egress:: -- --test-threads=1` covers
forged headers, signature tampering/expiry, state revocation, legacy records,
streaming, rejected targets/redirects, port conflicts, two-agent attribution,
restart and reserved-port lifetime, plus the original egress behavior.

## Combined gateway and queue acceptance

`local_egress::acceptance::two_agents_gateway_queue_restart_and_egress_attribution`
runs two production sandbox profiles, socket adapters, gateways, the real HTTP
router, SQLite queue storage, queue launch supervisors, and egress proxies. Each
agent submits with omitted, empty, and forged requester identity, with session
variables absent during submission and forged in the job environment. Every
notification targets the other session. The test restarts the HTTP server and
restores the host launch bindings before normal admission of the pending jobs.

All six jobs must finish successfully. Their connections must be logged under
their submitting agents, with exactly one tunnel per job and correct byte counts.
The sandbox must return a permission denial for direct public egress, the direct
sm listener, the other agent's gateway/proxy ports, and gateway-secret reads.
Gateway input carries forged identity, stamp, session, and credential headers.
The stored requester and durable wall identity must still match the listener.

This test has no external network dependency: a test-only DNS answer and dial
fixture send the proxy's approved public CONNECT tunnel to a local echo server.
The production destination checks and tunnel accounting still run. The dial
fixture is absent from production builds. Separate gateway verification, complete
HTTP route-policy, cancellation, hosted/owner, and native launch tests cover the
remaining #1979 acceptance cases.
