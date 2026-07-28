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
    (state / "stop_rc").write_text("0")
    (state / "phase").write_text("before")
    (state / "before_health").write_text("0")  # 0 == healthy (curl exit code)
    (state / "after_health").write_text("0")
    (state / "before_sessions").write_text("12")
    (state / "after_sessions").write_text("12")
    (state / "before_sessions_rc").write_text("0")
    (state / "after_sessions_rc").write_text("0")
    (state / "rendered_plist").write_text("<plist>canned</plist>\n")
    (state / "pids").write_text("4242")  # whitespace-separated; last value repeats
    (state / "pid_index").write_text("0")
    (state / "job_state").write_text("running")

    log = state / CALLS

    # A real build replaces the registered binary, which is what makes a later
    # phase-1 failure dangerous; the stub reproduces that.
    _write(
        bin_dir / "cargo",
        f"""#!/bin/bash
echo "cargo $*" >> "{log}"
rc="$(cat "{state}/cargo_rc")"
if [[ "$rc" == "0" ]]; then
  printf 'REBUILT' > "{tmp_path}/sm-server"
  chmod 755 "{tmp_path}/sm-server"
fi
exit "$rc"
""",
        executable=True,
    )
    # Both stubs record what is sitting at the registered path when they run, so
    # tests can assert the live path only ever holds a binary launchd's current
    # registration already accepted.
    _write(
        bin_dir / "codesign",
        f"""#!/bin/bash
reg="ABSENT"; [[ -f "{tmp_path}/sm-server" ]] && reg="$(cat "{tmp_path}/sm-server")"
echo "codesign $* [registered=$reg]" >> "{log}"
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
    rc="$(cat "{state}/${{phase}}_sessions_rc")"
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
        f"""#!/bin/bash
reg="ABSENT"; [[ -f "{tmp_path}/sm-server" ]] && reg="$(cat "{tmp_path}/sm-server")"
echo "cutover $* [registered=$reg]" >> "{log}"
case "$1" in
  render-plist) cat "{state}/rendered_plist"; exit 0 ;;
  stop-rust) exit "$(cat "{state}/stop_rc")" ;;
  start-rust) echo after > "{state}/phase"; exit "$(cat "{state}/cutover_rc")" ;;
esac
exit 0
""",
        executable=True,
    )

    binary = _write(tmp_path / "sm-server", "ORIGINAL", executable=True)
    config = _write(tmp_path / "config.yaml", "server:\n  port: 8420\n")
    # Matches the stub's render-plist output, so there is no divergence by default.
    plist = _write(tmp_path / "service.plist", "<plist>canned</plist>\n")

    return {
        "tmp": tmp_path,
        "state": state,
        "log": log,
        "binary": binary,
        "plist": plist,
        "run": _make_runner(bin_dir, cutover, binary, config, plist),
    }


def _make_runner(bin_dir: Path, cutover: Path, binary: Path, config: Path, plist: Path):
    def run(*args, **overrides):
        environ = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "SM_BINARY": str(binary),
            "SM_CARGO_OUTPUT": str(binary),
            "SM_TARGET_DIR": str(binary.parent / "target"),
            "SM_PLIST": str(plist),
            "SM_CUTOVER": str(cutover),
            "SM_CONFIG": str(config),
            "SM_PYTHON_LABELS": "com.example.legacy-python",
            "SM_HOST": "127.0.0.1",
            "SM_PORT": "9",
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

    Read-only calls are allowed: the preflight uses `launchctl print` and
    `cutover render-plist` to check preconditions before anything is stopped.
    """
    text = calls(env)
    assert "cutover restart-rust" not in text, f"restart was attempted:\n{text}"
    assert "cutover start-rust" not in text, f"restart was attempted:\n{text}"
    assert "cutover stop-rust" not in text, f"service was stopped:\n{text}"
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
    missing = str(env["tmp"] / "nope")

    result = env["run"](SM_BINARY=missing, SM_CARGO_OUTPUT=missing)

    assert result.returncode != 0
    assert "binary not found" in result.stderr
    assert_service_untouched(env)


