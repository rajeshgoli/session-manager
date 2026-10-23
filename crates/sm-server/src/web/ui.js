// Shared building blocks (spec 1710 D2): fetch, polling, time text, the
// context ring, chips, segmented controls, popovers, icons, and the small
// event bus pages use to reach the shell (navigate, open a panel, toast).
import { h } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import htm from 'htm';

export const html = htm.bind(h);

export const config = (() => {
  try {
    return JSON.parse(document.getElementById('sm-config').textContent);
  } catch (e) {
    return {};
  }
})();

// ---- bus --------------------------------------------------------------------

const listeners = {};
export const bus = {
  on(event, fn) {
    (listeners[event] = listeners[event] || new Set()).add(fn);
    return () => listeners[event].delete(fn);
  },
  emit(event, data) {
    (listeners[event] || []).forEach((fn) => fn(data));
  },
  request(event, data) {
    return [...(listeners[event] || [])].map((fn) => fn(data));
  },
};

/** Client-side navigation to a shell path (`/`, `/queue`, `/terminal/…`). */
export const navigate = (path) => bus.emit('navigate', path);
/** Open the side panel on `kind:id` (D1 `?open=`). */
export const openPanel = (ref) => bus.emit('open', ref);
export const closePanel = () => bus.emit('open', null);
/**
 * True when a keystroke lands in a text field. Reads the composed path because a
 * document listener sees a field inside a shadow root (the doc review sheet) retargeted to its host.
 */
export const typingIn = (event) => {
  const target = event.composedPath?.()[0] || event.target;
  return !!target?.closest?.('input,textarea,select,[contenteditable]') || !!target?.isContentEditable;
};
/** `action` adds a button, e.g. `{ label: 'Undo', run }`; `ms` how long it shows (6 s). */
export const toast = (text, onClick, { action, ms } = {}) => bus.emit('toast', { text, onClick, action, ms });
/** Open New agent, optionally filled in (Clone). */
export const newAgent = (prefill) => bus.emit('new-agent', prefill || {});

// Data the shell polls once for everyone (`queue`, `inbox`), by key.
export const shared = {};
export function setShared(key, value) {
  shared[key] = value;
  bus.emit(`shared:${key}`, value);
}
export function useShared(key) {
  const [value, setValue] = useState(shared[key]);
  useEffect(() => bus.on(`shared:${key}`, setValue), [key]);
  return value;
}

// Panel renderers by kind (`agent`, and later `job`, `doc`, `thread`, `ticket`).
export const panels = new Map();
export const registerPanel = (kind, component) => panels.set(kind, component);

/**
 * Open an item in the panel when a renderer for its kind exists, else its
 * page in a new tab.
 */
export function openItem(kind, id, href) {
  if (panels.has(kind)) openPanel(`${kind}:${id}`);
  else if (href) window.open(href, '_blank', 'noopener');
}

// ---- fetch ------------------------------------------------------------------

export class ApiError extends Error {
  constructor(message, status, body = null) {
    super(message);
    this.status = status;
    this.body = body;
  }
}

// The server stamps every response with its build. A tab opened before a
// deploy keeps running the code it loaded, so once a newer build answers,
// the page offers a reload and the next page switch loads it.
export const build = { stale: false };
function noteBuild(id) {
  if (id && config.build_id && id !== config.build_id && !build.stale) {
    build.stale = true;
    bus.emit('stale-build', true);
  }
}

/**
 * The latest parsed JSON answer per GET path (query dropped), at most `limit`
 * paths, oldest evicted (1859 B3). A bug report sends what the page fetched.
 */
export function pageDataRing(limit = 12) {
  const entries = new Map();
  let seq = 0;
  let pageMark = 0;
  const since = (mark) => Object.fromEntries([...entries].filter(([, e]) => e.seq > mark).map(([key, e]) => [key, e.value]));
  return {
    put(path, value) {
      const key = path.split('?')[0];
      entries.delete(key);
      entries.set(key, { value, seq: ++seq });
      while (entries.size > limit) entries.delete(entries.keys().next().value);
    },
    /** A mark; `since` returns the paths fetched after it. */
    mark: () => seq,
    since,
    /** The shell marks each page switch; `forPage` is what was fetched since. */
    markPage() { pageMark = seq; },
    forPage: () => since(pageMark),
  };
}
export const pageData = pageDataRing();

