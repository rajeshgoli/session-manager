// Run: node --experimental-vm-modules --test tests/web/bug.test.mjs
// Spec 1859 B6: the bug button's page data, capture fallback, dialog labels and the standing row.
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
const link = (name, parent) => load(name === 'preact' ? `${root}/vendor/preact.module.js` : name === 'preact/hooks' ? `${root}/vendor/hooks.module.js` : name === 'htm' ? `${root}/vendor/htm.module.js` : resolve(dirname(parent.identifier), name));
const bug = load(`${root}/bug.js`);
const board = load(`${root}/board.js`);
await bug.link(link);
await board.link(link);
await bug.evaluate();
await board.evaluate();
const { captureScreen, decodedBytes, SCREENSHOT_MAX_BYTES } = bug.namespace;
const { pageDataRing, trimPageData } = modules.get(`${root}/ui.js`).namespace;
const { bugBody, bugPrimary, bugStartBody, filedText } = modules.get(`${root}/board-start.js`).namespace;
const { rowActions, standingText } = board.namespace;

test('the page-data ring keys by path, keeps the newest 12 and answers since a mark', () => {
  const ring = pageDataRing();
  for (let i = 0; i < 14; i++) ring.put(`/p${i}?x=${i}`, { i });
  const all = ring.since(0);
  assert.deepEqual(Object.keys(all), Array.from({ length: 12 }, (_, i) => `/p${i + 2}`));
  ring.put('/p2?again', { i: 'new' });
  ring.put('/p14', { i: 14 });
  assert.equal(ring.since(0)['/p2'].i, 'new', 'a refetch replaces the value and makes the path newest');
  assert.equal(ring.since(0)['/p3'], undefined, 'the oldest path is evicted');
  ring.markPage();
  assert.deepEqual(ring.forPage(), {});
  ring.put('/client/board?clock_hours=3', { lanes: [] });
  assert.deepEqual(ring.forPage(), { '/client/board': { lanes: [] } });
});

test('page data over the cap drops the largest entries first', () => {
  const data = { '/big': 'x'.repeat(200), '/mid': 'y'.repeat(100), '/small': 'z' };
  assert.deepEqual(Object.keys(trimPageData(data, 150)), ['/mid', '/small']);
  assert.deepEqual(Object.keys(trimPageData(data, 30)), ['/small']);
  assert.deepEqual(trimPageData(data), data);
});

const png = (bytes) => `data:image/png;base64,${'A'.repeat(Math.ceil(bytes / 3) * 4)}`;
test('capture uses the screen ratio up to 2, then ratio 1, then gives up', async () => {
  const ratios = [];
  const ok = (url) => async (node, options) => { ratios.push(options.pixelRatio); return url; };
  assert.equal(await captureScreen({ node: {}, ratio: 3, render: ok(png(30)) }), 'A'.repeat(40));
  assert.deepEqual(ratios, [2]);
  ratios.length = 0;
  let calls = 0;
  const failOnce = async (node, options) => { ratios.push(options.pixelRatio); if (calls++ === 0) throw new Error('taint'); return png(3); };
  assert.equal(await captureScreen({ node: {}, ratio: 2, render: failOnce }), 'AAAA');
  assert.deepEqual(ratios, [2, 1]);
  ratios.length = 0;
  const huge = async (node, options) => { ratios.push(options.pixelRatio); return options.pixelRatio > 1 ? png(SCREENSHOT_MAX_BYTES + 3) : png(3); };
  assert.equal(await captureScreen({ node: {}, ratio: 2, render: huge }), 'AAAA', 'over 8 MiB at ratio 2 retries at ratio 1');
  assert.deepEqual(ratios, [2, 1]);
  assert.equal(await captureScreen({ node: {}, ratio: 2, render: async () => { throw new Error('no'); } }), null);
  assert.equal(await captureScreen({ node: {}, ratio: 1, render: async () => 'data:,' }), null);
  assert.equal(decodedBytes('AAAA'), 3);
  assert.equal(decodedBytes('AA=='), 1);
});

