// Agents page and agent panel (spec 1710 D6.1, D6.2). Data: GET /watch/state.
import { useEffect, useMemo, useState } from 'preact/hooks';
import {
  html, api, usePoll, useNow, useShared, config, age, duration, limitText, clock, ordinal,
  basename, homeRelative, providerLabel, Ring, Icon, Popover, Seg,
  openPanel, openItem, navigate, newAgent, toast, registerPanel, submissionId, stored, store,
} from './ui.js';

// ---- who has the ball --------------------------------------------------------

/** Card order within a repo (D6.1). */
export const BALL_ORDER = ['you', 'stalled', 'working', 'job_running', 'queue', 'review', 'idle', 'stopped'];
export const BALL_TONE = {
  you: 'magenta', stalled: 'red', working: 'green', job_running: 'green',
  queue: 'amber', review: 'amber', idle: 'muted', stopped: 'muted',
};

const capital = (text) => (text ? text[0].toUpperCase() + text.slice(1) : '');
const earliest = (values) => values.filter(Boolean).sort()[0];

/**
 * The agent's ball and its line, by D7's text rules computed from the
 * agent's own jobs and waits. `ctx` carries the inbox's needs-you rows by
 * session, queue positions by job, and the stall threshold.
 */
export function agentBall(agent, ctx, now = Date.now()) {
  if (agent.state === 'stopped') return { ball: 'stopped', text: `Stopped ${age(agent.last_activity, now)}`.trim() };
  const waits = agent.waiting_on || [];
  const ask = ctx.needsYou && ctx.needsYou.get(agent.id);
  if (ask) return { ball: 'you', text: `Waiting on you ${age(ask.newest_at, now)}: ${ask.preview || ask.title}` };
  const docReview = waits.find((w) => w.kind === 'owner_review');
  if (docReview) {
    return { ball: 'you', text: `Waiting on you ${age(docReview.since, now)}: ${(docReview.label || '').replace(/^Owner review · /, '')}` };
  }
  if (agent.state === 'working') return { ball: 'working', text: `Agent working ${age(agent.activity_since, now)}`.trim() };
  const jobs = agent.jobs || [];
  const running = jobs.filter((job) => job.state === 'running');
  const waiting = jobs.filter((job) => job.state === 'pending');
  const oldestWait = age(earliest(waiting.map((job) => job.queued_at)), now);
  if (running.length) {
    if (running.length === 1 && !waiting.length) {
      const job = running[0];
      const limit = limitText(job.timeout_seconds);
      return { ball: 'job_running', text: `${capital(job.type || 'Job')} running ${age(job.started_at, now)}${limit ? ` of ${limit}` : ''}` };
    }
    if (!waiting.length) return { ball: 'job_running', text: `${running.length} jobs running ${age(earliest(running.map((j) => j.started_at)), now)}` };
    return { ball: 'job_running', text: `${running.length} running · ${waiting.length} waiting ${oldestWait}` };
  }
  if (waiting.length) {
    const positions = ctx.positions || new Map();
    if (waiting.length === 1) {
      const position = positions.get(waiting[0].id);
      return { ball: 'queue', text: `Job waiting ${oldestWait}${position ? ` · ${ordinal(position.position)} in line` : ''}` };
    }
    const reasons = new Set(waiting.map((job) => (positions.get(job.id) || {}).holding_reason));
    const forSlot = reasons.size === 1 && reasons.has('concurrency_cap');
    return { ball: 'queue', text: `${waiting.length} jobs waiting ${oldestWait}${forSlot ? ' for a slot' : ''}` };
  }
  const review = waits.find((w) => w.kind === 'review');
  if (review) return { ball: 'review', text: `Codex review on PR #${review.pr_number}, ${age(review.since, now)}` };
  const since = agent.activity_since || agent.last_activity;
  const idleSeconds = (now - Date.parse(since)) / 1000;
  const hasTicket = (agent.claims || []).some((claim) => claim.kind === 'ticket');
  if (hasTicket && idleSeconds >= (ctx.stallMinutes || 15) * 60) {
    return { ball: 'stalled', text: `Stalled ${duration(idleSeconds)}: agent idle, nothing running` };
  }
  return { ball: 'idle', text: `Idle ${age(since, now)}`.trim() };
}

