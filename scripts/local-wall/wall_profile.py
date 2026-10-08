#!/usr/bin/env python3
"""Generate a production local-agent macOS sandbox; see README.md for the contract."""

import argparse
import json
from pathlib import Path
import re
import stat
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


def github_hosts(home):
    """Validate the owner's one explicit credential-file grant without reading it."""
    gh = home / ".config/gh"
    hosts = gh / "hosts.yml"
    for path in (home / ".config", gh, hosts):
        if path.is_symlink():
            raise ValueError("GitHub credential path must not contain symlinks")
    if hosts.exists():
        metadata = hosts.stat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise ValueError("GitHub credentials must be a regular file without hard-link aliases")
    return hosts


def prepare_github_config(home, state):
    """Host-only preparation; the agent wall must deny writes to xdg/config."""
    hosts = github_hosts(physical(home))
    state = physical(state)
    config = state / "xdg/config"
    if physical(config) != config or not config.is_dir():
        raise ValueError("GitHub configuration requires physical immutable agent config")
    directory = config / "gh"
    if directory.is_symlink():
        raise ValueError("private GitHub configuration must not be a symlink")
    directory.mkdir(mode=0o700, exist_ok=True)
    target = directory / "hosts.yml"
    if target.is_symlink():
        if target.readlink() != hosts:
            raise ValueError("private GitHub credential link has an unexpected target")
    elif target.exists():
        raise ValueError("private GitHub credentials must use the approved host file")
    else:
        target.symlink_to(hosts)
    settings = directory / "config.yml"
    if settings.is_symlink():
        raise ValueError("private GitHub settings must not be a symlink")
    try:
        with settings.open("x") as stream:
            stream.write("version: 1\n")
        settings.chmod(0o400)
    except FileExistsError:
        metadata = settings.stat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1 or settings.read_text() != "version: 1\n":
            raise ValueError("private GitHub settings must contain only the supported format version")
    return directory


