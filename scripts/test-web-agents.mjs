// The sidebar, hover card and landing in a browser (spec 2543 B-D, G; 1782 F); agent details and handoff.
// Run with node --test; install playwright or set PLAYWRIGHT_MODULE to its entry point.
// Set SHOTS_DIR to also save the 1440/390 px, light/dark, 15/19 px screenshots there.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';
const SHOTS = process.env.SHOTS_DIR;
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const shell = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1">
<script>document.documentElement.style.fontSize=(Number(localStorage.getItem("sm-text-size"))||15)+"px"</script>
<link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/queue.css">
<script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script>
<script id="sm-config" type="application/json">{"inbox_token":"fixture","refresh_seconds":3}</script>
<script type="module" src="/assets/app.js"></script></head><body><div id="app"></div></body></html>`;

// Memo scenario 1 at 19:47 UTC on 30 September, plus the overlaps the memo lists.
const NOW = '2026-09-30T19:47:00Z';
const SM = '/Users/rajesh/projects/session-manager';
const FAR = '/Users/rajesh/projects/fractal-algo-rust';
const noJobs = { running: 0, waiting: 0, review: null, text: 'No jobs', tone: null };
const ticket = (number) => [{ kind: 'ticket', number, repo: 'rajeshgoli/session-manager', title: `Ticket ${number}` }];
const agent = (name, provider, repo, section, key, facts, extra = {}) => ({
  id: name, name, provider, repo, state: facts.agent?.state === 'working' ? 'working' : 'idle', claims: [],
  context_percent: 30, working_dir: repo, jobs: [], waiting_on: [], docs: [], attach: `sm attach ${name}`,
  attention: { section, reason: null, order_key: key },
  facts: { agent: { state: 'idle', since: '2026-09-30T19:40:00Z' }, jobs: noJobs, you: null, finished: null, ...facts },
  ...extra,
});
const claude = (name) => ({ remote_control: { url: `https://claude.ai/code/session_${name}` } });
function fixture() {
  const sessions = [
    agent('sm-1726-engineer', 'codex-fork', SM, 'you', '2026-09-30T19:40:12Z', {
      agent: { state: 'idle', since: '2026-09-30T19:40:16Z' },
      you: { kind: 'message', since: '2026-09-30T19:40:12Z', text: 'one manual Chrome check: please open any terminal in Chrome and tell me whether it connects', more: 0, dismissible: true },
    }, { claims: ticket(1771) }),
    agent('sm-1790-reviewer', 'claude', SM, 'you', '2026-09-30T19:35:00Z', {
      agent: { state: 'working', since: '2026-09-30T19:44:00Z' },
      you: { kind: 'doc_review', since: '2026-09-30T19:35:00Z', text: 'Review: Fit and finish', more: 0, dismissible: false },
    }, { claims: ticket(1782), ...claude('rev') }),
    agent('far-1855', 'claude', FAR, 'finished', '8240483139', {
      agent: { state: 'idle', since: '2026-09-30T15:01:43Z' },
      finished: { at: '2026-09-30T15:01:00Z', text: '1855 done and closed: 68 views built, all checks pass, PR merged and the ticket closed.', read: false },
    }, { claims: ticket(1855), ...claude('far') }),
    agent('waits-in-line', 'claude', SM, 'waiting_long', '2026-09-30T18:45:00Z', {
      agent: { state: 'idle', since: '2026-09-30T18:42:00Z' },
      jobs: { running: 0, waiting: 1, review: null, text: 'Waiting 1h 2m · 3rd in line', tone: 'amber' },
    }, { claims: ticket(1777), attention: { section: 'waiting_long', reason: 'queue_wait', order_key: '2026-09-30T18:45:00Z' } }),
    agent('iter8-run', 'claude', FAR, 'moving', '8240469479', {
      agent: { state: 'idle', since: '2026-09-30T19:42:42Z' },
      jobs: { running: 2, waiting: 0, review: null, text: '2 running · 2h 56m', tone: 'green' },
    }, { claims: ticket(1858), ...claude('run') }),
    agent('sm-1776', 'codex-fork', SM, 'moving', '8240469100', { agent: { state: 'working', since: '2026-09-30T19:46:00Z' } }, { claims: ticket(1776) }),
    agent('sm-1782', 'claude', SM, 'moving', '8240469200', {
      agent: { state: 'working', since: '2026-09-30T19:40:00Z' },
      jobs: { running: 0, waiting: 1, review: null, text: 'Waiting 8m · 1st in line', tone: 'amber' },
    }, { claims: ticket(1782), ...claude('1782') }),
    agent('waits-review', 'claude', SM, 'waiting', '2026-09-30T19:35:00Z', {
      agent: { state: 'idle', since: '2026-09-30T19:35:00Z' },
      jobs: { running: 0, waiting: 0, review: { pr_number: 1790 }, text: 'Codex review on PR #1790 · 12m', tone: 'amber' },
    }, { claims: ticket(1790) }),
    ...['sm-1768', 'iter7-teacher', 'sm-1727', 'sm-reviewer', 'iter8-dedicated-reviewer', 'sm-scout'].map((name, i) => agent(
      name, i % 2 ? 'codex' : 'claude', i % 2 ? FAR : SM, 'idle', `1824046${9600 + i}`,
      { agent: { state: 'idle', since: new Date(Date.parse(NOW) - (11 + i * 30) * 60000).toISOString() } },
    )),
  ];
  return sessions;
}

