import json
from pathlib import Path

import pytest
import yaml

from scripts.rust_migration.shadow_config import (
    main,
    prepare_shadow_config,
    render_text_result,
)


def test_shadow_config_dry_run_appends_block_without_writing(tmp_path):
    config = tmp_path / "config.yaml"
    original = "server:\n  host: 127.0.0.1\n"
    config.write_text(original, encoding="utf-8")

    result = prepare_shadow_config(config_path=config)

    assert result["status"] == "dry_run"
    assert result["action"] == "append"
    assert result["changed"] is True
    assert result["backup_path"] is None
    assert config.read_text(encoding="utf-8") == original
    assert "+rust_shadow:" in result["diff"]
    assert "+  enabled: true" in result["diff"]
    assert "Dry run only" in render_text_result(result)


def test_shadow_config_write_creates_backup_and_preserves_unrelated_content(tmp_path):
    config = tmp_path / "config.yaml"
    original = "# keep this comment\nserver:\n  port: 8420\n"
    config.write_text(original, encoding="utf-8")

    result = prepare_shadow_config(
        config_path=config,
        endpoint="http://127.0.0.1:8421/__shadow/http",
        ledger_path=str(tmp_path / "rust_shadow.jsonl"),
        secret="shared-secret",
        write=True,
    )

    assert result["status"] == "written"
    assert result["backup_path"]
    backup_path = Path(result["backup_path"])
    assert backup_path.exists()
    assert backup_path.read_text(encoding="utf-8") == original
    updated = config.read_text(encoding="utf-8")
    assert "# keep this comment" in updated
    parsed = yaml.safe_load(updated)
    assert parsed["server"]["port"] == 8420
    assert parsed["rust_shadow"] == {
        "enabled": True,
        "endpoint": "http://127.0.0.1:8421/__shadow/http",
        "ledger_path": str(tmp_path / "rust_shadow.jsonl"),
        "secret": "shared-secret",
        "timeout_seconds": 0.5,
        "max_body_bytes": 65536,
    }


def test_shadow_config_replaces_existing_top_level_block(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text(
        "server:\n"
        "  port: 8420\n"
        "rust_shadow:\n"
        "  enabled: false\n"
        "  endpoint: \"http://old-shadow\"\n"
        "  timeout_seconds: 9\n"
        "# keep top-level comment\n"
        "after:\n"
        "  value: true\n",
        encoding="utf-8",
    )

    result = prepare_shadow_config(
        config_path=config,
        endpoint="http://new-shadow/__shadow/http",
        ledger_path="/tmp/new-shadow.jsonl",
        write=True,
    )

    assert result["action"] == "replace"
    parsed = yaml.safe_load(config.read_text(encoding="utf-8"))
    assert parsed["server"]["port"] == 8420
    assert parsed["after"]["value"] is True
    assert "# keep top-level comment" in config.read_text(encoding="utf-8")
    assert parsed["rust_shadow"]["enabled"] is True
    assert parsed["rust_shadow"]["endpoint"] == "http://new-shadow/__shadow/http"
    assert parsed["rust_shadow"]["ledger_path"] == "/tmp/new-shadow.jsonl"
    assert "secret" not in parsed["rust_shadow"]


def test_shadow_config_replaces_inline_or_commented_top_level_rust_shadow(tmp_path):
    cases = {
        "commented": "server: {}\nrust_shadow: # existing shadow config\n  enabled: false\nafter: {}\n",
        "inline": "server: {}\nrust_shadow: {enabled: false}\nafter: {}\n",
        "quoted": 'server: {}\n"rust_shadow": {enabled: false}\nafter: {}\n',
    }

    for name, original in cases.items():
        config = tmp_path / f"{name}.yaml"
        config.write_text(original, encoding="utf-8")

        result = prepare_shadow_config(config_path=config, write=True)

        updated = config.read_text(encoding="utf-8")
        parsed = yaml.safe_load(updated)
        assert result["action"] == "replace"
        assert parsed["server"] == {}
        assert parsed["after"] == {}
        assert parsed["rust_shadow"]["enabled"] is True
        assert parsed["rust_shadow"]["endpoint"] == "http://127.0.0.1:8421/__shadow/http"
        assert updated.count("rust_shadow:") == 1


def test_shadow_config_replaces_comments_that_resume_rust_shadow_mapping(tmp_path):
    config = tmp_path / "config.yaml"
    config.write_text(
        "server: {}\n"
        "rust_shadow:\n"
        "  enabled: false\n"
        "# comment between rust_shadow child keys\n"
        "  endpoint: \"http://old-shadow\"\n"
        "  ledger_path: \"/tmp/old-shadow.jsonl\"\n"
        "# keep top-level comment\n"
        "after: {}\n",
        encoding="utf-8",
    )

    result = prepare_shadow_config(config_path=config, write=True)

    updated = config.read_text(encoding="utf-8")
    parsed = yaml.safe_load(updated)
    assert result["action"] == "replace"
    assert parsed["server"] == {}
    assert parsed["after"] == {}
    assert parsed["rust_shadow"]["endpoint"] == "http://127.0.0.1:8421/__shadow/http"
    assert parsed["rust_shadow"]["ledger_path"] == "~/.local/share/claude-sessions/rust_shadow.jsonl"
    assert "old-shadow" not in updated
    assert "# comment between rust_shadow child keys" not in updated
    assert "# keep top-level comment" in updated


def test_shadow_config_rejects_duplicate_top_level_rust_shadow_sections(tmp_path):
    config = tmp_path / "config.yaml"
    original = (
        "server: {}\n"
        "rust_shadow:\n"
        "  endpoint: \"http://first-shadow\"\n"
        "after: {}\n"
        "rust_shadow: {endpoint: \"http://later-shadow\"}\n"
    )
    config.write_text(original, encoding="utf-8")

    with pytest.raises(ValueError, match="multiple top-level rust_shadow sections"):
        prepare_shadow_config(config_path=config, write=True)

    assert config.read_text(encoding="utf-8") == original
    assert not list(tmp_path.glob("config.yaml.shadow-backup-*"))


def test_shadow_config_cli_json_reports_duplicate_section_error(tmp_path, capsys):
    config = tmp_path / "config.yaml"
    original = "rust_shadow: {}\nserver: {}\nrust_shadow: {}\n"
    config.write_text(original, encoding="utf-8")

    assert main(["--config", str(config), "--write", "--json"]) == 2
    output = json.loads(capsys.readouterr().out)

    assert output["status"] == "error"
    assert "multiple top-level rust_shadow sections" in output["error"]
    assert config.read_text(encoding="utf-8") == original


def test_shadow_config_cli_json_dry_run(tmp_path, capsys):
    config = tmp_path / "config.yaml"
    config.write_text("server: {}\n", encoding="utf-8")

    assert main(["--config", str(config), "--json"]) == 0
    output = json.loads(capsys.readouterr().out)

    assert output["status"] == "dry_run"
    assert output["write"] is False
    assert output["config_path"] == str(config)
    assert config.read_text(encoding="utf-8") == "server: {}\n"
