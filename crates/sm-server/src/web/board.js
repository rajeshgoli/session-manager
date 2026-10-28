// Board (1710 D6.4). The server owns ticket states, ordering and clock rules.
import { createContext } from 'preact';
import { useContext, useEffect, useRef, useState } from 'preact/hooks';
import { html, api, bus, providerLabel, usePoll, stored, store, Seg, Popover, Links, openItem, openPanel, navigate, setShared, toast, age, useNow } from './ui.js';
import { TicketStart, LaneWhenReady, blockedReasons, canStartAnyway, chipText, retryBody, laneCandidates } from './board-start.js';
import { SharedLaunchSetup, LaneLaunchDefault } from './launch-setup.js';
import { exactConfig } from './launch-fields.js';
import { HandoffPopover } from './handoff.js';
import { PolicyPopover, reviewerText, setByText } from './reviews.js';

export const BALL_TONE = { you: 'magenta', working: 'green', job_running: 'green', queue: 'amber', review: 'amber', idle: 'muted', stalled: 'red', job_quiet: 'red', no_agent: 'red' };
export const OTHER_ROW_LIMIT = 10;
const STATE_TONE = { needs_you: 'magenta', ready: 'accent', in_progress: 'green', close_ready: 'cyan', standing: 'cyan', blocked: 'muted', done: 'muted' };
const ticketKey = (t) => `${t.repo}#${t.number}`;
const ticketLink = (t) => openItem('ticket', ticketKey(t), t.url || `https://github.com/${t.repo}/issues/${t.number}`);
export function groupTickets(tickets) {
  const done = tickets.filter((t) => t.state === 'done');
  done.sort((a, b) => Date.parse(b.closed_at || 0) - Date.parse(a.closed_at || 0));
  return {
    active: tickets.filter((t) => !['blocked', 'done'].includes(t.state)),
    blocked: tickets.filter((t) => t.state === 'blocked'),
    done,
  };
}
export function visibleOther(tickets) {
  const urgent = tickets.filter((t) => t.state === 'needs_you' || hasOperations(t));
  const rest = tickets.filter((t) => t.state !== 'needs_you' && !hasOperations(t));
  return [...urgent, ...rest.slice(0, Math.max(0, OTHER_ROW_LIMIT - urgent.length))];
}
/** Board sections in render order: each repo's other tickets sit right under
 * that repo's top lane, so tickets agents file without a lane stay near the
 * work they came from (#2152). Repos with no visible lane go last. */
export function boardSections(lanes, others) {
  const byRepo = new Map(others.map((group) => [group.repo, group]));
  const sections = [];
  for (const lane of lanes) {
    sections.push({ lane });
    const group = byRepo.get(lane.goal.repo);
    if (group) { sections.push({ other: group }); byRepo.delete(lane.goal.repo); }
  }
  return [...sections, ...[...byRepo.values()].map((other) => ({ other }))];
}
export const openBlockers = (ticket) => (ticket.waits_on || []).filter((item) => item.state !== 'done');
export const blockedText = (ticket) => openBlockers(ticket).map((item) => `#${item.number}`).join(', ');
export function clockSegments(segments, end, hours) {
  const span = hours * 3600000;
  const start = Date.parse(end) - span;
  return (segments || []).flatMap((s) => {
    const left = Math.max(0, (Date.parse(s.from) - start) / span * 100);
    const right = Math.min(100, (Date.parse(s.to) - start) / span * 100);
    return Number.isFinite(left) && right > left ? [{ ...s, left, width: right - left }] : [];
  });
}
function Clock({ ticket, end, hours }) {
  const clock = ticket.clock;
  if (!clock) return null;
  const click = () => {
    if (['queue', 'job_running', 'job_quiet'].includes(clock.ball)) navigate('/queue');
    else if (ticket.holder) openPanel(`agent:${ticket.holder.session_id}`);
    else ticketLink(ticket);
  };
  return html`<button class="ticket-clock" onClick=${click} title=${clock.text}>
    <span class="clock-strip" aria-label=${`Last ${hours} hours`}>
      ${clockSegments(clock.segments, end, hours).map((s) => html`<i class=${`clock-segment ${s.kind}`} style=${`left:${s.left}%;width:${s.width}%`}
        title=${`${s.kind.replaceAll('_', ' ')} · ${new Date(s.from).toLocaleTimeString()}–${new Date(s.to).toLocaleTimeString()}`}></i>`)}
    </span><span class=${`ball ${BALL_TONE[clock.ball] || 'muted'}`}>${clock.text}</span>
  </button>`;
}
export const canStart = (ticket) => ticket.state === 'ready' && !(ticket.warnings || []).includes('merged_not_closed');

