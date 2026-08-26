# App New Agent Flow

Issue: #1396  
Status: product and UX specification only

## Outcome

Add a mobile-first **New agent** flow to the Android Watch experience. The primary entry is contextual: expand an existing agent and choose **New agent** so the new agent inherits that agent's repository, provider family, execution node, and parent relationship. Repository and provider remain editable before creation.

A secondary global **New agent** command remains available in the Watch header menu. It creates a root agent and requires explicit repository and provider choices.

This ticket delivers the specification and browser-viewable static mocks only. It does not add an Android route, API, runtime, authentication rule, or production mutation.

## Why contextual creation is primary

Starting from an agent captures the user's most common intent with the least ambiguity: “add an agent alongside this work.” The existing Watch UI already groups agents by repository and exposes per-agent actions in the expanded card, so this placement preserves context without adding a permanent navigation destination.

The contextual path also matches the current `POST /sessions/spawn` contract:

- `parent_session_id` identifies the selected agent;
- `working_dir` defaults to the parent's working directory;
- `provider` defaults to the parent's provider;
- `node` defaults to the parent's node; and
- a non-empty initial prompt is accepted durably before the runtime starts.

The global command is useful when no existing agent is the right parent. It stays in the Watch header overflow rather than becoming a third bottom-navigation destination: creation is an action, not a persistent top-level area.

## Existing-product fit

The design intentionally reuses the Android app's current visual and interaction language:

- dark `InkBlack` background;
- `Panel` and `PanelElevated` surfaces with subtle borders;
- 18–22 dp rounded cards;
- cyan primary accents, emerald success, rose errors, and muted secondary text;
- monospace repository paths and provider IDs;
- compact status chips and horizontal per-agent action pills; and
- the existing Watch/Analytics bottom navigation.

The Watch screen currently opens an agent's expanded controls from its card. **New agent** joins those controls as the first contextual action. The secondary command is the first item in the Watch header overflow menu.

## Entry paths

### Primary: selected agent

1. On Watch, expand an agent card.
2. Choose **New agent** from its action row.
3. Open the creation surface with:
   - `parent_session_id` fixed to the selected agent;
   - repository preselected from `working_dir`;
   - agent type preselected from the supported provider-family mapping below; and
   - node inherited implicitly and not shown in v1.
4. The user may change repository or agent type before submission. Changing either value does not remove the parent relationship; the resulting child may therefore appear under a cross-repository group in the existing Watch tree.
5. Enter the initial task, optionally enter a name, and create.

The surface title is **New agent** and its context line is **From _agent name_**. A short **Inherited** label appears beside the prefilled repository and agent type until the user changes each field.

### Secondary: global command

1. Open the Watch header overflow.
2. Choose **New agent**.
3. Open the same creation surface without a parent.
4. Repository and agent type initially show unselected placeholders and must each be chosen explicitly.
5. Enter the initial task, optionally enter a name, and create.

The context line is **Start a root agent**. The request omits `parent_session_id`; it uses the server's default execution node.

### Navigation and presentation

On a phone, creation is a full-height surface above Watch with a back/close control and a sticky bottom action area. The form scrolls independently when the keyboard is visible. On wider layouts the same content may be presented as a centered dialog, but field order and state semantics do not change.

Back or close before submission discards the unsaved draft after a confirmation only if the user has changed a field. The loading state cannot be dismissed accidentally. Confirmation offers **Open agent** and **Back to Watch**.

## Form contract

Fields appear in this order:

1. **Repository** — required menu.
2. **Agent type** — required menu.
3. **Initial task** — required multiline text.
4. **Name** — optional text, collapsed under “Optional” only if later usability testing shows the shorter form is preferable.

The first release does not expose model, reasoning effort, wait interval, tracking, execution node, or raw provider IDs as independent controls. Model and reasoning effort use server/provider defaults. Contextual node inheritance and global default-node placement follow the spawn contracts. These can be added as advanced options in a later spec if real usage demands them.

### Initial task semantics

The initial task is the app equivalent of the single accepted `sm spawn` prompt source. The app sends one non-empty UTF-8 text value. The future server/API implementation remains responsible for durable acceptance, digest binding, and delivering the accepted bytes as specified by `specs/1264_spawn_brief_transport.md`.

The app must not create a blank agent and send the task afterward. That would lose the atomic spawn-brief guarantee and introduce an idle, unintended runtime.

