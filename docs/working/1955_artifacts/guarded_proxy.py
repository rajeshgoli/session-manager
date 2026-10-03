"""Prototype request logger with a fail-closed checkout assertion."""
import json
import runpy
import sys
from http.server import ThreadingHTTPServer

expected = sys.argv[4]
prototype = runpy.run_path('/Users/rajesh/.local/share/local-agents-proto/kit/proto_proxy.py', run_name='prototype')
handler = prototype['H']
forward = handler._forward


def checked_forward(self, raw):
    body = json.loads(raw) if raw else {}
    if body.get('tools'):
        system = str(body.get('messages', [{}])[0].get('content', ''))
        if f'Working directory: {expected}\n' not in system:
            prototype['emit']({'kind': 'invalid_checkout', 'expected': expected})
            return self._reply(400, {'error': 'benchmark checkout mismatch; request was not sent to the model'})
    return forward(self, raw)


handler._forward = checked_forward
ThreadingHTTPServer(('127.0.0.1', int(sys.argv[2])), handler).serve_forever()
