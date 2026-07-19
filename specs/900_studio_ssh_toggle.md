# Studio SSH Toggle — Implementation Contract (frozen)

Integration branch: `feat/studio-ssh-toggle`. Every worker branches off it and PRs back into it.
Full design/rationale: `/Users/rajesh/.claude/plans/prancy-prancing-fern.md` (approved).

## Goal

A phone/web toggle that exposes `ssh studio-away` (→ `studio-ssh.rajeshgo.li`) from any network,
key-only, over a dedicated cloudflared tunnel. OFF tears everything down. This doc PINS every
shared name so the four workstreams can proceed in parallel without re-exploring.

## Frozen shared constants

| Thing | Value |
|---|---|
| Public hostname | `studio-ssh.rajeshgo.li` |
| cloudflared tunnel name | `studio-ssh` |
| Dedicated sshd bind | `127.0.0.1:22222` (loopback only) |
| sshd LaunchAgent label | `com.rajesh.sm-studio-ssh-sshd` |
| tunnel LaunchAgent label | `com.rajesh.sm-studio-ssh-tunnel` |
| Plist path (each) | `~/Library/LaunchAgents/<label>.plist` |
| launchd domain | `gui/501` (uid 501) |
| cloudflared assets dir | `<repo>/.local/studio-ssh/cloudflared/` (`config.yml` + `<uuid>.json`) |
| sshd assets dir | `~/.local/share/session-manager/studio-ssh/` (`sshd_config`, host key, logs) |
| authorized_keys | `/Users/rajesh/.ssh/authorized_keys` (already contains the MacBook key) |
| server | Rust `sm-server`, axum 0.8, `127.0.0.1:8420` (crates/sm-server) — Python `src/` is LEGACY, do not touch |

## Frozen HTTP API (server ↔ Android ↔ web)

- `GET /admin/studio-ssh` → 200 JSON `StudioSshStatus`
- `POST /admin/studio-ssh` body `{ "enabled": true|false }` → 200 JSON `StudioSshStatus`

`StudioSshStatus` (exact field names):
```json
{
  "enabled": true,
  "status": "off",            // one of: "off" | "starting" | "on" | "error"
  "host": "studio-ssh.rajeshgo.li",
  "sshd_listening": false,
  "tunnel_running": false,
  "error": null               // string when status=="error", else null
}
```
Semantics: `enabled` = desired state (both LaunchAgents enabled in launchd). `status` = observed:
`on` only when `sshd_listening && tunnel_running`; `starting` when enabled but not yet both up;
`off` when disabled; `error` with `error` message on failure. POST enable returns immediately after
kicking off enable() — it may return `starting`; clients poll GET.

Auth (both routes): reuse the EXACT preamble of `disable_mobile_terminal` in `http.rs` — Cloudflare
Access mobile-app JWT + public-edge assertion + `request_actor_email` + owner gate
(`mobile_terminal_owner` via the `mobile_terminal_user_can_disable`-style check). Loopback requests
(`is_local_bypass_request`) skip Access — required for local curl tests and the reconcile loop.

## Workstream A — Rust server (`crates/sm-server/`)

Files: `src/config.rs`, `src/studio_ssh.rs` (new), `src/http.rs`, `src/lib.rs` (module decl),
`src/main.rs` (spawn reconcile task).

1. **config.rs**: add `StudioSshConfig` nested in `ExternalAccessConfig` as field `studio_ssh`
   (`#[serde(default)] pub studio_ssh: StudioSshConfig`). `ExternalAccessConfig` is passed through
   unchanged in `From<RawConfig>` (`external_access: raw.external_access`), so nesting here needs no
   other wiring. `StudioSshConfig` (`#[derive(Debug,Clone,Deserialize)]` + manual `Default`) fields:
   `hostname` (default `studio-ssh.rajeshgo.li`), `local_sshd_port: u16` (default `22222`),
   `sshd_launch_agent_label` (default `com.rajesh.sm-studio-ssh-sshd`),
   `tunnel_launch_agent_label` (default `com.rajesh.sm-studio-ssh-tunnel`). Plist paths derive from
   `~/Library/LaunchAgents/<label>.plist` at runtime.
