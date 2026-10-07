# Web UI demo recording

`fixtures/` is a ten-minute recording of what the sm web UI's data calls return while a scripted
team runs a small sprint. The static demo site (sm-demo.rajeshgo.li) replays it in a loop, with no
server behind it. `generate/record.py` rebuilds it.

Everything in it is invented: the repo is `acme/shop`, the owner is "Alex", the agents are
`lead` and `shop-41` … `shop-46`. The recorder refuses to write fixtures that contain the
recording machine's user or host name, or any name from its deny list.

## The storyline

Times are seconds into the loop; each chapter caption is also in `timeline.json`.

| t | What happens | Where it shows |
|---|---|---|
| 0–20 | `lead` adds goal #40 *Checkout v2* (six tickets, #44 after #41, #45 after #42, #46 after both), publishes the plan doc for review, and #44–#46 are armed with *Start when ready* | Board, Inbox (doc), Agents |
| 25 | Three tickets start: `shop-41` (Claude), `shop-42` (Codex), `shop-43` (Codex harness on a local Qwen model) | Agents, Board |
| 60–190 | `shop-41`'s test job holds the only tests slot; `shop-43`'s waits, then runs | Queue |
| 90–185 | `shop-42` asks whether coupons stack (*Needs you*); Alex answers at 185 | Agents, Inbox, Board badge |
| 140–270 | Codex reviews `shop-41`'s PR #51 and finds one P1; `shop-41` fixes it; the re-review is clean; #51 merges | Queue (review slot), Agents |
| 240–380 | Alex asks for changes on the plan; `lead` republishes; Alex approves | Inbox, doc page |
| 270, 575 | `shop-41` and `lead` sign the guestbook | Guestbook |
| 280–560 | #44, #45 and #46 start by themselves as their prerequisites merge, and merge in turn | Board, Agents |
| 590 | Goal #40 closes, so its lane leaves the Board | Board |

## Format

```
fixtures/
  timeline.json           the index
  ticks/0000/<name>.json  one response body per endpoint, as the server returned it
  ticks/0000/<name>.html  the doc reader page (HTML)
  ...
  ticks/0119/
```

`timeline.json`:

```json
{
  "schema_version": 1,
  "tick_seconds": 5,              // storyline seconds between ticks
  "duration_seconds": 600,        // loop length; 120 ticks
  "loop": true,
  "owner_name": "Alex", "repo": "acme/shop",
  "chapters": [{"t": 0, "caption": "A goal with six tickets lands on the Board"}, ...],
  "ticks": [{
    "index": 0, "t": 0,
    "captured_at": "2026-10-07T01:00:00.123456Z",
    "chapter": "A goal with six tickets lands on the Board",
    "responses": {
      "/watch/state": {"file": "ticks/0000/watch_state-1a2b3c4d.json", "captured_at": "...",
                       "status": 200, "content_type": "application/json"},
      "/inbox/agent/a0000041?format=json": {"file": "...", "redirect": "/inbox/thread/ticket%3Aacme%2Fshop%2341?format=json", ...}
    }
  }]
}
```

- **Keys are request URLs exactly as the UI builds them**, path plus query
  (`/client/board?clock_hours=3`, `/inbox?format=json&filter=open`). A tick holds every URL the
  UI requests on Agents, Board, Queue, Inbox, Guestbook and the doc page, plus each agent's,
  job's, thread's and doc's detail URLs for whatever existed at that tick. A URL missing from a
  tick did not exist then (an agent not yet spawned): answer 404.
- **Unchanged responses share a file.** When a response is byte-identical to the previous tick's,
  its entry points at the earlier file. `captured_at` on each response entry is when that file
  was captured.
- **Shift times on replay.** Every timestamp in a response is relative to that response's
  `captured_at`. To make ages read right ("3m ago", "running 2m"), shift each timestamp string
  by `now − captured_at`. Formats present: RFC 3339 with `Z` and 0, 3, 5 or 6 fractional digits,
  and `YYYY-MM-DD HH:MM:SS` (UTC, no zone) in `/sessions/{id}/tool-calls`. There are no numeric
  epoch timestamps.
