// Agents page and agent panel (spec 1710 D6.1, D6.2; 1782 F). Data: GET /watch/state.
import { useEffect, useRef, useState } from 'preact/hooks';
import { safeThreadHtml } from './inbox.js';
import {
  html, api, usePoll, useNow, config, age, limitText, clock,
  basename, homeRelative, providerLabel, Ring, Icon, Popover, Seg, Toggle, Links,
  openPanel, openItem, navigate, newAgent, toast, registerPanel, submissionId, stored, store, typingIn,
} from './ui.js';
import { HandoffPopover } from './handoff.js';

// ---- facts and attention (1782 B: the server computes both) ------------------

/** Attention sections, in display order. */
export const SECTIONS = ['you', 'finished', 'waiting_long', 'moving', 'waiting', 'idle', 'stopped'];
export const SECTION_LABEL = {
  you: 'Needs you', finished: 'Finished', waiting_long: 'Waiting long', moving: 'Moving',
  waiting: 'Waiting', idle: 'Idle', stopped: 'Stopped',
};
/** The summary strip's words for each count. */
const COUNT_LABEL = {
  you: 'needs you', finished: 'finished', waiting_long: 'waiting long', moving: 'moving', waiting: 'waiting', idle: 'idle',
};
/** Section colours (1782 A3). */
export const SECTION_TONE = {
  you: 'magenta', finished: 'cyan', waiting_long: 'amber', moving: 'green', waiting: 'amber', idle: 'muted', stopped: 'muted',
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
  const section = sectionOf(agent);
  const reason = agent.attention && agent.attention.reason;
  if (section === 'waiting_long' && (reason === 'quiet' || reason === 'stalled')) return 'red';
  if (section === 'idle' || section === 'stopped') return 'line';
  return SECTION_TONE[section];
}

/** Non-empty sections in order, each sorted by attention. */
export function sectionAgents(sessions) {
  const sorted = [...sessions].sort(attentionOrder);
  return SECTIONS
    .map((section) => ({ section, agents: sorted.filter((agent) => sectionOf(agent) === section) }))
    .filter((group) => group.agents.length);
}

/**
 * Idle agents without a ticket beyond the fourth fold away (1782 F); nothing
 * else folds, and the open agent (`keepId`) stays out of the fold.
 */
export function foldIdle(agents, keepId = null, keep = 4) {
  const shown = [];
  const folded = [];
  let loose = 0;
  for (const agent of agents) {
    if (hasTicket(agent) || agent.id === keepId || loose++ < keep) shown.push(agent);
    else folded.push(agent);
  }
  return { shown, folded };
}

/** "+ 3 more idle: sm-1768 (11m), …" */
export function foldText(folded, now = Date.now()) {
  const names = folded.map((agent) => `${agent.name} (${age(agent.facts && agent.facts.agent && agent.facts.agent.since, now) || '–'})`);
  return `+ ${folded.length} more idle: ${names.join(', ')}`;
}

/** The summary strip: zero counts drop out, except needs you. */
export function summaryCounts(counts) {
  const c = counts || {};
  return Object.keys(COUNT_LABEL)
    .map((section) => ({ section, n: c[section === 'you' ? 'needs_you' : section] || 0, label: COUNT_LABEL[section] }))
    .filter(({ section, n }) => n > 0 || section === 'you');
}

/** "● Working 3m" or "○ Idle 7m". */
export function agentFact(agent, now = Date.now()) {
  const fact = (agent.facts && agent.facts.agent) || { state: agent.state === 'stopped' ? 'stopped' : 'idle' };
  const since = age(fact.since, now);
  if (fact.state === 'working') return { text: `● Working ${since}`.trim(), tone: 'green' };
  return { text: `○ ${fact.state === 'stopped' ? 'Stopped' : 'Idle'} ${since}`.trim(), tone: 'muted' };
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
    return {
      text: `◆ ${age(facts.you.since, now) || 'now'}: ${facts.you.text}${more}`,
      tone: 'magenta',
      dismissible: !!facts.you.dismissible,
    };
  }
  if (facts.finished) return { text: `✔ ${facts.finished.text || 'Finishing…'}`, tone: 'cyan', dismissible: true };
  return null;
}

