// Run with node --test crates/sm-server/tests/watch_client.test.cjs.
// Small DOM double: executes the shipped client and its actual event handlers.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const vm = require('node:vm');

class Element {
  constructor(attrs = {}) { this.attrs = attrs; this.listeners = {}; this.children = []; this.hidden = false; this.value = attrs.value || ''; this.type = attrs.type; this.checked = 'checked' in attrs; }
  getAttribute(key) { return this.attrs[key]; }
  addEventListener(key, handler) { this.listeners[key] = handler; }
  appendChild(child) { this.children.push(child); child.parent = this; }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter(c => c !== this); }
  contains(child) { return this === child || this.children.some(c => c.contains(child)); }
  focus() { this.focused = true; }
  closest(selector) { return selector === 'details' ? this.parent : this.matches(selector) ? this : null; }
  matches(selector) {
    if (selector === 'details[data-id]') return this.tag === 'details' && 'data-id' in this.attrs;
    if (!selector.startsWith('[')) return selector === this.tag;
    const [, key, value] = selector.match(/^\[([^=\]]+)(?:="([^"]*)")?\]$/) || [];
    return key in this.attrs && (value === undefined || this.attrs[key] === value);
  }
  querySelectorAll(selector) { return this.children.flatMap(c => [c, ...c.querySelectorAll(selector)]).filter(c => selector.split(', ').some(s => c.matches(s))); }
  querySelector(selector) { return this.querySelectorAll(selector)[0]; }
  set innerHTML(html) {
    this.html = html; this.children = [];
    for (const match of html.matchAll(/<(details|input|button|p|div|span)\b([^>]*)>/g)) {
      const attrs = {};
      for (const a of match[2].matchAll(/([\w-]+)(?:="([^"]*)")?/g)) attrs[a[1]] = a[2] || '';
      const child = new Element(attrs); child.tag = match[1]; this.appendChild(child);
    }
  }
  get innerHTML() { return this.html; }
}
const tick = () => new Promise(resolve => setImmediate(resolve));
async function harness() {
  const W = new Element(), S = new Element(), panel = new Element(), defaultsButton = new Element();
  const card = new Element(), button = new Element({'data-handoff': 'agent/a'});
  card.tag = 'details'; card.appendChild(button); W.appendChild(card);
  let policy = {enabled: true, threshold_percent: 35, source: 'default', display: 'hands off at 35%'};
  let defaults = {providers: {claude: true, 'codex-fork': false, 'codex-app': false}, threshold_percent: 35, ask_on_codex_review: true, ask_on_doc_review: true, review_floor_percent: 20, reminder_percent: 50};
  let failure = null, sessions = [{id: 'agent/a', context_percent: 28}];
  const calls = [], intervals = [];
  const fetch = async (path, opts = {}) => {
    calls.push({path, ...opts});
    if (failure && opts.method === 'PUT') return {ok: false, json: async () => ({detail: failure})};
    const body = opts.body && JSON.parse(opts.body);
    if (body && path === '/handoff-defaults') defaults = {...defaults, ...body, providers: {...defaults.providers, ...body.providers}};
    if (body && path.includes('/handoff-policy')) policy = {...policy, ...body, display: body.ask_now ? 'asked 14:02' : policy.display};
    return {ok: true, json: async () => path.startsWith('/watch/state') ? {sessions: sessions.map(s => ({...s, handoff: policy})), counts: {live: sessions.length}} : path === '/handoff-defaults' ? defaults : policy};
  };
  const document = {visibilityState: 'visible', getElementById: id => ({w: W, ws: S, 'handoff-defaults': panel, 'handoff-defaults-open': defaultsButton}[id]), createElement: () => new Element(), addEventListener() {}};
  const window = {fetch};
  vm.runInNewContext(readFileSync(require.resolve('../src/http/watch_client.js'), 'utf8'), {window, document, fetch, location: {search: ''}, setInterval: fn => intervals.push(fn), setTimeout, navigator: {}});
  return {W, card, button, panel, defaultsButton, calls, intervals, window, document, setSessions: next => sessions = next, fail: text => failure = text,
    open: async () => { W.listeners.click({target: button, preventDefault() {}}); await tick(); return card.children.find(c => c.className === 'handoff-panel'); }};
}

