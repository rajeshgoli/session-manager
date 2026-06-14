import errno
import hashlib
import json
import urllib.error

from scripts.rust_migration.final_backup import (
    build_final_backup_report,
    configured_python_health_url,
    main as final_backup_main,
)


def _write_config(path, state_dir, *, host="127.0.0.1", port=8420):
    path.write_text(
        f"""
server:
  host: "{host}"
  port: {port}
paths:
  state_file: "{state_dir / 'sessions.json'}"
  log_dir: "{state_dir / 'logs'}"
sm_send:
  db_path: "{state_dir / 'message_queue.db'}"
response_relay:
  db_path: "{state_dir / 'response_relay.db'}"
tool_logging:
  db_path: "{state_dir / 'tool_usage.db'}"
telegram:
  topic_registry:
    path: "{state_dir / 'telegram_topics.json'}"
email:
  bridge_config: "{state_dir / 'email_send.yaml'}"
queue_runner:
  state_dir: "{state_dir / 'queue-runner'}"
codex_events:
  db_path: "{state_dir / 'codex_events.db'}"
codex_requests:
  db_path: "{state_dir / 'codex_requests.db'}"
codex_observability:
  db_path: "{state_dir / 'codex_observability.db'}"
""".strip()
        + "\n",
        encoding="utf-8",
    )


def _seed_state_tree(tmp_path):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "sessions.json").write_text('[{"id":"fixture"}]\n', encoding="utf-8")
    (state_dir / "message_queue.db").write_text("queue", encoding="utf-8")
    (state_dir / "response_relay.db").write_text("relay", encoding="utf-8")
    (state_dir / "tool_usage.db").write_text("tools", encoding="utf-8")
    (state_dir / "telegram_topics.json").write_text("{}\n", encoding="utf-8")
    (state_dir / "codex_events.db").write_text("events", encoding="utf-8")
    (state_dir / "codex_requests.db").write_text("requests", encoding="utf-8")
    (state_dir / "codex_observability.db").write_text("observability", encoding="utf-8")
    (state_dir / "email_send.yaml").write_text("registered_users: {}\n", encoding="utf-8")
    (state_dir / "logs").mkdir()
    (state_dir / "logs/server.log").write_text("log\n", encoding="utf-8")
    (state_dir / "queue-runner").mkdir()
    (state_dir / "queue-runner/queue_runner.db").write_text("jobs", encoding="utf-8")
    _write_config(config, state_dir)
    return config, state_dir


class _FakeHttpResponse:
    status = 200

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return False

    def getcode(self):
        return self.status


def _reachable_urlopen(*_args, **_kwargs):
    return _FakeHttpResponse()


def _refused_urlopen(*_args, **_kwargs):
    raise urllib.error.URLError(
        ConnectionRefusedError(errno.ECONNREFUSED, "Connection refused")
    )