test('the excluded dialog is filtered out of the capture', async () => {
  let filter;
  await captureScreen({ node: {}, ratio: 1, render: async (node, options) => { filter = options.filter; return png(3); } });
  assert.equal(filter({ dataset: { bugExclude: '1' } }), false);
  assert.equal(filter({ dataset: {} }), true);
  assert.equal(filter({}), true, 'text nodes have no dataset');
});

test('the primary button reads File bug, File and start, or Start after a failed start', () => {
  assert.equal(bugPrimary({ startAgent: false }), 'File bug');
  assert.equal(bugPrimary({ startAgent: true }), 'File and start');
  assert.equal(bugPrimary({ startAgent: true, busy: true }), 'Filing and starting…');
  assert.equal(bugPrimary({ startAgent: true, filed: { start_error: 'refused' } }), 'Start');
  assert.equal(bugPrimary({ startAgent: true, filed: { start_error: 'refused' }, busy: true }), 'Starting…');
});

test('the filing body carries the start choice only when Start an agent is on', () => {
  const captured = { screenshot: 'AAAA', page: 'Board', route: '/board?lane=3', page_data: { '/client/board': { lanes: [] } } };
  const form = { provider: 'claude', model: 'claude-opus-5-5', reasoning_effort: 'high', reviewer: null, working_dir: '/w' };
  const off = bugBody(captured, { text: 'Board wrong\nmore', screenshot: true, startAgent: false }, form);
  assert.equal(off.client, 'web');
  assert.equal(off.page, 'Board');
  assert.equal(off.route, '/board?lane=3');
  assert.equal(off.screenshot_png, 'AAAA');
  assert.deepEqual(off.page_data, captured.page_data);
  assert.equal(off.start, null);
  const on = bugBody(captured, { text: 'x', screenshot: false, startAgent: true }, form);
  assert.equal(on.screenshot_png, null);
  assert.deepEqual(on.start, { provider: 'claude', model: 'claude-opus-5-5', reasoning_effort: 'high', reviewer: null });
  assert.equal(bugBody({ ...captured, screenshot: null }, { text: 'x', screenshot: true }, form).screenshot_png, null);
});

test('a start retry is board Start on the filed issue, named by the server', () => {
  const body = bugStartBody({ repo: 'rajeshgoli/session-manager', number: 1870 }, { provider: 'codex-fork', model: null, reasoning_effort: 'high' });
  assert.equal(body.repo, 'rajeshgoli/session-manager');
  assert.equal(body.number, 1870);
  assert.equal(body.reasoning_effort, 'high');
  assert.equal(body.name, undefined);
  assert.equal(body.brief, undefined);
});

test('the filed toast names the issue, the agent and a missing board link', () => {
  const issue = { number: 1870, url: 'https://github.com/x/y/issues/1870' };
  assert.equal(filedText({ issue }), 'Filed #1870');
  assert.equal(filedText({ issue, started: { name: 'sm-1870-engineer' } }), 'Filed #1870 · started sm-1870-engineer');
  assert.equal(filedText({ issue, board_note: 'refused' }), 'Filed #1870 · not on the board');
});

test('the standing Bugs goal offers no Start or Close and counts open bugs', () => {
  const goal = { state: 'standing', waits_on: [{ number: 1, state: 'done' }, { number: 2, state: 'ready' }, { number: 3, state: 'in_progress' }] };
  const can = rowActions(goal);
  assert.equal(can.start, false);
  assert.equal(can.close, false);
  assert.equal(can.startBlocked, false);
  assert.equal(can.whenReady, false);
  assert.equal(can.menu, true);
  assert.equal(standingText(goal), 'Standing lane · 2 open bugs');
  assert.equal(standingText({ state: 'standing', waits_on: [] }), 'Standing lane · 0 open bugs');
  assert.equal(standingText({ waits_on: [{ number: 2, state: 'ready' }] }), 'Standing lane · 1 open bug');
});