/** Context the ball rules read, from the shell's shared queue and inbox data. */
export function useBallContext() {
  const queue = useShared('queue');
  const inbox = useShared('inbox');
  return useMemo(() => {
    const positions = new Map();
    for (const job of (queue && queue.queued) || []) positions.set(job.id, job);
    const needsYou = new Map();
    for (const row of (inbox && inbox.rows) || []) {
      if (row.group === 'needs_you' && row.session_id && !row.done) needsYou.set(row.session_id, row);
    }
    return { positions, needsYou, stallMinutes: config.stall_minutes || 15 };
  }, [queue, inbox]);
}

/** "#1854" and "+1" for more claims (D6.1). */
function ticketText(agent) {
  const claims = agent.claims || [];
  if (!claims.length) return 'no ticket';
  const first = claims[0];
  const label = first.kind === 'pr' ? `PR #${first.number}` : `#${first.number}`;
  return claims.length > 1 ? `${label} +${claims.length - 1}` : label;
}

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

/** The card icon's action (D6.1): Open in Claude when linked, else Terminal. */
function mainAction(agent) {
  if (claudeLink(agent)) openInClaude(agent);
  else navigate(`/terminal/${encodeURIComponent(agent.id)}`);
}

// ---- page -------------------------------------------------------------------

const refreshMs = () => Math.max(1, config.refresh_seconds || 3) * 1000;

export function AgentsPage({ openRef }) {
  const [filter, setFilter] = useState(() => stored('sm-agents-filter', 'live'));
  const [doc, error] = usePoll(
    () => api(`/watch/state${filter === 'stopped' ? '?stopped=1' : ''}`),
    // Stopped agents make the state large and slow, so that view polls less.
    filter === 'stopped' ? 30000 : refreshMs(),
    [filter],
  );
  const ctx = useBallContext();
  const now = useNow(15000);
  const selected = openRef && openRef.startsWith('agent:') ? openRef.slice(6) : null;
  const choose = (value) => {
    setFilter(value);
    store('sm-agents-filter', value);
  };

  if (!doc) {
    return html`<div class="content">${error ? html`<p class="err">${error.message}</p>` : html`<p class="muted">Loading…</p>`}</div>`;
  }
  const sessions = doc.sessions || [];
  const balls = new Map(sessions.map((agent) => [agent.id, agentBall(agent, ctx, now)]));
  const groups = groupAgents(sessions, balls);
  const counts = {};
  for (const { ball } of balls.values()) counts[ball] = (counts[ball] || 0) + 1;
  const n = (ball) => counts[ball] || 0;

  return html`<div class="content">
    <div class="toolbar">
      <div class="sum">
        <span><b>${n('working')}</b> working</span>
        <span><b>${n('job_running')}</b> running jobs</span>
        <span><b>${n('queue')}</b> waiting on the queue</span>
        <span><b>${n('review')}</b> waiting on review</span>
        <span class=${n('you') ? 'magenta' : ''}><b>${n('you')}</b> need you</span>
        ${n('stalled') ? html`<span class="red"><b>${n('stalled')}</b> stalled</span>` : null}
        <span><b>${n('idle')}</b> idle</span>
      </div>
      <span style="flex:1"></span>
      <${Seg} label="Show" value=${filter} onChange=${choose}
        options=${[{ value: 'live', label: 'Live' }, { value: 'stopped', label: 'With stopped' }]} />
    </div>
    ${!sessions.some((agent) => agent.state !== 'stopped')
      ? html`<p class="empty">No live agents. Start one with New agent.</p>`
      : null}
    ${groups.map(
      (group) => html`<div class="grp">${basename(group.repo)}</div>
        <div class="cards">
          ${group.agents.map(({ agent, depth }) => html`<${AgentCard}
            key=${agent.id} agent=${agent} ball=${balls.get(agent.id)} depth=${depth} selected=${agent.id === selected} />`)}
        </div>`,
    )}
  </div>`;
}

