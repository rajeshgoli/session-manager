import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
const source=readFileSync('crates/sm-server/src/web/queue-start.js','utf8');
const {requestStart}=await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);
const riskError=(risk)=>Object.assign(new Error(risk),{status:409,body:{detail:risk,code:'start_risk',risk}});

test('one tap starts the job and asks the server to warn first',async()=>{
 const calls=[];const job={id:'job a',state:'running'};
 const result=await requestStart(async(path,opts)=>{calls.push([path,opts]);return job;},'job a');
 assert.deepEqual(result,{started:true,job});
 assert.deepEqual(calls,[['/client/queue/jobs/job%20a/start',{method:'POST',body:{ask_if_risky:true}}]]);
});

test('a memory risk comes back as the sentence to confirm, and confirming accepts it',async()=>{
 const bodies=[];const api=async(path,opts)=>{bodies.push(opts.body);if(opts.body.ask_if_risky)throw riskError('It needs about 30.0 GiB.');return {id:'job'};};
 assert.deepEqual(await requestStart(api,'job'),{started:false,risk:'It needs about 30.0 GiB.'});
 assert.equal((await requestStart(api,'job',{acceptRisk:true})).started,true);
 assert.deepEqual(bodies,[{ask_if_risky:true},{ask_if_risky:false}]);
});

test('other refusals reach the caller with the server reason',async()=>{
 const refused=Object.assign(new Error('It was cancelled.'),{status:409,body:{detail:'It was cancelled.'}});
 await assert.rejects(requestStart(async()=>{throw refused;},'job'),/It was cancelled/);
});

test('two taps on one job send one start',async()=>{
 let release;let posts=0;const api=async()=>{posts++;await new Promise(r=>release=r);return {id:'job'};};
 const first=requestStart(api,'job');
 assert.deepEqual(await requestStart(api,'job'),{started:false,busy:true});
 release();assert.equal((await first).started,true);assert.equal(posts,1);
 assert.equal((await requestStart(async()=>({id:'job'}),'job')).started,true);
});
