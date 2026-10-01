// Exercise the terminal lifecycle without a browser, network, or real terminal.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
const strip = (text) => text.replace(/^import .*;\n/gm, '').replace(/^export /gm, '');
const web = (name) => readFile(new URL(`../crates/sm-server/src/web/${name}`, import.meta.url), 'utf8');
// The route module runs for real; it finds no direct entry and uses the relay.
const source = strip(await web('terminal-route.js')) + strip(await web('terminal.js'));
function fixture(direct = []) {
  let minted = 0;
  const states = [], timers = new Map(), sockets = [];
  let effect, cleanup, timerId = 0;
  class Socket {
    static OPEN = 1;
    constructor(url) { this.url = url; this.readyState = 0; sockets.push(this); }
    send() {}
    close() { this.readyState = 3; }
    fail() { this.readyState = 3; this.onclose({code:1006}); }
    attach() { this.readyState = 1; this.onopen(); this.onmessage({data:JSON.stringify({type:'status',state:'attached'})}); }
  }
  class Terminal {
    loadAddon() {} open() {} resize() {} reset() {} focus() {} dispose() {}
    onData() { return {dispose() {}}; } attachCustomKeyEventHandler() {}
  }
  const context = vm.createContext({
    URL, WebSocket:Socket, location:{href:'https://sm.example.com/terminal/test',protocol:'https:',hostname:'sm.example.com'},
    window:{Terminal,FitAddon:{FitAddon:class {proposeDimensions(){return null;}}},addEventListener(){},removeEventListener(){},
      innerWidth:1440,matchMedia:()=>({matches:false})},
    getComputedStyle:()=>({fontSize:'15px'}),document:{documentElement:{}},performance:{now:()=>0},
    setInterval:()=>0,clearInterval(){},config:{},stored:(key,fallback)=>fallback,store(){},sectionAgents:()=>[],
    SECTION_LABEL:{},SECTION_TONE:{},youFact:()=>null,jobsFact:()=>null,agentFact:()=>null,markAnswered(){},
    ResizeObserver:class {observe(){} disconnect(){}},
    useRef:()=>({current:null}), useState:initial=>{const index=states.push(initial)-1;return [initial,value=>{states[index]=value;}];},
    useEffect:fn=>{effect=fn;}, usePoll:()=>[null], panels:new Map(), html:()=>null, Icon:()=>null, Ring:()=>null,
    api:async()=>({ws_url:'/client/terminal',ticket_id:`test-${++minted}`,ticket_secret:'test',server_instance:'inst',direct}),
    fetch:async()=>({ok:true,json:async()=>({instance:'inst'})}),AbortController,
    setTimeout:fn=>{timers.set(++timerId,fn);return timerId;},clearTimeout:id=>timers.delete(id),
  });
  vm.runInContext(source+'\nTerminalPage({id:"test",open:false});',context);
  cleanup=effect();
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
