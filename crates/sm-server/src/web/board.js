// Board (1710 D6.4). The server owns ticket states, ordering and clock rules.
import { createContext } from 'preact';
import { useContext, useEffect, useRef, useState } from 'preact/hooks';
import { html, api, bus, providerLabel, usePoll, stored, store, Seg, Popover, Links, openItem, openPanel, navigate, setShared, toast, age, useNow } from './ui.js';
import { TicketStart, LaneWhenReady, blockedReasons, canStartAnyway, chipText, retryBody, laneCandidates } from './board-start.js';
import { SharedLaunchSetup, LaneLaunchDefault, distinctTickets } from './launch-setup.js';
import { TypePicker, exactConfig, typeConfig } from './launch-fields.js';
import { HandoffPopover } from './handoff.js';
import { PolicyPopover, reviewerText, setByText } from './reviews.js';

export const BALL_TONE = { you: 'magenta', working: 'green', job_running: 'green', queue: 'amber', review: 'amber', idle: 'muted', stalled: 'red', job_quiet: 'red', no_agent: 'red' };
export const BLOCKED_ROW_LIMIT = 12;
export const DONE_ROW_LIMIT = 3;
export const OTHER_ROW_LIMIT = 10;
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
const BoardSelection = createContext({selected:new Set(),toggle:()=>{}});
function TicketRow({ ticket, end, hours, onStart, onClose, busy, lanePolicy = null }) {
  const [menu, setMenu] = useState(null);
  const selection = useContext(BoardSelection);
  const now = useNow();
  const parts = ticket.sub_issues, can = rowActions(ticket), holder = ticket.holder;
  const config = ticket.auto_start ? { ...ticket.auto_start, reasoning_effort:ticket.auto_start.effort } : ticket.launch_preference?.config;
  const reviews = currentReviews(ticket);
  return html`<div class="board-ticket" data-ticket=${ticketKey(ticket)}>
    <div class="board-ticket-main">
      <div class="ticket-title-line"><input type="checkbox" aria-label=${`Select ${ticket.repo}#${ticket.number}`} checked=${selection.selected.has(ticketKey(ticket))} onChange=${()=>selection.toggle(ticketKey(ticket))} />
        <button class="ticket-title" onClick=${()=>ticketLink(ticket)}><span class="mono">#${ticket.number}</span> ${ticket.title}</button></div>
      <span class=${`ticket-state-label ${ticket.state==='needs_you'?'magenta':''}`}>${ticket.state.replaceAll('_',' ')}</span>
      <div class="ticket-direct-links"><a href=${ticket.url||`https://github.com/${ticket.repo}/issues/${ticket.number}`} target="_blank" rel="noopener noreferrer">Issue #${ticket.number} ↗</a>
        ${(ticket.prs||[]).map(pr=>html`<span><button class="link-btn" onClick=${()=>openPanel(`ticket:${pr.repo||ticket.repo}#${pr.number}`)}>PR #${pr.number} · ${(pr.state||'open').toLowerCase()}</button> <a aria-label=${`Open PR ${pr.number} on GitHub`} href=${pr.url||`https://github.com/${pr.repo||ticket.repo}/pull/${pr.number}`} target="_blank" rel="noopener noreferrer">↗</a></span>`)}</div>
      ${ticket.needs_you?html`<span class="sub magenta">${ticket.needs_you.text} · <a href=${ticket.needs_you.url} target="_blank" rel="noopener noreferrer">Open</a></span>`:null}
      ${ticket.state==='blocked'?html`<span class="sub">${blockedReasons(ticket).join(' ')}</span>`:null}
      ${(ticket.warnings||[]).includes('merged_not_closed')?html`<span class="sub amber">PR merged · close this ticket on GitHub.</span>`:null}
      ${ticket.state==='close_ready'?html`<span class="sub cyan">${parts?.done||0} of ${parts?.total||0} parts done</span>`:null}
      ${ticket.state==='standing'?html`<span class="sub cyan">${standingText(ticket)}</span>`:null}
      ${ticket.started_early?html`<span class="sub amber">Started early</span>`:null}
      <${Links} thread=${ticket.thread} docs=${ticket.docs||[]} />
      ${ticket.clock?html`<details class="board-time"><summary>Time spent · last ${hours}h</summary><${Clock} ticket=${ticket} end=${end} hours=${hours} /></details>`:null}
    </div>
    <div class="board-agent"><span class="board-column-label">Agent / terminal</span>${holder?html`
      <div class="ticket-agent-links"><button class="link-btn" onClick=${()=>openPanel(`agent:${holder.session_id}`)}>${holder.name}</button><button class="btn sm" aria-label=${`Terminal for ${holder.name}`} onClick=${()=>navigate(`/terminal/${encodeURIComponent(holder.session_id)}`)}>⌨</button></div>
      <span class=${`sub ${holder.state==='working'?'green':''}`}>${holder.state==='working'?'● Working':holder.state==='idle'?'○ Idle':holder.state==='stopped'||holder.state==='retired'?'Stopped':holder.state||'Activity unknown'}${holder.state==='working'?` · ${operationalAge(holder.since,now)}`:''}</span>
      <span class=${holder.provider === 'opencode' ? 'prov local' : 'sub'}>${holder.provider === 'opencode' ? providerLabel(holder.provider).toUpperCase() : holder.provider?.startsWith('codex')?'Codex':holder.provider==='claude'?'Claude':holder.provider||''}</span>`:html`<span class="sub">No agent assigned</span>`}</div>
    <div class="board-waits"><span class="board-column-label">Work / waits</span>
      ${(ticket.jobs||[]).map(job=>html`<button class=${`board-job ${job.quiet_since?'red':job.state==='running'?'green':'amber'}`} onClick=${()=>{navigate('/queue');openPanel(`job:${job.id}`);}}>
        <b>${job.label||job.id}</b><span>${job.state==='running'?'Running':'Waiting'} · ${operationalAge(job.since||job.started_at||job.queued_at,now)}</span>${job.quiet_since?html`<span>Quiet for ${operationalAge(job.quiet_since,now)}</span>`:null}</button>`)}
      ${reviews.map(r=>html`<span class=${`board-review ${r.by==='you'?'magenta':'amber'}`}>${r.reviewer_label||'Review'}${r.number?` · PR #${r.number}`:''}<span>Waiting ${operationalAge(r.since||r.waiting_since,now)}</span></span>`)}
      ${!ticket.jobs?.length&&!reviews.length?html`<span class="sub">${ticket.state==='blocked'?`Dependencies: ${blockedText(ticket)||'see ticket warnings'}`:holder?.state==='working'?'Agent working':holder?'No current job or review wait':'No current job'}</span>`:null}
    </div>
    <div class="board-launch"><span class="board-column-label">Launch</span>${config?html`<span class="sub launch-exact">${exactConfig(config)}</span>`:null}
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
function Fold({ tickets, label, end, hours, onStart, onClose, busy, lanePolicy = null }) {
  if (!tickets.length) return null;
  const blockers = [...new Map(tickets.flatMap(openBlockers).map((t) => [ticketKey(t), t])).values()];
  const caption = label === 'Blocked' ? `${tickets.length} blocked` : label === 'More done' ? `${tickets.length} more done` : `${tickets.length} more`;
  return html`<details class="board-fold"><summary>${caption}${label === 'Blocked' && blockers.length ? ` · waiting on ${blockers.map((t) => `#${t.number}`).join(', ')}` : ' ›'}</summary>
    ${tickets.map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${lanePolicy} slim />`)}
  </details>`;
}
function Lane({ lane, index, count, end, hours, onStart, onClose, mutate, busy, move }) {
  const [ending, setEnding] = useState(false);
  const [reviews, setReviews] = useState(false);
  const [lining, setLining] = useState(false);
  const [defaults, setDefaults] = useState(false);
  const [laneMenu, setLaneMenu] = useState(false);
  const policy = lane.review_policy;
  const groups = groupTickets(lane.tickets || []);
  const counts = lane.counts;
  const total = Object.values(counts).reduce((a, b) => a + b, 0) || 1;
  const short = false;
  const goalRow = (lane.tickets || []).find((t) => t.repo === lane.goal.repo && t.number === lane.goal.number);
  const showBlocked = groups.active.length + groups.blocked.length <= BLOCKED_ROW_LIMIT;
  return html`<section class="board-lane">
    <header class="lane-header">
      <span class="lane-rank">${lane.rank}</span>
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
      <div class="lane-actions">${lane.goal.state === 'close_ready' ? html`<button class="btn sm pri" disabled=${busy} onClick=${() => onClose(lane.goal)}>Close</button>` : null}
        <span class="anchor"><button class="btn sm" data-pop-anchor onClick=${()=>setLaneMenu(!laneMenu)}>Lane actions ⋯</button>
        ${laneMenu?html`<${Popover} onClose=${()=>setLaneMenu(false)} align="right" className="menu">
          <button onClick=${()=>{setLaneMenu(false);setLining(true);}}>Shared launch setup…</button>
          <button onClick=${()=>{setLaneMenu(false);setDefaults(true);}}>Future launch default…</button>
          <button disabled=${busy||index===0} onClick=${()=>{setLaneMenu(false);move(index,-1);}}>Move lane up</button>
          <button disabled=${busy||index===count-1} onClick=${()=>{setLaneMenu(false);move(index,1);}}>Move lane down</button>
          <button onClick=${()=>{setLaneMenu(false);setEnding(true);}}>End lane…</button><//>`:null}</span></div>
      ${short ? html`<${TicketRow} ticket=${groups.active[0]} compact end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} />` : null}
    </header>
    ${lining ? html`<div class="board-start-overlay"><${SharedLaunchSetup} tickets=${lane.tickets||[]} goals=${new Set([ticketKey(lane.goal)])} onClose=${()=>setLining(false)} onSaved=${boardChanged} /></div>` : null}
    ${defaults ? html`<div class="board-start-overlay"><${LaneLaunchDefault} lane=${lane} onClose=${()=>setDefaults(false)} onSaved=${boardChanged} /></div>` : null}
    ${ending ? html`<div class="lane-confirm">End this lane? Its tickets stay on GitHub.
      <button class="btn sm" onClick=${() => setEnding(false)}>Cancel</button>
      <button class="btn sm danger" disabled=${busy} onClick=${() => mutate(`/client/board/lanes/${lane.id}`, 'DELETE')}>End lane</button></div>` : null}
    ${lane.stale ? html`<p class="err">GitHub data is stale. Refresh to try again.</p>` : null}
    ${(lane.cycles || []).length ? html`<p class="err">Dependency cycle: ${lane.cycles.map((chain) => chain.map((t) => `#${t.number}`).join(' → ')).join('; ')}</p>` : null}
    ${(lane.longest_chain || []).length ? html`<details class="critical-path"><summary>Critical path</summary> ${lane.longest_chain.map((t, i) => html`${i ? ' → ' : ''}<button class="link-btn" onClick=${() => ticketLink(t)}>#${t.number}</button>`)}</details>` : null}
    ${!short ? groups.active.map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} />`) : null}
    ${(showBlocked ? groups.blocked : groups.blocked.filter(hasOperations)).map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} slim />`)}
    <div class="lane-folds">${!showBlocked ? html`<${Fold} tickets=${groups.blocked.filter(t=>!hasOperations(t))} label="Blocked" end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} />` : null}
      ${groups.done.filter((t,i)=>i<DONE_ROW_LIMIT || hasOperations(t)).map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} slim />`)}
      <${Fold} tickets=${groups.done.filter((t,i)=>i>=DONE_ROW_LIMIT && !hasOperations(t))} label="More done" end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} lanePolicy=${policy} /></div>
  </section>`;
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
export function BoardPage() {
  const [hours, setHours] = useState(() => { const saved = stored('sm-board-clock-hours', 3); return [3, 6, 24].includes(saved) ? saved : 3; });
  const [data, error, reload] = usePoll(() => api(`/client/board?clock_hours=${hours}`), 30000, [hours]);
  const [actionError, setActionError] = useState(null);
  const [busy, setBusy] = useState(false);
  const [adding, setAdding] = useState(false);
  const [starting, setStarting] = useState(null);
  const [selected, setSelected] = useState(new Set());
  const [batch, setBatch] = useState(null);
  const [filter, setFilter] = useState('all');
  const [query, setQuery] = useState('');
  const [settings, setSettings] = useState(null);
  useEffect(()=>{api('/client/settings').then(setSettings).catch(()=>{});},[]);
  const toggle = key => setSelected(prev=>{const next=new Set(prev);next.has(key)?next.delete(key):next.add(key);return next;});
  const allTickets = data ? distinctTickets([...data.lanes.flatMap(l=>l.tickets||[]),...(data.other||[]).flatMap(g=>g.tickets)]) : [];
  const goals = new Set(data?.lanes.map(l=>ticketKey(l.goal))||[]);
  const visibleLanes = data?.lanes.map(l=>({...l,tickets:(l.tickets||[]).filter(t=>matchesFilter(t,filter,query,l.goal.title))})).filter(l=>l.tickets.length)||[];
  const visibleOthers = (data?.other||[]).map(g=>({...g,tickets:g.tickets.filter(t=>matchesFilter(t,filter,query))})).filter(g=>g.tickets.length);
  const openBatch = preset => {const tickets=allTickets.filter(t=>selected.has(ticketKey(t)));if(!tickets.length){toast('Select tickets first');return;}setBatch({tickets,initialPreset:preset});};
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
  return html`<${AutoStartPaused.Provider} value=${!!(data && data.auto_start_paused)}><${BoardSelection.Provider} value=${{selected,toggle}}><div class="content board-content">
    <div class="board-toolbar"><span class="sub">${data ? `${data.lanes.length} lanes · updated ${age(data.generated_at)} ago` : 'Loading board…'}</span>
      <${Seg} label="Clock window" value=${hours} options=${[3, 6, 24].map((n) => ({ value: n, label: `${n} h` }))} onChange=${(n) => { store('sm-board-clock-hours', n); setHours(n); }} />
      <button class="btn" disabled=${busy} onClick=${() => mutate('/client/board/refresh', 'POST')}>Refresh</button>
      <span class="anchor"><button class="btn" data-pop-anchor onClick=${() => setAdding(!adding)}>Add lane</button>
        ${adding ? html`<${AddLane} repos=${data ? data.repos : []} mutate=${mutate} busy=${busy} onClose=${() => setAdding(false)} />` : null}</span>
    </div>
    <div class="board-filterbar"><${Seg} label="Board filter" value=${filter} onChange=${setFilter} options=${[{value:'all',label:'All work'},{value:'attention',label:'Needs attention'},{value:'armed',label:'Armed'},{value:'finished',label:'Finished'}]} />
      <input class="inp" aria-label="Find a ticket or goal" placeholder="Find a ticket or goal…" value=${query} onInput=${e=>setQuery(e.target.value)} /></div>
    <div class="board-presetbar"><span class="sub">QUICK PRESETS</span><${TypePicker} settings=${settings} other=${false} onPick=${t=>openBatch(typeConfig(t))} /><button class="link-btn" onClick=${()=>navigate('/settings')}>Manage</button></div>
    <div class="board-selectionbar"><button class="btn sm" onClick=${()=>{const boxes=[...document.querySelectorAll('.board-ticket input[type=checkbox]')].filter(el=>el.getClientRects().length);setSelected(prev=>new Set([...prev,...boxes.map(el=>el.closest('[data-ticket]').dataset.ticket)]));}}>Select visible</button>
      <span>${selected.size} selected${selected.size ? ` · ${allTickets.filter(t=>selected.has(ticketKey(t))&&!matchesFilter(t,filter,query)).length} outside current filter` : ''}</span>
      <button class="btn sm pri" disabled=${!selected.size} onClick=${()=>openBatch(null)}>Shared setup</button>${selected.size?html`<button class="link-btn" onClick=${()=>setSelected(new Set())}>Clear selection</button>`:null}</div>
    ${error || actionError ? html`<p class="err" role="alert">${actionError || error.message}</p>` : null}
    ${data ? html`
      ${data.lanes.length ? visibleLanes.map((lane) => html`<${Lane} key=${lane.id} lane=${lane} index=${data.lanes.findIndex(l=>l.id===lane.id)} count=${data.lanes.length}
        end=${data.generated_at} hours=${hours} onStart=${startTicket} onClose=${close} mutate=${mutate} busy=${busy} move=${move} />`) : html`<div class="stub">No active lanes. Add a goal ticket to start a lane.</div>`}
      ${visibleOthers.map((group) => html`<section class="board-lane other-tickets"><h2>${group.repo} · Other tickets</h2>
        ${visibleOther(group.tickets).map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${data.generated_at} hours=${hours} onStart=${startTicket} onClose=${close} busy=${busy} />`)}
        ${group.tickets.length > visibleOther(group.tickets).length ? html`<${Fold} tickets=${group.tickets.filter((ticket) => !visibleOther(group.tickets).includes(ticket))} label="More" end=${data.generated_at} hours=${hours} onStart=${startTicket} onClose=${close} busy=${busy} />` : null}</section>`)}
      <div class="clock-legend"><span class="ball green">agent working</span><span class="ball green">queue job running (hatched)</span><span class="ball amber">queue or review</span><span class="ball magenta">waiting on you</span><span class="ball red">nothing moving</span></div>` : null}
    ${batch?html`<div class="board-start-overlay"><${SharedLaunchSetup} tickets=${batch.tickets} goals=${goals} initialPreset=${batch.initialPreset} onClose=${()=>setBatch(null)} onSaved=${()=>{setSelected(new Set());reload();}} /></div>`:null}
    ${starting ? html`<div class="board-start-overlay"><${TicketStart} key=${`${ticketKey(starting.ticket)} ${starting.mode}`} ticket=${starting.ticket}
      mode=${starting.mode} onClose=${() => setStarting(null)} onStarted=${reload} /></div>` : null}
  </div><//><//>`;
}
