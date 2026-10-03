# Accept the retired `sm request-codex-review` name so agents on stale instructions still register reviews

**What happened.** sm-1788 opened PR #1808, then ran `sm request-codex-review 1808` as its instructions said. The installed CLI refused it, because #1786 renamed the command to `sm request-review`. The agent then fell back to posting `@codex review` by hand, which sm does not track. The web board therefore had no pending review for that agent. After 30 idle minutes it showed "Stalled: agent idle, nothing running", when the right status was "Codex review on PR #1808".

**Why the agent had the old name.** #1786 updated the instructions on origin/main here and in fractal-algo-rust, deskbar and office-automation. But agents read them from wherever they are launched: `~/projects/session-manager` was 51 commits behind origin/main, and `~/projects/fractal-algo-rust` 2 behind. Agents started before the deploy also carry the old instructions in their context.

**Evidence.** In `logs/5a74b97f-6dc8e5f20d0d.codex-fork.events.jsonl`, the final message at 2026-09-30T23:35:37Z says the CLI rejected `sm request-codex-review`. The next event is 00:06:12Z, the `[sm claim] Idle 30m` nudge. The board's idle reading was correct; the review registration was missing.

**Fix.** Make `request-codex-review` a hidden alias of `request-review`, so an agent following stale instructions registers its review instead of falling back.

sm-bug-fix (345aeec1)


You are a local agent. Your model runs on this Mac, not in the cloud, and your tier is Local: below Low. Work only on the ticket above.

- Do not delegate, spawn subagents, or start other agents.
- Keep the change as small as the ticket allows. Match the surrounding code's style.
- Add a test that fails before your fix and passes after it. Run it, then run the tests for the module you changed.
- Run tests with `scripts/test-rust-isolated.sh`, never bare `cargo test`. Pass a test-name filter to keep runs short.
- Before you finish, run `cargo fmt -p sm-server` and `cargo clippy -p sm-server --all-targets -- -D warnings`.
- Normally you would open a PR and run `sm request-review`, and answer `[sm remind]` with `sm status`. The instructions below replace that for this run.
- If you are blocked, stop and write down what blocks you and what you tried. Do not guess at requirements you cannot check.

Work only in this directory. Do not push, open a PR, or message anyone. Commit your fix locally with a test, run the tests, and stop.
Session Manager (`sm`) is not reachable in this run.
Two existing tests fail inside this run's sandbox on a clean checkout too, for environmental reasons: `queue::tests::process_ceiling_leaves_the_reserve_below_the_user_ceiling` and `runtime::tests::real_tmux_server_start_does_not_inherit_a_racing_threads_pipe`. You do not need to investigate them.
