import { html, Seg } from './ui.js';

export const SOL = { provider: 'codex-fork', model: 'gpt-6-sol', reasoning_effort: 'medium' };
export function claudeDefaults(settings) {
  const defaults = settings?.new_agent?.claude || {};
  return { provider: 'claude', model: defaults.model || null, reasoning_effort: defaults.effort || null };
}
export const exactConfig = value => `${value.provider === 'claude' ? 'Claude' : 'Codex'} · ${value.model || 'provider default'} · ${value.reasoning_effort || value.effort || 'default effort'}`;
export function Presets({ settings, onPick }) {
  const claude = claudeDefaults(settings);
  return html`<div class="launch-presets" aria-label="Complete launch presets">
    <button type="button" class="preset" onClick=${() => onPick(SOL)}><b>Sol / Medium</b><span>Codex · gpt-6-sol · medium</span></button>
    <button type="button" class="preset" onClick=${() => onPick(claude)}><b>Claude defaults</b><span>${exactConfig(claude)}</span></button>
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
