"""Unit tests for CLI transport timeout behavior."""

from unittest.mock import patch

from src.cli.client import _read_api_timeout, _read_mutation_api_timeout


def test_read_api_timeout_defaults_to_five_seconds():
    with patch.dict("os.environ", {}, clear=False):
        assert _read_api_timeout() == 5.0


def test_read_api_timeout_uses_positive_env_override():
    with patch.dict("os.environ", {"SM_API_TIMEOUT": "7.5"}, clear=False):
        assert _read_api_timeout() == 7.5


def test_read_api_timeout_rejects_invalid_or_non_positive_values():
    with patch.dict("os.environ", {"SM_API_TIMEOUT": "bogus"}, clear=False):
        assert _read_api_timeout() == 5.0

    with patch.dict("os.environ", {"SM_API_TIMEOUT": "0"}, clear=False):
        assert _read_api_timeout() == 5.0


def test_read_mutation_api_timeout_defaults_to_fifteen_seconds():
    with patch.dict("os.environ", {}, clear=False):
        assert _read_mutation_api_timeout() == 15.0


def test_read_mutation_api_timeout_uses_positive_env_override():
    with patch.dict("os.environ", {"SM_MUTATION_API_TIMEOUT": "12.5"}, clear=False):
        assert _read_mutation_api_timeout() == 12.5


def test_read_mutation_api_timeout_respects_higher_general_timeout():
    with patch.dict("os.environ", {"SM_API_TIMEOUT": "20"}, clear=False):
        assert _read_mutation_api_timeout() == 20.0
