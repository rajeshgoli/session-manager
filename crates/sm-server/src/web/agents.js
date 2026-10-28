// Agent facts, the hover card and the agent panel (spec 1710 D6.2; 1782 F; 2543 D). Data: GET /watch/state.
import { useEffect, useState } from 'preact/hooks';
import { safeThreadHtml, Thread } from './inbox.js';
import {
  html, api, inboxWrite, usePoll, useNow, config, age, clock,
  basename, homeRelative, providerLabel, Ring, Icon, Popover, Links,
  closePanel, openItem, navigate, newAgent, toast, registerPanel, submissionId, stored,
} from './ui.js';
import { HandoffPopover } from './handoff.js';
import { isShell, shellFact, ShellMenu, createShell, shellError } from './shell.js';

// ---- facts and attention (1782 B: the server computes both) ------------------

/** Attention sections, in display order. */
export const SECTIONS = ['blocked', 'you', 'finished', 'waiting_long', 'moving', 'waiting', 'idle', 'stopped'];
export const SECTION_LABEL = {
  blocked: 'Blocked on you', you: 'Needs you', finished: 'Finished', waiting_long: 'Waiting long', moving: 'Moving',
  waiting: 'Waiting', idle: 'Idle', stopped: 'Stopped',
};
/** Section colours (1782 A3). */
export const SECTION_TONE = {
  blocked: 'red', you: 'magenta', finished: 'cyan', waiting_long: 'amber', moving: 'green', waiting: 'amber', idle: 'muted', stopped: 'muted',
};

const sectionOf = (agent) => (agent.attention && agent.attention.section) || 'idle';
const sectionIndex = (agent) => {
  const index = SECTIONS.indexOf(sectionOf(agent));
  return index < 0 ? SECTIONS.indexOf('idle') : index;
};
const hasTicket = (agent) => (agent.claims || []).some((claim) => claim.kind === 'ticket');

/** Section, then the server's order key, then name (1782 B). */
export function attentionOrder(a, b) {
  const keyA = (a.attention && a.attention.order_key) || '';
  const keyB = (b.attention && b.attention.order_key) || '';
  return sectionIndex(a) - sectionIndex(b) || (keyA < keyB ? -1 : keyA > keyB ? 1 : 0) || a.name.localeCompare(b.name);
}

/** The card's left edge: its section colour, red for quiet or stalled, the line colour when idle. */
export function edgeTone(agent) {
  if (isShell(agent)) return 'line';
  const section = sectionOf(agent);
  const reason = agent.attention && agent.attention.reason;
  if (section === 'waiting_long' && (reason === 'quiet' || reason === 'stalled')) return 'red';
  if (section === 'idle' || section === 'stopped') return 'line';
  return SECTION_TONE[section];
}

/** Non-empty sections in order, each sorted by attention. */
export function sectionAgents(sessions) {
  const sorted = sessions.filter(session => !isShell(session)).sort(attentionOrder);
  return SECTIONS
    .map((section) => ({ section, agents: sorted.filter((agent) => sectionOf(agent) === section) }))
    .filter((group) => group.agents.length);
}

/** "● Working 3m" or "○ Idle 7m". */
export function agentFact(agent, now = Date.now()) {
  const fact = (agent.facts && agent.facts.agent) || { state: agent.state === 'stopped' ? 'stopped' : 'idle' };
  if (isShell(agent)) return shellFact(agent, now);
  const since = age(fact.since, now);
  if (fact.state === 'working') return { text: `● Working ${since}`.trim(), tone: 'green' };
  if (fact.state === 'parked') return { text: fact.text || 'Parked: model yielded', tone: 'amber' };
  return { text: `○ ${fact.state === 'stopped' ? 'Stopped' : 'Idle'} ${since}`.trim(), tone: 'muted' };
}

/** Commands an idle agent left running outside the queue (sm#2412). */
function BackgroundJobs({ jobs, now }) {
  if (!jobs || !jobs.length) return null;
  const memory = (bytes) => bytes >= 1073741824 ? `${(bytes / 1073741824).toFixed(1)} G` : `${Math.round(bytes / 1048576)} M`;
  return html`<section class="bg-jobs"><h4>Running in the background</h4>${jobs.map((job) => html`<div class="bg-job" key=${job.pid || job.agent_id}>
    <code title=${job.command || job.agent_type}>${job.command || `Agent: ${job.agent_type}`}</code>
    <small>${[job.started_at ? `started ${age(job.started_at, now)} ago` : null, job.memory_bytes != null ? memory(job.memory_bytes) : null].filter(Boolean).join(' · ')}</small>
  </div>`)}</section>`;
}

