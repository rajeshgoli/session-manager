// The sm web shell (spec 1710 D1, D2): router, left menu, top bar, side
// panel, command palette, rail badges and toasts. Pages are modules; a page
// whose module has not landed yet links to today's page (`legacy`) or shows
// a placeholder.
import { render } from 'preact';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import {
  html, api, bus, network, usePoll, useShared, setShared, stored, store, panels,
  openPanel, closePanel, navigate, openItem, toast, typingIn, Icon, Ring, Seg, gigabytes, basename,
} from './ui.js';
import { BoardPage } from './board.js';
import { InboxPage } from './inbox.js';
import { HistoryPage } from './history.js';
import { GuestbookPage } from './guestbook.js';
import { AgentsPage } from './agents.js';
import { QueuePage } from './queue.js';
import { AnalyticsPage } from './analytics.js';
import { SettingsPage } from './settings.js';
import { TerminalPage } from './terminal.js';
import { NewAgentPopover } from './start.js';

// ---- pages ------------------------------------------------------------------

const PAGES = [
  { key: 'agents', label: 'Agents', icon: 'agents', path: '/', key_hint: 'a' },
  { key: 'board', label: 'Board', icon: 'board', path: '/board', key_hint: 'b' },
  { key: 'queue', label: 'Queue', icon: 'queue', path: '/queue', key_hint: 'q' },
  { key: 'inbox', label: 'Inbox', icon: 'inbox', path: '/inbox', key_hint: 'i' },
  { key: 'analytics', label: 'Analytics', icon: 'analytics', path: '/analytics', minor: true },
  { key: 'history', label: 'History', icon: 'history', path: '/history', minor: true },
  { key: 'guestbook', label: 'Guestbook', icon: 'history', path: '/guestbook', minor: true },
  { key: 'settings', label: 'Settings', icon: 'settings', path: '/settings', key_hint: 's', bottom: true },
];

function pageFor(path) {
  if (path === '/' || path === '/watch') return 'agents';
  if (path.startsWith('/terminal/')) return 'terminal';
  const page = PAGES.find((p) => p.path !== '/' && (path === p.path || path.startsWith(`${p.path}/`)));
  return page ? page.key : 'agents';
}

function readLocation() {
  const params = new URLSearchParams(location.search);
  const open = params.get('open');
  return { path: location.pathname, open: params.get('panel') || (open?.includes(':') ? open : null) };
}

function urlFor(path, open) {
  const params = new URLSearchParams(path === location.pathname ? location.search : '');
  // History reserves `open` for its ticket filter. Keep panel state separate
  // there, while still accepting old `?open=ticket:…` links on arrival.
  const key = path === '/history' ? 'panel' : 'open';
  params.delete('panel');
  if (key === 'open' || params.get('open')?.includes(':')) params.delete('open');
  if (open) params.set(key, open);
  // Keep panel links readable: `?open=agent:65203ac8`.
  const query = params.toString().replace(/%3A/gi, ':');
  return `${path}${query ? `?${query}` : ''}`;
}

// ---- layout (D2 folding and sizing) ------------------------------------------

const REM = () => parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;

function initialLayout() {
  const saved = stored('sm-layout', {}) || {};
  return {
    rail: saved.rail === 'full' || saved.rail === 'folded' ? saved.rail : window.innerWidth < 1100 ? 'folded' : 'full',
    panel_width_rem: typeof saved.panel_width_rem === 'number' ? saved.panel_width_rem : 28,
    panel_mode: saved.panel_mode === 'wide' ? 'wide' : 'side',
  };
}

const clampWidth = (rem) => Math.max(20, Math.min(rem, (window.innerWidth * 0.6) / REM()));

// ---- shell ------------------------------------------------------------------

