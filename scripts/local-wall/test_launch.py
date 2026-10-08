#!/usr/bin/env python3
"""Prepare credential-free launch fixtures; invoked only by isolated Rust tests."""
import argparse
import json
import os
import shutil
from pathlib import Path
import subprocess
import sys

sys.dont_write_bytecode = True
import wall_profile as wall


def prepare(args):
    scripts = Path(__file__).resolve().parent
    home = args.root.resolve() / "h"
    state = home / "s/a"
    checkout = home / "w"
    executables = state / "xdg/config/executables"
    broker = state / "tmp/b"
    secret = home / "j"
    for path in [checkout, executables, broker, secret,
                 *[state / p for p in ("xdg/data", "xdg/cache", "xdg/state")]]:
        path.mkdir(parents=True, exist_ok=True)
        path.chmod(0o700)
    tmp = wall.temporary_directory(state, len(wall.user_temp()))
    python = Path(sys._base_executable).resolve()
    python_root = Path(os.path.commonpath([python, Path(sys.base_prefix).resolve()]))
    library = state / "xdg/config/adapter.dylib"
    opencode = executables / "opencode"
    attached_arguments = []
    if args.opencode_binary:
        shutil.copyfile(args.opencode_binary.resolve(strict=True), opencode)
        opencode.chmod(0o500)
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(opencode)], check=True)
        attached_arguments = ["--attached-spawn-executable", str(opencode)]
    shell = executables / "queue-zsh"
    if args.opencode_binary:
        shutil.copyfile("/bin/zsh", shell)
        shell.chmod(0o500)
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(shell)], check=True)
    subprocess.run([str(python), str(scripts / "build_adapter.py"),
                    "--endpoint", str(broker / "s"), "--peer-token", *map(str, args.peer_token),
                    "--direct-ports", str(args.gateway_port), str(args.egress_port), "24000", "24001",
                    "--control-port", str(args.control_port), "--control-fd", "198",
                    "--contained-spawns",
                    "--immutable-exec-dir", str(executables),
                    "--immutable-exec-dir", str(python_root), "--output", str(library),
                    *attached_arguments],
                   check=True, stdout=subprocess.DEVNULL)
    for source, name, extra in [("launch_supervisor.c", "supervisor", []),
                                ("test_adapter.c", "application", ["-pthread"]),
                                ("test_inherited_fds.c", "descriptor-probe", [])]:
        subprocess.run(["clang", "-std=c11", "-Wall", "-Wextra", "-Werror", *extra,
                        str(scripts / "native" / source), "-o", str(executables / name)], check=True)
    subprocess.run(["rustc", str(scripts / "native/test_exec.rs"),
                    "-o", str(executables / "rust-application")], check=True)
    profile_args = wall.parser().parse_args([
        "--home", str(home), "--checkout", str(checkout), "--state-root", str(home / "s"),
        "--state-dir", str(state), "--tmp-dir", tmp,
        "--agent-port", str(args.control_port), "--agent-port-range", f"{args.control_port}-{args.control_port}",
        "--gateway-port", str(args.gateway_port), "--gateway-port-range", f"{args.gateway_port}-{args.gateway_port}",
        "--egress-port", str(args.egress_port), "--egress-port-range", f"{args.egress_port}-{args.egress_port}",
        "--model-port", "24000", "--judge-port", "24001", "--service-state-dir", str(secret),
        "--broker-dir", str(broker), "--immutable-exec-dir", str(executables),
        "--contained-processes",
        "--immutable-exec-dir", str(python_root), "--read-only-dir", str(python_root),
    ])
    profile = state / "wall.sb"
    profile.write_text(wall.generate(profile_args, wall.listening_ports(), len(wall.user_temp())))
    text_command = Path(tmp) / "text-command"
    text_command.write_text("exit 37\n")
    text_command.chmod(0o700)
    environment = {
        "PATH": "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home),
        "TMPDIR": tmp, "TMUX_TMPDIR": tmp, "PYTHONDONTWRITEBYTECODE": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1",
        "SM_TEST_ISOLATION_ROOT": str(Path(tmp) / "isolation"),
        "SM_TEST_RUST_APPLICATION": str(executables / "rust-application"),
        "SM_TEST_PYTHON": str(python), "SM_TEST_MUTABLE": tmp,
        "SM_TEST_FD_PROBE": str(executables / "descriptor-probe"),
        "SM_TEST_CONTAINED": "1",
        "SM_TEST_SHELL": str(shell),
    }
    environment.update({f"XDG_{name.upper()}_HOME": str(state / "xdg" / name)
                        for name in ("config", "data", "cache", "state")})
    print(json.dumps({"profile": str(profile), "adapter": str(library),
                      "opencode": str(opencode),
                      "shell": str(shell),
                      "supervisor": str(executables / "supervisor"),
                      "application": str(executables / "application"),
                      "checkout": str(checkout), "python": str(python), "environment": environment}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--opencode-binary", type=Path)
    parser.add_argument("--control-port", type=int, required=True)
    parser.add_argument("--gateway-port", type=int, default=18600)
    parser.add_argument("--egress-port", type=int, default=18700)
    parser.add_argument("--peer-token", type=int, nargs=8, required=True)
    prepare(parser.parse_args())