### Name semantics

Name is optional. When omitted, Session Manager generates the canonical/session display name. When provided, client validation mirrors the existing friendly-name contract:

- 1–32 characters;
- ASCII letters, numbers, `_`, and `-` only; and
- server-side reserved-identity validation remains authoritative.

The client shows **Use letters, numbers, - or _ (32 max)** as helper text. A server rejection replaces the helper with its exact safe user-facing error.

## Agent type and provider semantics

The UI says **Agent type**, using user-facing names with the actual provider ID as supporting text:

| User-facing type | Submitted provider | Meaning |
| --- | --- | --- |
| Claude Code | `claude` | Managed Claude Code CLI session. |
| Codex | `codex-fork` | Supported detached Codex fork runtime. This is the same target selected by the current `sm spawn codex` alias. |

There is no separate provider-versus-type selection in v1; **Agent type** is the provider-family selector.

Legacy `codex` (stock/original runtime) and retired `codex-app` are not creation choices. For contextual prefilling:

- parent `claude` maps to **Claude Code**;
- parent `codex-fork` maps to **Codex**;
- a legacy parent `codex` maps to the supported **Codex** type and clearly shows supporting ID `codex-fork`; and
- a retired or unknown parent provider leaves Agent type unselected and requires an explicit supported choice.

This preserves the user's provider-family intent without silently creating a retired runtime. Provider availability is server-authoritative; an unavailable type may appear disabled with a short reason, and submission must revalidate it.

## Repository discovery and selection

Repository selection represents an exact server-side `working_dir`, not a GitHub repository slug and not a path on the Android device.

### Candidate source

For the first implementation, the server should return repositories it already knows from retained session records, including active, idle, and restorable stopped sessions. Discovery must:

- canonicalize the server-side absolute working directory;
- deduplicate by `(node, canonical working_dir)`;
- preserve separate worktrees as separate choices because they are distinct working directories;
- attach a display label from the final path component, optional Git remote metadata, node, availability, and latest activity;
- avoid cloning, fetching, checking out, or recursively scanning the host filesystem; and
- avoid accepting an arbitrary phone-entered path in v1.

The exact endpoint and schema are deferred, but it should be a read-only, authenticated client surface rather than discovery reconstructed solely on-device from the currently visible Watch page.

### Menu behavior

Each row shows:

- repository/worktree label, for example `session-manager/`;
- full working directory in monospace;
- **Current** when it matches the selected contextual agent;
- **Worktree** when remote metadata indicates the same Git repository at a different path; and
- **Unavailable** with a reason when the server can no longer use the directory.

Sort order is:

1. contextual current repository, when present;
2. available choices by most recent session activity; then
3. unavailable choices, newest first.

The menu supports local filtering once the list is large enough to require it. Matching covers label, path, and safe remote display metadata. Selecting a repository stores the opaque server-returned identity/path value; the app must not reconstruct or normalize paths itself.

### Empty and stale discovery

If no repository is discoverable, the global flow shows:

> No repositories available  
> Start an agent from the CLI once, then refresh this list.

**Create agent** remains disabled. The contextual flow may still use the selected agent's current repository even if the general discovery list is empty, unless the server marks that working directory unavailable.

If a previously selected repository disappears or becomes unavailable, show an inline repository error and require another choice. A **Refresh repositories** action repeats discovery without closing the draft.

## Validation

Validation runs on blur and on submit. The first invalid field receives focus, and its error is announced to accessibility services.

| Condition | Message | Effect |
| --- | --- | --- |
| Repository missing | **Choose a repository.** | Disable/stop submission. |
| Repository unavailable or stale | **This repository is no longer available. Choose another.** | Clear or replace the selection. |
| Agent type missing | **Choose an agent type.** | Disable/stop submission. |
| Agent type unavailable | Use the server-provided safe reason. | Require another type or retry. |
| Initial task blank after trimming | **Describe what this agent should do.** | Disable/stop submission. |
| Name fails local syntax | **Use 1–32 letters, numbers, - or _.** | Keep the entered value for correction. |
| Server rejects the request | Show the safe server detail in the form-level error panel. | Preserve the draft and allow retry. |

The UI may visually disable the primary button until the three required values are present, but submit-time validation is still required for accessibility, stale selections, and server-authoritative checks.

## Loading, error, and confirmation states

### Repository loading