/** "▶ 2 running · 2h 56m", "⏸ Waiting 8m · 1st in line", or "No jobs". */
export function jobsFact(agent) {
  // A paired reviewer leads with its round and keeps its own jobs' status.
  const paired = pairedText(agent.paired_reviewer);
  const own = agent.facts && agent.facts.jobs;
  if (paired) {
    if (!own || !own.tone) return { text: paired.text, tone: paired.active ? 'amber' : 'muted' };
    return { text: `${paired.text} · ${own.running > 0 ? '▶' : '⏸'} ${own.text}`, tone: own.tone };
  }
  const jobs = agent.facts && agent.facts.jobs;
  if (!jobs || !jobs.tone) return { text: (jobs && jobs.text) || 'No jobs', tone: 'muted' };
  // Something the agent left running (sm#2412); the text carries its own ◷.
  if (jobs.tone === 'background') return { text: jobs.text, tone: 'cyan' };
  return { text: `${jobs.running > 0 ? '▶' : '⏸'} ${jobs.text}`, tone: jobs.tone };
}

/** A paired reviewer's line: reviewing a round, or idle between rounds (1768 I3). */
export function pairedText(paired) {
  if (!paired) return null;
  if (['waiting_reviewer', 'reviewing', 'nudged'].includes(paired.request_state)) {
    return { active: true, text: `Reviewing PR #${paired.pr_number} for ${paired.author_name || 'its author'} · round ${paired.round}` };
  }
  return { active: false, text: `Paired reviewer for #${paired.ticket} · idle` };
}

/** An author's line while sm finds and runs its review (1768 I3). */
export function reviewWaitText(waiting, review, now = Date.now()) {
  if (!waiting) return `Waiting on review of PR #${review.pr_number} · ${age(review.since, now)}`;
  return `Waiting on review by ${waiting.reviewer_label || 'sm'} · ${age(waiting.since, now)}`;
}

/** The You line: an open question, else the finished summary, else nothing. */
export function youFact(agent, now = Date.now()) {
  const facts = agent.facts || {};
  if (facts.you) {
    const more = facts.you.more ? ` +${facts.you.more}` : '';
    const blocked = facts.you.kind === 'blocked';
    return {
      text: `◆ ${blocked ? 'Blocked · ' : ''}${age(facts.you.since, now) || 'now'}: ${facts.you.text}${more}`,
      tone: blocked ? 'red' : 'magenta',
      dismissible: !!facts.you.dismissible,
    };
  }
  if (facts.finished && !facts.finished.read) return { text: `✔ ${facts.finished.text || 'Finishing…'}`, tone: 'cyan', dismissible: true };
  return null;
}

const CLEARED = { blocked: 'Marked answered', message: 'Marked answered', doc_review: 'No review needed', finished: 'Marked read' };

/** ✓: clear what the card shows — a question, a review request or a
 * Finished summary — as Inbox Done would (1782 C4, sm#1851). */
export async function markAnswered(agent, after) {
  try {
    const result = await api(`/sessions/${encodeURIComponent(agent.id)}/needs-you/answered`, { method: 'POST', body: {} });
    toast(CLEARED[result?.kind] || 'Marked answered');
    if (after) after();
  } catch (error) {
    toast(error.message);
  }
}

/** "#1854" and "+1" for more claims (D6.1). */
function ticketText(agent) {
  const claims = agent.claims || [];
  if (!claims.length) return 'no ticket';
  const first = claims[0];
  const label = first.kind === 'pr' ? `PR #${first.number}` : `#${first.number}`;
  return claims.length > 1 ? `${label} +${claims.length - 1}` : label;
}

/** The title of the claim ticketText names, shown under the agent's name (sm#1900); '' when none. */
export function ticketTitle(agent) {
  const first = (agent.claims || [])[0];
  return (first && first.title && first.title.trim()) || '';
}

