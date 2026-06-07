# Stage 2 Config Manifest

Generated: 2026-06-06T12:42:08-07:00

Provenance commands:

- `python3 - <<PY (indent scan of config examples plus regex scan of code .get("key") calls)`
- `rg -n "config\.get|\.get\(\"[A-Za-z0-9_\.-]+\"" src config.yaml.example config/email_send.yaml.example config/client.yaml.example`
- `nl -ba src/cli/client.py` and `nl -ba config/client.yaml.example` for verified CLI/client env, path, timeout, and node-routing contracts
- `nl -ba src/main.py` for YAML plus local-env overlay precedence and Android/mobile auth env mapping
- `nl -ba src/node_runner.py` and selected `src/session_manager.py` node sections for remote placement, node-agent, hook secret, and restore-inventory cache contracts
- `nl -ba src/main.py src/session_manager.py src/mobile_analytics.py src/infra_supervisor.py src/output_monitor.py src/tmux_controller.py` for source-defined non-example defaults
- `nl -ba config.yaml.example config/client.yaml.example config/email_send.yaml.example` for exact example defaults and compatibility classifications

Reconciliation status: source-derived pass 6 for Stage 2 convergence review. Rows marked manual or supplemental are extracted from source patterns that are not directly represented by decorators, argparse metadata, or local SQLite files.
## Verified Server Config Keys Missing Or Ambiguous In Raw Regex Pass

The rows below are source-verified configuration contracts from `src/server.py`, `src/email_handler.py`, and app setup. They separate real config keys from the overinclusive raw `.get()` scan later in this artifact.

Stage 2 handoff rule: rows in verified sections and the exact example-default summary are configuration contracts for migration planning. Raw `.get()` rows and raw key-path rows later in this artifact are discovery evidence only; implementation tickets must not treat a raw row as a required config key unless it is promoted here, appears in the exact example-default summary, or is explicitly classified in a later Stage 3/4 decision.

| key path | source | default / coercion | outward contract | classification |
| --- | --- | --- | --- | --- |
| watch_frontend.dist_path | src/server.py:3525 | defaults to `web/sm-watch/dist` under repo root | decides whether `/watch` serves SPA or 503 JSON fallback | browser/watch diagnostics contract |
| paths.app_artifacts_dir | src/server.py:2837 | defaults to `data/apps` under repo root | storage root for `/apps/{app}/latest.apk`, hashed APK, and `meta.json` | mobile app distribution contract |
| paths.bug_reports_db | src/server.py:2843 | defaults to `data/bug_reports.db` under repo root | SQLite spool used by `/client/bug-reports` | native app support contract |
| bug_reports.max_reports | src/server.py:2849 | coerced to int, min 1, default 30 | bounds retained bug reports in SQLite spool | native app support contract |
| mobile_terminal.enabled | config.yaml.example:170, src/server.py:2476 | truthy config and runtime kill switch both required | enables high-priority mobile attach path | first-class mobile contract |
| mobile_terminal.public_path_prefix | src/server.py:2528 | normalized leading slash, empty for `/` | prefixes attach-ticket and WebSocket URLs behind reverse proxies | first-class mobile contract |
| mobile_terminal.ws_url | src/server.py:2548 | explicit URL overrides derived host; rejected when require_tls and not `wss://` | WebSocket URL returned to mobile app | first-class mobile contract |
| mobile_terminal.require_tls | src/server.py:2545 | default true; false permits `ws://` only for localhost/testserver-derived hosts | transport security for mobile terminal | security-sensitive mobile contract |
| mobile_terminal.allowed_origins | src/server.py:5563 | list normalized by stripping trailing slash; public host origin auto-added | WebSocket origin allowlist | security-sensitive mobile contract |
| mobile_terminal.ticket_ttl_seconds | src/server.py:5025 | int, min 5, max 300, default 30 | lifetime of single-use attach ticket | security-sensitive mobile contract |
| mobile_terminal.auth_frame_timeout_seconds | src/server.py:5573 | int, min 1, max 30, default 3 | first WebSocket auth-frame timeout | security-sensitive mobile contract |
| mobile_terminal.max_attach_seconds | src/server.py:5181 | int, min 30, max 86400, default 3600 | maximum mobile terminal attach lifetime | first-class mobile contract |
| mobile_terminal.initial_resize_wait_seconds | src/server.py:5182 | float, min 0, max 10, default 2.0 | pre-attach resize grace period for mobile renderer | first-class mobile contract |
| mobile_terminal.history_preload_lines | src/server.py:5352 | int, min 0, max 20000, default 4000 | scrollback replay before live stream | first-class mobile contract |
| mobile_terminal.max_concurrent_attaches_global | src/server.py:5037, src/server.py:5111 | int, min 1, max 64, default 4 | global attach/ticket quota | security-sensitive mobile contract |
| mobile_terminal.max_concurrent_attaches_per_user | src/server.py:5038, src/server.py:5112 | int, min 1, max 16, default 1 | per-user attach/ticket quota | security-sensitive mobile contract |
| mobile_terminal.max_concurrent_attaches_per_session | src/server.py:5039, src/server.py:5113 | int, min 1, max 16, default 1 | per-session attach/ticket quota | security-sensitive mobile contract |
| mobile_terminal.device_signature_max_skew_seconds | src/server.py:2731 | int, min 5, max 600, default 60 | anti-replay timestamp window for device signatures | security-sensitive mobile contract |
| mobile_terminal.allowed_users.*.email / aliases / interactive_shell_access / owner flags / registered_device_keys | src/server.py:2571-2633, src/server.py:5625-5631 | dict/list; disabled keys rejected | user/device authorization for mobile terminal and disable control | security-sensitive mobile contract |
| context_monitor.warning_percentage | src/server.py:8980 | default 50 | warning threshold for context usage hook alerts | managed-agent hook contract |
| context_monitor.critical_percentage | src/server.py:8980 | default 65 | urgent critical threshold for context usage hook alerts | managed-agent hook contract |
| email.bridge_config | config.yaml.example:150, src/main.py:253 | defaults through EmailHandler when unset | path to email bridge config used for inbound/outbound human email | external-service contract |
| email_bridge.webhook_path | src/email_handler.py:131, config/email_send.yaml.example:40 | normalized to leading slash; default `/api/email-inbound` | conditional dynamic inbound route alias | security-sensitive email ingress contract |
| email_bridge.worker_secret | src/email_handler.py:143, src/server.py:2072 | optional; when set must match configured header | shared-secret protection for inbound worker delivery | security-sensitive email ingress contract |
| email_bridge.worker_secret_header | src/email_handler.py:137, config/email_send.yaml.example:37 | default `x-email-worker-secret`, lowercased | header name carrying worker shared secret | security-sensitive email ingress contract |
| email_bridge.session_id_header | src/email_handler.py:149, config/email_send.yaml.example:39 | default `x-email-session-id`, lowercased | trusted explicit session-id routing header after worker-secret validation | security-sensitive email ingress contract |
| email_bridge.authorized_senders | src/email_handler.py:157, config/email_send.yaml.example:33 | string or list normalized to lowercase set | sender allowlist for inbound replies | security-sensitive email ingress contract |

## Verified Server Local-Env Overlay Contract

`load_config()` first loads YAML, then overlays values derived from a local env file. This is a first-class compatibility contract for public-host auth, Android/mobile bootstrap, cloudflared SSH attach, and cookie/session behavior.

| behavior / env key | source | default / coercion | precedence / mapping | outward contract | classification |
| --- | --- | --- | --- | --- | --- |
| local env file path | src/main.py:194-209 | default path is `<config.yaml parent>/.local/android-parity/values.env`; caller-provided `local_env_path` replaces that default; missing/empty file leaves YAML unchanged | YAML is loaded first; `_merge_dicts(config, _build_local_auth_overrides(env_values))` overlays env-derived nested keys over YAML | gitignored local deployment settings can override checked-in examples without editing tracked config | public-host/mobile auth overlay contract |
| local env parser | src/main.py:120-129 | strips each line; ignores blank lines, comments, and lines without `=`; splits on first `=`; strips key and value; no shell expansion or quote unwrapping documented | parsed key/value map feeds `_build_local_auth_overrides()` | Rust must preserve accepted env-file syntax and ignored-line behavior for existing local files | local-env parsing contract |
| `PUBLIC_HTTP_HOST` | src/main.py:137, src/main.py:156-160, src/main.py:171-176 | stripped string; empty means absent | maps to `auth.google.public_host`, derives `auth.google.redirect_uri = https://<host>/auth/google/callback`, maps to `external_access.public_http_host`, and is required for env-derived `auth.google.enabled = True` | public host advertised to browser/mobile clients and Google redirect/auth middleware | security-sensitive public/mobile contract |
| `PUBLIC_SSH_HOST` | src/main.py:138, src/main.py:177-178 | stripped string; empty means absent | maps to `external_access.public_ssh_host` | SSH/cloudflared target returned for remote attach/Termux flows | mobile/remote attach contract |
| `HTTP_ORIGIN_URL` | src/main.py:139, src/main.py:179-180 | stripped string; empty means absent | maps to `external_access.http_origin_url` | origin URL used when deriving public/mobile URLs behind tunnels or reverse proxies | mobile/browser routing contract |
| `SSH_USERNAME` | src/main.py:140, src/main.py:181-182 | stripped string; empty means absent | maps to `external_access.ssh_username` | username included in generated SSH/Termux attach metadata | mobile/remote attach contract |
| `SSH_PROXY_COMMAND` | src/main.py:141, src/main.py:183-184 | stripped string; empty means absent | maps to `external_access.ssh_proxy_command` | cloudflared/proxy command included in remote attach metadata | mobile/remote attach contract |
| `GOOGLE_WEB_CLIENT_ID` | src/main.py:142, src/main.py:161-162, src/main.py:171-172 | stripped string; empty means absent | maps to `auth.google.client_id`; required for env-derived Google auth enablement | browser Google auth client ID and mobile bootstrap auth metadata | security-sensitive auth contract |
| `GOOGLE_WEB_CLIENT_SECRET` | src/main.py:143, src/main.py:151-154, src/main.py:165-166, src/main.py:171-172 | stripped string; empty means absent | maps to `auth.google.client_secret`; required for env-derived auth enablement; used to derive session cookie secret when `SESSION_COOKIE_SECRET` is absent | Google OAuth callback exchange and derived cookie secret behavior | credential-bearing auth contract |
| `GOOGLE_ANDROID_CLIENT_ID` | src/main.py:144, src/main.py:163-164 | stripped string; empty means absent | maps to `auth.google.android_client_id` | Android device-auth ID-token audience and mobile bootstrap config | first-class mobile auth contract |
| `ALLOWLIST_EMAIL` | src/main.py:145-149, src/main.py:167-168, src/main.py:171-172 | replaces semicolons with commas; splits on comma; strips entries; drops empty entries | maps to `auth.google.allowlist_emails`; nonempty list required for env-derived auth enablement | public/browser/device auth allowlist | security-sensitive auth contract |
| `SESSION_COOKIE_SECRET` | src/main.py:150-154, src/main.py:169-170 | stripped string; if empty and `GOOGLE_WEB_CLIENT_SECRET` exists, derive `sha256("sm-google-session:<PUBLIC_HTTP_HOST>:<GOOGLE_WEB_CLIENT_SECRET>")` | maps to `auth.google.session_cookie_secret`; explicit env value wins over derived value; required for env-derived auth enablement | browser session-cookie signing and rollback compatibility for existing local env | credential-bearing auth contract |
| env-derived Google enablement | src/main.py:171-172 | sets `auth.google.enabled = True` only when `PUBLIC_HTTP_HOST`, `GOOGLE_WEB_CLIENT_ID`, `GOOGLE_WEB_CLIENT_SECRET`, nonempty `ALLOWLIST_EMAIL`, and session secret are present | local-env overlay can enable Google auth even if YAML has `auth.google.enabled: false`; missing any required field leaves enablement unset | prevents half-configured public auth from becoming active while preserving current mobile/browser bootstrap behavior | security-sensitive public/mobile auth contract |

