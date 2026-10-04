// Ticket Start uses the server-rendered name and brief, shared with the phone.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, config, Popover, Seg, Toggle, homeRelative, toast, openPanel, stored, store, trimPageData } from './ui.js';
import { EFFORTS } from './start.js';
import { TypePicker, OTHER_TYPE, agentTypes, typeConfig, matchType } from './launch-fields.js';
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

// Start when ready (1821 F4). Efforts match the server's agent-type check.
export const AUTO_EFFORTS = { claude: ['low', 'medium', 'high', 'xhigh', 'max'], 'codex-fork': ['medium', 'high', 'xhigh'] };
export const CUSTOM = OTHER_TYPE;
export const LAST_TYPE_KEY = 'sm-auto-start-type';

/** "opus[1m]" → "Opus", "claude-sonnet-4-5" → "Sonnet"; null is the provider default. */
export function modelShort(model) {
  if (!model) return 'default';
  if (model.startsWith('gpt-')) return model;
  const word = model.replace(/\[.*\]$/, '').replace(/^claude-/, '').split(/[-_\s]/)[0] || model;
  return word[0].toUpperCase() + word.slice(1);
}

export const typeText = (type) => `${modelShort(type.model)} ${type.effort || 'default'}`;

export { matchType };

export function chipText(auto, paused) {
  const head = auto.state === 'failed' ? '⏵ failed' : paused ? '⏵ paused' : '⏵ when ready';
  return [head, auto.agent_type, typeText({ model: auto.model, effort: auto.effort })].filter(Boolean).join(' · ');
}

/** The PUT /client/board/auto-start item. A null brief renders the template at start time. */
export function autoStartBody(ticket, choice, agentType, brief) {
  const body = { repo: ticket.repo, number: ticket.number, provider: choice.provider };
  if (agentType) body.agent_type = agentType;
  if (choice.model) body.model = choice.model;
  if (choice.reasoning_effort) body.reasoning_effort = choice.reasoning_effort;
  if (brief != null) body.brief = brief;
  return body;
}

const choiceOf = (auto) => ({ provider: auto.provider, model: auto.model, reasoning_effort: auto.effort });
export const retryBody = (ticket) => autoStartBody(ticket, choiceOf(ticket.auto_start), ticket.auto_start.agent_type, ticket.auto_start.brief);

/** A stored choice as the type it equals, else Custom; else the Tier line, else the last type used. */
export function defaultType(ticket, types, last) {
  const auto = ticket.auto_start;
  if (auto) return matchType(types, choiceOf(auto)) || CUSTOM;
  const named = (name) => name && (types.find((t) => t.name.toLowerCase() === name.toLowerCase()) || {}).name;
  return named(ticket.tier) || named(last) || '';
}

/** Open lane tickets nobody started or claimed, except the goal. */
export const laneCandidates = (lane) => (lane.tickets || []).filter((t) => ['blocked', 'ready'].includes(t.state)
  && !t.holder && !(t.warnings || []).includes('merged_not_closed') && !(t.repo === lane.goal.repo && t.number === lane.goal.number));

/** Report a bug (1859 B4): the POST /client/bug-reports body. `bug` is what the press captured. */
export function bugBody(bug, { text, screenshot, startAgent }, form) {
  return {
    text, client: 'web', client_version: config.build_id || null, page: bug.page, route: bug.route,
    page_data: trimPageData(bug.page_data || {}),
    screenshot_png: screenshot && bug.screenshot ? bug.screenshot : null,
    start: startAgent ? { provider: form.provider, model: form.model || null, reasoning_effort: form.reasoning_effort || null, reviewer: form.reviewer || null } : null,
  };
}

/** Board Start on the filed bug, after the server's start failed (decision 9). */
export function bugStartBody(issue, form) {
  return startBody({ repo: issue.repo, number: issue.number }, form);
}

/** The bug dialog's primary button: File bug, File and start, or Start once filed. */
export function bugPrimary({ startAgent, filed, busy }) {
  if (filed) return busy ? 'Starting…' : 'Start';
  if (busy) return startAgent ? 'Filing and starting…' : 'Filing…';
  return startAgent ? 'File and start' : 'File bug';
}

