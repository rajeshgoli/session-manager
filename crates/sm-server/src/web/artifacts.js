import { useState } from 'preact/hooks';
import { html, api, usePoll, config, ConfirmButton, bus } from './ui.js';
import { size } from './worktrees.js';
import { artifactSections, artifactBytes, duration, backupLabel, backupNights, age } from './artifacts-model.js';

function Row({ row: a, canAct, days, act, busy }) {
  const [open, setOpen] = useState(new URLSearchParams(location.search).get('name') === a.name);
  const dependents = (a.dependents || []).map(d => d.name).join(', ');
  const prompt = `${a.name}: ${size(artifactBytes(a))}${!a.rebuildable ? ' · not rebuildable' : ''}${dependents ? ` · input of ${dependents}` : ''}.`;
  return html`<article class="q-card artifact-row">
    <button class="artifact-toggle" onClick=${() => setOpen(!open)} aria-expanded=${open}>
      <strong>${a.name}</strong><span>${size(a.disk_bytes)}${a.state === 'frozen' ? ` → ${size(a.frozen_bytes)}` : ''}</span>
    </button>
    <div>${a.location} · ${a.state}${a.op ? ` · ${a.op}` : ''}${a.gpu_hours || a.gpu_cost_usd ? ' · GPU' : ''}</div>
    <p><em>${a.note || 'No note recorded'}</em></p>
    <div>${(a.consumers || []).map(c => `${c.ref} (${c.state})`).join(' · ') || 'No consumers'} · rebuild ${duration(a.rebuild_seconds)}</div>
    <div>${a.owner_session_name || 'No owner'} · ${age(a.created_at)}</div>
    <div>${backupLabel(a)}</div>
    ${a.backup_ship_bytes != null ? html`<div>Estimated packed size ${size(a.backup_ship_bytes)}</div>` : null}
    ${a.needs_you ? html`<p>${a.needs_you.reasons.join(' · ').replaceAll('_', ' ')}</p>` : null}
    ${a.investigating ? html`<p>investigating · ${a.investigating}</p>` : null}
    ${a.report ? html`<pre>${a.report.detail_json}</pre>` : null}
    ${open ? html`<div class="artifact-detail">
      <p>${a.description}</p><p>Recent use: ${a.last_used_at || 'not recorded'}</p>
      <h3>Recipe</h3><pre>${JSON.stringify(a.jobs || [], null, 2)}</pre>
      <h3>Recent events</h3><ul>${(a.events || []).slice(0,10).map(e => html`<li>${e.at} · ${e.kind} · ${e.detail_json}</li>`)}</ul>
      ${canAct ? html`<fieldset disabled=${busy}><legend>Owner actions</legend>
        <${ConfirmButton} label="Delete" confirmLabel="Delete" className="btn sm danger" prompt=${`Delete ${prompt}`} onConfirm=${() => act(a,'delete')} />
        ${a.state === 'frozen' ? html`<${ConfirmButton} label="Thaw" confirmLabel="Thaw" className="btn sm" prompt=${`Thaw ${prompt}`} onConfirm=${() => act(a,'thaw')} />` : null}
        ${a.state === 'live' || a.state === 'frozen' ? html`<${ConfirmButton} label=${`Keep live ${days} days`} confirmLabel="Keep live" className="btn sm" prompt=${`Keep live ${days} days: ${prompt}`} onConfirm=${() => act(a,'keep_live')} />` : null}
        ${!a.investigating ? html`<button class="btn sm" onClick=${() => act(a,'investigate')}>Investigate</button>` : null}
      </fieldset>` : null}
    </div>` : null}
  </article>`;
}

