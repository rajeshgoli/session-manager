# Auto PM playbook

Conclusions, not raw data. Each entry is at most three lines with dated evidence
or an experiment reference. Change only through readout PRs. At 1,500 lines,
merge or drop the weakest entries. Keep these six sections.

## How Rajesh works
- Reads and reviews docs; skips the inbox as a work queue. (session 8bbcb31a, 2026-10-15)
- On Android, uses the inbox as an alert for things he expects; rarely uses the web inbox. (session 8bbcb31a, 2026-10-15)
- Attention is the scarcest currency: anything spending it must pay back from day 0. (session 8bbcb31a, 2026-10-15)
- Judgment asks on long artifacts stall: fractal-algo-rust #2114, waiting since 2026-10-14. (session 8bbcb31a, 2026-10-15)
- Answers most asks fast: median doc-review ask 12–35 min and median blocking-reply ask under 70 min in each
  week since Sep 24 (174 asks). Stalls are a tail, not the norm. (readout 2026-W42)

## What worked

## What did not

## Rules
- Never infer disuse or backlog from stored state; check activity by surface. (two misreadings, session 8bbcb31a, 2026-10-15)
- Hide a feature only after 30 days of zero use on every surface. (Auto PM spec B.3, 2026-10-15)
- Exclude web `owner_events` before 2026-10-15 22:00 PDT: a tab loaded before the header deploy logged
  minute polls as inbox and queue opens. (readout 2026-W42)

## Method
- Weekly: measure due experiments, mine, plan, file, switch on, read out. (Auto PM spec H, 2026-10-15)
- Every visible-change plan carries a mock per surface; build PRs carry emulator and web screenshots. (Auto PM spec F.1/D.5, 2026-10-15)
- Test a hypothesis's premise against stored history before planning; a median metric cannot move a tail.
  (decision cards, readout 2026-W42)
- Metrics on `owner_events` need 14 days of it; the first usable baseline ends 2026-10-29. (readout 2026-W42)

## Open hypotheses
1. Asks that stall share a trait (long doc, many choices, no push) that a decision card fixes; find the
   trait first, since the median ask is answered within an hour. (fractal-algo-rust #2114; readout 2026-W42)
2. Pushes for awaited replies deserve their own class. (session 8bbcb31a, 2026-10-15)
