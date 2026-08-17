# Queue completion delivery and log inspection

Issue: #1255

## Problem

Queue completion notifications are durably inserted into `message_queue.db`, but
the queue worker does not trigger runtime delivery. A notification can therefore
remain pending until an unrelated operation drains the target session's queue.
`completion_notified_at` currently records durable enqueue, not terminal delivery.

`sm queue status` reports a server-local log path, but the Rust CLI has no
supported command for reading it. This makes the documented queue workflow
incomplete for remote clients and forces local agents to bypass the API.

## Contract

1. While the Rust runtime is enabled, Session Manager attempts delivery of all
   pending `queue-completion` messages after recovery and every completion retry
   interval. Target discovery is category-specific, but delivery drains the
   target's normal queue in FIFO order so a completion cannot bypass an older
   sequential message. Busy sessions retain the message for a later attempt.
2. Delivery remains idempotent through the existing durable message ID
   `queue-completion-<job-id>` and `delivered_at` marker.
3. One target's delivery failure must not discard another target's pending wake.
4. `sm queue log <job-id> [--lines N]` reads a bounded tail through the Session
   Manager API. The default is 200 lines and the maximum is 10,000 lines.
5. The server derives the log path as
   `<configured queue state dir>/logs/<durable job id>.log`. It does not trust a
   caller-supplied path or the persisted `log_path` field as read authority.
6. Missing jobs return 404. Missing log files return 404. Invalid line counts
   return 400. Log responses are JSON so the existing HTTP and HTTPS CLI clients
   share one implementation.

## Verification

- A pending completion message is discovered by category and delivered when the
  target runtime becomes writable.
- Reconciliation leaves busy or failed deliveries pending for retry.
- The log endpoint returns only the requested tail and rejects missing jobs,
  missing logs, and invalid bounds.
- The CLI parses `sm queue log`, emits the returned text unchanged, and supports
  `--lines`.
- A live queued failure produces an automatic wake and is inspectable without
  direct filesystem access.