/** Settings › Appearance › Ticket titles; on unless turned off in this browser. */
export const TITLES_KEY = 'sm-agent-titles';

const claudeLink = (agent) => {
  const url = agent.remote_control && agent.remote_control.url;
  return url && url.startsWith('https://claude.ai/code/') ? url : null;
};

/** The docked Claude window (D6.2): right half of the screen, reused per agent. */
export function openInClaude(agent) {
  const url = claudeLink(agent);
  if (!url) return;
  const s = window.screen;
  const left = (s.availLeft || 0) + s.availWidth / 2;
  window.open(url, `sm-claude-${agent.id}`,
    `popup,left=${left},top=${s.availTop || 0},width=${s.availWidth / 2},height=${s.availHeight}`);
}

const openTerminal = (agent) => navigate(`/terminal/${encodeURIComponent(agent.id)}`);

// A long finished summary stays readable on hover without a huge tooltip.
const hoverText = (text) => (text.length > 1000 ? `${text.slice(0, 1000)}…` : text);

/** A finished agent is safe to retire immediately only after its current turn is idle. */
export const canRetireImmediately = (agent) => !!agent.facts?.finished && agent.facts?.agent?.state === 'idle';

/** `onRetired(true)` after a retire; `onRetired(false)` when the agent remains live. */
export function RetireButton({ agent, onRetired, small = false }) {
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const retire = async (ifFinishedIdle = false) => {
    if (busy) return;
    setBusy(true);
    try {
      if (isShell(agent)) {
        await api(`/terminals/${encodeURIComponent(agent.id)}`, { method: 'DELETE' });
        toast(`Retired ${agent.name}`);
        setAsking(false);
        onRetired?.(true);
        return;
      }
      await api(`/sessions/${encodeURIComponent(agent.id)}/retire`, {
        method: 'POST', body: ifFinishedIdle ? { if_finished_idle: true } : {},
      });
      // Undo restores it, worktree and claims included (sm#1839), for 10 s.
      let undone = false;
      toast(`Retired ${agent.name}`, null, { ms: 10000, action: { label: 'Undo', run: async () => {
        if (undone) return;
        undone = true;
        try {
          await api(`/sessions/${encodeURIComponent(agent.id)}/restore`, { method: 'POST', body: {} });
          toast(`Restored ${agent.name}`);
          onRetired?.(false);
        } catch (error) { toast(error.message); }
      } } });
      setAsking(false);
      onRetired?.(true);
    } catch (error) {
      if (ifFinishedIdle && error.status === 409) {
        setAsking(true);
        onRetired?.(false);
      } else {
        toast(error.message);
      }
    } finally {
      setBusy(false);
    }
  };
  const click = (event) => {
    event.stopPropagation();
    if (asking) retire();
    else if (canRetireImmediately(agent)) retire(true);
    else setAsking(true);
  };
  return html`<span class="retire-action" onClick=${(event) => event.stopPropagation()}>
    <button type="button" class=${`btn danger ${small ? 'sm' : ''}`} disabled=${busy}
      onClick=${click}>Retire</button>
    ${asking ? html`<span class="retire-confirm">Retire ${agent.name}?
      <button type="button" class="btn sm" disabled=${busy} onClick=${() => setAsking(false)}>Cancel</button>
    </span>` : null}
  </span>`;
}

/** The Agent, Jobs and You facts (1782 F), shared by the card and the details band. */
function Facts({ agent, now, onAnswered, onRetired, cardSection }) {
  if (isShell(agent)) {
    const fact = shellFact(agent, now);
    return html`<span class="facts"><span class="fa muted" title=${fact.text}>${fact.text}</span></span>`;
  }
  const working = agentFact(agent, now);
  const jobs = jobsFact(agent);
  const you = youFact(agent, now);
  return html`<span class="facts">
      <span class=${`fa ${working.tone}`} title=${working.text}>${working.text}</span>
      <span class=${`fa ${jobs.tone}`} title=${jobs.text}>${jobs.text}</span>
    </span>
    ${you
      ? html`<span class=${`you ${you.tone}`}>
          <span class="fa" title=${hoverText(you.text)}>${you.text}</span>
          ${you.dismissible
            ? html`<button type="button" class="icon-btn ok" title=${you.tone === 'cyan' ? 'Mark read (x)' : 'Mark answered (x)'} aria-label=${you.tone === 'cyan' ? 'Mark read' : 'Mark answered'}
                onClick=${(event) => { event.stopPropagation(); markAnswered(agent, onAnswered); }}>✓</button>`
            : null}
          ${cardSection === 'finished' && agent.state !== 'stopped'
            ? html`<${RetireButton} agent=${agent} onRetired=${onRetired} small />` : null}</span>`
      : null}
    ${agent.facts?.note
      ? html`<span class="you note" title=${agent.facts.note.text}><span class="fa">📌 ${agent.facts.note.text}</span></span>`
      : null}`;
}

