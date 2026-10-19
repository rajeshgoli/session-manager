// Terminal page in a browser (spec 1782 G1-G4): switcher, keys only on phones, route and round trip.
// Run with node --test; install playwright or set PLAYWRIGHT_MODULE to its entry point.
// Set SHOTS_DIR to also save the 1440/390 px, light/dark, 15/19 px screenshots there.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';
const SHOTS = process.env.SHOTS_DIR;
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const ORIGIN = 'https://sm.example.com';
const shell = `<!doctype html><html data-theme="system"><head><meta name="viewport" content="width=device-width,initial-scale=1">
<script>document.documentElement.style.fontSize=(Number(localStorage.getItem("sm-text-size"))||15)+"px"</script>
<link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/vendor/xterm.css">
<script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script>
<script id="sm-config" type="application/json">{"inbox_token":"fixture","refresh_seconds":3}</script>
<script type="module" src="/assets/app.js"></script></head><body><div id="app"></div></body></html>`;

const ago = (minutes) => new Date(Date.now() - minutes * 60000).toISOString();
const noJobs = { running: 0, waiting: 0, review: null, text: 'No jobs', tone: null };
const agent = (name, provider, section, key, facts, extra = {}) => ({
  id: name, name, provider, repo: '/Users/rajesh/projects/session-manager', state: facts.agent?.state === 'working' ? 'working' : 'idle',
  claims: [], context_percent: 30, jobs: [], waiting_on: [], docs: [],
  attention: { section, reason: null, order_key: key },
  facts: { agent: { state: 'idle', since: ago(5) }, jobs: noJobs, you: null, finished: null, ...facts },
  ...extra,
});
function fixture() {
  return [
    agent('sm-1726-engineer', 'codex-fork', 'you', ago(7), {
      you: { kind: 'message', since: ago(7), text: 'one manual Chrome check: please open any terminal in Chrome', more: 0, dismissible: true },
    }),
    agent('far-1855', 'claude', 'finished', '8240483139', {
      finished: { at: ago(286), text: '1855 done and closed: 68 views built', read: false },
    }),
    agent('iter8-run', 'claude', 'moving', '8240469479', {
      jobs: { running: 2, waiting: 0, review: null, text: '2 running · 2h 56m', tone: 'green' },
    }, { remote_control: { url: 'https://claude.ai/code/session_run' } }),
    agent('sm-1776', 'codex-fork', 'moving', '8240469100', { agent: { state: 'working', since: ago(1) } }),
    // A paired reviewer mid-round with no queue jobs: its row shows the round, not "Idle".
    agent('sm-reviewer', 'codex', 'waiting', ago(12), {}, {
      paired_reviewer: { request_state: 'reviewing', pr_number: 1862, author_name: 'sm-1797', round: 1 },
    }),
    ...['sm-1768', 'sm-1727', 'sm-scout'].map((name, i) => agent(name, 'claude', 'idle', `1824046${9600 + i}`, { agent: { state: 'idle', since: ago(11 + i * 30) } })),
  ];
}

// A stand-in for the bridge: attaches on auth, writes a prompt, answers pings.
// `window.failDirect` makes a direct socket close before it attaches.
function fakeSocket() {
  window.sockets = [];
  window.WebSocket = class {
    static OPEN = 1;
    constructor(url) {
      this.url = url; this.readyState = 0; this.sent = [];
      window.sockets.push(this);
      setTimeout(() => {
        if (window.failDirect && url.startsWith('ws://localhost:8420')) { this.readyState = 3; this.onclose({ code: 1006 }); return; }
        this.readyState = 1; this.onopen();
      }, 5);
    }
    send(text) {
      const frame = JSON.parse(text);
      this.sent.push(frame);
      const reply = (f, ms) => setTimeout(() => this.readyState === 1 && this.onmessage({ data: JSON.stringify(f) }), ms);
      if (frame.type === 'auth') {
        reply({ type: 'status', state: 'attached' }, 5);
        reply({ type: 'output', sequence: 1, data: btoa(window.termOutput || 'rajesh@studio ~ % ') }, 10);
      }
      if (frame.type === 'ping') reply({ type: 'pong', id: frame.id }, this.url.startsWith('ws://localhost') ? 2 : 40);
    }
    close() { this.readyState = 3; }
  };
}

