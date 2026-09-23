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
  const scrollbarVisibility = [];
  const inputs = [];
  const elements = {};
  const makeElement = () => ({ style: {}, hidden: false, attributes: {}, listeners: {},
    clientHeight: 96, clientWidth: 320, querySelector() { return null; },
    classList: { toggle() {}, add() {}, remove() {}, contains() { return false; } },
    getBoundingClientRect: () => ({top: 4, height: 600}),
    setAttribute(name, value) { this.attributes[name] = value; },
    removeAttribute(name) { delete this.attributes[name]; },
    addEventListener(name, handler) { this.listeners[name] = handler; },
  });
  const context = vm.createContext({
    Terminal, FitAddon: { FitAddon }, Uint8Array, atob,
    document: { getElementById: id => elements[id] ||= makeElement() },
    window: { TerminalBridge: { input: data => inputs.push(data), written: seq => acks.push(Number(seq)), scrollbarVisibility: visible => scrollbarVisibility.push(visible) }, addEventListener() {}, requestAnimationFrame() {}, setTimeout() {}, clearTimeout() {} },
  });
  vm.runInContext(source, context);
  vm.runInContext('ready = true; opened = true;', context);
  const term = vm.runInContext('term', context);
  term.resize(40, 8);
  return { context, term, acks, element: elements['terminal-scrollbar'], elements, inputs, scrollbarVisibility, api: context.window };
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

test('shrinking the terminal keeps the history viewport instead of following the live screen', async () => {
  const r = renderer();
  try {
    r.api.smWriteText(1, Array.from({length: 100}, (_, i) => `line ${i}\r\n`).join(''));
    await drain(r.term);
    r.term.scrollToLine(15);
    const animationFrames = [];
    r.api.requestAnimationFrame = callback => animationFrames.push(callback);
    vm.runInContext('fitAddon.proposeDimensions = () => ({cols: 60, rows: 3}); installScrollHandlers = () => {}; fitAndReport();', r.context);
    r.term.scrollToLine(20); // WebView's delayed viewport scroll after the resize
    while (animationFrames.length) animationFrames.shift()();
    assert.equal(r.term.rows, 3);
    assert.equal(r.term.buffer.active.viewportY, 15);
    assert.equal(r.term.buffer.active.getLine(15).translateToString(true), 'line 15');
  } finally { r.term.dispose(); }
});

test('column reflow follows the same logical text, including a wrapped continuation', async () => {
  const r = renderer();
  try {
    r.term.resize(20, 8);
    r.api.smWriteText(1, Array.from({length: 100}, (_, i) => `${String(i).padStart(3, '0')}${'x'.repeat(27)}\r\n`).join(''));
    await drain(r.term);
    for (const start of [80, 81]) {
      vm.runInContext('cancelResizeAnchor();', r.context);
      r.term.scrollToLine(start);
      const animationFrames = [];
      r.api.requestAnimationFrame = cb => animationFrames.push(cb);
      vm.runInContext('fitAddon.proposeDimensions = () => ({cols: 40, rows: 8}); installScrollHandlers = () => {}; fitAndReport();', r.context);
      r.term.scrollToLine(70);
      while (animationFrames.length) animationFrames.shift()();
      const b = r.term.buffer.active;
      assert.equal(b.viewportY, 40);
      assert.equal(b.getLine(b.viewportY).translateToString(true), `040${'x'.repeat(27)}`);
      r.term.resize(20, 8);
    }
  } finally { r.term.dispose(); }
});

test('dragging the history thumb traverses thousands of lines and clamps at both ends', async () => {
  const r = renderer();
  try {
    r.api.smWriteText(1, 'I’ll — “quoted” • café → ✓\r\n' + 'history\r\n'.repeat(3900));
    await drain(r.term);
    const base = r.term.buffer.active.baseY;
    assert(base > 3800);
    assert.equal(r.element.hidden, false);
    r.api.smBeginScrollbarDrag(580);
    r.api.smDragScrollbar(20);
    assert.equal(r.term.buffer.active.viewportY, 0);
    r.api.smDragScrollbar(300);
    assert(Math.abs(r.term.buffer.active.viewportY - base / 2) < 100);
    r.api.smDragScrollbar(10000);
    assert.equal(r.term.buffer.active.viewportY, base);
    r.api.smEndScrollbarDrag();
    r.api.smDragScrollbar(0);
    assert.equal(r.term.buffer.active.viewportY, base);
    assert.equal(r.element.attributes['aria-valuenow'], String(base));
    assert.equal(r.term.buffer.active.getLine(0).translateToString(true), 'I’ll — “quoted” • café → ✓');
  } finally { r.term.dispose(); }
});

test('history scrollbar hides in alternate buffers and does not send terminal input', async () => {
  const r = renderer();
  try {
    r.api.smWriteText(1, 'history\r\n'.repeat(100));
    await drain(r.term);
    r.api.smWriteText(2, '\x1b[?1049h');
    await drain(r.term);
    assert.equal(r.element.hidden, true);
    assert.equal(r.scrollbarVisibility.at(-1), false);
    r.api.smBeginScrollbarDrag(300);
    r.api.smDragScrollbar(0);
    assert.equal(r.term.buffer.active.viewportY, 0);
    r.api.smWriteText(3, '\x1b[?1049l');
    await drain(r.term);
    assert.equal(r.element.hidden, false);
    assert.deepEqual(r.scrollbarVisibility, [false, true, false, true]);
  } finally { r.term.dispose(); }
});

