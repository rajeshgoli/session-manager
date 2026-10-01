// Run: node --experimental-vm-modules --test tests/web/board.test.mjs
// Load the actual unbundled modules using their browser import-map equivalents.
import { SourceTextModule } from 'node:vm';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import assert from 'node:assert/strict';
import { test } from 'node:test';
const root = resolve('crates/sm-server/src/web');
const modules = new Map();
function load(path) {
  if (modules.has(path)) return modules.get(path);
  const m = new SourceTextModule(readFileSync(path, 'utf8'), { identifier: path });
  modules.set(path, m);
  return m;
}
const board = load(`${root}/board.js`);
await board.link((name, parent) => load(name === 'preact' ? `${root}/vendor/preact.module.js` : name === 'preact/hooks' ? `${root}/vendor/hooks.module.js` : name === 'htm' ? `${root}/vendor/htm.module.js` : resolve(dirname(parent.identifier), name)));
await board.evaluate();
const { groupTickets, visibleOther, openBlockers, blockedText, clockSegments, BALL_TONE, canStart } = board.namespace;
const { startBody, providerDefaults, canStartAnyway, blockedReasons } = modules.get(`${root}/board-start.js`).namespace;
const { reviewFallback, reviewerText, switchKind, inheritedPolicy, tier } = modules.get(`${root}/reviews.js`).namespace;
const queueModel = load(`${root}/queue-model.js`);
await queueModel.link(() => { throw new Error('queue-model has no imports'); });
await queueModel.evaluate();
const { reviewJobText } = queueModel.namespace;
const agents = load(`${root}/agents.js`);
await agents.link((name, parent) => load(name === 'preact' ? `${root}/vendor/preact.module.js` : name === 'preact/hooks' ? `${root}/vendor/hooks.module.js` : name === 'htm' ? `${root}/vendor/htm.module.js` : resolve(dirname(parent.identifier), name)));
await agents.evaluate();
const { pairedText, reviewWaitText, jobsFact } = agents.namespace;
const { threadHref } = modules.get(`${root}/ui.js`).namespace;
const { tokens, tokensOf } = modules.get(`${root}/handoff.js`).namespace;
test('handoff thresholds read in tokens with three significant figures', () => {
  assert.deepEqual([350000, 1000000, 206720, 258400, 999999, 1250000, 512].map(tokens), ['350k', '1M', '207k', '258k', '1M', '1.25M', '512']);
  assert.equal(tokensOf(35, 1000000), '350k of 1M tokens');
  assert.equal(tokensOf('80', 258400), '207k of 258k tokens');
  assert.equal(tokensOf('', 258400), '');
  assert.equal(tokensOf(35, undefined), '');
});
test('rows preserve every actionable ticket and sort done by closure time', () => {
  const states = ['needs_you', 'close_ready', 'ready', 'in_progress', 'blocked', 'done'];
  const rows = states.map((state, number) => ({ state, number }));
  const g = groupTickets(rows);
  assert.deepEqual(g.active.map((t) => t.state), states.slice(0, 4));
  assert.deepEqual(g.blocked, [rows[4]]);
  assert.deepEqual(g.done, [rows[5]]);
  assert.deepEqual(groupTickets([{state:'done',closed_at:'2026-09-01'}, {state:'done',closed_at:'2026-09-20'}]).done.map(t => t.closed_at), ['2026-09-20','2026-09-01']);
});
test('other tickets always expose needs-you rows and blockers omit closed dependencies', () => {
  const tickets = Array.from({length:15}, (_,number) => ({state:'ready',number}));
  tickets[14].state = 'needs_you';
  assert.deepEqual(visibleOther(tickets).map(t => t.number), [14,0,1,2,3,4,5,6,7,8]);
  const blocked = {waits_on:[{number:2,state:'done'},{number:3,state:'in_progress'}]};
  assert.deepEqual(openBlockers(blocked).map(t => t.number), [3]);
  assert.equal(blockedText(blocked), '#3');
  assert.equal(threadHref({key:'agent:session-1',at:'message one'}), '/inbox?open=thread:session-1&at=message%20one');
});
test('clock clips at the chosen window and preserves gaps and quiet segments', () => {
  const end = '2026-09-30T03:00:00Z';
  const segments = [
    { kind: 'queue', from: '2026-09-29T23:00:00Z', to: '2026-09-30T01:00:00Z' },
    { kind: 'quiet', from: '2026-09-30T02:00:00Z', to: '2026-09-30T04:00:00Z' },
    { kind: 'working', from: '2026-09-29T22:00:00Z', to: '2026-09-29T23:00:00Z' },
  ];
  const result = clockSegments(segments, end, 3);
  assert.equal(result.length, 2);
  assert.equal(result[0].left, 0);
  assert.ok(Math.abs(result[0].width - 100 / 3) < 1e-8);
  assert.equal(result[1].kind, 'quiet');
  assert.ok(Math.abs(result[1].left - 200 / 3) < 1e-8);
  assert.equal(BALL_TONE.job_quiet, 'red');
  assert.equal(BALL_TONE.no_agent, 'red');
  assert.equal(clockSegments([], end, 24).length, 0);
});
test('Start uses rendered name and brief and omits provider-default model/effort', () => {
  const ticket = { repo: 'acme/widgets', number: 42 };
  const form = { provider: 'claude', name: 'sm-42-engineer', brief: 'Work ticket #42: widgets', model: null, reasoning_effort: null, working_dir: '/work/widgets' };
  assert.deepEqual(startBody(ticket, form), { ...ticket, provider: 'claude', name: form.name, brief: form.brief });
  assert.equal(startBody(ticket, { ...form, model: 'opus', reasoning_effort: 'high' }).model, 'opus');
  assert.equal(startBody({ ...ticket, state: 'blocked' }, form).start_blocked, true);
  const settings = { new_agent: { claude: { model: null, effort: null }, codex: { model: 'astra', effort: 'high' } } };
  const codex = providerDefaults(settings, 'codex-fork');
  assert.deepEqual(codex, { provider: 'codex-fork', model: 'astra', reasoning_effort: 'high' });
  assert.deepEqual(providerDefaults(settings, 'claude'), { provider: 'claude', model: null, reasoning_effort: null });
});

