import json
import os
import socket
import sys
import threading
import uuid
from pathlib import Path

import pytest

from scripts import verify_queue_authority as verifier


pytestmark = pytest.mark.skipif(sys.platform != "darwin", reason="macOS peer attestation")


def test_query_attests_kernel_peer_process_and_response_identity():
    path, listener = _listener()
    peer_path = verifier._process_path(os.getpid())
    signing_id = verifier._code_sign_identifier(os.getpid())
    server = _serve_one(listener, peer_path, signing_id)

    try:
        payload = verifier.query_attested_queue_job(
            socket_path=path,
            job_id="job_0123456789ab",
            expected_executable=peer_path,
            expected_launchd_label="test-authority",
            expected_code_sign_identifier=signing_id,
        )
    finally:
        _cleanup(path, listener, server)
    assert payload["error"]["code"] == "not_found"


@pytest.mark.parametrize("mismatch", ["executable", "signing_id"])
def test_query_rejects_peer_identity_mismatch(mismatch):
    path, listener = _listener()
    peer_path = verifier._process_path(os.getpid())
    signing_id = verifier._code_sign_identifier(os.getpid())
    server = _serve_one(listener, peer_path, signing_id)
    expected_path = Path("/wrong/sm-server") if mismatch == "executable" else peer_path
    expected_signing_id = "wrong.signing.id" if mismatch == "signing_id" else signing_id

    try:
        with pytest.raises(verifier.AuthorityVerificationError, match="mismatch"):
            verifier.query_attested_queue_job(
                socket_path=path,
                job_id="job_0123456789ab",
                expected_executable=expected_path,
                expected_launchd_label="test-authority",
                expected_code_sign_identifier=expected_signing_id,
            )
    finally:
        _cleanup(path, listener, server)


def test_response_limit_applies_to_the_final_newline_chunk():
    class OversizedResponse:
        remaining = verifier.MAX_RESPONSE_BYTES

        def recv(self, size):
            if self.remaining:
                length = min(size, self.remaining)
                self.remaining -= length
                return b"x" * length
            return b"x\n"

    with pytest.raises(verifier.AuthorityVerificationError, match="size limit"):
        verifier._read_response(OversizedResponse())


def _listener():
    path = Path("/tmp") / f"sm-auth-test-{uuid.uuid4().hex}.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(path))
    listener.listen(1)
    return path, listener


def _serve_one(listener, peer_path, signing_id):
    def serve():
        connection, _ = listener.accept()
        with connection:
            request = bytearray()
            while not request.endswith(b"\n"):
                chunk = connection.recv(2048)
                if not chunk:
                    return
                request.extend(chunk)
            if not request:
                return
            assert json.loads(request) == {
                "schema": verifier.REQUEST_SCHEMA,
                "job_id": "job_0123456789ab",
            }
            response = {
                "schema": verifier.RESPONSE_SCHEMA,
                "ok": False,
                "service": {
                    "pid": os.getpid(),
                    "launchd_label": "test-authority",
                    "executable_path": str(peer_path),
                    "code_sign_identifier": signing_id,
                },
                "job": None,
                "error": {"code": "not_found", "message": "fixture"},
            }
            connection.sendall(json.dumps(response).encode() + b"\n")

    thread = threading.Thread(target=serve)
    thread.start()
    return thread


def _cleanup(path, listener, server):
    server.join(timeout=2)
    listener.close()
    path.unlink(missing_ok=True)
    assert not server.is_alive()
