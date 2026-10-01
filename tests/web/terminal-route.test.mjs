// Run: node --test tests/web/terminal-route.test.mjs
// Spec 1782 G3 and G4 item 3: which socket the web terminal opens, and the round-trip text.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { chooseRoute, relayRoute, medianMs, routeText } from '../../crates/sm-server/src/web/terminal-route.js';

const page = { href: 'https://sm.example.com/terminal/abc', protocol: 'https:', hostname: 'sm.example.com' };
const LOCAL = { url: 'ws://localhost:8420/client/terminal', probe: 'http://localhost:8420/client/terminal/probe' };
const LAN = { url: 'wss://studio-lan.rajeshgo.li:8443/client/terminal', probe: 'https://studio-lan.rajeshgo.li:8443/client/terminal/probe' };
const ticket = (direct = [LOCAL, LAN]) => ({ ws_url: '/client/terminal', server_instance: 'inst-1', direct });

function memory() {
  const items = new Map();
  return { getItem: (k) => (items.has(k) ? items.get(k) : null), setItem: (k, v) => items.set(k, v), items };
}
/** A fetch whose probes answer from `answers`: an instance, 'hang', or 'fail'. */
function fakeFetch(answers) {
  const calls = [];
  const impl = (url, init) => {
    calls.push(url);
    const answer = answers[url];
    if (answer === 'hang') {
      return new Promise((_, reject) => init.signal.addEventListener('abort', () => reject(new Error('aborted'))));
    }
    if (answer === undefined || answer === 'fail') return Promise.reject(new TypeError('Failed to fetch'));
    return Promise.resolve({ ok: true, json: async () => ({ instance: answer }) });
  };
  return { impl, calls };
}

test('the first direct entry whose probe matches the instance wins, in list order', async () => {
  const fetch = fakeFetch({ [LOCAL.probe]: 'inst-1', [LAN.probe]: 'inst-1' });
  const route = await chooseRoute(ticket(), { location: page, fetchImpl: fetch.impl, storage: memory() });
  assert.deepEqual(route, { url: LOCAL.url, direct: true, label: 'Direct' });
  assert.deepEqual(fetch.calls, [LOCAL.probe, LAN.probe], 'all probes run in parallel');
});

test('a probe answering with another instance is skipped, so the ticket never goes to it', async () => {
  // The MacBook's own server answers on its localhost; only the Studio's LAN name matches.
  const fetch = fakeFetch({ [LOCAL.probe]: 'macbook', [LAN.probe]: 'inst-1' });
  const route = await chooseRoute(ticket(), { location: page, fetchImpl: fetch.impl, storage: memory() });
  assert.equal(route.url, LAN.url);
  const none = fakeFetch({ [LOCAL.probe]: 'macbook', [LAN.probe]: 'fail' });
  const relay = await chooseRoute(ticket(), { location: page, fetchImpl: none.impl, storage: memory() });
  assert.deepEqual(relay, { url: 'wss://sm.example.com/client/terminal', direct: false, label: 'Cloudflare' });
});

test('a probe that does not answer within the timeout counts as no match', async () => {
  const fetch = fakeFetch({ [LOCAL.probe]: 'hang' });
  const started = Date.now();
  const route = await chooseRoute(ticket([LOCAL]), { location: page, fetchImpl: fetch.impl, storage: memory(), ms: 50 });
  assert.equal(route.direct, false);
  assert.ok(Date.now() - started < 1000);
});

test('the chosen route is remembered for ten minutes and skips the probes', async () => {
  const storage = memory();
  const now = Date.parse('2026-09-30T20:00:00Z');
  const first = fakeFetch({ [LOCAL.probe]: 'inst-1' });
  await chooseRoute(ticket(), { location: page, fetchImpl: first.impl, storage, now });
  const again = fakeFetch({});
  const route = await chooseRoute(ticket(), { location: page, fetchImpl: again.impl, storage, now: now + 9 * 60000 });
  assert.equal(route.url, LOCAL.url);
  assert.deepEqual(again.calls, []);
  const later = await chooseRoute(ticket(), { location: page, fetchImpl: again.impl, storage, now: now + 11 * 60000 });
  assert.equal(later.direct, false, 'after ten minutes the probes run again');
  assert.equal(again.calls.length, 2);
  // A remembered relay also skips the probes.
  const relayOnly = fakeFetch({ [LOCAL.probe]: 'inst-1' });
  const kept = await chooseRoute(ticket(), { location: page, fetchImpl: relayOnly.impl, storage, now: now + 12 * 60000 });
  assert.equal(kept.direct, false);
  assert.deepEqual(relayOnly.calls, []);
});

test('no direct list, no instance, or no storage falls back safely', async () => {
  const fetch = fakeFetch({ [LOCAL.probe]: 'inst-1' });
  assert.equal((await chooseRoute({ ws_url: '/client/terminal' }, { location: page, fetchImpl: fetch.impl, storage: memory() })).direct, false);
  assert.equal((await chooseRoute(ticket(), { location: page, fetchImpl: fetch.impl, storage: null })).url, LOCAL.url);
});

test('the relay keeps today\'s relative url and names a local page Direct', () => {
  assert.equal(relayRoute(ticket(), page).url, 'wss://sm.example.com/client/terminal');
  const local = relayRoute(ticket(), { href: 'http://localhost:8420/terminal/x', protocol: 'http:', hostname: 'localhost' });
  assert.deepEqual(local, { url: 'ws://localhost:8420/client/terminal', direct: false, label: 'Direct' });
});

test('round trip: the median of the last three, in whole ms', () => {
  assert.equal(medianMs([]), null);
  assert.equal(medianMs([2.6]), 3);
  assert.equal(medianMs([2, 5]), 4);
  assert.equal(medianMs([90, 3.2, 40, 2.9]), 3, "the oldest sample drops out");
  assert.equal(routeText('Direct', []), 'Direct');
  assert.equal(routeText('Direct', [2.9, 3.4, 12]), 'Direct · 3 ms');
  assert.equal(routeText('Cloudflare', [61, 48, 52]), 'Cloudflare · 52 ms');
});
