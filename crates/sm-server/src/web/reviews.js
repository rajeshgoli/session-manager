// Review policies on the web (1768 I3): you pick one reviewer with its model
// and effort; sm shows, never asks for, the fallback.
import { useEffect, useState } from 'preact/hooks';
import { html, api, Seg, Popover } from './ui.js';

export const REVIEW_EFFORTS = { codex: ['medium', 'high', 'xhigh'], claude: ['low', 'medium', 'high', 'xhigh', 'max'] };
const FIRST = { codex: { model: 'gpt-6-sol', effort: 'medium' }, claude: { model: 'opus', effort: 'high' } };
const TIERS = { 'gpt-6-astra': 'top', fable: 'top', 'gpt-6-luna': 'low', 'gpt-5.6-luna': 'low', sonnet: 'low', haiku: 'low' };
export const tier = (model) => TIERS[model] || 'mid';
const run = (kind, model, effort) => ({ kind, model, effort });

/** The fallback sm computes for `reviewer`; mirrors `review::chain` (spec 1768 C2). */
export function reviewFallback(reviewer) {
  if (!reviewer) return [];
  const { kind, model } = reviewer;
  if (kind === 'github_codex') return [run('codex', 'gpt-6-sol', 'medium'), run('claude', 'opus', 'high')];
  if (kind === 'codex') {
    return [model === 'gpt-6-astra' ? run('claude', 'fable', 'xhigh')
      : ['gpt-6-luna', 'gpt-5.6-luna'].includes(model) ? run('claude', 'sonnet', 'high') : run('claude', 'opus', 'high')];
  }
  if (kind === 'claude') {
    return [model === 'fable' ? run('codex', 'gpt-6-astra', 'high')
      : ['sonnet', 'haiku'].includes(model) ? run('codex', 'gpt-6-luna', 'high') : run('codex', 'gpt-6-sol', 'medium')];
  }
  if (kind === 'paired') {
    const same = run(reviewer.provider === 'claude' ? 'claude' : 'codex', reviewer.model, reviewer.effort);
    return [same, ...reviewFallback(same)];
  }
  return [];
}

export function reviewerText(reviewer) {
  if (!reviewer) return 'the default';
  if (reviewer.kind === 'github_codex') return 'GitHub Codex';
  if (reviewer.kind === 'paired') return `Paired ${reviewer.provider === 'claude' ? 'Claude' : 'Codex'} · ${reviewer.model} · ${reviewer.effort}`;
  return `${reviewer.kind === 'codex' ? 'Codex' : 'Claude'} run · ${reviewer.model} · ${reviewer.effort}`;
}
export const fallbackText = (list) => (list || []).map(reviewerText).join(', then ');
export const setByText = (policy) => (policy ? `Set by ${policy.set_by_name}${policy.set_at ? ` · ${new Date(policy.set_at).toLocaleString()}` : ''}` : '');

/** Changing the reviewer kind keeps model and effort while the provider stays the same. */
export function switchKind(current, kind) {
  if (kind === 'github_codex') return { kind };
  const was = current?.kind === 'paired' ? current.provider : current?.kind;
  const provider = kind === 'paired' ? (was === 'claude' ? 'claude' : 'codex') : kind;
  const base = was === provider && current.model ? { model: current.model, effort: current.effort } : FIRST[provider];
  return kind === 'paired' ? { kind, provider, ...base } : { kind, ...base };
}
export const setPairedProvider = (provider) => ({ kind: 'paired', provider, ...FIRST[provider] });

/** The policy a scope would use without its own: the next scope out, as the server resolves it (C3). */
export function inheritedPolicy(listing, { scope, repo, lanePolicy }) {
  if (!listing) return null;
  if (scope === 'ticket' && lanePolicy) return { reviewer: lanePolicy.reviewer, source: 'the lane' };
  const repoPolicy = scope !== 'repo' && (listing.policies || []).find((p) => p.scope === 'repo' && p.repo === repo);
  if (repoPolicy) return { reviewer: repoPolicy.reviewer, source: `repo ${repo.split('/').pop()}` };
  return { reviewer: listing.default?.reviewer, source: 'the default' };
}

let modelLists = null;
/** Model suggestions per provider, fetched once per page load. */
export function useReviewModels() {
  const [models, setModels] = useState(modelLists || { codex: [], claude: [] });
  useEffect(() => {
    if (modelLists) return;
    Promise.all(['codex-fork', 'claude'].map((p) => api(`/client/session-models?provider=${p}`).then((d) => d.models || []).catch(() => [])))
      .then(([codex, claude]) => { modelLists = { codex, claude }; setModels(modelLists); });
  }, []);
  return models;
}