/** Pin a note saying why the agent waits; it replaces "stalled" (sm#1851). Opened from ⋯. */
function NoteEditor({ agent, onSaved, onClose }) {
  const current = agent.facts?.note?.text || '';
  const [text, setText] = useState(current);
  const [busy, setBusy] = useState(false);
  useEffect(() => setText(current), [agent.id, current]);
  const save = async (value) => {
    setBusy(true);
    try {
      await api(`/sessions/${encodeURIComponent(agent.id)}/note`, { method: 'PUT', body: { text: value } });
      toast(value.trim() ? 'Note pinned' : 'Note removed');
      if (onSaved) onSaved();
      onClose();
    } catch (error) {
      toast(error.message);
    } finally {
      setBusy(false);
    }
  };
  return html`<div class="note-edit">
    <input type="text" maxlength="200" aria-label="Note" autofocus placeholder="Why it waits, e.g. Waiting for the midnight window"
      value=${text} disabled=${busy} onInput=${(e) => setText(e.target.value)}
      onKeyDown=${(e) => { if (e.key === 'Escape') onClose(); if (e.key === 'Enter' && text.trim() !== current) save(text); }} />
    <button type="button" class="btn sm" disabled=${busy || !text.trim() || text.trim() === current}
      onClick=${() => save(text)}>Pin</button>
    ${current ? html`<button type="button" class="btn sm" disabled=${busy} onClick=${() => save('')}>Remove</button>` : null}
    <button type="button" class="btn sm" disabled=${busy} onClick=${onClose}>Cancel</button>
  </div>`;
}

/** Rename through PATCH /sessions/{id}; the server enforces sm's name rules. */
function RenameEditor({ agent, onSaved, onClose }) {
  const [text, setText] = useState(agent.name);
  const [busy, setBusy] = useState(false);
  const name = text.trim();
  const save = async () => {
    if (!name || name === agent.name || busy) return;
    setBusy(true);
    try {
      await api(`/sessions/${encodeURIComponent(agent.id)}`, { method: 'PATCH', body: { friendly_name: name } });
      toast(`Renamed to ${name}`);
      onSaved?.();
      onClose();
    } catch (error) {
      toast(error.message);
    } finally {
      setBusy(false);
    }
  };
  return html`<div class="note-edit">
    <input type="text" maxlength="32" aria-label="Name" autofocus value=${text} disabled=${busy}
      onInput=${(e) => setText(e.target.value)}
      onKeyDown=${(e) => { if (e.key === 'Escape') onClose(); if (e.key === 'Enter') save(); }} />
    <button type="button" class="btn sm" disabled=${busy || !name || name === agent.name} onClick=${save}>Rename</button>
    <button type="button" class="btn sm" disabled=${busy} onClick=${onClose}>Cancel</button>
  </div>`;
}

// ---- host restart ---------------------------------------------------------------

/**
 * After a host restart (sm#2054): who it interrupted, why, and one button
 * that restores them all. Each row restores or retires one agent.
 */
