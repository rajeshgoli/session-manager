// Owner preferences shared with the phone, except the browser's theme.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, config, Seg, Toggle, age, stored, store } from './ui.js';
import { TITLES_KEY } from './agents.js';
import { tokens, tokensOf } from './handoff.js';
import { ReviewerEditor, PolicyPopover, reviewerText, fallbackText, setByText } from './reviews.js';
import { DevicesList } from './devices.js';

const SECTIONS = [
  ['appearance', 'Appearance'], ['new-agents', 'New agents'], ['context-handoff', 'Context handoff'], ['reviews', 'Reviews'],
  ['queue-limits', 'Queue limits'], ['terminals', 'Terminals'], ['notifications', 'Notifications'], ['devices-access', 'Devices & access'], ['about', 'About'],
];
const SAMPLE = { repo: 'rajeshgoli/session-manager', number: 1706, title: 'Board Start preselects Fable', url: 'https://github.com/rajeshgoli/session-manager/issues/1706' };

export function sampleTicket(board) {
  return [...(board?.lanes || []), ...(board?.other || [])].flatMap(group => group.tickets || [])
    .find(ticket => ticket.state === 'ready') || SAMPLE;
}

export function preview(defaults, ticket) {
  const repoName = ticket.repo.split('/').pop();
  const values = { ...ticket, ticket: `#${ticket.number}`, number: String(ticket.number),
    repo_name: repoName, repo_short: defaults.repo_short[ticket.repo] || repoName };
  const expand = text => text.replace(/\{([^{}]+)\}/g, (match, key) => values[key] ?? match);
  return { name: expand(defaults.name_pattern).toLowerCase().replace(/[^a-z0-9-]+/g, '-').slice(0, 32),
    message: expand(defaults.message_template) };
}

function useResource(path) {
  const [state, setState] = useState({});
  const load = () => api(path).then(data => setState({ data })).catch(error => setState({ error: error.message }));
  useEffect(() => { let live = true; api(path).then(data => live && setState({ data }))
    .catch(error => live && setState({ error: error.message })); return () => { live = false; }; }, [path]);
  return [state, load, data => setState({ data })];
}

function Resource({ state, retry, children }) {
  if (state.error) return html`<p role="alert" class="settings-error">${state.error} <button class="btn sm" onClick=${retry}>Retry</button></p>`;
  if (!state.data) return html`<p class="sub">Loading…</p>`;
  return children(state.data);
}

// Field-local drafts survive failed requests and unrelated saves. Serialize writes
// at the page level so a slow response cannot reorder edits to the same setting.
function Field({ label, initial, save, onDraft, type = 'text', options, hint, placeholder, reset, min, max, step = '1', list }) {
  const [value, setValue] = useState(initial ?? '');
  const [status, setStatus] = useState('');
  const [error, setError] = useState(false);
  const saved = useRef(initial ?? '');
  const current = useRef(value);
  const pendingWrites = useRef(0);
  useEffect(() => {
    // Refresh a pristine field after an earlier save finishes while navigating.
    if (current.current === saved.current && !pendingWrites.current) {
      const next = initial ?? '';
      current.current = next; saved.current = next; setValue(next);
    }
  }, [initial]);
  const change = next => { current.current = next; setValue(next); setStatus(''); onDraft?.(next); };
  const commit = async next => {
    if (next === saved.current && !pendingWrites.current) return;
    pendingWrites.current++;
    setStatus('Saving…'); setError(false);
    try {
      await save(next);
      saved.current = next;
      if (current.current === next) setStatus('Saved');
    } catch (e) {
      if (current.current === next) { setError(true); setStatus(e.message); }
    } finally { pendingWrites.current--; }
  };
  const attrs = { class: 'inp', value, placeholder, min, max, step, list,
    onInput: e => change(type === 'checkbox' ? e.target.checked : e.target.value),
    onBlur: () => commit(current.current) };
  return html`<div class="settings-field">
    <label><span>${label}</span>
      ${type === 'checkbox' ? html`<${Toggle} label=${label} checked=${!!value} onChange=${next => { change(next); commit(next); }} />`
        : type === 'textarea' ? html`<textarea ...${attrs} rows="4" />` : options
        ? html`<select ...${attrs} onChange=${e => change(e.target.value)}>${options.map(([v, text]) => html`<option value=${v}>${text}</option>`)}</select>`
        : html`<input ...${attrs} type=${type} checked=${type === 'checkbox' ? !!value : undefined} />`}
    </label>
    ${hint ? html`<p class="sub">${hint}</p>` : null}
    <div class="settings-feedback">${reset ? html`<button class="btn sm" onClick=${() => { change(''); commit(''); }}>Reset</button>` : null}
      <span role=${error ? 'alert' : 'status'} class=${error ? 'settings-error' : 'saved'}>${status}</span></div>
  </div>`;
}

