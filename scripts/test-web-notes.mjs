// Browser behavior and screenshot proof for the Notes page and terminal pane (#1834).
// PLAYWRIGHT_MODULE=/path/to/playwright/index.mjs SHOTS_DIR=... node --test scripts/test-web-notes.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, readFile } from 'node:fs/promises';

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const origin = 'https://sm.example.com';
const shell = `<!doctype html><html data-theme="system"><head><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/vendor/xterm.css">
<script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script>
<script id="sm-config" type="application/json">{"inbox_token":"fixture","refresh_seconds":3}</script>
<script type="module" src="/assets/app.js"></script></head><body><div id="app"></div></body></html>`;

const stamp = () => new Date().toISOString();
const initial = [
  { id: 'one', title: 'Review loop brief', body: '# Review loop brief\nRequest a review with sm request-review once tests pass.', version: 1, updated_at: stamp() },
  { id: 'two', title: 'Merge checklist', body: '# Merge checklist\nConfirm the review is for your latest push, then squash merge.', version: 1, updated_at: stamp() },
];
function fakeSocket() {
  window.sockets = [];
  window.WebSocket = class {
    static OPEN = 1;
    constructor() {
      this.readyState = 0; this.sent = []; window.sockets.push(this);
      setTimeout(() => { this.readyState = 1; this.onopen(); }, 5);
    }
    send(data) {
      const frame = JSON.parse(data); this.sent.push(frame);
      if (frame.type === 'auth') setTimeout(() => this.onmessage({ data: JSON.stringify({ type: 'status', state: 'attached' }) }), 5);
      if (frame.type === 'ping') setTimeout(() => this.onmessage({ data: JSON.stringify({ type: 'pong', id: frame.id }) }), 5);
    }
    close() { this.readyState = 3; }
  };
}
function server() {
  const notes = initial.map(note => ({ ...note }));
  const calls = { starts: [], issues: [] };
  const handler = async route => {
    const request = route.request();
    const url = new URL(request.url());
    if (url.origin !== origin) return route.abort();
    if (url.pathname.startsWith('/assets/')) {
      const file = url.pathname.slice('/assets/'.length);
      return route.fulfill({ body: await readFile(new URL(file, assets)), contentType: file.endsWith('.css') ? 'text/css' : 'text/javascript' });
    }
    if (request.isNavigationRequest()) return route.fulfill({ body: shell, contentType: 'text/html' });
    const method = request.method();
    const json = (body, status = 200) => route.fulfill({ status, json: body });
    if (url.pathname === '/notes/search') {
      const q = (url.searchParams.get('q') || '').toLowerCase();
      return json(notes.filter(note => note.body.toLowerCase().includes(q) || note.title.toLowerCase().includes(q))
        .map(note => ({ id: note.id, title: note.title, updated_at: note.updated_at, chars: note.body.length,
          snippet: note.body, matches: q && note.body.toLowerCase().includes(q)
            ? [{ start: note.body.toLowerCase().indexOf(q), end: note.body.toLowerCase().indexOf(q) + q.length }] : [] })));
    }
    if (url.pathname === '/notes/preview') return json({ html: `<h1>${request.postDataJSON().body.split('\n')[0].replace(/^# /, '')}</h1>` });
    if (url.pathname === '/notes/import') {
      const note = { id: `import-${notes.length}`, title: 'Imported prompt', body: '# Imported prompt\nBody', version: 1, updated_at: stamp() };
      notes.unshift(note); return json({ ids: [note.id] });
    }
    if (url.pathname === '/notes' && method === 'POST') {
      const note = { id: `new-${notes.length}`, title: 'Untitled', body: request.postDataJSON().body, version: 1, updated_at: stamp() };
      notes.unshift(note); return json(note);
    }
    const noteId = url.pathname.match(/^\/notes\/([^/]+)$/)?.[1];
    if (noteId) {
      const note = notes.find(row => row.id === noteId);
      if (!note) return json({ detail: 'Missing' }, 404);
      if (method === 'PUT') {
        const hold = handler.holdNextSave;
        handler.holdNextSave = null;
        if (hold) { handler.saveStarted?.(); await hold; }
        const input = request.postDataJSON();
        if (input.if_version !== note.version) return json(note, 409);
        note.body = input.body; note.title = input.body.split('\n')[0].replace(/^# /, '') || 'Untitled';
        note.version++; note.updated_at = stamp(); return json(note);
      }
      const snapshot = { ...note };
      if (handler.nextLoadVersion) {
        snapshot.version = handler.nextLoadVersion;
        handler.nextLoadVersion = null;
      }
      const hold = handler.holdNextLoad;
      handler.holdNextLoad = null;
      if (hold) { handler.holdStarted?.(); await hold; }
      return json(snapshot);
    }
    if (url.pathname.endsWith('/revisions')) return json([{ version: 1, at: stamp() }]);
    if (url.pathname === '/client/board') return json({ lanes: [{ tickets: [{ repo: 'rajeshgoli/session-manager' }] }], other: [] });
    if (url.pathname === '/client/settings') return json({ new_agent: { workspaces: ['/repo'], provider: 'claude', claude: {}, codex: {} } });
    if (url.pathname === '/client/session-models') return json({ models: ['sonnet', 'opus'] });
    if (url.pathname === '/client/sessions' && method === 'POST') { calls.starts.push(request.postDataJSON()); return json({ id: 'started', name: 'new-agent' }); }
    if (url.pathname === '/github/issues') { calls.issues.push(request.postDataJSON()); return json({ number: 42, url: 'https://github.com/rajeshgoli/session-manager/issues/42' }); }
    if (url.pathname.endsWith('/browser-attach-ticket')) return json({ ticket_id: 'ticket', ticket_secret: 'secret', ws_url: '/client/terminal' });
    if (url.pathname === '/watch/state') return json({ sessions: [{ id: 'agent', name: 'sm-agent', provider: 'claude', repo: '/repo', state: 'idle', attention: { section: 'idle' }, facts: { agent: { state: 'idle', since: stamp() } } }], counts: {} });
    return json({});
  };
  handler.calls = calls;
  handler.notes = notes;
  return handler;
}

test('page, terminal pane, actions and a version conflict', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const shots = process.env.SHOTS_DIR;
  if (shots) await mkdir(shots, { recursive: true });
  try {
    for (const width of [1440, 390]) for (const colorScheme of ['light', 'dark']) {
      const context = await browser.newContext({ viewport: { width, height: width === 390 ? 844 : 900 }, colorScheme });
      await context.addInitScript(fakeSocket);
      const handler = server();
      await context.route('**/*', handler);
      const page = await context.newPage();
      const errors = []; page.on('pageerror', err => errors.push(err.message));
      await page.goto(`${origin}/notes`);
      await page.locator('.note-card').first().waitFor();
      await page.locator('.note-card-main').first().click();
      await page.locator('.notes-editor textarea').waitFor();
      assert.ok((await page.locator('.notes-columns').boundingBox()).width <= width);
      if (shots) await page.screenshot({ path: `${shots}/notes-page-${width}-${colorScheme}.png` });
      await page.getByRole('searchbox', { name: 'Search notes' }).fill('merge');
      await page.waitForFunction(() => document.querySelectorAll('.note-card').length === 1);
      assert.equal(await page.locator('.note-card').count(), 1);
      await page.getByRole('searchbox', { name: 'Search notes' }).fill('');
      await page.waitForFunction(() => document.querySelectorAll('.note-card').length === 2);
      await page.locator('.note-card').first().locator('.notes-actions').first().getByText('Start agent').click();
      await page.locator('.notes-popover').waitFor();
      if (shots) await page.screenshot({ path: `${shots}/notes-agent-${width}-${colorScheme}.png` });
      await page.locator('.note-card').first().locator('.notes-actions').first().getByText('Start agent').click();
      await page.locator('.note-card').first().locator('.notes-actions').first().getByText('File ticket').click();
      await page.locator('.notes-popover').waitFor();
      if (shots) await page.screenshot({ path: `${shots}/notes-ticket-${width}-${colorScheme}.png` });
      if (width === 1440 && colorScheme === 'light') {
        await page.locator('.notes-popover input').fill('rajeshgoli/session-manager');
        await page.locator('.notes-popover').getByRole('button', { name: 'File ticket' }).click();
        assert.equal(handler.calls.issues[0].title, 'Review loop brief');
        assert.match(handler.calls.issues[0].body, /Request a review/);
        await page.locator('.note-card').first().locator('.notes-actions').first().getByText('Start agent').click();
        const agentRequest = page.waitForResponse(response => new URL(response.url()).pathname === '/client/sessions' && response.request().method() === 'POST');
        await page.locator('.notes-popover').getByRole('button', { name: 'Start', exact: true }).click();
        await agentRequest;
        assert.equal(handler.calls.starts[0].working_dir, '/repo');
        assert.match(handler.calls.starts[0].initial_message, /Review loop brief/);
        await page.keyboard.press('Meta+j');
        await page.locator('.panel .note-card').first().waitFor();
        if (!await page.locator('.panel .notes-editor').count()) await page.locator('.panel .note-card-main').first().click();
        await page.locator('.panel .notes-editor textarea').fill('Saved before Escape');
        await page.keyboard.press('Escape');
        assert.equal(await page.locator('.panel').count(), 1, 'first Escape keeps the pane');
        await page.locator('.panel .notes-editor').waitFor({ state: 'hidden' });
        assert.equal(handler.notes.find(note => note.id === 'one').body, 'Saved before Escape');
        await page.locator('.panel .note-card-main').first().click();
        await page.locator('.panel .notes-editor textarea').fill('Saved before card collapse');
        await page.locator('.panel .note-card-main').first().click();
        await page.locator('.panel .notes-editor').waitFor({ state: 'hidden' });
        assert.equal(handler.notes.find(note => note.id === 'one').body, 'Saved before card collapse');
        await page.keyboard.press('Escape');
        assert.equal(await page.locator('.panel').count(), 0, 'second Escape closes the pane');
        await page.reload();
        await page.locator('.note-card-main').first().click();
        await page.locator('.notes-editor-slot textarea').fill('Edited just before import');
        await page.locator('.notes-tools input[type=file]').setInputFiles({ name: 'prompt.md', mimeType: 'text/markdown', buffer: Buffer.from('# Imported prompt\nBody') });
        await page.getByText('Imported prompt', { exact: true }).first().waitFor();
        assert.equal(handler.notes.find(note => note.id === 'one').body, 'Edited just before import');
      }
      assert.deepEqual(errors, []);
      await context.close();
    }

    for (const width of [1440, 390]) for (const colorScheme of ['light', 'dark']) {
      const terminalContext = await browser.newContext({ viewport: { width, height: width === 390 ? 844 : 900 }, colorScheme });
      await terminalContext.addInitScript(fakeSocket);
      await terminalContext.route('**/*', server());
      const terminal = await terminalContext.newPage();
      await terminal.goto(`${origin}/terminal/agent`);
      await terminal.getByText('Live', { exact: true }).waitFor();
      await terminal.keyboard.press('Meta+j');
      await terminal.locator('.term-panel .note-card').first().waitFor();
      await terminal.locator('.term-panel input[type=search]').fill('loop');
      await terminal.waitForFunction(() => document.querySelectorAll('.term-panel .note-card').length === 1);
      await terminal.locator('.term-panel .note-card-main').first().click();
      if (shots) await terminal.screenshot({ path: `${shots}/notes-terminal-${width}-${colorScheme}.png` });
      await terminal.locator('.term-panel .notes-editor textarea').fill('Paste without Enter');
      await terminal.locator('.term-panel .notes-editor .notes-actions').getByText('Type into terminal').click();
      await terminal.waitForFunction(() => window.sockets[0].sent.some(frame => frame.type === 'input'));
      assert.ok((await terminal.evaluate(() => window.sockets[0].sent.filter(frame => frame.type === 'input').map(frame => frame.data).join(''))).includes('\x1b[200~Paste without Enter\x1b[201~'));
      await terminalContext.close();
    }
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    const handler = server();
    await context.route('**/*', handler);
    const first = await context.newPage();
    await first.goto(`${origin}/notes`);
    await first.locator('.note-card').first().waitFor();
    await first.keyboard.press('Meta+j');
    await first.locator('.panel .note-card-main').first().click();
    const secondContext = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    await secondContext.route('**/*', handler);
    const second = await secondContext.newPage();
    await second.goto(`${origin}/notes`);
    await second.locator('.note-card-main').first().click();
    const secondSave = second.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
    await second.locator('.notes-editor textarea').fill('Changed on second browser');
    assert.equal((await secondSave).status(), 200);
    const firstSave = first.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
    await first.locator('.panel .notes-editor textarea').fill('Keep the first browser text');
    assert.equal((await firstSave).status(), 409);
    await first.getByRole('dialog', { name: 'Changed on another device' }).waitFor();
    await first.keyboard.press('Meta+j');
    assert.equal(await first.locator('.panel').count(), 1, 'pane remains while conflict is unresolved');
    assert.equal(await first.getByRole('dialog').count(), 1);
    await first.keyboard.press('Escape');
    await first.locator('.panel .note-card-main').first().evaluate(button => button.click());
    assert.equal(await first.locator('.panel .notes-editor textarea').inputValue(), 'Keep the first browser text');
    assert.equal(await first.getByRole('dialog').count(), 1, 'collapse keeps the conflict dialog');
    const historySettled = first.evaluate(() => new Promise(resolve => {
      let changes = 0;
      window.addEventListener('popstate', () => { if (++changes === 2) resolve(); });
    }));
    await first.goBack();
    await historySettled;
    assert.equal(new URL(first.url()).searchParams.get('open'), 'notes:view', 'browser history cannot dismiss the conflict');
    assert.equal(await first.getByRole('dialog').count(), 1);
    if (shots) await first.screenshot({ path: `${shots}/notes-conflict-1440-light.png` });
    const forcedSave = first.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
    await first.getByRole('button', { name: 'Keep mine' }).click();
    assert.equal((await forcedSave).status(), 200);
    await first.getByRole('dialog').waitFor({ state: 'hidden' });
    const pageConflict = second.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
    await second.locator('.notes-editor textarea').fill('Full-page unresolved version');
    assert.equal((await pageConflict).status(), 409);
    await second.getByRole('dialog', { name: 'Changed on another device' }).waitFor();
    await second.keyboard.press('g');
    await second.keyboard.press('b');
    assert.equal(new URL(second.url()).pathname, '/notes', 'keyboard navigation keeps the full-page conflict');
    assert.equal(await second.getByRole('dialog').count(), 1);
    await second.getByRole('button', { name: 'Load theirs' }).click();
    await second.getByRole('dialog').waitFor({ state: 'hidden' });
    await second.getByText('Board', { exact: true }).first().click();
    await second.waitForURL(`${origin}/board`);
    await secondContext.close();
    await context.close();
  } finally { await browser.close(); }
});

