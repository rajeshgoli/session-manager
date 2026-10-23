import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fileMatches, fileHref, installFileLinks } from '../../crates/sm-server/src/web/terminal-files.js';

test('paths, line suffixes, punctuation and URL boundaries', () => {
  const text = 'Read `/Users/me/memo.html`, ~/notes.md ./src/main.rs:42:3 (src/lib.rs:9). https://example.com/path/file.rs';
  assert.deepEqual(fileMatches(text).map(m => m.path), ['/Users/me/memo.html', '~/notes.md', './src/main.rs', 'src/lib.rs']);
  for (const match of fileMatches(text)) assert.equal(text.slice(match.index, match.index + match.text.length), match.text);
  assert.equal(fileHref('a/b', '/tmp/a#b.txt'), '/client/terminal/file?session=a%2Fb&path=%2Ftmp%2Fa%23b.txt');
});

test('wrapped xterm links use terminal cells, including wide and astral characters', () => {
  const rows = [
    [{ chars: '😀', width: 2 }, { chars: '', width: 0 }, { chars: ' ', width: 1 }, ...Array.from('/tmp/long', chars => ({ chars, width: 1 }))],
    Array.from('-file.txt ', chars => ({ chars, width: 1 })),
  ];
  const lines = rows.map((cells, row) => ({ length: cells.length, isWrapped: row === 1, getCell: col => ({ getChars: () => cells[col].chars, getWidth: () => cells[col].width }) }));
  let provider;
  const terminal = { buffer: { active: { length: lines.length, getLine: row => lines[row] } }, registerLinkProvider: value => { provider = value; } };
  let opened;
  installFileLinks(terminal, 'agent', (_, href) => { opened = href; });
  for (const row of [1, 2]) provider.provideLinks(row, links => {
    assert.equal(links.length, 1);
    assert.equal(links[0].text, '/tmp/long-file.txt');
    assert.deepEqual(links[0].range, { start: { x: 4, y: 1 }, end: { x: 9, y: 2 } });
    links[0].activate({});
  });
  assert.equal(opened, fileHref('agent', '/tmp/long-file.txt'));
});

test('a wide character wrapping from the final column does not split the path', () => {
  const rows = [
    [...Array.from('/tmp/a', chars => ({ chars, width: 1 })), { chars: '', width: 1 }],
    [{ chars: '界', width: 2 }, { chars: '', width: 0 }, ...Array.from('b.txt', chars => ({ chars, width: 1 }))],
  ];
  const lines = rows.map((cells, row) => ({ length: cells.length, isWrapped: row === 1, getCell: col => ({ getChars: () => cells[col].chars, getWidth: () => cells[col].width }) }));
  let provider;
  installFileLinks({ buffer: { active: { length: 2, getLine: row => lines[row] } }, registerLinkProvider: value => { provider = value; } }, 'agent', () => {});
  for (const row of [1, 2]) provider.provideLinks(row, links => {
    assert.equal(links.length, 1);
    assert.equal(links[0].text, '/tmp/a界b.txt');
    assert.deepEqual(links[0].range, { start: { x: 1, y: 1 }, end: { x: 7, y: 2 } });
  });
});
