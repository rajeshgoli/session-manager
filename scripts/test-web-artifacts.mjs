import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const fixture = {
  can_act:true, keep_live_days:30, needs_you_count:1, mode:'report_only', epics:[], recent:[], backup:{runs:[]},
  filesystems:[{path:'/artifacts',bytes:1e12,free:4e11,floor:5e10,enough:2e11,outstanding:1e10,mounted:true}],
  artifacts:[{name:'research-data',root:'/artifacts',state:'live',location:'root',note:'Retained market data',description_html:'<h3>Retention</h3><p><strong>Keep</strong> <a href="https://example.com/source">source</a> and https://example.com/report.</p><table><thead><tr><th>File</th><th>Use</th></tr></thead><tbody><tr><td>data</td><td>training</td></tr></tbody></table>',report:{detail_json:'{"note":"Retain data"}'},report_html:'<ul><li>Retain <code>data</code></li></ul>',disk_bytes:1e10,hot:true,needs_owner:true,needs_you:{reasons:['unrecorded']},consumers:[],jobs:[],events:[],rebuildable:false,backup_due:true,created_at:'2026-10-15',dependents:[{name:'simulation'}]},
    {name:'frozen-data',root:'/artifacts',state:'frozen',location:'cold',note:'Completed research',disk_bytes:1e10,frozen_bytes:1e9,hot:false,consumers:[],jobs:[],events:[],backup_due:true}]
};
const shell = `<!doctype html><html data-theme="dark"><head><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/queue.css"><script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script></head><body><div id="fixture"></div><script type="module">import {render,h} from 'preact';import {ArtifactsPage} from '/assets/artifacts.js';render(h(ArtifactsPage),document.getElementById('fixture'));</script></body></html>`;
test('artifact fixture renders on desktop and phone with owner confirmation and errors', async () => {
  const browser = await chromium.launch({channel:'chrome',headless:true});
  try {
    for (const width of [1440,390]) {
      const page = await browser.newPage({viewport:{width,height:900}});
      const errors=[]; const posts=[];
      page.on('pageerror', e=>errors.push(e.message));
      await page.route('**/*',async route => {
        const req=route.request();const url=new URL(req.url());
        if(url.pathname.startsWith('/assets/')) return route.fulfill({body:await readFile(new URL(url.pathname.slice(8),assets)),contentType:url.pathname.endsWith('.css')?'text/css':'text/javascript'});
        if(req.isNavigationRequest()) return route.fulfill({body:shell,contentType:'text/html'});
        if(req.method()==='POST') { posts.push(req.postDataJSON()); return route.fulfill({status:409,json:{detail:'artifact is in use'}}); }
        return route.fulfill({json:url.pathname==='/artifacts'?fixture:{}});
      });
      await page.goto('https://sm.example.com/artifacts');
      await page.getByRole('heading',{name:'Taking up space'}).waitFor();
      if(process.env.SHOTS_DIR) {await mkdir(process.env.SHOTS_DIR,{recursive:true}); await page.screenshot({path:`${process.env.SHOTS_DIR}/artifacts-${width}.png`,fullPage:true});}
      await page.getByRole('button',{name:/Retained market data/}).click();
      await page.getByRole('region',{name:'Folder details'}).waitFor();
      assert.equal(await page.locator('.artifact-row').count(),2);
      assert.ok(await page.getByRole('heading',{name:'Retention',exact:true}).isVisible());
      assert.equal(await page.getByRole('link',{name:'source',exact:true}).getAttribute('href'),'https://example.com/source');
      assert.equal(await page.getByRole('link',{name:'https://example.com/report',exact:true}).getAttribute('target'),'_blank');
      assert.equal(await page.getByRole('table').count(),1);
      assert.equal(await page.locator('.artifact-markdown code').textContent(),'data');
      assert.equal(await page.getByText('Needs you',{exact:false}).count(),0);
      assert.ok(await page.getByRole('button',{name:'Keep 30 days',exact:true}).isVisible());
      assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
      await page.getByRole('button',{name:'Delete folder…',exact:true}).click();
      assert.equal(posts.length,0,'first click must only confirm');
      assert.ok(await page.getByText(/input of simulation/).isVisible());
      await page.getByRole('button',{name:'Delete folder',exact:true}).click();
      await page.getByRole('alert').filter({hasText:'artifact is in use'}).waitFor();
      assert.deepEqual(posts,[{root:'/artifacts',action:'delete'}]);
      if(process.env.SHOTS_DIR) {await mkdir(process.env.SHOTS_DIR,{recursive:true}); await page.screenshot({path:`${process.env.SHOTS_DIR}/artifacts-detail-${width}.png`,fullPage:true});}
      assert.deepEqual(errors,[]);
      await page.close();
    }
  } finally {await browser.close();}
});

// The whole app, rail included: the rail must stay put while the page scrolls.
const appShell = `<!doctype html><html data-theme="dark"><head><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/queue.css">
<script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script>
<script id="sm-config" type="application/json">{"inbox_token":"fixture"}</script>
<script type="module" src="/assets/app.js"></script></head><body><div id="app"></div></body></html>`;

test('investigate opens the New agent launcher with the brief, and the page scrolls under a fixed rail', async () => {
  const browser = await chromium.launch({channel:'chrome',headless:true});
  try {
    const page = await browser.newPage({viewport:{width:1440,height:420}});
    const errors=[]; const posts=[];
    page.on('pageerror', e=>errors.push(e.message));
    await page.route('**/*',async route => {
      const req=route.request();const url=new URL(req.url());
      if(url.pathname.startsWith('/assets/')) return route.fulfill({body:await readFile(new URL(url.pathname.slice(8),assets)),contentType:url.pathname.endsWith('.css')?'text/css':'text/javascript'});
      if(req.isNavigationRequest()) return route.fulfill({body:appShell,contentType:'text/html'});
      if(req.method()==='POST') { posts.push(req.postDataJSON()); return route.fulfill({json:{brief:'Investigate managed artifact research-data in /artifacts.',working_dir:'/repo/checkout'}}); }
      if(url.pathname==='/client/settings') return route.fulfill({json:{new_agent:{provider:'claude',workspaces:['/repo/checkout']}}});
      return route.fulfill({json:url.pathname==='/artifacts'?fixture:{}});
    });
    await page.goto('https://sm.example.com/artifacts');
    await page.getByRole('heading',{name:'Taking up space'}).waitFor();
    await page.mouse.move(700,300);
    await page.mouse.wheel(0,2000);
    await page.waitForTimeout(200);
    assert.equal(await page.evaluate(()=>document.scrollingElement.scrollTop),0,'the window must not scroll');
    assert.ok(await page.evaluate(()=>document.querySelector('.artifacts-scroll').scrollTop>0),'the page scrolls inside its own box');
    await page.getByRole('button',{name:/Retained market data/}).click();
    await page.getByRole('button',{name:'Ask an agent to investigate'}).click();
    const message = page.locator('textarea');
    await message.waitFor();
    assert.equal(await message.inputValue(),'Investigate managed artifact research-data in /artifacts.');
    assert.deepEqual(posts,[{root:'/artifacts',action:'brief'}],'nothing is started until the owner starts it');
    assert.ok(await page.locator('.rail').isVisible());
    assert.deepEqual(errors,[]);
  } finally {await browser.close();}
});
