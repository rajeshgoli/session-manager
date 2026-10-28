// The sm web shell (spec 1710 D1, D2; 2543 B-D, G): router, sidebar, top bar,
// side panel, command palette, badges and toasts. Every page, the terminal
// included, renders inside one frame: sidebar | main | side panel. Pages are
// modules; a page whose module has not landed yet shows a placeholder.
import { render } from 'preact';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import {
  html, api, bus, build, network, clientNav, screenFor, pageData, usePoll, useShared, setShared, shared, stored, store, panels, config,
  openPanel, closePanel, navigate, openItem, openReader, toast, typingIn, Icon, basename,
} from './ui.js';
import { BoardPage } from './board.js';
import { InboxPage } from './inbox.js';
import { HistoryPage } from './history.js';
import { GuestbookPage } from './guestbook.js';
import { AgentCard, HostRestartBanner, SECTION_TONE } from './agents.js';
import { QueuePage } from './queue.js';
import { Dash } from './meters.js';
import { ArtifactsPage } from './artifacts.js';
import { AnalyticsPage } from './analytics.js';
import { SettingsPage } from './settings.js';
import { TerminalPage, Switcher, switcherGroups, shortFact, stepRow, landingFor, activeTerminal, retiredIds, IDLE_KEY } from './terminal.js';
import { NotesView } from './notes.js';
import { Reader, docPathFrom } from './reader.js';
import { NewAgentPopover } from './start.js';
import { BugButton } from './bug.js';
import { HiddenShells, hiddenShells, isShell } from './shell.js';

// ---- pages ------------------------------------------------------------------

// The sidebar's page links (2543 B.3): the Agents page is gone; `/` lands on a terminal (D.5).
const PAGES = [
  { key: 'inbox', label: 'Inbox', icon: 'inbox', path: '/inbox', key_hint: 'i' },
  { key: 'board', label: 'Board', icon: 'board', path: '/board', key_hint: 'b' },
  { key: 'queue', label: 'Queue', icon: 'queue', path: '/queue', key_hint: 'q' },
  { key: 'artifacts', label: 'Artifacts', icon: 'history', path: '/artifacts' },
  { key: 'notes', label: 'Notes', icon: 'history', path: '/notes', key_hint: 'n' },
  { key: 'analytics', label: 'Analytics', icon: 'analytics', path: '/analytics', minor: true },
  { key: 'history', label: 'History', icon: 'history', path: '/history', minor: true },
  { key: 'guestbook', label: 'Guestbook', icon: 'history', path: '/guestbook', minor: true },
  { key: 'settings', label: 'Settings', icon: 'settings', path: '/settings', key_hint: 's', bottom: true },
];

// The phone's bottom tab bar keeps today's items (B.10), the agents list first.
const HOME = { key: 'home', label: 'Agents', icon: 'agents', path: '/' };
const TABS = [HOME, ...['board', 'inbox', 'queue', 'artifacts', 'notes', 'analytics', 'history', 'guestbook', 'settings']
  .map((key) => PAGES.find((p) => p.key === key))];

function pageFor(path) {
  if (path.startsWith('/terminal/')) return 'terminal';
  if (path.startsWith('/read/')) return 'doc';
  const page = PAGES.find((p) => path === p.path || path.startsWith(`${p.path}/`));
  return page ? page.key : 'home';
}

function readLocation() {
  const params = new URLSearchParams(location.search);
  const open = params.get('open');
  // Documents no longer open in the side panel: an older `?open=doc:<path>` link lands on the document view (2543 F.4).
  if (open?.startsWith('doc:/')) {
    const path = open.slice('doc:'.length);
    history.replaceState(history.state, '', `/read${path}${path.includes('#') ? '' : location.hash}`);
    return { path: location.pathname, open: null };
  }
  // `/history` opens on agents now; an older link that filters tickets
  // (`repo`, `agent` or a ticket `open`) keeps landing on the tickets tab.
  if (location.pathname === '/history' && (params.has('repo') || params.has('agent') || (open && !open.includes(':')))) {
    history.replaceState(history.state, '', `/history/tickets${location.search}${location.hash}`);
  }
  return { path: location.pathname, open: params.get('panel') || (open?.includes(':') ? open : null) };
}

function urlFor(path, open) {
  const params = new URLSearchParams(path === location.pathname ? location.search : '');
  // History's tickets tab reserves `open` for its ticket filter. Keep panel
  // state separate there, while still accepting `?open=ticket:…` on arrival.
  const key = path === '/history/tickets' ? 'panel' : 'open';
  params.delete('panel');
  if (key === 'open' || params.get('open')?.includes(':')) params.delete('open');
  if (!open?.startsWith('thread:')) params.delete('at');
  if (open) params.set(key, open);
  // Keep panel links readable: `?open=agent:65203ac8`.
  const query = params.toString().replace(/%3A/gi, ':');
  return `${path}${query ? `?${query}` : ''}`;
}

// Where a details ref shows (1710 D2): a job, or an agent on History, opens as a band under the row
// it came from; everything else, and anything on a terminal, opens in the side panel.
const inBand = (ref, page) => page !== 'terminal' && (/^job:/.test(ref || '') || (/^agent:/.test(ref || '') && page === 'history'));
let openOrigin = null;

// ---- layout (D2 folding and sizing) ------------------------------------------

const REM = () => parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;

function initialLayout() {
  const saved = stored('sm-layout', {}) || {};
  return {
    panel_width_rem: typeof saved.panel_width_rem === 'number' ? saved.panel_width_rem : 28,
    panel_mode: saved.panel_mode === 'wide' ? 'wide' : 'side',
  };
}

const clampWidth = (rem) => Math.max(20, Math.min(rem, (window.innerWidth * 0.6) / REM()));
const refreshMs = () => Math.max(1, config.refresh_seconds || 3) * 1000;

// ---- sidebar state (2543 B.4, B.8) ---------------------------------------------

