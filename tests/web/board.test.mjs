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
const { startBody, providerDefaults } = modules.get(`${root}/board-start.js`).namespace;
const { threadHref } = modules.get(`${root}/ui.js`).namespace;
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

test('merged-but-open ready tickets cannot offer Start', () => {
  assert.equal(canStart({ state: 'ready', warnings: ['merged_not_closed'] }), false);
  assert.equal(canStart({ state: 'ready', warnings: [] }), true);
  assert.equal(canStart({ state: 'in_progress' }), false);
});
