// Terminal route and round-trip time (spec 1782 G3, G4 item 3). No DOM or
// Preact here, so the choice can be tested in node.

const ROUTE_KEY = 'sm-term-route';
const ROUTE_MS = 10 * 60 * 1000;
export const PROBE_MS = 700;

const loopback = (host) => host === 'localhost' || host === '127.0.0.1' || host === '[::1]';

/** Today's relative `ws_url`, through Cloudflare unless the page itself is local. */
export function relayRoute(ticket, location) {
  const url = new URL(ticket.ws_url, location.href);
  url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return { url: url.href, direct: false, label: loopback(location.hostname) ? 'Direct' : 'Cloudflare' };
}

function remembered(storage, now) {
  try {
    const value = JSON.parse(storage.getItem(ROUTE_KEY));
    return value && typeof value.url === 'string' && now - value.at < ROUTE_MS ? value.url : null;
  } catch (e) {
    return null;
  }
}

/** Remember a route ('relay' or a direct url) for ten minutes. */
export function rememberRoute(storage, url, now) {
  try {
    storage.setItem(ROUTE_KEY, JSON.stringify({ url, at: now }));
  } catch (e) {
    /* private window: probe again next time */
  }
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
 * the probes.
 */
export async function chooseRoute(ticket, { location, fetchImpl, storage, now = Date.now(), ms = PROBE_MS }) {
  const relay = relayRoute(ticket, location);
  const direct = Array.isArray(ticket.direct) ? ticket.direct : [];
  if (!ticket.server_instance || !direct.length) return relay;
  const known = remembered(storage, now);
  if (known === 'relay') return relay;
  if (known && direct.some((entry) => entry.url === known)) return { url: known, direct: true, label: 'Direct' };
  const answers = await Promise.all(direct.map((entry) => probe(fetchImpl, entry.probe, ticket.server_instance, ms)));
  const index = answers.indexOf(true);
  const chosen = index < 0 ? relay : { url: direct[index].url, direct: true, label: 'Direct' };
  rememberRoute(storage, chosen.direct ? chosen.url : 'relay', now);
  return chosen;
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