test('early start follows server warnings and blocked reasons never show an empty waits-on label', () => {
  const ticket = { repo: 'acme/widgets', number: 42, state: 'blocked', warnings: [], waits_on: [{ number: 41, state: 'ready' }] };
  const form = { provider: 'claude', name: 'sm-42-engineer', brief: 'Work #42' };
  assert.equal(canStartAnyway(ticket), true);
  assert.equal(startBody(ticket, form).start_blocked, true);
  assert.deepEqual(blockedReasons(ticket), ['#42 waits on #41, which is not done.']);
  for (const warning of ['stale', 'cycle', 'merged_not_closed']) {
    const blocked = { ...ticket, warnings: [warning], waits_on: [] };
    assert.equal(canStartAnyway(blocked), false);
    assert.equal(startBody(blocked, form).start_blocked, undefined);
    assert.equal(blockedReasons(blocked).length, 1);
    assert.doesNotMatch(blockedReasons(blocked)[0], /waits on/);
  }
  assert.match(blockedReasons({ ...ticket, warnings: ['stale', 'cycle'], waits_on: [] }).join(' '), /stale.*dependency cycle/);
});

test('merged-but-open ready tickets cannot offer Start', () => {
  assert.equal(canStart({ state: 'ready', warnings: ['merged_not_closed'] }), false);
  assert.equal(canStart({ state: 'ready', warnings: [] }), true);
  assert.equal(canStart({ state: 'in_progress' }), false);
});

test('the shown fallback matches the spec table (1768 C2)', () => {
  const text = (reviewer) => reviewFallback(reviewer).map(reviewerText).join(' > ');
  assert.equal(text({ kind: 'github_codex' }), 'Codex run · gpt-6-sol · medium > Claude run · opus · high');
  assert.equal(text({ kind: 'codex', model: 'gpt-6-astra', effort: 'medium' }), 'Claude run · fable · xhigh');
  for (const model of ['gpt-6-sol', 'gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-5.5', 'unknown']) assert.equal(text({ kind: 'codex', model, effort: 'high' }), 'Claude run · opus · high');
  for (const model of ['gpt-6-luna', 'gpt-5.6-luna']) assert.equal(text({ kind: 'codex', model, effort: 'high' }), 'Claude run · sonnet · high');
  assert.equal(text({ kind: 'claude', model: 'fable', effort: 'max' }), 'Codex run · gpt-6-astra · high');
  for (const model of ['opus', 'opus[1m]']) assert.equal(text({ kind: 'claude', model, effort: 'low' }), 'Codex run · gpt-6-sol · medium');
  for (const model of ['sonnet', 'haiku']) assert.equal(text({ kind: 'claude', model, effort: 'low' }), 'Codex run · gpt-6-luna · high');
  assert.equal(text({ kind: 'paired', provider: 'codex', model: 'gpt-6-astra', effort: 'high' }), 'Codex run · gpt-6-astra · high > Claude run · fable · xhigh');
  assert.equal(tier('fable'), 'top'); assert.equal(tier('opus'), 'mid'); assert.equal(tier('haiku'), 'low');
});

