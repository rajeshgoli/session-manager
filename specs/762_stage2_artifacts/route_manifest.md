# Stage 2 Route Manifest

Generated: 2026-06-06T12:42:08-07:00

Provenance commands:

- `python3 - <<PY (AST scan of src/server.py decorators, app.add_api_route, and app.mount)`
- `rg -n "add_api_route|@app\.(get|post|put|patch|delete|websocket)|app\.mount" src/server.py`

Reconciliation status: source-derived pass 2 for Stage 2 convergence review. Rows marked manual or supplemental are extracted from source patterns that are not directly represented by decorators, argparse metadata, or local SQLite files.
Decorated routes: 123

Dynamic add_api_route rows: 2

Mounted static rows: 1

## Reconciliation Notes

- Rows 1-2 and rows 3-4 are mutually exclusive runtime registrations: rows 1-2 exist only when the watch frontend dist directory is missing; rows 3-4 exist only when the built SPA directory is present.
- Row 83 is the concrete default inbound email path `/api/email-inbound`; it is registered by `app.add_api_route(...)` for that concrete default path and is explicitly Google-auth exempt in `GoogleAuthMiddleware`.
- Row 84 is conditional. The concrete runtime path is the normalized value returned by `EmailHandler.bridge_webhook_path()` from `config/email_send.yaml` key `email_bridge.webhook_path`; if that value differs from `/api/email-inbound`, the route is registered as an additional alias but is not added to `GoogleAuthMiddleware.exempt_paths` by current source.
- Generated method/path notation has been normalized to outward API notation. Conditional config-derived paths remain marked as config-derived instead of Python symbol names.

