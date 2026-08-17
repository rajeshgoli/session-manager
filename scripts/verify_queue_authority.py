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
CS_IDENTITY_HEADER_BYTES = 8
CF_NUMBER_SINT64_TYPE = 4
CF_STRING_ENCODING_UTF8 = 0x08000100
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
        launchd_pid = _launchd_job_pid(expected_launchd_label)
        if launchd_pid != peer_pid:
            raise AuthorityVerificationError(
                f"authority launchd job PID mismatch: peer {peer_pid}, "
                f"{expected_launchd_label} pid {launchd_pid}"
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
    # CS_OPS_IDENTITY writes a cs_identity header before its NUL-terminated string.
    identity = buffer.raw[CS_IDENTITY_HEADER_BYTES:].split(b"\0", 1)[0]
    if not identity:
        raise AuthorityVerificationError("csops returned an empty signing identifier")
    try:
        return identity.decode("utf-8")
    except UnicodeDecodeError as error:
        raise AuthorityVerificationError("csops returned a non-UTF-8 signing identifier") from error


def _launchd_job_pid(label: str) -> int:
    service_management = ctypes.CDLL(
        "/System/Library/Frameworks/ServiceManagement.framework/ServiceManagement"
    )
    core_foundation = ctypes.CDLL(
        "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation"
    )
    service_management.SMJobCopyDictionary.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    service_management.SMJobCopyDictionary.restype = ctypes.c_void_p
    core_foundation.CFStringCreateWithCString.argtypes = [
        ctypes.c_void_p,
        ctypes.c_char_p,
        ctypes.c_uint32,
    ]
    core_foundation.CFStringCreateWithCString.restype = ctypes.c_void_p
    core_foundation.CFDictionaryGetValue.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    core_foundation.CFDictionaryGetValue.restype = ctypes.c_void_p
    core_foundation.CFGetTypeID.argtypes = [ctypes.c_void_p]
    core_foundation.CFGetTypeID.restype = ctypes.c_ulong
    core_foundation.CFNumberGetTypeID.argtypes = []
    core_foundation.CFNumberGetTypeID.restype = ctypes.c_ulong
    core_foundation.CFNumberGetValue.argtypes = [
        ctypes.c_void_p,
        ctypes.c_int,
        ctypes.c_void_p,
    ]
    core_foundation.CFNumberGetValue.restype = ctypes.c_bool
    core_foundation.CFRelease.argtypes = [ctypes.c_void_p]
    core_foundation.CFRelease.restype = None

    try:
        domain = ctypes.c_void_p.in_dll(
            service_management, "kSMDomainUserLaunchd"
        ).value
    except ValueError as error:
        raise AuthorityVerificationError("user launchd domain is unavailable") from error
    if not domain:
        raise AuthorityVerificationError("user launchd domain is null")

    label_ref = core_foundation.CFStringCreateWithCString(
        None, label.encode("utf-8"), CF_STRING_ENCODING_UTF8
    )
    if not label_ref:
        raise AuthorityVerificationError("failed to encode launchd label")
    try:
        job = service_management.SMJobCopyDictionary(domain, label_ref)
        if not job:
            raise AuthorityVerificationError(f"launchd job {label} is not registered")
        try:
            pid_key = core_foundation.CFStringCreateWithCString(
                None, b"PID", CF_STRING_ENCODING_UTF8
            )
            if not pid_key:
                raise AuthorityVerificationError("failed to encode launchd PID key")
            try:
                pid_value = core_foundation.CFDictionaryGetValue(job, pid_key)
            finally:
                core_foundation.CFRelease(pid_key)
            if not pid_value:
                raise AuthorityVerificationError(f"launchd job {label} has no PID")
            if (
                core_foundation.CFGetTypeID(pid_value)
                != core_foundation.CFNumberGetTypeID()
            ):
                raise AuthorityVerificationError(f"launchd job {label} PID is not numeric")
            pid = ctypes.c_int64()
            if not core_foundation.CFNumberGetValue(
                pid_value, CF_NUMBER_SINT64_TYPE, ctypes.byref(pid)
            ):
                raise AuthorityVerificationError(f"launchd job {label} PID is unreadable")
            if pid.value <= 0:
                raise AuthorityVerificationError(f"launchd job {label} PID is invalid")
            return pid.value
        finally:
            core_foundation.CFRelease(job)
    finally:
        core_foundation.CFRelease(label_ref)


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


def _require_not_found_response(payload: dict[str, Any]) -> None:
    error = payload.get("error")
    if not (
        payload.get("ok") is False
        and payload.get("job") is None
        and isinstance(error, dict)
        and error.get("code") == "not_found"
    ):
        raise AuthorityVerificationError(
            "authority probe did not return exact not_found response"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("job_id")
    parser.add_argument("--socket", type=Path, required=True)
    parser.add_argument("--executable", type=Path, required=True)
    parser.add_argument("--launchd-label", required=True)
    parser.add_argument("--signing-id", required=True)
    parser.add_argument("--expect-not-found", action="store_true")
    args = parser.parse_args()
    payload = query_attested_queue_job(
        socket_path=args.socket,
        job_id=args.job_id,
        expected_executable=args.executable,
        expected_launchd_label=args.launchd_label,
        expected_code_sign_identifier=args.signing_id,
    )
    if args.expect_not_found:
        _require_not_found_response(payload)
    print(json.dumps(payload, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