test('native scrollbar handoff survives the WebView pointer cancellation', async () => {
  const r = renderer();
  try {
    r.api.smWriteText(1, 'history\r\n'.repeat(3900));
    await drain(r.term);
    r.api.smBeginScrollbarDrag(580);
    r.api.smBeginScrollbarDrag(570, true);
    r.element.listeners.pointercancel();
    r.api.smDragScrollbar(0);
    assert.equal(r.term.buffer.active.viewportY, 0);
    r.api.smEndScrollbarDrag();
    r.api.smDragScrollbar(600);
    assert.equal(r.term.buffer.active.viewportY, 0);
  } finally { r.term.dispose(); }
});

test('animated resizes retain the first content anchor and release it for user scrolling', async () => {
  const r = renderer();
  try {
    r.api.smWriteText(1, 'history\r\n'.repeat(100));
    await drain(r.term);
    r.term.scrollToLine(30);
    vm.runInContext('fitAddon.proposeDimensions = () => ({cols: 40, rows: 6}); installScrollHandlers = () => {}; fitAndReport();', r.context);
    r.term.scrollToLine(33); // delayed intermediate WebView layout
    vm.runInContext('fitAddon.proposeDimensions = () => ({cols: 40, rows: 3}); fitAndReport();', r.context);
    assert.equal(r.term.buffer.active.viewportY, 30);
    r.api.smBeginScrollbarDrag(580);
    r.api.smDragScrollbar(0);
    assert.equal(r.term.buffer.active.viewportY, 0);
    assert.equal(vm.runInContext('resizeAnchor', r.context), null);
  } finally { r.term.dispose(); }
});

test('Codex fork history remains navigable when tmux hides its alternate screen', async () => {
  const r = renderer();
  try {
    r.api.smSetProvider('codex-fork');
    // The real attach stream paints a normal xterm viewport with no scrollback,
    // even though the provider behind tmux owns a full-screen transcript.
    r.api.smWriteText(1, '\x1b[H\x1b[2JFull-screen transcript\x1b[6;1HComposer');
    await drain(r.term);
    assert.equal(r.term.buffer.active.type, 'normal');
    assert.equal(r.term.buffer.active.baseY, 0);
    assert.equal(r.element.hidden, false);
    assert.equal(r.element.attributes.role, 'group');
    assert.equal(r.element.attributes['aria-valuemax'], undefined);
    r.api.smScrollLines(-6);
    r.api.smScrollLines(6);
    const click = { preventDefault() {}, stopPropagation() {} };
    r.elements['history-earlier'].listeners.click(click);
    r.elements['history-later'].listeners.click(click);
    assert.deepEqual(r.inputs, ['\x1b[5~', '\x1b[6~', '\x1b[5~', '\x1b[6~']);
    assert.equal(r.term.buffer.active.baseY, 0);
  } finally { r.term.dispose(); }
});

test('provider history rail survives native handoff, ignores jitter, and stops after release', () => {
  const r = renderer();
  try {
    r.api.smSetProvider('codex-fork');
    r.api.smBeginScrollbarDrag(300);
    r.api.smBeginScrollbarDrag(280, true);
    r.element.listeners.pointercancel();
    r.api.smSetProvider('codex-fork'); // repeated Compose update must preserve drag
    r.api.smDragScrollbar(230);
    assert.deepEqual(r.inputs, []);
    r.api.smDragScrollbar(220);
    r.api.smDragScrollbar(300);
    r.api.smEndScrollbarDrag();
    r.api.smDragScrollbar(0);
    assert.deepEqual(r.inputs, ['\x1b[5~', '\x1b[6~']);
  } finally { r.term.dispose(); }
});

test('provider viewport swipes accumulate one page per 72 pixels', () => {
  const r = renderer();
  try {
    r.api.smSetProvider('codex-fork');
    for (let i = 0; i < 5; i++) r.api.smScrollPixels(-12);
    assert.deepEqual(r.inputs, []);
    r.api.smScrollPixels(-12);
    assert.deepEqual(r.inputs, ['\x1b[5~']);
    r.api.smScrollPixels(144);
    assert.deepEqual(r.inputs, ['\x1b[5~', '\x1b[6~', '\x1b[6~']);
    r.api.smScrollPixels(-60);
    r.api.smSetProvider('codex');
    r.api.smSetProvider('codex-fork');
    r.api.smScrollPixels(-12);
    assert.equal(r.inputs.length, 3);
    r.api.smScrollPixels(-60);
    assert.equal(r.inputs.length, 4);
    assert.equal(r.inputs.at(-1), '\x1b[5~');
  } finally { r.term.dispose(); }
});

test('provider-owned history takes precedence over incidental xterm history and restores local mode', async () => {
  const r = renderer();
  try {
    r.api.smWriteText(1, 'history\r\n'.repeat(100));
    await drain(r.term);
    const bottom = r.term.buffer.active.viewportY;
    r.api.smSetProvider('codex-fork');
    r.api.smScrollLines(-6);
    assert.equal(r.term.buffer.active.viewportY, bottom);
    r.api.smWriteText(2, '\x1b[?1049h');
    await drain(r.term);
    assert.equal(r.element.hidden, false);
    r.element.listeners.keydown({key: 'PageDown', preventDefault() {}, stopPropagation() {}});
    assert.deepEqual(r.inputs, ['\x1b[5~', '\x1b[6~']);
    r.api.smSetProvider('codex');
    assert.equal(r.element.hidden, true);
    r.api.smWriteText(3, '\x1b[?1049l');
    await drain(r.term);
    assert.equal(r.element.attributes.role, 'scrollbar');
    assert.equal(r.element.hidden, false);
    r.api.smScrollLines(-6);
    assert.equal(r.term.buffer.active.viewportY, bottom - 6);
    assert.equal(r.inputs.length, 2);
  } finally { r.term.dispose(); }
});
