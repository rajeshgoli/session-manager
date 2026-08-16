# sm#1244: queue process-group accounting

## Contract

A queue job owns its persisted process group, not only the wrapper process that
Session Manager spawned. Success, failure, cancellation, timeout, displacement,
and queue-capacity release must wait until that entire process group is absent.

On timeout, Session Manager sends `SIGTERM` to the group and allows the configured
cancel grace period for application cleanup. If the group remains, it sends
`SIGKILL` and still waits for group absence before recording the terminal state.
The wrapper exiting during cleanup is not terminal evidence.

## Application locks

Session Manager does not infer, validate, or remove locks created by a queued
application. A lock retained by an interrupted application requires that
application's authority-checked recovery path. The queue's responsibility is to
provide truthful process-group state so the application can make that decision.

## Reproduction

Job `job_eee1b42ed786` timed out after 900 seconds. Its wrapper exited before the
final-lane process completed SIGTERM accounting, so the queue recorded
`timed_out` approximately 36 seconds before the final-lane log and manifest
finished. The final-lane process then retained its fail-closed project lock.

The regression fixture traps `SIGTERM` in the inner command, waits one second,
and writes a cleanup marker after the wrapper has exited. The queue may publish
`timed_out` only after that marker exists and the process group is gone.

