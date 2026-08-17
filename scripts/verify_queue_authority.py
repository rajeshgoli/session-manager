#!/usr/bin/env python3
"""Verify and query the macOS Session Manager queue authority socket in-process."""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import socket
import stat
import struct
from pathlib import Path
from typing import Any

REQUEST_SCHEMA = "sm.queue_authority.request.v1"
RESPONSE_SCHEMA = "sm.queue_authority.response.v1"
SOL_LOCAL = 0
LOCAL_PEERPID = 0x002
CS_OPS_IDENTITY = 11
MAX_RESPONSE_BYTES = 1024 * 1024


class AuthorityVerificationError(RuntimeError):
    """The connected peer or its queue response failed closed verification."""


def query_attested_queue_job(
    *,
    socket_path: Path,
    job_id: str,
    expected_executable: Path,
    expected_launchd_label: str,
    expected_code_sign_identifier: str,
) -> dict[str, Any]:
    """Return one queue authority response after kernel and code identity checks."""
    socket_stat = os.lstat(socket_path)
    if stat.S_ISLNK(socket_stat.st_mode) or not stat.S_ISSOCK(socket_stat.st_mode):
        raise AuthorityVerificationError(f"authority path is not a direct socket: {socket_path}")
    if socket_stat.st_uid != os.geteuid():
        raise AuthorityVerificationError("authority socket owner does not match the caller")

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(5)
        connection.connect(str(socket_path))
        peer_uid, _peer_gid = _peer_credentials(connection)
        if peer_uid != os.geteuid():
            raise AuthorityVerificationError("authority peer uid does not match the caller")
        peer_pid = struct.unpack(
            "=i", connection.getsockopt(SOL_LOCAL, LOCAL_PEERPID, struct.calcsize("=i"))
        )[0]
        if peer_pid <= 0:
            raise AuthorityVerificationError("authority peer pid is invalid")

        peer_path = _process_path(peer_pid)
        if peer_path != expected_executable:
            raise AuthorityVerificationError(
                f"authority peer executable mismatch: {peer_path} != {expected_executable}"
            )
        signing_identifier = _code_sign_identifier(peer_pid)
        if signing_identifier != expected_code_sign_identifier:
            raise AuthorityVerificationError(
                "authority peer signing identifier mismatch: "
                f"{signing_identifier!r} != {expected_code_sign_identifier!r}"
            )

        request = json.dumps(
            {"schema": REQUEST_SCHEMA, "job_id": job_id}, separators=(",", ":")
        ).encode("utf-8")
        connection.sendall(request + b"\n")
        try:
            payload = json.loads(_read_response(connection))
        except json.JSONDecodeError as error:
            raise AuthorityVerificationError("authority response is not valid JSON") from error

    if not isinstance(payload, dict):
        raise AuthorityVerificationError("authority response must be a JSON object")

    if payload.get("schema") != RESPONSE_SCHEMA:
        raise AuthorityVerificationError("authority response schema mismatch")
    service = payload.get("service")
    if not isinstance(service, dict):
        raise AuthorityVerificationError("authority response is missing service identity")
    expected_service = {
        "pid": peer_pid,
        "launchd_label": expected_launchd_label,
        "executable_path": str(expected_executable),
        "code_sign_identifier": expected_code_sign_identifier,
    }
    for field, expected in expected_service.items():
        if service.get(field) != expected:
            raise AuthorityVerificationError(
                f"authority response {field} mismatch: {service.get(field)!r} != {expected!r}"
            )
    return payload


def _peer_credentials(connection: socket.socket) -> tuple[int, int]:
    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    libc.getpeereid.argtypes = [
        ctypes.c_int,
        ctypes.POINTER(ctypes.c_uint),
        ctypes.POINTER(ctypes.c_uint),
    ]
    libc.getpeereid.restype = ctypes.c_int
    uid = ctypes.c_uint()
    gid = ctypes.c_uint()
    if libc.getpeereid(connection.fileno(), ctypes.byref(uid), ctypes.byref(gid)) != 0:
        error = ctypes.get_errno()
        raise AuthorityVerificationError(f"getpeereid failed: errno {error}")
    return uid.value, gid.value


def _process_path(pid: int) -> Path:
    libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    libproc.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
    libproc.proc_pidpath.restype = ctypes.c_int
    buffer = ctypes.create_string_buffer(4096)
    length = libproc.proc_pidpath(pid, buffer, len(buffer))
    if length <= 0:
        error = ctypes.get_errno()
        raise AuthorityVerificationError(f"proc_pidpath failed for {pid}: errno {error}")
    return Path(os.fsdecode(buffer.value))


def _code_sign_identifier(pid: int) -> str:
    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    libc.csops.argtypes = [ctypes.c_int, ctypes.c_uint, ctypes.c_void_p, ctypes.c_size_t]
    libc.csops.restype = ctypes.c_int
    buffer = ctypes.create_string_buffer(4096)
    if libc.csops(pid, CS_OPS_IDENTITY, buffer, len(buffer)) != 0:
        error = ctypes.get_errno()
        raise AuthorityVerificationError(f"csops identity failed for {pid}: errno {error}")
    identity = buffer.raw[8:].split(b"\0", 1)[0]
    if not identity:
        raise AuthorityVerificationError("csops returned an empty signing identifier")
    try:
        return identity.decode("utf-8")
    except UnicodeDecodeError as error:
        raise AuthorityVerificationError("csops returned a non-UTF-8 signing identifier") from error


def _read_response(connection: socket.socket) -> str:
    response = bytearray()
    while True:
        chunk = connection.recv(4096)
        if not chunk:
            raise AuthorityVerificationError("authority response ended before newline")
        newline = chunk.find(b"\n")
        if newline >= 0:
            response.extend(chunk[:newline])
            if len(response) > MAX_RESPONSE_BYTES:
                raise AuthorityVerificationError("authority response exceeds size limit")
            if any(not chr(byte).isspace() for byte in chunk[newline + 1 :]):
                raise AuthorityVerificationError("authority returned more than one response")
            break
        response.extend(chunk)
        if len(response) > MAX_RESPONSE_BYTES:
            raise AuthorityVerificationError("authority response exceeds size limit")
    if not response:
        raise AuthorityVerificationError("authority response is empty")
    try:
        return response.decode("utf-8")
    except UnicodeDecodeError as error:
        raise AuthorityVerificationError("authority response is not UTF-8") from error


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("job_id")
    parser.add_argument("--socket", type=Path, required=True)
    parser.add_argument("--executable", type=Path, required=True)
    parser.add_argument("--launchd-label", required=True)
    parser.add_argument("--signing-id", required=True)
    args = parser.parse_args()
    payload = query_attested_queue_job(
        socket_path=args.socket,
        job_id=args.job_id,
        expected_executable=args.executable,
        expected_launchd_label=args.launchd_label,
        expected_code_sign_identifier=args.signing_id,
    )
    print(json.dumps(payload, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