2. **studio_ssh.rs** (new module, declared in `lib.rs`): pure functions taking `&StudioSshConfig`:
   - `pub fn status(cfg) -> StudioSshStatus` — check both agents via
     `launchctl print gui/<uid>/<label>` (loaded?) and TCP-connect `127.0.0.1:<port>`; derive status.
   - `pub fn enable(cfg) -> Result<StudioSshStatus>` — for sshd THEN tunnel:
     `launchctl enable gui/<uid>/<label>` then `launchctl bootstrap gui/<uid> <plist>`
     (ignore "already bootstrapped" errors) then `launchctl kickstart -k gui/<uid>/<label>`.
   - `pub fn disable(cfg) -> Result<StudioSshStatus>` — tunnel THEN sshd:
     `launchctl bootout gui/<uid>/<label>` (ignore not-loaded) then `launchctl disable gui/<uid>/<label>`.
   - `pub fn reconcile(cfg) -> StudioSshStatus` — if enabled-but-not-healthy, re-run the enable steps.
   - Get uid from `/usr/bin/id -u` (cache once) or `nix::unistd::getuid()`. Run launchctl via the
     established `command_output_with_timeout` pattern (see `http.rs:406`); treat launchctl's benign
     nonzero exits (already-loaded / not-loaded) as non-fatal.
   - Return type `StudioSshStatus` should be a `#[derive(Serialize)]` struct matching the frozen JSON
     — define it here and reuse in the handler.
3. **http.rs**:
   - `AppState` (struct ~`:337`, init ~`:379`): add `studio_ssh_enabled: Arc<AtomicBool>`, seeded
     from `studio_ssh::status(...).enabled` at construction.
   - Handlers modeled on `disable_mobile_terminal` (~`:4501`): `set_studio_ssh` (POST) and
     `get_studio_ssh` (GET). Copy the auth preamble verbatim (lines ~4505-4529). POST calls
     enable/disable, updates the AtomicBool, returns status JSON.
   - Register routes in the `router()` chain (~`:847`): `.route("/admin/studio-ssh",
     get(get_studio_ssh).post(set_studio_ssh))`.
   - Request struct `StudioSshToggleRequest { enabled: bool }`.
   - **Status surfacing**: add `studio_ssh` entry to the `health_detailed` checks map (~`:1001`),
     and add `studio_ssh_enabled: bool` + `studio_ssh_host: String` to `BootstrapExternalAccess`
     (~`:11283`) populated in `client_bootstrap_response` (~`:7385`).
4. **main.rs**: after the server/AppState is built, `tokio::spawn` a loop: every 30s, if
   `studio_ssh_enabled` is true, call `studio_ssh::reconcile(cfg)` (spawn_blocking since launchctl is
   sync). Never flip the desired flag; only repair toward it.

Build gate: `cargo build --release -p sm-server` must pass; add/adjust unit tests where the crate
already has them (`crates/sm-server/tests/`). Do NOT restart the running server or run any setup
script — the orchestrator handles deploy.

## Workstream B — Setup script + launchd/sshd/cloudflared assets

Files: `scripts/setup_studio_ssh.sh` (new, idempotent), plist + config TEMPLATES it writes.
The orchestrator RUNS this; the worker writes and locally UNIT-tests the sshd piece only.

The script must (idempotently):
1. `cloudflared tunnel create studio-ssh` if absent (parse/create `<uuid>.json` creds; cert is
   `~/.cloudflared/cert.pem`). Write `<repo>/.local/studio-ssh/cloudflared/config.yml`:
   ```yaml
   tunnel: <uuid>
   credentials-file: <repo>/.local/studio-ssh/cloudflared/<uuid>.json
   ingress:
     - hostname: studio-ssh.rajeshgo.li
       service: ssh://127.0.0.1:22222
     - service: http_status:404
   ```
