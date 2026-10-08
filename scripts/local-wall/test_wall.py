#!/usr/bin/env python3
"""Fixture-only profile and actual macOS sandbox checks; no live credentials."""

import importlib.util
import os
from pathlib import Path
import socket
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
# Framework launchers spawn the runtime image and are therefore unsuitable for
# tests of a wall that deliberately denies raw posix_spawn.
_python_runtime = Path(sys.base_prefix) / "Resources/Python.app/Contents/MacOS/Python"
PYTHON = str((_python_runtime if _python_runtime.is_file() else Path(sys._base_executable)).resolve())
MODULE = importlib.util.spec_from_file_location("wall_profile", Path(__file__).with_name("wall_profile.py"))
wall = importlib.util.module_from_spec(MODULE)
MODULE.loader.exec_module(wall)


class WallTests(unittest.TestCase):
    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix="smw-", dir="/private/tmp" if sys.platform == "darwin" else None)
        self.addCleanup(self.fixture.cleanup)
        self.home = Path(self.fixture.name).resolve() / "home"
        self.checkout = self.home / "worktrees/agent"
        self.root = self.home / ".local/share/claude-sessions/opencode"
        self.state = self.root / "agent-a"
        self.service = self.home / ".local/share/claude-sessions/local-egress"
        self.logs = self.root.parent / "logs"
        for path in [self.checkout, self.service, self.home / ".cargo/bin", self.state / "xdg/config",
                     self.logs,
                     *[self.state / p for p in ("xdg/data", "xdg/cache", "xdg/state", "tmp")]]:
            path.mkdir(parents=True, exist_ok=True)
        self.tmp = wall.temporary_directory(self.state, 65)
        self.args = wall.parser().parse_args([
            "--home", str(self.home), "--checkout", str(self.checkout),
            "--state-root", str(self.root), "--state-dir", str(self.state),
            "--tmp-dir", self.tmp, "--agent-port", "18500", "--gateway-port", "18600",
            "--egress-port", "18700", "--model-port", "8000", "--judge-port", "8441",
            "--service-state-dir", str(self.service),
            "--read-only-dir", str(self.logs),
            "--read-only-dir", str(self.home / ".cargo/bin"),
        ])

    def test_profile_reserves_future_ports_and_exempts_admitted_services(self):
        profile = wall.generate(self.args, {8000, 8441, 18600, 18700, 54321}, 65)
        for number in (18500, 18599, 18601, 18699, 18701, 18799, 8420, 8443, 1234, 54321):
            self.assertIn(f'(deny network-outbound (remote ip "localhost:{number}"))', profile)
        for number in (8000, 8441, 18600, 18700):
            self.assertNotIn(f'(deny network-outbound (remote ip "localhost:{number}"))', profile)
        self.assertNotIn('(remote ip "localhost:*")', profile)

    def test_profile_validation(self):
        invalid = [("model_port", "8420"), ("judge_port", "18550"),
                   ("agent_port", "18499"),
                   ("agent_port_range", "18599-18500"), ("model_port", "65536"),
                   ("gateway_port_range", "18500-18699"), ("tmp_dir", str(self.checkout)),
                   ("state_dir", str(self.checkout)), ("checkout", str(self.home)),
                   ("checkout", str(self.home / ".cargo")),
                   ("judge_port", "8000")]
        for name, value in invalid:
            old = getattr(self.args, name)
            with self.subTest(name=name, value=value):
                setattr(self.args, name, value)
                with self.assertRaises(ValueError):
                    wall.generate(self.args, set(), 65)
            setattr(self.args, name, old)

    def test_normal_length_private_tmp(self):
        path = wall.temporary_directory(self.state, len(str(self.state)) + 100)
        self.assertGreaterEqual(len(path), len(str(self.state)) + 100)
        self.assertTrue(Path(path).is_dir())
        self.assertTrue(wall.below(Path(path), self.state / "tmp"))
        with self.assertRaises(ValueError):
            wall.generate(self.args, set(), len(self.tmp) + 1)

    @unittest.skipUnless(sys.platform == "darwin", "requires macOS sandbox")
    def test_broker_and_executable_roots_cannot_be_replaced(self):
        broker = self.state / "tmp/broker"
        executable_root = self.state / "xdg/config/executables"
        broker.mkdir()
        executable_root.mkdir()
        endpoint = broker / "endpoint"
        endpoint.write_text("fixture")
        application = executable_root / "application"
        application.write_text("fixture")
        self.args.broker_dir = str(broker)
        self.args.immutable_exec_dir = [str(executable_root)]
        self.profile = self.state / "protected-wall.sb"
        self.profile.write_text(wall.generate(self.args, set(), 65))
        self.sandbox(f"open({str(Path(self.tmp) / 'ordinary')!r}, 'w').write('ok')",
                     True, "ordinary tmp writes")
        for path in (endpoint, application):
            self.sandbox(f"import os; os.unlink({str(path)!r})", False,
                         "protected file unlink")
        for path in (broker, broker.parent, executable_root, executable_root.parent):
            self.sandbox(f"import os; os.rename({str(path)!r}, {str(path) + '-moved'!r})",
                         False, "protected ancestor rename")

    def test_symlink_mutable_state_rejected(self):
        data = self.state / "xdg/data"
        data.rmdir()
        data.symlink_to(self.checkout, target_is_directory=True)
        with self.assertRaises(ValueError):
            wall.generate(self.args, set(), 65)

    def test_github_credential_aliases_and_mutable_overlap_rejected(self):
        gh = self.home / ".config/gh"
        gh.mkdir(parents=True)
        hosts = gh / "hosts.yml"
        hosts.symlink_to(self.checkout / "credential")
        with self.assertRaises(ValueError):
            wall.generate(self.args, set(), 65)
        hosts.unlink()
        hosts.write_text("fixture-token")
        os.link(hosts, self.checkout / "credential")
        with self.assertRaises(ValueError):
            wall.generate(self.args, set(), 65)
        (self.checkout / "credential").unlink()
        self.args.checkout = str(gh)
        with self.assertRaises(ValueError):
            wall.generate(self.args, set(), 65)

    @unittest.skipUnless(sys.platform == "darwin", "requires macOS sandbox")
    def test_github_client_reads_only_approved_credentials(self):
        executable = shutil.which("gh")
        self.assertIsNotNone(executable, "GitHub CLI is required for this fixture")
        gh = self.home / ".config/gh"
        gh.mkdir(parents=True)
        (gh / "hosts.yml").write_text("github.com:\n    oauth_token: fixture-github-token\n    user: fixture\n    git_protocol: https\n")
        (gh / "config.yml").write_text("editor: fixture-private-config\n")
        private = wall.prepare_github_config(self.home, self.state)
        self.assertEqual(wall.prepare_github_config(self.home, self.state), private)
        self.profile = self.state / "github-wall.sb"
        self.profile.write_text(wall.generate(self.args, set(), 65))
        self.sandbox("import os,subprocess; env=dict(os.environ); "
                     f"env['GH_CONFIG_DIR']={str(private)!r}; "
                     f"result=subprocess.run([{executable!r}, 'auth', 'token', '--hostname', 'github.com'], "
                     "env=env,capture_output=True,text=True); "
                     "assert result.returncode == 0, result.stderr; "
                     "assert result.stdout.strip() == 'fixture-github-token'",
                     True, "unmodified GitHub CLI credential access")
        for path in (private / "hosts.yml", private / "config.yml"):
            self.sandbox(f"import os; os.unlink({str(path)!r})", False,
                         "private GitHub configuration unlink")

    def test_service_secrets_cannot_overlap_writable_checkout(self):
        self.args.service_state_dir = [str(self.checkout)]
        with self.assertRaises(ValueError):
            wall.generate(self.args, set(), 65)

    def test_read_only_directories_cannot_expose_protected_trees(self):
        for path in (self.home, self.home.parent, self.root, self.service, self.state):
            self.args.read_only_dir = [str(path)]
            with self.subTest(path=path), self.assertRaises(ValueError):
                wall.generate(self.args, set(), 65)

    def test_lsof_failure_and_malformed_records_fail_closed(self):
        for code, out, err in [(2, "", ""), (1, "n*:1234\n", ""),
                               (0, "", "permission denied"), (0, "nunknown\n", "")]:
            with patch.object(wall.subprocess, "run", return_value=subprocess.CompletedProcess([], code, out, err)):
                with self.assertRaises(ValueError):
                    wall.listening_ports()
        with patch.object(wall.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "p123\nn*:8000\nn[::1]:8441\n", "")):
            self.assertEqual(wall.listening_ports(), {8000, 8441})

    def test_paths_are_escaped_and_control_characters_rejected(self):
        self.assertEqual(wall.quoted('/a/"b\\c'), '"/a/\\"b\\\\c"')
        with self.assertRaises(ValueError):
            wall.quoted("/a\n(allow default)")

    def sandbox(self, script, expected, label, permission_denial=True):
        result = subprocess.run(
            ["/usr/bin/sandbox-exec", "-f", str(self.profile), PYTHON, "-c",
             "print('fixture-started', flush=True); " + script],
            capture_output=True, text=True, timeout=15, cwd=self.checkout,
            env={"PATH": "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(self.home),
                 "TMPDIR": self.tmp, "TMUX_TMPDIR": self.tmp, "PYTHONDONTWRITEBYTECODE": "1"},
        )
        self.assertIn("fixture-started", result.stdout, f"{label}: interpreter did not start: {result.stderr}")
        self.assertEqual(result.returncode == 0, expected,
                         f"{label}: rc={result.returncode}, stderr={result.stderr[-1500:]}")
        if not expected and permission_denial:
            self.assertIn("PermissionError", result.stderr,
                          f"{label} failed for a reason other than sandbox permission: {result.stderr}")

    def inherited_listener(self, listener):
        # Only a trusted host creates/binds/listens. The wall permits using
        # this loopback-only capability while refusing any new IP listeners.
        fd = listener.fileno()
        script = (f"import socket; s=socket.socket(fileno={fd}); s.settimeout(5); "
                  "c,_=s.accept(); c.sendall(b'fixture-loopback'); c.close()")
        child = subprocess.Popen(["/usr/bin/sandbox-exec", "-f", str(self.profile), PYTHON, "-c", script],
                                 pass_fds=(fd,), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                 env={"PATH": "/usr/bin:/bin", "HOME": str(self.home), "TMPDIR": self.tmp})
        try:
            with socket.create_connection(listener.getsockname()[:2], timeout=5) as client:
                client.settimeout(6)
                self.assertEqual(client.recv(128), b"fixture-loopback")
            out, err = child.communicate(timeout=8)
            self.assertEqual(child.returncode, 0, out + err)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)

    @unittest.skipUnless(sys.platform == "darwin", "keychain fixture requires macOS")
    def test_keychain_service_cannot_return_a_readable_fixture_secret(self):
        # Explicit fixture keychain on EVERY command: never query, modify or
        # change the default keychain. Delete the created keychain before the
        # temporary directory is removed, also on test failure.
        keychain = self.state / "xdg/state/fixture.keychain-db"
        secret = "local-wall-fixture-secret"
        security = "/usr/bin/security"

        def host(*args):
            result = subprocess.run([security, *args], capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
            return result

        host("create-keychain", "-p", "fixture-password", str(keychain))
        self.addCleanup(subprocess.run, [security, "delete-keychain", str(keychain)],
                        capture_output=True, timeout=15)
        host("unlock-keychain", "-p", "fixture-password", str(keychain))
        host("add-generic-password", "-a", "wall-fixture", "-s", "wall-test", "-w", secret,
             "-A", str(keychain))
        query = [security, "find-generic-password", "-a", "wall-fixture", "-s", "wall-test", "-w", str(keychain)]
        self.assertEqual(host(*query[1:]).stdout.strip(), secret)
        profile = wall.generate(self.args, set(), 65)
        wall_path = self.state / "keychain-wall.sb"
        wall_path.write_text(profile)
        control_path = self.state / "keychain-control.sb"
        control_path.write_text("\n".join(line for line in profile.splitlines()
                                          if not line.startswith("(deny mach-lookup")) + "\n")
        # The keychain file is under readable mutable state. Prove the file and
        # other sandbox rules alone do not protect the secret, then test the
        # identical command with the credential-service denial enabled.
        control = subprocess.run(["/usr/bin/sandbox-exec", "-f", str(control_path), *query],
                                 capture_output=True, text=True, timeout=15)
        self.assertEqual(control.returncode, 0, control.stderr)
        self.assertEqual(control.stdout.strip(), secret)
        denied = subprocess.run(["/usr/bin/sandbox-exec", "-f", str(wall_path), *query],
                                capture_output=True, text=True, timeout=15)
        self.assertNotEqual(denied.returncode, 0)
        self.assertNotIn(secret, denied.stdout + denied.stderr)

    @unittest.skipUnless(sys.platform == "darwin", "host service fixture requires macOS")
    def test_application_launch_broker_lookup_is_denied(self):
        script = ("import ctypes, sys; lib=ctypes.CDLL('/usr/lib/libSystem.B.dylib'); "
                  "port=ctypes.c_uint.in_dll(lib,'bootstrap_port').value; out=ctypes.c_uint(); "
                  "rc=lib.bootstrap_look_up(port,b'com.apple.coreservices.launchservicesd',ctypes.byref(out)); "
                  "print(rc); sys.exit(0 if rc == 0 else 1)")
        control = subprocess.run([sys.executable, "-c", script], capture_output=True, text=True, timeout=15)
        self.assertEqual(control.returncode, 0, f"host LaunchServices control failed: {control.stdout} {control.stderr}")
        profile = self.state / "broker-wall.sb"
        profile.write_text(wall.generate(self.args, set(), 65))
        denied = subprocess.run(["/usr/bin/sandbox-exec", "-f", str(profile), PYTHON, "-c", script],
                                capture_output=True, text=True, timeout=15)
        self.assertNotEqual(denied.returncode, 0)
        self.assertNotEqual(denied.stdout.strip(), "0")

    @unittest.skipUnless(sys.platform == "darwin", "actual Seatbelt enforcement requires macOS")
    def test_kernel_process_environment_query_is_denied(self):
        secret = "fixture-only-outside-process-secret"
        outsider = subprocess.Popen([PYTHON, "-c", "import time; time.sleep(30)"],
                                    env={"PATH": "/usr/bin:/bin", "WALL_FIXTURE_SECRET": secret})
        try:
            # CTL_KERN=1, KERN_PROCARGS2=49. Query only this disposable PID.
            script = ("import ctypes; lib=ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True); "
                      f"mib=(ctypes.c_int*3)(1,49,{outsider.pid}); "
                      "buf=ctypes.create_string_buffer(1024*1024); n=ctypes.c_size_t(len(buf)); "
                      "rc=lib.sysctl(mib,3,buf,ctypes.byref(n),None,0); "
                      "errno=ctypes.get_errno()\n"
                      "if rc != 0: raise OSError(errno, 'sysctl process args denied')\n"
                      f"assert {secret.encode()!r} in buf.raw[:n.value]")
            profile = wall.generate(self.args, set(), 65)
            control = self.state / "sysctl-control.sb"
            control.write_text(profile.replace("(deny sysctl-read)\n", "")
                               .replace("(deny process-info*)\n", ""))
            self.profile = control
            self.sandbox(script, True, "control kernel query exposes fixture secret")
            self.profile = self.state / "sysctl-wall.sb"
            self.profile.write_text(profile)
            self.sandbox(script, False, "kernel query for outside process environment")
            self.sandbox("import subprocess; "
                         "assert int(subprocess.check_output(['/usr/sbin/sysctl', '-n', 'hw.ncpu'])) > 0; "
                         "assert subprocess.check_output(['/usr/sbin/sysctl', '-n', 'kern.osrelease']).strip()",
                         True, "admitted CPU and OS kernel queries")
            self.args.contained_processes = True
            executable_root = self.state / "xdg/config/executables"
            executable_root.mkdir()
            command_image = executable_root / "opencode"
            command_image.write_text("immutable runtime fixture")
            (executable_root / "launch-env").write_text("immutable launch entry fixture")
            self.args.immutable_exec_dir = [str(executable_root)]
            self.args.command_groups = command_image
            self.profile.write_text(wall.generate(self.args, set(), 65))
            self.sandbox("import ctypes; lib=ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True); "
                         "assert lib.setpgid(0,0) == -1; assert ctypes.get_errno() == 1",
                         True, "command helpers cannot leave their cancellation group")
            self.sandbox(script, False, "contained launch still denies outside environment")
            self.sandbox("import ctypes; lib=ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True); "
                         "buf=ctypes.create_string_buffer(1024); "
                         f"assert lib.proc_pidinfo({outsider.pid},3,0,buf,len(buf)) == 0; "
                         "assert ctypes.get_errno() == 1",
                         True, "command groups do not admit outside process metadata")
            self.sandbox("import ctypes,os; lib=ctypes.CDLL('/usr/lib/libSystem.B.dylib'); "
                         "pids=(ctypes.c_int*65536)(); "
                         "assert lib.proc_listallpids(pids,ctypes.sizeof(pids)) <= 0; "
                         f"assert lib.kill({outsider.pid},0) == -1",
                         True, "command groups do not admit PID listing or outside signals")
        finally:
            outsider.terminate()
            outsider.wait(timeout=5)

    @unittest.skipUnless(sys.platform == "darwin", "actual Seatbelt enforcement requires macOS")
    def test_hardlinks_cannot_cross_read_or_write_boundaries(self):
        self.profile = self.state / "link-wall.sb"
        self.profile.write_text(wall.generate(self.args, set(), 65))
        for name in ("server.secret", "wall.sb", "launch-serve.sh"):
            original = self.state / name
            original.write_text("fixture-immutable")
            for target in (self.state / "xdg/data" / name, Path(self.tmp) / name):
                self.sandbox(f"import os; os.link({str(original)!r}, {str(target)!r})",
                             False, f"hardlink immutable {name}")
                self.assertFalse(target.exists())
                self.assertEqual(original.read_text(), "fixture-immutable")
        credential = self.home / ".npmrc"
        credential.write_text("fixture-host-token")
        target = self.checkout / "host-token-alias"
        self.sandbox(f"import os; os.link({str(credential)!r}, {str(target)!r}); open({str(target)!r}).read()",
                     False, "hardlink host credential")
        self.assertFalse(target.exists())
        source = self.checkout / "mutable-source"
        source.write_text("fixture-mutable")
        private_alias = self.state / "xdg/data/mutable-alias"
        self.sandbox(f"import os; os.link({str(source)!r}, {str(private_alias)!r}); "
                     f"assert open({str(private_alias)!r}).read() == 'fixture-mutable'",
                     True, "hardlink wholly within mutable boundaries")
        forbidden = self.state / "immutable-alias"
        self.sandbox(f"import os; os.link({str(source)!r}, {str(forbidden)!r})",
                     False, "hardlink mutable source into immutable state")
        self.assertFalse(forbidden.exists())

    @unittest.skipUnless(sys.platform == "darwin", "actual Seatbelt enforcement requires macOS")
    def test_signals_stay_inside_inherited_sandbox(self):
        self.profile = self.state / "signal-wall.sb"
        self.profile.write_text(wall.generate(self.args, set(), 65))
        outsider = subprocess.Popen(["/bin/sleep", "30"])
        try:
            # Only this disposable fixture PID is targeted; never send a
            # broadcast signal or touch live server/agent processes.
            for number in (0, 15, 19, 9):
                self.sandbox(f"import os; os.kill({outsider.pid}, {number})",
                             False, f"signal outside sandbox: {number}")
                self.assertIsNone(outsider.poll())
            self.sandbox("import subprocess, signal; child=subprocess.Popen(['/bin/sleep', '30']); "
                         "child.terminate(); assert child.wait(timeout=5) == -signal.SIGTERM",
                         True, "terminate own sandbox child")
        finally:
            outsider.terminate()
            outsider.wait(timeout=5)

    @unittest.skipUnless(sys.platform == "darwin", "actual Seatbelt enforcement requires macOS")
    def test_cargo_starts_with_read_only_system_ssl_configuration(self):
        resolved = subprocess.run(["rustup", "which", "cargo"], capture_output=True,
                                  text=True, timeout=15, check=True)
        cargo = Path(resolved.stdout.strip()).resolve(strict=True)
        self.args.read_only_dir.append(str(cargo.parent.parent))
        self.profile = self.state / "cargo-wall.sb"
        profile = wall.generate(self.args, set(), 65)
        self.profile.write_text(profile)
        self.assertNotIn('(subpath "/private/etc/ssl")', profile)
        self.assertIn('(literal "/private/etc/ssl/openssl.cnf")', profile)
        self.sandbox("import subprocess; "
                     f"output = subprocess.check_output([{str(cargo)!r}, '--version'], text=True); "
                     "assert output.startswith('cargo ')",
                     True, "Cargo startup with system SSL configuration")
        (self.checkout / "Cargo.toml").write_text(
            '[package]\nname = "wall-cargo-fixture"\nversion = "0.1.0"\nedition = "2021"\n'
            '[lib]\npath = "lib.rs"\n')
        (self.checkout / "lib.rs").write_text(
            '#[test]\nfn fixture_runs() { assert_eq!(2 + 2, 4); }\n')
        compiler = cargo.with_name("rustc")
        cache = self.checkout / "cargo-home"
        self.sandbox("import os, subprocess; "
                     f"environment = dict(os.environ, RUSTC={str(compiler)!r}, CARGO_HOME={str(cache)!r}); "
                     f"output = subprocess.check_output([{str(cargo)!r}, 'test', '--offline', '--lib'], "
                     "env=environment, text=True); assert '1 passed' in output",
                     True, "offline Cargo compile and test with private writable cache")

    @unittest.skipUnless(sys.platform == "darwin", "actual Seatbelt enforcement requires macOS")
    def test_rust_toolchain_builds_from_explicit_read_access(self):
        # Resolve the host's installed compiler outside the wall, then admit
        # only that immutable toolchain. No host rustup config or cache grant.
        resolved = subprocess.run(["rustup", "which", "rustc"], capture_output=True,
                                  text=True, timeout=15, check=True)
        compiler = Path(resolved.stdout.strip()).resolve(strict=True)
        self.args.read_only_dir.append(str(compiler.parent.parent))
        self.profile = self.state / "rust-wall.sb"
        self.profile.write_text(wall.generate(self.args, set(), 65))
        source = self.checkout / "hello.rs"
        binary = self.checkout / "hello"
        source.write_text('fn main() { println!("fixture-rust"); }')
        self.sandbox("import subprocess; "
                     f"subprocess.run([{str(compiler)!r}, {str(source)!r}, '-o', {str(binary)!r}], check=True); "
                     f"assert subprocess.check_output([{str(binary)!r}], text=True).strip() == 'fixture-rust'",
                     True, "Rust compile and run with explicit toolchain access")

    @unittest.skipUnless(sys.platform == "darwin", "actual Seatbelt enforcement requires macOS")
    def test_actual_wall_files_network_and_future_successor(self):
        # Use ephemeral, unprivileged ports to avoid colliding with live services.
        # Reserve ranges before generation; successor sockets and files appear later.
        listeners = []
        for _ in range(7):
            server = socket.socket()
            server.bind(("127.0.0.1", 0))
            server.listen()
            self.addCleanup(server.close)
            listeners.append(server)
        ports = [s.getsockname()[1] for s in listeners]
        own, successor, gateway, future_gateway, proxy, model, judge = ports
        # Disjoint single-port ranges keep this check independent of live defaults;
        # the regular generation test above covers every default range member.
        # Choose adjacent server ports so a contiguous reserved server range
        # does not accidentally include any randomly allocated service ports.
        listeners[0].close()
        listeners[1].close()
        pair = None
        for _ in range(100):
            first = socket.socket()
            first.bind(("127.0.0.1", 0))
            first.listen()
            candidate = first.getsockname()[1]
            second = socket.socket()
            try:
                second.bind(("127.0.0.1", candidate + 1))
                second.listen()
            except OSError:
                first.close()
                second.close()
                continue
            if candidate + 1 in ports[2:]:
                first.close()
                second.close()
                continue
            pair = (first, second)
            break
        self.assertIsNotNone(pair, "could not allocate adjacent fixture ports")
        for server in pair:
            self.addCleanup(server.close)
        own, successor = [s.getsockname()[1] for s in pair]
        # Release the successor until AFTER the wall has been generated.
        pair[1].close()
        self.args.agent_port = str(own)
        self.args.agent_port_range = f"{own}-{successor}"
        self.args.gateway_port = str(gateway)
        self.args.gateway_port_range = f"{gateway}-{gateway}"
        self.args.egress_port = str(proxy)
        self.args.egress_port_range = f"{proxy}-{proxy}"
        self.args.model_port = str(model)
        self.args.judge_port = str(judge)
        self.profile = self.state / "wall.sb"
        self.profile.write_text(wall.generate(self.args, {own, gateway, proxy, model, judge, future_gateway}, 65))
        later = socket.socket()
        later.bind(("127.0.0.1", successor))
        later.listen()
        self.addCleanup(later.close)
        other = self.root / "agent-b"
        other.mkdir()
        secret = other / "server.secret"
        secret.write_text("fixture-other-secret")
        for path in [self.state / p for p in ("server.secret", "launch-serve.sh", "xdg/config/sm_judge.js")]:
            path.write_text("fixture-immutable")
        for name in (".ssh/key", ".claude/settings.json", ".codex/auth.json", ".aws/credentials",
                     ".config/session-manager/config.yaml", ".config/gh/config.yml", ".config/git/credentials",
                     ".gitconfig", "Library/Keychains/key", ".npmrc", ".pypirc",
                     "Library/Application Support/Firefox/Profiles/default/cookies.sqlite",
                     "Library/Application Support/Google/Chrome/Default/Cookies",
                     "private-unrecognized/credentials", ".config/future-client/session.json",
                     ".claude.json", ".netrc", ".git-credentials", ".cargo/credentials",
                     ".cargo/credentials.toml", ".cargo/config", ".cargo/config.toml"):
            target = self.home / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("fixture-credential")
            self.sandbox(f"open({str(target)!r}).read()", False, f"read {name}")
        gh_hosts = self.home / ".config/gh/hosts.yml"
        gh_hosts.write_text("fixture-github-token")
        self.sandbox(f"assert open({str(gh_hosts)!r}).read() == 'fixture-github-token'",
                     True, "owner-approved GitHub credential read")
        self.sandbox(f"open({str(gh_hosts)!r}, 'w').write('modified')",
                     False, "GitHub credential write")
        self.sandbox(f"import os; os.link({str(gh_hosts)!r}, {str(self.checkout / 'gh-alias')!r})",
                     False, "GitHub credential hardlink")
        self.sandbox(f"import os; os.listdir({str(gh_hosts.parent)!r})",
                     False, "GitHub configuration listing")
        for name in ("wall.sb", "server.secret", "launch-serve.sh", "xdg/config/sm_judge.js"):
            target = self.state / name
            original = target.read_text()
            self.sandbox(f"open({str(target)!r}, 'w').write('modified')", False, f"immutable {name}")
            self.assertEqual(target.read_text(), original)
        private_cargo = self.state / "xdg/cache/cargo"
        private_cargo.mkdir()
        for target in [self.checkout, private_cargo, *[self.state / p for p in
                       ("xdg/data", "xdg/cache", "xdg/state")], Path(self.tmp)]:
            self.sandbox(f"open({str(target / 'write-test')!r}, 'w').write('ok')", True, f"write {target}")
        self.sandbox("import os, subprocess; env = dict(os.environ, GIT_CONFIG_GLOBAL='/dev/null', "
                     "GIT_CONFIG_NOSYSTEM='1'); "
                     f"subprocess.run(['git', '-C', {str(self.checkout)!r}, 'init', '-q'], env=env, check=True); "
                     f"subprocess.run(['git', '-C', {str(self.checkout)!r}, '-c', 'user.name=wall-fixture', "
                     "'-c', 'user.email=wall-fixture@localhost', '-c', 'commit.gpgsign=false', "
                     "'commit', '--allow-empty', '-qm', 'fixture'], env=env, check=True)", True,
                     "Git work with host global config excluded")
        for target in (self.home / "bad", self.root / "bad", other / "bad", self.service / "bad"):
            self.sandbox(f"open({str(target)!r}, 'w').write('bad')", False, f"write {target}")
        host_cargo_binary = self.home / ".cargo/bin/cargo"
        host_cargo_binary.parent.mkdir(exist_ok=True)
        host_cargo_binary.write_text("fixture-host-binary")
        self.sandbox(f"open({str(host_cargo_binary)!r}).read()", True, "read admitted host tool")
        for target in (host_cargo_binary, self.home / ".cargo/config.toml", self.home / ".cargo/poison-cache"):
            original = target.read_text() if target.exists() else None
            self.sandbox(f"open({str(target)!r}, 'w').write('modified')", False, f"modify host cargo {target}")
            if original is not None:
                self.assertEqual(target.read_text(), original)
        for root in (Path("/private/tmp"), Path(wall.user_temp()).resolve()):
            target = root / ("wall-denied-" + self.state.parent.parent.parent.name + str(os.getpid()))
            self.addCleanup(target.unlink, missing_ok=True)
            self.sandbox(f"open({str(target)!r}, 'w').write('bad')", False, f"write general temp {root}")
        self.sandbox(f"open({str(secret)!r}).read()", False, "read later successor secret")
        link = self.checkout / "escape"
        link.symlink_to(other, target_is_directory=True)
        self.sandbox(f"open({str(link / 'server.secret')!r}).read()", False, "symlink into successor")
        self.sandbox(f"open({str(self.profile)!r}).read()", True, "read own profile")
        self.sandbox(f"import os; os.listdir({str(self.root)!r})", False, "list other agents' state")
        self.sandbox(f"import os; os.listdir({str(self.root.parent)!r})", False, "list host state")
        log = self.logs / "fixture.log"
        log.write_text("fixture-log")
        self.sandbox(f"open({str(log)!r}).read()", True, "read explicitly admitted sm log")
        credential_alias = self.checkout / "credential-alias"
        credential_alias.symlink_to(self.home / ".npmrc")
        self.sandbox(f"open({str(credential_alias)!r}).read()", False, "symlink to npm credential")
        credential = self.service / "gateway.secret"
        credential.write_text("fixture-service-secret")
        self.sandbox(f"open({str(credential)!r}).read()", False, "read service secret")
        blocked_socket = self.root.parent / "sm-fixture.sock"
        host_socket = socket.socket(socket.AF_UNIX)
        host_socket.bind(str(blocked_socket))
        host_socket.listen()
        self.addCleanup(host_socket.close)
        self.sandbox(f"import socket; socket.socket(socket.AF_UNIX).connect({str(blocked_socket)!r})",
                     False, "sm-state Unix socket")
        daemon_socket = self.home / ".docker/run/docker.sock"
        daemon_socket.parent.mkdir(parents=True)
        daemon = socket.socket(socket.AF_UNIX)
        daemon.bind(str(daemon_socket))
        daemon.listen()
        self.addCleanup(daemon.close)
        self.sandbox(f"import socket; socket.socket(socket.AF_UNIX).connect({str(daemon_socket)!r})",
                     False, "host daemon outside private tmp")
        daemon_alias = Path(self.tmp) / "daemon-alias.sock"
        daemon_alias.symlink_to(daemon_socket)
        self.sandbox(f"import socket; socket.socket(socket.AF_UNIX).connect({str(daemon_alias)!r})",
                     False, "private tmp symlink to host daemon")
        daemon.setblocking(False)
        with self.assertRaises(BlockingIOError):
            daemon.accept()
        for number, allowed in [(own, False), (successor, False), (future_gateway, False),
                                (gateway, True), (proxy, True), (model, True), (judge, True)]:
            self.sandbox(f"import socket; socket.create_connection(('127.0.0.1', {number}), timeout=2)",
                         allowed, f"connect {number}")
        with socket.socket() as late_service:
            late_service.bind(("127.0.0.1", 0))
            late_service.listen()
            late_port = late_service.getsockname()[1]
            self.sandbox(f"import socket; socket.create_connection(('127.0.0.1',{late_port}),timeout=2)",
                         False, "unadmitted host service launched after the profile")
        # Direct connections do not depend on outside network reachability: the
        # sandbox must reject at connect(), before any packet can leave the host.
        self.sandbox("import socket; socket.create_connection(('1.1.1.1',443),timeout=2)", False, "internet")
        self.sandbox("import socket; socket.getaddrinfo('example.com',443)", False, "DNS", permission_denial=False)
        for address, family in [("0.0.0.0", "AF_INET"), ("127.0.0.1", "AF_INET"),
                                ("::", "AF_INET6"), ("::1", "AF_INET6")]:
            self.sandbox(f"import socket; s=socket.socket(socket.{family}); s.bind(({address!r},0)); s.listen()",
                         False, f"new IP listener {address}")
        self.inherited_listener(pair[0])
        with socket.socket(socket.AF_INET6) as ipv6:
            ipv6.bind(("::1", 0))
            ipv6.listen()
            self.inherited_listener(ipv6)
        self.sandbox(f"import socket; s=socket.socket(socket.AF_UNIX); s.bind({str(Path(self.tmp) / 'private.sock')!r}); "
                     f"s.listen(); socket.socket(socket.AF_UNIX).connect({str(Path(self.tmp) / 'private.sock')!r})",
                     True, "private Unix socket")
        self.sandbox("import subprocess; subprocess.run(['tmux', 'new-session', '-d', '-s', 'wall-fixture', "
                     "'sleep 10'], check=True); subprocess.run(['tmux', 'list-sessions'], check=True); "
                     "subprocess.run(['tmux', 'kill-server'], check=True)", True, "private tmux")


if __name__ == "__main__":
    unittest.main(verbosity=2)