test('agent controls send partial updates, confirm now, refresh, and preserve edits', async () => {
  const h = await harness(), p = await h.open();
  assert.equal(p.querySelector('[data-threshold]').value, 35);
  const enabled = p.querySelector('[data-enabled]'); enabled.checked = false; enabled.onchange(); await tick();
  assert.deepEqual(JSON.parse(h.calls.find(c => c.method === 'PUT').body), {enabled: false});
  assert.ok(h.calls.some(c => c.path === '/watch/state'));
  const input = p.querySelector('[data-threshold]'); input.value = '44';
  h.document.activeElement = input;
  h.setSessions([{id: 'agent/a', context_percent: 29, state: 'working'}, {id: 'new-agent', name: 'new-agent', state: 'waiting'}]);
  h.intervals[0](); await tick(); assert.equal(input.value, '44'); assert.ok(h.W.contains(p)); assert.equal(input.focused, true);
  assert.ok(h.W.innerHTML.includes('new-agent')); assert.ok(h.W.innerHTML.includes('working'));
  input.onchange(); await tick();
  assert.deepEqual(JSON.parse(h.calls.filter(c => c.method === 'PUT').at(-1).body), {threshold_percent: 44});
  input.value = '101'; const before = h.calls.length; input.onchange(); await tick(); assert.equal(h.calls.length, before);
  p.querySelector('[data-default]').onclick(); await tick();
  assert.deepEqual(JSON.parse(h.calls.filter(c => c.method === 'PUT').at(-1).body), {use_default: true});
  const count = h.calls.length; p.querySelector('[data-now]').onclick(); await tick(); assert.equal(h.calls.length, count);
  p.querySelector('[data-yes]').onclick(); await tick();
  assert.deepEqual(JSON.parse(h.calls.filter(c => c.method === 'PUT').at(-1).body), {ask_now: true});
  assert.ok(h.W.innerHTML.includes('ctx 29% · asked 14:02'));
  h.fail('handoff policy is owner-only'); enabled.onchange(); await tick();
  assert.equal(p.querySelector('[role="status"]').textContent, 'handoff policy is owner-only');
  assert.equal(enabled.disabled, false);
  h.setSessions([]); h.intervals[0](); await tick();
  assert.equal(h.W.contains(p), false); assert.ok(h.W.innerHTML.includes('No live sessions'));
});

test('defaults expose every field and submit one field per change', async () => {
  const h = await harness(); h.defaultsButton.onclick(); await tick();
  assert.equal(h.panel.querySelectorAll('[data-field]').length, 8);
  for (const [field, value, expected] of [
    ['providers.codex-fork', true, {providers: {'codex-fork': true}}],
    ['threshold_percent', '40', {threshold_percent: 40}],
    ['ask_on_codex_review', false, {ask_on_codex_review: false}],
    ['ask_on_doc_review', false, {ask_on_doc_review: false}],
    ['review_floor_percent', '0', {review_floor_percent: 0}],
    ['reminder_percent', '60', {reminder_percent: 60}],
  ]) {
    const input = h.panel.querySelector(`[data-field="${field}"]`);
    if (input.type === 'checkbox') input.checked = value; else input.value = value;
    input.onchange(); await tick();
    const call = h.calls.filter(c => c.method === 'PUT').at(-1);
    assert.equal(call.path, '/handoff-defaults'); assert.equal(call.credentials, 'same-origin');
    assert.deepEqual(JSON.parse(call.body), expected);
  }
  const html = h.window.smWatchRender({sessions: [{id: '<bad>', name: '<script>', handoff: {display: '→ <next>'}, context_percent: 28}]});
  assert.ok(html.includes('ctx 28% · → &lt;next&gt;')); assert.ok(!html.includes('<script>'));
});
