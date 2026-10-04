import base64
import importlib.util
import json
import os
from pathlib import Path
import socket
import shutil
import subprocess
import tempfile
import time
import urllib.request

spec = importlib.util.spec_from_file_location("wall", "/tmp/sm-1974/local-wall/wall_profile.py")
wall = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wall)
with tempfile.TemporaryDirectory(prefix="sm-oc-fd-", dir="/private/tmp") as root:
    home = Path(root) / "home"
    checkout = home / "wt"
    state_root = home / "state"
    state = state_root / "agent"
    service = home / "service"
    for directory in [checkout, service, *[state / p for p in ("xdg/config/opencode", "xdg/data", "xdg/cache", "xdg/state", "tmp")]]:
        directory.mkdir(parents=True, exist_ok=True)
    (state / "xdg/config/opencode/opencode.json").write_text(json.dumps({"autoupdate": False, "share": "disabled"}))
    private_tmp = wall.temporary_directory(state, len(wall.user_temp()))
    shim = state / "probe-listener-shim.dylib"
    shutil.copyfile("/tmp/sm-1974/probe-listener-shim.dylib", shim)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        port = listener.getsockname()[1]
        args = wall.parser().parse_args([
            "--home", str(home), "--checkout", str(checkout), "--state-root", str(state_root),
            "--state-dir", str(state), "--tmp-dir", private_tmp, "--agent-port", str(port),
            "--agent-port-range", f"{port}-{port}", "--gateway-port", "18600", "--egress-port", "18700",
            "--model-port", "8000", "--judge-port", "8441", "--service-state-dir", str(service),
        ])
        profile = state / "wall.sb"
        profile.write_text(wall.generate(args, set(), len(wall.user_temp())) + "(deny network-inbound (local ip))\n")
        env = {"HOME": str(home), "PATH": "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
               "SHELL": "/bin/sh", "LANG": "en_US.UTF-8", "TMPDIR": private_tmp, "TMUX_TMPDIR": private_tmp,
               "OPENCODE_DISABLE_AUTOUPDATE": "1", "OPENCODE_DISABLE_MODELS_FETCH": "1",
               "OPENCODE_DISABLE_LSP_DOWNLOAD": "1", "OPENCODE_SERVER_PASSWORD": "fixture-password"}
        for name in ("CONFIG", "DATA", "CACHE", "STATE"):
            env[f"XDG_{name}_HOME"] = str(state / "xdg" / name.lower())
        log = open(state / "serve.log", "w+")
        child = subprocess.Popen([
            "/usr/bin/sandbox-exec", "-f", str(profile), "/usr/bin/env",
            f"DYLD_INSERT_LIBRARIES={shim}",
            f"SM_LOOPBACK_LISTENER_FD={listener.fileno()}", "/opt/homebrew/bin/opencode",
            "serve", "--pure", "--hostname", "127.0.0.1", "--port", str(port),
        ], cwd=checkout, env=env, pass_fds=(listener.fileno(),), stdout=log, stderr=log)
        answer = None
        last_error = None
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        authorization = base64.b64encode(b"opencode:fixture-password").decode()
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and child.poll() is None:
                try:
                    request = urllib.request.Request(f"http://127.0.0.1:{port}/global/health",
                                                     headers={"Authorization": "Basic " + authorization})
                    with opener.open(request, timeout=0.5) as response:
                        answer = json.load(response)
                    break
                except Exception as error:
                    last_error = str(error)
                time.sleep(0.1)
        finally:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
            log.seek(0)
            diagnostic = log.read()[-3000:]
            log.close()
        print(json.dumps({"health": answer, "last_error": last_error, "log": diagnostic}))
        if not answer or not answer.get("healthy"):
            raise SystemExit(1)
