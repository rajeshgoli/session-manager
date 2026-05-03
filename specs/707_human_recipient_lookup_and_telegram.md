# Human Recipient Lookup And Telegram Delivery

Issue: #707

## Summary

Make the human/operator a first-class Session Manager recipient. Agents should be able to discover and intentionally contact the user through SM with predictable lookup output and safe aliases, instead of relying on a hidden `sm send rajesh ...` email fallback.

The recommended v1 shape is:

```bash
sm lookup rajesh
sm lookup rajeshgoli
sm lookup user

sm send rajesh "message"
sm send rajeshgoli "message"
sm send user "message"

sm telegram rajesh "message"
sm tg rajesh "message"
```

`sm send` should use the configured default human channel. For this deployment, Telegram should be the normal default and email should be an explicit or fallback channel. `sm telegram` and its short alias `sm tg` should force Telegram delivery and fail clearly if Telegram is not available for that human recipient.

## Problem

Today the recipient surface is inconsistent:

- `sm lookup rajesh` fails with `Role not registered`.
- `sm lookup rajeshgoli` fails with `Role not registered`.
- `sm lookup user` fails with `Role not registered`.
- `sm send rajesh ...` works through a hidden registered-email fallback.
- `sm send rajeshgoli ...` fails.
- `sm send user ...` fails.

Agents cannot infer the right behavior from `sm lookup`, and the working path is discoverable only by trial and error. This also weakens the fallback for Telegram relay issues: the user can tell agents "reach me through SM", but agents need a documented recipient and delivery mechanism.

## Goals

1. Add a first-class human recipient resolver used by `sm lookup`, `sm send`, and explicit Telegram delivery.
2. Support aliases such as `rajesh`, `rajeshgoli`, and `user` from configuration.
3. Make `sm lookup <human-alias>` explain that this is a human/operator route and that email should be used sparingly.
4. Allow agents to intentionally send the user a Telegram message through SM.
5. Deliver Telegram human-recipient messages into the sender agent's existing SM-managed Telegram topic, not a global out-of-context chat.
6. Reserve human aliases so live agents cannot register names such as `user` or `rajesh` and create ambiguous routing.
7. Keep human delivery file-driven, not hard-coded in Python.
8. Record delivery telemetry/audit events without logging unnecessary secrets.

## Non-Goals

1. Do not expose a shell or attach capability through the human recipient path.
2. Do not create arbitrary public inbound messaging. This is an outbound operator notification surface for trusted local SM agents.
3. Do not require agents to know Telegram chat ids or email addresses.
4. Do not remove existing email fallback behavior; make it discoverable, explicit, and lower-precedence than Telegram.
5. Do not solve automatic agent-response relay correctness here; that is covered by #705 and #706.

## Recipient Model

Introduce a file-driven human recipient registry. The implementation may extend the existing email configuration if that is cleaner, but the resulting model should support both email and Telegram.

Recommended shape:

```yaml
humans:
  rajesh:
    display_name: "Human operator"
    aliases:
      - rajeshgoli
      - user
      - operator
    default_channel: telegram
    channels:
      telegram:
        enabled: true
        delivery: sender_session_topic
      email:
        enabled: true
        address_env: SM_OPERATOR_EMAIL
        use: fallback_only
```

Do not commit private email addresses, chat ids, or bot secrets to the public repository. Public docs/specs should use placeholders or environment variable names only.

Hard-coding `rajesh`, `rajeshgoli`, or `user` in command code should be avoided. Local config should define those aliases so future users can configure their own operator identity.

### Telegram Topic Routing

Human-recipient Telegram delivery should use the existing SM-managed Telegram topic for the sending agent whenever possible.

Example: if session `3401-consultant` runs `sm tg rajesh "..."`, the user should see the message in the `3401-consultant` Telegram thread that Session Manager already created and maintains for that agent. The command should not send an out-of-context direct chat or shared global "operator" topic by default.

Rationale:

1. The agent's thread carries the conversation context.
2. SM already owns topic creation, cleanup, naming, and routing for the agent.
3. Telegram threads are cheap and auto-cleaned by existing SM topic maintenance.
4. The user can read and reply in the same context without guessing which agent spoke.

If the sender has no Telegram topic, v1 should either create/use the sender's topic through the existing topic helper or fail clearly with a message that Telegram topic routing is unavailable. It should not silently fall back to an unrelated chat unless config explicitly allows that.

## Resolution Precedence

Human aliases are reserved identifiers. Session creation, `sm register`, and `sm name` should reject any friendly name, role, or alias that exactly matches a configured human canonical name or alias, unless the operation is registering the human recipient itself through config.

`sm send <recipient> ...` should resolve in this order:

1. Exact live session id.
2. Human recipient alias.
3. Exact live session friendly name, alias, or provider-native name.
4. Registered service role.
5. Existing email fallback if still configured separately.