async function open(browser, { viewport, colorScheme = 'light', size = 15, instance = 'inst-1', failDirect = false, touch = false }) {
  const context = await browser.newContext({ viewport, colorScheme, hasTouch: touch, isMobile: touch });
  const page = await context.newPage();
  await page.addInitScript((value) => localStorage.setItem('sm-text-size', String(value)), size);
  await page.addInitScript(fakeSocket);
  if (failDirect) await page.addInitScript(() => { window.failDirect = true; });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  const state = { sessions: fixture(), answered: [], retired: [], renamed: [], tickets: 0, images: [] };
  await page.route('**/*', async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    if (url.origin === 'http://localhost:8420' && url.pathname === '/client/terminal/probe') {
      return route.fulfill({ json: { instance }, headers: { 'access-control-allow-origin': ORIGIN, vary: 'Origin' } });
    }
    if (url.origin !== ORIGIN) return route.abort();
    if (url.pathname.startsWith('/assets/')) {
      const file = url.pathname.slice('/assets/'.length);
      return route.fulfill({ body: await readFile(new URL(file, assets)), contentType: file.endsWith('.css') ? 'text/css' : 'text/javascript' });
    }
    if (request.isNavigationRequest()) return route.fulfill({ body: shell, contentType: 'text/html' });
    if (/\/terminal-images$/.test(url.pathname)) {
      state.images.push(request.postDataJSON());
      return route.fulfill({ json: { path: '/tmp/agent/pasted-images/paste-test.png' } });
    }
    if (/\/browser-attach-ticket$/.test(url.pathname)) {
      state.tickets += 1;
      return route.fulfill({ json: {
        ticket_id: `t${state.tickets}`, ticket_secret: 's', ws_url: '/client/terminal', server_instance: 'inst-1',
        direct: [{ url: 'ws://localhost:8420/client/terminal', probe: 'http://localhost:8420/client/terminal/probe' }],
      } });
    }
    const answer = /^\/sessions\/([^/]+)\/needs-you\/answered$/.exec(url.pathname);
    if (answer && request.method() === 'POST') {
      const id = decodeURIComponent(answer[1]);
      state.answered.push(id);
      state.sessions = state.sessions.map((s) => (s.id === id ? { ...s, facts: { ...s.facts, you: null }, attention: { section: 'idle', reason: null, order_key: '0' } } : s));
      return route.fulfill({ json: { facts: null } });
    }
    const retire = /^\/sessions\/([^/]+)\/retire$/.exec(url.pathname);
    if (retire && request.method() === 'POST') {
      const id = decodeURIComponent(retire[1]);
      state.retired.push({ id, body: request.postDataJSON() });
      state.sessions = state.sessions.filter((s) => s.id !== id);
      return route.fulfill({ json: {} });
    }
    const rename = /^\/sessions\/([^/]+)$/.exec(url.pathname);
    if (rename && request.method() === 'PATCH') {
      const id = decodeURIComponent(rename[1]);
      const name = request.postDataJSON().friendly_name;
      state.renamed.push({ id, name });
      state.sessions = state.sessions.map((s) => (s.id === id ? { ...s, name } : s));
      return route.fulfill({ json: {} });
    }
    if (url.pathname === '/watch/state') return route.fulfill({ json: { sessions: state.sessions, counts: {} } });
    return route.fulfill({ json: {} });
  });
  return { context, page, errors, state };
}

const rows = (page) => page.locator('.sw-row .sw-nm').allInnerTexts();
// The phone bar prints the route; the desktop bar keeps it in the name's tooltip (#2242).
const routeShown = (page) => page.evaluate(() => document.querySelector('.term-route')?.textContent
  ?? (document.querySelector('.term-who')?.title || '').split(' · ').slice(-2).join(' · '));
const routeTimed = (page) => page.waitForFunction(() => /ms$/.test(document.querySelector('.term-route')?.textContent
  ?? document.querySelector('.term-who')?.title ?? ''));