const integerLimit = value => {
  if (value === '') return null;
  const n = Number(value);
  if (!Number.isInteger(n) || n < 0 || n > 16) throw new Error('Enter a whole number from 0 to 16.');
  return n;
};
// Terminal limits and their allowed ranges; matches the server's check.
const TERMINAL_LIMITS = [
  ['per_user', 'Open terminals (you)', 1, 256, 'How many terminals you can have open at once, across the web and the phone.'],
  ['per_session', 'Viewers per agent', 1, 256, 'How many terminals can show the same agent at once.'],
  ['global', 'Open terminals (everyone)', 1, 256, 'All terminals on this server at once.'],
  ['max_attach_seconds', 'Longest session (seconds)', 60, 86400, 'A terminal closes after this long; reconnect to continue.'],
];
const terminalLimit = (value, min, max) => {
  if (value === '') return null;
  const n = Number(value);
  if (!Number.isInteger(n) || n < min || n > max) throw new Error(`Enter a whole number from ${min} to ${max}.`);
  return n;
};
function shortNames(text) {
  const result = {};
  for (const line of text.split('\n').filter(line => line.trim())) {
    const parts = line.split('=');
    if (parts.length !== 2 || !parts[0].trim() || Object.hasOwn(result, parts[0].trim())) throw new Error('Use one unique owner/repository = shortname per line.');
    result[parts[0].trim()] = parts[1].trim();
  }
  return result;
}

export function SettingsPage() {
  const [settings, reload, setSettings] = useResource('/client/settings');
  const [section, setSection] = useState(() => location.hash.slice(1) || 'new-agents');
  const pending = useRef(Promise.resolve());
  const write = (path, body, method = 'PUT') => {
    const result = pending.current.then(() => api(path, { method, body })).then(data => {
      if (path === '/client/settings') setSettings(data);
      return data;
    });
    pending.current = result.catch(() => {});
    return result;
  };
  useEffect(() => { const hash = () => setSection(location.hash.slice(1) || 'new-agents');
    window.addEventListener('hashchange', hash); return () => window.removeEventListener('hashchange', hash); }, []);
  const selected = SECTIONS.some(([id]) => id === section) ? section : 'new-agents';
  return html`<div class="content settings-page">
    <nav class="settings-nav" aria-label="Settings sections">${SECTIONS.map(([id, name]) => html`<a href=${`#${id}`}
      class=${selected === id ? 'on' : ''} aria-current=${selected === id ? 'page' : null}>${name}</a>`)}</nav>
    <div class="settings-body" key=${selected}>
      ${selected === 'appearance' ? html`<${Appearance} />` : selected === 'context-handoff' ? html`<${Handoff} write=${write} />`
        : selected === 'notifications' ? html`<${Notifications} write=${write} />` : selected === 'devices-access' ? html`<${Devices} write=${write} />`
        : selected === 'about' ? html`<${About} />` : html`<${Resource} state=${settings} retry=${reload}>${data => selected === 'reviews'
          ? html`<${Reviews} data=${data} write=${write} />` : selected === 'queue-limits'
          ? html`<${QueueLimits} data=${data.queue_limits} write=${write} />`
          : selected === 'terminals' ? html`<${TerminalLimits} data=${data.terminal_limits} config=${data.terminal_config_limits || {}} write=${write} />`
          : html`<${NewAgents} data=${data.new_agent} retire=${data.auto_retire} write=${write} />`}</${Resource}>`}
    </div>
  </div>`;
}

