// Terminal page (spec 1710; 1782 G1-G4; 2543 C, E.1): session rows, phone keys, route and round-trip time.
// It renders inside the app frame; the app's sidebar lists the sessions and its side panel holds Details.
import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks';
import { html, api, bus, openPanel, closePanel, navigate, Icon, Ring, stored, toast, age, basename, readerPath, openReader } from './ui.js';
import { AgentMenus, RetireButton, openInClaude, sectionAgents, SECTION_LABEL, SECTION_TONE, youFact, jobsFact, agentFact, pairedText, markAnswered } from './agents.js';
import { BugButton } from './bug.js';
import { chooseRoute, shellBrowserAllowed, relayRoute, rememberRoute, routeText, findDirect, markDirect, recentlyDirect, DIRECT_RETRY_MS } from './terminal-route.js';
import { NotesView } from './notes.js';
import { CONTRAST_FLOOR, THEME_KEY, isLight, terminalTheme } from './terminal-theme.js';
import { installImagePaste } from './terminal-images.js';
import { installFileLinks, oscFilePath, fileHref } from './terminal-files.js';
import { installPageMouse } from './terminal-mouse.js';
import { isShell, shellFact, ShellCloseButton, ShellAwayPanel, ShellMenu, ShellName, setShellHidden, hiddenShells, HiddenShells } from './shell.js';
import './vendor/xterm.js';
import './vendor/addon-fit.js';

const keys = [['Esc', 'escape'], ['Tab', 'tab'], ['Shift+Tab', 'shift-tab'], ['↑', 'up'], ['↓', 'down'], ['Ctrl-C', 'ctrl-c']];
const back = () => history.length > 1 ? history.back() : navigate('/');
const PING_MS = 5000;
// A direct path that answered its probe attaches in well under this; past it, use the relay.
const DIRECT_HANDSHAKE_MS = 5000;
// A window resize reaches the PTY once the window has been still this long (#2242)...
const RESIZE_SETTLE_MS = 150;
// ...and once the agent has redrawn at that size, a one-column nudge makes it repaint every cell.
const REDRAW_SETTLE_MS = 400;
const REDRAW_NUDGE_MS = 120;
// Notes type text in pieces this small, this far apart, so Claude reads typing rather than a paste
// (#2623). 128 characters is at most 512 bytes, the piece `sm send` types (#1446).
const TYPE_CHUNK_CHARS = 128;
const TYPE_GAP_MS = 20;
/** Sessions retired from this page, which an older poll may still list as live. */
export const retiredIds = new Set();

/**
 * What the agent needs from you, for the top bar's pill (#2591): a doc out for
 * review opens in the document view, a question or blocking message opens its
 * thread beside the terminal. A permission prompt is already on screen, so no pill.
 */
export function needsYou(agent, now = Date.now()) {
  const you = agent?.facts?.you;
  if (!you || !['message', 'blocked', 'doc_review'].includes(you.kind)) return null;
  const since = age(you.since, now) || 'now';
  const doc = you.kind === 'doc_review' ? (agent.docs || []).find((d) => d.id === you.doc_id) : null;
  if (doc?.reader_path) return { kind: 'doc', label: 'Review doc', since, title: doc.title || you.text, reader: doc.reader_path, more: you.more || 0 };
  return { kind: 'thread', label: you.kind === 'blocked' ? 'Blocked on you' : you.kind === 'doc_review' ? 'Review doc' : 'Read message',
    since, title: you.text, more: you.more || 0 };
}

/** The terminal a document was opened from by its pill, so ⌘. in the document view comes back. */
export const pillReturn = { id: null, path: null };