test('desktop and phone image paste uploads once and types only a bracketed absolute path', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const viewport of [{ width: 1440, height: 900 }, { width: 390, height: 844 }]) {
      const { context, page, state, errors } = await open(browser, { viewport, touch: viewport.width < 900 });
      await page.goto(`${ORIGIN}/terminal/sm-1776`);
      await page.locator('.term-page[data-connection="Live"]').waitFor();
      await page.evaluate(() => {
        const data = new DataTransfer();
        data.items.add(new File([new Uint8Array([137,80,78,71,13,10,26,10])], 'clipboard.png', { type: 'image/png' }));
        data.setData('text/plain', 'unwanted accompanying clipboard text');
        document.querySelector('.xterm-helper-textarea').dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
      });
      await page.waitForFunction(() => window.sockets.some(ws => ws.sent.some(frame => frame.type === 'input' && frame.data.includes('paste-test.png'))));
      assert.equal(state.images.length, 1);
      assert.equal(state.images[0].mime, 'image/png');
      assert.equal(state.images[0].data, 'iVBORw0KGgo=');
      const input = await page.evaluate(() => window.sockets.flatMap(ws => ws.sent).filter(frame => frame.type === 'input').map(frame => frame.data).join(''));
      assert.equal(input, '\x1b[200~/tmp/agent/pasted-images/paste-test.png\x1b[201~');
      await page.evaluate(() => {
        const data = new DataTransfer(); data.setData('text/plain', 'text paste still works');
        document.querySelector('.xterm-helper-textarea').dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
      });
      assert.equal(state.images.length, 1);
      assert.equal(await page.evaluate(() => window.sockets.flatMap(ws => ws.sent).filter(frame => frame.type === 'input').at(-1).data), 'text paste still works');
      assert.deepEqual(errors, []);
      await context.close();
    }
  } finally { await browser.close(); }
});

test('macOS Control+V pastes browser images and text, and explains denied clipboard access', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { context, page, state, errors } = await open(browser, { viewport: { width: 1440, height: 900 } });
    await page.addInitScript(() => {
      Object.defineProperty(navigator, 'platform', { value: 'MacIntel' });
      window.clipboardReads = 0;
      window.clipboardMode = 'image';
      Object.defineProperty(navigator.clipboard, 'read', { value: async () => {
        window.clipboardReads++;
        if (window.clipboardMode === 'denied') throw new DOMException('Denied', 'NotAllowedError');
        if (window.clipboardMode === 'text') return [{ types: ['text/plain'], getType: async () => new Blob(['browser clipboard text'], { type: 'text/plain' }) }];
        return [{ types: ['image/png'], getType: async () => new Blob([new Uint8Array([137,80,78,71,13,10,26,10])], { type: 'image/png' }) }];
      } });
    });
    await page.goto(`${ORIGIN}/terminal/sm-1776`);
    await page.locator('.term-page[data-connection="Live"]').waitFor();
    await page.locator('.xterm-helper-textarea').focus();
    await page.keyboard.press('Control+v');
    await page.waitForFunction(() => window.sockets.some(ws => ws.sent.some(frame => frame.type === 'input' && frame.data.includes('paste-test.png'))), null, { timeout: 3000 });
    assert.equal(state.images.length, 1);
    assert.equal(await page.evaluate(() => window.clipboardReads), 1);
    assert.equal(await page.evaluate(() => window.sockets.flatMap(ws => ws.sent).filter(frame => frame.type === 'input').map(frame => frame.data).join('')), '\x1b[200~/tmp/agent/pasted-images/paste-test.png\x1b[201~');
    await page.evaluate(() => { window.clipboardMode = 'text'; });
    await page.keyboard.press('Control+v');
    await page.waitForFunction(() => window.sockets.flatMap(ws => ws.sent).filter(frame => frame.type === 'input').at(-1)?.data === 'browser clipboard text');
    await page.evaluate(() => { window.clipboardMode = 'denied'; });
    await page.keyboard.press('Control+v');
    await page.getByText('Could not read the browser clipboard. Allow clipboard access, or paste with Cmd+V.', { exact: true }).waitFor();
    assert.equal(state.images.length, 1);
    assert.equal(await page.evaluate(() => window.clipboardReads), 3);
    assert.equal(await page.evaluate(() => window.sockets.flatMap(ws => ws.sent).filter(frame => frame.type === 'input').length), 2);
    assert.deepEqual(errors, []);
    await context.close();
  } finally { await browser.close(); }
});

