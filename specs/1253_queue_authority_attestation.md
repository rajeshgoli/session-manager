# Queue Authority Attestation

Session Manager queue recovery authority is available only through the Unix-domain socket at:

```text
~/.local/share/claude-sessions/queue-runner/authority.sock
```

The ordinary HTTP queue API is not recovery authority. No `sm` CLI executable, `PATH` lookup, caller URL, redirect, environment endpoint, or subprocess response may substitute for this socket.

## Protocol

Send one newline-terminated UTF-8 JSON object and read one newline-terminated response. Requests are bounded to 1,024 bytes.

```json
{"schema":"sm.queue_authority.request.v1","job_id":"job_eee1b42ed786"}
```

A successful response uses this shape:

```json
{
  "schema": "sm.queue_authority.response.v1",
  "ok": true,
  "service": {
    "pid": 9288,
    "launchd_label": "com.rajeshgoli.session-manager-rust",
    "executable_path": "/Users/rajesh/projects/session-manager/.local/bin/sm-server",
    "code_sign_identifier": "com.rajeshgoli.sm-server"
  },
  "job": {
    "id": "job_eee1b42ed786",
    "type": "tests",
    "cwd": "/absolute/worktree",
    "argv": ["/absolute/python", "scripts/run_final_lane.py", "run"],
    "state": "timed_out",
    "process_group_id": 99695
  },
  "error": null
}
```

Errors preserve the schema and service object, set `ok` false and `job` null, and return `error.code` plus `error.message`.

## Required Verification

Consumers must complete these checks in-process before reading `job`:

1. Use the configured absolute socket path. `lstat` must report a socket, not a symlink, owned by the effective user.
2. Connect with `AF_UNIX`. `getpeereid` must report the effective user.
3. Read the kernel peer PID with `getsockopt(SOL_LOCAL, LOCAL_PEERPID)`. It must be positive.
4. Resolve that PID with `proc_pidpath`. It must equal `/Users/rajesh/projects/session-manager/.local/bin/sm-server` exactly.
5. Read that live PID's code-sign identity with `csops(pid, CS_OPS_IDENTITY)`. It must equal `com.rajeshgoli.sm-server`.
6. Send the bounded request. Reject timeout, EOF before newline, oversized output, multiple frames, non-UTF-8, or malformed JSON.
7. Require response schema `sm.queue_authority.response.v1`.
8. Require `service.pid` to equal the kernel peer PID. Require the exact launchd label, executable path, and code-sign identifier above.
9. Only then validate and consume the queue job fields. All missing or malformed fields fail closed.

The reference implementation is [`scripts/verify_queue_authority.py`](../scripts/verify_queue_authority.py). Project recovery code must execute equivalent logic in-process; invoking this file or another helper as a subprocess does not satisfy the authority contract.

## Legacy Locks

This transport authenticates Session Manager's queue record. It does not create missing provenance for a legacy lock. Two-field locks remain permanently fail-closed because their run-to-queue binding was not recorded independently when created.

