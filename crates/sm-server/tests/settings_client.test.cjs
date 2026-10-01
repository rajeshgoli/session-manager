// Execute shipped Settings logic and field handlers with a small hooks double.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
function harness() {
  const slots = [], effects = [];
  let index = 0;
  const context = vm.createContext({
    useState(initial) { const i = index++; if (!(i in slots)) slots[i] = typeof initial === 'function' ? initial() : initial;
      return [slots[i], next => { slots[i] = typeof next === 'function' ? next(slots[i]) : next; }]; },
    useRef(initial) { const i = index++; return slots[i] ||= { current: initial }; },
    useEffect(fn, deps) { const i = index++; if (!slots[i] || deps.some((d, n) => d !== slots[i][n])) effects.push(fn); slots[i] = deps; },
    html: (strings, ...values) => ({ strings, values }), console,
  });
  const source = fs.readFileSync('crates/sm-server/src/web/settings.js', 'utf8')
    .replace(/^import .*;\n/gm, '').replace(/export function /g, 'function ');
  vm.runInContext(source, context);
  const evaluate = expression => vm.runInContext(expression, context);
  const render = props => { index = 0; const tree = evaluate('Field')(props); effects.splice(0).forEach(fn => fn()); return tree; };
  const objects = tree => tree && typeof tree === 'object' ? [tree, ...Object.values(tree).flatMap(objects)] : [];
  return { evaluate, render, input: tree => objects(tree).find(v => v.class === 'inp'),
    text: tree => objects(tree).flatMap(v => v.values || []).filter(v => typeof v === 'string').join(' '), slots };
}

test('preview takes first Ready board ticket and applies server name normalization', () => {
  const h = harness();
  const ticket = h.evaluate('sampleTicket')({ lanes: [{ tickets: [{ state: 'blocked', number: 1 }, { state: 'ready', repo: 'owner/repo', number: 8, title: 'A $& title', url: 'https://example.com/8' }] }], other: [] });
  const result = h.evaluate('preview')({ repo_short: { 'owner/repo': 'xy' }, name_pattern: '{repo_short} {number}__{title}', message_template: '{ticket} {repo} {repo_name} {url} {title}' }, ticket);
  assert.equal(result.name, 'xy-8-a-title');
  assert.equal(result.message, '#8 owner/repo repo https://example.com/8 A $& title');
  assert.equal(h.evaluate('sampleTicket')({}).number, 1706);
});

test('field saves on blur, retains rejected draft and allows retry', async () => {
  const h = harness(), calls = [];
  let fail = true;
  const props = { label: 'Name', initial: 'old', save: async value => { calls.push(value); if (fail) throw Error('Rejected'); } };
  let tree = h.render(props);
  h.input(tree).onInput({ target: { value: 'draft' } });
  assert.equal(calls.length, 0);
  tree = h.render(props);
  await h.input(tree).onBlur();
  tree = h.render(props);
  assert.equal(h.input(tree).value, 'draft');
  assert.match(h.text(tree), /Rejected/);
  fail = false;
  await h.input(tree).onBlur();
  assert.match(h.text(h.render(props)), /Saved/);
  await h.input(h.render(props)).onBlur();
  assert.deepEqual(calls, ['draft', 'draft']);
});

test('late save does not mark a newer draft as saved or replace it', async () => {
  const h = harness(); let finish;
  const props = { initial: 'old', save: () => new Promise(resolve => { finish = resolve; }) };
  h.input(h.render(props)).onInput({ target: { value: 'first' } });
  const pending = h.input(h.render(props)).onBlur();
  h.input(h.render(props)).onInput({ target: { value: 'second' } });
  finish(); await pending;
  const tree = h.render({ ...props, initial: 'first' });
  assert.equal(h.input(tree).value, 'second');
  assert.doesNotMatch(h.text(tree), /Saved/);
});

test('returning to a pristine field adopts a save that completed during navigation', () => {
  const h = harness(); h.render({ initial: 'old' });
  h.render({ initial: 'new' });
  assert.equal(h.input(h.render({ initial: 'new' })).value, 'new');
});

test('queue reset clears override and invalid slot counts cannot be submitted', () => {
  const h = harness(), parse = h.evaluate('integerLimit');
  assert.equal(parse(''), null); assert.equal(parse('0'), 0); assert.equal(parse('16'), 16);
  for (const value of ['-1', '17', '1.5', 'word']) assert.throws(() => parse(value));
  assert.throws(() => h.evaluate('shortNames')('owner/repo = a\nowner/repo = b'));
});

