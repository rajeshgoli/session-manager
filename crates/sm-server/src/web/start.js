// The New agent popover (spec 1710 D6.5, sm#1981): pick a configured agent
// type and press Start, or Other for provider, model and effort; "Change"
// opens the workspace, name and first message in the same panel.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, Popover, Seg, homeRelative, navigate } from './ui.js';
import { TypePicker, OTHER_TYPE, agentTypes, matchType } from './launch-fields.js';
import { createShell, shellError } from './shell.js';
const TERMINAL_TYPE = '__terminal__';

export const EFFORTS = {
  claude: ['low', 'medium', 'high', 'max'],
  'codex-fork': ['medium', 'high', 'xhigh'],
};
export function useLocalModel() {
  const [local, setLocal] = useState(null);
  useEffect(() => {
    let alive = true;
    // The loaded model comes from session-models; the last model sm knew, loaded
    // or not, from /client/model, so an unloaded model can still be picked (sm#2297).
    const refresh = () => Promise.all([
      api('/client/session-models?provider=opencode'),
      api('/client/model').then(value => value?.model || null).catch(() => null),
    ])
      .then(([value, record]) => { if (alive) setLocal({ ...value, record }); })
      .catch(() => { if (alive) setLocal(null); });
    refresh();
    const timer = setInterval(refresh, 5000);
    return () => { alive = false; clearInterval(timer); };
  }, []);
  return local;
}
/** False once the dialog closes, so a model load that finishes later starts nothing. */
export function useOpen() {
  const open = useRef(true);
  useEffect(() => () => { open.current = false; }, []);
  return open;
}
/** The known local model needs loading before an agent can start on it. */
const needsLoad = local => !local?.models?.length && !!local?.record && !['loading', 'ready', 'draining'].includes(local.record.state);
export const localBlocked = (local, provider) => provider === 'opencode'
  && (local?.models?.length ? local.available === false : !needsLoad(local));
/** Loads the known local model if none is loaded; returns the model to start on. */
export async function ensureLocalLoaded(local) {
  if (local?.models?.length) return local.models[0];
  const { key, seats, context, identifier } = local.record;
  await api('/client/model/load', { method: 'POST', body: { key, seats, context } });
  return identifier;
}
/** The local model sm can start on, loaded or not, else null. */
export const localModelName = local => local?.models?.[0] || local?.record?.identifier || null;
export const LOCAL_TYPE = '__local__';
/** The local model as a preset card, local/<model> (sm#2307); null when sm knows no local model. */
export function localPreset(local, onPick) {
  const name = localModelName(local);
  if (!name) return null;
  const state = local.record?.state;
  const detail = local.models?.length ? local.reason || 'Local · loaded'
    : state === 'loading' ? 'Local · loading…'
    : state === 'draining' ? 'Local · unloading…'
    : 'Local · not loaded; Start loads it (minutes)';
  return { key: LOCAL_TYPE, title: `local/${name}`, detail, disabled: localBlocked(local, 'opencode'), onPick: () => onPick(name) };
}
/** Under Local, Model offers only the local model; local agents take no effort. */
export function LocalModelField({ local, value, onChange }) {
  const name = localModelName(local);
  const names = [...new Set([value, name].filter(Boolean))];
  return html`<div class="fld"><span class="l">Model</span>
    <select class="inp" aria-label="Model" value=${value || name || ''} onChange=${e => onChange(e.target.value)}>
      ${names.map(n => html`<option value=${n}>${n}</option>`)}</select></div>`;
}
const PROVIDERS = [{ value: 'claude', label: 'Claude' }, { value: 'codex-fork', label: 'Codex' }];
/** Claude, Codex, and Local while sm knows a local model. */
export const providerOptions = local => (localModelName(local) ? [...PROVIDERS, { value: 'opencode', label: 'Local' }] : PROVIDERS);
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
 * `prefill` may carry provider, model, effort and workspace (Clone), and a
 * name and first message (an artifact investigation).
 */
