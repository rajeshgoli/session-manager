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
    if (url.pathname === '/notes' && method === 'POST') {
      const note = { id: `new-${notes.length}`, title: 'Untitled', body: request.postDataJSON().body, version: 1, updated_at: stamp() };
      notes.unshift(note); return json(note);
    }
    const noteId = url.pathname.match(/^\/notes\/([^/]+)$/)?.[1];
    if (noteId) {
      const note = notes.find(row => row.id === noteId);
      if (!note) return json({ detail: 'Missing' }, 404);
      if (method === 'PUT') {
        const input = request.postDataJSON();
        if (input.if_version !== note.version) return json(note, 409);
        note.body = input.body; note.title = input.body.split('\n')[0].replace(/^# /, '') || 'Untitled';
        note.version++; note.updated_at = stamp(); return json(note);
      }
      return json(note);
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
        await page.keyboard.press('Escape');
        assert.equal(await page.locator('.panel').count(), 1, 'first Escape keeps the pane');
        assert.equal(await page.locator('.panel .notes-editor').count(), 0, 'first Escape collapses the note');
        await page.keyboard.press('Escape');
        assert.equal(await page.locator('.panel').count(), 0, 'second Escape closes the pane');
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
    await first.locator('.note-card-main').first().click();
    const secondContext = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    await secondContext.route('**/*', handler);
    const second = await secondContext.newPage();
    await second.goto(`${origin}/notes`);
    await second.locator('.note-card-main').first().click();
    const secondSave = second.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
    await second.locator('.notes-editor textarea').fill('Changed on second browser');
    assert.equal((await secondSave).status(), 200);
    const firstSave = first.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
    await first.locator('.notes-editor textarea').fill('Keep the first browser text');
    assert.equal((await firstSave).status(), 409);
    await first.getByRole('dialog', { name: 'Changed on another device' }).waitFor();
    if (shots) await first.screenshot({ path: `${shots}/notes-conflict-1440-light.png` });
    const forcedSave = first.waitForResponse(response => response.url().endsWith('/notes/one') && response.request().method() === 'PUT');
    await first.getByRole('button', { name: 'Keep mine' }).click();
    assert.equal((await forcedSave).status(), 200);
    await first.getByRole('dialog').waitFor({ state: 'hidden' });
    await secondContext.close();
    await context.close();
  } finally { await browser.close(); }
});
