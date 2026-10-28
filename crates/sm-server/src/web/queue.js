import { useEffect, useLayoutEffect, useState, useRef } from 'preact/hooks';
import { html, api, bus, closePanel, usePoll, useNow, useShared, setShared, stored, store, registerPanel, openPanel, navigate, Seg, duration, age, clock, gigabytes, toast } from './ui.js';
import { timelineSegments, limitsInsight, waitingGroups, chartPath, coverageNote, jobAgentId, jobAgentLabel, askJobQuestion, reviewJobText, reviewRowText, usageBreakdown, usageText, jobUsage, JOB_COLORS } from './queue-model.js';

import { requestStart } from './queue-start.js';

const ranges = [{ value: 1, label: '1h' }, { value: 24, label: '24h' }, { value: 168, label: '7d' }, { value: 720, label: '30d' }];
const pct = (value) => typeof value === 'number' ? `${value.toFixed(1)}%` : '—';
const gb = (value) => typeof value === 'number' ? `${gigabytes(value)}G` : '—';
const title = (job) => job.label || job.id;
const errorText = (error) => error ? html`<p class="err" role="alert">${error.message}</p>` : null;

export function QueuePage() {
  const [insightDismissed, setInsightDismissed] = useState(() => stored('queue-limits-insight-dismissed', false));
  const [queue, error, reloadQueue] = usePoll(async () => { const q = await api('/client/queue'); setShared('queue', q); return q; }, 5000);
  useEffect(() => bus.on('queue-changed', reloadQueue), [reloadQueue]);
  const [stats] = usePoll(() => api('/client/queue/stats?hours=24'), 60000);
  const now = useNow(5000);
  if (!queue) return html`<div class="content">${errorText(error) || 'Loading queue…'}</div>`;
  return html`<div class="content queue-page">${errorText(error)}
    <div class="q-tiles">${['review', 'tests', 'background', 'perf', 'service'].map((type) => {
      const slot = queue.slots?.by_type?.[type];
      const waiting = queue.queued.filter((job) => job.type === type).length;
      return html`<div class="q-card"><span class="q-label">${type}</span><strong>${slot?.running ?? '—'} <small>of ${slot?.max ?? '—'}</small></strong>${waiting ? html`<span class="amber">${waiting} waiting</span>` : null}</div>`;
    })}<div class="q-card"><span class="q-label">shell</span><strong>${queue.slots?.by_type?.shell?.running ?? 0} <small>running</small></strong><span class="muted">no limit</span></div></div>
    <${UsageNow} running=${queue.running} />
    <${MacChart} />
    <${LocalModelCard} card=${queue.local_model} />
    ${!insightDismissed ? html`<${HeldBack} stats=${stats} insightOnly=${true} onDismiss=${() => { store('queue-limits-insight-dismissed', true); setInsightDismissed(true); }} />` : null}
    <${ReviewsCard} reviews=${queue.reviews || []} now=${now} />
    <section class="q-card"><div class="q-heading"><h2>Running</h2><span class="muted">${queue.slots?.running ?? '—'} of ${queue.slots?.max ?? '—'} slots held · Last 3 hours → now</span></div>
      ${queue.running.length ? queue.running.map((job, i) => html`<${JobRow} key=${job.id} job=${job} now=${now} color=${i % JOB_COLORS} />`) : html`<p class="muted">No jobs running.</p>`}
      <h2>Waiting · in start order</h2>
      ${waitingGroups(queue.queued).map((group) => html`<div><p class="sub">${group.caption}</p>${group.jobs.map((job) => html`<${JobRow} key=${job.id} job=${job} now=${now} />`)}</div>`)}
      ${!queue.queued.length ? html`<p class="muted">No jobs waiting.</p>` : null}
    </section>
    ${queue.ended.length ? html`<a href="/analytics/queue" onClick=${(e) => { e.preventDefault(); navigate('/analytics/queue'); }}>${queue.ended.length} stopped in the last 24h ›</a>` : null}
  </div>`;
}

/** Every review in progress, whoever holds it, with Next reviewer (#2586). */
function ReviewsCard({ reviews, now }) {
  if (!reviews.length) return null;
  return html`<section class="q-card q-reviews"><div class="q-heading"><h2>Reviews</h2><span class="muted">${reviews.length} in progress</span></div>
    ${reviews.map((review) => html`<${ReviewRow} key=${`${review.request_id}:${review.step}`} review=${review} now=${now} />`)}
  </section>`;
}

