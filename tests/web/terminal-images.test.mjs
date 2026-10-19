import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const source = (await readFile(new URL('../../crates/sm-server/src/web/terminal-images.js', import.meta.url), 'utf8')).replace(/^export /gm, '');
function fixture({ upload } = {}) {
  let handler, attached = {}, removed = false;
  const uploads = [], pasted = [], errors = [];
  const host = {
    addEventListener(name, fn, capture) { assert.equal(name, 'paste'); assert.equal(capture, true); handler = fn; },
    removeEventListener(name, fn, capture) { assert.equal(fn, handler); assert.equal(capture, true); removed = true; },
  };
  class FileReader {
    readAsDataURL(file) { this.result = `data:${file.type};base64,${Buffer.from(file.bytes || 'image').toString('base64')}`; this.onload(); }
  }
  const context = vm.createContext({ FileReader, host, options: {
    connection: () => attached,
    upload: async body => { uploads.push(body); return upload ? upload(body) : { path: `/tmp/agent1/paste-${uploads.length}.png` }; },
    paste: path => pasted.push(path), report: error => errors.push(error),
  } });
  const cleanup = vm.runInContext(source + '\ninstallImagePaste(host, options)', context);
  const dispatch = clipboardData => {
    const event = { clipboardData, prevented: false, stopped: false,
      preventDefault() { this.prevented = true; }, stopImmediatePropagation() { this.stopped = true; } };
    handler(event); return event;
  };
  return { dispatch, uploads, pasted, errors, cleanup, get removed() { return removed; }, disconnect() { attached = null; }, reconnect() { attached = {}; } };
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
