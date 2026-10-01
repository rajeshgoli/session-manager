const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const source = fs.readFileSync('crates/sm-server/src/web/app.js', 'utf8');
const readLocationSource = source.slice(source.indexOf('function readLocation('), source.indexOf('function urlFor('));
const urlForSource = source.slice(source.indexOf('function urlFor('), source.indexOf('// ---- layout ('));

test('navigation drops open details, while opening details retains the current page', () => {
  const context = vm.createContext({ location: { pathname: '/queue', search: '?open=job%3Aold&at=question-1' }, URLSearchParams });
  vm.runInContext(`${urlForSource}\nthis.urlFor = urlFor;`, context);
  assert.equal(context.urlFor('/queue', null), '/queue');
  assert.equal(context.urlFor('/board', null), '/board');
  assert.equal(context.urlFor('/queue', 'job:new'), '/queue?open=job:new');
});

test('History opens on agents, and an older ticket-filter link lands on the tickets tab', () => {
  const visit = (pathname, search) => {
    const location = { pathname, search, hash: '' };
    const history = { state: null, replaceState(state, title, url) { const next = new URL(url, 'http://sm'); location.pathname = next.pathname; location.search = next.search; } };
    const context = vm.createContext({ location, history, URLSearchParams, URL });
    vm.runInContext(`${readLocationSource}\n${urlForSource}\nthis.readLocation = readLocation; this.urlFor = urlFor;`, context);
    return context;
  };
  assert.equal(visit('/history', '').readLocation().path, '/history');
  assert.equal(visit('/history', '?open=agent:abc').readLocation().open, 'agent:abc');
  for (const search of ['?repo=owner%2Frepo', '?agent=sm-1', '?open=1&limit=7']) {
    const context = visit('/history', search);
    assert.equal(context.readLocation().path, '/history/tickets', search);
    assert.equal(context.location.search, search);
  }
  // The tickets tab keeps `open` as its filter and puts panels in `panel`.
  const tickets = visit('/history/tickets', '?open=1');
  assert.equal(tickets.urlFor('/history/tickets', 'ticket:o/r#1'), '/history/tickets?open=1&panel=ticket:o%2Fr%231');
});
