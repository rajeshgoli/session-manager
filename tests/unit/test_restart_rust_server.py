"""Tests for scripts/restart-rust-server.sh - sm#1134.

The script restarts the live service, so the properties that matter most are
negative: if anything in phase 1 fails, launchd must not be touched and the
binary launchd is registered against must not be written. These tests drive the
real bash script with cargo/codesign/launchctl/curl and the cutover replaced by
stubs that record every invocation - including what was sitting at the
registered path at the time, which is how the install boundary is asserted.
"""

import os
import socket
import subprocess
import uuid
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "restart-rust-server.sh"

CALLS = "calls.log"
LABEL = "com.example.test"
SIGN_IDENTITY = "36FC54A873D584A34FCFEEA7D1F519B19A39DE72"
SIGN_REQUIREMENT = (
    'designated => identifier "com.rajeshgoli.sm-server" '
    f'and certificate root = H"{SIGN_IDENTITY.lower()}"'
)


def _write(path: Path, body: str, executable: bool = False) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body)
    if executable:
        path.chmod(0o755)
    return path


def fake_binary(state: Path, marker: str) -> str:
    """A runnable stand-in for sm-server.

    The marker line is how tests tell which build is sitting at a given path.
    --help advertises --check-config only when the knob says so, so the
    older-build path can be exercised too.
    """
    return f"""#!/bin/bash
# MARKER={marker}
echo "sm-server $*" >> "{state}/{CALLS}"
if [[ "$1" == "--help" ]]; then
  echo "Usage: sm-server [OPTIONS]"
  if [[ "$(cat "{state}/supports_check_config")" == "1" ]]; then
    echo "      --check-config"
  fi
  exit 0
fi
if [[ "$1" == "--check-config" ]]; then
  exit "$(cat "{state}/check_config_rc")"
fi
exit 0
"""


def _fake(env, marker: str) -> str:
    return fake_binary(env["state"], marker)


