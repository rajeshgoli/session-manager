// node web-demo/site/sw.test.js — the worker's pure helpers.
'use strict';
const assert = require('assert');
globalThis.self = { addEventListener() {}, location: { origin: 'http://demo' } };
const { shiftTimes, parseTime, normalize, markdown, noteTitle, snippet, historyFilter } = require('./sw.js');

const hour = 3600 * 1000;
assert.strictEqual(shiftTimes('"2026-10-07T01:46:17.685293Z"', hour), '"2026-10-07T02:46:17.685293Z"');
assert.strictEqual(shiftTimes('2026-10-07T23:59:59Z', 1000), '2026-10-08T00:00:00Z');
assert.strictEqual(shiftTimes('2026-10-07T01:00:00.999Z', 2), '2026-10-07T01:00:01.001Z');
assert.strictEqual(shiftTimes('2026-10-07T01:00:00.12345Z', -125), '2026-10-07T00:59:59.99845Z');
assert.strictEqual(shiftTimes('at 2026-10-07 01:46:17 ok', hour), 'at 2026-10-07 02:46:17 ok');
// Not a recorded format: left alone.
assert.strictEqual(shiftTimes('2026-10-07T01:46:17+00:00', hour), '2026-10-07T01:46:17+00:00');
assert.strictEqual(parseTime('2026-10-07T01:46:17.685293Z'), Date.UTC(2026, 9, 7, 1, 46, 17, 685));
assert.strictEqual(normalize('/inbox?format=json&filter=open'), normalize('/inbox?filter=open&format=json'));
assert.strictEqual(normalize('/docs/a.html?version=abc&from=%2Finbox'), '/docs/a.html?version=abc');
assert.strictEqual(normalize('/guestbook?format=json&repo=&before='), '/guestbook?before=&format=json&repo=');
// Notes: title and search snippet as notes.rs makes them; the preview's Markdown.
assert.strictEqual(noteTitle('## Plan  \nbody'), 'Plan');
assert.deepStrictEqual(snippet('Coupons do not stack', 'STACK'), { snippet: 'Coupons do not stack', matches: [{ start: 15, end: 20 }] });
assert.strictEqual(markdown('# Hi\n\n- [x] **done**\n- `code`\n\n<b>x</b>'),
  '<h1>Hi</h1>\n<ul>\n<li><input type="checkbox" disabled checked> <strong>done</strong></li>\n<li><code>code</code></li>\n</ul>\n<p>&lt;b&gt;x&lt;/b&gt;</p>');
// History search and repo filter run over the recorded unfiltered pages.
const agentsUrl = new URL('http://demo/history/agents?format=json&q=Cart&before=');
const agents = historyFilter(agentsUrl);
assert.strictEqual(normalize(agents.base.pathname + agents.base.search), normalize('/history/agents?format=json&q=&before='));
assert.deepStrictEqual(agents.apply({ agents: [{ id: 'a1', name: 'cart-31' }, { id: 'a2', name: 'scout' }], total: 2, next_before: null }),
  { agents: [{ id: 'a1', name: 'cart-31' }], total: 1, next_before: null });
assert.strictEqual(historyFilter(new URL('http://demo/history/agents?format=json&q=&before=')), null);
const tickets = historyFilter(new URL('http://demo/history?format=json&repo=acme/shop&before='));
assert.deepStrictEqual(tickets.apply({ rows: [{ repo: 'acme/shop' }, { repo: 'acme/web' }], next_before: null }).rows, [{ repo: 'acme/shop' }]);
console.log('ok');
