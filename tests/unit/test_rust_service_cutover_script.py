import os
import plistlib
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "rust-service-cutover.sh"


def run_script(*args: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(SCRIPT), *args],
        cwd=REPO_ROOT,
        text=True,
        capture_output=True,
        check=False,
        env=env,
    )


def fake_launchctl_env(tmp_path: Path) -> tuple[dict[str, str], Path]:
    """PATH with a launchctl that only records its calls, so a start-rust test can
    never register a real launchd job. Every label reads as not loaded."""
    bin_dir = tmp_path / "fake-bin"
    bin_dir.mkdir()
    calls = tmp_path / "launchctl-calls.log"
    launchctl = bin_dir / "launchctl"
    launchctl.write_text(
        f'#!/bin/sh\necho "$*" >> "{calls}"\n[ "$1" = print ] && exit 113\nexit 0\n',
        encoding="utf-8",
    )
    launchctl.chmod(0o755)
    env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}"}
    env.pop("CARGO_TARGET_DIR", None)
    return env, calls


def test_rust_service_cutover_script_has_valid_bash_syntax():
    result = subprocess.run(
        ["bash", "-n", str(SCRIPT)],
        cwd=REPO_ROOT,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr


def test_rust_service_cutover_plan_is_non_mutating_and_reports_blockers(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server:\n  host: 127.0.0.1\n  port: 8420\n", encoding="utf-8")
    missing_binary = tmp_path / "missing-sm-server"
    plist_path = tmp_path / "rust.plist"

    result = run_script(
        "plan",
        "--config",
        str(config),
        "--binary",
        str(missing_binary),
        "--port",
        "18420",
        "--plist",
        str(plist_path),
    )

    assert result.returncode == 0
    assert "Rust Session Manager service cutover plan" in result.stdout
    assert "rust_binary_not_executable" in result.stdout
    assert str(missing_binary) in result.stdout
    assert str(plist_path) in result.stdout
    assert not plist_path.exists()


def test_rust_service_cutover_render_plist_uses_rust_binary_and_config(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server:\n  host: 127.0.0.1\n  port: 18420\n", encoding="utf-8")
    binary = tmp_path / "sm-server"
    binary.write_text("#!/bin/sh\n", encoding="utf-8")
    binary.chmod(0o755)

    result = run_script(
        "render-plist",
        "--config",
        str(config),
        "--binary",
        str(binary),
        "--host",
        "127.0.0.1",
        "--port",
        "18420",
    )

    assert result.returncode == 0, result.stderr
    plist = plistlib.loads(result.stdout.encode("utf-8"))
    assert plist["Label"] == "com.rajeshgoli.session-manager-rust"
    assert plist["ProgramArguments"] == [
        str(binary),
        "--host",
        "127.0.0.1",
        "--port",
        "18420",
        "--config",
        str(config),
    ]
    assert plist["WorkingDirectory"] == str(REPO_ROOT)


def test_rust_service_cutover_persistently_disables_retired_python_service():
    script = SCRIPT.read_text(encoding="utf-8")

    assert 'launchctl disable "$DOMAIN/$label"; then' in script
    assert 'is_label_disabled "$label"' in script
    assert 'failed to verify disabled override for $label' in script
    assert 'echo "disabled and stopped $label"' in script
    assert 'require_no_python_labels' in script
    assert "rollback-python" not in script
    assert 'launchctl enable "$DOMAIN/$label"' not in script


def test_rust_service_cutover_defaults_to_the_installed_binary(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server:\n  port: 18420\n", encoding="utf-8")

    result = run_script("render-plist", "--config", str(config))

    assert result.returncode == 0, result.stderr
    plist = plistlib.loads(result.stdout.encode("utf-8"))
    assert plist["ProgramArguments"][0] == str(REPO_ROOT / ".local" / "bin" / "sm-server")


def test_rust_service_cutover_plan_flags_cargo_output_as_a_blocker(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server:\n  port: 18420\n", encoding="utf-8")
    env, _ = fake_launchctl_env(tmp_path)

    result = run_script(
        "plan",
        "--config",
        str(config),
        "--binary",
        "target/release/sm-server",
        "--port",
        "18421",
        env=env,
    )

    assert result.returncode == 0, result.stderr
    assert "rust_binary_in_build_dir" in result.stdout
    assert str(REPO_ROOT / "target" / "release" / "sm-server") in result.stdout


def test_rust_service_cutover_start_refuses_cargo_output_behind_an_alias(tmp_path):
    """A symlink into CARGO_TARGET_DIR is still cargo's output: start-rust must
    refuse before writing the plist or calling anything but launchctl print."""
    config = tmp_path / "config.yaml"
    config.write_text("server:\n  port: 18420\n", encoding="utf-8")
    target_dir = tmp_path / "cargo-target"
    built = target_dir / "release" / "sm-server"
    built.parent.mkdir(parents=True)
    built.write_text("#!/bin/sh\n", encoding="utf-8")
    built.chmod(0o755)
    alias = tmp_path / "sm-server"
    alias.symlink_to(built)
    plist_path = tmp_path / "rust.plist"
    env, calls = fake_launchctl_env(tmp_path)
    env["CARGO_TARGET_DIR"] = str(target_dir)

    result = run_script(
        "start-rust",
        "--config",
        str(config),
        "--binary",
        str(alias),
        "--port",
        "18422",
        "--label",
        "com.example.sm-cutover-test",
        "--plist",
        str(plist_path),
        "--log-dir",
        str(tmp_path / "logs"),
        env=env,
    )

    assert result.returncode != 0
    assert "rust_binary_in_build_dir" in result.stderr
    assert not plist_path.exists()
    recorded = calls.read_text(encoding="utf-8").splitlines() if calls.exists() else []
    assert all(line.startswith("print ") for line in recorded), recorded


def test_rust_service_cutover_start_accepts_an_installed_copy(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text("server:\n  port: 18420\n", encoding="utf-8")
    installed = tmp_path / "bin" / "sm-server"
    installed.parent.mkdir()
    installed.write_text("#!/bin/sh\n", encoding="utf-8")
    installed.chmod(0o755)
    plist_path = tmp_path / "rust.plist"
    env, calls = fake_launchctl_env(tmp_path)

    result = run_script(
        "start-rust",
        "--config",
        str(config),
        "--binary",
        str(installed),
        "--port",
        "18423",
        "--label",
        "com.example.sm-cutover-test",
        "--plist",
        str(plist_path),
        "--log-dir",
        str(tmp_path / "logs"),
        env=env,
    )

    assert result.returncode == 0, result.stdout + result.stderr
    assert plistlib.loads(plist_path.read_bytes())["ProgramArguments"][0] == str(installed)
    assert "bootstrap" in calls.read_text(encoding="utf-8")
