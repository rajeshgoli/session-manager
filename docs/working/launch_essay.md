# I stopped prompting agents and started managing them

*Rajesh Goli · draft for launch · prepared by `sm-marketing` (db9c55e5)*

<!-- Bracketed notes like [Rajesh: …] mark places only you can fill. Delete them before publishing. -->

There's a tempting way to use a modern coding agent. You hand it the whole
epic, say "keep going until it's done," and walk away. The agents are good
enough now that this sometimes works.

I did that for a while. Then I noticed what I was actually doing all day:
reading transcripts and guessing. How far along is it? Is "almost done"
twenty minutes or three hours? Did it forget the decision we made this
morning? Is it about to run the full test suite while another agent is
benchmarking on the same machine? I wasn't directing work. I was hoping.

So I stopped hoping and started running my agents the way I'd run an
engineering team.

## What a team does that one agent doesn't

A good engineering team doesn't hand a single engineer the whole roadmap and
check back in a month. It plans. The goal becomes tickets, the tickets get an
order, and each one goes to the person suited to it. Someone else reviews the
work before it ships. Shared resources, like the test cluster or the one
machine with the big GPU, get scheduled instead of fought over. And at any
moment, a lead can look at the board and say when it will land.

None of that depends on the workers being human. It depends on structure. So
I built the structure.

[Rajesh: one or two sentences on the moment this clicked — a specific epic
that went sideways with a single agent, if you have one.]

## Session Manager

[Session Manager](https://github.com/rajeshgoli/session-manager) is the
control room I run my agents from. It pools Claude, Codex and local models
into one team, across all my subscriptions, on my own machine.

- **I plan the sprint up front.** A goal becomes tickets with dependencies,
  each sized so a single agent can finish it without running out of context.
  Decisions live in the spec and the tickets, not in a memory that gets
  compressed.
- **Each ticket goes to the right model.** Design to the strongest model,
  routine implementation to a mid-tier one, mechanical chores to the cheapest.
  If one subscription is near its limit, I send work to another.
- **The server coordinates, not an agent.** When a ticket's prerequisites
  finish, its agent starts. When a test job completes, the waiting agent
  wakes up. When a PR is ready, a reviewer from another model gets it. Agents
  waiting on something cost nothing. Not one token goes on an agent
  supervising other agents.
- **My machine is scheduled.** Tests, benchmarks and GPU jobs wait their turn
  in a queue, and a benchmark can have the whole machine to itself.
- **I watch it on one screen.** A web dashboard sorts every agent by what
  needs me, what's moving and what's idle. A board shows each goal's tickets
  and what they wait on. Meters show how much quota I have left on each
  subscription and how loaded my machine is. When I'm away, the same view is
  on my phone, and agents' questions arrive in an inbox I can answer from
  anywhere.

<!-- ASSET: Agents page screenshot on demo data. -->

The result is that I know where things stand. Not "the agent says it's
nearly done," but "four of nine tickets are merged, two are in review, and the
critical path has two left."

## "Isn't this just vibe-coded slop?"

Fair question. Agents wrote nearly all of Session Manager. I directed them,
reviewed what mattered, and ran them through Session Manager itself. That's
the honest test of the idea: if the method produces slop, the tool built with
it would be slop.

Here's what came out of it since late January:

- about 950 merged pull requests, each starting as a ticket;
- written specs for the larger changes, reviewed before code was written;
- a review on 194 of the last 200 merged PRs;
- about 1,700 automated tests, and lint and format checks that must pass
  before any PR opens;
- a Rust server that replaced the first Python version, cutting memory by
  about 87% and making common reads 3–20× faster;
- remote access gated by mutual TLS, sign-in at the server, and a separate
  proof before any terminal opens.

What separates this from slop isn't who typed the code. It's whether there's
a plan, a review and a test standing between the code and `main`. Agents can
work inside that discipline just as people do. They just need someone to set
it up and a system that holds them to it.

## What I've learned running a team of agents

[Rajesh: three or four lessons, in your voice. Candidates, if they ring true:]

- **Size the ticket to the context, not the calendar.** A ticket is what one
  agent can finish without compacting. Anything bigger gets split before
  anyone starts.
- **Match the model to the work.** The best model writing boilerplate wastes
  money; a cheap model designing a system wastes time.
- **Never let an agent grade its own work.** A second model catches what the
  first one talked itself into.
- **Make waiting free.** Most token waste I saw was agents polling, checking
  and supervising. Move that into the server and it disappears.

## When not to bother

If the job is small, or it's exploration you can't plan ahead, hand it to one
agent and let it go. Planning has a cost. Session Manager runs that case fine
too — it's just one card on the dashboard. The team model pays off when the
work is big enough that you'd want a plan if people were doing it.

## Try it

There's a [live demo](https://sm-demo.rajeshgo.li) of a team running a sprint,
and the code is on [GitHub](https://github.com/rajeshgoli/session-manager)
under the MIT license. The tool is deliberately neutral: it gives you the
building blocks, and my way of working is just one way to use them.

If you run several agents at once and have felt that you're hoping rather
than directing, I'd like to hear how you handle it.
