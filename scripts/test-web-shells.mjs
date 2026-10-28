// Plain shell web journeys. Run with node --test; PLAYWRIGHT_MODULE may point to playwright-core/index.mjs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const ORIGIN = 'https://sm.test';
const doc = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/vendor/xterm.css">
<script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script>
<script id="sm-config" type="application/json">{"inbox_token":"fixture","refresh_seconds":3}</script>
<script type="module" src="/assets/app.js"></script></head><body><div id="app"></div></body></html>`;
const cwd = '/Users/r/projects/session-manager';
const row = (id, shell = false, busy = false) => ({
  id, name: shell ? 'zsh · session-manager' : id, kind: shell ? 'shell' : 'agent', provider: shell ? 'shell' : 'claude',
  state: busy ? 'working' : 'idle', working_dir: cwd, repo: cwd, claims: [], jobs: [], docs: [], waiting_on: [],
  facts: shell ? { text: busy ? 'running · cargo bench' : 'last input 12m', agent: { state: busy ? 'working' : 'idle' } }
    : { agent: { state: 'idle' }, jobs: { text: 'No jobs' } },
  attention: { section: busy ? 'moving' : 'idle', order_key: id },
});
const settings = () => ({ new_agent: { provider: 'claude', claude: {}, codex: {}, workspaces: [cwd], agent_types: [] },
  terminal_limits: {}, terminal_config_limits: { plain_terminal_enabled: true, max_plain_terminals: 4, idle_reap_minutes: 180, tunnel_linger_minutes: 15 } });
function sockets() {
  window.sockets = [];
  window.WebSocket = class {
    static OPEN = 1;
    constructor(url) {
      this.url = url; this.readyState = 0; window.sockets.push(this);
      setTimeout(() => {
        if (window.failDirect) { this.readyState = 3; this.onclose({ code: 1006 }); return; }
        this.readyState = 1; this.onopen();
      }, 5);
    }
    send(text) {
      const frame = JSON.parse(text);
      if (frame.type === 'auth') setTimeout(() => this.onmessage({ data: JSON.stringify({ type: 'status', state: 'attached' }) }), 5);
    }
    close() { this.readyState = 3; }
  };
  navigator.clipboard.writeText = async text => { window.copied = text; };
}
async function setup(browser, { width = 1440, home = false, failDirect = false, colorScheme = 'light', rememberedRelay = false } = {}) {
  const page = await browser.newPage({ viewport: { width, height: 1000 }, colorScheme });
  await page.addInitScript(sockets);
  if (rememberedRelay) await page.addInitScript(() => sessionStorage.setItem('sm-term-route', JSON.stringify({ url: 'relay', instance: 'one', at: Date.now() })));
  await page.addInitScript(value => { window.failDirect = value; }, failDirect);
  const errors = [], requests = [];
  page.on('pageerror', e => errors.push(e.message));
  const state = { sessions: [row('agent'), row('term-abcd', true), row('term-busy', true, true)], settings: settings(),
    keys: [{ fingerprint: 'SHA256:abc+/=', public_key: 'ssh-ed25519 AAAA fixture' }, { fingerprint: 'SHA256:revoked', revoked_at: '2026-10-19' }], routeError: null, createError: null, home, probes: 0 };
  await page.route('**/*', async route => {
    const req = route.request(), url = new URL(req.url()), path = url.pathname;
    const json = (body, status = 200) => route.fulfill({ json: body, status });
    if (path.startsWith('/assets/')) return route.fulfill({ body: await readFile(new URL(path.slice(8), assets)), contentType: path.endsWith('.css') ? 'text/css' : 'text/javascript' });
    if (req.isNavigationRequest()) return route.fulfill({ body: doc.replace('<html>', `<html data-theme="${colorScheme}">`), contentType: 'text/html' });
    if (path === '/client/terminal/probe') {
      state.probes++;
      return state.home ? route.fulfill({ json: { instance: 'one' }, headers: { 'access-control-allow-origin': ORIGIN } }) : route.abort();
    }
    if (req.method() !== 'GET') requests.push({ path, method: req.method(), body: req.postData() ? req.postDataJSON() : null });
    if (path === '/client/board') return json({ lanes: [], other: [] });
    if (path === '/watch/state') {
      if (state.watchWait) await state.watchWait;
      if (state.watchError) return json({ detail: 'watch unavailable' }, 503);
      return json({ sessions: state.sessions, counts: state.counts || {} });
    }
    if (/browser-attach-ticket$/.test(path)) return json({ ws_url: '/client/terminal', ticket_id: 'one', ticket_secret: 'secret', server_instance: 'one',
      direct: [{ url: 'ws://localhost:8420/client/terminal', probe: 'http://localhost:8420/client/terminal/probe' }] });
    if (path === '/terminals' && req.method() === 'POST') {
      if (state.createError) return json(state.createError, 409);
      return json({ id: 'term-abcd', cwd }, 201);
    }
    if (/^\/terminals\/[^/]+\/route$/.test(path)) {
      const away = req.postDataJSON().where === 'away';
      if (away && state.routeError) return json({ detail: state.routeError }, 409);
      return json({ ssh_host: away ? 'studio-away' : 'studio-lan.rajeshgo.li', ssh_user: 'rajesh', ssh_port: 22222, tmux_session: 'term-abcd',
        command: away ? `ssh -t studio-away tmux -L 'session-manager' attach-session -t term-abcd` : `ssh -t -p 22222 rajesh@studio-lan.rajeshgo.li tmux -L 'session-manager' attach-session -t term-abcd` });
    }
    if (/^\/terminals\/[^/]+$/.test(path) && req.method() === 'PATCH') {
      const id = path.split('/').at(-1), body = req.postDataJSON();
      if (body.name?.length > 40) return json({ detail: 'name_too_long' }, 400);
      state.sessions = state.sessions.map(s => s.id !== id ? s : { ...s,
        ...('name' in body ? { name: body.name?.trim() || 'zsh · session-manager', named: !!body.name?.trim() } : {}),
        ...('hidden' in body ? { hidden: body.hidden } : {}) });
      const t = state.sessions.find(s => s.id === id);
      return json({ id, name: t.name, named: !!t.named, hidden: !!t.hidden });
    }
    if (/^\/terminals\/[^/]+$/.test(path) && req.method() === 'DELETE') {
      state.sessions = state.sessions.filter(s => s.id !== path.split('/').at(-1)); return json({});
    }
    if (path === '/client/settings') {
      if (req.method() === 'PUT') Object.assign(state.settings.terminal_limits, req.postDataJSON().terminal_limits);
      return json(state.settings);
    }
    if (path === '/client/devices') return json({ devices: [{ user_id: 'owner', device_key_id: 'macbook', device_name: 'MacBook', kind: 'computer' }] });
    if (/^\/devices\/macbook\/ssh-keys/.test(path)) {
      if (req.method() === 'POST') state.keys.push({ fingerprint: 'SHA256:new', public_key: req.postDataJSON().public_key });
      if (req.method() === 'DELETE') state.keys = state.keys.filter(k => k.fingerprint !== decodeURIComponent(path.split('/').at(-1)));
      return json({ keys: state.keys });
    }
    return json({});
  });
  return { page, requests, state, errors };
}

for (const width of [390, 1440]) test(`Stale terminal links explain the ended state and open another agent at ${width}px`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const id of ['retired-agent', 'term-retired', 'missing', 'stopped-agent', 'term-stopped']) {
      const { page, requests, state, errors } = await setup(browser, { width });
      if (id.includes('stopped')) state.sessions.push({ ...row(id, id.startsWith('term-')), state: 'stopped', attention: { section: 'stopped' } });
      await page.goto(`${ORIGIN}/terminal/${id}`);
      await page.waitForSelector('[data-connection="Ended"]');
      assert.match(await page.locator('.term-notice').innerText(), /ended or could not be found.*stopped or been retired/s);
      assert.equal(await page.getByRole('button', { name: 'Reconnect', exact: true }).count(), 0);
      assert.equal(await page.evaluate(() => window.sockets.length), 0);
      assert.equal(requests.filter(r => /browser-attach-ticket$/.test(r.path)).length, 0);
      await page.getByRole('link', { name: 'Open another agent', exact: true }).click();
      // `/` lands on the top sidebar row (here the first shell) on a computer and stays the agents list on a phone (2543 D.5).
      await page.waitForURL(width > 720 ? `${ORIGIN}/terminal/term-abcd` : `${ORIGIN}/`);
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); }
});

test('Initial watch failure offers Retry and waits for identity before attaching', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const id of ['agent', 'term-abcd']) {
      const { page, requests, state, errors } = await setup(browser);
      state.watchError = true;
      await page.goto(`${ORIGIN}/terminal/${id}`);
      await page.waitForSelector('[data-connection="Ended"]');
      assert.match(await page.locator('.term-notice').innerText(), /Could not load session information/);
      assert.equal(await page.evaluate(() => window.sockets.length), 0);
      assert.equal(requests.filter(r => /browser-attach-ticket$/.test(r.path)).length, 0);
      let release;
      state.watchWait = new Promise(resolve => { release = resolve; });
      state.watchError = false;
      const pending = page.waitForRequest('**/watch/state');
      await page.getByRole('button', { name: 'Retry', exact: true }).click();
      await pending;
      assert.equal(await page.evaluate(() => window.sockets.length), 0);
      assert.equal(requests.filter(r => /browser-attach-ticket$/.test(r.path)).length, 0);
      release();
      if (id === 'agent') await page.waitForSelector('[data-connection="Live"]');
      else {
        await page.getByRole('heading', { name: 'Connect to your shell' }).waitFor();
        assert.equal(await page.evaluate(() => window.sockets.length), 0);
      }
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); }
});

test('A stopped shell replaces its SSH instructions with an actionable ended state', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, state, errors } = await setup(browser);
    await page.goto(`${ORIGIN}/terminal/term-abcd`);
    await page.getByRole('heading', { name: 'Connect to your shell' }).waitFor();
    state.sessions = state.sessions.map(s => s.id === 'term-abcd' ? { ...s, state: 'stopped' } : s);
    await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
    await page.waitForSelector('[data-connection="Ended"]');
    await page.getByRole('link', { name: 'Open another agent', exact: true }).waitFor();
    assert.equal(await page.getByRole('heading', { name: 'Connect to your shell' }).count(), 0);
    assert.equal(await page.evaluate(() => window.sockets.length), 0);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

for (const colorScheme of ['light', 'dark']) for (const width of [390, 1440]) test(`Shell Close header and connection card at ${width}px in ${colorScheme}`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, requests, state, errors } = await setup(browser, { width, colorScheme });
    await page.goto(`${ORIGIN}/terminal/term-abcd`);
    await page.getByRole('heading', { name: 'Connect to your shell' }).waitFor();
    await page.getByRole('button', { name: 'Copy SSH command', exact: true }).click();
    assert.equal(await page.evaluate(() => window.copied), `ssh -t studio-away tmux -L 'session-manager' attach-session -t term-abcd`);
    assert.equal(await page.evaluate(() => window.sockets.length), 0);
    assert.equal(requests.filter(r => r.body?.where === 'away').length, 0);
    // The shell bar is the agent bar: Notes, Details, Hide and ⋯ (the phone bar adds sm home); Close sits where Retire does, in red.
    assert.equal(await page.getByText('Hand off…', { exact: true }).count(), 0);
    assert.equal(await page.locator('.term-bar .term-home').count(), width < 900 ? 1 : 0);
    for (const name of ['Details', 'Hide', 'More', 'Close shell']) assert.equal(await page.locator('.term-bar').getByRole('button', { name, exact: true }).count(), 1, name);
    assert.equal(await page.locator('.term-bar .ring').count(), 0);
    assert.match(await page.locator('.term-bar').getByRole('button', { name: 'Close shell', exact: true }).getAttribute('class'), /\bdanger\b/);
    assert.equal(await page.locator('.term-bar .term-connection').count(), 0, 'the SSH card already says how to connect');
    state.routeError = 'no_ssh_key';
    await page.getByRole('button', { name: 'Open tunnel', exact: true }).click();
    await page.getByRole('alert').filter({ hasText: 'Add an SSH public key' }).waitFor();
    state.routeError = null;
    await page.getByRole('button', { name: 'Open tunnel', exact: true }).click();
    await page.getByText('Tunnel open', { exact: true }).waitFor();
    if (width === 1440) {
      const busy = page.locator('.sw-row').filter({ hasText: 'running · cargo bench' });
      assert.equal(await busy.locator('.sw-shell').textContent(), '▹');
      assert.equal(await busy.locator('.sw-fact').getAttribute('class'), 'sw-fact muted');
    }
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    if (process.env.SHOTS_DIR) { await mkdir(process.env.SHOTS_DIR, { recursive: true }); await page.screenshot({ path: `${process.env.SHOTS_DIR}/shell-away-${width}-${colorScheme}.png` }); }
    const headerClose = page.getByRole('button', { name: 'Close shell', exact: true });
    await headerClose.click();
    await page.getByRole('alertdialog', { name: 'Close this shell?', exact: true }).waitFor();
    if (process.env.SHOTS_DIR) await page.screenshot({ path: `${process.env.SHOTS_DIR}/shell-close-${width}-${colorScheme}.png` });
    await page.getByRole('button', { name: 'Keep open', exact: true }).click();
    assert.equal(requests.filter(r => r.method === 'DELETE').length, 0);
    await headerClose.click();
    await page.getByRole('alertdialog').getByRole('button', { name: 'Close shell', exact: true }).click();
    await page.waitForURL(url => url.pathname !== '/terminal/term-abcd');
    assert.ok(requests.some(r => r.path === '/terminals/term-abcd' && r.method === 'DELETE'));
    assert.ok(!requests.some(r => /\/sessions\/term-abcd\/(retire|restore)/.test(r.path)));
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

for (const failDirect of [false, true]) test(`Home shell attaches directly; failure never opens public relay: ${failDirect}`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, errors } = await setup(browser, { home: true, failDirect });
    await page.goto(`${ORIGIN}/terminal/term-abcd`);
    if (failDirect) await page.getByRole('heading', { name: 'Connect to your shell' }).waitFor();
    else await page.waitForSelector('[data-connection="Live"]');
    const urls = await page.evaluate(() => window.sockets.map(s => s.url));
    assert.deepEqual(urls, ['ws://localhost:8420/client/terminal']);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('Agent New shell here follows Clone, reports cap, then opens shell from that session', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, requests, state, errors } = await setup(browser);
    await page.goto(`${ORIGIN}/terminal/agent`);
    await page.waitForSelector('[data-connection="Live"]');
    await page.getByRole('button', { name: 'More', exact: true }).click();
    const buttons = await page.locator('.pop.menu button').allTextContents();
    assert.equal(buttons[buttons.indexOf('Clone') + 1], 'New shell here');
    state.createError = { detail: 'too_many_terminals', terminals: [{ id: 'term-old', cwd }] };
    await page.getByRole('button', { name: 'New shell here', exact: true }).click();
    await page.getByText(/Too many open shells.*term-old/).waitFor();
    state.createError = null;
    await page.getByRole('button', { name: 'New shell here', exact: true }).click();
    await page.waitForURL('**/terminal/term-abcd');
    assert.deepEqual(requests.filter(r => r.path === '/terminals').at(-1).body, { from_session: 'agent' });
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('New agent Terminal requests only folder; Settings save limits and manage device keys', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, requests, errors } = await setup(browser);
    await page.goto(`${ORIGIN}/`);
    await page.getByRole('button', { name: 'New agent', exact: true }).click();
    await page.getByRole('button', { name: /▹ Terminal/ }).click();
    assert.equal(await page.getByText('First message', { exact: true }).count(), 0);
    assert.equal(await page.locator('.pop input').count(), 0);
    assert.equal(await page.getByText('Model', { exact: true }).count(), 0);
    await page.getByRole('button', { name: 'Start', exact: true }).click();
    await page.waitForURL('**/terminal/term-abcd');
    assert.deepEqual(requests.find(r => r.path === '/terminals').body, { cwd });
    await page.goto(`${ORIGIN}/settings#terminals`);
    const limit = page.getByRole('spinbutton', { name: 'Open shells', exact: true });
    await limit.fill('6'); await limit.blur();
    await page.waitForResponse(r => new URL(r.url()).pathname === '/client/settings' && r.request().method() === 'PUT');
    assert.equal(requests.filter(r => r.path === '/client/settings').at(-1).body.terminal_limits.max_plain_terminals, 6);
    assert.equal(await page.getByRole('spinbutton', { name: 'Close idle shells after (minutes)', exact: true }).getAttribute('min'), '15');
    assert.equal(await page.getByRole('spinbutton', { name: 'Close unused SSH tunnel after (minutes)', exact: true }).getAttribute('max'), '120');
    await page.getByRole('switch', { name: 'Enable plain shells', exact: true }).click();
    await page.waitForFunction(() => document.body.textContent.includes('Saved'));
    await page.goto(`${ORIGIN}/settings#devices-access`);
    const keys = page.getByRole('region', { name: 'SSH keys for MacBook', exact: true });
    await keys.getByText('SHA256:abc+/=', { exact: true }).waitFor();
    assert.equal(await keys.getByText('SHA256:revoked', { exact: true }).count(), 0);
    await keys.getByRole('button', { name: 'Add key', exact: true }).click();
    await keys.getByRole('textbox').fill('ssh-rsa not-supported');
    await keys.getByRole('button', { name: 'Save key', exact: true }).click();
    await keys.getByText('Paste one Ed25519 public key beginning with ssh-ed25519.').waitFor();
    await keys.getByRole('textbox').fill('ssh-ed25519 AAAA test-device');
    await keys.getByRole('button', { name: 'Save key', exact: true }).click();
    await keys.getByText('SHA256:new', { exact: true }).waitFor();
    await keys.getByRole('button', { name: 'Remove SSH key SHA256:abc+/=', exact: true }).click();
    await keys.getByText('SHA256:abc+/=', { exact: true }).waitFor({ state: 'detached' });
    assert.ok(requests.some(r => r.path === '/devices/macbook/ssh-keys/SHA256%3Aabc%2B%2F%3D' && r.method === 'DELETE'));
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});


