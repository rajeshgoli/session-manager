// Start now on the queue page (sm#2383): run with node --test and PLAYWRIGHT_MODULE if needed.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
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
const job = (id, label, state, extra = {}) => ({
  id, label, state, type: 'tests', argv: ['/bin/sleep', '30'], cwd: '/tmp', queued_at: '2026-10-19T18:00:00Z',
  started_at: state === 'running' ? '2026-10-19T18:10:00Z' : null, notify_name: 'sm-2374', notify_session_id: 'demo0002',
  requester_name: 'sm-2374', requester_session_id: 'demo0002', max_wait_seconds: 7200, timeout_seconds: 900, ...extra,
});
const queue = () => ({
  generated_at: new Date().toISOString(), ended: [], owner_name: 'Owner', host: { available: false },
  slots: { by_type: { tests: { running: 0, max: 2 }, background: { running: 1, max: 1 } } },
  running: [job('job_run', 'nightly-index-with-a-long-label', 'running', { type: 'background' })],
  queued: [1, 2].map((n) => job(`job_w${n}`, `2374-board-checks-and-a-much-longer-label-${n}`, 'pending', {
    position: n, holding_reason: 'concurrency_cap',
    holding: { summary: 'waiting for an available job slot', detail: 'Waiting for an available slot.', estimated_wait_seconds: 6899 },
  })),
});
const inViewport = async (page, locator, width) => {
  const box = await locator.boundingBox();
  assert.ok(box, 'element is rendered');
  assert.ok(box.x >= 0 && box.x + box.width <= width + 0.5, `inside ${width}px: x=${box.x} w=${box.width}`);
  assert.ok(box.y >= 0 && box.y + box.height <= 900.5, `fully visible: y=${box.y} h=${box.height}`);
};

for (const width of [390, 1440]) test(`Start now is one tap and asks in place only on memory risk at ${width}px`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    const starts = [];
    await page.route('https://queue.test/**', async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: shell });
      if (url.pathname === '/client/queue') return route.fulfill({ json: queue() });
      if (url.pathname === '/client/queue/stats') return route.fulfill({ json: { available: false, waiting: [] } });
      if (url.pathname === '/client/utilization/series') return route.fulfill({ json: { hours: 24, buckets: [] } });
      const start = url.pathname.match(/^\/client\/queue\/jobs\/([^/]+)\/start$/);
      if (start) {
        const body = route.request().postDataJSON();
        starts.push({ id: start[1], body });
        if (start[1] === 'job_w2' && body.ask_if_risky) return route.fulfill({ status: 409, json: {
          detail: 'Starting now could run the Mac low on memory.', code: 'start_risk',
          risk: 'This job may need 96 GB and the Mac has 40 GB free above its reserve.',
        } });
        return route.fulfill({ json: job(start[1], 'started', 'running') });
      }
      if (url.pathname.startsWith('/assets/')) return route.fulfill({
        contentType: url.pathname.endsWith('.css') ? 'text/css' : 'text/javascript',
        body: await readFile(new URL(url.pathname.slice('/assets/'.length), assets), 'utf8'),
      });
      return route.fulfill({ json: {} });
    });
    await page.goto('https://queue.test/');
    const pill = (n) => page.getByRole('button', { name: `Start 2374-board-checks-and-a-much-longer-label-${n} now` });
    await pill(1).waitFor();
    await inViewport(page, pill(1), width);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, 'no horizontal scroll');

    await pill(1).click();
    await page.waitForFunction(() => window.toasts.length === 1);
    assert.deepEqual(starts, [{ id: 'job_w1', body: { ask_if_risky: true } }]);
    assert.match(await page.evaluate(() => window.toasts[0]), /^Started 2374-board-checks/);

    await pill(2).click();
    const pop = page.getByRole('dialog', { name: 'Start now' });
    await pop.getByText('This job may need 96 GB', { exact: false }).waitFor();
    await inViewport(page, pop, width);
    if (process.env.SHOTS_DIR) await page.screenshot({ path: `${process.env.SHOTS_DIR}/queue-start-risk-${width}.png`, animations: "disabled" });
    await pop.getByRole('button', { name: 'Start anyway' }).click();
    await page.waitForFunction(() => window.toasts.length === 2);
    assert.deepEqual(starts.slice(1), [
      { id: 'job_w2', body: { ask_if_risky: true } },
      { id: 'job_w2', body: { ask_if_risky: false } },
    ]);
    assert.equal(await pop.count(), 0, 'popover closes after starting');
    if (process.env.SHOTS_DIR) await page.screenshot({ path: `${process.env.SHOTS_DIR}/queue-start-${width}.png`, fullPage: true });
  } finally { await browser.close(); }
});
