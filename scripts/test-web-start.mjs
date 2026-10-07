// New agent workspace regression: run with node --test and PLAYWRIGHT_MODULE if needed.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const shell = `<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="stylesheet" href="/assets/app.css">
<script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script>
<script id="sm-config" type="application/json">{"inbox_token":"fixture"}</script>
<div id="app"></div><script type="module">
import {render} from 'preact'; import {html} from '/assets/ui.js';
import {NewAgentPopover} from '/assets/start.js';
render(html\`<\${NewAgentPopover} onClose=\${()=>{window.closedPopover=true}}/>\`,document.getElementById('app'));
</script>`;
for (const width of [390, 1440]) test(`New agent offers project folders and submits home paths at ${width}px`, async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    const requests = [];
    await page.route('https://start.test/**', async route => {
      const url = new URL(route.request().url());
      const json = body => route.fulfill({ json: body });
      if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: shell });
      if (url.pathname === '/client/settings') return json({
        new_agent: { provider: 'claude', claude: {}, codex: {}, workspaces: ['/Users/r/projects/saved'], agent_types: [] },
        workspace_folders: ['another', 'saved', 'backup-manager', 'finviz', 'deskbar', 'codex-fork', 'session-manager', 'fractal-algo-rust', ...Array.from({length: 12}, (_, i) => `extra-${i}`)].map(name => `/Users/r/projects/${name}`),
      });
      if (url.pathname === '/watch/state') return json({ sessions: [
        { state: 'idle', repo: '/Users/r/worktrees/live' },
        { state: 'stopped', repo: '/Users/r/worktrees/stopped' },
      ] });
      if (url.pathname === '/client/session-models') { requests.push(url); return json({ models: [] }); }
      if (url.pathname === '/client/sessions') { requests.push(route.request().postDataJSON()); return json({ id: 'new', name: 'test' }); }
      if (url.pathname.startsWith('/assets/')) return route.fulfill({
        contentType: url.pathname.endsWith('.css') ? 'text/css' : 'text/javascript',
        body: await readFile(new URL(url.pathname.slice('/assets/'.length), assets), 'utf8'),
      });
      return route.abort();
    });
    await page.goto('https://start.test/');
    await page.getByRole('button', { name: 'Change', exact: true }).click();
    const workspace = page.locator('select').filter({ has: page.locator('option[value="__other__"]') });
    assert.deepEqual(await workspace.locator('option').evaluateAll(options => options.map(o => o.value)), [
      ...['fractal-algo-rust', 'session-manager', 'codex-fork', 'deskbar', 'finviz', 'backup-manager', 'saved', 'another', 'extra-0', 'extra-1'].map(name => `/Users/r/projects/${name}`),
      '__more__', '__other__',
    ]);
    await workspace.selectOption('__more__');
    assert.equal(await workspace.inputValue(), '/Users/r/projects/saved');
    const all = await workspace.locator('option').evaluateAll(options => options.map(o => o.value));
    assert.equal(all.length, 22);
    assert.equal(new Set(all).size, all.length);
    assert.ok(all.includes('/Users/r/projects/extra-11'));
    assert.ok(all.includes('/Users/r/worktrees/live'));
    assert.ok(!all.includes('/Users/r/worktrees/stopped'));
    assert.ok(!all.includes('__more__'));
    const moreResponse = page.waitForResponse(r => new URL(r.url()).searchParams.get('working_dir') === '/Users/r/projects/extra-11');
    await workspace.selectOption('/Users/r/projects/extra-11');
    await moreResponse;
    assert.equal(await workspace.inputValue(), '/Users/r/projects/extra-11');
    await workspace.selectOption('/Users/r/projects/another');
    await page.waitForFunction(() => document.body.textContent.includes('Workspace'));
    await workspace.selectOption('__other__');
    const input = page.getByPlaceholder('~/projects/repo');
    await input.fill('relative/path');
    await page.getByRole('button', { name: 'Start', exact: true }).click();
    await page.getByText('Choose a workspace: an absolute path or ~/ path.', { exact: true }).waitFor();
    assert.equal(requests.filter(r => !(r instanceof URL)).length, 0);
    const modelsResponse = page.waitForResponse(r => new URL(r.url()).searchParams.get('working_dir') === '~/projects/another');
    await input.fill(' ~/projects/another ');
    await modelsResponse;
    await page.getByRole('button', { name: 'Start', exact: true }).click();
    await page.waitForFunction(() => window.closedPopover);
    assert.equal(requests.at(-1).working_dir, '~/projects/another');
  } finally { await browser.close(); }
});
