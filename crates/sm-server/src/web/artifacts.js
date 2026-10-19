import { useState, useEffect, useRef } from 'preact/hooks';
import { html, api, usePoll, config, ConfirmButton, bus, newAgent, LinkedText, linkifyHtml, Icon } from './ui.js';
import { safeThreadHtml } from './inbox.js';
import { artifactReport, artifactBytes, storageSize as size, diskName, needsDecision, needsAgentFollowup, artifactInventory, largestArtifacts, artifactSummary, backupSummary, backupUpdate, backupLabel, backupCard,
  opKind, OP_STEPS, opStep, opStatus, opProgress, actorLabel, actorId, clock, opName, opSizes, opSaved, ratio, savingsLine, placementLabel, thawTime, span } from './artifacts-model.js';

/** One freeze, thaw or move: what, how far along, and who asked (sm#2325). */
function Operation({ op, compact }) {
  const k = opKind(op.kind);
  const step = opStep(op);
  const progress = opProgress(op);
  const settled = op.state === 'done' || op.state === 'failed';
  return html`<article class=${`artifact-op tone-${k.tone} is-${op.state}`} aria-label=${`${settled ? k.past : k.now} ${opName(op)}`}>
    <div class="artifact-op-head">
      <span class="artifact-op-icon"><${Icon} name=${k.icon} size=${18} /></span>
      <div class="artifact-op-title"><small>${settled ? (op.state === 'failed' ? `${k.now} failed` : k.past) : k.now}</small><strong>${opName(op)}</strong></div>
      <span class="artifact-op-size num">${opSizes(op)}</span>
    </div>
    ${compact ? null : html`<ol class="artifact-steps" aria-label="Progress">${OP_STEPS.map((label, i) => html`<li class=${step < 0 ? 'failed' : i < step || (i === step && step === 3) ? 'past' : i === step ? 'now' : ''}>
      <span class="artifact-step-dot"></span><span class="artifact-step-label">${label}</span>
      ${i < 3 ? html`<span class="artifact-step-line"><span style=${`width:${i < step ? 100 : i === 1 && step === 1 && progress != null ? progress * 100 : 0}%`}></span></span>` : null}
    </li>`)}</ol>`}
    <p class=${`artifact-op-status${op.state === 'failed' || (op.error && op.state === 'waiting') ? ' artifact-warning' : ''}`}>${opStatus(op)}</p>
    <p class="artifact-op-meta">Asked by <b>${actorLabel(op)}</b>${actorId(op) ? html` <code>${actorId(op)}</code>` : null} · ${clock(op.created_at)}</p>
  </article>`;
}

/** A finished operation in the history list. */
function HistoryRow({ op }) {
  const k = opKind(op.kind);
  const saved = opSaved(op);
  return html`<li class=${`artifact-history-row tone-${k.tone}${op.state === 'failed' ? ' is-failed' : ''}`}>
    <span class="artifact-op-icon"><${Icon} name=${k.icon} size=${15} /></span>
    <div class="artifact-history-main">
      <strong>${op.state === 'failed' ? `${k.now} failed` : k.past} <span>${opName(op)}</span></strong>
      <small>${actorLabel(op)}${actorId(op) ? html` <code>${actorId(op)}</code>` : null}${span(op.seconds) ? ` · took ${span(op.seconds)}` : ''}${op.state === 'failed' && op.error ? ` · ${op.error}` : ''}</small>
    </div>
    <div class="artifact-history-side num">
      <span>${opSizes(op)}</span>
      <small>${saved ? html`<b>saved ${size(saved)}</b> · ` : null}${clock(op.finished_at)}</small>
    </div>
  </li>`;
}