The resolver can put human aliases before friendly-name lookup because registration/name writes prevent those collisions up front. Exact session ids remain first so an operator can always target a concrete session id.

`sm lookup <recipient>` should report an error if the configured registry somehow contains a collision, and command paths should fail closed rather than choosing arbitrarily.

## Commands

### `sm lookup`

Expected output for a human-only match should be concise and explicit:

```text
Human recipient: rajesh
Aliases: rajeshgoli, user, operator
Default delivery: telegram
Available delivery: telegram, email
Telegram delivery posts into the sending agent's SM-managed Telegram thread.
Email is available as fallback/explicit only; use email sparingly.
```

If only email is configured:

```text
Human recipient: rajesh
Default delivery: email
Available delivery: email
Email-only human route; use sparingly.
```

`sm lookup rajesh`, `sm lookup rajeshgoli`, and `sm lookup user` should all resolve to the same canonical human recipient when configured.

### `sm send`

`sm send` should accept human aliases after live-session/role resolution:

```bash
sm send rajesh "message"
sm send rajeshgoli "message"
sm send user "message"
```

Output should make the delivery channel visible:

```text
Telegram sent to rajesh
Thread: sender session topic
```

or:

```text
Email sent to rajesh
```

If the default channel is unavailable, the command may either fail clearly or fall back according to config. The recommended default is:

- use configured `default_channel`;
- if unavailable and `fallback_channels` is configured, try those in order;
- otherwise fail with a clear error.
- do not fall back from Telegram to email unless config explicitly allows fallback and command output states that email was used.

### `sm telegram`

Add an explicit Telegram command:

```bash
sm telegram rajesh "message"
sm tg rajesh "message"
```

Semantics:

- Resolve only human recipients, not live sessions.
- Force Telegram channel.
- Fail if the recipient has no enabled Telegram channel.
- Deliver into the sender agent's existing SM-managed Telegram thread.
- Reuse existing Telegram send/chunking/rate-limit behavior.
- Print a concise success/failure message.

This command gives agents an unambiguous way to contact the user when instructed: "Use `sm telegram rajesh ...` for this conversation." `sm tg` is an ergonomic alias for the same command.

## Safety And Abuse Controls

Human-recipient delivery should be intentionally lightweight but not silent:

- `sm lookup` explains that Telegram is normal for human contact and email should be sparse/explicit.
- `sm send`/`sm telegram` should include channel-specific audit telemetry.
- Rate limits should protect against accidental notification loops, with an override path only if one already exists for urgent SM messages.
- Telegram message content should go to the sender agent's SM-managed Telegram thread by default.
- Email should be used sparingly: explicit email command/channel or configured fallback only.
- Config secrets such as chat ids and email credentials must not be printed.
- The command should be available only to local SM clients with the same trust boundary as existing `sm send`; do not expose this as unauthenticated HTTP.

## Implementation Plan

1. Add or extend a config loader for human recipients and aliases.
2. Add reserved-name enforcement to spawn/name/register paths for configured human canonical names and aliases.
3. Update the CLI/API lookup path to return live sessions, roles, and human recipients instead of treating non-role matches as `Role not registered`.
4. Update `sm send` recipient resolution to support human aliases after exact session-id lookup.
5. Add `sm telegram <human> <message>` and `sm tg <human> <message>` as forced-Telegram delivery commands.
6. Wire Telegram delivery through the existing bot/notifier channel into the sender session's SM-managed Telegram topic.
7. Preserve existing email send behavior, but make it fallback/explicit rather than the normal human channel when Telegram is available.
8. Add tests for alias lookup, reserved-name rejection, forced Telegram delivery to sender topic, email fallback, missing Telegram topic/config, missing Telegram config, and secret redaction.
9. Update `sm -h` / command help so agents can discover the human contact path.

## Acceptance Criteria

1. `sm lookup rajesh`, `sm lookup rajeshgoli`, and `sm lookup user` resolve to the configured human recipient and explain that Telegram is the normal channel while email is sparse/explicit.
2. `sm send rajesh ...`, `sm send rajeshgoli ...`, and `sm send user ...` all route through the configured human default channel.
3. `sm telegram rajesh ...` and `sm tg rajesh ...` send via Telegram into the sender session's SM-managed Telegram topic, or fail clearly when that topic/channel is unavailable.
4. `sm spawn --name user ...`, `sm name <id> user`, and `sm register user` are rejected when `user` is a configured human alias.
5. Email delivery is still available where configured, but command/help text positions it as sparse fallback or explicit delivery, not the normal agent-to-user path.
6. Config secrets and private addresses are not printed in command output, committed example config, or routine logs.
7. Help text documents the human-recipient path clearly enough that an agent can use it when instructed.
