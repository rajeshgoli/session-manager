import json
from pathlib import Path

from fastapi import Request
from fastapi.responses import StreamingResponse
from fastapi.testclient import TestClient

from src.server import create_app


class _FakeShadowResponse:
    status_code = 200
    text = ""

    def json(self):
        return {
            "schema_version": 1,
            "support_status": "implemented_read",
            "comparison": "match",
        }


class _FakeAsyncClient:
    calls = []

    def __init__(self, *, timeout):
        self.timeout = timeout

    async def __aenter__(self):
        return self

    async def __aexit__(self, exc_type, exc, tb):
        return False

    async def post(self, endpoint, json):
        self.calls.append({"endpoint": endpoint, "json": json, "timeout": self.timeout})
        return _FakeShadowResponse()


class _FailingAsyncClient(_FakeAsyncClient):
    async def post(self, endpoint, json):
        self.calls.append({"endpoint": endpoint, "json": json, "timeout": self.timeout})
        raise RuntimeError("rust shadow offline")


def _shadow_config(tmp_path: Path, **overrides):
    config = {
        "enabled": True,
        "endpoint": "http://rust-shadow.test/__shadow/http",
        "ledger_path": str(tmp_path / "rust_shadow.jsonl"),
        "await_completion_for_tests": True,
    }
    config.update(overrides)
    return {"rust_shadow": config}


def test_rust_shadow_is_disabled_by_default(monkeypatch):
    _FakeAsyncClient.calls.clear()
    monkeypatch.setattr("src.rust_shadow.httpx.AsyncClient", _FakeAsyncClient)

    response = TestClient(create_app(config={})).get("/health")

    assert response.status_code == 200
    assert _FakeAsyncClient.calls == []


def test_rust_shadow_posts_sanitized_envelope_and_writes_ledger(tmp_path, monkeypatch):
    _FakeAsyncClient.calls.clear()
    monkeypatch.setattr("src.rust_shadow.httpx.AsyncClient", _FakeAsyncClient)
    app = create_app(config=_shadow_config(tmp_path))

    response = TestClient(app).get(
        "/health?probe=1",
        headers={"Authorization": "Bearer secret", "Cookie": "sm_auth=secret"},
    )

    assert response.status_code == 200
    assert len(_FakeAsyncClient.calls) == 1
    call = _FakeAsyncClient.calls[0]
    assert call["endpoint"] == "http://rust-shadow.test/__shadow/http"
    envelope = call["json"]
    assert envelope["request"]["method"] == "GET"
    assert envelope["request"]["path"] == "/health"
    assert envelope["request"]["query_string"] == "probe=1"
    assert "authorization" not in envelope["request"]["headers"]
    assert "cookie" not in envelope["request"]["headers"]
    assert envelope["python_response"]["status"] == 200
    assert envelope["python_response"]["body_sha256"]

    rows = (tmp_path / "rust_shadow.jsonl").read_text(encoding="utf-8").splitlines()
    assert len(rows) == 1
    ledger = json.loads(rows[0])
    assert ledger["method"] == "GET"
    assert ledger["path"] == "/health"
    assert ledger["rust_result"]["comparison"] == "match"


def test_rust_shadow_failure_keeps_authoritative_response_and_records_error(
    tmp_path, monkeypatch
):
    _FailingAsyncClient.calls.clear()
    monkeypatch.setattr("src.rust_shadow.httpx.AsyncClient", _FailingAsyncClient)
    app = create_app(config=_shadow_config(tmp_path))

    response = TestClient(app).get("/health")

    assert response.status_code == 200
    assert response.json() == {"status": "healthy"}
    rows = (tmp_path / "rust_shadow.jsonl").read_text(encoding="utf-8").splitlines()
    assert len(rows) == 1
    ledger = json.loads(rows[0])
    assert ledger["path"] == "/health"
    assert ledger["shadow_error"] == "RuntimeError"
    assert "rust shadow offline" in ledger["shadow_error_message"]


def test_rust_shadow_bounds_request_body_without_logging_raw_payload(tmp_path, monkeypatch):
    _FakeAsyncClient.calls.clear()
    monkeypatch.setattr("src.rust_shadow.httpx.AsyncClient", _FakeAsyncClient)
    app = create_app(config=_shadow_config(tmp_path, max_body_bytes=8))

    @app.post("/shadow-test/echo")
    async def echo(request: Request):
        return {"size": len(await request.body())}

    response = TestClient(app).post(
        "/shadow-test/echo",
        content='{"message":"this body is longer than eight bytes"}',
        headers={"content-type": "application/json"},
    )

    assert response.status_code == 200
    envelope = _FakeAsyncClient.calls[0]["json"]
    assert envelope["request"]["body_truncated"] is True
    assert envelope["request"]["body_omitted"] is True
    assert envelope["request"]["body_base64"] is None
    rows = (tmp_path / "rust_shadow.jsonl").read_text(encoding="utf-8").splitlines()
    assert "this body" not in rows[0]


def test_rust_shadow_skips_multipart_requests(tmp_path, monkeypatch):
    _FakeAsyncClient.calls.clear()
    monkeypatch.setattr("src.rust_shadow.httpx.AsyncClient", _FakeAsyncClient)
    app = create_app(config=_shadow_config(tmp_path))

    @app.post("/shadow-test/upload")
    async def upload():
        return {"ok": True}

    response = TestClient(app).post(
        "/shadow-test/upload",
        files={"artifact": ("app.apk", b"fake")},
    )

    assert response.status_code == 200
    assert _FakeAsyncClient.calls == []
    assert not (tmp_path / "rust_shadow.jsonl").exists()


def test_rust_shadow_skips_sse_responses(tmp_path, monkeypatch):
    _FakeAsyncClient.calls.clear()
    monkeypatch.setattr("src.rust_shadow.httpx.AsyncClient", _FakeAsyncClient)
    app = create_app(config=_shadow_config(tmp_path))

    @app.get("/shadow-test/events")
    async def events():
        async def stream():
            yield "event: hello\ndata: {}\n\n"

        return StreamingResponse(stream(), media_type="text/event-stream")

    response = TestClient(app).get("/shadow-test/events")

    assert response.status_code == 200
    assert _FakeAsyncClient.calls == []
    assert not (tmp_path / "rust_shadow.jsonl").exists()
