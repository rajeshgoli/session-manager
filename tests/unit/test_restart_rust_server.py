"""Tests for scripts/restart-rust-server.sh - sm#1134.

The script restarts the live service, so the properties that matter most are
negative: if anything in phase 1 fails, launchd must not be touched and the
binary launchd is registered against must not be written. These tests drive the
real bash script with cargo/codesign/launchctl/curl and the cutover replaced by
stubs that record every invocation - including what was sitting at the
registered path at the time, which is how the install boundary is asserted.
"""

import os
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "restart-rust-server.sh"

CALLS = "calls.log"
LABEL = "com.example.test"


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

    # The installed binary launchd is registered against, kept deliberately
    # separate from cargo's output just as the real deployment now is.
    installed = _write(tmp_path / "installed" / "sm-server", "ORIGINAL", executable=True)
    cargo_output = tmp_path / "cargo-out" / "sm-server"

    # Defaults; individual tests overwrite these knobs.
    (state / "cargo_rc").write_text("0")
    (state / "codesign_sign_rc").write_text("0")
    (state / "codesign_verify_rc").write_text("0")
    (state / "start_rc").write_text("0")
    (state / "stop_rc").write_text("0")
    (state / "stop_leaves_loaded").write_text("0")
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
    (state / "loaded_labels").write_text(f"{LABEL}\n")

    log = state / CALLS
    # Every stub records what is at the registered path when it runs.
    reg = f'reg="ABSENT"; [[ -f "{installed}" ]] && reg="$(cat "{installed}")"'

    _write(
        bin_dir / "cargo",
        f"""#!/bin/bash
{reg}
echo "cargo $* [registered=$reg]" >> "{log}"
rc="$(cat "{state}/cargo_rc")"
if [[ "$rc" == "0" ]]; then
  mkdir -p "$(dirname "{cargo_output}")"
  printf 'REBUILT' > "{cargo_output}"
  chmod 755 "{cargo_output}"
fi
exit "$rc"
""",
        executable=True,
    )
    _write(
        bin_dir / "codesign",
        f"""#!/bin/bash
{reg}
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
    # lingering Python service and how the script confirms the bootout worked.
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
    # stop-rust unloads the label, start-rust loads it back, mirroring what the
    # real cutover does to the launchd registration.
    cutover = _write(
        tmp_path / "cutover.sh",
        f"""#!/bin/bash
{reg}
echo "cutover $* [registered=$reg]" >> "{log}"
case "$1" in
  render-plist) cat "{state}/rendered_plist"; exit 0 ;;
  stop-rust)
    if [[ "$(cat "{state}/stop_leaves_loaded")" == "0" ]]; then
      grep -vx "{LABEL}" "{state}/loaded_labels" > "{state}/tmp_labels" || true
      mv "{state}/tmp_labels" "{state}/loaded_labels"
    fi
    exit "$(cat "{state}/stop_rc")"
    ;;
  start-rust)
    echo "{LABEL}" >> "{state}/loaded_labels"
    echo after > "{state}/phase"
    exit "$(cat "{state}/start_rc")"
    ;;
