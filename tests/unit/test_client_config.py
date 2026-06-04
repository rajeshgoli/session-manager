from pathlib import Path

from src.cli.client import (
    DEFAULT_API_URL,
    SessionManagerClient,
    read_client_config_api_url,
    resolve_api_url,
)


def test_read_client_config_api_url_from_top_level_key(tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text('api_url: "http://primary.example.test:8420/"\n')

    assert read_client_config_api_url(config_path) == "http://primary.example.test:8420"


def test_read_client_config_api_url_from_client_section(tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("client:\n  api_url: https://sm.example.test\n")

    assert read_client_config_api_url(config_path) == "https://sm.example.test"


def test_resolve_api_url_precedence(monkeypatch, tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("api_url: http://config.example.test:8420\n")
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(config_path))
    monkeypatch.setenv("SM_API_URL", "http://env.example.test:8420")

    assert resolve_api_url("http://explicit.example.test:8420") == "http://explicit.example.test:8420"
    assert resolve_api_url() == "http://env.example.test:8420"

    monkeypatch.delenv("SM_API_URL")
    assert resolve_api_url() == "http://config.example.test:8420"


def test_resolve_api_url_falls_back_to_localhost(monkeypatch, tmp_path: Path):
    config_path = tmp_path / "missing.yaml"
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(config_path))
    monkeypatch.delenv("SM_API_URL", raising=False)

    assert resolve_api_url() == DEFAULT_API_URL


def test_session_manager_client_uses_shared_config(monkeypatch, tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("api_url: http://config.example.test:8420\n")
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(config_path))
    monkeypatch.delenv("SM_API_URL", raising=False)

    client = SessionManagerClient()

    assert client.api_url == "http://config.example.test:8420"
