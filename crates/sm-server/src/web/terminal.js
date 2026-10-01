// Terminal page (spec 1710; 1782 G1-G4): switcher, phone keys, route and round-trip time.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, usePoll, panels, openPanel, closePanel, navigate, Icon, Ring, config, stored, store } from './ui.js';
import { openInClaude, sectionAgents, SECTION_LABEL, SECTION_TONE, youFact, jobsFact, agentFact, pairedText, markAnswered } from './agents.js';
import { chooseRoute, relayRoute, rememberRoute, routeText } from './terminal-route.js';
import './vendor/xterm.js';
import './vendor/addon-fit.js';

const keys = [['Esc', 'escape'], ['Tab', 'tab'], ['Shift+Tab', 'shift-tab'], ['↑', 'up'], ['↓', 'down'], ['Ctrl-C', 'ctrl-c']];
const back = () => history.length > 1 ? history.back() : navigate('/');
const PING_MS = 5000;
// A direct path that answered its probe attaches in well under this; past it, use the relay.
const DIRECT_HANDSHAKE_MS = 5000;
const refreshMs = () => Math.max(1, config.refresh_seconds || 3) * 1000;
const session = () => { try { return window.sessionStorage; } catch (e) { return null; } };

// ---- switcher (1782 G1) ---------------------------------------------------------

/** The row's second line: the you or finished text (with its ✓), else jobs or a review round, else the agent fact. */
export function switcherFact(agent, now = Date.now()) {
  const you = youFact(agent, now);
  if (you) return you;
  const jobs = agent.facts && agent.facts.jobs;
  return (jobs && jobs.tone) || pairedText(agent.paired_reviewer) ? jobsFact(agent) : agentFact(agent, now);
}

/**
 * Live agents in attention sections. A collapsed Idle section keeps only the
 * current agent. `order` is the visible rows, for ⌘⌥↑ and ⌘⌥↓.
 */
export function switcherGroups(sessions, currentId, idleOpen) {
  const live = sessions.filter((s) => s.state !== 'stopped' && !(s.attention && s.attention.section === 'stopped'));
  const groups = sectionAgents(live).map(({ section, agents }) => {
    const shown = section !== 'idle' || idleOpen ? agents : agents.filter((a) => a.id === currentId);
    return { section, count: agents.length, agents: shown };
  });
  return { groups, order: groups.flatMap((g) => g.agents) };
}

/** The row `step` away from the current one; from outside the list, the first or last. */
export function stepRow(order, currentId, step) {
  if (!order.length) return null;
  const at = order.findIndex((a) => a.id === currentId);
  if (at < 0) return step > 0 ? order[0] : order[order.length - 1];
  return order[Math.max(0, Math.min(order.length - 1, at + step))];
}

const narrow = () => window.innerWidth < 900;
// Key buttons only where there is no keyboard to send them (G2).
const touchKeys = () => window.matchMedia('(pointer: coarse)').matches || window.innerWidth < 720;

function useTouchKeys() {
  const [on, setOn] = useState(touchKeys);
  useEffect(() => {
    const update = () => setOn(touchKeys());
    window.addEventListener('resize', update);
    return () => window.removeEventListener('resize', update);
  }, []);
  return on;
}

function Switcher({ groups, currentId, idleOpen, toggleIdle, pick, reload }) {
  return html`<nav class="term-switch" aria-label="Agents">
    ${groups.map(({ section, count, agents }) => html`
      ${section === 'idle'
        ? html`<button type="button" class="sw-sec muted" aria-expanded=${idleOpen} onClick=${toggleIdle}>
            ${SECTION_LABEL.idle} · ${count} ${idleOpen ? '⌄' : '›'}</button>`
        : html`<div class=${`sw-sec ${SECTION_TONE[section]}`}>${SECTION_LABEL[section]}</div>`}
      ${agents.map((agent) => {
        const fact = switcherFact(agent);
        const label = fact.tone === 'cyan' ? 'read' : 'answered';
        return html`<div key=${agent.id} role="button" tabindex="-1" aria-current=${agent.id === currentId ? 'page' : null}
            class=${`sw-row${agent.id === currentId ? ' cur' : ''}`} onClick=${() => pick(agent)}>
          <i class=${`sw-dot ${SECTION_TONE[section]}`}></i>
          <span class="sw-nm" title=${agent.name}>${agent.name}</span>
          ${fact.dismissible
            ? html`<button type="button" class=${`icon-btn ok ${fact.tone}`} title=${`Mark ${label}`} aria-label=${`Mark ${agent.name} ${label}`}
                onClick=${(event) => { event.stopPropagation(); markAnswered(agent, reload); }}>✓</button>`
            : html`<span></span>`}
          <span class=${`sw-fact ${fact.tone}`} title=${fact.text}>${fact.text}</span>
        </div>`;
      })}`)}
  </nav>`;
}

// ---- page --------------------------------------------------------------------