test('switcher, keys and route at desktop and phone sizes', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const shots = SHOTS;
  if (shots) await mkdir(shots, { recursive: true });
  try {
    for (const viewport of [{ width: 1440, height: 900 }, { width: 390, height: 844 }]) {
      for (const colorScheme of ['light', 'dark']) {
        for (const size of [15, 19]) {
          const phone = viewport.width < 900;
          const { context, page, errors } = await open(browser, { viewport, colorScheme, size, touch: phone });
          await page.goto(`${ORIGIN}/terminal/iter8-run`);
          await page.locator('.term-page[data-connection="Live"]').waitFor();
          await routeTimed(page);
          assert.match(await routeShown(page), /^Direct · \d+ ms$/);
          // G2: key buttons on phones only.
          assert.equal(await page.locator('.term-keys').count(), phone ? 1 : 0);
          if (!phone) {
            // The desktop bar (#2242): who and how full on the left, actions on the right, Retire last;
            // no Back button and no "Live" while live.
            assert.deepEqual(await page.locator('.term-bar button').evaluateAll((b) => b.map((x) => x.getAttribute('aria-label') || x.textContent.trim())),
              ['Notes ⌘J', 'Details', 'Hand off…', 'More', 'Open in Claude', 'Redraw', 'Report a bug', 'Retire']);
            assert.equal(await page.locator('.term-bar .term-connection').count(), 0);
            assert.equal(await page.locator('.term-who .t').innerText(), 'iter8-run');
          }
          if (phone) {
            // G1: hidden by default below 900 px; the edge bar opens it over the terminal.
            assert.equal(await page.locator('.term-switch').count(), 0);
            await page.getByRole('button', { name: 'Unfold session list' }).click();
          }
          await page.locator('.sw-row').first().waitFor();
          assert.deepEqual(await page.locator('.sw-sec').allInnerTexts(), ['NEEDS YOU', 'FINISHED', 'MOVING', 'WAITING', 'IDLE · 3 ›']);
          assert.deepEqual(await rows(page), ['sm-1726-engineer', 'far-1855', 'sm-1776', 'iter8-run', 'sm-reviewer']);
          assert.equal(await page.locator('.sw-row', { hasText: 'sm-reviewer' }).locator('.sw-fact').innerText(), 'Reviewing PR #1862 for sm-1797 · round 1');
          assert.equal(await page.locator('.sw-row.cur .sw-nm').innerText(), 'iter8-run');
          assert.equal(await page.locator('.sw-row.cur .sw-fact').innerText(), '▶ 2 running · 2h 56m');
          assert.match(await page.locator('.sw-row').first().locator('.sw-fact').innerText(), /^◆ 7m: one manual Chrome check/);
          // ✓ on the open question and on the finished agent (Mark read); none elsewhere.
          assert.deepEqual(await page.locator('.sw-row .ok').evaluateAll((b) => b.map((x) => x.getAttribute('aria-label'))),
            ['Mark sm-1726-engineer answered', 'Mark far-1855 read']);
          const pageWidth = await page.evaluate(() => document.documentElement.scrollWidth);
          assert.ok(pageWidth <= viewport.width, `no horizontal scroll at ${viewport.width} px (got ${pageWidth})`);
          if (shots) await page.screenshot({ path: `${shots}/terminal-${viewport.width}-${colorScheme}-${size}.png` });
          assert.deepEqual(errors, []);
          await context.close();
        }
      }
    }
  } finally { await browser.close(); }
});

