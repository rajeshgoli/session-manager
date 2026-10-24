import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

test('doc reader marks each head check as background without marking other requests', () => {
  const source = readFileSync('crates/sm-server/src/http/doc_client.js', 'utf8');
  const api = source.slice(source.indexOf('  function api('), source.indexOf('  function revision('));
  const pollHead = source.slice(source.indexOf('  function pollHead('), source.indexOf('  // ---- inline bubbles'));
  const calls = [];
  const context = {
    CONFIG: {docId: '0123abcd', sha: 'abc123'},
    MSG: false,
    document: {visibilityState: 'visible'},
    S: {},
    agentPanel: {hidden: true},
    renderBar() {},
    renderBanner() {},
    fetch(url, options) {
      calls.push({url, options});
      return Promise.resolve({ok: true, text: () => Promise.resolve('{}')});
    }
  };
  vm.runInNewContext(`${api}\n${pollHead}\npollHead();`, context);
  assert.equal(calls[0].url, '/docs/0123abcd/head?sha=abc123');
  assert.equal(calls[0].options.headers['x-sm-bg'], '1');

  vm.runInNewContext("api('GET', '/view');", context);
  assert.equal(calls[1].url, '/docs/0123abcd/view');
  assert.equal(calls[1].options.headers['x-sm-bg'], undefined);
});