/** The open terminal, for the app's Notes pane ("Type into terminal") and focus after the palette closes. */
export const activeTerminal = { id: null, type: async () => false, focus: () => {} };
// Terminal links open in a new tab; web URLs only.
// A link to one of sm's own documents opens the document view (2543 F.4); anything else, a new tab.
const openTarget = (uri) => {
  const doc = readerPath(uri);
  if (doc) openReader(doc);
  else window.open(uri, '_blank', 'noopener,noreferrer');
};
const openLink = (event, uri) => {
  if (!/^https?:\/\//i.test(uri)) return;
  event?.preventDefault?.();
  openTarget(uri);
};
const session = () => { try { return window.sessionStorage; } catch (e) { return null; } };

// ---- switcher (1782 G1) ---------------------------------------------------------

/** The row's second line: the you or finished text (with its ✓), else jobs or a review round, else the agent fact. */
export function switcherFact(agent, now = Date.now()) {
  if (isShell(agent)) return shellFact(agent, now);
  const you = youFact(agent, now);
  if (you) return you;
  const jobs = agent.facts && agent.facts.jobs;
  return (jobs && jobs.tone) || pairedText(agent.paired_reviewer) ? jobsFact(agent) : agentFact(agent, now);
}

/**
 * Live agents in attention sections. A collapsed Idle section keeps only the
 * current agent. Hidden shells stay out, except the one open now; the
 * top bar's eye lists them. `order` is the visible rows, for ⌘⌥↑ and ⌘⌥↓.
 */
export function switcherGroups(sessions, currentId, idleOpen) {
  const live = sessions.filter((s) => s.state !== 'stopped' && !(s.attention && s.attention.section === 'stopped'));
  const groups = sectionAgents(live).map(({ section, agents }) => {
    const shown = section !== 'idle' || idleOpen ? agents : agents.filter((a) => a.id === currentId);
    return { section, count: agents.length, agents: shown };
  });
  const shells = live.filter(isShell).sort((a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id));
  if (shells.length) {
    const idle = groups.findIndex(group => group.section === 'idle');
    const shown = shells.filter(shell => !shell.hidden || shell.id === currentId);
    if (shown.length) groups.splice(idle < 0 ? groups.length : idle, 0, { section: 'terminals', count: shown.length, agents: shown });
  }
  return { groups, order: groups.flatMap((g) => g.agents) };
}

/** Where `/` lands (2543 D.5): the last terminal you had open if it is live, else the top row; null when none is live. */
export function landingFor(sessions, lastId) {
  const last = lastId && sessions.find((s) => s.id === lastId && s.state !== 'stopped' && s.attention?.section !== 'stopped');
  return last || switcherGroups(sessions, null, true).order[0] || null;
}

/** A one-line row's fact (2543 B.4): `41h`, `done`, `▶ 39m`, `⏸ review`, `2m`, `shell`. */
export function shortFact(agent, now = Date.now()) {
  if (isShell(agent)) return { text: 'shell', tone: 'muted' };
  const first = (text) => (text || '').split(' ')[0];
  const span = (text) => first((/\d+h(?: \d+m)?|\d+m|\d+s/.exec(text) || [''])[0]);
  const you = youFact(agent, now);
  if (you) return you.tone === 'cyan' ? { text: 'done', tone: 'cyan' } : { text: first(age(agent.facts.you.since, now)) || 'now', tone: you.tone };
  const jobs = agent.facts && agent.facts.jobs;
  if ((jobs && jobs.tone) || pairedText(agent.paired_reviewer)) {
    const fact = jobsFact(agent);
    if (fact.text.startsWith('▶')) return { text: `▶ ${span(fact.text)}`.trim(), tone: fact.tone };
    if (fact.text.startsWith('⏸')) {
      const why = /review/i.test(fact.text) ? 'review' : /queue|in line/i.test(fact.text) ? 'queue' : span(fact.text);
      return { text: `⏸ ${why}`.trim(), tone: fact.tone };
    }
    if (/review/i.test(fact.text)) return { text: 'review', tone: fact.tone };
    return { text: span(fact.text) || first(fact.text), tone: fact.tone };
  }
  const state = agent.facts && agent.facts.agent && agent.facts.agent.state;
  if (state === 'parked') return { text: 'parked', tone: 'amber' };
  return { text: first(age(agent.facts && agent.facts.agent && agent.facts.agent.since, now)), tone: state === 'working' ? 'green' : 'muted' };
}

/** The row `step` away from the current one; from outside the list, the first or last. */
export function stepRow(order, currentId, step) {
  if (!order.length) return null;
  const at = order.findIndex((a) => a.id === currentId);
  if (at < 0) return step > 0 ? order[0] : order[order.length - 1];
  return order[Math.max(0, Math.min(order.length - 1, at + step))];
}

/** After retiring the current agent: the row below it, else the row above; null when none is left. */
export function rowAfterRetire(order, currentId) {
  const rest = order.filter((a) => a.id !== currentId);
  if (!rest.length) return null;
  const at = order.findIndex((a) => a.id === currentId);
  return rest[Math.min(Math.max(at, 0), rest.length - 1)];
}

const narrow = () => window.innerWidth < 900;
/** Whether the Idle section is expanded; the sidebar and this page share it. */
export const IDLE_KEY = 'sm-term-switcher-idle';
// Key buttons only where there is no keyboard to send them (G2).
const touchKeys = () => window.matchMedia('(pointer: coarse)').matches || window.innerWidth < 720;

function useWidth(test) {
  const [on, setOn] = useState(test);
  useEffect(() => {
    const update = () => setOn(test());
    window.addEventListener('resize', update);
    return () => window.removeEventListener('resize', update);
  }, []);
  return on;
}

/** Two-line session rows in attention sections (the sidebar's agents-first arrangement). */
export function Switcher({ groups, currentId, idleOpen, toggleIdle, pick, reload, hover }) {
  const row = (section) => (agent) => {
    const fact = switcherFact(agent);
    const label = fact.tone === 'cyan' ? 'read' : 'answered';
    return html`<div key=${agent.id} role="button" tabindex="-1" aria-current=${agent.id === currentId ? 'page' : null}
        class=${`sw-row${agent.id === currentId ? ' cur' : ''}${agent.hidden ? ' sw-hidden' : ''}`} onClick=${() => pick(agent)}
        ...${hover ? hover(agent) : {}}>
      ${isShell(agent) ? html`<span class="sw-shell muted">▹</span>` : html`<i class=${`sw-dot ${SECTION_TONE[section]}`}></i>`}
      <span class="sw-nm" title=${agent.name}>${agent.name}</span>
      ${fact.dismissible
        ? html`<button type="button" class=${`icon-btn ok ${fact.tone}`} title=${`Mark ${label}`} aria-label=${`Mark ${agent.name} ${label}`}
            onClick=${(event) => { event.stopPropagation(); markAnswered(agent, reload); }}>✓</button>`
        : html`<span></span>`}
      <span class=${`sw-fact ${fact.tone}`} title=${fact.text}>${fact.text}</span>
    </div>`;
  };
  return html`<div class="term-switch" role="list" aria-label="Sessions">
    ${groups.map(({ section, count, agents }) => html`
      ${section === 'idle'
        ? html`<button type="button" class="sw-sec muted" aria-expanded=${idleOpen} onClick=${toggleIdle}>
            ${SECTION_LABEL.idle} · ${count} ${idleOpen ? '⌄' : '›'}</button>`
        : html`<div class=${`sw-sec ${section === 'terminals' ? 'muted sw-terminals-heading' : SECTION_TONE[section]}`}>${section === 'terminals' ? 'Terminals' : SECTION_LABEL[section]}</div>`}
      ${agents.map(row(section))}`)}
  </div>`;
}

// ---- page --------------------------------------------------------------------

/** `watch` is the app's poll of GET /watch/state: `[doc, error, reload]`. `moon` is the app's overnight button. */
export function TerminalPage({ id, watch, moon, open, visible = true, onEnded }) {
  const host = useRef(null);
  const control = useRef(null);
  const shown = useRef(visible);
  shown.current = visible;
  useLayoutEffect(() => { control.current?.visibility(); }, [visible]);
  const [transportConnection, setConnection] = useState('Connecting');
  const [transportError, setError] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [route, setRoute] = useState({ label: '', samples: [] });
  const [away, setAway] = useState(false);
  const [directError, setDirectError] = useState('');
  const [renaming, setRenaming] = useState(false);
  const showKeys = useWidth(touchKeys);
  // Touch screens keep the phone bar and its keys, even a tablet held wide.
  const wide = !useWidth(narrow) && !showKeys;
  const [doc, watchError, reload] = watch;
  const agent = doc && (doc.sessions || []).find((s) => s.id === id);
  const shell = isShell(agent);
  // Wait for identity before connecting, so a shell can never attach as an agent.
  const targetKind = agent && agent.state !== 'stopped' && agent.attention?.section !== 'stopped'
    ? (shell ? 'shell' : 'agent') : null;
  const lookupFailed = !doc && !!watchError;
  const unavailable = !!doc && !targetKind;
  const connection = lookupFailed || unavailable ? 'Ended' : targetKind ? transportConnection : 'Connecting';
  const error = lookupFailed ? 'Could not load session information. Retry to check whether this terminal is still available.'
    : unavailable ? 'This terminal has ended or could not be found. The session may have stopped or been retired. Choose another session in the sidebar.'
    : transportError;
  useEffect(() => {
    if (!visible && connection === 'Ended') onEnded?.(id);
  }, [visible, connection, id, onEnded]);
  // The pill and ⌘. open what the agent needs from you; a second press closes the thread again.
  const ask = !shell ? needsYou(agent) : null;
  const thread = `thread:${id}`;
  const openAsk = () => {
    if (open === thread) { closePanel(); setTimeout(() => control.current?.focus(), 0); return; }
    if (!ask) return;
    if (ask.kind === 'doc') { Object.assign(pillReturn, { id, path: ask.reader }); openReader(ask.reader); }
    else openPanel(thread);
  };
  const askRef = useRef(openAsk);
  askRef.current = openAsk;
  useEffect(() => visible ? bus.on('needs-you', () => askRef.current()) : undefined, [visible]);
  // The sidebar's order, for Retire's next row.
  const { order } = switcherGroups((doc && doc.sessions) || [], id, stored(IDLE_KEY, false));
  const hidden = hiddenShells(doc?.sessions);
  // A click on the bar gives the keyboard back to the terminal, except inside the
  // Report-a-bug dialog, which renders within the bar and has its own fields.
  const refocus = (event) => {
    if (event?.target?.closest?.('.board-start-overlay, .pop, .shell-name, .shell-name-input, .retire-confirm')) return;
    setTimeout(() => control.current?.focus(), 0);
  };
  // A retired agent's terminal has nothing left to show, so move on to the next agent,
  // or to `/` when none is left (going back could land on one retired earlier).
  const retired = (done) => {
    if (!done) { retiredIds.delete(id); reload(); return; }
    retiredIds.add(id);
    reload();
    const next = rowAfterRetire(order, id);
    navigate(next ? `/terminal/${encodeURIComponent(next.id)}` : '/');
  };
  // Hiding sends the shell to the tray like a minimised window: move on as Retire does.
  // Undo brings it back and returns here.
  const hideShell = (hidden) => setShellHidden(agent, hidden, (now, undone) => {
    reload();
    if (undone) navigate(`/terminal/${encodeURIComponent(agent.id)}`);
    else if (now) retired(true);
  });

  useEffect(() => {
    if (!targetKind) return;
    setAway(false);
    setDirectError('');
    const terminal = new window.Terminal({
      fontFamily: '"SF Mono", Menlo, monospace', fontSize: parseFloat(getComputedStyle(document.documentElement).fontSize) - 1, cursorBlink: true,
      // tmux keeps the history and the wheel scrolls it. sm's tmux draws on the normal
      // screen, so a scrollback here would hold a second, stale copy that a selection
      // dragged past the top scrolls into and the wheel cannot leave (#2618).
      lineHeight: 1.12, scrollback: 0, minimumContrastRatio: CONTRAST_FLOOR,
      // OSC 8 links, without xterm's confirm dialog. Claude Code links file paths as
      // file://, which open in the file view like a plain path does (#2597); other schemes do nothing.
      linkHandler: { allowNonHttpProtocols: true, activate: (event, uri) => {
        const path = oscFilePath(uri);
        if (!path) { openLink(event, uri); return; }
        event?.preventDefault?.();
        openTarget(fileHref(id, path));
      } },
    });
    const fit = new window.FitAddon.FitAddon();
    terminal.loadAddon(fit);
    // URLs and file paths, including ones Claude Code or Codex split across rows (#2580).
    installFileLinks(terminal, id, {
      url: openLink,
      file: (event, uri) => {
        event?.preventDefault?.();
        openTarget(uri);
      },
    });
    terminal.open(host.current);
    // Follow Settings › Appearance and, on System, the OS switching light or dark (#2304).
    const scheme = matchMedia('(prefers-color-scheme: dark)');
    const retheme = () => {
      const theme = terminalTheme(isLight(), stored(THEME_KEY));
      terminal.options.theme = theme;
      host.current.style.background = theme.background;
    };
    retheme();
    scheme.addEventListener('change', retheme);
    const themeWatch = new MutationObserver(retheme);
    // Settings › Appearance in another tab; the same tab's Settings is a different page.
    const storageWatch = (event) => { if (event.key === THEME_KEY) retheme(); };
    window.addEventListener('storage', storageWatch);
    themeWatch.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
    let socket = null, stopped = false, live = false, timer = null, handshakeTimer = null, retries = 0;
    let pinger = null, pingId = 0, samples = [];
    // Background probes for the direct path while on the relay; a switch that fails stops them for this page.
    let directTimer = null, directSwitch = true, switching = false;
    const stopDirect = () => { clearTimeout(directTimer); directTimer = null; };
    const pings = new Map();
    const send = (frame) => {
      if (!shown.current && ['resize', 'input', 'key'].includes(frame.type)) return;
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify(frame));
        if (frame.type === 'resize') sentSize = `${frame.cols}x${frame.rows}`;
      }
    };
    // Dragging a window edge fires dozens of sizes a second, and each one makes the agent
    // redraw. Claude's renderer rewrites only the cells it thinks changed, so a redraw cut
    // short by the next size leaves stale cells that no later redraw clears (#2242). Only
    // the size the window settles on goes to the PTY; a one-column nudge then repaints it all.
    let resizeTimer = null, nudgeTimer = null, sentSize = '';
    const fitNow = () => {
      clearTimeout(resizeTimer);
      if (!shown.current) return false;
      const size = fit.proposeDimensions();
      if (!size || !Number.isFinite(size.cols) || !Number.isFinite(size.rows)) return false;
      // At least 11 columns, so the nudge stays within the bridge's 10-column floor.
      terminal.resize(Math.max(11, Math.min(300, size.cols)), Math.max(2, Math.min(120, size.rows)));
      const now = `${terminal.cols}x${terminal.rows}`;
      if (now === sentSize || !socket || socket.readyState !== WebSocket.OPEN) return false;
      sentSize = now;
      send({ type: 'resize', cols: terminal.cols, rows: terminal.rows });
      return true;
    };
    const redraw = () => {
      clearTimeout(nudgeTimer);
      if (!live || !shown.current) return;
      send({ type: 'resize', cols: terminal.cols - 1, rows: terminal.rows });
      nudgeTimer = setTimeout(() => send({ type: 'resize', cols: terminal.cols, rows: terminal.rows }), REDRAW_NUDGE_MS);
    };
    const resize = () => {
      clearTimeout(resizeTimer);
      if (!shown.current) return;
      resizeTimer = setTimeout(() => {
        if (fitNow() && live) { clearTimeout(nudgeTimer); nudgeTimer = setTimeout(redraw, REDRAW_SETTLE_MS); }
      }, RESIZE_SETTLE_MS);
    };
    const input = (data) => {
      if (!live) return;
      // Stay below the transport's 8192-character input limit, including pastes.
      const chars = Array.from(data);
      for (let i = 0; i < chars.length; i += 4096) send({ type: 'input', data: chars.slice(i, i + 4096).join('') });
    };
    const focus = () => { if (shown.current) terminal.focus(); };
    control.current = {
      visibility: () => {
        clearTimeout(resizeTimer); clearTimeout(nudgeTimer);
        if (shown.current) {
          terminal.options.fontSize = parseFloat(getComputedStyle(document.documentElement).fontSize) - 1;
          retheme(); fitNow(); focus();
        }
        else terminal.blur();
      },
      key: (key) => { if (live) send({ type: 'key', key }); focus(); },
      paste: (text) => { if (!live || !shown.current) return false; input(`\x1b[200~${text}\x1b[201~`); focus(); return true; },
      // Unbracketed, with line breaks as LF (Ctrl-J), which inserts a newline in Claude and Codex instead of sending.
      type: async (text) => {
        if (!live || !shown.current) return false;
        const chars = Array.from(text.replace(/\r\n?/g, '\n'));
        for (let i = 0; i < chars.length; i += TYPE_CHUNK_CHARS) {
          if (i > 0) await new Promise(resolve => setTimeout(resolve, TYPE_GAP_MS));
          if (!live) return false;
          input(chars.slice(i, i + TYPE_CHUNK_CHARS).join(''));
        }
        focus();
        return true;
      },
      model: () => { input('\x1b[200~/model\x1b[201~\r'); focus(); },
      focus,
      redraw: () => { redraw(); focus(); },
    };
    Object.assign(activeTerminal, { id, type: async (text) => !!await control.current?.type(text), focus: () => control.current?.focus() });
    const mouse = installPageMouse(terminal, host.current, input, async (text) => {
      try { await navigator.clipboard.writeText(text); toast('Copied'); } catch (err) { toast(`Could not copy: ${err.message}`); }
    });
    const removeImagePaste = installImagePaste(host.current, {
      connection: () => !stopped && live ? socket : null,
      upload: (body) => api(`/client/sessions/${encodeURIComponent(id)}/terminal-images`, { method: 'POST', body }),
      paste: (path) => control.current?.paste(path),
      pasteText: (text) => terminal.paste(text),
      report: toast,
    });
    // Round trips (G3): the bridge answers each ping from its loop, without the PTY.
    const ping = () => {
      if (!live) return;
      pings.set(++pingId, performance.now());
      if (pings.size > 5) pings.delete(pings.keys().next().value);
      send({ type: 'ping', id: pingId });
    };
    const stopPing = () => { clearInterval(pinger); pinger = null; pings.clear(); };
    const retry = () => {
      if (stopped) return;
      live = false;
      // A failed TLS handshake can trigger another Keychain prompt on every
      // attempt. Stop after three retries; the user can explicitly reconnect.
      if (retries >= 3) {
        setConnection('Ended');
        setError('Terminal connection failed. Check any sign-in or Keychain prompt. If device certificates were just enabled, quit and reopen your browser; otherwise choose Reconnect.');
        return;
      }
      setConnection('Reconnecting');
      timer = setTimeout(() => connect(), Math.min(10000, 500 * 2 ** Math.min(retries++, 5)));
    };
    // A direct socket that ends before attaching uses a fresh ticket through the relay (G4).
    const fallback = (ws, instance, message = 'The local connection ended before the shell could open.') => {
      clearTimeout(handshakeTimer);
      if (switching) directSwitch = false;
      if (socket === ws) socket = null;
      ws.close();
      rememberRoute(session(), 'relay', instance, Date.now());
      if (shell) { setDirectError(message); setAway(true); setConnection('SSH'); return; }
      connect(true);
    };
    // On the relay soon after a direct socket, probe for the direct path a few times and
    // move back to it when it answers, so a server restart does not pin this tab to the
    // relay for ten minutes. Away from this machine's network no direct socket ran
    // recently, so nothing is probed.
    const watchDirect = (ws, ticket) => {
      if (shell) return;
      if (!directSwitch || !recentlyDirect(session(), Date.now())) return;
      const next = (i) => {
        if (i >= DIRECT_RETRY_MS.length) return;
        directTimer = setTimeout(async () => {
          const found = await findDirect(ticket, { fetchImpl: (url, init) => fetch(url, init) });
          if (stopped || socket !== ws || !live) return;
          if (!found) { next(i + 1); return; }
          rememberRoute(session(), found, ticket.server_instance, Date.now());
          send({ type: 'detach' });
          socket = null; live = false; stopPing(); switching = true;
          ws.close();
          connect();
        }, DIRECT_RETRY_MS[i] - (i ? DIRECT_RETRY_MS[i - 1] : 0));
      };
      next(0);
    };
    const connect = async (relayOnly = false) => {
      if (stopped) return;
      stopDirect();
      setError('');
      try {
        const ticket = await api(`/client/sessions/${encodeURIComponent(id)}/browser-attach-ticket`, { method: 'POST' });
        if (stopped) return;
        const chosen = relayOnly ? relayRoute(ticket, location)
          : await chooseRoute(ticket, { location, fetchImpl: (url, init) => fetch(url, init), storage: session(), fresh: shell });
        if (stopped) return;
        samples = [];
        setRoute({ label: chosen.label, samples });
        if (shell && !shellBrowserAllowed(chosen)) {
          setAway(true); setConnection('SSH'); return;
        }
        const ws = new WebSocket(chosen.url);
        socket = ws;
        let ended = false, attached = false;
        // A pending Keychain dialog may never produce onclose. Stop this
        // attempt without another automatic signing request after 30 seconds.
        handshakeTimer = setTimeout(() => {
          if (stopped || socket !== ws || live) return;
          if (chosen.direct) { fallback(ws, ticket.server_instance, 'The local connection took too long to open. Please try again.'); return; }
          ended = true; socket = null;
          setConnection('Ended');
          setError('Terminal connection timed out. Check any sign-in or Keychain prompt. If device certificates were just enabled, quit and reopen your browser; otherwise choose Reconnect.');
          ws.close();
        }, chosen.direct ? DIRECT_HANDSHAKE_MS : 30000);
        ws.onopen = () => {
          if (stopped || socket !== ws) { ws.close(); return; }
          terminal.reset();
          send({ type: 'auth', ticket_id: ticket.ticket_id, ticket_secret: ticket.ticket_secret, output_ack: true });
          sentSize = '';
          fitNow();
        };
        ws.onmessage = (event) => {
          if (stopped || socket !== ws) return;
          let frame;
          try { frame = JSON.parse(event.data); } catch { return; }
          if (frame.type === 'status' && frame.state === 'attached') {
            clearTimeout(handshakeTimer);
            attached = true; live = true; retries = 0; setConnection('Live'); focus();
            stopPing(); ping(); pinger = setInterval(ping, PING_MS);
            switching = false;
            if (chosen.direct) markDirect(session(), Date.now());
            else watchDirect(ws, ticket);
          } else if (frame.type === 'pong') {
            const sent = pings.get(frame.id);
            if (sent === undefined) return;
            pings.delete(frame.id);
            samples = [...samples, performance.now() - sent].slice(-3);
            setRoute({ label: chosen.label, samples });
          } else if (frame.type === 'output') {
            const bytes = Uint8Array.from(atob(frame.data), (c) => c.charCodeAt(0));
            terminal.write(bytes, () => {
              if (!stopped && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: 'output_ack', sequence: frame.sequence }));
            });
          } else if (frame.type === 'exit' || frame.type === 'error') {
            if (chosen.direct && !attached) { fallback(ws, ticket.server_instance, frame.message || frame.reason); return; }
            clearTimeout(handshakeTimer);
            ended = true; live = false; stopPing(); setConnection('Ended');
            setError(frame.message || frame.reason || '');
            ws.close();
          }
        };
        ws.onclose = (event) => {
          // A direct socket ending (a restart, say) is the moment the tab last used it.
          if (chosen.direct && attached) markDirect(session(), Date.now());
          if (stopped || socket !== ws) return;
          if (chosen.direct && !attached) { fallback(ws, ticket.server_instance); return; }
          clearTimeout(handshakeTimer);
          live = false; stopPing(); stopDirect();
          if (ended || event.code === 1000 || event.code === 1008) setConnection('Ended');
          else retry();
        };
      } catch (err) {
        if (stopped) return;
        setError(err.message);
        if ([400, 401, 403, 404, 409].includes(err.status)) setConnection('Ended');
        else retry();
      }
    };
    const data = terminal.onData(input);
    terminal.attachCustomKeyEventHandler((event) => {
      if (!shown.current) return false;
      if (event.metaKey && event.key === '[') {
        if (event.type === 'keydown') { event.preventDefault(); back(); }
        return false;
      }
      if (mouse.copyKey(event, /Mac/.test(navigator.platform))) return false;
      // Let the browser deliver Ctrl+V as a clipboard event, including images,
      // on Windows/Linux. On macOS the capture handler reads it explicitly.
      if (event.ctrlKey && !event.altKey && event.key.toLowerCase() === 'v') return false;
      // The app handles ⌘J, ⌘K, ⌘., ⌘\ and ⌘⌥↑/↓ (2543 G); Ctrl+K and Ctrl+\ still reach the terminal.
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'j') return false;
      if (event.metaKey && (event.key.toLowerCase() === 'k' || event.key === '\\' || event.key === '.' || (event.altKey && (event.key === 'ArrowUp' || event.key === 'ArrowDown')))) return false;
      return true;
    });
    const observer = new ResizeObserver(resize);
    observer.observe(host.current);
    const detach = () => { live = false; stopPing(); send({ type: 'detach' }); if (socket) socket.close(); };
    const pagehide = () => { stopped = true; clearTimeout(timer); clearTimeout(handshakeTimer); stopDirect(); detach(); };
    const pageshow = (event) => { if (event.persisted) setAttempt((n) => n + 1); };
    window.addEventListener('pagehide', pagehide);
    window.addEventListener('pageshow', pageshow);
    setConnection('Connecting');
    fitNow();
    connect();
    return () => {
      removeImagePaste(); mouse.dispose();
      stopped = true; clearTimeout(timer); clearTimeout(handshakeTimer); clearTimeout(resizeTimer); clearTimeout(nudgeTimer); stopDirect(); detach(); observer.disconnect(); themeWatch.disconnect(); scheme.removeEventListener('change', retheme); window.removeEventListener('storage', storageWatch); data.dispose(); terminal.dispose();
      window.removeEventListener('pagehide', pagehide); window.removeEventListener('pageshow', pageshow); control.current = null;
      if (activeTerminal.id === id) Object.assign(activeTerminal, { id: null, type: async () => false, focus: () => {} });
    };
  }, [id, attempt, targetKind]);

  const linked = agent?.remote_control?.url?.startsWith('https://claude.ai/code/');
  const live = connection === 'Live';
  const shellAway = away && targetKind === 'shell';
  const retire = !agent || agent.state === 'stopped' ? null
    : shell ? (targetKind === 'shell' ? html`<${ShellCloseButton} key=${id} shell=${agent} onClosed=${() => retired(true)} />` : null)
    : html`<${RetireButton} key=${id} agent=${agent} onRetired=${retired} small />`;
  const details = html`<button type="button" class="btn sm" onClick=${() => openPanel(`agent:${id}`)}>Details</button>`;
  // Hand off… and ⋯ live here, not in the Details panel beside this terminal (#2277).
  const menus = (compact) => !agent ? null
    : shell ? html`<${ShellMenu} key=${id} shell=${agent} small align="right" onSaved=${reload} onRename=${() => setRenaming(true)} onHide=${hideShell} />`
    : html`<${AgentMenus} key=${id} agent=${agent} onSaved=${reload} small compact=${compact} align="right" />`;
  // A shell's name renames in place; Hide sits beside ⋯ so the tray is one click away.
  const name = shell ? html`<${ShellName} key=${id} shell=${agent} editing=${renaming} setEditing=${setRenaming} onSaved=${reload} />
      ${agent.hidden ? html`<span class="shell-hidden-pill">Hidden</span>` : null}`
    : html`<span class="t">${agent ? agent.name : id}</span>`;
  const hide = shell && targetKind === 'shell' ? html`<button type="button" class="btn sm" title=${agent.hidden ? 'Put this shell back in the lists' : 'Hide this shell from the lists; it keeps running'}
    onClick=${() => hideShell(!agent.hidden)}>${agent.hidden ? 'Unhide' : 'Hide'}</button>` : null;
  const notes = html`<button type="button" class="btn sm" onClick=${() => openPanel('notes:view')}>Notes ⌘J</button>`;
  const pill = ask ? html`<button type="button" class=${`term-you${open === thread ? ' on' : ''}`} aria-pressed=${ask.kind === 'thread' ? open === thread : null}
      title=${`${ask.title}${ask.more ? ` (+${ask.more} more)` : ''} · ⌘.`}
      onClick=${(event) => { event.stopPropagation(); openAsk(); }}>
      <i class="pulse" aria-hidden="true"></i><span class="lbl">${ask.label}</span>
      ${wide ? html`<span class="age">${ask.since}</span><kbd>⌘.</kbd>` : null}</button>` : null;
  // The agent's ticket fills the bar's empty middle (2543 C): number, title, repo. After the agent
  // releases it, the last ticket stays, marked released (#2579). Number and title open it on GitHub.
  const held = !shell && agent ? ((agent.claims || []).find((c) => c.kind === 'ticket') || (agent.claims || [])[0]) : null;
  const claim = held || (!shell && agent?.last_ticket?.number ? agent.last_ticket : null);
  const claimUrl = claim && (claim.url || `https://github.com/${claim.repo}/${claim.kind === 'pr' ? 'pull' : 'issues'}/${claim.number}`);
  const ticket = claim ? html`<a class="term-ticket" href=${claimUrl} target="_blank" rel="noopener"
      title=${`Open ${claim.kind === 'pr' ? 'the PR' : 'the ticket'} on GitHub`} onClick=${(event) => event.stopPropagation()}>
      <span class="tk">#${claim.number}</span>
      ${claim.title ? html`<span class="ttl">${claim.title}</span>` : null}
      ${claim.repo ? html`<span class="repo">${basename(claim.repo)}</span>` : null}
      ${held ? null : html`<span class="repo">released</span>`}</a>` : null;
  // Desktop (#2242, 2543 C): who and how full on the left, then the ticket; connection words only when
  // not live, with the route in the dot's tooltip; actions on the right, then Moon, Bug and Retire last.
  const desktopBar = () => html`<div class="term-bar wide" onClick=${refocus}>
      <span class="term-who" title=${[agent?.state, connection, live && route.label ? routeText(route.label, route.samples) : ''].filter(Boolean).join(' · ')}>
        ${shell ? html`<span class="muted">▹</span>` : html`<i class=${`sw-dot ${SECTION_TONE[agent?.attention?.section] || 'muted'}`}></i>`}
        ${name}
        ${agent && !shell ? html`<${Ring} percent=${agent.context_percent} />` : null}
      </span>
      ${live || shellAway ? null : html`<span class="term-connection" role="status">${connection}</span>`}
      ${pill}
      ${ticket}
      <span class="sp"></span>
      ${notes}${details}${hide}${menus(false)}
      <span class="term-icons">
        <${HiddenShells} shells=${hidden} currentId=${id} onChanged=${reload} />
        ${linked ? html`<button type="button" class="icon-btn" title="Open in Claude" aria-label="Open in Claude"
          onClick=${() => openInClaude(agent)}><${Icon} name="external" /></button>` : null}
        <button type="button" class="icon-btn" disabled=${!live} title=${shell ? 'Redraw the screen' : "Redraw the agent's screen"} aria-label="Redraw"
          onClick=${() => control.current?.redraw()}><${Icon} name="redraw" /></button>
        ${moon}
        <${BugButton} page="Terminal" />
      </span>
      ${retire}
    </div>`;

  return html`<div class="term-page" data-connection=${connection}>
    ${wide ? desktopBar() : html`<div class="term-bar" onClick=${refocus}>
      <a class="term-home" href="/" title="Home" onClick=${(e) => { e.preventDefault(); navigate('/'); }}>sm</a>
      <button type="button" class="icon-btn" title="Back (⌘[)" onClick=${back}><${Icon} name="back" /></button>
      ${pill}
      ${notes}
      ${agent && !shell ? html`<${Ring} percent=${agent.context_percent} />` : null}
      ${shell ? html`<span class="muted">▹</span>` : null}${name}${shell || pill ? null : html`<span class="sub term-state">${agent ? agent.state : ''}</span>`}
      ${shellAway ? null : html`<span class=${`term-connection ${connection === 'Live' ? 'green' : ''}`} role="status">${connection}</span>`}
      ${connection === 'Live' && route.label
        ? html`<span class="term-route" title="Route and round-trip time">${routeText(route.label, route.samples)}</span>` : null}
      ${showKeys && !shell ? html`<span class="term-keys">${keys.map(([label, key]) => html`<button type="button" class="btn sm" disabled=${connection !== 'Live'}
        onClick=${() => control.current?.key(key)}>${label}</button>`)}
        <button type="button" class="btn sm" disabled=${connection !== 'Live'} onClick=${() => control.current?.model()}>/model</button></span>` : null}
      <span class="sp"></span>
      ${linked ? html`<button type="button" class="btn sm" onClick=${() => openInClaude(agent)}>Open in Claude</button>` : null}
      ${hide}
      ${retire}
      ${menus(true)}
      <${HiddenShells} shells=${hidden} currentId=${id} onChanged=${reload} />
      <${BugButton} page="Terminal" />
      <button type="button" class="btn sm" disabled=${!live} title="Repaint the agent's screen"
        onClick=${() => control.current?.redraw()}>Redraw</button>
      ${details}
    </div>`}
    <div class="term-main">
      <div class="term-body" ref=${host} hidden=${shellAway}></div>
      ${shellAway ? html`<${ShellAwayPanel} key=${id} id=${id} directError=${directError} onRetry=${() => setAttempt(n => n + 1)} />` : null}
    </div>
    ${connection !== 'Live' && !shellAway ? html`<div class="term-notice" role="status">${error || connection}
      ${lookupFailed ? html`<button type="button" class="btn" onClick=${reload}>Retry</button>`
        : unavailable ? html`<a class="btn" href="/" onClick=${(event) => { event.preventDefault(); navigate('/'); }}>Open another agent</a>`
        : connection === 'Ended' ? html`<button type="button" class="btn" onClick=${() => setAttempt((n) => n + 1)}>Reconnect</button>` : null}</div>` : null}
  </div>`;
}