/** Fold is remembered per view kind; a document uses the pages arrangement when its sidebar is open (B.4). */
const viewKind = (page) => (page === 'terminal' || page === 'doc' ? page : 'pages');
const arrangeKind = (kind) => (kind === 'doc' ? 'pages' : kind);
const SIDEBAR_KEY = 'sm-sidebar';
const ARRANGE_KEY = 'sm-sidebar-arrangement';

/** `{ pages, terminal, doc }`, each open or folded; the first run carries over the old rail and switcher choices. */
function initialSidebar() {
  const saved = stored(SIDEBAR_KEY, null);
  const pick = (value, fallback) => (value === 'open' || value === 'folded' ? value : fallback);
  const rail = (stored('sm-layout', {}) || {}).rail;
  const switcher = stored('sm-term-switcher', null);
  const next = saved && typeof saved === 'object' ? {
    pages: pick(saved.pages, window.innerWidth >= 1100 ? 'open' : 'folded'),
    terminal: pick(saved.terminal, window.innerWidth >= 900 ? 'open' : 'folded'),
    doc: pick(saved.doc, 'folded'),
  } : {
    pages: rail === 'full' ? 'open' : rail === 'folded' ? 'folded' : window.innerWidth >= 1100 ? 'open' : 'folded',
    terminal: switcher === true ? 'open' : switcher === false ? 'folded' : window.innerWidth >= 900 ? 'open' : 'folded',
    doc: 'folded',
  };
  if (!saved) store(SIDEBAR_KEY, next);
  return next;
}

/** Pages first on pages, agents first on a terminal, unless swapped. */
function initialArrangement() {
  const saved = stored(ARRANGE_KEY, {}) || {};
  return { pages: saved.pages === 'agents' ? 'agents' : 'pages', terminal: saved.terminal === 'pages' ? 'pages' : 'agents' };
}

const remembered = (key, setter) => (patch) => setter((prev) => {
  const next = { ...prev, ...patch };
  store(key, next);
  return next;
});

// ---- shell ------------------------------------------------------------------

