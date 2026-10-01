// Agents page in a browser (spec 1782 F): sections, three facts, ✓, both icons, keys.
// Run with node --test; install playwright or set PLAYWRIGHT_MODULE to its entry point.
// Set SHOTS_DIR to also save the 1440/390 px, light/dark, 15/19 px screenshots there.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';
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

test('Agents page: sections, facts, icons, ✓ and keys at desktop and phone sizes', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const shots = process.env.SHOTS_DIR;
  if (shots) await mkdir(shots, { recursive: true });
  try {
    for (const viewport of [{ width: 1440, height: 1000 }, { width: 390, height: 844 }]) {
      for (const colorScheme of ['light', 'dark']) {
        for (const size of [15, 19]) {
          const context = await browser.newContext({ viewport, colorScheme });
          const page = await context.newPage();
          await page.addInitScript((value) => localStorage.setItem('sm-text-size', String(value)), size);
          const errors = [];
          page.on('pageerror', (error) => errors.push(error.message));
          let sessions = fixture();
          const answered = [];
          const opened = [];
          await page.clock.install({ time: new Date(NOW) });
          await page.exposeFunction('recordOpen', (url) => opened.push(url));
          await page.addInitScript(() => { window.open = (url) => { window.recordOpen(url); return null; }; });
          await page.route('http://localhost/**', async (route) => {
            const request = route.request();
            const url = new URL(request.url());
            if (url.pathname.startsWith('/assets/')) {
              const file = url.pathname.slice('/assets/'.length);
              return route.fulfill({ body: await readFile(new URL(file, assets)), contentType: file.endsWith('.css') ? 'text/css' : 'text/javascript' });
            }
            if (request.isNavigationRequest()) return route.fulfill({ body: shell, contentType: 'text/html' });
            const answer = /^\/sessions\/([^/]+)\/needs-you\/answered$/.exec(url.pathname);
            if (answer && request.method() === 'POST') {
              const id = decodeURIComponent(answer[1]);
              answered.push(id);
              sessions = sessions.map((s) => (s.id === id ? { ...s, facts: { ...s.facts, you: null }, attention: { section: 'idle', reason: null, order_key: '0' } } : s));
              return route.fulfill({ json: { facts: null } });
            }
            if (url.pathname.endsWith('/last-turn')) return route.fulfill({ status: 404, json: { detail: 'none' } });
            if (url.pathname === '/watch/state') {
              const only = url.searchParams.get('session');
              const list = only ? sessions.filter((s) => s.id === only) : sessions;
              const counts = { live: sessions.length };
              for (const s of sessions) counts[s.attention.section === 'you' ? 'needs_you' : s.attention.section] = (counts[s.attention.section === 'you' ? 'needs_you' : s.attention.section] || 0) + 1;
              return route.fulfill({ json: { generated_at: NOW, sessions: list, counts } });
            }
            if (url.pathname === '/client/host-status') {
              return route.fulfill({ json: { memory_used_bytes: 115e9, memory_total_bytes: 256e9, cpu_percent: 21, gpu_percent: 0 } });
            }
            return route.fulfill({ json: {} });
          });
          await page.goto('http://localhost/');
          await page.waitForFunction(() => document.querySelectorAll('.acard').length > 5);

          const headers = await page.locator('.grp.sec').allInnerTexts();
          assert.deepEqual(headers.map((t) => t.trim().toLowerCase()), ['needs you', 'finished', 'waiting long', 'moving', 'waiting', 'idle · 6']);
          const strip = (await page.locator('.sum').innerText()).replace(/\s+/g, ' ').trim();
          assert.equal(strip, '2 needs you · 1 finished · 1 waiting long · 3 moving · 1 waiting · 6 idle');
          // Facts: the overlaps show side by side, and the You line only when present.
          const card = (name) => page.locator('.acard', { has: page.locator('.nm', { hasText: new RegExp(`^${name}$`) }) });
          assert.equal((await card('sm-1726-engineer').locator('.you').innerText()).startsWith('◆ 6m: one manual Chrome check'), true);
          assert.equal(await card('sm-1790-reviewer').locator('.facts .fa').first().innerText(), '● Working 3m');
          assert.equal(await card('sm-1790-reviewer').locator('.ok').count(), 0, 'a doc review has no ✓');
          assert.equal(await card('far-1855').locator('.you').innerText(), '✔ 1855 done and closed: 68 views built, all checks pass, PR merged and the ticket closed.');
          assert.equal(await card('iter8-run').locator('.facts .fa').nth(1).innerText(), '▶ 2 running · 2h 56m');
          assert.equal(await card('sm-1782').locator('.facts .fa').nth(1).innerText(), '⏸ Waiting 8m · 1st in line');
          assert.equal(await card('iter8-run').locator('.you').count(), 0);
          // Both icons on a Claude agent with a claude.ai session; Terminal only on Codex.
          assert.equal(await card('iter8-run').locator('.icons button').count(), 2);
          assert.equal(await card('sm-1726-engineer').locator('.icons button').count(), 1);
          // Idle agents without a ticket fold beyond the fourth.
          assert.equal(await page.locator('.fold-more').innerText(), '+ 2 more idle: iter8-dedicated-reviewer (2h 11m), sm-scout (2h 41m)');
          const pageWidth = await page.evaluate(() => document.documentElement.scrollWidth);
          assert.ok(pageWidth <= viewport.width, `no horizontal scroll at ${viewport.width} px (got ${pageWidth})`);
          const name = `agents-${viewport.width}-${colorScheme}-${size}`;
          if (shots) await page.screenshot({ path: `${shots}/${name}.png`, fullPage: true });
          assert.deepEqual(errors, []);

          if (viewport.width === 1440 && colorScheme === 'light' && size === 15) {
            // Clicking an icon does not open the band.
            await card('iter8-run').getByRole('button', { name: 'Open in Claude (c)' }).click();
            assert.deepEqual(opened, ['https://claude.ai/code/session_run']);
            assert.equal(new URL(page.url()).searchParams.get('open'), null);
            // j and k move the ring in display order; x presses ✓ on the selected card.
            await page.locator('body').click({ position: { x: 5, y: 900 } });
            await page.keyboard.press('j');
            assert.equal(await page.locator('.card.kb .nm').innerText(), 'sm-1790-reviewer');
            await page.keyboard.press('j');
            await page.keyboard.press('k');
            await page.keyboard.press('j');
            assert.equal(await page.locator('.card.kb .nm').innerText(), 'sm-1726-engineer');
            await page.keyboard.press('x');
            await page.getByText('Marked answered').waitFor();
            assert.deepEqual(answered, ['sm-1726-engineer']);
            await page.waitForFunction(() => document.querySelectorAll('.grp.sec')[0].textContent.trim() === 'Needs you'
              && document.querySelectorAll('#sec-you + .cards .acard').length === 1);
            // The answered agent moved to Idle with the ring; k stops at the first card.
            for (let i = 0; i < 10; i++) await page.keyboard.press('k');
            assert.equal(await page.locator('.card.kb .nm').innerText(), 'sm-1790-reviewer');
            // c opens Claude for the selected card, Enter opens its details band.
            await page.keyboard.press('c');
            assert.deepEqual(opened.at(-1), 'https://claude.ai/code/session_rev');
            await page.keyboard.press('Enter');
            await page.waitForFunction(() => new URLSearchParams(location.search).get('open') === 'agent:sm-1790-reviewer');
            await page.locator('.details-band').waitFor();
            if (shots) await page.screenshot({ path: `${shots}/agents-band-1440-light-15.png`, fullPage: true });
            // The fold opens on click; By repo is today's grouping and is remembered.
            await page.locator('.fold-more').click();
            assert.equal(await page.locator('#sec-idle + .cards .acard').count(), 7);
            await page.getByRole('radio', { name: 'By repo' }).click();
            assert.deepEqual(await page.locator('.grp:not(.sec)').allTextContents(), ['fractal-algo-rust', 'session-manager']);
            assert.equal(await page.evaluate(() => localStorage.getItem('sm-agents-view')), '"repo"');
            // t opens the selected agent's terminal.
            await page.keyboard.press('t');
            await page.waitForFunction(() => location.pathname === '/terminal/sm-1790-reviewer');
            assert.deepEqual(errors, []);
          }
          await context.close();
        }
      }
    }
  } finally { await browser.close(); }
});
