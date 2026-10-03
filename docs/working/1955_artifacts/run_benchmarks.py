"""#1955 sequential opencode benchmark; run through sm queue, never directly."""
import argparse
import http.client
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import threading
import time

ROOT = Path('/private/tmp/sm-1955')
KIT = Path('/Users/rajesh/.local/share/local-agents-proto/kit')
MTPLX = '/Users/rajesh/.local/share/mtplx-venv/bin/mtplx'
OPENCODE = '/opt/homebrew/bin/opencode'
abort = threading.Event()
finished = threading.Event()
agent = None
server = None
proxy = None
current_ticket = None


def emit(kind, **values):
    record = dict(t=time.time(), kind=kind, ticket=current_ticket, **values)
    with (ROOT / 'events.jsonl').open('a') as f:
        f.write(json.dumps(record) + '\n')
    print(kind, json.dumps(values), flush=True)


def memory():
    vm = subprocess.check_output(['vm_stat'], text=True)
    page_size = int(re.search(r'page size of (\d+)', vm).group(1))
    def pages(name):
        return int(re.search(rf'{name}:\s+(\d+)', vm).group(1)) * page_size
    pressure = subprocess.check_output(['/usr/bin/memory_pressure', '-Q'], text=True)
    total = int(re.search(r'The system has (\d+)', pressure).group(1))
    percent = int(re.search(r'memory free percentage:\s*(\d+)%', pressure).group(1))
    return dict(wired=pages('Pages wired down'), available=total * percent // 100,
                reclaimable=sum(pages(n) for n in ['Pages free', 'Pages inactive', 'Pages speculative', 'Pages purgeable']))


def request(path):
    conn = http.client.HTTPConnection('127.0.0.1', 8000, timeout=3)
    try:
        conn.request('GET', path)
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def terminate_agent():
    if agent is not None and agent.poll() is None:
        os.killpg(agent.pid, signal.SIGTERM)


def interrupt(signum, frame):
    abort.set()
    terminate_agent()


def sample():
    while not finished.is_set():
        try:
            values = memory()
            with (ROOT / 'memory.jsonl').open('a') as f:
                f.write(json.dumps(dict(t=time.time(), ticket=current_ticket, **values)) + '\n')
            if values['available'] < 32.6e9:
                emit('memory_abort', **values)
                abort.set()
                terminate_agent()
        except Exception as exc:
            emit('memory_sample_error', error=str(exc))
            abort.set()
            terminate_agent()
        finished.wait(5)


def setup(ticket):
    run = ROOT / str(ticket)
    runtime = run / 'runtime'
    runtime.mkdir(exist_ok=True)
    work = Path(f'/Users/rajesh/worktrees/sm-1955-opencode-{ticket}')
    env = {key: value for key, value in os.environ.items()
           if not any(word in key.upper() for word in ['TOKEN', 'SECRET', 'PASSWORD', 'API_KEY'])
           and key not in ['CLAUDE_SESSION_MANAGER_ID', 'SESSION_MANAGER_ID', 'OPENCODE_CONFIG', 'OPENCODE_CONFIG_CONTENT']}
    env.update(PWD=str(work), TMPDIR=str(runtime) + '/', XDG_CONFIG_HOME=str(runtime / 'config'),
               XDG_DATA_HOME=str(runtime / 'data'), XDG_CACHE_HOME=str(runtime / 'cache'),
               XDG_STATE_HOME=str(runtime / 'state'), OPENCODE_CONFIG=str(run / 'opencode.json'),
               CARGO_NET_OFFLINE='true', OPENCODE_DISABLE_AUTOUPDATE='true',
               OPENCODE_DISABLE_DEFAULT_PLUGINS='true', OPENCODE_DISABLE_SHARE='true',
               OPENCODE_DISABLE_CLAUDE_CODE='true', OPENCODE_DISABLE_EXTERNAL_SKILLS='true')
    for name in ['config', 'data', 'cache', 'state', 'bin']:
        (runtime / name).mkdir(exist_ok=True)
    shim = runtime / 'bin' / 'sm'
    shim.write_text('#!/bin/sh\necho "sm is not available in this run" >&2\nexit 1\n')
    shim.chmod(0o755)
    env['PATH'] = str(runtime / 'bin') + ':' + os.environ['PATH']
    permission = {'*': 'deny', **{name: 'allow' for name in
                  ['read', 'edit', 'bash', 'glob', 'grep', 'list', 'todowrite', 'todoread', 'external_directory']}}
    config = {
        '$schema': 'https://opencode.ai/config.json', 'model': 'mtplx/flash', 'small_model': 'mtplx/flash',
        'enabled_providers': ['mtplx'], 'autoupdate': False, 'share': 'disabled',
        'snapshot': False, 'mcp': {}, 'plugin': [], 'permission': permission,
        'lsp': False, 'formatter': False,
        'provider': {'mtplx': {'npm': '@ai-sdk/openai-compatible', 'name': 'Local Flash-Next',
            'options': {'baseURL': 'http://127.0.0.1:1236/v1', 'apiKey': 'local'},
            'models': {'flash': {'name': 'Qwen3.8-Flash-Next', 'limit': {'context': 200000, 'output': 16384}}}}},
        'agent': {'build': {'temperature': 0, 'topP': 1}},
    }
    (run / 'opencode.json').write_text(json.dumps(config, indent=2) + '\n')
    profile = subprocess.check_output(['zsh', str(KIT / 'proto_sandbox.sh'), f'flash-opencode-{ticket}'], env=env, text=True)
    expected = f'/Users/rajesh/worktrees/sm-1784-proto-flash-opencode-{ticket}'
    if expected not in profile:
        raise RuntimeError('prototype sandbox no longer has the expected checkout path')
    # Only the checkout path changes; all prototype network/credential rules stay intact.
    profile = profile.replace(expected, str(work))
    (run / 'profile.sb').write_text(profile)
    return run, work, env