function App() {
  const [loc, setLoc] = useState(readLocation);
  const [layout, setLayout] = useState(initialLayout);
  const [sidebar, setSidebar] = useState(initialSidebar);
  const [arrangement, setArrangement] = useState(initialArrangement);
  const [palette, setPalette] = useState(false);
  const [creating, setCreating] = useState(null);
  const [toasts, setToasts] = useState([]);
  const [offline, setOffline] = useState(network.offline);
  const [stale, setStale] = useState(build.stale);

  const updateLayout = useCallback((patch) => {
    setLayout((prev) => {
      const next = { ...prev, ...patch };
      store('sm-layout', next);
      return next;
    });
  }, []);
  // Stable, so the sheet's Escape and outside-click listeners stay attached across renders.
  const closeCreating = useCallback(() => setCreating(null), []);
  const updateSidebar = useCallback(remembered(SIDEBAR_KEY, setSidebar), []);
  const updateArrangement = useCallback(remembered(ARRANGE_KEY, setArrangement), []);
  // The sidebar, the terminal and the landing share one poll of the live sessions.
  const watch = usePoll(() => api('/watch/state'), refreshMs(), [], { background: true });

  useEffect(() => {
    let committedIndex = Number.isInteger(history.state?.smHistoryIndex) ? history.state.smHistoryIndex : 0;
    history.replaceState({ ...history.state, smHistoryIndex: committedIndex }, '', location.href);
    let reversing = false;
    const canLeaveNotes = async () => (await Promise.all(bus.request('notes-before-leave'))).every(Boolean);
    const pop = async () => {
      if (reversing) { reversing = false; return; }
      const targetIndex = Number.isInteger(history.state?.smHistoryIndex)
        ? history.state.smHistoryIndex : committedIndex - 1;
      const step = Math.sign(targetIndex - committedIndex) || -1;
      if (document.querySelector('.notes-view') && !(await canLeaveNotes())) {
        reversing = true;
        history.go(-step);
        return;
      }
      committedIndex = targetIndex;
      openOrigin = null; setLoc(readLocation());
    };
    window.addEventListener('popstate', pop);
    const offs = [
      bus.on('navigate', async (path) => {
        clientNav.acted();
        if (!(await canLeaveNotes())) return;
        openOrigin = null;
        if (build.stale) {
          location.href = urlFor(path, null);
          return;
        }
        history.pushState({ smHistoryIndex: ++committedIndex }, '', urlFor(path, null));
        setLoc(readLocation());
      }),
      bus.on('open', async (ref) => {
        clientNav.acted();
        const current = readLocation();
        if (current.open === ref) return;
        if (!(await canLeaveNotes())) return;
        openOrigin = document.activeElement?.closest?.('.board-ticket,.history-card,.q-job,.card') || null;
        const kind = ref?.split(':', 1)[0];
        // A job opened from outside Queue goes to Queue; anything else opens where you are.
        const page = pageFor(current.path);
        const path = kind === 'job' && !openOrigin && page !== 'terminal' && page !== 'queue' ? '/queue' : current.path;
        history.pushState({ smHistoryIndex: ++committedIndex }, '', urlFor(path, ref));
        setLoc(readLocation());
      }),
      bus.on('toast', (item) => {
        const id = Math.random();
        setToasts((prev) => [...prev, { ...item, id }]);
        setTimeout(() => setToasts((prev) => prev.filter((t) => t.id !== id)), item.ms || 6000);
      }),
      bus.on('new-agent', (prefill) => setCreating({ prefill: prefill || {}, at: 'bar' })),
      bus.on('network', setOffline),
      bus.on('stale-build', setStale),
    ];
    return () => {
      window.removeEventListener('popstate', pop);
      offs.forEach((off) => off());
    };
  }, []);

  const page = pageFor(loc.path);
  const termId = page === 'terminal' ? decodeURIComponent(loc.path.slice('/terminal/'.length)) : null;
  // Keep the same keyed component across page navigation, with one expiry per departure.
  const [kept, setKept] = useState(null);
  if (termId && (kept?.id !== termId || kept.leftAt !== null)) {
    setKept({ id: termId, leftAt: null });
  } else if (!termId && kept && kept.leftAt === null) {
    setKept({ ...kept, leftAt: Date.now() });
  }
  const mountedId = termId || kept?.id;
  const releaseKept = useCallback((id) => {
    setKept((current) => current?.id === id && current.leftAt !== null ? null : current);
  }, []);
  useEffect(() => {
    if (!kept || kept.leftAt === null) return;
    const timer = setTimeout(() => releaseKept(kept.id), Math.max(0, kept.leftAt + 15 * 60 * 1000 - Date.now()));
    return () => clearTimeout(timer);
  }, [kept, releaseKept]);
  const kind = viewKind(page);
  const folded = sidebar[kind] !== 'open';
  const [watchDoc] = watch;
  const sessions = (watchDoc && watchDoc.sessions) || [];
  const { order } = switcherGroups(sessions, termId, stored(IDLE_KEY, false));
  const toggleSidebar = () => updateSidebar({ [kind]: folded ? 'open' : 'folded' });
  // Every owner request names the screen it came from (spec 2170 A.5).
  clientNav.setScreen(() => screenFor(page, loc.open, `${location.pathname}${location.hash}`));
  // Page data for a bug report is what this page fetched since it opened.
  const marked = useRef(null);
  if (marked.current !== page) { marked.current = page; pageData.markPage(); }
  useKeyboard({ page, loc, layout, updateLayout, setPalette, palette, creating, toggleSidebar, order, termId });
  useRailData();
  useDetailsBand(loc.open, page);
  useLanding(page, loc, watchDoc, setLoc);

  useEffect(() => {
    const current = PAGES.find((p) => p.key === page);
    // The document view titles the tab once its document loads (F.7).
    if (page === 'doc') return;
    document.title = page === 'terminal' ? `${sessions.find((s) => s.id === termId)?.name || termId} · sm` : `sm · ${current ? current.label : 'Agents'}`;
  }, [page, termId, sessions]);
  // The landing reopens the last terminal you had open (D.5).
  useEffect(() => { if (termId) store('sm-term-last', termId); }, [termId]);

  const term = page === 'terminal';
  const docPath = page === 'doc' ? docPathFrom(loc.path, location.search) : null;
  const panelOpen = !!loc.open && !inBand(loc.open, page) && !(page === 'inbox' && /^(thread|work):/.test(loc.open));
  const cls = ['app', term && 'term', page === 'doc' && 'doc', folded && 'folded', panelOpen && 'with-panel', panelOpen && layout.panel_mode === 'wide' && 'wide']
    .filter(Boolean).join(' ');
  const termName = termId ? (sessions.find((s) => s.id === termId)?.name || termId) : '';
  const newAgent = (at) => creating && creating.at === at
    ? html`<${NewAgentPopover} key=${JSON.stringify(creating.prefill)} prefill=${creating.prefill} align=${at === 'sidebar' ? 'left' : 'right'}
        onClose=${closeCreating} />` : null;
  // Pages without a New agent button (the terminal, Notes) still open the sheet, at the top right.
  const barHost = term || page === 'notes';
  const closePalette = () => { setPalette(false); if (term) setTimeout(() => activeTerminal.focus(), 0); };
  return html`<div class=${cls} style=${`--panel-w:${clampWidth(layout.panel_width_rem)}rem`}>
      <${Sidebar} page=${page} termId=${termId} watch=${watch} folded=${folded} arrangement=${arrangement[arrangeKind(kind)]}
        setFolded=${(value) => updateSidebar({ [kind]: value ? 'folded' : 'open' })}
        swap=${() => updateArrangement({ [arrangeKind(kind)]: arrangement[arrangeKind(kind)] === 'pages' ? 'agents' : 'pages' })}
        onSearch=${() => setPalette(true)} onNew=${() => setCreating(creating ? null : { prefill: {}, at: 'sidebar' })} />
      <main class="main">
        ${mountedId ? html`<div key=${mountedId} hidden=${!term} style="display:flex;flex:1;min-height:0;flex-direction:column">
          <${TerminalPage} id=${mountedId} visible=${term} onEnded=${releaseKept} watch=${watch} moon=${html`<${MoonButton} />`} />
        </div>` : null}
        ${page === 'doc' ? html`<${Reader} key=${docPath} id=${docPath + location.hash} watch=${watch} moon=${html`<${MoonButton} />`} />` : null}
        ${!term && page !== 'doc' ? html`
          <${TopBar} page=${page} offline=${offline} stale=${stale} onSearch=${() => setPalette(true)}
            creating=${creating} setCreating=${setCreating} newAgent=${newAgent('bar')} watch=${watch} />
          <${NightBanner} />
          <${HostRestartBanner} />
          <${Page} page=${page} loc=${loc} watch=${watch} />` : null}
      </main>
      ${panelOpen ? html`<${Panel} openRef=${loc.open} layout=${layout} updateLayout=${updateLayout}
        beside=${term && loc.open === `agent:${termId}` ? 'terminal' : null}
        notesTarget=${term ? { id: termId, label: termName } : null} />` : null}
      <${TabBar} page=${page} />
    </div>
    ${barHost && creating?.at === 'bar' ? html`<span class="anchor new-agent-host">${newAgent('bar')}</span>` : null}
    ${creating?.at === 'sidebar' ? html`<span class="anchor new-agent-host side">${newAgent('sidebar')}</span>` : null}
    ${palette ? html`<${Palette} onClose=${closePalette} />` : null}
    <${Toasts} items=${toasts} />`;
}

/**
 * `/` and `/watch` (2543 D.5): wider than 720 px, the last terminal you had open if it is live,
 * else the top sidebar row; `?open=agent:<id>` lands on that agent's terminal with Details.
 * At phone width `/` stays: it is the agents list.
 */
