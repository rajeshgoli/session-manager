# sm#570: non-destructive overdue remind handling

Issue: #570

## Goal

When `sm remind` hits its hard threshold, avoid destructively interrupting tmux-backed Claude sessions before the overdue reminder is delivered. Also make the reminder text explicitly say the interrupt came from Session Manager, not from the user.

## Observed current behavior

### Source-level behavior

- `src/message_queue.py` hard-remind delivery queues:
  - `'[sm remind] Status overdue. Run: sm status "message" — if waiting on others: sm turn-complete — if done: sm task-complete'`
  - `delivery_mode="urgent"`
- tmux-backed urgent delivery in `src/message_queue.py` sends raw `Escape`, waits for the prompt, then injects the payload.
- `codex-app` urgent delivery does not use tmux `Escape`; it calls the provider interrupt RPC (`turn/interrupt`) in `src/session_manager.py`.

### Live behavior observed on April 14, 2026

#### Claude Code v2.1.84 in tmux

Disposable session probe:

1. Started `claude --dangerously-skip-permissions --permission-mode bypassPermissions` in a detached tmux session.
2. Prompted Claude to run `sleep 30` in Bash.
3. Observed live tool state:
   - `⏺ Bash(sleep 30)`
   - `⎿ Running…`
   - hint text: `(ctrl+b ctrl+b (twice) to run in background)`
4. Injected tmux `Escape`.
5. Result:
   - `⎿ Interrupted · What should Claude do instead?`

Second probe in the same Claude build:

1. Started another `Bash(sleep 30)` run.
2. Injected `Ctrl+B` via `tmux send-keys`.
3. Result:
   - `⎿ Running in the background (↓ to manage)`
   - prompt returned immediately
   - the task was not cancelled

Follow-up probe against a real Session Manager Claude child on April 14, 2026:

1. Spawned a disposable Claude child via `sm spawn claude ...`.
2. Instructed it to run `sleep 300` in the foreground and `sm send maintainer` when the task was backgrounded.
3. Injected a single tmux `Ctrl+B` to the child pane.
4. Child reported:
   - `probe result: sleep 300 backgrounded and still running`

Negative probe against a second disposable Claude child:

1. Spawned another Claude child running `sleep 300`.
2. Injected `Ctrl+B b` to the child pane.
3. Observed result in the pane:
   - the `sleep 300` tool remained foreground
   - a literal `b` appeared at the Claude prompt/composer
4. `sm what` still showed the child waiting on the foreground sleep
5. The probe child had to be terminated manually

Interpretation:

- Current `sm remind` hard-threshold behavior is destructive for tmux-backed Claude sessions because it uses the same raw `Escape` path as a true interrupt.
- For tmux automation, a single injected `Ctrl+B` is sufficient.
- We should **not** automate `Ctrl+B b`; it can leak a literal `b` into the Claude composer without backgrounding the task.
- The Claude docs say tmux users press it twice because the first keypress is consumed by tmux as the interactive prefix. `tmux send-keys` bypasses that prefix handling and sends the control key directly to Claude.

#### Codex CLI v0.120.0 in tmux

Disposable session probe:

1. Started `codex --no-alt-screen -a never -s danger-full-access` in a detached tmux session.
2. Prompted Codex to run `sleep 30` in the shell.
3. Observed live status:
   - `• Working (3s • esc to interrupt)`
4. Injected `Ctrl+B` once, then again.
5. Observed no documented or visible backgrounding transition.
6. Injected `Escape`.
7. Result:
   - `■ Conversation interrupted - tell the model what to do differently.`

Interpretation:

- Codex currently advertises `esc to interrupt`, not a background shortcut.
- The tmux `Ctrl+B` experiment did not yield a reliable backgrounding behavior.
- We should not reuse the Claude-specific backgrounding path for Codex.

## External documentation

- Claude Code interactive mode docs: `https://code.claude.com/docs/en/interactive-mode`
- Current documented controls include:
  - `Ctrl+B`: background running tasks
  - note: `Tmux users press twice`
  - `Ctrl+C`: cancel current input or generation