test('switching reviewer kind keeps model and effort only on the same provider', () => {
  assert.deepEqual(switchKind({ kind: 'github_codex' }, 'codex'), { kind: 'codex', model: 'gpt-6-sol', effort: 'medium' });
  assert.deepEqual(switchKind({ kind: 'codex', model: 'gpt-6-astra', effort: 'high' }, 'paired'), { kind: 'paired', provider: 'codex', model: 'gpt-6-astra', effort: 'high' });
  assert.deepEqual(switchKind({ kind: 'paired', provider: 'claude', model: 'fable', effort: 'max' }, 'claude'), { kind: 'claude', model: 'fable', effort: 'max' });
  assert.deepEqual(switchKind({ kind: 'codex', model: 'gpt-6-astra', effort: 'high' }, 'claude'), { kind: 'claude', model: 'opus', effort: 'high' });
  assert.deepEqual(switchKind({ kind: 'claude', model: 'fable', effort: 'max' }, 'github_codex'), { kind: 'github_codex' });
});

test('a scope without its own policy names the next scope out', () => {
  const listing = { default: { reviewer: { kind: 'github_codex' } }, policies: [{ scope: 'repo', repo: 'o/a', reviewer: { kind: 'claude', model: 'opus', effort: 'high' } }] };
  assert.equal(inheritedPolicy(listing, { scope: 'lane', repo: 'o/a' }).source, 'repo a');
  assert.equal(inheritedPolicy(listing, { scope: 'lane', repo: 'o/b' }).source, 'the default');
  assert.equal(inheritedPolicy(listing, { scope: 'repo', repo: 'o/a' }).source, 'the default');
  assert.equal(inheritedPolicy(listing, { scope: 'ticket', repo: 'o/a', lanePolicy: { reviewer: { kind: 'github_codex' } } }).source, 'the lane');
});

test('Start sends the chosen reviewer and nothing when the ticket keeps its policy', () => {
  const ticket = { repo: 'o/a', number: 1848, state: 'ready' };
  const form = { provider: 'claude', name: 'far-1848', brief: 'b', model: null, reasoning_effort: null };
  assert.equal(startBody(ticket, form).reviewer, undefined);
  const reviewer = { kind: 'paired', provider: 'codex', model: 'gpt-6-astra', effort: 'high' };
  assert.deepEqual(startBody(ticket, { ...form, reviewer }).reviewer, reviewer);
});

test('a review job card names the PR, reviewer, round, author and fallback reason', () => {
  const job = { review: { repo: 'o/far', pr_number: 1851, round: 1, reviewer_label: 'Codex run (gpt-6-sol, medium)', author_name: 'far-1848',
    why: 'GitHub Codex: paused (out of quota since 11:06 pm), so its fallback' } };
  assert.deepEqual(reviewJobText(job), { title: 'review · far #1851', reviewer: 'Codex run', detail: 'gpt-6-sol · medium · round 1 · for far-1848',
    why: 'GitHub Codex: paused (out of quota since 11:06 pm), so its fallback' });
  assert.equal(reviewJobText({ review: { ...job.review, why: 'default' } }).why, null);
  assert.equal(reviewJobText({ review: { ...job.review, policy_source: 'ticket #1848' } }).detail, 'gpt-6-sol · medium · round 1 · for far-1848 · ticket #1848 policy');
  assert.equal(reviewJobText({ label: 'cargo' }), null);
});

test('authors and paired reviewers read their review state on the Agents page', () => {
  const now = Date.parse('2026-10-01T03:00:00Z');
  assert.equal(reviewWaitText({ reviewer_label: 'far-1848-reviewer', since: '2026-10-01T02:42:00Z' }, { pr_number: 1851 }, now), 'Waiting on review by far-1848-reviewer · 18m');
  assert.equal(reviewWaitText(null, { pr_number: 1851, since: '2026-10-01T02:42:00Z' }, now), 'Waiting on review of PR #1851 · 18m');
  const paired = { pr_number: 1851, author_name: 'far-1848', round: 1, ticket: 1848 };
  for (const request_state of ['waiting_reviewer', 'reviewing', 'nudged']) {
    assert.deepEqual(pairedText({ ...paired, request_state }), { active: true, text: 'Reviewing PR #1851 for far-1848 · round 1' });
  }
  assert.deepEqual(pairedText({ ...paired, request_state: null }), { active: false, text: 'Paired reviewer for #1848 · idle' });
  assert.equal(pairedText(null), null);
  assert.deepEqual(jobsFact({ paired_reviewer: { ...paired, request_state: 'reviewing' }, facts: { jobs: { text: 'No jobs' } } }), { text: 'Reviewing PR #1851 for far-1848 · round 1', tone: 'amber' });
  assert.deepEqual(jobsFact({ paired_reviewer: { ...paired, request_state: 'reviewing' }, facts: { jobs: { running: 1, text: 'Tests running 3m', tone: 'green' } } }),
    { text: 'Reviewing PR #1851 for far-1848 · round 1 · ▶ Tests running 3m', tone: 'green' });
});