function useLanding(page, loc, doc, setLoc) {
  useEffect(() => {
    if (page !== 'home' || window.innerWidth <= 720) return;
    const go = (id, open) => {
      history.replaceState(history.state, '', urlFor(`/terminal/${encodeURIComponent(id)}`, open));
      setLoc(readLocation());
    };
    if (loc.open?.startsWith('agent:')) { go(loc.open.slice('agent:'.length), loc.open); return; }
    if (!doc) return;
    // A poll taken before a retire still lists the retired session; never land on it.
    const target = landingFor((doc.sessions || []).filter((s) => !retiredIds.has(s.id)), stored('sm-term-last', null));
    if (target) go(target.id, loc.open);
  }, [page, loc, doc]);
}

/** Mount the detail renderer in the clicked row without making it part of the page's data tree. */
function useDetailsBand(openRef, page) {
  useEffect(() => {
    if (!inBand(openRef, page)) return;
    const host = document.createElement('section');
    host.className = 'details-band';
    host.setAttribute('aria-label', 'Details');
    const split = openRef.indexOf(':');
    const Renderer = panels.get(openRef.slice(0, split));
    const controls = html`<span class="ctl"><button class="icon-btn" type="button" title="Close (Esc)" onClick=${closePanel}><${Icon} name="close" size="14" /></button></span>`;
    render(Renderer ? html`<${Renderer} id=${openRef.slice(split + 1)} controls=${controls} />` : null, host);
    const place = () => {
      const content = document.querySelector('.main > .content');
      if (!content) return;
      const anchor = [...content.querySelectorAll('[data-open-ref]')].find(node => node.dataset.openRef === openRef)
        || (openOrigin?.isConnected ? openOrigin : null);
      if (!anchor) { if (host.parentElement !== content) content.append(host); return; }
      if (anchor.classList.contains('card')) {
        const top = anchor.offsetTop;
        const cards = [...anchor.parentElement.children].filter(node => node.classList.contains('card') && node.offsetTop === top);
        const last = cards.at(-1) || anchor;
        if (last.nextElementSibling === host) return;
        last.after(host);
      } else { if (anchor.nextElementSibling === host) return; anchor.after(host); }
      host.scrollIntoView({ block: 'nearest' });
    };
    place();
    const observer = new MutationObserver(place);
    observer.observe(document.querySelector('.main'), { childList: true, subtree: true });
    window.addEventListener('resize', place);
    return () => { observer.disconnect(); window.removeEventListener('resize', place); render(null, host); host.remove(); };
  }, [openRef, page]);
}

function Page({ page, loc, watch }) {
  if (page === 'home') return html`<${Home} watch=${watch} />`;
  if (page === 'queue') return html`<${QueuePage} />`;
  if (page === 'artifacts') return html`<${ArtifactsPage} />`;
  if (page === 'analytics') return html`<${AnalyticsPage} path=${loc.path} />`;
  if (page === 'settings') return html`<${SettingsPage} />`;
  if (page === 'board') return html`<${BoardPage} />`;
  if (page === 'inbox') return html`<${InboxPage} openRef=${loc.open} />`;
  if (page === 'notes') return html`<div class="content notes-content"><${NotesView} /></div>`;
  if (page === 'history') return html`<${HistoryPage} path=${loc.path} />`;
  if (page === 'guestbook') return html`<${GuestbookPage} />`;
  const current = PAGES.find((p) => p.key === page) || HOME;
  return html`<div class="content"><div class="stub">
    <h2>${current.label}</h2>
    <p>The ${current.label} page is not built yet. It arrives in a later web ticket; until then the phone app has it.</p>
  </div></div>`;
}

// ---- rail -------------------------------------------------------------------

/** The rail's badges, polled every 30 s (D2), shared with pages that need them. */
function useRailData() {
  const [, , reloadArtifacts] = usePoll(async () => setShared('artifacts', await api('/artifacts')), 60000, [], { background: true });
  useEffect(() => bus.on('artifacts-changed', reloadArtifacts), [reloadArtifacts]);
  // The rail's counters are the shell's own reads, never the owner's (spec 2170 A.5).
  const [, , reloadQueue] = usePoll(async () => setShared('queue', await api('/client/queue')), 30000, [], { background: true });
  useEffect(() => bus.on('queue-changed', reloadQueue), [reloadQueue]);
  usePoll(async () => setShared('inbox', await api('/inbox?format=json')), 30000, [], { background: true });
  usePoll(async () => setShared('board_badge', await api('/client/board/badge')), 30000, [], { background: true });
  usePoll(async () => setShared('overnight', await api('/client/overnight')), 30000, [], { background: true });
}

// ---- overnight (sm#2494) ----------------------------------------------------

/** The banner's first line: why overnight is on, and since when; the phone's words. */
export function nightHeadline(status) {
  const away = status?.away;
  if (!away) return 'Overnight';
  const since = away.since_label;
  if (away.source === 'bedtime') return `Past bedtime${since ? ` · since ${since}` : ''}`;
  if (away.source === 'dnd') return `Do Not Disturb${since ? ` · since ${since}` : ''}`;
  return `Overnight${since ? ` since ${since}` : ''}`;
}

/** The second line: what sm does about it. */
export function nightDetail(status) {
  const away = status?.away;
  const alerts = !away || away.holds_alerts ? 'alerts wait' : 'your phone silences alerts';
  return `Agents work on their own; ${alerts}${away?.until_label ? ` until ${away.until_label}` : ''}.`;
}

/** What waits: blocked agents first, then unread messages; '' when nothing does. */
export function nightWaiting(status) {
  const waiting = status?.waiting;
  if (!waiting) return '';
  const blocked = waiting.blocked || [];
  const parts = [];
  if (blocked.length === 1) parts.push(`${blocked[0]} is blocked on you`);
  else if (blocked.length > 1) parts.push(`${blocked.length} agents blocked on you`);
  if (waiting.unread === 1) parts.push('1 new message');
  else if (waiting.unread > 1) parts.push(`${waiting.unread} new messages`);
  return parts.join(' · ');
}