def test_binary_is_rolled_back_when_signing_fails(env):
    """A build replaces the registered executable while the old process runs on.
    If a later phase-1 step fails, the on-disk binary must go back, or the next
    KeepAlive respawn boots a build the live registration never accepted."""
    (env["state"] / "codesign_sign_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert env["binary"].read_text() == "ORIGINAL"
    assert "rolled back" in result.stderr
    assert_service_untouched(env)


def test_binary_is_rolled_back_when_verification_fails(env):
    (env["state"] / "codesign_verify_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert env["binary"].read_text() == "ORIGINAL"


def test_successful_run_keeps_the_new_binary(env):
    result = env["run"]()

    assert result.returncode == 0, result.stderr
    assert env["binary"].read_text() == "REBUILT"


def test_new_binary_is_removed_when_none_existed_before(env):
    """After a `cargo clean` the registered path is empty while launchd's process
    runs on from its open inode. A build then creates a binary, and if phase 1
    fails it must not be left there for the next respawn to execute."""
    env["binary"].unlink()
    (env["state"] / "codesign_sign_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert not env["binary"].exists()
    assert_service_untouched(env)


def test_new_binary_is_kept_when_none_existed_and_the_run_succeeds(env):
    env["binary"].unlink()

    result = env["run"]()

    assert result.returncode == 0, result.stderr
    assert env["binary"].read_text() == "REBUILT"


def test_cargo_target_dir_is_pinned(env):
    """A redirected target dir would put the new build elsewhere while we signed
    and restarted a stale binary at the expected path."""
    result = env["run"](CARGO_TARGET_DIR=str(env["tmp"] / "redirected"))

    assert result.returncode == 0, result.stderr
    line = next(l for l in calls(env).splitlines() if l.startswith("cargo build"))
    assert f"--target-dir {env['tmp'] / 'target'}" in line
    assert str(env["tmp"] / "redirected") not in line


def test_binary_is_installed_only_while_the_job_is_stopped(env):
    """The registered path must never hold an unverified build while the job is
    still registered: a server that exited in that window would be respawned by
    KeepAlive onto a binary the old registration may reject."""
    result = env["run"]()

    assert result.returncode == 0, result.stderr
    lines = calls(env).splitlines()
    stop = next(l for l in lines if l.startswith("cutover stop-rust"))
    start = next(l for l in lines if l.startswith("cutover start-rust"))
    # Still the known-good build when the job is booted out...
    assert "[registered=ORIGINAL]" in stop
    # ...and only replaced once nothing can exec it.
    assert "[registered=REBUILT]" in start


def test_signing_happens_off_the_registered_path(env):
    """Signing and verification run against the staged copy, so the live path
    keeps the binary launchd's current registration already accepted."""
    env["run"]()

    signing = [l for l in calls(env).splitlines() if l.startswith("codesign --force")]
    assert signing, "expected a signing call"
    assert all("[registered=ORIGINAL]" in l for l in signing), signing


def test_stop_failure_leaves_the_previous_binary_in_place(env):
    (env["state"] / "stop_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "could not stop" in result.stderr
    assert env["binary"].read_text() == "ORIGINAL"
    assert "cutover start-rust" not in calls(env)


def test_staging_file_is_never_left_behind(env):
    env["run"]()

    assert list(env["tmp"].glob("sm-server.staging*")) == []


def test_no_backup_file_is_left_behind(env):
    env["run"]()

    leftovers = list(env["tmp"].glob("sm-server.restart-backup*"))
    assert leftovers == []


def test_cargo_output_mismatch_blocks_before_any_restart(env):
    """cargo builds its own path; signing a different SM_BINARY would deploy
    stale code while claiming a fresh build."""
    other = _write(env["tmp"] / "elsewhere", "STALE", executable=True)

    result = env["run"](SM_BINARY=str(other))

    assert result.returncode != 0
    assert "cargo builds" in result.stderr
    assert other.read_text() == "STALE"
    assert_service_untouched(env)


def test_cargo_output_mismatch_is_allowed_with_skip_build(env):
    other = _write(env["tmp"] / "elsewhere", "PREBUILT", executable=True)

    result = env["run"]("--skip-build", SM_BINARY=str(other))

    assert result.returncode == 0, result.stderr


# --- deployment settings that a plist rewrite would drop --------------------


def test_plist_divergence_blocks_before_any_restart(env):
    """Restarting rewrites the plist; anything we would not regenerate is a
    setting about to be silently dropped."""
    env["plist"].write_text("<plist>has --local-env with secrets</plist>\n")

    result = env["run"]()

    assert result.returncode != 0
    assert "would rewrite" in result.stderr
    assert "live plist vs what the restart would write" in result.stderr
    assert_service_untouched(env)


def test_allow_plist_change_proceeds_with_a_warning(env):
    env["plist"].write_text("<plist>different</plist>\n")

    result = env["run"]("--allow-plist-change")

    assert result.returncode == 0, result.stderr
    assert "WARNING" in result.stderr


def test_local_env_is_forwarded_to_the_cutover(env):
    overlay = _write(env["tmp"] / "local.env", "SECRET=1\n")

    result = env["run"](SM_LOCAL_ENV=str(overlay))

    assert result.returncode == 0, result.stderr
    line = next(l for l in calls(env).splitlines() if l.startswith("cutover start-rust"))
    assert f"--local-env {overlay}" in line


def test_unreadable_local_env_blocks_before_any_restart(env):
    result = env["run"](SM_LOCAL_ENV=str(env["tmp"] / "missing.env"))

    assert result.returncode != 0
    assert "local env overlay not readable" in result.stderr
    assert_service_untouched(env)


def test_plist_path_is_forwarded_to_the_cutover(env):
    """Otherwise the preflight compares one plist while the cutover writes and
    bootstraps a different (default) one."""
    env["run"]()

    line = next(l for l in calls(env).splitlines() if l.startswith("cutover start-rust"))
    assert f"--plist {env['plist']}" in line
    # The cutover recomputes the plist path from --label, so order matters.
    assert line.index("--label") < line.index("--plist")


def test_missing_cutover_blocks_before_the_binary_is_touched(env):
    """With no live plist the render check is skipped, so a missing cutover has
    to be caught in the preflight rather than after the rebuild."""
    env["plist"].unlink()

    result = env["run"](SM_CUTOVER=str(env["tmp"] / "gone.sh"))

    assert result.returncode != 0
    assert "cutover script not executable" in result.stderr
    assert env["binary"].read_text() == "ORIGINAL"
    assert "cargo build" not in calls(env)
    assert_service_untouched(env)


def test_missing_plist_is_not_a_divergence(env):
    """A first-time bootstrap has no plist to compare against."""
    env["plist"].unlink()

    result = env["run"]()

    assert result.returncode == 0, result.stderr


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
    assert text.index("codesign --verify") < text.index("cutover stop-rust")


# --- signing behaviour ------------------------------------------------------


def test_signs_with_a_stable_identifier(env):
    env["run"]()

    assert "--identifier com.rajeshgoli.sm-server" in calls(env)


def test_restart_goes_through_the_cutover_script(env):
    env["run"]()

    text = calls(env)
    # restart-rust is exactly stop-rust then start-rust; the install goes between.
    assert "cutover stop-rust" in text
    assert "cutover start-rust" in text
    # The stale-constraint bug is exactly what a bare kickstart cannot fix.
    assert "launchctl kickstart" not in text
    assert "launchctl bootout" not in text


def test_deployment_overrides_reach_the_cutover(env):
    """The cutover has its own defaults and reads none of our env vars, so a
    non-default deployment must be forwarded or we would sign one service and
    restart another - in the worst case, production."""
    other = _write(env["tmp"] / "other-sm-server", "#!/bin/bash\ntrue\n", executable=True)
    other_config = _write(env["tmp"] / "other.yaml", "server: {}\n")
    (env["state"] / "loaded_labels").write_text("com.example.other\n")

    result = env["run"](
        SM_LABEL="com.example.other",
        SM_BINARY=str(other),
        SM_CARGO_OUTPUT=str(other),
        SM_CONFIG=str(other_config),
        SM_PORT="10",
    )

    assert result.returncode == 0, result.stderr
    line = next(l for l in calls(env).splitlines() if l.startswith("cutover "))
    assert "--label com.example.other" in line
    assert f"--binary {other}" in line
    assert f"--config {other_config}" in line
    assert "--port 10" in line
    assert "--host 127.0.0.1" in line


def test_health_check_targets_the_port_handed_to_the_cutover(env):
    """The polled endpoint must be the one the service was told to listen on."""
    env["run"](SM_PORT="10")

    text = calls(env)
    assert "--port 10" in text
    assert "http://127.0.0.1:10/health" in text


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
    assert "start failed" in result.stderr


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


def test_healthy_server_with_unreadable_session_list_aborts(env):
    """Otherwise `|| true` turns a transient /sessions failure into an empty
    baseline, and the post-restart comparison is silently skipped."""
    (env["state"] / "before_sessions_rc").write_text("7")

    result = env["run"]()

    assert result.returncode != 0
    assert "refusing to" in result.stderr
    assert_service_untouched(env)


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