- `status` is the HTTP status; a few detail URLs legitimately record 404 (for example
  `/sessions/{id}/last-turn` before an agent's first turn).
- `redirect`: the server answered with a redirect (`/inbox/agent/{id}` goes to the agent's
  thread). Both URLs are recorded with the same body.
- Doc reader pages are recorded under the iframe's URL, one per published revision
  (`/docs/shop/docs/checkout_v2_plan.html?version=<12-char commit>`) plus the bare path, which is
  the newest revision.

## What is real and what is synthetic

The responses come from a real `sm-server` on fresh state, except where noted.

- **Real server, driven through the real CLI:** `sm status`, `sm ticket`, `sm pr`,
  `sm queue run`, `sm send alex --blocking`, `sm task-complete --sign-guestbook`, and the goal
  lane (`POST /board/lanes`, which reads the issues through a fake `gh`).
- **Real server, state written directly** (the server's runtime is off, so nothing else would
  write it): agent sessions and their activity times, queue job state changes and logs, Codex
  review requests and their review jobs, the owner's reply and doc reviews, published doc
  revisions, *Start when ready* rows (that route needs a signed-in owner), ticket and PR state
  after GitHub events, last-turn messages and tool calls.
- **Synthetic** (a runtime-off server can't answer them, or would answer from the recording
  machine): `/client/host-status`, the `host` field of `/client/queue`,
  `/client/utilization/series`, `/client/queue/stats` and `/client/usage/meters`.
- **Fake tools on the server's PATH** (`generate/fake-bin/`): `gh` answers from the recorder's
  world file (`gh-world.json`), including the issue, comment, PR and check reads behind the
  ticket panel; `codex` lists models. Nothing reaches GitHub.
- **Last week's sprint** (`prologue` in `record.py`): before the recording starts, a planner and
  four agents work goal #30 *Cart v1* (#31–#34, PRs #35–#38) through the same CLI calls, plus a
  `scout` with no ticket; then all of them retire and their times move 2–6 days into the past.
  They are what History lists and offers to bring back.
- The local-model agent is a Codex session whose model is `qwen3-coder-next`; Codex agents carry
  a tmux session name on a socket no tmux server listens on, because the server drops Codex
  sessions without one.

## Rebuilding

The recording plays in real time, so it takes about ten minutes. Run it through the queue:

```
sm queue run --type background --max-wait 2h --timeout 30m --label demo-record \
    --cwd <repo> -- python3 web-demo/generate/record.py
```

Options: `--tick 0.3` plays fast for a smoke test (the fixtures then cover the same storyline,
with compressed real time); `--out DIR` writes elsewhere (via `DIR.partial`, swapped in only after the leak scan passes); `--server` and `--sm` pick binaries
(default: `sm-server` and `sm` on PATH); `--port` (default 8431).

The scratch server runs from `/tmp/smdemo-rec` with `rust_core.runtime_enabled: false`, so it
never adopts, runs or stops queue jobs, and its config points every database at that directory.
`record.log`, `server.log` and `fake-gh.log` there explain a failed run.

## The static layer

Two pages are not part of the storyline, so they are not recorded per tick:

- **Analytics.** `generate/analytics.py` snapshots a real server's spend and time reports
  (every range, and each spend provider) into `fixtures/static/`, indexed in
  `fixtures/static.json`. The numbers are real; every repo, ticket, agent and account name is
  invented (the biggest repo becomes `pricing-engine`, tickets get numbers from 101 and made-up
  titles), agent and history links are dropped, and the script refuses to write if any original
  name survives. Rerun it against the live server to refresh the numbers:
  `python3 web-demo/generate/analytics.py [--server URL]`.
- **Notes.** `fixtures/static/notes.json` holds four invented notes with revisions. The worker
  serves them and lets a visitor create, edit, search, preview and restore notes for the
  length of the visit; each save shows "Nothing is saved in the demo".

`record.py` carries `static/` and `static.json` over when it replaces `fixtures/`.

## The static site

`build.py` turns the recording into the demo site: the real web UI from
`crates/sm-server/src/web/`, unchanged, plus a service worker that stands in for the server.

```
python3 web-demo/build.py            # writes web-demo/dist/ (git-ignored)
python3 web-demo/build.py --serve    # builds, then previews at http://localhost:8440/
```

`dist/` is self-contained; upload it to any static host that serves files at their paths and
`404.html` for unknown ones. Its layout:

| Path | What it is |
|---|---|
| `index.html`, `board/index.html`, … , `404.html` | the app shell (`http/web.rs` `shell_response`), with the demo banner and worker boot in place of the module load |
| `sw.js` | the service worker (source `site/sw.js`); it must sit at the root to control every path |
| `assets/` | a copy of `crates/sm-server/src/web/` |
| `demo/` | `demo.js` (banner, read-only notice, worker boot) and `demo.css` |
| `fixtures/` | a copy of `fixtures/` |

How it replays:

- **The worker answers every same-origin request** except `assets/`, `fixtures/` and `demo/`,
  which the host serves. A page navigation gets the shell; a `/docs/…` navigation (the
  reader's iframe, or a doc opened full screen) gets the recorded doc page. Every other GET is
  looked up in the current tick by path and query (query order and the reader's `from` are
  ignored) and answered with the recorded status and body; a URL the tick lacks is looked up in
  `static.json`, and failing that is a 404.
  Doc review drafts and the reopen target are not recorded; the worker answers them as a doc
  with no drafts that cannot be reopened, which shows the review sheet with Submit disabled.
- **The clock** starts on a visitor's first request and loops every `duration_seconds`; a
  visitor back after 15 minutes idle, or a new build, starts from the beginning. The banner's ↺
  restarts it. Timestamps in each response are shifted so that ages read as they did live.
- **Nothing writes.** Any non-GET request gets 403 `Demo — read only`, and the page shows a
  notice saying what the real app would have done, with a link to install it. That covers New
  agent, Start, Send, Retire, Restore, Archive, review submit, terminal attach and `/btw`.
  Notes are the exception (above), and `POST /client/board/seen`, which the Board sends by
  itself, gets a quiet 200.
- **The first visit** registers the worker and loads the app once the worker controls the
  page, so the app's first request already goes to the worker. Browsers without service
  workers get a one-line explanation.

For checking: `POST /__demo/seek?t=<seconds>` jumps the clock, and `GET /__demo/misses` lists
URLs the UI asked for that the current tick did not have. `node web-demo/site/sw.test.js`
tests the timestamp shift, the lookup key and the notes helpers.
