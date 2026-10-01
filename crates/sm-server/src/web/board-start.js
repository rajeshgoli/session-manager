// Ticket Start uses the server-rendered name and brief, shared with the phone.
import { useEffect, useState } from 'preact/hooks';
import { html, api, Popover, Seg, homeRelative, toast, openPanel } from './ui.js';
import { EFFORTS } from './start.js';
import { ReviewerEditor, reviewerText, switchKind } from './reviews.js';

export const REVIEWER_KINDS = [{ value: '', label: 'Lane default' }, { value: 'github_codex', label: 'GitHub' },
  { value: 'codex', label: 'Codex run' }, { value: 'claude', label: 'Claude run' }, { value: 'paired', label: 'Paired' }];

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
  if (form.reviewer) body.reviewer = form.reviewer;
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
      <${ReviewerRow} form=${form} set=${set} />
    ` : startable && !error ? html`<p>Loading…</p>` : null}
    ${error ? html`<p class="err" role="alert">${error}</p>` : null}
    <div class="row"><button class="btn" disabled=${busy} onClick=${onClose}>${startable ? 'Cancel' : 'Close'}</button>
      ${startable ? html`<button class="btn pri" disabled=${!form || busy} onClick=${start}>${busy ? 'Starting…' : blocked ? 'Start anyway' : 'Start'}</button>` : null}</div>
  <//>`;
}

/** The reviewer for this ticket (1768 I3): the policy it would use, or a ticket policy stored at Start. */
function ReviewerRow({ form, set }) {
  const resolved = form.review_policy;
  const own = resolved && resolved.source && resolved.source.startsWith('ticket');
  const kinds = REVIEWER_KINDS.map((k) => (k.value === '' && own ? { ...k, label: 'Ticket\'s own' } : k));
  const kind = form.reviewer ? form.reviewer.kind : '';
  const choose = (value) => set({ reviewer: value ? switchKind(form.reviewer || resolved?.resolved, value) : null });
  return html`<div class="fld top"><span class="l">Reviewer</span><div class="review-editor">
    <${Seg} label="Reviewer for this ticket" value=${kind} options=${kinds} onChange=${choose} />
    ${form.reviewer ? html`<${ReviewerEditor} value=${form.reviewer} kinds=${null} onChange=${(reviewer) => set({ reviewer })}
      note=${form.reviewer.kind === 'paired' ? `Starts at the first review request, in ${form.name}'s checkout, as ${form.name}-reviewer. It may build and run tests, never edit.` : null} />`
      : html`<p class="sub">${resolved ? `${reviewerText(resolved.resolved)} · from ${resolved.source}` : 'The policy a review request would use.'}</p>`}
  </div></div>`;
}
