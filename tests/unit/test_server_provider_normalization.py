"""Unit tests for provider normalization in API layer."""

from fastapi import HTTPException
import pytest

from src.server import _normalize_provider


def test_normalize_provider_accepts_codex_fork_aliases():
    assert _normalize_provider("codex-fork") == "codex-fork"
    assert _normalize_provider("codex_fork") == "codex-fork"
    assert _normalize_provider("codexfork") == "codex-fork"


def test_normalize_provider_rejects_unknown():
    with pytest.raises(HTTPException) as exc_info:
        _normalize_provider("unknown-provider")
    assert exc_info.value.status_code == 400