## Chosen approach

Split hard-remind delivery by provider family.

### 1. tmux-backed Claude sessions: background first, then inject remind

For providers that use the Claude tmux UI path, replace the hard-remind pre-injection interrupt with Claude-compatible backgrounding:

1. Send exactly one `Ctrl+B` to the target tmux pane instead of `Escape`.
2. Wait for Claude to return to a prompt-ready state.
3. Inject the overdue reminder text as urgent/direct input.

The overdue reminder text should change to something like:

```text
[sm remind] Status overdue. This interrupt came from Session Manager because your status is overdue, not from the user. Send a quick update with: sm status "message" — if waiting on others: sm turn-complete — if done: sm task-complete — then continue your prior work unless the reminder reveals a blocker.
```

Why this is the right behavior:

- It preserves the running Claude task instead of cancelling it.
- It tells the agent exactly why the prompt appeared.
- It reduces the chance that the agent mistakes the interrupt for a user redirect and goes idle.

### 2. Codex and codex-app: keep the existing interrupt path

Do not apply the Claude `Ctrl+B` backgrounding path to:

- `provider == "codex"`
- `provider == "codex-app"`
- `provider == "codex-fork"`

For these providers:

- keep the existing interrupt behavior
- update the overdue reminder text to explicitly say the interrupt is from Session Manager, not the user

Why:

- `codex-app` does not use tmux keystroke interrupt delivery here; it uses an interrupt RPC.
- live Codex tmux probing did not demonstrate a safe/documented `Ctrl+B` background behavior.
- forcing an unverified keybinding into Codex adds risk without evidence.

## Implementation shape

### Queue / delivery logic

- Introduce a provider-aware urgent remind pre-delivery path in `src/message_queue.py`.
- Use Claude backgrounding only for tmux-backed Claude sessions.
- Keep existing urgent delivery for all other providers.

### tmux controller support

- Add a small helper in `src/tmux_controller.py` for sending the Claude background key (`Ctrl+B`) so the behavior is named and testable.
- Do not send repeated follow-up characters such as `b`; the child-agent probe showed that `Ctrl+B b` can leave stray prompt input behind.
- Reuse the existing prompt-wait path after the keypress rather than adding a fixed sleep.

### Reminder copy

- Update the hard-threshold remind text in `src/message_queue.py`.
- Keep the existing `"[sm remind]"` prefix so dedup and existing filtering continue to work.

## Acceptance mapping

1. For tmux-backed Claude sessions, hard-threshold remind no longer cancels a running Claude bash task before delivering the message.
2. The Claude path sends exactly one injected `Ctrl+B`, not `Ctrl+B b`.
3. The hard-threshold remind text explicitly states that the interrupt came from Session Manager because status is overdue, not from the user.
4. The reminder text tells the agent to continue prior work after sending status unless blocked.
5. Codex, codex-app, and codex-fork remain on their existing interrupt path.
6. Existing remind dedup behavior still works because the prefix remains `"[sm remind]"`.

## Tests to add during implementation

- Unit test: hard remind for Claude-backed tmux session sends `Ctrl+B` instead of `Escape` before prompt wait.
- Unit test: Claude-backed hard remind does not append a literal `b` or any extra key after the single `Ctrl+B` background key.
- Unit test: hard remind for Codex-backed session still uses the existing urgent interrupt path.
- Unit test: hard remind text contains `not from the user`.
- Regression test: remind dedup still recognizes the new hard-remind text via the unchanged `"[sm remind]"` prefix.

## Risks

- Claude may background work only for certain active states, so prompt readiness must stay state-based rather than sleep-based.
- If we accidentally apply the Claude path to Codex, we risk undefined provider behavior.
- The longer reminder copy could affect tests or any code that asserts exact strings.

## Mitigations

- Limit the behavior change to tmux-backed Claude sessions only.
- Reuse the existing prompt-wait flow after the background keypress.
- Update exact-string tests together with the copy change.

## Classification

Single ticket.
