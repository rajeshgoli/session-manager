// Terminal link finding on a fake xterm buffer (#2580): URLs and file paths that
// Claude Code or Codex split across rows themselves, with no xterm wrap flag.
// Run with node --test.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { installFileLinks, linkMatches } = await import(new URL('../crates/sm-server/src/web/terminal-files.js', import.meta.url));

// rows: strings no wider than cols, or [string, true] for a row xterm wrapped itself.
function terminal(cols, rows) {
  const lines = rows.map(row => {
    const [text, isWrapped] = Array.isArray(row) ? row : [row, false];
    const chars = [...text.padEnd(cols)];
    return { isWrapped, length: cols, getCell: col => ({ getChars: () => (chars[col] === ' ' ? '' : chars[col]), getWidth: () => 1 }) };
  });
  let provider;
  return {
    cols,
    buffer: { active: { length: lines.length, getLine: i => lines[i] } },
    registerLinkProvider(p) { provider = p; },
    links(y) { let out; provider.provideLinks(y, links => { out = links; }); return out; },
  };
}

function linksAt(cols, rows, y) {
  const t = terminal(cols, rows);
  const opened = [];
  installFileLinks(t, 'agent', { url: (_, uri) => opened.push(uri), file: (_, uri) => opened.push(uri) });
  return t.links(y).map(link => { link.activate(); return { text: link.text, range: link.range, opened: opened.at(-1) }; });
}

const COLS = 40;
const fill = (prefix, word) => [prefix + word.slice(0, COLS - prefix.length), word.slice(COLS - prefix.length)];

test('a URL Claude Code split at the right edge is one link on both rows', () => {
  const url = 'https://github.com/rajeshgoli/session-manager/pull/2576/files';
  const [first, rest] = fill('⏺ See ', url);
  const rows = [first, `  ${rest} for it`];
  for (const y of [1, 2]) {
    const links = linksAt(COLS, rows, y);
    assert.equal(links.length, 1, `row ${y}`);
    assert.equal(links[0].text, url);
    assert.equal(links[0].opened, url);
    assert.deepEqual(links[0].range, { start: { x: 7, y: 1 }, end: { x: 2 + rest.length, y: 2 } });
  }
});

test('a path split over three rows, Codex indent, opens the whole file', () => {
  const path = '/Users/rajesh/worktrees/session-manager-2580/crates/sm-server/src/web/terminal-files.js';
  const rows = ['• Edited ' + path.slice(0, 31), '    ' + path.slice(31, 67), '    ' + path.slice(67) + ' (+12 -3)'];
  const links = linksAt(COLS, rows, 2);
  assert.equal(links.length, 1);
  assert.equal(links[0].text, path);
  assert.equal(links[0].opened, `/client/terminal/file?session=agent&path=${encodeURIComponent(path)}`);
});

test('a short link that merely ends at the edge does not swallow the next word', () => {
  const padded = ['⏺ The fix is in the crates/web/termin.js', '  and the test covers it'];
  assert.equal(padded[0].length, COLS);
  const links = linksAt(COLS, padded, 1);
  assert.deepEqual(links.map(l => l.text), ['crates/web/termin.js']);
  assert.deepEqual(linksAt(COLS, padded, 2), []);
});

test('rows xterm wrapped itself still join, and a row short of the edge never joins', () => {
  const url = 'https://example.com/a/very/long/path/that/wraps';
  const wrapped = [url.slice(0, COLS), [url.slice(COLS), true]];
  assert.deepEqual(linksAt(COLS, wrapped, 2).map(l => l.text), [url]);
  const short = ['see https://example.com/a', '  b/c'];
  assert.deepEqual(linksAt(COLS, short, 1).map(l => l.text), ['https://example.com/a']);
});

test('a file-looking piece of a URL is part of the URL, not a second link', () => {
  assert.deepEqual(linkMatches('open https://example.com/docs/memo.html now').map(m => m.text), ['https://example.com/docs/memo.html']);
});
