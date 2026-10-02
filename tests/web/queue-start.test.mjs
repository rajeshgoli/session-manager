import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
const source=readFileSync('crates/sm-server/src/web/queue-start.js','utf8');
const {startNowController}=await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);
const check={job_id:'job',state:'pending',warnings:['Slots full','Free memory unknown'],memory_available_bytes:null,memory_reserve_bytes:1024,memory_estimate_bytes:null};
test('Start now reads warnings first and sends no override on failed check or cancel',async()=>{
 const calls=[];let fail=true;const c=startNowController(async(path,opts)=>{calls.push([path,opts]);if(fail)throw Error('offline');return check;},'job',()=>0);
 await assert.rejects(c.inspect(),/offline/);assert.equal(calls.length,1);
 fail=false;const {checked_at,...snapshot}=await c.inspect();assert.deepEqual(snapshot,check);assert.ok(Number.isFinite(Date.parse(checked_at)));c.cancel();
 const result=await c.confirm('pending');assert.equal(result.submitted,false);assert.ok(calls.every(([,opts])=>!opts));
});
test('expired check requires another confirmation and nonpending job never starts',async()=>{
 let now=0;const calls=[];const c=startNowController(async(path,opts)=>{calls.push([path,opts]);return check;},'job',()=>now);
 await c.inspect();now=30000;assert.equal((await c.confirm('pending')).submitted,false);
 assert.equal((await c.confirm('pending')).submitted,true);assert.equal(calls.filter(([,o])=>o?.method==='POST').length,1);
 await assert.rejects(c.confirm('running'),/no longer waiting/);
});
test('double confirmation cannot send two in-flight override requests',async()=>{
 let resolve;let posts=0;const c=startNowController(async(path,opts)=>{if(opts){posts++;await new Promise(r=>resolve=r);}return check;},'job',()=>0);
 await c.inspect();const first=c.confirm('pending');assert.equal((await c.confirm('pending')).submitted,false);resolve();await first;assert.equal(posts,1);
});