export function ArtifactsPage() {
  const [data,error,reload] = usePoll(() => api('/artifacts'),60000);
  const [epic,setEpic] = useState('');
  const [actionError,setActionError] = useState(null);
  const [busy,setBusy] = useState(false);
  const rows = data?.artifacts || [];
  const frozen = rows.filter(a => a.state === 'frozen');
  const backup = data?.backup || {};
  const last = backup.runs?.[0];
  const percent = (bytes,total) => Math.max(0, Math.min(100,100*bytes/(total || 1)));
  const act = async (a,action) => {
    setBusy(true); setActionError(null);
    try { await api(`/artifacts/${encodeURIComponent(a.name)}/owner-action`,{method:'POST',body:{root:a.root,action}}); }
    catch (e) { setActionError(e.message); }
    finally { setBusy(false); reload(); bus.emit('artifacts-changed'); }
  };
  return html`<main class="artifacts-page">
    <link rel="stylesheet" href=${`/assets/artifacts.css?v=${config.build_id}`} />
    <h1>Artifacts</h1>
    ${error || actionError ? html`<p role="alert">${actionError || error.message}<button onClick=${reload}>Retry</button></p>` : null}
    ${!data ? html`<p>Loading artifacts…</p>` : html`
      <div class="q-tiles artifact-tiles">
        <section class="q-card"><span class="q-label">Frozen</span><strong>${size(frozen.reduce((n,a)=>n+artifactBytes(a),0))}</strong><small>${frozen.length} folders</small></section>
        <section class="q-card"><span class="q-label">Needs you</span><strong>${data.needs_you_count || 0}</strong><small>folders to inspect</small></section>
        <section class="q-card"><span class="q-label">Last backup</span><strong>${last ? size(last.include_bytes) : '—'}</strong><small>${last ? age(last.started_at) : 'No runs yet'}</small></section>
      </div>
      <div class="artifact-filesystems">${(data.filesystems || []).map(f => html`<section class="q-card"><h2>${f.path}</h2>
        <div class="artifact-meter">
        <meter min="0" max=${f.bytes || 1} value=${(f.bytes || 0)-(f.free || 0)} />
        ${f.mounted !== false ? html`<i title="Floor" class="floor" style=${{left:`${percent(f.bytes-f.floor,f.bytes)}%`}} /><i title="Enough free space" class="enough" style=${{left:`${percent(f.bytes-f.enough,f.bytes)}%`}} />` : null}
        </div>
        <p>${f.mounted === false ? 'Not mounted' : `${size(f.free)} free of ${size(f.bytes)}`}</p>
        <p>Floor ${size(f.floor)} · enough ${size(f.enough)} · outstanding ${size(f.outstanding)}</p>
      </section>`)}</div>
      <section class="q-card"><h2>Backup${backup.enabled === false ? ' · not enabled' : ''}</h2>
        <div class="artifact-nights">${backupNights(backup.runs || []).map(n=>html`<span title=${n.date}>${n.date.slice(5)}<strong>${n.mark}</strong></span>`)}</div>
        <p>${last ? `${size(last.include_bytes)} · duration ${duration((Date.parse(last.finished_at)-Date.parse(last.started_at))/1000)} · ${last.state}` : 'No backup runs recorded.'}</p>
        <dl class="artifact-backup-details"><dt>Confirmed</dt><dd>${backup.confirmed_at || 'Not reported'}</dd><dt>Weekly check</dt><dd>${backup.weekly_check || 'Not reported'}</dd><dt>Backup disk used</dt><dd>${size(backup.disk_used_bytes)}</dd><dt>Holding old versions</dt><dd>${size(backup.holding_bytes)}</dd></dl>
        <p>Scope: ${(backup.scope_roots || []).join(' · ') || 'Not reported'} · skipped ${backup.skipped_count ?? 'not reported'}</p>
        <p>Download: ${backup.fetch_rate || 'unmeasured'} · disk write: ${backup.apply_rate || 'unmeasured'} · restore: ${backup.probe_rate || 'unmeasured'}</p>
        <p>Rate in force: ${backup.effective_rate || `${backup.rate_default || 'unknown'} /s (default)`}</p>
      </section>
      <label>Epic <select value=${epic} onChange=${e => setEpic(e.target.value)}><option value="">All epics</option>${(data.epics || []).map(e => html`<option value=${e.label}>${e.label}</option>`)}</select></label>
      ${artifactSections(data.artifacts || [],epic).map(g => html`<section><h2 class="q-heading">${g.title} · ${g.rows.length} · ${size(g.rows.reduce((n,a)=>n+artifactBytes(a),0))}</h2>${g.rows.map(a => html`<${Row} key=${a.root+a.name} row=${a} canAct=${data.can_act} days=${data.keep_live_days || 30} act=${act} busy=${busy} />`)}</section>`)}
      ${!(data.artifacts || []).length ? html`<p>No artifacts recorded.</p>` : null}
      <footer>Last 7 days: ${(data.recent || []).map(e => `${e.count} ${e.kind}`).join(' · ') || 'no activity'} · ${data.mode}</footer>
    `}
  </main>`;
}