def main():
    global server, proxy, agent, current_ticket
    parser = argparse.ArgumentParser()
    parser.add_argument('--setup-only', action='store_true')
    parser.add_argument('--tickets', default='1913,1855')
    args = parser.parse_args()
    tickets = [int(t) for t in args.tickets.split(',')]
    prepared = {ticket: setup(ticket) for ticket in tickets}
    if args.setup_only:
        for ticket, (run, work, env) in prepared.items():
            with (run / 'resolved-config.json').open('w') as log:
                subprocess.run(['sandbox-exec', '-f', str(run / 'profile.sb'), OPENCODE, '--pure',
                                'debug', 'config'], cwd=work, env=env, stdout=log, check=True)
        return
    from preflight import check
    for ticket, (run, work, env) in prepared.items():
        check(run, work, env, OPENCODE)
    existing = subprocess.run(['pgrep', '-f', 'mtplx-venv/'], capture_output=True, text=True)
    if existing.returncode != 1:
        raise RuntimeError(f'MTPLX is running or process inventory failed: {existing.stdout} {existing.stderr}')
    mem = memory()
    emit('precheck', **mem)
    if mem['reclaimable'] < 180e9 or mem['available'] < 32.6e9:
        raise RuntimeError('not enough memory to load the model safely')
    signal.signal(signal.SIGTERM, interrupt)
    signal.signal(signal.SIGINT, interrupt)
    try:
        with (ROOT / 'mtplx.log').open('w') as log:
            server = subprocess.Popen([MTPLX, 'serve', '--model', 'Youssofal/Qwen3.8-Flash-Next-MTPLX-Optimized-Speed',
                '--profile', 'turbo', '--host', '127.0.0.1', '--port', '8000', '--context-window', '200000',
                '--max-active-requests', '2', '--batching-preset', 'agent', '--yes'],
                env={**os.environ, 'MTPLX_SESSION_BANK_MAX_BYTES': '16G'}, stdout=log, stderr=subprocess.STDOUT)
        threading.Thread(target=sample, daemon=True).start()
        deadline = time.monotonic() + 600
        while time.monotonic() < deadline and not abort.is_set():
            if server.poll() is not None:
                raise RuntimeError('MTPLX exited before readiness')
            try:
                if request('/v1/models')[0] == 200:
                    break
            except OSError:
                pass
            abort.wait(2)
        else:
            raise RuntimeError('MTPLX did not become ready')
        emit('model_ready')
        for ticket in tickets:
            if abort.is_set():
                raise RuntimeError('benchmark aborted')
            current_ticket = ticket
            run, work, env = prepared[ticket]
            with (run / 'proxy.log').open('w') as log:
                proxy = subprocess.Popen(['python3', str(ROOT / 'guarded_proxy.py'),
                    str(run / 'requests.jsonl'), '1236', '8000', str(work)], stdout=log, stderr=subprocess.STDOUT)
            abort.wait(1)
            if proxy.poll() is not None:
                raise RuntimeError('request proxy could not start')
            brief = (run / 'baseline-brief.md').read_text()
            start = time.time()
            emit('agent_start', checkout=str(work))
            with (run / 'agent.jsonl').open('w') as stdout, (run / 'agent.stderr').open('w') as stderr:
                agent = subprocess.Popen(['sandbox-exec', '-f', str(run / 'profile.sb'), OPENCODE,
                    '--pure', 'run', '--dir', str(work), '--format', 'json', '--model', 'mtplx/flash', brief],
                    cwd=work, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
                try:
                    code = agent.wait(timeout=10800)
                except subprocess.TimeoutExpired:
                    emit('agent_cap')
                    terminate_agent()
                    try:
                        code = agent.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        os.killpg(agent.pid, signal.SIGKILL)
                        code = agent.wait()
            emit('agent_end', exit_code=code, elapsed=time.time()-start)
            (run / 'result.json').write_text(json.dumps(dict(ticket=ticket, start=start, end=time.time(), exit_code=code), indent=2))
            proxy.terminate()
            proxy.wait(timeout=10)
            proxy = None
            emit('agent_state', head=subprocess.check_output(['git', '-C', str(work), 'rev-parse', 'HEAD'], text=True).strip(),
                 status=subprocess.check_output(['git', '-C', str(work), 'status', '--short'], text=True))
            if code != 0:
                raise RuntimeError(f'opencode exited {code}; inspect run before retrying')
    finally:
        terminate_agent()
        if proxy is not None and proxy.poll() is None:
            proxy.terminate()
            proxy.wait(timeout=10)
        if server is not None:
            emit('model_stop_begin')
            # The CLI otherwise escalates after only 10 seconds. Its grace period
            # exceeds our caller timeout so it cannot reach its SIGKILL fallback.
            result = subprocess.run([MTPLX, 'stop', '--port', '8000', '--grace-seconds', '300'],
                                    capture_output=True, text=True, timeout=180)
            emit('model_stop_end', exit_code=result.returncode, output=result.stdout, error=result.stderr)
            try:
                server.wait(timeout=60)
            except subprocess.TimeoutExpired:
                emit('model_still_running', pid=server.pid)
                raise RuntimeError('graceful MTPLX stop did not finish; no kill attempted')
        finished.set()


if __name__ == '__main__':
    main()