@pytest.fixture
def env(tmp_path, request):
    """A sandbox with stubbed cargo, codesign, launchctl, curl, and cutover."""
    bin_dir = tmp_path / "bin"
    state = tmp_path / "state"
    state.mkdir(parents=True, exist_ok=True)

    # The installed binary launchd is registered against, kept deliberately
    # separate from cargo's output just as the real deployment now is. These have
    # to be runnable: the script execs them to validate the configuration.
    installed = _write(
        tmp_path / "installed" / "sm-server", fake_binary(state, "ORIGINAL"), executable=True
    )
    cargo_output = tmp_path / "cargo-out" / "sm-server"

    # Defaults; individual tests overwrite these knobs.
    (state / "cargo_rc").write_text("0")
    (state / "codesign_sign_rc").write_text("0")
    (state / "codesign_verify_rc").write_text("0")
    (state / "codesign_inspect_rc").write_text("0")
    (state / "codesign_requirement_rc").write_text("0")
    (state / "signing_identity_present").write_text("1")
    (state / "available_signing_identity").write_text(SIGN_IDENTITY)
    (state / "signed_identifier").write_text("com.rajeshgoli.sm-server")
    (state / "signed_signature").write_text("")
    (state / "signed_authority").write_text("Office Automate Local Signing")
    (state / "requirement_identity").write_text(SIGN_IDENTITY.lower())
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
    (state / "job_program").write_text(str(installed))
    (state / "supports_check_config").write_text("1")
    (state / "check_config_rc").write_text("0")
    (state / "authority_rc").write_text("0")
    (state / "authority_payload").write_text(
        '{"schema":"sm.queue_authority.response.v1","ok":false,'
        '"job":null,"error":{"code": "not_found"}}\n'
    )
    # What a successful `cargo build` drops at the cargo output path.
    _write(state / "rebuilt-binary", fake_binary(state, "REBUILT"))

    log = state / CALLS
    # Every stub records which build is at the registered path when it runs.
    reg = (
        f'reg="ABSENT"; [[ -f "{installed}" ]] '
        f"""&& reg="$(sed -n 's/^# MARKER=//p' "{installed}")\""""
    )

    _write(
        bin_dir / "cargo",
        f"""#!/bin/bash
{reg}
echo "cargo $* [registered=$reg]" >> "{log}"
rc="$(cat "{state}/cargo_rc")"
if [[ "$rc" == "0" ]]; then
  mkdir -p "$(dirname "{cargo_output}")"
  cp "{state}/rebuilt-binary" "{cargo_output}"
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
    -dvvv)
      echo "Identifier=$(cat "{state}/signed_identifier")" >&2
      signature="$(cat "{state}/signed_signature")"
      [[ -n "$signature" ]] && echo "Signature=$signature" >&2
      authority="$(cat "{state}/signed_authority")"
      [[ -n "$authority" ]] && echo "Authority=$authority" >&2
      exit "$(cat "{state}/codesign_inspect_rc")"
      ;;
    -dr)
      requirement="designated => identifier \\\"$(cat "{state}/signed_identifier")\\\" and certificate root = H\\\"$(cat "{state}/requirement_identity")\\\""
      echo "$requirement" >> "{state}/requirements.log"
      echo "$requirement" >&2
      exit "$(cat "{state}/codesign_requirement_rc")"
      ;;
  esac
done
exit "$(cat "{state}/codesign_sign_rc")"
""",
        executable=True,
    )
    _write(
        bin_dir / "security",
        f"""#!/bin/bash
echo "security $*" >> "{log}"
if [[ "$(cat "{state}/signing_identity_present")" == "1" ]]; then
  echo "  1) $(cat "{state}/available_signing_identity") \"Office Automate Local Signing\""
  echo '     1 valid identities found'
else
  echo '     0 valid identities found'
fi
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
  echo "	program = $(cat "{state}/job_program")"
  echo "	pid = ${{pids[$idx]}}"
  echo "		state = active"
  # Only walk the pid list once the service has been restarted, so that
  # preflight reads do not consume the sequence a crash loop is modelled with.
  if [[ "$(cat "{state}/phase")" == "after" ]]; then
    echo "$(( idx + 1 ))" > "{state}/pid_index"
  fi
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
    authority_verifier = _write(
        bin_dir / "verify-queue-authority",
        f"""#!/usr/bin/env python3
import pathlib
import json
import sys

state = pathlib.Path({str(state)!r})
with pathlib.Path({str(log)!r}).open("a") as handle:
    handle.write("verify-queue-authority " + " ".join(sys.argv[1:]) + "\\n")
payload_text = (state / "authority_payload").read_text()
sys.stdout.write(payload_text)
rc = int((state / "authority_rc").read_text())
if rc:
    raise SystemExit(rc)
if "--expect-not-found" in sys.argv:
    payload = json.loads(payload_text)
    error = payload.get("error")
    if not (
        payload.get("ok") is False
        and payload.get("job") is None
        and isinstance(error, dict)
        and error.get("code") == "not_found"
    ):
        raise SystemExit(3)
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
    signing_config = _write(
        tmp_path / "rust-server-signing.env",
        f"SM_SIGN_IDENTITY={SIGN_IDENTITY}\n"
        f"SM_SIGN_DESIGNATED_REQUIREMENT={SIGN_REQUIREMENT}\n",
    )
    # Matches the stub's render-plist output, so there is no divergence by default.
    plist = _write(tmp_path / "service.plist", "<plist>canned</plist>\n")
    authority_socket_path = Path("/tmp") / f"sm-rst-{uuid.uuid4().hex}.sock"
    authority_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    authority_socket.bind(str(authority_socket_path))

    def cleanup_authority_socket():
        authority_socket.close()
        authority_socket_path.unlink(missing_ok=True)

    request.addfinalizer(cleanup_authority_socket)

    return {
        "tmp": tmp_path,
        "state": state,
        "log": log,
        "installed": installed,
        "cargo_output": cargo_output,
        "signing_config": signing_config,
        "plist": plist,
        "authority_socket": authority_socket,
        "authority_socket_path": authority_socket_path,
        "authority_verifier": authority_verifier,
        "lock": installed.parent / "restart.lock",
        "run": _make_runner(
            bin_dir,
            cutover,
            installed,
            cargo_output,
            config,
            signing_config,
            plist,
            authority_socket_path,
            authority_verifier,
        ),
    }


def _make_runner(
    bin_dir,
    cutover,
    installed,
    cargo_output,
    config,
    signing_config,
    plist,
    authority_socket_path,
    authority_verifier,
):
    binary_dir_lock = installed.parent / "restart.lock"
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
            "SM_SIGNING_CONFIG": str(signing_config),
            "SM_PYTHON_LABELS": "com.example.legacy-python",  # extra, on top of the enforced set
            "SM_HOST": "127.0.0.1",
            "SM_PORT": "9",
            "SM_HEALTH_TIMEOUT": "3",
            "SM_PID_SETTLE_SECONDS": "2",
            "SM_UNLOAD_TIMEOUT": "2",
            "SM_LABEL": LABEL,
            "SM_SIGN_IDENTITY": SIGN_IDENTITY,
            "SM_LOCK": str(binary_dir_lock),
            "SM_QUEUE_AUTHORITY_SOCKET": str(authority_socket_path),
            "SM_QUEUE_AUTHORITY_VERIFIER": str(authority_verifier),
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
    assert "MARKER=ORIGINAL" in env["installed"].read_text(), "the registered binary was written"


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
    assert "codesign with persistent identity" in result.stderr
    assert_service_untouched(env)


def test_verify_failure_leaves_service_untouched(env):
    (env["state"] / "codesign_verify_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "signature verification failed" in result.stderr
    assert_service_untouched(env)


def test_missing_signing_identity_blocks_before_service_mutation(env):
    env["signing_config"].write_text("# identity deliberately missing\n")
    result = env["run"](SM_SIGN_IDENTITY="")

    assert result.returncode != 0
    assert "signing config must contain exactly one SM_SIGN_IDENTITY" in result.stderr
    assert "cargo build" not in calls(env)
    assert_service_untouched(env)


def test_unusable_keychain_identity_blocks_before_service_mutation(env):
    (env["state"] / "signing_identity_present").write_text("0")

    result = env["run"]()

    assert result.returncode != 0
    assert "not a valid usable codesigning identity" in result.stderr
    assert "cargo build" not in calls(env)
    assert_service_untouched(env)


def test_ad_hoc_staged_output_blocks_before_service_mutation(env):
    (env["state"] / "signed_signature").write_text("adhoc")

    result = env["run"]()

    assert result.returncode != 0
    assert "staged signature is ad-hoc" in result.stderr
    assert_service_untouched(env)


def test_wrong_staged_identifier_blocks_before_service_mutation(env):
    (env["state"] / "signed_identifier").write_text("com.example.wrong")

    result = env["run"]()

    assert result.returncode != 0
    assert "staged signature identifier is not" in result.stderr
    assert_service_untouched(env)


def test_wrong_staged_certificate_authority_blocks_before_service_mutation(env):
    (env["state"] / "requirement_identity").write_text("0" * 40)

    result = env["run"]()

    assert result.returncode != 0
    assert "stable configured certificate-anchored requirement" in result.stderr
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
    """The state a first adoption starts in. With the job not loaded, the plist
    is the only evidence of what it will run, and it still names cargo's output -
    so building would overwrite the executable the next spawn uses."""
    env["plist"].write_text(
        f"<plist><array><string>{env['cargo_output']}</string></array></plist>\n"
    )
    (env["state"] / "loaded_labels").write_text("")  # job not loaded

    result = env["run"]()

    assert result.returncode != 0
    assert "still set up to run from cargo's output" in result.stderr
    assert "--adopt" in result.stderr
    assert "cargo build" not in calls(env)
    assert_service_untouched(env)


def test_adopt_installs_the_running_build_without_rebuilding(env):
    env["plist"].write_text(
        f"<plist><array><string>{env['cargo_output']}</string></array></plist>\n"
    )
    # The genuine pre-migration state: the loaded job runs cargo's output.
    (env["state"] / "job_program").write_text(str(env["cargo_output"]))
    _write(env["cargo_output"], _fake(env, "ALREADY_RUNNING"), executable=True)

    result = env["run"]("--adopt", "--allow-plist-change")

    assert result.returncode == 0, result.stderr
    assert "cargo build" not in calls(env)
    assert "MARKER=ALREADY_RUNNING" in env["installed"].read_text()
    assert "[registered=ORIGINAL]" in cutover_line(env, "stop-rust")


def test_adopt_refused_once_already_migrated(env):
    """Repeating --adopt would install whatever stale artifact is left in the
    target directory - a silent downgrade that every later check would pass."""
    _write(env["cargo_output"], _fake(env, "STALE_OLD_BUILD"), executable=True)
    # job_program already points at the installed binary (the migrated state)

    result = env["run"]("--adopt")

    assert result.returncode != 0
    assert "only for a service still registered against cargo's output" in result.stderr
    assert "MARKER=ORIGINAL" in env["installed"].read_text()
    assert_service_untouched(env)


def test_adopt_and_skip_build_conflict(env):
    result = env["run"]("--adopt", "--skip-build")

    assert result.returncode == 2
    assert "pick one" in result.stderr


def test_adopt_with_nothing_to_adopt(env):
    env["plist"].write_text(
        f"<plist><array><string>{env['cargo_output']}</string></array></plist>\n"
    )
    (env["state"] / "job_program").write_text(str(env["cargo_output"]))

    result = env["run"]("--adopt", "--allow-plist-change")

    assert result.returncode != 0
    assert "nothing to adopt" in result.stderr
    assert_service_untouched(env)


def test_loaded_registration_is_checked_not_just_the_plist(env):
    """An edited-but-not-reloaded plist still leaves launchd executing the old
    program, so the loaded job is the authoritative source."""
    (env["state"] / "job_program").write_text(str(env["cargo_output"]))

    result = env["run"]()

    assert result.returncode != 0
    assert "still set up to run from cargo's output" in result.stderr
    assert "cargo build" not in calls(env)
    assert_service_untouched(env)


def test_concurrent_restart_is_refused(env):
    """Two runs can both see the job unloaded and then race the install."""
    env["lock"].mkdir(parents=True)
    (env["lock"] / "pid").write_text(str(os.getpid()))  # a pid that is alive

    result = env["run"]()

    assert result.returncode != 0
    assert "already running" in result.stderr
    assert_service_untouched(env)


def test_dead_holder_lock_is_reported_not_removed(env):
    """Liveness-check-then-remove cannot be made atomic in shell: two racers
    would both delete and recreate the lock and both believe they own it. So a
    dead holder is reported for a human to clear, never reclaimed silently."""
    env["lock"].mkdir(parents=True)
    (env["lock"] / "pid").write_text("999999")  # not a live pid

    result = env["run"]()

    assert result.returncode != 0
    assert "is not running" in result.stderr
    assert f"rm -rf {env['lock']}" in result.stderr
    assert env["lock"].exists(), "a lock we do not own must not be removed"
    assert_service_untouched(env)


def test_loaded_registration_wins_over_a_stale_plist(env):
    """Job already on the installed binary, plist not yet reloaded. The rebuild
    is safe and must not be refused; the plist change is the divergence guard's
    business."""
    env["plist"].write_text(
        f"<plist><array><string>{env['cargo_output']}</string></array></plist>\n"
    )
    (env["state"] / "job_program").write_text(str(env["installed"]))

    result = env["run"]("--allow-plist-change")

    assert result.returncode == 0, result.stderr
    assert "cargo build" in calls(env)
    assert "--adopt" not in result.stderr


def test_lock_is_released_afterwards(env):
    assert env["run"]().returncode == 0
    assert not env["lock"].exists()

    # and a second run can therefore take it again
    assert env["run"]().returncode == 0


def test_lock_is_released_after_a_failure(env):
    (env["state"] / "codesign_sign_rc").write_text("1")

    assert env["run"]().returncode != 0
    assert not env["lock"].exists()


def test_symlink_alias_of_cargo_output_is_rejected(env):
    """A string compare would pass while launchd still execs cargo's artifact."""
    _write(env["cargo_output"], _fake(env, "REBUILT"), executable=True)
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


@pytest.mark.parametrize("mode", [(), ("--skip-build",), ("--adopt",)])
def test_cargo_output_registration_is_rejected_in_every_mode(env, mode):
    """A run that does not build must still not leave the service registered
    against cargo's output, or the next ordinary build replaces the live binary."""
    _write(env["cargo_output"], _fake(env, "SOMETHING"), executable=True)

    result = env["run"](*mode, SM_BINARY=str(env["cargo_output"]))

    assert result.returncode != 0
    assert "cargo's output" in result.stderr
    assert "cutover stop-rust" not in calls(env)


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
    assert f"--sign {SIGN_IDENTITY}" in calls(env)


def test_tracked_signing_config_supplies_the_durable_default(env):
    result = env["run"](SM_SIGN_IDENTITY="")

    assert result.returncode == 0, result.stderr
    assert f"--sign {SIGN_IDENTITY}" in calls(env)


def test_ca_issued_rotation_accepts_distinct_leaf_and_requirement_root(env):
    """A CA-issued leaf must not be compared to the root named by `-dr`.

    The rotation supplies the leaf fingerprint used by `--sign` plus the exact
    disposable-proof requirement. They intentionally differ, while the staged
    requirement must still equal the configured stable certificate anchor.
    """
    rotated_leaf = "A" * 40
    rotated_root = "b" * 40
    rotated_requirement = (
        'designated => identifier "com.rajeshgoli.sm-server" '
        f'and certificate root = H"{rotated_root}"'
    )
    (env["state"] / "available_signing_identity").write_text(rotated_leaf)
    (env["state"] / "requirement_identity").write_text(rotated_root)

    result = env["run"](
        SM_SIGN_IDENTITY=rotated_leaf,
        SM_SIGN_DESIGNATED_REQUIREMENT=rotated_requirement,
    )

    assert result.returncode == 0, result.stderr
    assert f"--sign {rotated_leaf}" in calls(env)
    assert (env["state"] / "requirements.log").read_text().splitlines() == [rotated_requirement]


def test_two_different_staged_binaries_require_the_same_certificate_requirement(env):
    """Changed bytes must not turn the TCC-facing identity back into a CDHash.

    The two runs stage different fake cargo outputs. The codesign stub records
    the exact designated requirement requested by the real script's inspection
    path, so this catches a regression that checks only the identifier or only
    the first staged binary.
    """
    first = env["run"]()
    assert first.returncode == 0, first.stderr

    _write(env["state"] / "rebuilt-binary", _fake(env, "REBUILT_CHANGED"))
    second = env["run"]()
    assert second.returncode == 0, second.stderr
    assert "MARKER=REBUILT_CHANGED" in env["installed"].read_text()

    requirements = (env["state"] / "requirements.log").read_text().splitlines()
    assert requirements == [SIGN_REQUIREMENT, SIGN_REQUIREMENT]


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
    assert "MARKER=REBUILT" in env["installed"].read_text()


def test_job_still_loaded_after_stop_aborts_before_install(env):
    """The cutover runs `launchctl bootout ... || true` and reports success
    regardless, so a job that refused to unload has to be caught here."""
    (env["state"] / "stop_leaves_loaded").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "still loaded" in result.stderr
    assert "MARKER=ORIGINAL" in env["installed"].read_text()
    assert "cutover start-rust" not in calls(env)


def test_stop_failure_leaves_the_previous_binary_in_place(env):
    (env["state"] / "stop_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "could not stop" in result.stderr
    assert "MARKER=ORIGINAL" in env["installed"].read_text()
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
    _write(other, _fake(env, "ORIGINAL"), executable=True)
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


@pytest.mark.parametrize(
    "bind,expected_probe",
    [
        ("0.0.0.0", "http://127.0.0.1:9"),
        ("::", "http://[::1]:9"),
        ("::1", "http://[::1]:9"),
        ("127.0.0.1", "http://127.0.0.1:9"),
    ],
)
def test_probe_url_is_a_valid_loopback_authority(env, bind, expected_probe):
    """The bind address is not usable as a probe URL: a wildcard is not something
    to connect to, the server's local bypass only trusts loopback Host values, and
    an IPv6 literal has to be bracketed."""
    env["run"](SM_HOST=bind)

    text = calls(env)
    assert f"{expected_probe}/health" in text, text
    # ...while launchd is still told to bind exactly what was asked for.
    assert f"--host {bind}" in cutover_line(env, "start-rust")


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


def test_log_dir_is_forwarded_when_set(env):
    """A deployment registered with a custom --log-dir otherwise has no way to
    make the rendered plist match: the comparison blocks every restart, and
    --allow-plist-change silently rewrites both launchd log paths."""
    log_dir = env["tmp"] / "custom-logs"
    log_dir.mkdir()

    result = env["run"](SM_LOG_DIR=str(log_dir))

    assert result.returncode == 0, result.stderr
    assert f"--log-dir {log_dir}" in cutover_line(env, "start-rust")


def test_log_dir_is_not_forwarded_by_default(env):
    """Unset must mean 'the cutover's own default', or every existing deployment
    would see a plist diff on the first run."""
    env["run"]()

    assert "--log-dir" not in cutover_line(env, "start-rust")


def test_unwritable_log_dir_blocks_before_the_service_is_stopped(env):
    """write_plist creates the log directory, and that runs after the bootout."""
    parent = env["tmp"] / "locked-logs"
    parent.mkdir()
    parent.chmod(0o500)
    try:
        result = env["run"](SM_LOG_DIR=str(parent / "nested"))
    finally:
        parent.chmod(0o700)

    assert result.returncode != 0
    assert "cannot write the launchd log directory" in result.stderr
    assert_service_untouched(env)


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


def test_cutover_enforced_python_labels_are_checked_even_when_overridden(env):
    """SM_PYTHON_LABELS cannot narrow the set that matters: the cutover enforces
    its own hard-coded list in start-rust, after it has already stopped us."""
    (env["state"] / "loaded_labels").write_text(
        f"{LABEL}\ncom.rajeshgoli.session-manager\n"
    )

    result = env["run"](SM_PYTHON_LABELS="com.example.something-else")

    assert result.returncode != 0
    assert "com.rajeshgoli.session-manager is still loaded" in result.stderr
    assert_service_untouched(env)


def test_enforced_python_label_list_matches_the_cutover():
    """Drift guard: if the cutover's list changes, this script must follow, or the
    preflight silently stops covering what start-rust will reject."""
    import re

    cutover = (REPO_ROOT / "scripts" / "rust-service-cutover.sh").read_text()
    declared = re.search(r"^PYTHON_LABELS=\((.*?)\)$", cutover, re.M).group(1)
    enforced = set(re.findall(r'"([^"]+)"', declared))

    script = SCRIPT.read_text()
    ours = re.search(r'^CUTOVER_PYTHON_LABELS="([^"]*)"$', script, re.M).group(1)

    assert set(ours.split()) == enforced, (
        f"restart-rust-server.sh checks {sorted(set(ours.split()))} but "
        f"rust-service-cutover.sh enforces {sorted(enforced)}"
    )


def test_invalid_config_blocks_before_the_service_is_stopped(env):
    """Readable is not valid. A malformed config would only fail once the new
    server started - after bootout, which KeepAlive turns into a crash loop."""
    (env["state"] / "check_config_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "rejected the configuration" in result.stderr
    assert_service_untouched(env)


def test_config_validation_is_skipped_on_a_binary_without_the_flag(env):
    """--skip-build/--adopt can deploy a build from before --check-config."""
    (env["state"] / "supports_check_config").write_text("0")

    result = env["run"]()

    assert result.returncode == 0, result.stderr
    assert "no --check-config" in result.stderr


def test_config_validation_covers_the_listen_address(env):
    """A bad host/port would otherwise pass phase 1 and only fail once launchd
    ran the replacement, after the bootout."""
    env["run"](SM_PORT="10")

    check = next(l for l in calls(env).splitlines() if "--check-config" in l)
    assert "--host 127.0.0.1" in check
    assert "--port 10" in check


def test_config_validation_passes_the_local_env_overlay(env):
    overlay = _write(env["tmp"] / "local.env", "SECRET=1\n")

    result = env["run"](SM_LOCAL_ENV=str(overlay))

    assert result.returncode == 0, result.stderr


def test_unwritable_plist_blocks_before_the_service_is_stopped(env):
    """start-rust rewrites the plist after bootout, so this would leave it down."""
    env["plist"].chmod(0o444)
    try:
        result = env["run"]()
    finally:
        env["plist"].chmod(0o644)

    assert result.returncode != 0
    assert "not writable" in result.stderr
    assert_service_untouched(env)


def test_unwritable_plist_directory_blocks_before_the_service_is_stopped(env):
    unwritable = env["tmp"] / "locked"
    unwritable.mkdir()
    unwritable.chmod(0o500)
    try:
        result = env["run"](SM_PLIST=str(unwritable / "svc.plist"))
    finally:
        unwritable.chmod(0o700)

    assert result.returncode != 0
    assert "cannot write the launchd plist directory" in result.stderr
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


@pytest.mark.parametrize(
    "var", ["SM_HEALTH_TIMEOUT", "SM_PID_SETTLE_SECONDS", "SM_UNLOAD_TIMEOUT"]
)
@pytest.mark.parametrize("value", ["abc", "12x", "-1", "3.5"])
def test_rejects_non_integer_timeouts_before_stopping(env, var, value):
    """These reach arithmetic expansion only in phase 2, after the bootout, where
    under `set -u` a typo aborts the script and leaves the service down."""
    result = env["run"](**{var: value})

    assert result.returncode == 2, result.stderr
    assert f"{var} must be a non-negative integer" in result.stderr
    assert_service_untouched(env)


@pytest.mark.parametrize(
    "var", ["SM_HEALTH_TIMEOUT", "SM_PID_SETTLE_SECONDS", "SM_UNLOAD_TIMEOUT"]
)
def test_leading_zero_timeouts_are_read_as_base_ten(env, var):
    """Bash treats a leading zero as octal, so `08` would abort the arithmetic in
    phase 2 - after the bootout - and `010` would silently mean 8."""
    result = env["run"](**{var: "08"})

    assert result.returncode == 0, result.stderr


def test_leading_zero_allow_drop_is_read_as_base_ten(env):
    (env["state"] / "after_sessions").write_text("4")  # 12 -> 4 is a drop of 8

    # Octal would make this 0 and fail the comparison; base 10 tolerates it.
    result = env["run"]("--allow-drop", "08")

    assert result.returncode == 0, result.stderr


def test_empty_timeout_override_falls_back_to_the_default(env):
    """`${VAR:-N}` treats empty as unset, so an empty override is the default,
    not an error."""
    result = env["run"](SM_UNLOAD_TIMEOUT="")

    assert result.returncode == 0, result.stderr


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
    assert "queue authority peer verified" in result.stdout
    assert (
        f"verify-queue-authority job_000000000000 --socket "
        f"{env['authority_socket_path']} --executable {env['installed']} "
        f"--launchd-label {LABEL} --signing-id com.rajeshgoli.sm-server "
        f"--expect-not-found"
    ) in calls(env)


def test_missing_queue_authority_socket_fails_after_restart(env):
    env["authority_socket"].close()
    env["authority_socket_path"].unlink()

    result = env["run"]()

    assert result.returncode != 0
    assert "queue authority socket is missing" in result.stderr


def test_queue_authority_peer_attestation_failure_is_reported(env):
    (env["state"] / "authority_rc").write_text("1")

    result = env["run"]()

    assert result.returncode != 0
    assert "queue authority peer attestation or exact probe validation failed" in result.stderr


def test_queue_authority_probe_must_return_not_found(env):
    (env["state"] / "authority_payload").write_text(
        '{"schema":"sm.queue_authority.response.v1","ok":true}\n'
    )

    result = env["run"]()

    assert result.returncode != 0
    assert "queue authority peer attestation or exact probe validation failed" in result.stderr


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
    assert "MARKER=ORIGINAL" in env["installed"].read_text()
