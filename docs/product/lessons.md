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