export function HostRestartBanner() {
  const [doc, , reload] = usePoll(() => api('/host-restarts/latest'), 60000);
  const [busy, setBusy] = useState(false);
  const restart = doc && doc.restart;
  if (!restart || !restart.open_count) return null;
  const open = (restart.members || []).filter((member) => member.open);
  const act = async (path, body, done) => {
    setBusy(true);
    try {
      const result = await api(path, { method: 'POST', body });
      const failed = (result.results || []).filter((item) => item.outcome === 'failed');
      toast(failed.length ? `${failed.length} could not be restored: ${failed[0].error}` : done);
    } catch (error) {
      toast(error.message);
    }
    setBusy(false);
    reload();
  };
  const base = `/host-restarts/${encodeURIComponent(restart.id)}`;
  return html`<div class="review-banner restart-banner" role="status">
    <span class="chip amber">Restart</span>
    <span><b>The Mac restarted at ${restart.restarted_at_text}</b> — ${open.length} agent${open.length === 1 ? ' was' : 's were'} interrupted.
      <span class="muted"> ${restart.cause_summary}</span></span>
    <button type="button" class="btn pri" disabled=${busy}
      onClick=${() => act(`${base}/restore`, {}, `Restored ${open.length} agents`)}>Restore all</button>
    <div class="restart-members">${open.map((member) => html`<div key=${member.session_id}>
      <b>${member.name}</b> <span class="muted">${member.mid_turn ? 'mid-turn' : 'idle'}${member.decision === 'failed' ? ` · restore failed: ${member.error}` : ''}${(member.killed_jobs || []).length ? ` · killed: ${member.killed_jobs.map((job) => job.label).join(', ')}` : ''}</span>
      <button type="button" class="btn sm" disabled=${busy}
        onClick=${() => act(`${base}/restore`, { session_ids: [member.session_id] }, `Restored ${member.name}`)}>Restore</button>
      <button type="button" class="btn sm" disabled=${busy}
        onClick=${() => act(`${base}/members/${encodeURIComponent(member.session_id)}/leave`, undefined, `Left ${member.name} retired`)}>Leave retired</button>
    </div>`)}</div>
  </div>`;
}

// ---- hover card (2543 D.3) ----------------------------------------------------

const refreshMs = () => Math.max(1, config.refresh_seconds || 3) * 1000;

/** Today's Agents page card for one agent; a click or its terminal icon opens the terminal. */
export function AgentCard({ agent, now, onAnswered, onRetired, onOpen = () => openTerminal(agent) }) {
  const codex = (agent.provider || '').startsWith('codex');
  const section = sectionOf(agent);
  const faded = ['idle', 'stopped'].includes(section) && !hasTicket(agent);
  const linked = claudeLink(agent);
  const live = agent.state !== 'stopped';
  const tall = !!youFact(agent, now);
  const title = stored(TITLES_KEY, true) !== false ? ticketTitle(agent) : '';
  const cls = ['card', 'acard', `edge-${edgeTone(agent)}`, tall && 'tall', title && 'titled', faded && 'faded'].filter(Boolean).join(' ');
  const iconButton = (title, name, action) => html`<button type="button" class="icon-btn" title=${title} aria-label=${title}
    onClick=${(event) => { event.stopPropagation(); action(); }}><${Icon} name=${name} /></button>`;
  return html`<div class=${cls} role="button" tabindex="0" onClick=${onOpen}
    onKeyDown=${(event) => event.key === 'Enter' && event.target === event.currentTarget && onOpen()}>
    ${isShell(agent) ? html`<span class="ring none">▹</span>` : html`<${Ring} percent=${agent.context_percent} />`}
    <span class="nm" title=${agent.name}>${agent.name}</span>
    <span class="icons">
      ${live ? iconButton('Terminal', 'terminal', onOpen) : null}
      ${linked ? iconButton('Open in Claude', 'external', () => openInClaude(agent)) : null}
    </span>
    ${title ? html`<span class="ttl" title=${title}>${title}</span>` : null}
    <span class="ln"><span class=${`prov ${isShell(agent) ? 'muted' : agent.provider === 'opencode' ? 'local' : codex ? 'codex' : 'claude'}`}>${isShell(agent) ? 'TERMINAL' : providerLabel(agent.provider).toUpperCase()}</span>
      ${isShell(agent) ? null : html`<span class="tk"> ${ticketText(agent)}</span>`}
      ${agent.repo ? html`<span class="repo"> ${basename(agent.repo)}</span>` : null}</span>
    <${Facts} agent=${agent} now=${now} onAnswered=${onAnswered} onRetired=${onRetired} cardSection=${section} />
  </div>`;
}

// ---- agent panel -------------------------------------------------------------

