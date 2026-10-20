// Plain shell presentation and owner actions (plain_terminal_memo, appendix C).
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, age, basename, homeRelative, navigate, Popover, Icon, toast } from './ui.js';

export const isShell = row => row?.kind === 'shell';
// The server allows this many characters in a shell name.
export const SHELL_NAME_MAX = 40;
/** A tile's title: the owner's name, else the folder. */
export const shellTitle = shell => shell.named ? shell.name : basename(shell.working_dir || shell.repo) || shell.name;
export function shellFact(shell, now = Date.now()) {
  const fact = shell.facts?.agent;
  const text = shell.facts?.text || fact?.text;
  return { text: text || `last input ${age(fact?.since || shell.last_activity, now) || 'now'}`, tone: 'muted' };
}
function ShellTile({ shell, now }) {
  const folder = shell.working_dir || shell.repo;
  const fact = shellFact(shell, now).text;
  const detail = fact.startsWith('running') ? fact.replace(/^running/, 'Running') : fact.replace(/^last input/, 'Last used');
  const href = `/terminal/${encodeURIComponent(shell.id)}`;
  return html`<a class="terminal-tile" href=${href}
    onClick=${event => { if (!event.metaKey && !event.ctrlKey && !event.shiftKey) { event.preventDefault(); navigate(href); } }}>
    <span class="terminal-tile-icon"><${Icon} name="terminal" size="22" /></span>
    <span class="terminal-tile-content"><strong>${shellTitle(shell)}</strong>
      <span class="terminal-tile-path" title=${folder}>${homeRelative(folder)}</span>
      <span class="terminal-tile-fact">${detail}</span></span>
    <${Icon} name="unfold" size="16" />
  </a>`;
}
/** Open shells as tiles. Hidden ones live behind the top bar's eye (HiddenShells). */
export function ShellTiles({ shells, now }) {
  const shown = shells.filter(shell => !shell.hidden);
  if (!shown.length) return null;
  return html`<section class="terminal-section" aria-label="Terminals">
    <div class="terminal-section-heading"><h2>Terminals</h2><span>${shown.length} open</span></div>
    <div class="terminal-tiles">${shown.map(shell => html`<${ShellTile} key=${shell.id} shell=${shell} now=${now} />`)}</div>
  </section>`;
}
/** Live hidden shells, by name. */
export const hiddenShells = sessions => (sessions || [])
  .filter(row => isShell(row) && row.hidden && row.state !== 'stopped' && row.attention?.section !== 'stopped')
  .sort((a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id));

