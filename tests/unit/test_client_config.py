from pathlib import Path

import pytest

from src.cli.client import (
    ClientConfigError,
    DEFAULT_API_URL,
    SessionManagerClient,
    read_client_config_api_url,
    read_client_config_default_node,
    resolve_api_url,
    resolve_default_node,
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


def test_resolve_api_url_rejects_invalid_explicit_url(monkeypatch, tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("api_url: http://config.example.test:8420\n")
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(config_path))
    monkeypatch.delenv("SM_API_URL", raising=False)

    with pytest.raises(ClientConfigError, match="explicit api_url must be http"):
        resolve_api_url("config.example.test:8420")


def test_resolve_api_url_rejects_invalid_env_url(monkeypatch, tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("api_url: http://config.example.test:8420\n")
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(config_path))
    monkeypatch.setenv("SM_API_URL", "env.example.test:8420")

    with pytest.raises(ClientConfigError, match="SM_API_URL must be http"):
        resolve_api_url()


def test_resolve_api_url_falls_back_to_localhost(monkeypatch, tmp_path: Path):
    config_path = tmp_path / "missing.yaml"
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(config_path))
    monkeypatch.delenv("SM_API_URL", raising=False)

    assert resolve_api_url() == DEFAULT_API_URL


def test_present_malformed_client_config_raises(monkeypatch, tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("api_url: [unterminated\n")
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(config_path))
    monkeypatch.delenv("SM_API_URL", raising=False)

    with pytest.raises(ClientConfigError, match="Invalid Session Manager client config"):
        resolve_api_url()


def test_present_non_mapping_client_config_raises(tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("- api_url: http://primary.example.test:8420\n")

    with pytest.raises(ClientConfigError, match="expected a YAML mapping"):
        read_client_config_api_url(config_path)


def test_invalid_client_config_api_url_raises(tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("api_url: studio.local:8420\n")

    with pytest.raises(ClientConfigError, match="api_url must be http"):
        read_client_config_api_url(config_path)


def test_read_client_config_default_node_from_top_level_key(tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("default_node: macbook\n")

    assert read_client_config_default_node(config_path) == "macbook"


def test_read_client_config_default_node_from_client_section(tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("client:\n  default_node: worker\n")

    assert read_client_config_default_node(config_path) == "worker"


def test_resolve_default_node_precedence(monkeypatch, tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("default_node: config-node\n")
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(config_path))
    monkeypatch.setenv("SM_DEFAULT_NODE", "env-node")

    assert resolve_default_node("explicit-node") == "explicit-node"
    assert resolve_default_node() == "env-node"

    monkeypatch.delenv("SM_DEFAULT_NODE")
    assert resolve_default_node() == "config-node"


def test_invalid_client_config_default_node_raises(tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("default_node: ''\n")

    with pytest.raises(ClientConfigError, match="default_node must be a non-empty string"):
        read_client_config_default_node(config_path)


def test_session_manager_client_uses_shared_config(monkeypatch, tmp_path: Path):
    config_path = tmp_path / "client.yaml"
    config_path.write_text("api_url: http://config.example.test:8420\ndefault_node: macbook\n")
    monkeypatch.setenv("SM_CLIENT_CONFIG", str(config_path))
    monkeypatch.delenv("SM_API_URL", raising=False)
    monkeypatch.delenv("SM_DEFAULT_NODE", raising=False)

    client = SessionManagerClient()

    assert client.api_url == "http://config.example.test:8420"
    assert client.default_node == "macbook"
