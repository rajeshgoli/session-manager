import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const screenshots = process.env.SM_SCREENSHOT_DIR || '/private/tmp/sm-1836-screenshots';
const shell = `<!doctype html><html data-theme="light"><head><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/queue.css">
<script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script>
<script id="sm-config" type="application/json">{"inbox_token":"fixture"}</script>
<script type="module" src="/assets/app.js"></script></head><body><div id="app"></div></body></html>`;

test('work threads, reply targets, Archive and the fold at desktop and phone widths', async () => {
  await mkdir(screenshots, { recursive: true });
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const width of [1440, 390]) for (const theme of ['light', 'dark']) {
      const page = await browser.newPage({ viewport: { width, height: width === 390 ? 844 : 900 } });
      const errors = [], sends = [];
      page.on('pageerror', error => errors.push(error.message));
      let archived = false;
      const at = new Date().toISOString();
      const rows = () => [
        { thread_key:'ticket:owner/repo#1782', kind:'agent', title:'#1782 Fit and finish', repo:'owner/repo', status:'live', group:archived ? 'folded' : 'needs_you', folded_by:archived ? 'archived' : null,
          preview:'Your review · memo revision 3', agents:['sm-1782','sm-1782-2'], doc_count:2, revision_count:5, session_id:'successor', newest_at:at },
        { thread_key:'ticket:owner/repo#1768', kind:'doc', title:'#1768 One review command', repo:'owner/repo', status:'merged', group:'earlier', folded_by:null,
          preview:'Memo merged · sm-1768 ended', agents:['sm-1768'], doc_count:1, revision_count:2, session_id:'old', newest_at:at },
        { thread_key:'agent:reviewer', kind:'agent', title:'sm-reviewer', repo:'owner/repo', status:'ended', group:'folded', folded_by:'ended',
          preview:'Review posted', agents:['sm-reviewer'], doc_count:0, revision_count:0, session_id:'reviewer', newest_at:at },
      ];
      await page.route('http://localhost/**', async route => {
        const request = route.request(), url = new URL(request.url());
        if (url.pathname.startsWith('/assets/')) {
          const file = url.pathname.slice('/assets/'.length);
          return route.fulfill({ body:await readFile(new URL(file, assets)), contentType:file.endsWith('.css') ? 'text/css' : 'text/javascript' });
        }
        if (url.pathname.startsWith('/docs/')) return route.fulfill({ contentType:'text/html', body:`<!doctype html><title>Fit and finish memo</title><script>
          window.__smDoc = { config:{title:'Fit and finish memo',docId:'memo'}, openReview(){ document.body.append('Review sheet opened'); } };
        </script>` });
        if (request.isNavigationRequest()) return route.fulfill({ body:shell, contentType:'text/html' });
        if (url.pathname === '/inbox/archive') archived = true;
        if (url.pathname === '/inbox/unarchive') archived = false;
        if (url.pathname.endsWith('/send')) sends.push(request.postDataJSON());
        let data = {};
        if (url.pathname === '/inbox') data = { rows:rows().filter(row => url.searchParams.get('filter') === 'docs' ? row.doc_count : true) };
        else if (url.pathname === '/watch/state') data = { sessions:[] };
        else if (url.pathname.startsWith('/inbox/thread/')) data = {
          title:'#1782 Fit and finish', status:'live', can_send:true,
          reply_to:{ id:'successor', name:'sm-1782-2', restores:false, retired_at:null },
          reply_options:[
            { id:'successor', name:'sm-1782-2', status:'live', can_send:true, recipient_id:'successor', restores:false },
            { id:'original', name:'sm-1782', status:'ended', can_send:true, recipient_id:'original', restores:true, retired_at:'2026-09-30T12:00:00Z' },
          ],
          items:[
            { type:'message', at, sender:{id:'original',name:'sm-1782',status:'ended'}, html:'<div class="b"><h3>First review</h3><div class="md" data-msg="one"><p data-sm-line="1">Please review the first memo.</p></div></div>' },
            { type:'doc_revision', at, sender:{id:'original',name:'sm-1782',status:'ended'}, doc_id:'memo', pr:1785, sha:'abc1234567', review_state:'published', html:'<div class="ev">Published: <a href="/docs/owner/repo/memo?sha=abc1234567">Fit and finish memo</a></div>' },
            { type:'message', at, sender:{id:'successor',name:'sm-1782-2',status:'live'}, html:'<div class="b ask"><h3>Revision 3</h3><div class="md" data-msg="two"><p data-sm-line="1">The revised memo is ready.</p></div></div>' },
            { type:'doc_revision', at, sender:{id:'successor',name:'sm-1782-2',status:'live'}, doc_id:'memo', pr:1802, sha:'def1234567', review_state:'requested', html:'<div class="ev">Asked for review: <a href="/docs/owner/repo/memo?sha=def1234567">Fit and finish memo</a></div>' },
          ], review_asks:[] };
        await route.fulfill({ json:data });
      });
      await page.goto('http://localhost/inbox');
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      await page.locator('.inbox-row').first().waitFor();
      assert.equal(await page.locator('.inbox-row').count(), 2);
      assert.match(await page.locator('.inbox-fold-toggle').textContent(), /Folded · 1 threads \(sm-reviewer\)/);
      await page.screenshot({ path:`${screenshots}/${width}-${theme}-inbox.png`, fullPage:true });

      await page.locator('.inbox-row').first().click();
      await page.locator('.thread-doc-card').last().waitFor();
      assert.equal(await page.locator('.thread-doc-card').count(), 2);
      assert.deepEqual(await page.locator('.thread-entry .thread-sender').allTextContents(), ['sm-1782','sm-1782-2']);
      await page.getByRole('combobox', { name:'Reply to' }).selectOption('original');
      await page.getByText(/sm-1782 retired at .*replying brings it back/).waitFor();
      await page.getByRole('textbox', { name:'Reply' }).fill('One more question');
      await page.getByRole('button', { name:'Send', exact:true }).click();
      await page.getByRole('textbox', { name:'Reply' }).waitFor();
      await page.waitForFunction(() => document.querySelector('textarea[aria-label="Reply"]')?.value === '');
      assert.equal(sends.at(-1).to, 'original');
      await page.screenshot({ path:`${screenshots}/${width}-${theme}-thread.png`, fullPage:true });

      await page.locator('.thread-doc-card').last().getByRole('button', { name:'Review' }).click();
      await page.frameLocator('.doc-reader iframe').getByText('Review sheet opened').waitFor();

      await page.getByRole('button', { name:'← Inbox' }).click();
      await page.locator('.inbox-fold-toggle').click();
      assert.equal(await page.locator('.inbox-row').count(), 3);
      await page.screenshot({ path:`${screenshots}/${width}-${theme}-fold.png`, fullPage:true });
      await page.reload();
      await page.locator('.inbox-row').nth(2).waitFor();
      await page.locator('.inbox-entry').first().getByRole('button', { name:'Archive' }).click();
      await page.locator('.inbox-fold-toggle').getByText(/Folded · 2 threads/).waitFor();
      await page.locator('.inbox-entry').filter({ hasText:'#1782 Fit and finish' }).getByRole('button', { name:'Unarchive' }).click();
      await page.locator('.inbox-fold-toggle').getByText(/Folded · 1 threads/).waitFor();
      assert.deepEqual(errors, []);
      await page.close();
    }
  } finally { await browser.close(); }
});
