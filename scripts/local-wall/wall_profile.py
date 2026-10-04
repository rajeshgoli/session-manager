#!/usr/bin/env python3
"""Generate a production local-agent macOS sandbox; see README.md for the contract."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys


def port(value):
    number = int(value)
    if not 1 <= number <= 65535:
        raise ValueError(f"invalid TCP port: {value}")
    return number


def port_range(value):
    start, end = map(port, value.split("-"))
    if end < start:
        raise ValueError(f"reversed port range: {value}")
    return set(range(start, end + 1))


def physical(value):
    path = Path(value).expanduser()
    if not path.is_absolute():
        raise ValueError(f"path must be absolute: {value}")
    return path.resolve(strict=True)


def below(path, parent):
    return path != parent and parent in path.parents


def quoted(path):
    # Seatbelt uses quoted strings with C-style escaping. Control characters
    # have no valid purpose in launch paths; reject instead of relying on escapes.
    value = str(path)
    if any(ord(c) < 32 or ord(c) == 127 for c in value):
        raise ValueError("sandbox paths cannot contain control characters")
    return json.dumps(value, ensure_ascii=False)


def listening_ports():
    result = subprocess.run(
        ["/usr/sbin/lsof", "-nP", "-iTCP", "-sTCP:LISTEN", "-F", "n"],
        capture_output=True, text=True, timeout=10, check=False,
    )
    # lsof exits 1 with no output when no matching sockets exist. Any diagnostic
    # or other failure means we cannot build a trustworthy snapshot.
    if result.stderr or result.returncode not in (0, 1):
        raise ValueError("could not collect listening TCP ports")
    if result.returncode == 1 and result.stdout:
        raise ValueError("incomplete listening TCP port snapshot")
    ports = set()
    for line in result.stdout.splitlines():
        if line.startswith("n"):
            match = re.search(r":([0-9]+)$", line)
            if not match:
                raise ValueError("unrecognised lsof TCP listener")
            ports.add(port(match[1]))
    return ports


def user_temp():
    result = subprocess.run(
        ["/usr/bin/getconf", "DARWIN_USER_TEMP_DIR"],
        capture_output=True, text=True, timeout=10, check=True,
    )
    value = result.stdout.strip()
    if not value.startswith("/"):
        raise ValueError("could not determine the macOS per-user temporary path")
    # Keep its /var spelling: TMPDIR's socket-path length is measured as passed
    # to a tool, rather than after resolving /var to /private/var.
    return value


def temporary_directory(state_dir, minimum_length):
    """A private directory that preserves normal macOS socket-path failures."""
    root = state_dir / "tmp"
    root.mkdir(exist_ok=True)
    if root.is_symlink():
        raise ValueError("agent tmp must not be a symlink")
    # One extra character for TMPDIR's trailing slash. Avoid gratuitous padding
    # for state folders whose paths already exceed the macOS temporary path.
    padding = max(0, minimum_length - len(str(root)) - 2)
    path = root / ("t" + "x" * padding)
    if path.exists() and (path.is_symlink() or not path.is_dir()):
        raise ValueError("private temporary path is not a physical directory")
    path.mkdir(exist_ok=True)
    return str(path) + "/"


def generate(args, listeners, minimum_tmp_length):
    checkout = physical(args.checkout)
    state_root = physical(args.state_root)
    state = physical(args.state_dir)
    home = physical(args.home)
    for path in (checkout, state_root, state, home):
        if not path.is_dir():
            raise ValueError(f"not a directory: {path}")
        quoted(path)
    if state.parent != state_root:
        raise ValueError("agent state must be a direct child of state_root")
    if checkout == state_root or below(state_root, checkout) or below(checkout, state_root):
        raise ValueError("checkout and state_root must not overlap")
    if checkout == home or not below(checkout, home):
        raise ValueError("checkout must be a directory inside home")
    cargo = home / ".cargo"
    if cargo.is_symlink():
        raise ValueError("cargo directory must not be a symlink")
    if state_root == cargo or below(state_root, cargo) or below(cargo, state_root):
        raise ValueError("cargo and state_root must not overlap")
    if checkout == cargo or below(checkout, cargo) or below(cargo, checkout):
        raise ValueError("checkout and host cargo must not overlap")
    mutable = [state / name for name in ("xdg/data", "xdg/cache", "xdg/state", "tmp")]
    for path in mutable:
        if physical(path) != path or not path.is_dir():
            raise ValueError(f"mutable state must be a physical directory: {path}")
    tmp = physical(args.tmp_dir)
    if tmp != state / "tmp" and not below(tmp, state / "tmp"):
        raise ValueError("temporary directory must be inside agent state/tmp")
    if len(str(tmp) + "/") < minimum_tmp_length:
        raise ValueError("TMPDIR is shorter than the macOS per-user temporary path")

    agent_ports = port_range(args.agent_port_range)
    gateways = port_range(args.gateway_port_range)
    egress = port_range(args.egress_port_range)
    if agent_ports & gateways or agent_ports & egress or gateways & egress:
        raise ValueError("agent, gateway and egress port ranges must not overlap")
    own = port(args.agent_port)
    gateway = port(args.gateway_port)
    proxy = port(args.egress_port)
    if own not in agent_ports or gateway not in gateways or proxy not in egress:
        raise ValueError("agent/gateway/egress ports must be inside their reserved ranges")
    admitted = {gateway, proxy, port(args.model_port), port(args.judge_port)}
    if len(admitted) != 4:
        raise ValueError("gateway, egress, model and judge ports must be distinct")
    if admitted & (agent_ports | {8420, 8443}):
        raise ValueError("an admitted service overlaps an agent server or sm port")
    if {port(args.model_port), port(args.judge_port)} & (gateways | egress):
        raise ValueError("model/judge ports overlap reserved gateway/egress ports")
    forbidden = (set(listeners) | agent_ports | gateways | egress
                 | {8420, 8443, 8000, 1234, 1235, 1236}) - admitted
    services = [physical(value) for value in args.service_state_dir]
    for path in services:
        if not path.is_dir():
            raise ValueError(f"service state is not a directory: {path}")
        if path == state or below(state, path) or below(path, state):
            raise ValueError("service secrets and agent state must not overlap")
        if path == checkout or below(path, checkout) or below(checkout, path):
            raise ValueError("service secrets and checkout must not overlap")
        if path == cargo or below(path, cargo) or below(cargo, path):
            raise ValueError("service secrets and cargo must not overlap")

    lines = ["(version 1)", "(allow default)", "(deny file-write*)"]
    # Do not allow the whole Darwin temp directory or the entire state folder.
    # Profile, plugin, launch scripts, password and usage ledger are host-owned.
    writable = [checkout, *mutable, Path("/dev")]
    lines.append("(allow file-write* " + " ".join(
        f"(subpath {quoted(p)})" for p in writable) + ")")
    denied_reads = [(home / p).resolve() for p in (
        ".ssh", ".claude", ".codex", ".config/session-manager", ".config/gh", ".config/git", ".gitconfig",
        "Library/Keychains", ".aws", ".claude.json", ".netrc", ".git-credentials",
        ".cargo/credentials", ".cargo/credentials.toml", ".cargo/config", ".cargo/config.toml",
    )] + services
    lines.append("(deny file-read* " + " ".join(
        f"(subpath {quoted(p)})" for p in denied_reads) + ")")
    # File restrictions do not stop Security.framework from asking securityd
    # to read an unlocked keychain on its behalf. Block both legacy and modern
    # credential-service endpoints, including their per-function variants.
    lines.append('(deny mach-lookup (global-name-regex '
                 '#"^com[.]apple[.](SecurityServer|securityd)([.].*)?$"))')
    # Negative filter protects siblings created AFTER this snapshot. An allow
    # for our directory alone would not override a denial of the entire root.
    # realpath() needs metadata on the root to resolve our own state. Listing
    # the root still reveals siblings, so deny its data reads separately.
    lines.append(f"(deny file-read* (require-all (subpath {quoted(state_root)}) "
                 f"(require-not (subpath {quoted(state)})) "
                 f"(require-not (literal {quoted(state_root)}))))")
    lines.append(f"(deny file-read-data (literal {quoted(state_root)}))")
    lines.extend([
        "(deny network-outbound)",
        f'(allow network-outbound (remote ip "localhost:*") '
        f'(remote unix-socket (subpath {quoted(state / "tmp")})))',
    ])
    for number in sorted(forbidden):
        lines.append(f'(deny network-outbound (remote ip "localhost:{number}"))')
    return "\n".join(lines) + "\n"


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    for name in ("checkout", "state-root", "state-dir", "tmp-dir", "agent-port",
                 "gateway-port", "egress-port", "model-port", "judge-port"):
        result.add_argument("--" + name, required=True)
    result.add_argument("--home", default=str(Path.home()))
    result.add_argument("--agent-port-range", default="18500-18599")
    result.add_argument("--gateway-port-range", default="18600-18699")
    result.add_argument("--egress-port-range", default="18700-18799")
    result.add_argument("--service-state-dir", action="append", required=True,
                        help="host-owned judge/proxy secret directory; repeat for each")
    return result


def main():
    try:
        if len(sys.argv) == 3 and sys.argv[1] == "--prepare-tmp":
            state = physical(sys.argv[2])
            print(temporary_directory(state, len(user_temp())))
        else:
            args = parser().parse_args()
            # Build and validate EVERYTHING before emitting any profile bytes.
            print(generate(args, listening_ports(), len(user_temp())), end="")
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"local wall: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