/**
 * One reviewer: kind, then provider (paired), model and effort. `kinds` lists the
 * kinds offered; pass `kinds={null}` when the caller draws its own kind switch.
 */
export function ReviewerEditor({ value, onChange, paired = false, kinds, note }) {
  const models = useReviewModels();
  const options = kinds === null ? null : kinds || [{ value: 'github_codex', label: 'GitHub Codex' }, { value: 'codex', label: 'Codex run' },
    { value: 'claude', label: 'Claude run' }, ...(paired ? [{ value: 'paired', label: 'Paired' }] : [])];
  const provider = value.kind === 'paired' ? value.provider : value.kind;
  const list = models[provider] || [];
  const choices = value.model && !list.includes(value.model) ? [value.model, ...list] : list;
  const fallback = reviewFallback(value);
  return html`<div class="review-editor">
    ${options ? html`<${Seg} label="Reviewer" value=${value.kind} options=${options} onChange=${(kind) => onChange(switchKind(value, kind))} />` : null}
    ${value.kind === 'paired' ? html`<${Seg} label="Paired reviewer provider" value=${value.provider}
      options=${[{ value: 'codex', label: 'Codex' }, { value: 'claude', label: 'Claude' }]} onChange=${(p) => onChange(setPairedProvider(p))} />` : null}
    ${value.kind === 'github_codex' ? html`<p class="sub">GitHub Codex chooses its own model · about 0.05% of a Codex week per review</p>`
      : html`<div class="review-pickers">
        <label><span class="sub">Model</span><select class="inp" value=${value.model} onChange=${(e) => onChange({ ...value, model: e.target.value })}>
          ${choices.map((model) => html`<option value=${model}>${model}</option>`)}</select></label>
        <label><span class="sub">Effort</span><select class="inp" value=${value.effort} onChange=${(e) => onChange({ ...value, effort: e.target.value })}>
          ${REVIEW_EFFORTS[provider].map((effort) => html`<option value=${effort}>${effort}</option>`)}</select></label>
        <span class="sub">${tier(value.model)} tier</span></div>`}
    ${note ? html`<p class="sub">${note}</p>` : null}
    <p class="sub">If it can't review: ${fallbackText(fallback)}. Set by sm.</p>
  </div>`;
}

const SCOPE_NAME = { repo: 'this repo', lane: 'this lane', ticket: 'this ticket' };

/** The Board's and Settings' policy editor for one repo, lane or ticket. */
export function PolicyPopover({ scope, repo, number, title, policy, lanePolicy, onClose, onSaved, align }) {
  const [listing, setListing] = useState(null);
  const [own, setOwn] = useState(!!policy);
  const [draft, setDraft] = useState(policy?.reviewer || { kind: 'github_codex' });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  useEffect(() => { api('/review-policies').then(setListing).catch((e) => setError(e.message)); }, []);
  const inherited = inheritedPolicy(listing, { scope, repo, lanePolicy });
  const save = async () => {
    setBusy(true); setError(null);
    try {
      await api('/review-policies', { method: 'PUT', body: { scope, repo, number: scope === 'repo' ? 0 : number, reviewer: own ? draft : null } });
      onSaved?.(); onClose();
    } catch (e) { setError(e.message); } finally { setBusy(false); }
  };
  return html`<${Popover} onClose=${onClose} align=${align} className="review-policy-pop">
    <h2>${title}</h2>
    <${Seg} label="Policy source" value=${own} onChange=${setOwn}
      options=${[{ value: false, label: scope === 'ticket' ? 'Use the lane or default' : 'Use the default' }, { value: true, label: `Set for ${SCOPE_NAME[scope]}` }]} />
    ${own ? html`<${ReviewerEditor} value=${draft} onChange=${setDraft} paired=${scope === 'ticket'} />`
      : html`<p class="sub">${inherited ? `Uses ${inherited.source}: ${reviewerText(inherited.reviewer)}.` : 'Loading…'}</p>`}
    ${policy ? html`<p class="sub">${setByText(policy)}</p>` : null}
    <p class="sub">Changes apply to the next review request.</p>
    ${error ? html`<p class="err" role="alert">${error}</p>` : null}
    <div class="row"><button class="btn" disabled=${busy} onClick=${onClose}>Cancel</button>
      <button class="btn pri" disabled=${busy} onClick=${save}>${busy ? 'Saving…' : 'Save'}</button></div>
  <//>`;
}