def _sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def test_configured_python_health_url_uses_server_host_and_port(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    _write_config(config, state_dir, host="127.0.0.1", port=18420)

    assert configured_python_health_url(config) == "http://127.0.0.1:18420/health"


def test_configured_python_health_url_maps_bind_all_host_to_loopback(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    _write_config(config, state_dir, host="0.0.0.0", port=18421)

    assert configured_python_health_url(config) == "http://127.0.0.1:18421/health"


def test_configured_python_health_url_maps_ipv6_bind_all_to_ipv6_loopback(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    _write_config(config, state_dir, host="::", port=18423)

    assert configured_python_health_url(config) == "http://[::1]:18423/health"


def test_final_backup_blocks_when_python_origin_answers(tmp_path):
    config, _state_dir = _seed_state_tree(tmp_path)
    backup_root = tmp_path / "final-backup"

    report = build_final_backup_report(
        config_path=config,
        output_dir=backup_root,
        execute=True,
        stopped_hold_seconds=0,
        urlopen=_reachable_urlopen,
    )

    assert report["status"] == "blocked"
    assert report["backup"] is None
    assert not backup_root.exists()
    assert report["python_origin"]["stopped"] is False
    assert any(
        blocker["store_id"] == "python_origin"
        and blocker["kind"] == "python_origin_reachable"
        for blocker in report["blockers"]
    )


def test_final_backup_default_probe_uses_configured_server_port(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    _write_config(config, state_dir, host="127.0.0.1", port=18422)
    seen_urls = []

    def record_urlopen(url, **_kwargs):
        seen_urls.append(url)
        return _FakeHttpResponse()

    report = build_final_backup_report(
        config_path=config,
        output_dir=tmp_path / "final-backup",
        execute=True,
        stopped_hold_seconds=0,
        urlopen=record_urlopen,
    )

    assert report["status"] == "blocked"
    assert seen_urls == ["http://127.0.0.1:18422/health"]


def test_final_backup_explicit_health_url_overrides_config(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    _write_config(config, state_dir, host="127.0.0.1", port=18422)
    seen_urls = []

    def record_urlopen(url, **_kwargs):
        seen_urls.append(url)
        return _FakeHttpResponse()

    report = build_final_backup_report(
        config_path=config,
        output_dir=tmp_path / "final-backup",
        python_health_url="http://127.0.0.1:19999/health",
        execute=True,
        stopped_hold_seconds=0,
        urlopen=record_urlopen,
    )

    assert report["status"] == "blocked"
    assert seen_urls == ["http://127.0.0.1:19999/health"]


def test_final_backup_executes_after_connection_refused_and_writes_ledger(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    backup_root = tmp_path / "final-backup"
    ledger = tmp_path / "migration-ledger.jsonl"
    seen_urls = []

    def record_refused(url, **_kwargs):
        seen_urls.append(url)
        return _refused_urlopen(url, **_kwargs)

    report = build_final_backup_report(
        config_path=config,
        output_dir=backup_root,
        execute=True,
        ledger_path=ledger,
        record_ledger=True,
        stopped_hold_seconds=0,
        urlopen=record_refused,
    )

    assert report["status"] == "copied"
    assert report["python_origin"]["stopped"] is True
    assert len(report["python_origin"]["attempts"]) == 2
    assert len(seen_urls) == 2
    assert report["backup"]["status"] == "copied"
    assert report["ledger"]["written"] is True
    manifest_path = backup_root / "state-backup-manifest.json"
    assert manifest_path.exists()
    assert (backup_root / "stores/sessions_state").read_text(encoding="utf-8") == (
        state_dir / "sessions.json"
    ).read_text(encoding="utf-8")
    rows = [json.loads(line) for line in ledger.read_text(encoding="utf-8").splitlines()]
    assert len(rows) == 1
    assert rows[0]["kind"] == "final_backup"
    assert rows[0]["python_origin_stopped"] is True
    assert rows[0]["manifest_path"] == str(manifest_path)
    assert rows[0]["manifest_sha256"] == _sha256(manifest_path)
    stores = {row["store_id"]: row for row in rows[0]["store_evidence"]}
    assert stores["sessions_state"]["backup_sha256"] == _sha256(
        backup_root / "stores/sessions_state"
    )
    assert stores["log_dir"]["backup_file_count"] == 1


def test_final_backup_blocks_when_origin_reappears_during_hold(tmp_path):
    config, _state_dir = _seed_state_tree(tmp_path)
    backup_root = tmp_path / "final-backup"
    calls = 0

    def refused_then_reachable(*args, **kwargs):
        nonlocal calls
        calls += 1
        if calls == 1:
            return _refused_urlopen(*args, **kwargs)
        return _reachable_urlopen(*args, **kwargs)

    report = build_final_backup_report(
        config_path=config,
        output_dir=backup_root,
        execute=True,
        stopped_hold_seconds=0,
        urlopen=refused_then_reachable,
    )

    assert report["status"] == "blocked"
    assert report["backup"] is None
    assert not backup_root.exists()
    assert report["python_origin"]["stopped"] is False
    assert len(report["python_origin"]["attempts"]) == 2


def test_final_backup_cli_reports_blocker_for_invalid_health_url(tmp_path, capsys):
    config, _state_dir = _seed_state_tree(tmp_path)
    backup_root = tmp_path / "backup"

    exit_code = final_backup_main(
        [
            "--config",
            str(config),
            "--output-dir",
            str(backup_root),
            "--python-health-url",
            "not a url",
            "--execute",
            "--json",
            "--fail-on-blockers",
        ]
    )
    rendered = json.loads(capsys.readouterr().out)

    assert exit_code == 1
    assert rendered["status"] == "blocked"
    assert rendered["backup"] is None
    assert not backup_root.exists()
