import { useState } from 'preact/hooks';
import { html, api, config, usePoll, useNow, setShared, registerPanel, openPanel, navigate, toast, Seg, duration, age, clock, gigabytes } from './ui.js';
import { timelineSegments, limitsInsight, waitingGroups, chartPath } from './queue-model.js';

const ranges = [{ value: 1, label: '1h' }, { value: 24, label: '24h' }, { value: 168, label: '7d' }, { value: 720, label: '30d' }];
const pct = (value) => typeof value === 'number' ? `${value.toFixed(1)}%` : '—';
const gb = (value) => typeof value === 'number' ? `${gigabytes(value)}G` : '—';
const title = (job) => job.label || job.id;
const errorText = (error) => error ? html`<p class="err" role="alert">${error.message}</p>` : null;

export function QueuePage() {
  const [queue, error] = usePoll(async () => { const q = await api('/client/queue'); setShared('queue', q); return q; }, 5000);
  const [stats] = usePoll(() => api('/client/queue/stats?hours=24'), 60000);
  const now = useNow(5000);
  if (!queue) return html`<div class="content">${errorText(error) || 'Loading queue…'}</div>`;
  return html`<div class="content queue-page">${errorText(error)}
    <div class="q-tiles">${['tests', 'background', 'perf', 'service'].map((type) => {
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
    ['MEM', host.memory_used_bytes, host.queue_memory_bytes, host.memory_total_bytes, `${gb(host.memory_used_bytes)}/${gb(host.memory_total_bytes)}`, gb],
    ['CPU', host.cpu_percent, host.queue_cpu_percent, 100, pct(host.cpu_percent), pct],
    ['GPU', host.gpu_percent, host.queue_gpu_percent, 100, pct(host.gpu_percent), pct],
  ];
  return html`<div class="q-card q-mac"><span class="q-label">Mac now</span>${metrics.map(([label, total, queue, max, text, format]) => html`<div class="q-meter"><span>${label}</span><i><s style=${`width:${Math.min(100, (total || 0) / (max || 1) * 100)}%`}></s>${typeof queue === 'number' ? html`<s class="q" style=${`width:${Math.min(100, queue / (max || 1) * 100)}%`}></s>` : null}</i><span>${text}${typeof queue === 'number' ? ` · queue ${format(queue)}` : ''}</span></div>`)}</div>`;
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
    ${limits ? html`<a href="/settings/queue" onClick=${(e) => { e.preventDefault(); navigate('/settings/queue'); }}>Queue limits…</a>` : null}
  </section>`;
}

function JobRow({ job, now }) {
  const deadline = Date.parse(job.wait_deadline_at) - now;
  return html`<button class="q-job" type="button" onClick=${() => openPanel(`job:${job.id}`)}>
    <span class="q-job-title">${job.position ? `${job.position}. ` : ''}${title(job)}<small>${job.type} · ${job.requester_name || job.notify_name || ''}</small></span>
    <span class="q-timeline">${timelineSegments(job, now).map((s) => html`<i class=${s.kind} style=${`left:${s.left}%;width:${s.width}%`}></i>`)}</span>
    <span class=${job.quiet_since ? 'red' : ''}>${job.state === 'running' ? `${age(job.started_at, now)}${job.timeout_seconds ? ` of ${duration(job.timeout_seconds)}` : " · no time limit"}` : `${age(job.queued_at, now)} waited`}
      ${job.quiet_since ? ` · quiet ${age(job.quiet_since, now)}` : job.low_cpu ? html`<span class="muted"> · low CPU</span>` : ''}
      ${job.state === 'pending' && deadline < 21600000 && Number.isFinite(deadline) ? ` · gives up in ${duration(deadline / 1000)}` : ''}</span>
  </button>`;
}

