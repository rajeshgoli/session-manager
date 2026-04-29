"""Managed local queue runner for resource-contended commands."""

from __future__ import annotations

import asyncio
import contextlib
import json
import os
import shlex
import signal
import sqlite3
import subprocess
import uuid
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any, Optional


TERMINAL_STATES = {"succeeded", "failed", "timed_out", "cancelled", "displaced"}
ACTIVE_STATES = {"pending", "running"}
DEFAULT_STATE_DIR = "~/.local/share/claude-sessions/queue-runner"


@dataclass
class PolicyRun:
    """One configured policy-admitted queue run."""

    id: str
    policy: str
    decision: str
    suppression_reason: Optional[str]
    failed_gates: list[str]
    dedupe_token: Optional[str]
    requested_at: datetime
    admitted_at: Optional[datetime] = None
    queue_job_id: Optional[str] = None
    notify_session_id: Optional[str] = None
    label: Optional[str] = None
    cwd: Optional[str] = None
    queue_type: Optional[str] = None
    command: Optional[list[str]] = None
    script_path: Optional[str] = None
    metadata: Optional[dict[str, str]] = None
    status: Optional[str] = None
    exit_code: Optional[int] = None
    queued_at: Optional[datetime] = None
    started_at: Optional[datetime] = None
    finished_at: Optional[datetime] = None
    log_path: Optional[str] = None
    updated_at: Optional[datetime] = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "policy": self.policy,
            "decision": self.decision,
            "suppression_reason": self.suppression_reason,
            "failed_gates": self.failed_gates,
            "dedupe_token": self.dedupe_token,
            "requested_at": self.requested_at.isoformat(),
            "admitted_at": self.admitted_at.isoformat() if self.admitted_at else None,
            "queue_job_id": self.queue_job_id,
            "notify_session_id": self.notify_session_id,
            "label": self.label,
            "cwd": self.cwd,
            "queue_type": self.queue_type,
            "command": self.command,
            "script_path": self.script_path,
            "metadata": self.metadata or {},
            "status": self.status,
            "exit_code": self.exit_code,
            "queued_at": self.queued_at.isoformat() if self.queued_at else None,
            "started_at": self.started_at.isoformat() if self.started_at else None,
            "finished_at": self.finished_at.isoformat() if self.finished_at else None,
            "log_path": self.log_path,
            "updated_at": self.updated_at.isoformat() if self.updated_at else None,
        }


@dataclass
class QueueJob:
    """One managed queue runner job."""

    id: str
    type: str
    label: str
    requester_session_id: Optional[str]
    notify_session_id: Optional[str]
    cwd: str
    argv: Optional[list[str]]
    script_path: Optional[str]
    env: dict[str, str]
    timeout_seconds: int
    state: str
    holding_reason: Optional[str]
    queued_at: datetime
    started_at: Optional[datetime] = None
    finished_at: Optional[datetime] = None
    pid: Optional[int] = None
    process_group_id: Optional[int] = None
    exit_code: Optional[int] = None
    log_path: Optional[str] = None
    exit_code_path: Optional[str] = None
    wrapper_path: Optional[str] = None
    queued_notified_at: Optional[datetime] = None
    started_notified_at: Optional[datetime] = None
    completion_notified_at: Optional[datetime] = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "type": self.type,
            "label": self.label,
            "requester_session_id": self.requester_session_id,
            "notify_session_id": self.notify_session_id,
            "cwd": self.cwd,
            "argv": self.argv,
            "script_path": self.script_path,
            "env": self.env,
            "timeout_seconds": self.timeout_seconds,
            "state": self.state,
            "holding_reason": self.holding_reason,
            "queued_at": self.queued_at.isoformat(),
            "started_at": self.started_at.isoformat() if self.started_at else None,
            "finished_at": self.finished_at.isoformat() if self.finished_at else None,
            "pid": self.pid,
            "process_group_id": self.process_group_id,
            "exit_code": self.exit_code,
            "log_path": self.log_path,
            "exit_code_path": self.exit_code_path,
            "wrapper_path": self.wrapper_path,
            "queued_notified_at": self.queued_notified_at.isoformat() if self.queued_notified_at else None,
            "started_notified_at": self.started_notified_at.isoformat() if self.started_notified_at else None,
            "completion_notified_at": self.completion_notified_at.isoformat() if self.completion_notified_at else None,
        }


def _parse_dt(value: Optional[str]) -> Optional[datetime]:
    return datetime.fromisoformat(value) if value else None


def _duration_seconds(value: str | int | None, default: int) -> int:
    if value is None or value == "":
        return default
    if isinstance(value, int):
        return value
    raw = str(value).strip().lower()
    try:
        if raw.endswith("ms"):
            return max(1, int(float(raw[:-2]) / 1000))
        if raw.endswith("s"):
            return max(1, int(float(raw[:-1])))
        if raw.endswith("m"):
            return max(1, int(float(raw[:-1]) * 60))
        if raw.endswith("h"):
            return max(1, int(float(raw[:-1]) * 3600))
        return max(1, int(float(raw)))
    except ValueError as exc:
        raise ValueError(f"invalid duration: {value}") from exc


