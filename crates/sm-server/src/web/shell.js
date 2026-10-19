// Plain shell presentation and owner actions (plain_terminal_memo, appendix C).
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, age, basename, homeRelative, navigate, Popover, Icon, toast } from './ui.js';

export const isShell = row => row?.kind === 'shell';
export function shellFact(shell, now = Date.now()) {
  const fact = shell.facts?.agent;
  const text = shell.facts?.text || fact?.text;
  return { text: text || `last input ${age(fact?.since || shell.last_activity, now) || 'now'}`, tone: 'muted' };
}
export function ShellTiles({ shells, now }) {
  if (!shells.length) return null;
  return html`<section class="terminal-section" aria-label="Terminals">
    <div class="terminal-section-heading"><h2>Terminals</h2><span>${shells.length} open</span></div>
    <div class="terminal-tiles">${shells.map(shell => {
      const folder = shell.working_dir || shell.repo;
      const fact = shellFact(shell, now).text;
      const detail = fact.startsWith('running') ? fact.replace(/^running/, 'Running') : fact.replace(/^last input/, 'Last used');
      return html`<a key=${shell.id} class="terminal-tile" href=${`/terminal/${encodeURIComponent(shell.id)}`}
        onClick=${event => { if (!event.metaKey && !event.ctrlKey && !event.shiftKey) { event.preventDefault(); navigate(`/terminal/${encodeURIComponent(shell.id)}`); } }}>
        <span class="terminal-tile-icon"><${Icon} name="terminal" size="22" /></span>
        <span class="terminal-tile-content"><strong>${basename(folder) || shell.name}</strong>
          <span class="terminal-tile-path" title=${folder}>${homeRelative(folder)}</span>
          <span class="terminal-tile-fact">${detail}</span></span>
        <${Icon} name="unfold" size="16" />
      </a>`;
    })}</div>
  </section>`;
}
export function shellError(error) {
  if (error.message === 'plain_disabled') return 'Plain shells are disabled in Settings → Terminals.';
  if (error.message === 'no_ssh_key') return 'Add an SSH public key for this device in Settings → Devices & access.';
  if (error.message === 'too_many_terminals') {
    const open = error.payload?.terminals || error.body?.terminals || [];
    return `Too many open shells. Retire one first.${open.length ? ` Open: ${open.map(t => `${t.id} · ${t.cwd}`).join(', ')}` : ''}`;
  }
  return error.message;
}
export async function createShell(body) {
  const result = await api('/terminals', { method: 'POST', body });
  navigate(`/terminal/${encodeURIComponent(result.id)}`);
  return result;
}
// Reading home metadata never opens the tunnel. Open tunnel explicitly asks for away.
export const homeShellRoute = id => api(`/terminals/${encodeURIComponent(id)}/route`, { method: 'POST', body: { where: 'home' } });
// Keep the server's tmux arguments and quoting, including its configured socket.
// The studio-away alias supplies the SSH user, port and ProxyCommand.
export function awayShellCommand(route) {
  const command = route?.command || '';
  const tmux = command.indexOf(' tmux ');
  return tmux < 0 ? '' : `ssh -t studio-away${command.slice(tmux)}`;
}
async function copyCommand(command) {
  try { await navigator.clipboard.writeText(command); toast('SSH command copied'); }
  catch { toast(command); }
}

export function ShellMenu({ shell, away = false, small = false, align }) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const action = async fn => {
    setBusy(true);
    try { await fn(); setOpen(false); }
    catch (error) { toast(shellError(error)); }
    finally { setBusy(false); }
  };
  return html`<span class="anchor"><button type="button" class=${small ? 'btn sm' : 'btn'} data-pop-anchor aria-label="More"
    onClick=${() => setOpen(!open)}>⋯</button>
    ${open ? html`<${Popover} align=${align} onClose=${() => setOpen(false)} className="menu">
      <button type="button" disabled=${busy} onClick=${() => action(() => createShell({ cwd: shell.working_dir || shell.repo }))}>New shell here</button>
      <button type="button" disabled=${busy} onClick=${() => action(async () => {
        const route = await homeShellRoute(shell.id);
        await copyCommand(away ? awayShellCommand(route) : route.command);
      })}>Copy ssh command</button>
    <//>` : null}</span>`;
}