// A stand-in for every route these tests read; `state.sessions` is what GET /watch/state returns.
async function open(browser, { viewport, colorScheme = 'light', size = 15, sessions = fixture(), storage = {} }) {
  const context = await browser.newContext({ viewport, colorScheme });
  const page = await context.newPage();
  await page.addInitScript(([value, saved]) => {
    localStorage.setItem('sm-text-size', String(value));
    for (const [key, item] of Object.entries(saved)) localStorage.setItem(key, JSON.stringify(item));
  }, [size, storage]);
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  const state = { sessions, opened: [], answered: [] };
  await page.clock.setFixedTime(new Date(NOW));
  await page.exposeFunction('recordOpen', (url) => state.opened.push(url));
  await page.addInitScript(() => { window.open = (url) => { window.recordOpen(url); return null; }; });
  await page.route('http://localhost/**', async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    if (url.pathname.startsWith('/assets/')) {
      const file = url.pathname.slice('/assets/'.length);
      return route.fulfill({ body: await readFile(new URL(file, assets)), contentType: file.endsWith('.css') ? 'text/css' : 'text/javascript' });
    }
    if (request.isNavigationRequest()) return route.fulfill({
      body: shell.replace('<html>', `<html data-theme="${colorScheme}">`), contentType: 'text/html',
    });
    const answer = /^\/sessions\/([^/]+)\/needs-you\/answered$/.exec(url.pathname);
    if (answer && request.method() === 'POST') {
      const id = decodeURIComponent(answer[1]);
      state.answered.push(id);
      state.sessions = state.sessions.map((s) => (s.id === id ? { ...s, facts: { ...s.facts, you: null }, attention: { section: 'idle', reason: null, order_key: '0' } } : s));
      return route.fulfill({ json: { facts: null } });
    }
    if (url.pathname === '/client/board') return route.fulfill({ json: { lanes: [], other: [] } });
    if (url.pathname === '/watch/state') return route.fulfill({ json: { generated_at: NOW, sessions: state.sessions, counts: {} } });
    if (url.pathname === '/client/settings') return route.fulfill({ json: { new_agent: { provider: 'claude', claude: {}, codex: {}, agent_types: [], workspaces: ['/tmp/repo'] } } });
    if (url.pathname === '/client/queue') return route.fulfill({ json: { running: [{ id: 'j1' }], queued: [{ id: 'j2', queued_at: NOW }] } });
    if (url.pathname === '/inbox') return route.fulfill({ json: { needs_you_count: 1, rows: [] } });
    if (url.pathname.endsWith('/last-turn')) return route.fulfill({ status: 404, json: { detail: 'none' } });
    return route.fulfill({ json: {} });
  });
  return { context, page, errors, state };
}

const shell1 = { id: 'studio-zsh', name: 'studio-zsh', kind: 'shell', state: 'idle', repo: SM, working_dir: SM, claims: [],
  attention: { section: 'idle', reason: null, order_key: '0' }, facts: { agent: { state: 'idle', since: NOW } } };