function Appearance() {
  const [theme, setTheme] = useState(() => { try { return localStorage.getItem('sm-theme') || 'system'; } catch { return 'system'; } });
  const [textSize, setTextSize] = useState(() => {
    try { const size = Number(localStorage.getItem('sm-text-size')); return Number.isInteger(size) && size >= 13 && size <= 19 ? size : 15; }
    catch { return 15; }
  });
  const [titles, setTitles] = useState(() => stored(TITLES_KEY, true) !== false);
  const [status, setStatus] = useState('');
  const chooseTitles = value => { setTitles(value); store(TITLES_KEY, value); setStatus('Saved'); };
  const choose = value => { setTheme(value); document.documentElement.dataset.theme = value;
    try { localStorage.setItem('sm-theme', value); setStatus('Saved'); } catch { setStatus('Applied until reload; browser storage is unavailable.'); } };
  const chooseSize = size => { setTextSize(size); document.documentElement.style.fontSize = `${size}px`;
    try { localStorage.setItem('sm-text-size', String(size)); setStatus('Saved'); } catch { setStatus('Applied until reload; browser storage is unavailable.'); } };
  return html`<h2>Appearance</h2><${Seg} label="Theme" value=${theme} onChange=${choose}
    options=${['system', 'light', 'dark'].map(value => ({ value, label: value[0].toUpperCase() + value.slice(1) }))} />
    <div class="text-size-setting"><label for="sm-text-size">Text size</label>
      <input id="sm-text-size" type="range" min="13" max="19" step="1" value=${textSize}
        onInput=${event => chooseSize(Number(event.target.value))} />
      <output for="sm-text-size">${textSize} px</output>
      <button class="btn sm" type="button" onClick=${() => chooseSize(15)}>Reset</button></div>
    <div class="settings-field"><label><span>Ticket titles on agent cards</span>
      <${Toggle} label="Ticket titles on agent cards" checked=${titles} onChange=${chooseTitles} /></label>
      <p class="sub">Shows each agent's ticket title under its name on the Agents page.</p></div>
    <p class="sub">Saved in this browser only.</p><p role="status" class="saved">${status}</p>`;
}

// Auto-retire delay range in minutes; matches the server's check.
const RETIRE_MINUTES = [15, 1440];
export function retireMinutes(value) {
  const n = Number(value);
  if (!String(value).trim() || !Number.isInteger(n) || n < RETIRE_MINUTES[0] || n > RETIRE_MINUTES[1]) throw new Error(`Enter a whole number from ${RETIRE_MINUTES[0]} to ${RETIRE_MINUTES[1]}.`);
  return n;
}

