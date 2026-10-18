import { useState } from 'preact/hooks';
import { html, api, usePoll, config, ConfirmButton, bus, newAgent, LinkedText, linkifyHtml } from './ui.js';
import { safeThreadHtml } from './inbox.js';
import { artifactReport, artifactBytes, storageSize as size, diskName, needsDecision, needsAgentFollowup, artifactInventory, largestArtifacts, artifactSummary, backupSummary, backupUpdate, backupLabel, backupCard } from './artifacts-model.js';

function Row({ row: a, open, select }) {
  return html`<button class="artifact-row" onClick=${() => select(a)} aria-expanded=${open}>
    <span class="artifact-row-heading"><strong>${a.note || a.name.replaceAll('_',' ')}</strong><span>${size(artifactBytes(a))}</span></span>
    <small class=${needsDecision(a) ? 'artifact-warning' : ''}>${artifactSummary(a)}</small>
  </button>`;
}
function Detail({ a, data, close, act, investigate, busy }) {
  const dependents = (a.dependents || []).map(d => d.name).join(', ');
  const prompt = `${a.name}: ${size(artifactBytes(a))}${!a.rebuildable ? ' · no complete rebuild recipe recorded' : ''}${dependents ? ` · input of ${dependents}` : ''}.`;
  return html`<section class="artifact-detail" aria-label="Folder details">
    <div class="artifact-detail-heading"><h2>${a.name}</h2><button class="btn sm" onClick=${close} aria-label="Close details">Close</button></div>
    <p class="artifact-muted">${size(artifactBytes(a))} · ${artifactSummary(a)}</p>
    ${a.description_html ? html`<div class="artifact-markdown" dangerouslySetInnerHTML=${{__html:linkifyHtml(safeThreadHtml(a.description_html))}} />` : html`<p class="artifact-description"><${LinkedText} text=${a.description || a.note} /></p>`}
    ${a.state === 'unrecorded' ? html`<p>An agent can recover the context from git, saved files and job history. You do not need to reconstruct it.</p>` : null}
    ${(a.consumers || []).length ? html`<p>Used by ${a.consumers.map(c => c.ref).join(', ')}</p>` : null}
    <p class="artifact-muted">${backupLabel(a)}</p>
    ${a.report ? html`<div class="artifact-markdown">${a.report_html ? html`<div dangerouslySetInnerHTML=${{__html:linkifyHtml(safeThreadHtml(a.report_html))}} />` : html`<${LinkedText} text=${artifactReport(a)} />`}</div>` : null}
    ${data.can_act ? html`<fieldset disabled=${busy}><legend>Owner actions</legend>
      ${!a.investigating ? html`<button class="btn artifact-primary" onClick=${() => investigate(a)}>Ask an agent to investigate</button>` : html`<p>Agent investigating</p>`}
      ${a.state === 'frozen' ? html`<${ConfirmButton} label="Restore" confirmLabel="Restore" className="btn sm" prompt=${`Restore ${prompt}`} onConfirm=${() => act(a,'thaw')} />` : null}
      ${['live','frozen'].includes(a.state) ? html`<${ConfirmButton} label=${`Keep ${data.keep_live_days || 30} days`} confirmLabel="Keep available" className="btn sm" prompt=${`Keep available: ${prompt}`} onConfirm=${() => act(a,'keep_live')} />` : null}
      ${a.state !== 'deleted' ? html`<${ConfirmButton} label="Delete folder…" confirmLabel="Delete folder" className="btn sm danger" prompt=${`Delete ${prompt}`} onConfirm=${() => act(a,'delete')} />` : null}
    </fieldset>` : null}
    <details><summary>Technical details</summary><p>${a.root}/${a.name}</p>${a.size_error ? html`<p>Measurement: ${a.size_error}</p>` : null}<p>Last used: ${a.last_used_at || 'Not recorded'}</p>
      <h3>Recipe</h3><pre>${JSON.stringify(a.jobs || [],null,2)}</pre><h3>Recent events</h3>
      <ul>${(a.events || []).slice(0,10).map(e=>html`<li>${e.at} · ${e.kind} · ${e.detail_json}</li>`)}</ul>
    </details>
  </section>`;
}
function BackupCard({ backup }) {
  const [night,setNight] = useState(null);
  const card = backupCard(backup);
  if (!card) return html`<p class="artifact-warning">${backupSummary(backup)}</p>`;
  const update = backupUpdate(backup);
  return html`<section class="artifact-backup" aria-label="Backup">
    <div class="artifact-disk-label"><h2>Backup</h2><span>${backupSummary(backup)}</span></div>
    <div class="artifact-nights" aria-label="Last 7 nights">${card.nights.map(n=>html`<button class=${n.ok === false ? 'bad' : n.ok ? 'ok' : ''} title=${n.detail} aria-label=${n.detail} onClick=${()=>setNight(night === n ? null : n)}>${n.mark}</button>`)}</div>
    ${night ? html`<small class=${night.ok === false ? 'artifact-warning' : 'artifact-muted'}>${night.detail}</small>` : null}
    <p class="artifact-muted">${card.lastNight}</p>
    <p class=${card.checkOk ? 'artifact-muted' : 'artifact-warning'}>${card.check}</p>
    ${card.disk ? html`<strong>${card.disk.line}</strong>
      <meter aria-label="Remote disk used space" min="0" max=${card.disk.total} value=${card.disk.used}/>
      <p class="artifact-muted">${card.disk.holding}</p>
      ${card.disk.pruned ? html`<p class="artifact-muted">${card.disk.pruned}</p>` : null}
      <p class=${card.disk.failing ? 'artifact-warning' : 'artifact-muted'}>Drive health: ${card.disk.failing ? html`<b>${card.disk.health}</b>` : card.disk.health}</p>` : html`<p class="artifact-muted">Remote disk: not reported yet</p>`}
    <p class="artifact-muted">${card.rates} · in force ${card.inForce}</p>
    ${update ? html`<p class=${backup.update.ok ? 'artifact-muted' : 'artifact-warning'}>${update}</p>` : null}
  </section>`;
}
export function ArtifactsPage() {
  const [data,error,reload] = usePoll(() => api('/artifacts'),60000);
  const [browse,setBrowse] = useState(false);
  const [query,setQuery] = useState('');
  const [limit,setLimit] = useState(25);
  const [selected,setSelected] = useState(new URLSearchParams(location.search).get('name'));
  const [actionError,setActionError] = useState(null);
  const [busy,setBusy] = useState(false);
  const rows = data?.artifacts || [];
  const inventory = artifactInventory(rows);
  const decisions = inventory.filter(needsDecision);
  const followup = inventory.filter(a => needsAgentFollowup(a) && !needsDecision(a));
  const filtered = artifactInventory(rows,query);
  const unmeasured = inventory.filter(a => artifactBytes(a) == null).length;
  const unrecorded = inventory.filter(a => a.state === 'unrecorded').length;
  const frozen = inventory.filter(a => a.state === 'frozen');
  const current = rows.find(a => (typeof selected === 'string' ? a.name === selected : a.name === selected?.name && a.root === selected?.root));
  const act = async (a,action) => {
    setBusy(true); setActionError(null);
    try { await api(`/artifacts/${encodeURIComponent(a.name)}/owner-action`,{method:'POST',body:{root:a.root,action}}); }
    catch (e) { setActionError(e.message); }
    finally { setBusy(false); reload(); bus.emit('artifacts-changed'); }
  };
  // The owner starts the investigator from the New agent launcher like any
  // other agent, so the agent type is the owner's choice (sm#2262).
  const investigate = async a => {
    setBusy(true); setActionError(null);
    try {
      const b = await api(`/artifacts/${encodeURIComponent(a.name)}/owner-action`,{method:'POST',body:{root:a.root,action:'brief'}});
      newAgent({ workspace: b.working_dir, message: b.brief, name: b.name, provider: b.provider, model: b.model, effort: b.effort });
    } catch (e) { setActionError(e.message); }
    finally { setBusy(false); }
  };
  const renderRows = list => list.map(a=>html`<div key=${a.root+a.name}><${Row} row=${a} select=${setSelected} open=${current === a}/>${current === a ? html`<${Detail} a=${a} data=${data} close=${()=>setSelected(null)} act=${act} investigate=${investigate} busy=${busy}/>` : null}</div>`);
  const shown = browse ? filtered.slice(0,limit) : largestArtifacts(rows);
  const currentShown = shown.includes(current) || (!browse && (decisions.slice(0,5).includes(current) || followup.slice(0,5).includes(current)));
  return html`<div class="content artifacts-scroll"><main class="artifacts-page">
    <link rel="stylesheet" href=${`/assets/artifacts.css?v=${config.build_id}`} />
    <header class="artifact-header"><div><h1>${browse ? 'All artifacts' : 'Artifacts'}</h1><p>Storage & saved work</p></div><button class="btn sm" onClick=${browse ? ()=>setBrowse(false) : reload}>${browse ? 'Overview' : 'Refresh'}</button></header>
    ${error || actionError ? html`<p role="alert">${actionError || error.message}<button class="btn sm" onClick=${reload}>Retry</button></p>` : null}
    ${!data ? html`<p>Loading storage…</p>` : html`
      ${browse ? html`<label class="artifact-search"><span>Find saved work</span><input type="search" placeholder="Search folders, notes or projects" value=${query} onInput=${e=>{setQuery(e.target.value);setLimit(25);}} /></label><p class="artifact-muted">Largest first · unmeasured folders last</p>` : html`
        <section class="artifact-storage" aria-label="Disk usage">${(data.filesystems || []).map(f=>html`<div class="artifact-disk">
          <div class="artifact-disk-label"><h2>${diskName(f)}</h2><span>${f.mounted === false || f.error ? 'Unavailable' : `${size(f.free)} free`}</span></div>
          ${f.bytes > 0 && f.mounted !== false && !f.error ? html`<strong>${size(Math.max(0,f.bytes-f.free))} <span>of ${size(f.bytes)} used</span></strong>
            <meter aria-label=${`${diskName(f)} used space`} min="0" max=${f.bytes} value=${f.bytes-f.free} class=${f.free < f.floor ? 'artifact-low-space' : ''}/>
            ${f.free < f.floor ? html`<small class="artifact-warning">Space is running low</small>` : null}` : null}
        </div>`)}</section>
        <${BackupCard} backup=${data.backup} />
        ${decisions.length ? html`<section><h2 class="artifact-heading">Cleanup to review</h2>${renderRows(decisions.slice(0,5))}${decisions.length > 5 ? html`<button class="btn sm" onClick=${()=>setBrowse(true)}>Browse all ${decisions.length} proposals</button>` : null}</section>` : null}
        <h2 class="artifact-heading">Taking up space</h2><p class="artifact-muted">Largest folders over 1 GB</p>
      `}
      <section aria-label=${browse ? 'All folders' : 'Largest folders'}>${renderRows(shown)}</section>
      ${!shown.length ? html`<p class="artifact-muted">${browse ? 'No matching folders' : 'No measured folders over 1 GB'}</p>` : null}
      ${current && !currentShown ? html`<${Detail} a=${current} data=${data} close=${()=>setSelected(null)} act=${act} investigate=${investigate} busy=${busy}/>` : null}
      ${browse ? filtered.length > limit ? html`<button class="btn sm" onClick=${()=>setLimit(limit+25)}>Show 25 more · ${filtered.length-limit} remaining</button>` : null : html`
        <button class="artifact-browse" onClick=${()=>setBrowse(true)}>Browse all ${inventory.length} folders <span>›</span></button>
        <section class="artifact-background"><h2 class="artifact-heading">Managed in the background</h2>
          ${followup.length ? html`<p>${followup.length} storage issues need agent follow-up.</p>${renderRows(followup.slice(0,5))}` : null}
          ${frozen.length ? html`<p>${frozen.length} compressed folders · ${size(frozen.reduce((n,a)=>n+(artifactBytes(a)||0),0))}</p>` : null}
          ${unrecorded ? html`<p>${unrecorded} folders need an agent’s record. This is bookkeeping, not a decision for you.</p>` : null}
          ${unmeasured ? html`<p>${unmeasured} folder sizes unavailable. The list above is incomplete until measured.</p>` : null}
          <p>Keep work you still need. Agents check its history and dependencies before proposing cleanup.</p>
        </section>
      `}
    `}
  </main></div>`;
}