test('Studio shell ignores remembered Cloudflare route and uses localhost', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, state, errors } = await setup(browser, { home: true, rememberedRelay: true });
    await page.goto(`${ORIGIN}/terminal/term-abcd`);
    await page.waitForSelector('[data-connection="Live"]');
    assert.ok(state.probes > 0);
    assert.deepEqual(await page.evaluate(() => window.sockets.map(s => s.url)), ['ws://localhost:8420/client/terminal']);
    await page.locator('.term-bar').getByRole('button', { name: 'Close shell', exact: true }).waitFor();
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

for (const failDirect of [false, true]) test(`Shell can retry a local connection after ${failDirect ? 'a failed socket' : 'an unavailable probe'}`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, state, requests, errors } = await setup(browser, { home: failDirect, failDirect });
    await page.goto(`${ORIGIN}/terminal/term-abcd`);
    await page.getByRole('heading', { name: 'Connect to your shell' }).waitFor();
    if (failDirect) await page.getByText('The local connection ended before the shell could open.', { exact: false }).waitFor();
    state.home = true;
    await page.evaluate(() => { window.failDirect = false; });
    await page.getByRole('button', { name: 'Connect in browser', exact: true }).click();
    await page.waitForSelector('[data-connection="Live"]');
    assert.ok(state.probes >= 2);
    assert.ok((await page.evaluate(() => window.sockets.map(s => s.url))).every(url => url === 'ws://localhost:8420/client/terminal'));
    assert.equal(requests.filter(r => r.body?.where === 'away').length, 0);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

