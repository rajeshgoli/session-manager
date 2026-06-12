import json

from scripts.rust_migration.state_preflight import (
    REPO_ROOT,
    build_state_preflight_report,
    main as state_preflight_main,
    render_text_report,
)


def _write_config(path, state_dir, extra=""):
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
paths_extra_unused: true
{extra}
""".strip()
        + "\n",
        encoding="utf-8",
    )


def test_state_preflight_passes_with_required_state_and_warns_optional_missing(tmp_path):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "sessions.json").write_text("[]\n", encoding="utf-8")
    (state_dir / "message_queue.db").write_text("queue", encoding="utf-8")
    (state_dir / "logs").mkdir()
    _write_config(config, state_dir)

    report = build_state_preflight_report(config_path=config)

    assert report["status"] == "passed"
    assert report["summary"]["stores"] == 17
    assert report["summary"]["blockers"] == 0
    assert report["summary"]["warnings"] > 0
    stores = {row["id"]: row for row in report["stores"]}
    assert stores["sessions_state"]["exists"] is True
    assert stores["sessions_state"]["sha256"]
    assert stores["message_queue_db"]["exists"] is True
    assert stores["message_queue_db"]["copyable"] is True
    assert stores["response_relay_db"]["exists"] is False


def test_state_preflight_blocks_missing_required_session_state(tmp_path):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    _write_config(config, state_dir)

    report = build_state_preflight_report(config_path=config)

    assert report["status"] == "blocked"
    assert {
        "store_id": "sessions_state",
        "kind": "missing",
        "severity": "blocker",
        "detail": f"path does not exist: {state_dir / 'sessions.json'}",
    } in report["blockers"]


def test_state_preflight_uses_custom_state_file_sibling_defaults(tmp_path):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "sessions.json").write_text("[]\n", encoding="utf-8")
    config.write_text(
        f"""
paths:
  state_file: "{state_dir / 'sessions.json'}"
""".strip()
        + "\n",
        encoding="utf-8",
    )

    report = build_state_preflight_report(config_path=config)
    stores = {row["id"]: row for row in report["stores"]}

    assert stores["codex_events_db"]["path"] == str(state_dir / "codex_events.db")
    assert stores["codex_requests_db"]["path"] == str(state_dir / "codex_requests.db")
    assert stores["codex_observability_db"]["path"] == str(
        state_dir / "codex_observability.db"
    )
    assert stores["queue_runner_state_dir"]["path"] == str(state_dir / "queue-runner")


def test_state_preflight_reports_wrong_kind_as_blocker(tmp_path):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "sessions.json").mkdir()
    _write_config(config, state_dir)

    report = build_state_preflight_report(config_path=config)

    assert report["status"] == "blocked"
    assert any(
        issue["store_id"] == "sessions_state" and issue["kind"] == "wrong_kind"
        for issue in report["blockers"]
    )


def test_state_preflight_text_report_includes_warnings(tmp_path):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "sessions.json").write_text("[]\n", encoding="utf-8")
    _write_config(config, state_dir)

    text = render_text_report(build_state_preflight_report(config_path=config))

    assert "Rust state ownership preflight" in text
    assert "status: passed" in text
    assert "Warnings:" in text


def test_state_preflight_cli_json_fails_on_blockers(tmp_path, capsys):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    _write_config(config, state_dir)

    exit_code = state_preflight_main(
        ["--config", str(config), "--json", "--fail-on-blockers"]
    )
    report = json.loads(capsys.readouterr().out)

    assert exit_code == 1
    assert report["status"] == "blocked"
    assert report["summary"]["blockers"] == 1


def test_state_preflight_repo_owned_defaults_do_not_follow_supplied_config_dir(tmp_path):
    config = tmp_path / "rehearsal" / "config.yaml"
    config.parent.mkdir()
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "sessions.json").write_text("[]\n", encoding="utf-8")
    config.write_text(
        f"""
paths:
  state_file: "{state_dir / 'sessions.json'}"
""".strip()
        + "\n",
        encoding="utf-8",
    )

    report = build_state_preflight_report(config_path=config)
    stores = {row["id"]: row for row in report["stores"]}

    assert stores["bug_reports_db"]["path"] == str(REPO_ROOT / "data/bug_reports.db")
    assert stores["app_artifacts_dir"]["path"] == str(REPO_ROOT / "data/apps")
    assert stores["email_bridge_config"]["path"] == str(
        REPO_ROOT / "config/email_send.yaml"
    )


def test_state_preflight_reports_client_config_env_override(tmp_path, monkeypatch):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    client_config = tmp_path / "client.yaml"
    state_dir.mkdir()
    (state_dir / "sessions.json").write_text("[]\n", encoding="utf-8")
    client_config.write_text("api_url: http://127.0.0.1:8420\n", encoding="utf-8")
    _write_config(config, state_dir)
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(client_config))

    report = build_state_preflight_report(config_path=config)
    stores = {row["id"]: row for row in report["stores"]}

    assert report["inputs"]["client_config"] == str(client_config)
    assert stores["client_yaml"]["path"] == str(client_config)
    assert stores["client_yaml"]["exists"] is True
    assert stores["client_yaml"]["copyable"] is True


def test_state_preflight_reports_client_config_xdg_default(tmp_path, monkeypatch):
    config = tmp_path / "config.yaml"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "sessions.json").write_text("[]\n", encoding="utf-8")
    _write_config(config, state_dir)
    monkeypatch.delenv("SM_CLIENT_CONFIG", raising=False)
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path / "xdg"))

    report = build_state_preflight_report(config_path=config)
    stores = {row["id"]: row for row in report["stores"]}

    expected = tmp_path / "xdg/session-manager/client.yaml"
    assert report["inputs"]["client_config"] == str(expected)
    assert stores["client_yaml"]["path"] == str(expected)
    assert stores["client_yaml"]["exists"] is False
    assert stores["client_yaml"]["issues"][0]["severity"] == "warning"
