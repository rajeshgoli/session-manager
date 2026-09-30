// Run with: node tests/unit/web_terminal_behavior.cjs
// Exercise transport lifecycle without a browser or live agent.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const source = fs.readFileSync(path.join(__dirname, '../../crates/sm-server/src/web/terminal.js'), 'utf8')
  .replace(/^import .*;\n/gm, '').replace('export function TerminalPage', 'function TerminalPage');

function harness() {
  const refs = [], states = [], sockets = [], timers = new Map(), listeners = new Map();
  let effect, disposed = false, requests = 0, timerId = 0, terminal;
  class Socket {
    static OPEN = 1;
    constructor(url) { this.url = url; this.sent = []; this.readyState = 0; sockets.push(this); }
    send(value) { this.sent.push(JSON.parse(value)); }
    close() { this.readyState = 3; this.onclose?.({code:1000}); }
    open() { this.readyState = 1; this.onopen(); }
    frame(value) { this.onmessage({data:JSON.stringify(value)}); }
    drop() { this.readyState = 3; this.onclose({code:1006}); }
  }
  class Terminal {
    constructor() { terminal = this; this.output = []; }
    loadAddon() {} open() {} reset() {} focus() {}
    resize(cols, rows) { this.cols = cols; this.rows = rows; }
    write(bytes, done) { this.output.push(Buffer.from(bytes).toString()); done(); }
    onData(fn) { this.input = fn; return {dispose(){}}; }
    attachCustomKeyEventHandler(fn) { this.keyboard = fn; }
    dispose() { disposed = true; }
  }
  const context = vm.createContext({
    useRef: (value) => { const ref = {current:value}; refs.push(ref); return ref; },
    useState: (value) => { const i = states.length; states.push(value); return [value, (next) => { states[i] = typeof next === 'function' ? next(states[i]) : next; }]; },
    useEffect: (fn) => { effect = fn; }, usePoll: () => [null],
    html: () => null, panels:new Map(), Icon(){}, Ring(){}, openPanel(){}, closePanel(){}, navigate(){}, openInClaude(){},
    api: async () => { requests++; return {ticket_id:'ticket-'+requests, ticket_secret:'secret', ws_url:'/client/terminal'}; },
    window: {Terminal, FitAddon:{FitAddon:class {proposeDimensions(){return {cols:85, rows:30};}}},
      addEventListener:(name, fn) => listeners.set(name, fn), removeEventListener:(name) => listeners.delete(name)},
    ResizeObserver:class {observe(){} disconnect(){}}, WebSocket:Socket,
    setTimeout:(fn, delay) => { const id = ++timerId; timers.set(id, {fn, delay}); return id; },
    clearTimeout:(id) => timers.delete(id), URL, location:{href:'https://sm.example.com/terminal/test', protocol:'https:'},
    atob:(s) => Buffer.from(s, 'base64').toString('binary'), Uint8Array, history:{length:1},
  });
  vm.runInContext(source+'\nTerminalPage({id:"test", open:null});', context);
  const cleanup = effect();
  return {refs, states, sockets, timers, listeners, cleanup, get terminal(){return terminal;}, get disposed(){return disposed;}, get requests(){return requests;}};
}

(async () => {
  const h = harness();
  await new Promise(setImmediate);
  const ws = h.sockets[0];
  assert.equal(ws.url.href, 'wss://sm.example.com/client/terminal');
  ws.open();
  assert.equal(ws.sent[0].type, 'auth');
  assert.equal(ws.sent[0].output_ack, true);
  assert.equal(ws.sent[1].type, 'resize');
  h.terminal.input('before live');
  assert.equal(ws.sent.length, 2);
  ws.frame({type:'status', state:'attached'});
  assert.equal(h.states[0], 'Live');
  h.terminal.input('é'.repeat(9000));
  assert.equal(ws.sent.filter(f => f.type === 'input').map(f => f.data).join('').length, 9000);
  assert.ok(ws.sent.filter(f => f.type === 'input').every(f => f.data.length <= 4096));
  h.refs[1].current.model();
  assert.equal(ws.sent.at(-1).data, '\x1b[200~/model\x1b[201~\r');
  h.refs[1].current.key('shift-tab');
  assert.equal(ws.sent.at(-1).key, 'shift-tab');
  ws.frame({type:'output', data:Buffer.from('café').toString('base64'), sequence:7});
  assert.equal(h.terminal.output[0], 'café');
  assert.equal(ws.sent.at(-1).sequence, 7);
  ws.drop();
  assert.equal(h.states[0], 'Reconnecting');
  assert.equal([...h.timers.values()][0].delay, 500);
  for (let i = 0; i < 7; i++) {
    const [id, timer] = [...h.timers][0]; h.timers.delete(id); timer.fn(); await new Promise(setImmediate);
    const next = h.sockets.at(-1); next.open(); next.drop();
  }
  assert.equal([...h.timers.values()][0].delay, 10000);
  assert.equal(h.requests, 8);
  h.cleanup();
  assert.equal(h.timers.size, 0);
  assert.ok(h.disposed);

  const end = harness(); await new Promise(setImmediate);
  end.sockets[0].open(); end.sockets[0].frame({type:'exit', reason:'session ended'});
  assert.equal(end.states[0], 'Ended'); assert.equal(end.timers.size, 0); end.cleanup();

  const leave = harness(); await new Promise(setImmediate); leave.sockets[0].open();
  leave.listeners.get('pagehide')();
  assert.equal(leave.sockets[0].sent.at(-1).type, 'detach');
  assert.equal(leave.timers.size, 0); leave.cleanup();
  console.log('Browser terminal lifecycle: passed');
})().catch(error => {console.error(error); process.exitCode = 1;});