test('late note loads do not replace edits or a newly selected note', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const scenario of ['edit', 'selection', 'conflict']) {
      const context = await browser.newContext();
      const handler = server();
      await context.route('**/*', handler);
      const page = await context.newPage();
      await page.goto(`${origin}/notes`);
      await page.locator('.note-card').first().waitFor();
      if (scenario !== 'selection') {
        await page.locator('.note-card-main').first().click();
        await page.locator('.notes-editor textarea').waitFor();
      }
      if (scenario === 'conflict') {
        await page.locator('.notes-editor textarea').fill('Dirty before a slow visibility check');
        handler.nextLoadVersion = 3;
      }
      let release;
      handler.holdNextLoad = new Promise(resolve => { release = resolve; });
      const loadHeld = new Promise(resolve => { handler.holdStarted = resolve; });
      const delayedRequest = page.waitForRequest(request => request.url().endsWith('/notes/one') && request.method() === 'GET');
      if (scenario !== 'selection') await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
      else await page.locator('.note-card-main').first().click();
      await delayedRequest;
      await loadHeld;
      if (scenario === 'edit') await page.locator('.notes-editor textarea').fill('Typed during a slow refresh');
      else {
        await page.locator('.note-card-main').nth(1).click();
        await page.waitForFunction(() => document.querySelector('.notes-editor textarea')?.value.includes('Merge checklist'));
        assert.match(await page.locator('.notes-editor textarea').inputValue(), /Merge checklist/);
      }
      const delayedResponse = page.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'GET');
      release();
      await delayedResponse;
      assert.equal(await page.locator('.notes-editor textarea').inputValue(),
        scenario === 'edit' ? 'Typed during a slow refresh' : initial[1].body);
      if (scenario === 'conflict') assert.equal(await page.getByRole('dialog').count(), 0);
      await context.close();
    }
  } finally { await browser.close(); }
});