test('reverting to the old value while a save is pending still submits the revert', async () => {
  const h = harness(), calls = [], finishes = [];
  const props = { initial: 'old', save: value => { calls.push(value); return new Promise(resolve => finishes.push(resolve)); } };
  h.input(h.render(props)).onInput({ target: { value: 'new' } });
  const first = h.input(h.render(props)).onBlur();
  h.input(h.render(props)).onInput({ target: { value: 'old' } });
  const second = h.input(h.render(props)).onBlur();
  assert.deepEqual(calls, ['new', 'old']);
  finishes[0](); await first;
  finishes[1](); await second;
  assert.match(h.text(h.render(props)), /Saved/);
  assert.equal(h.input(h.render(props)).value, 'old');
});

test('handoff settings list only the providers with a context gauge', () => {
  const providers = harness().evaluate('HANDOFF_PROVIDERS.map(([provider]) => provider).join()');
  assert.equal(providers, 'claude,codex-fork');
  assert.equal(harness().evaluate('THRESHOLDS.map(([key]) => key).join()'), 'threshold_percent,reminder_percent,review_floor_percent');
});

test('terminal limits accept blank as reset and refuse values outside the range', () => {
  const h = harness();
  const limit = h.evaluate('terminalLimit');
  assert.equal(limit('', 1, 256), null);
  assert.equal(limit('100', 1, 256), 100);
  assert.throws(() => limit('0', 1, 256), /from 1 to 256/);
  assert.throws(() => limit('1.5', 60, 86400), /from 60 to 86400/);
  assert.equal(h.evaluate("TERMINAL_LIMITS.map(([key]) => key).join()"), 'per_user,per_session,global,max_attach_seconds');
});

test('Reviews sits after Context handoff and its meter limit refuses values outside 50 to 100', () => {
  const h = harness();
  assert.equal(h.evaluate("SECTIONS.map(([id]) => id).slice(2, 4).join()"), 'context-handoff,reviews');
  const percent = h.evaluate('meterPercent');
  assert.equal(percent('95'), 95); assert.equal(percent('100'), 100);
  for (const value of ['', '49', '101', '90.5', 'word']) assert.throws(() => percent(value), /from 50 to 100/);
  assert.equal(h.evaluate('meterText')(63.6), '64%');
  assert.equal(h.evaluate('meterText')(null), '—');
});

test('policy rows list every board repo, then stored lane and ticket policies', () => {
  const rows = harness().evaluate('policyRows')({ policies: [
    { scope: 'repo', repo: 'o/b', number: 0, reviewer: { kind: 'github_codex' } },
    { scope: 'ticket', repo: 'o/a', number: 1848, reviewer: { kind: 'claude', model: 'fable', effort: 'max' } },
    { scope: 'lane', repo: 'o/a', number: 1843, reviewer: { kind: 'codex', model: 'gpt-6-astra', effort: 'high' } },
  ] }, ['o/a', 'o/b']);
  assert.deepEqual(Array.from(rows, r => `${r.scope} ${r.repo} ${r.number} ${r.policy ? 'set' : 'default'}`),
    ['repo o/a 0 default', 'repo o/b 0 set', 'ticket o/a 1848 set', 'lane o/a 1843 set']);
});

test('agent types need distinct names and a model before Save', () => {
  const problem = harness().evaluate('typeProblem');
  const row = (name, model = 'sonnet') => ({ name, provider: 'claude', model, effort: 'high' });
  assert.equal(problem([row('Top'), row('Low')]), '');
  assert.equal(problem([row('Top'), row(' ')]), 'Give every agent type a name.');
  assert.equal(problem([row('Mid'), row('mid ')]), 'Agent type names must be different.');
  assert.equal(problem([row('Mid', '  ')]), 'Give every agent type a model.');
});

test('auto-retire minutes accept 15 to 1440 whole minutes only', () => {
  const { evaluate } = harness();
  const minutes = evaluate('retireMinutes');
  assert.equal(minutes('60'), 60);
  assert.equal(minutes('15'), 15);
  assert.equal(minutes('1440'), 1440);
  for (const bad of ['', '14', '1441', '30.5', 'soon']) {
    assert.throws(() => minutes(bad), /Enter a whole number from 15 to 1440/);
  }
});