export function filedText(result) {
  return `Filed #${result.issue.number}${result.started ? ` · started ${result.started.name}` : ''}${result.board_note ? ' · not on the board' : ''}`;
}

/** The new-agent provider and its saved defaults. */
const defaultProvider = (settings) => (settings.new_agent.provider || '').startsWith('codex') ? 'codex-fork' : 'claude';

const autoStartPath = (ticket) => `/client/board/auto-start?${new URLSearchParams({ repo: ticket.repo, number: ticket.number })}`;

/** Start, or with mode when_ready, Start when ready (1821 F4): an agent type fills provider, model and effort. */
export function TicketStart({ ticket, mode = 'start', bug = null, onClose, onStarted = () => {} }) {
  if (mode === 'bug') ticket = { repo: null, number: null, state: 'ready' };
  const later = mode === 'when_ready';
  const reporting = mode === 'bug';
  const auto = later && ticket.auto_start;
  const [form, setForm] = useState(null);
  const [settings, setSettings] = useState(null);
  const [models, setModels] = useState([]);
  const [preview, setPreview] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const [edited, setEdited] = useState(false);
  const [report, setReport] = useState({ text: '', screenshot: !!(bug && bug.screenshot), startAgent: false });
  const [filed, setFiled] = useState(null);
  const types = agentTypes(settings);
  const typeName = (form && matchType(types, form)) || CUSTOM;
  useEffect(() => {
    if (!reporting) return;
    let alive = true;
    Promise.all([api('/client/bug-reports/options'), api('/client/settings')])
      .then(([options, saved]) => {
        if (!alive) return;
        setForm({ ...providerDefaults(saved, defaultProvider(saved)), working_dir: options.working_dir, review_policy: options.review_policy, reviewer: null });
        setSettings(saved);
      })
      .catch((e) => alive && setError(e.message));
    return () => { alive = false; };
  }, []);
  useEffect(() => {
    if (reporting) return;
    if (!later && ticket.state === 'blocked' && !canStartAnyway(ticket)) return;
    let alive = true;
    const query = new URLSearchParams({ repo: ticket.repo, number: ticket.number });
    if (!later && ticket.state === 'blocked') query.set('start_blocked', 'true');
    Promise.all([api(`/client/board/start-options?${query}`), api('/client/settings')])
      .then(([options, saved]) => {
        if (!alive) return;
        let next = { ...options, template: options.brief };
        const tier = (saved.new_agent.agent_types || []).find(t => t.name.toLowerCase() === ticket.tier?.toLowerCase());
        const preference = ticket.launch_preference?.source === 'lane' && tier ? null : ticket.launch_preference?.config;
        if (preference && !auto) next = { ...next, ...preference, brief: preference.brief ?? options.brief };
        if (later) {
          const kinds = saved.new_agent.agent_types || [];
          const name = defaultType(ticket, kinds, stored(LAST_TYPE_KEY, ''));
          const type = kinds.find((t) => t.name === name);
          if (auto) next = { ...next, ...choiceOf(auto), brief: auto.brief ?? options.brief };
          else if (type && !preference) next = { ...next, provider: type.provider, model: type.model, reasoning_effort: type.effort };
          setEdited(auto ? auto.brief != null : !!preference && preference.brief != null);
        }
        setForm(next); setSettings(saved);
      })
      .catch((e) => alive && setError(e.message));
    return () => { alive = false; };
  }, [ticket.repo, ticket.number]);
  const wantModels = !!form && (!reporting || report.startAgent);
  useEffect(() => {
    if (!wantModels) return;
    let alive = true;
    setModels([]);
    const query = new URLSearchParams({ provider: form.provider });
    if (form.working_dir) query.set('working_dir', form.working_dir);
    api(`/client/session-models?${query}`).then((data) => alive && setModels(data.models || []))
      .catch((e) => alive && setError(e.message));
    return () => { alive = false; };
  }, [wantModels, form && form.provider, form && form.working_dir]);
  const set = (patch) => setForm({ ...form, ...patch });
  const onType = (type) => store(LAST_TYPE_KEY, type.name);
  const run = async (method, path, body, done) => {
    if (busy) return;
    setBusy(true); setError(null);
    try {
      const result = await api(path, { method, body });
      onClose(); onStarted(); done(result);
    } catch (e) { setError(e.message); }
    finally { setBusy(false); }
  };
  const fileBug = async () => {
    if (filed) {
      run('POST', '/client/board/start', bugStartBody(filed.issue, form), (result) => toast(`Started ${result.name}`, () => openPanel(`agent:${result.session_id}`)));
      return;
    }
    if (busy) return;
    setBusy(true); setError(null);
    try {
      const result = await api('/client/bug-reports', { method: 'POST', body: bugBody(bug, report, form) });
      if (result.start_error) { setFiled(result); setError(result.start_error); return; }
      onClose();
      toast(filedText(result), result.started ? () => openPanel(`agent:${result.started.session_id}`) : () => window.open(result.issue.url, '_blank', 'noopener'));
    } catch (e) { setError(e.message); }
    finally { setBusy(false); }
  };
  const start = () => (later
    ? run('PUT', '/client/board/auto-start', autoStartBody(ticket, form, typeName === CUSTOM ? null : typeName, edited ? form.brief : null),
      () => toast(`#${ticket.number} starts when ready`))
    : run('POST', '/client/board/start', startBody(ticket, form), (result) => toast(`Started ${result.name}`, () => openPanel(`agent:${result.session_id}`))));
  const choices = form && form.model && !models.includes(form.model) ? [form.model, ...models] : models;
  const blocked = ticket.state === 'blocked';
  const startable = later || !blocked || canStartAnyway(ticket);
  if (reporting) {
    const locked = !!filed;
    const ready = report.text.trim() && (!report.startAgent || form);
    return html`<${Popover} onClose=${onClose} className="ticket-start bug-report">
      <h2>Report a bug</h2>
      <${BugText} value=${report.text} disabled=${locked} onInput=${(text) => setReport({ ...report, text })} />
      <p class="sub">Public: goes into a GitHub issue. The first line is the title.</p>
      <div class="line bug-shot">${bug.screenshot ? html`<img src=${`data:image/png;base64,${bug.screenshot}`} alt="Screenshot" />` : null}
        <span>Screenshot<br /><span class="sub">${bug.screenshot ? 'Private on sm, with page data and server facts' : 'Screenshot unavailable'}</span></span>
        <${Toggle} label="Screenshot" checked=${report.screenshot} disabled=${locked || !bug.screenshot} onChange=${(screenshot) => setReport({ ...report, screenshot })} /></div>
      <div class="line"><span>Start an agent</span>
        <${Toggle} label="Start an agent" checked=${report.startAgent} disabled=${locked} onChange=${(startAgent) => setReport({ ...report, startAgent })} /></div>
      ${report.startAgent ? form ? html`<${AgentFields} form=${form} set=${set} settings=${settings} models=${choices}
        named=${html`<div class="line"><span>Named from the new ticket, as Start does</span></div>`} />` : !error ? html`<p>Loading…</p>` : null : null}
      ${filed ? html`<p>Filed <a href=${filed.issue.url} target="_blank" rel="noopener">#${filed.issue.number}</a>; the agent did not start.</p>` : null}
      ${error ? html`<p class="err" role="alert">${error}</p>` : null}
      <div class="row"><button class="btn" disabled=${busy} onClick=${onClose}>${filed ? 'Close' : 'Cancel'}</button>
        <button class="btn pri" disabled=${!ready || busy} onClick=${fileBug}>${bugPrimary({ startAgent: report.startAgent, filed, busy })}</button></div>
    <//>`;
  }
  return html`<${Popover} onClose=${onClose} className="ticket-start">
    <h2>${later ? `Start #${ticket.number} when ready` : `${startable ? 'Start' : 'Blocked'} #${ticket.number}`}</h2>
    <p class="sub">${ticket.title}</p>
    ${later ? html`<p>${blocked ? `${blockedReasons(ticket)[0]} It starts at the next refresh after that.` : 'Ready: it starts at the next refresh.'}</p>
      ${auto && auto.state === 'failed' ? html`<p class="err">Could not start after ${auto.attempts} tries: ${auto.last_error}</p>` : null}
      ${settings && settings.new_agent.auto_start_paused ? html`<p class="amber">Auto-start is paused (Settings › New agents).</p>` : null}`
      : blocked ? blockedReasons(ticket).map((reason) => html`<p>${reason}</p>`) : null}
    ${startable && form ? html`
      <${AgentFields} form=${form} set=${set} settings=${settings} models=${choices} onType=${onType}
        efforts=${later ? AUTO_EFFORTS : EFFORTS} reviewer=${!later}
        named=${html`<div class="line"><span>Named ${form.name}${later ? ` · first message ${edited ? 'edited' : 'default'}` : ''}</span><span>
          ${later && edited ? html`<button class="link-btn" onClick=${() => { setEdited(false); set({ brief: form.template }); }}>Use default</button> ` : null}
          <button class="link-btn" onClick=${() => setPreview(!preview)}>Preview</button></span></div>
          ${preview ? html`<pre class="brief-preview">${form.brief}</pre>` : null}`}
        extra=${html`
          ${later ? null : html`<label class="fld"><span class="l">Name</span><input class="inp" value=${form.name} onInput=${(e) => set({ name: e.target.value })} /></label>`}
          <div class="fld"><span class="l">Workspace</span><span class="mono">${homeRelative(form.working_dir)}</span></div>
          <label class="fld top"><span class="l">First message</span><textarea class="inp" rows="6" value=${form.brief}
            onInput=${(e) => { if (later) setEdited(true); set({ brief: e.target.value }); }}></textarea></label>`} />
    ` : startable && !error ? html`<p>Loading…</p>` : null}
    ${error ? html`<p class="err" role="alert">${error}</p>` : null}
    <div class="row"><button class="btn" disabled=${busy} onClick=${onClose}>${startable && !later ? 'Cancel' : 'Close'}</button>
      ${auto ? html`<button class="btn" disabled=${busy} onClick=${() => run('DELETE', autoStartPath(ticket), undefined, () => toast('Cancelled'))}>Cancel auto-start</button>` : null}
      ${startable ? html`<button class="btn pri" disabled=${!form || busy} onClick=${start}>${busy ? 'Starting…' : later ? 'Start when ready' : blocked ? 'Start anyway' : 'Start'}</button>` : null}</div>
  <//>`;
}

