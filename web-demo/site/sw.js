// The demo's server. Every same-origin request the web UI makes lands here
// and is answered from the recording in /fixtures/ for the current point in
// the storyline; nothing reaches a live sm-server and nothing writes.
//
// Pass-through (the static host serves them): /assets/, /fixtures/, /demo/
// and this file. Page navigations get the app shell; /docs/… navigations
// (the reader's iframe, or full screen) get the recorded doc page.
'use strict';

const BUILD = '__SM_DEMO_BUILD__';
const STATE_CACHE = 'sm-demo-state';
const CLOCK_KEY = '/__demo/clock-state';
// A visitor who comes back after this long starts the story again.
const IDLE_RESTART_MS = 15 * 60 * 1000;
const READ_ONLY = 'Demo — read only';
const PASS_THROUGH = /^\/(assets|fixtures|demo)\/|^\/sw\.js$|^\/favicon\.ico$/;
const READER_PATH = /^\/(docs|messages|t)\//;

let timelinePromise = null;
const files = new Map();
let clock = null;
// URLs asked for but not recorded at that tick, for checking coverage
// (GET /__demo/misses).
const misses = new Map();

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin || PASS_THROUGH.test(url.pathname)) return;
  event.respondWith(handle(event.request, url).catch((error) => json(500, { detail: String(error) })));
});

async function handle(request, url) {
  if (url.pathname.startsWith('/__demo/')) return demoRoute(request, url);
  if (request.mode === 'navigate' && !READER_PATH.test(url.pathname)) return fetch('/');
  if (request.method !== 'GET' && request.method !== 'HEAD') {
    readOnly();
    return json(403, { detail: READ_ONLY, error: READ_ONLY });
  }
  if (url.pathname === '/health') return json(200, { status: 'healthy' });
  const timeline = await loadTimeline();
  const now = await storyNow(timeline);
  const entry = lookup(now.tick, url);
  if (!entry && /^\/docs\/[^/]+\/drafts$/.test(url.pathname)) return json(200, { drafts: [] });
  if (!entry && /^\/docs\/[^/]+\/reopen-target$/.test(url.pathname)) return json(200, { kind: 'refused', reason: READ_ONLY });
  if (!entry) {
    const key = url.pathname + url.search;
    if (misses.size < 500 || misses.has(key)) misses.set(key, [...(misses.get(key) || []), Math.floor(now.t)].slice(-5));
    return json(404, { detail: 'Not found' });
  }
  let body = shiftTimes(await fileText(entry.file), Date.now() - now.recordedAt);
  const html = (entry.content_type || '').includes('text/html');
  if (html) body = injectDemo(body);
  return new Response(request.method === 'HEAD' ? null : body, {
    status: entry.status || 200,
    headers: { 'content-type': entry.content_type || 'application/json', 'cache-control': 'no-store' },
  });
}

// ---- the story clock ---------------------------------------------------------

async function loadClock() {
  if (clock) return clock;
  try {
    const stored = await (await caches.open(STATE_CACHE)).match(CLOCK_KEY);
    clock = stored ? await stored.json() : null;
  } catch (_) { clock = null; }
  return clock;
}

async function saveClock() {
  clock.saved = Date.now();
  try {
    await (await caches.open(STATE_CACHE)).put(CLOCK_KEY, new Response(JSON.stringify(clock)));
  } catch (_) { /* The clock still runs from memory. */ }
}

async function restart(atSeconds = 0) {
  clock = { build: BUILD, start: Date.now() - atSeconds * 1000, lastSeen: Date.now(), saved: 0 };
  await saveClock();
}

async function storyNow(timeline) {
  const now = Date.now();
  await loadClock();
  if (!clock || clock.build !== BUILD || now - clock.lastSeen > IDLE_RESTART_MS) {
    await restart();
  } else {
    clock.lastSeen = now;
    if (now - clock.saved > 30000) await saveClock();
  }
  const d = timeline.duration_seconds;
  const t = ((((now - clock.start) / 1000) % d) + d) % d;
  let tick = timeline.ticks[0];
  for (const candidate of timeline.ticks) if (candidate.t <= t) tick = candidate;
  return { t, tick, recordedAt: recordedAt(timeline, tick, t) };
}

// The recording-machine time that story time `t` corresponds to, so ages
// ("3m ago", "running 2m") advance between ticks as they did live.
function recordedAt(timeline, tick, t) {
  const next = timeline.ticks[timeline.ticks.indexOf(tick) + 1];
  const base = parseTime(tick.captured_at);
  const span = next ? parseTime(next.captured_at) - base : timeline.tick_seconds * 1000;
  return base + ((t - tick.t) / timeline.tick_seconds) * span;
}

