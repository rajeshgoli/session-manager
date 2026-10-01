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
export const toast = (text, onClick) => bus.emit('toast', { text, onClick });
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
  constructor(message, status) {
    super(message);
    this.status = status;
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

export async function api(path, { method = 'GET', body, headers = {} } = {}) {
  const options = { method, credentials: 'same-origin', cache: 'no-store', headers: { ...headers } };
  if (body !== undefined) {
    options.headers['Content-Type'] = 'application/json';
    options.body = JSON.stringify(body);
  }
  const response = await fetch(path, options);
  noteBuild(response.headers.get('x-sm-build'));
  const text = await response.text();
  let value = null;
  try {
    value = text ? JSON.parse(text) : null;
  } catch (e) {
    value = null;
  }
  if (!response.ok) {
    const detail = value && (typeof value.detail === 'string' ? value.detail : value.error);
    throw new ApiError(detail || `HTTP ${response.status}`, response.status);
  }
  return value;
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
 */
export function usePoll(load, ms, deps = []) {
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
      first = false;
      if (!running) {
        running = true;
        try {
          const value = await saved.current();
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

export const threadHref = thread => `/inbox?open=thread:${encodeURIComponent(thread.key.replace(/^agent:/, ''))}${thread.at ? `&at=${encodeURIComponent(thread.at)}` : ''}`;

/** Shared related-item chips. Empty kinds do not occupy space. */
export function Links({ ticket, prs = [], agent, jobs = [], thread, docs = [] }) {
  const ticketRef = item => `ticket:${item.repo || ticket?.repo}#${item.number}`;
  const pending = review => !!review?.waiting_since;
  const reviewText = review => review ? ` · ${review.by === 'you' ? 'your review' : 'Codex review'}, round ${review.round}${review.verdict ? ` · ${review.verdict.replace('_', ' ')}` : ''}${pending(review) ? ` · waiting ${age(review.waiting_since)}` : ''}` : '';
  const visibleJobs = jobs.filter(job => ['running', 'pending', 'waiting', 'queued'].includes(job.state));
  if (!ticket && !prs.length && !agent && !visibleJobs.length && !thread && !docs.length) return null;
  return html`<div class="links">
    ${ticket ? html`<button class="link-chip" onClick=${() => openPanel(ticketRef(ticket))}>#${ticket.number} ↗</button>` : null}
    ${prs.map(pr => html`<button class=${`link-chip ${pending(pr.review) ? pr.review.by === 'you' ? 'magenta' : 'amber' : ''}`}
      onClick=${() => openPanel(ticketRef(pr))}>PR #${pr.number} · ${(pr.state || 'open').toLowerCase()}${reviewText(pr.review)}</button>`)}
    ${agent ? html`<span class="link-pair"><button class="link-chip" onClick=${() => openPanel(`agent:${agent.id}`)}>${agent.name} · ${providerLabel(agent.provider)} · ${agent.fact || agent.state || ''}</button><button class="link-chip" aria-label=${`Terminal for ${agent.name}`} onClick=${() => navigate(`/terminal/${encodeURIComponent(agent.id)}`)}>⌨</button></span>` : null}
    ${visibleJobs.slice(0, 3).map(job => html`<button class=${`link-chip ${job.quiet_since ? 'red' : job.state === 'running' ? 'green' : 'amber'}`}
      onClick=${() => { navigate('/queue'); openPanel(`job:${job.id}`); }}>${job.label || job.id} · ${job.quiet_since ? 'quiet' : job.state === 'running' ? 'running' : 'waiting'} ${age(job.quiet_since || job.since || job.started_at || job.queued_at)}</button>`)}
    ${visibleJobs.length > 3 ? html`<span class="link-chip">+${visibleJobs.length - 3} jobs</span>` : null}
    ${thread ? html`<button class=${`link-chip ${thread.needs_you ? 'magenta' : ''}`} onClick=${() => { location.href = threadHref(thread); }}>Inbox · ${thread.needs_you ? 'question' : thread.count}</button>` : null}
    ${docs.map(doc => html`<button class="link-chip" onClick=${() => openPanel(`doc:${doc.reader_path}`)}>${doc.title}</button>`)}
  </div>`;
}

export function providerLabel(provider) {
  return provider && provider.startsWith('codex') ? 'Codex' : 'Claude';
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
  }, []);
  const style = align === 'right' ? 'right:0;top:calc(100% + 6px)' : 'left:0;top:calc(100% + 6px)';
  return html`<div class=${`pop ${className}`} ref=${box} style=${style} role="dialog">${children}</div>`;
}

const ICONS = {
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
  wide: '<path d="M9.5 2.5h4v4M6.5 13.5h-4v-4M13.5 2.5 9 7M2.5 13.5 7 9"/>',
  narrow: '<path d="M13.5 6.5h-4v-4M2.5 9.5h4v4M9.5 6.5l4-4M6.5 9.5l-4 4"/>',
  back: '<path d="M13 8H3M7 4 3 8l4 4"/>',
  external: '<path d="M9 2.5h4.5V7M13.5 2.5 7.5 8.5M12 9.5v4H2.5V4h4"/>',
  terminal: '<rect x="1.8" y="2.8" width="12.4" height="10.4" rx="1.5"/><path d="M4.5 6l2 2-2 2M8 10.5h3.5"/>',
  more: '<circle cx="3.5" cy="8" r=".6"/><circle cx="8" cy="8" r=".6"/><circle cx="12.5" cy="8" r=".6"/>',
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
