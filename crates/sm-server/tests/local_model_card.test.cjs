const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync('crates/sm-server/src/web/queue.js', 'utf8');
const component = source.slice(source.indexOf('export function LocalModelCard'), source.indexOf('export function MacChart')).replace('export ', '');
const context = vm.createContext({ html: (parts, ...values) => parts.reduce((out, part, i) => out + part + (values[i] ?? ''), '') });
vm.runInContext(component, context);
const cards = JSON.parse(fs.readFileSync('android-app/app/src/test/resources/local-model-card.json', 'utf8'));
for (const card of cards) {
  test(`web Queue renders ${card.state} / ${card.seats_text} with shared server wording`, () => {
    const rendered = context.LocalModelCard({ card });
    for (const text of ['Local model', 'Model', 'Seats', 'Memory headroom', card.model_text, card.seats_text, card.memory_text]) assert.ok(rendered.includes(text), text);
    if (card.reload_text) assert.ok(rendered.includes(card.reload_text));
    else assert.ok(!rendered.includes('<dt>Reload</dt>'));
  });
}
test('web Queue places the Local model card beside Mac memory and handles old responses', () => {
  assert.ok(source.includes('<${LocalModelCard} card=${queue.local_model} />'));
  assert.equal(context.LocalModelCard({}), null);
});
test('web agent row shows why a local agent is parked', () => {
  const agents = fs.readFileSync('crates/sm-server/src/web/agents.js', 'utf8');
  const fact = agents.slice(agents.indexOf('export function agentFact('), agents.indexOf('/** "▶', agents.indexOf('export function agentFact('))).replace('export ', '');
  const ctx = vm.createContext({ age: () => '' });
  vm.runInContext(fact, ctx);
  const label = 'parked: model yielded to perf run';
  assert.equal(ctx.agentFact({ facts: { agent: { state: 'parked', text: label } } }).text, label);
});
