import test from 'node:test';
import assert from 'node:assert/strict';
import { artifactInventory, largestArtifacts, artifactBytes, storageSize, needsDecision, backupLabel, backupCard } from '../../crates/sm-server/src/web/artifacts-model.js';

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

test('the Backup card shows nights, the remote disk and its health, and copes with an older mini', () => {
  assert.equal(backupCard({enabled:false}), null);
  const base = {enabled:true, fetch_rate:18.75e6, apply_rate:100e6, probe_rate:25e6, effective_rate:18.75e6};
  const old = backupCard({...base, card:{nights:[{date:'2026-10-16',ok:false,error:'backup disk full: needs 5 bytes'},{date:'2026-10-17',ok:true,error:null}],
    last_night:{bytes:2e9,seconds:300,confirmed_at:'2026-10-17T09:35:00Z'}, weekly_check:{date:'2026-10-12',ok:true,files_checked:120,bad:0}, disk:null}});
  assert.equal(old.disk, null);
  assert.deepEqual(old.nights.map(n => n.mark), ['✗','✓']);
  assert.equal(old.nights[0].detail, '2026-10-16: backup disk full: needs 5 bytes');
  assert.match(old.lastNight, /^Last backup: 2\.0 GB in 5 min, confirmed /);
  assert.equal(old.check, 'Weekly check 2026-10-12: 120 files, all ok');
  assert.equal(old.rates, 'Download 150 Mbit/s · Disk 800 Mbit/s · Restore 200 Mbit/s');
  assert.equal(old.inForce, '150 Mbit/s');
  const disk = {total_bytes:4e12, free_bytes:1e12, holding_bytes:3e10, holding_oldest:'2026-09-17', health:'failing', pruned:null, pruned_on:'2026-10-01'};
  const card = backupCard({...base, card:{nights:[], last_night:null, weekly_check:{date:'2026-10-12',ok:false,files_checked:120,bad:2}, disk}});
  assert.equal(card.disk.line, '3.0 TB used · 1.0 TB free of 4.0 TB');
  assert.equal(card.disk.holding, 'Holding 30 GB · oldest 2026-09-17');
  assert.equal(card.disk.pruned, 'Holding shortened for space on 2026-10-01');
  assert.equal(card.disk.health, 'Failing');
  assert.ok(card.disk.failing);
  assert.equal(card.check, 'Weekly check 2026-10-12: 120 files, 2 bad');
  assert.equal(card.lastNight, 'No confirmed backup yet');
  assert.equal(backupCard({...base, card:{disk:{...disk, health:'not_reported', pruned_on:null}}}).disk.health, 'Not reported');
  assert.equal(backupCard({...base, card:{disk:{...disk, health:'not_reported', pruned_on:null}}}).disk.pruned, null);
});
