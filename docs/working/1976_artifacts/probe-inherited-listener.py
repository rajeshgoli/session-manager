import json
from pathlib import Path
import socket
import subprocess
import tempfile

with tempfile.TemporaryDirectory(prefix="sm-fd-probe-") as root:
    path = Path(root) / "deny.sb"
    path.write_text("(version 1)\n(allow default)\n(deny network-inbound (local ip))\n")
    for listen_again in (False, True):
        with socket.socket() as server:
            server.bind(("127.0.0.1", 0))
            server.listen()
            fd = server.fileno()
            script = (f"import socket; s=socket.socket(fileno={fd}); s.settimeout(3); "
                      + ("s.listen(); " if listen_again else "")
                      + "c,_=s.accept(); c.sendall(b'fixture-only'); c.close()")
            child = subprocess.Popen(
                ["/usr/bin/sandbox-exec", "-f", str(path), "/opt/homebrew/bin/python3", "-c", script],
                pass_fds=(fd,), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            with socket.create_connection(server.getsockname(), timeout=3) as client:
                client.settimeout(4)
                try:
                    response = client.recv(128).decode()
                except OSError as error:
                    response = str(error)
            out, err = child.communicate(timeout=8)
            print(json.dumps({"listen_again": listen_again, "rc": child.returncode,
                              "response": response, "stdout": out, "stderr": err}))
            if listen_again:
                if child.returncode == 0 or "PermissionError" not in err:
                    raise SystemExit("re-listen was not denied")
            elif child.returncode != 0 or response != "fixture-only":
                raise SystemExit("inherited listener did not accept and reply")