/** Drops the largest entries until the serialized whole fits `cap` characters. */
export function trimPageData(data, cap = 300000) {
  const out = { ...data };
  const size = (value) => JSON.stringify(value)?.length || 0;
  while (Object.keys(out).length && size(out) > cap) {
    const largest = Object.keys(out).reduce((a, b) => (size(out[b]) > size(out[a]) ? b : a));
    delete out[largest];
  }
  return out;
}

// ---- client headers (spec 2170 A.5) -----------------------------------------

// A request within this long of the owner's last tap, key or page switch is
// theirs; anything later, or a poll's repeat, is background.
export const FOREGROUND_MS = 2000;
const PANEL_SCREENS = { agent: 'session/detail', job: 'queue/job', thread: 'inbox/thread', doc: 'doc' };

/** The `x-sm-screen` name for a page and its open panel: `board`, `session/detail`, `doc`. */
export function screenFor(page, open, path = '') {
  const kind = open ? open.split(':', 1)[0] : null;
  if (kind) return PANEL_SCREENS[kind] || `${page}/${kind}`;
  if (page === 'settings' && path.endsWith('#auto-pm')) return 'settings/autopm';
  return page;
}

/** What a page load is: `link` for a deep path opened fresh, else `launch`. */
export function loadNav(type, path) {
  return type === 'navigate' && path !== '/' && path !== '/watch' ? 'link' : 'launch';
}

export function clientNavState(now = () => Date.now()) {
  const state = { screen: null, pending: 'launch', last: now(), polling: false };
  return {
    state,
    /** The owner tapped, typed or switched page. */
    acted() { state.last = now(); },
    setScreen(screen) { state.screen = screen; },
    /** Headers for the request starting now; the first owner request after a load carries how it began. */
    headers() {
      const out = {};
      const screen = typeof state.screen === 'function' ? state.screen() : state.screen;
      if (screen) out['x-sm-screen'] = screen;
      if (state.polling || now() - state.last > FOREGROUND_MS) {
        out['x-sm-bg'] = '1';
        return out;
      }
      out['x-sm-nav'] = state.pending || 'nav';
      state.pending = null;
      return out;
    },
  };
}
export const clientNav = clientNavState();
if (typeof window !== 'undefined') {
  try {
    const entry = performance.getEntriesByType('navigation')[0];
    clientNav.state.pending = loadNav(entry?.type, location.pathname);
  } catch (e) { /* older browser: a load is a launch */ }
  const acted = () => clientNav.acted();
  window.addEventListener('pointerdown', acted, true);
  window.addEventListener('keydown', acted, true);
  window.addEventListener('popstate', acted);
}

// ---- switches (spec 2170 B.1) -----------------------------------------------

// Every response names the switch version; a new one re-reads the switches,
// and an open tab re-reads them every 30 s anyway.
export const SWITCH_REFRESH_MS = 30000;
let switchVersion = null;
let switchRead = null;
function noteSwitches(version) {
  if (!version || version === switchVersion) return;
  switchVersion = version;
  readSwitches();
}
export function readSwitches() {
  if (switchRead) return switchRead;
  const was = clientNav.state.polling;
  clientNav.state.polling = true;
  const request = api('/client/settings');
  clientNav.state.polling = was;
  switchRead = request
    .then((settings) => setShared('switches', settings?.switches || {}))
    .catch(() => {})
    .finally(() => { switchRead = null; });
  return switchRead;
}
if (typeof document !== 'undefined') {
  setInterval(() => { if (!document.hidden) readSwitches(); }, SWITCH_REFRESH_MS);
}
/** Whether an auto PM switch is on; a missing key is off. */
export function useSwitch(key) {
  return useShared('switches')?.[key] === true;
}

