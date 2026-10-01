import { useEffect, useState } from 'preact/hooks';
import { html, api, bus, closePanel, usePoll, useNow, useShared, setShared, registerPanel, openPanel, navigate, Seg, duration, age, clock, gigabytes, meterBand } from './ui.js';
import { timelineSegments, limitsInsight, waitingGroups, chartPath, jobAgentId, jobAgentLabel, askJobQuestion, reviewJobText } from './queue-model.js';

const ranges = [{ value: 1, label: '1h' }, { value: 24, label: '24h' }, { value: 168, label: '7d' }, { value: 720, label: '30d' }];
const pct = (value) => typeof value === 'number' ? `${value.toFixed(1)}%` : '—';
const gb = (value) => typeof value === 'number' ? `${gigabytes(value)}G` : '—';
const title = (job) => job.label || job.id;
const errorText = (error) => error ? html`<p class="err" role="alert">${error.message}</p>` : null;

export function QueuePage() {
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
    })}<${MacNow} host=${queue.host} /></div>
    <${MacChart} />
    <${HeldBack} stats=${stats} insightOnly=${true} />
    <section class="q-card"><div class="q-heading"><h2>Running</h2><span class="muted">Last 3 hours → now</span></div>
      ${queue.running.length ? queue.running.map((job) => html`<${JobRow} key=${job.id} job=${job} now=${now} />`) : html`<p class="muted">No jobs running.</p>`}
      <h2>Waiting · in start order</h2>
      ${waitingGroups(queue.queued).map((group) => html`<div><p class="sub">${group.caption}</p>${group.jobs.map((job) => html`<${JobRow} key=${job.id} job=${job} now=${now} />`)}</div>`)}
      ${!queue.queued.length ? html`<p class="muted">No jobs waiting.</p>` : null}
    </section>
    ${queue.ended.length ? html`<a href="/analytics/queue" onClick=${(e) => { e.preventDefault(); navigate('/analytics/queue'); }}>${queue.ended.length} stopped in the last 24h ›</a>` : null}
  </div>`;
}

export function MacNow({ host }) {
  if (!host || host.available === false) return html`<div class="q-card q-mac">Mac now · unavailable</div>`;
  const metrics = [
    ['MEM', host.memory_used_bytes, host.queue_memory_bytes, host.memory_total_bytes, `${gigabytes(host.memory_used_bytes)}/${gigabytes(host.memory_total_bytes)}G${typeof host.queue_memory_bytes === 'number' ? ` · queue ${gb(host.queue_memory_bytes)}` : ''}`],
    ['CPU', host.cpu_percent, host.queue_cpu_percent, 100, `${Math.round(host.cpu_percent || 0)}%`],
    ['GPU', host.gpu_percent, host.queue_gpu_percent, 100, `${Math.round(host.gpu_percent || 0)}%`],
  ];
  return html`<div class="q-card q-mac"><span class="q-label">Mac now</span>${metrics.map(([label, total, queue, max, text]) => {
    const totalWidth = Math.max(0, Math.min(100, (total || 0) / (max || 1) * 100));
    const shareWidth = Math.max(0, Math.min(totalWidth, (queue || 0) / (max || 1) * 100));
    const color = meterBand(totalWidth / 100, label.toLowerCase());
    return html`<div class="q-meter" style=${`--meter-color:var(--${color})`}><span>${label}</span><i><s style=${`width:${totalWidth}%;opacity:${typeof queue === 'number' ? '.4' : '1'}`}></s>${typeof queue === 'number' ? html`<s class="q" style=${`width:${shareWidth}%`}></s>` : null}</i><span>${text}</span></div>`;
  })}</div>`;
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
      <p class="q-chart-caption">${selected ? `${clock(selected.start)} · CPU ${pct(selected.cpu_avg)} · GPU ${pct(selected.gpu_avg)} · memory ${gb(selected.mem_used_avg)} · queue ${gb(selected.queue_memory_avg)} / CPU ${pct(selected.queue_cpu_avg)} / GPU ${pct(selected.queue_gpu_avg)} · ${selected.pending_max ?? '—'} waiting` : `Average CPU ${pct(series.summary?.cpu_avg)}, GPU ${pct(series.summary?.gpu_avg)} · elevated memory pressure ${duration(series.summary?.pressure_elevated_seconds || 0)}`}</p>
    </div>`}
  </section>`;
}

export function HeldBack({ stats, insightOnly = false }) {
  const limits = limitsInsight(stats);
  if (insightOnly && !limits) return null;
  return html`<section class="q-card q-insight"><h2>${insightOnly ? 'The queue limits are holding jobs back.' : 'Held back?'}</h2>
    ${stats?.available ? (stats.waiting || []).filter((r) => r.job_seconds > 0 && (!insightOnly || r.group === 'limits')).map((row) => html`<p>${{ limits: 'Queue limits', perf_rules: 'Perf rules', memory: 'Memory', other: 'Other rules' }[row.group]} held jobs for ${(row.job_seconds / 3600).toFixed(1)} hours in total. The Mac had room to run ${Math.round(100 * row.headroom_job_seconds / row.job_seconds)}% of that.${row.unknown_job_seconds ? ` Headroom was unknown for ${duration(row.unknown_job_seconds)}.` : ''}</p>`) : html`<p class="muted">No utilization data recorded.</p>`}
    ${stats?.by_type?.filter((r) => r.peak_rss_p95_bytes != null).map((r) => html`<p class="sub">${r.type} jobs peak at ${gb(r.peak_rss_p95_bytes)} each (95th percentile).</p>`)}
    ${limits ? html`<a href="/settings#queue-limits">Queue limits…</a>` : null}
  </section>`;
}

function JobRow({ job, now }) {
  const deadline = Date.parse(job.wait_deadline_at) - now;
  const review = reviewJobText(job);
  return html`<button class="q-job" type="button" data-open-ref=${`job:${job.id}`} onClick=${() => openPanel(`job:${job.id}`)}>
    ${review ? html`<span class="q-job-title">${job.position ? `${job.position}. ` : ''}${review.title} <b>${review.reviewer}</b><small>${review.detail}</small>${review.why ? html`<small class="amber">${review.why}</small>` : null}</span>`
      : html`<span class="q-job-title">${job.position ? `${job.position}. ` : ''}${title(job)}<small>${job.type} · ${jobAgentLabel(job)}</small></span>`}
    <span class="q-timeline">${timelineSegments(job, now).map((s) => html`<i class=${s.kind} style=${`left:${s.left}%;width:${s.width}%`}></i>`)}</span>
    <span class=${job.quiet_since ? 'red' : ''}>${job.state === 'running' ? `${age(job.started_at, now)}${job.timeout_seconds ? ` of ${duration(job.timeout_seconds)}` : " · no time limit"}` : `${age(job.queued_at, now)} waited`}
      ${job.quiet_since ? ` · quiet ${age(job.quiet_since, now)}` : job.low_cpu ? html`<span class="muted"> · low CPU</span>` : ''}
      ${job.state === 'pending' && deadline < 21600000 && Number.isFinite(deadline) ? ` · gives up in ${duration(deadline / 1000)}` : ''}</span>
  </button>`;
}

const jobQuestions = new Map();
const QUESTION_DONE = ['completed', 'failed', 'timed_out'];

function JobPanel({ id, controls }) {
  const encoded = encodeURIComponent(id);
  const [job, error, reload] = usePoll(() => api(`/queue-jobs/${encoded}`), 5000, [id]);
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
  const [check, setCheck] = useState(null), [cancel, setCancel] = useState(false), [note, setNote] = useState('');
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
        ${job.state === 'pending' ? html`<button class="btn" disabled=${busy} onClick=${() => perform(async () => setCheck(await api(`/client/queue/jobs/${encoded}/start-check`)))}>Start now</button>` : null}
        ${active ? html`<button class="btn" disabled=${busy} onClick=${() => setCancel(!cancel)}>Cancel</button><button class="btn" disabled=${busy || !follows} aria-pressed=${!!following} onClick=${() => perform(() => api(`/queue-jobs/${encoded}/follow`, { method: following ? 'DELETE' : 'POST', body: {} }))}>${following ? 'Following' : 'Follow'}</button>` : null}
        ${canAsk || request ? html`<button class="btn" onClick=${() => setAsk(!ask)}>Ask agent</button>` : null}${knownAgent ? html`<button class="btn" onClick=${() => openPanel(`agent:${agentId}`)}>Open agent</button>` : null}
      </div>
      ${check && job.state === 'pending' ? html`<section class="q-card"><h3>Start this job now?</h3>${check.warnings.map((warning) => html`<p class="amber">${warning}</p>`)}<p>Memory available ${gb(check.memory_available_bytes)} · reserve ${gb(check.memory_reserve_bytes)} · estimate ${gb(check.memory_estimate_bytes)} (${check.memory_estimate_source || 'unknown'})</p><button class="btn" disabled=${busy} onClick=${() => perform(async () => { await api(`/client/queue/jobs/${encoded}/start`, { method: 'POST', body: {} }); setCheck(null); })}>Start anyway</button> <button class="btn" onClick=${() => setCheck(null)}>Keep waiting</button></section>` : null}
      ${cancel && active ? html`<section class="q-card"><label>Cancellation note (optional)<textarea class="inp" maxLength="1000" value=${note} onInput=${(e) => setNote(e.target.value)} /></label><button class="btn" disabled=${busy} onClick=${() => perform(async () => { await api(`/queue-jobs/${encoded}/cancel`, { method: 'POST', body: { note: note.trim() || null } }); closePanel(); bus.emit('queue-changed'); })}>Cancel job</button></section>` : null}
      ${ask ? html`<section class="q-card"><div class="q-suggestions">${['How long do you expect this to run?', 'What is this job for?', 'Is it safe to cancel this?', 'Is this job stuck?'].map((q) => html`<button class="btn sm" onClick=${() => setQuestion(q)}>${q}</button>`)}</div><textarea aria-label="Question for agent" class="inp" value=${question} onInput=${(e) => setQuestion(e.target.value)} /><button class="btn pri" disabled=${busy || pending || !canAsk || !question.trim()} onClick=${send}>${pending ? 'Asking…' : 'Send'}</button>
        ${pending ? html`<p class="sub" role="status">${jobAgentLabel(job)} is answering… (${request.status})</p>` : null}
        ${request?.status === 'completed' ? html`<div class="summary">${request.result}</div>` : null}
        ${request && ['failed', 'timed_out'].includes(request.status) ? html`<p class="err">${request.error || 'The question did not finish. Try again.'}</p>` : null}
      </section>` : null}
      ${job.review ? html`<dl class="q-details q-review"><dt>Reviews</dt><dd><a href=${`https://github.com/${job.review.repo}/pull/${job.review.pr_number}`} target="_blank" rel="noopener">${job.review.repo} PR #${job.review.pr_number}</a> · round ${job.review.round}</dd>
        <dt>Reviewer</dt><dd>${job.review.reviewer_label || '—'}</dd><dt>For</dt><dd>${job.review.author_name}</dd>
        <dt>Why</dt><dd>${job.review.policy_source || 'default'} policy${job.review.why !== 'default' ? html` · <span class="amber">${job.review.why}</span>` : ''}</dd>
        <dt>Checkout</dt><dd>${job.cwd}</dd></dl>` : null}
      <dl class="q-details"><dt>Command</dt><dd><code>${job.argv?.join(' ') || job.script_path || '—'}</code></dd><dt>Folder</dt><dd>${job.cwd}</dd><dt>Queued</dt><dd>${clock(job.queued_at)}</dd><dt>Started</dt><dd>${clock(job.started_at) || '—'}</dd><dt>Finished</dt><dd>${clock(job.finished_at) || '—'}</dd><dt>Limits</dt><dd>${job.timeout_seconds ? duration(job.timeout_seconds) : "No time limit"} · CPU ${pct(job.cpu_percent)} · GPU ${pct(job.gpu_percent)} · memory ${gb(job.memory_bytes)}</dd><dt>Why it waits</dt><dd>${job.holding?.detail || job.holding?.summary || '—'}</dd><dt>What happened</dt><dd>${job.ended_summary || job.termination_reason || job.state}${job.exit_code != null ? ` · exit ${job.exit_code}` : ''}${job.cancel_detail?.note ? ` · ${job.cancel_detail.note}` : ''}</dd></dl>
      <h3>CPU and GPU over this run</h3>${errorText(usageError)}<p class="sub">CPU: amber · GPU: cyan · percent of one core / GPU second per second · scale ${sampleMax.toFixed(0)}%</p>
      ${usage?.samples?.length ? html`<svg class="q-job-chart" viewBox="0 0 1000 120" preserveAspectRatio="none" role="img" aria-label="Job CPU and GPU over its run">${['cpu_percent', 'gpu_percent'].map((key, i) => html`<path d=${chartPath(usage.samples, key, sampleMax)} fill="none" stroke=${i ? 'var(--cyan)' : 'var(--amber)'} stroke-width="2" vector-effect="non-scaling-stroke" />`)}</svg><div class="q-heading sub"><span>${clock(usage.samples[0].at)}</span><span>${clock(usage.samples.at(-1).at)}</span></div>` : html`<p class="muted">No usage samples recorded.</p>`}
      <div class="q-actions">${usage?.next_before_ms ? html`<button class="btn sm" onClick=${() => setBefore(usage.next_before_ms)}>Earlier samples</button>` : null}${before ? html`<button class="btn sm" onClick=${() => setBefore(null)}>Latest samples</button>` : null}</div>
      <h3>Last 40 log lines</h3><pre class="q-log">${logError?.message || (log ? log.text || 'No log yet' : 'Loading log…')}</pre>
    </div>`;
}
registerPanel('job', JobPanel);