function App() {
  const [loc, setLoc] = useState(readLocation);
  const [layout, setLayout] = useState(initialLayout);
  const [palette, setPalette] = useState(false);
  const [creating, setCreating] = useState(null);
  const [toasts, setToasts] = useState([]);
  const [offline, setOffline] = useState(network.offline);

  const updateLayout = useCallback((patch) => {
    setLayout((prev) => {
      const next = { ...prev, ...patch };
      store('sm-layout', next);
      return next;
    });
  }, []);

  useEffect(() => {
    const pop = () => setLoc(readLocation());
    window.addEventListener('popstate', pop);
    const offs = [
      bus.on('navigate', (path) => {
        const target = PAGES.find((p) => p.path === path);
        if (target && target.legacy) {
          location.href = path;
          return;
        }
        history.pushState(null, '', urlFor(path, path.startsWith('/terminal/') ? null : readLocation().open));
        setLoc(readLocation());
      }),
      bus.on('open', (ref) => {
        const current = readLocation();
        if (current.open === ref) return;
        history.pushState(null, '', urlFor(current.path, ref));
        setLoc(readLocation());
      }),
      bus.on('toast', (item) => {
        const id = Math.random();
        setToasts((prev) => [...prev, { ...item, id }]);
        setTimeout(() => setToasts((prev) => prev.filter((t) => t.id !== id)), 6000);
      }),
      bus.on('new-agent', (prefill) => setCreating(prefill)),
      bus.on('network', setOffline),
    ];
    return () => {
      window.removeEventListener('popstate', pop);
      offs.forEach((off) => off());
    };
  }, []);

  const page = pageFor(loc.path);
  useKeyboard({ page, loc, layout, updateLayout, setPalette, palette, creating });
  useRailData();

  useEffect(() => {
    const current = PAGES.find((p) => p.key === page);
    document.title = page === 'terminal' ? 'sm · Terminal' : `sm · ${current ? current.label : 'Agents'}`;
  }, [page]);

  if (page === 'terminal') {
    return html`<${TerminalPage} id=${decodeURIComponent(loc.path.slice('/terminal/'.length))} open=${loc.open} />
      <${Toasts} items=${toasts} />`;
  }

  const panelOpen = !!loc.open && !(page === 'inbox' && /^(doc|thread):/.test(loc.open));
  const cls = ['app', layout.rail === 'folded' && 'folded', panelOpen && layout.panel_mode === 'wide' && 'wide']
    .filter(Boolean).join(' ');
  return html`<div class=${cls} style=${`--panel-w:${clampWidth(layout.panel_width_rem)}rem`}>
      <${Rail} page=${page} layout=${layout} updateLayout=${updateLayout} />
      <main class="main">
        <${TopBar} page=${page} offline=${offline} onSearch=${() => setPalette(true)}
          creating=${creating} setCreating=${setCreating} />
        <${Page} page=${page} loc=${loc} />
      </main>
      ${panelOpen ? html`<${Panel} openRef=${loc.open} layout=${layout} updateLayout=${updateLayout} />` : null}
    </div>
    ${palette ? html`<${Palette} onClose=${() => setPalette(false)} />` : null}
    <${Toasts} items=${toasts} />`;
}

function Page({ page, loc }) {
  if (page === 'agents') return html`<${AgentsPage} openRef=${loc.open} />`;
  if (page === 'queue') return html`<${QueuePage} />`;
  if (page === 'analytics') return html`<${AnalyticsPage} path=${loc.path} />`;
  if (page === 'settings') return html`<${SettingsPage} />`;
  if (page === 'board') return html`<${BoardPage} />`;
  if (page === 'inbox') return html`<${InboxPage} openRef=${loc.open} />`;
  if (page === 'history') return html`<${HistoryPage} path=${loc.path} />`;
  if (page === 'guestbook') return html`<${GuestbookPage} />`;
  const current = PAGES.find((p) => p.key === page);
  return html`<div class="content"><div class="stub">
    <h2>${current.label}</h2>
    <p>The ${current.label} page is not built yet. It arrives in a later web ticket; until then the phone app has it.</p>
  </div></div>`;
}

