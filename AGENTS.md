# Working in this repo

I am Rajesh, I own the repo and am the only human on it. I work with many repos and use agents to do my work. This repo is session manager, which provides core primitives that agents use to run my workflows. My agents use session manager to talk to each other (sm send), schedule tasks and manage device contention (sm queue), request reviews (sm request-codex-review) etc. I use session manager to give me an overview of what's going on with agents, spawn them as needed, track what the agent is doing on the go. This is THE way I know agents are doing what they're supposed to be doing.  

This file is the whole standing contract. Read all of it.

---

## 1. Writing for me

**Define every term before you use it.** Name a phase, gate, structure, or abbreviation and define it in plain words at first use. Never assume a term from an earlier document survived in my memory — I may have read it, but very likely I did not memorise it. When terms predate this repo, I usually made a call on those names. sm send, sm queue run, sm request-codex-review etc., are examples of concepts that have precise meaning and I understand them. When you mean one of these, don't use a generic term. Any abbreviations for decisions or tickets made in the recent past are likely not remembered by me. T1a, Ruling R1, D1b etc., mean nothing to me. An unparseable sentence means I ask you questions and waste tokens and wall time.

**Write in executive style.** Conclusion first, then the justification. Active sentences -- I did X, not X has been completed. Assume I was looking at something else a minute ago. Two ideas to help write: one, write for someone tired, reading at 2 a.m., fresh to the thread. Second, write for an intelligent outsider, who understands trading and technology, but not this repo's shorthands.

**No internal shorthand, and explain in market terms.** Wave letters, gate codes, phase labels, and seat names are fine between agents and wrong in anything addressed to me. 

**Documents are a memo plus appendices, and both ends bind.**

- The memo is 6–9 printed pages and holds my *entire* decision surface. A decision not in the memo will not get made.
- The appendices are unlimited and must be exhaustive enough that an implementing agent cannot go wrong or return with a blocking question.
- I won't read appendices. Don't put anything there that I must see or understand.
- The memo is self-contained: no vocabulary I did not have or you did not introduce.
- **No tombstones; present tense**, as though the document always said this. Do not narrate revisions in the body. The exception is if you're writing a plan document. Then the plan can say what was or strike through and update inline etc. as needed.
- What *did* change between revisions goes in a **FAQ inside the memo** — material you assert I must read, so it counts against the same 6–9 pages. Distinct from the appendix FAQ, which is unlimited and carries nuance and history.
- **Start from `docs/memo_template.html`** — this is a stylesheet and skeleton that works best for printing and reading. 

**Spec should fit in memo as appendices.** The memo carries the decisions, the appendices carry implementation detail deep enough to build from. One document, converging over two or three review rounds. Beyond the memo rules:

- **Formal definitions are great, but they must be backed by examples.** Examples help me understand and usually show holes in reasoning.
- **Every memo requirement appears in the appendices or is dropped with a reason**, and every behaviour any appendix specifies must have code location or test.
- **A spec two engineers would implement differently in observable behaviour is broken.** That, plus no blocking findings, is convergence. It covers behaviour, interfaces, and persisted shapes — not private decomposition or naming. Do not overspecify internals to satisfy it.
- **Cite code or other docs in footnotes as needed.**
- **The author line carries the `sm` id in backticks** — the restore key when a question about intent arrives weeks later.

**Reviewing a document: re-read the whole thing after each revision, not the diff.** A change in one section may have invalidated an assumption in another. A finding qualifies when this change introduced it and you can name what breaks. On a multi-option architectural call, post which option you would pick and why rather than only critiquing.

**Prefer a image or chart to a paragraph.** Please use charts, diagrams, or tables to clarify concepts. This is much easier for me to grok than it is to read dense text. Unlike you, I don't entirely live in the textual dimension :).

 A design document ships as a single self-contained HTML file in `docs/working/` that prints cleanly, and **graduates to `specs/` when the work it specifies ships** — `working/` is deleted at arc close, so a converged spec left there disappears from under the implementation and review that depend on it. Until then it is unshipped, which is why `docs/specs/` may not change without the implementation changing with it.