const boardChanged = () => bus.emit('board-changed');
const AutoStartPaused = createContext(false);
/** Start when ready is offered on a blocked ticket nobody holds (1821 F4). */
export const canStartWhenReady = (ticket) => ticket.state === 'blocked' && !ticket.holder
  && !(ticket.warnings || []).includes('merged_not_closed');
/** Which actions a ticket row offers. The standing Bugs goal offers none but ⋯ (1859 B5). */
export const rowActions = (ticket) => ({
  start: canStart(ticket), startBlocked: ticket.state === 'blocked', whenReady: canStartWhenReady(ticket),
  close: ticket.state === 'close_ready', menu: ticket.state !== 'done',
});
export function standingText(ticket) {
  const open = openBlockers(ticket).length;
  return `Standing lane · ${open} open bug${open === 1 ? '' : 's'}`;
}
function WhenReadyChip({ ticket, onStart }) {
  const paused = useContext(AutoStartPaused);
  const [retrying, setRetrying] = useState(false);
  const auto = ticket.auto_start;
  const failed = auto.state === 'failed';
  const retry = async () => {
    setRetrying(true);
    // An edited type no longer matches: retry as Custom.
    const put = (body) => api('/client/board/auto-start', { method: 'PUT', body });
    try { await put(retryBody(ticket)).catch(() => put({ ...retryBody(ticket), agent_type: null })); boardChanged(); }
    catch (e) { toast(e.message); }
    finally { setRetrying(false); }
  };
  return html`<button class=${`when-ready-chip ${failed ? 'amber' : paused ? 'muted' : ''}`} title=${auto.last_error || ''}
    onClick=${() => onStart(ticket, 'when_ready')}>${chipText(auto, paused)}</button>
    ${failed ? html`<button class="btn sm" disabled=${retrying} onClick=${retry}>Retry</button>` : null}`;
}
export function operationalAge(since, now = Date.now()) {
  const time = Date.parse(since);
  return Number.isFinite(time) && time <= now ? age(since, now) : 'age unavailable';
}
export function currentReviews(ticket) {
  if (ticket.reviews) return ticket.reviews;
  const list = (ticket.prs || []).flatMap(pr => (pr.reviews || [pr.review]).filter(r=>r?.waiting_since).map(r=>({...r,since:r.waiting_since,reviewer_label:r.by==='you'?'Your review':'Codex review',number:pr.number})));
  if (ticket.review && !list.some(r=>r.by!=='you')) list.push(ticket.review);
  return list;
}
export const hasOperations = t => !!(t.holder || t.needs_you || t.auto_start || t.jobs?.length || currentReviews(t).length || t.warnings?.length);
export function matchesFilter(ticket, filter, query = '', goalTitle = '') {
  const match = filter==='all' || filter==='finished' && ticket.state==='done' || filter==='armed' && !!ticket.auto_start || filter==='attention' && (ticket.needs_you || ticket.warnings?.length || ticket.auto_start?.state==='failed' || ticket.jobs?.some(j=>j.quiet_since) || ['stalled','no_agent'].includes(ticket.clock?.ball));
  return !!match && `${ticket.repo} #${ticket.number} ${ticket.title} ${goalTitle}`.toLowerCase().includes(query.toLowerCase());
}
function TicketRow({ ticket, end, hours, onStart, onClose, busy, lanePolicy = null }) {
  const [menu, setMenu] = useState(null);
  const now = useNow();
  const parts = ticket.sub_issues, can = rowActions(ticket), holder = ticket.holder;
  const config = ticket.auto_start ? { ...ticket.auto_start, reasoning_effort:ticket.auto_start.effort } : ticket.launch_preference?.config;
  const reviews = currentReviews(ticket);
  return html`<div class="board-ticket" data-ticket=${ticketKey(ticket)}>
    <div class="board-ticket-main">
      <div class="ticket-title-line"><span class=${`ticket-state-label ${STATE_TONE[ticket.state]||''}`}>${ticket.state.replaceAll('_',' ')}</span>
        <button class="ticket-title" onClick=${()=>ticketLink(ticket)}><span class="mono">#${ticket.number}</span> ${ticket.title}</button></div>
      <div class="ticket-direct-links"><a href=${ticket.url||`https://github.com/${ticket.repo}/issues/${ticket.number}`} target="_blank" rel="noopener noreferrer">Issue ↗</a>
        ${(ticket.prs||[]).map(pr=>html`<span><button class="link-btn" onClick=${()=>openPanel(`ticket:${pr.repo||ticket.repo}#${pr.number}`)}>PR #${pr.number} · ${(pr.state||'open').toLowerCase()}</button> <a aria-label=${`Open PR ${pr.number} on GitHub`} href=${pr.url||`https://github.com/${pr.repo||ticket.repo}/pull/${pr.number}`} target="_blank" rel="noopener noreferrer">↗</a></span>`)}
        <${Links} thread=${ticket.thread} docs=${ticket.docs||[]} /></div>
      ${ticket.needs_you?html`<span class="sub magenta">${ticket.needs_you.text} · <a href=${ticket.needs_you.url} target="_blank" rel="noopener noreferrer">Open</a></span>`:null}
      ${ticket.state==='blocked'?html`<span class="sub">${blockedReasons(ticket).join(' ')}</span>`:null}
      ${(ticket.warnings||[]).includes('merged_not_closed')?html`<span class="sub amber">PR merged · close this ticket on GitHub.</span>`:null}
      ${ticket.state==='close_ready'?html`<span class="sub cyan">${parts?.done||0} of ${parts?.total||0} parts done</span>`:null}
      ${ticket.state==='standing'?html`<span class="sub cyan">${standingText(ticket)}</span>`:null}
      ${ticket.started_early?html`<span class="sub amber">Started early</span>`:null}
    </div>
    <div class="board-agent">${holder?html`
      <div class="ticket-agent-links"><button class="link-btn" onClick=${()=>openPanel(`agent:${holder.session_id}`)}>${holder.name}</button><button class="btn sm" aria-label=${`Terminal for ${holder.name}`} onClick=${()=>navigate(`/terminal/${encodeURIComponent(holder.session_id)}`)}>⌨</button>
      <span class=${`sub ${holder.state==='working'?'green':''}`}>${holder.state==='working'?'● Working':holder.state==='idle'?'○ Idle':holder.state==='stopped'||holder.state==='retired'?'Stopped':holder.state||'Activity unknown'}${holder.state==='working'?` · ${operationalAge(holder.since,now)}`:''}</span>
      <span class=${holder.provider === 'opencode' ? 'prov local' : 'sub'}>${holder.provider === 'opencode' ? providerLabel(holder.provider).toUpperCase() : holder.provider?.startsWith('codex')?'Codex':holder.provider==='claude'?'Claude':holder.provider||''}</span></div>`:html`<span class="sub board-empty" title="No agent assigned">No agent</span>`}
      <${Clock} ticket=${ticket} end=${end} hours=${hours} /></div>
    <div class="board-waits">
      ${(ticket.jobs||[]).map(job=>html`<button class=${`board-job ${job.quiet_since?'red':job.state==='running'?'green':'amber'}`} onClick=${()=>{navigate('/queue');openPanel(`job:${job.id}`);}}>
        <b>${job.label||job.id}</b><span>${job.state==='running'?'Running':'Waiting'} · ${operationalAge(job.since||job.started_at||job.queued_at,now)}</span>${job.quiet_since?html`<span>Quiet for ${operationalAge(job.quiet_since,now)}</span>`:null}</button>`)}
      ${reviews.map(r=>html`<span class=${`board-review ${r.by==='you'?'magenta':'amber'}`}>${r.reviewer_label||'Review'}${r.number?` · PR #${r.number}`:''}<span>Waiting ${operationalAge(r.since||r.waiting_since,now)}</span></span>`)}
    </div>
    <div class="board-launch">${config?html`<span class="sub launch-exact">${exactConfig(config)}</span>`:null}
      <div class="ticket-actions">
        ${ticket.auto_start?html`<${WhenReadyChip} ticket=${ticket} onStart=${onStart} />`:can.whenReady&&!['stale','cycle'].some(w=>ticket.warnings?.includes(w))?html`<button class="btn sm" onClick=${()=>onStart(ticket,'when_ready')}>Start when ready</button>`:null}
        ${can.start?html`<button class="btn sm pri" onClick=${()=>onStart(ticket)}>Start</button>`:null}
        ${can.startBlocked&&!canStartAnyway(ticket)?html`<button class="btn sm" onClick=${()=>onStart(ticket)}>Why blocked</button>`:null}
        ${can.close?html`<button class="btn sm pri" disabled=${busy} onClick=${()=>onClose(ticket)}>Close</button>`:null}
        ${can.menu?html`<span class="anchor"><button class="icon-btn" data-pop-anchor aria-label=${`More actions for #${ticket.number}`} onClick=${()=>setMenu(menu?null:'more')}>⋯</button>
          ${menu==='more'?html`<${Popover} onClose=${()=>setMenu(null)} align="right" className="menu">
            ${canStartAnyway(ticket)?html`<button onClick=${()=>{setMenu(null);onStart(ticket);}}>Start anyway…</button>`:null}
            ${can.close?html`<button onClick=${()=>{setMenu(null);onStart(ticket);}}>Start instead</button>`:null}
            ${canStartWhenReady(ticket)||ticket.auto_start?html`<button onClick=${()=>{setMenu(null);onStart(ticket,'when_ready');}}>${ticket.auto_start?'Edit / cancel auto-start':'Start when ready…'}</button>`:null}
            <button onClick=${()=>setMenu('handoff')}>Hand off…</button><button onClick=${()=>setMenu('policy')}>Review policy…</button><//>`:null}
          ${menu==='policy'?html`<${PolicyPopover} scope="ticket" repo=${ticket.repo} number=${ticket.number} title=${`Reviews for #${ticket.number}`} policy=${ticket.review_policy} lanePolicy=${lanePolicy} align="right" onClose=${()=>setMenu(null)} onSaved=${boardChanged} />`:null}
          ${menu==='handoff'?html`<${HandoffPopover} scope="ticket" align="right" ticket=${{repo:ticket.repo,number:ticket.number}} agent=${holder?{id:holder.session_id,name:holder.name,provider:holder.provider,state:holder.state}:null} onClose=${()=>setMenu(null)} />`:null}</span>`:null}
      </div>
      ${ticket.review_policy?html`<button class="review-pill" title=${setByText(ticket.review_policy)} onClick=${()=>setMenu('policy')}>Review: ${reviewerText(ticket.review_policy.reviewer)}</button>`:null}
    </div>
  </div>`;
}
/** Column names once per lane instead of on every row. */
const ColumnHeads = ({ hours }) => html`<div class="board-ticket board-cols" aria-hidden="true"><span>Ticket</span><span>Agent · last ${hours} h</span><span>Work / waits</span><span>Launch</span></div>`;
function Fold({ tickets, label, end, hours, onStart, onClose, busy, lanePolicy = null, open = false }) {
  if (!tickets.length) return null;
  const blockers = [...new Map(tickets.flatMap(openBlockers).map((t) => [ticketKey(t), t])).values()];
  const caption = label === 'Blocked' ? `${tickets.length} blocked` : label === 'Done' ? `${tickets.length} done` : `${tickets.length} more`;
  return html`<details class="board-fold" open=${open}><summary>${caption}${label === 'Blocked' && blockers.length ? ` · waiting on ${blockers.map((t) => `#${t.number}`).join(', ')}` : ' ›'}</summary>
    ${tickets.map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${lanePolicy} slim />`)}
  </details>`;
}
function Lane({ lane, index, count, end, hours, onStart, onClose, mutate, busy, move, showDone = false, collapsed = false, onToggle }) {
  const [ending, setEnding] = useState(false);
  const [reviews, setReviews] = useState(false);
  const [lining, setLining] = useState(false);
  const [defaults, setDefaults] = useState(false);
  const [laneMenu, setLaneMenu] = useState(false);
  const [handoff, setHandoff] = useState(false);
  const policy = lane.review_policy;
  const groups = groupTickets(lane.tickets || []);
  const counts = lane.counts;
  const total = Object.values(counts).reduce((a, b) => a + b, 0) || 1;
  const goalRow = (lane.tickets || []).find((t) => t.repo === lane.goal.repo && t.number === lane.goal.number);
  return html`<section class=${`board-lane${collapsed ? ' collapsed' : ''}`} data-lane=${lane.id}>
    <header class="lane-header">
      <button class="lane-toggle" aria-expanded=${!collapsed} aria-label=${`${collapsed ? 'Expand' : 'Collapse'} lane ${lane.rank}`} onClick=${onToggle}><span class="lane-chevron">${collapsed ? '▸' : '▾'}</span><span class="lane-rank">${lane.rank}</span></button>
      <div class="lane-heading"><button class="ticket-title" onClick=${() => ticketLink(lane.goal)}>
        <span class="sub">${lane.goal.repo.split('/').pop()}</span> <span class="mono">#${lane.goal.number}</span> · ${lane.goal.title}</button>
        <div class="lane-progress" aria-label=${`${counts.done} done, ${counts.in_progress} in progress, ${total} total`}>
          <i class="done" style=${`width:${counts.done / total * 100}%`}></i><i class="in-progress" style=${`width:${counts.in_progress / total * 100}%`}></i></div>
        <span class="sub">${Object.entries(counts).filter(([, n]) => n).map(([state, n]) => `${n} ${state.replaceAll('_', ' ')}`).join(' · ')}
          ${lane.goal.state === 'standing' ? html`${Object.values(counts).some(Boolean) ? ' · ' : ''}<span class="cyan">${standingText(goalRow || { waits_on: lane.tickets || [] })}</span>` : null}</span>
      </div>
      <span class="anchor"><button class="review-pill" title=${policy ? setByText(policy) : 'This lane uses the default review policy'} onClick=${() => setReviews(!reviews)}>
        Reviews: <b>${policy ? reviewerText(policy.reviewer) : 'default'}</b> ▾</button>
        ${reviews ? html`<${PolicyPopover} scope="lane" repo=${lane.goal.repo} number=${lane.goal.number} title=${`Reviews for lane ${lane.rank} · ${lane.goal.title}`}
          policy=${policy} align="right" onClose=${() => setReviews(false)} onSaved=${boardChanged} />` : null}</span>
      <span class="anchor"><button class="review-pill" data-pop-anchor title=${lane.handoff ? 'Set for this lane; a ticket or agent setting wins' : 'This lane uses the handoff default'}
        onClick=${() => setHandoff(!handoff)}>Handoff: <b>${laneHandoffText(lane.handoff)}</b> ▾</button>
        ${handoff ? html`<${HandoffPopover} lane=${lane.goal} align="right" onClose=${() => { setHandoff(false); boardChanged(); }} />` : null}</span>
      <div class="lane-actions">${lane.goal.state === 'close_ready' ? html`<button class="btn sm pri" disabled=${busy} onClick=${() => onClose(lane.goal)}>Close</button>` : null}
        <span class="anchor"><button class="btn sm" data-pop-anchor onClick=${()=>setLaneMenu(!laneMenu)}>Lane actions ⋯</button>
        ${laneMenu?html`<${Popover} onClose=${()=>setLaneMenu(false)} align="right" className="menu">
          <button onClick=${()=>{setLaneMenu(false);setLining(true);}}>Shared launch setup…</button>
          <button onClick=${()=>{setLaneMenu(false);setDefaults(true);}}>Future launch default…</button>
          <button disabled=${busy||index===0} onClick=${()=>{setLaneMenu(false);move(index,-1);}}>Move lane up</button>
          <button disabled=${busy||index===count-1} onClick=${()=>{setLaneMenu(false);move(index,1);}}>Move lane down</button>
          <button onClick=${()=>{setLaneMenu(false);setEnding(true);}}>End lane…</button><//>`:null}</span></div>
    </header>
    ${lining ? html`<div class="board-start-overlay"><${SharedLaunchSetup} tickets=${lane.tickets||[]} goals=${new Set([ticketKey(lane.goal)])} onClose=${()=>setLining(false)} onSaved=${boardChanged} /></div>` : null}
    ${defaults ? html`<div class="board-start-overlay"><${LaneLaunchDefault} lane=${lane} onClose=${()=>setDefaults(false)} onSaved=${boardChanged} /></div>` : null}
    ${ending ? html`<div class="lane-confirm">End this lane? Its tickets stay on GitHub.
      <button class="btn sm" onClick=${() => setEnding(false)}>Cancel</button>
      <button class="btn sm danger" disabled=${busy} onClick=${() => mutate(`/client/board/lanes/${lane.id}`, 'DELETE')}>End lane</button></div>` : null}
    ${lane.stale ? html`<p class="err">GitHub data is stale. Refresh to try again.</p>` : null}
    ${(lane.cycles || []).length ? html`<p class="err">Dependency cycle: ${lane.cycles.map((chain) => chain.map((t) => `#${t.number}`).join(' → ')).join('; ')}</p>` : null}
    ${collapsed ? null : html`
    ${(lane.longest_chain || []).length ? html`<details class="critical-path"><summary>Critical path</summary> ${lane.longest_chain.map((t, i) => html`${i ? ' → ' : ''}<button class="link-btn" onClick=${() => ticketLink(t)}>#${t.number}</button>`)}</details>` : null}
    <${ColumnHeads} hours=${hours} />
    ${groups.active.map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} />`)}
    ${groups.blocked.filter(hasOperations).map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} slim />`)}
    <div class="lane-folds"><${Fold} tickets=${groups.blocked.filter(t=>!hasOperations(t))} label="Blocked" end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} />
      ${groups.done.filter(hasOperations).map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} slim />`)}
      <${Fold} tickets=${groups.done.filter(t=>!hasOperations(t))} label="Done" open=${showDone} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} /></div>`}
  </section>`;
}
/** The lane's Handoff pill: what the lane sets, or "default" (sm#2293). */
export function laneHandoffText(value) {
  if (!value) return 'default';
  if (value.enabled === false) return 'off';
  if (value.threshold_percent != null) return `${value.threshold_percent}%`;
  if (value.enabled === true) return 'on';
  return 'custom';
}

function AddLane({ repos, mutate, busy, onClose }) {
  const [repo, setRepo] = useState((repos[0] || {}).repo || '');
  const [number, setNumber] = useState('');
  return html`<${Popover} onClose=${onClose} align="right"><h2>Add lane</h2>
    <form onSubmit=${async (e) => { e.preventDefault(); if (await mutate('/client/board/lanes', 'POST', { repo: repo.trim(), number: Number(number) })) onClose(); }}>
      <label class="fld"><span class="l">Repo</span><input class="inp" required placeholder="owner/repo" list="board-repos" value=${repo} onInput=${(e) => setRepo(e.target.value)} /></label>
      <datalist id="board-repos">${repos.map((r) => html`<option value=${r.repo}></option>`)}</datalist>
      <label class="fld"><span class="l">Goal ticket</span><input class="inp" required type="number" min="1" step="1" value=${number} onInput=${(e) => setNumber(e.target.value)} /></label>
      <div class="row"><button class="btn" type="button" onClick=${onClose}>Cancel</button><button class="btn pri" disabled=${busy}>Add lane</button></div>
    </form><//>`;
}
/** Why a ticket needs the owner now, or null. Lower order is more pressing (#2563). */
export function attentionReason(t) {
  if (t.state === 'done') return null;
  if (t.needs_you) return { order: 0, tone: 'magenta', text: t.needs_you.text, url: t.needs_you.url };
  if (t.state === 'close_ready') return { order: 1, tone: 'cyan', text: 'Every part is done', close: true };
  if ((t.warnings || []).includes('merged_not_closed')) return { order: 1, tone: 'amber', text: 'PR merged · close this ticket on GitHub' };
  if (t.auto_start?.state === 'failed') return { order: 2, tone: 'amber', text: `Auto-start failed${t.auto_start.last_error ? ` · ${t.auto_start.last_error}` : ''}` };
  if (t.jobs?.some((j) => j.quiet_since)) return { order: 3, tone: 'red', text: 'A queue job has gone quiet' };
  if (['stalled', 'no_agent'].includes(t.clock?.ball)) return { order: 3, tone: 'red', text: `Nothing moving · ${t.clock.text}` };
  return null;
}
function boardRows(lanes, others) {
  const seen = new Set();
  return [...lanes.flatMap((lane) => (lane.tickets || []).map((ticket) => ({ ticket, lane }))),
    ...others.flatMap((group) => group.tickets.map((ticket) => ({ ticket, lane: null })))]
    .filter(({ ticket }) => !seen.has(ticketKey(ticket)) && seen.add(ticketKey(ticket)));
}
/** Every ticket across lanes that waits on the owner, most pressing first; each once. */
export const attentionItems = (lanes, others = []) => boardRows(lanes, others)
  .flatMap((row) => { const reason = attentionReason(row.ticket); return reason ? [{ ...row, ...reason }] : []; })
  .sort((a, b) => a.order - b.order);
/** Startable tickets nobody holds, in lane order; each once. */
export const readyItems = (lanes, others = []) => boardRows(lanes, others)
  .filter(({ ticket }) => canStart(ticket) && !ticket.holder && !attentionReason(ticket));
export const shortGoal = (title) => title.split(/\s*[:(]/)[0].trim() || title;
function MoveRow({ item, jump, action }) {
  const { ticket, lane } = item;
  return html`<div class="move-row">
    <button class="lane-badge" title=${lane ? lane.goal.title : ticket.repo} onClick=${() => lane ? jump(lane.id) : ticketLink(ticket)}>${lane ? lane.rank : ticket.repo.split('/').pop()}</button>
    <button class="ticket-title" onClick=${() => lane ? jump(lane.id, ticketKey(ticket)) : ticketLink(ticket)}><span class="mono">#${ticket.number}</span> ${ticket.title}</button>
    <span class=${`sub move-reason ${item.tone || ''}`}>${item.text || 'Ready to start'}</span>
    <span class="move-action">${action}</span></div>`;
}
function YourMove({ attention, ready, jump, onStart, onClose, busy }) {
  if (!attention.length && !ready.length) return html`<section class="board-move empty"><span class="sub">Nothing on the board waits on you.</span></section>`;
  return html`<section class="board-move" aria-label="Your move">
    <h2>Your move <span class="sub">${attention.length ? `${attention.length} waiting on you` : ''}${attention.length && ready.length ? ' · ' : ''}${ready.length ? `${ready.length} ready to start` : ''}</span></h2>
    ${attention.map((item) => html`<${MoveRow} key=${ticketKey(item.ticket)} item=${item} jump=${jump} action=${
      item.url ? html`<a class="btn sm pri" href=${item.url} target="_blank" rel="noopener noreferrer">Open</a>`
      : item.close ? html`<button class="btn sm pri" disabled=${busy} onClick=${() => onClose(item.ticket)}>Close</button>`
      : html`<button class="btn sm" onClick=${() => item.lane ? jump(item.lane.id, ticketKey(item.ticket)) : ticketLink(item.ticket)}>Show</button>`} />`)}
    ${ready.map((item) => html`<${MoveRow} key=${ticketKey(item.ticket)} item=${item} jump=${jump}
      action=${html`<button class="btn sm pri" onClick=${() => onStart(item.ticket)}>Start</button>`} />`)}
  </section>`;
}
function LaneStrip({ lanes, attention, collapsed, jump, setAll }) {
  if (!lanes.length) return null;
  const allFolded = lanes.every((l) => collapsed.includes(l.id));
  return html`<nav class="lane-strip" aria-label="Lanes">
    ${lanes.map((lane) => {
      const waiting = attention.filter((i) => i.lane?.id === lane.id).length, running = lane.counts?.in_progress || 0;
      return html`<button key=${lane.id} class=${`lane-chip${collapsed.includes(lane.id) ? ' folded' : ''}`} title=${lane.goal.title} onClick=${() => jump(lane.id)}>
        <b>${lane.rank}</b><span class="lane-chip-title">${shortGoal(lane.goal.title)}</span>
        ${waiting ? html`<span class="count magenta" title=${`${waiting} waiting on you`}>${waiting}</span>` : null}
        ${running ? html`<span class="count green" title=${`${running} in progress`}>${running}▸</span>` : null}</button>`;
    })}
    <button class="link-btn lane-strip-all" onClick=${() => setAll(!allFolded)}>${allFolded ? 'Expand all' : 'Collapse all'}</button>
  </nav>`;
}
export function BoardPage() {
  const [hours, setHours] = useState(() => { const saved = stored('sm-board-clock-hours', 3); return [3, 6, 24].includes(saved) ? saved : 3; });
  const [data, error, reload] = usePoll(() => api(`/client/board?clock_hours=${hours}`), 30000, [hours]);
  const [actionError, setActionError] = useState(null);
  const [busy, setBusy] = useState(false);
  const [adding, setAdding] = useState(false);
  const [starting, setStarting] = useState(null);
  const [filter, setFilter] = useState('all');
  const [query, setQuery] = useState('');
  const [collapsed, setCollapsed] = useState(() => { const saved = stored('sm-board-collapsed', []); return Array.isArray(saved) ? saved : []; });
  const saveCollapsed = (ids) => { store('sm-board-collapsed', ids); setCollapsed(ids); };
  const toggleLane = (id) => saveCollapsed(collapsed.includes(id) ? collapsed.filter((x) => x !== id) : [...collapsed, id]);
  /** Scroll to a lane or a ticket row, opening whatever hides it. */
  const jump = (laneId, key = null) => {
    if (laneId != null && collapsed.includes(laneId)) saveCollapsed(collapsed.filter((x) => x !== laneId));
    requestAnimationFrame(() => requestAnimationFrame(() => {
      const lane = laneId != null ? document.querySelector(`[data-lane="${laneId}"]`) : document;
      const el = key ? lane?.querySelector(`[data-ticket="${CSS.escape(key)}"]`) : lane;
      if (!el || el === document) return;
      const fold = el.closest('details'); if (fold) fold.open = true;
      el.scrollIntoView({ behavior: 'smooth', block: key ? 'center' : 'start' });
      el.classList.remove('flash'); void el.offsetWidth; el.classList.add('flash');
    }));
  };
  const visibleLanes = data?.lanes.map(l=>({...l,tickets:(l.tickets||[]).filter(t=>matchesFilter(t,filter,query,l.goal.title))})).filter(l=>l.tickets.length)||[];
  const visibleOthers = (data?.other||[]).map(g=>({...g,tickets:g.tickets.filter(t=>matchesFilter(t,filter,query))})).filter(g=>g.tickets.length);
  const refreshTimer = useRef(null);
  useEffect(() => () => clearTimeout(refreshTimer.current), []);
  useEffect(() => bus.on('board-changed', reload), [reload]);
  useEffect(() => {
    if (!data) return;
    // Acknowledge each rendered snapshot, never an unseen poll result.
    const acknowledge = () => {
      if (document.hidden) return;
      api('/client/board/seen', { method: 'POST' }).then(() => api('/client/board/badge'))
        .then((badge) => setShared('board_badge', badge)).catch((e) => setActionError(e.message));
    };
    acknowledge();
    document.addEventListener('visibilitychange', acknowledge);
    return () => document.removeEventListener('visibilitychange', acknowledge);
  }, [data]);
  const mutate = async (path, method, body) => {
    if (busy) return false;
    setBusy(true); setActionError(null);
    try {
      await api(path, { method, body });
      await reload();
      if (path === '/client/board/refresh') {
        // The 202 response queues a GitHub pass; it does not contain its result.
        clearTimeout(refreshTimer.current);
        refreshTimer.current = setTimeout(reload, 2000);
      }
      return true;
    }
    catch (e) { setActionError(e.message); return false; }
    finally { setBusy(false); }
  };
  const move = (index, delta) => {
    const ids = data.lanes.map((lane) => lane.id);
    [ids[index], ids[index + delta]] = [ids[index + delta], ids[index]];
    return mutate('/client/board/order', 'PUT', { lane_ids: ids });
  };
  const close = (ticket) => mutate('/client/board/close', 'POST', { repo: ticket.repo, number: ticket.number });
  const startTicket = (ticket, mode = 'start') => setStarting({ ticket, mode });
  const attention = data ? attentionItems(data.lanes, data.other || []) : [];
  const ready = data ? readyItems(data.lanes, data.other || []) : [];
  return html`<${AutoStartPaused.Provider} value=${!!(data && data.auto_start_paused)}><div class="content board-content">
    <div class="board-toolbar"><${Seg} label="Board filter" value=${filter} onChange=${setFilter} options=${[{value:'all',label:'All work'},{value:'attention',label:'Needs attention'},{value:'armed',label:'Armed'},{value:'finished',label:'Finished'}]} />
      <input class="inp" aria-label="Find a ticket or goal" placeholder="Find a ticket or goal…" value=${query} onInput=${e=>setQuery(e.target.value)} />
      <span class="sub board-updated">${data ? `${data.lanes.length} lanes · updated ${age(data.generated_at)} ago` : 'Loading board…'}</span>
      <${Seg} label="Clock window" value=${hours} options=${[3, 6, 24].map((n) => ({ value: n, label: `${n} h` }))} onChange=${(n) => { store('sm-board-clock-hours', n); setHours(n); }} />
      <button class="btn" disabled=${busy} onClick=${() => mutate('/client/board/refresh', 'POST')}>Refresh</button>
      <span class="anchor"><button class="btn" data-pop-anchor onClick=${() => setAdding(!adding)}>Add lane</button>
        ${adding ? html`<${AddLane} repos=${data ? data.repos : []} mutate=${mutate} busy=${busy} onClose=${() => setAdding(false)} />` : null}</span>
    </div>
    ${data ? html`<${YourMove} attention=${attention} ready=${ready} jump=${jump} onStart=${startTicket} onClose=${close} busy=${busy} />
      <${LaneStrip} lanes=${data.lanes} attention=${attention} collapsed=${collapsed} jump=${jump}
        setAll=${(fold) => saveCollapsed(fold ? data.lanes.map((l) => l.id) : [])} />` : null}
    ${error || actionError ? html`<p class="err" role="alert">${actionError || error.message}</p>` : null}
    ${data ? html`
      ${data.lanes.length ? null : html`<div class="stub">No active lanes. Add a goal ticket to start a lane.</div>`}
      ${boardSections(visibleLanes, visibleOthers).map(({ lane, other: group }) => lane ? html`<${Lane} key=${lane.id} lane=${lane} index=${data.lanes.findIndex(l=>l.id===lane.id)} count=${data.lanes.length}
        end=${data.generated_at} hours=${hours} onStart=${startTicket} onClose=${close} mutate=${mutate} busy=${busy} move=${move} showDone=${filter==='finished'}
        collapsed=${collapsed.includes(lane.id)} onToggle=${() => toggleLane(lane.id)} />` : html`<section key=${`other ${group.repo}`} class="board-lane other-tickets"><h2>${group.repo} · Other tickets</h2><${ColumnHeads} hours=${hours} />
        ${visibleOther(group.tickets).map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${data.generated_at} hours=${hours} onStart=${startTicket} onClose=${close} busy=${busy} />`)}
        ${group.tickets.length > visibleOther(group.tickets).length ? html`<${Fold} tickets=${group.tickets.filter((ticket) => !visibleOther(group.tickets).includes(ticket))} label="More" end=${data.generated_at} hours=${hours} onStart=${startTicket} onClose=${close} busy=${busy} />` : null}</section>`)}
      <div class="clock-legend"><span class="ball green">agent working</span><span class="ball green">queue job running (hatched)</span><span class="ball amber">queue or review</span><span class="ball magenta">waiting on you</span><span class="ball red">nothing moving</span></div>` : null}
    ${starting ? html`<div class="board-start-overlay"><${TicketStart} key=${`${ticketKey(starting.ticket)} ${starting.mode}`} ticket=${starting.ticket}
      mode=${starting.mode} onClose=${() => setStarting(null)} onStarted=${reload} /></div>` : null}
  </div><//>`;
}
