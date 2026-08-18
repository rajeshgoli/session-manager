# Service capacity class

Issue: #1318

## Measured basis

At the incident timestamp, two long-lived Vite/API processes owned by
`expectation-layer-orch` occupied the two `background` slots. Four jobs total
were running, below the configured global cap of eight. The watchdog was held
by the background type cap, not by global or tests/perf contention.

The active services are the only current durable service callers. Historical
24-hour `background` jobs include one-shot work, so timeout, label, and argv
cannot safely classify a job as a service.

## Contract

- `service` is an explicit submitter-selected, durable queue type.
- Existing records remain their persisted type; no migration or heuristic
  reclassification is performed.
- `queue_runner.types.service` is opt-in. A service submission fails loudly
  until that configuration is present.
- When configured, service capacity must be positive and strictly below
  `max_running_jobs`; it can never silently consume the entire global pool.
- Service jobs still count against the global cap, while retaining a separate
  per-type cap from finite `background` jobs.
- Recovery checks live persisted service rows before every admission attempt.
  If a configuration reduction leaves more live services than the service cap
  or consumes the configured non-service reserve, recovery fails with the
  counts and job IDs; it leaves those rows unchanged and admits nothing.
- Admission order places finite background work before newly queued services.
  Perf cooldown and test ordering are unchanged. Perf displacement remains
  limited to finite background jobs and never terminates a service.

## Required proof

- Hostile scheduler coverage proves two services admit finite background work
  while a third service is held.
- Hostile recovery coverage proves the service-capacity boundary admits finite
  background work, while a reduction from 8/4 to 4/3 with four live services
  fails loudly, identifies every service row, and leaves finite work pending.
- Config coverage proves missing service configuration and global-consuming
  service capacities fail loudly.
- CLI/API accepts and projects `service`; unsupported values still fail.

## Rollout

Deployment must add the explicitly chosen service configuration without raising
global or background caps. After deployment, hand the new `--type service`
contract to `d0d5f272` (`expectation-layer-orch`) for future submissions only.
Do not resubmit, alter, or otherwise mutate its current Vite/API services.

## Ticket classification

Single ticket.
