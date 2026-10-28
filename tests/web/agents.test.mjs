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
const { sectionAgents, agentFact, jobsFact, youFact, edgeTone, canRetireImmediately, ticketTitle } = agents.namespace;

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
  assert.deepEqual(youFact(fixtures.far, now), { text: '✔ 1855 done and closed: 68 views built', tone: 'cyan', dismissible: true });
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

test('shells stay out of agent sections', () => {
  const idle = agent('terminal', 'idle', '1', {}, { kind: 'shell' });
  const busy = agent('busy-terminal', 'moving', '2', {}, { kind: 'shell', state: 'working' });
  assert.deepEqual(sectionAgents([fixtures.idle, idle, busy]).map(group => group.agents.map(a => a.name)), [[fixtures.idle.name]]);
});

test('only a finished idle agent retires without confirmation', () => {
  assert.equal(canRetireImmediately(fixtures.far), true);
  assert.equal(canRetireImmediately(fixtures.idle), false);
  assert.equal(canRetireImmediately(fixtures.working), false);
  assert.equal(canRetireImmediately({ ...fixtures.far, facts: {
    ...fixtures.far.facts, agent: { state: 'working' },
  } }), false);
});

test('the card title is the title of the claim the card numbers (sm#1900)', () => {
  const claim = (kind, number, title) => ({ kind, number, title, repo: 'rajeshgoli/session-manager' });
  assert.equal(ticketTitle({ claims: [claim('ticket', 1900, 'Show ticket titles next to agent names')] }),
    'Show ticket titles next to agent names');
  assert.equal(ticketTitle({ claims: [claim('pr', 1901, 'Ticket titles on agent cards'), claim('ticket', 1900, 'Other')] }),
    'Ticket titles on agent cards');
  assert.equal(ticketTitle({ claims: [claim('ticket', 1900, '  ')] }), '');
  assert.equal(ticketTitle({ claims: [] }), '');
  assert.equal(ticketTitle({}), '');
});


test('agent details retain completed tickets and PRs without duplicate claim links', () => {
  const ticket = { repo: 'rajeshgoli/session-manager', kind: 'ticket', number: 2071 };
  const pr = { repo: ticket.repo, kind: 'pr', number: 2072, ended_at: '2026-10-07', state: 'merged', url: 'https://github.com/rajeshgoli/session-manager/pull/2072' };
  const old = { ...ticket, ended_at: '2026-10-06', state: 'closed' };
  const active = { ...ticket, ended_at: null, state: 'open' };
  const history = [old, pr, active];
  assert.deepEqual(agents.namespace.detailClaims([active], history), [active, pr]);
  assert.deepEqual(agents.namespace.detailClaims([], [old, pr]), [pr, old]);
  assert.deepEqual(history, [old, pr, active]);
  assert.equal(agents.namespace.detailClaims([], [old, { ...old, repo: 'rajeshgoli/other' }]).length, 2);
  assert.deepEqual(agents.namespace.detailClaims(), []);
});

test("a blocked agent's You line is red (sm#2494)", () => {
  const fact = youFact({ facts: { you: { kind: 'blocked', since: new Date(Date.now() - 5 * 60000).toISOString(), text: 'Log in to GitHub', more: 0, dismissible: true } } });
  assert.equal(fact.tone, 'red');
  assert.match(fact.text, /^◆ Blocked · 5m: Log in to GitHub$/);
});
