import json
from pathlib import Path
import subprocess
import tempfile

PROFILES = {
    "deny": "(version 1)\n(allow default)\n(deny network-inbound)\n",
    "localhost": '(version 1)\n(allow default)\n(deny network-inbound)\n(allow network-inbound (local ip "localhost:*"))\n',
    "literal": '(version 1)\n(allow default)\n(deny network-inbound)\n(allow network-inbound (local ip "127.0.0.1:*"))\n',
}
with tempfile.TemporaryDirectory(prefix="sm-inbound-") as root:
    for name, profile in PROFILES.items():
        path = Path(root) / (name + ".sb")
        path.write_text(profile)
        for address in ("127.0.0.1", "0.0.0.0"):
            result = subprocess.run(
                ["/usr/bin/sandbox-exec", "-f", str(path), "/opt/homebrew/bin/python3", "-c",
                 f"import socket; s=socket.socket(); s.bind(({address!r},0)); s.listen(); print(s.getsockname())"],
                capture_output=True, text=True, timeout=10,
            )
            print(json.dumps({"profile": name, "bind": address, "rc": result.returncode,
                              "stdout": result.stdout.strip(), "stderr": result.stderr.strip()}))
            expected = 0 if name == "localhost" else (65 if name == "literal" else 1)
            if result.returncode != expected:
                raise SystemExit("unexpected inbound-filter result")