test('navigation waits for a pending save conflict', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const handler = server();
    const firstContext = await browser.newContext();
    const secondContext = await browser.newContext();
    await firstContext.route('**/*', handler);
    await secondContext.route('**/*', handler);
    const first = await firstContext.newPage();
    const second = await secondContext.newPage();
    for (const page of [first, second]) {
      await page.goto(`${origin}/notes`);
      await page.locator('.note-card-main').first().click();
      await page.locator('.notes-editor textarea').waitFor();
    }
    const newerSave = second.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
    await second.locator('.notes-editor textarea').fill('Saved on another device');
    assert.equal((await newerSave).status(), 200);
    let release;
    handler.holdNextSave = new Promise(resolve => { release = resolve; });
    const saveStarted = new Promise(resolve => { handler.saveStarted = resolve; });
    await first.locator('.notes-editor textarea').fill('Draft to preserve');
    await first.getByText('Board', { exact: true }).first().click();
    await saveStarted;
    assert.equal(new URL(first.url()).pathname, '/notes');
    release();
    await first.getByRole('dialog', { name: 'Changed on another device' }).waitFor();
    assert.equal(new URL(first.url()).pathname, '/notes');
    assert.equal(await first.locator('.notes-editor textarea').inputValue(), 'Draft to preserve');
    await firstContext.close();
    await secondContext.close();
  } finally { await browser.close(); }
});