const sideRows = (page) => page.locator('.sidebar .sb-line:not(.sb-idle)').evaluateAll((rows) => rows.map((row) =>
  `${row.querySelector('.sw-nm').textContent} ${row.querySelector('.sb-fact').textContent}`));

test('sidebar on every page (2543 B, D): pages first, agents first, hover card, + and folding', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  if (SHOTS) await mkdir(SHOTS, { recursive: true });
  try {
    for (const colorScheme of ['light', 'dark']) {
      for (const size of [15, 19]) {
        const { context, page, errors, state } = await open(browser, { viewport: { width: 1440, height: 1000 }, colorScheme, size, sessions: [...fixture(), shell1] });
        await page.goto('http://localhost/board');
        await page.locator('.sidebar .sb-line').first().waitFor();
        // One left column: search, every page link, the Agents heading, rows, meters, Settings.
        assert.equal(await page.locator('nav.sidebar').count(), 1);
        assert.equal(await page.locator('.rail').count(), 0, 'no second left column');
        assert.equal(await page.locator('.sidebar .sb-top .search').count(), 1);
        assert.deepEqual(await page.locator('.sidebar .sb-pages .item .lb').allInnerTexts(),
          ['Inbox', 'Board', 'Queue', 'Artifacts', 'Notes', 'Analytics', 'History', 'Guestbook']);
        assert.equal(await page.locator('.sidebar .sb-pages .item.on .lb').innerText(), 'Board');
        assert.equal(await page.locator('.sidebar .item', { hasText: 'Queue' }).locator('b').innerText(), '1 · 1');
        assert.equal(await page.locator('.sidebar > .item .lb').innerText(), 'Settings');
        assert.equal(await page.locator('.sidebar .dash').count(), 1);
        assert.equal(await page.locator('.tabbar').isVisible(), false);
        // The heading: 14 live agents, 2 need you; the shell is not an agent.
        assert.equal((await page.locator('.sb-head .sb-live').innerText()).trim(), '14 live ›');
        assert.equal(await page.locator('.sb-head b.magenta').innerText(), '2');
        // Pages first: one-line rows with the short fact, in attention order; Idle folded to one row.
        assert.deepEqual(await sideRows(page), [
          'sm-1790-reviewer 12m', 'sm-1726-engineer 6m', 'far-1855 done', 'waits-in-line ⏸ queue', 'sm-1776 1m', 'sm-1782 ⏸ queue',
          'iter8-run ▶ 2h', 'waits-review ⏸ review', 'studio-zsh shell']);
        assert.equal((await page.locator('.sidebar .sb-idle').innerText()).trim(), 'Idle · 6 ›');
        // The top bar: page name; Moon, Bug, then New agent in the corner; search lives in the sidebar.
        assert.equal(await page.locator('.bar h1').innerText(), 'Board');
        assert.deepEqual(await page.locator('.bar > button, .bar > .anchor > button').evaluateAll((b) => b.filter((x) => x.offsetParent)
          .map((x) => x.getAttribute('aria-label') || x.textContent.trim())), ['Overnight mode', 'Report a bug', 'New agent']);
        const width = await page.evaluate(() => document.documentElement.scrollWidth);
        assert.ok(width <= 1440, `no horizontal scroll (got ${width})`);
        assert.ok(Math.abs((await page.locator('.sidebar').boundingBox()).width - 15.5 * size) <= 1, 'the sidebar is 15.5 rem');
        if (SHOTS) await page.screenshot({ path: `${SHOTS}/sidebar-board-${colorScheme}-${size}.png` });

        if (colorScheme === 'light' && size === 15) {
          // The hover card: today's agent card after 400 ms; none on a shell row; a click opens the terminal.
          await page.locator('.sb-line', { hasText: 'studio-zsh' }).hover();
          await page.waitForTimeout(600);
          assert.equal(await page.locator('.sb-card').count(), 0, 'no card on a shell row');
          await page.locator('.sb-line', { hasText: 'iter8-run' }).hover();
          assert.equal(await page.locator('.sb-card').count(), 0, 'not before 400 ms');
          const card = page.locator('.sb-card .acard');
          await card.waitFor();
          assert.equal(await card.locator('.nm').innerText(), 'iter8-run');
          assert.equal(await card.locator('.facts .fa').nth(1).innerText(), '▶ 2 running · 2h 56m');
          assert.equal(await card.locator('.icons button').count(), 2, 'Terminal and Open in Claude');
          await card.getByRole('button', { name: 'Open in Claude' }).click();
          assert.deepEqual(state.opened, ['https://claude.ai/code/session_run']);
          if (SHOTS) await page.screenshot({ path: `${SHOTS}/sidebar-hover-card.png` });
          // ✓ in the card clears the question, as on the Agents page.
          await page.locator('.sb-line', { hasText: 'sm-1726-engineer' }).hover();
          await page.locator('.sb-card .acard', { hasText: 'sm-1726-engineer' }).getByRole('button', { name: 'Mark answered' }).click();
          assert.deepEqual(state.answered, ['sm-1726-engineer']);
          // + opens the New session sheet in the sidebar.
          await page.locator('.sb-head').getByRole('button', { name: 'New agent' }).click();
          const start = page.locator('.new-agent-host.side .pop').getByRole('button', { name: 'Start' });
          await start.waitFor();
          // Not clipped by the sidebar and reachable in the window: the button is what sits under its own centre.
          await start.scrollIntoViewIfNeeded();
          assert.ok(await start.evaluate((b) => { const r = b.getBoundingClientRect(); return document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) === b; }));
          // The fixed test clock slows effect flushes; give the sheet's listeners a moment to attach.
          await page.waitForTimeout(300);
          await page.mouse.click(900, 600);
          await page.locator('.new-agent-host .pop').waitFor({ state: 'detached' });
          // ⌘K searches from any page.
          await page.keyboard.press('Meta+k');
          await page.locator('.palette').waitFor();
          await page.locator('.scrim').click({ position: { x: 5, y: 5 } });
          await page.locator('.palette').waitFor({ state: 'detached' });
          // The chevron swaps to agents first: icons for pages, today's two-line rows; remembered for pages.
          await page.locator('.sb-head .sb-live').click();
          await page.locator('.sidebar .term-switch .sw-row').first().waitFor();
          assert.equal(await page.locator('.sidebar .sb-icons .item.icon').count(), 8);
          assert.equal(await page.locator('.sidebar .sb-icons .item.on').getAttribute('aria-label'), 'Board');
          assert.equal((await page.locator('.sb-head .sb-live').innerText()).trim(), '14 live ⌄');
          assert.deepEqual(await page.evaluate(() => JSON.parse(localStorage.getItem('sm-sidebar-arrangement'))), { pages: 'agents', terminal: 'agents' });
          if (SHOTS) await page.screenshot({ path: `${SHOTS}/sidebar-agents-first-board.png` });
          await page.locator('.sb-head .sb-live').click();
          // ⌘\ folds to the strip, with a magenta dot while something needs you; remembered per view kind.
          await page.keyboard.press('Meta+Backslash');
          await page.locator('.sidebar').waitFor({ state: 'detached' });
          assert.equal(await page.locator('.sb-edge .sb-signal.magenta').count(), 1);
          assert.deepEqual(await page.evaluate(() => JSON.parse(localStorage.getItem('sm-sidebar'))), { pages: 'folded', terminal: 'open', doc: 'folded' });
          assert.ok((await page.locator('.sb-edge').boundingBox()).width <= 15);
          await page.getByRole('button', { name: 'Unfold sidebar' }).click();
          await page.locator('.sidebar').waitFor();
          // A row opens its terminal, where the sidebar puts agents first with that row current.
          await page.locator('.sb-line', { hasText: 'iter8-run' }).click();
          await page.waitForFunction(() => location.pathname === '/terminal/iter8-run');
          await page.locator('.sidebar .term-switch .sw-row.cur', { hasText: 'iter8-run' }).waitFor();
          assert.equal(await page.locator('.sidebar .sb-icons .item.on').count(), 0);
          // ⌘⌥↓ steps to the next row's terminal; from Board it opens the first row's.
          await page.keyboard.press('Meta+Alt+ArrowDown');
          await page.waitForFunction(() => location.pathname === '/terminal/waits-review');
          await page.locator('.sidebar .sb-icons .item[aria-label="Inbox"]').click();
          await page.waitForFunction(() => location.pathname === '/inbox');
          await page.keyboard.press('Meta+Alt+ArrowDown');
          await page.waitForFunction(() => location.pathname === '/terminal/sm-1790-reviewer');
          // Notes' main action is New note.
          await page.locator('.sidebar .sb-icons .item[aria-label="Notes"]').click();
          await page.waitForFunction(() => location.pathname === '/notes');
          assert.equal(await page.locator('.bar > button.pri').innerText(), 'New note');
          assert.equal(await page.locator('.notes-page .notes-tools .btn.pri').count(), 0, "the page's own + New moved to the bar");
        }
        assert.deepEqual(errors, []);
        await context.close();
      }
    }
  } finally { await browser.close(); }
});