- Keep the form shell visible.
- Show lightweight skeleton rows inside the open repository menu.
- Preserve a contextual inherited repository while refreshing.
- Do not replace the whole Watch screen with the app's initial-loading spinner.

### Creation loading

- Change the primary label to **Creating agent…** with a spinner.
- Disable fields, back/close, and duplicate submission.
- Keep the entered summary visible so the user can verify what is being created.
- Do not optimistically add an agent to Watch before the server confirms creation.

### Request failure

- Return to the editable form with all values preserved.
- Show one form-level error panel with a concise safe message and **Try again** behavior through the primary button.
- Never expose shell commands, stack traces, credentials, or unrestricted filesystem details beyond repository display data already authorized for the app.

### Confirmation

After a successful response, replace the form with a confirmation state showing:

- **Agent started**;
- friendly/canonical name and session ID;
- agent type/provider;
- repository label and path;
- **Child of _agent_** for contextual creation or **Root agent** for global creation; and
- any safe server warning beneath the summary.

**Open agent** returns to Watch, scrolls to the new session, expands it, and highlights it briefly. **Back to Watch** returns without forced expansion. If the new session is not yet in the next Watch refresh, keep a temporary “Starting” row keyed by the confirmed session ID until normal refresh includes it or a bounded timeout surfaces an error.

## Accessibility and mobile behavior

- All targets are at least 48 dp.
- Menus and the creation surface trap focus appropriately and expose labels, selected values, errors, loading, and confirmation through semantic roles/live regions.
- Color is never the only status signal; text and icons accompany cyan, rose, and emerald states.
- The sticky action area respects system/IME insets.
- Repository paths wrap or ellipsize in menus but are available through accessibility text.
- The form keeps field order and labels stable across contextual and global entry so learning transfers between paths.

## Deferred implementation work

The following are explicitly out of scope for #1396:

- Android navigation, Compose screens, view models, repositories, DTOs, and persistence;
- any server or Rust/Python endpoint for repository discovery or app-driven creation;
- the final API request/response schema, idempotency contract, retry semantics, and compatibility migration;
- auth policy, Cloudflare Access enforcement, Google/device bearer handling, owner/role authorization, and route-local proof;
- security review of repository/path disclosure, allowlists, symlink resolution, node trust, path confinement, and provider/runtime authorization;
- spawn-brief durable storage, digest verification, launch-intent audit records, log redaction, and rate limiting;
- provider installation/availability checks and server configuration changes;
- model, reasoning effort, wait/track, execution-node, worktree creation, clone, fetch, checkout, or arbitrary-path UI;
- analytics events, telemetry retention, rollout, deployment, APK publication, and production session creation.

Before implementation, the API/security design must make the server authoritative for repository availability, provider availability, authorization, idempotent creation, and safe error text. Existing authenticated mobile routes are context, not authorization for a new write route.

## Acceptance criteria for a later implementation

1. An expanded Watch agent exposes **New agent** and opens a form prefilled with editable repository and agent type.
2. Contextual creation retains the selected agent as parent even when repository or type changes.
3. The Watch header overflow exposes the secondary global command with repository and type initially unselected.
4. Only **Claude Code**/`claude` and **Codex**/`codex-fork` are launch choices.
5. Repository choices follow the discovery, deduplication, worktree, sort, empty, and stale semantics above.
6. Initial task is required and is accepted atomically with creation; no blank-then-send flow exists.
7. Validation, repository loading, creation loading, request failure, and confirmation are accessible and preserve the draft where appropriate.
8. Confirmation can return to Watch and locate the server-confirmed session.
9. No client-side path construction, filesystem scanning, clone/fetch/checkout, or retired-provider creation occurs.

## Static mock guide

Open `specs/1396_app_new_agent_mocks/index.html` directly or through a local static HTTP server. The mock control rail switches between contextual and global entry and can force form, validation, repository loading, empty repository, creation loading, and confirmation states. Normal form interactions also open both menus, validate input, simulate loading, and reach confirmation without making network calls.

## Source notes

This specification was derived from the current Android Watch/navigation/theme implementation, `android-app/README.md`, the Rust and Python session creation/spawn contracts, the provider retirement/mapping rules, and `specs/1264_spawn_brief_transport.md` as of issue #1396.

## Ticket classification

Single ticket. #1396 contains only this product/UX specification and static mock package, which one agent can complete without context compaction.
