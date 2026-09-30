// Owner preferences shared with the phone, except the browser's theme.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, config, Seg, ConfirmButton } from './ui.js';

const SECTIONS = [
  ['appearance', 'Appearance'], ['new-agents', 'New agents'], ['context-handoff', 'Context handoff'],
  ['queue-limits', 'Queue limits'], ['notifications', 'Notifications'], ['devices-access', 'Devices & access'], ['about', 'About'],
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
      ${type === 'textarea' ? html`<textarea ...${attrs} rows="4" />` : options
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
        : selected === 'about' ? html`<${About} />` : html`<${Resource} state=${settings} retry=${reload}>${data => selected === 'queue-limits'
          ? html`<${QueueLimits} data=${data.queue_limits} write=${write} />`
          : html`<${NewAgents} data=${data.new_agent} write=${write} />`}</${Resource}>`}
    </div>
  </div>`;
}

function Appearance() {
  const [theme, setTheme] = useState(() => { try { return localStorage.getItem('sm-theme') || 'system'; } catch { return 'system'; } });
  const [status, setStatus] = useState('');
  const choose = value => { setTheme(value); document.documentElement.dataset.theme = value;
    try { localStorage.setItem('sm-theme', value); setStatus('Saved'); } catch { setStatus('Applied until reload; browser storage is unavailable.'); } };
  return html`<h2>Appearance</h2><${Seg} label="Theme" value=${theme} onChange=${choose}
    options=${['system', 'light', 'dark'].map(value => ({ value, label: value[0].toUpperCase() + value.slice(1) }))} />
    <p class="sub">Saved in this browser only.</p><p role="status" class="saved">${status}</p>`;
}

function NewAgents({ data, write }) {
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
    <h2>First message when starting a ticket</h2>
    <${Field} label="Template" type="textarea" initial=${data.message_template} onDraft=${value => patch('message_template', value)}
      hint="Placeholders: {ticket} {number} {repo} {repo_name} {repo_short} {title} {url}"
      save=${value => save({ message_template: value })} />
    <div class="settings-preview"><span class="sub">Message preview · ${sample.repo} #${sample.number}</span><pre>${rendered.message}</pre></div>`;
}

function QueueLimits({ data, write }) {
  return html`<h2>Queue limits</h2><p class="sub">Changes apply without a restart. Lower limits hold new starts; running jobs continue. Reset restores the configured value shown in the field.</p>
    <div class="settings-grid">${[['max_running', 'Total'], ['tests', 'Tests'], ['perf', 'Performance'], ['background', 'Background'], ['service', 'Service']].map(([key, label]) => html`<${Field}
      label=${label} type="number" min="0" max="16" initial=${data[key]} placeholder=${String(config.queue_config_limits?.[key] ?? '')} reset=${true}
      save=${value => write('/client/settings', { queue_limits: { [key]: integerLimit(value) } })} />`)}</div>`;
}

function handoffProviders(providers) {
  return [...new Set(['claude', 'codex-fork', 'codex', ...Object.keys(providers)])].filter(provider => provider !== 'codex-app');
}

function Handoff({ write }) {
  const [state, reload] = useResource('/handoff-defaults');
  return html`<h2>Context handoff</h2><p class="sub">Let a fresh agent take over when context fills up. Individual agents can override these defaults.</p>
    <${Resource} state=${state} retry=${reload}>${data => html`
      ${handoffProviders(data.providers).map(provider => html`<${Field}
        label=${`Enable ${provider}`} type="checkbox" initial=${!!data.providers[provider]} save=${value => write('/handoff-defaults', { providers: { [provider]: value } })} />`)}
      ${[['threshold_percent', 'Context threshold (%)'], ['review_floor_percent', 'Review floor (%)'], ['reminder_percent', 'Reminder at (%)']].map(([key, label]) => html`<${Field}
        label=${label} type="number" min=${key === 'review_floor_percent' ? '0' : '0.01'} max="100" step="any" initial=${String(data[key])}
        save=${value => { if (!value.trim() || !Number.isFinite(Number(value))) throw new Error('Enter a percentage.'); return write('/handoff-defaults', { [key]: Number(value) }); }} />`)}
      ${[['ask_on_codex_review', 'Ask on Codex review request'], ['ask_on_doc_review', 'Ask on document review request']].map(([key, label]) => html`<${Field}
        label=${label} type="checkbox" initial=${data[key]} save=${value => write('/handoff-defaults', { [key]: value })} />`)}
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
  const [devices, reloadDevices] = useResource('/client/mobile-terminal/devices');
  const [status, setStatus] = useState('');
  const revoke = async device => { setStatus('Revoking…'); try {
    await write(`/client/mobile-terminal/devices/${encodeURIComponent(device.device_key_id)}?user_id=${encodeURIComponent(device.user_id)}`, undefined, 'DELETE');
    setStatus('Device revoked'); reloadDevices();
  } catch (e) { setStatus(e.message); } };
  return html`<h2>Devices & access</h2><${Resource} state=${ssh} retry=${reloadSsh}>${data => html`
    <${Field} label="Studio SSH" type="checkbox" initial=${data.enabled} save=${async enabled => {
      const result = await write('/admin/studio-ssh', { enabled }, 'POST');
      if (result.error || result.status === 'error') throw new Error(result.error || 'Studio SSH failed');
      reloadSsh();
    }} /><p class="sub">${data.host} · ${data.status}${data.error ? ` · ${data.error}` : ''}</p>`}</${Resource}>
    <h3>Enrolled devices</h3><${Resource} state=${devices} retry=${reloadDevices}>${data => html`
      ${data.devices.length ? data.devices.map(device => html`<div class="settings-device">
        <div><strong>${device.name || device.device_name || device.device_key_id}</strong><p class="sub">${device.kind || 'Device'} · ${device.user_id}${device.last_used_at ? ` · Last used ${new Date(device.last_used_at).toLocaleString()}` : ''}</p></div>
        ${device.revoked ? html`<span class="sub">Revoked</span>` : html`<${ConfirmButton} label="Revoke" prompt="Revoke this device's access?" confirmLabel="Revoke" onConfirm=${() => revoke(device)} />`}</div>`)
        : html`<p class="sub">No enrolled devices.</p>`}
    `}</${Resource}><p role="status">${status}</p>`;
}

function About() {
  const [app, reload] = useResource('/apps/session-manager-android/meta.json');
  return html`<h2>About</h2><dl class="settings-about"><dt>Server version</dt><dd>${config.server_version || 'Unavailable'}</dd>
    <dt>Web build</dt><dd>${config.build_id}</dd></dl>
    <h3>Latest Android app</h3><${Resource} state=${app} retry=${reload}>${data => html`<p>${data.version_name || data.artifact_hash}
      ${data.version_code ? ` · build ${data.version_code}` : ''}</p><p class="sub">Published ${new Date(data.uploaded_at).toLocaleString()}</p>`}</${Resource}>`;
}
