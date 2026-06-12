import json

from scripts.rust_migration.state_backup import (
    build_backup_plan,
    main as state_backup_main,
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


def test_state_backup_dry_run_plans_copyable_existing_stores(tmp_path):
    config, _state_dir = _seed_state_tree(tmp_path)
    backup_root = tmp_path / "backup"

    report = build_backup_plan(config_path=config, output_dir=backup_root)

    assert report["status"] == "planned"
    assert report["mode"] == "dry_run"
    assert report["backup_root"] == str(backup_root)
    assert report["summary"]["blockers"] == 0
    assert report["summary"]["planned"] > 0
    assert not backup_root.exists()
    entries = {entry["store_id"]: entry for entry in report["entries"]}
    assert entries["sessions_state"]["action"] == "copy"
    assert entries["sessions_state"]["destination"] == str(
        backup_root / "stores/sessions_state"
    )
    assert entries["bug_reports_db"]["action"] == "skip"
    assert entries["bug_reports_db"]["skip_reason"] == "missing"


def test_state_backup_execute_copies_files_dirs_and_manifest(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    backup_root = tmp_path / "backup"

    report = build_backup_plan(config_path=config, output_dir=backup_root, execute=True)

    assert report["status"] == "copied"
    assert report["summary"]["blockers"] == 0
    assert report["summary"]["copied"] == report["summary"]["planned"]
    assert (backup_root / "stores/sessions_state").read_text(encoding="utf-8") == (
        state_dir / "sessions.json"
    ).read_text(encoding="utf-8")
    assert (backup_root / "stores/log_dir/server.log").read_text(encoding="utf-8") == "log\n"
    manifest = json.loads((backup_root / "state-backup-manifest.json").read_text())
    assert manifest["status"] == "copied"
    assert manifest["backup_root"] == str(backup_root)
    entries = {entry["store_id"]: entry for entry in manifest["entries"]}
    assert entries["sessions_state"]["backup_size_bytes"] == (
        state_dir / "sessions.json"
    ).stat().st_size
    assert entries["sessions_state"]["backup_sha256"]
    assert entries["log_dir"]["backup_file_count"] == 1
    assert entries["log_dir"]["backup_size_bytes"] == len("log\n")


def test_state_backup_blocks_missing_required_state(tmp_path):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    _write_config(config, state_dir)

    report = build_backup_plan(config_path=config, output_dir=tmp_path / "backup")

    assert report["status"] == "blocked"
    assert any(
        blocker["store_id"] == "sessions_state" and blocker["kind"] == "missing"
        for blocker in report["blockers"]
    )


def test_state_backup_execute_requires_new_output_dir(tmp_path):
    config, _state_dir = _seed_state_tree(tmp_path)
    backup_root = tmp_path / "backup"
    backup_root.mkdir()

    report = build_backup_plan(config_path=config, output_dir=backup_root, execute=True)

    assert report["status"] == "blocked"
    assert {
        "store_id": "backup_root",
        "kind": "backup_root_exists",
        "severity": "blocker",
        "detail": f"backup root already exists: {backup_root}",
    } in report["blockers"]


def test_state_backup_blocks_unsafe_directory_roots(tmp_path):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "sessions.json").write_text("[]\n", encoding="utf-8")
    config.write_text(
        f"""
paths:
  state_file: "{state_dir / 'sessions.json'}"
  log_dir: "/"
""".strip()
        + "\n",
        encoding="utf-8",
    )

    report = build_backup_plan(config_path=config, output_dir=tmp_path / "backup")
    entries = {entry["store_id"]: entry for entry in report["entries"]}

    assert report["status"] == "blocked"
    assert entries["log_dir"]["action"] == "skip"
    assert entries["log_dir"]["skip_reason"] == "preflight_issue"
    assert any(
        blocker["store_id"] == "log_dir"
        and blocker["kind"] == "unsafe_source_root"
        for blocker in report["blockers"]
    )


def test_state_backup_blocks_top_level_symlink_store(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    real_logs = state_dir / "real-logs"
    real_logs.mkdir()
    (real_logs / "server.log").write_text("log\n", encoding="utf-8")
    log_link = state_dir / "log-link"
    log_link.symlink_to(real_logs, target_is_directory=True)
    config.write_text(
        f"""
paths:
  state_file: "{state_dir / 'sessions.json'}"
  log_dir: "{log_link}"
""".strip()
        + "\n",
        encoding="utf-8",
    )

    report = build_backup_plan(config_path=config, output_dir=tmp_path / "backup")
    entries = {entry["store_id"]: entry for entry in report["entries"]}

    assert report["status"] == "blocked"
    assert entries["log_dir"]["action"] == "skip"
    assert entries["log_dir"]["skip_reason"] == "symlink_source"
    assert any(
        blocker["store_id"] == "log_dir" and blocker["kind"] == "symlink_source"
        for blocker in report["blockers"]
    )


def test_state_backup_blocks_output_dir_inside_copied_directory_source(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    backup_root = state_dir / "logs/backup"

    report = build_backup_plan(config_path=config, output_dir=backup_root, execute=True)

    assert report["status"] == "blocked"
    assert not backup_root.exists()
    assert any(
        blocker["store_id"] == "log_dir"
        and blocker["kind"] == "backup_root_inside_source"
        for blocker in report["blockers"]
    )


def test_state_backup_skips_symlink_children_without_following(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    external = tmp_path / "outside-secret"
    external.write_text("do-not-copy\n", encoding="utf-8")
    (state_dir / "logs/link").symlink_to(external)
    backup_root = tmp_path / "backup"

    report = build_backup_plan(config_path=config, output_dir=backup_root, execute=True)

    assert report["status"] == "copied"
    assert not (backup_root / "stores/log_dir/link").exists()
    assert any(
        warning["store_id"] == "log_dir" and warning["kind"] == "skipped_symlink"
        for warning in report["warnings"]
    )


def test_state_backup_text_report_and_cli_json(tmp_path, capsys):
    config, _state_dir = _seed_state_tree(tmp_path)
    backup_root = tmp_path / "backup"
    report = build_backup_plan(config_path=config, output_dir=backup_root)

    text = render_text_report(report)
    assert "Rust state backup plan" in text
    assert "status: planned" in text
    assert "copy  sessions_state" in text
    assert "kind=file" in text
    assert "size_bytes=" in text
    assert "sha256=" in text
    assert "file_count=" in text

    exit_code = state_backup_main(
        ["--config", str(config), "--output-dir", str(backup_root), "--json"]
    )
    rendered = json.loads(capsys.readouterr().out)
    assert exit_code == 0
    assert rendered["status"] == "planned"
