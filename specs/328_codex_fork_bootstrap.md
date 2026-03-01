# sm#328 — Codex Fork Bootstrap (Completed)

## Scope
Bootstrap a delivery-owned fork of `openai/codex` for Session Manager integration work in epic `#316`.

## Deliverables

1. Fork created in delivery account.
- Repository: [rajeshgoli/codex](https://github.com/rajeshgoli/codex)
- Default branch: `main`
- Fork URL: [https://github.com/rajeshgoli/codex](https://github.com/rajeshgoli/codex)

2. Default branch protection configured.
- Branch: `main`
- No force pushes
- No branch deletions
- Linear history required
- Conversation resolution required
- Required status check: `rust-core`
- Admins enforced

3. CI baseline established for bridge work.
- Added workflow in fork: `.github/workflows/sm-fork-baseline.yml`
- Baseline checks:
  - build: `cargo build -p codex-core -p codex-protocol --locked`
  - test: `cargo test -p codex-protocol --locked`
  - Linux deps: `pkg-config`, `libcap-dev`
- Successful runs on fork `main`:
  - [sm-fork-baseline (workflow_dispatch) #22533880416](https://github.com/rajeshgoli/codex/actions/runs/22533880416)
  - [sm-fork-baseline (push) #22533879040](https://github.com/rajeshgoli/codex/actions/runs/22533879040)

4. Upstream sync workflow documented.
- Doc: [docs/session_manager_fork_strategy.md](https://github.com/rajeshgoli/codex/blob/main/docs/session_manager_fork_strategy.md)
- Cadence: weekly (or immediate for priority upstream fixes)
- Flow: sync branch -> merge upstream -> run baseline CI -> PR to `main`

5. Release tagging conventions documented for SM pinning.
- Tag format: `sm-fork-v<YYYY.MM.DD>-schema-v<N>`
- Tag notes include:
  - upstream base commit SHA
  - fork commit SHA
  - schema version
  - SM compatibility notes

## Fork PR Used During Bootstrap

- [rajeshgoli/codex#1](https://github.com/rajeshgoli/codex/pull/1)
  - Purpose: stabilize baseline workflow to use protocol test suite while retaining bridge build coverage

## Acceptance Criteria Mapping

- Fork repository exists and is accessible from SM automation: satisfied.
- CI runs successfully on the fork default branch: satisfied.
- Upstream sync playbook and ownership are documented: satisfied.
- Ticket `#324` can proceed using fork bootstrap output: satisfied.
