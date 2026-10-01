// Hand off… (spec 1782 appendix I): automatic handoff for an agent and its
// successors, or for the ticket it holds, then Hand off now. Opened from the
// agent band and from a Board ticket's ⋯.
import { useEffect, useState } from 'preact/hooks';
import { html, api, Popover, Seg, Toggle } from './ui.js';

/** 350000 → "350k", 1000000 → "1M": three significant figures. */
export function tokens(count) {
  if (!Number.isFinite(count)) return '';
  const round = value => Number(value.toPrecision(3));
  if (count >= 1e6 || round(count / 1e3) >= 1000) return `${round(count / 1e6)}M`;
  if (count >= 1e3) return `${round(count / 1e3)}k`;
  return String(round(count));
}

/** "350k of 1M tokens", or '' while the percent or window is unknown. */
export function tokensOf(percent, window) {
  const value = percent === '' || percent == null ? NaN : Number(percent);
  return Number.isFinite(value) && window ? `${tokens(window * value / 100)} of ${tokens(window)} tokens` : '';
}

const ticketPath = ({ repo, number }) => `/handoff-policy/ticket/${repo.split('/').map(encodeURIComponent).join('/')}/${number}`;
const SOURCE = { override: 'Set for this agent', ticket: 'From its ticket', default: 'Using the default' };

/**
 * `agent`: `{id, provider, state}` or null. `ticket`: `{repo: "owner/name",
 * number}` or null; without it the agent's ticket claim is used. `scope`
 * picks the first view: `agent` or `ticket`.
 */
export function HandoffPopover({ agent: given, ticket, scope: initialScope, align, onClose }) {
  // A Board holder carries only its id and name; its provider and model,
  // which set the window, come from `/watch/state`.
  const [watched, setWatched] = useState(null);
  useEffect(() => {
    if (!given?.id || given.model) return;
    let live = true;
    api(`/watch/state?session=${encodeURIComponent(given.id)}`)
      .then(doc => live && setWatched((doc?.sessions || []).find(session => session.id === given.id) || null)).catch(() => {});
    return () => { live = false; };
  }, [given?.id]);
  const agent = given && watched ? { ...watched, ...given, provider: watched.provider, model: watched.model } : given;
  const claim = (agent?.claims || []).find(item => item.kind === 'ticket');
  const held = ticket || (claim && { repo: claim.repo, number: claim.number });
  const forTicket = held && held.repo?.includes('/') ? held : null;
  const [scope, setScope] = useState(initialScope === 'ticket' && forTicket || !agent ? 'ticket' : 'agent');
  const [defaults, setDefaults] = useState(null);
  const [policy, setPolicy] = useState(null);
  const [draft, setDraft] = useState('');
  const [note, setNote] = useState('Loading…');
  const [asking, setAsking] = useState(false);
  const path = scope === 'ticket' ? forTicket && ticketPath(forTicket) : `/sessions/${encodeURIComponent(agent.id)}/handoff-policy`;
  const provider = agent?.provider === 'codex-fork' ? 'codex-fork' : 'claude';
  const base = defaults?.provider_thresholds?.[provider]?.threshold_percent;
  // The agent's own window: Claude's is 1M for a `[1m]` model, else 200k.
  // An unheld ticket has no provider yet: show only what the ticket sets.
  const window = !agent ? null : provider === 'claude' && agent.model ? (agent.model.endsWith('[1m]') ? 1e6 : 2e5) : defaults?.window_tokens?.[provider];
  // A ticket override leaves unset fields to each agent's provider default.
  const enabled = policy ? policy.enabled ?? (agent ? !!defaults?.providers?.[provider] : false) : false;
  const threshold = policy?.threshold_percent ?? (agent ? base : null);
  const describe = value => scope === 'ticket'
    ? value.enabled !== null || value.threshold_percent !== null ? `Set for ticket #${forTicket.number}`
      : agent ? 'Using the default' : 'Not set: the agent that takes this ticket uses its provider’s default'
    : value.has_gauge === false ? 'This agent has no context gauge, so it cannot hand off automatically.'
    : SOURCE[value.source] || '';
  useEffect(() => { api('/handoff-defaults').then(setDefaults).catch(() => {}); }, []);
  useEffect(() => {
    if (!path) return;
    let live = true;
    setPolicy(null); setNote('Loading…');
    api(path).then(value => { if (live) { setPolicy(value); setDraft(''); setNote(describe(value)); } })
      .catch(error => live && setNote(error.message));
    return () => { live = false; };
  }, [path]);
  const write = async body => {
    setNote('Saving…');
    try {
      const value = await api(body.ask_now ? `/sessions/${encodeURIComponent(agent.id)}/handoff-policy` : path, { method: 'PUT', body });
      if (!body.ask_now) { setPolicy(value); setDraft(''); }
      setNote(body.ask_now ? 'Asked the agent to hand off' : `Saved · ${describe(value)}`);
    } catch (error) {
      setNote(error.message);
    }
  };
  const commit = () => {
    if (draft === '') return;
    const number = Number(draft);
    if (!Number.isInteger(number) || number < 1 || number > 100) setNote('Enter a whole number from 1 to 100');
    else write({ threshold_percent: number });
  };
  const shown = draft !== '' ? draft : threshold ?? '';
  const live = agent && agent.state !== 'stopped';
  return html`<${Popover} onClose=${onClose} align=${align} className="handoff-pop">
    <h2>Automatic handoff</h2>
    ${agent && forTicket ? html`<${Seg} label="Handoff scope" value=${scope} onChange=${setScope}
      options=${[{ value: 'agent', label: 'This agent and its successors' }, { value: 'ticket', label: `Ticket #${forTicket.number}` }]} />` : null}
    ${policy ? html`<label class="check"><${Toggle} label="Hand off automatically" checked=${enabled}
          onChange=${value => write({ enabled: value })} /> Hand off automatically</label>
        <div class="fld"><span class="l">Hand off at</span>
          <span class="handoff-at"><input class="inp num" type="number" min="1" max="100" step="1" aria-label="Hand off at, percent of context"
            value=${shown} onInput=${event => setDraft(event.target.value)} onChange=${commit} />
            <span>%${tokensOf(shown, window) ? html` · <b>${tokensOf(shown, window)}</b>` : null}</span></span></div>
        <div class="row" style="justify-content:flex-start">
          <button type="button" class="btn sm" onClick=${() => write({ use_default: true })}>Use default</button></div>`
      : null}
    <span class="sub" role="status">${note}</span>
    ${live ? html`<hr class="pop-rule" />
      <div class="row" style="justify-content:flex-start">${asking
        ? html`<span class="confirm">Ask ${agent.name || 'this agent'} to hand off now?
            <button type="button" class="btn sm danger" onClick=${() => { setAsking(false); write({ ask_now: true }); }}>Confirm handoff</button>
            <button type="button" class="btn sm" onClick=${() => setAsking(false)}>Cancel</button></span>`
        : html`<button type="button" class="btn sm" onClick=${() => setAsking(true)}>Hand off now</button>`}</div>` : null}
  <//>`;
}
