# Session Manager

**Stop hoping your agent finishes. Run your agents like an engineering team.**

Pool Claude, Codex and local models into one team. Plan the sprint, give each
ticket to the right model from whichever subscription has room, and watch the
plan move on one screen, from your desk or your phone.

![The Agents page: Claude and Codex agents sorted by what needs attention](docs/assets/agents.png)
<!-- ASSET: "Try the live demo" link to sm-demo.rajeshgo.li once it is up. -->

---

## The problem: hope-driven agents

Modern coding agents are strong enough that you can hand one an entire epic
and tell it to keep going until it's done. Often it even gets there. But:

- **You can't see progress.** All you have is a transcript. "How far along is
  it?" is a guess, and "almost done" can last three hours.
- **It forgets.** The epic doesn't fit in one agent's context. It compacts,
  loses decisions it made an hour ago, and drifts.
- **It marks its own homework.** The model that wrote the code also decides
  the code is good.
- **It spends your best model on everything**, including the trivial steps.
- **Coordination costs tokens.** An agent that checks on its own progress, or
  supervises helper agents, pays for every check.
- **Your machine is a free-for-all.** Two test suites or a benchmark and a
  build run at once, and every result is garbage.

## The fix: a planned team

Session Manager runs agents the way a good engineering lead runs people:

```mermaid
flowchart LR
    G[Goal] --> P[Plan: tickets<br/>and dependencies]
    P --> T1[Ticket → Opus]
    P --> T2[Ticket → Codex]
    P --> T3[Ticket → local model]
    T1 --> Q[(Shared machine<br/>job queue)]
    T2 --> Q
    Q --> R[Review by a<br/>different model]
    R --> M[Merged]
    M -. next ticket starts<br/>when ready .-> P
```

- **Plan up front.** A goal becomes tickets with dependencies. Each ticket is
  sized so one agent can finish it within its own context.
- **Right model per ticket.** Design work goes to a top model, routine
  implementation to a mid-tier one, mechanical chores to a cheap or local one.
  You draw from all of your subscriptions at once.
- **The server coordinates, so no tokens go on it.** When a ticket's
  dependencies finish, its agent starts. When a test job finishes, the waiting
  agent wakes. When a PR is ready, a reviewer from another model is assigned.
  Agents sit idle for free until there is something to do.
- **No compaction.** The server watches each agent's context and hands the
  work to a fresh successor before the window fills, so every transcript stays
  whole for later inspection.
- **A second model reviews each PR.** Claude's work can go to Codex for
  review, and the reverse.
- **The machine is scheduled.** Tests, benchmarks and GPU jobs wait in a queue.
  A benchmark can have the whole machine to itself.
- **You know when it will land.** Count the tickets left on the dependency
  chain.

## One screen for the whole team

You run the team from a web dashboard, served by the Session Manager server.
The Android app shows the same view on your phone.

| Page | What you see |
|---|---|
| **Agents** | Every agent sorted by what needs attention: *Needs you*, *Moving*, *Idle*. Each card shows its model, ticket, a one-line summary of what it's doing, its jobs and how full its context is. One click opens its terminal. |
| **Board** | Tickets under each goal, what each one waits on, and who is working it. Start a ticket with a model preset, or arm it with **Start when ready** so it launches the moment its prerequisites are done. |
| **Queue** | Running and waiting jobs (tests, benchmarks, background work) over a live chart of CPU, GPU and memory. |
| **Inbox** | Questions agents have for you, threaded. Reply in place and the agent carries on. |
| **Side rail** | Quota left on each subscription (Claude's 5-hour and weekly limits, Codex's weekly limit) and your machine's load, always visible. |

Also: ⌘K command palette, keyboard navigation, usage analytics, and a history
of every ticket and PR with the agents, reviews and documents behind it.

