import plistlib
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "rust-service-cutover.sh"


def run_script(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(SCRIPT), *args],
        cwd=REPO_ROOT,
        text=True,
        capture_output=True,
        check=False,
    )


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


def test_rust_service_cutover_persistently_disables_python_and_reenables_on_rollback():
    script = SCRIPT.read_text(encoding="utf-8")

    assert 'launchctl disable "$DOMAIN/$label"; then' in script
    assert 'is_label_disabled "$label"' in script
    assert 'failed to verify disabled override for $label' in script
    assert 'echo "disabled and stopped $label"' in script
    assert 'require_no_python_labels' in script
    assert 'launchctl enable "$DOMAIN/$label"' in script