export async function api(path, { method = 'GET', body, headers = {} } = {}) {
  const options = { method, credentials: 'same-origin', cache: 'no-store', headers: { ...clientNav.headers(), ...headers } };
  if (body !== undefined) {
    options.headers['Content-Type'] = 'application/json';
    options.body = JSON.stringify(body);
  }
  const response = await fetch(path, options);
  noteBuild(response.headers.get('x-sm-build'));
  noteSwitches(response.headers.get('x-sm-switches'));
  const text = await response.text();
  let value = null;
  try {
    value = text ? JSON.parse(text) : null;
  } catch (e) {
    value = null;
  }
  if (!response.ok) {
    const detail = value && (typeof value.detail === 'string' ? value.detail : value.error);
    throw new ApiError(detail || `HTTP ${response.status}`, response.status, value);
  }
  if (method === 'GET' && value !== null) pageData.put(path, value);
  return value;
}

/**
 * An owner write that carries the inbox token. The token comes with the page
 * and lasts 24 hours, so a tab left open longer re-reads it from a fresh
 * page shell and tries once more (sm#2261).
 */
export async function inboxWrite(path, body) {
  const send = () => api(path, { method: 'POST', headers: config.inbox_token ? { 'X-SM-Doc-Token': config.inbox_token } : {}, body });
  const used = config.inbox_token;
  try {
    return await send();
  } catch (e) {
    if (e.status !== 401 && e.status !== 403) throw e;
    let fresh = null;
    try {
      const page = await (await fetch('/', { credentials: 'same-origin', cache: 'no-store' })).text();
      const shell = new DOMParser().parseFromString(page, 'text/html');
      fresh = JSON.parse(shell.getElementById('sm-config')?.textContent || '{}').inbox_token;
    } catch (ignored) { /* keep the original error */ }
    if (!fresh || fresh === used) throw e;
    config.inbox_token = fresh;
    return send();
  }
}

// ---- network state and polling ---------------------------------------------

// Polls failing right now; the page is offline while any is.
const failing = new Set();
let nextPoll = 0;
export const network = {
  get offline() {
    return failing.size > 0;
  },
  report(poll, ok) {
    const before = failing.size > 0;
    if (ok) failing.delete(poll);
    else failing.add(poll);
    if (before !== failing.size > 0) bus.emit('network', failing.size > 0);
  },
};

/**
 * Call `load` now and every `ms` while the browser tab is visible (D2), and
 * keep the last good value on a failure. Returns `[value, error, reload]`.
 * `background` marks even the first load as not the owner's (spec 2170 A.5):
 * the shell's own counters, not a page opening.
 */
export function usePoll(load, ms, deps = [], { background = false } = {}) {
  const [state, setState] = useState({ value: undefined, error: null });
  const saved = useRef(load);
  saved.current = load;
  const tick = useRef(() => {});
  useEffect(() => {
    let timer = null;
    let alive = true;
    let running = false;
    let first = true;
    const poll = ++nextPoll;
    const run = async () => {
      clearTimeout(timer);
      // The first load runs even in a hidden tab; later polls wait for it to show.
      if (document.hidden && !first) return;
      const wasFirst = first;
      first = false;
      if (!running) {
        running = true;
        try {
          // A poll's repeats are background; its first load is the page opening.
          const was = clientNav.state.polling;
          clientNav.state.polling = background || !wasFirst;
          const pending = saved.current();
          clientNav.state.polling = was;
          const value = await pending;
          if (alive) setState({ value, error: null });
          if (alive) network.report(poll, true);
        } catch (error) {
          if (alive) setState((prev) => ({ value: prev.value, error }));
          // A 4xx answer means the server is reachable.
          if (alive) network.report(poll, error instanceof ApiError && error.status < 500);
        } finally {
          running = false;
        }
      }
      if (alive && ms) timer = setTimeout(run, ms);
    };
    const visible = () => {
      if (!document.hidden) run();
      else clearTimeout(timer);
    };
    tick.current = run;
    document.addEventListener('visibilitychange', visible);
    run();
    return () => {
      alive = false;
      network.report(poll, true);
      clearTimeout(timer);
      document.removeEventListener('visibilitychange', visible);
    };
  }, deps);
  return [state.value, state.error, () => tick.current()];
}

