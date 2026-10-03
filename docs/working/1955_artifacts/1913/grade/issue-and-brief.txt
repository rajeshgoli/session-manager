# Queue jobs inherit live server listener file descriptors

## Problem

Jobs launched by `sm queue run` inherit the live Rust server's open listener file descriptors. A child test process retained the production HTTP listener and handover socket while running an isolated test suite.

## Reproduction

Run `scripts/test-rust-isolated.sh` through `sm queue run --type tests`. The `http::tests::test_isolation_direct_harness_without_wrapper_cannot_open_production_state_paths` test starts a child test binary and inspects it with `lsof -p <pid>`. That child had `127.0.0.1:8420 (LISTEN)` and `~/.local/share/claude-sessions/handover.sock` open. The test correctly failed because a test child should not hold production server sockets.

Observed in the #1832 validation queue job `job_ab4ea472297f` on 1 October 2026. The test's effective database paths were isolated; the live socket descriptors were inherited from the queue runner, not opened by the test.

## Expected behavior

Queue job processes should receive only the descriptors they need for stdin, stdout, and stderr. Server listener and authority socket descriptors must be closed on exec or explicitly closed in the job child. Otherwise jobs can keep old listeners alive across server restarts and give tests or tools access to production sockets.


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