export function ShellCloseButton({ shell, onClosed }) {
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const closeButton = useRef(null);
  const keepButton = useRef(null);
  useEffect(() => {
    if (!asking) return;
    keepButton.current?.focus();
    const escape = event => { if (event.key === 'Escape') { event.preventDefault(); setAsking(false); closeButton.current?.focus(); } };
    window.addEventListener('keydown', escape);
    return () => window.removeEventListener('keydown', escape);
  }, [asking]);
  const close = async () => {
    setBusy(true);
    try {
      await api(`/terminals/${encodeURIComponent(shell.id)}`, { method: 'DELETE' });
      toast('Shell closed');
      onClosed();
    } catch (error) { toast(shellError(error)); }
    finally { setBusy(false); }
  };
  return html`<span class="retire-action shell-close" onClick=${event => event.stopPropagation()}>
    <button type="button" class="btn sm" aria-label="Close shell" ref=${closeButton} onClick=${() => setAsking(true)} disabled=${busy}>
      <${Icon} name="close" size="14" /> Close</button>
    ${asking ? html`<span class="retire-confirm shell-close-confirm" role="alertdialog" aria-label="Close this shell?">
      <strong>Close this shell?</strong><span>Any processes running in it will stop.</span>
      <span class="row"><button type="button" class="btn sm" ref=${keepButton} disabled=${busy} onClick=${() => { setAsking(false); closeButton.current?.focus(); }}>Keep open</button>
        <button type="button" class="btn sm danger" disabled=${busy} onClick=${close}>${busy ? 'Closing…' : 'Close shell'}</button></span>
    </span>` : null}
  </span>`;
}

export function ShellAwayPanel({ id, onRetry, directError }) {
  const [command, setCommand] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [opened, setOpened] = useState(false);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setError('');
    homeShellRoute(id).then(route => { if (current) setCommand(awayShellCommand(route)); })
      .catch(error => { if (current) setError(shellError(error)); });
    return () => { current = false; };
  }, [id, attempt]);
  const open = async () => {
    setBusy(true); setError(''); setOpened(false);
    try {
      const route = await api(`/terminals/${encodeURIComponent(id)}/route`, { method: 'POST', body: { where: 'away' } });
      setCommand(route.command); setOpened(true);
    } catch (error) { setError(shellError(error)); }
    finally { setBusy(false); }
  };
  return html`<section class="shell-away" aria-label="Shell SSH access">
    <div class="shell-connect-card">
      <span class="shell-connect-icon"><${Icon} name="terminal" size="24" /></span>
      <h2>Connect to your shell</h2>
      <p>On the Studio or your home network, you can use this browser.</p>
      <button type="button" class="btn pri shell-connect-browser" onClick=${onRetry}>Connect in browser</button>
      <p class="shell-connect-hint" role="status">${directError || 'The local connection isn’t available yet.'}${' '}
        Check that local network access is allowed for this site, then try again.</p>
      <div class="shell-connect-ssh">
        <h3>Away from home?</h3>
        <p>Open the SSH tunnel, then run this command in your terminal.</p>
        ${command ? html`<div class="shell-command"><pre>${command}</pre>
          <button type="button" class="btn sm" aria-label="Copy SSH command" onClick=${() => copyCommand(command)}>Copy</button></div>`
          : html`<p class="sub">${error ? 'SSH command unavailable.' : 'Loading SSH command…'}</p>`}
        <div class="row"><button type="button" class="btn" disabled=${busy} onClick=${open}>${busy ? 'Opening tunnel…' : 'Open tunnel'}</button>
          ${!command && error ? html`<button type="button" class="btn" onClick=${() => setAttempt(n => n + 1)}>Retry</button>` : null}
          ${opened ? html`<span class="shell-tunnel-open" role="status">Tunnel open</span>` : null}</div>
        ${error ? html`<p class="err" role="alert">${error}</p>` : null}
        <a class="shell-keys-link" href="/settings#devices-access" onClick=${e => { e.preventDefault(); navigate('/settings#devices-access'); }}>Manage device SSH keys</a>
      </div>
    </div>
  </section>`;
}
