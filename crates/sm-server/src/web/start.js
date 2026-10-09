// The New agent popover (spec 1710 D6.5, sm#1981): pick a configured agent
// type and press Start, or Other for provider, model and effort; "Change"
// opens the workspace, name and first message in the same panel.
import { useEffect, useState } from 'preact/hooks';
import { html, api, Popover, Seg, homeRelative, openPanel, toast } from './ui.js';
import { TypePicker, OTHER_TYPE, agentTypes, matchType } from './launch-fields.js';

export const EFFORTS = {
  claude: ['low', 'medium', 'high', 'max'],
  'codex-fork': ['medium', 'high', 'xhigh'],
};
export function useLocalModel() {
  const [local, setLocal] = useState(null);
  useEffect(() => {
    let alive = true;
    const refresh = () => api('/client/session-models?provider=opencode')
      .then(value => { if (alive) setLocal(value); })
      .catch(() => { if (alive) setLocal(null); });
    refresh();
    const timer = setInterval(refresh, 5000);
    return () => { alive = false; clearInterval(timer); };
  }, []);
  return local;
}
export const localBlocked = (local, provider) => provider === 'opencode' && (!local?.models?.length || local.available === false);
export function LocalChoice({ local, provider, onPick }) {
  return local?.models?.length ? html`<div class="line"><button type="button" class=${`btn ${provider === 'opencode' ? 'pri' : ''}`}
    disabled=${local.available === false} onClick=${() => onPick(local.models[0])}>${`Local (${local.models[0]})`}</button>
    ${local.reason ? html`<span class="sub">${local.reason}</span>` : null}</div>` : null;
}
const PROVIDERS = [{ value: 'claude', label: 'Claude' }, { value: 'codex-fork', label: 'Codex' }];
const OTHER = '__other__';
const MORE = '__more__';
const TOP_WORKSPACES = ['fractal-algo-rust', 'session-manager', 'codex-fork', 'deskbar', 'finviz', 'backup-manager'];

/** Settings key for a provider's saved defaults (D4). */
const defaultsKey = (provider) => (provider === 'claude' ? 'claude' : 'codex');
const normalizeProvider = (provider) => (provider && provider.startsWith('codex') ? 'codex-fork' : 'claude');
const providerDefaultModel = (provider) => (provider === 'claude' ? 'Claude Code default model' : 'Codex default model');

/** Folders offered for a new agent: saved workspaces, project folders, then live agents' folders. */
function workspaceChoices(settings, watch) {
  const seen = new Set();
  const out = [];
  const add = (path) => {
    if (path && !seen.has(path)) {
      seen.add(path);
      out.push(path);
    }
  };
  (settings.new_agent.workspaces || []).forEach(add);
  (settings.workspace_folders || []).forEach(add);
  for (const agent of (watch && watch.sessions) || []) if (agent.state !== 'stopped') add(agent.repo);
  const preferred = TOP_WORKSPACES.flatMap(name => out.filter(path => path.replace(/\/$/, '').split('/').at(-1) === name));
  return [...preferred, ...out.filter(path => !preferred.includes(path))];
}

const validWorkspace = (path) => !!path && (path.startsWith('/') || path === '~' || path.startsWith('~/'));

/**
 * `prefill` may carry provider, model, effort and workspace (Clone).
 */