async function setOvernight(on) {
  const previous = shared.overnight;
  setShared('overnight', { ...(previous || {}), on, pending: true });
  try {
    setShared('overnight', await api('/client/overnight', { method: 'PUT', body: { on } }));
  } catch (error) {
    setShared('overnight', previous);
    toast(`Couldn't change overnight mode: ${error.message}`);
  }
}

/** The moon: one click tells agents you are away; click again when back. */
function MoonButton() {
  const status = useShared('overnight');
  const on = !!status?.on;
  const auto = on && status?.away && !status.manual;
  const title = on ? `${nightHeadline(status)}. Click when you're back.` : 'Overnight mode: tell agents you are away';
  return html`<button type="button" class=${['moon', on && 'on', auto && 'auto'].filter(Boolean).join(' ')}
    aria-pressed=${on} aria-label="Overnight mode" title=${title} onClick=${() => setOvernight(!on)}>
    <${Icon} name=${on ? 'moonFill' : 'moon'} size=${17} /></button>`;
}

/** Under the top bar while overnight is on. */
function NightBanner() {
  const status = useShared('overnight');
  if (!status?.on) return null;
  const waiting = nightWaiting(status);
  return html`<div class="night" role="status">
    <span class="orb"><${Icon} name="moonFill" size=${17} /></span>
    <div class="txt">
      <div class="hd">${nightHeadline(status)}</div>
      <div class="dt">${nightDetail(status)}</div>
      ${waiting ? html`<div class=${`wt ${status.waiting?.blocked?.length ? 'red' : ''}`}>${waiting}</div>` : null}
    </div>
    <button type="button" onClick=${() => setOvernight(false)}>I'm back</button>
  </div>`;
}

function badges(queue, inbox, board) {
  const out = {};
  if (board) out.board = { count: board.count || 0 };
  if (queue) {
    const waiting = queue.queued || [];
    const long = waiting.some((job) => job.queued_at && Date.now() - Date.parse(job.queued_at) >= 30 * 60 * 1000);
    // The sidebar shows running · waiting beside Queue (2543 B.3); the folded icon and the tab bar, waiting.
    out.queue = { count: waiting.length, text: `${(queue.running || []).length} · ${waiting.length}`, tone: long ? 'amber' : '' };
  }
  // A blocked agent (sm#2494) turns the badge red.
  const blocked = (inbox?.rows || []).some((row) => row.group === 'blocked');
  if (inbox) out.inbox = { count: inbox.needs_you_count || 0, tone: blocked ? 'red' : inbox.needs_you_count ? 'magenta' : '' };
  return out;
}

function useBadges() {
  const queue = useShared('queue');
  const inbox = useShared('inbox');
  const board = useShared('board_badge');
  const counts = badges(queue, inbox, board);
  const artifacts = useShared('artifacts');
  counts.artifacts = { count: (artifacts?.artifacts || []).filter(a => a.needs_you?.reasons?.includes('would_delete')).length, tone: 'amber' };
  return { counts, inbox };
}

/** A page link: a full row, or (`icon`) an icon with its badge as a dot and the label as tooltip. */
function pageLink(p, page, counts, { icon = false, full = false } = {}) {
  const badge = counts[p.key];
  const click = (event) => {
    if (event.metaKey || event.ctrlKey || event.shiftKey) return;
    event.preventDefault();
    navigate(p.path);
  };
  const text = full && badge?.text ? badge.text : badge?.count || '';
  return html`<a href=${p.path} class=${['item', icon && 'icon', page === p.key && 'on', p.minor && 'minor'].filter(Boolean).join(' ')}
    title=${icon ? p.label : ''} aria-label=${icon ? p.label : null} aria-current=${page === p.key ? 'page' : null} onClick=${click}>
    <${Icon} name=${p.icon} /><span class="lb">${p.label}</span>
    ${badge ? html`<b class=${[badge.tone, badge.count || (full && badge.text) ? '' : 'zero'].filter(Boolean).join(' ')}>${text}</b>` : null}
  </a>`;
}

const live = (s) => s.state !== 'stopped' && s.attention?.section !== 'stopped';
const HOVER_MS = 400;

/**
 * The one left column (2543 B): search, page links, the Agents heading and the live
 * sessions (the only part that scrolls), meters, Settings. Pages first shows every page
 * link and one-line session rows; agents first shows the links as icons and today's
 * two-line rows. Folded, it is a 14 px strip with a dot when something needs you.
 */