test('navigation saves text typed while an earlier save is pending', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const context = await browser.newContext();
    const handler = server();
    await context.route('**/*', handler);
    const page = await context.newPage();
    await page.goto(`${origin}/notes`);
    await page.locator('.note-card-main').first().click();
    let release;
    handler.holdNextSave = new Promise(resolve => { release = resolve; });
    const saveStarted = new Promise(resolve => { handler.saveStarted = resolve; });
    await page.locator('.notes-editor textarea').fill('First edit');
    await page.getByText('Board', { exact: true }).first().click();
    await saveStarted;
    await page.locator('.notes-editor textarea').fill('Second edit during save');
    release();
    await page.waitForURL(`${origin}/board`);
    assert.equal(handler.notes.find(note => note.id === 'one').body, 'Second edit during save');
    await context.close();
  } finally { await browser.close(); }
});

test('an open editor stays visible when a save removes its search match', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const pane of [false, true]) {
      const context = await browser.newContext({ viewport: { width: pane ? 1440 : 390, height: 900 } });
      const handler = server();
      await context.route('**/*', handler);
      const page = await context.newPage();
      await page.goto(`${origin}/notes`);
      await page.locator('.notes-page .note-card').first().waitFor();
      if (pane) await page.keyboard.press('Meta+j');
      const view = pane ? page.locator('.panel .notes-view') : page.locator('.notes-page');
      await view.locator('.note-card').first().waitFor();
      await view.getByRole('searchbox', { name: 'Search notes' }).fill('Review loop');
      await page.waitForFunction(isPane => document.querySelectorAll(isPane ? '.panel .note-card' : '.notes-page .note-card').length === 1, pane);
      await view.locator('.note-card-main').first().click();
      const editor = view.locator('.notes-editor textarea');
      await editor.waitFor();
      const saved = page.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
      await editor.fill('Text without the original match');
      assert.equal((await saved).status(), 200);
      await page.waitForFunction(isPane => {
        const view = document.querySelector(isPane ? '.panel .notes-view' : '.notes-page');
        return view?.querySelector('input[type=search]')?.value === '' && view.querySelectorAll('.note-card').length === 2;
      }, pane);
      assert.equal(await editor.inputValue(), 'Text without the original match');
      assert.equal(handler.notes.find(note => note.id === 'one').body, 'Text without the original match');
      await context.close();
    }
  } finally { await browser.close(); }
});

