import { test } from 'node:test';
import assert from 'node:assert/strict';
import { timelineSegments, limitsInsight, waitingGroups, chartPath, coverageNote } from '../crates/sm-server/src/web/queue-model.js';

test('quiet red starts at server quiet_since; earlier run stays green', () => {
  const now = Date.parse('2026-09-29T12:00:00Z');
  const segments = timelineSegments({ queued_at: '2026-09-29T09:00:00Z', started_at: '2026-09-29T10:00:00Z', quiet_since: '2026-09-29T11:00:00Z' }, now);
  assert.deepEqual(segments.map((s) => s.kind), ['waiting', 'running', 'quiet']);
  assert.ok(Math.abs(segments[2].left - 200 / 3) < 1e-9);
  assert.equal(timelineSegments({ queued_at: '2026-09-29T08:00:00Z' }, now)[0].width, 100);
});
test('insight needs an hour held and 25 percent headroom, including unknown in denominator', () => {
  const stats = (seconds, headroom) => ({ waiting: [{ group: 'limits', job_seconds: seconds, headroom_job_seconds: headroom }] });
  assert.equal(limitsInsight(stats(3599, 3599)), null);
  assert.equal(limitsInsight(stats(4000, 999)), null);
  assert.ok(limitsInsight(stats(4000, 1000)));
});
test('group captions never reorder waiting jobs', () => {
  const rows = [3, 1, 2].map((position) => ({ id: String(position), position, holding: { summary: position === 2 ? 'Memory' : 'Slot' } }));
  assert.deepEqual(waitingGroups(rows).flatMap((g) => g.jobs.map((j) => j.position)), [1, 2, 3]);
});
test('missing chart samples create gaps; absent queue values are not zero', () => {
  const path = chartPath([{ cpu: 50 }, {}, { cpu: 20 }], 'cpu', 100);
  assert.equal((path.match(/M/g) || []).length, 2);
  assert.equal(chartPath([{ cpu: 50 }], 'queue_cpu_avg', 100), '');
});

test('Ask agent targets the notification recipient and uses a polling side question', async () => {
  const { askJobQuestion, jobAgentLabel, jobQuestionPrompt } = await import('../crates/sm-server/src/web/queue-model.js');
  const job = { id: 'job-test', label: 'Download', type: 'tests', state: 'running', started_at: '2026-09-29T12:00:00Z', notify_session_id: 'responsible', notify_name: 'Responsible agent', requester_session_id: 'submitter', requester_name: 'Submitting agent' };
  assert.equal(jobAgentLabel(job), 'Responsible agent');
  assert.match(jobQuestionPrompt('Rajesh', job, 'How long?', Date.parse('2026-09-29T12:10:00Z')), /running for 10m/);
  const calls = [];
  const request = await askJobQuestion(async (...args) => { calls.push(args); return { request_id: 'answer-1', status: 'pending' }; }, 'Rajesh', job, 'How long?');
  assert.equal(request.request_id, 'answer-1');
  assert.equal(calls[0][0], '/sessions/responsible/what');
  assert.equal(calls[0][1].body.delivery_mode, 'poll');
  assert.match(calls[0][1].body.prompt, /ID job-test.*How long\?/);
  const busyCalls = [];
  await assert.rejects(askJobQuestion(async (...args) => {
    busyCalls.push(args); throw Object.assign(new Error('already answering request somebody-else'), { status: 409 });
  }, 'Rajesh', job, 'How long?'), /Responsible agent is answering another question/);
  assert.equal(busyCalls.length, 1, 'must not attach to or poll the conflicting question');
});
test('usage caption names recorded time when samples cover under 95% of the range', () => {
  assert.equal(coverageNote({ hours: 24, summary: { covered_seconds: 12437 } }), 'Recorded 3h 27m of the last 24h');
  assert.equal(coverageNote({ hours: 168, summary: { covered_seconds: 0 } }), 'Recorded 0m of the last 7d');
  assert.equal(coverageNote({ hours: 1, summary: { covered_seconds: 3420 } }), '');
  assert.equal(coverageNote(null), '');
});
test('breakdown names each job, lumps tiny ones, and leaves the rest to the Mac', async () => {
  const { usageBreakdown, usageText, jobUsage } = await import('../crates/sm-server/src/web/queue-model.js');
  const GIB = 2 ** 30;
  const running = [
    { id: 'w', label: '2171-wave8', type: 'service', usage: { memory_bytes: 0.2 * GIB, cpu_percent: 0, gpu_percent: 0 } },
    { id: 's', label: '2171-iter7-score', type: 'background', usage: { memory_bytes: 131 * GIB, cpu_percent: 0.3, gpu_percent: 35 } },
    { id: 'x', label: 'shell', type: 'shell', shell: { current_bytes: 4 * GIB } },
    { id: 'u', label: 'no sample yet', type: 'tests' },
  ];
  const host = { memory_total_bytes: 256 * GIB, memory_used_bytes: 209 * GIB, cpu_percent: 64, gpu_percent: 100 };
  const { jobs, rows } = usageBreakdown(running, host);
  assert.deepEqual(jobs.map((j) => j.color), [0, 1, 2, 3]);
  const [memory, cpu, gpu] = rows;
  assert.deepEqual(memory.segments.map((s) => s.label), ['2171-iter7-score', 'shell', '1 smaller job']);
  assert.equal(memory.segments[0].color, 1, 'colour follows the running list, not the size order');
  assert.ok(Math.abs(memory.other - (209 - 131 - 4.2) * GIB) < 1);
  assert.equal(cpu.segments.length, 1, 'jobs under 1% of the machine are one slice');
  assert.equal(cpu.segments[0].color, 'small');
  assert.equal(cpu.other, 63.7);
  assert.equal(gpu.segments[0].share, 35);
  assert.equal(gpu.otherShare, 65);
  assert.equal(jobUsage(running[3]).memory, null);
  // No host reading: jobs still show, with nothing claimed for the rest.
  const bare = usageBreakdown(running, { available: false });
  assert.deepEqual(bare.rows.map((r) => r.key), ['cpu', 'gpu']);
  assert.equal(bare.rows[1].other, null);
  assert.deepEqual([usageText('memory', 131 * GIB), usageText('memory', 1.5 * GIB), usageText('memory', 2 ** 20 * 300), usageText('cpu', 0.29), usageText('gpu', 35.2), usageText('cpu', null)], ['131 GB', '1.5 GB', '300 MB', '0.3%', '35%', '—']);
});
