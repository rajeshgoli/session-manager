// Run: node --experimental-vm-modules --test tests/web/agents.test.mjs
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
const agents = load(`${root}/agents.js`);
await agents.link((name, parent) => load(name === 'preact' ? `${root}/vendor/preact.module.js` : name === 'preact/hooks' ? `${root}/vendor/hooks.module.js` : name === 'htm' ? `${root}/vendor/htm.module.js` : resolve(dirname(parent.identifier), name)));
await agents.evaluate();
const { sectionAgents, foldIdle, foldText, summaryCounts, agentFact, jobsFact, youFact, edgeTone, groupAgents } = agents.namespace;

// Spec 1782 appendix B's worked examples, as GET /watch/state draws them at 19:47 UTC.
const now = Date.parse('2026-09-30T19:47:00Z');
const noJobs = { running: 0, waiting: 0, text: 'No jobs', tone: null };
const agent = (name, section, key, facts = {}, extra = {}) => ({
  id: name, name, state: 'idle', repo: '/Users/r/projects/session-manager', claims: [],
  attention: { section, reason: null, order_key: key },
  facts: { agent: { state: 'idle', since: '2026-09-30T19:40:00Z' }, jobs: noJobs, you: null, finished: null, ...facts },
  ...extra,
});
const fixtures = {
  engineer: agent('sm-1726-engineer', 'you', '2026-09-30T19:40:12Z', {
    agent: { state: 'idle', since: '2026-09-30T19:40:16Z' },
    you: { kind: 'message', since: '2026-09-30T19:40:12Z', text: '1771 is waiting for one manual Chrome check…', more: 0, dismissible: true },
  }),
  run: agent('iter8-run', 'moving', '8240469479', {
    agent: { state: 'idle', since: '2026-09-30T19:42:42Z' },
    jobs: { running: 2, waiting: 0, text: '2 running · 2h 56m', tone: 'green' },
  }),
  working: agent('sm-1776', 'moving', '8240469100', { agent: { state: 'working', since: '2026-09-30T19:46:00Z' } }, { state: 'working' }),
  far: agent('far-1855', 'finished', '8240483139', {
    agent: { state: 'idle', since: '2026-09-30T15:01:43Z' },
    finished: { at: '2026-09-30T15:01:00Z', text: '1855 done and closed: 68 views built', read: false },
  }),
  idle: agent('sm-1768', 'idle', '18240469600', { agent: { state: 'idle', since: '2026-09-30T19:36:39Z' } }),
  askWhileWorking: agent('asks-while-working', 'you', '2026-09-30T19:35:00Z', {
    agent: { state: 'working', since: '2026-09-30T19:44:00Z' },
    you: { kind: 'message', since: '2026-09-30T19:35:00Z', text: 'Merge it?', more: 1, dismissible: true },
  }),
  queued: agent('waits-third', 'waiting_long', '2026-09-30T18:45:00Z', {
    jobs: { running: 0, waiting: 1, text: 'Waiting 1h 2m · 3rd in line', tone: 'amber' },
  }, { attention: { section: 'waiting_long', reason: 'queue_wait', order_key: '2026-09-30T18:45:00Z' } }),
  review: agent('waits-review', 'waiting', '2026-09-30T19:35:00Z', {
    jobs: { running: 0, waiting: 0, review: { pr_number: 1790 }, text: 'Codex review on PR #1790 · 12m', tone: 'amber' },
  }),
  stalled: agent('stalled-34m', 'waiting_long', '2026-09-30T19:13:00Z', {}, {
    claims: [{ kind: 'ticket', number: 1700 }],
    attention: { section: 'waiting_long', reason: 'stalled', order_key: '2026-09-30T19:13:00Z' },
  }),
  docReview: agent('doc-review', 'you', '2026-09-30T19:30:00Z', {
    you: { kind: 'doc_review', since: '2026-09-30T19:30:00Z', text: 'Review: Fit and finish', more: 0, dismissible: false },
  }),
};

test('sections follow attention order, then order key, then name, and skip empty sections', () => {
  const sections = sectionAgents(Object.values(fixtures));
  assert.deepEqual(sections.map((s) => s.section), ['you', 'finished', 'waiting_long', 'moving', 'waiting', 'idle']);
  assert.deepEqual(sections[0].agents.map((a) => a.name), ['doc-review', 'asks-while-working', 'sm-1726-engineer']);
  assert.deepEqual(sections[2].agents.map((a) => a.name), ['waits-third', 'stalled-34m']);
  assert.deepEqual(sections[3].agents.map((a) => a.name), ['sm-1776', 'iter8-run']);
});

