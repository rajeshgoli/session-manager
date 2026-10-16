import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const fixture = {
  can_act:true, keep_live_days:30, needs_you_count:1, mode:'report_only', epics:[], recent:[], backup:{runs:[]},
  filesystems:[{path:'/artifacts',bytes:1e12,free:4e11,floor:5e10,enough:2e11,outstanding:1e10,mounted:true}],
  artifacts:[{name:'research-data',root:'/artifacts',state:'live',location:'root',note:'Retained market data',disk_bytes:1e10,hot:true,needs_owner:true,needs_you:{reasons:['unrecorded']},consumers:[],jobs:[],events:[],rebuildable:false,backup_due:true,created_at:'2026-10-15',dependents:[{name:'simulation'}]},
    {name:'frozen-data',root:'/artifacts',state:'frozen',location:'cold',note:'Completed research',disk_bytes:1e10,frozen_bytes:1e9,hot:false,consumers:[],jobs:[],events:[],backup_due:true}]
};
const shell = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/queue.css"><script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script></head><body><div id="fixture"></div><script type="module">import {render,h} from 'preact';import {ArtifactsPage} from '/assets/artifacts.js';render(h(ArtifactsPage),document.getElementById('fixture'));</script></body></html>`;
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
      await page.goto('https://sm.example.com/artifacts?name=research-data');
      await page.getByText('Retained market data',{exact:true}).waitFor();
      assert.equal(await page.locator('.artifact-row').count(),2);
      assert.ok(await page.getByText('Needs you · 1',{exact:false}).isVisible());
      assert.ok(await page.getByRole('button',{name:'Keep live 30 days',exact:true}).isVisible());
      assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
      await page.getByRole('button',{name:'Delete',exact:true}).click();
      assert.equal(posts.length,0,'first click must only confirm');
      assert.ok(await page.getByText(/input of simulation/).isVisible());
      await page.getByRole('button',{name:'Delete',exact:true}).click();
      await page.getByRole('alert').filter({hasText:'artifact is in use'}).waitFor();
      assert.deepEqual(posts,[{root:'/artifacts',action:'delete'}]);
      if(process.env.SHOTS_DIR) {await mkdir(process.env.SHOTS_DIR,{recursive:true}); await page.screenshot({path:`${process.env.SHOTS_DIR}/artifacts-${width}.png`,fullPage:true});}
      assert.deepEqual(errors,[]);
      await page.close();
    }
  } finally {await browser.close();}
});