function Sidebar({ page, termId, watch, folded, arrangement, setFolded, swap, onSearch, onNew }) {
  const [doc, , reload] = watch;
  const [idleOpen, setIdleOpen] = useState(() => stored(IDLE_KEY, false));
  const [card, setCard] = useState(null);
  const timer = useRef(null);
  const { counts, inbox } = useBadges();
  const term = page === 'terminal';
  const sessions = (doc && doc.sessions) || [];
  const current = term ? termId : null;
  const { groups } = switcherGroups(sessions, current, idleOpen);
  const agents = sessions.filter((s) => !isShell(s) && live(s));
  const needs = agents.filter((s) => ['blocked', 'you'].includes(s.attention?.section)).length;
  const signal = agents.some((s) => s.attention?.section === 'blocked') || (inbox?.rows || []).some((row) => row.group === 'blocked') ? 'red'
    : needs || inbox?.needs_you_count ? 'magenta' : '';
  useEffect(() => () => clearTimeout(timer.current), []);
  useEffect(() => { if (folded) setCard(null); }, [folded]);

  const refocus = () => { if (term) setTimeout(() => activeTerminal.focus(), 0); };
  const pick = (agent) => {
    clearTimeout(timer.current);
    setCard(null);
    // Between 720 and 900 px the sidebar covers the page, so it closes once you choose (B.9).
    if (window.innerWidth < 900) setFolded(true);
    if (agent.id === current) refocus();
    else navigate(`/terminal/${encodeURIComponent(agent.id)}`);
  };
  const toggleIdle = () => { store(IDLE_KEY, !idleOpen); setIdleOpen(!idleOpen); refocus(); };
  // The hover card (D.3): after 400 ms on an agent row; it stays while the pointer is on the row or the card.
  const touch = typeof matchMedia === 'function' && matchMedia('(hover: none)').matches;
  const hover = (agent) => (isShell(agent) || touch ? {} : {
    onMouseEnter: (event) => {
      const row = event.currentTarget;
      clearTimeout(timer.current);
      timer.current = setTimeout(() => {
        const box = row.getBoundingClientRect();
        setCard({ id: agent.id, top: box.top, left: (row.closest('.sb-wrap')?.getBoundingClientRect().right || box.right) + 6 });
      }, HOVER_MS);
    },
    onMouseLeave: () => { clearTimeout(timer.current); timer.current = setTimeout(() => setCard(null), 150); },
  });
  const shown = card && sessions.find((s) => s.id === card.id);

  const strip = html`<button type="button" class="sb-edge" title=${`${folded ? 'Unfold' : 'Fold'} sidebar (⌘\\)`}
    aria-label=${`${folded ? 'Unfold' : 'Fold'} sidebar`} aria-expanded=${!folded}
    onClick=${() => { setFolded(!folded); refocus(); }}>
    ${folded && signal ? html`<i class=${`sb-signal ${signal}`}></i>` : null}
    <${Icon} name=${folded ? 'unfold' : 'fold'} size="14" /></button>`;
  if (folded) return html`<div class="sb-wrap">${strip}</div>`;
  const pagesFirst = arrangement === 'pages';
  const top = PAGES.filter((p) => !p.bottom);
  return html`<div class="sb-wrap">
    <nav class=${`sidebar ${pagesFirst ? 'pages-first' : 'agents-first'}`} aria-label="Sidebar">
      <div class="sb-top">
        <a class="sb-brand" href="/" onClick=${(e) => { e.preventDefault(); navigate('/'); }}>sm</a>
        <button type="button" class="search" onClick=${onSearch}><span>Search or jump…</span><kbd>⌘K</kbd></button>
      </div>
      ${pagesFirst
        ? html`<div class="sb-pages">${top.map((p) => pageLink(p, page, counts, { full: true }))}</div>`
        : html`<div class="sb-icons">${top.map((p) => pageLink(p, page, counts, { icon: true }))}</div>`}
      <div class="sb-head">
        <span class="lb">Agents</span>
        <button type="button" class="plain-btn sb-live" title=${pagesFirst ? 'Show agents first' : 'Show pages first'}
          aria-label=${`${agents.length} live; ${pagesFirst ? 'show agents first' : 'show pages first'}`} onClick=${swap}>
          ${agents.length} live ${pagesFirst ? '›' : '⌄'}</button>
        ${needs ? html`<b class="magenta" title=${`${needs} need${needs === 1 ? 's' : ''} you`}>${needs}</b>` : null}
        <span class="sp"></span>
        <button type="button" class="icon-btn sb-new" data-pop-anchor title="New agent or terminal" aria-label="New agent"
          onClick=${onNew}>+</button>
      </div>
      <div class="sb-agents">
        ${pagesFirst
          ? html`<${Lines} groups=${groups} currentId=${current} idleOpen=${idleOpen} toggleIdle=${toggleIdle} pick=${pick} hover=${hover} />`
          : html`<${Switcher} groups=${groups} currentId=${current} idleOpen=${idleOpen} toggleIdle=${toggleIdle}
              pick=${pick} reload=${reload} hover=${hover} />`}
      </div>
      <${Dash} />
      ${PAGES.filter((p) => p.bottom).map((p) => pageLink(p, page, counts))}
    </nav>
    ${strip}
    ${shown ? html`<div class="sb-card" style=${`top:${Math.max(8, Math.min(card.top, window.innerHeight - 220))}px;left:${card.left}px`}
        onMouseEnter=${() => clearTimeout(timer.current)} onMouseLeave=${() => setCard(null)}>
        <${AgentCard} agent=${shown} now=${Date.now()} onAnswered=${reload} onRetired=${reload} onOpen=${() => pick(shown)} /></div>` : null}
  </div>`;
}

/** Pages first's one-line rows (B.4): dot, name, short fact; no headings, a gap between sections. */
function Lines({ groups, currentId, idleOpen, toggleIdle, pick, hover }) {
  return html`<div class="sb-lines" role="list" aria-label="Sessions">${groups.map(({ section, count, agents }) => html`<div class="sb-group" key=${section}>
    ${section === 'idle' ? html`<button type="button" class="sb-line sb-idle" aria-expanded=${idleOpen} onClick=${toggleIdle}>
      Idle · ${count} ${idleOpen ? '⌄' : '›'}</button>` : null}
    ${agents.map((agent) => {
      const fact = shortFact(agent);
      return html`<div key=${agent.id} role="button" tabindex="-1" aria-current=${agent.id === currentId ? 'page' : null}
        class=${`sb-line${agent.id === currentId ? ' cur' : ''}`} onClick=${() => pick(agent)} ...${hover(agent)}>
        ${isShell(agent) ? html`<span class="sw-shell muted">▹</span>` : html`<i class=${`sw-dot ${SECTION_TONE[section]}`}></i>`}
        <span class="sw-nm" title=${agent.name}>${agent.name}</span>
        <span class=${`sb-fact ${fact.tone}`}>${fact.text}</span></div>`;
    })}</div>`)}</div>`;
}

/** The phone's bottom tab bar (B.10), unchanged from the old rail at that width. */
function TabBar({ page }) {
  const { counts } = useBadges();
  return html`<nav class="tabbar" aria-label="Pages">${TABS.map((p) => pageLink(p, page, counts))}</nav>`;
}