test('landing at / (2543 D.5): last live terminal, else the top row, else an empty page; the list on a phone', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const cases = [
      [{}, fixture(), '/terminal/sm-1790-reviewer'],
      [{ 'sm-term-last': 'iter8-run' }, fixture(), '/terminal/iter8-run'],
      [{ 'sm-term-last': 'retired-long-ago' }, fixture(), '/terminal/sm-1790-reviewer'],
      // Only idle agents: the top idle row.
      [{}, fixture().filter((s) => s.attention.section === 'idle'), '/terminal/sm-1768'],
    ];
    for (const [storage, sessions, path] of cases) {
      const { context, page, errors } = await open(browser, { viewport: { width: 1440, height: 900 }, sessions, storage });
      await page.goto('http://localhost/');
      await page.waitForFunction((want) => location.pathname === want, path);
      await page.locator('.term-page').waitFor();
      assert.deepEqual(errors, []);
      await context.close();
    }
    {
      const { context, page } = await open(browser, { viewport: { width: 1440, height: 900 }, sessions: [] });
      await page.goto('http://localhost/');
      await page.locator('.home-empty', { hasText: 'No live agents' }).getByRole('button', { name: 'New agent' }).waitFor();
      assert.equal(new URL(page.url()).pathname, '/');
      await context.close();
    }
    {
      // A link to an agent's details lands on its terminal with Details open.
      const { context, page } = await open(browser, { viewport: { width: 1440, height: 900 } });
      await page.goto('http://localhost/?open=agent:sm-1776');
      await page.waitForFunction(() => location.pathname === '/terminal/sm-1776' && new URLSearchParams(location.search).get('open') === 'agent:sm-1776');
      await page.locator('.term-panel .phd .t', { hasText: 'sm-1776' }).waitFor();
      await context.close();
    }
    {
      // Phone: `/` is the agents list; list pages have the tab bar and no sidebar.
      const { context, page, errors } = await open(browser, { viewport: { width: 390, height: 844 } });
      await page.goto('http://localhost/');
      await page.locator('.home-list .sw-row').first().waitFor();
      assert.equal(new URL(page.url()).pathname, '/');
      assert.equal(await page.locator('.tabbar').isVisible(), true);
      assert.deepEqual(await page.locator('.tabbar .item').evaluateAll((items) => items.filter((x) => x.offsetParent).map((x) => x.textContent.trim())),
        ['Agents', 'Board', 'Inbox1', 'Queue1', 'Artifacts', 'Notes', 'Settings']);
      assert.equal(await page.locator('.sb-wrap').isVisible(), false);
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth) <= 390);
      if (SHOTS) await page.screenshot({ path: `${SHOTS}/home-390.png` });
      await page.goto('http://localhost/board');
      await page.locator('.bar h1', { hasText: 'Board' }).waitFor();
      assert.equal(await page.locator('.sb-wrap').isVisible(), false);
      assert.equal(await page.locator('.bar > .search').isVisible(), true, 'search stays in the bar on a phone');
      assert.deepEqual(errors, []);
      await context.close();
    }
  } finally { await browser.close(); }
});