function NewAgents({ data, retire, write }) {
  const [draft, setDraft] = useState({});
  const [board] = useResource('/client/board');
  const [claude] = useResource('/client/session-models?provider=claude');
  const [codex] = useResource('/client/session-models?provider=codex-fork');
  const patch = (key, value) => setDraft(prev => ({ ...prev, [key]: value }));
  const save = body => write('/client/settings', { new_agent: body });
  const sample = sampleTicket(board.data), rendered = preview({ ...data, ...draft }, sample);
  return html`<h2>Defaults</h2><p class="sub">Shared with the phone. Changes save when you leave a field.</p>
    <${Field} label="Default agent" initial=${data.provider} options=${[['claude', 'Claude'], ['codex-fork', 'Codex']]}
      save=${value => save({ provider: value })} />
    <div class="settings-grid">${[['claude', 'Claude', claude, ['low', 'medium', 'high', 'max']], ['codex', 'Codex', codex, ['medium', 'high', 'xhigh']]].map(([key, name, models, efforts]) => html`<div>
      <${Field} label=${`${name} model`} initial=${data[key].model} placeholder="Provider default" list=${`${key}-models`}
        hint="Leave blank for the provider default, choose a suggestion, or enter a model name."
        save=${value => save({ [key]: { model: value.trim() || null } })} />
      <datalist id=${`${key}-models`}>${(models.data?.models || []).map(model => html`<option value=${model} />`)}</datalist>
      ${models.error ? html`<p class="sub">Model suggestions unavailable. You can still enter a model name.</p>` : null}
      <${Field} label=${`${name} effort`} initial=${data[key].effort} options=${[['', 'Provider default'], ...efforts.map(value => [value, value])]}
        save=${value => save({ [key]: { effort: value || null } })} /></div>`)}</div>
    <${Field} label="Agent name" initial=${data.name_pattern} onDraft=${value => patch('name_pattern', value)} save=${value => save({ name_pattern: value })}
      hint="Placeholders: {ticket} {number} {repo} {repo_name} {repo_short} {title} {url}" />
    <p class="settings-preview"><span class="sub">Name preview · #${sample.number}</span><code>${rendered.name}</code></p>
    <${Field} label="Short repo names" type="textarea" initial=${Object.entries(data.repo_short).map(([repo, name]) => `${repo} = ${name}`).join('\n')}
      hint="One owner/repository = shortname per line. Short names use 1–12 lowercase letters or digits and must be unique."
      onDraft=${value => { try { patch('repo_short', shortNames(value)); } catch { /* Keep the last parseable preview. */ } }}
      save=${value => save({ repo_short: shortNames(value) })} />
    <${Field} label="Workspaces" type="textarea" initial=${data.workspaces.join('\n')} hint="One absolute path per line."
      save=${value => save({ workspaces: value.split('\n').map(path => path.trim()).filter(Boolean) })} />
    <${AgentTypes} types=${data.agent_types || []} save=${save} />
    <${Field} label="Pause auto-start" type="checkbox" initial=${data.auto_start_paused}
      hint="Holds every Start when ready without losing it; ready tickets start when you turn it off."
      save=${value => save({ auto_start_paused: value })} />
    ${retire ? html`<${Field} label="Retire finished agents automatically" type="checkbox" initial=${retire.enabled}
      hint="Agents sm started retire once finished and idle with nothing waiting on them. A reply, a doc review or an sm send brings one back."
      save=${value => write('/client/settings', { auto_retire: { enabled: value } })} />
    <${Field} label="After (minutes idle)" type="number" min=${String(RETIRE_MINUTES[0])} max=${String(RETIRE_MINUTES[1])} initial=${retire.idle_minutes}
      hint="An hour is past the provider's prompt cache, so retiring then costs no extra tokens."
      save=${value => write('/client/settings', { auto_retire: { idle_minutes: retireMinutes(value) } })} />` : null}
    <h2>First message when starting a ticket</h2>
    <${Field} label="Template" type="textarea" initial=${data.message_template} onDraft=${value => patch('message_template', value)}
      hint="Placeholders: {ticket} {number} {repo} {repo_name} {repo_short} {title} {url}"
      save=${value => save({ message_template: value })} />
    <div class="settings-preview"><span class="sub">Message preview · ${sample.repo} #${sample.number}</span><pre>${rendered.message}</pre></div>`;
}

function QueueLimits({ data, write }) {
  return html`<h2>Queue limits</h2><p class="sub">Changes apply without a restart. Lower limits hold new starts; running jobs continue. Reset restores the configured value shown in the field.</p>
    <div class="settings-grid">${[['max_running', 'Total'], ['tests', 'Tests'], ['perf', 'Performance'], ['background', 'Background'], ['service', 'Service'], ['review', 'Review runs']].map(([key, label]) => html`<${Field}
      label=${label} type="number" min="0" max="16" initial=${data[key]} placeholder=${String(config.queue_config_limits?.[key] ?? '')} reset=${true}
      save=${value => write('/client/settings', { queue_limits: { [key]: integerLimit(value) } })} />`)}</div>`;
}

export function meterPercent(value) {
  const n = Number(value);
  if (value === '' || !Number.isInteger(n) || n < 50 || n > 100) throw new Error('Enter a whole number from 50 to 100.');
  return n;
}

/** Policy rows for Settings › Reviews: every repo, then stored lane and ticket policies. */
export function policyRows(listing, repos) {
  const policies = listing?.policies || [];
  const repoNames = [...new Set([...(repos || []), ...policies.filter(p => p.scope === 'repo').map(p => p.repo)])].sort();
  return [
    ...repoNames.map(repo => ({ scope: 'repo', repo, number: 0, policy: policies.find(p => p.scope === 'repo' && p.repo === repo) || null })),
    ...policies.filter(p => p.scope !== 'repo').map(policy => ({ scope: policy.scope, repo: policy.repo, number: policy.number, policy })),
  ];
}