test('collapse saves edits typed while an earlier save is pending', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const action of ['card', 'Escape']) {
      const context = await browser.newContext();
      const handler = server();
      await context.route('**/*', handler);
      const page = await context.newPage();
      await page.goto(`${origin}/notes`);
      await page.locator('.notes-page .note-card').first().waitFor();
      await page.keyboard.press('Meta+j');
      const pane = page.locator('.panel .notes-view');
      await pane.locator('.note-card-main').first().click();
      const editor = pane.locator('.notes-editor textarea');
      await editor.waitFor();
      let release;
      handler.holdNextSave = new Promise(resolve => { release = resolve; });
      const saveStarted = new Promise(resolve => { handler.saveStarted = resolve; });
      await editor.fill('First edit');
      await saveStarted;
      if (action === 'card') await pane.locator('.note-card-main').first().click();
      else await page.keyboard.press('Escape');
      await editor.fill('Second edit during collapse');
      release();
      await pane.locator('.notes-editor').waitFor({ state: 'hidden' });
      assert.equal(handler.notes.find(note => note.id === 'one').body, 'Second edit during collapse');
      await context.close();
    }
  } finally { await browser.close(); }
});

test('a save conflict during collapse keeps the editor and latest text', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const handler = server();
    const firstContext = await browser.newContext();
    const secondContext = await browser.newContext();
    await firstContext.route('**/*', handler);
    await secondContext.route('**/*', handler);
    const first = await firstContext.newPage();
    const second = await secondContext.newPage();
    await first.goto(`${origin}/notes`);
    await first.locator('.notes-page .note-card').first().waitFor();
    await first.keyboard.press('Meta+j');
    const pane = first.locator('.panel .notes-view');
    await pane.locator('.note-card-main').first().click();
    const editor = pane.locator('.notes-editor textarea');
    await second.goto(`${origin}/notes`);
    await second.locator('.note-card-main').first().click();
    const newerSave = second.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
    await second.locator('.notes-editor textarea').fill('Changed elsewhere');
    assert.equal((await newerSave).status(), 200);
    let release;
    handler.holdNextSave = new Promise(resolve => { release = resolve; });
    const saveStarted = new Promise(resolve => { handler.saveStarted = resolve; });
    await editor.fill('First local edit');
    await saveStarted;
    await first.keyboard.press('Escape');
    await editor.fill('Latest local edit');
    release();
    await first.getByRole('dialog', { name: 'Changed on another device' }).waitFor();
    assert.equal(await editor.inputValue(), 'Latest local edit');
    assert.equal(await pane.locator('.notes-editor').count(), 1);
    await firstContext.close();
    await secondContext.close();
  } finally { await browser.close(); }
});

test('Back and Forward keep their entries, and New clears an active search', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const context = await browser.newContext({ viewport: { width: 390, height: 844 } });
    await context.route('**/*', server());
    const page = await context.newPage();
    await page.goto(`${origin}/notes`);
    await page.locator('.note-card').first().waitFor();
    await page.keyboard.press('Meta+j');
    await page.locator('.panel').waitFor();
    await page.goBack();
    await page.locator('.panel').waitFor({ state: 'hidden' });
    await page.goForward();
    await page.locator('.panel').waitFor();
    await page.keyboard.press('Meta+j');
    await page.locator('.panel').waitFor({ state: 'hidden' });
    await page.getByRole('searchbox', { name: 'Search notes' }).fill('no such note');
    await page.getByText('No matching notes.').waitFor();
    await page.getByRole('button', { name: '+ New' }).click();
    await page.locator('.notes-editor textarea').waitFor();
    assert.equal(await page.getByRole('searchbox', { name: 'Search notes' }).inputValue(), '');
    await context.close();
  } finally { await browser.close(); }
});