## Verified Nodes And Remote-Placement Config Contract

`nodes.*` is a security-sensitive trust-boundary contract. It controls where sessions are created, how commands are SSH-routed, what hook/API environment remote sessions receive, how node-agent WebSocket auth works, and what `/nodes` exposes.

| key / behavior | source | default / coercion | outward contract | classification |
| --- | --- | --- | --- | --- |
| `nodes` root | src/node_runner.py:46-55 | non-mapping `nodes` is treated as `{}`; `primary` node is always present as `NodeConfig(id="primary")` | missing or malformed node config still leaves local primary execution available | remote-placement compatibility contract |
| `nodes.default` | src/node_runner.py:40-44, src/node_runner.py:74-75, src/session_manager.py:822-832 | value is stringified/stripped by `_clean_optional`; empty or missing means `primary`; if configured default is not in registry, default falls back to `primary` | top-level session placement when no explicit node and no parent node are provided | remote-placement compatibility contract |
| `nodes.registry.<id>` | src/node_runner.py:51-61 | registry must be a mapping; each raw id is stringified/stripped; empty ids are skipped; non-mapping node values become empty node configs | names available remote execution nodes and drives `/nodes` output and create-session validation | security-sensitive remote-placement contract |
| `nodes.registry.<id>.ssh` | src/node_runner.py:61-64, src/node_runner.py:127-146 | optional stringified/stripped value; remote command execution for non-primary nodes raises `ValueError` when missing | SSH destination for remote command/session execution | command-execution trust-boundary contract |
| `nodes.registry.<id>.ssh_proxy_command` | src/node_runner.py:64, src/node_runner.py:330-343 | optional stringified/stripped value; passed to OpenSSH as `-o ProxyCommand=<value>` | cloudflared/proxy routing for remote nodes | security-sensitive remote-connectivity contract |
| `nodes.registry.<id>.control_path` | src/node_runner.py:65, src/node_runner.py:330-343, src/node_runner.py:358-362 | optional stringified/stripped value; `~` expanded with `os.path.expanduser`; passed to OpenSSH as `-S <path>` | SSH control socket reuse and path compatibility | remote-connectivity path contract |
| OpenSSH defaults for remote nodes | src/node_runner.py:330-343 | all remote commands include `ControlMaster=auto`, `ControlPersist=600`, and `ConnectTimeout=5`; `-tt` is added for remote attach commands | preserves SSH multiplexing, timeout, and TTY behavior for remote sessions | remote-execution compatibility contract |
| `nodes.registry.<id>.api_url` | src/node_runner.py:66, src/session_manager.py:862-876 | optional stringified/stripped value; exported to remote session runtime env as `SM_API_URL` | remote managed agents call the intended SM API endpoint | remote agent runtime contract |
| `nodes.registry.<id>.hook_base_url` | src/node_runner.py:67, src/session_manager.py:862-876 | optional stringified/stripped value; exported as `SM_HOOK_BASE_URL` after trailing slash removal | remote hooks post to the intended SM hook base URL | remote hook delivery contract |
| `nodes.registry.<id>.hook_secret` | src/node_runner.py:68, src/session_manager.py:767-776, src/session_manager.py:862-876, src/server.py:1477-1514 | optional stringified/stripped value; exported as `SM_HOOK_SECRET`; checked against `x-sm-hook-secret` for remote hook payloads; also used as node-agent secret fallback when `node_token` is absent | remote hook spoofing defense and remote agent bootstrap secret | credential-bearing trust-boundary contract |
| `nodes.registry.<id>.node_token` | src/node_runner.py:69, src/session_manager.py:767-776 | optional stringified/stripped value; preferred over `hook_secret` as node-agent WebSocket auth secret | node-agent WebSocket authentication for remote codex-fork control | credential-bearing node-agent contract |
| `nodes.registry.<id>.projects_root` | src/node_runner.py:70, src/node_runner.py:94-105 | optional stringified/stripped value; not `~` expanded by `NodeRegistry`; exposed in `/nodes` list | remote project root metadata used by operator/client placement flows | remote-placement metadata contract |
| `nodes.registry.<id>.log_dir` | src/node_runner.py:71, src/node_runner.py:94-105 | optional stringified/stripped value; not `~` expanded by `NodeRegistry`; exposed in `/nodes` list | remote log directory metadata for node-agent/session operations | remote-placement metadata contract |
| `/nodes` exposure shape | src/node_runner.py:94-105 | exposes `id`, `primary`, `ssh`, `api_url`, `hook_base_url`, `projects_root`, and `log_dir`; does not expose `hook_secret`, `node_token`, `control_path`, or `ssh_proxy_command` | Rust must preserve public metadata versus secret/private field separation | security-sensitive response contract |
| `nodes.restore_inventory_cache_seconds` | src/session_manager.py:176-178, src/session_manager.py:645 | `float(...)`, default `10.0`; cache comparison uses `max(0.0, value)` | controls remote restore-inventory cache freshness for `/nodes/{id}/restore-candidates` behavior | remote-node restore compatibility contract |

## Verified Source-Defined Non-Example Config Defaults

These config families are read by source but are absent from checked-in example defaults. They remain part of the Stage 2 handoff because later Rust tickets must preserve or consciously reclassify their paths, intervals, and timing behavior.