test('switching agents, ✓, the idle fold and ⌘\\ at 1440 px', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, errors, state } = await open(browser, { viewport: { width: 1440, height: 900 } });
    await page.goto(`${ORIGIN}/terminal/sm-reviewer`);
    await page.locator('.term-page[data-connection="Live"]').waitFor();
    // sm-reviewer is the last row: ⌘⌥↓ stays, ⌘⌥↑ goes to the row above and reattaches.
    await page.keyboard.press('Meta+Alt+ArrowDown');
    assert.equal(new URL(page.url()).pathname, '/terminal/sm-reviewer');
    await page.keyboard.press('Meta+Alt+ArrowUp');
    await page.waitForFunction(() => location.pathname === '/terminal/iter8-run');
    await page.waitForFunction(() => window.sockets.length === 2 && window.sockets[1].sent.some((f) => f.type === 'auth'));
    assert.ok(await page.evaluate(() => window.sockets[0].sent.some((f) => f.type === 'detach')), 'the old socket detaches');
    assert.equal(await page.evaluate(() => window.sockets[1].sent.find((f) => f.type === 'auth').ticket_id), 't2');
    assert.equal(await page.locator('.sw-row.cur .sw-nm').innerText(), 'iter8-run');
    await page.locator('.term-page[data-connection="Live"]').waitFor();
    await page.keyboard.press('Meta+Alt+ArrowUp');
    await page.waitForFunction(() => location.pathname === '/terminal/sm-1776');
    await page.keyboard.press('Meta+Alt+ArrowUp');
    await page.waitForFunction(() => location.pathname === '/terminal/far-1855');
    await page.keyboard.press('Meta+Alt+ArrowDown');
    await page.waitForFunction(() => location.pathname === '/terminal/sm-1776');
    // A click on a row switches too, and focus goes back to the terminal.
    await page.locator('.sw-row', { hasText: 'iter8-run' }).click();
    await page.waitForFunction(() => location.pathname === '/terminal/iter8-run');
    await page.locator('.term-page[data-connection="Live"]').waitFor();
    await page.waitForFunction(() => document.activeElement?.classList.contains('xterm-helper-textarea'));
    // The idle section opens and is remembered.
    await page.getByRole('button', { name: /Idle · 3/ }).click();
    assert.deepEqual((await rows(page)).slice(-3), ['sm-1768', 'sm-1727', 'sm-scout']);
    assert.equal(await page.evaluate(() => localStorage.getItem('sm-term-switcher-idle')), 'true');
    // ✓ clears the question without switching.
    await page.getByRole('button', { name: 'Mark sm-1726-engineer answered', exact: true }).click();
    await page.waitForFunction(() => document.querySelectorAll('.sw-row .ok').length === 1);
    assert.deepEqual(state.answered, ['sm-1726-engineer']);
    assert.equal(new URL(page.url()).pathname, '/terminal/iter8-run');
    // ⌘\ hides the switcher and remembers it.
    await page.keyboard.press('Meta+Backslash');
    await page.waitForFunction(() => !document.querySelector('.term-switch'));
    assert.equal(await page.evaluate(() => localStorage.getItem('sm-term-switcher')), 'false');
    // The fold control is the list's own edge bar, not a bar button by Back (#2242).
    assert.equal(await page.locator('.term-bar').getByRole('button', { name: /agent/i }).count(), 0);
    // Folded, only the edge bar stays, and the terminal starts right after it.
    const box = (q) => page.locator(q).boundingBox();
    const edge = await box('.term-edge');
    assert.ok(edge.x === 0 && edge.width <= 16 && edge.height > 800, JSON.stringify(edge));
    assert.ok(Math.abs((await box('.term-body')).x - edge.width) <= 1);
    // Unfold and fold by the edge bar, twice: the terminal is always right of the list.
    for (let round = 0; round < 2; round += 1) {
      await page.getByRole('button', { name: 'Unfold session list' }).click();
      await page.locator('.term-switch').waitFor();
      assert.equal(await page.evaluate(() => localStorage.getItem('sm-term-switcher')), 'true');
      const list = await box('.term-switch-wrap'), open = await box('.term-edge');
      assert.ok(Math.abs(open.x - list.width) <= 1, 'the edge bar sits at the list\'s right edge');
      assert.ok(Math.abs((await box('.term-body')).x - (open.x + open.width)) <= 1, 'the terminal sits right of the list');
      await page.getByRole('button', { name: 'Fold session list' }).click();
      await page.waitForFunction(() => !document.querySelector('.term-switch'));
      assert.ok(Math.abs((await box('.term-body')).x - edge.width) <= 1);
    }
    if (process.env.SHOTS_DIR) await page.screenshot({ path: `${process.env.SHOTS_DIR}/terminal-folded-1440.png` });
    // After all that switching, sm at the top left goes home in one click.
    await page.locator('.term-bar').getByRole('link', { name: 'sm', exact: true }).click();
    await page.waitForFunction(() => location.pathname === '/' && !document.querySelector('.term-bar'));
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('a burst of window sizes reaches the PTY once, then a one-column nudge repaints it; Redraw nudges too', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, errors } = await open(browser, { viewport: { width: 1440, height: 900 } });
    await page.goto(`${ORIGIN}/terminal/iter8-run`);
    await page.locator('.term-page[data-connection="Live"]').waitFor();
    const sizes = () => page.evaluate(() => window.sockets[0].sent.filter((f) => f.type === 'resize').map((f) => `${f.cols}x${f.rows}`));
    const [first] = await sizes();
    assert.equal((await sizes()).length, 1, 'one size on attach');
    // Twenty sizes in quick succession, as dragging a window edge produces (#2242).
    for (let i = 0; i < 20; i += 1) {
      await page.setViewportSize({ width: 1400 - i * 20, height: 900 - i * 5 });
      await page.waitForTimeout(30);
    }
    await page.waitForFunction(() => window.sockets[0].sent.filter((f) => f.type === 'resize').length >= 4);
    await page.waitForTimeout(600);
    const after = (await sizes()).slice(1);
    assert.equal(after.length, 3, `only the settled size and the nudge pair: ${after}`);
    const [cols, rows] = after[0].split('x').map(Number);
    assert.notEqual(after[0], first);
    assert.deepEqual(after.slice(1), [`${cols - 1}x${rows}`, `${cols}x${rows}`]);
    await page.getByRole('button', { name: 'Redraw' }).click();
    await page.waitForFunction(() => window.sockets[0].sent.filter((f) => f.type === 'resize').length === 6);
    assert.deepEqual((await sizes()).slice(4), [`${cols - 1}x${rows}`, `${cols}x${rows}`]);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('Retire in the bar: a finished agent retires at once and the page moves to the next row', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, errors, state } = await open(browser, { viewport: { width: 1440, height: 900 } });
    // A working agent asks first; Cancel leaves it alone.
    await page.goto(`${ORIGIN}/terminal/sm-1776`);
    await page.locator('.term-page[data-connection="Live"]').waitFor();
    await page.locator('.term-bar').getByRole('button', { name: 'Retire', exact: true }).click();
    assert.equal((await page.locator('.term-bar .retire-confirm').innerText()).replace(/\s+/g, ' '), 'Retire sm-1776? Cancel');
    assert.equal(await page.locator('.term-bar').evaluate((bar) => bar.scrollHeight <= bar.clientHeight), true, 'the confirmation stays on one line');
    await page.locator('.term-bar .retire-confirm').getByRole('button', { name: 'Cancel' }).click();
    assert.deepEqual(state.retired, []);
    // The finished, idle agent retires on one click and the terminal moves to the row below it.
    await page.locator('.sw-row', { hasText: 'far-1855' }).click();
    await page.waitForFunction(() => location.pathname === '/terminal/far-1855');
    await page.locator('.term-page[data-connection="Live"]').waitFor();
    await page.locator('.term-bar').getByRole('button', { name: 'Retire', exact: true }).click();
    await page.waitForFunction(() => location.pathname === '/terminal/sm-1776');
    assert.deepEqual(state.retired, [{ id: 'far-1855', body: { if_finished_idle: true } }]);
    await page.waitForFunction(() => ![...document.querySelectorAll('.sw-row .sw-nm')].some((n) => n.textContent === 'far-1855'));
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('Retire confirmation stays usable above details with a long name on a phone', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, state } = await open(browser, { viewport: { width: 390, height: 844 } });
    state.sessions = state.sessions.map((agent) => agent.id === 'sm-1776'
      ? { ...agent, name: 'sm-1776-with-a-very-long-name-that-must-wrap-inside-the-confirmation' }
      : agent);
    await page.goto(`${ORIGIN}/terminal/sm-1776?open=agent:sm-1776`);
    await page.locator('.term-panel').waitFor();
    await page.locator('.term-bar .retire-action > button').click();
    const prompt = page.locator('.term-bar .retire-confirm');
    await prompt.waitFor();
    const cancel = prompt.getByRole('button', { name: 'Cancel' });
    const box = await prompt.boundingBox();
    const cancelBox = await cancel.boundingBox();
    assert.ok(box.x >= 0 && box.x + box.width <= 390, 'confirmation fits inside the phone viewport');
    assert.ok(cancelBox.x >= 0 && cancelBox.x + cancelBox.width <= 390, 'Cancel stays in view');
    assert.equal(await cancel.evaluate((button) => {
      const box = button.getBoundingClientRect();
      return document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2) === button;
    }), true, 'details panel does not cover Cancel');
    await cancel.click();
    assert.deepEqual(state.retired, []);
  } finally { await browser.close(); }
});