/** What freezing saved, and what froze, thawed or moved lately. */
function FreezeCard({ activity }) {
  const [all, setAll] = useState(false);
  const s = activity.savings || {};
  const history = activity.history || [];
  const shown = all ? history : history.slice(0, 5);
  const archive = s.data_bytes ? Math.max(2, (s.frozen_bytes / s.data_bytes) * 100) : 0;
  const roots = (s.roots || []).filter(r => r.folders);
  return html`<section class="artifact-freeze" aria-label="Freeze and thaw">
    <div class="artifact-freeze-total">
      <div><small>Freezing saved</small><strong class="num">${size(s.saved_bytes || 0)}</strong></div>
      ${s.data_bytes && s.frozen_bytes ? html`<span class="artifact-ratio num">${ratio(s.data_bytes, s.frozen_bytes)} smaller</span>` : null}
    </div>
    <p class="artifact-muted">${savingsLine(s)}</p>
    ${s.data_bytes ? html`<div class="artifact-freeze-bar" role="img" aria-label=${`Archives ${size(s.frozen_bytes)} of ${size(s.data_bytes)} of data`}>
      <span class="archive" style=${`width:${archive}%`}></span><span class="saved"></span></div>
      <div class="artifact-freeze-legend"><span><i class="archive"></i>Archives ${size(s.frozen_bytes)}</span><span><i class="saved"></i>Saved ${size(s.saved_bytes)}</span></div>` : null}
    ${roots.length > 1 ? html`<ul class="artifact-freeze-roots">${roots.map(r => html`<li><span>${r.root}</span><span class="num">${r.folders} folders · saved ${size(r.saved_bytes)}</span></li>`)}</ul>` : null}
    ${history.length ? html`<h3>Recent freezes, thaws and moves</h3>
      <ul class="artifact-history">${shown.map(op => html`<${HistoryRow} key=${op.id} op=${op} />`)}</ul>
      ${history.length > 5 ? html`<button class="artifact-browse" onClick=${() => setAll(!all)}>${all ? 'Show fewer' : `Show all ${history.length}`} <span>${all ? '‹' : '›'}</span></button>` : null}` : null}
  </section>`;
}

const STATE = { live: 'Live', frozen: 'Frozen', building: 'Building', unrecorded: 'Unrecorded', deleted: 'Deleted' };
/** Where a folder lives and what freezing it saves. */
function StorageFacts({ a, activity }) {
  const op = (activity?.operations || []).find(o => o.names?.includes(a.name) && o.root === a.root && !['done', 'failed'].includes(o.state));
  const state = a.op ? opKind(a.op).now : STATE[a.state] || a.state;
  const frozen = a.state === 'frozen';
  const facts = [
    ['State', state],
    ['Lives on', placementLabel(a)],
    ['Data', a.data_bytes != null ? size(a.data_bytes) : null],
    frozen ? ['Archive', a.frozen_bytes != null ? `${size(a.frozen_bytes)}${ratio(a.data_bytes, a.frozen_bytes) ? ` · ${ratio(a.data_bytes, a.frozen_bytes)} smaller` : ''}` : null] : ['On disk', a.disk_bytes != null ? size(a.disk_bytes) : null],
    frozen ? ['Saved', a.saved_bytes != null ? size(a.saved_bytes) : null] : null,
    frozen ? ['Thaw time', thawTime(a)] : null,
  ].filter(f => f && f[1]);
  const past = (activity?.history || []).filter(o => o.names?.includes(a.name) && o.root === a.root).slice(0, 4);
  return html`<div class="artifact-storage-facts">
    <dl class="artifact-facts">${facts.map(([k, v]) => html`<div class=${k === 'Saved' ? 'good' : ''}><dt>${k}</dt><dd class="num">${v}</dd></div>`)}</dl>
    ${a.placement ? html`<p class="artifact-path"><code>${a.placement.path}</code></p>` : null}
    ${op ? html`<${Operation} op=${op} />` : null}
    ${past.length ? html`<ul class="artifact-history">${past.map(o => html`<${HistoryRow} key=${o.id} op=${o} />`)}</ul>` : null}
  </div>`;
}