/** Re-render every `ms` so relative times stay current. */
export function useNow(ms = 30000) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(timer);
  }, [ms]);
  return now;
}

// ---- local storage ----------------------------------------------------------

export function stored(key, fallback) {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : JSON.parse(raw);
  } catch (e) {
    return fallback;
  }
}

export function store(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch (e) {
    /* private window: the choice lasts until reload */
  }
}

// ---- text -------------------------------------------------------------------

const ms = (iso) => (iso ? Date.parse(iso) : NaN);

/** Spec 1710 D7 ages: under an hour "{m}m", else "{h}h {m}m". */
export function duration(seconds) {
  const minutes = Math.max(0, Math.floor(seconds / 60));
  if (minutes < 60) return `${minutes}m`;
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

export function age(iso, now = Date.now()) {
  const at = ms(iso);
  return Number.isFinite(at) ? duration((now - at) / 1000) : '';
}

/** A job limit: "3h", "8h", "90m". */
export function limitText(seconds) {
  if (!seconds) return '';
  if (seconds % 3600 === 0) return `${seconds / 3600}h`;
  return duration(seconds);
}

/** "3:05 pm" today, else "29 Sep 3:05 pm". */
export function clock(iso) {
  const at = ms(iso);
  if (!Number.isFinite(at)) return '';
  const date = new Date(at);
  const time = date.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }).toLowerCase();
  if (new Date().toDateString() === date.toDateString()) return time;
  return `${date.toLocaleDateString([], { day: 'numeric', month: 'short' })} ${time}`;
}

export const ordinal = (n) => {
  const tail = n % 100 >= 11 && n % 100 <= 13 ? 'th' : ['th', 'st', 'nd', 'rd'][n % 10] || 'th';
  return `${n}${tail}`;
};

export const basename = (path) => (path || '').replace(/\/+$/, '').split('/').pop() || path || '';

/** `/Users/me/projects/x` → `~/projects/x`. */
export const homeRelative = (path) => (path || '').replace(/^\/Users\/[^/]+/, '~');

export function gigabytes(bytes) {
  return Math.round((bytes || 0) / 2 ** 30);
}

/** Usage heat shared by the shell and the Queue tile. */
export function meterBand(fraction, kind) {
  const value = Number.isFinite(fraction) ? fraction : 0;
  if (value > 0.85) return 'red';
  if (value >= 0.60) return 'amber';
  return kind === 'gpu' ? 'cyan' : 'green';
}

export function Toggle({ checked, onChange, label, disabled = false }) {
  return html`<button type="button" class="toggle" role="switch" aria-label=${label}
    aria-checked=${!!checked} disabled=${disabled}
    onClick=${() => onChange(!checked)}>${label ? html`<span class="sr-only">${label}</span>` : null}</button>`;
}

// A bare http(s) URL, without trailing sentence punctuation or a closing bracket.
const URL_RE = /https?:\/\/[^\s<>"'`]*[^\s<>"'`.,;:!?)\]}]/g;

/** Plain text with each bare URL as a link that opens in a new tab. */
export function LinkedText({ text }) {
  const source = String(text || '');
  const out = [];
  let last = 0;
  for (const match of source.matchAll(URL_RE)) {
    out.push(source.slice(last, match.index), html`<a href=${match[0]} target="_blank" rel="noopener noreferrer">${match[0]}</a>`);
    last = match.index + match[0].length;
  }
  out.push(source.slice(last));
  return out;
}

/** Sanitized HTML with bare URLs in its text turned into links; every link opens in a new tab. */
export function linkifyHtml(source) {
  const doc = new DOMParser().parseFromString(source, 'text/html');
  const walker = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT);
  const texts = [];
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (!node.parentElement.closest('a') && node.data.match(URL_RE)) texts.push(node);
  }
  for (const node of texts) {
    const parts = doc.createDocumentFragment();
    let last = 0;
    for (const match of node.data.matchAll(URL_RE)) {
      parts.append(node.data.slice(last, match.index));
      const link = doc.createElement('a');
      link.href = match[0];
      link.textContent = match[0];
      parts.append(link);
      last = match.index + match[0].length;
    }
    parts.append(node.data.slice(last));
    node.replaceWith(parts);
  }
  for (const link of doc.body.querySelectorAll('a[href]')) { link.target = '_blank'; link.rel = 'noopener noreferrer'; }
  return doc.body.innerHTML;
}

