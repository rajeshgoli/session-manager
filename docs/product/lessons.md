# Maintainer Lessons

Durable lessons from maintainer sessions. Read this before handling the maintainer queue, and append short high-signal lessons when a session uncovers something future maintainers should know.

## Current Lessons

- Claude Code UI forks can create a new Session Manager session ID while continuing in the same tmux runtime. When that happens, inherited recurring reminders need to be self-identifying so the active fork can cancel them directly.
- The repo already supports generic file-driven service-role bootstrap via `service_roles.<role>.bootstrap_prompt_file` in `config.yaml`. Prefer that path over hard-coded prompt text when workflow instructions need to evolve.
- The maintainer role should prefer the `codex` provider by default. Keep the file-backed `service_roles.maintainer.preferred_providers` entry and the legacy maintainer fallback defaults aligned.
- The local PR review workflow doc lives at `~/.agent-os/workflows/pr_review_process.md`. Use that file for the Codex review / merge loop.
- Maintainer work is not done at PR creation. It is done only after review feedback is handled, the PR is merged, Session Manager is restarted on merged code, and the reporting agent has been notified of the fix.
- Use the reporting agent for missing repro/debug details sparingly. Ask only for specific facts you cannot recover from the running system or repository state.
- Session restore features only work if explicit kill paths preserve the stopped `Session` record instead of deleting it during cleanup. Restore also needs provider-native resume metadata captured while the session is alive, not reconstructed later from guesswork.
- Session restore cannot survive a reboot if `paths.state_file` points at `/tmp`. Keep the session registry on durable storage such as `~/.local/share/claude-sessions/sessions.json`, and migrate legacy temp-backed state before relying on restore after crash/OOM.
- Email bridge identity and user routing should stay file-driven in `config/email_send.yaml`, not hard-coded in Python. Keeping registered users, authorized inbound senders, and the webhook path in one gitignored file makes maintainer changes operational instead of code-only.
- When Telegram is healthy but a specific agent is hard to find, check whether the topic was created before the effective display identity was known. Claude native titles can be discovered lazily from transcripts after topic creation, so the external display sync marker must be current before assuming routing is broken.
- List/readiness paths such as `/sessions`, `/client/sessions`, `sm all`, and `sm me` must not perform live display identity discovery or external sync. Use cached session identity there so stale tmux panes, transcript discovery, or Telegram topic drift cannot block the maintainer queue.
- Mobile attach readiness depends on both local `sm-android-sshd` and the public cloudflared tunnel. If Termux reports `websocket: bad handshake`, check `com.rajesh.sm-android-tunnel` before debugging tmux or app code.
- Telegram topic cleanup must treat `Topic_id_invalid` as already-cleaned remote state and tombstone the local registry record; otherwise one invalid topic can stay active and fail deletion forever.
- Codex app Telegram topics may have no tmux session name. Cleanup must decide whether those records are routable from the live Session Manager session, not from tmux existence alone.
- Live `sm send <friendly-name>` routing must beat registered-email fallback in every mode. When triaging reports about named agents going to email, verify against the running merged service first and keep regression coverage around the CLI resolver path.
- `sm send` should not reuse the generic 5s CLI API timeout. Real `/sessions/{id}/input` delivery can take longer under queue contention, and resolution timeouts must surface as unavailability instead of falling through to email fallback for live session IDs.
- tmux-backed Claude urgent delivery can preserve long-running work by sending a single `Ctrl+B`, waiting for the prompt, then sending `Escape` before injecting the urgent payload. Live `codex` and `codex-fork` probes did not show a reliable equivalent backgrounding path, so keep them on their native interrupt behavior until proven otherwise.
- After `gh pr merge`, wait for GitHub to finish the merge before pulling and restarting launchd. Pulling `main` too early can leave the local checkout behind the merged commit, and a "successful restart" on stale code does not satisfy maintainer completion.
- If a launchd-managed Session Manager restart still serves pre-merge server behavior even though the source tree and launchd path are correct, clear `src/__pycache__/server*.pyc` and kickstart again before assuming the merged server fix failed. Live verification showed stale bytecode could survive a fast restart path.