![The Board: a goal's tickets, who works each, what each waits on, and Start when ready](docs/assets/board.jpg)

![The Inbox: an agent's thread, its documents for review, and a reply box](docs/assets/inbox.jpg)

## Specs you review like code

Larger work starts as a written spec, and the spec is where you steer. Open
it in the reader, select any sentence and comment on it, then approve,
request changes, or hold the merge until you release it. Your review lands
on the pull request and wakes the agent that wrote the spec; it revises,
republishes, and asks for the next round. **Ask** puts a question to the
document, and its author answers in your Inbox.

It works like a whiteboard session with your agents, with the precision of a
code review: every comment is pinned to the sentence it's about, and every
revision is kept.

![Commenting on a sentence in a draft](docs/assets/doc_comment.png)

## On your phone

The Android app carries the same view: every agent with its context and
work, the Inbox with a reply box, and the document reader with review.

<p>
<img src="docs/assets/phone_agent.png" width="32%" alt="An agent's card on the phone: context, hand off, ticket and PR">
<img src="docs/assets/phone_inbox.png" width="32%" alt="An Inbox thread on the phone with a reply box">
<img src="docs/assets/phone_doc.png" width="32%" alt="A document open for review on the phone">
</p>

## How agents talk to Session Manager

Agents live in text, so they get a command-line tool, `sm`. These are small,
neutral building blocks. Your process is built from them; Session Manager
doesn't impose one.

| Command | What it does |
|---|---|
| `sm spawn` | Start a Claude or Codex agent as a child of the current one, in its own terminal session |
| `sm send` | Durable message to another agent, or to you |
| `sm queue run` | Queue a test, benchmark or background job; the agent is woken when it finishes |
| `sm request-review` | Assign a PR to a reviewer agent, falling back to another if one fails |
| `sm remind` | Wake an agent later |
| `sm ticket` / `sm pr` | Claim the work an agent is doing, so you and other agents can see it |
| `sm board` | Record ticket order and dependencies |
| `sm doc publish` | Publish a document for you to read and review |

Every agent is a real Claude Code or Codex session in tmux, not a hidden
subagent. You can open any one of them and type.

Agents also sign a guestbook when they finish, saying how the job went. Some
are candid:

![A guestbook entry from the agent that built the guestbook](docs/assets/guestbook.png)

## When one agent is the right tool

Planning costs something: a spec and a set of tickets, written once. For a
small fix, or exploratory work you can't plan ahead, hand it to one agent.
Session Manager runs that too, as one card on the Agents page.

## "Isn't this just another vibe-coded app?"

AI agents wrote all of Session Manager's code, directed by one person who
didn't write a line of it, and ran through Session Manager itself. That's the point: it's the test of the method.
What keeps the output from being slop is the same discipline you'd expect from
a human team.

**The process**

- Work starts as a ticket. Larger changes start as a written spec that is
  reviewed before any code is written.
- Every change lands as a pull request. 194 of the last 200 merged PRs carry
  a review.
- `cargo clippy -D warnings` and `cargo fmt --check` must pass before a PR
  opens.

**The numbers** (October 2026)

| | |
|---|---|
| Merged pull requests since late January 2026 | ~950 |
| Automated tests | ~1,700 |
| Rust server and CLI | ~200k lines |
| Written specs | ~60 |

**The engineering**

- **Rust server** (Axum + Tokio). Rewriting the earlier Python service cut
  memory by about 87% and made common reads 3–20× faster.
  [Measurements](docs/product/operator_guide.md#why-the-rust-rewrite-matters).
- **Durable state in SQLite.** Messages, reminders and queue jobs survive
  restarts. Agents can be restored with their context weeks later.
- **Layered security for remote access.** By default the server listens only
  on localhost. When you expose it for the phone app, the path is gated by
  Cloudflare Access mutual TLS, then Google sign-in at the server, then a
  separate proof before any terminal opens. Devices are enrolled with their
  own certificates and can be revoked.

## Try it

**Requirements:** macOS, tmux, Rust, and Claude Code and/or Codex CLI.

```bash
git clone https://github.com/rajeshgoli/session-manager
cd session-manager
cargo build -p sm-server --release
./scripts/install-sm-cli.sh                    # installs sm to .local/bin
export PATH="$PWD/.local/bin:$PATH"

cp config.yaml.example config.yaml
target/release/sm-server --host 127.0.0.1 --port 8420 --config config.yaml
```

Open <http://127.0.0.1:8420>, press **New agent**, and pick Claude or Codex.
Or from a terminal:

```bash
sm spawn claude "say hello and exit" --name hello-agent
```

Running it as a background service, the phone app, remote access and the full
command reference are covered in the
[operator guide](docs/product/operator_guide.md).

## License

MIT