export const threadHref = thread => `/inbox?open=thread:${encodeURIComponent(thread.key.replace(/^agent:/, ''))}${thread.at ? `&at=${encodeURIComponent(thread.at)}` : ''}`;

/** Open external handoff links in the browser and local docs in the reader. */
export function openDoc(path) {
  try {
    const url = new URL(path, location.origin);
    if (url.origin === location.origin) openPanel(`doc:${url.pathname}${url.search}${url.hash}`);
    else if (url.protocol === 'https:' || url.protocol === 'http:') window.open(url.href, '_blank', 'noopener');
  } catch (_) {}
}

/** Shared related-item chips. Empty kinds do not occupy space. */
export function Links({ ticket, tickets = [], prs = [], agent, jobs = [], thread, docs = [] }) {
  const ticketRef = item => `ticket:${item.repo || ticket?.repo}#${item.number}`;
  const pending = review => !!review?.waiting_since;
  const reviewText = review => review ? ` · ${review.by === 'you' ? 'your review' : 'Codex review'}, round ${review.round}${review.verdict ? ` · ${review.verdict.replace('_', ' ')}` : ''}${pending(review) ? ` · waiting ${age(review.waiting_since)}` : ''}` : '';
  const visibleJobs = jobs.filter(job => ['running', 'pending', 'waiting', 'queued'].includes(job.state));
  if (!ticket && !tickets.length && !prs.length && !agent && !visibleJobs.length && !thread && !docs.length) return null;
  return html`<div class="links">
    ${[ticket, ...tickets].filter(Boolean).map(item => html`<button class="link-chip" onClick=${() => openPanel(ticketRef(item))}>#${item.number} ↗</button>`)}
    ${prs.map(pr => html`<button class=${`link-chip ${pending(pr.review) ? pr.review.by === 'you' ? 'magenta' : 'amber' : ''}`}
      onClick=${() => openPanel(ticketRef(pr))}>PR #${pr.number} · ${(pr.state || 'open').toLowerCase()}${reviewText(pr.review)}</button>`)}
    ${agent ? html`<span class="link-pair"><button class="link-chip" onClick=${() => openPanel(`agent:${agent.id}`)}>${agent.name} · ${providerLabel(agent.provider)} · ${agent.fact || agent.state || ''}</button><button class="link-chip" aria-label=${`Terminal for ${agent.name}`} onClick=${() => navigate(`/terminal/${encodeURIComponent(agent.id)}`)}>⌨</button></span>` : null}
    ${visibleJobs.slice(0, 3).map(job => html`<button class=${`link-chip ${job.quiet_since ? 'red' : job.state === 'running' ? 'green' : 'amber'}`}
      onClick=${() => { navigate('/queue'); openPanel(`job:${job.id}`); }}>${job.label || job.id} · ${job.quiet_since ? 'quiet' : job.state === 'running' ? 'running' : 'waiting'} ${age(job.quiet_since || job.since || job.started_at || job.queued_at)}</button>`)}
    ${visibleJobs.length > 3 ? html`<span class="link-chip">+${visibleJobs.length - 3} jobs</span>` : null}
    ${thread ? html`<button class=${`link-chip ${thread.needs_you ? 'magenta' : ''}`} onClick=${() => { location.href = threadHref(thread); }}>Inbox · ${thread.needs_you ? 'question' : thread.count}</button>` : null}
    ${docs.map(doc => html`<button class="link-chip" onClick=${() => openDoc(doc.reader_path)}>${doc.title}</button>`)}
  </div>`;
}

export function providerLabel(provider) {
  return provider === 'opencode' ? 'Local' : provider && provider.startsWith('codex') ? 'Codex' : 'Claude';
}

// ---- components -------------------------------------------------------------

/** The phone's context ring: green at empty, amber at half, red when full. */
export function ringColor(percent) {
  const p = Math.max(0, Math.min(100, percent));
  return p <= 50
    ? `color-mix(in srgb, var(--amber) ${p * 2}%, var(--green))`
    : `color-mix(in srgb, var(--red) ${(p - 50) * 2}%, var(--amber))`;
}

export function Ring({ percent, suffix = '' }) {
  if (typeof percent !== 'number') return html`<span class="ring none">–</span>`;
  const p = Math.round(percent);
  return html`<span class="ring" style=${`--p:${p};--rc:${ringColor(p)}`} title=${`Context ${p}%`}
    >${p}${suffix}</span
  >`;
}

export function Seg({ options, value, onChange, label }) {
  return html`<span class="seg" role="radiogroup" aria-label=${label}>
    ${options.map(
      (option) => html`<button
        type="button"
        role="radio"
        aria-checked=${option.value === value}
        class=${option.value === value ? 'on' : ''}
        onClick=${() => onChange(option.value)}
      >
        ${option.label}
      </button>`,
    )}
  </span>`;
}

/**
 * A floating box under its anchor; closes on Esc or a click outside.
 * `align` is `left` or `right` against the anchor.
 */
export function Popover({ onClose, children, align = 'left', className = '' }) {
  const box = useRef(null);
  useEffect(() => {
    const key = (event) => {
      if (event.key === 'Escape') {
        event.stopPropagation();
        onClose();
      }
    };
    const click = (event) => {
      if (box.current && !box.current.contains(event.target) && !event.target.closest('[data-pop-anchor]')) onClose();
    };
    document.addEventListener('keydown', key, true);
    document.addEventListener('mousedown', click, true);
    return () => {
      document.removeEventListener('keydown', key, true);
      document.removeEventListener('mousedown', click, true);
    };
  }, [onClose]);
  useEffect(() => {
    // Keep the box on screen.
    const el = box.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    if (rect.right > window.innerWidth - 8) el.style.transform = `translateX(${window.innerWidth - 8 - rect.right}px)`;
    if (rect.left < 8) el.style.transform = `translateX(${8 - rect.left}px)`;
    // A box opened near the bottom of a scrolled page scrolls into view.
    if (rect.bottom > window.innerHeight) el.scrollIntoView({ block: 'nearest' });
  }, []);
  const style = align === 'right' ? 'right:0;top:calc(100% + 6px)' : 'left:0;top:calc(100% + 6px)';
  return html`<div class=${`pop ${className}`} ref=${box} style=${style} role="dialog">${children}</div>`;
}

const ICONS = {
  moon: '<path d="M13.2 10.1A5.6 5.6 0 0 1 5.9 2.8a5.6 5.6 0 1 0 7.3 7.3z"/>',
  moonFill: '<path d="M13.2 10.1A5.6 5.6 0 0 1 5.9 2.8a5.6 5.6 0 1 0 7.3 7.3z" fill="currentColor"/>',
  agents:
    '<circle cx="8" cy="5.5" r="2.6"/><path d="M2.8 14c.6-2.8 2.7-4.3 5.2-4.3s4.6 1.5 5.2 4.3"/>',
  board: '<rect x="2" y="2.5" width="12" height="11" rx="1.5"/><path d="M2 6.5h12M6 6.5v7"/>',
  queue: '<path d="M2.5 4h11M2.5 8h11M2.5 12h7"/>',
  inbox: '<path d="M2 9.5l1.8-6h8.4L14 9.5v3.5H2z"/><path d="M2 9.5h3.5l1 1.6h3l1-1.6H14"/>',
  analytics: '<path d="M3 13.5V8M8 13.5V3M13 13.5V6"/>',
  history: '<circle cx="8" cy="8" r="5.8"/><path d="M8 4.8V8l2.3 1.6"/>',
  settings:
    '<circle cx="8" cy="8" r="2.2"/><path d="M8 1.8v2M8 12.2v2M1.8 8h2M12.2 8h2M3.6 3.6 5 5M11 11l1.4 1.4M3.6 12.4 5 11M11 5l1.4-1.4"/>',
  fold: '<path d="M9.5 4 5.5 8l4 4"/>',
  unfold: '<path d="M6.5 4l4 4-4 4"/>',
  close: '<path d="M4 4l8 8M12 4l-8 8"/>',
  'eye-off': '<path d="M2 8s2.2-4.2 6-4.2c1.3 0 2.4.5 3.3 1.1M14 8s-2.2 4.2-6 4.2c-1.3 0-2.4-.5-3.3-1.1"/><path d="M6.6 9.4a2 2 0 0 1 2.8-2.8M2.5 13.5l11-11"/>',
  edit: '<path d="M10.2 3.3l2.5 2.5-6.9 6.9-3.1.6.6-3.1z"/><path d="M9 4.5l2.5 2.5"/>',
  wide: '<path d="M9.5 2.5h4v4M6.5 13.5h-4v-4M13.5 2.5 9 7M2.5 13.5 7 9"/>',
  narrow: '<path d="M13.5 6.5h-4v-4M2.5 9.5h4v4M9.5 6.5l4-4M6.5 9.5l-4 4"/>',
  back: '<path d="M13 8H3M7 4 3 8l4 4"/>',
  redraw: '<path d="M13 8a5 5 0 1 1-1.5-3.6M13 2.5v3h-3"/>',
  external: '<path d="M9 2.5h4.5V7M13.5 2.5 7.5 8.5M12 9.5v4H2.5V4h4"/>',
  terminal: '<rect x="1.8" y="2.8" width="12.4" height="10.4" rx="1.5"/><path d="M4.5 6l2 2-2 2M8 10.5h3.5"/>',
  more: '<circle cx="3.5" cy="8" r=".6"/><circle cx="8" cy="8" r=".6"/><circle cx="12.5" cy="8" r=".6"/>',
  freeze: '<path d="M8 1.8v12.4M2.6 4.9l10.8 6.2M2.6 11.1l10.8-6.2M6.3 2.8 8 4.4l1.7-1.6M6.3 13.2 8 11.6l1.7 1.6M2.4 7 4.6 7.6 4 9.8M13.6 9 11.4 8.4l.6-2.2"/>',
  thaw: '<circle cx="8" cy="8" r="2.8"/><path d="M8 1.6v1.6M8 12.8v1.6M1.6 8h1.6M12.8 8h1.6M3.5 3.5l1.1 1.1M11.4 11.4l1.1 1.1M3.5 12.5l1.1-1.1M11.4 4.6l1.1-1.1"/>',
  move: '<path d="M2.5 8h9M8.5 4.5 12 8l-3.5 3.5M13.8 3v10"/>',
};

export function Icon({ name, size = 16 }) {
  return html`<svg
    viewBox="0 0 16 16"
    width=${size}
    height=${size}
    fill="none"
    stroke="currentColor"
    stroke-width="1.4"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
    dangerouslySetInnerHTML=${{ __html: ICONS[name] || '' }}
  ></svg>`;
}

/** A button that asks inline before it acts. */
export function ConfirmButton({ label, prompt, confirmLabel, onConfirm, className = 'btn sm' }) {
  const [asking, setAsking] = useState(false);
  if (!asking) return html`<button type="button" class=${className} onClick=${() => setAsking(true)}>${label}</button>`;
  return html`<span class="confirm"
    >${prompt}
    <button type="button" class="btn sm danger" onClick=${() => (setAsking(false), onConfirm())}>${confirmLabel}</button>
    <button type="button" class="btn sm" onClick=${() => setAsking(false)}>Cancel</button></span
  >`;
}

/** A fresh id for exactly-once sends. */
export function submissionId() {
  const bytes = new Uint8Array(12);
  crypto.getRandomValues(bytes);
  return `web-${Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')}`;
}
