import { useEffect, useState } from 'preact/hooks';
import { html, api, Popover, toast, stored } from './ui.js';
import { TypePicker, ConfigFields, claudeDefaults, exactConfig, typeConfig } from './launch-fields.js';

export const keyOf = t => `${t.repo}#${t.number}`;
export const keepEdit = () => ({config:{mode:'keep'},message:{mode:'keep'},behavior:'keep'});
export const distinctTickets = tickets => [...new Map(tickets.map(t => [keyOf(t),t])).values()];
export const defaultSelection = tickets => distinctTickets(tickets).map(({repo,number}) => ({repo,number}));
export function bulkReasons(ticket, goals = new Set()) {
  return [goals.has(keyOf(ticket)) && 'lane goal', ticket.holder && 'held',
    (ticket.prs || []).some(p => (p.state || '').toLowerCase() === 'open') && 'open PR',
    !['ready','blocked'].includes(ticket.state) && ticket.state.replaceAll('_',' '),
    ...(ticket.warnings || []).filter(w => ['stale','cycle','merged_not_closed'].includes(w))].filter(Boolean);
}
const reasonsText = reasons => reasons.map(r => ({lane_goal:'lane goal',not_on_board:'no longer on board',open_pr:'open PR',merged_not_closed:'merged PR; ticket still open',state:'ticket state is not eligible'}[r] || r.replaceAll('_',' '))).join(', ');
function EditFields({ value, onChange, settings }) {
  const set = patch => onChange({...value,...patch});
  const config = value.config.mode === 'set' ? value.config : claudeDefaults(settings);
  return html`<${TypePicker} settings=${settings} other=${false} onPick=${t => set({config:{mode:'set',...typeConfig(t)}})} />
    <label class="fld"><span class="l">Launch configuration</span><select class="inp" value=${value.config.mode} onChange=${e => set({config:e.target.value === 'keep' ? {mode:'keep'} : {mode:'set',...claudeDefaults(settings)}})}><option value="keep">Keep each ticket's current choice</option><option value="set">Set provider, model and effort</option></select></label>
    ${value.config.mode === 'set' ? html`<${ConfigFields} value=${config} onChange=${c => set({config:{mode:'set',...c}})} />` : null}
    <label class="fld"><span class="l">Start behavior</span><select class="inp" value=${value.behavior} onChange=${e => set({behavior:e.target.value})}>
      <option value="keep">Keep current authorization</option><option value="manual">Manual — cancel existing authorization</option><option value="when_ready">Start when ready — authorize or retry</option></select></label>
    <label class="fld"><span class="l">First message</span><select class="inp" value=${value.message.mode} onChange=${e => set({message:e.target.value === 'custom' ? {mode:'custom',text:''} : {mode:e.target.value}})}>
      <option value="keep">Keep each ticket's current message</option><option value="default">Use each ticket's default message</option><option value="custom">Use this exact message</option></select></label>
    ${value.message.mode === 'custom' ? html`<textarea class="inp" aria-label="Shared first message" rows="4" value=${value.message.text} onInput=${e => set({message:{mode:'custom',text:e.target.value}})} />` : null}`;
}
export function SharedLaunchSetup({ tickets, goals, initialPreset, onClose, onSaved }) {
  const chosen = distinctTickets(tickets);
  const [settings,setSettings] = useState(null);
  const [common,setCommon] = useState(keepEdit);
  const [exceptions,setExceptions] = useState({});
  const [preview,setPreview] = useState(null);
  const [requestId,setRequestId] = useState(null);
  const [busy,setBusy] = useState(false);
  const [error,setError] = useState(null);
  const [messages,setMessages] = useState({});
  useEffect(() => { let live=true;api('/client/settings').then(s => {if(!live)return;setSettings(s);if(initialPreset)setCommon({...keepEdit(),config:{mode:'set',...initialPreset}});}).catch(e => live&&setError(e.message));return()=>{live=false;};},[]);
  const change = fn => {fn();setPreview(null);setRequestId(null);setError(null);};
  const draft = () => ({selection:defaultSelection(chosen),common,exceptions:Object.entries(exceptions).map(([key,edit]) => ({...defaultSelection(chosen.filter(t=>keyOf(t)===key))[0],...edit})),last_agent_type:stored('sm-auto-start-type','') || null});
  const review = async () => {setBusy(true);setError(null);try {const p=await api('/client/board/launch-preview',{method:'POST',body:draft()});setPreview(p);setRequestId(crypto.randomUUID());}catch(e){setError(e.message);}finally{setBusy(false);}};
  const save = async () => {if(busy||!preview)return;setBusy(true);setError(null);try{const result=await api('/client/board/launch-selection',{method:'PUT',body:{token:preview.token,request_id:requestId}});onSaved();onClose();toast(`Saved ${result.preferences_count} preferences · authorized ${result.authorized_count} · cancelled ${result.cancelled_count}`);}catch(e){setError(e.message + (e.body?.changed?.length ? ' Changed: '+e.body.changed.map(t=>`${t.repo}#${t.number}`).join(', ') : ''));if(e.status===409){setPreview(null);setRequestId(null);}}finally{setBusy(false);}};
  const showMessage = async t => {try {const options=await api(`/client/board/start-options?${new URLSearchParams({repo:t.repo,number:t.number})}`);setMessages(m=>({...m,[keyOf(t)]:options.brief}));}catch(e){setError(e.message);}};
  const eligible = chosen.filter(t=>!bulkReasons(t,goals).length).length;
  return html`<${Popover} onClose=${busy?()=>{}:onClose} className="ticket-start shared-launch">
    <h2>Shared launch setup</h2><p>${chosen.length} selected · ${preview?preview.eligible_count:eligible} eligible · ${preview?preview.excluded_count:chosen.length-eligible} excluded</p>
    <p class="sub">Applies to this selection only. Future lane defaults and running agents are unchanged.</p>
    ${settings?.new_agent.auto_start_paused ? html`<p class="amber">Auto-start is globally paused. Saving authorization does not resume it.</p>` : null}
    ${settings ? html`<fieldset disabled=${busy}><${EditFields} value=${common} settings=${settings} onChange=${v=>change(()=>setCommon(v))} />
      <h3>Tickets and exceptions</h3>${chosen.map(t=>{const key=keyOf(t),reasons=bulkReasons(t,goals);return html`<details class="launch-exception" key=${key}><summary><b>#${t.number}</b> ${t.title}<span class="sub">${reasons.length?`Excluded: ${reasons.join(', ')}`:exceptions[key]?'Ticket exception':'Uses shared choices'}${t.auto_start?` · ${t.auto_start.state} authorization`:''}</span></summary>
      ${!reasons.length ? html`<button type="button" class="btn sm" onClick=${()=>change(()=>setExceptions(prev=>{const next={...prev};if(next[key])delete next[key];else next[key]=structuredClone(common);return next;}))}>${exceptions[key]?'Use shared choices':'Set ticket exception'}</button>
      ${exceptions[key]?html`<${EditFields} value=${exceptions[key]} settings=${settings} onChange=${v=>change(()=>setExceptions(prev=>({...prev,[key]:v})))} />`:null}
      <button type="button" class="link-btn" onClick=${()=>showMessage(t)}>Preview ticket default message</button>${messages[key]?html`<pre class="brief-preview">${messages[key]}</pre>`:null}`:null}</details>`;})}</fieldset>`:html`<p>Loading settings…</p>`}
    ${preview?html`<section class="launch-preview" aria-label="Selection preview"><h3>Review this change</h3><p>${preview.ready_count} ready · ${preview.blocked_count} blocked · ${preview.renew_failed_count} failed authorizations renewed · ${preview.cancel_count} cancellations</p>
      <p class="sub">Ready authorized tickets can launch at the next recomputation. Armed does not mean queued or running.</p>
      ${preview.items.map(i=>html`<div class="launch-preview-item"><b>${i.repo}#${i.number}</b><span>${i.eligible?exactConfig(i.effective_config):`Excluded: ${reasonsText(i.reasons)}`}</span>${i.eligible?html`<span>${i.behavior==='when_ready'?'Authorize start when ready':i.behavior==='manual'?'Manual; cancel authorization':'Keep authorization'} · ${i.authorization_state || 'no current authorization'}</span><pre>${i.effective_config.brief ?? 'Use ticket default message at launch'}</pre>`:null}</div>`)}</section>`:null}
    ${error?html`<p class="err" role="alert">${error}</p>`:null}
    <div class="row"><button class="btn" disabled=${busy} onClick=${onClose}>Cancel</button><button class="btn" disabled=${busy||!settings||chosen.length>100} onClick=${review}>${preview?'Refresh preview':'Review selection'}</button>
      ${preview?html`<button class="btn pri" disabled=${busy||!preview.eligible_count} onClick=${save}>${busy?'Saving…':preview.items.some(i=>i.eligible&&i.behavior==='when_ready')?`Authorize ${preview.items.filter(i=>i.eligible&&i.behavior==='when_ready').length} tickets`:preview.cancel_count?`Save and cancel ${preview.cancel_count} authorizations`:'Save preferences'}</button>`:null}</div>
    ${chosen.length>100?html`<p class="err">Select at most 100 tickets per change.</p>`:null}
  <//>`;
}
export function LaneLaunchDefault({ lane, onClose, onSaved }) {
  const [settings,setSettings]=useState(null),[saved,setSaved]=useState(null),[config,setConfig]=useState(null),[busy,setBusy]=useState(false),[error,setError]=useState(null);
  useEffect(()=>{Promise.all([api('/client/settings'),api(`/client/board/launch-default?lane_id=${lane.id}`)]).then(([s,d])=>{setSettings(s);setSaved(d);setConfig(d.config||{...claudeDefaults(s),brief:null});}).catch(e=>setError(e.message));},[]);
  const save=async clear=>{setBusy(true);setError(null);try{await api('/client/board/launch-default',{method:'PUT',body:{lane_id:lane.id,expected_revision:saved.revision,config:clear?null:config}});onSaved();onClose();}catch(e){setError(e.message);}finally{setBusy(false);}};
  return html`<${Popover} onClose=${onClose} className="ticket-start shared-launch"><h2>Future lane default</h2><p>${lane.goal.title}</p><p>Only tickets new to the entire board and initially in this lane capture this configuration. Moving an existing ticket here keeps its settings. This never authorizes a start.</p>
    ${config?html`<fieldset disabled=${busy}><${TypePicker} settings=${settings} other=${false} onPick=${t=>setConfig({...config,...typeConfig(t)})} /><${ConfigFields} value=${config} onChange=${c=>setConfig({...config,...c})} />
      <label class="fld"><span class="l">First message</span><select class="inp" value=${config.brief===null?'default':'custom'} onChange=${e=>setConfig({...config,brief:e.target.value==='default'?null:''})}><option value="default">Use each ticket's default</option><option value="custom">Use this exact text</option></select></label>
      ${config.brief!==null?html`<textarea class="inp" rows="4" value=${config.brief} onInput=${e=>setConfig({...config,brief:e.target.value})} />`:null}</fieldset>`:null}
    ${error?html`<p class="err" role="alert">${error}</p>`:null}<div class="row"><button class="btn" onClick=${onClose}>Cancel</button><button class="btn" disabled=${busy||!saved} onClick=${()=>save(true)}>Clear default</button><button class="btn pri" disabled=${busy||!saved} onClick=${()=>save(false)}>Save future default</button></div><//>`;
}
