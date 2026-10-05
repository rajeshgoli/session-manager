// Settings › Worktrees (sm#1987): retired agents' worktrees sm left, and
// the actions of `sm worktree delete` and `sm worktree keep`.
import { useState } from 'preact/hooks';
import { html, api, usePoll, config, toast, homeRelative, ConfirmButton } from './ui.js';

export function size(bytes) {
  if (bytes == null) return 'size unknown';
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
  return `${value >= 10 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

function since(iso) {
  const at = Date.parse(iso || '');
  if (!Number.isFinite(at)) return '';
  const minutes = Math.max(0, Math.floor((Date.now() - at) / 60000));
  if (minutes < 60) return `${minutes}m`;
  if (minutes < 48 * 60) return `${Math.floor(minutes / 60)}h`;
  return `${Math.floor(minutes / 1440)}d`;
}

function Row({ row, act }) {
  const [keeping, setKeeping] = useState(false);
  const [reason, setReason] = useState('');
  const item = row.ticket ? `#${row.ticket}` : row.pr ? `PR #${row.pr}` : '';
  const build = row.build_bytes ? ` · ${size(row.build_bytes)} build output` : '';
  return html`<li>
    <div class="device-description"><strong>${homeRelative(row.path)}</strong>
      <span>${row.repo}${item ? ` ${item}` : ''} · ${size(row.bytes)}${build}${row.retired_at ? ` · retired ${since(row.retired_at)} ago` : ''}</span>
      <span>${row.reason}${(row.sessions || []).length ? ` · ${row.sessions.join(', ')}` : ''}</span>
    </div>
    <div class="worktree-actions">
      ${row.build_bytes ? html`<button type="button" class="btn sm" onClick=${() => act(row, 'build')}>Delete build output</button>` : null}
      <${ConfirmButton} label="Delete" className="btn sm danger" confirmLabel="Delete"
        prompt=${`Delete ${homeRelative(row.path)}? ${row.reason}.`} onConfirm=${() => act(row, 'worktree')} />
      ${row.kept ? html`<button type="button" class="btn sm" onClick=${() => act(row, 'unkeep')}>Stop keeping</button>`
        : keeping ? html`<form class="worktree-keep" onSubmit=${(event) => { event.preventDefault(); if (reason.trim()) act(row, 'keep', reason.trim()); }}>
            <input type="text" placeholder="Why keep it" value=${reason} onInput=${(event) => setReason(event.target.value)} />
            <button type="submit" class="btn sm" disabled=${!reason.trim()}>Keep</button>
            <button type="button" class="btn sm" onClick=${() => setKeeping(false)}>Cancel</button></form>`
        : html`<button type="button" class="btn sm" onClick=${() => setKeeping(true)}>Keep…</button>`}
    </div>
  </li>`;
}

export function LeftoverWorktrees() {
  const [data, loadError, reload] = usePoll(() => api('/worktrees/leftover'), 60000);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);
  const act = async (row, action, reason) => {
    setBusy(true); setError(null);
    const name = homeRelative(row.path);
    try {
      if (action === 'keep' || action === 'unkeep') {
        await api('/worktrees/keep', { method: 'POST', body: { path: row.path, reason, off: action === 'unkeep' } });
        toast(action === 'keep' ? `Keeping ${name}` : `No longer keeping ${name}`);
      } else {
        const done = await api('/worktrees/delete', { method: 'POST', body: { path: row.path, scope: action } });
        toast(done.removed ? `Deleted ${name}${done.rescued ? `; commits on ${done.rescued}` : ''}` : `Deleted build output in ${name}`);
      }
    } catch (failure) {
      setError(`${name}: ${failure.message}`);
    } finally {
      setBusy(false);
      reload();
    }
  };
  const rows = (data && data.worktrees) || [];
  const total = rows.reduce((sum, row) => sum + (row.bytes || 0), 0);
  const build = rows.reduce((sum, row) => sum + (row.build_bytes || 0), 0);
  return html`<section class="device-settings" aria-label="Left-over worktrees" aria-busy=${busy}>
    <link rel="stylesheet" href=${`/assets/devices.css?v=${config.build_id}`} />
    <p class="sub">Retired agents' worktrees sm left. sm deletes one once nothing in it would be lost, checking hourly.
      Delete build output is always safe; Delete drops the folder and uncommitted changes, not commits.</p>
    ${error ? html`<p class="device-error" role="alert">${error}</p>` : null}
    ${loadError ? html`<p class="device-error" role="alert">Could not load: ${loadError.message}
      <button type="button" class="btn sm" onClick=${reload}>Retry</button></p>` : null}
    ${!data && !loadError ? html`<p role="status">Measuring…</p>` : null}
    ${data && !rows.length ? html`<p class="sub">None.</p>` : null}
    ${rows.length ? html`<p>${rows.length} worktree${rows.length === 1 ? '' : 's'} · ${size(total)}${build ? `, ${size(build)} of it build output` : ''}</p>` : null}
    <ul class="device-list">${rows.map((row) => html`<${Row} key=${row.path} row=${row} act=${act} />`)}</ul>
  </section>`;
}