for (const colorScheme of ['light', 'dark']) for (const width of [390, 1440]) test(`Shells are sidebar rows and a dedicated switcher group at ${width}px in ${colorScheme}`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, state, errors } = await setup(browser, { width, colorScheme });
    state.counts = { idle: 2, moving: 1, live: 3, needs_you: 0 };
    state.sessions = state.sessions.map(s => s.kind === 'shell' ? { ...s, claims: [{ title: 'Should never be a shell ticket', kind: 'ticket', number: 99 }] } : s);
    // A computer lists sessions in the sidebar on every page; a phone's `/` is the agents list (2543 B.10, D.5).
    await page.goto(width > 720 ? `${ORIGIN}/board` : `${ORIGIN}/`);
    const shells = width > 720 ? page.locator('.sidebar .sb-line', { has: page.locator('.sw-shell') }) : page.locator('.home-list .sw-row', { has: page.locator('.sw-shell') });
    await shells.first().waitFor();
    assert.equal(await shells.count(), 2);
    if (width > 720) {
      assert.deepEqual(await shells.locator('.sb-fact').allTextContents(), ['shell', 'shell']);
      assert.equal((await page.locator('.sb-head .sb-live').innerText()).trim(), '1 live ›', 'shells are not counted as agents');
    }
    assert.equal(await page.getByText('Should never be a shell ticket', { exact: true }).count(), 0);
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    if (process.env.SHOTS_DIR) {
      await mkdir(process.env.SHOTS_DIR, { recursive: true });
      await page.screenshot({ path: `${process.env.SHOTS_DIR}/shell-rows-${width}-${colorScheme}.png`, fullPage: true });
    }
    await shells.filter({ hasText: 'zsh · session-manager' }).first().click();
    await page.waitForURL('**/terminal/term-abcd');
    await page.getByRole('heading', { name: 'Connect to your shell' }).waitFor();
    if (width < 900) await page.getByRole('button', { name: 'Unfold sidebar', exact: true }).click();
    await page.getByText('Terminals', { exact: true }).waitFor();
    const sections = (await page.locator('.term-switch .sw-sec').allTextContents()).map(text => text.trim());
    assert.ok(sections.includes('Terminals'));
    assert.ok(!sections.includes('Moving'), 'a running shell does not create a moving agent section');
    assert.ok(sections.some(text => text.startsWith('Idle · 1')), 'only the real agent is idle');
    assert.equal(await page.locator('.sw-row').count(), 2, 'both shells remain visible even with Idle folded');
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

