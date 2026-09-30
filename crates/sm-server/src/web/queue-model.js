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
