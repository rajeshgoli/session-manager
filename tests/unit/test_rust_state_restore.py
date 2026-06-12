import json

from scripts.rust_migration.state_backup import build_backup_plan
from scripts.rust_migration.state_restore import (
    build_restore_report,
    main as state_restore_main,
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


def _executed_backup(tmp_path):
    config, state_dir = _seed_state_tree(tmp_path)
    backup_root = tmp_path / "backup"
    backup = build_backup_plan(config_path=config, output_dir=backup_root, execute=True)
    assert backup["status"] == "copied"
    return backup_root / "state-backup-manifest.json", backup_root, state_dir


def test_state_restore_verifies_executed_backup(tmp_path):
    manifest_path, _backup_root, _state_dir = _executed_backup(tmp_path)

    report = build_restore_report(manifest_path=manifest_path)

    assert report["status"] == "verified"
    assert report["mode"] == "verify"
    assert report["summary"]["blockers"] == 0
    assert report["summary"]["verified"] > 0
    assert any(warning["store_id"] == "bug_reports_db" for warning in report["warnings"])
    text = render_text_report(report)
    assert "Rust state backup restore report" in text
    assert "status: verified" in text


def test_state_restore_execute_copies_to_rehearsal_root(tmp_path):
    manifest_path, _backup_root, _state_dir = _executed_backup(tmp_path)
    restore_root = tmp_path / "restore"

    report = build_restore_report(
        manifest_path=manifest_path,
        restore_dir=restore_root,
        execute_restore=True,
    )

    assert report["status"] == "restored"
    assert report["summary"]["restored"] == report["summary"]["verified"]
    assert (restore_root / "stores/sessions_state").read_text(encoding="utf-8") == (
        '[{"id":"fixture"}]\n'
    )
    assert (restore_root / "stores/log_dir/server.log").read_text(encoding="utf-8") == "log\n"
    restore_report = json.loads((restore_root / "state-restore-report.json").read_text())
    assert restore_report["status"] == "restored"


def test_state_restore_skips_symlink_children_in_backup_dirs(tmp_path):
    manifest_path, backup_root, _state_dir = _executed_backup(tmp_path)
    external = tmp_path / "external-secret.txt"
    external.write_text("do-not-restore\n", encoding="utf-8")
    (backup_root / "stores/log_dir/secret-link").symlink_to(external)
    restore_root = tmp_path / "restore"

    report = build_restore_report(
        manifest_path=manifest_path,
        restore_dir=restore_root,
        execute_restore=True,
    )

    assert report["status"] == "restored"
    assert not (restore_root / "stores/log_dir/secret-link").exists()
    assert any(
        warning["store_id"] == "log_dir"
        and warning["kind"] == "skipped_backup_symlink"
        for warning in report["warnings"]
    )


def test_state_restore_blocks_missing_manifest(tmp_path):
    report = build_restore_report(manifest_path=tmp_path / "missing.json")

    assert report["status"] == "blocked"
    assert any(
        blocker["store_id"] == "manifest" and blocker["kind"] == "manifest_missing"
        for blocker in report["blockers"]
    )


def test_state_restore_blocks_hash_mismatch(tmp_path):
    manifest_path, backup_root, _state_dir = _executed_backup(tmp_path)
    (backup_root / "stores/sessions_state").write_text("changed\n", encoding="utf-8")

    report = build_restore_report(manifest_path=manifest_path)

    assert report["status"] == "blocked"
    assert any(
        blocker["store_id"] == "sessions_state"
        and blocker["kind"] in {"sha256_mismatch", "size_mismatch"}
        for blocker in report["blockers"]
    )


def test_state_restore_blocks_missing_copied_destination(tmp_path):
    manifest_path, backup_root, _state_dir = _executed_backup(tmp_path)
    (backup_root / "stores/message_queue_db").unlink()

    report = build_restore_report(manifest_path=manifest_path)

    assert report["status"] == "blocked"
    assert any(
        blocker["store_id"] == "message_queue_db"
        and blocker["kind"] == "backup_missing"
        for blocker in report["blockers"]
    )


def test_state_restore_blocks_store_id_path_traversal(tmp_path):
    manifest_path, backup_root, _state_dir = _executed_backup(tmp_path)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    backup_payload = backup_root / "stores/traversal-payload"
    backup_payload.write_text("payload\n", encoding="utf-8")
    manifest["entries"] = [
        {
            "store_id": "../../outside-restore",
            "label": "Traversal",
            "action": "copy",
            "kind": "file",
            "destination": str(backup_payload),
            "source": str(tmp_path / "live-source"),
            "required": False,
            "backup_size_bytes": len("payload\n"),
            "backup_sha256": None,
        }
    ]
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    restore_root = tmp_path / "restore"

    report = build_restore_report(
        manifest_path=manifest_path,
        restore_dir=restore_root,
        execute_restore=True,
    )

    assert report["status"] == "blocked"
    assert not restore_root.exists()
    assert not (tmp_path / "outside-restore").exists()
    assert any(
        blocker["kind"] == "unsafe_store_id"
        and blocker["store_id"] == "../../outside-restore"
        for blocker in report["blockers"]
    )


def test_state_restore_blocks_skipped_required_store(tmp_path):
    manifest_path, _backup_root, _state_dir = _executed_backup(tmp_path)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    for entry in manifest["entries"]:
        if entry["store_id"] == "sessions_state":
            entry["action"] = "skip"
            entry["skip_reason"] = "missing"
            entry["required"] = True
            entry["destination"] = None
            break
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

    report = build_restore_report(manifest_path=manifest_path)

    assert report["status"] == "blocked"
    assert any(
        blocker["store_id"] == "sessions_state"
        and blocker["kind"] == "required_store_skipped"
        for blocker in report["blockers"]
    )


def test_state_restore_blocks_unsafe_restore_roots(tmp_path):
    manifest_path, backup_root, _state_dir = _executed_backup(tmp_path)

    existing = tmp_path / "existing-restore"
    existing.mkdir()
    existing_report = build_restore_report(
        manifest_path=manifest_path,
        restore_dir=existing,
        execute_restore=True,
    )
    assert existing_report["status"] == "blocked"
    assert any(
        blocker["store_id"] == "restore_root"
        and blocker["kind"] == "restore_root_exists"
        for blocker in existing_report["blockers"]
    )

    nested_report = build_restore_report(
        manifest_path=manifest_path,
        restore_dir=backup_root / "restore",
        execute_restore=True,
    )
    assert nested_report["status"] == "blocked"
    assert any(
        blocker["store_id"] == "restore_root"
        and blocker["kind"] == "restore_root_inside_backup"
        for blocker in nested_report["blockers"]
    )

    link_target = tmp_path / "linked-restore-target"
    restore_link = tmp_path / "restore-link"
    restore_link.symlink_to(link_target)
    link_report = build_restore_report(
        manifest_path=manifest_path,
        restore_dir=restore_link,
        execute_restore=True,
    )
    assert link_report["status"] == "blocked"
    assert any(
        blocker["store_id"] == "restore_root"
        and blocker["kind"] == "restore_root_symlink"
        for blocker in link_report["blockers"]
    )


def test_state_restore_cli_json(tmp_path, capsys):
    manifest_path, _backup_root, _state_dir = _executed_backup(tmp_path)

    exit_code = state_restore_main(
        ["--manifest", str(manifest_path), "--json", "--fail-on-blockers"]
    )
    rendered = json.loads(capsys.readouterr().out)

    assert exit_code == 0
    assert rendered["status"] == "verified"
