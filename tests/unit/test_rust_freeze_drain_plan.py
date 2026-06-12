import json

from scripts.rust_migration.freeze_drain_plan import (
    build_freeze_drain_plan,
    main as freeze_drain_main,
    render_text_report,
)


def _write_config(path, state_dir):
    path.write_text(
        f"""
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
    (state_dir / "sessions.json").write_text("[]\n", encoding="utf-8")
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


def test_freeze_drain_plan_is_dry_run_and_not_active(tmp_path):
    config, _state_dir = _seed_state_tree(tmp_path)

    report = build_freeze_drain_plan(config_path=config)

    assert report["status"] == "planned"
    assert report["mode"] == "dry_run"
    assert report["freeze_active"] is False
    assert report["rust_ownership_active"] is False
    assert report["summary"]["writer_families"] >= 12
    assert report["summary"]["blockers"] == 0


def test_freeze_drain_plan_covers_required_writer_families(tmp_path):
    config, _state_dir = _seed_state_tree(tmp_path)

    report = build_freeze_drain_plan(config_path=config)
    families = {row["id"]: row for row in report["writer_families"]}

    for family_id in {
        "tool_audit_and_telemetry",
        "native_bug_reports",
        "email_human_delivery",
        "codex_state",
        "queue_runner",
        "locks_worktrees",
        "nodes",
    }:
        assert family_id in families

    assert "tool_usage_db" in families["tool_audit_and_telemetry"]["store_ids"]
    assert "telegram_topics_json" in families["tool_audit_and_telemetry"]["store_ids"]
    assert "bug_reports_db" in families["native_bug_reports"]["store_ids"]
    assert "email_bridge_config" in families["email_human_delivery"]["store_ids"]
    assert "codex_events_db" in families["codex_state"]["store_ids"]
    assert "queue_runner_state_dir" in families["queue_runner"]["store_ids"]
    assert "sessions_state" in families["locks_worktrees"]["store_ids"]


def test_freeze_drain_text_report_names_actions(tmp_path):
    config, _state_dir = _seed_state_tree(tmp_path)

    text = render_text_report(build_freeze_drain_plan(config_path=config))

    assert "Rust freeze/drain plan" in text
    assert "freeze_active: false" in text
    assert "tool_audit_and_telemetry" in text
    assert "freeze:" in text
    assert "drain:" in text


def test_freeze_drain_record_plan_writes_plan_only_ledger(tmp_path):
    config, _state_dir = _seed_state_tree(tmp_path)
    ledger = tmp_path / "migration-ledger.jsonl"

    report = build_freeze_drain_plan(
        config_path=config,
        ledger_path=ledger,
        record_plan=True,
    )

    assert report["status"] == "planned"
    assert report["ledger"]["written"] is True
    rows = [json.loads(line) for line in ledger.read_text(encoding="utf-8").splitlines()]
    assert len(rows) == 1
    assert rows[0]["kind"] == "freeze_drain_plan"
    assert rows[0]["freeze_active"] is False
    assert rows[0]["rust_ownership_active"] is False
    assert rows[0]["writer_families"]


def test_freeze_drain_record_plan_blocks_unsafe_ledger_path(tmp_path):
    config, _state_dir = _seed_state_tree(tmp_path)
    ledger_link = tmp_path / "ledger-link"
    ledger_target = tmp_path / "target-ledger.jsonl"
    ledger_link.symlink_to(ledger_target)

    report = build_freeze_drain_plan(
        config_path=config,
        ledger_path=ledger_link,
        record_plan=True,
    )

    assert report["status"] == "blocked"
    assert report["ledger"]["written"] is False
    assert any(
        blocker["store_id"] == "ledger" and blocker["kind"] == "ledger_is_symlink"
        for blocker in report["blockers"]
    )
    assert not ledger_target.exists()


def test_freeze_drain_blocks_missing_required_preflight_state(tmp_path):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    _write_config(config, state_dir)

    report = build_freeze_drain_plan(config_path=config)

    assert report["status"] == "blocked"
    assert any(
        blocker["store_id"] == "sessions_state" and blocker["kind"] == "missing"
        for blocker in report["blockers"]
    )


def test_freeze_drain_cli_json_and_ledger(tmp_path, capsys):
    config, _state_dir = _seed_state_tree(tmp_path)
    ledger = tmp_path / "ledger.jsonl"

    exit_code = freeze_drain_main(
        [
            "--config",
            str(config),
            "--record-plan",
            "--ledger",
            str(ledger),
            "--json",
            "--fail-on-blockers",
        ]
    )
    rendered = json.loads(capsys.readouterr().out)

    assert exit_code == 0
    assert rendered["ledger"]["written"] is True
    assert ledger.exists()