const CLEARED = { message: 'Marked answered', doc_review: 'No review needed', finished: 'Marked read' };

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

/** `onRetired(true)` after a retire; `onRetired(false)` when a busy agent needs the confirmation instead. */
export function RetireButton({ agent, onRetired, small = false }) {
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const retire = async (ifFinishedIdle = false) => {
    if (busy) return;
    setBusy(true);
    try {
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
          onRetired?.(true);
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
    if (canRetireImmediately(agent)) retire(true);
    else setAsking(true);
  };
  return asking
    ? html`<span class="confirm retire-confirm" onClick=${(event) => event.stopPropagation()}>Retire ${agent.name}?
        <button type="button" class="btn sm danger" disabled=${busy} onClick=${() => retire()}>Retire</button>
        <button type="button" class="btn sm" disabled=${busy} onClick=${() => setAsking(false)}>Cancel</button></span>`
    : html`<button type="button" class=${`btn danger ${small ? 'sm' : ''}`} disabled=${busy}
        onClick=${click}>Retire</button>`;
}

/** The Agent, Jobs and You facts (1782 F), shared by the card and the details band. */
function Facts({ agent, now, onAnswered, onRetired, cardSection }) {
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

/** Pin a note saying why the agent waits; it replaces "stalled" (sm#1851). */
function NoteEditor({ agent, onSaved }) {
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
    } catch (error) {
      toast(error.message);
    } finally {
      setBusy(false);
    }
  };
  return html`<section><h3>Note</h3><div class="note-edit">
    <input type="text" maxlength="200" placeholder="Why it waits, e.g. Waiting for the midnight window"
      value=${text} disabled=${busy} onInput=${(e) => setText(e.target.value)}
      onKeyDown=${(e) => { if (e.key === 'Enter' && text.trim() !== current) save(text); }} />
    <button type="button" class="btn sm" disabled=${busy || !text.trim() || text.trim() === current}
      onClick=${() => save(text)}>Pin</button>
    ${current ? html`<button type="button" class="btn sm" disabled=${busy} onClick=${() => save('')}>Remove</button>` : null}
  </div></section>`;
}

// ---- page -------------------------------------------------------------------

const refreshMs = () => Math.max(1, config.refresh_seconds || 3) * 1000;

