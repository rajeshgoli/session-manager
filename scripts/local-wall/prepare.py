#!/usr/bin/env python3
"""Host-only artifact preparation from a registration supplied by sm, never a job."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

sys.dont_write_bytecode = True
import wall_profile as wall


def independent_tree(root):
    """Reject pre-existing aliases before granting a mutable tree to an agent."""
    def traversal_error(error):
        raise error

    for directory, folders, files in os.walk(root, followlinks=False, onerror=traversal_error):
        for name in [*folders, *files]:
            path = Path(directory) / name
            if not path.is_symlink() and path.is_file() and path.stat().st_nlink != 1:
                raise ValueError(f"hard-link alias in agent tree: {path}")


def prepare(request):
    sources = Path(__file__).resolve().parent
    home = wall.physical(request["home"])
    state = wall.physical(request["state"])
    checkout = wall.physical(request["checkout"])
    config = state / "xdg/config"
    executables = config / "executables"
    if config.is_symlink() or wall.physical(config) != config:
        raise ValueError("agent configuration must be a physical directory")
    if executables.is_symlink():
        raise ValueError("staged executables must not be a symlink")
    for root in [checkout, state]:
        independent_tree(root)
    # The host holds the preparation lock and has finished every old launch.
    # Rebuild the entire admitted root: withdrawn tools and interrupted copies
    # must not survive into a new registration.
    if executables.exists():
        shutil.rmtree(executables)
    executables.mkdir(mode=0o700)
    # A linked worktree's common Git directory is shared mutable host state.
    # Providers must supply an independent checkout, with no shared objects.
    git = checkout / ".git"
    if git.is_symlink() or (git.exists() and not git.is_dir()) or any(
            (git / path).exists() for path in ("commondir", "objects/info/alternates")):
        raise ValueError("local agents require independent Git metadata inside the checkout")
    if git.exists():
        configuration = git / "config"
        if configuration.is_symlink() or (configuration.exists() and (
                not configuration.is_file() or configuration.stat().st_nlink != 1)):
            raise ValueError("checkout Git configuration must be an independent regular file")
        lock = git / "config.lock"
        if lock.exists() or lock.is_symlink():
            raise ValueError("checkout Git configuration is already locked")
        identity = request["git_identity"]
        for key, value in [("user.name", identity["name"]), ("user.email", identity["email"])]:
            subprocess.run(["/usr/bin/git", "-C", str(checkout), "config", "--local",
                            "--no-includes", key, value], check=True,
                           env={"PATH": "/usr/bin:/bin", "GIT_CONFIG_NOSYSTEM": "1",
                                "GIT_CONFIG_GLOBAL": "/dev/null"})
    names = {"supervisor"}
    staged = {}
    for tool in request["tools"]:
        name = tool["name"]
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*", name) or name in names:
            raise ValueError("invalid or duplicate staged tool name")
        names.add(name)
        source = wall.physical(tool["source"])
        if not source.is_file() or source.stat().st_nlink != 1:
            raise ValueError("staged tool must be an independent regular file")
        if source == checkout or wall.below(source, checkout) or wall.below(source, state):
            raise ValueError("staged tool source must be outside mutable agent trees")
        target = executables / name
        if target.is_symlink() or (target.exists() and target.stat().st_nlink != 1):
            raise ValueError("staged tool output must not have aliases")
        temporary = executables / ("." + name + ".new")
        if temporary.exists() or temporary.is_symlink():
            raise ValueError("unexpected staged tool temporary file")
        try:
            shutil.copyfile(source, temporary)
            temporary.chmod(0o500)
            subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(temporary)], check=True)
            temporary.replace(target)
        finally:
            temporary.unlink(missing_ok=True)
        staged[name] = str(target)
    supervisor = executables / ".supervisor.new"
    if supervisor.exists() or supervisor.is_symlink():
        raise ValueError("unexpected supervisor temporary file")
    try:
        subprocess.run(["clang", "-std=c11", "-Wall", "-Wextra", "-Werror",
                        str(sources / "native/launch_supervisor.c"), "-o", str(supervisor)], check=True)
        supervisor.chmod(0o500)
        supervisor.replace(executables / "supervisor")
    finally:
        supervisor.unlink(missing_ok=True)
    tmp = wall.temporary_directory(state, len(wall.user_temp()))
    gh = wall.prepare_github_config(home, state)
    cargo = state / "xdg/cache/cargo"
    cargo.mkdir(mode=0o700, exist_ok=True)
    if cargo.is_symlink():
        raise ValueError("private Cargo home must not be a symlink")
    ports = request["ports"]
    roots = [str(executables), *request["executable_roots"]]
    arguments = ["--home", str(home), "--checkout", str(checkout),
                 "--state-root", request["state_root"], "--state-dir", str(state), "--tmp-dir", tmp,
                 "--broker-dir", request["broker_dir"], "--broker-endpoint", request["endpoint"],
                 "--contained-processes"]
    for name in ("agent", "gateway", "egress", "model", "judge"):
        arguments.extend(["--" + name + "-port", str(ports[name])])
    for name in ("agent", "gateway", "egress"):
        arguments.extend(["--" + name + "-port-range", request["ranges"][name]])
    for flag, values in [("--service-state-dir", request["service_roots"]),
                         ("--read-only-dir", request["read_only_roots"]),
                         ("--immutable-exec-dir", roots)]:
        for value in values:
            arguments.extend([flag, value])
    profile = config / "wall.sb"
    temporary_profile = config / ".wall.sb.new"
    if temporary_profile.exists() or temporary_profile.is_symlink():
        if (temporary_profile.is_symlink() or not temporary_profile.is_file()
                or temporary_profile.stat().st_nlink != 1):
            raise ValueError("profile temporary file has aliases or an unexpected type")
        temporary_profile.unlink()
    try:
        generated = wall.generate(wall.parser().parse_args(arguments), wall.listening_ports(), len(wall.user_temp()))
        # Rust has checked the saved queue registration and host configuration.
        # Its original launch-time deny list must retain the pending job hash.
        if not request.get("preserve_profile", False):
            temporary_profile.write_text(generated)
            temporary_profile.chmod(0o400)
            temporary_profile.replace(profile)
    finally:
        temporary_profile.unlink(missing_ok=True)
    adapter = config / "adapter.dylib"
    command = [sys.executable, str(sources / "build_adapter.py"), "--endpoint", request["endpoint"],
               "--peer-token", *map(str, request["peer_token"]), "--direct-ports",
               *[str(ports[name]) for name in ("gateway", "egress", "model", "judge")],
               "--control-port", str(ports["agent"]), "--control-fd", "198", "--contained-spawns",
               "--output", str(adapter)]
    for root in roots:
        command.extend(["--immutable-exec-dir", root])
    subprocess.run(command, check=True, stdout=subprocess.DEVNULL)
    return {"profile": str(profile), "adapter": str(adapter), "supervisor": str(executables / "supervisor"),
            "tmp": tmp, "gh": str(gh), "cargo": str(cargo), "executables": str(executables), "tools": staged}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("request", type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(prepare(json.loads(args.request.read_text()))))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"local wall preparation: {error}", file=sys.stderr)
        sys.exit(1)