// Display retained work without projecting ended claims as current ownership.
export function detailClaims(active = [], retained = []) {
  const items = new Map();
  for (const claim of [...active, ...retained.slice().reverse()]) {
    const key = `${claim.repo}#${claim.number}:${claim.kind}`;
    if (!items.has(key)) items.set(key, claim);
  }
  return [...items.values()];
}

function useAgent(id) {
  const [doc, error, reload] = usePoll(async () => {
    const watch = await api(`/watch/state?session=${encodeURIComponent(id)}`);
    const shell = (watch.sessions || []).some(s => s.id === id && isShell(s));
    const history = shell ? {} : await api(`/claims?session=${encodeURIComponent(id)}&active=false`);
    return { ...watch, retainedClaims: history.claims };
  }, refreshMs(), [id]);
  const agent = doc && (doc.sessions || []).find((session) => session.id === id);
  return { agent, claims: detailClaims(agent?.claims, doc?.retainedClaims), reload, loading: !doc && !error, error: doc && !agent ? new Error('This agent is not known to sm.') : error };
}

/** `beside` is 'terminal' when the panel sits next to that agent's terminal, whose bar carries
 * every action, so the panel is details only (sm#2270, sm#2277). */
export function AgentPanel({ id, controls, beside }) {
  const { agent, claims, reload, loading, error } = useAgent(id);
  const now = useNow(15000);
  if (!agent) {
    return html`<div class="phd"><span class="ring none">–</span><span class="t">${loading ? 'Loading…' : 'Agent'}</span>
      ${controls}<span class="s">${error ? error.message : ''}</span></div>`;
  }
  if (isShell(agent)) return html`<div class="phd"><span class="ring none">▹</span><span class="t">${agent.name}</span>${controls}
      <span class="s">Terminal · ${shellFact(agent, now).text}</span></div>
    ${beside === 'terminal' ? null : html`<${AgentActions} agent=${agent} onSaved=${reload} onRetired=${done => { if (done) closePanel(); reload(); }} />`}
    <div class="pbody"><h3>Folder</h3><p class="mono">${homeRelative(agent.working_dir || agent.repo)}</p></div>`;
  const since = agent.state === 'stopped' ? agent.last_activity : agent.activity_since || agent.last_activity;
  const parts = [
    providerLabel(agent.provider),
    agent.model,
    agent.reasoning_effort,
    `${agent.state === 'working' ? 'working' : agent.state} since ${clock(since)}`,
  ].filter(Boolean);
  return html`
    <div class="phd">
      <${Ring} percent=${agent.context_percent} suffix="%" />
      <span class="t" title=${agent.name}>${agent.name}</span>
      ${controls}
      <span class="s">${parts.join(' · ')}</span>
    </div>
    ${beside === 'terminal' ? null : html`<${AgentActions} agent=${agent} onSaved=${reload}
      onRetired=${(done) => { if (done) closePanel(); reload(); }} />`}
    <${Links} prs=${claims.filter(item => item.kind === 'pr')}
      jobs=${agent.jobs || []} docs=${agent.docs || []} />
    <${BackgroundJobs} jobs=${[...(agent.background_jobs || []), ...(agent.background_agents || [])]} now=${now} />
    <${AgentStatus} agent=${agent} claims=${claims} now=${now} onAnswered=${reload} />
    <${Thread} key=${agent.id} id=${agent.id} embedded
      empty=${html`<div class="pbody"><${LastTurn} id=${agent.id} /></div>${agent.state !== 'stopped' ? html`<${MessageBox} agent=${agent} />` : null}`} />
  `;
}

function AgentActions({ agent, onSaved, onRetired }) {
  const linked = claudeLink(agent);
  const live = agent.state !== 'stopped';
  return html`<div class="acts">
    ${linked ? html`<button type="button" class="btn pri" onClick=${() => openInClaude(agent)}>Open in Claude ↗</button>` : null}
    ${live
      ? html`<button type="button" class=${linked ? 'btn' : 'btn pri'}
          onClick=${() => navigate(`/terminal/${encodeURIComponent(agent.id)}`)}>⌨ Terminal</button>`
      : null}
    ${live ? html`<${RetireButton} agent=${agent} onRetired=${onRetired} />` : null}
    <${AgentMenus} agent=${agent} onSaved=${onSaved} />
  </div>`;
}