export function AgentsPage({ openRef }) {
  const [filter, setFilter] = useState(() => stored('sm-agents-filter', 'live'));
  const [view, setView] = useState(() => (stored('sm-agents-view', 'attention') === 'repo' ? 'repo' : 'attention'));
  const [unfold, setUnfold] = useState(false);
  const [cursor, setCursor] = useState(null);
  const [jump, setJump] = useState(null);
  const [doc, error, reload] = usePoll(
    () => api(`/watch/state${filter === 'stopped' ? '?stopped=1' : ''}`),
    // Stopped agents make the state large and slow, so that view polls less.
    filter === 'stopped' ? 30000 : refreshMs(),
    [filter],
  );
  const now = useNow(15000);
  const selected = openRef && openRef.startsWith('agent:') ? openRef.slice(6) : null;
  const keys = useRef({ order: [], cursor: null });
  const choose = (value) => {
    setFilter(value);
    store('sm-agents-filter', value);
  };
  const chooseView = (value) => {
    setView(value);
    store('sm-agents-view', value);
  };
  useEffect(() => { if (selected) setCursor(selected); }, [selected]);
  useAgentKeys(keys, setCursor, reload);
  useEffect(() => {
    if (!jump || view !== 'attention') return;
    const target = document.getElementById(`sec-${jump}`);
    if (target) target.scrollIntoView({ block: 'start', behavior: 'smooth' });
    setJump(null);
  }, [jump, view]);
  useEffect(() => {
    const node = cursor && document.querySelector('.card.kb');
    if (node) node.scrollIntoView({ block: 'nearest' });
  }, [cursor]);

  if (!doc) {
    return html`<div class="content">${error ? html`<p class="err">${error.message}</p>` : html`<p class="muted">Loading…</p>`}</div>`;
  }
  const sessions = doc.sessions || [];
  const titles = stored(TITLES_KEY, true) !== false;
  const order = [];
  const card = (agent, depth = 0) => {
    order.push(agent);
    return html`<${AgentCard} key=${agent.id} agent=${agent} depth=${depth} now=${now} showRepo=${view === 'attention'}
      titles=${titles} selected=${agent.id === selected} cursor=${agent.id === cursor} onAnswered=${reload} onRetired=${reload} />`;
  };
  const sections = view === 'attention' ? sectionAgents(sessions) : [];
  const groups = view === 'repo' ? groupAgents(sessions) : [];
  const body = view === 'attention'
    ? sections.map(({ section, agents }) => {
      let shown = agents;
      let folded = [];
      if (section === 'idle' && !unfold) ({ shown, folded } = foldIdle(agents, selected));
      return html`<div class=${`grp sec ${SECTION_TONE[section]}`} id=${`sec-${section}`}>
          ${SECTION_LABEL[section]}${['idle', 'stopped'].includes(section) ? ` · ${agents.length}` : ''}</div>
        <div class="cards">
          ${shown.map((agent) => card(agent))}
          ${folded.length
            ? html`<button type="button" class="fold-more" onClick=${() => setUnfold(true)}>${foldText(folded, now)}</button>`
            : null}
        </div>`;
    })
    : groups.map((group) => html`<div class="grp">${basename(group.repo)}</div>
        <div class="cards">${group.agents.map(({ agent, depth }) => card(agent, depth))}</div>`);
  keys.current = { order, cursor };

  return html`<div class="content">
    <div class="toolbar">
      <div class="sum">
        ${summaryCounts(doc.counts).map(({ section, n, label }, index) => html`${index ? html`<span class="dot">·</span>` : null}
          <button type="button" class=${`plain-btn ${n ? SECTION_TONE[section] : ''}`}
            onClick=${() => { chooseView('attention'); setJump(section); }}><b>${n}</b> ${label}</button>`)}
      </div>
      <span style="flex:1"></span>
      <${Seg} label="Order" value=${view} onChange=${chooseView}
        options=${[{ value: 'attention', label: 'By attention' }, { value: 'repo', label: 'By repo' }]} />
      <${Seg} label="Show" value=${filter} onChange=${choose}
        options=${[{ value: 'live', label: 'Live' }, { value: 'stopped', label: 'With stopped' }]} />
    </div>
    ${!sessions.some((agent) => agent.state !== 'stopped')
      ? html`<p class="empty">No live agents. Start one with New agent.</p>`
      : null}
    ${body}
  </div>`;
}

/**
 * The page's keys, when not typing (1782 F): j and k move the selection ring,
 * Enter opens the details band, t the terminal, c Claude, x presses ✓.
 */
function useAgentKeys(keys, setCursor, reload) {
  useEffect(() => {
    let lastG = 0;
    const down = (event) => {
      if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey || typingIn(event)) return;
      // "g a" and friends belong to the shell's page jumps.
      if (event.key === 'g') { lastG = Date.now(); return; }
      if (lastG && Date.now() - lastG < 1500) { lastG = 0; return; }
      const { order, cursor } = keys.current;
      const index = order.findIndex((agent) => agent.id === cursor);
      const agent = index >= 0 ? order[index] : null;
      if (event.key === 'j' || event.key === 'k') {
        if (!order.length) return;
        event.preventDefault();
        const next = index < 0 ? 0 : Math.min(order.length - 1, Math.max(0, index + (event.key === 'j' ? 1 : -1)));
        setCursor(order[next].id);
        return;
      }
      if (!agent) return;
      if (event.key === 'Enter') {
        // A focused button or card handles its own Enter.
        const target = event.composedPath?.()[0] || event.target;
        if (target?.closest?.('button,a,[role=button]')) return;
        event.preventDefault();
        openPanel(`agent:${agent.id}`);
      } else if (event.key === 't' && agent.state !== 'stopped') {
        event.preventDefault();
        openTerminal(agent);
      } else if (event.key === 'c' && claudeLink(agent)) {
        event.preventDefault();
        openInClaude(agent);
      } else if (event.key === 'x' && agent.facts && agent.facts.you && agent.facts.you.dismissible) {
        event.preventDefault();
        markAnswered(agent, reload);
      }
    };
    document.addEventListener('keydown', down);
    return () => document.removeEventListener('keydown', down);
  }, [keys, setCursor, reload]);
}

