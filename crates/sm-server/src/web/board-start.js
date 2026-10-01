// Ticket Start uses the server-rendered name and brief, shared with the phone.
import { useEffect, useState } from 'preact/hooks';
import { html, api, Popover, Seg, homeRelative, toast, openPanel } from './ui.js';
import { EFFORTS } from './start.js';

export const canStartAnyway = (ticket) => ticket.state === 'blocked'
  && !(ticket.warnings || []).some((warning) => ['stale', 'cycle', 'merged_not_closed'].includes(warning));

export function blockedReasons(ticket) {
  const blockers = (ticket.waits_on || []).filter((item) => item.state !== 'done').map((item) => `#${item.number}`);
  const reasons = [];
  if (blockers.length) reasons.push(`#${ticket.number} waits on ${blockers.join(', ')}, which ${blockers.length === 1 ? 'is' : 'are'} not done.`);
  if ((ticket.warnings || []).includes('stale')) reasons.push('GitHub data is stale. Refresh the Board to check this ticket.');
  if ((ticket.warnings || []).includes('cycle')) reasons.push('This ticket is in a dependency cycle. Fix its ticket links before starting.');
  if ((ticket.warnings || []).includes('merged_not_closed')) reasons.push('A PR has merged. Close this ticket on GitHub.');
  if (!reasons.length) reasons.push('This ticket is blocked. Refresh the Board to check why.');
  return reasons;
}

export function startBody(ticket, form) {
  const body = { repo: ticket.repo, number: ticket.number, provider: form.provider, name: form.name, brief: form.brief };
  if (canStartAnyway(ticket)) body.start_blocked = true;
  if (form.model) body.model = form.model;
  if (form.reasoning_effort) body.reasoning_effort = form.reasoning_effort;
  return body;
}

export function providerDefaults(settings, provider) {
  const saved = settings.new_agent[provider === 'claude' ? 'claude' : 'codex'] || {};
  return { provider, model: saved.model || null, reasoning_effort: saved.effort || null };
}

export function TicketStart({ ticket, onClose, onStarted }) {
  const [form, setForm] = useState(null);
  const [settings, setSettings] = useState(null);
  const [models, setModels] = useState([]);
  const [expanded, setExpanded] = useState(false);
  const [preview, setPreview] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  useEffect(() => {
    if (ticket.state === 'blocked' && !canStartAnyway(ticket)) return;
    let alive = true;
    const query = new URLSearchParams({ repo: ticket.repo, number: ticket.number });
    if (ticket.state === 'blocked') query.set('start_blocked', 'true');
    Promise.all([api(`/client/board/start-options?${query}`), api('/client/settings')])
      .then(([options, saved]) => { if (alive) { setForm(options); setSettings(saved); } })
      .catch((e) => alive && setError(e.message));
    return () => { alive = false; };
  }, [ticket.repo, ticket.number]);
  useEffect(() => {
    if (!form) return;
    let alive = true;
    setModels([]);
    const query = new URLSearchParams({ provider: form.provider, working_dir: form.working_dir });
    api(`/client/session-models?${query}`).then((data) => alive && setModels(data.models || []))
      .catch((e) => alive && setError(e.message));
    return () => { alive = false; };
  }, [form && form.provider, form && form.working_dir]);
  const set = (patch) => setForm((prev) => ({ ...prev, ...patch }));
  const start = async () => {
    if (busy) return;
    setBusy(true); setError(null);
    try {
      const result = await api('/client/board/start', { method: 'POST', body: startBody(ticket, form) });
      onClose(); onStarted();
      toast(`Started ${result.name}`, () => openPanel(`agent:${result.session_id}`));
    } catch (e) { setError(e.message); }
    finally { setBusy(false); }
  };
  const choices = form && form.model && !models.includes(form.model) ? [form.model, ...models] : models;
  const blocked = ticket.state === 'blocked';
  const startable = !blocked || canStartAnyway(ticket);
  return html`<${Popover} onClose=${onClose} className="ticket-start">
    <h2>${startable ? 'Start' : 'Blocked'} #${ticket.number}</h2>
    <p class="sub">${ticket.title}</p>
    ${blocked ? blockedReasons(ticket).map((reason) => html`<p>${reason}</p>`) : null}
    ${startable && form ? html`
      <${Seg} label="Provider" value=${form.provider}
        options=${[{ value: 'claude', label: 'Claude' }, { value: 'codex-fork', label: 'Codex' }]}
        onChange=${(provider) => set(providerDefaults(settings, provider))} />
      <div class="line"><span>${form.model || (form.provider === 'claude' ? 'Claude Code default model' : 'Codex default model')} · ${form.reasoning_effort || 'default effort'} · ${homeRelative(form.working_dir)}</span>
        <button class="link-btn" onClick=${() => setExpanded(!expanded)}>${expanded ? 'Less' : 'Change'}</button></div>
      <div class="line"><span>Named ${form.name}</span><button class="link-btn" onClick=${() => setPreview(!preview)}>Preview</button></div>
      ${preview ? html`<pre class="brief-preview">${form.brief}</pre>` : null}
      ${expanded ? html`
        <label class="fld"><span class="l">Model</span><select class="inp" value=${form.model || ''} onChange=${(e) => set({ model: e.target.value || null })}>
          <option value="">Provider default</option>${choices.map((model) => html`<option value=${model}>${model}</option>`)}</select></label>
        <div class="fld"><span class="l">Effort</span><${Seg} label="Effort" value=${form.reasoning_effort || ''}
          options=${[{ value: '', label: 'default' }, ...(EFFORTS[form.provider] || []).map((e) => ({ value: e, label: e }))]}
          onChange=${(value) => set({ reasoning_effort: value || null })} /></div>
        <label class="fld"><span class="l">Name</span><input class="inp" value=${form.name} onInput=${(e) => set({ name: e.target.value })} /></label>
        <div class="fld"><span class="l">Workspace</span><span class="mono">${homeRelative(form.working_dir)}</span></div>
        <label class="fld top"><span class="l">First message</span><textarea class="inp" rows="6" value=${form.brief} onInput=${(e) => set({ brief: e.target.value })}></textarea></label>` : null}
    ` : startable && !error ? html`<p>Loading…</p>` : null}
    ${error ? html`<p class="err" role="alert">${error}</p>` : null}
    <div class="row"><button class="btn" disabled=${busy} onClick=${onClose}>${startable ? 'Cancel' : 'Close'}</button>
      ${startable ? html`<button class="btn pri" disabled=${!form || busy} onClick=${start}>${busy ? 'Starting…' : blocked ? 'Start anyway' : 'Start'}</button>` : null}</div>
  <//>`;
}
