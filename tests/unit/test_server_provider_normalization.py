import pytest
from fastapi import HTTPException

from src.codex_provider_policy import REMOVED_CODEX_SERVER_ENTRYPOINT_MESSAGE
from src.server import _normalize_provider


def test_normalize_provider_accepts_codex_app_alias():
    assert _normalize_provider("codex-app") == "codex-app"
    assert _normalize_provider("codex_app") == "codex-app"


def test_normalize_provider_rejects_removed_codex_server_entrypoints():
    with pytest.raises(HTTPException) as err:
        _normalize_provider("codex-server")
    assert err.value.status_code == 400
    assert err.value.detail == REMOVED_CODEX_SERVER_ENTRYPOINT_MESSAGE

    with pytest.raises(HTTPException) as err2:
        _normalize_provider("codex-app-server")
    assert err2.value.status_code == 400
    assert err2.value.detail == REMOVED_CODEX_SERVER_ENTRYPOINT_MESSAGE