| key / family | source | default / coercion | outward contract | classification |
| --- | --- | --- | --- | --- |
| `response_relay.db_path` | src/main.py:287-293, src/response_relay.py:74-79 | default `~/.local/share/claude-sessions/response_relay.db`; `ResponseRelayLedger` expands `~`, creates parent directory, opens SQLite WAL DB | durable turn-bound response relay state for delivered input and assistant output | persistence compatibility contract |
| `tool_logging.db_path` | src/main.py:323-326, src/tool_logger.py:72-77 | default `~/.local/share/claude-sessions/tool_usage.db`; `ToolLogger` expands `~`, creates parent directory, and initializes SQLite | security audit/tool telemetry database path | security/audit persistence contract |
| `paths.message_queue_db` | src/mobile_analytics.py:15, src/mobile_analytics.py:106-110, src/server.py:3781-3786 | default `~/.local/share/claude-sessions/message_queue.db`; `Path(...).expanduser()` | `/client/analytics/summary` reads send/remind registration metrics from the message queue DB for Android/mobile analytics | first-class mobile analytics persistence contract |
| `paths.server_log_file` | src/mobile_analytics.py:16, src/mobile_analytics.py:106-110, src/server.py:3781-3786 | default `/tmp/session-manager.log`; `Path(...).expanduser()` | `/client/analytics/summary` reads retained spawn/self-heal timing signals from server logs | first-class mobile analytics log-input contract |
| `watchdog.check_interval` / `watchdog.timeout` | src/main.py:1043-1049 | defaults `30` and `10`; no explicit coercion beyond downstream watchdog use | event-loop watchdog cadence and stall detection threshold | internal runtime health contract |
| `service_role_maintenance.poll_interval_seconds` | src/session_manager.py:267-270, src/session_manager.py:5828-5836 | `float(...)`, default `60.0` | cadence for durable service-role maintenance/autobootstrap loop | service-role lifecycle contract |
| `codex.session_index_path` | src/session_manager.py:297-299, src/session_manager.py:2303-2399 | default `~/.codex/session_index.jsonl`; `Path(...).expanduser()` | Codex native-title sync reads the provider session index to keep SM display identity aligned with Codex threads | Codex display identity contract |
| `codex_fork.event_poll_interval_seconds` / `control_timeout_seconds` / `fork_timeout_seconds` | src/session_manager.py:323-330 | `float(...)` defaults `0.5`, `5.0`, and `30.0` | event polling, control command, and fork operation timing for codex-fork IPC | codex-fork IPC timing contract |
| `codex_fork.control_tmux_fallback_enabled` | src/session_manager.py:332-335 | `_coerce_rollout_flag(...)`, default `True` | allows tmux fallback when codex-fork structured control is unavailable or degraded | codex-fork compatibility fallback contract |
| `codex_fork.tool_input_max_chars` / `tool_output_preview_max_chars` / `tool_payload_max_items` | src/session_manager.py:336-343 | `int(...)` defaults `2000`, `1200`, and `100`; clamped to at least `200`, `100`, and `10` | bounds tool input/output preview payloads surfaced through codex-fork event/control paths | codex-fork payload compatibility contract |
| `codex_fork_runtime_maintenance.poll_interval_seconds` | src/session_manager.py:358-360, src/session_manager.py:6065-6073 | `float(...)`, default `300.0` | cadence for codex-fork runtime maintenance loop | codex-fork runtime lifecycle contract |
| `codex_app_server.command` / `args` / `app_server_args` / `default_model` | src/session_manager.py:273-275, src/session_manager.py:364-368 | `codex_app_server` section falls back to `codex`; `command` defaults to `codex.command`; `args` use `app_server_args`, then `args`, then `[]`; `default_model` falls back to `codex.default_model` | launch shape for Codex app-server/native app integration | Codex app-server launch contract |
| `codex_app_server.approval_policy` / `sandbox` / `approval_decision` / `request_timeout_seconds` | src/session_manager.py:364-372 | defaults `never`, `workspace-write`, `decline`, and `60` | approval/sandbox/request-timeout metadata used by Codex app-server sessions | Codex app-server security/runtime contract |
| `codex_app_server.client_name` / `client_title` / `client_version` | src/session_manager.py:373-375 | defaults `session-manager`, `Claude Session Manager`, and `0.1.0` | client metadata advertised to Codex app-server/native app flows | Codex app-server client metadata contract |
| `infra_supervisor.enabled` / `check_interval_seconds` | src/infra_supervisor.py:25-28, src/infra_supervisor.py:83-91 | `enabled` uses Python truthiness with default `True`; `check_interval_seconds` is `int(...)`, default `30`, clamped to at least `10` | controls background repair loop for local sidecar infrastructure used by Android/mobile attach | first-class mobile infrastructure contract |
| `infra_supervisor.android_sshd.*` | src/infra_supervisor.py:36-50 | label default `com.rajesh.sm-android-sshd`; plist default `~/Library/LaunchAgents/com.rajesh.sm-android-sshd.plist`; config default `~/.local/share/session-manager/android-sshd/sshd_config`; paths expand `~` | launchd/sshd repair metadata for Android SSH attach support | mobile attach infrastructure contract |
| `infra_supervisor.android_tunnel.*` | src/infra_supervisor.py:51-67 | label default `com.rajesh.sm-android-tunnel`; plist default `~/Library/LaunchAgents/com.rajesh.sm-android-tunnel.plist`; paths expand `~`; `public_probe_timeout_seconds` is `int(...)`, default `10`, clamped to at least `3`, invalid values fall back to `10` | cloudflared/public tunnel repair and diagnostics for Android/mobile attach | mobile/public-host infrastructure contract |
| `infra_supervisor.ac_caffeinate.*` | src/infra_supervisor.py:69-77 | label default `com.rajesh.sm-ac-caffeinate`; plist default `~/Library/LaunchAgents/com.rajesh.sm-ac-caffeinate.plist`; path expands `~` | local keep-awake sidecar repair for long-running mobile/remote sessions | operator/mobile infrastructure contract |
| `infra_supervisor.tmux.base_session` | src/infra_supervisor.py:79-81 | stringified and stripped; default `base`; empty string falls back to `base` | tmux base-session repair target exposed in infrastructure health/repair behavior | tmux infrastructure compatibility contract |
| `timeouts.output_monitor.cleanup_notify_timeout_seconds` | src/output_monitor.py:112-118, src/output_monitor.py:847-934 | default `2`; no explicit coercion in `OutputMonitor` | timeout for cleanup/notification callbacks during output-monitor events | managed-agent notification timing contract |
| `timeouts.output_monitor.native_title_refresh_interval_seconds` | src/output_monitor.py:112-118, src/output_monitor.py:253-256 | default `5`; no explicit coercion in `OutputMonitor` | background native title refresh throttling | display/identity compatibility contract |
| `timeouts.tmux.send_keys_settle_max_seconds` | src/tmux_controller.py:50-65, src/tmux_controller.py:607-621 | default `0.9`; used through `float(...)` at delay calculation | upper bound for adaptive post-send settle delay | tmux input timing contract |
| `timeouts.tmux.send_keys_settle_per_ki_chars` | src/tmux_controller.py:50-65, src/tmux_controller.py:607-621 | default `0.06`; used through `float(...)` at delay calculation | adaptive delay per KiB of input beyond base threshold | tmux input timing contract |
| `timeouts.tmux.send_keys_settle_per_extra_line` | src/tmux_controller.py:50-65, src/tmux_controller.py:607-621 | default `0.015`; used through `float(...)` at delay calculation | adaptive delay per extra line of input | tmux input timing contract |
| `timeouts.tmux.send_keys_max_chunk_chars` | src/tmux_controller.py:50-65, src/tmux_controller.py:621 | `int(...)`, default `4096`, clamped at use to at least `1` | max text chunk size for tmux input delivery | tmux input compatibility contract |
| `timeouts.tmux.submit_verify_seconds` | src/tmux_controller.py:50-65, src/tmux_controller.py:970-1018 | default `0.6`; used as async sleep around submit verification/retry | submit/Enter verification timing | tmux submit behavior contract |
| `timeouts.tmux.submit_retry_seconds` | src/tmux_controller.py:50-65, src/tmux_controller.py:970-1018 | default `0.6`; used as async sleep around submit retry | submit retry timing | tmux submit behavior contract |
| `timeouts.tmux.shell_fd_limit` | src/tmux_controller.py:50-65, src/tmux_controller.py:707-708, src/session_manager.py:8199-8205 | `int(...)`, default `65536`; only applies when positive | file-descriptor limit exported into managed shells | managed shell runtime contract |

## Verified CLI/Client Config And Environment Contracts

The rows below are source-verified client-side contracts from `src/cli/client.py` and `config/client.yaml.example`. They are outward-facing because they decide which server the `sm` CLI talks to, where top-level sessions run, whether local attach uses tmux or SSH, and how long mutating operations wait.

| key / env / behavior | source | default / coercion | precedence | outward contract | classification |
| --- | --- | --- | --- | --- | --- |
| client config path | src/cli/client.py:44-57 | `SM_CLIENT_CONFIG` path if set; otherwise `$XDG_CONFIG_HOME/session-manager/client.yaml`; otherwise `~/.config/session-manager/client.yaml`; `~` expanded | `SM_CLIENT_CONFIG` > `XDG_CONFIG_HOME` > home config path | shared client config discovery for CLI and GUI clients | client config contract |
| client config payload shape | src/cli/client.py:70-83 | missing file is ignored; invalid YAML or non-mapping raises `ClientConfigError` | applies before reading `api_url`, `default_node`, or `local_node` | invalid present client config fails safely instead of silently using partial values | client config validation contract |
| API URL resolution | src/cli/client.py:60-67, src/cli/client.py:86-104, src/cli/client.py:166-185, config/client.yaml.example:1-8 | strips whitespace and trailing slash; only `http://` or `https://` accepted; fallback `http://127.0.0.1:8420` | explicit client arg > `SM_API_URL` > `api_url` > `client.api_url` > localhost default | base API target for all CLI HTTP requests; non-http(s) explicit/env/config values raise `ClientConfigError` | client config + env override contract |
| default execution node | src/cli/client.py:110-139, src/cli/client.py:188-198, config/client.yaml.example:10-12 | non-empty string after stripping; invalid configured empty value raises `ClientConfigError`; unset is `None` | explicit CLI node/default-node arg > `SM_DEFAULT_NODE` > `default_node` > `client.default_node` | top-level `sm claude`/`sm codex` placement default; managed sessions still inherit parent node | remote placement contract |
| local node identity | src/cli/client.py:110-115, src/cli/client.py:142-163, src/cli/client.py:201-211, config/client.yaml.example:14-16 | non-empty string after stripping; invalid configured empty value raises `ClientConfigError`; unset is `None` | explicit local-node arg > `SM_LOCAL_NODE` > `local_node` > `client.local_node` | tells the CLI when a remote/default node is actually local so attach can use local tmux instead of SSH | attach/routing contract |
| `SM_API_TIMEOUT` | src/cli/client.py:19, src/cli/client.py:29-41 | float seconds; default `5.0`; invalid or non-positive values fall back to `5.0` | env only | default timeout for generic CLI API requests | CLI timeout contract |
| `SM_SEND_API_TIMEOUT` | src/cli/client.py:20, src/cli/client.py:214-227 | float seconds; valid positive env wins; invalid/non-positive ignored; default `max(API_TIMEOUT, 15.0)` | env with `SM_API_TIMEOUT` fallback interaction | dedicated timeout for `sm send` resolution/delivery requests | CLI timeout contract |
| `SM_MUTATION_API_TIMEOUT` | src/cli/client.py:21, src/cli/client.py:230-243 | float seconds; valid positive env wins; invalid/non-positive ignored; default `max(_read_api_timeout(), 15.0)` | env with `SM_API_TIMEOUT` fallback interaction | timeout for mutation-style session requests | CLI timeout contract |
| kill timeout constant | src/cli/client.py:22 | `30` seconds | code default | kill/cleanup requests wait longer because cleanup may involve network I/O | CLI timeout contract |
| resolved client fields | src/cli/client.py:246-259 | `api_url`, `default_node`, `local_node`, and `session_id` resolved at `SessionManagerClient` construction; current Python reads legacy `CLAUDE_SESSION_MANAGER_ID`, while Rust should prefer canonical `SESSION_MANAGER_ID` and accept the legacy alias during migration | resolution rules above plus inherited environment | observable behavior for CLI commands that need self/agent-scoped identity, default placement, and local attach decisions | CLI runtime contract |

## Exact Example Defaults And Classifications

This table is the Stage 2 handoff for checked-in example defaults. The raw key-path table below remains useful for completeness checks, but implementation tickets should use this table for default values and compatibility classifications.

