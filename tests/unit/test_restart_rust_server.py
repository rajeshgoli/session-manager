"""Tests for scripts/restart-rust-server.sh - sm#1134.

The script restarts the live service, so the property that matters most is
negative: if the build or the signature fails, nothing may touch launchd. These
tests drive the real bash script with cargo/codesign/launchctl/curl replaced by
PATH stubs that record every invocation.
"""

import os
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "restart-rust-server.sh"

CALLS = "calls.log"


def _write(path: Path, body: str, executable: bool = False) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body)
    if executable:
        path.chmod(0o755)
    return path


@pytest.fixture
def env(tmp_path):
    """A sandbox with stubbed cargo, codesign, launchctl, curl, and cutover."""
    bin_dir = tmp_path / "bin"
    state = tmp_path / "state"
    state.mkdir(parents=True, exist_ok=True)

    # Defaults; individual tests overwrite these knobs.
    (state / "cargo_rc").write_text("0")
    (state / "codesign_sign_rc").write_text("0")
    (state / "codesign_verify_rc").write_text("0")
    (state / "cutover_rc").write_text("0")
    (state / "phase").write_text("before")
    (state / "before_health").write_text("0")  # 0 == healthy (curl exit code)
    (state / "after_health").write_text("0")
    (state / "before_sessions").write_text("12")
    (state / "after_sessions").write_text("12")
    (state / "pids").write_text("4242")  # whitespace-separated; last value repeats
    (state / "pid_index").write_text("0")
    (state / "job_state").write_text("running")

    log = state / CALLS

    _write(
        bin_dir / "cargo",
        f'#!/bin/bash\necho "cargo $*" >> "{log}"\nexit "$(cat "{state}/cargo_rc")"\n',
        executable=True,
    )
    _write(
        bin_dir / "codesign",
        f"""#!/bin/bash
echo "codesign $*" >> "{log}"
for a in "$@"; do
  case "$a" in
    --verify) exit "$(cat "{state}/codesign_verify_rc")" ;;
    -dvvv) echo "Identifier=com.rajeshgoli.sm-server" >&2; exit 0 ;;
  esac
done
exit "$(cat "{state}/codesign_sign_rc")"
""",
        executable=True,
    )
    # `launchctl print` is the only subcommand the script uses. It exits nonzero
    # for a label that is not loaded, which is how the preflight detects a
    # lingering Python service.
    (state / "loaded_labels").write_text("com.example.test\n")
    _write(
        bin_dir / "launchctl",
        f"""#!/bin/bash
echo "launchctl $*" >> "{log}"
if [[ "$1" == "print" ]]; then
  label="${{2##*/}}"
  grep -qx "$label" "{state}/loaded_labels" || exit 1
  idx="$(cat "{state}/pid_index")"
  read -r -a pids <<< "$(cat "{state}/pids")"
  last=$(( ${{#pids[@]}} - 1 ))
  (( idx > last )) && idx=$last
  echo "	state = $(cat "{state}/job_state")"
  echo "	pid = ${{pids[$idx]}}"
  echo "		state = active"
  echo "$(( idx + 1 ))" > "{state}/pid_index"
fi
exit 0
""",
        executable=True,
    )
    _write(
        bin_dir / "curl",
        f"""#!/bin/bash
url="${{@: -1}}"
phase="$(cat "{state}/phase")"
echo "curl $url ($phase)" >> "{log}"
case "$url" in
  */health) exit "$(cat "{state}/${{phase}}_health")" ;;
  */sessions)
    rc="$(cat "{state}/${{phase}}_health")"
    [[ "$rc" != "0" ]] && exit "$rc"
    n="$(cat "{state}/${{phase}}_sessions")"
    printf '{{"sessions":['
    for ((i = 0; i < n; i++)); do
      [[ $i -gt 0 ]] && printf ','
      printf '{{"id":"s%s"}}' "$i"
    done
    printf ']}}'
    ;;
esac
exit 0
""",
        executable=True,
    )
    cutover = _write(
        tmp_path / "cutover.sh",
        f'#!/bin/bash\necho "cutover $*" >> "{log}"\necho after > "{state}/phase"\n'
        f'exit "$(cat "{state}/cutover_rc")"\n',
        executable=True,
    )

    binary = _write(tmp_path / "sm-server", "#!/bin/bash\ntrue\n", executable=True)
    config = _write(tmp_path / "config.yaml", "server:\n  port: 8420\n")

    return {
        "tmp": tmp_path,
        "state": state,
        "log": log,
        "run": _make_runner(bin_dir, cutover, binary, config),
    }


def _make_runner(bin_dir: Path, cutover: Path, binary: Path, config: Path):
    def run(*args, **overrides):
        environ = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "SM_BINARY": str(binary),
            "SM_CUTOVER": str(cutover),
            "SM_CONFIG": str(config),
            "SM_PYTHON_LABELS": "com.example.legacy-python",
            "SM_BASE_URL": "http://127.0.0.1:9",
            "SM_HEALTH_TIMEOUT": "3",
            "SM_PID_SETTLE_SECONDS": "2",
            "SM_LABEL": "com.example.test",
        }
        environ.update(overrides)
        return subprocess.run(
            ["bash", str(SCRIPT), *args],
            env=environ,
            capture_output=True,
            text=True,
        )

    return run

