// Executes the shipped browser handlers against a small DOM double, as watch_client.test.cjs does.
const {test} = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const tick = () => new Promise(resolve => setImmediate(resolve));
class Element {
  constructor(dataset = {}) { this.dataset = dataset; this.listeners = {}; this.value = ''; this.options = []; this.type = 'button'; this.isConnected = true; }
  addEventListener(event, handler) { this.listeners[event] = handler; }
  querySelectorAll() { return []; }
  querySelector() { return null; }
  closest() { return this; }
  hasAttribute(key) { return key.slice(5).replace(/-([a-z])/g,(_,c)=>c.toUpperCase()) in this.dataset; }
  replaceChildren(...items) { this.options = items; this.value = items[0]?.value || ''; }
  showModal() { this.open = true; }
  close() { this.open = false; this.listeners.close?.(); }
}
async function harness(defaultModel = 'unavailable') {
  const ids = Object.fromEntries(['board','board-message','board-start','board-start-form','board-start-error','board-submit','board-model-note','board-cancel','board-start-title'].map(id=>[id,new Element()]));
  const fields = Object.fromEntries(['provider','model','reasoning_effort','name','brief'].map(name=>[name,new Element()]));
  const form = ids['board-start-form'];
  form.fields = fields; form.elements = {namedItem:name=>fields[name]};
  const calls = [], intervals = [], windowEvents = {};
  let failure, models = ['fable','opus'];
  const board = {html:'<p>lane</p>',start_defaults:{provider:'claude',model:defaultModel,reasoning_effort:'high'}};
  const fetch = async (path,options={}) => {
    calls.push({path,...options});
    if (path === '/client/board/start' && failure) return {ok:false,status:409,json:async()=>({detail:failure})};
    let result = path.startsWith('/client/session-models') ? {models} : path.startsWith('/client/board/start-options') ? {working_dir:'/repo/widgets',name:'widgets-2',brief:'Work ticket #2'} : path === '/client/board/start' ? {name:'widgets-2',session_id:'new00001'} : board;
    return {ok:true,json:async()=>result};
  };
  const document = {hidden:false,querySelector:selector=>ids[selector.slice(1)],addEventListener(){}};
  class FormData {
    constructor(form) { this.values = Object.entries(form.fields).map(([name,field])=>[name,field.value]); }
    get(name) { return this.values.find(([key])=>key===name)?.[1]; }
    [Symbol.iterator]() { return this.values[Symbol.iterator](); }
  }
  vm.runInNewContext(fs.readFileSync('crates/sm-server/src/http/board_client.js','utf8'), {
    document,fetch,FormData,Option:class {constructor(label,value){this.value=value;}},
    window:{addEventListener:(name,handler)=>windowEvents[name]=handler,dispatchEvent(){}},
    localStorage:{getItem(){return null},setItem(){}},location:{hash:''},
    setInterval:fn=>intervals.push(fn),setTimeout(){},Event:class {},console,
  });
  await tick();
  return {ids,fields,calls,intervals,document,windowEvents,fail:value=>failure=value,setModels:value=>models=value};
}

test('visible refresh reads before seen; hidden pages do not acknowledge; focus refreshes', async()=>{
  const h = await harness();
  assert.deepEqual(h.calls.slice(0,2).map(c=>c.path),['/client/board?html=true','/client/board/seen']);
  assert.equal(h.ids.board.innerHTML,'<p>lane</p>');
  h.document.hidden = true;
  const count = h.calls.length;
  await h.intervals[0]();
  assert.equal(h.calls.length,count);
  h.document.hidden = false;
  await h.windowEvents.focus();
  assert.equal(h.calls.at(-1).path,'/client/board/seen');
});

test('Add lane remains a native submit button and posts its form values',async()=>{
  const h = await harness();
  const button = new Element(); button.type='submit';
  await h.ids.board.listeners.click({target:button});
  assert.ok(!button.disabled);
  const form = new Element(); form.id='board-add';
  form.fields = {repo:{value:'acme/widgets'},number:{value:'7'}};
  form.querySelector = ()=>button;
  await h.ids.board.listeners.submit({target:form,preventDefault(){}});
  assert.deepEqual(JSON.parse(h.calls.find(c=>c.path==='/client/board/lanes').body),{repo:'acme/widgets',number:7});
});

test('Start uses resolved checkout and fallback model, shows errors, then refreshes on success',async()=>{
  const h = await harness();
  await h.ids.board.listeners.click({target:new Element({start:'2',repo:'acme/widgets'})});
  assert.ok(h.ids['board-start'].open);
  assert.equal(h.fields.name.value,'widgets-2');
  assert.equal(h.fields.model.value,'fable');
  assert.match(h.ids['board-model-note'].textContent,/unavailable/);
  assert.match(h.calls.find(c=>c.path.startsWith('/client/session-models')).path,/working_dir=%2Frepo%2Fwidgets/);
  h.fail('#2 is held by someone');
  await h.ids['board-start-form'].listeners.submit({preventDefault(){}});
  assert.equal(h.ids['board-start-error'].textContent,'#2 is held by someone');
  assert.ok(h.ids['board-start'].open);
  h.fail(null);
  await h.ids['board-start-form'].listeners.submit({preventDefault(){}});
  assert.equal(h.ids['board-start'].open,false);
  assert.equal(h.ids['board-message'].textContent,'Started widgets-2');
  const sent = JSON.parse(h.calls.filter(c=>c.path==='/client/board/start').at(-1).body);
  assert.equal(sent.number,2); assert.equal(sent.repo,'acme/widgets'); assert.equal(sent.model,'fable');
});

test('Start preselects and submits configured Opus 1M at high effort',async()=>{
  const h = await harness('opus[1m]');
  h.setModels(['fable','sonnet','opus','opus[1m]','haiku']);
  await h.ids.board.listeners.click({target:new Element({start:'2',repo:'acme/widgets'})});
  assert.equal(h.fields.model.value,'opus[1m]');
  assert.equal(h.fields.reasoning_effort.value,'high');
  assert.equal(h.ids['board-model-note'].textContent,'');
  await h.ids['board-start-form'].listeners.submit({preventDefault(){}});
  const sent = JSON.parse(h.calls.find(c=>c.path==='/client/board/start').body);
  assert.equal(sent.model,'opus[1m]');
  assert.equal(sent.reasoning_effort,'high');
});

test('nav_board_badge hides zero and clears after the board is seen',async()=>{
  const source = fs.readFileSync('crates/sm-server/src/owner_docs.rs','utf8');
  const start = source.indexOf("(() => {{\n  const badge");
  const script = source.slice(start,source.indexOf('</script>',start)).replaceAll('{{','{').replaceAll('}}','}');
  const badge = new Element(), handlers = {};
  let count = 3;
  vm.runInNewContext(script, {document:{hidden:false,getElementById:()=>badge}, window:{addEventListener:(name,fn)=>handlers[name]=fn},setInterval(){},fetch:async()=>({ok:true,json:async()=>({count})})});
  await tick(); assert.equal(badge.hidden,false); assert.equal(badge.textContent,'3');
  count=0; await handlers['sm-board-seen'](); assert.equal(badge.hidden,true);
});