**Give me decisions and options, not questions.** Bring only what I alone can decide, with a recommendation
and a reason. "Should we delete this?" is not a question for me; "I recommend deleting this because X — confirm?" is. If it's deep in the code that agents wrote, I have no idea why it came about. That is like asking a programmer to reason about assembly. Bring it to my level if my decision is needed. If not make one and tell me.

**Reach me in-session, in prose.** Don't use AskUserQuestion unless I ask you to interview me. I am often on mobile — so prose, one question at a time works best. If sending me email, batch all your questions together.  Email (`sm email rajesh`) only on my standing ask, at an event I named such as convergence, or after roughly seventeen minutes of silence, which signals I stepped away. Say in the subject if it blocks.

---
## 2. Cost and context

**Token budget beats wall-clock.** Out of tokens means work stops until a quota reset a week away; wall-clock delay costs hours. When they conflict, spend time and save tokens.

**Match the model to the work.** Ill-defined problems, design authority, and spec authorship take the top tier; well-specified implementation and review the middle; mechanical work — inventory, fixture plumbing, narrow docs — the low. State tier and effort explicitly on every spawn.

| Tier | Claude | Codex |
|---|---|---|
| Top | fable, xhigh effort | astra, high effort |
| Mid | opus 1M, high | terra, high or sol, medium |
| Low | sonnet 1M, high | luna, high |

**Top tier agents may delegate routine work.** You may use lower-tier subagents for research, lookup, inventory, and well specified fixes as needed. Note that if I assign you a task, and don't explicitly say you are an orchestrator, do not delegate core work. For e.g., Astra/high or Fable/high on a ticket task was chosen because the ticket work is complex. This means you can use research agents of lower tier, but implementation or spec writing or review or whatever the core task was MUST be done inline by the agent. 

**Delegation mechanism.** An expensive seat may use its provider's built-in subagents for work inside its own task.

**One agent, one task.** Retire on completion. Reuse only when restoring context is genuinely cheaper than briefing fresh.

**A ticket is what one agent finishes without compaction.** If it does not fit, split it before briefing anyone.

**Use `sm send` between agents ONLY if instructed to use it** — If your task requires you to talk to other agents use sm send. If I did not say anything, and if you are not an orchestrator, do not message another agent. Don't poll another agent's output; go idle and you will be woken.

**When told to stand by, go idle.**

**Long-running commands go through Session Manager from the start.** Submit them with `sm queue run --type <tests|perf|background> --label <label> --cwd <worktree> -- <command>`, then go idle. The durable queue automatically sends an `[sm queue]` completion wake to the current managed session (or an explicit `--notify` target); do not add a second watcher, `sleep`, `tail -f`, or poll. A process started outside the queue cannot be registered later with `sm watch-job`. `perf` jobs hold the entire machine. Use it only when you need noise-free machine. You will be asked to wall time budget it and the job will be terminated if it doesn't complete in the window. 

**On `[sm remind]`, run `sm status "what you are doing now"` and carry on.** Not an interrupt, and it is what makes a seat legible to the watchdog.

**Split parallel work across non-overlapping file sets.** A worktree stops agents corrupting each other's git state; it does nothing about two tickets editing one module and colliding at merge. If the file sets overlap, serialize.

**Never wrap a message containing backticks or `$()` in double quotes.** The shell substitutes before Session Manager sees it, which can run a local command and corrupt the message. Use single quotes, a heredoc, or a file payload. Keep in mind when relaying review text through `sm send`.

**Report `sm` bugs rather than working around them.** File in `rajeshgoli/session-manager`, then reach the maintainer: `sm lookup maintainer` for the seated session id, then `sm send <id>` with the description and issue link (`sm roster` lists registered roles).

## 3. Your workflow

If I explicitly asked you to be a maintainer, register as maintainer, sm maintainer, or sm register maintainer. Otherwise, do not register as maintiner.

