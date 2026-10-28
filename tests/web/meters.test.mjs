// Run: node --experimental-vm-modules --test tests/web/meters.test.mjs
// Spec 2543 B.6 and I.8: the sidebar's compact usage meters.
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
const meters = load(`${root}/meters.js`);
await meters.link((name, parent) => load(name === 'preact' ? `${root}/vendor/preact.module.js` : name === 'preact/hooks' ? `${root}/vendor/hooks.module.js` : name === 'htm' ? `${root}/vendor/htm.module.js` : resolve(dirname(parent.identifier), name)));
await meters.evaluate();
const { usageRows, macReadings } = meters.namespace;

const now = Date.parse('2026-10-27T12:00:00Z');
const hours = (h) => new Date(now + h * 3600000).toISOString();
const meter = (provider, window, percent, pace = null, resetsIn = window === 'week' ? 48 : 3, extra = {}) => ({
  provider, account_key: `${provider}-a`, label: null, window, scope: null, percent,
  resets_at: hours(resetsIn), observed_at: hours(0), pace, ...extra,
});
const row = (usage, provider) => usageRows({ meters: usage }, now).find((r) => r.provider === provider);
const shorts = (r) => r.windows.map((w) => w.short);
const G = 2 ** 30;
const host = (cpu, usedG, gpu = 5) => ({ cpu_percent: cpu, memory_used_bytes: usedG * G, memory_total_bytes: 256 * G, gpu_percent: gpu });

test('nothing to know: week windows only, no notes, Mac quiet', () => {
  const usage = [meter('claude', 'five_hour', 20, { kind: 'on_pace', percent: 40 }), meter('claude', 'week', 25, { kind: 'on_pace', percent: 55 }, 48),
    meter('codex', 'week', 6, { kind: 'on_pace', percent: 20 }, 120)];
  assert.deepEqual(shorts(row(usage, 'claude')), ['wk']);
  assert.deepEqual(row(usage, 'claude').notes, []);
  assert.deepEqual(shorts(row(usage, 'codex')), ['wk']);
  // Codex is on pace for 20% but less than half the week has gone: no note.
  assert.deepEqual(row(usage, 'codex').notes, []);
  assert.deepEqual(macReadings(host(10, 100)).readings, []);
});

test('Claude will run out: five-hour shows, amber note names the week', () => {
  const usage = [meter('claude', 'five_hour', 64, { kind: 'on_pace', percent: 90 }), meter('claude', 'week', 81, { kind: 'runs_out', at: hours(27) })];
  const claude = row(usage, 'claude');
  assert.deepEqual(shorts(claude), ['5h', 'wk']);
  assert.deepEqual(claude.windows.map((w) => w.band), ['amber', 'amber']);
  assert.equal(claude.notes.length, 1);
  assert.equal(claude.notes[0].tone, 'amber');
  assert.match(claude.notes[0].text, /^Week runs out /);
});

test('Codex will be left over: teal note once half the week has gone', () => {
  const usage = [meter('codex', 'week', 18, { kind: 'on_pace', percent: 24 }, 48)];
  const codex = row(usage, 'codex');
  assert.equal(codex.notes.length, 1);
  assert.equal(codex.notes[0].tone, 'cyan');
  assert.match(codex.notes[0].text, /^On pace for 24% · about 76% unused at reset /);
  // On pace for exactly half: no note.
  assert.deepEqual(row([meter('codex', 'week', 18, { kind: 'on_pace', percent: 50 }, 48)], 'codex').notes, []);
});

test('Mac under load: only amber or red readings, memory as used/total', () => {
  const mac = macReadings(host(100, 200));
  assert.deepEqual(mac.readings.map((r) => [r.short, r.band, r.text]), [['CPU', 'red', '100%'], ['Mem', 'amber', '200/256G']]);
  assert.match(mac.title, /CPU 100% · Memory 200\/256G · GPU 5%/);
});

test('no readings: Claude and Codex show an empty week window', () => {
  const rows = usageRows({ meters: [] }, now);
  assert.deepEqual(rows.map((r) => [r.provider, shorts(r), r.windows[0].text]), [['claude', ['wk'], '–'], ['codex', ['wk'], '–']]);
});

test('five-hour at 59% is hidden, at 60% shown', () => {
  assert.deepEqual(shorts(row([meter('claude', 'five_hour', 59), meter('claude', 'week', 10)], 'claude')), ['wk']);
  assert.deepEqual(shorts(row([meter('claude', 'five_hour', 60), meter('claude', 'week', 10)], 'claude')), ['5h', 'wk']);
});

test('a five-hour window that runs out shows with its note, below 60%', () => {
  const claude = row([meter('claude', 'five_hour', 40, { kind: 'runs_out', at: hours(1) }), meter('claude', 'week', 10)], 'claude');
  assert.deepEqual(shorts(claude), ['5h', 'wk']);
  assert.match(claude.notes[0].text, /^5-hour runs out /);
});

test('a runs-out window at 85% gets a red note', () => {
  const claude = row([meter('claude', 'week', 85, { kind: 'runs_out', at: hours(10) })], 'claude');
  assert.equal(claude.notes[0].tone, 'red');
});

test('a five-hour window on pace for 10% gets no note', () => {
  const claude = row([meter('claude', 'five_hour', 5, { kind: 'on_pace', percent: 10 }, 1), meter('claude', 'week', 10)], 'claude');
  assert.deepEqual(claude.notes, []);
});

test('no host reading: no Mac row', () => {
  assert.equal(macReadings(null), null);
  assert.equal(macReadings({ available: false }), null);
});

test('several accounts name their windows', () => {
  const usage = [meter('codex', 'week', 30, null, 48, { account_key: 'a', label: 'work@x' }),
    meter('codex', 'week', 40, null, 48, { account_key: 'b', label: 'home@x' })];
  assert.deepEqual(shorts(row(usage, 'codex')), ['wk work', 'wk home']);
});
