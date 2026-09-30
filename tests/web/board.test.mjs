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
const { groupTickets, clockSegments, BALL_TONE, canStart } = board.namespace;
const { startBody, providerDefaults } = modules.get(`${root}/board-start.js`).namespace;
test('rows preserve every actionable ticket and fold only blocked/done', () => {
  const states = ['needs_you', 'ready', 'in_progress', 'blocked', 'done'];
  const rows = states.map((state, number) => ({ state, number }));
  const g = groupTickets(rows);
  assert.deepEqual(g.active.map((t) => t.state), states.slice(0, 3));
  assert.deepEqual(g.blocked, [rows[3]]);
  assert.deepEqual(g.done, [rows[4]]);
  assert.equal(groupTickets(rows.slice(2)).active.length, 1);
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
