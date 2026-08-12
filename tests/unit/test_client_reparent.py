"""Unit tests for consent-based reparent operator endpoints."""

from unittest.mock import patch

from src.cli.client import MUTATION_API_TIMEOUT, SessionManagerClient


def _client() -> SessionManagerClient:
    return SessionManagerClient(api_url="http://127.0.0.1:8420")


def test_list_reparent_requests_preserves_rows():
    client = _client()
    with patch.object(
        client,
        "_request_with_status",
        return_value=({"requests": [{"id": "request1"}]}, 200, False),
    ) as request:
        result = client.list_reparent_requests()

    assert result["ok"] is True
    assert result["requests"] == [{"id": "request1"}]
    request.assert_called_once_with("GET", "/reparent-requests")


def test_human_reparent_decision_targets_exact_request():
    client = _client()
    with patch.object(
        client,
        "_request_with_status",
        return_value=({"id": "request/1", "status": "applied"}, 200, False),
    ) as request:
        result = client.decide_reparent_request_as_human("request/1", approve=True)

    assert result["ok"] is True
    request.assert_called_once_with(
        "POST",
        "/reparent-requests/request%2F1/human-approve",
        {},
        timeout=MUTATION_API_TIMEOUT,
    )


def test_reparent_repair_preserves_stage_action():
    client = _client()
    with patch.object(
        client,
        "_request_with_status",
        return_value=({"id": "request1", "status": "repaired"}, 200, False),
    ) as request:
        result = client.repair_reparent_request("request1", "rollback_precommit")

    assert result["ok"] is True
    request.assert_called_once_with(
        "POST",
        "/reparent-requests/request1/repair",
        {"action": "rollback_precommit"},
        timeout=MUTATION_API_TIMEOUT,
    )