esac
exit 0
""",
        executable=True,
    )

    config = _write(tmp_path / "config.yaml", "server:\n  port: 8420\n")
    # Matches the stub's render-plist output, so there is no divergence by default.
    plist = _write(tmp_path / "service.plist", "<plist>canned</plist>\n")

    return {
        "tmp": tmp_path,
        "state": state,
        "log": log,
        "installed": installed,
        "cargo_output": cargo_output,
        "plist": plist,
        "run": _make_runner(bin_dir, cutover, installed, cargo_output, config, plist),
    }


def _make_runner(bin_dir, cutover, installed, cargo_output, config, plist):
    def run(*args, **overrides):
        environ = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "SM_BINARY": str(installed),
            "SM_CARGO_OUTPUT": str(cargo_output),
            "SM_TARGET_DIR": str(cargo_output.parent.parent / "target"),
            "SM_PLIST": str(plist),
            "SM_CUTOVER": str(cutover),
            "SM_CONFIG": str(config),
            "SM_PYTHON_LABELS": "com.example.legacy-python",
            "SM_HOST": "127.0.0.1",
            "SM_PORT": "9",
            "SM_HEALTH_TIMEOUT": "3",
            "SM_PID_SETTLE_SECONDS": "2",
            "SM_UNLOAD_TIMEOUT": "2",
            "SM_LABEL": LABEL,
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


def cutover_line(env, subcommand: str) -> str:
    prefix = f"cutover {subcommand}"
    return next(l for l in calls(env).splitlines() if l.startswith(prefix))


def assert_service_untouched(env):
    """No restart, and no launchd call that could change service state.

    Read-only calls are allowed: the preflight uses `launchctl print` and
    `cutover render-plist` to check preconditions before anything is stopped.
    """
    text = calls(env)
    for mutating in ("restart-rust", "start-rust", "stop-rust"):
        assert f"cutover {mutating}" not in text, f"service was touched:\n{text}"
    for mutating in ("bootout", "bootstrap", "kickstart", "unload", "load"):
        assert f"launchctl {mutating}" not in text, f"launchd was mutated:\n{text}"
    assert env["installed"].read_text() == "ORIGINAL", "the registered binary was written"


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


def test_build_producing_nothing_leaves_service_untouched(env):
    """cargo can exit 0 without leaving an executable where we expect one."""
    cargo = env["tmp"] / "bin" / "cargo"
    cargo.write_text(f'#!/bin/bash\necho "cargo $*" >> "{env["log"]}"\nexit 0\n')
    cargo.chmod(0o755)

    result = env["run"]()

    assert result.returncode != 0
    assert "produced no executable" in result.stderr
    assert_service_untouched(env)


def test_missing_installed_binary_with_skip_build(env):
    env["installed"].unlink()

    result = env["run"]("--skip-build")

    assert result.returncode != 0
    assert "no installed binary" in result.stderr


def test_build_runs_before_anything_stops(env):
    env["run"]()

    text = calls(env)
    assert text.index("cargo build") < text.index("codesign --force")
    assert text.index("codesign --verify") < text.index("cutover stop-rust")


def test_registered_binary_is_never_written_during_phase_one(env):
    """The whole guarantee in one assertion: everything before the bootout sees
    the previously installed build at the registered path."""
    env["run"]()

    lines = calls(env).splitlines()
    before_stop = lines[: next(i for i, l in enumerate(lines) if l.startswith("cutover stop-rust"))]
    recorded = [l for l in before_stop if "[registered=" in l]
    assert recorded, "expected stubs to have recorded the registered path"
    assert all("[registered=ORIGINAL]" in l for l in recorded), recorded


def test_build_refused_while_live_registration_runs_from_cargo_output(env):
    """The state a first adoption starts in: config already points at the
    installed path, but the loaded job still runs from cargo's output. Building
    would overwrite the executable that job is using."""
    env["plist"].write_text(
        f"<plist><array><string>{env['cargo_output']}</string></array></plist>\n"
    )

    result = env["run"]()

    assert result.returncode != 0
    assert "still runs from cargo's output" in result.stderr
    assert "--adopt" in result.stderr
    assert "cargo build" not in calls(env)
    assert_service_untouched(env)


def test_adopt_installs_the_running_build_without_rebuilding(env):
    env["plist"].write_text(
        f"<plist><array><string>{env['cargo_output']}</string></array></plist>\n"
    )
    _write(env["cargo_output"], "ALREADY_RUNNING", executable=True)

    result = env["run"]("--adopt", "--allow-plist-change")

    assert result.returncode == 0, result.stderr
    assert "cargo build" not in calls(env)
    assert env["installed"].read_text() == "ALREADY_RUNNING"
    assert "[registered=ORIGINAL]" in cutover_line(env, "stop-rust")


def test_adopt_and_skip_build_conflict(env):
    result = env["run"]("--adopt", "--skip-build")

    assert result.returncode == 2
    assert "pick one" in result.stderr


def test_adopt_with_nothing_to_adopt(env):
    env["plist"].write_text(
        f"<plist><array><string>{env['cargo_output']}</string></array></plist>\n"
    )

    result = env["run"]("--adopt", "--allow-plist-change")

    assert result.returncode != 0
    assert "nothing to adopt" in result.stderr
    assert_service_untouched(env)


def test_symlink_alias_of_cargo_output_is_rejected(env):
    """A string compare would pass while launchd still execs cargo's artifact."""
    _write(env["cargo_output"], "REBUILT", executable=True)
    alias = env["tmp"] / "alias-sm-server"
    alias.symlink_to(env["cargo_output"])

    result = env["run"](SM_BINARY=str(alias))

    assert result.returncode != 0
    assert "resolves to the same file as cargo's output" in result.stderr
    assert "cargo build" not in calls(env)


