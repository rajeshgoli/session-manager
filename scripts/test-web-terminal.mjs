// Terminal page in a browser (spec 1782 G1-G4): switcher, keys only on phones, route and round trip.
// Run with node --test; install playwright or set PLAYWRIGHT_MODULE to its entry point.
// Set SHOTS_DIR to also save the 1440/390 px, light/dark, 15/19 px screenshots there.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';
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
        reply({ type: 'output', sequence: 1, data: btoa('rajesh@studio ~ % ') }, 10);
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
  const state = { sessions: fixture(), answered: [], tickets: 0 };
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
    if (url.pathname === '/watch/state') return route.fulfill({ json: { sessions: state.sessions, counts: {} } });
    return route.fulfill({ json: {} });
  });
  return { context, page, errors, state };
}

const rows = (page) => page.locator('.sw-row .sw-nm').allInnerTexts();
const routeShown = (page) => page.locator('.term-route').innerText();

test('switcher, keys and route at desktop and phone sizes', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const shots = process.env.SHOTS_DIR;
  if (shots) await mkdir(shots, { recursive: true });
  try {
    for (const viewport of [{ width: 1440, height: 900 }, { width: 390, height: 844 }]) {
      for (const colorScheme of ['light', 'dark']) {
        for (const size of [15, 19]) {
          const phone = viewport.width < 900;
          const { context, page, errors } = await open(browser, { viewport, colorScheme, size, touch: phone });
          await page.goto(`${ORIGIN}/terminal/iter8-run`);
          await page.getByText('Live', { exact: true }).waitFor();
          await page.waitForFunction(() => /ms$/.test(document.querySelector('.term-route')?.textContent || ''));
          assert.match(await routeShown(page), /^Direct · \d+ ms$/);
          // G2: key buttons on phones only.
          assert.equal(await page.locator('.term-keys').count(), phone ? 1 : 0);
          if (phone) {
            // G1: hidden by default below 900 px; the bar button opens it over the terminal.
            assert.equal(await page.locator('.term-switch').count(), 0);
            await page.getByRole('button', { name: 'Agents' }).click();
          }
          await page.locator('.sw-row').first().waitFor();
          assert.deepEqual(await page.locator('.sw-sec').allInnerTexts(), ['NEEDS YOU', 'FINISHED', 'MOVING', 'IDLE · 3 ›']);
          assert.deepEqual(await rows(page), ['sm-1726-engineer', 'far-1855', 'sm-1776', 'iter8-run']);
          assert.equal(await page.locator('.sw-row.cur .sw-nm').innerText(), 'iter8-run');
          assert.equal(await page.locator('.sw-row.cur .sw-fact').innerText(), '▶ 2 running · 2h 56m');
          assert.match(await page.locator('.sw-row').first().locator('.sw-fact').innerText(), /^◆ 7m: one manual Chrome check/);
          assert.equal(await page.locator('.sw-row .ok').count(), 1, 'only the dismissible question has ✓');
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
    await page.goto(`${ORIGIN}/terminal/iter8-run`);
    await page.getByText('Live', { exact: true }).waitFor();
    // iter8-run is the last row: ⌘⌥↓ stays, ⌘⌥↑ goes to the row above and reattaches.
    await page.keyboard.press('Meta+Alt+ArrowDown');
    assert.equal(new URL(page.url()).pathname, '/terminal/iter8-run');
    await page.keyboard.press('Meta+Alt+ArrowUp');
    await page.waitForFunction(() => location.pathname === '/terminal/sm-1776');
    await page.waitForFunction(() => window.sockets.length === 2 && window.sockets[1].sent.some((f) => f.type === 'auth'));
    assert.ok(await page.evaluate(() => window.sockets[0].sent.some((f) => f.type === 'detach')), 'the old socket detaches');
    assert.equal(await page.evaluate(() => window.sockets[1].sent.find((f) => f.type === 'auth').ticket_id), 't2');
    assert.equal(await page.locator('.sw-row.cur .sw-nm').innerText(), 'sm-1776');
    await page.getByText('Live', { exact: true }).waitFor();
    await page.keyboard.press('Meta+Alt+ArrowUp');
    await page.waitForFunction(() => location.pathname === '/terminal/far-1855');
    await page.keyboard.press('Meta+Alt+ArrowDown');
    await page.waitForFunction(() => location.pathname === '/terminal/sm-1776');
    // A click on a row switches too, and focus goes back to the terminal.
    await page.locator('.sw-row', { hasText: 'iter8-run' }).click();
    await page.waitForFunction(() => location.pathname === '/terminal/iter8-run');
    await page.getByText('Live', { exact: true }).waitFor();
    await page.waitForFunction(() => document.activeElement?.classList.contains('xterm-helper-textarea'));
    // The idle section opens and is remembered.
    await page.getByRole('button', { name: /Idle · 3/ }).click();
    assert.deepEqual((await rows(page)).slice(-3), ['sm-1768', 'sm-1727', 'sm-scout']);
    assert.equal(await page.evaluate(() => localStorage.getItem('sm-term-switcher-idle')), 'true');
    // ✓ clears the question without switching.
    await page.getByRole('button', { name: 'Mark sm-1726-engineer answered', exact: true }).click();
    await page.waitForFunction(() => document.querySelectorAll('.sw-row .ok').length === 0);
    assert.deepEqual(state.answered, ['sm-1726-engineer']);
    assert.equal(new URL(page.url()).pathname, '/terminal/iter8-run');
    // ⌘\ hides the switcher and remembers it.
    await page.keyboard.press('Meta+Backslash');
    await page.waitForFunction(() => !document.querySelector('.term-switch'));
    assert.equal(await page.evaluate(() => localStorage.getItem('sm-term-switcher')), 'false');
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('route: another server on localhost and a failed direct socket both use Cloudflare', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    // The probe answers with another instance (say the MacBook's own server): skipped.
    const other = await open(browser, { viewport: { width: 1440, height: 900 }, instance: 'macbook' });
    await other.page.goto(`${ORIGIN}/terminal/iter8-run`);
    await other.page.waitForFunction(() => /ms$/.test(document.querySelector('.term-route')?.textContent || ''));
    assert.match(await routeShown(other.page), /^Cloudflare · \d+ ms$/);
    assert.deepEqual(await other.page.evaluate(() => window.sockets.map((s) => s.url)), ['wss://sm.example.com/client/terminal']);
    assert.equal(other.state.tickets, 1);
    await other.context.close();
    // The probe matches but the direct socket closes before attaching: a new ticket, through Cloudflare.
    const failed = await open(browser, { viewport: { width: 1440, height: 900 }, failDirect: true });
    await failed.page.goto(`${ORIGIN}/terminal/iter8-run`);
    await failed.page.getByText('Live', { exact: true }).waitFor();
    assert.deepEqual(await failed.page.evaluate(() => window.sockets.map((s) => s.url)),
      ['ws://localhost:8420/client/terminal', 'wss://sm.example.com/client/terminal']);
    assert.equal(failed.state.tickets, 2);
    assert.equal(JSON.parse(await failed.page.evaluate(() => sessionStorage.getItem('sm-term-route'))).url, 'relay');
    assert.deepEqual(failed.errors, []);
  } finally { await browser.close(); }
});