/**
 * By repo: repos with any agent not idle or stopped first, then by name;
 * within a repo, top-level agents in attention order; children follow their
 * parent one step in (D6.1).
 */
export function groupAgents(sessions) {
  const byId = new Map(sessions.map((agent) => [agent.id, agent]));
  const children = new Map();
  const roots = [];
  for (const agent of sessions) {
    const parent = agent.parent_session_id && byId.get(agent.parent_session_id);
    if (parent) (children.get(parent.id) || children.set(parent.id, []).get(parent.id)).push(agent);
    else roots.push(agent);
  }
  const repos = new Map();
  for (const agent of roots) (repos.get(agent.repo) || repos.set(agent.repo, []).get(agent.repo)).push(agent);
  const busy = (agents) => agents.some((agent) => !['idle', 'stopped'].includes(sectionOf(agent)));
  const walk = (agent, depth, out, seen) => {
    if (seen.has(agent.id)) return;
    seen.add(agent.id);
    out.push({ agent, depth });
    for (const child of (children.get(agent.id) || []).sort(attentionOrder)) walk(child, depth + 1, out, seen);
  };
  return [...repos.entries()]
    .map(([repo, agents]) => {
      const out = [];
      const seen = new Set();
      for (const agent of agents.sort(attentionOrder)) walk(agent, 0, out, seen);
      return { repo, agents: out, busy: busy(out.map((entry) => entry.agent)) };
    })
    .sort((a, b) => Number(b.busy) - Number(a.busy) || basename(a.repo).localeCompare(basename(b.repo)));
}

function AgentCard({ agent, depth, now, showRepo, titles, selected, cursor, onAnswered, onRetired }) {
  const codex = (agent.provider || '').startsWith('codex');
  const section = sectionOf(agent);
  const faded = ['idle', 'stopped'].includes(section) && !hasTicket(agent);
  const linked = claudeLink(agent);
  const live = agent.state !== 'stopped';
  const tall = !!youFact(agent, now);
  const title = titles ? ticketTitle(agent) : '';
  const cls = ['card', 'acard', `edge-${edgeTone(agent)}`, tall && 'tall', title && 'titled', selected && 'sel', cursor && 'kb',
    faded && 'faded', depth && 'child'].filter(Boolean).join(' ');
  const iconButton = (title, name, action) => html`<button type="button" class="icon-btn" title=${title} aria-label=${title}
    onClick=${(event) => { event.stopPropagation(); action(); }}><${Icon} name=${name} /></button>`;
  return html`<div class=${cls} data-open-ref=${`agent:${agent.id}`} style=${depth > 1 ? `margin-left:${depth * 18}px` : ''} role="button" tabindex="0"
    onClick=${() => openPanel(`agent:${agent.id}`)}
    onKeyDown=${(event) => event.key === 'Enter' && event.target === event.currentTarget && openPanel(`agent:${agent.id}`)}>
    <${Ring} percent=${agent.context_percent} />
    <span class="nm" title=${agent.name}>${agent.name}</span>
    <span class="icons">
      ${live ? iconButton('Terminal (t)', 'terminal', () => openTerminal(agent)) : null}
      ${linked ? iconButton('Open in Claude (c)', 'external', () => openInClaude(agent)) : null}
    </span>
    ${title ? html`<span class="ttl" title=${title}>${title}</span>` : null}
    <span class="ln"><span class=${`prov ${codex ? 'codex' : 'claude'}`}>${codex ? 'CODEX' : 'CLAUDE'}</span>
      <span class="tk"> ${ticketText(agent)}</span>
      ${showRepo && agent.repo ? html`<span class="repo"> ${basename(agent.repo)}</span>` : null}</span>
    <${Facts} agent=${agent} now=${now} onAnswered=${onAnswered} onRetired=${onRetired} cardSection=${section} />
  </div>`;
}

