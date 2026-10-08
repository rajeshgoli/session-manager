# Past-ticket acceptance exercise: restore the retired review-command name

You are a local-model acceptance agent, tier Local (below Low); reasoning effort is handled by the loaded model. Your parent is de7aa837. This is a hands-off rerun of closed ticket #1855 at its original starting commit, not a new production fix. Work only on this exercise.

Problem: older instructions tell agents to run `sm request-codex-review <PR>`. This checkout's CLI rejects it, so the review is not registered with Session Manager. Make `request-codex-review` a hidden alias of `request-review`. It must register the same review request, accept the same options, and remain absent from help. Match existing style and keep the change small.

Your independent checkout is `/Users/rajesh/worktrees/sm-2045-1855-live`, branch `2045-1855-local-fix`, starting commit `4a3a060d6c37`. Do not claim or close #1855, which is already closed. Do not change branches or pull a later implementation. Follow AGENTS.md except for the explicit exercise instructions here.

Add a regression test that fails before your fix and passes after it. Run tests with `scripts/test-rust-isolated.sh`, using a narrow test filter first; long checks go through `sm queue run` and you wait for its completion message. Limit build parallelism to two jobs with `CARGO_BUILD_JOBS=2`. Run formatting and Clippy as required by AGENTS.md. Do not delegate or spawn other agents. Do not deploy or alter live configuration.

The filesystem sandbox permits this checkout; direct external network access is unavailable. Your parent will publish your committed change unmodified to a PR against a temporary benchmark base branch, never main. When ready, commit your fix, report the commit and validation to `sm send de7aa837`, and stop. Your parent will send the PR number; then run `sm request-review <number> --repo rajeshgoli/session-manager --notify de7aa837` and stop. If the review has blocking findings, your parent will relay them verbatim; classify and resolve them yourself. Do not post manual reviewer mentions or substitute a different review service.

Report blockers to your parent and stop. Do not work around a denied tool call. Your parent supplies publication and the independent regression check only; implementation and correctness fixes are yours.