/** Hand off… and ⋯, in the Details panel and in the terminal bar. `compact` folds Hand off…
 * into ⋯ for the phone's bar; `small` sizes the buttons for a bar. */
export function AgentMenus({ agent, onSaved, compact = false, small = false, align }) {
  const [menu, setMenu] = useState(null);
  const live = agent.state !== 'stopped';
  const close = () => setMenu(null);
  const btn = small ? 'btn sm' : 'btn';
  if (isShell(agent)) return html`<${ShellMenu} shell=${agent} small=${small} align=${align} />`;
  return html`${live && !compact
      ? html`<span class="anchor"><button type="button" class=${btn} data-pop-anchor
          onClick=${() => setMenu(menu === 'handoff' ? null : 'handoff')}>Hand off…</button>
          ${menu === 'handoff' ? html`<${HandoffPopover} agent=${agent} align=${align} onClose=${close} showAskNow=${false} />` : null}</span>`
      : null}
    <span class="anchor"><button type="button" class=${btn} data-pop-anchor aria-label="More"
      onClick=${() => setMenu(menu === 'more' || (compact && menu) ? null : 'more')}>⋯</button>
      ${menu === 'more' ? html`<${MoreMenu} agent=${agent} align=${align} onClose=${close} onSaved=${onSaved}
        onHandoff=${live && compact ? () => setMenu('handoff') : null} />` : null}
      ${menu === 'handoff' && compact ? html`<${HandoffPopover} agent=${agent} align=${align} onClose=${close} showAskNow=${false} />` : null}</span>`;
}

function FollowButton({ id, menu = false, onClose }) {
  const [follows, , reload] = usePoll(() => api('/client/follows'), 30000, [id]);
  const [busy, setBusy] = useState(false);
  const following = !!(follows && (follows.follows || []).some(
    (follow) => follow.target_kind === 'session' && follow.session_id === id && follow.state !== 'done',
  ));
  const toggle = async () => {
    setBusy(true);
    try {
      await api(`/sessions/${encodeURIComponent(id)}/follow`, { method: following ? 'DELETE' : 'POST', body: {} });
      reload();
      onClose?.();
    } catch (error) {
      toast(error.message);
    } finally {
      setBusy(false);
    }
  };
  return html`<button type="button" class=${menu ? '' : following ? 'btn on' : 'btn'} aria-pressed=${following}
    disabled=${busy || !follows} onClick=${toggle}>${following ? menu ? 'Unfollow' : 'Following' : 'Follow'}</button>`;
}

function MoreMenu({ agent, align, onClose, onSaved, onHandoff }) {
  const [asking, setAsking] = useState(false);
  const [editing, setEditing] = useState(null);
  const [busy, setBusy] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(agent.attach);
      toast(`Copied: ${agent.attach}`);
    } catch (error) {
      toast(agent.attach);
    }
    onClose();
  };
  const handoffNow = async () => {
    setBusy(true);
    try {
      await api(`/sessions/${encodeURIComponent(agent.id)}/handoff-policy`, { method: 'PUT', body: { ask_now: true } });
      toast(`Asked ${agent.name} to hand off`);
      onClose();
    } catch (error) {
      toast(error.message);
    } finally {
      setBusy(false);
    }
  };
  if (editing) {
    const Editor = editing === 'rename' ? RenameEditor : NoteEditor;
    // A fresh Popover, so it re-fits the viewport for the wider editor.
    return html`<${Popover} key=${editing} onClose=${onClose} align=${align}><${Editor} agent=${agent} onSaved=${onSaved} onClose=${onClose} /><//>`;
  }
  return html`<${Popover} onClose=${onClose} align=${align} className="menu">
    ${onHandoff ? html`<button type="button" onClick=${onHandoff}>Hand off…</button>` : null}
    ${agent.state !== 'stopped'
      ? asking
        ? html`<div class="confirm" style="padding:6px 10px">Ask ${agent.name} to hand off now?
            <button type="button" class="btn sm danger" disabled=${busy} onClick=${handoffNow}>Confirm handoff</button>
            <button type="button" class="btn sm" disabled=${busy} onClick=${() => setAsking(false)}>Cancel</button></div>`
        : html`<button type="button" onClick=${() => setAsking(true)}>Hand off now</button>`
      : null}
    <button type="button" onClick=${() => { onClose(); newAgent({
      provider: agent.provider, model: agent.model, effort: agent.reasoning_effort, workspace: agent.working_dir,
    }); }}>Clone</button>
    <button type="button" disabled=${busy} onClick=${async () => {
      setBusy(true);
      try { await createShell({ from_session: agent.id }); onClose(); }
      catch (error) { toast(shellError(error)); }
      finally { setBusy(false); }
    }}>New shell here</button>
    <${FollowButton} id=${agent.id} menu onClose=${onClose} />
    <button type="button" onClick=${copy}>Copy attach command</button>
    <button type="button" onClick=${() => setEditing('rename')}>Rename…</button>
    ${agent.state !== 'stopped' || agent.facts?.note ? html`<button type="button" onClick=${() => setEditing('note')}>
      ${agent.facts?.note ? 'Edit note…' : 'Pin a note…'}</button>` : null}
  <//>`;
}

