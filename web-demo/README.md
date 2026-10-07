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
  world file (`gh-world.json`); `codex` lists models. Nothing reaches GitHub.
- The local-model agent is a Codex session whose model is `qwen3-coder-next`; Codex agents carry
  a tmux session name on a socket no tmux server listens on, because the server drops Codex
  sessions without one.

## Rebuilding

The recording plays in real time, so it takes about ten minutes. Run it through the queue:

```
sm queue run --type service --label demo-record --timeout 1800 --cwd <repo> -- \
    python3 web-demo/generate/record.py
```

Options: `--tick 0.3` plays fast for a smoke test (the fixtures then cover the same storyline,
with compressed real time); `--out DIR` writes elsewhere; `--server` and `--sm` pick binaries
(default: `sm-server` and `sm` on PATH); `--port` (default 8431).

The scratch server runs from `/tmp/smdemo-rec` with `rust_core.runtime_enabled: false`, so it
never adopts, runs or stops queue jobs, and its config points every database at that directory.
`record.log`, `server.log` and `fake-gh.log` there explain a failed run.
