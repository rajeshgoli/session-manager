#!/usr/bin/env python3
"""Explicit live-network acceptance test; requires macOS, gh auth and cargo.

Uses a network-only wall to test the proxy admission boundary independently of
#1974's provider/filesystem composition. Creates and deletes its own Git branch.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time


def run(command, env=None, profile=None, cwd=None):
    if profile:
        command = ["/usr/bin/sandbox-exec", "-p", profile, *command]
    result = subprocess.run(command, env=env, cwd=cwd, capture_output=True, text=True, timeout=120)
    if result.returncode:
        raise AssertionError(f"{command[0]} failed ({result.returncode}): {result.stderr[-3000:]}")
    return result.stdout


def control(directory, operation, agent):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(5)
        stream.connect(str(directory / "control.sock"))
        stream.sendall(json.dumps({operation: agent}).encode() + b"\n")
        chunks = []
        while chunk := stream.recv(4096):
            chunks.append(chunk)
        reply = json.loads(b"".join(chunks))
        assert not reply["error"], reply
        return reply["registration"]


def start(binary, directory):
    process = subprocess.Popen([binary, "--local-egress-service", str(directory)], stderr=subprocess.PIPE)
    for _ in range(100):
        try:
            control(directory, "Get", "agent-a")
            return process
        except (OSError, ValueError):
            if process.poll() is not None:
                raise AssertionError(process.stderr.read().decode())
            time.sleep(0.05)
    process.kill()
    raise AssertionError("proxy did not become ready")


def wall(port):
    return f'(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip "localhost:{port}"))'


def environment(port):
    env = os.environ.copy()
    for key in ("GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "ALL_PROXY", "all_proxy"):
        env.pop(key, None)
    proxy = f"http://127.0.0.1:{port}"
    for key in ("HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"):
        env[key] = proxy
    env.update(NO_PROXY="localhost,127.0.0.1,::1", no_proxy="localhost,127.0.0.1,::1",
               GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_COUNT="2",
               GIT_CONFIG_KEY_0="credential.helper", GIT_CONFIG_VALUE_0="",
               GIT_CONFIG_KEY_1="credential.helper", GIT_CONFIG_VALUE_1="!gh auth git-credential",
               CARGO_NET_OFFLINE="false")
    return env


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--repo", default="rajeshgoli/session-manager")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    with tempfile.TemporaryDirectory(prefix="sme-", dir="/private/tmp") as temporary:
        root = Path(temporary)
        directory = root / "service"
        directory.mkdir(mode=0o700)
        process = start(binary, directory)
        branch = f"sm-1978-egress-test-{os.getpid()}"
        pushed = False
        try:
            a = control(directory, "Register", "agent-a")
            b = control(directory, "Register", "agent-b")
            assert a["port"] != b["port"]
            env = environment(a["port"])
            profile = wall(a["port"])
            # Both standard web clients and git reach public HTTPS using CONNECT.
            run(["/usr/bin/curl", "--fail", "--silent", "--max-time", "30", "https://docs.rs"], env, profile)
            prs = json.loads(run([shutil.which("gh"), "pr", "list", "--repo", args.repo, "--state", "all", "--limit", "1", "--json", "number"], env, profile))
            run([shutil.which("gh"), "pr", "view", str(prs[0]["number"]), "--repo", args.repo, "--json", "number"], env, profile)
            git = root / "git"
            git.mkdir()
            run(["/usr/bin/git", "init", "-q"], env, profile, git)
            run(["/usr/bin/git", "remote", "add", "origin", f"https://github.com/{args.repo}.git"], env, profile, git)
            run(["/usr/bin/git", "fetch", "--depth=1", "origin", "main"], env, profile, git)
            run(["/usr/bin/git", "push", "origin", f"FETCH_HEAD:refs/heads/{branch}"], env, profile, git)
            pushed = True
            run(["/usr/bin/git", "push", "origin", "--delete", branch], env, profile, git)
            pushed = False
            crate = root / "crate"
            crate.mkdir()
            (crate / "src").mkdir()
            (crate / "src/lib.rs").write_text("")
            (crate / "Cargo.toml").write_text('[package]\nname="egress-check"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nitoa="1"\n')
            env["CARGO_HOME"] = str(root / "cargo")  # Empty cache: download is required.
            run([shutil.which("cargo"), "fetch"], env, profile, crate)
            # A cannot reach B, public IP directly, or DNS through the wall.
            probe = root / "probe.py"
            probe.write_text('import socket,sys\ntry:\n s=socket.create_connection((sys.argv[1],int(sys.argv[2])),timeout=2)\nexcept OSError:\n sys.exit(0)\ns.close()\nsys.exit(1)\n')
            python = shutil.which("python3")
            for host, port in [("127.0.0.1", b["port"]), ("1.1.1.1", 443)]:
                run([python, str(probe), host, str(port)], env, profile)
            dns = root / "dns.py"
            dns.write_text('import socket,sys\ns=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\ntry:\n s.sendto(b"dns",("1.1.1.1",53))\nexcept OSError:\n sys.exit(0)\nsys.exit(1)\n')
            run([python, str(dns)], env, profile)
            # Refusal reasons and attribution for literal/system-resolved names.
            for authority in ("127.0.0.1:8420", "localhost:443", "169.254.169.254:443", "github.com:22", "github.com:80"):
                with socket.create_connection(("127.0.0.1", a["port"]), timeout=5) as stream:
                    stream.sendall(f"CONNECT {authority} HTTP/1.1\r\n\r\n".encode())
                    assert stream.recv(4096).startswith(b"HTTP/1.1 403")
            # Crash recovery restores the exact ports and appends to existing log.
            process.kill()
            process.wait(timeout=5)
            old_log = (directory / "connections.jsonl").read_bytes()
            process = start(binary, directory)
            assert control(directory, "Get", "agent-a") == a
            assert control(directory, "Get", "agent-b") == b
            run(["/usr/bin/curl", "--fail", "--silent", "--max-time", "30", "https://docs.rs"], environment(b["port"]), wall(b["port"]))
            control(directory, "Unregister", "agent-b")  # Drains final logs.
            log = (directory / "connections.jsonl").read_bytes()
            assert log.startswith(old_log)
            records = [json.loads(line) for line in log.splitlines()]
            assert any(r["agent_id"] == "agent-a" and r["host"] == "github.com" and r["bytes_to_host"] > 0 for r in records)
            assert any(r["agent_id"] == "agent-b" and r["host"] == "docs.rs" and r["bytes_to_agent"] > 0 for r in records)
            assert any(r["host"] == "static.crates.io" and r["bytes_to_agent"] > 0 for r in records)
            print(f"PASS: web, gh, git fetch/push, uncached cargo, wall isolation, restart and attribution ({len(records)} records)")
        finally:
            if pushed:
                run(["/usr/bin/git", "push", f"https://github.com/{args.repo}.git", "--delete", branch], environment(a["port"]), cwd=git)
            process.terminate()
            process.wait(timeout=5)


if __name__ == "__main__":
    main()