| file / key family | source | exact example defaults / value summary | compatibility classification |
| --- | --- | --- | --- |
| `config.yaml` `server` | config.yaml.example:4-6 | `host: "127.0.0.1"`, `port: 8420` | operator/server bind contract |
| `config.yaml` `paths` | config.yaml.example:8-10 | `log_dir: "/tmp/claude-sessions"`, `state_file: "/tmp/claude-sessions/sessions.json"` | state/log path compatibility contract |
| `config.yaml` `monitor` and `worktree_cleanup` | config.yaml.example:12-27 | `idle_timeout: 300`, `poll_interval: 1.0`, notify `errors: false`, `permission_prompts: true`, `completion: false`, `idle: true`, `worktree_cleanup.notify_dirty: true` | managed-session monitoring and operator notification contract |
| `config.yaml` `tmux` | config.yaml.example:29-43 | `socket_name: "session-manager"`, `native_scrollback: true`, `history_limit: 100000` | terminal/tmux attach and scrollback compatibility contract |
| `config.yaml` `timeouts.tmux` | config.yaml.example:45-67 | `shell_export_settle_seconds: 0.1`, `claude_init_seconds: 3`, `claude_init_no_prompt_seconds: 1`, `send_keys_timeout_seconds: 5`, `send_keys_settle_seconds: 0.3` | provider launch and tmux input timing contract |
| `config.yaml` `timeouts.output_monitor` | config.yaml.example:69-78 | `idle_cooldown_seconds: 300`, `permission_debounce_seconds: 30` | monitor/notification timing contract |
| `config.yaml` `timeouts.message_queue` | config.yaml.example:80-108 | `subprocess_timeout_seconds: 2`, `async_send_timeout_seconds: 5`, `input_delivery_wait_seconds: 1.0`, `initial_retry_delay_seconds: 1.0`, `max_retry_delay_seconds: 30`, `watch_poll_interval_seconds: 2`, `skip_fence_window_seconds: 8` | durable message delivery and watch timing contract |
| `config.yaml` `timeouts.server` | config.yaml.example:110-126 | `slow_request_threshold_seconds: 1.0`, `request_timing_threshold_seconds: 0.1`, `hook_timing_threshold_seconds: 0.05`, `summary_generation_timeout_seconds: 60` | HTTP timing-log, hook watchdog, and summary timeout contract |
| `config.yaml` `telegram` | config.yaml.example:128-142 | `token: "YOUR_BOT_TOKEN_HERE"`, `allowed_chat_ids: []`, `topic_registry.path: "~/.local/share/claude-sessions/telegram_topics.json"`, `topic_cleanup.enabled: false`, `topic_cleanup.interval_seconds: 900` | Telegram external-service and durable topic registry contract |
| `config.yaml` `email` and `services` | config.yaml.example:144-154 | `smtp_config: ""`, `imap_config: ""`, `bridge_config: "config/email_send.yaml"`, `office_automate_url: "http://192.168.5.140:8080"` | external-service config contract |
| `config.yaml` `external_access` | config.yaml.example:156-164 | `public_http_host: "sm.example.com"`, `public_http_path_prefix: ""`, `public_ssh_host: "ssh.sm.example.com"`, `http_origin_url: "http://127.0.0.1:8420"`, `ssh_username: "your-ssh-user"`, `ssh_proxy_command: "cloudflared access ssh --hostname %h"` | public/mobile/Termux attach and reverse-proxy contract |
| `config.yaml` `mobile_terminal` | config.yaml.example:166-191 | `enabled: false`; example user `you` has `email: "you@example.com"`, `interactive_shell_access: true`, `mobile_terminal_owner: true`, one enabled device key placeholder; `ticket_ttl_seconds: 30`, `auth_frame_timeout_seconds: 3`, `max_attach_seconds: 3600`, `max_concurrent_attaches_per_user: 1`, `max_concurrent_attaches_per_session: 1`, `max_concurrent_attaches_global: 4`, `require_tls: true` | first-class mobile terminal and security-sensitive shell access contract |
| `config.yaml` `auth.google` | config.yaml.example:193-202 | `enabled: false`, `public_host: "sm.example.com"`, `client_id: "YOUR_GOOGLE_WEB_CLIENT_ID"`, `client_secret: "YOUR_GOOGLE_WEB_CLIENT_SECRET"`, `redirect_uri: "https://sm.example.com/auth/google/callback"`, `allowlist_emails: ["you@example.com"]`, `session_cookie_secret: "SET_IN_LOCAL_ENV_OR_OVERRIDE"` | public/browser/device auth contract; local-env overlay may override and enable |
| `config.yaml` `claude` | config.yaml.example:204-212 | `command: "claude"`, `args: ["--bypass-permissions"]`, `default_model: "sonnet"` | provider launch contract |
| `config.yaml` `codex` | config.yaml.example:214-238 | `command: "codex"`, `args: ["--dangerously-bypass-approvals-and-sandbox"]`, `app_server_args: []`, `default_model: null`, `approval_policy: "never"`, `approval_decision: "decline"`, `sandbox: "workspace-write"`, `request_timeout_seconds: 60`, review defaults `default_wait: 600`, `menu_settle_seconds: 1.0`, `branch_settle_seconds: 1.0`, `steer_delay_seconds: 5.0` | provider launch, approval, sandbox, and review workflow contract |
| `config.yaml` `codex_rollout` | config.yaml.example:240-247 | `enable_durable_events: true`, `enable_structured_requests: true`, `enable_observability_projection: true`, `enable_codex_tui: true`, `provider_mapping_phase: "pre_cutover"` | rollout-gate and compatibility-window contract |
| `config.yaml` `codex_fork` | config.yaml.example:249-266 | `artifact_release: "v0.1.0-sm"`, `artifact_ref: "8f00aa11b22cc33dd44ee55ff66778899aabbccd"`, platforms `darwin-arm64`, `darwin-x86_64`, `linux-x86_64`, `rollback_provider: "codex"`, `rollback_command: "sm codex-legacy"`, `event_schema_version: 2` | codex-fork runtime pinning and rollback contract |
| `config.yaml` `service_roles` | config.yaml.example:268-289 | `maintainer.auto_bootstrap: true`, `working_dir: "/path/to/session-manager"`, `friendly_name: "maintainer"`, preferred providers `codex-fork`, `codex`, `claude`, `bootstrap_prompt_file: "docs/product/maintainer_bootstrap.md"`, `task_complete_ttl_seconds: 600`; `chief-scientist.auto_bootstrap: true`, preferred providers `codex-fork`, `claude`, `working_dir: "/path/to/research-lane"`, `friendly_name: "chief-scientist"`, `bootstrap_prompt_file: "~/.sm/boot_docs/chief-scientist.md"` | durable service-role bootstrap contract |
| `config.yaml` `codex_events` | config.yaml.example:291-299 | `db_path: "~/.local/share/claude-sessions/codex_events.db"`, `ring_size: 1000`, `retention_max_events_per_session: 5000`, `retention_max_age_days: 14`, `prune_every_writes: 200`, `payload_preview_chars: 1500`, `working_delta_window_seconds: 2.5` | durable event schema/retention and activity-state contract |
| `config.yaml` `codex_requests` | config.yaml.example:301-303 | `db_path: "~/.local/share/claude-sessions/codex_requests.db"` | structured request ledger persistence contract |
| `config.yaml` `codex_observability` | config.yaml.example:305-312 | `db_path: "~/.local/share/claude-sessions/codex_observability.db"`, `retention_max_age_days: 14`, `retention_tool_events_per_session: 20000`, `retention_turn_events_per_session: 5000`, `payload_max_chars: 4000`, `prune_interval_seconds: 3600` | observability DB/retention contract |
| `config.yaml` `child_agents` | config.yaml.example:314-342 | `auto_complete.enabled: true`, `idle_timeout: 600`, `detect_completion_phrases: true`, completion patterns `complete`, `done`, `finished`, `all tests pass`; cleanup `auto_kill_on_complete: false`, `auto_archive_transcript: true`, `archive_path: "/tmp/claude-sessions/archives"`; notifications complete/error/idle all `true`; progress token/tool tracking `true`, `snapshot_interval: 30` | parent-child lifecycle, cleanup, and progress compatibility contract |
| `config.yaml` `remind`, `dispatch`, and `sm_send` | config.yaml.example:344-370 | `remind.soft_threshold_seconds: 180`, `remind.hard_gap_seconds: 120`; `dispatch.auto_remind.soft_threshold_seconds: 210`, `hard_threshold_seconds: 420`; `sm_send.db_path: "~/.local/share/claude-sessions/message_queue.db"`, `input_poll_interval: 5`, `input_stale_timeout: 120`, `max_batch_size: 10`, `urgent_delay_ms: 500` | inter-agent messaging/reminder timing and persistence contract |
| `config.yaml` `queue_runner` | config.yaml.example:372-394 | `enabled: true`, `state_dir: "~/.local/share/claude-sessions/queue-runner"`, `max_running_jobs: 2`, `perf_cooldown_seconds: 30`, `cancel_grace_seconds: 10`, memory `min_free_bytes: 2147483648`, `retry_interval_seconds: 10`, resource sampling `enabled: true`, `interval_seconds: 15`, type defaults `tests 2/900`, `perf 1/2700`, `background 2/3600` | command-execution queue/resource-throttling contract |
| `config.yaml` `sessions` | config.yaml.example:396-404 | `default_working_dir_behavior: "inherit"`, `inherit_environment_vars: ["SSH_AUTH_SOCK", "PATH"]`, naming `pattern: "{friendly_name}"`, `fallback: "child-{short_id}"` | session creation environment and naming contract |
| `config/client.yaml.example` | config/client.yaml.example:1-16 | `api_url: "http://127.0.0.1:8420"`; `default_node` and `local_node` are optional commented examples `"macbook"` with no active default | CLI/API target and node-routing contract; exact precedence in verified CLI/client section |
| `config/email_send.yaml.example` `resend` | config/email_send.yaml.example:1-7 | `api_key: "re_PLACEHOLDER"`, `domain: "sm.example.com"`, `reply_address: "reply@sm.example.com"`; optional commented `reply_domain: "example.com"` | outbound email provider and routed-reply address contract |
| `config/email_send.yaml.example` `humans` and `users` | config/email_send.yaml.example:9-31 | `humans.operator.display_name: "Human operator"`, aliases `user`, `owner`, `default_channel: "telegram"`, Telegram `enabled: true`, `delivery: "sender_session_topic"`, email `enabled: true`, `address_env: "SM_OPERATOR_EMAIL"`, `use: "fallback_only"`; `users.operator.email: "operator@example.com"`, `name: "Human operator"`, aliases `owner` | human-recipient registry and external delivery contract |
| `config/email_send.yaml.example` `email_bridge` | config/email_send.yaml.example:32-40 | `authorized_senders: ["operator@example.com"]`, `webhook_path: "/api/email-inbound"`; optional commented `worker_secret`, `worker_secret_header: "x-email-worker-secret"`, `session_id_header: "x-email-session-id"` | security-sensitive inbound email webhook and trusted-header contract |

## Raw Example Key Paths (supporting inventory)