/** `/`: wider than 720 px the landing moves on to a terminal, so this shows only when none is live; at phone width, the agents list. */
function Home({ watch }) {
  const [doc, error, reload] = watch;
  const [idleOpen, setIdleOpen] = useState(() => stored(IDLE_KEY, false));
  if (!doc) return html`<div class="content">${error ? html`<p class="err">${error.message}</p>` : html`<p class="muted">Loading…</p>`}</div>`;
  const { groups } = switcherGroups(doc.sessions || [], null, idleOpen);
  if (!groups.length || window.innerWidth > 720) {
    return html`<div class="content home-empty"><p>No live agents</p>
      <button type="button" class="btn pri" onClick=${() => bus.emit('new-agent', {})}>New agent</button></div>`;
  }
  return html`<div class="content home-list"><${Switcher} groups=${groups} currentId=${null} idleOpen=${idleOpen}
    toggleIdle=${() => { store(IDLE_KEY, !idleOpen); setIdleOpen(!idleOpen); }}
    pick=${(agent) => navigate(`/terminal/${encodeURIComponent(agent.id)}`)} reload=${reload} /></div>`;
}

// ---- top bar ----------------------------------------------------------------

/** List pages (2543 C): the page name; then Moon, Bug and the page's main action in the top-right corner. */
function TopBar({ page, offline, stale, onSearch, creating, setCreating, newAgent, watch }) {
  const [doc, , reload] = watch;
  const hidden = hiddenShells(doc?.sessions);
  const current = PAGES.find((p) => p.key === page) || HOME;
  return html`<header class="bar">
    <h1>${current.label}</h1>
    <button type="button" class="search" onClick=${onSearch}><span>Search or jump…</span><kbd>⌘K</kbd></button>
    <span class="sp"></span>
    ${offline ? html`<span class="offline" role="status">Offline, retrying</span>` : null}
    ${stale ? html`<button type="button" class="stale-build" onClick=${() => location.reload()}>sm was updated · Reload</button>` : null}
    ${hidden.length ? html`<${HiddenShells} shells=${hidden} onChanged=${reload} />` : null}
    <${MoonButton} />
    <${BugButton} page=${current.label} />
    ${page === 'notes'
      ? html`<button type="button" class="btn pri" onClick=${() => bus.emit('notes-new')}>New note</button>`
      : html`<span class="anchor">
          <button type="button" class="btn pri" data-pop-anchor onClick=${() => setCreating(creating ? null : { prefill: {}, at: 'bar' })}>New agent</button>
          ${newAgent}
        </span>`}
  </header>`;
}

// ---- side panel ---------------------------------------------------------------

