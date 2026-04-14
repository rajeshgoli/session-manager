import os

from src.cli import commands


class _FakeClient:
    def __init__(self, runtime=None, rollout=None):
        self._runtime = runtime
        self._rollout = rollout

    def get_codex_fork_runtime(self):
        return self._runtime

    def get_rollout_flags(self):
        return self._rollout


def test_cmd_codex_fork_info_text_output(monkeypatch, capsys):
    client = _FakeClient(
        runtime={
            "command": "codex",
            "args": ["--danger"],
            "artifact_release": "v0.2.0-sm",
            "artifact_ref": "abc123",
            "is_pinned": True,
            "event_schema_version": 2,
            "artifact_platforms": ["darwin-arm64", "linux-x86_64"],
            "rollback_provider": "codex",
            "rollback_command": "sm codex-legacy",
        }
    )
    monkeypatch.setattr(commands, "_collect_codex_fork_maintenance_info", lambda payload: {
        "repo_root": "/tmp/codex-fork",
        "fork_head": "fork123",
        "upstream_head": "up123",
        "divergence_ahead": 10,
        "divergence_behind": 1226,
        "binary_mtime": "2026-04-01T10:00:00Z",
        "head_commit_committed_at": "2026-04-10T09:00:00+00:00",
        "binary_older_than_fork_head": True,
        "latest_upstream_release_tag": "rust-v0.120.0",
        "latest_upstream_release_published_at": "2026-04-11T02:53:49Z",
        "latest_upstream_release_newer_than_binary": True,
        "latest_upstream_release_commit": "96254a763",
        "fork_contains_latest_upstream_release": False,
        "release_build_script": "/repo/scripts/codex_fork/release_artifacts.sh",
        "maintenance_spec": "/repo/specs/546_codex_fork_release_sync_mechanism.md",
        "build_recommended": True,
        "build_reasons": [
            "local codex binary predates the current fork HEAD commit",
            "local codex binary predates upstream release rust-v0.120.0",
        ],
        "sync_recommended": True,
        "sync_reasons": ["fork does not yet contain upstream release rust-v0.120.0"],
    })
    rc = commands.cmd_codex_fork_info(client)
    assert rc == 0
    output = capsys.readouterr().out
    assert "Codex-fork runtime metadata" in output
    assert "artifact_ref: abc123" in output
    assert "event_schema_version: 2" in output
    assert "binary_older_than_fork_head: True" in output
    assert "latest_upstream_release: rust-v0.120.0 (2026-04-11T02:53:49Z)" in output
    assert "latest_upstream_release_newer_than_binary: True" in output
    assert "build_recommended: True" in output
    assert "build_reason: local codex binary predates the current fork HEAD commit" in output
    assert "fork_contains_latest_upstream_release: False" in output
    assert "sync_reason: fork does not yet contain upstream release rust-v0.120.0" in output


def test_cmd_codex_fork_info_json_output(monkeypatch, capsys):
    client = _FakeClient(
        runtime={
            "artifact_ref": "abc123",
            "event_schema_version": 2,
            "is_pinned": True,
        }
    )
    monkeypatch.setattr(commands, "_collect_codex_fork_maintenance_info", lambda payload: {
        "latest_upstream_release_tag": "rust-v0.120.0",
        "fork_contains_latest_upstream_release": True,
        "sync_recommended": False,
        "sync_reasons": [],
    })
    rc = commands.cmd_codex_fork_info(client, json_output=True)
    assert rc == 0
    output = capsys.readouterr().out
    assert "\"artifact_ref\": \"abc123\"" in output
    assert "\"maintenance\"" in output
    assert "\"latest_upstream_release_tag\": \"rust-v0.120.0\"" in output