/** The bug text, focused when the dialog opens. */
function BugText({ value, disabled, onInput }) {
  const box = useRef(null);
  useEffect(() => { if (box.current) box.current.focus(); }, []);
  return html`<textarea class="inp" rows="5" ref=${box} aria-label="What's wrong" placeholder="What's wrong?" value=${value} disabled=${disabled}
    onInput=${(e) => onInput(e.target.value)}></textarea>`;
}

/**
 * The agent section Start, Start when ready and Report a bug share (1859 B4, sm#1981):
 * one click on a configured agent type, or Other for the Provider, Model and Effort
 * fields; then the model · effort · workspace line, `named`, `extra`, and the Reviewer row.
 */
export function AgentFields({ form, set, settings, models, efforts = EFFORTS, reviewer = true, named = null, extra = null, onType = null }) {
  const matched = matchType(agentTypes(settings), form);
  const [other, setOther] = useState(!matched);
  const chosen = other || !matched ? CUSTOM : matched;
  const pick = (type) => {
    setOther(!type);
    if (!type) return;
    set(typeConfig(type));
    if (onType) onType(type);
  };
  return html`
    <${TypePicker} settings=${settings} value=${chosen} onPick=${pick} />
    ${chosen === CUSTOM ? html`
      <${Seg} label="Provider" value=${form.provider}
        options=${[{ value: 'claude', label: 'Claude' }, { value: 'codex-fork', label: 'Codex' }]}
        onChange=${(provider) => set(providerDefaults(settings, provider))} />
      <label class="fld"><span class="l">Model</span><select class="inp" value=${form.model || ''} onChange=${(e) => set({ model: e.target.value || null })}>
        <option value="">Provider default</option>${models.map((model) => html`<option value=${model}>${model}</option>`)}</select></label>
      <div class="fld"><span class="l">Effort</span><${Seg} label="Effort" value=${form.reasoning_effort || ''}
        options=${[{ value: '', label: 'default' }, ...(efforts[form.provider] || []).map((e) => ({ value: e, label: e }))]}
        onChange=${(value) => set({ reasoning_effort: value || null })} /></div>` : null}
    <div class="line"><span>${form.model || (form.provider === 'claude' ? 'Claude Code default model' : 'Codex default model')} · ${form.reasoning_effort || 'default effort'} · ${form.working_dir ? homeRelative(form.working_dir) : 'no checkout'}</span></div>
    ${named}
    ${extra}
    ${reviewer ? html`<${ReviewerRow} form=${form} set=${set} />` : null}`;
}