/** `beside` is 'terminal' for the open terminal's own agent; `notesTarget` lets Notes type into that terminal. */
function Panel({ openRef, layout, updateLayout, beside = null, notesTarget = null }) {
  const split = openRef.indexOf(':');
  const kind = split < 0 ? openRef : openRef.slice(0, split);
  const id = split < 0 ? '' : openRef.slice(split + 1);
  const Renderer = panels.get(kind);
  const grip = useRef(null);
  const wide = layout.panel_mode === 'wide';

  const drag = (event) => {
    event.preventDefault();
    const handle = grip.current;
    handle.classList.add('drag');
    const move = (e) => {
      const rem = clampWidth((window.innerWidth - e.clientX) / REM());
      document.querySelector('.app').style.setProperty('--panel-w', `${rem}rem`);
      handle.dataset.rem = rem;
    };
    const up = () => {
      handle.classList.remove('drag');
      window.removeEventListener('mousemove', move);
      window.removeEventListener('mouseup', up);
      if (handle.dataset.rem) updateLayout({ panel_width_rem: Math.round(Number(handle.dataset.rem) * 10) / 10 });
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', up);
  };

  const controls = html`<span class="ctl">
    <button type="button" class="icon-btn" title=${wide ? 'Side panel (⌘.)' : 'Widen (⌘.)'}
      onClick=${() => updateLayout({ panel_mode: wide ? 'side' : 'wide' })}><${Icon} name=${wide ? 'narrow' : 'wide'} size="14" /></button>
    <button type="button" class="icon-btn" title="Close (Esc)" onClick=${closePanel}><${Icon} name="close" size="14" /></button>
  </span>`;
  return html`<aside class=${notesTarget ? 'panel term-panel' : 'panel'} aria-label=${kind === 'notes' ? 'Notes' : 'Details'}>
    <div class="grip" ref=${grip} onMouseDown=${drag}></div>
    ${kind === 'notes' ? html`<${NotesView} pane onClose=${closePanel} target=${notesTarget || undefined}
        onType=${notesTarget ? (text) => activeTerminal.paste(text) : undefined} />` : Renderer
      ? html`<${Renderer} key=${openRef} id=${id} controls=${controls} beside=${beside} />`
      : html`<div class="phd"><span></span><span class="t">${kind}</span>${controls}
          <span class="s">This item does not open in the panel yet.</span></div>`}
  </aside>`;
}

// ---- command palette (⌘K) ----------------------------------------------------

function Palette({ onClose }) {
  const [query, setQuery] = useState('');
  const [items, setItems] = useState(() => PAGES.map(pageItem));
  const [index, setIndex] = useState(0);
  const [loading, setLoading] = useState(true);
  const input = useRef(null);
  useEffect(() => {
    input.current && input.current.focus();
    Promise.allSettled([
      api('/watch/state'),
      api('/client/board'),
      api('/docs', { headers: { Accept: 'application/json' } }),
    ]).then(([watch, board, docs]) => {
      const out = PAGES.map(pageItem);
      if (watch.status === 'fulfilled') {
        for (const agent of watch.value.sessions || []) {
          out.push({ kind: agent.kind === 'shell' ? 'Terminal' : 'Agent', label: agent.name, detail: basename(agent.repo),
            run: () => navigate(`/terminal/${encodeURIComponent(agent.id)}`) });
        }
      }
      if (board.status === 'fulfilled') {
        const seen = new Set();
        const tickets = [
          ...(board.value.lanes || []).flatMap((lane) => lane.tickets || []),
          ...(board.value.other || []).flatMap((group) => group.tickets || []),
        ];
        for (const ticket of tickets) {
          const key = `${ticket.repo}#${ticket.number}`;
          if (seen.has(key)) continue;
          seen.add(key);
          out.push({
            kind: 'Ticket', label: `#${ticket.number} ${ticket.title}`, detail: basename(ticket.repo),
            run: () => openItem('ticket', key, ticket.url),
          });
        }
      }
      if (docs.status === 'fulfilled') {
        for (const doc of docs.value.docs || []) {
          out.push({
            kind: 'Doc', label: doc.title || doc.name, detail: doc.author_session_name || '',
            run: () => openReader(doc.reader_path),
          });
        }
      }
      setItems(out);
      setLoading(false);
    });
  }, []);

  const matches = useMemo(() => {
    const terms = query.toLowerCase().replace(/#/g, '').split(/\s+/).filter(Boolean);
    const hits = items.filter((item) => {
      const text = `${item.label} ${item.detail}`.toLowerCase().replace(/#/g, '');
      return terms.every((term) => text.includes(term));
    });
    return hits.slice(0, 60);
  }, [items, query]);
  useEffect(() => setIndex(0), [query]);

  const choose = (item) => {
    if (!item) return;
    onClose();
    item.run();
  };
  const key = (event) => {
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onClose(); }
    else if (event.key === 'ArrowDown') { event.preventDefault(); setIndex((i) => Math.min(i + 1, matches.length - 1)); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); setIndex((i) => Math.max(i - 1, 0)); }
    else if (event.key === 'Enter') { event.preventDefault(); choose(matches[index]); }
  };
  return html`<div class="scrim" onMouseDown=${(e) => e.target === e.currentTarget && onClose()}>
    <div class="palette" role="dialog" aria-label="Search or jump">
      <input ref=${input} placeholder="Agents, tickets, docs, pages…" value=${query}
        onInput=${(e) => setQuery(e.target.value)} onKeyDown=${key} />
      <ul role="listbox">
        ${matches.map((item, i) => html`<li role="option" aria-selected=${i === index} class=${i === index ? 'on' : ''}
          onMouseEnter=${() => setIndex(i)} onClick=${() => choose(item)}>
          <span class="k">${item.kind}</span><span>${item.label}</span><span class="d">${item.detail}</span></li>`)}
        ${loading ? html`<li class="muted">Loading agents, tickets and docs…</li>` : null}
        ${matches.length || loading ? null : html`<li class="muted">Nothing matches.</li>`}
      </ul>
    </div>
  </div>`;
}

const pageItem = (p) => ({ kind: 'Page', label: p.label, detail: p.key_hint ? `g ${p.key_hint}` : '', run: () => navigate(p.path) });

// ---- keyboard (D2) -------------------------------------------------------------

function useKeyboard({ page, loc, layout, updateLayout, setPalette, palette, creating, toggleSidebar, order, termId }) {
  const state = useRef({});
  state.current = { page, loc, layout, palette, creating, toggleSidebar, order, termId };
  useEffect(() => {
    let pendingG = 0;
    const down = (event) => {
      const { page: current, loc: where, layout: now, palette: paletteOpen } = state.current;
      const mod = event.metaKey || event.ctrlKey;
      if (mod && event.key.toLowerCase() === 'j') {
        event.preventDefault();
        if (where.open === 'notes:view' && document.querySelector('.notes-dialog')) return;
        where.open === 'notes:view' ? closePanel() : openPanel('notes:view');
        return;
      }
      // 2543 G: on a terminal only ⌘ counts, so Ctrl+K and Ctrl+\ still reach the agent.
      const term = current === 'terminal';
      const cmd = term ? event.metaKey : mod;
      if (event.metaKey && event.altKey && (event.key === 'ArrowUp' || event.key === 'ArrowDown')) {
        event.preventDefault();
        const next = stepRow(state.current.order, term ? state.current.termId : null, event.key === 'ArrowUp' ? -1 : 1);
        if (next && next.id !== state.current.termId) navigate(`/terminal/${encodeURIComponent(next.id)}`);
        return;
      }
      if (cmd && !event.altKey && event.key.toLowerCase() === 'k') {
        event.preventDefault();
        setPalette((open) => !open);
        return;
      }
      if (cmd && event.key === '\\') {
        event.preventDefault();
        state.current.toggleSidebar();
        return;
      }
      if (term) return;
      if (mod && event.key === '.') {
        if (where.open && !inBand(where.open, current)) {
          event.preventDefault();
          updateLayout({ panel_mode: now.panel_mode === 'wide' ? 'side' : 'wide' });
        }
        return;
      }
      if (paletteOpen || mod || event.altKey) return;
      if (event.key === 'Escape') {
        // The first Escape in Notes collapses its open editor; the next closes the pane.
        if (where.open === 'notes:view' && document.querySelector('.notes-dialog')) return;
        if (where.open === 'notes:view' && document.querySelector('.notes-pane .notes-editor')) return;
        if (where.open) closePanel();
        return;
      }
      if (typingIn(event)) return;
      if (event.key === 'g') {
        pendingG = Date.now();
        return;
      }
      if (pendingG && Date.now() - pendingG < 1500) {
        pendingG = 0;
        const target = event.key === 'a' ? HOME : PAGES.find((p) => p.key_hint === event.key);
        if (target) {
          event.preventDefault();
          navigate(target.path);
        }
      }
    };
    document.addEventListener('keydown', down);
    return () => document.removeEventListener('keydown', down);
  }, []);
}

// ---- toasts -------------------------------------------------------------------

function Toasts({ items }) {
  return html`<div class="toasts" aria-live="polite">
    ${items.map((item) => item.action
    ? html`<div class="toast" key=${item.id}>${item.text}
        <button type="button" class="toast-action" onClick=${() => item.action.run()}>${item.action.label}</button></div>`
    : html`<button type="button" class="toast" key=${item.id}
      onClick=${() => item.onClick && item.onClick()}>${item.text}</button>`)}
  </div>`;
}

render(html`<${App} />`, document.getElementById('app'));
