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
```

`sm send` should use the configured default human channel. `sm telegram` should force Telegram delivery and fail clearly if Telegram is not configured for that human recipient.

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
3. Make `sm lookup <human-alias>` explain that this is a human/operator route and should be used sparingly or when requested by the user.
4. Allow agents to intentionally send the user a Telegram message through SM.
5. Preserve existing live-session routing semantics so human aliases do not accidentally steal messages meant for active agents.
6. Keep human delivery file-driven, not hard-coded in Python.
7. Record delivery telemetry/audit events without logging unnecessary secrets.

## Non-Goals

1. Do not expose a shell or attach capability through the human recipient path.
2. Do not create arbitrary public inbound messaging. This is an outbound operator notification surface for trusted local SM agents.
3. Do not require agents to know Telegram chat ids or email addresses.
4. Do not remove existing email fallback behavior; make it discoverable and consistent.
5. Do not solve automatic agent-response relay correctness here; that is covered by #705 and #706.

## Recipient Model

Introduce a file-driven human recipient registry. The implementation may extend the existing email configuration if that is cleaner, but the resulting model should support both email and Telegram.

Recommended shape:

```yaml
humans:
  rajesh:
    display_name: Rajesh
    aliases:
      - rajeshgoli
      - user
      - operator
    default_channel: telegram
    channels:
      telegram:
        enabled: true
        chat_id: "${SM_TELEGRAM_USER_CHAT_ID}"
      email:
        enabled: true
        address: rajeshgoli@gmail.com
```

Hard-coding `rajesh`, `rajeshgoli`, or `user` in command code should be avoided. Local config should define those aliases so future users can configure their own operator identity.

## Resolution Precedence

`sm send <recipient> ...` should resolve in this order:

1. Exact live session id.
2. Exact live session friendly name, alias, or provider-native name.
3. Registered service role.
4. Human recipient alias.
5. Existing email fallback if still configured separately.

This preserves the existing lesson that live named sessions beat registered-email fallback. If a human alias collides with a live session name, `sm send` should route to the live session. To force the human path in a collision, support one explicit namespace:

```bash
sm send human:rajesh "message"
sm telegram rajesh "message"
```

`sm lookup <recipient>` should not hide collisions. If both a live session and a human recipient match, it should print both matches and explain which route `sm send` will choose by default.

## Commands

### `sm lookup`

Expected output for a human-only match should be concise and explicit:

```text
Human recipient: rajesh
Aliases: rajeshgoli, user, operator
Default delivery: telegram
Available delivery: telegram, email
Use sparingly; this notifies the human/operator directly.
```

If only email is configured:

```text
Human recipient: rajesh
Default delivery: email
Available delivery: email
Use sparingly; this notifies the human/operator directly.
```

`sm lookup rajesh`, `sm lookup rajeshgoli`, and `sm lookup user` should all resolve to the same canonical human recipient when configured.

### `sm send`

`sm send` should accept human aliases after live-session/role resolution:

```bash
sm send rajesh "message"
sm send rajeshgoli "message"
sm send user "message"
sm send human:rajesh "message"
```

Output should make the delivery channel visible:

```text
Telegram sent to rajesh
```

or:

```text
Email sent to rajesh <rajeshgoli@gmail.com>
```

If the default channel is unavailable, the command may either fail clearly or fall back according to config. The recommended default is:

- use configured `default_channel`;
- if unavailable and `fallback_channels` is configured, try those in order;
- otherwise fail with a clear error.

### `sm telegram`

Add an explicit Telegram command:

```bash
sm telegram rajesh "message"
```

Semantics:

- Resolve only human recipients, not live sessions.
- Force Telegram channel.
- Fail if the recipient has no enabled Telegram channel.
- Reuse existing Telegram send/chunking/rate-limit behavior.
- Print a concise success/failure message.

This command gives agents an unambiguous way to contact the user when instructed: "Use `sm telegram rajesh ...` for this conversation."

## Safety And Abuse Controls

Human-recipient delivery should be intentionally lightweight but not silent:

- `sm lookup` warns that it notifies the human/operator directly.
- `sm send`/`sm telegram` should include channel-specific audit telemetry.
- Rate limits should protect against accidental notification loops, with an override path only if one already exists for urgent SM messages.
- Message content should go to the configured delivery channel, but config secrets such as chat ids and email credentials must not be printed.
- The command should be available only to local SM clients with the same trust boundary as existing `sm send`; do not expose this as unauthenticated HTTP.

## Implementation Plan

1. Add or extend a config loader for human recipients and aliases.
2. Update the CLI/API lookup path to return live sessions, roles, and human recipients instead of treating non-role matches as `Role not registered`.
3. Update `sm send` recipient resolution to support human aliases after live-session and role resolution.
4. Add `human:<name>` explicit namespace for collision-free human sends.
5. Add `sm telegram <human> <message>` as a forced-Telegram delivery command.
6. Wire Telegram delivery through the existing bot/notifier channel so chunking and telemetry stay consistent.
7. Preserve existing email send behavior, but make it part of the human-recipient resolver when possible.
8. Add tests for alias lookup, collision precedence, forced human namespace, forced Telegram delivery, email fallback, missing Telegram config, and secret redaction.
9. Update `sm -h` / command help so agents can discover the human contact path.

## Acceptance Criteria

1. `sm lookup rajesh`, `sm lookup rajeshgoli`, and `sm lookup user` resolve to the configured human recipient and warn that this notifies the human/operator.
2. `sm send rajesh ...`, `sm send rajeshgoli ...`, and `sm send user ...` all route through the configured human default channel when there is no live-session collision.
3. `sm telegram rajesh ...` sends via Telegram or fails clearly when Telegram is not configured.
4. A live session named `user` still receives `sm send user ...` by default; `sm send human:rajesh ...` still reaches the human.
5. Config secrets are not printed in command output or routine logs.
6. Help text documents the human-recipient path clearly enough that an agent can use it when instructed.