function Row({ row: a, open, select }) {
  return html`<button class="artifact-row" onClick=${() => select(a)} aria-expanded=${open}>
    <span class="artifact-row-heading"><strong>${a.note || a.name.replaceAll('_',' ')}</strong><span>${size(artifactBytes(a))}</span></span>
    <small class=${needsDecision(a) ? 'artifact-warning' : ''}>${artifactSummary(a)}</small>
  </button>`;
}
function Detail({ a, data, activity, close, act, investigate, busy }) {
  const dependents = (a.dependents || []).map(d => d.name).join(', ');
  const prompt = `${a.name}: ${size(artifactBytes(a))}${!a.rebuildable ? ' · no complete rebuild recipe recorded' : ''}${dependents ? ` · input of ${dependents}` : ''}.`;
  return html`<section class="artifact-detail" aria-label="Folder details">
    <div class="artifact-detail-heading"><h2>${a.name}</h2><button class="btn sm" onClick=${close} aria-label="Close details">Close</button></div>
    <p class="artifact-muted">${size(artifactBytes(a))} · ${artifactSummary(a)}</p>
    <${StorageFacts} a=${a} activity=${activity} />
    ${a.description_html ? html`<div class="artifact-markdown" dangerouslySetInnerHTML=${{__html:linkifyHtml(safeThreadHtml(a.description_html))}} />` : html`<p class="artifact-description"><${LinkedText} text=${a.description || a.note} /></p>`}
    ${a.state === 'unrecorded' ? html`<p>An agent can recover the context from git, saved files and job history. You do not need to reconstruct it.</p>` : null}
    ${(a.consumers || []).length ? html`<p>Used by ${a.consumers.map(c => c.ref).join(', ')}</p>` : null}
    <p class="artifact-muted">${backupLabel(a)}</p>
    ${a.report ? html`<div class="artifact-markdown">${a.report_html ? html`<div dangerouslySetInnerHTML=${{__html:linkifyHtml(safeThreadHtml(a.report_html))}} />` : html`<${LinkedText} text=${artifactReport(a)} />`}</div>` : null}
    ${data.can_act ? html`<fieldset disabled=${busy}><legend>Owner actions</legend>
      ${!a.investigating ? html`<button class="btn artifact-primary" onClick=${() => investigate(a)}>Ask an agent to investigate</button>` : html`<p>Agent investigating</p>`}
      ${a.state === 'frozen' && !a.op ? html`<${ConfirmButton} label="Thaw" confirmLabel="Thaw" className="btn sm" prompt=${`Thaw ${prompt}`} onConfirm=${() => act(a,'thaw')} />` : null}
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
  if (!card) return null;
  const update = backupUpdate(backup);
  if (card.empty) return html`<p>No confirmed backup yet.</p>${update ? html`<p class=${backup.update.ok ? '' : 'artifact-warning'}>${update}</p>` : null}`;
  return html`<section class="artifact-backup" aria-label="Backup">
    <div class="artifact-disk-label"><h2>Backup</h2><span>${backupSummary(backup)}</span></div>
    ${card.nights.length ? html`<div class="artifact-nights" aria-label="Recent nights">${card.nights.map(n=>html`<button class=${n.ok ? 'ok' : 'bad'} title=${n.detail} aria-label=${n.detail} onClick=${()=>setNight(night === n ? null : n)}>${n.mark}</button>`)}</div>` : null}
    ${night ? html`<small class=${night.ok ? 'artifact-muted' : 'artifact-warning'}>${night.detail}</small>` : null}
    ${card.lastNight ? html`<p class="artifact-muted">${card.lastNight}</p>` : null}
    ${card.check ? html`<p class=${card.checkOk ? 'artifact-muted' : 'artifact-warning'}>${card.check}</p>` : null}
    ${card.disk ? html`<strong>${card.disk.line}</strong>
      <meter aria-label="Remote disk used space" min="0" max=${card.disk.total} value=${card.disk.used}/>
      <p class="artifact-muted">${card.disk.holding}</p>
      ${card.disk.pruned ? html`<p class="artifact-muted">${card.disk.pruned}</p>` : null}
      <p class=${card.disk.failing ? 'artifact-warning' : 'artifact-muted'}>Drive health: ${card.disk.failing ? html`<b>${card.disk.health}</b>` : card.disk.health}</p>` : null}
    ${card.rates ? html`<p class="artifact-muted">${card.rates}</p>` : null}
    ${update ? html`<p class=${backup.update.ok ? 'artifact-muted' : 'artifact-warning'}>${update}</p>` : null}
  </section>`;
}
export function ArtifactsPage() {
  const [data,error,reload] = usePoll(() => api('/artifacts'),60000);
  // Operations poll on their own, fast while one is queued or copying, so a
  // freeze started from the CLI shows queued, copying, then done (sm#2325).
  const [live,setLive] = useState(false);
  const [activity] = usePoll(() => api('/artifact-activity'), live ? 4000 : 20000, [live], { background: true });
  const ops = activity?.operations || [];
  const inFlight = ops.filter(o => o.state === 'running' || o.state === 'waiting');
  useEffect(() => setLive(inFlight.length > 0), [inFlight.length > 0]);
  // A phase change moves folders and disk space: reload the full record.
  const signature = ops.map(o => `${o.id}:${o.phase}`).join(',');
  const seen = useRef(null);
  useEffect(() => { if (seen.current !== null && seen.current !== signature) reload(); seen.current = signature; }, [signature]);
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
  const renderRows = list => list.map(a=>html`<div key=${a.root+a.name}><${Row} row=${a} select=${setSelected} open=${current === a}/>${current === a ? html`<${Detail} a=${a} data=${data} activity=${activity} close=${()=>setSelected(null)} act=${act} investigate=${investigate} busy=${busy}/>` : null}</div>`);
  const shown = browse ? filtered.slice(0,limit) : largestArtifacts(rows);
  const currentShown = shown.includes(current) || (!browse && (decisions.slice(0,5).includes(current) || followup.slice(0,5).includes(current)));
  return html`<div class="content artifacts-scroll"><main class="artifacts-page">
    <link rel="stylesheet" href=${`/assets/artifacts.css?v=${config.build_id}`} />
    <header class="artifact-header"><div><h1>${browse ? 'All artifacts' : 'Artifacts'}</h1><p>Storage & saved work</p></div><button class="btn sm" onClick=${browse ? ()=>setBrowse(false) : reload}>${browse ? 'Overview' : 'Refresh'}</button></header>
    ${error || actionError ? html`<p role="alert">${actionError || error.message}<button class="btn sm" onClick=${reload}>Retry</button></p>` : null}
    ${!data ? html`<p>Loading storage…</p>` : html`
      ${browse ? html`<label class="artifact-search"><span>Find saved work</span><input type="search" placeholder="Search folders, notes or projects" value=${query} onInput=${e=>{setQuery(e.target.value);setLimit(25);}} /></label><p class="artifact-muted">Largest first · unmeasured folders last</p>` : html`
        <section class="artifact-storage" aria-label="Disk usage">${(data.filesystems || []).map(f=>html`<div class="artifact-disk">
          <div class="artifact-disk-label"><h2>${diskName(f)}<small>${f.role === 'cold' ? 'Cold disk' : 'Artifact root'}</small></h2><span>${f.mounted === false || f.error ? 'Unavailable' : `${size(f.free)} free`}</span></div>
          ${f.bytes > 0 && f.mounted !== false && !f.error ? html`<strong class="num">${size(Math.max(0,f.bytes-f.free))} <span>of ${size(f.bytes)} used</span></strong>
            <div class=${`artifact-disk-bar${f.free < f.floor ? ' artifact-low-space' : ''}`} role="meter" aria-label=${`${diskName(f)} used space`} aria-valuemin="0" aria-valuemax=${f.bytes} aria-valuenow=${f.bytes-f.free}>
              <span class="used" style=${`width:${Math.min(100,(f.bytes-f.free)/f.bytes*100)}%`}></span>
              ${f.floor ? html`<span class="floor" style=${`left:${Math.max(0,(f.bytes-f.floor)/f.bytes*100)}%`} title=${`Floor: sm keeps ${size(f.floor)} free`}></span>` : null}
            </div>
            <small class=${f.free < f.floor ? 'artifact-warning' : 'artifact-muted'}>${f.free < f.floor ? `Below its floor: sm keeps ${size(f.floor)} free` : `Floor ${size(f.floor)} · ${size(Math.max(0,f.free-f.floor))} above it`}</small>` : null}
        </div>`)}</section>
        ${!data.backup?.enabled ? html`<p class="artifact-warning">${backupSummary(data.backup)}</p>` : backupCard(data.backup)?.attention ? html`<p class="artifact-warning">${backupCard(data.backup).attention}</p>` : null}
        ${ops.length ? html`<section aria-label="Operations"><h2 class="artifact-heading">${inFlight.length ? `In progress · ${inFlight.length === 1 ? 'one operation' : `${inFlight.length} operations, one at a time`}` : 'Just finished'}</h2>
          <div class="artifact-ops">${ops.map(op => html`<${Operation} key=${op.id} op=${op} />`)}</div></section>` : null}
        ${decisions.length ? html`<section><h2 class="artifact-heading">Cleanup to review</h2>${renderRows(decisions.slice(0,5))}${decisions.length > 5 ? html`<button class="btn sm" onClick=${()=>setBrowse(true)}>Browse all ${decisions.length} proposals</button>` : null}</section>` : null}
        <h2 class="artifact-heading">Taking up space</h2><p class="artifact-muted">Largest folders over 1 GB</p>
      `}
      <section aria-label=${browse ? 'All folders' : 'Largest folders'}>${renderRows(shown)}</section>
      ${!shown.length ? html`<p class="artifact-muted">${browse ? 'No matching folders' : 'No measured folders over 1 GB'}</p>` : null}
      ${current && !currentShown ? html`<${Detail} a=${current} data=${data} activity=${activity} close=${()=>setSelected(null)} act=${act} investigate=${investigate} busy=${busy}/>` : null}
      ${browse ? filtered.length > limit ? html`<button class="btn sm" onClick=${()=>setLimit(limit+25)}>Show 25 more · ${filtered.length-limit} remaining</button>` : null : html`
        <button class="artifact-browse" onClick=${()=>setBrowse(true)}>Browse all ${inventory.length} folders <span>›</span></button>
        ${activity && (activity.savings?.folders || activity.history?.length) ? html`<h2 class="artifact-heading">Freeze & thaw</h2><${FreezeCard} activity=${activity} />` : null}
        <section class="artifact-background"><h2 class="artifact-heading">Managed in the background</h2>
          ${followup.length ? html`<p>${followup.length} storage issues need agent follow-up.</p>${renderRows(followup.slice(0,5))}` : null}
          ${frozen.length && !activity ? html`<p>${frozen.length} frozen folders · ${size(frozen.reduce((n,a)=>n+(artifactBytes(a)||0),0))}</p>` : null}
          ${unrecorded ? html`<p>${unrecorded} folders need an agent’s record. This is bookkeeping, not a decision for you.</p>` : null}
          ${unmeasured ? html`<p>${unmeasured} folder sizes unavailable. The list above is incomplete until measured.</p>` : null}
          <${BackupCard} backup=${data.backup} />
          <p>Keep work you still need. Agents check its history and dependencies before proposing cleanup.</p>
        </section>
      `}
    `}
  </main></div>`;
}
