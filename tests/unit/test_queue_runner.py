"""Unit tests for managed local queue runner (#672)."""

from __future__ import annotations

import asyncio
import sqlite3
import sys
from datetime import datetime, timedelta
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient

from src.cli.commands import cmd_queue_ci_run, cmd_queue_run
from src.models import Session, SessionStatus
from src.queue_runner import QueueJob, QueueRunner
import src.queue_runner as queue_runner_module
from src.server import create_app
from src.session_manager import SessionManager


def _session(session_id: str, tmp_path) -> Session:
    return Session(
        id=session_id,
        name=f"claude-{session_id}",
        working_dir=str(tmp_path),
        tmux_session=f"claude-{session_id}",
        provider="claude",
        log_file=str(tmp_path / f"{session_id}.log"),
        status=SessionStatus.RUNNING,
        friendly_name="queue-owner",
    )


def _runner(mock_sm, tmp_path, extra_config=None) -> QueueRunner:
    config = {
        "queue_runner": {
            "state_dir": str(tmp_path / "queue-runner"),
            "max_running_jobs": 2,
            "perf_cooldown_seconds": 0,
            "cancel_grace_seconds": 0,
            "memory": {"min_free_bytes": 0, "retry_interval_seconds": 1},
            "resource_sampling": {"enabled": True, "interval_seconds": 1},
            "types": {
                "tests": {"max_concurrent": 2, "default_timeout_seconds": 5},
                "perf": {"max_concurrent": 1, "default_timeout_seconds": 5},
                "background": {"max_concurrent": 2, "default_timeout_seconds": 5},
            },
        }
    }
    if extra_config:
        config["queue_runner"].update(extra_config)
    return QueueRunner(mock_sm, config=config)


@pytest.fixture
def mock_sm(tmp_path):
    session = _session("agent672", tmp_path)
    sm = MagicMock()
    sm.sessions = {session.id: session}
    sm.get_session.side_effect = lambda sid: sm.sessions.get(sid)
    sm.get_effective_session_name.side_effect = lambda current: current.friendly_name if current else None
    sm.lookup_agent_registration.return_value = None
    sm.message_queue_manager = MagicMock()
    return sm


def test_memory_reader_counts_inactive_pages_as_available(mock_sm, tmp_path, monkeypatch):
    runner = _runner(mock_sm, tmp_path, extra_config={"memory": {"min_free_bytes": 2 * 1024 * 1024 * 1024}})
    vm_stat = """Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                                3773.
Pages active:                            329631.
Pages inactive:                          329351.
Pages speculative:                          836.
"""
    monkeypatch.setattr(queue_runner_module.subprocess, "check_output", lambda *args, **kwargs: vm_stat)

    assert runner._read_free_memory_bytes() > 2 * 1024 * 1024 * 1024
    assert runner._memory_gate_passes() is True