// ---- agent panel -------------------------------------------------------------

function useAgent(id) {
  const [doc, error, reload] = usePoll(() => api(`/watch/state?session=${encodeURIComponent(id)}`), refreshMs(), [id]);
  const agent = doc && (doc.sessions || []).find((session) => session.id === id);
  return { agent, reload, loading: !doc && !error, error: doc && !agent ? new Error('This agent is not known to sm.') : error };
}

export function AgentPanel({ id, controls }) {
  const { agent, reload, loading, error } = useAgent(id);
  const now = useNow(15000);
  const [tab, setTab] = useState('work');
  useEffect(() => setTab('work'), [id]);
  if (!agent) {
    return html`<div class="phd"><span class="ring none">–</span><span class="t">${loading ? 'Loading…' : 'Agent'}</span>
      ${controls}<span class="s">${error ? error.message : ''}</span></div>`;
  }
  const claim = (agent.claims || []).find((c) => c.kind === 'ticket') || (agent.claims || [])[0];
  const since = agent.state === 'stopped' ? agent.last_activity : agent.activity_since || agent.last_activity;
  const parts = [
    providerLabel(agent.provider),
    agent.model,
    agent.reasoning_effort,
    claim && `${claim.kind === 'pr' ? 'PR ' : ''}#${claim.number}`,
    `${agent.state === 'working' ? 'working' : agent.state} since ${clock(since)}`,
  ].filter(Boolean);
  return html`
    <div class="phd">
      <${Ring} percent=${agent.context_percent} suffix="%" />
      <span class="t" title=${agent.name}>${agent.name}</span>
      ${controls}
      <span class="s">${parts.join(' · ')}</span>
    </div>
    <${AgentActions} agent=${agent} onRetired=${reload} />
    <${Links} ticket=${(agent.claims || []).find(item => item.kind === 'ticket')}
      prs=${(agent.claims || []).filter(item => item.kind === 'pr')}
      jobs=${agent.jobs || []} thread=${agent.thread}
      docs=${agent.docs || []} />
    <div class="tabs" role="tablist">
      ${[['work', 'Work'], ['activity', 'Activity'], ['summary', 'Summary']].map(
        ([key, label]) => html`<button type="button" role="tab" aria-selected=${tab === key}
          class=${tab === key ? 'on' : ''} onClick=${() => setTab(key)}>${label}</button>`,
      )}
    </div>
    <div class="pbody">
      ${tab === 'work' ? html`<${WorkTab} agent=${agent} now=${now} onAnswered=${reload} />` : null}
      ${tab === 'activity' ? html`<${ActivityTab} id=${agent.id} />` : null}
      ${tab === 'summary' ? html`<${SummaryTab} agent=${agent} />` : null}
    </div>
    ${agent.state !== 'stopped' ? html`<${MessageBox} agent=${agent} />` : null}
  `;
}