test('Details opens beside the terminal on desktop and below it on a phone; a ticket opens there too', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const viewport of [{ width: 1440, height: 900 }, { width: 390, height: 844 }]) {
      const { page, state, errors } = await open(browser, { viewport });
      state.sessions = state.sessions.map((agent) => agent.id === 'sm-1776'
        ? { ...agent, claims: [{ kind: 'ticket', repo: 'owner/repo', number: 2200, title: 'Three issues' }] } : agent);
      await page.route(`${ORIGIN}/claims?**`, (route) => route.fulfill({ json: { claims: [] } }));
      await page.goto(`${ORIGIN}/terminal/sm-1776`);
      await page.locator('.term-page[data-connection="Live"]').waitFor();
      const before = await page.locator('.term-body').boundingBox();
      await page.locator('.term-bar').getByRole('button', { name: 'Details' }).click();
      const panel = page.locator('.term-panel');
      await panel.waitFor();
      assert.equal(new URL(page.url()).pathname, '/terminal/sm-1776', 'Details stays on the terminal page');
      assert.equal(new URL(page.url()).searchParams.get('open'), 'agent:sm-1776');
      const body = await page.locator('.term-body').boundingBox();
      const box = await panel.boundingBox();
      if (viewport.width > 900) {
        assert.ok(body.x + body.width <= box.x + 1, 'panel sits to the right of the terminal');
        assert.ok(body.width < before.width, 'terminal narrows to make room');
      } else {
        assert.ok(body.y + body.height <= box.y + 1, `panel sits below the terminal ${JSON.stringify({ body, box })}`);
        assert.ok(body.height < before.height, 'terminal shortens to make room');
      }
      // Beside its own terminal (#2277): details only. Every action is in the bar, so the panel
      // has no action row; one view, no tabs (#2280); no Note section until a note is pinned.
      await panel.locator('.agent-status .agent-ticket').waitFor();
      assert.equal(await panel.locator('.acts').count(), 0);
      assert.equal(await panel.locator('.tabs').count(), 0);
      assert.equal(await panel.locator('.pbody h3', { hasText: 'Note' }).count(), 0);
      // Rename from the bar's ⋯; on a phone, Hand off… folds into ⋯.
      const bar = page.locator('.term-bar');
      await bar.getByRole('button', { name: 'More' }).click();
      const menu = bar.locator('.pop.menu');
      assert.equal(await menu.getByRole('button', { name: 'Hand off…' }).count(), viewport.width > 900 ? 0 : 1);
      await menu.getByRole('button', { name: 'Rename…' }).click();
      const field = bar.getByRole('textbox', { name: 'Name' });
      await field.fill(viewport.width > 900 ? 'sm-1776-b' : 'sm-1776-c');
      await field.press('Enter');
      await panel.locator('.phd .t', { hasText: viewport.width > 900 ? 'sm-1776-b' : 'sm-1776-c' }).waitFor();
      assert.equal(state.renamed.at(-1).id, 'sm-1776');
      assert.equal(await bar.locator('.pop').count(), 0, 'the editor closes after a rename');
      if (SHOTS) await page.screenshot({ path: `${SHOTS}/terminal-details-${viewport.width}.png` });
      const ticket = panel.getByRole('button', { name: /#2200/ }).first();
      await ticket.waitFor();
      await ticket.click();
      await page.waitForFunction(() => new URLSearchParams(location.search).get('open') === 'ticket:owner/repo#2200');
      assert.equal(new URL(page.url()).pathname, '/terminal/sm-1776', 'a ticket opens beside the terminal too');
      await panel.locator('.ticket-reader').waitFor();
      assert.deepEqual(errors, []);
      await page.context().close();
    }
  } finally { await browser.close(); }
});