const meterText = value => typeof value === 'number' ? `${Math.round(value)}%` : '—';
const timeText = iso => iso ? new Date(iso).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) : '';

function Reviews({ data, write }) {
  const [status, reloadStatus] = useResource('/client/reviews/status');
  const [listing, reloadListing] = useResource('/review-policies');
  const [board] = useResource('/client/board');
  const [reviewer, setReviewer] = useState(data.reviews.reviewer);
  const [saved, setSaved] = useState(''), [checking, setChecking] = useState(false), [editing, setEditing] = useState(null);
  const choose = async next => {
    setReviewer(next); setSaved('Saving…');
    try { await write('/client/settings', { reviews: { reviewer: next } }); setSaved('Saved'); }
    catch (e) { setSaved(e.message); }
  };
  const tryNow = async () => {
    setChecking(true);
    try { await write('/client/reviews/github-codex/check', undefined, 'POST'); setSaved('The next review request checks GitHub Codex.'); reloadStatus(); }
    catch (e) { setSaved(e.message); } finally { setChecking(false); }
  };
  const github = status.data?.github_codex;
  const day = status.data?.last_24h;
  const rows = policyRows(listing.data, (board.data?.repos || []).map(r => r.repo));
  const reviewed = day ? day.github_codex + day.codex_runs + day.claude_runs : 0;
  const total = day ? reviewed + day.no_reviewer : 0;
  return html`<h2>Reviews</h2><p class="sub">Shared with the phone. Agents run sm request-review; sm picks the reviewer below and moves to its fallback when it can't review.</p>
    ${github?.state === 'paused' ? html`<div class="review-banner" role="status"><span class="chip amber">Paused</span>
      <span><b>GitHub Codex is out of code-review quota</b> since ${timeText(github.paused_at)}. Reviews go to local runs.
        ${github.next_check_at ? ` The next check is after ${timeText(github.next_check_at)}.` : ''}</span>
      <button class="btn sm" disabled=${checking} onClick=${tryNow}>Try now</button></div>` : null}
    <h3>Default reviewer <span class="sub">every repo, lane and ticket without its own</span></h3>
    <${ReviewerEditor} value=${reviewer} onChange=${choose} />
    <p role="status" class="saved">${saved}</p>
    <h3>Narrower policies</h3>
    <${Resource} state=${listing} retry=${reloadListing}>${() => html`<div class="review-policies">${rows.map(row => html`<div class="review-policy-row">
      <span class="sub">${row.scope === 'repo' ? `Repo · ${row.repo.split('/').pop()}` : row.scope === 'lane' ? `Lane · ${row.repo.split('/').pop()} #${row.number}` : `Ticket ${row.repo.split('/').pop()} #${row.number}`}</span>
      <span>${row.policy ? html`<b>${reviewerText(row.policy.reviewer)}</b> <span class="sub">${setByText(row.policy)} · falls back to ${fallbackText(row.policy.fallback)}</span>` : html`<span class="sub">Uses the default</span>`}</span>
      <span class="anchor"><button class="btn sm" onClick=${() => setEditing(`${row.scope}:${row.repo}:${row.number}`)}>${row.policy ? 'Change' : 'Set'}</button>
        ${editing === `${row.scope}:${row.repo}:${row.number}` ? html`<${PolicyPopover} scope=${row.scope} repo=${row.repo} number=${row.number} align="right"
          title=${`Reviews for ${row.scope === 'repo' ? row.repo : `${row.scope} ${row.repo.split('/').pop()} #${row.number}`}`} policy=${row.policy}
          onClose=${() => setEditing(null)} onSaved=${reloadListing} />` : null}</span>
    </div>`)}</div>`}</${Resource}>
    <h3>Limits and today</h3>
    <div class="settings-grid">
      <${Field} label="Review runs at once" type="number" min="0" max="16" initial=${data.queue_limits.review} placeholder="4" reset=${true}
        hint='Queue type "review"; another run waits for a slot.' save=${value => write('/client/settings', { queue_limits: { review: integerLimit(value) } })} />
      <${Field} label="Skip a provider at (% of its weekly meter)" type="number" min="50" max="100" initial=${data.reviews.skip_meter_percent}
        hint=${status.data ? `Codex ${meterText(status.data.meters.codex)} · Claude ${meterText(status.data.meters.claude)} now. 100 never skips.` : '100 never skips.'}
        save=${value => write('/client/settings', { reviews: { skip_meter_percent: meterPercent(value) } })} />
    </div>
    <${Resource} state=${status} retry=${reloadStatus}>${() => html`<div class="review-day">
      <p><b>Last 24 hours: ${total}</b> <span class="sub">· GitHub Codex ${day.github_codex} · Codex runs ${day.codex_runs} · Claude runs ${day.claude_runs} · ${day.no_reviewer ? html`<span class="magenta">${day.no_reviewer} left unreviewed</span>` : 'none left unreviewed'}</span></p>
      <div class="review-bar" aria-hidden="true">${[['github_codex', 'cyan'], ['codex_runs', 'green'], ['claude_runs', 'amber'], ['no_reviewer', 'magenta']].map(([key, color]) => day[key]
        ? html`<i style=${`flex:${day[key]};background:var(--${color})`} title=${`${key.replace('_', ' ')}: ${day[key]}`}></i>` : null)}</div>
      ${status.data.running.length ? html`<p class="sub">Now: ${status.data.running.map(r => `${r.repo.split('/').pop()} #${r.pr_number} · ${r.reviewer_label || 'starting'}`).join('; ')}</p>` : null}
    </div>`}</${Resource}>`;
}

function TerminalLimits({ data, config, write }) {
  return html`<h2>Terminals</h2><p class="sub">Shared with the phone. Changes apply to the next terminal you open. Reset restores the configured value shown in the field.
    A terminal that stops answering for 30 seconds closes on its own.</p>
    <div class="settings-grid">${TERMINAL_LIMITS.map(([key, label, min, max, hint]) => html`<${Field}
      label=${label} type="number" min=${String(min)} max=${String(max)} initial=${data[key]} placeholder=${String(config[key] ?? '')} reset=${true} hint=${hint}
      save=${value => write('/client/settings', { terminal_limits: { [key]: terminalLimit(value, min, max) } })} />`)}</div>`;
}

// Only providers with a context gauge can hand off: plain Codex and
// codex-app are not listed.
const HANDOFF_PROVIDERS = [
  ['claude', 'Claude agents', () => 'Off. Claude agents keep working until their context fills.'],
  ['codex-fork', 'Codex agents', window => `Off. Codex compacts its own context near the end of its ${window ? `${tokens(window)} ` : ''}window.`],
];
const THRESHOLDS = [
  ['threshold_percent', 'Hand off at', '0.01', ''],
  ['reminder_percent', 'Remind at', '0.01', ''],
  ['review_floor_percent', 'Review floor', '0', ' (below this, a review request does not ask for a handoff)'],
];

function Threshold({ label, initial, min, window, note, save }) {
  const [draft, setDraft] = useState(initial);
  return html`<${Field} label=${`${label} (%)`} type="number" min=${min} max="100" step="any" initial=${initial}
    onDraft=${setDraft} hint=${`${tokensOf(draft, window) ? `· ${tokensOf(draft, window)}` : ''}${note}`}
    save=${value => { if (!value.trim() || !Number.isFinite(Number(value))) throw new Error('Enter a percentage.'); return save(Number(value)); }} />`;
}

function Handoff({ write }) {
  const [state, reload, setData] = useResource('/handoff-defaults');
  const save = body => write('/handoff-defaults', body).then(data => { setData(data); return data; });
  return html`<h2>Context handoff</h2><p class="sub">Let a fresh agent take over when context fills up. Shared with the phone.</p>
    <${Resource} state=${state} retry=${reload}>${data => html`
      ${HANDOFF_PROVIDERS.map(([provider, name, off]) => {
        const on = !!data.providers[provider];
        const thresholds = data.provider_thresholds?.[provider] || {};
        return html`<section class="handoff-provider" key=${provider}><h3>${name}</h3>
          <${Field} label="Hand off automatically" type="checkbox" initial=${on} save=${value => save({ providers: { [provider]: value } })} />
          ${on ? THRESHOLDS.map(([key, label, min, note]) => html`<${Threshold} key=${key} label=${label} min=${min} note=${note}
              initial=${String(thresholds[key] ?? '')} window=${data.window_tokens?.[provider]}
              save=${value => save({ provider_thresholds: { [provider]: { [key]: value } } })} />`)
            : html`<p class="sub">${off(data.window_tokens?.[provider])}</p>`}
        </section>`;
      })}
      ${HANDOFF_PROVIDERS.some(([provider]) => data.providers[provider]) ? html`<h3>When a review is requested</h3>
        ${[['ask_on_codex_review', 'Codex review of a PR: ask the agent to hand off first'], ['ask_on_doc_review', 'Your review of a doc: ask the agent to hand off first']].map(([key, label]) => html`<${Field}
          label=${label} type="checkbox" initial=${data[key]} save=${value => save({ [key]: value })} />`)}` : null}
      <p class="sub">To try handoff on one agent or one ticket, leave these off and use Hand off… in that agent's details or in a ticket's ⋯ on the Board. The agents that take over inherit the setting.</p>
    `}</${Resource}>`;
}

function Notifications({ write }) {
  const [state, reload] = useResource('/client/push/status');
  const [status, setStatus] = useState(''), [busy, setBusy] = useState(false);
  const send = async () => { setBusy(true); setStatus('Sending…'); try {
    const result = await write('/client/push/test', undefined, 'POST');
    setStatus(`Sent to ${result.sent} phone(s). ${(result.failed || []).map(f => `${f.device_name}: ${f.error}`).join('; ')}`);
  } catch (e) { setStatus(e.message); } finally { setBusy(false); } };
  return html`<h2>Notifications</h2><${Resource} state=${state} retry=${reload}>${data => html`
    <p>${data.devices.length ? `Registered for push: ${data.devices.map(d => d.device_name).join(', ')}` : 'No phone is registered for push.'}</p>
    ${!data.configured ? html`<p class="sub">Push delivery is not configured on the server.</p>` : null}
    <button class="btn" disabled=${busy || !data.configured || !data.devices.length} onClick=${send}>Send a test</button>
    <p class="sub">Manage registration in the phone app.</p>`}</${Resource}><p role="status">${status}</p>`;
}

function Devices({ write }) {
  const [ssh, reloadSsh] = useResource('/admin/studio-ssh');
  return html`<h2>Devices & access</h2><${DevicesList} /><h3>Studio SSH</h3><${Resource} state=${ssh} retry=${reloadSsh}>${data => html`
    <${Field} label="Studio SSH" type="checkbox" initial=${data.enabled} save=${async enabled => {
      const result = await write('/admin/studio-ssh', { enabled }, 'POST');
      if (result.error || result.status === 'error') throw new Error(result.error || 'Studio SSH failed');
      reloadSsh();
    }} /><p class="sub">${data.host} · ${data.status}${data.error ? ` · ${data.error}` : ''}</p>`}</${Resource}>
    `;
}

function About() {
  const [app, reloadApp] = useResource('/apps/session-manager-android/meta.json');
  const [health, setHealth] = useState(null);
  useEffect(() => {
    let live = true;
    fetch('/health').then(response => response.ok ? response.json() : null).then(body => live && setHealth(body?.status === 'healthy'))
      .catch(() => live && setHealth(false));
    return () => { live = false; };
  }, []);
  const row = (dot, label, value, sub) => html`<div class="about-row"><span class=${`about-dot ${dot}`} aria-hidden="true"></span>
    <span class="about-label">${label}</span><span class="about-value">${value}${sub ? html`<span class="sub"> · ${sub}</span>` : null}</span></div>`;
  return html`<h2 class="about-title"><span class="about-mark">sm</span> About</h2><div class="about-rows">
    ${row(health === null ? 'slate' : health ? 'green' : 'red', 'Server', config.server_version || 'Unavailable',
      [config.server_started_at && `up ${age(config.server_started_at)}`, health === null ? 'checking' : health ? 'healthy' : 'not answering'].filter(Boolean).join(' · '))}
    ${row('green', 'Web build', html`<code>${config.build_id}</code>`)}
    ${app.error ? row('red', 'Phone app', 'Unavailable', html`${app.error} <button class="btn sm" onClick=${reloadApp}>Retry</button>`)
      : !app.data ? row('slate', 'Phone app', 'Loading…')
      : row('green', 'Phone app', `${app.data.version_name || app.data.artifact_hash}${app.data.version_code ? ` · build ${app.data.version_code}` : ''}`,
        `built ${new Date(app.data.uploaded_at).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' })}`)}
    ${row('accent', 'Repository', html`<a href="https://github.com/rajeshgoli/session-manager" target="_blank" rel="noopener">rajeshgoli/session-manager ↗</a>`)}
  </div>`;
}

// Start when ready picks one of these (1821 F1); the server checks the whole list on save.
const TYPE_EFFORTS = { claude: ['low', 'medium', 'high', 'xhigh', 'max'], 'codex-fork': ['medium', 'high', 'xhigh'] };
const MAX_AGENT_TYPES = 8;
export function typeProblem(rows) {
  const names = rows.map(row => row.name.trim().toLowerCase());
  if (names.some(name => !name)) return 'Give every agent type a name.';
  if (new Set(names).size !== names.length) return 'Agent type names must be different.';
  if (rows.some(row => !row.model.trim())) return 'Give every agent type a model.';
  return '';
}
function AgentTypes({ types, save }) {
  const [rows, setRows] = useState(types);
  const [status, setStatus] = useState('');
  const [error, setError] = useState(false);
  const base = useRef(types);
  useEffect(() => {
    // Follow a change saved elsewhere (the phone) unless this list has unsaved edits.
    if (JSON.stringify(rows) === JSON.stringify(base.current)) setRows(types);
    base.current = types;
  }, [JSON.stringify(types)]);
  const dirty = JSON.stringify(rows) !== JSON.stringify(types);
  const problem = typeProblem(rows);
  const edit = next => { setStatus(''); setRows(next); };
  const update = (index, patch) => edit(rows.map((row, i) => i === index ? { ...row, ...patch } : row));
  const provider = (index, value) => update(index, { provider: value, model: '',
    effort: TYPE_EFFORTS[value].includes(rows[index].effort) ? rows[index].effort : 'high' });
  const commit = async () => {
    setStatus('Saving…'); setError(false);
    try {
      await save({ agent_types: rows.map(row => ({ ...row, name: row.name.trim(), model: row.model.trim() })) });
      setStatus('Saved');
    } catch (e) { setError(true); setStatus(e.message); }
  };
  return html`<h2>Agent types</h2>
    <p class="sub">Start when ready and Start lane when ready offer these. A ticket whose text has a line such as <code>Tier: Mid</code> starts out as that type.</p>
    <div class="agent-types">
      <div class="agent-type head"><span>Name</span><span>Provider</span><span>Model</span><span>Effort</span></div>
      ${rows.map((row, index) => html`<div class="agent-type">
        <input class="inp" aria-label="Name" value=${row.name} onInput=${e => update(index, { name: e.target.value })} />
        <select class="inp" aria-label="Provider" value=${row.provider} onChange=${e => provider(index, e.target.value)}>
          <option value="claude">Claude</option><option value="codex-fork">Codex</option></select>
        <input class="inp" aria-label="Model" value=${row.model} list=${row.provider === 'claude' ? 'claude-models' : 'codex-models'}
          onInput=${e => update(index, { model: e.target.value })} />
        <select class="inp" aria-label="Effort" value=${row.effort} onChange=${e => update(index, { effort: e.target.value })}>
          ${TYPE_EFFORTS[row.provider].map(effort => html`<option value=${effort}>${effort}</option>`)}</select>
        <button class="icon-btn" title=${`Remove ${row.name || 'this type'}`} onClick=${() => edit(rows.filter((_, i) => i !== index))}>×</button>
      </div>`)}
    </div>
    <div class="settings-feedback">
      <button class="btn sm" disabled=${rows.length >= MAX_AGENT_TYPES}
        onClick=${() => edit([...rows, { name: '', provider: 'claude', model: '', effort: 'high' }])}>Add type</button>
      ${dirty ? html`<button class="btn sm" onClick=${() => edit(types)}>Revert</button>
        <button class="btn sm pri" disabled=${!!problem} onClick=${commit}>Save agent types</button>` : null}
      <span role=${error || (dirty && problem) ? 'alert' : 'status'} class=${error || (dirty && problem) ? 'settings-error' : 'saved'}>${dirty && problem ? problem : status}</span></div>`;
}