def test_cmd_codex_fork_info_reports_probe_skip(monkeypatch, capsys):
    client = _FakeClient(
        runtime={
            "command": "codex",
            "artifact_ref": "abc123",
            "event_schema_version": 2,
        }
    )
    monkeypatch.setattr(commands, "_collect_codex_fork_maintenance_info", lambda payload: {
        "binary_path": "codex",
        "binary_exists": False,
        "maintenance_probe_status": "skipped",
        "maintenance_probe_reason": "runtime command is not a filesystem path and could not be resolved via PATH",
        "build_recommended": False,
        "build_reasons": [],
        "sync_recommended": False,
        "sync_reasons": [],
    })
    rc = commands.cmd_codex_fork_info(client)
    assert rc == 0
    output = capsys.readouterr().out
    assert "maintenance_probe_status: skipped" in output
    assert "maintenance_probe_reason: runtime command is not a filesystem path" in output


def test_cmd_codex_fork_info_unavailable(capsys):
    client = _FakeClient(runtime=None, rollout=None)
    rc = commands.cmd_codex_fork_info(client)
    assert rc == 1
    assert "endpoint unavailable or incompatible" in capsys.readouterr().err


def test_collect_codex_fork_maintenance_info_skips_unresolved_bare_command(monkeypatch):
    monkeypatch.setattr(commands, "_resolve_runtime_binary_path", lambda command: None)

    info = commands._collect_codex_fork_maintenance_info({"command": "codex"})

    assert info["maintenance_probe_status"] == "skipped"
    assert info["build_recommended"] is False
    assert info["sync_recommended"] is False


def test_collect_codex_fork_maintenance_info_tracks_latest_release(monkeypatch, tmp_path):
    repo_root = tmp_path / "codex-fork"
    repo_root.mkdir()
    (repo_root / ".git").write_text("")
    binary_path = repo_root / "codex-rs" / "target" / "release" / "codex"
    binary_path.parent.mkdir(parents=True)
    binary_path.write_text("binary")
    old_timestamp = 1_775_000_000
    os.utime(binary_path, (old_timestamp, old_timestamp))

    def _fake_run_text(args):
        cmd = " ".join(args)
        if "rev-parse --abbrev-ref HEAD" in cmd:
            return "main"
        if "rev-parse --short HEAD" in cmd:
            return "fork123"
        if "log -1 --format=%cI HEAD" in cmd:
            return "2026-04-10T09:00:00+00:00"
        if "rev-parse --short upstream/main" in cmd:
            return "up456"
        if "rev-list --left-right --count HEAD...upstream/main" in cmd:
            return "10\t1226"
        if "gh release view" in cmd:
            return '{"tagName":"rust-v0.120.0","publishedAt":"2026-04-11T02:53:49Z"}'
        if "rev-list -n 1 refs/tags/rust-v0.120.0" in cmd:
            return "releasecommit"
        return None

    def _fake_run(args):
        cmd = " ".join(args)
        if "fetch upstream main --tags --quiet" in cmd:
            return 0
        if "merge-base --is-ancestor releasecommit HEAD" in cmd:
            return 1
        return 0

    monkeypatch.setattr(commands, "_run_text_command", _fake_run_text)
    monkeypatch.setattr(commands, "_run_command", _fake_run)

    info = commands._collect_codex_fork_maintenance_info({"command": str(binary_path)})

    assert info["latest_upstream_release_tag"] == "rust-v0.120.0"
    assert info["latest_upstream_release_commit"] == "releasecommit"
    assert info["binary_older_than_fork_head"] is True
    assert info["latest_upstream_release_newer_than_binary"] is True
    assert info["build_recommended"] is True
    assert info["release_build_script"].endswith("scripts/codex_fork/release_artifacts.sh")
    assert info["maintenance_spec"].endswith("specs/546_codex_fork_release_sync_mechanism.md")
    assert info["fork_contains_latest_upstream_release"] is False
    assert info["sync_recommended"] is True
    assert info["sync_reasons"] == ["fork does not yet contain upstream release rust-v0.120.0"]
