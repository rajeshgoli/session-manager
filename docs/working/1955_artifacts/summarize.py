"""Summarize measured requests; stream-first-event timings are not token latency."""
import json
from pathlib import Path
import statistics

root = Path('/private/tmp/sm-1955')
kit = Path('/Users/rajesh/.local/share/local-agents-proto/kit')
read = lambda p: [json.loads(line) for line in p.read_text().splitlines() if line.startswith('{')]
gens = [r for r in read(root / 'mtplx.log') if r.get('event') == 'mtplx_openai_generation']
memory = read(root / 'memory.jsonl')
old_requests = read(kit / 'runs/proxy.jsonl')
output = []
for ticket in [1913, 1855]:
    run = root / str(ticket)
    times = json.loads((run / 'result.json').read_text())
    reqs = read(run / 'requests.jsonl')
    turns = [r for r in reqs if r.get('n_tools', 0) > 0]
    events = read(run / 'agent.jsonl')
    tools = [r['part'] for r in events if r.get('type') == 'tool_use']
    prompt_sizes = {r['usage']['prompt_tokens'] for r in turns}
    generations = [r for r in gens if r['prompt_tokens'] in prompt_sizes]
    mem = [r for r in memory if times['start'] <= r['t'] <= times['end']]
    old = kit / 'runs' / f'flash-claude-{ticket}'
    t0, t1 = float((old / 't0').read_text()), float((old / 't_end').read_text())
    old_turns = [r for r in old_requests if r.get('kind') == 'model' and t0 <= r['t_start'] <= t1 and r.get('n_tools', 0) > 0]
    old_first = old_turns[0].get('usage', {})
    old_tokens = sum(old_first.get(k, 0) for k in ['input_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens'])
    new_total = sum(r['usage'].get('prompt_tokens', 0) for r in turns)
    cached = sum(r['usage'].get('prompt_tokens_details', {}).get('cached_tokens', 0) for r in turns)
    record = dict(ticket=ticket, agent_commit=json.loads((root / 'manifest.json').read_text())[1 if ticket == 1913 else 0]['start_commit'],
                  active_seconds=times['end']-times['start'], permission_wait_seconds=0,
                  agent_turns=len(turns), auxiliary_requests=len(reqs)-len(turns),
                  first_prompt_tokens=turns[0]['usage']['prompt_tokens'],
                  claude_first_prompt_tokens=old_tokens, claude_active_minutes=32 if ticket == 1913 else 12,
                  tool_calls=len(tools), tool_errors=sum(t['state']['status']=='error' for t in tools),
                  input_tokens=new_total, reused_tokens=cached, reuse_fraction=cached/new_total,
                  completion_tokens=sum(r['usage'].get('completion_tokens', 0) for r in turns),
                  peak_wired_gb=max(r['wired'] for r in mem)/1e9,
                  lowest_available_gb=min(r['available'] for r in mem)/1e9,
                  server_generation_records=len(generations),
                  server_decode_median_tok_s=statistics.median(r['tok_s'] for r in generations),
                  request_errors=sum(bool(r.get('status') != 200 or r.get('error') or r.get('client_error')) for r in reqs),
                  timing_note='Proxy first delta includes empty role/keepalive events; no first-content-token latency claimed.')
    import subprocess
    record['agent_commit'] = subprocess.check_output(['git','-C',f'/Users/rajesh/worktrees/sm-1955-opencode-{ticket}','rev-parse','HEAD'],text=True).strip()
    output.append(record)
(root / 'metrics.json').write_text(json.dumps(output, indent=2) + '\n')
print(json.dumps(output, indent=2))