| key path | file | line | source |
| --- | --- | --- | --- |
| server | config.yaml.example | 4 | example/default |
| server.host | config.yaml.example | 5 | example/default |
| server.port | config.yaml.example | 6 | example/default |
| paths | config.yaml.example | 8 | example/default |
| paths.log_dir | config.yaml.example | 9 | example/default |
| paths.state_file | config.yaml.example | 10 | example/default |
| monitor | config.yaml.example | 12 | example/default |
| monitor.idle_timeout | config.yaml.example | 14 | example/default |
| monitor.poll_interval | config.yaml.example | 16 | example/default |
| monitor.notify | config.yaml.example | 18 | example/default |
| monitor.notify.errors | config.yaml.example | 19 | example/default |
| monitor.notify.permission_prompts | config.yaml.example | 20 | example/default |
| monitor.notify.completion | config.yaml.example | 21 | example/default |
| monitor.notify.idle | config.yaml.example | 22 | example/default |
| worktree_cleanup | config.yaml.example | 25 | example/default |
| worktree_cleanup.notify_dirty | config.yaml.example | 27 | example/default |
| tmux | config.yaml.example | 30 | example/default |
| tmux.socket_name | config.yaml.example | 34 | example/default |
| tmux.native_scrollback | config.yaml.example | 39 | example/default |
| tmux.history_limit | config.yaml.example | 43 | example/default |
| timeouts | config.yaml.example | 46 | example/default |
| timeouts.tmux | config.yaml.example | 48 | example/default |
| timeouts.tmux.shell_export_settle_seconds | config.yaml.example | 51 | example/default |
| timeouts.tmux.claude_init_seconds | config.yaml.example | 55 | example/default |
| timeouts.tmux.claude_init_no_prompt_seconds | config.yaml.example | 59 | example/default |
| timeouts.tmux.send_keys_timeout_seconds | config.yaml.example | 63 | example/default |
| timeouts.tmux.send_keys_settle_seconds | config.yaml.example | 67 | example/default |
| timeouts.output_monitor | config.yaml.example | 70 | example/default |
| timeouts.output_monitor.idle_cooldown_seconds | config.yaml.example | 74 | example/default |
| timeouts.output_monitor.permission_debounce_seconds | config.yaml.example | 78 | example/default |
| timeouts.message_queue | config.yaml.example | 81 | example/default |
| timeouts.message_queue.subprocess_timeout_seconds | config.yaml.example | 84 | example/default |
| timeouts.message_queue.async_send_timeout_seconds | config.yaml.example | 88 | example/default |
| timeouts.message_queue.input_delivery_wait_seconds | config.yaml.example | 91 | example/default |
| timeouts.message_queue.initial_retry_delay_seconds | config.yaml.example | 95 | example/default |
| timeouts.message_queue.max_retry_delay_seconds | config.yaml.example | 98 | example/default |
| timeouts.message_queue.watch_poll_interval_seconds | config.yaml.example | 102 | example/default |
| timeouts.message_queue.skip_fence_window_seconds | config.yaml.example | 108 | example/default |
| timeouts.server | config.yaml.example | 111 | example/default |
| timeouts.server.slow_request_threshold_seconds | config.yaml.example | 114 | example/default |
| timeouts.server.request_timing_threshold_seconds | config.yaml.example | 118 | example/default |
| timeouts.server.hook_timing_threshold_seconds | config.yaml.example | 122 | example/default |
| timeouts.server.summary_generation_timeout_seconds | config.yaml.example | 126 | example/default |
| telegram | config.yaml.example | 128 | example/default |
| telegram.token | config.yaml.example | 130 | example/default |
| telegram.allowed_chat_ids | config.yaml.example | 132 | example/default |
| telegram.topic_registry | config.yaml.example | 136 | example/default |
| telegram.topic_registry.path | config.yaml.example | 139 | example/default |
| telegram.topic_cleanup | config.yaml.example | 140 | example/default |
| telegram.topic_cleanup.enabled | config.yaml.example | 141 | example/default |
| telegram.topic_cleanup.interval_seconds | config.yaml.example | 142 | example/default |
| email | config.yaml.example | 144 | example/default |
| email.smtp_config | config.yaml.example | 147 | example/default |
| email.imap_config | config.yaml.example | 148 | example/default |
| email.bridge_config | config.yaml.example | 150 | example/default |
| services | config.yaml.example | 152 | example/default |
| services.office_automate_url | config.yaml.example | 154 | example/default |
| external_access | config.yaml.example | 158 | example/default |
| external_access.public_http_host | config.yaml.example | 159 | example/default |
| external_access.public_http_path_prefix | config.yaml.example | 160 | example/default |
| external_access.public_ssh_host | config.yaml.example | 161 | example/default |
| external_access.http_origin_url | config.yaml.example | 162 | example/default |
| external_access.ssh_username | config.yaml.example | 163 | example/default |
| external_access.ssh_proxy_command | config.yaml.example | 164 | example/default |
| mobile_terminal | config.yaml.example | 169 | example/default |
| mobile_terminal.enabled | config.yaml.example | 170 | example/default |
| mobile_terminal.allowed_users | config.yaml.example | 171 | example/default |
| mobile_terminal.allowed_users.you | config.yaml.example | 172 | example/default |
| mobile_terminal.allowed_users.you.email | config.yaml.example | 173 | example/default |
| mobile_terminal.allowed_users.you.interactive_shell_access | config.yaml.example | 174 | example/default |
| mobile_terminal.allowed_users.you.mobile_terminal_owner | config.yaml.example | 176 | example/default |
| mobile_terminal.allowed_users.you.registered_device_keys | config.yaml.example | 177 | example/default |
| mobile_terminal.allowed_users.you.registered_device_keys.label | config.yaml.example | 179 | example/default |
| mobile_terminal.allowed_users.you.registered_device_keys.public_key | config.yaml.example | 180 | example/default |
| mobile_terminal.allowed_users.you.registered_device_keys.enabled | config.yaml.example | 184 | example/default |
| mobile_terminal.ticket_ttl_seconds | config.yaml.example | 185 | example/default |
| mobile_terminal.auth_frame_timeout_seconds | config.yaml.example | 186 | example/default |
| mobile_terminal.max_attach_seconds | config.yaml.example | 187 | example/default |
| mobile_terminal.max_concurrent_attaches_per_user | config.yaml.example | 188 | example/default |
| mobile_terminal.max_concurrent_attaches_per_session | config.yaml.example | 189 | example/default |
| mobile_terminal.max_concurrent_attaches_global | config.yaml.example | 190 | example/default |
| mobile_terminal.require_tls | config.yaml.example | 191 | example/default |
| auth | config.yaml.example | 193 | example/default |
| auth.google | config.yaml.example | 194 | example/default |
| auth.google.enabled | config.yaml.example | 195 | example/default |
| auth.google.public_host | config.yaml.example | 196 | example/default |
| auth.google.client_id | config.yaml.example | 197 | example/default |
| auth.google.client_secret | config.yaml.example | 198 | example/default |
| auth.google.redirect_uri | config.yaml.example | 199 | example/default |
| auth.google.allowlist_emails | config.yaml.example | 200 | example/default |
| auth.google.session_cookie_secret | config.yaml.example | 202 | example/default |
| claude | config.yaml.example | 205 | example/default |
| claude.command | config.yaml.example | 207 | example/default |
| claude.args | config.yaml.example | 209 | example/default |
| claude.default_model | config.yaml.example | 212 | example/default |
| codex | config.yaml.example | 215 | example/default |
| codex.command | config.yaml.example | 217 | example/default |
| codex.args | config.yaml.example | 219 | example/default |
| codex.app_server_args | config.yaml.example | 222 | example/default |
| codex.default_model | config.yaml.example | 224 | example/default |
| codex.approval_policy | config.yaml.example | 226 | example/default |
| codex.approval_decision | config.yaml.example | 228 | example/default |
| codex.sandbox | config.yaml.example | 230 | example/default |
| codex.request_timeout_seconds | config.yaml.example | 232 | example/default |
| codex.review | config.yaml.example | 234 | example/default |
| codex.review.default_wait | config.yaml.example | 235 | example/default |
| codex.review.menu_settle_seconds | config.yaml.example | 236 | example/default |
| codex.review.branch_settle_seconds | config.yaml.example | 237 | example/default |
| codex.review.steer_delay_seconds | config.yaml.example | 238 | example/default |
| codex_rollout | config.yaml.example | 241 | example/default |
| codex_rollout.enable_durable_events | config.yaml.example | 242 | example/default |
| codex_rollout.enable_structured_requests | config.yaml.example | 243 | example/default |
| codex_rollout.enable_observability_projection | config.yaml.example | 244 | example/default |
| codex_rollout.enable_codex_tui | config.yaml.example | 245 | example/default |
| codex_rollout.provider_mapping_phase | config.yaml.example | 247 | example/default |
| codex_fork | config.yaml.example | 250 | example/default |
| codex_fork.artifact_release | config.yaml.example | 255 | example/default |
| codex_fork.artifact_ref | config.yaml.example | 256 | example/default |
| codex_fork.artifact_platforms | config.yaml.example | 258 | example/default |
| codex_fork.rollback_provider | config.yaml.example | 263 | example/default |
| codex_fork.rollback_command | config.yaml.example | 264 | example/default |
| codex_fork.event_schema_version | config.yaml.example | 266 | example/default |
| service_roles | config.yaml.example | 271 | example/default |
| service_roles.maintainer | config.yaml.example | 272 | example/default |
| service_roles.maintainer.auto_bootstrap | config.yaml.example | 273 | example/default |
| service_roles.maintainer.working_dir | config.yaml.example | 274 | example/default |
| service_roles.maintainer.friendly_name | config.yaml.example | 275 | example/default |
| service_roles.maintainer.preferred_providers | config.yaml.example | 276 | example/default |
| service_roles.maintainer.bootstrap_prompt_file | config.yaml.example | 280 | example/default |
| service_roles.maintainer.task_complete_ttl_seconds | config.yaml.example | 281 | example/default |
| service_roles.chief-scientist | config.yaml.example | 282 | example/default |
| service_roles.chief-scientist.auto_bootstrap | config.yaml.example | 283 | example/default |
| service_roles.chief-scientist.preferred_providers | config.yaml.example | 284 | example/default |
| service_roles.chief-scientist.working_dir | config.yaml.example | 287 | example/default |
| service_roles.chief-scientist.friendly_name | config.yaml.example | 288 | example/default |
| service_roles.chief-scientist.bootstrap_prompt_file | config.yaml.example | 289 | example/default |
| codex_events | config.yaml.example | 292 | example/default |
| codex_events.db_path | config.yaml.example | 293 | example/default |
| codex_events.ring_size | config.yaml.example | 294 | example/default |
| codex_events.retention_max_events_per_session | config.yaml.example | 295 | example/default |
| codex_events.retention_max_age_days | config.yaml.example | 296 | example/default |
| codex_events.prune_every_writes | config.yaml.example | 297 | example/default |
| codex_events.payload_preview_chars | config.yaml.example | 298 | example/default |
| codex_events.working_delta_window_seconds | config.yaml.example | 299 | example/default |
| codex_requests | config.yaml.example | 302 | example/default |
| codex_requests.db_path | config.yaml.example | 303 | example/default |
| codex_observability | config.yaml.example | 306 | example/default |
| codex_observability.db_path | config.yaml.example | 307 | example/default |
| codex_observability.retention_max_age_days | config.yaml.example | 308 | example/default |
| codex_observability.retention_tool_events_per_session | config.yaml.example | 309 | example/default |
| codex_observability.retention_turn_events_per_session | config.yaml.example | 310 | example/default |
| codex_observability.payload_max_chars | config.yaml.example | 311 | example/default |
| codex_observability.prune_interval_seconds | config.yaml.example | 312 | example/default |
| child_agents | config.yaml.example | 314 | example/default |
| child_agents.auto_complete | config.yaml.example | 316 | example/default |
| child_agents.auto_complete.enabled | config.yaml.example | 317 | example/default |
| child_agents.auto_complete.idle_timeout | config.yaml.example | 318 | example/default |
| child_agents.auto_complete.detect_completion_phrases | config.yaml.example | 319 | example/default |
| child_agents.auto_complete.completion_patterns | config.yaml.example | 320 | example/default |
| child_agents.cleanup | config.yaml.example | 327 | example/default |
| child_agents.cleanup.auto_kill_on_complete | config.yaml.example | 328 | example/default |
| child_agents.cleanup.auto_archive_transcript | config.yaml.example | 329 | example/default |
| child_agents.cleanup.archive_path | config.yaml.example | 330 | example/default |
| child_agents.notifications | config.yaml.example | 333 | example/default |
| child_agents.notifications.notify_parent_on_complete | config.yaml.example | 334 | example/default |
| child_agents.notifications.notify_parent_on_error | config.yaml.example | 335 | example/default |
| child_agents.notifications.notify_parent_on_idle | config.yaml.example | 336 | example/default |
| child_agents.progress | config.yaml.example | 339 | example/default |
| child_agents.progress.enable_token_tracking | config.yaml.example | 340 | example/default |
| child_agents.progress.enable_tool_tracking | config.yaml.example | 341 | example/default |
| child_agents.progress.snapshot_interval | config.yaml.example | 342 | example/default |
| remind | config.yaml.example | 345 | example/default |
| remind.soft_threshold_seconds | config.yaml.example | 347 | example/default |
| remind.hard_gap_seconds | config.yaml.example | 349 | example/default |
| dispatch | config.yaml.example | 352 | example/default |
| dispatch.auto_remind | config.yaml.example | 353 | example/default |
| dispatch.auto_remind.soft_threshold_seconds | config.yaml.example | 355 | example/default |
| dispatch.auto_remind.hard_threshold_seconds | config.yaml.example | 357 | example/default |
| sm_send | config.yaml.example | 360 | example/default |
| sm_send.db_path | config.yaml.example | 362 | example/default |
| sm_send.input_poll_interval | config.yaml.example | 364 | example/default |
| sm_send.input_stale_timeout | config.yaml.example | 366 | example/default |
| sm_send.max_batch_size | config.yaml.example | 368 | example/default |
| sm_send.urgent_delay_ms | config.yaml.example | 370 | example/default |
| queue_runner | config.yaml.example | 373 | example/default |
| queue_runner.enabled | config.yaml.example | 374 | example/default |
| queue_runner.state_dir | config.yaml.example | 375 | example/default |
| queue_runner.max_running_jobs | config.yaml.example | 376 | example/default |
| queue_runner.perf_cooldown_seconds | config.yaml.example | 377 | example/default |
| queue_runner.cancel_grace_seconds | config.yaml.example | 378 | example/default |
| queue_runner.memory | config.yaml.example | 379 | example/default |
| queue_runner.memory.min_free_bytes | config.yaml.example | 380 | example/default |
| queue_runner.memory.retry_interval_seconds | config.yaml.example | 381 | example/default |
| queue_runner.resource_sampling | config.yaml.example | 382 | example/default |
| queue_runner.resource_sampling.enabled | config.yaml.example | 383 | example/default |
| queue_runner.resource_sampling.interval_seconds | config.yaml.example | 384 | example/default |
| queue_runner.types | config.yaml.example | 385 | example/default |
| queue_runner.types.tests | config.yaml.example | 386 | example/default |
| queue_runner.types.tests.max_concurrent | config.yaml.example | 387 | example/default |
| queue_runner.types.tests.default_timeout_seconds | config.yaml.example | 388 | example/default |
| queue_runner.types.perf | config.yaml.example | 389 | example/default |
| queue_runner.types.perf.max_concurrent | config.yaml.example | 390 | example/default |
| queue_runner.types.perf.default_timeout_seconds | config.yaml.example | 391 | example/default |
| queue_runner.types.background | config.yaml.example | 392 | example/default |
| queue_runner.types.background.max_concurrent | config.yaml.example | 393 | example/default |
| queue_runner.types.background.default_timeout_seconds | config.yaml.example | 394 | example/default |
| sessions | config.yaml.example | 397 | example/default |
| sessions.default_working_dir_behavior | config.yaml.example | 398 | example/default |
| sessions.inherit_environment_vars | config.yaml.example | 399 | example/default |
| sessions.naming | config.yaml.example | 402 | example/default |
| sessions.naming.pattern | config.yaml.example | 403 | example/default |
| sessions.naming.fallback | config.yaml.example | 404 | example/default |
| api_url | config/client.yaml.example | 8 | example/default |
| resend | config/email_send.yaml.example | 1 | example/default |
| resend.api_key | config/email_send.yaml.example | 2 | example/default |
| resend.domain | config/email_send.yaml.example | 3 | example/default |
| resend.reply_address | config/email_send.yaml.example | 5 | example/default |
| humans | config/email_send.yaml.example | 9 | example/default |
| humans.operator | config/email_send.yaml.example | 10 | example/default |
| humans.operator.display_name | config/email_send.yaml.example | 11 | example/default |
| humans.operator.aliases | config/email_send.yaml.example | 12 | example/default |
| humans.operator.default_channel | config/email_send.yaml.example | 15 | example/default |
| humans.operator.channels | config/email_send.yaml.example | 16 | example/default |
| humans.operator.channels.telegram | config/email_send.yaml.example | 17 | example/default |
| humans.operator.channels.telegram.enabled | config/email_send.yaml.example | 18 | example/default |
| humans.operator.channels.telegram.delivery | config/email_send.yaml.example | 19 | example/default |
| humans.operator.channels.email | config/email_send.yaml.example | 20 | example/default |
| humans.operator.channels.email.enabled | config/email_send.yaml.example | 21 | example/default |
| humans.operator.channels.email.address_env | config/email_send.yaml.example | 22 | example/default |
| humans.operator.channels.email.use | config/email_send.yaml.example | 23 | example/default |
| users | config/email_send.yaml.example | 25 | example/default |
| users.operator | config/email_send.yaml.example | 26 | example/default |
| users.operator.email | config/email_send.yaml.example | 27 | example/default |
| users.operator.name | config/email_send.yaml.example | 28 | example/default |
| users.operator.aliases | config/email_send.yaml.example | 29 | example/default |
| email_bridge | config/email_send.yaml.example | 32 | example/default |
| email_bridge.authorized_senders | config/email_send.yaml.example | 33 | example/default |
| email_bridge.webhook_path | config/email_send.yaml.example | 40 | example/default |