def test_lexical_alias_of_cargo_output_is_rejected(env):
    aliased = str(env["cargo_output"].parent / ".." / env["cargo_output"].parent.name
                  / env["cargo_output"].name)

    result = env["run"](SM_BINARY=aliased)

    assert result.returncode != 0
    assert "resolves to the same file as cargo's output" in result.stderr


def test_relative_overrides_resolve_against_the_repo_root(env):
    """The cutover resolves relative paths against its repo root, so we must
    too, or we would install one file and register another."""
    result = env["run"](SM_CONFIG="definitely-not-here.yaml")

    assert result.returncode != 0
    assert f"config not readable: {REPO_ROOT}/definitely-not-here.yaml" in result.stderr
    assert_service_untouched(env)


def test_service_must_not_be_registered_against_cargo_output(env):
    """If they were the same path, a build would write the live binary."""
    same = str(env["cargo_output"])

    result = env["run"](SM_BINARY=same)

    assert result.returncode != 0
    assert "must not be" in result.stderr
    assert "cutover stop-rust" not in calls(env)


# --- signing and the restart path -------------------------------------------


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


# --- the install boundary ---------------------------------------------------


def test_binary_is_installed_only_while_the_job_is_stopped(env):
    result = env["run"]()

    assert result.returncode == 0, result.stderr
    # Still the known-good build when the job is booted out...
    assert "[registered=ORIGINAL]" in cutover_line(env, "stop-rust")
    # ...and only replaced once nothing can exec it.
    assert "[registered=REBUILT]" in cutover_line(env, "start-rust")
    assert env["installed"].read_text() == "REBUILT"


def test_job_still_loaded_after_stop_aborts_before_install(env):
    """The cutover runs `launchctl bootout ... || true` and reports success
    regardless, so a job that refused to unload has to be caught here."""
    (env["state"] / "stop_leaves_loaded").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "still loaded" in result.stderr
    assert env["installed"].read_text() == "ORIGINAL"
    assert "cutover start-rust" not in calls(env)