**Name yourself.** If you are not maintainer, Before you begin work, check your name with `sm me`. If it is `claude-<slug>`, `codex-fork-<slug>`, or anything similar, replace it with `sm name <newname>`. `<ticket>-engineer`, `<ticket>-scout`, `<spec-section>-engineer`, `<pr>-spec-repair`, `<ticket>-spec-author`, `<ticket>-spec-reviewer` and `<pr>-reviewer-<round>` all beat `claude-<slug>`, because a name that says what you were doing is what lets me restore you.

**Worktrees.** Every agent works in its own worktree under `~/worktrees/sm-<ticket>-<slug>`, build outputs inside. If you're maintainer, use a stable `~/worktrees/sm-maintainer` worktree where possible. You may use a ticket specific worktree where needed, but you should ensure it's deleted when it's no longer useful.

 Workflow as usual:
 1. Rebuild and restart session manager as required. If it's pure sm app update, you don't need to restart session manager, otherwise you may need to. Restart using `scripts/restart-rust-server.sh`. Follow other maintainer lessons from `docs/product/lessons.md` as needed. 
 2. If I need to test something let me know. For example, if something can be tested with sm cli or sm app, let me know exactly what to try out.  If you can test directly that's preferred. For example, if you can reliable reproduce the issue I reported and you can verify it no longer occurs, you can tell me what you did and ask me to try it optionally.
 3. Once all feature requests above are completed and verified, you may exit to step 4. If I have feedback or if you find live test failures, repeat steps 1 and 2 until exit to 4 criteria is met. 
 4. Once functionality is in place, create a PR for your changes.
 5. Use instructions in Review loop section to get your PR in a clean mergable state.
 6. Once clean, squash merge the PR, delete local and remote branches or worktrees you may have created.
 7. Rebuild and restart session manager if required. Be sure to use `scripts/restart-rust-server.sh` if restarting.
 8. Clean up any old builds and binary detritus so it's in clean state. Let me know. Cargo clean is a must and sm binary should still contain your latest code.
 9. If there are process learnings of things that all future workers on this repo need to know, write them down in `docs/product/lessons.md`. Note the bar to writing here should be high. You're costing tokens on every agent that follows you. Default to not writing if you're in doubt.
    
## 4. Review loop
Request a review with `sm request-codex-review <pr-number>`. Treat the response as registration only, then go idle — do not poll. If Session Manager cannot take the request, post `@codex review` as a PR comment, check back after five minutes, again after five more. If codex hasn't acknowledged your review after 10 minutes with 👀 smiley, you can re-post the request. If nothing has landed after 20 minutes, you can re-post the review request.

Before acting on a review, confirm it belongs to your current request and was posted after your latest push. A review existing is not enough on its own.

Then:

1. **Classify every finding: valid, partially valid, or invalid.** Do not skip this. A review is not gospel — push back with reasoning where it is wrong.
2. **Correctness only** — no document nits, no wording preferences, no nits about following process for process's sake. That excludes process *preference*, not process *correctness*: when the thing under review is a workflow, an instruction file, CI, or a deploy step, its behaviour is the correctness surface, and a defect in it is a correctness finding however procedural it sounds. "This deploys without re-running the tests" is a bug, not a nit.
3. **Any unresolved P1 blocks.** A P1 is resolved either by fixing it or by answering it: a P1 you classify invalid, with your reasoning posted on the PR, is resolved and does not block. It is unresolved only while it is neither fixed nor answered — otherwise a single false positive strands the PR forever, since there is no code change to push for the next round. If the reviewer re-raises the same P1 after reading your reasoning, that is a real disagreement: escalate it rather than looping. A round that returns only P2 or lower, or a clean review, exits the loop. Do not keep chasing P2s and P3s. `specs/1268_pr_review_process.md` does not apply here. Session manager is not a high risk repo. The high risk repo is my primary repo only. 
5. Fix, push, and re-review at the exact head. Fewest rounds to correctness — which does not mean dropping correctness issues.