/**
 * The reviewer for this ticket (1768 I3): one line naming the policy it would use,
 * opening to the editor on Change (sm#1981), or open while a ticket policy is chosen.
 */
function ReviewerRow({ form, set }) {
  const [open, setOpen] = useState(!!form.reviewer);
  const resolved = form.review_policy;
  const own = resolved && resolved.source && resolved.source.startsWith('ticket');
  const kinds = REVIEWER_KINDS.map((k) => (k.value === '' && own ? { ...k, label: 'Ticket\'s own' } : k));
  const kind = form.reviewer ? form.reviewer.kind : '';
  const choose = (value) => set({ reviewer: value ? switchKind(form.reviewer || resolved?.resolved, value) : null });
  const current = resolved ? `${reviewerText(resolved.resolved)} · from ${resolved.source}` : 'The policy a review request would use';
  if (!open) {
    return html`<div class="line reviewer-line"><span>Reviewer · ${current}</span>
      <button type="button" class="link-btn" onClick=${() => setOpen(true)}>Change</button></div>`;
  }
  return html`<div class="fld top"><span class="l">Reviewer</span><div class="review-editor">
    <${Seg} label="Reviewer for this ticket" value=${kind} options=${kinds} onChange=${choose} />
    ${form.reviewer ? html`<${ReviewerEditor} value=${form.reviewer} kinds=${null} onChange=${(reviewer) => set({ reviewer })}
      note=${form.reviewer.kind === 'paired' ? `Starts at the first review request, in ${form.name ? `${form.name}'s checkout, as ${form.name}-reviewer` : 'the new agent\'s checkout, as its reviewer'}. It may build and run tests, never edit.` : null} />`
      : html`<p class="sub">${current}.</p>`}
  </div></div>`;
}

/** Start lane when ready (1821 F4): a type and first message per ticket, one request. */
export function LaneWhenReady({ lane, onClose, onSaved }) {
  const tickets = laneCandidates(lane);
  const [types, setTypes] = useState(null);
  const [rows, setRows] = useState({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const key = (t) => `${t.repo}#${t.number}`;
  useEffect(() => {
    let alive = true;
    api('/client/settings').then((saved) => {
      if (!alive) return;
      const kinds = saved.new_agent.agent_types || [];
      const last = stored(LAST_TYPE_KEY, '');
      setRows(Object.fromEntries(tickets.map((t) => [key(t), { type: defaultType(t, kinds, last),
        edited: !!t.auto_start && t.auto_start.brief != null, text: t.auto_start ? t.auto_start.brief : null, open: false }])));
      setTypes(kinds);
    }).catch((e) => alive && setError(e.message));
    return () => { alive = false; };
  }, []);
  const set = (t, patch) => setRows((prev) => ({ ...prev, [key(t)]: { ...prev[key(t)], ...patch } }));
  const choose = (t, type) => { set(t, { type }); if (type && type !== CUSTOM) store(LAST_TYPE_KEY, type); };
  const toggleMessage = async (t) => {
    const row = rows[key(t)];
    if (row.open || row.template != null) { set(t, { open: !row.open }); return; }
    try {
      const options = await api(`/client/board/start-options?${new URLSearchParams({ repo: t.repo, number: t.number })}`);
      set(t, { open: true, template: options.brief, text: row.edited ? row.text : options.brief });
    } catch (e) { setError(e.message); }
  };
  const item = (t) => {
    const row = rows[key(t)];
    const brief = row.edited ? row.text : null;
    if (row.type === CUSTOM) return retryBody({ ...t, auto_start: { ...t.auto_start, agent_type: null, brief } });
    const type = types.find((k) => k.name === row.type);
    return autoStartBody(t, { ...type, reasoning_effort: type.effort }, type.name, brief);
  };
  const save = async () => {
    if (busy) return;
    setBusy(true); setError(null);
    try {
      // Cancel first: the PUT's recompute could start a dropped ticket.
      for (const t of tickets.filter((t) => t.auto_start && !rows[key(t)].type)) await api(autoStartPath(t), { method: 'DELETE' });
      const chosen = tickets.filter((t) => rows[key(t)].type);
      if (chosen.length) {
        await api('/client/board/auto-start/lane', { method: 'PUT',
          body: { goal_repo: lane.goal.repo, goal_number: lane.goal.number, tickets: chosen.map(item) } });
      }
      onClose(); onSaved();
      toast(`Lane ${lane.rank} saved`);
    } catch (e) { setError(e.message); }
    finally { setBusy(false); }
  };
  return html`<${Popover} onClose=${onClose} className="ticket-start lane-when-ready">
    <h2>Start lane ${lane.rank} when ready</h2>
    <p class="sub">${lane.goal.title}</p>
    ${types ? html`
      <div class="lwr-table">
        <div class="lwr-row lwr-head"><span>Ticket</span><span>Agent type</span><span>First message</span></div>
        ${tickets.map((t) => { const row = rows[key(t)]; const auto = t.auto_start; return html`<div class="lwr-row" key=${key(t)}>
          <span class="lwr-ticket"><span class="mono">#${t.number}</span> ${t.title}<span class="sub">${t.state === 'ready' ? 'Ready · starts at the next refresh.' : blockedReasons(t)[0]}${auto && auto.state === 'failed' ? ' Last start failed.' : ''}</span></span>
          <span><select class="inp" aria-label=${`Agent type for #${t.number}`} value=${row.type} onChange=${(e) => choose(t, e.target.value)}>
            <option value="">Don't start</option>
            ${types.map((type) => html`<option value=${type.name}>${type.name} · ${typeText(type)}</option>`)}
            ${auto && defaultType(t, types, '') === CUSTOM ? html`<option value=${CUSTOM}>Custom · ${typeText({ model: auto.model, effort: auto.effort })}</option>` : null}
          </select></span>
          <span><button class="link-btn" disabled=${!row.type} onClick=${() => toggleMessage(t)}>${row.edited ? 'edited' : 'default'}</button></span>
          ${row.open && row.type ? html`<div class="lwr-message"><textarea class="inp" rows="5" value=${row.text} aria-label=${`First message for #${t.number}`}
            onInput=${(e) => set(t, { text: e.target.value, edited: true })}></textarea>
            ${row.edited ? html`<button class="link-btn" onClick=${() => set(t, { edited: false, text: row.template })}>Use default</button>` : null}</div>` : null}
        </div>`; })}
      </div>` : !error ? html`<p>Loading…</p>` : null}
    ${error ? html`<p class="err" role="alert">${error}</p>` : null}
    <div class="row"><button class="btn" disabled=${busy} onClick=${onClose}>Cancel</button>
      <button class="btn pri" disabled=${!types || busy} onClick=${save}>${busy ? 'Saving…' : 'Save'}</button></div>
  <//>`;
}
