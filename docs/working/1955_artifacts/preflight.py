"""Run an actual opencode tool call against a deterministic local fixture."""
import json
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def check(run, work, env, binary):
    seen = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            seen.append(body)
            if not body.get('tools'):
                delta = {'content': 'Preflight'}
                finish = 'stop'
            elif any(m.get('role') == 'tool' for m in body['messages']):
                delta = {'content': 'Preflight complete'}
                finish = 'stop'
            else:
                delta = {'tool_calls': [{'index': 0, 'id': 'preflight', 'type': 'function',
                    'function': {'name': 'bash', 'arguments': json.dumps({
                        'command': 'pwd; git rev-parse HEAD; touch .1955-preflight && rm .1955-preflight',
                        'description': 'Verify historical checkout and permitted writes'})}}]}
                finish = 'tool_calls'
            events = [dict(id='preflight', object='chat.completion.chunk', created=0, model='flash',
                           choices=[dict(index=0, delta=delta, finish_reason=None)]),
                      dict(id='preflight', object='chat.completion.chunk', created=0, model='flash',
                           choices=[dict(index=0, delta={}, finish_reason=finish)],
                           usage=dict(prompt_tokens=1, completion_tokens=1, total_tokens=2))]
            response = ''.join('data: ' + json.dumps(e) + '\n\n' for e in events) + 'data: [DONE]\n\n'
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(response.encode())))
            self.end_headers()
            self.wfile.write(response.encode())

    fixture = ThreadingHTTPServer(('127.0.0.1', 1236), Handler)
    thread = threading.Thread(target=fixture.serve_forever, daemon=True)
    thread.start()
    try:
        result = subprocess.run(['sandbox-exec', '-f', str(run / 'profile.sb'), binary,
            '--pure', 'run', '--dir', str(work), '--title', '1955 preflight', '--format', 'json',
            '--model', 'mtplx/flash', 'Verify the checkout with the provided tool call, then stop.'],
            cwd=work, env=env, capture_output=True, text=True, timeout=30)
        (run / 'preflight.jsonl').write_text(result.stdout)
        (run / 'preflight.stderr').write_text(result.stderr)
        rows = [json.loads(line) for line in result.stdout.splitlines() if line.startswith('{')]
        tools = [r['part'] for r in rows if r.get('type') == 'tool_use']
        expected_head = subprocess.check_output(['git', '-C', str(work), 'rev-parse', 'HEAD'], text=True).strip()
        assert result.returncode == 0, result.stderr
        assert tools and tools[0]['state']['status'] == 'completed', tools
        assert tools[0]['state']['metadata']['exit'] == 0, tools
        assert str(work) in tools[0]['state']['output'], tools
        assert expected_head in tools[0]['state']['output'], tools
        actual = [b for b in seen if b.get('tools')]
        assert actual and f'Working directory: {work}\n' in actual[0]['messages'][0]['content']
        print(f'preflight PASS: {work} @ {expected_head[:12]} (actual opencode tool write)', flush=True)
    finally:
        fixture.shutdown()
        fixture.server_close()