def calls(env) -> str:
    return env["log"].read_text() if env["log"].exists() else ""


def assert_service_untouched(env):
    """No restart, and no launchd call that could change service state.

    Read-only `launchctl print` is allowed: the preflight uses it to check
    preconditions before anything is stopped.
    """
    text = calls(env)
    assert "cutover" not in text, f"restart was attempted:\n{text}"
    for mutating in ("bootout", "bootstrap", "kickstart", "unload", "load"):
        assert f"launchctl {mutating}" not in text, f"launchd was mutated:\n{text}"


# --- the ordering guarantee -------------------------------------------------


def test_build_failure_leaves_service_untouched(env):
    (env["state"] / "cargo_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "build failed" in result.stderr
    assert_service_untouched(env)


def test_sign_failure_leaves_service_untouched(env):
    (env["state"] / "codesign_sign_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "codesign failed" in result.stderr
    assert_service_untouched(env)


def test_verify_failure_leaves_service_untouched(env):
    (env["state"] / "codesign_verify_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "signature verification failed" in result.stderr
    assert_service_untouched(env)


def test_missing_binary_leaves_service_untouched(env):
    result = env["run"](SM_BINARY=str(env["tmp"] / "nope"))

    assert result.returncode != 0
    assert "binary not found" in result.stderr
    assert_service_untouched(env)


def test_lingering_python_label_blocks_before_anything_stops(env):
    """start-rust refuses to start alongside Python, but restart-rust stops the
    service first - so that precondition has to be caught in the preflight."""
    (env["state"] / "loaded_labels").write_text(
        "com.example.test\ncom.example.legacy-python\n"
    )

    result = env["run"]()

    assert result.returncode != 0
    assert "com.example.legacy-python is still loaded" in result.stderr
    assert_service_untouched(env)


def test_unreadable_config_leaves_service_untouched(env):
    result = env["run"](SM_CONFIG=str(env["tmp"] / "missing.yaml"))

    assert result.returncode != 0
    assert "config not readable" in result.stderr
    assert_service_untouched(env)


def test_build_runs_before_any_restart(env):
    env["run"]()

    text = calls(env)
    assert text.index("cargo build") < text.index("codesign --force")
    assert text.index("codesign --verify") < text.index("cutover restart-rust")


# --- signing behaviour ------------------------------------------------------


def test_signs_with_a_stable_identifier(env):
    env["run"]()

    assert "--identifier com.rajeshgoli.sm-server" in calls(env)


def test_restart_goes_through_the_cutover_script(env):
    env["run"]()

    text = calls(env)
    assert "cutover restart-rust" in text
    # The stale-constraint bug is exactly what a bare kickstart cannot fix.
    assert "launchctl kickstart" not in text
    assert "launchctl bootout" not in text


# --- post-restart verification ---------------------------------------------


def test_happy_path_succeeds(env):
    result = env["run"]()

    assert result.returncode == 0, result.stderr
    assert "session count ok (12 -> 12)" in result.stdout
    assert "pid 4242 stable" in result.stdout


def test_unhealthy_after_restart_fails(env):
    (env["state"] / "after_health").write_text("7")

    result = env["run"]()

    assert result.returncode != 0
    assert "did not become healthy" in result.stderr


def test_cutover_failure_is_reported(env):
    (env["state"] / "cutover_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "restart failed" in result.stderr


def test_pid_churn_is_detected_as_a_crash_loop(env):
    (env["state"] / "pids").write_text("100 200 300")

    result = env["run"]()

    assert result.returncode != 0
    assert "crash loop" in result.stderr


def test_not_running_state_is_detected(env):
    (env["state"] / "job_state").write_text("not running")

    result = env["run"]()

    assert result.returncode != 0
    assert "expected 'running'" in result.stderr


def test_session_drop_fails(env):
    (env["state"] / "after_sessions").write_text("9")

    result = env["run"]()

    assert result.returncode != 0
    assert "session count dropped 12 -> 9" in result.stderr


def test_allow_drop_tolerates_expected_churn(env):
    (env["state"] / "after_sessions").write_text("11")

    result = env["run"]("--allow-drop", "1")

    assert result.returncode == 0, result.stderr


def test_session_growth_is_not_a_failure(env):
    (env["state"] / "after_sessions").write_text("15")

    result = env["run"]()

    assert result.returncode == 0, result.stderr


def test_recovery_restart_when_server_was_down(env):
    """A down server has no before-count; that must not block recovery."""
    (env["state"] / "before_health").write_text("7")

    result = env["run"]()

    assert result.returncode == 0, result.stderr
    assert "recovery restart" in result.stdout
    assert "skipping comparison" in result.stdout


def test_skip_build_still_signs(env):
    result = env["run"]("--skip-build")

    assert result.returncode == 0, result.stderr
    text = calls(env)
    assert "cargo build" not in text
    assert "codesign --force" in text


def test_rejects_bad_allow_drop(env):
    result = env["run"]("--allow-drop", "-1")

    assert result.returncode == 2
    assert "non-negative integer" in result.stderr
    assert_service_untouched(env)