// ---- rail -------------------------------------------------------------------

/** The rail's badges, polled every 30 s (D2), shared with pages that need them. */
function useRailData() {
  const [, , reloadQueue] = usePoll(async () => setShared('queue', await api('/client/queue')), 30000);
  useEffect(() => bus.on('queue-changed', reloadQueue), [reloadQueue]);
  usePoll(async () => setShared('inbox', await api('/inbox?format=json')), 30000);
  usePoll(async () => setShared('board_badge', await api('/client/board/badge')), 30000);
}

function badges(queue, inbox, board) {
  const out = {};
  if (board) out.board = { count: board.count || 0 };
  if (queue) {
    const waiting = queue.queued || [];
    const long = waiting.some((job) => job.queued_at && Date.now() - Date.parse(job.queued_at) >= 30 * 60 * 1000);
    out.queue = { count: waiting.length, tone: long ? 'amber' : '' };
  }
  if (inbox) out.inbox = { count: inbox.needs_you_count || 0, tone: inbox.needs_you_count ? 'magenta' : '' };
  return out;
}

function Rail({ page, layout, updateLayout }) {
  const queue = useShared('queue');
  const inbox = useShared('inbox');
  const board = useShared('board_badge');
  const counts = badges(queue, inbox, board);
  const folded = layout.rail === 'folded';
  const item = (p) => {
    const badge = counts[p.key];
    const click = (event) => {
      if (p.legacy || event.metaKey || event.ctrlKey || event.shiftKey) return;
      event.preventDefault();
      navigate(p.path);
    };
    return html`<a href=${p.path} class=${['item', page === p.key && 'on', p.minor && 'minor'].filter(Boolean).join(' ')}
      title=${folded ? p.label : ''} aria-current=${page === p.key ? 'page' : null} onClick=${click}>
      <${Icon} name=${p.icon} /><span class="lb">${p.label}</span>
      ${badge ? html`<b class=${[badge.tone, badge.count ? '' : 'zero'].filter(Boolean).join(' ')}>${badge.count || ''}</b>` : null}
    </a>`;
  };
  return html`<nav class="rail" aria-label="Pages">
    <div class="brand"><a href="/" onClick=${(e) => { e.preventDefault(); navigate('/'); }}>sm</a>
      <button type="button" class="fold" title=${folded ? 'Expand menu (⌘\\)' : 'Fold menu (⌘\\)'}
        onClick=${() => updateLayout({ rail: folded ? 'full' : 'folded' })}>
        <${Icon} name=${folded ? 'unfold' : 'fold'} size="14" /></button></div>
    ${PAGES.filter((p) => !p.bottom).map(item)}
    <span class="grow"></span>
    ${PAGES.filter((p) => p.bottom).map(item)}
  </nav>`;
}

// ---- top bar ----------------------------------------------------------------

function Meter({ label, total, queue, text, className = '' }) {
  const width = (value) => `width:${Math.max(0, Math.min(100, value || 0))}%`;
  return html`<span class=${`meter ${className}`} title=${label}>${label}
    <i><s style=${width(total)}></s>${typeof queue === 'number' ? html`<s class="q" style=${width(queue)}></s>` : null}</i>${text}</span>`;
}