def test_stop_failure_leaves_the_previous_binary_in_place(env):
    (env["state"] / "stop_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "could not stop" in result.stderr
    assert env["installed"].read_text() == "ORIGINAL"
    assert "cutover start-rust" not in calls(env)


def test_staging_file_is_never_left_behind(env):
    env["run"]()

    assert list((env["installed"].parent).glob("sm-server.staging*")) == []


def test_staging_file_is_cleaned_up_after_failure(env):
    (env["state"] / "codesign_verify_rc").write_text("1")

    env["run"]()

    assert list((env["installed"].parent).glob("sm-server.staging*")) == []


# --- deployment settings ----------------------------------------------------


def test_deployment_overrides_reach_the_cutover(env):
    """The cutover has its own defaults and reads none of our env vars, so a
    non-default deployment must be forwarded or we would sign one service and
    restart another - in the worst case, production."""
    other = env["tmp"] / "other" / "sm-server"
    _write(other, "ORIGINAL", executable=True)
    other_config = _write(env["tmp"] / "other.yaml", "server: {}\n")

    result = env["run"](
        SM_BINARY=str(other),
        SM_CONFIG=str(other_config),
        SM_PORT="10",
    )

    assert result.returncode == 0, result.stderr
    line = cutover_line(env, "start-rust")
    assert f"--binary {other}" in line
    assert f"--config {other_config}" in line
    assert "--port 10" in line
    assert "--host 127.0.0.1" in line


def test_health_check_targets_the_port_handed_to_the_cutover(env):
    env["run"](SM_PORT="10")

    text = calls(env)
    assert "--port 10" in text
    assert "http://127.0.0.1:10/health" in text


def test_plist_path_is_forwarded_to_the_cutover(env):
    env["run"]()

    line = cutover_line(env, "start-rust")
    assert f"--plist {env['plist']}" in line
    # The cutover recomputes the plist path from --label, so order matters.
    assert line.index("--label") < line.index("--plist")


def test_local_env_is_forwarded_to_the_cutover(env):
    overlay = _write(env["tmp"] / "local.env", "SECRET=1\n")

    result = env["run"](SM_LOCAL_ENV=str(overlay))

    assert result.returncode == 0, result.stderr
    assert f"--local-env {overlay}" in cutover_line(env, "start-rust")


def test_unreadable_local_env_blocks_before_any_restart(env):
    result = env["run"](SM_LOCAL_ENV=str(env["tmp"] / "missing.env"))

    assert result.returncode != 0
    assert "local env overlay not readable" in result.stderr
    assert_service_untouched(env)


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


def test_missing_plist_is_not_a_divergence(env):
    """A first-time bootstrap has no plist to compare against."""
    env["plist"].unlink()

    result = env["run"]()

    assert result.returncode == 0, result.stderr


def test_cargo_target_dir_is_pinned(env):
    """A redirected target dir would leave us installing a stale artifact."""
    result = env["run"](CARGO_TARGET_DIR=str(env["tmp"] / "redirected"))

    assert result.returncode == 0, result.stderr
    line = next(l for l in calls(env).splitlines() if l.startswith("cargo build"))
    assert "--target-dir" in line
    assert str(env["tmp"] / "redirected") not in line


# --- preconditions ----------------------------------------------------------


def test_lingering_python_label_blocks_before_anything_stops(env):
    """start-rust refuses to start alongside Python, but it stops the service
    first - so that precondition has to be caught in the preflight."""
    (env["state"] / "loaded_labels").write_text(f"{LABEL}\ncom.example.legacy-python\n")

    result = env["run"]()

    assert result.returncode != 0
    assert "com.example.legacy-python is still loaded" in result.stderr
    assert_service_untouched(env)


def test_unreadable_config_leaves_service_untouched(env):
    result = env["run"](SM_CONFIG=str(env["tmp"] / "missing.yaml"))

    assert result.returncode != 0
    assert "config not readable" in result.stderr
    assert_service_untouched(env)


def test_missing_cutover_blocks_before_the_binary_is_touched(env):
    """With no live plist the render check is skipped, so a missing cutover has
    to be caught in the preflight rather than later."""
    env["plist"].unlink()

    result = env["run"](SM_CUTOVER=str(env["tmp"] / "gone.sh"))

    assert result.returncode != 0
    assert "cutover script not executable" in result.stderr
    assert "cargo build" not in calls(env)
    assert_service_untouched(env)


def test_rejects_bad_allow_drop(env):
    result = env["run"]("--allow-drop", "-1")

    assert result.returncode == 2
    assert "non-negative integer" in result.stderr
    assert_service_untouched(env)


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


def test_start_failure_is_reported(env):
    (env["state"] / "start_rc").write_text("1")

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


def test_skip_build_reinstalls_the_installed_binary(env):
    result = env["run"]("--skip-build")

    assert result.returncode == 0, result.stderr
    text = calls(env)
    assert "cargo build" not in text
    assert "codesign --force" in text
    assert env["installed"].read_text() == "ORIGINAL"
