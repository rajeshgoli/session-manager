// The New agent popover (spec 1710 D6.5): pick Claude or Codex and press
// Start; the owner's saved defaults fill in model, effort and workspace, and
// "Change" opens every field in the same panel.
import { useEffect, useState } from 'preact/hooks';
import { html, api, Popover, Seg, homeRelative, openPanel, toast } from './ui.js';

export const EFFORTS = {
  claude: ['low', 'medium', 'high', 'max'],
  'codex-fork': ['medium', 'high', 'xhigh'],
};
const PROVIDERS = [{ value: 'claude', label: 'Claude' }, { value: 'codex-fork', label: 'Codex' }];
const OTHER = '__other__';

/** Settings key for a provider's saved defaults (D4). */
const defaultsKey = (provider) => (provider === 'claude' ? 'claude' : 'codex');
const normalizeProvider = (provider) => (provider && provider.startsWith('codex') ? 'codex-fork' : 'claude');
const providerDefaultModel = (provider) => (provider === 'claude' ? 'Claude Code default model' : 'Codex default model');

/** Folders offered for a new agent: saved workspaces, then live agents' folders. */
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
  for (const agent of (watch && watch.sessions) || []) if (agent.state !== 'stopped') add(agent.repo);
  return out;
}

/**
 * `prefill` may carry provider, model, effort and workspace (Clone).
 */
export function NewAgentPopover({ prefill = {}, onClose }) {
  const [settings, setSettings] = useState(null);
  const [watch, setWatch] = useState(null);
  const [error, setError] = useState(null);
  const [expanded, setExpanded] = useState(false);
  const [form, setForm] = useState(null);
  const [models, setModels] = useState([]);
  const [busy, setBusy] = useState(false);

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
    if (folder && folder.startsWith('/')) query.set('working_dir', folder);
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
    if (!folder || !folder.startsWith('/')) {
      setError('Choose a workspace: an absolute folder path.');
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
    const summary = [form.model || providerDefaultModel(form.provider), form.effort || 'default effort', homeRelative(folder) || 'no workspace'];
    const modelOptions = models.includes(form.model) || !form.model ? models : [form.model, ...models];
    content = html`
      <${Seg} label="Provider" value=${form.provider} onChange=${switchProvider} options=${PROVIDERS} />
      ${expanded
        ? html`
          <div class="fld"><span class="l">Model</span>
            <select class="inp" value=${form.model || ''} onChange=${(e) => set({ model: e.target.value || null })}>
              <option value="">Provider default</option>
              ${modelOptions.map((model) => html`<option value=${model}>${model}</option>`)}
            </select></div>
          <div class="fld"><span class="l">Effort</span>
            <${Seg} label="Effort" value=${form.effort || ''} onChange=${(value) => set({ effort: value || null })}
              options=${[{ value: '', label: 'default' }, ...EFFORTS[form.provider].map((e) => ({ value: e, label: e }))]} /></div>
          <div class="fld"><span class="l">Workspace</span>
            <select class="inp" value=${form.workspace} onChange=${(e) => set({ workspace: e.target.value })}>
              ${choices.map((path) => html`<option value=${path}>${homeRelative(path)}</option>`)}
              <option value=${OTHER}>Other…</option>
            </select></div>
          ${form.workspace === OTHER
            ? html`<div class="fld"><span class="l"></span><input class="inp mono" placeholder="/Users/…/projects/repo"
                value=${form.otherWorkspace} onInput=${(e) => set({ otherWorkspace: e.target.value })} /></div>`
            : null}
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
        <button type="button" class="btn pri" disabled=${busy} onClick=${start}>${busy ? 'Starting…' : 'Start'}</button>
      </div>`;
  }
  return html`<${Popover} onClose=${onClose} align="right">
    <h2>New agent</h2>
    ${content}
  <//>`;
}
