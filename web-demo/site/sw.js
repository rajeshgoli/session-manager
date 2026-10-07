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
const NOTES_KEY = '/__demo/notes-state';
// A visitor who comes back after this long starts the story again.
const IDLE_RESTART_MS = 15 * 60 * 1000;
const READ_ONLY = 'Demo — read only';
// What the page's notice says a write would have done (first match wins).
const NOTICES = [
  [/^\/notes(\/|$)/, 'Nothing is saved in the demo. Install Session Manager to keep your notes.'],
  [/^\/sessions\/[^/]+\/restore$/, 'Restore brings a retired agent back with its whole conversation. This demo is a recording, so it can\'t; install Session Manager to try it.'],
  [/.*/, 'This demo is a recording, so nothing changes here. Install Session Manager to do this for real.'],
];
// Writes the UI makes by itself, not because the visitor asked: answer them quietly.
const SILENT_WRITES = /^\/client\/board\/seen$/;
const PASS_THROUGH = /^\/(assets|fixtures|demo)\/|^\/sw\.js$|^\/favicon\.ico$/;
const READER_PATH = /^\/(docs|messages|t)\//;

let timelinePromise = null;
let staticPromise = null;
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
  if (url.pathname.startsWith('/notes')) return notesRoute(request, url);
  if (request.method !== 'GET' && request.method !== 'HEAD') {
    if (SILENT_WRITES.test(url.pathname)) return json(200, {});
    readOnly(url.pathname);
    return json(403, { detail: READ_ONLY, error: READ_ONLY });
  }
  if (url.pathname === '/health') return json(200, { status: 'healthy' });
  const timeline = await loadTimeline();
  const now = await storyNow(timeline);
  let entry = lookup(now.tick, url);
  let recordedAt = now.recordedAt;
  // History search and repo filter: the recorder captured the unfiltered
  // lists; filter them here the way the server would.
  const filter = !entry && historyFilter(url);
  if (filter) entry = lookup(now.tick, filter.base);
  if (!entry) {
    // Not part of the storyline (Analytics): one snapshot, its ages kept real.
    entry = (await loadStatic()).get(normalize(url.pathname + url.search)) || null;
    if (entry) recordedAt = parseTime(entry.captured_at);
  }
  if (!entry && /^\/docs\/[^/]+\/drafts$/.test(url.pathname)) return json(200, { drafts: [] });
  if (!entry && /^\/docs\/[^/]+\/reopen-target$/.test(url.pathname)) return json(200, { kind: 'refused', reason: READ_ONLY });
  if (!entry) {
    const key = url.pathname + url.search;
    if (misses.size < 500 || misses.has(key)) misses.set(key, [...(misses.get(key) || []), Math.floor(now.t)].slice(-5));
    return json(404, { detail: 'Not found' });
  }
  let body = shiftTimes(await fileText(entry.file), Date.now() - recordedAt);
  if (filter) body = JSON.stringify(filter.apply(JSON.parse(body)));
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

