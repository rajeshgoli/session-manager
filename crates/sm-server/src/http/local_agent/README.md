# Verified local-agent HTTP policy

The main API router and terminal LAN router verify the gateway signature before
ordinary route dispatch. A bare `X-SM-Local-Agent` header is removed and has no
authority. A supplied gateway signature must verify; missing, stale, corrupt or
inactive service state never falls back to the localhost owner bypass. The
registered agent must also exist as a live session.

The production verifier reads the same private `local-egress` service directory
as `ServiceClient::production`. Host composition/scratch servers can set
`AppState::with_local_agent_gateway_directory` to the service client's directory.
Tests use isolated paths. Neither directory selection nor registration is an
agent-accessible HTTP operation.

After verification, the request carries `VerifiedLocalAgent` in its extensions
and `local_identity::current()` during its dispatch. This scoped value cannot be
constructed from an identity string. It is not inherited by spawned tasks.
Code starting durable work must capture the verified identity before spawning;
#2009 persists it independently of requester and notification recipient.

The request layer removes credentials, cookies, forwarding metadata and wire
stamp headers, and supplies the existing session header names with the verified
id. Existing session-credential checks accept only that live agent while in the
verified scope; parent/child and reviewer assignment checks still apply. Outside
this scope the credential and owner/hosted behavior remains unchanged. Owner
authorization helpers refuse to promote the scoped identity.

## Caller and recipient rules

`local_agent.rs::policy` is the complete method/path allowlist. The table below
specifies the identity-bearing routes. All identifiers refer to exact registered
session ids. Bodies must be JSON objects. Omitted or forged caller fields are
replaced before typed handler extraction; recipient fields are preserved.

| Route (POST unless stated) | Bound caller or restriction |
| --- | --- |
| `/claims`, `/claims/release`, `/claims/worktree` | `requester_session_id` |
| `/worktrees/keep`, `/worktrees/delete` | `requester_session_id`; existing worktree ownership rules apply |
| `/queue-jobs`, `/review-requests` | `requester_session_id`; preserve `notify_target` and its aliases |
| `/merge-holds`, `/merge-holds/release` | `requester_session_id` |
| `/email/send`, `/humans/{identifier}/email` | `requester_session_id`; preserve recipients |
| `/humans/{identifier}/messages` | `sender_session_id`; preserve recipient path |
| `/sessions/{id}/input`, `/sessions/input-batch` | `sender_session_id`; preserve targets/recipients; remove supplied parent provenance and bind reply-reminder cancellation to caller |
| `/docs`, `/board/links`, `/board/lanes` | `session_id` (author/actor) |
| `PUT /review-policies` | `session_id`; retain ticket-only and author exclusion checks; reviewer selection is unchanged |
| `/review-requests/{id}/submit` | `session_id`; retain assigned-reviewer check |
| `/sessions/{id}/what` | `requester_session_id`; preserve target path |
| `/sessions/{id}/retire` and `/kill` compatibility alias | `requester_session_id`; retain live direct-parent and protected-root checks |
| `/sessions/{id}/reparent-requests`, `/reparent-tree-requests` | `requester_session_id`; preserve subject, target and target-parent ids; retain consent rules |
| `/reparent-requests/{id}/approve`, `/reject` | `requester_session_id`; retain consent rules |
| `/sessions/{id}/notify-on-stop` | `requester_session_id` and `sender_session_id`; preserve watched target path |
| `PATCH /sessions/{id}`, `/sessions/{id}/agent-status` | Exact own id in path; reject `is_em` changes |
| `/sessions/{id}/task-complete`, `/turn-complete`, `/clear`, `/context-monitor`, `/handoff` | Exact own id in path and bound `requester_session_id`; preserve notification recipient |
| `/scheduler/remind` | Query `session_id` is caller; preserve reminder message and timing |
| `DELETE /scheduler/remind/{id}` | Stored reminder recipient must be caller |
| `DELETE /review-requests/{id}` | Stored requester must be caller, regardless of notify recipient |
| `DELETE /queue-jobs/{id}`, `/queue-jobs/{id}/cancel` | Fail closed pending #2009's persisted submitting-agent check |
| `GET /reparent-requests` and `/{id}` | Verified session headers and existing consent visibility rules |

Explicitly admitted GETs are agent-facing session, registry, node, human,
queue/review, published-document, claims/board/history, bug and usage reads, plus
health. Their selectors name data to read, not a caller, and remain unchanged.
HEAD follows the corresponding GET rule.

Every other method/path is denied to a stamped caller. This includes owner
clients/settings/terminal/inbox/notes, credential rotation and human reparent
approval/repair, handoff policy, role/maintainer changes, hooks, deploy, shadow
forwarding, session creation/restore, subagent registration and unknown routes.
Adding a new route never implicitly grants local-agent access. #1956 consumes
the verified identity for provider-level no-spawn enforcement; this change adds
no opencode implementation.

Queue submission and cancellation return 503 for verified local agents until
#2009 installs durable wall binding. This prevents a partially deployed gateway
from executing a user command on the host. Hosted/owner queues are unchanged.
#1974 still owns final runtime composition before local-agent launch.

## Verification

`http::local_agent::tests` exercises every registered path/method through the
middleware to prove either verified dispatch or denial, including scope isolation
and preservation of targets. Real-router tests verify forged/omitted identities,
reminder persistence and cancellation, review cancellation with notify elsewhere,
parent authorization, own/cross-agent status updates, terminal/owner denials and
the temporary queue boundary. The unsigned-request test confirms that a forged
bare local-agent header has no effect on existing hosted/owner behavior.
