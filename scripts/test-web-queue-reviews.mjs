// Reviews in progress on the queue page, with Next reviewer (#2586).
// Run with node --test and PLAYWRIGHT_MODULE if needed; SHOTS_DIR saves screenshots.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const { reviewRowText } = await import('../crates/sm-server/src/web/queue-model.js');
const SHOTS = process.env.SHOTS_DIR;
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const shell = `<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/queue.css">
<style>html{font-size:15px} html,body,#app{height:100%;margin:0} #app{display:flex;flex-direction:column}</style>
<script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script>
<script id="sm-config" type="application/json">{"inbox_token":"fixture"}</script>
<div id="app"></div><script type="module">
import {render} from 'preact'; import {html, bus} from '/assets/ui.js';
import {QueuePage} from '/assets/queue.js';
window.toasts = []; bus.on('toast', (t) => window.toasts.push(t.text));
render(html\`<\${QueuePage}/>\`,document.getElementById('app'));
</script>`;

const minutesAgo = (m) => new Date(Date.now() - m * 60000).toISOString();
const review = (id, extra) => ({
  request_id: id, repo: 'rajeshgoli/session-manager', round: 1, policy_source: 'default', why: 'default',
  author_name: 'sm-2575', step: 0, job_id: null, ...extra,
});
const reviews = () => [
  review('r-icons', { pr_number: 2576, title: 'web: give Artifacts, Notes and Guestbook their own sidebar icons',
    kind: 'github_codex', step_state: 'picked_up', reviewer_label: 'GitHub Codex', since: minutesAgo(34), next_label: 'Codex run (gpt-6-sol, medium)' }),
  review('r-tmux', { pr_number: 2578, title: 'Hide the tmux status bar on sm’s tmux server', round: 2, step: 2,
    kind: 'claude', step_state: 'ready', job_id: 'job_rev', reviewer_label: 'Claude run (opus, high)', since: minutesAgo(3), next_label: null }),
];
const queue = (rows) => ({
  generated_at: new Date().toISOString(), ended: [], owner_name: 'Rajesh', host: { available: false },
  slots: { by_type: { review: { running: 1, max: 2 } } }, running: [], queued: [], reviews: rows,
});

test('review rows say who holds the review, for how long, and what Next reviewer does', () => {
  const now = Date.now();
  const [icons, tmux] = reviews().map((r) => reviewRowText(r, now));
  assert.equal(icons.reviewer, 'GitHub Codex');
  assert.equal(icons.status, 'Reviewing · 34 min');
  assert.equal(icons.context, 'session-manager · for sm-2575');
  assert.equal(icons.ask, 'Hand to Codex run?');
  assert.equal(icons.askDetail, 'gpt-6-sol · medium');
  assert.equal(tmux.reviewer, 'Claude run');
  assert.equal(tmux.model, 'opus · high');
  assert.equal(tmux.context, 'session-manager · for sm-2575 · round 2');
  assert.equal(tmux.ask, 'No one is left to review. End it and tell sm-2575?');
  assert.equal(tmux.confirm, 'End review');
  assert.equal(reviewRowText({ ...reviews()[0], step_state: 'posted', since: minutesAgo(0) }, now).status, 'Waiting for Codex to start · just now');
  assert.equal(reviewRowText({ ...reviews()[1], step_state: 'queued' }, now).status, 'Waiting for a review slot · 3 min');
});

for (const width of [390, 1440]) test(`Next reviewer asks in place, names the step, and handles a review that moved on at ${width}px`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    let rows = reviews();
    const posts = [];
    await page.route('https://queue.test/**', async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: shell });
      if (url.pathname === '/client/queue') return route.fulfill({ json: queue(rows) });
      if (url.pathname === '/client/queue/stats') return route.fulfill({ json: { available: false, waiting: [] } });
      if (url.pathname === '/client/utilization/series') return route.fulfill({ json: { hours: 24, buckets: [] } });
      const next = url.pathname.match(/^\/client\/review-requests\/([^/]+)\/next$/);
      if (next) {
        posts.push({ id: next[1], body: route.request().postDataJSON() });
        if (next[1] === 'r-tmux') return route.fulfill({ status: 409, json: { detail: 'This review has already moved on.' } });
        rows = rows.filter((r) => r.request_id !== next[1]);
        return route.fulfill({ json: { id: next[1] } });
      }
      if (url.pathname.startsWith('/assets/')) return route.fulfill({
        contentType: url.pathname.endsWith('.css') ? 'text/css' : 'text/javascript',
        body: await readFile(new URL(url.pathname.slice('/assets/'.length), assets), 'utf8'),
      });
      return route.fulfill({ json: {} });
    });
    await page.goto('https://queue.test/');
    const card = page.locator('.q-reviews');
    await card.getByText('2 in progress').waitFor();
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, 'no horizontal scroll');
    const icons = card.locator('.q-review', { hasText: '#2576' });
    assert.equal(await icons.locator('a.q-review-pr').getAttribute('href'), 'https://github.com/rajeshgoli/session-manager/pull/2576');
    if (SHOTS) await card.screenshot({ path: `${SHOTS}/queue-reviews-${width}.png` });

    // Asking first: nothing is sent until the owner confirms; Keep backs out.
    await icons.getByRole('button', { name: 'Next reviewer' }).click();
    await icons.getByText('Hand to Codex run?').waitFor();
    if (SHOTS) await card.screenshot({ path: `${SHOTS}/queue-reviews-ask-${width}.png` });
    await icons.getByRole('button', { name: 'Keep' }).click();
    assert.deepEqual(posts, []);
    await icons.getByRole('button', { name: 'Next reviewer' }).click();
    await icons.getByRole('button', { name: 'Hand over' }).click();
    await page.waitForFunction(() => window.toasts.length === 1);
    assert.deepEqual(posts, [{ id: 'r-icons', body: { step: 0 } }]);
    assert.equal(await page.evaluate(() => window.toasts[0]), 'PR #2576 handed to Codex run');
    await card.getByText('1 in progress').waitFor();

    // The server says the review already moved on: say so plainly.
    const tmux = card.locator('.q-review', { hasText: '#2578' });
    await tmux.getByRole('button', { name: 'Next reviewer' }).click();
    await tmux.getByRole('button', { name: 'End review' }).click();
    await page.waitForFunction(() => window.toasts.length === 2);
    assert.deepEqual(posts[1], { id: 'r-tmux', body: { step: 2 } });
    assert.equal(await page.evaluate(() => window.toasts[1]), 'This review has already moved on.');
  } finally { await browser.close(); }
});

test('no reviews in progress: no Reviews card', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.route('https://queue.test/**', async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: shell });
      if (url.pathname === '/client/queue') return route.fulfill({ json: queue([]) });
      if (url.pathname.startsWith('/assets/')) return route.fulfill({
        contentType: url.pathname.endsWith('.css') ? 'text/css' : 'text/javascript',
        body: await readFile(new URL(url.pathname.slice('/assets/'.length), assets), 'utf8'),
      });
      return route.fulfill({ json: {} });
    });
    await page.goto('https://queue.test/');
    await page.getByText('No jobs running.').waitFor();
    assert.equal(await page.locator('.q-reviews').count(), 0);
  } finally { await browser.close(); }
});