/**
 * Repos with any agent not idle or stopped first, then by name; within a
 * repo, top-level agents by ball, then most recent activity; children follow
 * their parent one step in (D6.1).
 */
export function groupAgents(sessions, balls) {
  const byId = new Map(sessions.map((agent) => [agent.id, agent]));
  const rank = (agent) => {
    const { ball } = balls.get(agent.id);
    // Idle agents without a ticket sink to the bottom.
    const noTicket = ball === 'idle' && !(agent.claims || []).length;
    return BALL_ORDER.indexOf(ball) + (noTicket ? 0.5 : 0);
  };
  const recent = (agent) => Date.parse(agent.activity_since || agent.last_activity) || 0;
  const order = (a, b) => rank(a) - rank(b) || recent(b) - recent(a) || a.name.localeCompare(b.name);
  const children = new Map();
  const roots = [];
  for (const agent of sessions) {
    const parent = agent.parent_session_id && byId.get(agent.parent_session_id);
    if (parent) (children.get(parent.id) || children.set(parent.id, []).get(parent.id)).push(agent);
    else roots.push(agent);
  }
  const repos = new Map();
  for (const agent of roots) (repos.get(agent.repo) || repos.set(agent.repo, []).get(agent.repo)).push(agent);
  const busy = (agents) => agents.some((agent) => !['idle', 'stopped'].includes(balls.get(agent.id).ball));
  const walk = (agent, depth, out, seen) => {
    if (seen.has(agent.id)) return;
    seen.add(agent.id);
    out.push({ agent, depth });
    for (const child of (children.get(agent.id) || []).sort(order)) walk(child, depth + 1, out, seen);
  };
  return [...repos.entries()]
    .map(([repo, agents]) => {
      const out = [];
      const seen = new Set();
      for (const agent of agents.sort(order)) walk(agent, 0, out, seen);
      return { repo, agents: out, busy: busy(out.map((entry) => entry.agent)) };
    })
    .sort((a, b) => Number(b.busy) - Number(a.busy) || basename(a.repo).localeCompare(basename(b.repo)));
}

function AgentCard({ agent, ball, depth, selected }) {
  const codex = (agent.provider || '').startsWith('codex');
  const faded = ball.ball === 'stopped' || (ball.ball === 'idle' && !(agent.claims || []).length);
  const linked = claudeLink(agent);
  const live = agent.state !== 'stopped';
  const cls = ['card', selected && 'sel', faded && 'faded', depth && 'child'].filter(Boolean).join(' ');
  return html`<div class=${cls} style=${depth > 1 ? `margin-left:${depth * 18}px` : ''} role="button" tabindex="0"
    onClick=${() => openPanel(`agent:${agent.id}`)}
    onKeyDown=${(event) => event.key === 'Enter' && openPanel(`agent:${agent.id}`)}>
    <${Ring} percent=${agent.context_percent} />
    <span class="nm" title=${agent.name}>${agent.name}</span>
    <span class="ln"><span class=${`prov ${codex ? 'codex' : 'claude'}`}>${codex ? 'CODEX' : 'CLAUDE'}</span>
      <span class="tk"> ${ticketText(agent)}</span></span>
    <span class=${`ln ball ${BALL_TONE[ball.ball]}`} title=${ball.text}>${ball.text}</span>
    ${live
      ? html`<button type="button" class="icon-btn act" title=${linked ? 'Open in Claude' : 'Terminal'}
          onClick=${(event) => { event.stopPropagation(); mainAction(agent); }}>
          <${Icon} name=${linked ? 'external' : 'terminal'} /></button>`
      : html`<span></span>`}
  </div>`;
}

// ---- agent panel -------------------------------------------------------------