class QueueRunner:
    """Admits and runs local commands under shared machine resource policy."""

    def __init__(self, session_manager: Any, config: Optional[dict[str, Any]] = None):
        self.session_manager = session_manager
        self.config = (config or {}).get("queue_runner", {})
        self.enabled = bool(self.config.get("enabled", True))
        self.state_dir = Path(str(self.config.get("state_dir", DEFAULT_STATE_DIR))).expanduser()
        self.log_dir = self.state_dir / "logs"
        self.db_path = self.state_dir / "queue_runner.db"
        self.policy_db_path = self.state_dir / "policy_runs.db"
        self.state_dir.mkdir(parents=True, exist_ok=True)
        self.log_dir.mkdir(parents=True, exist_ok=True)

        self.type_config = self._load_type_config()
        self.max_running_jobs = int(self.config.get("max_running_jobs", 2))
        self.cancel_grace_seconds = int(self.config.get("cancel_grace_seconds", 10))
        self.perf_cooldown_seconds = int(self.config.get("perf_cooldown_seconds", 30))
        memory_config = self.config.get("memory", {})
        self.min_free_bytes = int(memory_config.get("min_free_bytes", 2 * 1024 * 1024 * 1024))
        self.memory_retry_interval_seconds = int(memory_config.get("retry_interval_seconds", 10))
        sampling_config = self.config.get("resource_sampling", {})
        self.resource_sampling_enabled = bool(sampling_config.get("enabled", True))
        self.resource_sampling_interval_seconds = int(sampling_config.get("interval_seconds", 15))

        self._jobs: dict[str, QueueJob] = {}
        self._processes: dict[str, asyncio.subprocess.Process] = {}
        self._completion_tasks: dict[str, asyncio.Task[Any]] = {}
        self._scheduler_task: Optional[asyncio.Task[Any]] = None
        self._resource_sampler_task: Optional[asyncio.Task[Any]] = None
        self._lock = asyncio.Lock()
        self._started = False
        self._init_db()
        self._init_policy_db()
        self._load_jobs()

    def _load_type_config(self) -> dict[str, dict[str, int]]:
        configured = self.config.get("types", {})
        defaults = {
            "tests": {"max_concurrent": 2, "default_timeout_seconds": 900},
            "perf": {"max_concurrent": 1, "default_timeout_seconds": 2700},
            "background": {"max_concurrent": 2, "default_timeout_seconds": 3600},
        }
        configured_items = configured.items() if isinstance(configured, dict) else []
        for name, values in configured_items:
            if name in defaults and isinstance(values, dict):
                defaults[name].update({
                    key: int(values[key])
                    for key in ("max_concurrent", "default_timeout_seconds")
                    if key in values
                })
        return defaults

    def _connect(self) -> sqlite3.Connection:
        conn = sqlite3.connect(str(self.db_path))
        conn.row_factory = sqlite3.Row
        return conn

    def _connect_policy(self) -> sqlite3.Connection:
        conn = sqlite3.connect(str(self.policy_db_path))
        conn.row_factory = sqlite3.Row
        return conn

    def _init_db(self) -> None:
        with self._connect() as conn:
            conn.execute(
                """
                CREATE TABLE IF NOT EXISTS queue_jobs (
                    id TEXT PRIMARY KEY,
                    type TEXT NOT NULL,
                    label TEXT NOT NULL,
                    requester_session_id TEXT,
                    notify_session_id TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    argv_json TEXT,
                    script_path TEXT,
                    env_json TEXT NOT NULL,
                    timeout_seconds INTEGER NOT NULL,
                    state TEXT NOT NULL,
                    holding_reason TEXT,
                    queued_at TEXT NOT NULL,
                    started_at TEXT,
                    finished_at TEXT,
                    pid INTEGER,
                    process_group_id INTEGER,
                    exit_code INTEGER,
                    log_path TEXT,
                    exit_code_path TEXT,
                    wrapper_path TEXT,
                    queued_notified_at TEXT,
                    started_notified_at TEXT,
                    completion_notified_at TEXT
                )
                """
            )
            existing_columns = {
                row[1] for row in conn.execute("PRAGMA table_info(queue_jobs)").fetchall()
            }
            for column in ("queued_notified_at", "started_notified_at"):
                if column not in existing_columns:
                    conn.execute(f"ALTER TABLE queue_jobs ADD COLUMN {column} TEXT")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_queue_jobs_state_type_queued ON queue_jobs(state, type, queued_at)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_queue_jobs_notify_state ON queue_jobs(notify_session_id, state)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_queue_jobs_finished ON queue_jobs(finished_at)")
            conn.execute(
                """
                CREATE TABLE IF NOT EXISTS queue_resource_samples (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    sampled_at TEXT NOT NULL,
                    pending_by_type_json TEXT NOT NULL,
                    running_by_type_json TEXT NOT NULL,
                    total_running INTEGER NOT NULL,
                    memory_json TEXT NOT NULL,
                    cpu_json TEXT NOT NULL,
                    gpu_json TEXT
                )
                """
            )

    def _init_policy_db(self) -> None:
        with self._connect_policy() as conn:
            conn.execute(
                """
                CREATE TABLE IF NOT EXISTS queue_policy_runs (
                    id TEXT PRIMARY KEY,
                    policy TEXT NOT NULL,
                    decision TEXT NOT NULL,
                    suppression_reason TEXT,
                    failed_gates_json TEXT NOT NULL,
                    dedupe_token TEXT,
                    requested_at TEXT NOT NULL,
                    admitted_at TEXT,
                    queue_job_id TEXT,
                    notify_session_id TEXT,
                    label TEXT,
                    cwd TEXT,
                    queue_type TEXT,
                    command_json TEXT,
                    script_path TEXT,
                    metadata_json TEXT NOT NULL
                )
                """
            )
            conn.execute(
                """
                CREATE TABLE IF NOT EXISTS queue_policy_results (
                    policy_run_id TEXT PRIMARY KEY,
                    queue_job_id TEXT NOT NULL,
                    policy TEXT NOT NULL,
                    dedupe_token TEXT,
                    status TEXT NOT NULL,
                    exit_code INTEGER,
                    queued_at TEXT NOT NULL,
                    started_at TEXT,
                    finished_at TEXT,
                    log_path TEXT,
                    artifact_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )
                """
            )
            conn.execute("CREATE INDEX IF NOT EXISTS idx_policy_runs_policy_requested ON queue_policy_runs(policy, requested_at)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_policy_runs_policy_admitted ON queue_policy_runs(policy, admitted_at)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_policy_runs_policy_token ON queue_policy_runs(policy, dedupe_token, admitted_at)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_policy_runs_queue_job ON queue_policy_runs(queue_job_id)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_policy_results_policy_token ON queue_policy_results(policy, dedupe_token)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_policy_results_policy_finished ON queue_policy_results(policy, finished_at)")

    def _row_to_job(self, row: sqlite3.Row) -> QueueJob:
        return QueueJob(
            id=row["id"],
            type=row["type"],
            label=row["label"],
            requester_session_id=row["requester_session_id"],
            notify_session_id=row["notify_session_id"] or None,
            cwd=row["cwd"],
            argv=json.loads(row["argv_json"]) if row["argv_json"] else None,
            script_path=row["script_path"],
            env=json.loads(row["env_json"] or "{}"),
            timeout_seconds=int(row["timeout_seconds"]),
            state=row["state"],
            holding_reason=row["holding_reason"],
            queued_at=datetime.fromisoformat(row["queued_at"]),
            started_at=_parse_dt(row["started_at"]),
            finished_at=_parse_dt(row["finished_at"]),
            pid=row["pid"],
            process_group_id=row["process_group_id"],
            exit_code=row["exit_code"],
            log_path=row["log_path"],
            exit_code_path=row["exit_code_path"],
            wrapper_path=row["wrapper_path"],
            queued_notified_at=_parse_dt(row["queued_notified_at"]) if "queued_notified_at" in row.keys() else None,
            started_notified_at=_parse_dt(row["started_notified_at"]) if "started_notified_at" in row.keys() else None,
            completion_notified_at=_parse_dt(row["completion_notified_at"]),
        )

    def _policy_from_config(self, policy: str) -> dict[str, Any]:
        policies = self.config.get("policies", {})
        if not isinstance(policies, dict) or policy not in policies or not isinstance(policies[policy], dict):
            raise ValueError(f"unknown queue policy: {policy}")
        return policies[policy]

    def _policy_dedupe_mode(self, policy_config: dict[str, Any]) -> str:
        dedupe = policy_config.get("dedupe", {}) if isinstance(policy_config.get("dedupe", {}), dict) else {}
        mode = dedupe.get("mode")
        if mode:
            mode = str(mode)
        elif "min_interval_seconds" in policy_config and "token_window" in dedupe:
            mode = "both"
        elif "min_interval_seconds" in policy_config:
            mode = "time"
        elif "token_window" in dedupe:
            mode = "token"
        else:
            mode = "none"
        if mode not in {"none", "time", "token", "both"}:
            raise ValueError(f"unknown queue policy dedupe mode: {mode}")
        return mode

    def _policy_retention_config(self, policy_config: dict[str, Any]) -> tuple[int, int]:
        retention = policy_config.get("retention", {}) if isinstance(policy_config.get("retention", {}), dict) else {}
        dedupe = policy_config.get("dedupe", {}) if isinstance(policy_config.get("dedupe", {}), dict) else {}
        token_window = int(dedupe.get("token_window", 0) or 0)
        admitted = int(retention.get("admitted_runs", 200) or 200)
        suppressed = int(retention.get("suppressed_runs", 200) or 200)
        return max(1, admitted, token_window), max(1, suppressed)

    def _prune_policy_runs(self, policy: str, policy_config: dict[str, Any]) -> None:
        admitted_keep, suppressed_keep = self._policy_retention_config(policy_config)
        with self._connect_policy() as conn:
            for decision, keep in (("admitted", admitted_keep), ("suppressed", suppressed_keep)):
                rows = conn.execute(
                    """
                    SELECT id FROM queue_policy_runs
                    WHERE policy=? AND decision=?
                    ORDER BY requested_at DESC
                    LIMIT -1 OFFSET ?
                    """,
                    (policy, decision, keep),
                ).fetchall()
                stale_ids = [row["id"] for row in rows]
                if not stale_ids:
                    continue
                placeholders = ",".join("?" for _ in stale_ids)
                conn.execute(f"DELETE FROM queue_policy_results WHERE policy_run_id IN ({placeholders})", stale_ids)
                conn.execute(f"DELETE FROM queue_policy_runs WHERE id IN ({placeholders})", stale_ids)

    def _reconcile_policy_results(self) -> None:
        with self._connect_policy() as conn:
            rows = conn.execute(
                """
                SELECT * FROM queue_policy_runs
                WHERE decision='admitted' AND queue_job_id IS NOT NULL
                """
            ).fetchall()
        for row in rows:
            job = self._jobs.get(row["queue_job_id"])
            if job:
                self._sync_policy_result_for_job(job)
                continue
            with self._connect_policy() as conn:
                exists = conn.execute(
                    "SELECT 1 FROM queue_policy_results WHERE policy_run_id=?",
                    (row["id"],),
                ).fetchone()
                if exists:
                    continue
                now = datetime.now().isoformat()
                conn.execute(
                    """
                    INSERT INTO queue_policy_results
                    (policy_run_id, queue_job_id, policy, dedupe_token, status, exit_code, queued_at, started_at, finished_at, log_path, artifact_json, updated_at)
                    VALUES (?, ?, ?, ?, 'lost', NULL, ?, NULL, ?, NULL, '{}', ?)
                    """,
                    (row["id"], row["queue_job_id"], row["policy"], row["dedupe_token"], row["admitted_at"] or row["requested_at"], now, now),
                )

    def _policy_row_to_run(self, row: sqlite3.Row, result_row: Optional[sqlite3.Row] = None) -> PolicyRun:
        return PolicyRun(
            id=row["id"],
            policy=row["policy"],
            decision=row["decision"],
            suppression_reason=row["suppression_reason"],
            failed_gates=json.loads(row["failed_gates_json"] or "[]"),
            dedupe_token=row["dedupe_token"],
            requested_at=datetime.fromisoformat(row["requested_at"]),
            admitted_at=_parse_dt(row["admitted_at"]),
            queue_job_id=row["queue_job_id"],
            notify_session_id=row["notify_session_id"] or None,
            label=row["label"],
            cwd=row["cwd"],
            queue_type=row["queue_type"],
            command=json.loads(row["command_json"]) if row["command_json"] else None,
            script_path=row["script_path"],
            metadata=json.loads(row["metadata_json"] or "{}"),
            status=result_row["status"] if result_row else None,
            exit_code=result_row["exit_code"] if result_row else None,
            queued_at=datetime.fromisoformat(result_row["queued_at"]) if result_row and result_row["queued_at"] else None,
            started_at=_parse_dt(result_row["started_at"]) if result_row else None,
            finished_at=_parse_dt(result_row["finished_at"]) if result_row else None,
            log_path=result_row["log_path"] if result_row else None,
            updated_at=datetime.fromisoformat(result_row["updated_at"]) if result_row and result_row["updated_at"] else None,
        )

    def _load_jobs(self) -> None:
        with self._connect() as conn:
            for row in conn.execute("SELECT * FROM queue_jobs"):
                job = self._row_to_job(row)
                self._jobs[job.id] = job

    def _persist_job(self, job: QueueJob) -> None:
        with self._connect() as conn:
            conn.execute(
                """
                INSERT OR REPLACE INTO queue_jobs
                (id, type, label, requester_session_id, notify_session_id, cwd, argv_json,
                 script_path, env_json, timeout_seconds, state, holding_reason, queued_at,
                 started_at, finished_at, pid, process_group_id, exit_code, log_path,
                 exit_code_path, wrapper_path, queued_notified_at, started_notified_at, completion_notified_at)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    job.id,
                    job.type,
                    job.label,
                    job.requester_session_id,
                    job.notify_session_id or "",
                    job.cwd,
                    json.dumps(job.argv) if job.argv else None,
                    job.script_path,
                    json.dumps(job.env),
                    job.timeout_seconds,
                    job.state,
                    job.holding_reason,
                    job.queued_at.isoformat(),
                    job.started_at.isoformat() if job.started_at else None,
                    job.finished_at.isoformat() if job.finished_at else None,
                    job.pid,
                    job.process_group_id,
                    job.exit_code,
                    job.log_path,
                    job.exit_code_path,
                    job.wrapper_path,
                    job.queued_notified_at.isoformat() if job.queued_notified_at else None,
                    job.started_notified_at.isoformat() if job.started_notified_at else None,
                    job.completion_notified_at.isoformat() if job.completion_notified_at else None,
                ),
            )
        self._sync_policy_result_for_job(job)

    async def start(self) -> None:
        if not self.enabled or self._started:
            return
        self._started = True
        async with self._lock:
            for job in list(self._jobs.values()):
                if job.state == "running":
                    await self._recover_running_job_locked(job)
                elif job.state == "pending":
                    job.holding_reason = None
                    self._persist_job(job)
        self._schedule()
        self._ensure_resource_sampler()

    async def stop(self) -> None:
        for task in [self._scheduler_task, self._resource_sampler_task, *self._completion_tasks.values()]:
            if task:
                task.cancel()
        for task in [self._scheduler_task, self._resource_sampler_task, *self._completion_tasks.values()]:
            if task:
                with contextlib.suppress(asyncio.CancelledError):
                    await task
        self._scheduler_task = None
        self._resource_sampler_task = None
        self._completion_tasks.clear()
        self._started = False

    async def create_job(
        self,
        *,
        job_type: str,
        label: Optional[str],
        argv: Optional[list[str]],
        script: Optional[str],
        cwd: str,
        env: Optional[dict[str, str]],
        notify_session_id: Optional[str],
        requester_session_id: Optional[str],
        timeout: str | int | None,
    ) -> QueueJob:
        if not self.enabled:
            raise ValueError("queue runner is disabled")
        if job_type not in self.type_config:
            raise ValueError(f"unknown queue job type: {job_type}")
        if bool(argv) == bool(script):
            raise ValueError("exactly one of argv or script is required")
        cwd_path = Path(cwd).expanduser().resolve()
        if not cwd_path.exists() or not cwd_path.is_dir():
            raise ValueError(f"cwd does not exist or is not a directory: {cwd}")
        if notify_session_id and not self.session_manager.get_session(notify_session_id):
            raise ValueError("notify target not found")

        job_id = f"job_{uuid.uuid4().hex[:12]}"
        job_dir = self.state_dir / job_id
        job_dir.mkdir(parents=True, exist_ok=True)
        script_path = None
        if script is not None:
            script_path = str(job_dir / "submitted.zsh")
            Path(script_path).write_text(script, encoding="utf-8")
        timeout_seconds = _duration_seconds(
            timeout,
            self.type_config[job_type]["default_timeout_seconds"],
        )
        safe_env = {str(k): str(v) for k, v in (env or {}).items()}
        summary = label or (Path(argv[0]).name if argv else "script")
        now = datetime.now()
        job = QueueJob(
            id=job_id,
            type=job_type,
            label=summary,
            requester_session_id=requester_session_id,
            notify_session_id=notify_session_id,
            cwd=str(cwd_path),
            argv=argv,
            script_path=script_path,
            env=safe_env,
            timeout_seconds=timeout_seconds,
            state="pending",
            holding_reason=None,
            queued_at=now,
            log_path=str(self.log_dir / f"{job_id}.log"),
            exit_code_path=str(job_dir / "exit.code"),
            wrapper_path=str(job_dir / "run.zsh"),
        )
        self._write_wrapper(job)
        async with self._lock:
            self._jobs[job.id] = job
            self._persist_job(job)
            await self._admit_jobs_locked()
            if job.state == "pending" and job.queued_notified_at is None:
                self._notify_queued(job)
                job.queued_notified_at = datetime.now()
                self._persist_job(job)
            if job.state == "pending":
                self._schedule()
        self._ensure_resource_sampler()
        return job

    async def create_policy_run(
        self,
        *,
        policy: str,
        dedupe_token: Optional[str],
        label: Optional[str],
        argv: Optional[list[str]],
        script: Optional[str],
        cwd: Optional[str],
        env: Optional[dict[str, str]],
        requester_session_id: Optional[str],
        timeout: str | int | None,
        job_type: Optional[str] = None,
        metadata: Optional[dict[str, str]] = None,
    ) -> PolicyRun:
        policy_config = self._policy_from_config(policy)
        mode = self._policy_dedupe_mode(policy_config)
        dedupe_config = policy_config.get("dedupe", {}) if isinstance(policy_config.get("dedupe", {}), dict) else {}
        token_window = int(dedupe_config.get("token_window", 0) or 0)
        min_interval = int(policy_config.get("min_interval_seconds", 0) or 0)
        if mode in {"token", "both"} and not dedupe_token:
            raise ValueError("--dedupe-token is required for this policy")
        if mode in {"time", "both"} and min_interval <= 0:
            raise ValueError("policy min_interval_seconds must be positive for time gating")
        if mode in {"token", "both"} and token_window <= 0:
            raise ValueError("policy dedupe.token_window must be positive for token gating")

        run_id = f"qpol_{uuid.uuid4().hex[:12]}"
        now = datetime.now()
        failed_gates: list[str] = []
        suppression_reason = None
        with self._connect_policy() as conn:
            conn.execute("BEGIN IMMEDIATE")
            if mode in {"token", "both"}:
                rows = conn.execute(
                    """
                    SELECT dedupe_token FROM queue_policy_runs
                    WHERE policy = ? AND decision = 'admitted' AND dedupe_token IS NOT NULL
                    ORDER BY admitted_at DESC
                    LIMIT ?
                    """,
                    (policy, token_window),
                ).fetchall()
                recent_tokens = {row["dedupe_token"] for row in rows}
                if dedupe_token in recent_tokens:
                    failed_gates.append("dedupe_token")
            if mode in {"time", "both"}:
                row = conn.execute(
                    """
                    SELECT admitted_at FROM queue_policy_runs
                    WHERE policy = ? AND decision = 'admitted' AND admitted_at IS NOT NULL
                    ORDER BY admitted_at DESC LIMIT 1
                    """,
                    (policy,),
                ).fetchone()
                if row and (now - datetime.fromisoformat(row["admitted_at"])).total_seconds() < min_interval:
                    failed_gates.append("time_gate")
            if failed_gates:
                suppression_reason = "dedupe_token" if "dedupe_token" in failed_gates else "time_gate"
                conn.execute(
                    """
                    INSERT INTO queue_policy_runs
                    (id, policy, decision, suppression_reason, failed_gates_json, dedupe_token, requested_at, metadata_json)
                    VALUES (?, ?, 'suppressed', ?, ?, ?, ?, ?)
                    """,
                    (run_id, policy, suppression_reason, json.dumps(failed_gates), dedupe_token, now.isoformat(), json.dumps(metadata or {})),
                )
                conn.commit()
                self._prune_policy_runs(policy, policy_config)
                return self.get_policy_run(run_id) or PolicyRun(
                    id=run_id, policy=policy, decision="suppressed", suppression_reason=suppression_reason,
                    failed_gates=failed_gates, dedupe_token=dedupe_token, requested_at=now, metadata=metadata or {},
                )

            configured_type = str(policy_config.get("type") or "background")
            if job_type and job_type != configured_type and not bool(policy_config.get("allow_type_override", False)):
                raise ValueError("policy does not allow --type override")
            effective_type = job_type or configured_type
            effective_cwd = cwd or policy_config.get("cwd")
            if not effective_cwd:
                raise ValueError("cwd is required when policy does not configure one")
            effective_timeout = timeout if timeout is not None else policy_config.get("timeout_seconds")
            effective_label = label or (f"{policy}@{str(dedupe_token)[:12]}" if dedupe_token else policy)
            notify_session_id = requester_session_id if requester_session_id and self.session_manager.get_session(requester_session_id) else None
            conn.execute(
                """
                INSERT INTO queue_policy_runs
                (id, policy, decision, suppression_reason, failed_gates_json, dedupe_token, requested_at, admitted_at,
                 notify_session_id, label, cwd, queue_type, command_json, metadata_json)
                VALUES (?, ?, 'admitted', NULL, '[]', ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    run_id, policy, dedupe_token, now.isoformat(), now.isoformat(), notify_session_id,
                    effective_label, str(effective_cwd), effective_type, json.dumps(argv) if argv else None, json.dumps(metadata or {}),
                ),
            )
            conn.commit()

        try:
            job = await self.create_job(
                job_type=effective_type,
                label=effective_label,
                argv=argv,
                script=script,
                cwd=str(effective_cwd),
                env=env,
                notify_session_id=notify_session_id,
                requester_session_id=requester_session_id,
                timeout=effective_timeout,
            )
        except Exception:
            with self._connect_policy() as conn:
                conn.execute(
                    "UPDATE queue_policy_runs SET decision='suppressed', suppression_reason='queue_create_failed', failed_gates_json=? WHERE id=?",
                    (json.dumps(["queue_create_failed"]), run_id),
                )
            raise

        with self._connect_policy() as conn:
            conn.execute("UPDATE queue_policy_runs SET queue_job_id=?, script_path=? WHERE id=?", (job.id, job.script_path, run_id))
        self._sync_policy_result_for_job(job)
        self._prune_policy_runs(policy, policy_config)
        return self.get_policy_run(run_id) or PolicyRun(id=run_id, policy=policy, decision="admitted", suppression_reason=None, failed_gates=[], dedupe_token=dedupe_token, requested_at=now)

    def _sync_policy_result_for_job(self, job: QueueJob) -> None:
        if not job.id:
            return
        with self._connect_policy() as conn:
            row = conn.execute("SELECT * FROM queue_policy_runs WHERE queue_job_id=? AND decision='admitted'", (job.id,)).fetchone()
            if not row:
                return
            now = datetime.now().isoformat()
            conn.execute(
                """
                INSERT OR REPLACE INTO queue_policy_results
                (policy_run_id, queue_job_id, policy, dedupe_token, status, exit_code, queued_at, started_at, finished_at, log_path, artifact_json, updated_at)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    row["id"], job.id, row["policy"], row["dedupe_token"], job.state, job.exit_code,
                    job.queued_at.isoformat(), job.started_at.isoformat() if job.started_at else None,
                    job.finished_at.isoformat() if job.finished_at else None, job.log_path, "{}", now,
                ),
            )

    def get_policy_run(self, run_id: str) -> Optional[PolicyRun]:
        self._reconcile_policy_results()
        with self._connect_policy() as conn:
            row = conn.execute("SELECT * FROM queue_policy_runs WHERE id=?", (run_id,)).fetchone()
            if not row:
                return None
            result = conn.execute("SELECT * FROM queue_policy_results WHERE policy_run_id=?", (run_id,)).fetchone()
            return self._policy_row_to_run(row, result)

    def get_policy_status(self, *, policy: str, dedupe_token: Optional[str] = None, run_id: Optional[str] = None) -> Optional[PolicyRun]:
        if run_id:
            run = self.get_policy_run(run_id)
            if run is None or run.policy != policy:
                return None
            return run
        if not dedupe_token:
            raise ValueError("dedupe_token or run_id is required")
        self._reconcile_policy_results()
        with self._connect_policy() as conn:
            row = conn.execute(
                """
                SELECT * FROM queue_policy_runs
                WHERE policy=? AND dedupe_token=? AND decision='admitted'
                ORDER BY admitted_at DESC LIMIT 1
                """,
                (policy, dedupe_token),
            ).fetchone()
            if not row:
                return None
            result = conn.execute("SELECT * FROM queue_policy_results WHERE policy_run_id=?", (row["id"],)).fetchone()
            return self._policy_row_to_run(row, result)

    def list_policy_runs(self, *, policy: str, limit: int = 50, include_suppressed: bool = False) -> list[PolicyRun]:
        self._reconcile_policy_results()
        with self._connect_policy() as conn:
            if include_suppressed:
                rows = conn.execute(
                    "SELECT * FROM queue_policy_runs WHERE policy=? ORDER BY requested_at DESC LIMIT ?",
                    (policy, limit),
                ).fetchall()
            else:
                rows = conn.execute(
                    "SELECT * FROM queue_policy_runs WHERE policy=? AND decision='admitted' ORDER BY requested_at DESC LIMIT ?",
                    (policy, limit),
                ).fetchall()
            runs = []
            for row in rows:
                result = conn.execute("SELECT * FROM queue_policy_results WHERE policy_run_id=?", (row["id"],)).fetchone()
                runs.append(self._policy_row_to_run(row, result))
            return runs

    def list_jobs(
        self,
        *,
        notify_session_id: Optional[str] = None,
        job_type: Optional[str] = None,
        state: Optional[str] = None,
        include_terminal: bool = False,
    ) -> list[QueueJob]:
        jobs = list(self._jobs.values())
        if notify_session_id:
            jobs = [job for job in jobs if job.notify_session_id == notify_session_id]
        if job_type:
            jobs = [job for job in jobs if job.type == job_type]
        if state:
            if state == "done":
                jobs = [job for job in jobs if job.state in TERMINAL_STATES]
            else:
                jobs = [job for job in jobs if job.state == state]
        elif not include_terminal:
            jobs = [job for job in jobs if job.state in ACTIVE_STATES]
        return sorted(jobs, key=lambda job: job.queued_at)

    def get_job(self, job_id: str) -> Optional[QueueJob]:
        return self._jobs.get(job_id)

    async def cancel_job(self, job_id: str) -> Optional[QueueJob]:
        async with self._lock:
            job = self._jobs.get(job_id)
            if job is None:
                return None
            if job.state in TERMINAL_STATES:
                return job
            if job.state == "pending":
                await self._finish_job_locked(job, "cancelled", exit_code=None, notify=True)
                return job
            await self._terminate_job_locked(job, state="cancelled")
            return job

    def _write_wrapper(self, job: QueueJob) -> None:
        assert job.wrapper_path and job.exit_code_path
        lines = [
            "#!/bin/zsh",
            "set +e",
            f"cd {shlex.quote(job.cwd)} || exit 127",
        ]
        for key, value in job.env.items():
            lines.append(f"export {shlex.quote(key)}={shlex.quote(value)}")
        if job.argv:
            command = " ".join(shlex.quote(part) for part in job.argv)
            lines.append(command)
        else:
            lines.append(f"/bin/zsh {shlex.quote(str(job.script_path))}")
        lines.extend([
            "code=$?",
            f"printf '%s\\n' \"$code\" > {shlex.quote(job.exit_code_path)}",
            "exit \"$code\"",
        ])
        path = Path(job.wrapper_path)
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        path.chmod(0o700)

    def _schedule(self) -> None:
        if not self._started:
            return
        if self._scheduler_task and not self._scheduler_task.done():
            return
        self._scheduler_task = asyncio.create_task(self._scheduler_loop())

    async def _scheduler_loop(self) -> None:
        try:
            while True:
                async with self._lock:
                    changed = await self._admit_jobs_locked()
                    has_pending = any(job.state == "pending" for job in self._jobs.values())
                if not has_pending:
                    return
                await asyncio.sleep(self.memory_retry_interval_seconds if not changed else 0.1)
        except asyncio.CancelledError:
            raise

    async def _admit_jobs_locked(self) -> bool:
        changed = False
        while True:
            if await self._maybe_displace_for_perf_locked():
                changed = True
                continue
            candidate = self._next_admissible_job_locked()
            if candidate is None:
                break
            if candidate.holding_reason:
                candidate.holding_reason = None
                self._persist_job(candidate)
            await self._start_job_locked(candidate)
            changed = True
        return changed

    async def _maybe_displace_for_perf_locked(self) -> bool:
        perf_job = self._oldest_pending("perf")
        if not perf_job:
            return False
        if self._running_count() < self.max_running_jobs:
            return False
        if self._running_count("perf") >= self.type_config["perf"]["max_concurrent"]:
            return False
        if not self._memory_gate_passes() or self._perf_cooldown_active() or self._perf_blocked_by_tests_after_perf():
            return False
        backgrounds = [job for job in self._jobs.values() if job.state == "running" and job.type == "background"]
        if not backgrounds:
            return False
        oldest_background = min(backgrounds, key=lambda job: job.started_at or job.queued_at)
        await self._terminate_job_locked(oldest_background, state="displaced")
        return True

    def _next_admissible_job_locked(self) -> Optional[QueueJob]:
        if self._running_count() >= self.max_running_jobs:
            self._mark_pending_holding("concurrency_cap")
            return None
        if not self._memory_gate_passes():
            self._mark_pending_holding("memory_pressure")
            return None
        for job_type in ("perf", "tests", "background"):
            job = self._oldest_pending(job_type)
            if not job:
                continue
            if self._running_count(job_type) >= self.type_config[job_type]["max_concurrent"]:
                job.holding_reason = "concurrency_cap"
                self._persist_job(job)
                continue
            if job_type == "perf" and self._perf_cooldown_active():
                job.holding_reason = "perf_cooldown"
                self._persist_job(job)
                continue
            if job_type == "perf" and self._perf_blocked_by_tests_after_perf():
                job.holding_reason = "awaiting_tests"
                self._persist_job(job)
                continue
            return job
        return None

    def _mark_pending_holding(self, reason: str) -> None:
        for job in self._jobs.values():
            if job.state == "pending" and job.holding_reason != reason:
                job.holding_reason = reason
                self._persist_job(job)

    def _running_count(self, job_type: Optional[str] = None) -> int:
        return sum(
            1
            for job in self._jobs.values()
            if job.state == "running" and (job_type is None or job.type == job_type)
        )

    def _oldest_pending(self, job_type: str) -> Optional[QueueJob]:
        pending = [job for job in self._jobs.values() if job.state == "pending" and job.type == job_type]
        return min(pending, key=lambda job: job.queued_at) if pending else None

    def _perf_cooldown_active(self) -> bool:
        now = datetime.now()
        for job in self._jobs.values():
            if job.type in {"perf", "tests"} and job.finished_at:
                if (now - job.finished_at).total_seconds() < self.perf_cooldown_seconds:
                    return True
        return False

    def _perf_blocked_by_tests_after_perf(self) -> bool:
        finished = [
            job for job in self._jobs.values()
            if job.type in {"perf", "tests"} and job.finished_at is not None
        ]
        if not finished:
            return False
        latest = max(finished, key=lambda job: job.finished_at or datetime.min)
        if latest.type != "perf":
            return False
        return any(
            job.type == "tests" and job.state in {"pending", "running"}
            for job in self._jobs.values()
        )

    def _memory_gate_passes(self) -> bool:
        if self.min_free_bytes <= 0:
            return True
        available = self._read_free_memory_bytes()
        return available is None or available >= self.min_free_bytes

    def _read_free_memory_bytes(self) -> Optional[int]:
        try:
            output = subprocess.check_output(["vm_stat"], text=True, timeout=1)
        except Exception:
            return None
        page_size = 4096
        available_pages = 0
        for line in output.splitlines():
            if "page size of" in line:
                parts = [part for part in line.split() if part.isdigit()]
                if parts:
                    page_size = int(parts[0])
            # macOS keeps reclaimable memory in the inactive queue. Literal
            # free pages can be very low even when memory_pressure reports the
            # machine is healthy, so the queue gate should use available pages.
            if line.startswith(("Pages free:", "Pages speculative:", "Pages inactive:")):
                digits = "".join(ch for ch in line if ch.isdigit())
                if digits:
                    available_pages += int(digits)
        return available_pages * page_size if available_pages else None

    async def _start_job_locked(self, job: QueueJob) -> None:
        assert job.wrapper_path and job.log_path
        log_handle = open(job.log_path, "ab", buffering=0)
        process = await asyncio.create_subprocess_exec(
            "/bin/zsh",
            job.wrapper_path,
            stdout=log_handle,
            stderr=asyncio.subprocess.STDOUT,
            start_new_session=True,
            env=self._subprocess_env(job),
        )
        log_handle.close()
        job.pid = process.pid
        job.process_group_id = process.pid
        job.started_at = datetime.now()
        job.state = "running"
        job.holding_reason = None
        self._processes[job.id] = process
        if job.queued_notified_at is not None and job.started_notified_at is None:
            self._notify_started(job)
            job.started_notified_at = datetime.now()
        self._persist_job(job)
        task = asyncio.create_task(self._wait_for_job(job.id))
        self._completion_tasks[job.id] = task

    def _subprocess_env(self, job: QueueJob) -> dict[str, str]:
        env = {str(key): str(value) for key, value in job.env.items()}
        if "PATH" not in env:
            env["PATH"] = "/usr/bin:/bin:/usr/sbin:/sbin"
        return env

    async def _wait_for_job(self, job_id: str) -> None:
        try:
            job = self._jobs.get(job_id)
            process = self._processes.get(job_id)
            if not job or not process:
                return
            try:
                exit_code = await asyncio.wait_for(process.wait(), timeout=job.timeout_seconds)
                async with self._lock:
                    if job.state != "running":
                        return
                    await self._finish_job_locked(
                        job,
                        "succeeded" if exit_code == 0 else "failed",
                        exit_code=exit_code,
                        notify=True,
                    )
            except asyncio.TimeoutError:
                async with self._lock:
                    if job.state != "running":
                        return
                    await self._terminate_job_locked(job, state="timed_out")
        finally:
            self._processes.pop(job_id, None)
            self._completion_tasks.pop(job_id, None)
            self._schedule()
            self._ensure_resource_sampler()

    async def _recover_running_job_locked(self, job: QueueJob) -> None:
        if job.exit_code_path and Path(job.exit_code_path).exists():
            exit_code = self._read_exit_code(job)
            await self._finish_job_locked(
                job,
                "succeeded" if exit_code == 0 else "failed",
                exit_code=exit_code,
                notify=job.completion_notified_at is None,
            )
            return
        if self._job_timed_out(job):
            await self._terminate_job_locked(job, state="timed_out")
            return
        if job.pid and self._pid_exists(job.pid):
            task = asyncio.create_task(self._poll_recovered_job(job.id))
            self._completion_tasks[job.id] = task
            return
        await self._finish_job_locked(job, "failed", exit_code=None, notify=job.completion_notified_at is None)

    async def _poll_recovered_job(self, job_id: str) -> None:
        try:
            while True:
                await asyncio.sleep(2)
                async with self._lock:
                    job = self._jobs.get(job_id)
                    if not job or job.state != "running":
                        return
                    if job.exit_code_path and Path(job.exit_code_path).exists():
                        exit_code = self._read_exit_code(job)
                        await self._finish_job_locked(
                            job,
                            "succeeded" if exit_code == 0 else "failed",
                            exit_code=exit_code,
                            notify=job.completion_notified_at is None,
                        )
                        return
                    if self._job_timed_out(job):
                        await self._terminate_job_locked(job, state="timed_out")
                        return
                    if job.pid and not self._pid_exists(job.pid):
                        await self._finish_job_locked(job, "failed", exit_code=None, notify=job.completion_notified_at is None)
                        return
        finally:
            self._completion_tasks.pop(job_id, None)
            self._schedule()

    def _job_timed_out(self, job: QueueJob) -> bool:
        if not job.started_at:
            return False
        return (datetime.now() - job.started_at).total_seconds() >= job.timeout_seconds

    def _read_exit_code(self, job: QueueJob) -> Optional[int]:
        try:
            return int(Path(str(job.exit_code_path)).read_text(encoding="utf-8").strip())
        except Exception:
            return None

    def _pid_exists(self, pid: int) -> bool:
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
        except PermissionError:
            return True

    async def _terminate_job_locked(self, job: QueueJob, *, state: str) -> None:
        pgid = job.process_group_id or job.pid
        if pgid:
            with contextlib.suppress(ProcessLookupError, PermissionError):
                os.killpg(pgid, signal.SIGTERM)
            await asyncio.sleep(self.cancel_grace_seconds)
            if job.pid and self._pid_exists(job.pid):
                with contextlib.suppress(ProcessLookupError, PermissionError):
                    os.killpg(pgid, signal.SIGKILL)
        exit_code = self._read_exit_code(job)
        await self._finish_job_locked(job, state, exit_code=exit_code, notify=True)

    async def _finish_job_locked(
        self,
        job: QueueJob,
        state: str,
        *,
        exit_code: Optional[int],
        notify: bool,
    ) -> None:
        job.state = state
        job.exit_code = exit_code
        job.finished_at = datetime.now()
        job.holding_reason = None
        if notify and job.completion_notified_at is None:
            self._notify_completion(job)
            job.completion_notified_at = datetime.now()
        self._persist_job(job)

    def _notify_completion(self, job: QueueJob) -> None:
        mq = getattr(self.session_manager, "message_queue_manager", None)
        if not mq or not job.notify_session_id:
            return
        runtime = "-"
        if job.started_at and job.finished_at:
            runtime = f"{int((job.finished_at - job.started_at).total_seconds())}s"
        queued = "-"
        if job.finished_at:
            queued = f"{int(((job.started_at or job.finished_at) - job.queued_at).total_seconds())}s"
        exit_text = f" exit={job.exit_code}" if job.exit_code is not None else ""
        stderr_tail = self._tail_log(job.log_path, max_bytes=8192)
        text = (
            f"[sm queue] {job.id} completed: {job.state}{exit_text} "
            f"runtime={runtime} queue={queued}. Log: {job.log_path or '-'}"
        )
        if stderr_tail:
            text += f"\nlog tail:\n{stderr_tail}"
        mq.queue_message(target_session_id=job.notify_session_id, text=text, delivery_mode="sequential")

    def _notify_queued(self, job: QueueJob) -> None:
        mq = getattr(self.session_manager, "message_queue_manager", None)
        if not mq or not job.notify_session_id:
            return
        position = len([other for other in self._jobs.values() if other.state == "pending" and other.type == job.type and other.queued_at <= job.queued_at])
        text = (
            f"[sm queue] {job.id} queued: {job.type}, position {position}, "
            f"holding on {job.holding_reason or 'queue'}. Log: {job.log_path or '-'}"
        )
        mq.queue_message(target_session_id=job.notify_session_id, text=text, delivery_mode="sequential")

    def _notify_started(self, job: QueueJob) -> None:
        mq = getattr(self.session_manager, "message_queue_manager", None)
        if not mq or not job.notify_session_id:
            return
        text = f"[sm queue] {job.id} started: {job.type}, pid {job.pid or '-'}. Log: {job.log_path or '-'}"
        mq.queue_message(target_session_id=job.notify_session_id, text=text, delivery_mode="sequential")

    def _tail_log(self, log_path: Optional[str], max_bytes: int) -> str:
        if not log_path:
            return ""
        try:
            path = Path(log_path)
            size = path.stat().st_size
            with path.open("rb") as handle:
                handle.seek(max(0, size - max_bytes))
                return handle.read().decode(errors="replace").strip()
        except Exception:
            return ""

    def _ensure_resource_sampler(self) -> None:
        if not self.resource_sampling_enabled or not self._started:
            return
        has_active = any(job.state in ACTIVE_STATES for job in self._jobs.values())
        if has_active and (self._resource_sampler_task is None or self._resource_sampler_task.done()):
            self._resource_sampler_task = asyncio.create_task(self._resource_sampler_loop())

    async def _resource_sampler_loop(self) -> None:
        try:
            while True:
                jobs_snapshot = list(self._jobs.values())
                if not any(job.state in ACTIVE_STATES for job in jobs_snapshot):
                    return
                await asyncio.to_thread(self._record_resource_sample, jobs_snapshot)
                await asyncio.sleep(self.resource_sampling_interval_seconds)
        except asyncio.CancelledError:
            raise

    def _record_resource_sample(self, jobs_snapshot: list[QueueJob]) -> None:
        pending = self._counts_by_type("pending", jobs_snapshot)
        running = self._counts_by_type("running", jobs_snapshot)
        memory = {"free_bytes": self._read_free_memory_bytes()}
        cpu = self._read_cpu_sample(jobs_snapshot)
        with self._connect() as conn:
            conn.execute(
                """
                INSERT INTO queue_resource_samples
                (sampled_at, pending_by_type_json, running_by_type_json, total_running, memory_json, cpu_json, gpu_json)
                VALUES (?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    datetime.now().isoformat(),
                    json.dumps(pending),
                    json.dumps(running),
                    sum(running.values()),
                    json.dumps(memory),
                    json.dumps(cpu),
                    None,
                ),
            )

    def _counts_by_type(self, state: str, jobs_snapshot: list[QueueJob]) -> dict[str, int]:
        return {
            job_type: sum(1 for job in jobs_snapshot if job.state == state and job.type == job_type)
            for job_type in self.type_config
        }

    def _read_cpu_sample(self, jobs_snapshot: list[QueueJob]) -> dict[str, Any]:
        pids = [str(job.pid) for job in jobs_snapshot if job.state == "running" and job.pid]
        sample: dict[str, Any] = {"loadavg": os.getloadavg() if hasattr(os, "getloadavg") else None}
        if not pids:
            return sample
        try:
            output = subprocess.check_output(["ps", "-o", "pid=,%cpu=", "-p", ",".join(pids)], text=True, timeout=1)
            sample["processes"] = output.strip()
        except Exception:
            sample["processes"] = None
        return sample