function TopBar({ page, offline, onSearch, creating, setCreating }) {
  const [host] = usePoll(() => api('/client/host-status'), 10000);
  const queue = useShared('queue');
  const current = PAGES.find((p) => p.key === page);
  const mac = host && host.available !== false ? host : null;
  const memTotal = mac && mac.memory_total_bytes;
  const memPct = (bytes) => (memTotal ? (100 * bytes) / memTotal : 0);
  const running = queue ? (queue.running || []).length : null;
  const waiting = queue ? (queue.queued || []).length : null;
  return html`<header class="bar">
    <h1>${current ? current.label : ''}</h1>
    <button type="button" class="search" onClick=${onSearch}><span>Search or jump…</span><kbd>⌘K</kbd></button>
    <span class="sp"></span>
    ${offline ? html`<span class="offline" role="status">Offline, retrying</span>` : null}
    <span class="meters">
      ${mac
        ? html`<${Meter} label="CPU" total=${mac.cpu_percent} queue=${mac.queue_cpu_percent}
            text=${`${Math.round(mac.cpu_percent || 0)}%`} />
          <${Meter} label="GPU" total=${mac.gpu_percent} queue=${mac.queue_gpu_percent}
            text=${typeof mac.gpu_percent === 'number' ? `${Math.round(mac.gpu_percent)}%` : '–'} className="opt" />
          <${Meter} label="Memory" total=${memPct(mac.memory_used_bytes)}
            queue=${typeof mac.queue_memory_bytes === 'number' ? memPct(mac.queue_memory_bytes) : undefined}
            text=${`${gigabytes(mac.memory_used_bytes)}/${gigabytes(memTotal)} GB`} className="opt" />`
        : null}
      ${queue
        ? html`<a class="meter link" href="/queue" onClick=${(e) => { e.preventDefault(); navigate('/queue'); }}>
            Queue ${running} running · ${waiting} waiting</a>`
        : null}
    </span>
    <span class="anchor">
      <button type="button" class="btn pri" data-pop-anchor onClick=${() => setCreating(creating ? null : {})}>New agent</button>
      ${creating ? html`<${NewAgentPopover} key=${JSON.stringify(creating)} prefill=${creating} onClose=${() => setCreating(null)} />` : null}
    </span>
  </header>`;
}

// ---- side panel ---------------------------------------------------------------

function Panel({ openRef, layout, updateLayout }) {
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
  return html`<aside class="panel" aria-label="Details">
    <div class="grip" ref=${grip} onMouseDown=${drag}></div>
    ${Renderer
      ? html`<${Renderer} key=${openRef} id=${id} controls=${controls} />`
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
          out.push({ kind: 'Agent', label: agent.name, detail: basename(agent.repo), run: () => openPanel(`agent:${agent.id}`) });
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
            run: () => openItem('doc', doc.reader_path, doc.reader_path),
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

function useKeyboard({ page, loc, layout, updateLayout, setPalette, palette, creating }) {
  const state = useRef({});
  state.current = { page, loc, layout, palette, creating };
  useEffect(() => {
    let pendingG = 0;
    const down = (event) => {
      const { page: current, loc: where, layout: now, palette: paletteOpen } = state.current;
      const mod = event.metaKey || event.ctrlKey;
      if (current === 'terminal') return;
      if (mod && event.key.toLowerCase() === 'k') {
        event.preventDefault();
        setPalette((open) => !open);
        return;
      }
      if (mod && event.key === '\\') {
        event.preventDefault();
        updateLayout({ rail: now.rail === 'folded' ? 'full' : 'folded' });
        return;
      }
      if (mod && event.key === '.') {
        if (where.open) {
          event.preventDefault();
          updateLayout({ panel_mode: now.panel_mode === 'wide' ? 'side' : 'wide' });
        }
        return;
      }
      if (paletteOpen || mod || event.altKey) return;
      if (event.key === 'Escape') {
        if (where.open && !typingIn(event)) closePanel();
        return;
      }
      if (typingIn(event)) return;
      if (event.key === 'g') {
        pendingG = Date.now();
        return;
      }
      if (pendingG && Date.now() - pendingG < 1500) {
        pendingG = 0;
        const target = PAGES.find((p) => p.key_hint === event.key);
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
    ${items.map((item) => html`<button type="button" class="toast" key=${item.id}
      onClick=${() => item.onClick && item.onClick()}>${item.text}</button>`)}
  </div>`;
}

render(html`<${App} />`, document.getElementById('app'));
