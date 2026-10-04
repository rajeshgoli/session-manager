import { html, Seg } from './ui.js';

export function claudeDefaults(settings) {
  const defaults = settings?.new_agent?.claude || {};
  return { provider: 'claude', model: defaults.model || null, reasoning_effort: defaults.effort || null };
}
export const exactConfig = value => `${value.provider === 'claude' ? 'Claude' : 'Codex'} · ${value.model || 'provider default'} · ${value.reasoning_effort || value.effort || 'default effort'}`;
/** The agent types configured in Settings › New agents. */
export const agentTypes = settings => settings?.new_agent?.agent_types || [];
export const typeConfig = type => ({ provider: type.provider, model: type.model, reasoning_effort: type.effort });
export const OTHER_TYPE = '__custom__';
/** The agent type whose provider, model and effort equal the choice, else ''. */
export function matchType(types, choice) {
  const type = (types || []).find((t) => t.provider === choice.provider && t.model === (choice.model || null)
    && t.effort === (choice.reasoning_effort || null));
  return type ? type.name : '';
}
/**
 * Every configured agent type as a one-click button (sm#1981), plus Other,
 * which hands the choice to the provider, model and effort fields.
 * `value` is the chosen type's name, OTHER_TYPE, or null; onPick gets the type, or null for Other.
 */
export function TypePicker({ settings, value = null, onPick, other = true }) {
  const button = (key, title, detail, pick) => html`<button type="button" key=${key} class=${`preset ${value === key ? 'on' : ''}`}
    aria-pressed=${value === key} onClick=${pick}><b>${title}</b><span>${detail}</span></button>`;
  return html`<div class="launch-presets" role="group" aria-label="Agent type">
    ${agentTypes(settings).map(t => button(t.name, t.name, exactConfig(typeConfig(t)), () => onPick(t)))}
    ${other ? button(OTHER_TYPE, 'Other', 'Choose provider, model and effort', () => onPick(null)) : null}
  </div>`;
}
export function ConfigFields({ value, onChange }) {
  const provider = value.provider || 'claude';
  return html`<div class="launch-config-grid">
    <label class="fld"><span class="l">Provider</span><select class="inp" value=${provider} onChange=${e => onChange({ provider: e.target.value, model: null, reasoning_effort: null })}>
      <option value="claude">Claude</option><option value="codex-fork">Codex</option></select></label>
    <label class="fld"><span class="l">Exact model</span><input class="inp" aria-label="Exact model" placeholder="Provider default" value=${value.model || ''} onInput=${e => onChange({ ...value, model: e.target.value || null })} /></label>
    <label class="fld"><span class="l">Reasoning effort</span><select class="inp" value=${value.reasoning_effort || ''} onChange=${e => onChange({ ...value, reasoning_effort: e.target.value || null })}>
      <option value="">Provider default</option>${(provider === 'claude' ? ['low','medium','high','xhigh','max'] : ['medium','high','xhigh']).map(e => html`<option value=${e}>${e}</option>`)}</select></label>
  </div>`;
}