export function NewAgentPopover({ prefill = {}, onClose }) {
  const [settings, setSettings] = useState(null);
  const local = useLocalModel();
  const [watch, setWatch] = useState(null);
  const [error, setError] = useState(null);
  const [expanded, setExpanded] = useState(false);
  const [form, setForm] = useState(null);
  const [models, setModels] = useState([]);
  const [busy, setBusy] = useState(false);
  const [other, setOther] = useState(false);
  const [allFolders, setAllFolders] = useState(false);

  useEffect(() => {
    let alive = true;
    Promise.all([api('/client/settings'), api('/watch/state').catch(() => null)])
      .then(([value, state]) => {
        if (!alive) return;
        const saved = value.new_agent;
        const provider = normalizeProvider(prefill.provider || saved.provider);
        const defaults = saved[defaultsKey(provider)] || {};
        const workspace = prefill.workspace || (saved.workspaces || [])[0] || '';
        setSettings(value);
        setWatch(state);
        setForm({
          provider,
          model: prefill.provider ? prefill.model || null : defaults.model || null,
          effort: prefill.provider ? prefill.effort || null : defaults.effort || null,
          workspace,
          otherWorkspace: '',
          name: '',
          message: '',
        });
      })
      .catch((e) => alive && setError(e.message));
    return () => { alive = false; };
  }, []);

  const folder = form && (form.workspace === OTHER ? form.otherWorkspace.trim() : form.workspace);
  useEffect(() => {
    if (!form) return undefined;
    // A slower answer for an earlier provider or folder must not replace this one.
    let current = true;
    const query = new URLSearchParams({ provider: form.provider });
    if (validWorkspace(folder)) query.set('working_dir', folder);
    api(`/client/session-models?${query}`)
      .then((value) => current && setModels(value.models || []))
      .catch(() => current && setModels([]));
    return () => { current = false; };
  }, [form && form.provider, folder]);

  const set = (patch) => setForm((prev) => ({ ...prev, ...patch }));
  const switchProvider = (provider) => {
    const defaults = settings.new_agent[defaultsKey(provider)] || {};
    set({ provider, model: defaults.model || null, effort: defaults.effort || null });
  };

  const start = async () => {
    if (localBlocked(local, form.provider)) { setError(local?.reason || 'no local model loaded'); return; }
    if (!validWorkspace(folder)) {
      setError('Choose a workspace: an absolute path or ~/ path.');
      setExpanded(true);
      return;
    }
    setBusy(true);
    setError(null);
    const body = { provider: form.provider, working_dir: folder };
    if (form.model) body.model = form.model;
    if (form.effort) body.reasoning_effort = form.effort;
    if (form.name.trim()) body.name = form.name.trim();
    if (form.message.trim()) body.initial_message = form.message.trim();
    try {
      const started = await api('/client/sessions', { method: 'POST', body });
      onClose();
      toast(`Started ${started.name}`, () => openPanel(`agent:${started.id}`));
    } catch (e) {
      setError(e.message);
    } finally {
      setBusy(false);
    }
  };

  let content;
  if (!form) {
    content = error ? html`<p class="err">${error}</p>` : html`<p class="muted">Loading…</p>`;
  } else {
    const choices = workspaceChoices(settings, watch);
    if (form.workspace && form.workspace !== OTHER && !choices.includes(form.workspace)) choices.unshift(form.workspace);
    const visibleChoices = allFolders ? choices : choices.slice(0, 10);
    // A cloned workspace stays selectable even when it is outside the first ten.
    if (!allFolders && form.workspace !== OTHER && form.workspace && !visibleChoices.includes(form.workspace)) {
      visibleChoices.splice(9, 1, form.workspace);
    }
    const summary = [form.model || providerDefaultModel(form.provider), form.effort || 'default effort', homeRelative(folder) || 'no workspace'];
    const modelOptions = models.includes(form.model) || !form.model ? models : [form.model, ...models];
    const matched = matchType(agentTypes(settings), { ...form, reasoning_effort: form.effort });
    const chosen = other || !matched ? OTHER_TYPE : matched;
    const pick = (type) => {
      setOther(!type);
      if (type) set({ provider: type.provider, model: type.model, effort: type.effort });
    };
    content = html`
      <${TypePicker} settings=${settings} value=${chosen} onPick=${pick} />
      <${LocalChoice} local=${local} provider=${form.provider} onPick=${model => { setOther(true); set({ provider: 'opencode', model, effort: null }); }} />
      ${chosen === OTHER_TYPE
        ? html`
          <${Seg} label="Provider" value=${form.provider} onChange=${switchProvider} options=${PROVIDERS} />
          ${form.provider !== 'opencode' ? html`<div class="fld"><span class="l">Model</span>
            <select class="inp" value=${form.model || ''} onChange=${(e) => set({ model: e.target.value || null })}>
              <option value="">Provider default</option>
              ${modelOptions.map((model) => html`<option value=${model}>${model}</option>`)}
            </select></div>
          <div class="fld"><span class="l">Effort</span>
            <${Seg} label="Effort" value=${form.effort || ''} onChange=${(value) => set({ effort: value || null })}
              options=${[{ value: '', label: 'default' }, ...(EFFORTS[form.provider] || []).map((e) => ({ value: e, label: e }))]} /></div>` : null}`
        : null}
      ${expanded
        ? html`
          <div class="fld"><span class="l">Workspace</span>
            <select class="inp" value=${form.workspace} onChange=${(e) => {
                if (e.target.value === MORE) {
                  e.target.value = form.workspace;
                  setAllFolders(true);
                } else set({ workspace: e.target.value });
              }}>
              ${visibleChoices.map((path) => html`<option value=${path}>${homeRelative(path)}</option>`)}
              ${!allFolders && choices.length > visibleChoices.length ? html`<option value=${MORE}>More folders…</option>` : null}
              <option value=${OTHER}>Other…</option>
            </select></div>
          ${form.workspace === OTHER
            ? html`<div class="fld"><span class="l"></span><input class="inp mono" placeholder="~/projects/repo"
                value=${form.otherWorkspace} onInput=${(e) => set({ otherWorkspace: e.target.value })} /></div>`
            : null}
          ${settings.workspace_error ? html`<p class="muted">${settings.workspace_error}</p>` : null}
          <div class="fld"><span class="l">Name</span>
            <input class="inp" placeholder="sm picks one" value=${form.name} onInput=${(e) => set({ name: e.target.value })} /></div>
          <div class="fld top"><span class="l">First message</span>
            <textarea class="inp" rows="4" placeholder="Optional" value=${form.message}
              onInput=${(e) => set({ message: e.target.value })}></textarea></div>`
        : html`
          <div class="line"><span>${summary.join(' · ')}</span>
            <button type="button" class="link-btn" onClick=${() => setExpanded(true)}>Change</button></div>
          <div class="line"><span>${form.name.trim() ? `Named ${form.name.trim()}` : 'sm picks the name'} · ${form.message.trim() ? 'with a first message' : 'no first message'}</span></div>`}
      ${error ? html`<p class="err">${error}</p>` : null}
      <div class="row">
        <button type="button" class="btn" onClick=${onClose}>Cancel</button>
        <button type="button" class="btn pri" disabled=${busy || localBlocked(local, form.provider)} onClick=${start}>${busy ? 'Starting…' : 'Start'}</button>
      </div>`;
  }
  return html`<${Popover} onClose=${onClose} align="right">
    <h2>New agent</h2>
    ${content}
  <//>`;
}
