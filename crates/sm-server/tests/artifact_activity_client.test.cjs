// The web Artifacts page and the Android app word freezes, thaws and moves
// alike: both read this fixture (sm#2325).
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const fixture = JSON.parse(fs.readFileSync('android-app/app/src/test/resources/artifact-activity.json', 'utf8'));
const load = () => import(require('node:url').pathToFileURL(require('node:path').resolve('crates/sm-server/src/web/artifacts-model.js')));

test('web words each operation like the app', async () => {
  const m = await load();
  for (const c of fixture.operations) {
    assert.equal(m.opStatus(c.op), c.status, `status ${c.op.id}`);
    assert.equal(m.opStep(c.op), c.step, `step ${c.op.id}`);
    assert.equal(m.opName(c.op), c.name);
    assert.equal(m.opSizes(c.op), c.sizes);
    assert.equal(m.actorLabel(c.op), c.actor);
    assert.equal(m.actorId(c.op), c.actor_id);
    assert.equal(m.opSaved(c.op), c.saved);
  }
});
test('web words savings like the app', async () => {
  const m = await load();
  for (const c of fixture.savings) {
    assert.equal(m.savingsLine(c.savings), c.line);
    assert.equal(m.ratio(c.savings.data_bytes, c.savings.frozen_bytes), c.ratio);
  }
});
test('web words each folder\'s place and state like the app', async () => {
  const m = await load();
  for (const c of fixture.artifacts) {
    assert.equal(m.artifactSummary(c.artifact), c.summary);
    assert.equal(m.placementLabel(c.artifact), c.placement);
    assert.equal(m.thawTime(c.artifact), c.thaw);
  }
});
test('estimated progress stops short of done and needs an estimate', async () => {
  const m = await load();
  assert.equal(m.opProgress({ state: 'running', expected_seconds: 100, elapsed_seconds: 50 }), 0.5);
  assert.equal(m.opProgress({ state: 'running', expected_seconds: 100, elapsed_seconds: 500 }), 0.97);
  assert.equal(m.opProgress({ state: 'waiting', expected_seconds: 100, elapsed_seconds: null }), null);
  assert.equal(m.span(45), '45 s');
  assert.equal(m.span(4800), '1 h 20 min');
});
