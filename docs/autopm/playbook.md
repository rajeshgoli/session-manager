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
- Reads a message from an agent he typed to in the last 2 h in 1–3 min (median, 56); other agent messages
  in 10–14 min. Does doc reviews mostly on web: 71 web, 16 Android, Oct 16–23. (readout 2026-W43)

## What worked

## What did not
- Decision cards for stalled doc asks: 18 of 154 asks waited ≥4 h, 5 of them overnight; ask load barely moves
  the rate (11% → 16%); 3 of 71 in Oct 9–23. Dropped before planning. (readout 2026-W43)
- A push class for awaited replies: those replies are already read fastest (1–3 min). Dropped. (readout 2026-W43)

## Rules
- Never infer disuse or backlog from stored state; check activity by surface. (two misreadings, session 8bbcb31a, 2026-10-15)
- Hide a feature only after 30 days of zero use on every surface. (Auto PM spec B.3, 2026-10-15)
- Exclude web `owner_events` before 2026-10-15 22:00 PDT: a tab loaded before the header deploy logged
  minute polls as inbox and queue opens. (readout 2026-W42)
- Exclude `doc.head` rows written before #2517 deploys: the doc reader's 60 s revision poll, 7% of rows.
  (readout 2026-W43)

## Method
- Weekly: measure due experiments, mine, plan, file, switch on, read out. (Auto PM spec H, 2026-10-15)
- Every visible-change plan carries a mock per surface; build PRs carry emulator and web screenshots. (Auto PM spec F.1/D.5, 2026-10-15)
- Test a hypothesis's premise against stored history before planning, on the subset the change targets,
  not the pooled population. (decision cards, readout 2026-W42)
- Metrics on `owner_events` need 14 days of it; the first usable baseline ends 2026-10-29. (readout 2026-W42)
- Snapshot live databases with `sqlite3 -readonly <db> '.backup <scratch>'` before joining them; ATTACH on
  the live files fails in the agent sandbox. (2026-10-23)

## Open hypotheses
1. Pushes reaching Rajesh at the desk (149 of 204, Oct 16–23; active on web, terminal or agent prompt in the
   prior 5 min) are redundant. Test: does he act on the pushed item at the desk within 10 min? (readout 2026-W43)
