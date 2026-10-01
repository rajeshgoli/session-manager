// Board (1710 D6.4). The server owns ticket states, ordering and clock rules.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, usePoll, stored, store, Seg, Popover, Links, openItem, openPanel, navigate, setShared, age } from './ui.js';
import { TicketStart, blockedReasons, canStartAnyway } from './board-start.js';

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
  const urgent = tickets.filter((t) => t.state === 'needs_you');
  const rest = tickets.filter((t) => t.state !== 'needs_you');
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

function TicketRow({ ticket, end, hours, onStart, onClose, busy, compact = false, slim = false }) {
  const [menu, setMenu] = useState(false);
  const parts = ticket.sub_issues;
  const agent = ticket.holder ? { id: ticket.holder.session_id, name: ticket.holder.name,
    provider: ticket.holder.provider, fact: `${ticket.holder.state === 'working' ? '● Working' : '○ Idle'}${ticket.holder.since ? ` ${age(ticket.holder.since)}` : ''}` } : null;
  return html`<div class=${`board-ticket ${compact ? 'compact' : ''}`}>
    <button class="ticket-title" onClick=${() => ticketLink(ticket)}><span class="mono">#${ticket.number}</span> ${ticket.title}</button>
    ${canStart(ticket) ? html`<button class="btn sm pri" onClick=${() => onStart(ticket)}>Start</button>` : null}
    ${ticket.state === 'blocked' ? html`<button class="btn sm" onClick=${() => onStart(ticket)}>${canStartAnyway(ticket) ? 'Start anyway' : 'Why blocked'}</button>` : null}
    ${ticket.state === 'close_ready' ? html`<span class="ticket-actions"><button class="btn sm pri" disabled=${busy} onClick=${() => onClose(ticket)}>Close</button>
      <span class="anchor"><button class="icon-btn" title="More ticket actions" onClick=${() => setMenu(!menu)}>⋯</button>
        ${menu ? html`<${Popover} onClose=${() => setMenu(false)}><button class="btn sm" onClick=${() => { setMenu(false); onStart(ticket); }}>Start instead</button><//>` : null}</span></span>` : null}
    ${ticket.state !== 'blocked' && (ticket.warnings || []).includes('merged_not_closed') ? html`<span class="sub">PR merged · close this ticket on GitHub.</span>` : null}
    ${ticket.state === 'blocked' ? html`<span class="sub ticket-state">${blockedReasons(ticket).join(' ')}</span>` : null}
    ${ticket.state === 'close_ready' ? html`<span class="sub ticket-state cyan">${parts?.done || 0} of ${parts?.total || 0} parts done · All parts done</span>` : null}
    ${ticket.started_early ? html`<span class="sub ticket-state">started early</span>` : null}
    ${!slim ? html`<${Clock} ticket=${ticket} end=${end} hours=${hours} />` : null}
    ${!slim && !ticket.clock && !['ready', 'blocked', 'close_ready'].includes(ticket.state) ? html`<span class="sub ticket-state">${ticket.state.replaceAll('_', ' ')}${ticket.holder ? ` · ${ticket.holder.name}` : ''}</span>` : null}
    <${Links} ticket=${ticket} prs=${ticket.prs || []} agent=${agent} jobs=${ticket.jobs || []} thread=${ticket.thread} docs=${ticket.docs || []} />
  </div>`;
}
function Fold({ tickets, label, end, hours, onStart, onClose, busy }) {
  if (!tickets.length) return null;
  const blockers = [...new Map(tickets.flatMap(openBlockers).map((t) => [ticketKey(t), t])).values()];
  const caption = label === 'Blocked' ? `${tickets.length} blocked` : label === 'More done' ? `${tickets.length} more done` : `${tickets.length} more`;
  return html`<details class="board-fold"><summary>${caption}${label === 'Blocked' && blockers.length ? ` · waiting on ${blockers.map((t) => `#${t.number}`).join(', ')}` : ' ›'}</summary>
    ${tickets.map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} slim />`)}
  </details>`;
}
function Lane({ lane, index, count, end, hours, onStart, onClose, mutate, busy, move }) {
  const [ending, setEnding] = useState(false);
  const groups = groupTickets(lane.tickets || []);
  const counts = lane.counts;
  const total = Object.values(counts).reduce((a, b) => a + b, 0) || 1;
  const short = groups.active.length === 1;
  const showBlocked = groups.active.length + groups.blocked.length <= BLOCKED_ROW_LIMIT;
  return html`<section class="board-lane">
    <header class="lane-header">
      <span class="lane-rank">${lane.rank}</span>
      <div class="lane-heading"><button class="ticket-title" onClick=${() => ticketLink(lane.goal)}>
        <span class="sub">${lane.goal.repo.split('/').pop()}</span> <span class="mono">#${lane.goal.number}</span> · ${lane.goal.title}</button>
        <div class="lane-progress" aria-label=${`${counts.done} done, ${counts.in_progress} in progress, ${total} total`}>
          <i class="done" style=${`width:${counts.done / total * 100}%`}></i><i class="in-progress" style=${`width:${counts.in_progress / total * 100}%`}></i></div>
        <span class="sub">${Object.entries(counts).filter(([, n]) => n).map(([state, n]) => `${n} ${state.replaceAll('_', ' ')}`).join(' · ')}</span>
      </div>
      <div class="lane-actions">${lane.goal.state === 'close_ready' ? html`<button class="btn sm pri" disabled=${busy} onClick=${() => onClose(lane.goal)}>Close</button>` : null}
        <button class="icon-btn" title="Move lane up" disabled=${busy || index === 0} onClick=${() => move(index, -1)}>↑</button>
        <button class="icon-btn" title="Move lane down" disabled=${busy || index === count - 1} onClick=${() => move(index, 1)}>↓</button>
        <button class="btn sm" disabled=${busy} onClick=${() => setEnding(!ending)}>End</button></div>
      ${short ? html`<${TicketRow} ticket=${groups.active[0]} compact end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} />` : null}
    </header>
    ${ending ? html`<div class="lane-confirm">End this lane? Its tickets stay on GitHub.
      <button class="btn sm" onClick=${() => setEnding(false)}>Cancel</button>
      <button class="btn sm danger" disabled=${busy} onClick=${() => mutate(`/client/board/lanes/${lane.id}`, 'DELETE')}>End lane</button></div>` : null}
    ${lane.stale ? html`<p class="err">GitHub data is stale. Refresh to try again.</p>` : null}
    ${(lane.cycles || []).length ? html`<p class="err">Dependency cycle: ${lane.cycles.map((chain) => chain.map((t) => `#${t.number}`).join(' → ')).join('; ')}</p>` : null}
    ${(lane.longest_chain || []).length ? html`<div class="critical-path">Critical path: ${lane.longest_chain.map((t, i) => html`${i ? ' → ' : ''}<button class="link-btn" onClick=${() => ticketLink(t)}>#${t.number}</button>`)}</div>` : null}
    ${!short ? groups.active.map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} />`) : null}
    ${showBlocked ? groups.blocked.map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} slim />`) : null}
    <div class="lane-folds">${!showBlocked ? html`<${Fold} tickets=${groups.blocked} label="Blocked" end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} />` : null}
      ${groups.done.slice(0, DONE_ROW_LIMIT).map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} slim />`)}
      <${Fold} tickets=${groups.done.slice(DONE_ROW_LIMIT)} label="More done" end=${end} hours=${hours} onStart=${onStart} onClose=${onClose} busy=${busy} /></div>
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
  const refreshTimer = useRef(null);
  useEffect(() => () => clearTimeout(refreshTimer.current), []);
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
  return html`<div class="content board-content">
    <div class="board-toolbar"><span class="sub">${data ? `${data.lanes.length} lanes · updated ${age(data.generated_at)} ago` : 'Loading board…'}</span>
      <${Seg} label="Clock window" value=${hours} options=${[3, 6, 24].map((n) => ({ value: n, label: `${n} h` }))} onChange=${(n) => { store('sm-board-clock-hours', n); setHours(n); }} />
      <button class="btn" disabled=${busy} onClick=${() => mutate('/client/board/refresh', 'POST')}>Refresh</button>
      <span class="anchor"><button class="btn" data-pop-anchor onClick=${() => setAdding(!adding)}>Add lane</button>
        ${adding ? html`<${AddLane} repos=${data ? data.repos : []} mutate=${mutate} busy=${busy} onClose=${() => setAdding(false)} />` : null}</span>
    </div>
    ${error || actionError ? html`<p class="err" role="alert">${actionError || error.message}</p>` : null}
    ${data ? html`
      ${data.lanes.length ? data.lanes.map((lane, index) => html`<${Lane} key=${lane.id} lane=${lane} index=${index} count=${data.lanes.length}
        end=${data.generated_at} hours=${hours} onStart=${setStarting} onClose=${close} mutate=${mutate} busy=${busy} move=${move} />`) : html`<div class="stub">No active lanes. Add a goal ticket to start a lane.</div>`}
      ${(data.other || []).map((group) => html`<section class="board-lane other-tickets"><h2>${group.repo} · Other tickets</h2>
        ${visibleOther(group.tickets).map((ticket) => html`<${TicketRow} key=${ticketKey(ticket)} ticket=${ticket} end=${data.generated_at} hours=${hours} onStart=${setStarting} onClose=${close} busy=${busy} />`)}
        ${group.tickets.length > visibleOther(group.tickets).length ? html`<${Fold} tickets=${group.tickets.filter((ticket) => !visibleOther(group.tickets).includes(ticket))} label="More" end=${data.generated_at} hours=${hours} onStart=${setStarting} onClose=${close} busy=${busy} />` : null}</section>`)}
      <div class="clock-legend"><span class="ball green">agent working</span><span class="ball green">queue job running (hatched)</span><span class="ball amber">queue or review</span><span class="ball magenta">waiting on you</span><span class="ball red">nothing moving</span></div>` : null}
    ${starting ? html`<div class="board-start-overlay"><${TicketStart} key=${ticketKey(starting)} ticket=${starting} onClose=${() => setStarting(null)} onStarted=${reload} /></div>` : null}
  </div>`;
}