## Raw code-read `.get()` keys (overinclusive; includes payload fields)

| key | file | line | source |
| --- | --- | --- | --- |
| PUBLIC_HTTP_HOST | src/main.py | 137 | code-read |
| PUBLIC_SSH_HOST | src/main.py | 138 | code-read |
| HTTP_ORIGIN_URL | src/main.py | 139 | code-read |
| SSH_USERNAME | src/main.py | 140 | code-read |
| SSH_PROXY_COMMAND | src/main.py | 141 | code-read |
| GOOGLE_WEB_CLIENT_ID | src/main.py | 142 | code-read |
| GOOGLE_WEB_CLIENT_SECRET | src/main.py | 143 | code-read |
| GOOGLE_ANDROID_CLIENT_ID | src/main.py | 144 | code-read |
| ALLOWLIST_EMAIL | src/main.py | 147 | code-read |
| SESSION_COOKIE_SECRET | src/main.py | 150 | code-read |
| server | src/main.py | 219 | code-read |
| host | src/main.py | 219 | code-read |
| server | src/main.py | 220 | code-read |
| port | src/main.py | 220 | code-read |
| paths | src/main.py | 223 | code-read |
| log_dir | src/main.py | 223 | code-read |
| paths | src/main.py | 224 | code-read |
| state_file | src/main.py | 224 | code-read |
| monitor | src/main.py | 236 | code-read |
| notify | src/main.py | 237 | code-read |
| idle_timeout | src/main.py | 239 | code-read |
| poll_interval | src/main.py | 240 | code-read |
| errors | src/main.py | 241 | code-read |
| permission_prompts | src/main.py | 242 | code-read |
| completion | src/main.py | 243 | code-read |
| idle | src/main.py | 244 | code-read |
| email | src/main.py | 249 | code-read |
| smtp_config | src/main.py | 251 | code-read |
| imap_config | src/main.py | 252 | code-read |
| bridge_config | src/main.py | 253 | code-read |
| telegram | src/main.py | 257 | code-read |
| token | src/main.py | 260 | code-read |
| services | src/main.py | 261 | code-read |
| allowed_chat_ids | src/main.py | 264 | code-read |
| allowed_user_ids | src/main.py | 265 | code-read |
| office_automate_url | src/main.py | 266 | code-read |
| topic_cleanup | src/main.py | 270 | code-read |
| enabled | src/main.py | 271 | code-read |
| interval_seconds | src/main.py | 273 | code-read |
| response_relay | src/main.py | 287 | code-read |
| sm_send | src/main.py | 312 | code-read |
| db_path | src/main.py | 315 | code-read |
| tool_logging | src/main.py | 324 | code-read |
| db_path | src/main.py | 325 | code-read |
| subagents | src/main.py | 519 | code-read |
| chat_id | src/main.py | 640 | code-read |
| thread_id | src/main.py | 641 | code-read |
| watchdog | src/main.py | 1044 | code-read |
| check_interval | src/main.py | 1047 | code-read |
| timeout | src/main.py | 1048 | code-read |
| timeouts | src/session_manager.py | 167 | code-read |
| message_queue | src/session_manager.py | 167 | code-read |
| input_delivery_wait_seconds | src/session_manager.py | 169 | code-read |
| nodes | src/session_manager.py | 177 | code-read |
| restore_inventory_cache_seconds | src/session_manager.py | 177 | code-read |
| codex_events | src/session_manager.py | 202 | code-read |
| working_delta_window_seconds | src/session_manager.py | 202 | code-read |
| telegram | src/session_manager.py | 209 | code-read |
| default_forum_chat_id | src/session_manager.py | 210 | code-read |
| topic_registry | src/session_manager.py | 211 | code-read |
| path | src/session_manager.py | 213 | code-read |
| maintainer_agent | src/session_manager.py | 227 | code-read |
| service_roles | src/session_manager.py | 228 | code-read |
| working_dir | src/session_manager.py | 230 | code-read |
| friendly_name | src/session_manager.py | 234 | code-read |
| preferred_providers | src/session_manager.py | 236 | code-read |
| bootstrap_prompt_file | src/session_manager.py | 248 | code-read |
| boot_prompt_file | src/session_manager.py | 248 | code-read |
| task_complete_ttl_seconds | src/session_manager.py | 250 | code-read |
| service_role_maintenance | src/session_manager.py | 267 | code-read |
| poll_interval_seconds | src/session_manager.py | 269 | code-read |
| codex | src/session_manager.py | 273 | code-read |
| codex_app_server | src/session_manager.py | 274 | code-read |
| codex_rollout | src/session_manager.py | 275 | code-read |
| enable_durable_events | src/session_manager.py | 278 | code-read |
| enable_structured_requests | src/session_manager.py | 281 | code-read |
| enable_observability_projection | src/session_manager.py | 284 | code-read |
| enable_codex_tui | src/session_manager.py | 287 | code-read |
| provider_mapping_phase | src/session_manager.py | 291 | code-read |
| command | src/session_manager.py | 294 | code-read |
| args | src/session_manager.py | 295 | code-read |
| default_model | src/session_manager.py | 296 | code-read |
| session_index_path | src/session_manager.py | 298 | code-read |
| codex_fork | src/session_manager.py | 300 | code-read |
| command | src/session_manager.py | 301 | code-read |
| args | src/session_manager.py | 303 | code-read |
| default_model | src/session_manager.py | 305 | code-read |
| event_schema_version | src/session_manager.py | 306 | code-read |
| artifact_ref | src/session_manager.py | 307 | code-read |
| artifact_release | src/session_manager.py | 312 | code-read |
| rollback_provider | src/session_manager.py | 321 | code-read |
| rollback_command | src/session_manager.py | 322 | code-read |
| event_poll_interval_seconds | src/session_manager.py | 324 | code-read |
| control_timeout_seconds | src/session_manager.py | 327 | code-read |
| fork_timeout_seconds | src/session_manager.py | 330 | code-read |
| control_tmux_fallback_enabled | src/session_manager.py | 333 | code-read |
| tool_input_max_chars | src/session_manager.py | 337 | code-read |
| tool_output_preview_max_chars | src/session_manager.py | 340 | code-read |
| tool_payload_max_items | src/session_manager.py | 343 | code-read |
| codex_fork_runtime_maintenance | src/session_manager.py | 358 | code-read |
| poll_interval_seconds | src/session_manager.py | 360 | code-read |
| command | src/session_manager.py | 366 | code-read |
| app_server_args | src/session_manager.py | 367 | code-read |
| args | src/session_manager.py | 367 | code-read |
| default_model | src/session_manager.py | 368 | code-read |
| approval_policy | src/session_manager.py | 369 | code-read |
| sandbox | src/session_manager.py | 370 | code-read |
| approval_decision | src/session_manager.py | 371 | code-read |
| request_timeout_seconds | src/session_manager.py | 372 | code-read |
| client_name | src/session_manager.py | 373 | code-read |
| client_title | src/session_manager.py | 374 | code-read |
| client_version | src/session_manager.py | 375 | code-read |
| codex_events | src/session_manager.py | 378 | code-read |
| db_path | src/session_manager.py | 381 | code-read |
| ring_size | src/session_manager.py | 382 | code-read |
| retention_max_events_per_session | src/session_manager.py | 383 | code-read |
| retention_max_age_days | src/session_manager.py | 384 | code-read |
| prune_every_writes | src/session_manager.py | 385 | code-read |
| payload_preview_chars | src/session_manager.py | 386 | code-read |
| codex_requests | src/session_manager.py | 391 | code-read |
| db_path | src/session_manager.py | 391 | code-read |
| codex_observability | src/session_manager.py | 394 | code-read |
| retention_max_age_days | src/session_manager.py | 395 | code-read |
| db_path | src/session_manager.py | 408 | code-read |
| payload_max_chars | src/session_manager.py | 417 | code-read |
| prune_interval_seconds | src/session_manager.py | 418 | code-read |
| id | src/session_manager.py | 607 | code-read |
| sessions | src/session_manager.py | 665 | code-read |
| source_session_id | src/session_manager.py | 689 | code-read |
| id | src/session_manager.py | 689 | code-read |
| status | src/session_manager.py | 690 | code-read |
| id | src/session_manager.py | 736 | code-read |
| id | src/session_manager.py | 741 | code-read |
| topics | src/session_manager.py | 927 | code-read |
| sessions | src/session_manager.py | 1224 | code-read |
| provider | src/session_manager.py | 1225 | code-read |
| tmux_session | src/session_manager.py | 1226 | code-read |
| log_file | src/session_manager.py | 1227 | code-read |
| codex_thread_id | src/session_manager.py | 1228 | code-read |
| name | src/session_manager.py | 1238 | code-read |
| id | src/session_manager.py | 1238 | code-read |
| em_topic | src/session_manager.py | 1366 | code-read |
| maintainer_session_id | src/session_manager.py | 1367 | code-read |
| agent_role_last_session_ids | src/session_manager.py | 1369 | code-read |
| agent_registrations | src/session_manager.py | 1379 | code-read |
| adoption_proposals | src/session_manager.py | 1390 | code-read |
| state | src/session_manager.py | 1876 | code-read |
| thread_id | src/session_manager.py | 1962 | code-read |
| session_id | src/session_manager.py | 1964 | code-read |
| thread_name | src/session_manager.py | 1967 | code-read |
| name | src/session_manager.py | 1967 | code-read |
| state | src/session_manager.py | 1973 | code-read |
| reason | src/session_manager.py | 1985 | code-read |
| duration_ms | src/session_manager.py | 2125 | code-read |
| tool_input | src/session_manager.py | 2137 | code-read |
| output_preview | src/session_manager.py | 2141 | code-read |
| error_message | src/session_manager.py | 2146 | code-read |
| thread | src/session_manager.py | 2255 | code-read |
| thread | src/session_manager.py | 2255 | code-read |
| id | src/session_manager.py | 2257 | code-read |
| thread_id | src/session_manager.py | 2258 | code-read |
| thread_id | src/session_manager.py | 2259 | code-read |
| session_id | src/session_manager.py | 2260 | code-read |
| forkedFromId | src/session_manager.py | 2263 | code-read |
| forked_from_id | src/session_manager.py | 2264 | code-read |
| forkedFromId | src/session_manager.py | 2265 | code-read |
| forked_from_id | src/session_manager.py | 2266 | code-read |
| id | src/session_manager.py | 2317 | code-read |
| thread_name | src/session_manager.py | 2318 | code-read |
| updated_at | src/session_manager.py | 2321 | code-read |
| schema_version | src/session_manager.py | 2419 | code-read |
| executed | src/session_manager.py | 2425 | code-read |
| success | src/session_manager.py | 2426 | code-read |
| ts | src/session_manager.py | 2436 | code-read |
| error_message | src/session_manager.py | 2437 | code-read |
| tool_name | src/session_manager.py | 2442 | code-read |
| session_id | src/session_manager.py | 2454 | code-read |
| session_id | src/session_manager.py | 2454 | code-read |
| turn_id | src/session_manager.py | 2455 | code-read |
| call_id | src/session_manager.py | 2456 | code-read |
| tool_kind | src/session_manager.py | 2458 | code-read |
| duration_ms | src/session_manager.py | 2460 | code-read |
| item | src/session_manager.py | 2476 | code-read |
| item | src/session_manager.py | 2476 | code-read |
| type | src/session_manager.py | 2477 | code-read |
| schema_version | src/session_manager.py | 2481 | code-read |
| ts | src/session_manager.py | 2487 | code-read |
| turn_id | src/session_manager.py | 2488 | code-read |
| turn_id | src/session_manager.py | 2488 | code-read |
| session_id | src/session_manager.py | 2489 | code-read |
| session_id | src/session_manager.py | 2489 | code-read |
| name | src/session_manager.py | 2492 | code-read |
| arguments | src/session_manager.py | 2497 | code-read |
| call_id | src/session_manager.py | 2503 | code-read |
| role | src/session_manager.py | 2524 | code-read |
| content | src/session_manager.py | 2528 | code-read |
| type | src/session_manager.py | 2536 | code-read |
| text | src/session_manager.py | 2538 | code-read |
| type | src/session_manager.py | 2555 | code-read |
| payload | src/session_manager.py | 2571 | code-read |
| payload | src/session_manager.py | 2571 | code-read |
| threadId | src/session_manager.py | 2652 | code-read |
| thread_id | src/session_manager.py | 2653 | code-read |
| thread | src/session_manager.py | 2654 | code-read |
| threadId | src/session_manager.py | 2655 | code-read |
| thread_id | src/session_manager.py | 2656 | code-read |
| session_id | src/session_manager.py | 2657 | code-read |
| session_id | src/session_manager.py | 2658 | code-read |
| turnId | src/session_manager.py | 2672 | code-read |
| turn_id | src/session_manager.py | 2673 | code-read |
| turnId | src/session_manager.py | 2674 | code-read |
| turn_id | src/session_manager.py | 2675 | code-read |
| event_type | src/session_manager.py | 2817 | code-read |
| type | src/session_manager.py | 2817 | code-read |
| payload | src/session_manager.py | 2819 | code-read |
| payload | src/session_manager.py | 2819 | code-read |
| itemId | src/session_manager.py | 2825 | code-read |
| item_id | src/session_manager.py | 2825 | code-read |
| message_item_id | src/session_manager.py | 2825 | code-read |
| delta | src/session_manager.py | 2832 | code-read |
| item | src/session_manager.py | 2837 | code-read |
| item | src/session_manager.py | 2837 | code-read |
| type | src/session_manager.py | 2838 | code-read |
| id | src/session_manager.py | 2842 | code-read |
| itemId | src/session_manager.py | 2843 | code-read |
| item_id | src/session_manager.py | 2844 | code-read |
| message_item_id | src/session_manager.py | 2845 | code-read |
| text | src/session_manager.py | 2855 | code-read |
| last_agent_message | src/session_manager.py | 2871 | code-read |
| event_type | src/session_manager.py | 2889 | code-read |
| type | src/session_manager.py | 2889 | code-read |
| payload | src/session_manager.py | 2928 | code-read |
| payload | src/session_manager.py | 2928 | code-read |
| session_id | src/session_manager.py | 2929 | code-read |
| session_id | src/session_manager.py | 2931 | code-read |
| ts | src/session_manager.py | 2946 | code-read |
| seq | src/session_manager.py | 2953 | code-read |
| session_epoch | src/session_manager.py | 2955 | code-read |
| last_agent_message | src/session_manager.py | 2972 | code-read |
| ts | src/session_manager.py | 2974 | code-read |
| session_id | src/session_manager.py | 2977 | code-read |
| session_id | src/session_manager.py | 2977 | code-read |
| schema_version | src/session_manager.py | 2985 | code-read |
| schema_version | src/session_manager.py | 2985 | code-read |
| session_id | src/session_manager.py | 2989 | code-read |
| thread_id | src/session_manager.py | 2990 | code-read |
| schema_version | src/session_manager.py | 3001 | code-read |
| persisted | src/session_manager.py | 3007 | code-read |
| ts | src/session_manager.py | 3015 | code-read |
| persisted | src/session_manager.py | 3017 | code-read |
| persisted | src/session_manager.py | 3026 | code-read |
| seq | src/session_manager.py | 3026 | code-read |
| type | src/session_manager.py | 3065 | code-read |
| payload | src/session_manager.py | 3068 | code-read |
| payload | src/session_manager.py | 3068 | code-read |
| id | src/session_manager.py | 3069 | code-read |
| cwd | src/session_manager.py | 3070 | code-read |
| timestamp | src/session_manager.py | 3071 | code-read |
| timestamp | src/session_manager.py | 3071 | code-read |
| id | src/session_manager.py | 3113 | code-read |
| cwd | src/session_manager.py | 3114 | code-read |
| started_at | src/session_manager.py | 3124 | code-read |
| events | src/session_manager.py | 3138 | code-read |
| event_type | src/session_manager.py | 3143 | code-read |
| payload_preview | src/session_manager.py | 3145 | code-read |
| payload | src/session_manager.py | 3146 | code-read |
| session_id | src/session_manager.py | 3147 | code-read |
| seq | src/session_manager.py | 3168 | code-read |
| session_epoch | src/session_manager.py | 3177 | code-read |
| session_epoch_key | src/session_manager.py | 3220 | code-read |
| session_epoch_key | src/session_manager.py | 3220 | code-read |
| seq | src/session_manager.py | 3221 | code-read |
| event_type | src/session_manager.py | 3312 | code-read |
| type | src/session_manager.py | 3312 | code-read |
| payload | src/session_manager.py | 3315 | code-read |
| payload | src/session_manager.py | 3315 | code-read |
| last_agent_message | src/session_manager.py | 3316 | code-read |
| claude | src/session_manager.py | 3514 | code-read |
| command | src/session_manager.py | 3515 | code-read |
| args | src/session_manager.py | 3516 | code-read |
| default_model | src/session_manager.py | 3517 | code-read |
| payload | src/session_manager.py | 3973 | code-read |
| payload | src/session_manager.py | 3973 | code-read |
| event_type | src/session_manager.py | 3974 | code-read |
| type | src/session_manager.py | 3974 | code-read |
| ts | src/session_manager.py | 3990 | code-read |
| maintainer | src/session_manager.py | 4127 | code-read |
| maintainer | src/session_manager.py | 4158 | code-read |
| preferred_providers | src/session_manager.py | 4370 | code-read |
| provider | src/session_manager.py | 4370 | code-read |
| working_dir | src/session_manager.py | 4373 | code-read |
| friendly_name | src/session_manager.py | 4374 | code-read |
| bootstrap_prompt | src/session_manager.py | 4377 | code-read |
| bootstrap_prompt_file | src/session_manager.py | 4379 | code-read |
| boot_prompt_file | src/session_manager.py | 4379 | code-read |
| task_complete_ttl_seconds | src/session_manager.py | 4381 | code-read |
| auto_bootstrap | src/session_manager.py | 4393 | code-read |
| service_roles | src/session_manager.py | 4410 | code-read |
| task_complete_ttl_seconds | src/session_manager.py | 4432 | code-read |
| bootstrap_prompt | src/session_manager.py | 4473 | code-read |
| bootstrap_prompt_file | src/session_manager.py | 4474 | code-read |
| claude | src/session_manager.py | 4500 | code-read |
| command | src/session_manager.py | 4500 | code-read |
| maintainer | src/session_manager.py | 4545 | code-read |
| task_complete_ttl_seconds | src/session_manager.py | 4545 | code-read |
| auto_bootstrap | src/session_manager.py | 4569 | code-read |
| claude | src/session_manager.py | 4715 | code-read |
| transcript_root | src/session_manager.py | 4716 | code-read |
| type | src/session_manager.py | 4773 | code-read |
| cwd | src/session_manager.py | 4774 | code-read |
| timestamp | src/session_manager.py | 4777 | code-read |
| type | src/session_manager.py | 4778 | code-read |
| customTitle | src/session_manager.py | 4779 | code-read |
| type | src/session_manager.py | 4782 | code-read |
| agentName | src/session_manager.py | 4783 | code-read |
| cwd | src/session_manager.py | 4865 | code-read |
| title | src/session_manager.py | 4868 | code-read |
| mtime_ns | src/session_manager.py | 4871 | code-read |
| started_at | src/session_manager.py | 4873 | code-read |
| title | src/session_manager.py | 4889 | code-read |
| mtime_ns | src/session_manager.py | 4889 | code-read |
| humans | src/session_manager.py | 5185 | code-read |
| email | src/session_manager.py | 5188 | code-read |
| bridge_config | src/session_manager.py | 5188 | code-read |
| humans | src/session_manager.py | 5196 | code-read |
| aliases | src/session_manager.py | 5218 | code-read |
| aliases | src/session_manager.py | 5218 | code-read |
| channels | src/session_manager.py | 5219 | code-read |
| channels | src/session_manager.py | 5219 | code-read |
| auto_bootstrap | src/session_manager.py | 5807 | code-read |
| task_complete_ttl_seconds | src/session_manager.py | 5809 | code-read |
| ok | src/session_manager.py | 6138 | code-read |
| error | src/session_manager.py | 6139 | code-read |
| error | src/session_manager.py | 6139 | code-read |
| code | src/session_manager.py | 6140 | code-read |
| message | src/session_manager.py | 6141 | code-read |
| result | src/session_manager.py | 6143 | code-read |
| result | src/session_manager.py | 6143 | code-read |
| epoch | src/session_manager.py | 6144 | code-read |
| epoch | src/session_manager.py | 6146 | code-read |
| ok | src/session_manager.py | 6189 | code-read |
| error | src/session_manager.py | 6190 | code-read |
| error | src/session_manager.py | 6190 | code-read |
| code | src/session_manager.py | 6191 | code-read |
| ok | src/session_manager.py | 6200 | code-read |
| error | src/session_manager.py | 6201 | code-read |
| error | src/session_manager.py | 6201 | code-read |
| code | src/session_manager.py | 6202 | code-read |
| message | src/session_manager.py | 6203 | code-read |
| epoch | src/session_manager.py | 6206 | code-read |
| pane_dead | src/session_manager.py | 6246 | code-read |
| pane_dead_status | src/session_manager.py | 6247 | code-read |
| pane_current_command | src/session_manager.py | 6248 | code-read |
| item | src/session_manager.py | 6619 | code-read |
| item | src/session_manager.py | 6619 | code-read |
| turnId | src/session_manager.py | 6620 | code-read |
| id | src/session_manager.py | 6621 | code-read |
| turnId | src/session_manager.py | 6657 | code-read |
| item | src/session_manager.py | 6674 | code-read |
| item | src/session_manager.py | 6674 | code-read |
| id | src/session_manager.py | 6675 | code-read |
| turnId | src/session_manager.py | 6676 | code-read |
| type | src/session_manager.py | 6681 | code-read |
| command | src/session_manager.py | 6693 | code-read |
| cwd | src/session_manager.py | 6694 | code-read |
| filePath | src/session_manager.py | 6695 | code-read |
| path | src/session_manager.py | 6695 | code-read |
| diffSummary | src/session_manager.py | 6696 | code-read |
| summary | src/session_manager.py | 6696 | code-read |
| delta | src/session_manager.py | 6703 | code-read |
| type | src/session_manager.py | 6719 | code-read |
| status | src/session_manager.py | 6722 | code-read |
| command | src/session_manager.py | 6754 | code-read |
| cwd | src/session_manager.py | 6755 | code-read |
| exitCode | src/session_manager.py | 6756 | code-read |
| filePath | src/session_manager.py | 6757 | code-read |
| path | src/session_manager.py | 6757 | code-read |
| diffSummary | src/session_manager.py | 6758 | code-read |
| summary | src/session_manager.py | 6758 | code-read |
| errorCode | src/session_manager.py | 6761 | code-read |
| errorMessage | src/session_manager.py | 6762 | code-read |
| session_id | src/session_manager.py | 6804 | code-read |
| session_id | src/session_manager.py | 6810 | code-read |
| phase | src/session_manager.py | 6884 | code-read |
| phase | src/session_manager.py | 6885 | code-read |
| state | src/session_manager.py | 7036 | code-read |
| state | src/session_manager.py | 7146 | code-read |
| cause_event_type | src/session_manager.py | 7147 | code-read |
| session_id | src/session_manager.py | 7202 | code-read |
| ok | src/session_manager.py | 7214 | code-read |
| request_method | src/session_manager.py | 7216 | code-read |
| thread_id | src/session_manager.py | 7225 | code-read |
| turn_id | src/session_manager.py | 7226 | code-read |
| item_id | src/session_manager.py | 7227 | code-read |
| decision | src/session_manager.py | 7232 | code-read |
| claude | src/session_manager.py | 7496 | code-read |
| command | src/session_manager.py | 7497 | code-read |
| args | src/session_manager.py | 7498 | code-read |
| codex | src/session_manager.py | 7810 | code-read |
| review | src/session_manager.py | 7811 | code-read |

Reconciliation notes:

- `nodes.*`, `auth.google.*`, `mobile_terminal.*`, email bridge secret/header keys, queue runner policy keys, and provider command/args are security-sensitive compatibility contracts.
- Raw code-read keys include local row/payload fields as well as real config reads. Treat them as search evidence for follow-up reconciliation, not as final config contracts, unless covered by a verified section, the exact example-default summary, or a later explicit classification.