def generate(args, listeners, minimum_tmp_length):
    if args.command_groups and not args.contained_processes:
        raise ValueError("command groups require host session containment")
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
    # Owner policy in #1978 admits this one GitHub credential file. Never
    # resolve an alias into some other credential grant.
    gh = home / ".config/gh"
    gh_hosts = github_hosts(home)
    if any(path == gh or below(path, gh) or below(gh, path)
           for path in (checkout, state_root)):
        raise ValueError("GitHub credential directory and mutable agent trees must not overlap")
    mutable = [state / name for name in ("xdg/data", "xdg/cache", "xdg/state", "tmp")]
    for path in mutable:
        if physical(path) != path or not path.is_dir():
            raise ValueError(f"mutable state must be a physical directory: {path}")
    tmp = physical(args.tmp_dir)
    if tmp != state / "tmp" and not below(tmp, state / "tmp"):
        raise ValueError("temporary directory must be inside agent state/tmp")
    if len(str(tmp) + "/") < minimum_tmp_length:
        raise ValueError("TMPDIR is shorter than the macOS per-user temporary path")

    # The broker socket lives below private tmp so Unix IPC remains private,
    # but neither the endpoint nor a containing directory may be replaced.
    # Executable roots carry the same protection across native exec/spawn.
    protected_writes = [physical(value) for value in args.immutable_exec_dir]
    for path in protected_writes:
        if not path.is_dir() or path == Path("/"):
            raise ValueError("immutable executable root must be a narrow directory")
        if path == checkout or below(path, checkout) or below(checkout, path):
            raise ValueError("immutable executables must not overlap the checkout")
        if any(path == p or below(path, p) or below(p, path) for p in mutable):
            raise ValueError("immutable executables must not overlap mutable state")
    if args.broker_dir:
        broker = physical(args.broker_dir)
        if not broker.is_dir() or not below(broker, state / "tmp"):
            raise ValueError("broker directory must be strictly inside private state/tmp")
        if tmp == broker or below(tmp, broker):
            raise ValueError("application TMPDIR must not be inside the broker directory")
        protected_writes.append(broker)
    if args.broker_endpoint:
        endpoint = Path(args.broker_endpoint)
        if (not args.broker_dir or not endpoint.is_absolute() or physical(endpoint.parent) != broker
                or endpoint.is_symlink() or not endpoint.is_socket()
                or len(bytes(endpoint)) >= 104):
            raise ValueError("broker endpoint must resolve inside the protected broker directory")

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

    read_only = [physical(value) for value in args.read_only_dir]
    for path in read_only:
        if not path.is_dir():
            raise ValueError(f"read-only tool/log path is not a directory: {path}")
        # Never turn a broad host tree, agent root or secret tree into an
        # exception. The caller must supply credential-free immutable tooling
        # or a dedicated log directory, not arbitrary user data or caches.
        protected = [home, state_root, *services]
        if any(path == p or below(p, path) for p in protected):
            raise ValueError("read-only directory contains a protected host tree")
        if any(below(path, p) for p in [state_root, *services]):
            raise ValueError("read-only directory overlaps agent or service state")
    lines = ["(version 1)", "(allow default)", "(deny file-write*)",
             "(deny file-read-data)",
             # dyld opens the filesystem root before resolving its OS cache.
             # This permits only the root directory, never its descendants.
             '(allow file-read-data (literal "/"))']
    # Metadata is needed to resolve physical paths; file contents and directory
    # listings are denied everywhere unless explicitly admitted here. Do not
    # admit all of /Library, /etc, /usr/local, /opt or the user's home.
    runtime = [Path(p) for p in (
        "/System", "/bin", "/sbin", "/usr/bin", "/usr/sbin", "/usr/lib",
        "/usr/libexec", "/usr/share", "/dev", "/Library/Apple",
        "/Library/Developer/CommandLineTools", "/Applications/Xcode.app/Contents/Developer",
        "/opt/homebrew/Cellar", "/opt/homebrew/bin", "/opt/homebrew/lib",
        "/opt/homebrew/share", "/opt/homebrew/opt", "/usr/local/bin",
        "/usr/local/lib", "/usr/local/share",
        "/private/var/db/timezone", "/private/etc/ssl/cert.pem",
        "/private/etc/passwd", "/private/etc/group", "/private/etc/hosts",
        "/private/etc/localtime", "/private/etc/services", "/private/etc/protocols",
    )]
    lines.append("(allow file-read-data " + " ".join(
        f"(subpath {quoted(p)})" for p in [checkout, state, *runtime, *read_only]) + ")")
    lines.append(f"(allow file-read-data (literal {quoted(gh_hosts)}))")
    # Do not allow the whole Darwin temp directory or the entire state folder.
    # Profile, plugin, launch scripts, password and usage ledger are host-owned.
    writable = [checkout, *mutable, Path("/dev")]
    lines.append("(allow file-write* " + " ".join(
        f"(subpath {quoted(p)})" for p in writable) + ")")
    if protected_writes:
        ancestors = sorted({parent for path in protected_writes
                            for parent in path.parents if parent != Path("/")})
        lines.append("(deny file-write* " + " ".join(
            [f"(subpath {quoted(p)})" for p in protected_writes]
            + [f"(literal {quoted(p)})" for p in ancestors]) + ")")
    lines.extend(["(deny signal)", "(allow signal (target same-sandbox))"])
    # Keep every descendant in the host's private launch session. A raw spawn
    # syscall can set a new session internally, so it is denied too; the
    # contained adapter implements supported spawns with fork and exec.
    if args.contained_processes:
        blocked = "SYS_setsid SYS_posix_spawn" if args.command_groups else "SYS_setsid SYS_setpgid SYS_posix_spawn"
        lines.append(f"(deny syscall-unix (syscall-number {blocked}))")
    # Kernel process-argument queries can expose another process's initial
    # environment without reading its credential files. Admit only runtime
    # hardware/OS facts, never process argument/environment or mutation queries.
    lines.extend(["(deny sysctl-read)", "(deny sysctl-write)"])
    # KERN_PROCARGS2 also uses a process-info check; a sysctl denial alone
    # does not block that legacy numeric query on this Mac.
    lines.extend(["(deny process-info*)",
                  "(allow process-info* (target same-sandbox))"])
    system_queries = (
        "hw.activecpu", "hw.byteorder", "hw.cacheconfig", "hw.cachelinesize_compat",
        "hw.cpufamily", "hw.cpufrequency_compat", "hw.cputype", "hw.cpusubtype",
        "hw.l1dcachesize_compat", "hw.l1icachesize_compat", "hw.l2cachesize_compat",
        "hw.l3cachesize_compat", "hw.logicalcpu_max", "hw.machine", "hw.model",
        "hw.memsize", "hw.ncpu", "hw.nperflevels", "hw.packages", "hw.pagesize_compat",
        "hw.pagesize", "hw.physicalcpu", "hw.physicalcpu_max", "hw.logicalcpu",
        "hw.cpufrequency", "hw.tbfrequency_compat", "hw.vectorunit", "machdep.cpu.brand_string",
        "kern.argmax", "kern.hostname", "kern.maxfilesperproc", "kern.maxproc",
        "kern.osproductversion", "kern.osrelease", "kern.ostype", "kern.osvariant_status",
        "kern.osversion", "kern.secure_kernel", "kern.sysv.semmns", "kern.usrstack64",
        "kern.version", "vm.loadavg", "sysctl.name2oid", "sysctl.oidfmt", "sysctl.name",
    )
    lines.append("(allow sysctl-read " + " ".join(
        f'(sysctl-name "{name}")' for name in system_queries) +
        ' (sysctl-name-prefix "hw.optional.arm.")'
        ' (sysctl-name-prefix "hw.optional.armv8_")'
        ' (sysctl-name-prefix "sysctl.oidfmt.")'
        ' (sysctl-name-prefix "sysctl.name.")'
        ' (sysctl-name-prefix "hw.perflevel"))')
    denied_reads = [(home / p).resolve() for p in (
        ".ssh", ".claude", ".codex", ".config/session-manager", ".config/git", ".gitconfig",
        "Library/Keychains", ".aws", ".claude.json", ".netrc", ".git-credentials",
        ".cargo/credentials", ".cargo/credentials.toml", ".cargo/config", ".cargo/config.toml",
    )] + services
    lines.append("(deny file-read* " + " ".join(
        f"(subpath {quoted(p)})" for p in denied_reads) + ")")
    lines.append(f"(deny file-read-data (require-all (subpath {quoted(gh)}) "
                 f"(require-not (literal {quoted(gh_hosts)}))))")
    # Host services run outside this process's wall. Do not let tools delegate
    # keychain access, application launching or other privileged operations to
    # them. IPC needed by local tools uses private Unix sockets or admitted TCP.
    lines.extend(["(deny mach-lookup)", "(deny mach-register)", "(deny appleevent-send)"])
    # Negative filter protects siblings created AFTER this snapshot. An allow
    # for our directory alone would not override a denial of the entire root.
    # realpath() needs metadata on the root to resolve our own state. Listing
    # the root still reveals siblings, so deny its data reads separately.
    lines.append(f"(deny file-read* (require-all (subpath {quoted(state_root)}) "
                 f"(require-not (subpath {quoted(state)})) "
                 f"(require-not (literal {quoted(state_root)}))))")
    lines.append(f"(deny file-read-data (literal {quoted(state_root)}))")
    lines.extend([
        # Seatbelt's localhost inbound filter also matches wildcard/LAN binds.
        # New IP listeners must be allocated outside the wall and passed in.
        "(deny network-inbound (local ip))",
        "(deny network-outbound)",
        '(allow network-outbound ' + ' '.join(
            f'(remote ip "localhost:{number}")' for number in sorted(admitted)) + ' '
        f'(remote unix-socket (subpath {quoted(state / "tmp")})))',
    ])
    if args.broker_endpoint:
        lines.append(f"(allow network-outbound (remote unix-socket (literal {quoted(args.broker_endpoint)})))")
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
    result.add_argument("--read-only-dir", action="append", default=[],
                        help="host-approved credential-free toolchain or dedicated log directory")
    result.add_argument("--broker-dir", help="host-owned endpoint directory inside private state/tmp")
    result.add_argument("--broker-endpoint", help="exact host-owned short endpoint alias resolving inside broker-dir")
    result.add_argument("--command-groups", action="store_true",
                        help="allow command groups inside the host-owned private session")
    result.add_argument("--contained-processes", action="store_true",
                        help="require fork/exec adapter spawns; prevent descendants leaving the host session")
    result.add_argument("--immutable-exec-dir", action="append", default=[],
                        help="host-staged executable root; protect contents and all ancestors")
    return result


def main():
    try:
        if len(sys.argv) == 3 and sys.argv[1] == "--prepare-tmp":
            state = physical(sys.argv[2])
            print(temporary_directory(state, len(user_temp())))
        elif len(sys.argv) == 4 and sys.argv[1] == "--prepare-gh":
            print(prepare_github_config(sys.argv[2], sys.argv[3]))
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
