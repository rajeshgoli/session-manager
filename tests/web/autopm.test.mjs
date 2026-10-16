// Run: node --experimental-vm-modules --test tests/web/autopm.test.mjs
// Spec 2170 A.5 and F.3 (sm#2176): the web's client headers and the What changed card's text.
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
const autopm = load(`${root}/autopm.js`);
await autopm.link(link);
await autopm.evaluate();
const { liveSummary, changeMeta, skipPatch, budgetText } = autopm.namespace;
const { clientNavState, screenFor, loadNav, FOREGROUND_MS } = modules.get(`${root}/ui.js`).namespace;

test('owner requests carry the screen and how they began; later ones are background', () => {
  let now = 1000;
  const nav = clientNavState(() => now);
  nav.setScreen('board');
  assert.deepEqual(nav.headers(), { 'x-sm-screen': 'board', 'x-sm-nav': 'launch' }, 'the first request after a load');
  assert.deepEqual(nav.headers(), { 'x-sm-screen': 'board', 'x-sm-nav': 'nav' });
  now += FOREGROUND_MS + 1;
  assert.deepEqual(nav.headers(), { 'x-sm-screen': 'board', 'x-sm-bg': '1' }, 'no tap for a while: a poll');
  nav.acted();
  nav.setScreen(() => 'doc');
  assert.deepEqual(nav.headers(), { 'x-sm-screen': 'doc', 'x-sm-nav': 'nav' });
  nav.state.polling = true;
  assert.equal(nav.headers()['x-sm-bg'], '1', "a poll's repeat is background even right after a tap");
});

test('screens name the page or its open panel, and a deep page load is a link', () => {
  assert.equal(screenFor('board', null), 'board');
  assert.equal(screenFor('agents', 'agent:abc'), 'session/detail');
  assert.equal(screenFor('inbox', 'thread:t1'), 'inbox/thread');
  assert.equal(screenFor('inbox', 'doc:/docs/x'), 'doc');
  assert.equal(screenFor('history', 'ticket:o/r#1'), 'history/ticket');
  assert.equal(screenFor('settings', null, '/settings#auto-pm'), 'settings/autopm');
  assert.equal(loadNav('navigate', '/board'), 'link');
  assert.equal(loadNav('navigate', '/'), 'launch');
  assert.equal(loadNav('reload', '/board'), 'launch');
});

test('the What changed card counts live changes and dates each one', () => {
  assert.equal(liveSummary([]), 'None live');
  assert.equal(liveSummary([{ state: 'live' }, { state: 'measuring' }, { state: 'live' }]), '2 live · 1 measuring');
  const oct21 = Date.UTC(2026, 9, 21, 12), nov4 = Date.UTC(2026, 10, 4, 12);
  assert.match(changeMeta({ state: 'live', live_at_ms: oct21, measure_at_ms: nov4 }, oct21), /^Live since Oct 21 · measured Nov 4$/);
  assert.match(changeMeta({ state: 'measuring', live_at_ms: oct21, measure_at_ms: nov4 }, nov4 + 1), /measuring now$/);
  assert.match(changeMeta({ state: 'reverted', ended_at_ms: oct21, decision_note: 'missed its bar' }), /^Reverted Oct 21 · missed its bar$/);
});

test('Skip this week writes now, and undoing it writes null', () => {
  const now = new Date('2026-10-15T10:00:00Z');
  assert.deepEqual(skipPatch(false, now), { autopm: { skip_window_starting: '2026-10-15T10:00:00.000Z' } });
  assert.deepEqual(skipPatch(true, now), { autopm: { skip_window_starting: null } });
  assert.equal(budgetText({ spent: 7, allowance: 20 }), '7 of 20');
  assert.equal(budgetText({ spent: 0, allowance: 0, skipped: true }), 'Skipped this week');
  assert.equal(budgetText(null), 'No meter data');
});