test('Report a bug: typing goes to the dialog, not the terminal', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page } = await open(browser, { viewport: { width: 1440, height: 900 } });
    await page.goto(`${ORIGIN}/terminal/sm-1776`);
    await page.locator('.term-page[data-connection="Live"]').waitFor();
    await page.locator('.term-bar .bug-btn').click();
    const field = page.locator('.board-start-overlay').locator('textarea, input[type="text"], input:not([type])').first();
    await field.waitFor();
    await field.click();
    // The bar's click handler hands focus back to the terminal on a timer; let it run.
    await page.waitForTimeout(100);
    await page.keyboard.type('broken');
    assert.equal(await field.inputValue(), 'broken');
    const typed = await page.evaluate(() => window.sockets.flatMap((s) => s.sent).filter((f) => f.type === 'input').map((f) => f.data).join(''));
    assert.equal(typed, '', 'no keystrokes reach the terminal');
  } finally { await browser.close(); }
});

test('route: another server on localhost and a failed direct socket both use Cloudflare', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    // The probe answers with another instance (say the MacBook's own server): skipped.
    const other = await open(browser, { viewport: { width: 1440, height: 900 }, instance: 'macbook' });
    await other.page.goto(`${ORIGIN}/terminal/iter8-run`);
    await routeTimed(other.page);
    assert.match(await routeShown(other.page), /^Cloudflare · \d+ ms$/);
    assert.deepEqual(await other.page.evaluate(() => window.sockets.map((s) => s.url)), ['wss://sm.example.com/client/terminal']);
    assert.equal(other.state.tickets, 1);
    await other.context.close();
    // The probe matches but the direct socket closes before attaching: a new ticket, through Cloudflare.
    const failed = await open(browser, { viewport: { width: 1440, height: 900 }, failDirect: true });
    await failed.page.goto(`${ORIGIN}/terminal/iter8-run`);
    await failed.page.locator('.term-page[data-connection="Live"]').waitFor();
    assert.deepEqual(await failed.page.evaluate(() => window.sockets.map((s) => s.url)),
      ['ws://localhost:8420/client/terminal', 'wss://sm.example.com/client/terminal']);
    assert.equal(failed.state.tickets, 2);
    assert.equal(JSON.parse(await failed.page.evaluate(() => sessionStorage.getItem('sm-term-route'))).url, 'relay');
    assert.deepEqual(failed.errors, []);
  } finally { await browser.close(); }
});