/** Above the conversation (sm#2280): where the agent stands. The link chips carry PRs, jobs and
 * docs; the thread below carries its turns and your messages. */
function AgentStatus({ agent, claims, now, onAnswered }) {
  const reviews = (agent.waiting_on || []).filter((w) => w.kind === 'review');
  const tickets = claims.filter((claim) => claim.kind === 'ticket');
  return html`<div class="pbody agent-status">
    <div class="acard-facts"><${Facts} agent=${agent} now=${now} onAnswered=${onAnswered} /></div>
    ${tickets.map((claim) => html`<button type="button" class="plain-btn agent-ticket"
      onClick=${() => openItem('ticket', `${claim.repo}#${claim.number}`, claim.url)}>
      <span class="tk">#${claim.number}</span> ${claim.title || claim.repo}${claim.state ? html` <span class="sub">· ${claim.state}</span>` : null}${claim.ended_at ? html` <span class="sub">· claim ended</span>` : null}</button>`)}
    ${reviews.map((review) => html`<div><span class="ball amber">${reviewWaitText(agent.waiting_on_review, review, now)}</span>
      <span class="sub">PR #${review.pr_number} · requested ${age(review.since, now)} ago</span></div>`)}
    ${agent.status_text ? html`<div class="quote">${agent.status_text}${agent.status_at ? html` <span class="sub">· ${clock(agent.status_at)}</span>` : null}</div>` : null}
    <div class="sub mono">${homeRelative(agent.working_dir || agent.repo)}</div>
  </div>`;
}

// What the agent wrote at the end of its latest turn; none yet is a 404.
function LastTurn({ id }) {
  const [turn] = usePoll(() => api(`/sessions/${encodeURIComponent(id)}/last-turn`).catch(() => null), 30000, [id]);
  if (!turn) return null;
  return html`<section><h3>Last turn</h3>
    <div class="quote last-turn md" dangerouslySetInnerHTML=${{ __html: safeThreadHtml(turn.html || '') }} />
    <div class="last-turn-foot"><span class="sub">${clock(turn.at)}</span></div></section>`;
}

/** An owner message, as a reply sent from the phone (POST /inbox/agent/{id}/send). */
function MessageBox({ agent }) {
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const send = async () => {
    const body = text.trim();
    if (!body || busy) return;
    setBusy(true);
    try {
      await inboxWrite(`/inbox/agent/${encodeURIComponent(agent.id)}/send`, { submission_id: submissionId(), body });
      setText('');
      toast(`Sent to ${agent.name}`);
    } catch (error) {
      toast(error.status === 401 || error.status === 403 ? 'Could not send: reload the page and try again.' : error.message);
    } finally {
      setBusy(false);
    }
  };
  return html`<div class="msgbox">
    <textarea class="inp" rows="1" placeholder=${`Message ${agent.name}…`} value=${text}
      onInput=${(event) => setText(event.target.value)}
      onKeyDown=${(event) => { if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); send(); } }}></textarea>
    <button type="button" class="btn pri" disabled=${busy || !text.trim()} onClick=${send}>↵</button>
  </div>`;
}

registerPanel('agent', AgentPanel);
