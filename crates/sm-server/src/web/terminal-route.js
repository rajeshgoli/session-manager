// Terminal route and round-trip time (spec 1782 G3, G4 item 3). No DOM or
// Preact here, so the choice can be tested in node.

const ROUTE_KEY = 'sm-term-route';
const DIRECT_KEY = 'sm-term-direct-at';
const ROUTE_MS = 10 * 60 * 1000;
export const PROBE_MS = 700;
// On the relay after this tab was on a direct socket, look for it again this long after attaching (#2242).
export const DIRECT_RETRY_MS = [2000, 5000, 15000, 30000, 60000];

const loopback = (host) => host === 'localhost' || host === '127.0.0.1' || host === '[::1]';

/** Today's relative `ws_url`, through Cloudflare unless the page itself is local. */
export function relayRoute(ticket, location) {
  const url = new URL(ticket.ws_url, location.href);
  url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return { url: url.href, direct: false, label: loopback(location.hostname) ? 'Direct' : 'Cloudflare' };
}

/** The remembered route, if it is under ten minutes old and names this server instance. */
function remembered(storage, instance, now) {
  try {
    const value = JSON.parse(storage.getItem(ROUTE_KEY));
    const fresh = value && typeof value.url === 'string' && now - value.at < ROUTE_MS && value.instance === instance;
    return fresh ? value.url : null;
  } catch (e) {
    return null;
  }
}

/** Remember a route ('relay' or a direct url) to this server instance for ten minutes. */
export function rememberRoute(storage, url, instance, now) {
  try {
    storage.setItem(ROUTE_KEY, JSON.stringify({ url, instance, at: now }));
  } catch (e) {
    /* private window: probe again next time */
  }
}

/** Note that this tab is on a direct socket now. */
export function markDirect(storage, now) {
  try { storage.setItem(DIRECT_KEY, String(now)); } catch (e) { /* private window */ }
}

/**
 * True when this tab was on a direct socket in the last ten minutes: a relay
 * route then most likely came from a server restart, not from being away from
 * this machine's network, so probing for the direct path again is worth it.
 */
export function recentlyDirect(storage, now) {
  try { return now - Number(storage.getItem(DIRECT_KEY)) < ROUTE_MS; } catch (e) { return false; }
}

/** True when the probe answers with this server's instance within the timeout. */
async function probe(fetchImpl, url, instance, ms) {
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), ms);
  try {
    const response = await fetchImpl(url, { signal: abort.signal, cache: 'no-store', credentials: 'omit' });
    if (!response.ok) return false;
    const body = await response.json();
    return !!body && body.instance === instance;
  } catch (e) {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Pick the socket for one connection. Probe every direct entry in parallel and
 * take the first, in list order, whose instance matches the ticket's; a probe
 * that answers with another instance is someone else's server and is skipped.
 * With no match, use the relay. A route remembered within ten minutes skips
 * the probes, unless the server instance has changed since.
 */
export async function chooseRoute(ticket, { location, fetchImpl, storage, now = Date.now(), ms = PROBE_MS }) {
  const relay = relayRoute(ticket, location);
  const direct = Array.isArray(ticket.direct) ? ticket.direct : [];
  if (!ticket.server_instance || !direct.length) return relay;
  const known = remembered(storage, ticket.server_instance, now);
  if (known === 'relay') return relay;
  if (known && direct.some((entry) => entry.url === known)) return { url: known, direct: true, label: 'Direct' };
  const found = await findDirect(ticket, { fetchImpl, ms });
  const chosen = found ? { url: found, direct: true, label: 'Direct' } : relay;
  rememberRoute(storage, chosen.direct ? chosen.url : 'relay', ticket.server_instance, now);
  return chosen;
}

/** The first direct url, in list order, whose probe answers with the ticket's instance; else null. */
export async function findDirect(ticket, { fetchImpl, ms = PROBE_MS }) {
  const direct = Array.isArray(ticket.direct) ? ticket.direct : [];
  if (!ticket.server_instance || !direct.length) return null;
  const answers = await Promise.all(direct.map((entry) => probe(fetchImpl, entry.probe, ticket.server_instance, ms)));
  const index = answers.indexOf(true);
  return index < 0 ? null : direct[index].url;
}

/** The median of the last three round trips, in whole ms. */
export function medianMs(samples) {
  const last = samples.slice(-3).sort((a, b) => a - b);
  if (!last.length) return null;
  const mid = Math.floor(last.length / 2);
  return Math.round(last.length % 2 ? last[mid] : (last[mid - 1] + last[mid]) / 2);
}

/** "Direct · 3 ms", or the route alone before the first pong. */
export function routeText(label, samples) {
  const ms = medianMs(samples);
  return ms === null ? label : `${label} · ${ms} ms`;
}