test('links: a printed URL and an OSC 8 link open in a new tab, with tmux mouse mode on', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, errors } = await open(browser, { viewport: { width: 1440, height: 900 } });
    // tmux with `mouse on` turns on SGR mouse tracking before drawing the pane.
    await page.addInitScript(() => {
      window.termOutput = '\x1b[?1000h\x1b[?1006h'
        + 'PR: https://github.com/rajeshgoli/session-manager/pull/2177\r\n'
        + '\x1b]8;;https://sm.example.com/docs/memo\x07the memo\x1b]8;;\x07\r\n'
        + '\x1b]8;;javascript:alert(1)\x07bad\x1b]8;;\x07\r\n';
      window.opened = [];
      window.open = (url, target, features) => { window.opened.push([url, target, features]); return null; };
    });
    await page.goto(`${ORIGIN}/terminal/iter8-run`);
    await page.locator('.term-page[data-connection="Live"]').waitFor();
    await page.locator('.xterm-rows', { hasText: 'the memo' }).waitFor();
    const cell = await page.evaluate(() => {
      const rows = document.querySelector('.xterm-rows').getBoundingClientRect();
      const row = document.querySelector('.xterm-rows > div');
      return { x: rows.left, y: rows.top, h: row.getBoundingClientRect().height };
    });
    const cw = await page.evaluate(() => {
      const span = document.createElement('span'); span.textContent = 'M'.repeat(100);
      document.querySelector('.xterm-rows > div').appendChild(span);
      const w = span.getBoundingClientRect().width / 100; span.remove(); return w;
    });
    const at = (col, row) => [cell.x + (col + 0.5) * cw, cell.y + (row + 0.5) * cell.h];
    const click = async (col, row) => {
      const [x, y] = at(col, row);
      await page.mouse.move(x, y);
      await page.waitForTimeout(100);
      await page.mouse.click(x, y);
    };
    await click(10, 0);
    await click(2, 1);
    await click(1, 2);
    assert.deepEqual(await page.evaluate(() => window.opened), [
      ['https://github.com/rajeshgoli/session-manager/pull/2177', '_blank', 'noopener,noreferrer'],
      ['https://sm.example.com/docs/memo', '_blank', 'noopener,noreferrer'],
    ]);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('terminal theme: picked in Settings, light or dark from Appearance, switching live (#2304)', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, errors } = await open(browser, { viewport: { width: 1440, height: 900 } });
    await page.goto(`${ORIGIN}/settings#appearance`);
    const pick = page.getByRole('radio', { name: /Solarized/ });
    await pick.click();
    assert.equal(await pick.getAttribute('aria-checked'), 'true');
    assert.equal(await page.evaluate(() => localStorage.getItem('sm-term-theme')), '"solarized"');
    if (SHOTS) await page.locator('.term-themes').screenshot({ path: `${SHOTS}/settings-terminal-themes.png` });
    // The cards redraw when the OS turns dark under Theme: System.
    const card = () => page.evaluate(() => getComputedStyle(document.querySelector('.term-theme.on .term-theme-sample')).backgroundColor);
    assert.equal(await card(), 'rgb(253, 246, 227)');
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.waitForFunction(() => getComputedStyle(document.querySelector('.term-theme.on .term-theme-sample')).backgroundColor === 'rgb(0, 43, 54)');
    await page.emulateMedia({ colorScheme: 'light' });

    await page.goto(`${ORIGIN}/terminal/iter8-run`);
    await page.locator('.term-body .xterm').waitFor();
    const background = () => page.evaluate(() => getComputedStyle(document.querySelector('.term-body')).backgroundColor);
    assert.equal(await background(), 'rgb(253, 246, 227)', 'Solarized light on a light page');
    // System follows the OS while the page is open.
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.waitForFunction(() => getComputedStyle(document.querySelector('.term-body')).backgroundColor === 'rgb(0, 43, 54)');
    // An explicit Theme wins over the OS.
    await page.evaluate(() => { document.documentElement.dataset.theme = 'light'; });
    await page.waitForFunction(() => getComputedStyle(document.querySelector('.term-body')).backgroundColor === 'rgb(253, 246, 227)');
    if (SHOTS) await page.screenshot({ path: `${SHOTS}/terminal-solarized-light.png` });
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});
