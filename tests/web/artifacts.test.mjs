import test from 'node:test';
import assert from 'node:assert/strict';
import { artifactSections, backupLabel } from '../../crates/sm-server/src/web/artifacts-model.js';

test('artifact fixture partitions once, sorts by stored bytes, and filters epics', () => {
  const rows = [
    {name:'attention',needs_owner:true,hot:true,state:'live',disk_bytes:10,epic:'one'},
    {name:'small',hot:true,state:'live',disk_bytes:20,epic:'one'},
    {name:'large',hot:true,state:'live',disk_bytes:30,epic:'two'},
    {name:'archive',hot:false,state:'frozen',frozen_bytes:5},
    {name:'cold',hot:false,state:'live',disk_bytes:40},
    {name:'deleted',state:'deleted',disk_bytes:100},
  ];
  assert.deepEqual(artifactSections(rows).map(g=>[g.title,g.rows.map(a=>a.name)]),[
    ['Needs you',['attention']],['Hot',['large','small']],['Cold, live',['cold']],['Frozen',['archive']],
  ]);
  assert.deepEqual(artifactSections(rows,'one').flatMap(g=>g.rows.map(a=>a.name)),['attention','small']);
});
test('backup status does not claim an unconfirmed copy is backed up', () => {
  assert.match(backupLabel({backup_due:true,rebuildable:false}),/^not backed up/);
  assert.equal(backupLabel({backup_due:false,backed_up_at:'today',rebuildable:false}),'backed up (not rebuildable)');
});