function AgentActions({ agent, onRetired }) {
  const [menu, setMenu] = useState(null);
  const linked = claudeLink(agent);
  const live = agent.state !== 'stopped';
  const close = () => setMenu(null);
  return html`<div class="acts">
    ${linked ? html`<button type="button" class="btn pri" onClick=${() => openInClaude(agent)}>Open in Claude ↗</button>` : null}
    ${live
      ? html`<button type="button" class=${linked ? 'btn' : 'btn pri'}
          onClick=${() => navigate(`/terminal/${encodeURIComponent(agent.id)}`)}>⌨ Terminal</button>`
      : null}
    ${live ? html`<${RetireButton} agent=${agent} onRetired=${onRetired} />` : null}
    ${live
      ? html`<span class="anchor"><button type="button" class="btn" data-pop-anchor
          onClick=${() => setMenu(menu === 'handoff' ? null : 'handoff')}>Hand off…</button>
          ${menu === 'handoff' ? html`<${HandoffPopover} agent=${agent} onClose=${close} showAskNow=${false} />` : null}</span>`
      : null}
    <span class="anchor"><button type="button" class="btn" data-pop-anchor aria-label="More"
      onClick=${() => setMenu(menu === 'more' ? null : 'more')}>⋯</button>
      ${menu === 'more' ? html`<${MoreMenu} agent=${agent} onClose=${close} />` : null}</span>
  </div>`;
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

function MoreMenu({ agent, onClose }) {
  const [asking, setAsking] = useState(false);
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
  return html`<${Popover} onClose=${onClose} className="menu">
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
    <${FollowButton} id=${agent.id} menu onClose=${onClose} />
    <button type="button" onClick=${copy}>Copy attach command</button>
  <//>`;
}

const DOC_LINK = (doc) => doc.reader_path || doc.browser_url || doc.url;

function WorkTab({ agent, now, onAnswered }) {
  const claims = agent.claims || [];
  const jobs = (agent.jobs || []).filter((job) => ['running', 'pending'].includes(job.state));
  const reviews = (agent.waiting_on || []).filter((w) => w.kind === 'review');
  const docs = agent.docs || [];
  const ticketClaims = claims.filter((claim) => claim.kind !== 'pr');
  const prClaims = claims.filter((claim) => claim.kind === 'pr');
  const itemButton = (kind, id, href, children) => html`<button type="button" class="plain-btn"
    onClick=${() => openItem(kind, id, href)}>${children}</button>`;
  return html`
    <div class="acard-facts"><${Facts} agent=${agent} now=${now} onAnswered=${onAnswered} /></div>
    ${ticketClaims.length
      ? html`<section><h3>Ticket</h3><ul>${ticketClaims.map((claim) => html`<li>
          ${itemButton('ticket', `${claim.repo}#${claim.number}`, claim.url,
            html`<span class="tk">#${claim.number}</span> ${claim.title || claim.repo}`)}</li>`)}</ul></section>`
      : null}
    ${prClaims.length
      ? html`<section><h3>Pull requests</h3><ul>${prClaims.map((claim) => html`<li>
          <a href=${claim.url} target="_blank" rel="noopener"><span class="tk">PR #${claim.number}</span> ${claim.title || ''}</a>
          ${claim.state ? html` <span class="sub">${claim.state}</span>` : null}</li>`)}</ul></section>`
      : null}
    ${reviews.length
      ? html`<section><h3>Reviews</h3><ul>${reviews.map((review) => html`<li>
          <span class="ball amber">${reviewWaitText(agent.waiting_on_review, review, now)}</span> <span class="sub">PR #${review.pr_number} · requested ${age(review.since, now)} ago</span></li>`)}</ul></section>`
      : null}
    ${docs.length
      ? html`<section><h3>Docs</h3><ul>${docs.slice(0, 6).map((doc) => html`<li>
          ${itemButton('doc', doc.reader_path || doc.id, DOC_LINK(doc), doc.title || doc.name || doc.id)}
          ${doc.state ? html` <span class="sub">${String(doc.state).replace(/_/g, ' ')}</span>` : null}</li>`)}</ul></section>`
      : null}
    ${jobs.length
      ? html`<section><h3>Queue jobs</h3><ul>${jobs.map((job) => html`<li>
          ${itemButton('job', job.id, null, html`<span class=${`ball ${job.state === 'running' ? 'green' : 'amber'}`}>${job.label}</span>`)}
          <span class="sub"> ${job.state === 'running'
            ? `running ${age(job.started_at, now)}${job.timeout_seconds ? ` of ${limitText(job.timeout_seconds)}` : ''}`
            : `waiting ${age(job.queued_at, now)}`}</span></li>`)}</ul></section>`
      : null}
    ${agent.status_text
      ? html`<section><h3>Last words</h3><div class="quote">${agent.status_text}</div>
          ${agent.status_at ? html`<div class="sub">${clock(agent.status_at)}</div>` : null}</section>`
      : null}
    <${LastTurn} id=${agent.id} />
    <${NoteEditor} agent=${agent} onSaved=${onAnswered} />
    <section><h3>Workspace</h3><div class="sub mono">${homeRelative(agent.working_dir || agent.repo)}</div></section>
  `;
}

// What the agent wrote at the end of its latest turn; none yet is a 404.
function LastTurn({ id }) {
  const [turn] = usePoll(() => api(`/sessions/${encodeURIComponent(id)}/last-turn`).catch(() => null), 30000, [id]);
  if (!turn) return null;
  // Reply continues in the agent's Inbox thread, where its answer appears.
  const reply = () => { navigate('/inbox'); openPanel(`thread:${id}`); };
  return html`<section><h3>Last turn</h3>
    <div class="quote last-turn md" dangerouslySetInnerHTML=${{ __html: safeThreadHtml(turn.html || '') }} />
    <div class="last-turn-foot"><span class="sub">${clock(turn.at)}</span>
      <button type="button" class="btn sm" onClick=${reply}>Reply</button></div></section>`;
}

function ActivityTab({ id }) {
  const [value, error] = usePoll(() => api(`/sessions/${encodeURIComponent(id)}/tool-calls?limit=10`), 10000, [id]);
  if (!value) return html`<p class="muted">${error ? error.message : 'Loading…'}</p>`;
  const calls = value.tool_calls || [];
  if (!calls.length) return html`<p class="muted">No tool calls recorded.</p>`;
  return html`<ul>${calls.map((call) => html`<li class="tool">
    <span class="when">${clock(call.timestamp)}</span><span class="mono">${call.tool_name}</span></li>`)}</ul>`;
}

const WHAT_DONE = ['completed', 'failed', 'timed_out'];

/** The phone's Summarize progress: POST /sessions/{id}/what in poll mode. */
function SummaryTab({ agent }) {
  const [request, setRequest] = useState(null);
  const [error, setError] = useState(null);
  useEffect(() => {
    if (!request || WHAT_DONE.includes(request.status)) return undefined;
    const timer = setTimeout(async () => {
      try {
        setRequest(await api(`/btw-requests/${encodeURIComponent(request.request_id)}`));
        setError(null);
      } catch (e) {
        // Keep polling: a copy of the request re-arms this effect.
        setError(e.message);
        setRequest((current) => ({ ...current }));
      }
    }, 2000);
    return () => clearTimeout(timer);
  }, [request]);
  const start = async () => {
    setError(null);
    try {
      setRequest(await api(`/sessions/${encodeURIComponent(agent.id)}/what`, { method: 'POST', body: { delivery_mode: 'poll' } }));
    } catch (e) {
      const active = e.status === 409 && /request (\S+)$/.exec(e.message);
      if (active) {
        try {
          setRequest(await api(`/btw-requests/${encodeURIComponent(active[1])}`));
          return;
        } catch (inner) {
          setError(inner.message);
          return;
        }
      }
      setError(e.message);
    }
  };
  const pending = request && !WHAT_DONE.includes(request.status);
  return html`
    <div class="row" style="display:flex;gap:8px;align-items:center">
      <button type="button" class="btn" disabled=${pending || agent.state === 'stopped'} onClick=${start}>
        ${request ? 'Summarize again' : 'Summarize progress'}</button>
      ${pending ? html`<span class="sub">Asking ${agent.name}…</span>` : null}
    </div>
    ${error ? html`<p class="err">${error}</p>` : null}
    ${request && request.status === 'completed' ? html`<div class="summary">${request.result}</div>` : null}
    ${request && ['failed', 'timed_out'].includes(request.status)
      ? html`<p class="err">${request.error || 'The summary did not finish.'}</p>` : null}
  `;
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
      await api(`/inbox/agent/${encodeURIComponent(agent.id)}/send`, {
        method: 'POST',
        headers: config.inbox_token ? { 'x-sm-doc-token': config.inbox_token } : {},
        body: { submission_id: submissionId(), body },
      });
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