| # | method | path | handler | source | kind | request models | other params | response model | include schema |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | GET | /watch | watch_frontend_not_available | src/server.py:3539 | decorator |  |  |  | False |
| 2 | GET | /watch/{_path:path} | watch_frontend_not_available_path | src/server.py:3543 | decorator |  | _path: str |  | False |
| 3 | GET | /watch | watch_frontend_root | src/server.py:3549 | decorator |  |  |  | False |
| 4 | MOUNT | /watch | WatchStaticFiles(directory=str(watch_dist), html=True) | src/server.py:3552 | mount |  |  |  |  |
| 5 | GET | / | root | src/server.py:3633 | decorator |  |  |  |  |
| 6 | GET | /events/state | event_state | src/server.py:3640 | decorator |  |  |  |  |
| 7 | GET | /events | event_stream | src/server.py:3653 | decorator |  |  |  |  |
| 8 | POST | /hooks/tmux-client | tmux_client_hook | src/server.py:3683 | decorator |  | event: str, session: Optional[str], client_session: Optional[str], tty: Optional[str], client_pid: Optional[str] |  |  |
| 9 | GET | /auth/session | auth_session | src/server.py:3727 | decorator |  |  |  |  |
| 10 | GET | /client/bootstrap | client_bootstrap | src/server.py:3777 | decorator |  |  | ClientBootstrapResponse |  |
| 11 | GET | /client/analytics/summary | client_analytics_summary | src/server.py:3782 | decorator |  |  |  |  |
| 12 | POST | /auth/device/google | auth_device_google | src/server.py:3791 | decorator | request: DeviceGoogleAuthRequest |  | DeviceGoogleAuthResponse |  |
| 13 | POST | /deploy/{app_name} | deploy_app_artifact | src/server.py:3828 | decorator |  | app_name: str | AppArtifactDeployResponse |  |
| 14 | GET | /apps/{app_name}/latest.apk | get_latest_app_artifact | src/server.py:3922 | decorator |  | app_name: str |  |  |
| 15 | GET | /apps/{app_name}/{artifact_hash}.apk | get_hashed_app_artifact | src/server.py:3937 | decorator |  | app_name: str, artifact_hash: str |  |  |
| 16 | GET | /apps/{app_name}/meta.json | get_app_artifact_metadata | src/server.py:3953 | decorator |  | app_name: str | AppArtifactMetadataResponse |  |
| 17 | GET | /apk | get_legacy_apk_download | src/server.py:3961 | decorator |  |  |  |  |
| 18 | GET | /auth/google/login | google_login | src/server.py:3966 | decorator |  | next: Optional[str] |  |  |
| 19 | GET | /auth/google/callback | google_callback | src/server.py:3995 | decorator |  | state: Optional[str], code: Optional[str], error: Optional[str] |  |  |
| 20 | GET | /logged-out | logged_out_landing | src/server.py:4051 | decorator |  |  |  | False |
| 21 | GET | /auth/logout | auth_logout | src/server.py:4090 | decorator |  | next: Optional[str] |  |  |
| 22 | GET | /health | health | src/server.py:4101 | decorator |  |  |  |  |
| 23 | GET | /health/detailed | health_detailed | src/server.py:4106 | decorator |  |  | HealthCheckResponse |  |
| 24 | POST | /sessions | create_session | src/server.py:4559 | decorator | request: CreateSessionRequest |  | SessionResponse |  |
| 25 | POST | /sessions/create | create_session_endpoint | src/server.py:4602 | decorator |  | working_dir: str, provider: str, parent_session_id: Optional[str], node: Optional[str] |  |  |
| 26 | GET | /sessions | list_sessions | src/server.py:4659 | decorator |  | include_stopped: bool |  |  |
| 27 | GET | /nodes | list_nodes | src/server.py:4674 | decorator |  |  |  |  |
| 28 | POST | /nodes/{node_id}/ping | ping_node | src/server.py:4684 | decorator |  | node_id: str |  |  |
| 29 | GET | /nodes/{node_id}/restore-candidates | list_node_restore_candidates | src/server.py:4695 | decorator |  | node_id: str, refresh: bool |  |  |
| 30 | POST | /nodes/{node_id}/restore-candidates/{session_id}/restore | restore_node_restore_candidate | src/server.py:4709 | decorator |  | node_id: str, session_id: str | SessionResponse |  |
| 31 | WEBSOCKET | /nodes/agent | node_agent_websocket | src/server.py:4727 | decorator |  |  |  |  |
| 32 | GET | /client/sessions | list_client_sessions | src/server.py:4792 | decorator |  |  |  |  |
| 33 | POST | /client/request-status | request_client_status | src/server.py:4813 | decorator |  |  | ClientRequestStatusResponse |  |
| 34 | POST | /client/bug-reports | submit_client_bug_report | src/server.py:4855 | decorator | payload: ClientBugReportRequest |  | ClientBugReportResponse |  |
| 35 | POST | /client/sessions/{session_id}/attach-ticket | create_mobile_attach_ticket | src/server.py:4995 | decorator |  | session_id: str | MobileAttachTicketResponse |  |
| 36 | GET | /client/terminal | mobile_terminal_websocket_required | src/server.py:5531 | decorator |  |  |  |  |
| 37 | WEBSOCKET | /client/terminal | mobile_terminal_websocket | src/server.py:5555 | decorator |  |  |  |  |
| 38 | POST | /client/mobile-terminal/disable | disable_mobile_terminal | src/server.py:5620 | decorator |  |  | MobileTerminalDisableResponse |  |
| 39 | GET | /sessions/{session_id}/attach-descriptor | get_attach_descriptor | src/server.py:5672 | decorator |  | session_id: str |  |  |
| 40 | GET | /sessions/context-monitor | get_context_monitor_status | src/server.py:5685 | decorator |  |  |  |  |
| 41 | POST | /sessions/{session_id}/fork | fork_session | src/server.py:5701 | decorator | request: ForkSessionRequest | session_id: str |  |  |
| 42 | GET | /sessions/{session_id} | get_session | src/server.py:5738 | decorator |  | session_id: str | SessionResponse |  |
| 43 | GET | /client/sessions/{session_id} | get_client_session | src/server.py:5751 | decorator |  | session_id: str |  |  |
| 44 | GET | /sessions/{session_id}/codex-events | get_codex_events | src/server.py:5768 | decorator |  | session_id: str, since_seq: Optional[int], limit: int |  |  |
| 45 | GET | /sessions/{session_id}/activity-actions | get_codex_activity_actions | src/server.py:5792 | decorator |  | session_id: str, limit: int |  |  |
| 46 | GET | /sessions/{session_id}/codex-pending-requests | list_codex_pending_requests | src/server.py:5815 | decorator |  | session_id: str, include_orphaned: bool |  |  |
| 47 | POST | /sessions/{session_id}/codex-requests/{request_id}/respond | respond_codex_request | src/server.py:5840 | decorator | request: CodexRequestRespondRequest | session_id: str, request_id: str |  |  |
| 48 | PATCH | /sessions/{session_id} | update_session | src/server.py:5899 | decorator |  | session_id: str, friendly_name: Optional[str], is_em: Optional[bool] | SessionResponse |  |
| 49 | PUT | /sessions/{session_id}/role | set_session_role | src/server.py:5968 | decorator | request: SetRoleRequest | session_id: str | SessionResponse |  |
| 50 | DELETE | /sessions/{session_id}/role | clear_session_role | src/server.py:5998 | decorator |  | session_id: str | SessionResponse |  |
| 51 | PUT | /sessions/{session_id}/maintainer | set_session_maintainer | src/server.py:6017 | decorator | request: SetMaintainerRequest | session_id: str | SessionResponse |  |
| 52 | DELETE | /sessions/{session_id}/maintainer | clear_session_maintainer | src/server.py:6038 | decorator | request: SetMaintainerRequest | session_id: str | SessionResponse |  |
| 53 | POST | /maintainer/ensure | ensure_maintainer | src/server.py:6057 | decorator | request: EnsureMaintainerRequest |  | EnsureMaintainerResponse |  |
| 54 | POST | /registry/{role}/ensure | ensure_agent_registry_role | src/server.py:6083 | decorator | request: EnsureRoleRequest | role: str | EnsureMaintainerResponse |  |
| 55 | GET | /registry | list_agent_registry | src/server.py:6114 | decorator |  |  |  |  |
| 56 | GET | /registry/{role} | lookup_agent_registry | src/server.py:6127 | decorator |  | role: str | AgentRegistrationResponse |  |
| 57 | POST | /sessions/{session_id}/registry | register_agent_role | src/server.py:6142 | decorator | request: RoleRegistrationRequest | session_id: str | AgentRegistrationResponse |  |
| 58 | DELETE | /sessions/{session_id}/registry | unregister_agent_role | src/server.py:6171 | decorator | request: RoleRegistrationRequest | session_id: str | AgentRegistrationResponse |  |
| 59 | POST | /sessions/{session_id}/context-monitor | set_context_monitor | src/server.py:6199 | decorator | request: ContextMonitorRequest | session_id: str |  |  |
| 60 | POST | /sessions/{session_id}/notify-on-stop | arm_stop_notify | src/server.py:6245 | decorator | request: ArmStopNotifyRequest | session_id: str |  |  |
| 61 | PUT | /sessions/{session_id}/task | update_task | src/server.py:6304 | decorator |  | session_id: str, task: str |  |  |
| 62 | POST | /sessions/{session_id}/input | send_input | src/server.py:6319 | decorator | request: SendInputRequest | session_id: str |  |  |
| 63 | POST | /sessions/input-batch | send_input_batch | src/server.py:6343 | decorator | request: SendInputBatchRequest |  | SendInputBatchResponse |  |
| 64 | POST | /sessions/{session_id}/key | send_key | src/server.py:6368 | decorator |  | session_id: str, key: str |  |  |
| 65 | POST | /sessions/{session_id}/clear | clear_session | src/server.py:6388 | decorator | request: ClearSessionRequest | session_id: str |  |  |
| 66 | POST | /sessions/{session_id}/invalidate-cache | invalidate_session_cache | src/server.py:6444 | decorator |  | session_id: str, arm_skip: bool |  |  |
| 67 | DELETE | /sessions/{session_id} | kill_session | src/server.py:6466 | decorator |  | session_id: str |  |  |
| 68 | POST | /sessions/{session_id}/restore | restore_session | src/server.py:6494 | decorator |  | session_id: str | SessionResponse |  |
| 69 | POST | /sessions/{session_id}/open | open_terminal | src/server.py:6515 | decorator |  | session_id: str |  |  |
| 70 | GET | /sessions/{session_id}/output | capture_output | src/server.py:6532 | decorator |  | session_id: str, lines: int |  |  |
| 71 | GET | /sessions/{session_id}/tool-calls | get_tool_calls | src/server.py:6550 | decorator |  | session_id: str, limit: int |  |  |
| 72 | GET | /sessions/{session_id}/last-message | get_last_message | src/server.py:6643 | decorator |  | session_id: str |  |  |
| 73 | GET | /sessions/{session_id}/summary | get_summary | src/server.py:6652 | decorator |  | session_id: str, lines: int |  |  |
| 74 | POST | /sessions/{session_id}/subagents | register_subagent_start | src/server.py:6745 | decorator | request: SubagentStartRequest | session_id: str | SubagentResponse |  |
| 75 | POST | /sessions/{session_id}/subagents/{agent_id}/stop | register_subagent_stop | src/server.py:6782 | decorator | request: SubagentStopRequest | session_id: str, agent_id: str |  |  |
| 76 | GET | /sessions/{session_id}/subagents | list_subagents | src/server.py:6820 | decorator |  | session_id: str |  |  |
| 77 | POST | /notify | send_notification | src/server.py:6846 | decorator | request: NotifyRequest |  |  |  |
| 78 | GET | /humans | list_human_recipients | src/server.py:6879 | decorator |  |  |  |  |
| 79 | GET | /humans/{identifier} | lookup_human_recipient | src/server.py:6889 | decorator |  | identifier: str | HumanRecipientResponse |  |
| 80 | POST | /humans/{identifier}/telegram | send_human_telegram | src/server.py:6894 | decorator | request: HumanDeliveryRequest | identifier: str |  |  |
| 81 | POST | /humans/{identifier}/email | send_human_email | src/server.py:6899 | decorator | request: HumanDeliveryRequest | identifier: str |  |  |
| 82 | POST | /email/send | send_registered_email | src/server.py:6904 | decorator | request: SendEmailRequest |  |  |  |
| 83 | POST | /api/email-inbound | inbound_email_webhook | src/server.py:6913 | add_api_route(default email webhook) | InboundEmailRequest |  | status payload dict |  |
| 84 | POST | normalized `email_bridge.webhook_path` value when configured != /api/email-inbound | inbound_email_webhook | src/server.py:6927 | add_api_route(configured email alias) | InboundEmailRequest | runtime path from config/email_send.yaml email_bridge.webhook_path | status payload dict |  |
| 85 | POST | /hooks/claude | claude_hook | src/server.py:7183 | decorator |  |  |  |  |
| 86 | POST | /sessions/spawn | spawn_child_session | src/server.py:7665 | decorator | request: SpawnChildRequest |  |  |  |
| 87 | GET | /sessions/{session_id}/review-results | get_review_results | src/server.py:7742 | decorator |  | session_id: str |  |  |
| 88 | POST | /sessions/{session_id}/review | start_review | src/server.py:7794 | decorator | request: StartReviewRequest | session_id: str |  |  |
| 89 | POST | /sessions/review | spawn_review | src/server.py:7820 | decorator | request: SpawnReviewRequest |  |  |  |
| 90 | POST | /reviews/pr | start_pr_review | src/server.py:7860 | decorator | request: PRReviewRequest |  |  |  |
| 91 | GET | /sessions/{parent_session_id}/children | list_children_sessions | src/server.py:7879 | decorator |  | parent_session_id: str, recursive: bool, status: Optional[str], include_terminated: bool |  |  |
| 92 | GET | /admin/rollout-flags | get_rollout_flags | src/server.py:7951 | decorator |  |  |  |  |
| 93 | GET | /admin/codex-fork-runtime | get_codex_fork_runtime | src/server.py:7968 | decorator |  |  |  |  |
| 94 | GET | /admin/codex-launch-gates | get_codex_launch_gates | src/server.py:7982 | decorator |  |  |  |  |
| 95 | POST | /sessions/{target_session_id}/kill | kill_session_with_check | src/server.py:7996 | decorator | request: KillSessionRequest | target_session_id: str |  |  |
| 96 | POST | /sessions/{session_id}/handoff | schedule_handoff | src/server.py:8035 | decorator | request: HandoffRequest | session_id: str |  |  |
| 97 | POST | /sessions/{target_session_id}/adoption-proposals | create_adoption_proposal | src/server.py:8064 | decorator | request: CreateAdoptionProposalRequest | target_session_id: str |  |  |
| 98 | POST | /adoption-proposals/{proposal_id}/accept | accept_adoption_proposal | src/server.py:8084 | decorator |  | proposal_id: str |  |  |
| 99 | POST | /adoption-proposals/{proposal_id}/reject | reject_adoption_proposal | src/server.py:8104 | decorator |  | proposal_id: str |  |  |
| 100 | POST | /sessions/{session_id}/task-complete | task_complete | src/server.py:8124 | decorator | request: TaskCompleteRequest | session_id: str |  |  |
| 101 | POST | /sessions/{session_id}/turn-complete | turn_complete | src/server.py:8186 | decorator | request: TaskCompleteRequest | session_id: str |  |  |
| 102 | GET | /sessions/{session_id}/send-queue | get_send_queue | src/server.py:8211 | decorator |  | session_id: str |  |  |
| 103 | POST | /scheduler/remind | schedule_reminder | src/server.py:8227 | decorator |  | session_id: str, message: str, delay_seconds: int, recurring_interval_seconds: Optional[int] |  |  |
| 104 | DELETE | /scheduler/remind/{reminder_id} | cancel_scheduled_reminder | src/server.py:8262 | decorator |  | reminder_id: str |  |  |
| 105 | POST | /sessions/{session_id}/remind | register_remind | src/server.py:8284 | decorator | request: PeriodicRemindRequest | session_id: str |  |  |
| 106 | DELETE | /sessions/{session_id}/remind | cancel_remind | src/server.py:8313 | decorator |  | session_id: str |  |  |
| 107 | POST | /job-watches | create_job_watch | src/server.py:8329 | decorator | request: JobWatchCreateRequest |  | JobWatchResponse |  |
| 108 | GET | /job-watches | list_job_watches | src/server.py:8363 | decorator |  | target_session_id: Optional[str], include_inactive: bool |  |  |
| 109 | DELETE | /job-watches/{watch_id} | cancel_job_watch | src/server.py:8382 | decorator |  | watch_id: str | JobWatchResponse |  |
| 110 | POST | /queue-jobs | create_queue_job | src/server.py:8397 | decorator | request: QueueJobCreateRequest |  | QueueJobResponse |  |
| 111 | GET | /queue-jobs | list_queue_jobs | src/server.py:8431 | decorator |  | notify_target: Optional[str], type: Optional[str], state: Optional[str], include_terminal: bool |  |  |
| 112 | GET | /queue-jobs/{job_id} | get_queue_job | src/server.py:8461 | decorator |  | job_id: str | QueueJobResponse |  |
| 113 | DELETE | /queue-jobs/{job_id} | cancel_queue_job | src/server.py:8476 | decorator |  | job_id: str | QueueJobResponse |  |
| 114 | POST | /queue-policy-runs | create_queue_policy_run | src/server.py:8491 | decorator | request: QueuePolicyRunCreateRequest |  | QueuePolicyRunResponse |  |
| 115 | GET | /queue-policy-runs | list_queue_policy_runs | src/server.py:8519 | decorator |  | policy: str, limit: int, include_suppressed: bool |  |  |
| 116 | GET | /queue-policy-runs/status | get_queue_policy_run_status | src/server.py:8536 | decorator |  | policy: str, dedupe_token: Optional[str], id: Optional[str] | QueuePolicyRunResponse |  |
| 117 | GET | /queue-policy-runs/{run_id} | get_queue_policy_run | src/server.py:8558 | decorator |  | run_id: str | QueuePolicyRunResponse |  |
| 118 | POST | /codex-review-requests | create_codex_review_request | src/server.py:8573 | decorator | request: CodexReviewRequestCreateRequest |  | CodexReviewRequestResponse |  |
| 119 | GET | /codex-review-requests | list_codex_review_requests | src/server.py:8613 | decorator |  | notify_target: Optional[str], repo: Optional[str], pr_number: Optional[int], include_inactive: bool |  |  |
| 120 | GET | /codex-review-requests/{request_id} | get_codex_review_request | src/server.py:8648 | decorator |  | request_id: str | CodexReviewRequestResponse |  |
| 121 | DELETE | /codex-review-requests/{request_id} | cancel_codex_review_request | src/server.py:8663 | decorator |  | request_id: str | CodexReviewRequestResponse |  |
| 122 | POST | /sessions/{session_id}/agent-status | set_agent_status | src/server.py:8678 | decorator | request: AgentStatusRequest | session_id: str |  |  |
| 123 | POST | /sessions/{target_session_id}/watch | watch_session | src/server.py:8714 | decorator |  | target_session_id: str, watcher_session_id: str, timeout_seconds: int |  |  |
| 124 | POST | /hooks/tool-use | hook_tool_use | src/server.py:8750 | decorator |  |  |  |  |
| 125 | POST | /hooks/context-usage | hook_context_usage | src/server.py:8884 | decorator |  |  |  |  |
| 126 | POST | /admin/cleanup-idle-topics | cleanup_idle_topics | src/server.py:9033 | decorator |  |  |  |  |