function useAgent(id) {
  const [doc, error] = usePoll(() => api(`/watch/state?session=${encodeURIComponent(id)}`), refreshMs(), [id]);
  const agent = doc && (doc.sessions || []).find((session) => session.id === id);
  return { agent, loading: !doc && !error, error: doc && !agent ? new Error('This agent is not known to sm.') : error };
}

export function AgentPanel({ id, controls }) {
  const { agent, loading, error } = useAgent(id);
  const ctx = useBallContext();
  const now = useNow(15000);
  const [tab, setTab] = useState('work');
  useEffect(() => setTab('work'), [id]);
  if (!agent) {
    return html`<div class="phd"><span class="ring none">–</span><span class="t">${loading ? 'Loading…' : 'Agent'}</span>
      ${controls}<span class="s">${error ? error.message : ''}</span></div>`;
  }
  const ball = agentBall(agent, ctx, now);
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
    <${AgentActions} agent=${agent} />
    <div class="tabs" role="tablist">
      ${[['work', 'Work'], ['activity', 'Activity'], ['summary', 'Summary']].map(
        ([key, label]) => html`<button type="button" role="tab" aria-selected=${tab === key}
          class=${tab === key ? 'on' : ''} onClick=${() => setTab(key)}>${label}</button>`,
      )}
    </div>
    <div class="pbody">
      ${tab === 'work' ? html`<${WorkTab} agent=${agent} ball=${ball} now=${now} />` : null}
      ${tab === 'activity' ? html`<${ActivityTab} id=${agent.id} />` : null}
      ${tab === 'summary' ? html`<${SummaryTab} agent=${agent} />` : null}
    </div>
    ${agent.state !== 'stopped' ? html`<${MessageBox} agent=${agent} />` : null}
  `;
}

function AgentActions({ agent }) {
  const [menu, setMenu] = useState(null);
  const linked = claudeLink(agent);
  const live = agent.state !== 'stopped';
  const close = () => setMenu(null);
  return html`<div class="acts">
    ${linked ? html`<button type="button" class="btn pri" onClick=${() => openInClaude(agent)}>Open in Claude ↗</button>` : null}
    ${live
      ? html`<button type="button" class=${linked ? 'btn' : 'btn pri'}
          onClick=${() => navigate(`/terminal/${encodeURIComponent(agent.id)}`)}>Terminal</button>`
      : null}
    <${FollowButton} id=${agent.id} />
    ${live
      ? html`<span class="anchor"><button type="button" class="btn" data-pop-anchor
          onClick=${() => setMenu(menu === 'handoff' ? null : 'handoff')}>Hand off</button>
          ${menu === 'handoff' ? html`<${HandoffPopover} id=${agent.id} onClose=${close} />` : null}</span>`
      : null}
    <button type="button" class="btn" onClick=${() => newAgent({
      provider: agent.provider, model: agent.model, effort: agent.reasoning_effort, workspace: agent.working_dir,
    })}>Clone</button>
    <span class="anchor"><button type="button" class="btn" data-pop-anchor aria-label="More"
      onClick=${() => setMenu(menu === 'more' ? null : 'more')}>⋯</button>
      ${menu === 'more' ? html`<${MoreMenu} agent=${agent} onClose=${close} />` : null}</span>
  </div>`;
}

function FollowButton({ id }) {
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
    } catch (error) {
      toast(error.message);
    } finally {
      setBusy(false);
    }
  };
  return html`<button type="button" class=${following ? 'btn on' : 'btn'} aria-pressed=${following}
    disabled=${busy || !follows} onClick=${toggle}>${following ? 'Following' : 'Follow'}</button>`;
}

/** The phone's handoff controls (GET/PUT /sessions/{id}/handoff-policy). */
function HandoffPopover({ id, onClose }) {
  const path = `/sessions/${encodeURIComponent(id)}/handoff-policy`;
  const [policy, setPolicy] = useState(null);
  const [note, setNote] = useState('Loading…');
  const [asking, setAsking] = useState(false);
  useEffect(() => {
    api(path).then((value) => { setPolicy(value); setNote(`Using ${value.source} policy`); })
      .catch((error) => setNote(error.message));
  }, [path]);
  const write = async (body) => {
    setNote('Saving…');
    try {
      const value = await api(path, { method: 'PUT', body });
      setPolicy(value);
      setNote(body.ask_now ? 'Asked the agent to hand off' : 'Saved');
    } catch (error) {
      setNote(error.message);
    }
  };
  const threshold = (event) => {
    const number = Number(event.target.value);
    if (!Number.isInteger(number) || number < 1 || number > 100) setNote('Enter an integer from 1 to 100');
    else write({ threshold_percent: number });
  };
  return html`<${Popover} onClose=${onClose}>
    <h2>Context handoff</h2>
    ${policy
      ? html`<label class="check"><input type="checkbox" checked=${policy.enabled}
            onChange=${(event) => write({ enabled: event.target.checked })} /> Hand off automatically</label>
          <div class="fld"><span class="l">Threshold (%)</span>
            <input class="inp num" type="number" min="1" max="100" step="1" style="width:6rem"
              value=${policy.threshold_percent} onChange=${threshold} /></div>
          <div class="row" style="justify-content:flex-start">
            <button type="button" class="btn sm" onClick=${() => write({ use_default: true })}>Use default</button>
            ${asking
              ? html`<span class="confirm">Ask this agent to hand off?
                  <button type="button" class="btn sm danger" onClick=${() => { setAsking(false); write({ ask_now: true }); }}>Confirm handoff</button>
                  <button type="button" class="btn sm" onClick=${() => setAsking(false)}>Cancel</button></span>`
              : html`<button type="button" class="btn sm" onClick=${() => setAsking(true)}>Hand off now</button>`}
          </div>`
      : null}
    <span class="sub" role="status">${note}</span>
  <//>`;
}