async function demoRoute(request, url) {
  if (url.pathname === '/__demo/misses') return json(200, Object.fromEntries(misses));
  const timeline = await loadTimeline();
  if (url.pathname === '/__demo/restart' && request.method === 'POST') await restart();
  // Jump to story time `t` seconds, for checking a chapter without waiting.
  if (url.pathname === '/__demo/seek' && request.method === 'POST') await restart(Number(url.searchParams.get('t')) || 0);
  const now = await storyNow(timeline);
  let chapter = timeline.chapters[0];
  for (const candidate of timeline.chapters) if (candidate.t <= now.t) chapter = candidate;
  return json(200, { t: now.t, duration: timeline.duration_seconds, chapter: chapter.caption });
}

// ---- the recording ------------------------------------------------------------

function loadTimeline() {
  if (!timelinePromise) {
    timelinePromise = fetch(`/fixtures/timeline.json?b=${BUILD}`).then((response) => {
      if (!response.ok) throw new Error(`timeline.json: HTTP ${response.status}`);
      return response.json();
    }).then((timeline) => {
      for (const tick of timeline.ticks) {
        tick.byKey = new Map();
        for (const [key, entry] of Object.entries(tick.responses)) tick.byKey.set(normalize(key), entry);
      }
      return timeline;
    });
    timelinePromise.catch(() => { timelinePromise = null; });
  }
  return timelinePromise;
}

// Keys are recorded exactly as the UI builds them. Query order and the
// reader's own `from` (full-screen return path) never change the answer.
function normalize(pathAndQuery) {
  const url = new URL(pathAndQuery, 'http://demo');
  const params = [...url.searchParams].filter(([name]) => name !== 'from').sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return url.pathname + (params.length ? `?${new URLSearchParams(params)}` : '');
}

function lookup(tick, url) {
  return tick.byKey.get(normalize(url.pathname + url.search)) || null;
}

async function fileText(file) {
  if (!files.has(file)) {
    const pending = fetch(`/fixtures/${file}`).then((response) => {
      if (!response.ok) throw new Error(`${file}: HTTP ${response.status}`);
      return response.text();
    });
    files.set(file, pending);
    pending.catch(() => files.delete(file));
  }
  return files.get(file);
}

// ---- responses ----------------------------------------------------------------

function json(status, value) {
  return new Response(JSON.stringify(value), {
    status, headers: { 'content-type': 'application/json', 'cache-control': 'no-store' },
  });
}

async function readOnly() {
  const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  for (const client of windows) client.postMessage({ type: 'sm-demo-read-only', text: READ_ONLY });
}

// Doc pages get the banner and the read-only notice too.
function injectDemo(html) {
  const tags = '<link rel="stylesheet" href="/demo/demo.css"><script src="/demo/demo.js" defer></script>';
  const at = html.indexOf('</head>');
  return at < 0 ? tags + html : html.slice(0, at) + tags + html.slice(at);
}

// ---- timestamps -----------------------------------------------------------------

// RFC 3339 with `Z` (0–6 fractional digits) and `YYYY-MM-DD HH:MM:SS` (UTC).
const STAMP = /(\d{4})-(\d{2})-(\d{2})([T ])(\d{2}):(\d{2}):(\d{2})(\.\d+)?(Z?)/g;

function parseTime(text) {
  STAMP.lastIndex = 0;
  const m = STAMP.exec(text);
  STAMP.lastIndex = 0;
  if (!m) return NaN;
  const ms = m[8] ? Number(`${m[8].slice(1)}00`.slice(0, 3)) : 0;
  return Date.UTC(+m[1], +m[2] - 1, +m[3], +m[5], +m[6], +m[7], ms);
}

const pad = (n, width = 2) => String(n).padStart(width, '0');

function shiftTimes(text, deltaMs) {
  const delta = Math.round(deltaMs);
  return text.replace(STAMP, (match, y, mo, d, sep, h, mi, s, frac = '', zulu) => {
    // `T` forms are RFC 3339 only with `Z`; the space form never has one.
    if ((sep === 'T') !== (zulu === 'Z')) return match;
    const ms = frac ? Number(`${frac.slice(1)}00`.slice(0, 3)) : 0;
    const at = new Date(Date.UTC(+y, +mo - 1, +d, +h, +mi, +s, ms) + delta);
    let out = `${pad(at.getUTCFullYear(), 4)}-${pad(at.getUTCMonth() + 1)}-${pad(at.getUTCDate())}${sep}`
      + `${pad(at.getUTCHours())}:${pad(at.getUTCMinutes())}:${pad(at.getUTCSeconds())}`;
    if (frac) {
      const digits = frac.length - 1;
      const millis = pad(at.getUTCMilliseconds(), 3);
      out += `.${digits <= 3 ? millis.slice(0, digits) : millis + frac.slice(4)}`;
    }
    return out + zulu;
  });
}

// For the unit test (node); the worker global has no `module`.
if (typeof module !== 'undefined') module.exports = { shiftTimes, parseTime, normalize, recordedAt };
