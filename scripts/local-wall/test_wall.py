#!/usr/bin/env python3
"""Fixture-only profile and actual macOS sandbox checks; no live credentials."""

import importlib.util
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
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
        for path in [self.checkout, self.service, self.home / ".cargo", self.state / "xdg/config",
                     *[self.state / p for p in ("xdg/data", "xdg/cache", "xdg/state", "tmp")]]:
            path.mkdir(parents=True, exist_ok=True)
        self.tmp = wall.temporary_directory(self.state, 65)
        self.args = wall.parser().parse_args([
            "--home", str(self.home), "--checkout", str(self.checkout),
            "--state-root", str(self.root), "--state-dir", str(self.state),
            "--tmp-dir", self.tmp, "--agent-port", "18500", "--gateway-port", "18600",
            "--egress-port", "18700", "--model-port", "8000", "--judge-port", "8441",
            "--service-state-dir", str(self.service),
        ])

    def test_profile_reserves_future_ports_and_exempts_admitted_services(self):
        profile = wall.generate(self.args, {8000, 8441, 18600, 18700, 54321}, 65)
        for number in (18500, 18599, 18601, 18699, 18701, 18799, 8420, 8443, 1234, 54321):
            self.assertIn(f'(deny network-outbound (remote ip "localhost:{number}"))', profile)
        for number in (8000, 8441, 18600, 18700):
            self.assertNotIn(f'(deny network-outbound (remote ip "localhost:{number}"))', profile)

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

    def test_symlink_mutable_state_rejected(self):
        data = self.state / "xdg/data"
        data.rmdir()
        data.symlink_to(self.checkout, target_is_directory=True)
        with self.assertRaises(ValueError):
            wall.generate(self.args, set(), 65)

    def test_service_secrets_cannot_overlap_writable_checkout(self):
        self.args.service_state_dir = [str(self.checkout)]
        with self.assertRaises(ValueError):
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
            ["/usr/bin/sandbox-exec", "-f", str(self.profile), sys.executable, "-c", script],
            capture_output=True, text=True, timeout=15,
            env={"PATH": "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(self.home),
                 "TMPDIR": self.tmp, "TMUX_TMPDIR": self.tmp, "PYTHONDONTWRITEBYTECODE": "1"},
        )
        self.assertEqual(result.returncode == 0, expected,
                         f"{label}: rc={result.returncode}, stderr={result.stderr[-1500:]}")
        if not expected and permission_denial:
            self.assertIn("PermissionError", result.stderr,
                          f"{label} failed for a reason other than sandbox permission: {result.stderr}")

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
                     ".config/session-manager/config.yaml", ".config/gh/hosts.yml", ".config/git/credentials",
                     ".gitconfig", "Library/Keychains/key",
                     ".claude.json", ".netrc", ".git-credentials", ".cargo/credentials",
                     ".cargo/credentials.toml", ".cargo/config", ".cargo/config.toml"):
            target = self.home / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("fixture-credential")
            self.sandbox(f"open({str(target)!r}).read()", False, f"read {name}")
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
        host_cargo_binary.parent.mkdir()
        host_cargo_binary.write_text("fixture-host-binary")
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
        self.sandbox(f"import os; os.listdir({str(self.root.parent)!r})", True, "read sm log directory")
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
        # Direct connections do not depend on outside network reachability: the
        # sandbox must reject at connect(), before any packet can leave the host.
        self.sandbox("import socket; socket.create_connection(('1.1.1.1',443),timeout=2)", False, "internet")
        self.sandbox("import socket; socket.getaddrinfo('example.com',443)", False, "DNS", permission_denial=False)
        self.sandbox("import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); "
                     "socket.create_connection(s.getsockname(),timeout=2)", True, "new test loopback listener")
        self.sandbox(f"import socket; s=socket.socket(socket.AF_UNIX); s.bind({str(Path(self.tmp) / 'private.sock')!r}); "
                     f"s.listen(); socket.socket(socket.AF_UNIX).connect({str(Path(self.tmp) / 'private.sock')!r})",
                     True, "private Unix socket")
        self.sandbox("import subprocess; subprocess.run(['tmux', 'new-session', '-d', '-s', 'wall-fixture', "
                     "'sleep 10'], check=True); subprocess.run(['tmux', 'list-sessions'], check=True); "
                     "subprocess.run(['tmux', 'kill-server'], check=True)", True, "private tmux")


if __name__ == "__main__":
    unittest.main(verbosity=2)
