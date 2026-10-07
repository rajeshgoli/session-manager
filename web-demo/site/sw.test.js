// node web-demo/site/sw.test.js — the worker's pure helpers.
'use strict';
const assert = require('assert');
globalThis.self = { addEventListener() {}, location: { origin: 'http://demo' } };
const { shiftTimes, parseTime, normalize } = require('./sw.js');

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
console.log('ok');
