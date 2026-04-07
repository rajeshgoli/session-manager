"""Regression test for issue #528: Stop hook should not wait for EOF."""

from __future__ import annotations

import http.server
import json
import os
import socketserver
import subprocess
import threading
import time
from pathlib import Path


class _HookCaptureHandler(http.server.BaseHTTPRequestHandler):
    request_body: bytes | None = None
    request_path: str | None = None
    request_event = threading.Event()

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        type(self).request_body = self.rfile.read(length)
        type(self).request_path = self.path
        type(self).request_event.set()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"received"}')

    def log_message(self, format: str, *args) -> None:  # noqa: A003
        return


def test_notify_server_reads_first_json_line_without_waiting_for_eof(tmp_path: Path) -> None:
    """The Stop hook should exit promptly even if stdin stays open after one JSON line."""
    hook_path = Path(__file__).resolve().parents[2] / "hooks" / "notify_server.sh"
    assert hook_path.exists()

    log_path = tmp_path / "claude-hooks.log"

    _HookCaptureHandler.request_body = None
    _HookCaptureHandler.request_path = None
    _HookCaptureHandler.request_event.clear()

    with socketserver.TCPServer(("127.0.0.1", 0), _HookCaptureHandler) as server:
        port = server.server_address[1]
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()
        try:
            env = os.environ.copy()
            env.update(
                {
                    "CLAUDE_SESSION_MANAGER_ID": "testsession",
                    "SM_HOOK_URL": f"http://127.0.0.1:{port}/hooks/claude",
                    "CLAUDE_HOOK_LOG_PATH": str(log_path),
                }
            )
            proc = subprocess.Popen(
                ["/bin/bash", str(hook_path)],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=env,
            )

            assert proc.stdin is not None
            proc.stdin.write(
                '{"hook_event_name":"Stop","session_id":"native-session","transcript_path":"/tmp/demo.jsonl"}\n'
            )
            proc.stdin.flush()

            time.sleep(0.2)
            assert proc.poll() == 0

            proc.stdin.close()
            proc.stdin = None
            assert proc.wait(timeout=2) == 0
            stdout = proc.stdout.read() if proc.stdout is not None else ""
            stderr = proc.stderr.read() if proc.stderr is not None else ""
            assert stdout == ""
            assert stderr == ""

            assert _HookCaptureHandler.request_event.wait(2)
            assert _HookCaptureHandler.request_path == "/hooks/claude"
            payload = json.loads((_HookCaptureHandler.request_body or b"").decode("utf-8"))
            assert payload["hook_event_name"] == "Stop"
            assert payload["session_id"] == "native-session"
            assert payload["session_manager_id"] == "testsession"
            assert log_path.read_text(encoding="utf-8").count("Hook called") == 1
        finally:
            server.shutdown()
            server.server_close()
            server_thread.join(timeout=2)