function ReviewRow({ review, now }) {
  const [asking, setAsking] = useState(false), [busy, setBusy] = useState(false);
  const text = reviewRowText(review, now);
  const next = async () => {
    setBusy(true);
    try {
      await api(`/client/review-requests/${encodeURIComponent(review.request_id)}/next`, { method: 'POST', body: { step: review.step } });
      toast(text.done);
    } catch (e) {
      toast(e.status === 409 ? 'This review has already moved on.' : e.message);
    } finally {
      setBusy(false); setAsking(false); bus.emit('queue-changed');
    }
  };
  return html`<div class=${`q-review${asking ? ' asking' : ''}`}>
    <a class="q-review-pr" href=${`https://github.com/${review.repo}/pull/${review.pr_number}`} target="_blank" rel="noopener">
      <span class="q-review-title"><b>${text.pr}</b> ${text.title}</span>
      <small>${text.context}</small>
    </a>
    <div class="q-review-who">
      <span><b>${text.reviewer}</b>${text.model ? html`<small> ${text.model}</small>` : null}</span>
      <small>${text.status}</small>
    </div>
    <div class="q-review-act"><button type="button" class="btn sm" aria-pressed=${asking} onClick=${() => setAsking(!asking)}>Next reviewer</button></div>
    ${asking ? html`<div class="q-review-ask" role="group" aria-label=${text.ask}><span>${text.ask}${text.askDetail ? html` <small>${text.askDetail}</small>` : null}</span>
      <button type="button" class="btn sm pri" disabled=${busy} onClick=${next}>${busy ? 'Moving…' : text.confirm}</button>
      <button type="button" class="btn sm" disabled=${busy} onClick=${() => setAsking(false)}>Keep</button></div>` : null}
  </div>`;
}

export function LocalModelCard({ card }) {
  if (!card) return null;
  return html`<section class="q-card"><h2>Local model</h2><dl class="q-details">
    <dt>Model</dt><dd>${card.model_text}</dd>
    <dt>Seats</dt><dd>${card.seats_text}</dd>
    <dt>Memory headroom</dt><dd>${card.memory_text}</dd>
    ${card.reload_text ? html`<dt>Reload</dt><dd>${card.reload_text}</dd>` : null}
  </dl></section>`;
}

export function MacChart() {
  const [hours, setHours] = useState(24);
  const [series, error] = usePoll(() => api(`/client/utilization/series?hours=${hours}`), 30000, [hours]);
  const [hover, setHover] = useState(null);
  const buckets = series?.hours === hours ? series.buckets || [] : [];
  const maxMemory = series?.memory_total_bytes || Math.max(1, ...buckets.map((b) => b.mem_used_max || 0));
  const pendingMax = Math.max(1, ...buckets.map((b) => b.pending_max || 0));
  const metric = (key, max, color) => html`<path d=${chartPath(buckets, key, max)} fill="none" stroke=${`var(--${color})`} stroke-width="2" vector-effect="non-scaling-stroke" />`;
  const selected = buckets[hover];
  return html`<section class="q-card"><div class="q-heading"><h2>Mac usage</h2><${Seg} label="Usage range" value=${hours} onChange=${(v) => { setHours(v); setHover(null); }} options=${ranges} /></div>
    <div class="q-legend"><span class="amber">CPU</span><span class="cyan">GPU</span><span class="green">Memory in use</span><span class="red">Memory pressure</span><span>Jobs waiting</span><span>Darker fill: queue share</span></div>
    ${errorText(error)}
    ${!buckets.length ? html`<p class="muted">${series ? 'No usage recorded in this range.' : 'Loading usage…'}</p>` : html`<div class="q-chart">
      <span class="muted">100%</span>
      <svg viewBox="0 0 1000 150" preserveAspectRatio="none" role="img" aria-label="CPU, GPU, memory, queue share and waiting jobs over time" onMouseLeave=${() => setHover(null)}>
        ${buckets.map((b, i) => {
          const x = i * 1000 / buckets.length, w = 1000 / buckets.length;
          const area = (key, max, color, opacity) => typeof b[key] === 'number' ? html`<rect x=${x} y=${120 - b[key] / max * 120} width=${w} height=${b[key] / max * 120} fill=${`var(--${color})`} opacity=${opacity} />` : null;
          return html`<g>${b.pressure_max >= 2 ? html`<rect x=${x} y="0" width=${w} height="120" fill="var(--red)" opacity=".14" />` : null}
            ${area('mem_used_avg', maxMemory, 'green', .13)}${area('queue_memory_avg', maxMemory, 'green', .35)}
            ${area('queue_cpu_avg', 100, 'amber', .2)}${area('queue_gpu_avg', 100, 'cyan', .2)}
            <rect x=${x} y=${150 - (b.pending_max || 0) / pendingMax * 24} width=${w} height=${(b.pending_max || 0) / pendingMax * 24} fill="var(--slate)" opacity=".5" />
          </g>`;
        })}
        ${metric('cpu_avg', 100, 'amber')}${metric('gpu_avg', 100, 'cyan')}
        ${buckets.map((b, i) => html`<rect x=${i * 1000 / buckets.length} y="0" width=${1000 / buckets.length} height="150" fill="transparent" onMouseEnter=${() => setHover(i)}><title>${clock(b.start)} · CPU ${pct(b.cpu_avg)} · GPU ${pct(b.gpu_avg)} · memory ${gb(b.mem_used_avg)} · queue ${gb(b.queue_memory_avg)}, ${pct(b.queue_cpu_avg)} CPU, ${pct(b.queue_gpu_avg)} GPU · ${b.pending_max ?? '—'} waiting</title></rect>`)}
      </svg><div class="q-heading muted"><span>${clock(series.start)}</span><span>${clock(series.end)}</span></div>
      <p class="q-chart-caption">${selected ? `${clock(selected.start)} · CPU ${pct(selected.cpu_avg)} · GPU ${pct(selected.gpu_avg)} · memory ${gb(selected.mem_used_avg)} · queue ${gb(selected.queue_memory_avg)} / CPU ${pct(selected.queue_cpu_avg)} / GPU ${pct(selected.queue_gpu_avg)} · ${selected.pending_max ?? '—'} waiting` : `${coverageNote(series) ? `${coverageNote(series)} · ` : ''}Average CPU ${pct(series.summary?.cpu_avg)}, GPU ${pct(series.summary?.gpu_avg)} · elevated memory pressure ${duration(series.summary?.pressure_elevated_seconds || 0)}`}</p>
    </div>`}
  </section>`;
}

export function HeldBack({ stats, insightOnly = false, onDismiss }) {
  const limits = limitsInsight(stats);
  if (insightOnly && !limits) return null;
  return html`<section class="q-card q-insight"><div class="q-heading"><h2>${insightOnly ? 'The queue limits are holding jobs back.' : 'Held back?'}</h2>${onDismiss ? html`<button class="btn sm" aria-label="Dismiss queue limits insight" onClick=${onDismiss}>Dismiss</button>` : null}</div>
    ${stats?.available ? (stats.waiting || []).filter((r) => r.job_seconds > 0 && (!insightOnly || r.group === 'limits')).map((row) => html`<p>${{ limits: 'Queue limits', perf_rules: 'Perf rules', memory: 'Memory', other: 'Other rules' }[row.group]} held jobs for ${(row.job_seconds / 3600).toFixed(1)} hours in total. The Mac had room to run ${Math.round(100 * row.headroom_job_seconds / row.job_seconds)}% of that.${row.unknown_job_seconds ? ` Headroom was unknown for ${duration(row.unknown_job_seconds)}.` : ''}</p>`) : html`<p class="muted">No utilization data recorded.</p>`}
    ${stats?.by_type?.filter((r) => r.peak_rss_p95_bytes != null).map((r) => html`<p class="sub">${r.type} jobs peak at ${gb(r.peak_rss_p95_bytes)} each (95th percentile).</p>`)}
    ${limits ? html`<a href="/settings#queue-limits">Queue limits…</a>` : null}
  </section>`;
}

/**
 * Start now (sm#2383): one tap starts a waiting job. Only a real risk to the
 * Mac's memory asks first, in a popover anchored to this button; a second
 * tap on the button, or Start anyway, accepts it. Skipping the line is the
 * point of the button and never asks.
 */
export function StartNow({ job, primary = false }) {
  const [phase, setPhase] = useState('idle'); // idle | starting | risk | failed
  const [message, setMessage] = useState('');
  const [above, setAbove] = useState(false);
  const root = useRef(null);
  const pop = useRef(null);
  const open = phase === 'risk' || phase === 'failed';
  // Open upward when the scroll area has no room below, so the choice never
  // needs a scroll to reach.
  useLayoutEffect(() => {
    if (!open || !pop.current) { setAbove(false); return; }
    const box = root.current.closest('.content')?.getBoundingClientRect() ?? { top: 0, bottom: innerHeight };
    const button = root.current.getBoundingClientRect();
    const height = pop.current.offsetHeight + 6;
    setAbove(button.bottom + height > Math.min(box.bottom, innerHeight) && button.top - height >= Math.max(box.top, 0));
  }, [open, message]);
  useEffect(() => {
    if (!open) return;
    const away = (event) => { if (!root.current?.contains(event.target)) setPhase('idle'); };
    const escape = (event) => { if (event.key === 'Escape') setPhase('idle'); };
    document.addEventListener('pointerdown', away);
    document.addEventListener('keydown', escape);
    return () => { document.removeEventListener('pointerdown', away); document.removeEventListener('keydown', escape); };
  }, [open]);
  const start = async (acceptRisk) => {
    if (phase === 'starting') return;
    setPhase('starting'); setMessage('');
    try {
      const result = await requestStart(api, job.id, { acceptRisk });
      if (result.busy) { setPhase('idle'); return; } // the other button for this job owns the request
      if (result.risk) { setPhase('risk'); setMessage(result.risk); return; }
      setPhase('idle');
      toast(`Started ${title(job)}`);
      bus.emit('queue-changed');
    } catch (error) {
      setPhase('failed'); setMessage(error.message);
    }
  };
  const starting = phase === 'starting';
  return html`<span class="q-start" ref=${root} onClick=${(event) => event.stopPropagation()}>
    <button type="button" class=${`btn ${primary ? 'pri' : 'q-start-btn'}`} disabled=${starting} aria-busy=${starting}
      aria-label=${primary ? null : `Start ${title(job)} now`} aria-expanded=${open} onClick=${() => start(phase === 'risk')}>
      ${starting ? html`<i class="q-spin" aria-hidden="true"></i>` : html`<i class="q-play" aria-hidden="true"></i>`}
      ${primary ? (starting ? 'Starting…' : 'Start now') : 'Start'}
    </button>
    ${open ? html`<span ref=${pop} class=${`q-start-pop ${phase}${above ? ' above' : ''}`} role=${phase === 'failed' ? 'alert' : 'dialog'} aria-label="Start now">
      <strong>${phase === 'risk' ? 'Low on memory' : "Couldn't start"}</strong>
      <span>${message}</span>
      ${phase === 'risk' ? html`<small>If memory runs short, sm stops this job first and puts it back in line.</small>` : null}
      <span class="q-start-acts">
        <button type="button" class="btn sm" onClick=${() => setPhase('idle')}>${phase === 'risk' ? 'Not now' : 'OK'}</button>
        ${phase === 'risk' ? html`<button type="button" class="btn sm pri" onClick=${() => start(true)}>Start anyway</button>` : null}
      </span>
    </span>` : null}
  </span>`;
}

// The details band opens after the element carrying data-open-ref, so a
// waiting row's wrapper carries it and the band lands below the Start pill.
function JobRow({ job, now, color }) {
  if (job.state !== 'pending') return html`<${JobRowButton} job=${job} now=${now} anchor=${true} color=${color} />`;
  return html`<div class="q-job-line" data-open-ref=${`job:${job.id}`}><${JobRowButton} job=${job} now=${now} /><${StartNow} job=${job} /></div>`;
}

// A shell job (sm#2412): memory against its reservation where other jobs show time.
function ShellJobRowButton({ job, now, anchor, color }) {
  const reserved = job.shell?.reserved_bytes ?? job.memory_bytes;
  const used = job.shell?.current_bytes ?? job.usage?.memory_bytes;
  const over = !!job.shell?.over_reservation;
  const share = reserved && used != null ? Math.min(100, (used / reserved) * 100) : 0;
  return html`<button class="q-job" type="button" data-open-ref=${anchor ? `job:${job.id}` : null} onClick=${() => openPanel(`job:${job.id}`)}>
    <span class="q-job-title"><${JobName} job=${job} color=${color} /><small>shell · ${jobAgentLabel(job)}</small>${over ? html`<small class="amber">${job.holding?.summary}</small>` : null}</span>
    <span class=${`q-membar${over ? ' over' : ''}`} role="img" aria-label=${`${gb(used)} of ${gb(reserved)} reserved`}><i style=${`width:${share}%`}></i></span>
    <span>${age(job.queued_at || job.started_at, now)} · <span class=${over ? 'amber' : ''}>${used != null ? `${gb(used)} of ` : ''}${gb(reserved)} reserved</span></span>
  </button>`;
}

function JobRowButton({ job, now, anchor = false, color = null }) {
  if (job.type === 'shell') return html`<${ShellJobRowButton} job=${job} now=${now} anchor=${anchor} color=${color} />`;
  const deadline = Date.parse(job.wait_deadline_at) - now;
  const review = reviewJobText(job);
  const nearIdle = job.near_idle_since ? html`<span title=${`Slot released · near-idle since ${clock(job.near_idle_since)}`}> · near-idle</span>` : null;
  return html`<button class="q-job" type="button" data-open-ref=${anchor ? `job:${job.id}` : null} onClick=${() => openPanel(`job:${job.id}`)}>
    ${review ? html`<span class="q-job-title">${job.position ? `${job.position}. ` : ''}${review.title} <b>${review.reviewer}</b><small>${review.detail}${nearIdle}</small>${review.why ? html`<small class="amber">${review.why}</small>` : null}</span>`
      : html`<span class="q-job-title">${job.position ? `${job.position}. ` : ''}<${JobName} job=${job} color=${color} /><small>${job.type} · ${jobAgentLabel(job)}${nearIdle}</small></span>`}
    <span class="q-timeline">${timelineSegments(job, now).map((s) => html`<i class=${s.kind} style=${`left:${s.left}%;width:${s.width}%`}></i>`)}</span>
    <span class=${job.quiet_since ? 'red' : ''}>${job.state === 'running' ? `${age(job.started_at, now)}${job.timeout_seconds ? ` of ${duration(job.timeout_seconds)}` : " · no time limit"}` : `${age(job.queued_at, now)} waited`}
      ${job.quiet_since ? ` · quiet ${age(job.quiet_since, now)}` : job.low_cpu && !job.near_idle_since ? html`<span class="muted"> · low CPU</span>` : ''}
      ${job.state === 'pending' && deadline < 21600000 && Number.isFinite(deadline) ? ` · gives up in ${duration(deadline / 1000)}` : ''}
      ${job.state === 'running' ? html`<${UsageLine} job=${job} />` : null}</span>
  </button>`;
}

/** A job's name with its breakdown colour and a pin when pinned (sm#2475). */
function JobName({ job, color }) {
  return html`<span class="q-name">${color != null ? html`<i class=${`q-dot j${color}`} aria-hidden="true"></i>` : null}<span>${title(job)}</span>${job.pinned_at ? html`<${PinIcon} label="Pinned" />` : null}</span>`;
}

function PinIcon({ label }) {
  return html`<svg class="q-pin" viewBox="0 0 16 16" width="13" height="13" role=${label ? 'img' : null} aria-label=${label} aria-hidden=${label ? null : 'true'}><path fill="currentColor" d="M10.2 1.3a1 1 0 0 1 1.4 0l3.1 3.1a1 1 0 0 1-.4 1.6l-1.9.7-2.3 2.3.3 3a1 1 0 0 1-1.7.8L6.4 10.5l-3.7 3.7a.7.7 0 0 1-1-1l3.7-3.7-2.3-2.3a1 1 0 0 1 .8-1.7l3 .3 2.3-2.3.7-1.9a1 1 0 0 1 .3-.3z" /></svg>`;
}

/** A running job's live usage, against its memory cap when it has one. */
function UsageLine({ job }) {
  const use = jobUsage(job);
  if (use.memory == null && use.cpu == null && use.gpu == null) return null;
  const cap = job.type === 'shell' ? null : job.memory_bytes;
  const parts = [
    use.memory != null ? `${usageText('memory', use.memory)}${cap ? ` of ${usageText('memory', cap)} cap` : ''}` : null,
    use.cpu != null ? `CPU ${usageText('cpu', use.cpu)}` : null,
    use.gpu ? `GPU ${usageText('gpu', use.gpu)}` : null,
  ].filter(Boolean);
  const over = cap && use.memory > cap * .85;
  return html`<small class=${`q-use${over ? ' amber' : ''}`}>${parts.join(' · ')}</small>`;
}

const METRIC_NAMES = { memory: 'Memory', cpu: 'CPU', gpu: 'GPU' };

/**
 * Right now (sm#2475): which running job is using how much of the Mac's
 * memory, CPU and GPU, and how much the rest of the Mac uses. Hovering a job,
 * here or on a bar, picks out its slice on all three bars; tapping opens it.
 */
export function UsageNow({ running }) {
  const [host] = usePoll(() => api('/client/host-status'), 10000);
  const [focus, setFocus] = useState(null);
  const { jobs, rows } = usageBreakdown(running, host);
  // Nothing measured yet means nothing to attribute: no card.
  if (!jobs.some((job) => job.memory != null || job.cpu != null)) return null;
  const total = (key) => rows.find((r) => r.key === key);
  // The legend names jobs with a visible slice, largest first; the rest share a line.
  const shown = new Set(rows.flatMap((row) => row.segments.map((s) => s.id)));
  const named = jobs.filter((job) => shown.has(job.id)).sort((a, b) => (b.memory || 0) - (a.memory || 0));
  const small = jobs.filter((job) => !shown.has(job.id) && (job.memory > 0 || job.cpu > 0 || job.gpu > 0));
  const other = Object.fromEntries(rows.map((r) => [r.key, r.other]));
  const open = (id) => id && openPanel(`job:${id}`);
  const dim = (id) => focus != null && focus !== id ? ' dim' : '';
  const valueText = (row) => html`${row.used == null ? '' : row.key === 'memory' ? `${usageText('memory', row.used)} of ${usageText('memory', row.scale)}` : `${usageText(row.key, row.used)} busy`}<small>${usageText(row.key, row.queue)} in jobs</small>`;
  return html`<section class="q-card q-now" onMouseLeave=${() => setFocus(null)}>
    <div class="q-heading"><h2>Right now</h2><span class="muted">What each running job is using</span></div>
    <div class="q-now-bars">${rows.map((row) => html`<div class="q-now-row">
      <span class="q-now-name">${METRIC_NAMES[row.key]}</span>
      <span class="q-stack" role="img" aria-label=${`${METRIC_NAMES[row.key]}: ${row.segments.map((s) => `${s.label} ${usageText(row.key, s.value)}`).concat(row.other != null ? [`rest of the Mac ${usageText(row.key, row.other)}`] : []).join(', ')}`}>
        ${row.segments.map((s) => html`<i class=${`${s.color === 'small' ? 'small' : `j${s.color}`}${dim(s.id ?? 'small')}`} style=${`width:${s.share}%`} title=${`${s.label} · ${usageText(row.key, s.value)}`}
          onMouseEnter=${() => setFocus(s.id ?? 'small')} onClick=${() => open(s.id)}></i>`)}
        ${row.otherShare ? html`<i class=${`other${dim('other')}`} style=${`width:${row.otherShare}%`} title=${`Rest of the Mac · ${usageText(row.key, row.other)}`} onMouseEnter=${() => setFocus('other')}></i>` : null}
      </span>
      <span class="q-now-val">${valueText(row)}</span>
    </div>`)}</div>
    <ul class="q-now-legend">
      ${named.map((job) => html`<li><button type="button" class=${`q-now-key${dim(job.id)}`} onMouseEnter=${() => setFocus(job.id)} onFocus=${() => setFocus(job.id)} onBlur=${() => setFocus(null)} onClick=${() => open(job.id)}>
        <i class=${`q-dot j${job.color}`} aria-hidden="true"></i><span class="q-now-label">${job.label}</span>
        <span class="q-now-nums">${[job.memory > 0 && usageText('memory', job.memory), job.cpu > 0 && `CPU ${usageText('cpu', job.cpu)}`, job.gpu > 0 && `GPU ${usageText('gpu', job.gpu)}`].filter(Boolean).join(' · ')}</span>
      </button></li>`)}
      ${small.length ? html`<li><span class=${`q-now-key${dim('small')}`} title=${small.map((job) => job.label).join(', ')} onMouseEnter=${() => setFocus('small')}>
        <i class="q-dot small" aria-hidden="true"></i><span class="q-now-label">${small.length} smaller ${small.length === 1 ? 'job' : 'jobs'}<small>${small.map((job) => job.label).join(', ')}</small></span>
        <span class="q-now-nums">${usageText('memory', small.reduce((sum, job) => sum + (job.memory || 0), 0))}</span>
      </span></li>` : null}
      ${Object.values(other).some((v) => v > 0) ? html`<li><span class=${`q-now-key rest${dim('other')}`} onMouseEnter=${() => setFocus('other')}>
        <i class="q-dot other" aria-hidden="true"></i><span class="q-now-label">Rest of the Mac<small>apps, agents and the system</small></span>
        <span class="q-now-nums">${[other.memory > 0 && usageText('memory', other.memory), other.cpu > 0 && `CPU ${usageText('cpu', other.cpu)}`, other.gpu > 0 && `GPU ${usageText('gpu', other.gpu)}`].filter(Boolean).join(' · ')}</span>
      </span></li>` : null}
    </ul>
    ${total('memory') ? null : html`<p class="sub">The Mac's own reading is unavailable; only the jobs' share is shown.</p>`}
  </section>`;
}

/** Live usage in the job panel: memory, CPU and GPU as shares of the Mac. */
function usingNow(job) {
  const use = jobUsage(job);
  if (use.memory == null && use.cpu == null) return 'Not measured yet';
  return [`${usageText('memory', use.memory)} memory`, `CPU ${usageText('cpu', use.cpu)}`, `GPU ${usageText('gpu', use.gpu)} of the Mac`].join(' · ');
}

/** What the job asked for: time always, a memory cap when set, and CPU and
 * GPU only for perf jobs, the one type that reserves them. */
function limitsText(job) {
  return [
    job.timeout_seconds ? `${duration(job.timeout_seconds)} to run` : 'No time limit',
    job.memory_bytes ? `memory cap ${usageText('memory', job.memory_bytes)}` : 'no memory cap',
    job.type === 'perf' ? `CPU ${pct(job.cpu_percent)} · GPU ${pct(job.gpu_percent)} reserved` : null,
  ].filter(Boolean).join(' · ');
}

const jobQuestions = new Map();
const QUESTION_DONE = ['completed', 'failed', 'timed_out'];

function JobPanel({ id, controls }) {
  const encoded = encodeURIComponent(id);
  const [job, error, reload] = usePoll(() => api(`/queue-jobs/${encoded}`), 5000, [id]);
  useEffect(() => bus.on('queue-changed', reload), [reload]);
  const [log, logError] = usePoll(async () => {
    try { return await api(`/queue-jobs/${encoded}/log?lines=40`); }
    catch (error) {
      if (error.status === 404) return { text: '' };
      throw error;
    }
  }, 5000, [id]);
  const [before, setBefore] = useState(null);
  const usageInterval = !before && job?.state === 'running' ? 5000 : 0;
  const [usage, usageError] = usePoll(() => api(`/client/queue/jobs/${encoded}/usage${before ? `?before_ms=${before}` : ''}`), usageInterval, [id, before, usageInterval]);
  const [follows, , reloadFollows] = usePoll(() => api('/client/follows'), 30000, [id]);
  const [cancel, setCancel] = useState(false), [note, setNote] = useState('');
  const [ask, setAsk] = useState(jobQuestions.has(id)), [question, setQuestion] = useState(''), [busy, setBusy] = useState(false), [failure, setFailure] = useState('');
  const [request, setRequest] = useState(jobQuestions.get(id) || null);
  const queue = useShared('queue');
  const agentId = job && jobAgentId(job);
  const [agentState] = usePoll(() => agentId ? api(`/watch/state?session=${encodeURIComponent(agentId)}`) : null, 30000, [agentId]);
  const knownAgent = agentState?.sessions?.find((agent) => agent.id === agentId);
  const canAsk = knownAgent && knownAgent.state !== 'stopped';
  const pending = request && !QUESTION_DONE.includes(request.status);
  useEffect(() => {
    if (!pending) return;
    let alive = true;
    const timer = setTimeout(async () => {
      try {
        const updated = await api(`/btw-requests/${encodeURIComponent(request.request_id)}`);
        jobQuestions.set(id, updated);
        if (alive) { setRequest(updated); setFailure(''); }
      } catch (error) {
        if (alive) { setFailure(error.message); setRequest((current) => ({ ...current })); }
      }
    }, 2000);
    return () => { alive = false; clearTimeout(timer); };
  }, [request, pending]);
  const perform = async (fn) => {
    if (busy) return;
    setBusy(true); setFailure('');
    try { await fn(); reload(); reloadFollows(); } catch (e) { setFailure(e.message); } finally { setBusy(false); }
  };
  if (!job) return html`<div class="phd"><span class="t">Job</span>${controls}<span class="s">${error?.message || 'Loading…'}</span></div>`;
  const active = ['running', 'pending'].includes(job.state);
  const shell = job.type === 'shell';
  const following = follows?.follows?.some((f) => f.job_id === job.id && f.state !== 'done');
  const send = () => perform(async () => {
    const created = await askJobQuestion(api, queue?.owner_name || 'The owner', job, question);
    jobQuestions.set(id, created);
    setRequest(created);
  });
  const sampleMax = Math.max(100, ...(usage?.samples || []).map((s) => s.cpu_percent || 0), ...(usage?.samples || []).map((s) => s.gpu_percent || 0));
  const review = reviewJobText(job);
  return html`<div class="phd"><span class=${job.quiet_since ? 'red' : 'sub'}>${job.state}${job.quiet_since ? ' · quiet' : ''}</span><span class="t">${review ? `${review.title} · round ${job.review.round}` : title(job)}</span>${controls}<span class="s">${review ? job.review.reviewer_label : `${job.type} · ${jobAgentLabel(job)}`}</span></div>
    <div class="q-panel-body">${errorText(error)}${failure ? html`<p class="err" role="alert">${failure}</p>` : null}
      <div class="q-actions">
        ${job.state === 'pending' ? html`<${StartNow} job=${job} primary=${true} />` : null}
        ${active ? html`<button class="btn" disabled=${busy} onClick=${() => setCancel(!cancel)}>${shell ? 'Stop' : 'Cancel'}</button>${shell ? null : html`<button class="btn" disabled=${busy || !follows} aria-pressed=${!!following} onClick=${() => perform(() => api(`/queue-jobs/${encoded}/follow`, { method: following ? 'DELETE' : 'POST', body: {} }))}>${following ? 'Following' : 'Follow'}</button>`}` : null}
        ${active && !shell ? html`<button class=${`btn q-pin-btn${job.pinned_at ? ' on' : ''}`} disabled=${busy} aria-pressed=${!!job.pinned_at} onClick=${() => perform(async () => { await api(`/client/queue/jobs/${encoded}/pin`, { method: job.pinned_at ? 'DELETE' : 'POST', body: {} }); toast(job.pinned_at ? `Unpinned ${title(job)}` : `Pinned ${title(job)}`); bus.emit('queue-changed'); })}><${PinIcon} />${job.pinned_at ? 'Pinned' : 'Pin'}</button>` : null}
        ${canAsk || request ? html`<button class="btn" onClick=${() => setAsk(!ask)}>Ask agent</button>` : null}${knownAgent ? html`<button class="btn" onClick=${() => openPanel(`agent:${agentId}`)}>Open agent</button>` : null}
      </div>
      ${active && !shell ? html`<p class=${`q-pin-note${job.pinned_at ? ' on' : ''}`}>${job.pinned_at
        ? `Pinned ${clock(job.pinned_at)}. If the Mac runs short of memory, sm stops every other job first, and stops this one only at critical pressure with nothing else left.`
        : job.type === 'service' ? 'If the Mac runs short of memory, sm stops service jobs after every other unpinned job. Pin it to keep it through anything short of critical pressure.'
        : job.owner_forced_at ? 'You started this early, so if the Mac runs short of memory, sm stops it first and puts it back in line. Pin it to keep it running.'
        : 'If the Mac runs short of memory, sm stops the largest unpinned job first. Pin it to keep it running.'}</p>` : null}
      ${cancel && active ? html`<section class="q-card"><label>${shell ? `Note to ${jobAgentLabel(job)} (optional)` : 'Cancellation note (optional)'}<textarea class="inp" maxLength="1000" value=${note} onInput=${(e) => setNote(e.target.value)} /></label><button class="btn" disabled=${busy} onClick=${() => perform(async () => { await api(`/queue-jobs/${encoded}/cancel`, { method: 'POST', body: { note: note.trim() || null } }); closePanel(); bus.emit('queue-changed'); })}>${shell ? 'Stop it' : 'Cancel job'}</button>${shell ? html`<p class="sub">sm stops the command and tells ${jobAgentLabel(job)} how to rerun it through the queue.</p>` : null}</section>` : null}
      ${ask ? html`<section class="q-card"><div class="q-suggestions">${['How long do you expect this to run?', 'What is this job for?', 'Is it safe to cancel this?', 'Is this job stuck?'].map((q) => html`<button class="btn sm" onClick=${() => setQuestion(q)}>${q}</button>`)}</div><textarea aria-label="Question for agent" class="inp" value=${question} onInput=${(e) => setQuestion(e.target.value)} /><button class="btn pri" disabled=${busy || pending || !canAsk || !question.trim()} onClick=${send}>${pending ? 'Asking…' : 'Send'}</button>
        ${pending ? html`<p class="sub" role="status">${jobAgentLabel(job)} is answering… (${request.status})</p>` : null}
        ${request?.status === 'completed' ? html`<div class="summary">${request.result}</div>` : null}
        ${request && ['failed', 'timed_out'].includes(request.status) ? html`<p class="err">${request.error || 'The question did not finish. Try again.'}</p>` : null}
      </section>` : null}
      ${job.review ? html`<dl class="q-details q-review"><dt>Reviews</dt><dd><a href=${`https://github.com/${job.review.repo}/pull/${job.review.pr_number}`} target="_blank" rel="noopener">${job.review.repo} PR #${job.review.pr_number}</a> · round ${job.review.round}</dd>
        <dt>Reviewer</dt><dd>${job.review.reviewer_label || '—'}</dd><dt>For</dt><dd>${job.review.author_name}</dd>
        <dt>Why</dt><dd>${job.review.policy_source || 'default'} policy${job.review.why !== 'default' ? html` · <span class="amber">${job.review.why}</span>` : ''}</dd>
        <dt>Checkout</dt><dd>${job.cwd}</dd></dl>` : null}
      ${shell ? html`<dl class="q-details"><dt>How it got here</dt><dd>${job.holding?.detail || '—'}</dd><dt>Memory</dt><dd>${job.shell?.current_bytes != null ? `${gb(job.shell.current_bytes)} now · ` : ''}${job.shell?.peak_bytes != null ? `peak ${gb(job.shell.peak_bytes)} · ` : ''}${gb(job.shell?.reserved_bytes ?? job.memory_bytes)} reserved${job.shell?.over_reservation ? html`<br /><span class="amber">Over its reservation: if the Mac runs short of memory, sm stops this first.</span>` : ''}</dd><dt>Command</dt><dd><code>${job.argv?.join(' ') || '—'}</code></dd><dt>Folder</dt><dd>${job.cwd || '—'}</dd><dt>Started</dt><dd>${clock(job.queued_at)}</dd><dt>In the queue</dt><dd>${clock(job.started_at) || '—'}</dd><dt>Ended</dt><dd>${clock(job.finished_at) || '—'}</dd><dt>What happened</dt><dd>${job.ended_summary || job.termination_reason || job.state}${job.cancel_detail?.note ? ` · ${job.cancel_detail.note}` : ''}</dd></dl>` : html`<dl class="q-details"><dt>Command</dt><dd><code>${job.argv?.join(' ') || job.script_path || '—'}</code></dd><dt>Folder</dt><dd>${job.cwd}</dd><dt>Queued</dt><dd>${clock(job.queued_at)}</dd><dt>Started</dt><dd>${clock(job.started_at) || '—'}</dd><dt>Finished</dt><dd>${clock(job.finished_at) || '—'}</dd>${job.state === 'running' ? html`<dt>Using now</dt><dd>${usingNow(queue?.running?.find((live) => live.id === job.id) || job)}</dd><dt>Slot</dt><dd>${job.near_idle_since ? `Released · near-idle since ${clock(job.near_idle_since)}` : 'Held'}</dd>` : null}<dt>Limits</dt><dd>${limitsText(job)}</dd><dt>Why it waits</dt><dd>${job.holding?.detail || job.holding?.summary || '—'}</dd><dt>What happened</dt><dd>${job.ended_summary || job.termination_reason || job.state}${job.exit_code != null ? ` · exit ${job.exit_code}` : ''}${job.cancel_detail?.note ? ` · ${job.cancel_detail.note}` : ''}</dd></dl>`}
      <h3>CPU and GPU over this run</h3>${errorText(usageError)}<p class="sub">CPU: amber · GPU: cyan · percent of one core / GPU second per second · scale ${sampleMax.toFixed(0)}%</p>
      ${usage?.samples?.length ? html`<svg class="q-job-chart" viewBox="0 0 1000 120" preserveAspectRatio="none" role="img" aria-label="Job CPU and GPU over its run">${['cpu_percent', 'gpu_percent'].map((key, i) => html`<path d=${chartPath(usage.samples, key, sampleMax)} fill="none" stroke=${i ? 'var(--cyan)' : 'var(--amber)'} stroke-width="2" vector-effect="non-scaling-stroke" />`)}</svg><div class="q-heading sub"><span>${clock(usage.samples[0].at)}</span><span>${clock(usage.samples.at(-1).at)}</span></div>` : html`<p class="muted">No usage samples recorded.</p>`}
      <div class="q-actions">${usage?.next_before_ms ? html`<button class="btn sm" onClick=${() => setBefore(usage.next_before_ms)}>Earlier samples</button>` : null}${before ? html`<button class="btn sm" onClick=${() => setBefore(null)}>Latest samples</button>` : null}</div>
      ${shell ? null : html`<h3>Last 40 log lines</h3><pre class="q-log">${logError?.message || (log ? log.text || 'No log yet' : 'Loading log…')}</pre>`}
    </div>`;
}
registerPanel('job', JobPanel);
