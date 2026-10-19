import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const source = (await readFile(new URL('../../crates/sm-server/src/web/terminal-images.js', import.meta.url), 'utf8')).replace(/^export /gm, '');
function fixture({ upload, platform = 'MacIntel', read } = {}) {
  let attached = {}, reads = 0;
  const uploads = [], pasted = [], textPastes = [], errors = [], handlers = new Map();
  const host = {
    addEventListener(name, fn, capture) { assert.equal(capture, true); handlers.set(name, fn); },
    removeEventListener(name, fn, capture) { assert.equal(fn, handlers.get(name)); assert.equal(capture, true); handlers.delete(name); },
  };
  class FileReader {
    readAsDataURL(file) { this.result = `data:${file.type};base64,${Buffer.from(file.bytes || 'image').toString('base64')}`; this.onload(); }
  }
  const context = vm.createContext({ FileReader, host, navigator: { platform, clipboard: { read: () => { reads++; return read ? read() : Promise.resolve([]); } } }, options: {
    connection: () => attached,
    upload: async body => { uploads.push(body); return upload ? upload(body) : { path: `/tmp/agent1/paste-${uploads.length}.png` }; },
    paste: path => pasted.push(path), pasteText: text => textPastes.push(text), report: error => errors.push(error),
  } });
  const cleanup = vm.runInContext(source + '\ninstallImagePaste(host, options)', context);
  const dispatch = (clipboardData, key) => {
    const event = { clipboardData, ...key, prevented: false, stopped: false,
      preventDefault() { this.prevented = true; }, stopImmediatePropagation() { this.stopped = true; } };
    handlers.get(key ? 'keydown' : 'paste')(event); return event;
  };
  return { dispatch, key: overrides => dispatch(null, { key: 'v', ctrlKey: true, ...overrides }), uploads, pasted, textPastes, errors, cleanup,
    get removed() { return handlers.size === 0; }, get reads() { return reads; }, disconnect() { attached = null; }, reconnect() { attached = {}; } };
}
const image = (type = 'image/png', size = 42) => ({ type, size });
const clipboard = (...files) => ({ items: files.map(file => ({ kind: 'file', type: file.type, getAsFile: () => file })) });
const settle = () => new Promise(setImmediate);

test('text-only paste reaches xterm unchanged', async () => {
  const f = fixture();
  const event = f.dispatch({ items: [{ kind: 'string', type: 'text/plain' }], getData: () => 'hello' });
  await settle(); assert.equal(event.prevented, false); assert.equal(event.stopped, false); assert.equal(f.uploads.length, 0);
});
test('image paste consumes accompanying text and uploads files in paste order', async () => {
  const f = fixture();
  const event = f.dispatch(clipboard(image()));
  f.dispatch(clipboard(image('image/jpeg')));
  await settle();
  assert.equal(event.prevented, true); assert.equal(event.stopped, true);
  assert.equal(f.uploads[0].mime, 'image/png'); assert.equal(f.uploads[1].mime, 'image/jpeg');
  assert.deepEqual(f.pasted, ['/tmp/agent1/paste-1.png', '/tmp/agent1/paste-2.png']);
});
test('mobile files-only paste and all supported formats upload', async () => {
  const f = fixture();
  for (const type of ['image/png', 'image/jpeg', 'image/gif', 'image/webp']) f.dispatch({ files: [image(type)] });
  await settle(); assert.equal(f.uploads.length, 4); assert.equal(f.pasted.length, 4);
});
test('unsupported, empty and oversized images never upload or fall through to text paste', async () => {
  const f = fixture();
  for (const file of [image('image/svg+xml'), image('image/png', 0), image('image/png', 10 * 1024 * 1024 + 1)]) {
    const event = f.dispatch(clipboard(file)); assert.equal(event.prevented, true);
  }
  await settle(); assert.equal(f.uploads.length, 0); assert.equal(f.errors.length, 3);
});
test('disconnected terminal reports failure without uploading', async () => {
  const f = fixture(); f.disconnect(); f.dispatch(clipboard(image()));
  await settle(); assert.equal(f.uploads.length, 0); assert.match(f.errors[0], /not connected/);
});
test('upload failures remain visible and the next paste still works', async () => {
  let fail = true;
  const f = fixture({ upload: async () => { if (fail) { fail = false; throw new Error('network unavailable'); } return { path: '/tmp/next.png' }; } });
  f.dispatch(clipboard(image())); f.dispatch(clipboard(image())); await settle();
  assert.match(f.errors[0], /network unavailable/); assert.deepEqual(f.pasted, ['/tmp/next.png']);
});
test('late uploads never paste after reconnect or unmount', async () => {
  for (const stop of [f => f.reconnect(), f => f.cleanup()]) {
    let finish;
    const f = fixture({ upload: () => new Promise(resolve => { finish = resolve; }) });
    f.dispatch(clipboard(image())); await settle(); stop(f); finish({ path: '/tmp/late.png' }); await settle();
    assert.deepEqual(f.pasted, []);
  }
  const f = fixture(); f.cleanup(); assert.equal(f.removed, true);
});