function loadStatic() {
  if (!staticPromise) {
    staticPromise = fetch(`/fixtures/static.json?b=${BUILD}`)
      .then((response) => (response.ok ? response.json() : { responses: {} }))
      .then((index) => new Map(Object.entries(index.responses).map(([key, entry]) => [normalize(key), entry])));
    staticPromise.catch(() => { staticPromise = null; });
  }
  return staticPromise;
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

// agent_history.rs matches_query and history.rs's repo filter, over one page.
function historyFilter(url) {
  const base = new URL(url);
  if (url.pathname === '/history/agents') {
    const q = (url.searchParams.get('q') || '').trim().toLowerCase();
    if (!q) return null;
    base.searchParams.set('q', '');
    const number = /^#?\d+$/.test(q) ? Number(q.replace('#', '')) : null;
    const matches = (agent) => agent.id.startsWith(q)
      || [agent.name, agent.role, agent.working_dir].some((text) => (text || '').toLowerCase().includes(q))
      || (number !== null && Object.values(agent.work || {}).flat().some((item) => item && item.number === number));
    return { base, apply: (page) => { const agents = page.agents.filter(matches); return { ...page, agents, total: agents.length, next_before: null }; } };
  }
  if (url.pathname === '/history') {
    const repo = (url.searchParams.get('repo') || '').trim();
    if (!repo) return null;
    base.searchParams.set('repo', '');
    return { base, apply: (page) => ({ ...page, rows: page.rows.filter((row) => row.repo === repo), next_before: null }) };
  }
  return null;
}

// ---- responses ----------------------------------------------------------------

function json(status, value) {
  return new Response(JSON.stringify(value), {
    status, headers: { 'content-type': 'application/json', 'cache-control': 'no-store' },
  });
}

async function readOnly(path) {
  const text = NOTICES.find(([pattern]) => pattern.test(path))[1];
  const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  for (const client of windows) client.postMessage({ type: 'sm-demo-read-only', text });
}

// ---- notes ----------------------------------------------------------------------

// Notes work for real within a visit (new, edit, search, preview, history),
// starting from fixtures/static/notes.json. The browser may stop the worker
// between requests, so changes are kept in the state cache for the visit (one
// story clock); a new visit starts from the seed again.
let notesPromise = null;
let notesVisit = null;
let lastSaveNotice = 0;

async function visitId() {
  const current = await loadClock();
  return current ? `${current.build}:${current.start}` : null;
}

async function loadNotes() {
  const visit = await visitId();
  if (notesPromise && visit !== notesVisit) notesPromise = null;
  notesVisit = visit;
  if (!notesPromise) notesPromise = storedNotes(visit).then((stored) => stored || seedNotes());
  notesPromise.catch(() => { notesPromise = null; });
  return notesPromise;
}

async function storedNotes(visit) {
  try {
    const stored = await (await caches.open(STATE_CACHE)).match(NOTES_KEY);
    const state = stored ? await stored.json() : null;
    return state && visit && state.visit === visit ? new Map(state.notes.map((note) => [note.id, note])) : null;
  } catch (_) { return null; }
}

async function saveNotes(notes) {
  try {
    const state = { visit: notesVisit, notes: [...notes.values()] };
    await (await caches.open(STATE_CACHE)).put(NOTES_KEY, new Response(JSON.stringify(state)));
  } catch (_) { /* Notes still work from memory. */ }
}

function seedNotes() {
  return fetch(`/fixtures/static/notes.json?b=${BUILD}`).then((response) => {
    if (!response.ok) throw new Error(`notes.json: HTTP ${response.status}`);
    return response.text();
  }).then((text) => {
    const seed = JSON.parse(text);
    const notes = JSON.parse(shiftTimes(text, Date.now() - parseTime(seed.captured_at))).notes;
    return new Map(notes.map((note) => [note.id, { ...note, title: noteTitle(note.body) }]));
  });
}

// notes.rs note_title: the first line without leading #s, 80 characters.
function noteTitle(body) {
  return [...(body.split('\n')[0] || '').replace(/^#+/, '').trim()].slice(0, 80).join('');
}

function saveNotice() {
  if (Date.now() - lastSaveNotice < 20000) return;
  lastSaveNotice = Date.now();
  readOnly('/notes');
}

function snippet(body, q) {
  const at = q ? body.toLowerCase().indexOf(q.toLowerCase()) : -1;
  if (at < 0) return { snippet: body.slice(0, 200), matches: [] };
  const from = Math.max(0, at - 60);
  return { snippet: body.slice(from, at + 140), matches: [{ start: at - from, end: at - from + q.length }] };
}

function noteJson(note) {
  const { id, title, body, version, updated_at: updatedAt } = note;
  return { id, title, body, version, updated_at: updatedAt };
}

function saveNote(note, body) {
  const at = new Date().toISOString();
  note.revisions = [{ version: note.version + 1, at, body }, ...(note.revisions || [])];
  Object.assign(note, { body, title: noteTitle(body), version: note.version + 1, updated_at: at });
  return note;
}

async function notesRoute(request, url) {
  const notes = await loadNotes();
  const response = await notesResponse(notes, request, url);
  if (request.method !== 'GET' && response.ok) await saveNotes(notes);
  return response;
}

async function notesResponse(notes, request, url) {
  const method = request.method;
  const input = method === 'POST' || method === 'PUT' ? await request.json().catch(() => ({})) : {};
  const parts = url.pathname.split('/').slice(2).map(decodeURIComponent);
  if (parts[0] === 'search' && method === 'GET') {
    const q = url.searchParams.get('q') || '';
    const rows = [...notes.values()].filter((note) => !q || note.body.toLowerCase().includes(q.toLowerCase()));
    rows.sort((a, b) => (a.updated_at < b.updated_at ? 1 : -1));
    return json(200, rows.map((note) => ({ id: note.id, title: note.title, updated_at: note.updated_at,
      chars: [...note.body].length, ...snippet(note.body, q) })));
  }
  if (parts[0] === 'preview' && method === 'POST') return json(200, { html: markdown(input.body || '') });
  if (parts.length === 0 && method === 'POST') {
    const at = new Date().toISOString();
    const id = `demo-${notes.size + 1}-${Date.now().toString(36)}`;
    const note = { id, title: noteTitle(input.body || ''), body: input.body || '', version: 1, updated_at: at, revisions: [{ version: 1, at, body: input.body || '' }] };
    notes.set(id, note);
    readOnly('/notes');
    lastSaveNotice = Date.now();
    return json(200, noteJson(note));
  }
  const note = notes.get(parts[0]);
  if (parts[0] === 'import' || !note) {
    if (method !== 'GET') { readOnly('/notes'); return json(403, { detail: READ_ONLY }); }
    return json(404, { detail: 'Note not found' });
  }
  if (parts.length === 1 && method === 'GET') return json(200, noteJson(note));
  if (parts.length === 1 && method === 'PUT') { saveNotice(); return json(200, noteJson(saveNote(note, input.body ?? ''))); }
  if (parts.length === 1 && method === 'DELETE') { notes.delete(note.id); saveNotice(); return json(200, {}); }
  if (parts[1] === 'revisions' && method === 'GET') {
    return json(200, (note.revisions || []).map(({ version, at }) => ({ version, at })));
  }
  if (parts[1] === 'restore' && method === 'POST') {
    const revision = (note.revisions || []).find((r) => r.version === input.version);
    if (!revision) return json(404, { detail: 'Revision not found' });
    saveNotice();
    return json(200, noteJson(saveNote(note, revision.body)));
  }
  readOnly('/notes');
  return json(403, { detail: READ_ONLY });
}

// Enough Markdown for the preview: headings, lists, task boxes, code, quotes,
// emphasis and links. The real server renders with pulldown-cmark.
function markdown(source) {
  const esc = (text) => text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  const inline = (text) => esc(text)
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[^*])\*([^*]+)\*/g, '$1<em>$2</em>')
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2">$1</a>');
  const out = [];
  let list = null;
  let para = [];
  const flush = () => {
    if (para.length) out.push(`<p>${inline(para.join(' '))}</p>`);
    if (list) out.push(`</${list}>`);
    para = []; list = null;
  };
  const lines = source.split('\n');
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (line.startsWith('```')) {
      flush();
      const code = [];
      for (i += 1; i < lines.length && !lines[i].startsWith('```'); i += 1) code.push(lines[i]);
      out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`);
      continue;
    }
    let m;
    if ((m = line.match(/^(#{1,6})\s+(.*)$/))) { flush(); out.push(`<h${m[1].length}>${inline(m[2])}</h${m[1].length}>`); continue; }
    if ((m = line.match(/^\s*(?:[-*]|(\d+)\.)\s+(.*)$/))) {
      const kind = m[1] ? 'ol' : 'ul';
      if (para.length || list !== kind) { flush(); out.push(`<${kind}>`); list = kind; }
      const task = m[2].match(/^\[([ xX])\]\s+(.*)$/);
      out.push(task ? `<li><input type="checkbox" disabled${task[1] === ' ' ? '' : ' checked'}> ${inline(task[2])}</li>` : `<li>${inline(m[2])}</li>`);
      continue;
    }
    if ((m = line.match(/^>\s?(.*)$/))) { flush(); out.push(`<blockquote>${inline(m[1])}</blockquote>`); continue; }
    if (!line.trim()) { flush(); continue; }
    if (list) flush();
    para.push(line.trim());
  }
  flush();
  return out.join('\n');
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
if (typeof module !== 'undefined') module.exports = { shiftTimes, parseTime, normalize, recordedAt, markdown, noteTitle, snippet, historyFilter };
