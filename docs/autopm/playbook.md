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
- Overrides the queue by hand in bursts: 178 Start presses Oct 19–Nov 1 on four days; 87 on jobs held for memory
  another running job had declared, 62 on concurrency limits. No kill followed. (readout 2026-W45)
- Works mainly in the web terminal: 1,066 terminal switches across 162 sessions Nov 2–16; scanning bursts
  of 3+ quick switches are modest (83 in two weeks). (readout 2026-W47)

## What worked

## What did not
- Decision cards for stalled doc asks: 18 of 154 asks waited ≥4 h, 5 of them overnight; ask load barely moves
  the rate (11% → 16%); 3 of 71 in Oct 9–23. Dropped before planning. (readout 2026-W43)
- A push class for awaited replies: those replies are already read fastest (1–3 min). Dropped. (readout 2026-W43)
- Reserving memory from a repeat job's earlier peak: 26 of 96 repeat jobs declaring ≥64 GiB exceeded 1.5× their
  prior peak; small jobs fit (92%) but were not the blockers. Dropped. (readout 2026-W46)
- Manual queue starts are episodic, not standing: 178 in Oct 19–Nov 1, 1 in Nov 2–9 despite 269 memory-held
  jobs. Check a burst's next week before planning on it. (readout 2026-W46)
- Holding pushes while he is at his desk: asked 2026-11-02, opened, never answered; sm abandoned it 2026-11-16
  and its build (PR #2536) closed unmerged. (readout 2026-W47)

## Rules
- Never infer disuse or backlog from stored state; check activity by surface. (two misreadings, session 8bbcb31a, 2026-10-15)
- Hide a feature only after 30 days of zero use on every surface. (Auto PM spec B.3, 2026-10-15)
- Act without Rajesh: pick changes that ship behind a switch, not ones needing his yes, and never hand him a
  next step. x-2026w44 was asked Nov 2, opened, unanswered. (his review of readout 2026-W46, 2026-11-09)
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
- `bug_reports.db` refuses `-readonly` opens in the sandbox; use `sqlite3 <db> '.backup <scratch>'` without
  `-readonly` (it runs in WAL mode, so never `cp`). Queue jobs: `queue-runner/queue_runner.db`. (2026-11-02)
- Treat a change that loosens queue memory admission as ask-only in practice: it decides whether trading jobs
  get killed (#2507; 29 memory-pressure kills Oct 19–Nov 2, 28 in fractal-algo-rust). (readout 2026-W45)
- Rank owner actions by p90 `duration_ms` (handler time) to find waits he feels: retire's 10 s cleanup wait
  showed as p90 10.1 s (x-2026w47-retire-no-wait). (readout 2026-W47)
- `note.search` rows follow each note autosave, not an owner search; Android logged 1,901 doc-screen rows as
  nav on 2026-11-02 (23–36 on other days). Exclude both from counts. (readout 2026-W47)
- After `sm autopm start`, move the experiment `planned → building` yourself; sm moves only
  `building → dark`, on its timer. x-2026w47 sat in planned after its build deployed. (2026-11-16)

## Open hypotheses
1. Board open and start-from-board are slow in the tail: p90 4.1 s and 4.6 s, Nov 2–15. Find where the time
   goes before planning. (readout 2026-W47)
