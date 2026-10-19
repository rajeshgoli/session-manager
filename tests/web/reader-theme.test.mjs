import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {followReaderTheme} from '../../crates/sm-server/src/web/reader-theme.js';

test('reader applies Appearance on load, changes, and OS changes; disconnects on close',()=>{
 const listeners={}; let setting='light', observed, disconnected=false;
 const inner={dataset:{theme:'dark'}}; const outer={documentElement:{dataset:{}}};
 const frame={contentWindow:{document:{documentElement:inner}},addEventListener:(k,f)=>listeners[k]=f,removeEventListener:k=>delete listeners[k]};
 const media={matches:true,addEventListener:(k,f)=>listeners.media=f,removeEventListener:()=>delete listeners.media};
 class Observer { constructor(f){observed=f;} observe(){} disconnect(){disconnected=true;} }
 const stop=followReaderTheme(frame,outer,{getItem:()=>setting},media,Observer);
 assert.equal(inner.dataset.theme,'light');
 inner.dataset.theme='dark';listeners.load();assert.equal(inner.dataset.theme,'light');
 setting='dark';observed();assert.equal(inner.dataset.theme,'dark');
 setting='system';media.matches=false;listeners.media();assert.equal(inner.dataset.theme,'light');
 media.matches=true;listeners.media();assert.equal(inner.dataset.theme,'dark');
 stop();assert.equal(disconnected,true);assert.deepEqual(listeners,{});
});

test('memo button respects reader default, explicitly selects light on dark OS, and tracks external changes',()=>{
 const html=readFileSync('docs/memo_template.html','utf8');
 const script=html.match(/<script>([\s\S]*?)<\/script>/)[1];
 const attrs={}, buttons=[]; let observe;
 const root={getAttribute:k=>attrs[k],hasAttribute:k=>k in attrs,setAttribute:(k,v)=>attrs[k]=v,removeAttribute:k=>delete attrs[k]};
 const context={document:{documentElement:root,createElement:()=>({}),body:{appendChild:b=>buttons.push(b)}},window:{frameElement:{}},
   localStorage:{getItem:()=>{throw Error('should not restore in reader');},setItem:()=>{throw Error('should not save in reader');}},
   matchMedia:()=>({matches:true,addEventListener:()=>{}}),MutationObserver:class { constructor(f){observe=f;} observe(){} }};
 vm.runInNewContext(script,context);
 const button=buttons[0];assert.equal(button.textContent,'☀︎ Light');button.onclick();assert.equal(attrs['data-theme'],'light');assert.equal(button.textContent,'☾ Dark');
 root.setAttribute('data-theme','dark');observe();assert.equal(button.textContent,'☀︎ Light');
});

test('doc rows open stored pages inside the reader and external notes in a browser',()=>{
 const source=readFileSync('crates/sm-server/src/web/ui.js','utf8');
 const functionSource=source.slice(source.indexOf('export function openDoc('),source.indexOf('/** Shared related-item chips.')).replace('export ','');
 const panels=[],external=[];
 const context={URL,location:{origin:'https://sm.example.com'},openPanel:p=>panels.push(p),window:{open:(...args)=>external.push(args)}};
 vm.runInNewContext(`${functionSource}\nopenDoc('/docs/~handoff/agent');openDoc('https://github.com/a/b/issues/1');openDoc('javascript:alert(1)');`,context);
 assert.deepEqual(panels,['doc:/docs/~handoff/agent']);assert.deepEqual(external,[['https://github.com/a/b/issues/1','_blank','noopener']]);
});