test('long agent name keeps the details Retire confirmation and Cancel in a phone viewport', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    const sessions = fixture().map((agent) => agent.id === 'sm-1776'
      ? { ...agent, name: 'sm-1776-with-a-very-long-name-that-must-wrap-inside-the-confirmation' }
      : agent);
    await page.route('http://localhost/**', async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname.startsWith('/assets/')) {
        const file = url.pathname.slice('/assets/'.length);
        return route.fulfill({ body: await readFile(new URL(file, assets)), contentType: file.endsWith('.css') ? 'text/css' : 'text/javascript' });
      }
      if (route.request().isNavigationRequest()) return route.fulfill({ body: shell, contentType: 'text/html' });
      if (url.pathname === '/client/board') return route.fulfill({ json: { lanes: [], other: [] } });
      if (url.pathname === '/watch/state') return route.fulfill({ json: { generated_at: NOW, sessions, counts: {} } });
      return route.fulfill({ json: {} });
    });
    await page.goto('http://localhost/board?open=agent:sm-1776');
    await page.locator('.panel .acts .retire-action > button').click();
    const prompt = page.locator('.panel .retire-confirm');
    const cancel = prompt.getByRole('button', { name: 'Cancel' });
    const box = await prompt.boundingBox();
    const cancelBox = await cancel.boundingBox();
    assert.ok(box.x >= 0 && box.x + box.width <= 390, 'confirmation fits inside the phone viewport');
    assert.ok(cancelBox.x >= 0 && cancelBox.x + cancelBox.width <= 390, 'Cancel stays in view');
    await cancel.click();
  } finally { await browser.close(); }
});

