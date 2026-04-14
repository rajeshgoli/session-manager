from pathlib import Path
from tempfile import TemporaryDirectory
from datetime import UTC, datetime, timedelta
from unittest.mock import MagicMock

from fastapi.testclient import TestClient

from src.models import Session, SessionStatus
from src.server import create_app


def _session(session_id: str, name: str, provider: str, working_dir: str, tokens_used: int = 0) -> Session:
    session = Session(
        id=session_id,
        name=name,
        working_dir=working_dir,
        tmux_session=f"{provider}-{session_id}",
        status=SessionStatus.RUNNING,
        provider=provider,
        log_file=f"/tmp/{session_id}.log",
    )
    session.tokens_used = tokens_used
    return session


def test_client_analytics_summary_reports_live_metrics():
    with TemporaryDirectory() as temp_dir:
        temp_root = Path(temp_dir)
        message_queue_db = temp_root / "message_queue.db"
        server_log = temp_root / "session-manager.log"
        now = datetime.now(UTC)
        start_time = now - timedelta(hours=2)
        spawn_a = now - timedelta(hours=1, minutes=50)
        self_heal = now - timedelta(hours=1, minutes=40)
        spawn_b = now - timedelta(hours=50)
        send_a = now - timedelta(hours=1, minutes=30)
        send_b = now - timedelta(minutes=30)
        track_send = now - timedelta(minutes=15)

        message_queue_db.write_bytes(b"")
        server_log.write_text(
            "\n".join(
                [
                    f"{start_time.strftime('%Y-%m-%d %H:%M:%S')},000 - __main__ - INFO - Starting Claude Session Manager...",
                    f"{spawn_a.strftime('%Y-%m-%d %H:%M:%S')},000 - src.session_manager - INFO - Created session claude-11111111 (id=11111111)",
                    f"{self_heal.strftime('%Y-%m-%d %H:%M:%S')},000 - src.infra_supervisor - WARNING - Recovered android attach sshd via launchctl (bootstrap, kickstart)",
                    f"{spawn_b.strftime('%Y-%m-%d %H:%M:%S')},000 - src.session_manager - INFO - Created session codex-fork-22222222 (id=22222222)",
                ]
            )
        )

        import sqlite3

        with sqlite3.connect(str(message_queue_db)) as conn:
            conn.executescript(
                """
                CREATE TABLE message_queue (
                    id TEXT PRIMARY KEY,
                    target_session_id TEXT NOT NULL,
                    sender_session_id TEXT,
                    sender_name TEXT,
                    text TEXT NOT NULL,
                    delivery_mode TEXT DEFAULT 'sequential',
                    queued_at TIMESTAMP NOT NULL,
                    timeout_at TIMESTAMP,
                    notify_on_delivery INTEGER DEFAULT 0,
                    notify_after_seconds INTEGER,
                    delivered_at TIMESTAMP,
                    notify_on_stop INTEGER DEFAULT 0,
                    remind_soft_threshold INTEGER,
                    remind_hard_threshold INTEGER,
                    parent_session_id TEXT,
                    message_category TEXT DEFAULT NULL,
                    remind_cancel_on_reply_session_id TEXT,
                    from_sm_send INTEGER DEFAULT 0
                );
                CREATE TABLE remind_registrations (
                    id TEXT PRIMARY KEY,
                    target_session_id TEXT NOT NULL UNIQUE,
                    soft_threshold_seconds INTEGER NOT NULL,
                    hard_threshold_seconds INTEGER NOT NULL,
                    registered_at TIMESTAMP NOT NULL,
                    last_reset_at TIMESTAMP NOT NULL,
                    soft_fired INTEGER DEFAULT 0,
                    is_active INTEGER DEFAULT 1,
                    cancel_on_reply_session_id TEXT,
                    tracked_status_nudge_fired INTEGER DEFAULT 0
                );
                """
            )
            conn.execute(
                """
                INSERT INTO message_queue
                (id, target_session_id, text, queued_at, message_category, from_sm_send)
                VALUES
                ('m1', '11111111', 'msg', ?, NULL, 1),
                ('m2', '11111111', 'msg', ?, NULL, 1),
                ('m3', '22222222', 'track', ?, 'track_remind', 0)
                """
                ,
                (send_a.isoformat(), send_b.isoformat(), track_send.isoformat()),
            )
            conn.execute(
                """
                INSERT INTO remind_registrations
                (id, target_session_id, soft_threshold_seconds, hard_threshold_seconds, registered_at, last_reset_at, soft_fired, is_active, cancel_on_reply_session_id)
                VALUES
                ('r1', '11111111', 300, 600, ?, ?, 1, 1, 'owner-1'),
                ('r2', '22222222', 300, 600, ?, ?, 0, 1, 'owner-2')
                """
                ,
                (
                    start_time.isoformat(),
                    start_time.isoformat(),
                    start_time.isoformat(),
                    start_time.isoformat(),
                ),
            )
            conn.commit()

        session_a = _session("11111111", "claude-11111111", "claude", "/tmp/repo-a", tokens_used=1200)
        session_b = _session("22222222", "codex-fork-22222222", "codex-fork", "/tmp/repo-b", tokens_used=800)

        manager = MagicMock()
        manager.list_sessions.return_value = [session_a, session_b]
        manager.get_effective_session_name.side_effect = lambda current: current.name
        manager.get_activity_state.side_effect = lambda current: {
            "11111111": "working",
            "22222222": "thinking",
        }[current.id]

        app = create_app(
            session_manager=manager,
            config={
                "paths": {
                    "message_queue_db": str(message_queue_db),
                    "server_log_file": str(server_log),
                }
            },
        )
        app.state.infra_supervisor = MagicMock()
        app.state.infra_supervisor.snapshot.return_value = {
            "android_sshd": {"status": "ok", "message": "ready"},
            "tmux_base": {"status": "warning", "message": "recreated"},
        }
        client = TestClient(app)

        response = client.get("/client/analytics/summary")

        assert response.status_code == 200
        payload = response.json()
        assert payload["kpis"]["active_sessions"]["value"] == 2
        assert payload["kpis"]["sends_24h"]["value"] == 2
        assert payload["kpis"]["spawns_24h"]["value"] == 1
        assert payload["kpis"]["active_tracks"]["value"] == 2
        assert payload["kpis"]["overdue_tracks"]["value"] == 1
        assert payload["totals"]["tokens_live"] == 2000
        assert payload["reliability"]["restart_count_24h"] == 1
        assert payload["reliability"]["self_heal_count_24h"] == 1
        assert payload["state_distribution"] == [
            {"key": "working", "label": "working", "count": 1},
            {"key": "thinking", "label": "thinking", "count": 1},
            {"key": "waiting", "label": "waiting", "count": 0},
            {"key": "idle", "label": "idle", "count": 0},
        ]
        assert payload["provider_distribution"][0]["label"] in {"claude", "codex-fork"}
        assert len(payload["throughput"]) == 12
        assert payload["health_checks"] == [
            {"key": "android_sshd", "label": "Android attach SSHD", "status": "ok", "message": "ready"},
            {"key": "tmux_base", "label": "tmux base", "status": "warning", "message": "recreated"},
        ]