2. `cloudflared tunnel route dns studio-ssh studio-ssh.rajeshgo.li` (idempotent; ignore "already
   exists").
3. Create `~/.local/share/session-manager/studio-ssh/`, generate a dedicated ed25519 HOST key
   (`ssh-keygen -t ed25519 -f .../ssh_host_ed25519_key -N ""`), and write `sshd_config`:
   ```
   Port 22222
   ListenAddress 127.0.0.1
   HostKey <dir>/ssh_host_ed25519_key
   PidFile <dir>/sshd.pid
   PasswordAuthentication no
   KbdInteractiveAuthentication no
   ChallengeResponseAuthentication no
   PubkeyAuthentication yes
   AuthorizedKeysFile /Users/rajesh/.ssh/authorized_keys
   AllowUsers rajesh
   PermitRootLogin no
   UsePAM no
   LogLevel VERBOSE
   ```
   NOTE: a non-root user-run sshd only permits login as the running user (rajesh) — that is exactly
   our single-user case and needs no root. **You MUST prove it**: launch
   `/usr/sbin/sshd -D -f <sshd_config>` in the background and confirm
   `ssh -p 22222 -o StrictHostKeyChecking=no -i ~/.ssh/id_ed25519 rajesh@127.0.0.1 true` succeeds AND
   `ssh -p 22222 -o PubkeyAuthentication=no rajesh@127.0.0.1 true` is REFUSED (key-only). If the
   dedicated sshd cannot be made to work as non-root, STOP and report — do not silently fall back.
4. Write both LaunchAgent plists to `~/Library/LaunchAgents/`:
   - sshd plist: ProgramArguments `[/usr/sbin/sshd, -D, -f, <sshd_config>]`, `RunAtLoad` true,
     `KeepAlive` true, std out/err to the sshd dir.
   - tunnel plist: ProgramArguments `[/opt/homebrew/bin/cloudflared, tunnel, --config,
     <repo>/.local/studio-ssh/cloudflared/config.yml, run, studio-ssh]`, `RunAtLoad` true,
     `KeepAlive` true, logs under the cloudflared dir.
5. Leave BOTH agents **installed but disabled** (`launchctl bootout` if loaded, then
   `launchctl disable gui/501/<label>`) so default state is OFF. Print a summary.

The script takes `--dry-run` (print actions, touch nothing outward-facing) so the orchestrator can
inspect before the real run.

## Workstream C — Android app (`android-app/`, Kotlin/Compose) + BUILD & DEPLOY

Base URL is `https://sm.rajeshgo.li` → relative Retrofit paths. Files:
`data/model/ApiModels.kt`, `data/remote/ApiService.kt`, `data/repository/SessionManagerRepository.kt`,
`ui/watch/WatchViewModel.kt`, `ui/watch/WatchScreen.kt`.

- DTOs: `StudioSshToggleRequest(enabled: Boolean)`, `StudioSshStatusResponse(enabled, status, host,
  sshdListening, tunnelRunning, error)` with `@SerialName` matching the frozen snake_case JSON
  (`sshd_listening`, `tunnel_running`).
- `ApiService`: `@GET("admin/studio-ssh") suspend fun getStudioSshStatus(): StudioSshStatusResponse`
  and `@POST("admin/studio-ssh") suspend fun setStudioSsh(@Body req): StudioSshStatusResponse`.
- Repository: `fetchStudioSshStatus(...)` via `executeReadRequest`, `setStudioSsh(...)` via
  `classifyWriteFailure` — mirror `requestStatus()`.
- ViewModel: add `studioSshEnabled/studioSshStatus/studioSshBusy/studioSshError` to `WatchUiState`;
  `toggleStudioSsh(enabled, onComplete)` (optimistic "starting", toast result) and
  `refreshStudioSshStatus()`; hook the status refresh into the existing 5s poll loop.
- UI: a `StudioSshToggleCard` (Material3 `Switch` + `StatusChip`/`statusDot` for
  off/starting/on/error, and show the `host` + a one-line hint "ssh studio-away") inserted as a
  `LazyColumn` item right after `HeaderBar` on the Watch screen.
- **Build & deploy**: build the release APK and PUSH the in-app update so a phone can pull it
  tomorrow. Investigate `scripts/deploy_android_app.sh` and the app-update path
  (`AppUpdateRepository`, server `app_artifacts`); run whatever that pipeline needs so the new
  version is the one the updater serves. Report the exact version/build you published and how the
  phone will receive it. If the build toolchain is unavailable, report immediately with specifics.

## Workstream D — Web (`web/sm-watch/`, React/Vite)

Add a Studio SSH toggle + status chip in `src/App.tsx` (near the pause control) hitting
`GET`/`POST /admin/studio-ssh` (same origin as `/watch`). Reflect `enabled`/`status`. Run
`npm run build` so `web/sm-watch/dist/` is updated (served by the Rust static mount).

## Per-worker git + review workflow (all workstreams)

1. `cd /Users/rajesh/projects/session-manager && git fetch origin`
2. Create your OWN worktree off the integration branch (isolates you from siblings):
   `git worktree add /Users/rajesh/projects/sm-wt-<chunk> -b feat/studio-ssh-<chunk> origin/feat/studio-ssh-toggle`
   then `cd` into it. (If `origin/feat/studio-ssh-toggle` isn't fetched yet, base off local
   `feat/studio-ssh-toggle`.)
3. Implement ONLY your workstream's files. Keep commits scoped.
4. Self-review to clean per `docs/working/pr_review_process.md`: push your branch, open a PR
   with `gh pr create --base feat/studio-ssh-toggle`, run the review loop
   (`sm request-codex-review <pr#>` or the fallback), and address any **P1** feedback. P2/lower is
   optional.
5. When your PR is clean (no P1s) and your build gate passes, MERGE it into the integration branch
   (`gh pr merge <pr#> --squash --delete-branch`), remove your worktree
   (`git worktree remove <path>`), and report back: PR URL, merge status, build/test results, and
   anything the orchestrator must do.
6. Do NOT touch other workstreams' files, do NOT restart the running server, do NOT run
   `setup_studio_ssh.sh` (workstream B is written but run by the orchestrator).