@pytest.mark.asyncio
async def test_queue_job_runs_and_notifies(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path)

    job = await runner.create_job(
        job_type="tests",
        label="hello",
        argv=[sys.executable, "-c", "print('queued hello')"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=None,
    )

    for _ in range(30):
        if runner.get_job(job.id).state in {"succeeded", "failed"}:
            break
        await asyncio.sleep(0.1)

    completed = runner.get_job(job.id)
    assert completed.state == "succeeded"
    assert completed.exit_code == 0
    assert "queued hello" in (tmp_path / "queue-runner" / "logs" / f"{job.id}.log").read_text()
    mock_sm.message_queue_manager.queue_message.assert_called()


@pytest.mark.asyncio
async def test_memory_gate_holds_and_pending_cancel(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path)
    runner._memory_gate_passes = MagicMock(return_value=False)
    runner._started = True

    job = await runner.create_job(
        job_type="tests",
        label="held",
        argv=[sys.executable, "-c", "print('no start')"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=None,
    )

    assert runner.get_job(job.id).state == "pending"
    assert runner.get_job(job.id).holding_reason == "memory_pressure"
    assert "queued:" in mock_sm.message_queue_manager.queue_message.call_args_list[-1].kwargs["text"]
    assert runner._scheduler_task is not None

    runner._memory_gate_passes = MagicMock(return_value=True)
    for _ in range(20):
        if runner.get_job(job.id).state in {"running", "succeeded"}:
            break
        await asyncio.sleep(0.1)
    assert runner.get_job(job.id).state in {"running", "succeeded"}

    cancelled = await runner.cancel_job(job.id)
    await runner.stop()
    assert cancelled.state in {"cancelled", "succeeded"}
    mock_sm.message_queue_manager.queue_message.assert_called()


@pytest.mark.asyncio
async def test_timeout_terminates_job(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path)

    job = await runner.create_job(
        job_type="tests",
        label="timeout",
        argv=[sys.executable, "-c", "import time; time.sleep(5)"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=1,
    )

    for _ in range(40):
        if runner.get_job(job.id).state == "timed_out":
            break
        await asyncio.sleep(0.1)

    assert runner.get_job(job.id).state == "timed_out"


@pytest.mark.asyncio
async def test_perf_displaces_running_background(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path, extra_config={"max_running_jobs": 1})

    background = await runner.create_job(
        job_type="background",
        label="background",
        argv=[sys.executable, "-c", "import time; time.sleep(5)"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=10,
    )
    assert runner.get_job(background.id).state == "running"

    perf = await runner.create_job(
        job_type="perf",
        label="perf",
        argv=[sys.executable, "-c", "print('perf')"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=5,
    )

    for _ in range(40):
        if runner.get_job(background.id).state == "displaced" and runner.get_job(perf.id).state in {"running", "succeeded"}:
            break
        await asyncio.sleep(0.1)

    assert runner.get_job(background.id).state == "displaced"
    assert runner.get_job(perf.id).state in {"running", "succeeded"}


@pytest.mark.asyncio
async def test_pending_tests_block_back_to_back_perf_after_perf_completion(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path)

    completed_perf = await runner.create_job(
        job_type="perf",
        label="perf1",
        argv=[sys.executable, "-c", "print('perf1')"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=5,
    )
    for _ in range(30):
        if runner.get_job(completed_perf.id).state == "succeeded":
            break
        await asyncio.sleep(0.1)

    runner._memory_gate_passes = MagicMock(return_value=False)
    tests_job = await runner.create_job(
        job_type="tests",
        label="tests",
        argv=[sys.executable, "-c", "import time; time.sleep(0.1)"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=5,
    )
    perf2 = await runner.create_job(
        job_type="perf",
        label="perf2",
        argv=[sys.executable, "-c", "print('perf2')"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=5,
    )

    runner._memory_gate_passes = MagicMock(return_value=True)
    async with runner._lock:
        await runner._admit_jobs_locked()

    assert runner.get_job(tests_job.id).state in {"running", "succeeded"}
    assert runner.get_job(perf2.id).state == "pending"


@pytest.mark.asyncio
async def test_start_recovers_dead_running_job(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path)
    job = await runner.create_job(
        job_type="tests",
        label="done",
        argv=[sys.executable, "-c", "print('done')"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=None,
    )
    await runner.cancel_job(job.id)

    with sqlite3.connect(runner.db_path) as conn:
        conn.execute(
            "UPDATE queue_jobs SET state='running', pid=99999999, process_group_id=99999999, "
            "finished_at=NULL, exit_code=NULL, completion_notified_at=NULL WHERE id=?",
            (job.id,),
        )

    recovered = _runner(mock_sm, tmp_path)
    await recovered.start()
    assert recovered.get_job(job.id).state == "failed"
    await recovered.stop()


@pytest.mark.asyncio
async def test_start_times_out_recovered_running_job(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path)
    job = await runner.create_job(
        job_type="tests",
        label="recovered-timeout",
        argv=[sys.executable, "-c", "import time; time.sleep(10)"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=1,
    )

    for _ in range(20):
        if runner.get_job(job.id).state == "running":
            break
        await asyncio.sleep(0.1)
    assert runner.get_job(job.id).state == "running"

    await runner.stop()
    with sqlite3.connect(runner.db_path) as conn:
        conn.execute(
            "UPDATE queue_jobs SET started_at=? WHERE id=?",
            ((datetime.now() - timedelta(seconds=5)).isoformat(), job.id),
        )

    recovered = _runner(mock_sm, tmp_path)
    await recovered.start()
    assert recovered.get_job(job.id).state == "timed_out"
    await recovered.stop()


@pytest.mark.asyncio
async def test_resource_sampling_records_when_queue_non_empty(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path)
    runner._memory_gate_passes = MagicMock(return_value=False)
    await runner.start()
    await runner.create_job(
        job_type="tests",
        label="held",
        argv=[sys.executable, "-c", "print('held')"],
        script=None,
        cwd=str(tmp_path),
        env={},
        notify_session_id="agent672",
        requester_session_id="agent672",
        timeout=None,
    )
    await asyncio.sleep(0.2)
    with sqlite3.connect(runner.db_path) as conn:
        count = conn.execute("SELECT COUNT(*) FROM queue_resource_samples").fetchone()[0]
    await runner.stop()
    assert count >= 1


def test_queue_job_endpoints_roundtrip(tmp_path):
    session_manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={"queue_runner": {"state_dir": str(tmp_path / "queue-runner"), "memory": {"min_free_bytes": 0}}},
    )
    session = _session("agent672", tmp_path)
    session_manager.sessions[session.id] = session
    session_manager.message_queue_manager = MagicMock()

    app = create_app(session_manager=session_manager)
    client = TestClient(app)

    response = client.post(
        "/queue-jobs",
        json={
            "type": "tests",
            "label": "api",
            "argv": [sys.executable, "-c", "print('api')"],
            "cwd": str(tmp_path),
            "notify_target": "agent672",
            "requester_session_id": "agent672",
            "timeout_seconds": 5,
        },
    )
    assert response.status_code == 200
    job_id = response.json()["id"]
    assert client.get(f"/queue-jobs/{job_id}").status_code == 200
    assert client.get("/queue-jobs?notify_target=agent672").json()["jobs"]


def test_cmd_queue_run_captures_argv_and_env(tmp_path, monkeypatch):
    client = MagicMock()
    client.create_queue_job.return_value = {
        "ok": True,
        "data": {"id": "job_cli", "type": "tests", "state": "pending", "log_path": "/tmp/job.log"},
    }
    monkeypatch.setenv("PATH", "/bin")

    code = cmd_queue_run(
        client,
        "agent672",
        job_type="tests",
        label="cli",
        cwd=str(tmp_path),
        timeout="10s",
        env_pairs=["EXTRA=1"],
        notify_target=None,
        command=["--", sys.executable, "-m", "pytest"],
        script_file=None,
    )

    assert code == 0
    call = client.create_queue_job.call_args.kwargs
    assert call["argv"] == [sys.executable, "-m", "pytest"]
    assert call["env"]["PATH"] == "/bin"
    assert call["env"]["EXTRA"] == "1"
    assert call["timeout_seconds"] == 10


@pytest.mark.asyncio
async def test_policy_run_suppresses_recent_token(mock_sm, tmp_path):
    runner = _runner(
        mock_sm,
        tmp_path,
        extra_config={
            "policies": {
                "ci": {
                    "type": "tests",
                    "cwd": str(tmp_path),
                    "dedupe": {"mode": "token", "token_window": 3},
                }
            }
        },
    )

    first = await runner.create_policy_run(
        policy="ci",
        dedupe_token="abc123",
        label=None,
        argv=[sys.executable, "-c", "print('first')"],
        script=None,
        cwd=None,
        env={},
        requester_session_id="agent672",
        timeout=None,
    )
    second = await runner.create_policy_run(
        policy="ci",
        dedupe_token="abc123",
        label=None,
        argv=[sys.executable, "-c", "print('second')"],
        script=None,
        cwd=None,
        env={},
        requester_session_id="agent672",
        timeout=None,
    )

    assert first.decision == "admitted"
    assert first.queue_job_id
    assert first.notify_session_id == "agent672"
    assert second.decision == "suppressed"
    assert second.suppression_reason == "dedupe_token"
    assert second.failed_gates == ["dedupe_token"]


@pytest.mark.asyncio
async def test_policy_run_suppresses_min_interval(mock_sm, tmp_path):
    runner = _runner(
        mock_sm,
        tmp_path,
        extra_config={
            "policies": {
                "ci": {
                    "type": "tests",
                    "cwd": str(tmp_path),
                    "min_interval_seconds": 60,
                    "dedupe": {"mode": "time"},
                }
            }
        },
    )

    first = await runner.create_policy_run(
        policy="ci",
        dedupe_token="one",
        label="first",
        argv=[sys.executable, "-c", "print('first')"],
        script=None,
        cwd=None,
        env={},
        requester_session_id="agent672",
        timeout=None,
    )
    second = await runner.create_policy_run(
        policy="ci",
        dedupe_token="two",
        label="second",
        argv=[sys.executable, "-c", "print('second')"],
        script=None,
        cwd=None,
        env={},
        requester_session_id="agent672",
        timeout=None,
    )

    assert first.decision == "admitted"
    assert second.decision == "suppressed"
    assert second.suppression_reason == "time_gate"
    assert second.failed_gates == ["time_gate"]


@pytest.mark.asyncio
async def test_policy_run_prunes_by_configured_retention(mock_sm, tmp_path):
    runner = _runner(
        mock_sm,
        tmp_path,
        extra_config={
            "policies": {
                "ci": {
                    "type": "tests",
                    "cwd": str(tmp_path),
                    "dedupe": {"mode": "none"},
                    "retention": {"admitted_runs": 1, "suppressed_runs": 1},
                }
            }
        },
    )

    first = await runner.create_policy_run(
        policy="ci",
        dedupe_token="one",
        label="first",
        argv=[sys.executable, "-c", "print('first')"],
        script=None,
        cwd=None,
        env={},
        requester_session_id="agent672",
        timeout=None,
    )
    second = await runner.create_policy_run(
        policy="ci",
        dedupe_token="two",
        label="second",
        argv=[sys.executable, "-c", "print('second')"],
        script=None,
        cwd=None,
        env={},
        requester_session_id="agent672",
        timeout=None,
    )

    runs = runner.list_policy_runs(policy="ci", include_suppressed=True)
    assert [run.id for run in runs] == [second.id]
    assert runner.get_policy_run(first.id) is None


@pytest.mark.asyncio
async def test_policy_run_fails_fast_when_queue_runner_disabled(mock_sm, tmp_path):
    runner = _runner(
        mock_sm,
        tmp_path,
        extra_config={
            "enabled": False,
            "policies": {
                "ci": {
                    "type": "tests",
                    "cwd": str(tmp_path),
                    "dedupe": {"mode": "token", "token_window": 3},
                }
            },
        },
    )

    with pytest.raises(ValueError, match="queue runner is disabled"):
        await runner.create_policy_run(
            policy="ci",
            dedupe_token="same",
            label="disabled",
            argv=[sys.executable, "-c", "print('disabled')"],
            script=None,
            cwd=None,
            env={},
            requester_session_id="agent672",
            timeout=None,
        )

    assert runner.list_policy_runs(policy="ci", include_suppressed=True) == []


@pytest.mark.asyncio
async def test_policy_run_create_job_failure_does_not_consume_gate(mock_sm, tmp_path):
    runner = _runner(
        mock_sm,
        tmp_path,
        extra_config={
            "policies": {
                "ci": {
                    "type": "tests",
                    "cwd": str(tmp_path),
                    "dedupe": {"mode": "token", "token_window": 3},
                }
            }
        },
    )
    original_create_job = runner.create_job
    calls = 0

    async def flaky_create_job(**kwargs):
        nonlocal calls
        calls += 1
        if calls == 1:
            raise ValueError("boom")
        return await original_create_job(**kwargs)

    runner.create_job = flaky_create_job

    with pytest.raises(ValueError, match="boom"):
        await runner.create_policy_run(
            policy="ci",
            dedupe_token="same",
            label="first",
            argv=[sys.executable, "-c", "print('first')"],
            script=None,
            cwd=None,
            env={},
            requester_session_id="agent672",
            timeout=None,
        )

    retry = await runner.create_policy_run(
        policy="ci",
        dedupe_token="same",
        label="retry",
        argv=[sys.executable, "-c", "print('retry')"],
        script=None,
        cwd=None,
        env={},
        requester_session_id="agent672",
        timeout=None,
    )

    assert retry.decision == "admitted"
    assert retry.queue_job_id
    assert [run.dedupe_token for run in runner.list_policy_runs(policy="ci")] == ["same"]


@pytest.mark.asyncio
async def test_policy_retention_preserves_token_window(mock_sm, tmp_path):
    runner = _runner(
        mock_sm,
        tmp_path,
        extra_config={
            "policies": {
                "ci": {
                    "type": "tests",
                    "cwd": str(tmp_path),
                    "dedupe": {"mode": "token", "token_window": 3},
                    "retention": {"admitted_runs": 1, "suppressed_runs": 1},
                }
            }
        },
    )

    for token in ["one", "two", "three"]:
        await runner.create_policy_run(
            policy="ci",
            dedupe_token=token,
            label=token,
            argv=[sys.executable, "-c", f"print('{token}')"],
            script=None,
            cwd=None,
            env={},
            requester_session_id="agent672",
            timeout=None,
        )

    duplicate = await runner.create_policy_run(
        policy="ci",
        dedupe_token="one",
        label="duplicate",
        argv=[sys.executable, "-c", "print('duplicate')"],
        script=None,
        cwd=None,
        env={},
        requester_session_id="agent672",
        timeout=None,
    )

    admitted = runner.list_policy_runs(policy="ci")
    assert len(admitted) == 3
    assert duplicate.decision == "suppressed"
    assert duplicate.suppression_reason == "dedupe_token"


@pytest.mark.asyncio
async def test_policy_status_by_id_enforces_policy_scope(mock_sm, tmp_path):
    runner = _runner(
        mock_sm,
        tmp_path,
        extra_config={
            "policies": {
                "ci": {"type": "tests", "cwd": str(tmp_path), "dedupe": {"mode": "none"}},
                "other": {"type": "tests", "cwd": str(tmp_path), "dedupe": {"mode": "none"}},
            }
        },
    )

    run = await runner.create_policy_run(
        policy="ci",
        dedupe_token="one",
        label="first",
        argv=[sys.executable, "-c", "print('first')"],
        script=None,
        cwd=None,
        env={},
        requester_session_id="agent672",
        timeout=None,
    )

    assert runner.get_policy_status(policy="ci", run_id=run.id).id == run.id
    assert runner.get_policy_status(policy="other", run_id=run.id) is None


@pytest.mark.asyncio
async def test_policy_run_rejects_type_override_unless_enabled(mock_sm, tmp_path):
    runner = _runner(
        mock_sm,
        tmp_path,
        extra_config={
            "policies": {
                "ci": {
                    "type": "background",
                    "cwd": str(tmp_path),
                    "dedupe": {"mode": "none"},
                }
            }
        },
    )

    with pytest.raises(ValueError, match="type override"):
        await runner.create_policy_run(
            policy="ci",
            dedupe_token=None,
            label="override",
            argv=[sys.executable, "-c", "print('override')"],
            script=None,
            cwd=None,
            env={},
            requester_session_id="agent672",
            timeout=None,
            job_type="tests",
        )


def test_queue_policy_run_endpoints_roundtrip(tmp_path):
    session_manager = SessionManager(
        log_dir=str(tmp_path / "logs"),
        state_file=str(tmp_path / "sessions.json"),
        config={
            "queue_runner": {
                "state_dir": str(tmp_path / "queue-runner"),
                "memory": {"min_free_bytes": 0},
                "policies": {
                    "ci": {
                        "type": "tests",
                        "cwd": str(tmp_path),
                        "dedupe": {"mode": "token", "token_window": 3},
                    }
                },
            }
        },
    )
    session = _session("agent672", tmp_path)
    session_manager.sessions[session.id] = session
    session_manager.message_queue_manager = MagicMock()

    app = create_app(session_manager=session_manager)
    client = TestClient(app)

    response = client.post(
        "/queue-policy-runs",
        json={
            "policy": "ci",
            "dedupe_token": "abc123",
            "argv": [sys.executable, "-c", "print('api')"],
            "requester_session_id": "agent672",
            "timeout_seconds": 5,
            "metadata": {"source": "test"},
        },
    )
    assert response.status_code == 200
    payload = response.json()
    assert payload["decision"] == "admitted"
    assert payload["queue_job_id"]
    assert client.get(f"/queue-policy-runs/{payload['id']}").status_code == 200
    assert client.get("/queue-policy-runs/status?policy=ci&dedupe_token=abc123").json()["id"] == payload["id"]
    assert client.get("/queue-policy-runs?policy=ci").json()["runs"]


def test_cmd_queue_ci_run_captures_policy_args(tmp_path, monkeypatch):
    client = MagicMock()
    client.create_queue_policy_run.return_value = {
        "ok": True,
        "data": {
            "id": "qpol_cli",
            "policy": "ci",
            "decision": "admitted",
            "queue_job_id": "job_cli",
            "status": "pending",
            "log_path": "/tmp/job.log",
        },
    }
    monkeypatch.setenv("PATH", "/bin")

    code = cmd_queue_ci_run(
        client,
        "agent672",
        policy="ci",
        dedupe_token="abc123",
        label="cli",
        cwd=str(tmp_path),
        timeout="10s",
        job_type="tests",
        env_pairs=["EXTRA=1"],
        metadata_pairs=["commit=abc123"],
        command=["--", sys.executable, "-m", "pytest"],
        script_file=None,
    )

    assert code == 0
    call = client.create_queue_policy_run.call_args.kwargs
    assert call["policy"] == "ci"
    assert call["dedupe_token"] == "abc123"
    assert call["argv"] == [sys.executable, "-m", "pytest"]
    assert call["env"]["PATH"] == "/bin"
    assert call["env"]["EXTRA"] == "1"
    assert call["metadata"] == {"commit": "abc123"}
    assert call["timeout_seconds"] == 10


def test_resource_sample_uses_supplied_job_snapshot(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path)
    queue_job = QueueJob(
        id="snapshot",
        type="tests",
        label="snapshot",
        requester_session_id="agent672",
        notify_session_id="agent672",
        cwd=str(tmp_path),
        argv=[sys.executable, "-c", "print('x')"],
        script_path=None,
        env={},
        timeout_seconds=5,
        state="pending",
        holding_reason=None,
        queued_at=datetime.now(),
    )

    runner._jobs.clear()
    runner._record_resource_sample([queue_job])

    with sqlite3.connect(runner.db_path) as conn:
        pending_json = conn.execute(
            "SELECT pending_by_type_json FROM queue_resource_samples ORDER BY sampled_at DESC LIMIT 1"
        ).fetchone()[0]
    assert '"tests": 1' in pending_json


def test_subprocess_env_uses_captured_values_only(mock_sm, tmp_path):
    runner = _runner(mock_sm, tmp_path)
    queue_job = QueueJob(
        id="manual",
        type="tests",
        label="manual",
        requester_session_id="agent672",
        notify_session_id="agent672",
        cwd=str(tmp_path),
        argv=[sys.executable, "-c", "print('x')"],
        script_path=None,
        env={"PATH": "/captured/bin", "CUSTOM": "yes"},
        timeout_seconds=5,
        state="pending",
        holding_reason=None,
        queued_at=datetime.now(),
    )

    env = runner._subprocess_env(queue_job)
    assert env == {"PATH": "/captured/bin", "CUSTOM": "yes"}