const clipboardItem = (type, file = image(type)) => ({ types: [type], getType: async () => file });
test('macOS Control+V reads during the key event and uploads without a native paste event', async () => {
  const f = fixture({ read: async () => [clipboardItem('image/png')] });
  const event = f.key();
  assert.equal(f.reads, 1, 'clipboard read starts synchronously during user activation');
  assert.equal(event.prevented, true); assert.equal(event.stopped, true);
  await settle(); assert.equal(f.uploads.length, 1); assert.equal(f.pasted.length, 1);
});
test('macOS Control+V prefers a supported image representation over text or TIFF', async () => {
  const f = fixture({ read: async () => [{ types: ['image/tiff', 'text/plain', 'image/png'], getType: async type => {
    assert.equal(type, 'image/png'); return image(type);
  } }] });
  f.key(); await settle(); assert.equal(f.uploads[0].mime, 'image/png'); assert.deepEqual(f.textPastes, []);
});
test('macOS Control+V sends text through the normal terminal paste function', async () => {
  const f = fixture({ read: async () => [clipboardItem('text/plain', { text: async () => 'hello\nworld' })] });
  f.key(); await settle(); assert.deepEqual(f.textPastes, ['hello\nworld']); assert.equal(f.uploads.length, 0);
});
test('native Cmd+V, Windows/Linux Control+V, and modified keys keep the native paste path', async () => {
  for (const platform of ['Win32', 'Linux x86_64']) {
    const f = fixture({ platform }); const event = f.key();
    assert.equal(event.prevented, false); assert.equal(f.reads, 0);
  }
  const f = fixture();
  for (const modifiers of [{ ctrlKey: false, metaKey: true }, { altKey: true }, { shiftKey: true }]) {
    assert.equal(f.key(modifiers).prevented, false);
  }
  assert.equal(f.reads, 0);
});
test('held Control+V does not repeat clipboard uploads', async () => {
  const f = fixture({ read: async () => [clipboardItem('image/png')] });
  f.key(); f.key({ repeat: true }); await settle(); assert.equal(f.reads, 1); assert.equal(f.uploads.length, 1);
});
test('clipboard read denial and unavailable APIs give a visible recovery and do not block the next paste', async () => {
  let calls = 0;
  const f = fixture({ read: () => ++calls === 1 ? Promise.reject(new Error('NotAllowedError')) : Promise.resolve([clipboardItem('image/png')]) });
  f.key(); f.key(); await settle();
  assert.match(f.errors[0], /Allow clipboard access.*Cmd\+V/); assert.equal(f.uploads.length, 1);
  const unavailable = fixture({ read: () => { throw new Error('Unsupported'); } });
  unavailable.key(); assert.match(unavailable.errors[0], /Cmd\+V/);
});
test('clipboard reads resolving after reconnect or unmount never type or upload', async () => {
  for (const stop of [f => f.reconnect(), f => f.cleanup()]) {
    let finish;
    const f = fixture({ read: () => new Promise(resolve => { finish = resolve; }) });
    f.key(); stop(f); finish([clipboardItem('image/png')]); await settle();
    assert.deepEqual(f.uploads, []); assert.deepEqual(f.pasted, []);
  }
});
test('a pending clipboard read does not suppress a later read failure', async () => {
  let finish, calls = 0;
  const f = fixture({ read: () => ++calls === 1 ? new Promise(resolve => { finish = resolve; }) : Promise.reject(new Error('denied')) });
  f.key(); f.key(); await settle(); finish([clipboardItem('image/png')]); await settle();
  assert.equal(f.uploads.length, 1); assert.match(f.errors[0], /clipboard access/);
});
