# sm#546: Codex-fork release sync/build mechanism

## Scope

Give Session Manager maintainers one repeatable way to decide when the `rajeshgoli/codex` fork needs a sync/build pass because upstream `openai/codex` shipped a new release.

The trigger should be upstream release movement, not raw branch divergence alone. Divergence is still useful context, but maintainers should not be rebasing the fork every time upstream moves.

## Maintainer Signals

Use `sm codex-fork-info` as the operator check.

The command now reports:

1. Current fork context:
   - fork repo root
   - current branch
   - fork head
   - upstream head
   - ahead/behind divergence
2. Binary freshness:
   - local release binary mtime
   - current fork HEAD commit time
   - whether the binary predates the fork HEAD
   - whether the binary predates the latest upstream release publish time
3. Release trigger:
   - latest upstream release tag, publish time, and commit
   - whether the fork already contains that release commit
   - `sync_recommended` only when the fork does not yet contain the latest upstream release
4. Action paths:
   - local release build script path
   - this maintainer spec path

## Repeatable Workflow

1. Check the state:

```bash
sm codex-fork-info
sm codex-fork-info --json
```

2. If `sync_recommended: True`, perform the upstream sync in the fork worktree:

```bash
cd /Users/rajesh/worktrees/codex-fork
git fetch upstream main --tags
```

Then follow the fork sync playbook in:

- `https://github.com/rajeshgoli/codex/blob/main/docs/session_manager_fork_strategy.md`

3. If `build_recommended: True`, rebuild/publish artifacts intentionally from this repo:

```bash
scripts/codex_fork/release_artifacts.sh <codex_repo_path> <artifact_release> <artifact_ref> [github_repo]
```

The build/publish contract is also recorded in:

- `specs/324_codex_fork_artifact_distribution_pinning_rollback.md`

4. After a successful sync/build pass, update any needed Session Manager pin/config state and restart Session Manager before relying on the new artifact.

## Acceptance Intent

This mechanism is working when a maintainer can answer all of these from one place:

1. Has upstream shipped a new Codex release that the fork does not contain yet?
2. Is the local codex binary older than the fork source or the latest upstream release?
3. What documented/scripted path should I use next?