for (const width of [390, 1440]) test(`Shell renames in place from the bar and from ⋯ at ${width}px`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, requests, state, errors } = await setup(browser, { width, home: true });
    await page.goto(`${ORIGIN}/terminal/term-abcd`);
    await page.waitForSelector('[data-connection="Live"]');
    const patches = () => requests.filter(r => r.method === 'PATCH');
    await page.getByRole('button', { name: 'Rename zsh · session-manager', exact: true }).click();
    const field = page.getByRole('textbox', { name: 'Shell name', exact: true });
    assert.equal(await field.inputValue(), 'zsh · session-manager');
    await page.waitForFunction(() => { const el = document.activeElement; return el?.classList.contains('shell-name-input') && el.selectionEnd - el.selectionStart === el.value.length; });
    await field.fill('throwaway');
    await field.press('Escape');
    await page.getByRole('button', { name: 'Rename zsh · session-manager', exact: true }).waitFor();
    assert.equal(patches().length, 0);
    await page.getByRole('button', { name: 'Rename zsh · session-manager', exact: true }).click();
    await field.fill('  bench runner ');
    await field.press('Enter');
    await page.getByRole('button', { name: 'Rename bench runner', exact: true }).waitFor();
    assert.deepEqual(patches().at(-1), { path: '/terminals/term-abcd', method: 'PATCH', body: { name: 'bench runner' } });
    if (process.env.SHOTS_DIR) { await mkdir(process.env.SHOTS_DIR, { recursive: true }); await page.screenshot({ path: `${process.env.SHOTS_DIR}/shell-renamed-${width}.png` }); }
    // ⋯ › Rename… edits the same name in the bar; blank restores the default.
    await page.getByRole('button', { name: 'More', exact: true }).click();
    await page.getByRole('button', { name: 'Rename…', exact: true }).click();
    await field.fill('');
    await field.press('Enter');
    await page.getByRole('button', { name: 'Rename zsh · session-manager', exact: true }).waitFor();
    assert.deepEqual(patches().at(-1).body, { name: null });
    // Too long: the server refuses, the field stays open with the text.
    await page.getByRole('button', { name: 'Rename zsh · session-manager', exact: true }).click();
    await field.evaluate(el => el.removeAttribute('maxlength'));
    await field.fill('x'.repeat(41));
    await field.press('Enter');
    await page.getByText('Shell names are at most 40 characters.').waitFor();
    assert.equal(await field.inputValue(), 'x'.repeat(41));
    assert.equal(state.sessions.find(s => s.id === 'term-abcd').name, 'zsh · session-manager');
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

