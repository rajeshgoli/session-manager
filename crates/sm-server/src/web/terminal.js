import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, usePoll, panels, openPanel, closePanel, navigate, Icon, Ring } from './ui.js';
import { openInClaude } from './agents.js';
import './vendor/xterm.js';
import './vendor/addon-fit.js';

const keys = [['Esc', 'escape'], ['Tab', 'tab'], ['Shift+Tab', 'shift-tab'], ['↑', 'up'], ['↓', 'down'], ['Ctrl-C', 'ctrl-c']];
const back = () => history.length > 1 ? history.back() : navigate('/');

export function TerminalPage({ id, open }) {
  const host = useRef(null);
  const control = useRef(null);
  const [connection, setConnection] = useState('Connecting');
  const [error, setError] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [doc] = usePoll(() => api(`/watch/state?session=${encodeURIComponent(id)}`), 10000, [id]);
  const agent = doc && (doc.sessions || []).find((s) => s.id === id);
  const Renderer = panels.get('agent');

  useEffect(() => {
    const terminal = new window.Terminal({
      fontFamily: '"SF Mono", Menlo, monospace', fontSize: 13, cursorBlink: true,
      scrollback: 10000, theme: { background: '#0E0F14', foreground: '#D7DAE3' },
    });
    const fit = new window.FitAddon.FitAddon();
    terminal.loadAddon(fit);
    terminal.open(host.current);
    let socket = null, stopped = false, live = false, timer = null, retries = 0;
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
    };
    const retry = () => {
      if (stopped) return;
      live = false;
      setConnection('Reconnecting');
      timer = setTimeout(connect, Math.min(10000, 500 * 2 ** Math.min(retries++, 5)));
    };
    const connect = async () => {
      if (stopped) return;
      setError('');
      try {
        const ticket = await api(`/client/sessions/${encodeURIComponent(id)}/browser-attach-ticket`, { method: 'POST' });
        if (stopped) return;
        const url = new URL(ticket.ws_url, location.href);
        url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
        const ws = new WebSocket(url);
        socket = ws;
        let ended = false;
        ws.onopen = () => {
          if (stopped) { ws.close(); return; }
          terminal.reset();
          send({ type: 'auth', ticket_id: ticket.ticket_id, ticket_secret: ticket.ticket_secret, output_ack: true });
          resize();
        };
        ws.onmessage = (event) => {
          if (stopped || socket !== ws) return;
          let frame;
          try { frame = JSON.parse(event.data); } catch { return; }
          if (frame.type === 'status' && frame.state === 'attached') {
            live = true; retries = 0; setConnection('Live'); terminal.focus();
          } else if (frame.type === 'output') {
            const bytes = Uint8Array.from(atob(frame.data), (c) => c.charCodeAt(0));
            terminal.write(bytes, () => {
              if (!stopped && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: 'output_ack', sequence: frame.sequence }));
            });
          } else if (frame.type === 'exit' || frame.type === 'error') {
            ended = true; live = false; setConnection('Ended');
            setError(frame.message || frame.reason || '');
            ws.close();
          }
        };
        ws.onclose = (event) => {
          if (stopped || socket !== ws) return;
          live = false;
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
      return true;
    });
    const observer = new ResizeObserver(resize);
    observer.observe(host.current);
    const detach = () => { live = false; send({ type: 'detach' }); if (socket) socket.close(); };
    const pagehide = () => { stopped = true; clearTimeout(timer); detach(); };
    const pageshow = (event) => { if (event.persisted) setAttempt((n) => n + 1); };
    window.addEventListener('pagehide', pagehide);
    window.addEventListener('pageshow', pageshow);
    setConnection('Connecting');
    resize();
    connect();
    return () => {
      stopped = true; clearTimeout(timer); detach(); observer.disconnect(); data.dispose(); terminal.dispose();
      window.removeEventListener('pagehide', pagehide); window.removeEventListener('pageshow', pageshow); control.current = null;
    };
  }, [id, attempt]);

  return html`<div class="term-page">
    <div class="term-bar">
      <button type="button" class="icon-btn" title="Back (⌘[)" onClick=${back}><${Icon} name="back" /></button>
      ${agent ? html`<${Ring} percent=${agent.context_percent} />` : null}
      <span class="t">${agent ? agent.name : id}</span><span class="sub term-state">${agent ? agent.state : ''}</span>
      <span class=${`term-connection ${connection === 'Live' ? 'green' : ''}`} role="status">${connection}</span>
      <span class="term-keys">${keys.map(([label, key]) => html`<button type="button" class="btn sm" disabled=${connection !== 'Live'}
        onClick=${() => control.current?.key(key)}>${label}</button>`)}
        <button type="button" class="btn sm" disabled=${connection !== 'Live'} onClick=${() => control.current?.model()}>/model</button></span>
      <span class="sp"></span>
      ${agent?.remote_control?.url?.startsWith('https://claude.ai/code/') ? html`<button type="button" class="btn sm" onClick=${() => openInClaude(agent)}>Open in Claude</button>` : null}
      <button type="button" class="btn sm" onClick=${() => openPanel(`agent:${id}`)}>Details</button>
    </div>
    <div class="term-body" ref=${host}></div>
    ${connection !== 'Live' ? html`<div class="term-notice" role="status">${error || connection}
      ${connection === 'Ended' ? html`<button type="button" class="btn" onClick=${() => setAttempt((n) => n + 1)}>Reconnect</button>` : null}</div>` : null}
    ${open && Renderer ? html`<aside class="panel term-panel"><${Renderer} id=${id} controls=${html`<span class="ctl">
      <button type="button" class="icon-btn" title="Close details" onClick=${closePanel}><${Icon} name="close" size="14" /></button></span>`} /></aside>` : null}
  </div>`;
}