function MoreMenu({ agent, onClose }) {
  const [retiring, setRetiring] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(agent.attach);
      toast(`Copied: ${agent.attach}`);
    } catch (error) {
      toast(agent.attach);
    }
    onClose();
  };
  const retire = async () => {
    try {
      await api(`/sessions/${encodeURIComponent(agent.id)}/retire`, { method: 'POST', body: {} });
      toast(`Retired ${agent.name}`);
      onClose();
    } catch (error) {
      toast(error.message);
    }
  };
  return html`<${Popover} onClose=${onClose} align="right" className="menu">
    <button type="button" onClick=${copy}>Copy attach command</button>
    ${agent.state === 'stopped'
      ? null
      : retiring
        ? html`<div class="confirm" style="padding:6px 10px">Retire ${agent.name}?
            <button type="button" class="btn sm danger" onClick=${retire}>Retire</button>
            <button type="button" class="btn sm" onClick=${() => setRetiring(false)}>Cancel</button></div>`
        : html`<button type="button" class="red" onClick=${() => setRetiring(true)}>Retire…</button>`}
  <//>`;
}

const DOC_LINK = (doc) => doc.reader_path || doc.browser_url || doc.url;

function WorkTab({ agent, ball, now }) {
  const claims = agent.claims || [];
  const jobs = (agent.jobs || []).filter((job) => ['running', 'pending'].includes(job.state));
  const reviews = (agent.waiting_on || []).filter((w) => w.kind === 'review');
  const docs = agent.docs || [];
  const ticketClaims = claims.filter((claim) => claim.kind !== 'pr');
  const prClaims = claims.filter((claim) => claim.kind === 'pr');
  const itemButton = (kind, id, href, children) => html`<button type="button" class="plain-btn"
    onClick=${() => openItem(kind, id, href)}>${children}</button>`;
  return html`
    <div><span class=${`ball ${BALL_TONE[ball.ball]}`}>${ball.text}</span></div>
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
          <span class="ball amber">Codex review on PR #${review.pr_number}</span> <span class="sub">requested ${age(review.since, now)} ago</span></li>`)}</ul></section>`
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
    <section><h3>Workspace</h3><div class="sub mono">${homeRelative(agent.working_dir || agent.repo)}</div></section>
  `;
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