for (const width of [390, 1440]) test(`Hide sends a shell to the tray and Undo brings it back at ${width}px`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, requests, state, errors } = await setup(browser, { width, home: true });
    await page.goto(`${ORIGIN}/terminal/term-abcd`);
    await page.waitForSelector('[data-connection="Live"]');
    await page.locator('.term-bar').getByRole('button', { name: 'Hide', exact: true }).click();
    await page.waitForURL(url => url.pathname !== '/terminal/term-abcd');
    assert.deepEqual(requests.filter(r => r.method === 'PATCH').at(-1).body, { hidden: true });
    assert.equal(state.sessions.find(s => s.id === 'term-abcd').hidden, true);
    // The shell leaves the switcher; an eye with a count appears in the top bar.
    if (width < 900) await page.getByRole('button', { name: 'Unfold sidebar', exact: true }).click();
    await page.locator('.sw-row').filter({ hasText: 'cargo bench' }).waitFor();
    assert.equal(await page.locator('.sw-row').filter({ hasText: 'last input' }).count(), 0);
    const eye = page.locator('.term-bar').getByRole('button', { name: '1 hidden terminal', exact: true });
    await eye.click();
    const hiddenRow = page.locator('.hidden-shell');
    assert.equal(await hiddenRow.count(), 1);
    if (process.env.SHOTS_DIR) { await mkdir(process.env.SHOTS_DIR, { recursive: true }); await page.screenshot({ path: `${process.env.SHOTS_DIR}/shell-eye-${width}.png` }); }
    // Opening a hidden shell keeps it hidden until Unhide.
    await hiddenRow.locator('.hidden-shell-open').click();
    await page.waitForURL('**/terminal/term-abcd');
    await page.locator('.term-bar .shell-hidden-pill').waitFor();
    await page.locator('.term-bar').getByRole('button', { name: 'Unhide', exact: true }).click();
    await page.locator('.term-bar').getByRole('button', { name: 'Hide', exact: true }).waitFor();
    assert.equal(state.sessions.find(s => s.id === 'term-abcd').hidden, false);
    assert.ok(page.url().endsWith('/terminal/term-abcd'));
    await page.locator('.hidden-shells-btn').waitFor({ state: 'detached' });
    // Hide, then Undo from the toast returns to the shell.
    await page.locator('.term-bar').getByRole('button', { name: 'Hide', exact: true }).click();
    await page.waitForURL(url => url.pathname !== '/terminal/term-abcd');
    await page.getByRole('button', { name: 'Undo', exact: true }).last().click();
    await page.waitForURL('**/terminal/term-abcd');
    assert.equal(state.sessions.find(s => s.id === 'term-abcd').hidden, false);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('A list page shows the eye only while a shell is hidden; its menu opens and unhides', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const { page, state, requests, errors } = await setup(browser);
    await page.goto(`${ORIGIN}/board`);
    const shells = page.locator('.sidebar .sb-line', { has: page.locator('.sw-shell') });
    await shells.first().waitFor();
    assert.equal(await shells.count(), 2);
    assert.equal(await page.locator('.hidden-shells-btn').count(), 0);
    state.sessions = state.sessions.map(s => s.id === 'term-abcd' ? { ...s, hidden: true }
      : s.id === 'term-busy' ? { ...s, name: 'bench runner', named: true } : s);
    await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
    await page.waitForFunction(() => document.querySelectorAll('.sidebar .sb-line .sw-shell').length === 1);
    assert.deepEqual(await shells.locator('.sw-nm').allTextContents(), ['bench runner']);
    await page.locator('header.bar').getByRole('button', { name: '1 hidden terminal', exact: true }).click();
    await page.locator('.hidden-shell').getByText('last input 12m').waitFor();
    if (process.env.SHOTS_DIR) { await mkdir(process.env.SHOTS_DIR, { recursive: true }); await page.screenshot({ path: `${process.env.SHOTS_DIR}/board-eye.png` }); }
    await page.locator('.hidden-shell').getByRole('button', { name: 'Unhide', exact: true }).click();
    await page.waitForFunction(() => document.querySelectorAll('.sidebar .sb-line .sw-shell').length === 2);
    await page.locator('.hidden-shells-btn').waitFor({ state: 'detached' });
    assert.deepEqual(requests.filter(r => r.method === 'PATCH').at(-1).body, { hidden: false });
    // Hiding another shell later shows the eye closed.
    state.sessions = state.sessions.map(s => s.id === 'term-busy' ? { ...s, hidden: true } : s);
    await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
    await page.locator('.hidden-shells-btn').waitFor();
    assert.equal(await page.locator('.hidden-shells-pop').count(), 0);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});