export function NewAgentPopover({ prefill = {}, onClose, align = 'right' }) {
  const [settings, setSettings] = useState(null);
  const local = useLocalModel();
  const open = useOpen();
  const [watch, setWatch] = useState(null);
  const [error, setError] = useState(null);
  const [expanded, setExpanded] = useState(!!prefill.message);
  const [form, setForm] = useState(null);
  const [models, setModels] = useState([]);
  const [busy, setBusy] = useState(false);
  const [other, setOther] = useState(false);
  const [allFolders, setAllFolders] = useState(false);
  const [terminal, setTerminal] = useState(false);

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
          name: prefill.name || '',
          message: prefill.message || '',
        });
      })
      .catch((e) => alive && setError(e.message));
    return () => { alive = false; };
  }, []);

  const folder = form && (form.workspace === OTHER ? form.otherWorkspace.trim() : form.workspace);
  useEffect(() => {
    if (!form || terminal) return undefined;
    // A slower answer for an earlier provider or folder must not replace this one.
    let current = true;
    const query = new URLSearchParams({ provider: form.provider });
    if (validWorkspace(folder)) query.set('working_dir', folder);
    api(`/client/session-models?${query}`)
      .then((value) => current && setModels(value.models || []))
      .catch(() => current && setModels([]));
    return () => { current = false; };
  }, [form && form.provider, folder, terminal]);

  const set = (patch) => setForm((prev) => ({ ...prev, ...patch }));
  const switchProvider = (provider) => {
    if (provider === 'opencode') { setOther(true); set({ provider, model: localModelName(local), effort: null }); return; }
    const defaults = settings.new_agent[defaultsKey(provider)] || {};
    set({ provider, model: defaults.model || null, effort: defaults.effort || null });
  };

  const start = async () => {
    if (!terminal && localBlocked(local, form.provider)) { setError(local?.reason || 'no local model loaded'); return; }
    if (!validWorkspace(folder)) {
      setError('Choose a workspace: an absolute path or ~/ path.');
      setExpanded(true);
      return;
    }
    setBusy(true);
    setError(null);
    if (terminal) {
      try { await createShell({ cwd: folder }); onClose(); }
      catch (e) { setError(shellError(e)); }
      finally { setBusy(false); }
      return;
    }
    const body = { provider: form.provider, working_dir: folder };
    if (form.model) body.model = form.model;
    if (form.provider === 'opencode') {
      try {
        body.model = await ensureLocalLoaded(local);
        if (!open.current) return;
      } catch (e) {
        setError(`Local model did not load: ${e.message}`);
        setBusy(false);
        return;
      }
    }
    if (form.effort) body.reasoning_effort = form.effort;
    if (form.name.trim()) body.name = form.name.trim();
    if (form.message.trim()) body.initial_message = form.message.trim();
    try {
      const started = await api('/client/sessions', { method: 'POST', body });
      onClose();
      // The owner opens the new agent's terminal after almost every start (sm#2529).
      navigate(`/terminal/${encodeURIComponent(started.id)}`);
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
    const summary = [form.model || providerDefaultModel(form.provider), ...(form.provider === 'opencode' ? [] : [form.effort || 'default effort']), homeRelative(folder) || 'no workspace'];
    const modelOptions = models.includes(form.model) || !form.model ? models : [form.model, ...models];
    const matched = matchType(agentTypes(settings), { ...form, reasoning_effort: form.effort });
    const chosen = terminal ? TERMINAL_TYPE : other ? OTHER_TYPE : form.provider === 'opencode' ? LOCAL_TYPE : matched || OTHER_TYPE;
    const pick = (type) => {
      setTerminal(false);
      setOther(!type);
      if (type) set({ provider: type.provider, model: type.model, effort: type.effort });
    };
    content = html`
      <${TypePicker} settings=${settings} value=${chosen} onPick=${pick}
        extra=${[localPreset(local, model => { setTerminal(false); setOther(false); set({ provider: 'opencode', model, effort: null }); }),
          { key: TERMINAL_TYPE, title: '▹ Terminal', detail: 'zsh in a folder', onPick: () => { setTerminal(true); setError(null); } }].filter(Boolean)} />
      ${chosen === OTHER_TYPE
        ? html`
          <${Seg} label="Provider" value=${form.provider} onChange=${switchProvider} options=${providerOptions(local)} />
          ${form.provider === 'opencode' ? html`<${LocalModelField} local=${local} value=${form.model} onChange=${model => set({ model })} />` : html`<div class="fld"><span class="l">Model</span>
            <select class="inp" value=${form.model || ''} onChange=${(e) => set({ model: e.target.value || null })}>
              <option value="">Provider default</option>
              ${modelOptions.map((model) => html`<option value=${model}>${model}</option>`)}
            </select></div>
          <div class="fld"><span class="l">Effort</span>
            <${Seg} label="Effort" value=${form.effort || ''} onChange=${(value) => set({ effort: value || null })}
              options=${[{ value: '', label: 'default' }, ...(EFFORTS[form.provider] || []).map((e) => ({ value: e, label: e }))]} /></div>`}`
        : null}
      ${expanded || terminal
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
          ${terminal ? null : html`<div class="fld"><span class="l">Name</span>
            <input class="inp" placeholder="sm picks one" value=${form.name} onInput=${(e) => set({ name: e.target.value })} /></div>
          <div class="fld top"><span class="l">First message</span>
            <textarea class="inp" rows="4" placeholder="Optional" value=${form.message}
              onInput=${(e) => set({ message: e.target.value })}></textarea></div>`}`
        : html`
          <div class="line"><span>${summary.join(' · ')}</span>
            <button type="button" class="link-btn" onClick=${() => setExpanded(true)}>Change</button></div>
          <div class="line"><span>${form.name.trim() ? `Named ${form.name.trim()}` : 'sm picks the name'} · ${form.message.trim() ? 'with a first message' : 'no first message'}</span></div>`}
      ${error ? html`<p class="err">${error}</p>` : null}
      <div class="row">
        <button type="button" class="btn" onClick=${onClose}>Cancel</button>
        <button type="button" class="btn pri" disabled=${busy || (!terminal && localBlocked(local, form.provider))} onClick=${start}>${busy ? (!terminal && form.provider === 'opencode' && !local?.models?.length ? 'Loading model…' : 'Starting…') : 'Start'}</button>
      </div>`;
  }
  return html`<${Popover} onClose=${onClose} align=${align}>
    <h2>New agent</h2>
    ${content}
  <//>`;
}
