import test from 'node:test';
import assert from 'node:assert/strict';
import { artifactInventory, largestArtifacts, artifactBytes, storageSize, needsDecision, backupLabel } from '../../crates/sm-server/src/web/artifacts-model.js';

test('overview is bounded, largest first, excludes tiny folders and does not invent unknown sizes', () => {
  const rows = [{name:'unknown',state:'unrecorded'}, {name:'tiny',state:'live',disk_bytes:100000},
    {name:'large',state:'live',disk_bytes:2e9,note:'Market research'}, {name:'compressed',state:'frozen',disk_bytes:9e9,frozen_bytes:1e9},
    {name:'deleted',state:'deleted',disk_bytes:99e9}];
  assert.deepEqual(largestArtifacts(rows).map(a=>a.name),['large','compressed']);
  assert.equal(artifactInventory(rows).at(-1).name,'unknown');
  assert.equal(artifactBytes(rows[0]),null);
  assert.deepEqual(artifactInventory(rows,'market').map(a=>a.name),['large']);
  assert.equal(largestArtifacts(Array.from({length:10},(_,i)=>({name:`${i}`,state:'live',disk_bytes:(i+1)*1e9}))).length,5);
});
test('missing records and project notices are not owner decisions', () => {
  assert.equal(needsDecision({needs_owner:true,needs_you:{reasons:['unrecorded','project_added']}}),false);
  assert.equal(needsDecision({needs_you:{reasons:['would_delete']}}),true);
});
test('rounding preserves useful scale and unknown values', () => {
  assert.equal(storageSize(null),'Not measured');
  assert.equal(storageSize(1.734e12),'1.7 TB');
  assert.equal(storageSize(100000),'<1 MB');
  assert.equal(storageSize(25600427493),'26 GB');
});
test('backup status does not claim an unconfirmed copy is backed up', () => {
  assert.match(backupLabel({backup_due:true,rebuildable:false}),/^not backed up/);
  assert.equal(backupLabel({backup_due:false,backed_up_at:'today',rebuildable:false}),'backed up (not rebuildable)');
});

test('failed operations and vanished folders remain agent follow-up, not owner decisions', async () => {
  const {needsAgentFollowup} = await import('../../crates/sm-server/src/web/artifacts-model.js');
  const rows = ['operation_failed','consumer_unverifiable','vanished'].map((reason,i)=>({name:`issue-${i}`,state:reason==='vanished'?'deleted':'live',disk_bytes:2e9,needs_you:{reasons:[reason]}}));
  assert.ok(rows.every(needsAgentFollowup));
  assert.ok(rows.every(a=>!needsDecision(a)));
  assert.equal(artifactInventory(rows).length,3);
  assert.equal(largestArtifacts(rows).length,0);
  assert.equal(artifactInventory(rows,'issue-2').length,1);
});
