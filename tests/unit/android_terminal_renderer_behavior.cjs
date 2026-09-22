// Run with node --test. Exercise the shipped xterm parser without a browser.
const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
global.self = global;
const asset = path.resolve(__dirname, '../../android-app/app/src/main/assets/sm_terminal');
const { Terminal } = require(path.join(asset, 'vendor/xterm.js'));
const { FitAddon } = require(path.join(asset, 'vendor/addon-fit.js'));
const source = fs.readFileSync(path.join(asset, 'terminal.html'), 'utf8').match(/<script>([\s\S]*?)<\/script>/)[1];

function renderer() {
  const acks = [];
  const element = { style: {}, classList: { toggle() {}, contains() { return false; } } };
  const context = vm.createContext({
    Terminal, FitAddon: { FitAddon }, Uint8Array, atob,
    document: { getElementById: () => element },
    window: { TerminalBridge: { written: seq => acks.push(Number(seq)) }, addEventListener() {}, requestAnimationFrame() {} },
  });
  vm.runInContext(source, context);
  vm.runInContext('ready = true; opened = true;', context);
  const term = vm.runInContext('term', context);
  term.resize(40, 8);
  return { context, term, acks, api: context.window };
}

const drain = term => new Promise(resolve => term.write('', resolve));

test('duplicate bridge deliveries do not replay text or cursor movement', async () => {
  const r = renderer();
  try {
    r.api.smWriteText(1, 'first\r\n');
    r.api.smWriteBase64(2, Buffer.from('second\r\n').toString('base64'));
    r.api.smWriteText(1, 'first\r\n');
    r.api.smWriteBase64(2, Buffer.from('second\r\n').toString('base64'));
    await drain(r.term);
    assert.equal(r.term.buffer.active.cursorY, 2);
    assert.deepEqual(r.acks, [1, 2]);
  } finally { r.term.dispose(); }
});

test('status updates preserve scroll position and do not refit or focus', async () => {
  const r = renderer();
  try {
    r.api.smWriteText(1, Array.from({length: 100}, (_, i) => `line ${i}\r\n`).join(''));
    await drain(r.term);
    r.term.scrollToLine(15);
    vm.runInContext('fitAndReport = () => { throw new Error("unexpected fit"); };', r.context);
    for (let i = 0; i < 100; i++) r.api.smSetStatus('attached');
    r.api.smWriteText(2, 'new output\r\n');
    await drain(r.term);
    assert.equal(r.term.buffer.active.viewportY, 15);
    assert.equal(r.term.buffer.active.getLine(15).translateToString(true), 'line 15');
  } finally { r.term.dispose(); }
});

test('long redraws and split UTF-8 survive a burst of ordered frames', async () => {
  const r = renderer();
  try {
    const bytes = Buffer.from('\x1b[2J\x1b[H' + 'old line\r\n'.repeat(700) + '\x1b[H\x1b[2Kcomplete answer 🦀\x1b[2;1H\x1b[2Klast line');
    for (let i = 0; i < bytes.length; i++) r.api.smWriteBase64(i + 1, bytes.subarray(i, i + 1).toString('base64'));
    await drain(r.term);
    const buffer = r.term.buffer.active;
    assert.equal(buffer.getLine(buffer.baseY).translateToString(true), 'complete answer 🦀');
    assert.equal(buffer.getLine(buffer.baseY + 1).translateToString(true), 'last line');
    assert.equal(r.acks.length, bytes.length);
  } finally { r.term.dispose(); }
});
