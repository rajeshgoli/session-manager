// Pure presentation rules shared by the Queue page and its regression tests.
export function timelineSegments(job, now) {
  const start = now - 3 * 3600000;
  const x = (at) => Math.max(0, Math.min(100, (Date.parse(at) - start) / (now - start) * 100));
  const out = [];
  const add = (kind, from, to) => {
    const left = x(from), right = to ? x(to) : 100;
    if (Number.isFinite(left) && right > left) out.push({ kind, left, width: right - left });
  };
  add('waiting', job.queued_at, job.started_at);
  if (job.started_at) {
    add('running', job.started_at, job.quiet_since || job.finished_at);
    if (job.quiet_since) add('quiet', job.quiet_since, job.finished_at);
  }
  return out;
}
export function limitsInsight(stats) {
  const limits = stats?.waiting?.find((row) => row.group === 'limits');
  return limits && limits.job_seconds >= 3600 && limits.headroom_job_seconds / limits.job_seconds >= .25 ? limits : null;
}
export function waitingGroups(jobs) {
  const groups = new Map();
  for (const job of [...jobs].sort((a, b) => a.position - b.position)) {
    const caption = [job.holding?.summary || 'Waiting for admission', job.lane_rank != null ? `lane ${job.lane_rank}${job.lane_goal?.title ? ` · ${job.lane_goal.title}` : ''}` : ''].filter(Boolean).join(' · ');
    // Only combine adjacent jobs: grouping by reason must never change start order.
    const last = [...groups.values()].at(-1);
    if (last?.caption === caption) last.jobs.push(job);
    else groups.set(job.id, { caption, jobs: [job] });
  }
  return [...groups.values()];
}
/** "Recorded 3h 27m of the last 24h" when samples cover under 95% of the
 * range, so a stopped recorder reads as missing data, not an idle Mac (#2006). */
export function coverageNote(series) {
  const window = (series?.hours || 0) * 3600, covered = series?.summary?.covered_seconds || 0;
  if (!window || covered >= window * 0.95) return '';
  const minutes = Math.floor(covered / 60);
  const recorded = minutes < 60 ? `${minutes}m` : `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
  return `Recorded ${recorded} of the last ${series.hours <= 24 ? `${series.hours}h` : `${series.hours / 24}d`}`;
}

export function chartPath(buckets, key, max, height = 120) {
  let path = '', pen = false;
  buckets.forEach((bucket, index) => {
    const value = bucket[key];
    if (typeof value !== 'number') { pen = false; return; }
    const x = (index + .5) / buckets.length * 1000;
    const y = height - Math.max(0, Math.min(1, value / max)) * height;
    path += `${pen ? 'L' : 'M'}${x.toFixed(2)},${y.toFixed(2)} `;
    pen = true;
  });
  return path;
}

export const jobAgentId = (job) => job.notify_session_id || job.requester_session_id || null;
export const jobAgentLabel = (job) => job.notify_name || (job.notify_session_id ? job.notify_session_id.slice(0, 8) : job.requester_name || job.requester_session_id?.slice(0, 8) || 'no agent');

export function jobQuestionPrompt(owner, job, question, now = Date.now()) {
  const at = Date.parse(job.state === 'running' ? job.started_at : job.queued_at);
  const minutes = Math.max(0, Math.floor((now - at) / 60000));
  const duration = minutes < 60 ? `${minutes}m` : `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
  const timing = ['running', 'pending'].includes(job.state) && Number.isFinite(at)
    ? `${job.state === 'running' ? 'running for' : 'waiting'} ${duration}` : job.state;
  return `${owner} is asking from sm web about your queue job ${job.label || job.id} (ID ${job.id}, type ${job.type}, ${timing}): ${question.trim()}`;
}

/** A conflict belongs to somebody else's question; never attach to it. */
export async function askJobQuestion(api, owner, job, question) {
  try {
    return await api(`/sessions/${encodeURIComponent(jobAgentId(job))}/what`, {
      method: 'POST', body: { delivery_mode: 'poll', prompt: jobQuestionPrompt(owner, job, question) },
    });
  } catch (error) {
    if (error.status === 409) throw new Error(`${jobAgentLabel(job)} is answering another question — try again in a minute`);
    throw error;
  }
}

/**
 * A review job's card text (spec 1768 I2): the PR it reviews, the reviewer,
 * its model, round and author, and why this reviewer when it is a fallback.
 */
export function reviewJobText(job) {
  const review = job.review;
  if (!review) return null;
  const match = /^(.*?) \((.*)\)$/.exec(review.reviewer_label || '');
  return {
    title: `review · ${review.repo.split('/').pop()} #${review.pr_number}`,
    reviewer: match ? match[1] : review.reviewer_label || 'Review run',
    detail: [match ? match[2].replace(', ', ' · ') : null, `round ${review.round}`, `for ${review.author_name}`,
      review.policy_source && review.policy_source !== 'default' ? `${review.policy_source} policy` : null].filter(Boolean).join(' · '),
    why: review.why === 'default' ? null : review.why,
  };
}
