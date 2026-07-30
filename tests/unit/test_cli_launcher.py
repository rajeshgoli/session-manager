import os
import sys
from pathlib import Path

import pytest

from src.cli import launcher


def _executable(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/bin/sh\nexit 0\n")
    path.chmod(0o755)
    return path


def _python_entry_point(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/usr/bin/env python3\nfrom src.cli.launcher import main\n")
    path.chmod(0o755)
    return path


def _shell_wrapped_python_entry_point(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "#!/bin/sh\n"
        "'''exec' \"/a long path/venv/bin/python3\" \"$0\" \"$@\"\n"
        "' '''\n"
        "from src.cli.launcher import main\n"
    )
    path.chmod(0o755)
    return path


def test_find_rust_sm_skips_cached_python_entry_point(tmp_path):
    cached_entry = _executable(tmp_path / "venv/bin/sm")
    rust_cli = _executable(tmp_path / "target/release/sm")

    found = launcher.find_rust_sm(
        argv0=str(cached_entry),
        environ={"PATH": str(cached_entry.parent)},
        repo_root=tmp_path,
    )

    assert found == rust_cli.resolve()


def test_find_rust_sm_uses_later_path_entry_after_cached_entry(tmp_path):
    cached_entry = _executable(tmp_path / "venv/bin/sm")
    rust_cli = _executable(tmp_path / "bin/sm")

    found = launcher.find_rust_sm(
        argv0=str(cached_entry),
        environ={
            "PATH": os.pathsep.join(
                [str(cached_entry.parent), str(rust_cli.parent)]
            )
        },
        repo_root=tmp_path / "missing-repo",
    )

    assert found == rust_cli.resolve()


def test_find_rust_sm_skips_a_distinct_python_console_script(tmp_path):
    cached_entry = _python_entry_point(tmp_path / "venv/bin/sm")
    other_python_entry = _python_entry_point(tmp_path / "other-venv/bin/sm")
    rust_cli = _executable(tmp_path / "bin/sm")

    found = launcher.find_rust_sm(
        argv0=str(cached_entry),
        environ={
            "PATH": os.pathsep.join(
                [str(other_python_entry.parent), str(rust_cli.parent)]
            )
        },
        repo_root=tmp_path / "missing-repo",
    )

    assert found == rust_cli.resolve()


def test_find_rust_sm_skips_a_shell_wrapped_python_console_script(tmp_path):
    cached_entry = _python_entry_point(tmp_path / "venv/bin/sm")
    shell_wrapper = _shell_wrapped_python_entry_point(
        tmp_path / "long path/venv/bin/sm"
    )
    rust_cli = _executable(tmp_path / "bin/sm")

    found = launcher.find_rust_sm(
        argv0=str(cached_entry),
        environ={
            "PATH": os.pathsep.join([str(shell_wrapper.parent), str(rust_cli.parent)])
        },
        repo_root=tmp_path / "missing-repo",
    )

    assert found == rust_cli.resolve()


def test_main_execs_rust_cli_with_unchanged_arguments(monkeypatch, tmp_path):
    rust_cli = _executable(tmp_path / "sm")
    observed = {}

    def fake_execv(path, argv):
        observed["path"] = path
        observed["argv"] = argv
        raise RuntimeError("exec")

    monkeypatch.setattr(launcher, "find_rust_sm", lambda: rust_cli)
    monkeypatch.setattr(launcher.os, "execv", fake_execv)
    monkeypatch.setattr(launcher.sys, "argv", ["/cached/venv/bin/sm", "btw", "abc123"])

    with pytest.raises(RuntimeError, match="exec"):
        launcher.main()

    assert observed == {
        "path": str(rust_cli),
        "argv": [str(rust_cli), "btw", "abc123"],
    }
    assert os.environ["SM_WATCH_PYTHON"] == sys.executable
def test_main_fails_clearly_when_rust_cli_is_missing(monkeypatch, capsys):
    monkeypatch.setattr(launcher, "find_rust_sm", lambda: None)

    assert launcher.main([]) == 127
    assert "Rust sm CLI not found" in capsys.readouterr().err


def test_main_reports_exec_failure_without_a_traceback(monkeypatch, tmp_path, capsys):
    rust_cli = _executable(tmp_path / "sm")
    monkeypatch.setattr(launcher, "find_rust_sm", lambda: rust_cli)
    monkeypatch.setattr(
        launcher.os,
        "execv",
        lambda _path, _argv: (_ for _ in ()).throw(OSError("bad architecture")),
    )

    assert launcher.main(["what", "abc123"]) == 126
    error = capsys.readouterr().err
    assert f"failed to execute Rust sm CLI at {rust_cli}" in error
    assert "bad architecture" in error
