// Exercise the terminal lifecycle without a browser, network, or real terminal.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
const strip = (text) => text.replace(/^import .*;\n/gm, '').replace(/^export /gm, '');
const web = (name) => readFile(new URL(`../crates/sm-server/src/web/${name}`, import.meta.url), 'utf8');
// The route module runs for real; it finds no direct entry and uses the relay.
const source = strip(await web('terminal-route.js')) + strip(await web('terminal-images.js')) + strip(await web('terminal.js'));
function fixture(direct = [], { storage, probes } = {}) {
  let minted = 0;
  const states = [], timers = new Map(), sockets = [];
  const effects = [], layoutEffects = [];
  let cleanup, timerId = 0, refIndex = 0;
  class Socket {
    static OPEN = 1;
    constructor(url) { this.url = url; this.readyState = 0; sockets.push(this); }
    send() {}
    close() { this.readyState = 3; }
    fail() { this.readyState = 3; this.onclose({code:1006}); }
    attach() { this.readyState = 1; this.onopen(); this.onmessage({data:JSON.stringify({type:'status',state:'attached'})}); }
  }
  class Terminal {
    constructor() { this.options = {}; }
    loadAddon() {} open() {} resize() {} reset() {} focus() {} dispose() {}
    onData() { return {dispose() {}}; } attachCustomKeyEventHandler() {}
  }
  const context = vm.createContext({
    URL, WebSocket:Socket, location:{href:'https://sm.example.com/terminal/test',protocol:'https:',hostname:'sm.example.com'},
    window:{Terminal,FitAddon:{FitAddon:class {proposeDimensions(){return null;}}},addEventListener(){},removeEventListener(){},
      innerWidth:1440,matchMedia:()=>({matches:false}),sessionStorage:storage},
    getComputedStyle:()=>({fontSize:'15px'}),document:{documentElement:{}},performance:{now:()=>0},
    CONTRAST_FLOOR:4.5, THEME_KEY:'theme', isLight:()=>false, terminalTheme:()=>({background:'#000'}),
    matchMedia:()=>({addEventListener(){},removeEventListener(){}}), MutationObserver:class {observe(){} disconnect(){}},
    setInterval:()=>0,clearInterval(){},config:{},stored:(key,fallback)=>fallback,store(){},sectionAgents:()=>[],
    SECTION_LABEL:{},SECTION_TONE:{},youFact:()=>null,jobsFact:()=>null,agentFact:()=>null,markAnswered(){},
    ResizeObserver:class {observe(){} disconnect(){}},
    isShell:()=>false, hiddenShells:()=>[], age:()=>'', basename:(path)=>path, openItem(){}, pairedText:()=>null,
    installFileLinks(){}, installPageMouse:()=>({dispose(){},copyKey:()=>false}),
    RetireButton:null, AgentMenus:null, ShellCloseButton:null, ShellMenu:null, ShellName:null, ShellAwayPanel:null, HiddenShells:null, setShellHidden(){},
    openPanel(){}, navigate(){}, openInClaude(){}, routeText:()=>'',
    useRef:initial=>({current:refIndex++ === 0 ? {style:{},addEventListener(){},removeEventListener(){}} : initial}), useState:initial=>{const index=states.push(initial)-1;return [initial,value=>{states[index]=value;}];},
    useLayoutEffect:fn=>{layoutEffects.push(fn);}, useEffect:fn=>{effects.push(fn);}, usePoll:()=>[null], panels:new Map(), html:()=>null, Icon:()=>null, Ring:()=>null, BugButton:()=>null,
    api:async()=>({ws_url:'/client/terminal',ticket_id:`test-${++minted}`,ticket_secret:'test',server_instance:'inst',direct}),
    fetch:async()=>({ok:true,json:async()=>({instance:probes?probes.shift()??'inst':'inst'})}),AbortController,toast(){},
    setTimeout:fn=>{timers.set(++timerId,fn);return timerId;},clearTimeout:id=>timers.delete(id),
  });
  // The app's poll of GET /watch/state: one live agent, so the page connects.
  vm.runInContext(source+'\nTerminalPage({id:"test",watch:[{sessions:[{id:"test",name:"test",state:"idle",attention:{section:"idle"}}]},null,()=>{}]});',context);
  // Run mount hooks in Preact order and retain every effect's cleanup.
  const cleanups = [...layoutEffects, ...effects].map(fn => fn()).filter(fn => typeof fn === 'function');
  cleanup=()=>cleanups.forEach(fn=>fn());
  const settle=async()=>{for(let n=0;n<40;n++)await Promise.resolve();};
  return {minted:()=>minted,states,sockets,timers,cleanup,settle,async tick(){const [id,fn]=timers.entries().next().value;timers.delete(id);fn();await settle();}};
}
test('failed handshakes stop after three retries and present a reconnect action',async()=>{
  const f=fixture();await f.settle();
  for(let n=0;n<3;n++){f.sockets.at(-1).fail();assert.equal(f.states[0],'Reconnecting');await f.tick();}
  f.sockets.at(-1).fail();assert.equal(f.sockets.length,4);assert.equal(f.timers.size,0);
  assert.equal(f.states[0],'Ended');assert.match(f.states[1],/choose Reconnect/);f.cleanup();
});
test('a successful attach resets the retry budget and cleanup cancels pending retries',async()=>{
  const f=fixture();await f.settle();
  for(let n=0;n<3;n++){f.sockets.at(-1).fail();await f.tick();}
  f.sockets.at(-1).attach();assert.equal(f.states[0],'Live');
  f.sockets.at(-1).fail();assert.equal(f.states[0],'Reconnecting');assert.equal(f.timers.size,1);
  f.cleanup();assert.equal(f.timers.size,0);
});
test('a stalled handshake ends without retrying or accepting a late attachment',async()=>{
  const f=fixture();await f.settle();
  const socket=f.sockets[0];assert.equal(f.timers.size,1);
  await f.tick();assert.equal(socket.readyState,3);assert.equal(f.states[0],'Ended');
  assert.match(f.states[1],/timed out/);assert.equal(f.timers.size,0);
  socket.fail();socket.attach();assert.equal(f.states[0],'Ended');
  assert.equal(f.sockets.length,1);assert.equal(f.timers.size,0);f.cleanup();
});
test('cleanup cancels a pending handshake deadline',async()=>{
  const f=fixture();await f.settle();assert.equal(f.timers.size,1);
  f.cleanup();assert.equal(f.timers.size,0);assert.equal(f.sockets[0].readyState,3);
});
test('a direct socket that closes before attaching mints a new ticket and uses the relay',async()=>{
  const f=fixture([{url:'ws://localhost:8420/client/terminal',probe:'http://localhost:8420/client/terminal/probe'}]);
  await f.settle();
  assert.equal(f.sockets[0].url,'ws://localhost:8420/client/terminal');
  f.sockets[0].fail();await f.settle();
  assert.equal(f.minted(),2);assert.equal(f.sockets.length,2);
  assert.equal(f.sockets[1].url,'wss://sm.example.com/client/terminal');
  assert.notEqual(f.states[0],'Reconnecting','the fallback is not a retry');
  f.sockets[1].attach();assert.equal(f.states[0],'Live');f.cleanup();
});
const memory=(items={})=>({getItem:k=>k in items?items[k]:null,setItem:(k,v)=>{items[k]=String(v);},items});
const DIRECT=[{url:'ws://localhost:8420/client/terminal',probe:'http://localhost:8420/client/terminal/probe'}];
test('on the relay soon after a direct socket, the page moves back to direct once its probe answers',async()=>{
  // A restart: the first probe misses, so this connection uses the relay.
  const storage=memory({'sm-term-direct-at':String(Date.now()-60000)});
  const f=fixture(DIRECT,{storage,probes:['other','other']});await f.settle();
  assert.equal(f.sockets[0].url,'wss://sm.example.com/client/terminal');
  f.sockets[0].attach();assert.equal(f.states[0],'Live');
  // The first background probe misses too; the second answers and the page reconnects directly.
  assert.equal(f.timers.size,1);await f.tick();
  assert.equal(f.sockets.length,1);assert.equal(f.timers.size,1);
  await f.tick();
  assert.equal(f.sockets[0].readyState,3);assert.equal(f.sockets.length,2);
  assert.equal(f.sockets[1].url,'ws://localhost:8420/client/terminal');
  assert.equal(JSON.parse(storage.items['sm-term-route']).url,'ws://localhost:8420/client/terminal');
  f.sockets[1].attach();assert.equal(f.states[0],'Live');assert.equal(f.timers.size,0);f.cleanup();
});
test('away from this machine (no recent direct socket) the relay probes nothing in the background',async()=>{
  const storage=memory();
  const f=fixture(DIRECT,{storage,probes:['other']});await f.settle();
  assert.equal(f.sockets[0].url,'wss://sm.example.com/client/terminal');
  f.sockets[0].attach();assert.equal(f.timers.size,0);f.cleanup();
});
test('background probes stop after five misses',async()=>{
  const storage=memory({'sm-term-direct-at':String(Date.now())});
  const f=fixture(DIRECT,{storage,probes:Array(6).fill('other')});await f.settle();
  f.sockets[0].attach();
  for(let n=0;n<5;n++){assert.equal(f.timers.size,1);await f.tick();}
  assert.equal(f.timers.size,0);assert.equal(f.sockets.length,1);f.cleanup();
});
test('a switch back to direct that fails returns to the relay and stops probing for this page',async()=>{
  const storage=memory({'sm-term-direct-at':String(Date.now())});
  const f=fixture(DIRECT,{storage,probes:['other']});await f.settle();
  f.sockets[0].attach();await f.tick();
  assert.equal(f.sockets[1].url,'ws://localhost:8420/client/terminal');
  f.sockets[1].fail();await f.settle();
  assert.equal(f.sockets[2].url,'wss://sm.example.com/client/terminal');
  f.sockets[2].attach();assert.equal(f.states[0],'Live');assert.equal(f.timers.size,0);f.cleanup();
});
test('a direct socket open longer than ten minutes still counts as recent when it drops',async()=>{
  const storage=memory();
  const f=fixture(DIRECT,{storage});await f.settle();
  f.sockets[0].attach();
  // Long before the restart.
  storage.items['sm-term-direct-at']=String(Date.now()-60*60000);
  f.sockets[0].fail();
  assert.ok(Date.now()-Number(storage.items['sm-term-direct-at'])<1000);f.cleanup();
});
