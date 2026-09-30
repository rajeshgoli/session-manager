const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const source = fs.readFileSync('crates/sm-server/src/web/app.js', 'utf8');
const urlForSource = source.slice(source.indexOf('function urlFor('), source.indexOf('// ---- layout ('));

test('navigation drops open details, while opening details retains the current page', () => {
  const context = vm.createContext({ location: { pathname: '/queue', search: '?open=job%3Aold&at=question-1' }, URLSearchParams });
  vm.runInContext(`${urlForSource}\nthis.urlFor = urlFor;`, context);
  assert.equal(context.urlFor('/queue', null), '/queue');
  assert.equal(context.urlFor('/board', null), '/board');
  assert.equal(context.urlFor('/queue', 'job:new'), '/queue?open=job:new');
});
