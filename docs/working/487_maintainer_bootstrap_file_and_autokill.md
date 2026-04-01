# sm#487: maintainer bootstrap prompt file + lessons doc + auto-retire

## Summary

Move the maintainer service role onto the generic file-driven service-role bootstrap path so future workflow changes can be made by editing repo docs instead of touching Python constants. Add a persistent maintainer lessons doc, and automatically retire only auto-bootstrapped maintainer sessions after they have reported `sm task-complete` for 10 minutes.

## Observed Current Behavior

1. Generic service roles already support editable prompt files via `service_roles.<role>.bootstrap_prompt_file` in `config.yaml`.
2. `maintainer` is partially wired into that path, but `SessionManager._refresh_maintainer_service_role_spec()` overwrites the normalized spec with inline prompt text and clears `bootstrap_prompt_file`, so maintainer still behaves as a code-defined special case unless `service_roles.maintainer` is configured explicitly.
3. There is no persistent repo-local lessons doc for maintainer sessions to read before starting work.
4. `sm task-complete` persists `agent_task_completed_at`, but there is no background policy that retires completed auto-bootstrapped maintainer sessions after a grace window.

## Desired Behavior

1. Maintainer should be configurable like `linkedin_agent`: prompt text should live in editable repo files.
2. Maintainer startup instructions should explicitly point the agent at `docs/product/lessons.md` for prior-session learnings.
3. Maintainer instructions should explicitly require:
   - use `sm task-complete` when work is actually finished
   - always report resolution back to the reporting agent via `sm send`
   - request debug info from the reporting agent sparingly, only when blocked
   - treat work as incomplete until PR review, merge, and post-merge restart are done
   - follow the local PR review workflow doc for merge handling
4. Auto-retire should apply only to maintainer sessions created by the auto-bootstrap path. Manually registered maintainer sessions must not be killed by the timeout.

## Implementation

### 1. Move maintainer defaults onto repo-managed prompt files

- Add `docs/product/maintainer_bootstrap.md` as the default maintainer bootstrap prompt file.
- Add `docs/product/lessons.md` as the persistent repo-local lessons file.
- Change the default maintainer bootstrap spec so it can carry `bootstrap_prompt_file`, not just inline `bootstrap_prompt`.
- Preserve `maintainer_agent.bootstrap_prompt_file` through `_refresh_maintainer_service_role_spec()` instead of clearing it.
- Update tracked config/docs so maintainers can be customized without code edits.

### 2. Track whether a maintainer session was auto-bootstrapped

- Add persisted session metadata that marks sessions created by `ensure_role_session()` for an auto-bootstrap service role.
- Set this metadata when a maintainer session is created by the auto-bootstrap path.
- Leave the metadata unset for manually created sessions that later claim `maintainer` via `sm register` / `sm maintainer`.

### 3. Auto-retire completed auto-bootstrapped maintainers

- Add a SessionManager-owned periodic maintenance loop that scans live sessions.
- If all of the following are true, kill the session:
  - role resolves to `maintainer`
  - session was auto-bootstrapped
  - `agent_task_completed_at` is set
  - completion age exceeds 10 minutes
  - session is still live
- Do not apply the policy to manually registered maintainers.
- Keep the loop narrow and config-backed enough that it can be extended later without coupling it to output monitoring.

## Tests

1. Maintainer spec preserves `bootstrap_prompt_file` when derived from legacy `maintainer_agent` config.
2. Maintainer bootstrap prompt renders from repo prompt file and includes lessons reference.
3. Auto-bootstrapped maintainer sessions are marked as such.
4. Manually registered maintainer sessions are not marked auto-bootstrapped.
5. Background maintenance kills only completed auto-bootstrapped maintainer sessions older than threshold.
6. Background maintenance does not kill manual maintainer sessions, incomplete sessions, or non-maintainer roles.

## Ticket Classification

Single ticket.