test('three facts for the overlaps Rajesh listed', () => {
  assert.deepEqual(agentFact(fixtures.engineer, now), { text: '○ Idle 6m', tone: 'muted' });
  assert.deepEqual(jobsFact(fixtures.engineer), { text: 'No jobs', tone: 'muted' });
  assert.deepEqual(youFact(fixtures.engineer, now), { text: '◆ 6m: 1771 is waiting for one manual Chrome check…', tone: 'magenta', dismissible: true });
  // Working and still asking: both show.
  assert.equal(agentFact(fixtures.askWhileWorking, now).text, '● Working 3m');
  assert.equal(youFact(fixtures.askWhileWorking, now).text, '◆ 12m: Merge it? +1');
  assert.deepEqual(jobsFact(fixtures.run), { text: '▶ 2 running · 2h 56m', tone: 'green' });
  assert.deepEqual(jobsFact(fixtures.queued), { text: '⏸ Waiting 1h 2m · 3rd in line', tone: 'amber' });
  assert.deepEqual(jobsFact(fixtures.review), { text: '⏸ Codex review on PR #1790 · 12m', tone: 'amber' });
  assert.equal(youFact(fixtures.run, now), null);
  // Finished shows only without a question; a doc review cannot be dismissed.
  assert.deepEqual(youFact(fixtures.far, now), { text: '✔ 1855 done and closed: 68 views built', tone: 'cyan', dismissible: false });
  const finishing = { ...fixtures.far, facts: { ...fixtures.far.facts, finished: { at: '2026-09-30T15:01:00Z', text: null } } };
  assert.equal(youFact(finishing, now).text, '✔ Finishing…');
  assert.equal(youFact(fixtures.docReview, now).dismissible, false);
  const quiet = { ...fixtures.run, facts: { ...fixtures.run.facts, jobs: { running: 1, waiting: 0, quiet: true, text: 'Quiet 40m: 1858-run', tone: 'red' } } };
  assert.deepEqual(jobsFact(quiet), { text: '▶ Quiet 40m: 1858-run', tone: 'red' });
});

test('card edges use the section colour, red for stalled or quiet, the line colour when idle', () => {
  assert.equal(edgeTone(fixtures.engineer), 'magenta');
  assert.equal(edgeTone(fixtures.far), 'cyan');
  assert.equal(edgeTone(fixtures.queued), 'amber');
  assert.equal(edgeTone(fixtures.stalled), 'red');
  assert.equal(edgeTone(fixtures.run), 'green');
  assert.equal(edgeTone(fixtures.idle), 'line');
});

test('only idle agents without a ticket beyond the fourth fold', () => {
  const loose = Array.from({ length: 6 }, (_, i) => agent(`idle-${i}`, 'idle', `1${i}`, { agent: { state: 'idle', since: '2026-09-30T19:36:00Z' } }));
  const ticketed = agent('holds-ticket', 'idle', '0', {}, { claims: [{ kind: 'ticket', number: 1 }] });
  const { shown, folded } = foldIdle([ticketed, ...loose]);
  assert.deepEqual(shown.map((a) => a.name), ['holds-ticket', 'idle-0', 'idle-1', 'idle-2', 'idle-3']);
  assert.deepEqual(folded.map((a) => a.name), ['idle-4', 'idle-5']);
  assert.equal(foldText(folded, now), '+ 2 more idle: idle-4 (11m), idle-5 (11m)');
  assert.equal(foldIdle(loose.slice(0, 4)).folded.length, 0);
});

test('the summary strip drops zero counts except needs you', () => {
  assert.deepEqual(summaryCounts({ needs_you: 0, finished: 1, waiting_long: 0, moving: 3, waiting: 0, idle: 5 }).map((c) => `${c.n} ${c.label}`),
    ['0 needs you', '1 finished', '3 moving', '5 idle']);
  assert.deepEqual(summaryCounts({ needs_you: 2, waiting_long: 1, waiting: 4 }).map((c) => c.section), ['you', 'waiting_long', 'waiting']);
});

test('By repo puts busy repos first and children after their parent', () => {
  const parent = agent('parent', 'idle', '1', {}, { repo: '/r/b' });
  const child = agent('child', 'moving', '1', {}, { repo: '/r/b', parent_session_id: 'parent' });
  const other = agent('other', 'idle', '1', {}, { repo: '/r/a' });
  const groups = groupAgents([other, child, parent]);
  assert.deepEqual(groups.map((g) => g.repo), ['/r/b', '/r/a']);
  assert.deepEqual(groups[0].agents.map((e) => [e.agent.name, e.depth]), [['parent', 0], ['child', 1]]);
});