test('agent details are one view (#2280): status, then the thread with its docs, one compose box', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    const sent = [];
    await page.route('http://localhost/**', async (route) => {
      const request = route.request();
      const url = new URL(request.url());
      if (url.pathname.startsWith('/assets/')) {
        const file = url.pathname.slice('/assets/'.length);
        return route.fulfill({ body: await readFile(new URL(file, assets)), contentType: file.endsWith('.css') ? 'text/css' : 'text/javascript' });
      }
      if (request.isNavigationRequest()) return route.fulfill({ body: shell, contentType: 'text/html' });
      if (url.pathname === '/client/board') return route.fulfill({ json: { lanes: [], other: [] } });
      if (url.pathname === '/watch/state') {
        const only = url.searchParams.get('session');
        const all = fixture().map((s) => s.id === 'sm-1776' ? { ...s, facts: { ...s.facts, you: { kind: 'prompt', since: NOW, text: 'Allow prompt in its terminal', more: 0, dismissible: false } } } : s);
        const list = only ? all.filter((s) => s.id === only) : all;
        return route.fulfill({ json: { generated_at: NOW, sessions: list, counts: {} } });
      }
      if (url.pathname.endsWith('/last-turn')) return route.fulfill({ json: { at: NOW, html: '<p>Done; one question left.</p>' } });
      if (url.pathname === '/inbox/thread/ticket%3Arajeshgoli%2Fsession-manager%231726/send' && request.method() === 'POST') {
        sent.push(JSON.parse(request.postData()));
        return route.fulfill({ json: {} });
      }
      if (url.pathname === '/inbox/agent/sm-1776') return route.fulfill({ status: 404, json: { detail: 'Thread not found' } });
      if (url.pathname === '/inbox/agent/sm-1726-engineer') {
        return route.fulfill({ json: { thread_key: 'ticket:rajeshgoli/session-manager#1726', title: 'sm-1726-engineer', status: 'idle', can_send: true,
          reply_options: [{ id: 'sm-1726-engineer', name: 'sm-1726-engineer', status: 'live', can_send: true }, { id: 'sm-1726-helper', name: 'sm-1726-helper', status: 'live', can_send: true }], items: [
          { type: 'doc_revision', at: NOW, sender: { name: 'sm-1726-engineer' }, sha: 'abcdef1234', review_state: 'requested',
            html: '<a href="/docs/session-manager/docs/working/1726.html">1726 memo</a>' },
          { html: '<div class="b ask" data-msg="m1"><p data-sm-line="1">Can you run one manual Chrome check?</p></div>' },
        ] } });
      }
      return route.fulfill({ json: {} });
    });
    // No tabs: the status block sits above the thread, and the question shows in the thread.
    await page.goto('http://localhost/board?open=agent:sm-1726-engineer');
    await page.locator('.panel .agent-status').waitFor();
    assert.equal(await page.locator('.panel .tabs').count(), 0);
    await page.locator('.panel .thread-items', { hasText: 'Can you run one manual Chrome check?' }).waitFor();
    assert.equal(await page.locator('.panel .thread-doc-card strong').innerText(), '1726 memo');
    assert.equal(await page.locator('.panel .msgbox').count(), 0, 'one compose box, the thread\'s');
    // A ticket's work thread sends through its own route, so the chosen recipient is the one that gets it.
    await page.getByRole('combobox', { name: 'Reply to' }).selectOption('sm-1726-helper');
    await page.getByRole('textbox', { name: 'Reply' }).fill('Ran it, passes');
    await page.getByRole('button', { name: 'Send' }).click();
    await page.waitForFunction(() => document.querySelector('textarea[aria-label="Reply"]')?.value === '');
    assert.equal(sent[0].body, 'Ran it, passes');
    assert.equal(sent[0].to, 'sm-1726-helper');
    if (SHOTS) await page.screenshot({ path: `${SHOTS}/agent-details-one-view.png`, fullPage: true });
    // An agent with no thread yet shows its last turn and the plain message box.
    await page.goto('http://localhost/board?open=agent:sm-1776');
    await page.locator('.panel .last-turn', { hasText: 'Done; one question left.' }).waitFor();
    assert.equal(await page.locator('.panel .msgbox').count(), 1);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('Hand off…: a cleared field takes a typed percent; presets; per-agent review switches (#2283)', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    const puts = [];
    let policy = { enabled: true, threshold_percent: 35, source: 'default', has_gauge: true, ask_on_codex_review: true, ask_on_doc_review: true };
    await page.route('http://localhost/**', async (route) => {
      const request = route.request();
      const url = new URL(request.url());
      if (url.pathname.startsWith('/assets/')) {
        const file = url.pathname.slice('/assets/'.length);
        return route.fulfill({ body: await readFile(new URL(file, assets)), contentType: file.endsWith('.css') ? 'text/css' : 'text/javascript' });
      }
      if (request.isNavigationRequest()) return route.fulfill({ body: shell, contentType: 'text/html' });
      if (url.pathname === '/client/board') return route.fulfill({ json: { lanes: [], other: [] } });
      if (url.pathname === '/watch/state') return route.fulfill({ json: { generated_at: NOW, sessions: fixture(), counts: {} } });
      if (url.pathname === '/sessions/sm-1776/handoff-policy') {
        if (request.method() === 'PUT') {
          const body = request.postDataJSON();
          puts.push(body);
          policy = { ...policy, ...body, source: 'override' };
        }
        return route.fulfill({ json: policy });
      }
      if (url.pathname === '/inbox/agent/sm-1776') return route.fulfill({ status: 404, json: { detail: 'Thread not found' } });
      return route.fulfill({ json: {} });
    });
    await page.goto('http://localhost/board?open=agent:sm-1776');
    await page.locator('.panel .acts').getByRole('button', { name: 'Hand off…' }).click();
    const pop = page.locator('.handoff-pop');
    const field = pop.getByRole('spinbutton', { name: 'Hand off at, percent of context' });
    await field.waitFor();
    assert.equal(await field.inputValue(), '35');
    // Clearing keeps the field empty, so typing 50 means 50, not 350.
    await field.fill('');
    assert.equal(await field.inputValue(), '');
    await field.pressSequentially('50');
    await field.press('Enter');
    await page.waitForFunction(() => document.querySelector('.handoff-pop [role="status"]')?.textContent.startsWith('Saved'));
    assert.deepEqual(puts.at(-1), { threshold_percent: 50 });
    await pop.getByRole('button', { name: '65%' }).click();
    await page.waitForFunction(() => document.querySelector('.handoff-pop input[type="number"]')?.value === '65');
    assert.deepEqual(puts.at(-1), { threshold_percent: 65 });
    await pop.getByRole('switch', { name: 'Hand off before a Codex review' }).click();
    for (let i = 0; i < 100 && puts.length < 3; i += 1) await page.waitForTimeout(50);
    assert.deepEqual(puts.at(-1), { ask_on_codex_review: false });
    await pop.getByRole('switch', { name: 'Hand off before a Codex review', checked: false }).waitFor();
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('Settings › Context handoff (#2294): defaults grid, review switches, Set differently with Reset', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const width of [1440, 390]) {
      const page = await browser.newPage({ viewport: { width, height: 900 } });
      const errors = [];
      page.on('pageerror', (error) => errors.push(error.message));
      const puts = [];
      let items = [
        { kind: 'lane', repo: 'rajeshgoli/fractal-algo-rust', number: 2130, title: 'Iteration 11', override: { enabled: true, threshold_percent: 50, ask_on_codex_review: false, ask_on_doc_review: null } },
        { kind: 'ticket', repo: 'rajeshgoli/session-manager', number: 2264, title: 'Backup mini', override: { enabled: false, threshold_percent: null, ask_on_codex_review: null, ask_on_doc_review: null } },
        { kind: 'agent', id: 'sm-1776', name: 'sm-1776', override: { enabled: null, threshold_percent: 65, ask_on_codex_review: null, ask_on_doc_review: null } },
      ];
      const defaults = { providers: { claude: true, 'codex-fork': false, opencode: false }, window_tokens: { claude: 1000000 },
        provider_thresholds: { claude: { threshold_percent: 35, reminder_percent: 45, review_floor_percent: 20 } },
        ask_on_codex_review: true, ask_on_doc_review: true };
      await page.route('http://localhost/**', async (route) => {
        const request = route.request();
        const url = new URL(request.url());
        if (url.pathname.startsWith('/assets/')) {
          const file = url.pathname.slice('/assets/'.length);
          return route.fulfill({ body: await readFile(new URL(file, assets)), contentType: file.endsWith('.css') ? 'text/css' : 'text/javascript' });
        }
        if (request.isNavigationRequest()) return route.fulfill({ body: shell, contentType: 'text/html' });
        if (request.method() === 'PUT') {
          puts.push([url.pathname, request.postDataJSON()]);
          if (request.postDataJSON().use_default) items = items.filter((item) => !url.pathname.includes(String(item.number || item.id)));
          return route.fulfill({ json: url.pathname === '/handoff-defaults' ? defaults : {} });
        }
        if (url.pathname === '/handoff-defaults') return route.fulfill({ json: defaults });
        if (url.pathname === '/handoff-overrides') return route.fulfill({ json: { items } });
        if (url.pathname === '/client/board') return route.fulfill({ json: { lanes: [], other: [] } });
        if (url.pathname === '/watch/state') return route.fulfill({ json: { generated_at: NOW, sessions: [], counts: {} } });
        return route.fulfill({ json: {} });
      });
      await page.goto('http://localhost/settings#context-handoff');
      const field = page.getByRole('spinbutton', { name: 'Hand off at' });
      await field.waitFor();
      assert.equal(await field.inputValue(), '35');
      assert.equal(await page.locator('.hd-row:not(.hd-head)').count(), 3, 'one row per provider');
      assert.deepEqual(await page.locator('.hd-overrides .hd-what').allInnerTexts(),
        ['on · at 50% · no handoff before Codex review', 'off', 'at 65%']);
      const pageWidth = await page.evaluate(() => document.documentElement.scrollWidth);
      assert.ok(pageWidth <= width, `no horizontal scroll at ${width} px (got ${pageWidth})`);
      if (SHOTS) await page.screenshot({ path: `${SHOTS}/settings-handoff-${width}.png`, fullPage: true });
      if (width === 1440) {
        await field.fill('');
        await field.pressSequentially('50');
        await field.press('Enter');
        for (let i = 0; i < 100 && !puts.length; i += 1) await page.waitForTimeout(50);
        assert.deepEqual(puts[0], ['/handoff-defaults', { provider_thresholds: { claude: { threshold_percent: 50 } } }]);
        await page.locator('.hd-overrides li', { hasText: 'Ticket #2264' }).getByRole('button', { name: 'Reset' }).click();
        await page.locator('.hd-overrides li', { hasText: 'Ticket #2264' }).waitFor({ state: 'detached' });
        assert.deepEqual(puts.at(-1), ['/handoff-policy/ticket/rajeshgoli/session-manager/2264', { use_default: true }]);
      }
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); }
});