function JobPanel({ id, controls }) {
  const encoded = encodeURIComponent(id);
  const [job, error, reload] = usePoll(() => api(`/queue-jobs/${encoded}`), 5000, [id]);
  const [log, logError] = usePoll(() => api(`/queue-jobs/${encoded}/log?lines=40`), 5000, [id]);
  const [before, setBefore] = useState(null);
  const usageInterval = !before && job?.state === 'running' ? 5000 : 0;
  const [usage, usageError] = usePoll(() => api(`/client/queue/jobs/${encoded}/usage${before ? `?before_ms=${before}` : ''}`), usageInterval, [id, before, usageInterval]);
  const [follows, , reloadFollows] = usePoll(() => api('/client/follows'), 30000, [id]);
  const [check, setCheck] = useState(null), [cancel, setCancel] = useState(false), [note, setNote] = useState('');
  const [ask, setAsk] = useState(false), [question, setQuestion] = useState(''), [busy, setBusy] = useState(false), [failure, setFailure] = useState('');
  const perform = async (fn) => {
    if (busy) return;
    setBusy(true); setFailure('');
    try { await fn(); reload(); reloadFollows(); } catch (e) { setFailure(e.message); } finally { setBusy(false); }
  };
  if (!job) return html`<div class="phd"><span class="t">Job</span>${controls}<span class="s">${error?.message || 'Loading…'}</span></div>`;
  const active = ['running', 'pending'].includes(job.state);
  const following = follows?.follows?.some((f) => f.job_id === job.id && f.state !== 'done');
  const agent = job.requester_session_id || job.notify_session_id;
  const send = () => perform(async () => {
    await api(`/inbox/agent/${encodeURIComponent(agent)}/send`, { method: 'POST', headers: config.inbox_token ? { 'x-sm-doc-token': config.inbox_token } : {}, body: { submission_id: crypto.randomUUID(), body: `About queue job ${title(job)} (ID ${job.id}, ${job.type}, ${job.state}): ${question.trim()}` } });
    setQuestion(''); setAsk(false); toast('Sent to the agent');
  });
  const sampleMax = Math.max(100, ...(usage?.samples || []).map((s) => s.cpu_percent || 0), ...(usage?.samples || []).map((s) => s.gpu_percent || 0));
  return html`<div class="phd"><span class=${job.quiet_since ? 'red' : 'sub'}>${job.state}${job.quiet_since ? ' · quiet' : ''}</span><span class="t">${title(job)}</span>${controls}<span class="s">${job.type} · ${job.requester_name || job.notify_name || ''}</span></div>
    <div class="q-panel-body">${errorText(error)}${failure ? html`<p class="err" role="alert">${failure}</p>` : null}
      <div class="q-actions">
        ${job.state === 'pending' ? html`<button class="btn" disabled=${busy} onClick=${() => perform(async () => setCheck(await api(`/client/queue/jobs/${encoded}/start-check`)))}>Start now</button>` : null}
        ${active ? html`<button class="btn" disabled=${busy} onClick=${() => setCancel(!cancel)}>Cancel</button><button class="btn" disabled=${busy || !follows} aria-pressed=${!!following} onClick=${() => perform(() => api(`/queue-jobs/${encoded}/follow`, { method: following ? 'DELETE' : 'POST', body: {} }))}>${following ? 'Following' : 'Follow'}</button>` : null}
        ${agent ? html`<button class="btn" onClick=${() => setAsk(!ask)}>Ask agent</button>` : null}
      </div>
      ${check && job.state === 'pending' ? html`<section class="q-card"><h3>Start this job now?</h3>${check.warnings.map((warning) => html`<p class="amber">${warning}</p>`)}<p>Memory available ${gb(check.memory_available_bytes)} · reserve ${gb(check.memory_reserve_bytes)} · estimate ${gb(check.memory_estimate_bytes)} (${check.memory_estimate_source || 'unknown'})</p><button class="btn" disabled=${busy} onClick=${() => perform(async () => { await api(`/client/queue/jobs/${encoded}/start`, { method: 'POST', body: {} }); setCheck(null); })}>Start anyway</button> <button class="btn" onClick=${() => setCheck(null)}>Keep waiting</button></section>` : null}
      ${cancel && active ? html`<section class="q-card"><label>Cancellation note (optional)<textarea class="inp" maxLength="1000" value=${note} onInput=${(e) => setNote(e.target.value)} /></label><button class="btn" disabled=${busy} onClick=${() => perform(async () => { await api(`/queue-jobs/${encoded}/cancel`, { method: 'POST', body: { note: note.trim() || null } }); setCancel(false); })}>Cancel job</button></section>` : null}
      ${ask ? html`<section class="q-card"><div class="q-suggestions">${['How long do you expect this to run?', 'What is this job for?', 'Is it safe to cancel this?', 'Is this job stuck?'].map((q) => html`<button class="btn sm" onClick=${() => setQuestion(q)}>${q}</button>`)}</div><textarea aria-label="Question for agent" class="inp" value=${question} onInput=${(e) => setQuestion(e.target.value)} /><button class="btn pri" disabled=${busy || !question.trim()} onClick=${send}>Send</button></section>` : null}
      <dl class="q-details"><dt>Command</dt><dd><code>${job.argv?.join(' ') || job.script_path || '—'}</code></dd><dt>Folder</dt><dd>${job.cwd}</dd><dt>Queued</dt><dd>${clock(job.queued_at)}</dd><dt>Started</dt><dd>${clock(job.started_at) || '—'}</dd><dt>Finished</dt><dd>${clock(job.finished_at) || '—'}</dd><dt>Limits</dt><dd>${job.timeout_seconds ? duration(job.timeout_seconds) : "No time limit"} · CPU ${pct(job.cpu_percent)} · GPU ${pct(job.gpu_percent)} · memory ${gb(job.memory_bytes)}</dd><dt>Why it waits</dt><dd>${job.holding?.summary || '—'}</dd><dt>What happened</dt><dd>${job.ended_summary || job.termination_reason || job.state}${job.exit_code != null ? ` · exit ${job.exit_code}` : ''}${job.cancel_detail?.note ? ` · ${job.cancel_detail.note}` : ''}</dd></dl>
      <h3>CPU and GPU over this run</h3>${errorText(usageError)}<p class="sub">CPU: amber · GPU: cyan · percent of one core / GPU second per second · scale ${sampleMax.toFixed(0)}%</p>
      ${usage?.samples?.length ? html`<svg class="q-job-chart" viewBox="0 0 1000 120" preserveAspectRatio="none" role="img" aria-label="Job CPU and GPU over its run">${['cpu_percent', 'gpu_percent'].map((key, i) => html`<path d=${chartPath(usage.samples, key, sampleMax)} fill="none" stroke=${i ? 'var(--cyan)' : 'var(--amber)'} stroke-width="2" vector-effect="non-scaling-stroke" />`)}</svg><div class="q-heading sub"><span>${clock(usage.samples[0].at)}</span><span>${clock(usage.samples.at(-1).at)}</span></div>` : html`<p class="muted">No usage samples recorded.</p>`}
      <div class="q-actions">${usage?.next_before_ms ? html`<button class="btn sm" onClick=${() => setBefore(usage.next_before_ms)}>Earlier samples</button>` : null}${before ? html`<button class="btn sm" onClick=${() => setBefore(null)}>Latest samples</button>` : null}</div>
      <h3>Last 40 log lines</h3><pre class="q-log">${log?.text || logError?.message || 'Loading log…'}</pre>
    </div>`;
}
registerPanel('job', JobPanel);