/** The top bar's eye: only while a shell is hidden. Lists them to open or unhide. */
export function HiddenShells({ shells, currentId, onChanged, now }) {
  const [open, setOpen] = useState(false);
  if (!shells?.length) return null;
  const label = `${shells.length} hidden terminal${shells.length === 1 ? '' : 's'}`;
  const go = shell => { setOpen(false); navigate(`/terminal/${encodeURIComponent(shell.id)}`); };
  return html`<span class="anchor hidden-shells">
    <button type="button" class=${`icon-btn hidden-shells-btn${open ? ' on' : ''}`} data-pop-anchor title=${label} aria-label=${label}
      aria-expanded=${open} onClick=${() => setOpen(!open)}><${Icon} name="eye-off" />
      <span class="hidden-shells-count" aria-hidden="true">${shells.length}</span></button>
    ${open ? html`<${Popover} align="right" onClose=${() => setOpen(false)} className="hidden-shells-pop">
      <div class="hidden-shells-head">Hidden terminals</div>
      ${shells.map(shell => html`<div key=${shell.id} class=${`hidden-shell${shell.id === currentId ? ' cur' : ''}`}>
        <button type="button" class="hidden-shell-open" onClick=${() => go(shell)}>
          <span class="hidden-shell-name">${shell.name}</span>
          <span class="hidden-shell-fact">${shellFact(shell, now).text}</span></button>
        <button type="button" class="btn sm" onClick=${() => setShellHidden(shell, false, () => onChanged?.())}>Unhide</button>
      </div>`)}
    <//>` : null}
  </span>`;
}
export function shellError(error) {
  if (error.message === 'plain_disabled') return 'Plain shells are disabled in Settings → Terminals.';
  if (error.message === 'no_ssh_key') return 'Add an SSH public key for this device in Settings → Devices & access.';
  if (error.message === 'too_many_terminals') {
    const open = error.payload?.terminals || error.body?.terminals || [];
    return `Too many open shells. Retire one first.${open.length ? ` Open: ${open.map(t => `${t.id} · ${t.cwd}`).join(', ')}` : ''}`;
  }
  if (error.message === 'name_too_long') return `Shell names are at most ${SHELL_NAME_MAX} characters.`;
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
/** PATCH a shell: `{ name }` (blank or null restores the default) or `{ hidden }`. */
export const updateShell = (id, body) => api(`/terminals/${encodeURIComponent(id)}`, { method: 'PATCH', body });
/** Hide or unhide. Hiding offers Undo. `then(hidden, undone)` runs after each change. */
export async function setShellHidden(shell, hidden, then) {
  try { await updateShell(shell.id, { hidden }); }
  catch (error) { toast(shellError(error)); return false; }
  if (!hidden) { toast(`${shell.name} is back in the lists`); then?.(false, false); return true; }
  let undone = false;
  toast(`Hid ${shell.name}. It keeps running.`, null, { ms: 8000, action: { label: 'Undo', run: async () => {
    if (undone) return;
    undone = true;
    try { await updateShell(shell.id, { hidden: false }); then?.(false, true); }
    catch (error) { toast(shellError(error)); }
  } } });
  then?.(true, false);
  return true;
}
/** Saves a name; resolves to the saved name, or null when the save failed. */
async function saveName(shell, text) {
  const name = text.trim();
  if (name === shell.name) return name;
  try {
    const saved = await updateShell(shell.id, { name: name || null });
    toast(name ? `Renamed to ${saved.name}` : `Name reset to ${saved.name}`);
    return saved.name;
  } catch (error) { toast(shellError(error)); return null; }
}

/** The shell's name in the terminal bar. A click edits it in place: Enter or leaving saves, Esc cancels. */
export function ShellName({ shell, editing, setEditing, onSaved }) {
  const [shown, setShown] = useState(null);
  useEffect(() => { setShown(null); }, [shell.name]);
  const name = shown ?? shell.name;
  if (editing) {
    return html`<${ShellNameField} shell=${shell} onDone=${saved => {
      if (saved && saved !== shell.name) { setShown(saved); onSaved?.(); }
      setEditing(false);
    }} />`;
  }
  return html`<button type="button" class="shell-name t" title="Rename" aria-label=${`Rename ${name}`} onClick=${() => setEditing(true)}>
    <span class="shell-name-text">${name}</span><${Icon} name="edit" size="13" /></button>`;
}

// Mounted fresh for each edit, so it always starts from the current name.
function ShellNameField({ shell, onDone }) {
  const [text, setText] = useState(shell.name);
  const [busy, setBusy] = useState(false);
  const input = useRef(null);
  const done = useRef(false);
  useEffect(() => { input.current?.focus(); input.current?.select(); }, []);
  const finish = async (save) => {
    if (done.current) return;
    done.current = true;
    if (!save) { onDone(null); return; }
    setBusy(true);
    const saved = await saveName(shell, text);
    setBusy(false);
    if (saved === null) { done.current = false; requestAnimationFrame(() => input.current?.focus()); return; }
    onDone(saved);
  };
  return html`<input ref=${input} class="shell-name-input" type="text" aria-label="Shell name" maxlength=${SHELL_NAME_MAX}
    placeholder=${`zsh · ${basename(shell.working_dir || shell.repo)}`} value=${text} readOnly=${busy}
    size=${Math.max(12, Math.min(SHELL_NAME_MAX, text.length + 2))}
    onInput=${event => setText(event.target.value)} onBlur=${() => finish(true)}
    onKeyDown=${event => {
      if (event.key === 'Enter') { event.preventDefault(); finish(true); }
      else if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); finish(false); }
    }} />`;
}

function RenameEditor({ shell, onSaved, onClose }) {
  const [text, setText] = useState(shell.name);
  const [busy, setBusy] = useState(false);
  const save = async () => {
    setBusy(true);
    const saved = await saveName(shell, text);
    setBusy(false);
    if (saved === null) return;
    if (saved !== shell.name) onSaved?.();
    onClose();
  };
  return html`<div class="note-edit">
    <input type="text" maxlength=${SHELL_NAME_MAX} aria-label="Shell name" autofocus value=${text} disabled=${busy}
      placeholder=${`zsh · ${basename(shell.working_dir || shell.repo)}`}
      onInput=${(e) => setText(e.target.value)}
      onKeyDown=${(e) => { if (e.key === 'Escape') onClose(); if (e.key === 'Enter') save(); }} />
    <button type="button" class="btn sm" disabled=${busy || text.trim() === shell.name} onClick=${save}>Rename</button>
    <button type="button" class="btn sm" disabled=${busy} onClick=${onClose}>Cancel</button>
  </div>`;
}

async function copyCommand(command) {
  try { await navigator.clipboard.writeText(command); toast('SSH command copied'); }
  catch { toast(command); }
}

/** ⋯ for a shell. `onRename` edits the name where it shows (the terminal bar); without it, Rename… opens an editor here. */
export function ShellMenu({ shell, away = false, small = false, align, onSaved, onRename, onHide }) {
  const [open, setOpen] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [busy, setBusy] = useState(false);
  const close = () => { setOpen(false); setRenaming(false); };
  const action = async fn => {
    setBusy(true);
    try { await fn(); setOpen(false); }
    catch (error) { toast(shellError(error)); }
    finally { setBusy(false); }
  };
  const live = shell.state !== 'stopped';
  return html`<span class="anchor"><button type="button" class=${small ? 'btn sm' : 'btn'} data-pop-anchor aria-label="More"
    onClick=${() => open ? close() : setOpen(true)}>⋯</button>
    ${open && renaming ? html`<${Popover} key="rename" align=${align} onClose=${close}><${RenameEditor} shell=${shell} onSaved=${onSaved} onClose=${close} /><//>` : null}
    ${open && !renaming ? html`<${Popover} align=${align} onClose=${close} className="menu">
      ${live ? html`<button type="button" onClick=${() => { if (onRename) { close(); onRename(); } else setRenaming(true); }}>Rename…</button>
        <button type="button" disabled=${busy} onClick=${() => action(async () => {
          if (onHide) onHide(!shell.hidden);
          else await setShellHidden(shell, !shell.hidden, () => onSaved?.());
        })}>${shell.hidden ? 'Unhide' : 'Hide'}</button>` : null}
      <button type="button" disabled=${busy} onClick=${() => action(() => createShell({ cwd: shell.working_dir || shell.repo }))}>New shell here</button>
      <button type="button" disabled=${busy} onClick=${() => action(async () => {
        const route = await homeShellRoute(shell.id);
        await copyCommand(away ? awayShellCommand(route) : route.command);
      })}>Copy ssh command</button>
    <//>` : null}</span>`;
}

/** Closing ends the shell for good, so it reads like Retire and asks first. */
export function ShellCloseButton({ shell, onClosed, small = true }) {
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
    <button type="button" class=${`btn danger${small ? ' sm' : ''}`} aria-label="Close shell" ref=${closeButton} onClick=${() => setAsking(true)} disabled=${busy}>Close</button>
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
