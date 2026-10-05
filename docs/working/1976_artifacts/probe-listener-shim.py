import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile

LIBRARY = "/tmp/sm-1974/probe-listener-shim.dylib"
subprocess.run(["/usr/bin/clang", "-dynamiclib", "-Wall", "-Wextra", "-Werror",
                "/tmp/sm-1974/probe-listener-shim.c", "-o", LIBRARY], check=True)
with tempfile.TemporaryDirectory(prefix="sm-listener-shim-") as root:
    path = Path(root) / "deny.sb"
    path.write_text("(version 1)\n(allow default)\n(deny network-inbound (local ip))\n")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        port = listener.getsockname()[1]
        env = dict(os.environ, DYLD_INSERT_LIBRARIES=LIBRARY, SM_LOOPBACK_LISTENER_FD=str(listener.fileno()))
        script = ("import socket, os, ctypes; lib=ctypes.CDLL('/usr/lib/libSystem.B.dylib'); "
                  "lib._dyld_get_image_name.restype=ctypes.c_char_p; "
                  "print('loader',os.environ.get('DYLD_INSERT_LIBRARIES'),os.environ.get('SM_LOOPBACK_LISTENER_FD'), "
                  "[lib._dyld_get_image_name(i).decode() for i in range(lib._dyld_image_count()) if b'shim' in lib._dyld_get_image_name(i)],flush=True); "
                  f"inherited=socket.socket(fileno={listener.fileno()}); print('inherited',inherited.getsockname(),flush=True); "
                  f"s=socket.socket(); s.bind(('127.0.0.1',{port})); s.listen(); "
                  "s.settimeout(4); c,_=s.accept(); c.sendall(b'fixture-shim'); c.close()")
        # SIP strips DYLD variables when executing sandbox-exec/env themselves;
        # set them via env's arguments immediately before the unsigned tool.
        child = subprocess.Popen(["/usr/bin/sandbox-exec", "-f", str(path), "/usr/bin/env",
                                  f"DYLD_INSERT_LIBRARIES={LIBRARY}",
                                  f"SM_LOOPBACK_LISTENER_FD={listener.fileno()}",
                                  "/opt/homebrew/bin/python3", "-c", script],
                                 pass_fds=(listener.fileno(),), env=env,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        with socket.create_connection(listener.getsockname(), timeout=4) as client:
            client.settimeout(5)
            try:
                response = client.recv(128).decode()
            except OSError as error:
                response = str(error)
        out, err = child.communicate(timeout=8)
        print(json.dumps({"rc": child.returncode, "response": response, "stdout": out, "stderr": err}))
        if child.returncode != 0 or response != "fixture-shim":
            raise SystemExit("listener adapter failed")
