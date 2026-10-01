// Ticket Start uses the server-rendered name and brief, shared with the phone.
import { useEffect, useState } from 'preact/hooks';
import { html, api, Popover, Seg, homeRelative, toast, openPanel } from './ui.js';
import { EFFORTS } from './start.js';

export function startBody(ticket, form) {
  const body = { repo: ticket.repo, number: ticket.number, provider: form.provider, name: form.name, brief: form.brief };
  if (ticket.state === 'blocked') body.start_blocked = true;
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
  const blockers = (ticket.waits_on || []).filter((item) => item.state !== 'done').map((item) => `#${item.number}`);
  return html`<${Popover} onClose=${onClose} className="ticket-start">
    <h2>Start #${ticket.number}</h2>
    <p class="sub">${ticket.title}</p>
    ${ticket.state === 'blocked' ? html`<p>#${ticket.number} waits on ${blockers.join(', ')}, which ${blockers.length === 1 ? 'is' : 'are'} not done.</p>` : null}
    ${form ? html`
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
    ` : !error ? html`<p>Loading…</p>` : null}
    ${error ? html`<p class="err" role="alert">${error}</p>` : null}
    <div class="row"><button class="btn" disabled=${busy} onClick=${onClose}>Cancel</button>
      <button class="btn pri" disabled=${!form || busy} onClick=${start}>${busy ? 'Starting…' : ticket.state === 'blocked' ? 'Start anyway' : 'Start'}</button></div>
  <//>`;
}