export function TerminalPage({ id, open }) {
  const host = useRef(null);
  const control = useRef(null);
  const [connection, setConnection] = useState('Connecting');
  const [error, setError] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [route, setRoute] = useState({ label: '', samples: [] });
  const [switcher, setSwitcher] = useState(() => stored('sm-term-switcher', !narrow()));
  const [idleOpen, setIdleOpen] = useState(() => stored('sm-term-switcher-idle', false));
  const showKeys = useTouchKeys();
  const [doc, , reload] = usePoll(() => api('/watch/state'), refreshMs(), []);
  const agent = doc && (doc.sessions || []).find((s) => s.id === id);
  const { groups, order } = switcherGroups((doc && doc.sessions) || [], id, idleOpen);
  const Renderer = panels.get('agent');

  const toggleSwitcher = () => { store('sm-term-switcher', !switcher); setSwitcher(!switcher); };
  const toggleIdle = () => { store('sm-term-switcher-idle', !idleOpen); setIdleOpen(!idleOpen); };
  const pick = (next) => {
    // Below 900 px the switcher covers the terminal, so it closes once you choose.
    if (narrow() && switcher) { store('sm-term-switcher', false); setSwitcher(false); }
    if (next.id !== id) navigate(`/terminal/${encodeURIComponent(next.id)}`);
  };
  const refocus = () => setTimeout(() => control.current?.focus(), 0);

  // ⌘\ toggles the switcher; ⌘⌥↑ and ⌘⌥↓ switch agents. xterm lets these through.
  const latest = useRef({});
  latest.current = { order, id, pick, toggleSwitcher };
  useEffect(() => {
    const down = (event) => {
      if (!event.metaKey) return;
      const now = latest.current;
      if (event.key === '\\' && !event.altKey) {
        event.preventDefault();
        now.toggleSwitcher();
      } else if (event.altKey && (event.key === 'ArrowUp' || event.key === 'ArrowDown')) {
        event.preventDefault();
        const next = stepRow(now.order, now.id, event.key === 'ArrowUp' ? -1 : 1);
        if (next) now.pick(next);
      }
    };
    window.addEventListener('keydown', down);
    return () => window.removeEventListener('keydown', down);
  }, []);

  useEffect(() => {
    const terminal = new window.Terminal({
      fontFamily: '"SF Mono", Menlo, monospace', fontSize: parseFloat(getComputedStyle(document.documentElement).fontSize) - 1, cursorBlink: true,
      scrollback: 10000, theme: { background: '#0E0F14', foreground: '#D7DAE3' },
    });
    const fit = new window.FitAddon.FitAddon();
    terminal.loadAddon(fit);
    terminal.open(host.current);
    let socket = null, stopped = false, live = false, timer = null, handshakeTimer = null, retries = 0;
    let pinger = null, pingId = 0, samples = [];
    const pings = new Map();
    const send = (frame) => {
      if (socket && socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(frame));
    };
    const resize = () => {
      const size = fit.proposeDimensions();
      if (!size) return;
      terminal.resize(Math.max(10, Math.min(300, size.cols)), Math.max(2, Math.min(120, size.rows)));
      send({ type: 'resize', cols: terminal.cols, rows: terminal.rows });
    };
    const input = (data) => {
      if (!live) return;
      // Stay below the transport's 8192-character input limit, including pastes.
      const chars = Array.from(data);
      for (let i = 0; i < chars.length; i += 4096) send({ type: 'input', data: chars.slice(i, i + 4096).join('') });
    };
    control.current = {
      key: (key) => { if (live) send({ type: 'key', key }); terminal.focus(); },
      model: () => { input('\x1b[200~/model\x1b[201~\r'); terminal.focus(); },
      focus: () => terminal.focus(),
    };
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
    const fallback = (ws, instance) => {
      clearTimeout(handshakeTimer);
      if (socket === ws) socket = null;
      ws.close();
      rememberRoute(session(), 'relay', instance, Date.now());
      connect(true);
    };
    const connect = async (relayOnly = false) => {
      if (stopped) return;
      setError('');
      try {
        const ticket = await api(`/client/sessions/${encodeURIComponent(id)}/browser-attach-ticket`, { method: 'POST' });
        if (stopped) return;
        const chosen = relayOnly ? relayRoute(ticket, location)
          : await chooseRoute(ticket, { location, fetchImpl: (url, init) => fetch(url, init), storage: session() });
        if (stopped) return;
        samples = [];
        setRoute({ label: chosen.label, samples });
        const ws = new WebSocket(chosen.url);
        socket = ws;
        let ended = false, attached = false;
        // A pending Keychain dialog may never produce onclose. Stop this
        // attempt without another automatic signing request after 30 seconds.
        handshakeTimer = setTimeout(() => {
          if (stopped || socket !== ws || live) return;
          if (chosen.direct) { fallback(ws, ticket.server_instance); return; }
          ended = true; socket = null;
          setConnection('Ended');
          setError('Terminal connection timed out. Check any sign-in or Keychain prompt. If device certificates were just enabled, quit and reopen your browser; otherwise choose Reconnect.');
          ws.close();
        }, chosen.direct ? DIRECT_HANDSHAKE_MS : 30000);
        ws.onopen = () => {
          if (stopped || socket !== ws) { ws.close(); return; }
          terminal.reset();
          send({ type: 'auth', ticket_id: ticket.ticket_id, ticket_secret: ticket.ticket_secret, output_ack: true });
          resize();
        };
        ws.onmessage = (event) => {
          if (stopped || socket !== ws) return;
          let frame;
          try { frame = JSON.parse(event.data); } catch { return; }
          if (frame.type === 'status' && frame.state === 'attached') {
            clearTimeout(handshakeTimer);
            attached = true; live = true; retries = 0; setConnection('Live'); terminal.focus();
            stopPing(); ping(); pinger = setInterval(ping, PING_MS);
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
            if (chosen.direct && !attached) { fallback(ws, ticket.server_instance); return; }
            clearTimeout(handshakeTimer);
            ended = true; live = false; stopPing(); setConnection('Ended');
            setError(frame.message || frame.reason || '');
            ws.close();
          }
        };
        ws.onclose = (event) => {
          if (stopped || socket !== ws) return;
          if (chosen.direct && !attached) { fallback(ws, ticket.server_instance); return; }
          clearTimeout(handshakeTimer);
          live = false; stopPing();
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
      if (event.metaKey && event.key === '[') {
        if (event.type === 'keydown') { event.preventDefault(); back(); }
        return false;
      }
      // The page handles ⌘\ and ⌘⌥↑/↓ (G1).
      if (event.metaKey && (event.key === '\\' || (event.altKey && (event.key === 'ArrowUp' || event.key === 'ArrowDown')))) return false;
      return true;
    });
    const observer = new ResizeObserver(resize);
    observer.observe(host.current);
    const detach = () => { live = false; stopPing(); send({ type: 'detach' }); if (socket) socket.close(); };
    const pagehide = () => { stopped = true; clearTimeout(timer); clearTimeout(handshakeTimer); detach(); };
    const pageshow = (event) => { if (event.persisted) setAttempt((n) => n + 1); };
    window.addEventListener('pagehide', pagehide);
    window.addEventListener('pageshow', pageshow);
    setConnection('Connecting');
    resize();
    connect();
    return () => {
      stopped = true; clearTimeout(timer); clearTimeout(handshakeTimer); detach(); observer.disconnect(); data.dispose(); terminal.dispose();
      window.removeEventListener('pagehide', pagehide); window.removeEventListener('pageshow', pageshow); control.current = null;
    };
  }, [id, attempt]);

  return html`<div class="term-page">
    <div class="term-bar" onClick=${refocus}>
      <button type="button" class="icon-btn" title="Back (⌘[)" onClick=${back}><${Icon} name="back" /></button>
      <button type="button" class=${`icon-btn${switcher ? ' on' : ''}`} title="Agents (⌘\\)" aria-label="Agents"
        aria-pressed=${switcher} onClick=${toggleSwitcher}><${Icon} name=${switcher ? 'fold' : 'unfold'} /></button>
      ${agent ? html`<${Ring} percent=${agent.context_percent} />` : null}
      <span class="t">${agent ? agent.name : id}</span><span class="sub term-state">${agent ? agent.state : ''}</span>
      <span class=${`term-connection ${connection === 'Live' ? 'green' : ''}`} role="status">${connection}</span>
      ${connection === 'Live' && route.label
        ? html`<span class="term-route" title="Route and round-trip time">${routeText(route.label, route.samples)}</span>` : null}
      ${showKeys ? html`<span class="term-keys">${keys.map(([label, key]) => html`<button type="button" class="btn sm" disabled=${connection !== 'Live'}
        onClick=${() => control.current?.key(key)}>${label}</button>`)}
        <button type="button" class="btn sm" disabled=${connection !== 'Live'} onClick=${() => control.current?.model()}>/model</button></span>` : null}
      <span class="sp"></span>
      ${agent?.remote_control?.url?.startsWith('https://claude.ai/code/') ? html`<button type="button" class="btn sm" onClick=${() => openInClaude(agent)}>Open in Claude</button>` : null}
      <button type="button" class="btn sm" onClick=${() => openPanel(`agent:${id}`)}>Details</button>
    </div>
    <div class=${`term-main${switcher ? ' with-switch' : ''}`}>
      ${switcher ? html`<div class="term-switch-wrap" onClick=${refocus}><${Switcher} groups=${groups} currentId=${id}
        idleOpen=${idleOpen} toggleIdle=${toggleIdle} pick=${pick} reload=${reload} /></div>` : null}
      <div class="term-body" ref=${host}></div>
    </div>
    ${connection !== 'Live' ? html`<div class="term-notice" role="status">${error || connection}
      ${connection === 'Ended' ? html`<button type="button" class="btn" onClick=${() => setAttempt((n) => n + 1)}>Reconnect</button>` : null}</div>` : null}
    ${open && Renderer ? html`<aside class="panel term-panel"><${Renderer} id=${id} controls=${html`<span class="ctl">
      <button type="button" class="icon-btn" title="Close details" onClick=${closePanel}><${Icon} name="close" size="14" /></button></span>`} /></aside>` : null}
  </div>`;
}
