// Run with node --test; install playwright or set PLAYWRIGHT_MODULE to its entry point.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assets = new URL('../crates/sm-server/src/web/', import.meta.url);
const shell = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="stylesheet" href="/assets/app.css"><link rel="stylesheet" href="/assets/queue.css">
<script type="importmap">{"imports":{"preact":"/assets/vendor/preact.module.js","preact/hooks":"/assets/vendor/hooks.module.js","htm":"/assets/vendor/htm.module.js"}}</script>
<script id="sm-config" type="application/json">{"inbox_token":"fixture"}</script>
<script type="module" src="/assets/app.js"></script></head><body><div id="app"></div></body></html>`;

test('History links and Inbox scrolling at desktop and mobile sizes', async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    for (const viewport of [{width:1440,height:1000}, {width:390,height:844}]) {
      const page = await browser.newPage({viewport});
      const errors = [];
      page.on('pageerror', error => errors.push(error.message));
      const requests = [];
      let count = 80, failSend = false, threadLoads = 0, answered = 0, retired = 0;
      await page.clock.install();
      await page.route('http://localhost/**', async route => {
        const request = route.request(), url = new URL(request.url());
        if (url.pathname.startsWith('/assets/')) {
          const file = url.pathname.slice('/assets/'.length);
          return route.fulfill({body:await readFile(new URL(file, assets)), contentType:file.endsWith('.css') ? 'text/css' : 'text/javascript'});
        }
        if (request.isNavigationRequest()) return route.fulfill({body:shell, contentType:'text/html'});
        let data = {};
        if (url.pathname.startsWith('/history')) {
          requests.push(url);
          data = {rows:[{repo:'owner/repo',number:1,title:'Fixture ticket',state:'open'}], next_before:'older-cursor',
            agents:[{id:'abc12345',name:'sm-1768',provider:'claude',state:'retired',ended_at:new Date(Date.now()-240000).toISOString(),
              working_dir:'/Users/fixture/projects/session-manager',restorable:false,unrestorable_reason:'Runs on node mini',
              last_turn:{at:'2026-09-30T12:00:00Z',text:'Memo 1768 is merged and published.'},
              work:{tickets:[{repo:'owner/repo',number:1768,title:'Memo',state:'closed'}],prs:[],docs:[]}}]};
        } else if (url.pathname === '/watch/state') {
          data = {sessions:retired ? [] : [{id:'fixture',name:'fixture-agent',provider:'claude',state:'idle',claims:[{kind:'ticket',repo:'owner/repo',number:1782}],jobs:[],
            facts:{agent:{state:'idle',since:new Date(Date.now()-300000).toISOString()},you:answered ? null : {kind:'message',dismissible:true}}}]};
        } else if (url.pathname === '/sessions/fixture/retire') {
          retired++;
        } else if (url.pathname === '/sessions/fixture/needs-you/answered') {
          answered++;
        } else if (url.pathname.endsWith('/send')) {
          if (failSend) return route.fulfill({status:500,json:{detail:'Send failed'}});
          count++;
        } else if (url.pathname.startsWith('/inbox/agent/')) {
          threadLoads++;
          data = {title:'Long thread',status:'working',can_send:true,reply_to:'Fixture agent',items:[{type:'turn',finished:true,at:'2026-09-30T12:00:00Z',html:'<p>Finished summary</p>'},
            {type:'turn',finished:false,at:'2026-09-30T12:05:00Z',html:'<p>Answer to the owner</p>'},
            ...Array.from({length:count}, (_,i) => ({html:`<div class="b"><p data-sm-line="1">Message ${i+1}: enough text to wrap over multiple lines on mobile.</p></div>`}))]};
        } else if (url.pathname === '/inbox') data = {rows:[]};
        else if (url.pathname.startsWith('/t/')) data = {item:{title:'Fixture ticket',state:'open'},events:[]};
        await route.fulfill({json:data});
      });
      const latest = () => requests.at(-1).searchParams;
      const change = async (key, value, action) => {
        const requested = page.waitForResponse(response => new URL(response.url()).searchParams.get(key) === value);
        await action();
        await requested;
      };
      // History opens on agents: name, ticket, age, folder, last turn, Restore.
      await page.goto('http://localhost/history');
      await page.getByRole('heading', {name:'Bring back an agent'}).waitFor();
      const agent = page.locator('.history-agent');
      await agent.waitFor();
      assert.equal(new URL(requests.at(-1)).pathname, '/history/agents');
      assert.match(await agent.textContent(), /sm-1768.*CLAUDE.*#1768.*retired 4m ago.*session-manager/s);
      assert.match(await agent.locator('.history-last-turn').textContent(), /Last turn: “Memo 1768 is merged/);
      const restore = agent.getByRole('button', {name:'Restore'});
      assert.ok(await restore.isDisabled());
      assert.equal(await restore.getAttribute('title'), 'Runs on node mini');
      await page.getByRole('radio', {name:'Tickets'}).click();
      await page.waitForFunction(() => location.pathname === '/history/tickets' && document.querySelector('.history-card button.text-button')?.textContent.includes('Fixture ticket'));
      // An older `/history` link that filters tickets lands on the tickets tab.
      for (const filter of ['agent=sm-test','repo=owner%2Frepo','open=1','agent=sm-test&repo=owner%2Frepo&open=1&limit=7&before=initial-cursor']) {
        const previous = requests.length;
        await page.goto(`http://localhost/history?${filter}`);
        await page.waitForFunction(() => document.querySelector('.history-card'));
        assert.ok(requests.length > previous);
        assert.equal(new URL(page.url()).pathname, '/history/tickets');
        for (const [key,value] of new URLSearchParams(filter)) assert.equal(latest().get(key),value);
        assert.equal(await page.locator('.list-filter input').inputValue(), new URLSearchParams(filter).get('repo') || '');
        assert.equal(await page.locator('.panel').count(), 0, 'open=1 is a filter, not a panel');
      }
      await change('before','older-cursor',() => page.getByRole('button', {name:'Older →'}).click());
      assert.equal(latest().get('limit'),'7');
      await change('repo','replacement/repo',() => page.locator('.list-filter input').fill('replacement/repo'));
      assert.equal(latest().get('before'),'');
      await change('repo','',() => page.locator('.list-filter input').fill(''));
      assert.equal(latest().get('agent'),'sm-test');
      assert.equal(latest().get('open'),'1');
      await page.getByRole('button', {name:'#1 Fixture ticket',exact:true}).click();
      await page.waitForFunction(() => new URLSearchParams(location.search).has('panel'));
      assert.equal(new URL(page.url()).searchParams.get('open'),'1');
      await page.keyboard.press('Escape');
      await page.waitForFunction(() => !new URLSearchParams(location.search).has('panel'));
      assert.equal(new URL(page.url()).searchParams.get('open'),'1');
      assert.equal(await page.locator('.list-filter input').inputValue(),'');

      // Wait for the data request, not the page navigation, which also carries `q`.
      const agentsRequest = page.waitForRequest(request => { const url = new URL(request.url());
        return url.pathname === '/history/agents' && url.searchParams.get('format') === 'json' && url.searchParams.get('q') === 'former'; });
      await page.goto('http://localhost/history/agents?q=former&limit=3');
      await agentsRequest;
      await page.waitForFunction(() => document.querySelector('.list-filter input')?.value === 'former');
      assert.equal(latest().get('limit'),'3');

      await page.goto('http://localhost/inbox?open=thread:fixture');
      // The thread header carries its agent's links and clears a question answered elsewhere.
      await page.locator('.thread-links').getByText('#1782 ↗').waitFor();
      assert.match(await page.locator('.thread-links').textContent(), /fixture-agent · Claude · ○ Idle 5m/);
      await page.getByRole('button', {name:'✓ Answered'}).click();
      await page.getByText('Marked answered').waitFor();
      await page.getByRole('button', {name:'✓ Answered'}).waitFor({state:'detached'});
      assert.equal(answered, 1);
      // A finished summary is the last turn; an answer to the owner is a reply.
      assert.deepEqual(await page.locator('.thread-items .turn .lbl').evaluateAll(nodes => nodes.map(node => node.textContent.split(' · ')[0])), ['Last turn', 'Reply']);
      const atBottom = () => {
        const el = document.querySelector('.thread-items');
        return el && el.scrollTop > 0 && Math.abs(el.scrollHeight-el.clientHeight-el.scrollTop) < 2;
      };
      await page.waitForFunction(atBottom);
      await page.locator('.thread-items').evaluate(el => {el.scrollTop=200;});
      const position = await page.locator('.thread-items').evaluate(el => el.scrollTop);
      const loads = threadLoads;
      count++;
      await page.clock.runFor(30100);
      await page.waitForFunction(n => document.querySelector('.thread-items').textContent.includes(`Message ${n}:`),count);
      assert.ok(threadLoads > loads);
      assert.equal(await page.locator('.thread-items').evaluate(el => el.scrollTop), position, 'poll preserves reading position');
      failSend = true;
      await page.getByRole('textbox', {name:'Reply'}).fill('Reply fixture');
      await page.getByRole('button', {name:'Send',exact:true}).click();
      await page.getByRole('alert').filter({hasText:'Send failed'}).waitFor();
      assert.equal(await page.locator('.thread-items').evaluate(el => el.scrollTop), position, 'failed send preserves reading position');
      failSend = false;
      await page.getByRole('button', {name:'Send',exact:true}).click();
      await page.waitForFunction(atBottom);
      assert.equal(await page.getByRole('textbox', {name:'Reply'}).inputValue(),'');
      await page.locator('.thread-items').evaluate(el => {el.scrollTop=300;});
      await page.clock.runFor(30100);
      assert.equal(await page.locator('.thread-items').evaluate(el => el.scrollTop),300,'post-send polling preserves reading position');
      // Retire sits beside Done while the agent is live, behind an inline confirm.
      await page.getByRole('button', {name:'Retire…'}).click();
      await page.getByText('Retire fixture-agent?').waitFor();
      await page.getByRole('button', {name:'Retire',exact:true}).click();
      await page.getByText('Retired fixture-agent').waitFor();
      await page.getByRole('button', {name:'Retire…'}).waitFor({state:'detached'});
      assert.equal(retired, 1);
      assert.deepEqual(errors,[]);
      await page.close();
    }
  } finally { await browser.close(); }
});
