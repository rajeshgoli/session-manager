use std::{
    net::SocketAddr,
    os::{fd::AsRawFd, unix::net::UnixListener},
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use sm_server::{
    activity_ledger::ActivityRecorder,
    config::AppConfig,
    eprintln,
    handover::{self, Shutdown},
    http::{router, AppState, BtwWorkers},
    owner_settings,
    queue::{QueueRecoverySummary, RetainedQueueStore},
    queue_authority::{QueueAuthorityServer, QueueAuthorityServiceIdentity},
    sessions::{expand_home, SessionStore},
    terminal_lan,
    usage_identity::IdentityPoller,
};
use tokio::net::TcpListener;

/// Run one pass of a background loop. A panic ends the pass, not the loop:
/// a dead loop stops its work silently until the next restart (#1996).
fn run_background_pass(name: &str, pass: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(pass)).is_err() {
        eprintln!("{name} pass panicked; the loop continues");
    }
}

/// How often the Studio SSH reconcile loop repairs toward the desired state.
const STUDIO_SSH_RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const QUEUE_COMPLETION_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const REPARENT_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
/// Owner follows deliver every 5s and sweep for finished targets every
/// third pass (sm#1569).
const FOLLOW_DELIVERY_INTERVAL: Duration = Duration::from_secs(5);
const FOLLOW_SWEEP_EVERY_PASSES: u64 = 3;
/// How often Finished rows are checked for the 10-minute text fallback.
const FINISHED_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// How long after a post-restart start sm waits before restoring agents.
const HOST_RESTART_RESTORE_DELAY: Duration = Duration::from_secs(10);
/// The open-file soft limit sm-server raises itself to at startup. launchd
/// starts it at 256, which every queue job, tmux server, and agent it spawns
/// inherits; a parallel test suite run as a queue job exhausts that ("Too many
/// open files", sm#1336). Matches scripts/test-rust-isolated.sh.
const OPEN_FILE_SOFT_LIMIT_TARGET: u64 = 8192;

#[derive(Debug, Parser)]
#[command(version, about = "Rust Session Manager server scaffold")]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8421)]
    port: u16,
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,
    #[arg(long)]
    local_env: Option<PathBuf>,
    /// Run the separately supervised host-only local-agent HTTPS proxy.
    #[arg(long)]
    local_egress_service: Option<PathBuf>,
    /// Own a local provider and its queue/socket authority in a host tmux window.
    #[arg(long)]
    local_wall_owner: Option<PathBuf>,
    /// Load and validate the configuration, then exit without binding a port or
    /// touching any state. scripts/restart-rust-server.sh uses this to reject a
    /// bad config while the old server is still running, rather than discovering
    /// it after the service has been stopped.
    #[arg(long)]
    check_config: bool,
    /// Internal application-owned backup preparation worker.
    #[arg(long, requires = "backup_db", hide = true)]
    backup_prepare: Option<String>,
    #[arg(long, requires = "backup_prepare", hide = true)]
    backup_db: Option<PathBuf>,
    /// Internal worker building the backup mini's sm for this commit (#2264).
    #[arg(long, requires_all = ["backup_cli_dir", "backup_cli_source", "backup_cli_version"], hide = true)]
    backup_cli_build: Option<String>,
    #[arg(long, requires = "backup_cli_build", hide = true)]
    backup_cli_dir: Option<PathBuf>,
    #[arg(long, requires = "backup_cli_build", hide = true)]
    backup_cli_source: Option<String>,
    #[arg(long, requires = "backup_cli_build", hide = true)]
    backup_cli_version: Option<String>,
    /// Wait for the serving slot to pass its listeners before startup writes.
    #[arg(long)]
    take_over: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // The LAN listener and ACME client both use rustls. Their dependency graph
    // enables more than one crypto backend, so choose one before either runs.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let args = Args::parse();
    if let Some(id) = args.backup_prepare {
        return sm_server::managed_artifacts::backup::lifecycle::run_worker(
            args.backup_db.unwrap(),
            &id,
        );
    }
    if let Some(commit) = args.backup_cli_build {
        return sm_server::managed_artifacts::backup::cli_build::run_worker(
            &commit,
            &args.backup_cli_dir.unwrap(),
            &args.backup_cli_source.unwrap(),
            &args.backup_cli_version.unwrap(),
        );
    }
    if let Some(path) = args.local_wall_owner {
        #[cfg(target_os = "macos")]
        {
            raise_open_file_soft_limit();
            return sm_server::local_wall::owner::run(path).await;
        }
        #[cfg(not(target_os = "macos"))]
        anyhow::bail!("local wall ownership requires macOS: {}", path.display());
    }
    if let Some(directory) = args.local_egress_service {
        raise_open_file_soft_limit();
        return sm_server::local_egress::run_service(&directory)
            .await
            .map_err(Into::into);
    }
    let config = AppConfig::load_from_path_with_local_env(&args.config, args.local_env.as_deref())?;
    let address: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .with_context(|| format!("invalid listen address {}:{}", args.host, args.port))?;
    if config.usage.enabled && config.usage.poll_interval_secs == 0 {
        anyhow::bail!("usage.poll_interval_secs must be > 0 when usage is enabled");
    }
    if config.usage.enabled && config.usage.db_path.trim().is_empty() {
        anyhow::bail!("usage.db_path must not be empty when usage is enabled");
    }
    if config.usage.enabled
        && (!config.usage.premium_cap_ratio.is_finite()
            || config.usage.premium_cap_ratio <= 0.0
            || config.usage.premium_cap_ratio > 1.0)
    {
        anyhow::bail!("usage.premium_cap_ratio must be greater than 0 and at most 1");
    }
    // After the address is parsed so a bad --host/--port is caught too, but
    // before binding, so this can run while the old server still holds the port.
    if args.check_config {
        println!(
            "configuration ok: {} (listen {address})",
            args.config.display()
        );
        // Report the overlay too. "configuration ok" on its own is misleading:
        // the overlay carries the Google auth credentials, is addressed relative
        // to the config file, and is skipped silently when absent - so a config
        // that parses cleanly can still have no working sign-in.
        let overlay =
            sm_server::config::local_env_overlay_path(&args.config, args.local_env.as_deref());
        println!("owner name: {}", config.owner_name);
        println!(
            "managed_artifacts:\n{}",
            serde_yaml::to_string(&config.managed_artifacts)?
        );
        if config.managed_artifacts.enabled {
            for root in &config.managed_artifacts.resolved()?.roots {
                for (path, role) in [(&root.path, "root"), (&root.cold_store, "cold")] {
                    match nix::sys::statvfs::statvfs(std::path::Path::new(path)) {
                        Ok(s) => {
                            let bytes = u64::from(s.blocks()) * s.fragment_size();
                            let floor = bytes * config.managed_artifacts.floor_percent / 100;
                            println!("artifact {role}: {path} mounted=true bytes={bytes} free={} floor={floor} enough={floor}",u64::from(s.blocks_available())*s.fragment_size());
                        }
                        Err(_) => println!("artifact {role}: {path} mounted=false"),
                    }
                }
            }
        }
        println!(
            "local env overlay: {} ({})",
            overlay.display(),
            if overlay.exists() {
                "found"
            } else {
                "MISSING - auth overrides not applied"
            }
        );
        // Same trap as the overlay: the email bridge file is addressed relative
        // to the config file and a missing one only surfaces as a 503 on send.
        let bridge_path = &config.email.bridge_config;
        match sm_server::email::EmailBridge::load(&config) {
            Ok(bridge) if bridge.bridge_is_available() => {
                println!("email bridge: {bridge_path} (ready)");
            }
            Ok(bridge) => println!(
                "email bridge: {bridge_path} (UNAVAILABLE - sm email will fail: {})",
                bridge.availability_error_detail()
            ),
            Err(error) => println!(
                "email bridge: {bridge_path} (UNAVAILABLE - sm email will fail: {error:#})"
            ),
        }
        // Follows fall back to email without push, so a broken key file only
        // shows up as notifications arriving by email instead of on the phone.
        match sm_server::push_fcm::FcmSender::from_config(&config) {
            Some(Ok(sender)) => println!("push: ready (project {})", sender.project_id()),
            Some(Err(error)) => println!("push: broken: {error:#}"),
            None => println!("push: not configured"),
        }
        return Ok(());
    }
    raise_open_file_soft_limit();
    let queue_state_dir_config = config.queue_runner_state_dir();
    let queue_state_dir = expand_home(&queue_state_dir_config.to_string_lossy());
    let state_file = expand_home(&config.paths.state_file);
    let handover_dir = state_file
        .parent()
        .context("session state has no parent directory")?;
    let serving_slot = if args.take_over {
        let serving = handover::connect_serving(handover_dir)?;
        if serving.is_none() {
            eprintln!("no serving slot to take over from; starting cold");
        }
        serving
    } else {
        None
    };
    let (listener, mut authority_server, handover_listener, mut takeover_stream, mut inherited_lan) =
        if let Some(mut stream) = serving_slot {
            handover::send_request(&mut stream)?;
            let (mut fds, has_lan) = handover::receive_listeners(&stream)?;
            let listener = std::net::TcpListener::from(fds.remove(0));
            let authority = UnixListener::from(fds.remove(0));
            let handover_listener = UnixListener::from(fds.remove(0));
            let lan = has_lan.then(|| std::net::TcpListener::from(fds.remove(0)));
            (
                listener,
                QueueAuthorityServer::from_listener(
                    authority,
                    &queue_state_dir,
                    QueueAuthorityServiceIdentity::current()?,
                ),
                handover_listener,
                Some(stream),
                lan,
            )
        } else {
            (
                std::net::TcpListener::bind(address)
                    .with_context(|| format!("failed to bind {address}"))?,
                QueueAuthorityServer::bind(
                    &queue_state_dir,
                    QueueAuthorityServiceIdentity::current()?,
                )?,
                handover::bind_socket(handover_dir)?,
                None,
                None,
            )
        };
    listener.set_nonblocking(true)?;
    eprintln!(
        "sm-server queue authority on {}",
        authority_server.socket_path().display()
    );
    let btw_workers = BtwWorkers::default();
    let mut resuming_old_slot = false;
    'generations: loop {
        let shutdown = Shutdown::default();
        sm_server::queue::set_live_queue_shutdown(shutdown.clone());
        // No generation workers start until every fallible state preparation
        // step succeeds. Failed attempts drop their state while the outer
        // listener remains bound, so queued connections survive the retry.
        let InitializedGeneration {
            state,
            reconciliation_snapshot,
            poller,
            local_model,
            #[cfg(target_os = "macos")]
            local_walls,
        } = initialize_with_retry(
            resuming_old_slot.then_some(ROLLBACK_INITIALIZATION_RETRY),
            || {
                if takeover_stream.is_some() || resuming_old_slot {
                    // LOCAL_PEERPID must report this process after handover.
                    authority_server.rebind_after_handover()?;
                }
                if resuming_old_slot {
                    write_active_slot(handover_dir)?;
                }
                initialize_generation(
                    &config,
                    args.port,
                    &queue_state_dir,
                    &shutdown,
                    &btw_workers,
                )
            },
        )
        .await?;
        #[cfg(target_os = "macos")]
        let _local_wall_guard = local_walls.as_ref().map(|walls| walls.guard());
        #[cfg(target_os = "macos")]
        if let Some(walls) = &local_walls {
            match walls.reconcile() {
                Ok(failures) => {
                    for failure in failures {
                        eprintln!("local wall restore deferred: {failure}");
                    }
                }
                Err(error) => eprintln!("local wall restore failed: {error:#}"),
            }
            let worker = walls.clone();
            let stopped = shutdown.clone();
            thread::spawn(move || {
                while !stopped.is_stopped() {
                    thread::sleep(Duration::from_secs(5));
                    if stopped.is_stopped() {
                        break;
                    }
                    match worker.reconcile() {
                        Ok(failures) => {
                            for failure in failures {
                                eprintln!("local wall restore deferred: {failure}");
                            }
                        }
                        Err(error) => eprintln!("local wall restore failed: {error:#}"),
                    }
                }
            });
        }
        let authority_thread = authority_server.spawn(shutdown.clone())?;
        let (handover_tx, mut handover_rx) = tokio::sync::mpsc::channel(1);
        let handover_acceptor =
            handover::spawn_acceptor(&handover_listener, shutdown.clone(), handover_tx)?;

        if config.managed_artifacts.enabled {
            let artifact_state = state.clone();
            let artifact_shutdown = shutdown.clone();
            let interval = Duration::from_secs(config.managed_artifacts.scan_seconds);
            thread::spawn(move || loop {
                if artifact_shutdown.is_stopped() {
                    break;
                }
                run_background_pass("managed artifacts", || {
                    if let Err(error) = artifact_state.backup_pass() {
                        eprintln!("backup pass failed: {error:#}");
                    }
                    if let Err(error) = artifact_state.managed_artifacts_pass() {
                        eprintln!("managed artifacts pass failed: {error:#}");
                    }
                });
                thread::sleep(interval);
            });
        }
        if config.rust_core.runtime_enabled {
            let reload_dir = queue_state_dir.clone();
            let reload_shutdown = shutdown.clone();
            thread::spawn(move || loop {
                if reload_shutdown.is_stopped() {
                    break;
                }
                run_background_pass("local model reload", || {
                    if let Err(error) = sm_server::local_model::retry_reload(&reload_dir) {
                        eprintln!("local model reload failed: {error:#}");
                    }
                });
                thread::sleep(QUEUE_COMPLETION_RETRY_INTERVAL);
            });
        }

        if let Some(model) = local_model {
            let judge = sm_server::local_judge::LocalJudgeRuntime::from_config(&config);
            let judge_shutdown = shutdown.clone();
            thread::spawn(move || loop {
                if judge_shutdown.is_stopped() {
                    break;
                }
                run_background_pass("local model", || {
                    if let Err(error) = model.reconcile_draining() {
                        eprintln!("local model reconcile failed: {error:#}");
                    }
                });
                run_background_pass("local judge", || {
                    if let Err(error) = judge.reconcile() {
                        eprintln!("local judge reconcile failed: {error:#}");
                    }
                });
                thread::sleep(Duration::from_secs(5));
            });
        }
        // Only the live queue server records: scratch servers run with the
        // runtime off so they stay clear of live queue state (sm#1609).
        if config.rust_core.runtime_enabled && config.utilization.enabled {
            if cfg!(target_os = "macos") {
                sm_server::utilization::spawn_recorder(
                    sm_server::utilization::RecorderSettings {
                        db_path: expand_home(&config.utilization.db_path),
                        queue_db_path: queue_state_dir.join("queue_runner.db"),
                        interval: Duration::from_secs(config.utilization.sample_interval_seconds),
                        retention_days: config.utilization.retention_days,
                        quiet_minutes: config.queue_runner.quiet_minutes,
                        quiet_alert_repeat_minutes: config.queue_runner.quiet_alert_repeat_minutes,
                        message_queue_db_path: expand_home(&config.sm_send.db_path),
                    },
                    shutdown.clone(),
                );
            } else {
                eprintln!("utilization recorder: host sampling is only available on macOS");
            }
        }

        if config.rust_core.runtime_enabled {
            let message_queue_db_path = expand_home(&config.sm_send.db_path);
            let cancel_grace_seconds = config.queue_runner.cancel_grace_seconds;
            // Stored owner limits apply from the first pass; `PUT /client/settings`
            // changes the shared policy after that (sm#1718).
            let settings = SessionStore::new(expand_home(&config.paths.state_file))
                .owner_settings()
                .unwrap_or_else(|error| {
                    eprintln!("owner settings unreadable, using config queue limits: {error:#}");
                    owner_settings::defaults()
                });
            let admission_policy = owner_settings::queue_admission_policy(&config, &settings);
            sm_server::queue::set_live_queue_admission_policy(&queue_state_dir, admission_policy);
            sm_server::queue::spawn_host_memory_guard(
                queue_state_dir.clone(),
                cancel_grace_seconds,
                admission_policy,
            );
            sm_server::agent_memory_guard::spawn_agent_memory_guard(
                sm_server::agent_memory_guard::AgentMemoryGuardConfig {
                    session_state_file: expand_home(&config.paths.state_file),
                    queue_state_dir: queue_state_dir.clone(),
                    message_queue_db_path: message_queue_db_path.clone(),
                    limit_bytes: config.queue_runner.memory.agent_process_max_bytes,
                    exempt_commands: config
                        .queue_runner
                        .memory
                        .agent_process_exempt_commands
                        .clone(),
                },
            );
            match RetainedQueueStore::recover_queue_jobs_in_state_dir_after_boot(
                &queue_state_dir,
                &message_queue_db_path,
                cancel_grace_seconds,
                admission_policy,
                sm_server::host_restart::system_boot_time(),
            ) {
                Ok(summary) if summary != QueueRecoverySummary::default() => {
                    eprintln!("queue runtime recovery: {summary:?}");
                }
                Ok(_) => {}
                Err(error) => eprintln!("queue runtime recovery failed: {error:#}"),
            }
            let queue_shutdown = shutdown.clone();
            let retry_queue_state_dir = queue_state_dir.clone();
            thread::spawn(move || loop {
                thread::sleep(QUEUE_COMPLETION_RETRY_INTERVAL);
                if queue_shutdown.is_stopped() {
                    break;
                }
                run_background_pass("queue completion wake retry", || {
                    if let Err(error) =
                        RetainedQueueStore::retry_unnotified_queue_job_completions_in_state_dir_with_policy(
                            &retry_queue_state_dir,
                            &message_queue_db_path,
                            admission_policy,
                        )
                    {
                        eprintln!("queue completion wake retry failed: {error:#}");
                    }
                });
            });
        }

        if state.detected_host_restart().is_some() {
            // Restored agents' hooks call back into this server, so give it a
            // moment to start serving first (sm#2054).
            let restore_state = state.clone();
            thread::spawn(move || {
                thread::sleep(HOST_RESTART_RESTORE_DELAY);
                run_background_pass("host restart auto-restore", || {
                    restore_state.auto_restore_after_host_restart();
                });
            });
        }
        let lan_control = terminal_lan::LanControl::default();
        let expected_lan = inherited_lan.is_some();
        if state.config().terminal_direct.lan.enabled {
            tokio::spawn(terminal_lan::run(
                Arc::new(state.clone()),
                lan_control.clone(),
                inherited_lan.take(),
                shutdown.clone(),
            ));
        }
        // Reparent lifecycle and notification delivery can take the cross-process
        // apply lock, access the retained queue, and talk to tmux.  Keep all of
        // that work on one dedicated worker: watch polling is a snapshot read and
        // must never wait behind it.
        let reparent_state = state.clone();
        let reparent_shutdown = shutdown.clone();
        thread::spawn(move || loop {
            thread::sleep(REPARENT_RECONCILE_INTERVAL);
            if reparent_shutdown.is_stopped() {
                break;
            }
            run_background_pass("reparent background reconciliation", || {
                if let Err(error) = reparent_state.reconcile_reparent_background() {
                    eprintln!("reparent background reconciliation failed: {error:#}");
                }
            });
        });
        if state.config().rust_core.runtime_enabled {
            let follow_state = state.clone();
            let follow_shutdown = shutdown.clone();
            thread::spawn(move || {
                let mut pass: u64 = 0;
                loop {
                    if follow_shutdown.is_stopped() {
                        break;
                    }
                    run_background_pass("owner follow", || {
                        match follow_state.run_follow_pass(pass % FOLLOW_SWEEP_EVERY_PASSES == 0) {
                            Ok(problems) => {
                                for problem in problems {
                                    eprintln!("owner follow: {problem}");
                                }
                            }
                            Err(error) => eprintln!("owner follow pass failed: {error:#}"),
                        }
                    });
                    pass = pass.wrapping_add(1);
                    thread::sleep(FOLLOW_DELIVERY_INTERVAL);
                }
            });
            // Fills Finished rows whose agent wrote nothing after `sm task-complete`
            // and drops expired ones (spec 1782 D2).
            let turns = sm_server::turn_messages::TurnMessageStore::new(expand_home(
                &state.config().sm_send.db_path,
            ));
            let finished_shutdown = shutdown.clone();
            thread::spawn(move || loop {
                thread::sleep(FINISHED_SWEEP_INTERVAL);
                if finished_shutdown.is_stopped() {
                    break;
                }
                run_background_pass("finished row sweep", || {
                    if let Err(error) = turns.sweep(time::OffsetDateTime::now_utc()) {
                        eprintln!("finished row sweep failed: {error:#}");
                    }
                });
            });
            let queue_delivery_state = state.clone();
            let queue_delivery_shutdown = shutdown.clone();
            thread::spawn(move || loop {
                if queue_delivery_shutdown.is_stopped() {
                    break;
                }
                run_background_pass("background wake delivery retry", || {
                    if let Err(error) = queue_delivery_state.drain_background_retry_wakes() {
                        eprintln!("background wake delivery retry failed: {error:#}");
                    }
                });
                thread::sleep(QUEUE_COMPLETION_RETRY_INTERVAL);
            });
        }
        let reconciliation_state = state.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) =
                reconciliation_state.reconcile_seat_sessions(reconciliation_snapshot)
            {
                eprintln!("usage ledger session reconciliation failed: {error:#}");
            }
        });

        if let Some(poller) = poller {
            let poll_interval = state.config().usage.poll_interval_secs.max(1);
            let scan_interval = state.config().usage.scan_interval_secs.max(1);
            let identity_poller = poller.clone();
            let identity_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(poll_interval));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    if identity_shutdown.is_stopped() {
                        break;
                    }
                    let poller = identity_poller.clone();
                    match tokio::task::spawn_blocking(move || {
                        poller.poll_once(time::OffsetDateTime::now_utc())
                    })
                    .await
                    {
                        Ok(errors) => {
                            for (provider, error) in errors {
                                eprintln!(
                                    "{} account identity poll failed: {error:#}",
                                    provider.as_str()
                                );
                            }
                        }
                        Err(error) => eprintln!("account identity poll task failed: {error}"),
                    }
                }
            });
            let usage_state = state.clone();
            let scan_poller = poller;
            let usage_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(scan_interval));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    if usage_shutdown.is_stopped() {
                        break;
                    }
                    let scan_state = usage_state.clone();
                    let poller = scan_poller.clone();
                    match tokio::task::spawn_blocking(move || {
                        let identity_errors = poller.poll_once(time::OffsetDateTime::now_utc());
                        let scan = scan_state.scan_usage_ledger();
                        (identity_errors, scan)
                    })
                    .await
                    {
                        Ok((identity_errors, scan)) => {
                            for (provider, error) in identity_errors {
                                eprintln!(
                                    "{} account identity pre-scan poll failed: {error:#}",
                                    provider.as_str()
                                );
                            }
                            if let Err(error) = scan {
                                eprintln!("usage token ledger scan failed: {error:#}");
                            }
                        }
                        Err(error) => eprintln!("usage token ledger task failed: {error}"),
                    }
                }
            });
            // Turns and tool spans for Analytics › Time, on the usage scan's cadence but its own
            // task and database, so neither scan waits on the other (sm#1676).
            let recorder = Arc::new(std::sync::Mutex::new(ActivityRecorder::new(
                expand_home(&state.config().activity.db_path),
                expand_home(&state.config().usage.db_path),
            )));
            let activity_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(scan_interval));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    if activity_shutdown.is_stopped() {
                        break;
                    }
                    let recorder = recorder.clone();
                    match tokio::task::spawn_blocking(move || {
                        recorder
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .scan()
                    })
                    .await
                    {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => eprintln!("activity recorder scan failed: {error:#}"),
                        Err(error) => eprintln!("activity recorder task failed: {error}"),
                    }
                }
            });
        }

        let plain_state = state.clone();
        let plain_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if plain_shutdown.is_stopped() {
                    break;
                }
                let state = plain_state.clone();
                match tokio::task::spawn_blocking(move || state.reconcile_plain_terminals()).await {
                    Ok(Ok(())) => {}
                    result => eprintln!("plain terminal reconciliation: {result:?}"),
                }
            }
        });

        // Log accounting and revocation must keep running while launchctl is slow.
        let ssh_observer = state.clone();
        let ssh_observer_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if ssh_observer_shutdown.is_stopped() {
                    break;
                }
                let state = ssh_observer.clone();
                match tokio::task::spawn_blocking(move || state.observe_shell_ssh()).await {
                    Ok(Ok(())) => {}
                    result => eprintln!("SSH log observer: {result:?}"),
                }
            }
        });

        let studio_state = state.clone();
        let studio_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(STUDIO_SSH_RECONCILE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if studio_shutdown.is_stopped() {
                    break;
                }
                let state = studio_state.clone();
                match tokio::task::spawn_blocking(move || state.reconcile_shell_ssh()).await {
                    Ok(Ok(())) => {}
                    result => eprintln!("shell SSH reconciliation: {result:?}"),
                }
            }
        });

        let serving_listener = listener.try_clone()?;
        serving_listener.set_nonblocking(true)?;
        let serving_listener = TcpListener::from_std(serving_listener)?;
        let mut stopped = shutdown.subscribe();
        let mut server_task = tokio::spawn(async move {
            axum::serve(
                serving_listener,
                router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = stopped.changed().await;
            })
            .await
        });
        eprintln!("sm-server listening on http://{address}");

        if let Some(stream) = takeover_stream.as_mut() {
            let ready = tokio::time::timeout(Duration::from_secs(4), async {
                loop {
                    if server_task.is_finished() {
                        anyhow::bail!("replacement HTTP server stopped");
                    }
                    if tokio::task::spawn_blocking(move || handover::probe_health(address))
                        .await?
                        .is_ok()
                        && (!expected_lan || lan_control.is_running())
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await;
            ready.context("replacement did not become healthy in four seconds")??;
            write_active_slot(handover_dir)?;
            handover::send_serving(stream)?;
        } else if std::env::var("XPC_SERVICE_NAME")
            .is_ok_and(|label| label.ends_with(".blue") || label.ends_with(".green"))
        {
            let ready = tokio::time::timeout(Duration::from_secs(4), async {
                loop {
                    if server_task.is_finished() {
                        anyhow::bail!("HTTP server stopped before becoming healthy");
                    }
                    if tokio::task::spawn_blocking(move || handover::probe_health(address))
                        .await?
                        .is_ok()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await;
            ready.context("slot did not become healthy in four seconds")??;
            write_active_slot(handover_dir)?;
        }
        let mut decision_rx = takeover_stream.take().map(|mut stream| {
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            tokio::task::spawn_blocking(move || {
                let decision =
                    handover::read_decision(&mut stream).unwrap_or(handover::Decision::Rollback);
                let _ = sender.blocking_send((decision, stream));
            });
            receiver
        });

        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut lan_stability = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = lan_stability.tick(), if expected_lan && decision_rx.is_some() => {
                    if !lan_control.is_running() {
                        shutdown.stop();
                        let _ = tokio::time::timeout(Duration::from_secs(10), &mut server_task).await;
                        return Err(anyhow::anyhow!("inherited terminal LAN listener stopped during handover"));
                    }
                }
                decision = async {
                    match decision_rx.as_mut() {
                        Some(receiver) => receiver.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    decision_rx = None;
                    match decision {
                        Some((handover::Decision::Commit | handover::Decision::PeerExited, _connection)) => {
                            authority_server.claim_socket();
                        }
                        Some((handover::Decision::Rollback, _connection)) => {
                            authority_server.keep_socket_for_successor();
                            shutdown.stop();
                            let _ = lan_control.pause().await;
                            let _ = tokio::time::timeout(Duration::from_secs(10), &mut server_task).await;
                            return Err(anyhow::anyhow!("serving slot rolled back handover"));
                        }
                        None => {}
                    }
                }
                request = handover_rx.recv() => {
                let Some(mut stream) = request else { anyhow::bail!("handover acceptor stopped"); };
                let replacement_pid = match handover::peer_pid(&stream) {
                    Ok(pid) => pid,
                    Err(error) => {
                        eprintln!("handover refused: replacement identity unavailable: {error:#}");
                        continue;
                    }
                };
                if !btw_workers.is_empty() {
                    eprintln!("handover refused: /btw worker is active");
                    drop(stream);
                    continue;
                }
                resuming_old_slot = true;
                shutdown.stop();
                let (lan_result, drain_result) = tokio::join!(
                    lan_control.pause(),
                    tokio::time::timeout(Duration::from_secs(10), &mut server_task),
                );
                let lan_listener = match lan_result {
                    Ok(listener) => listener,
                    Err(error) => {
                        eprintln!("handover LAN pause failed, resuming old slot: {error:#}");
                        lan_control.stop().await;
                        drop(authority_thread);
                        let _ = handover_acceptor.join();
                        continue 'generations;
                    }
                };
                if drain_result.is_err() {
                    server_task.abort();
                }
                    // An existing authority request is read-only; do not let a
                    // stalled client hold the HTTP listener out of service.
                    drop(authority_thread);
                    let _ = handover_acceptor.join();
                    if !btw_workers.is_empty() {
                        eprintln!("handover refused: /btw worker started during request drain");
                        drop(stream);
                        inherited_lan = lan_listener;
                        continue 'generations;
                    }
                    let mut fds = vec![listener.as_raw_fd(), authority_server.listener_fd(), handover_listener.as_raw_fd()];
                    if let Some(lan) = &lan_listener { fds.push(lan.as_raw_fd()); }
                    let result = match handover::send_listeners(&stream, &fds, lan_listener.is_some()) {
                        Ok(()) => {
                            let (returned_stream, result) = tokio::task::spawn_blocking(move || {
                                let result = handover::await_stable_serving(&mut stream, address);
                                (stream, result)
                            }).await?;
                            stream = returned_stream;
                            result
                        },
                        Err(error) => Err(error),
                    };
                    if result.is_ok() {
                        authority_server.keep_socket_for_successor();
                        if let Err(error) = handover::send_decision(&mut stream, handover::Decision::Commit) {
                            authority_server.claim_socket();
                            eprintln!("handover commit failed: {error:#}");
                        } else {
                            // The listener and all shared work now belong to the
                            // replacement. Runtime teardown would wait forever
                            // for legacy blocking monitor threads, so exit now.
                            std::process::exit(0);
                        }
                    }
                    if let Err(error) = result {
                        eprintln!("handover rolled back: {error:#}");
                    }
                    btw_workers.block_recovery();
                    let _ = handover::send_decision(&mut stream, handover::Decision::Rollback);
                    let recovery_workers = btw_workers.clone();
                    thread::spawn(move || {
                        if let Err(error) = handover::wait_peer_exit(replacement_pid) {
                            eprintln!("replacement exit watch failed; /btw recovery remains parked: {error:#}");
                            return;
                        }
                        recovery_workers.allow_recovery();
                    });
                    inherited_lan = lan_listener;
                    continue 'generations;
                }
            _ = sigterm.recv() => {
                shutdown.stop();
                let _ = tokio::join!(
                    lan_control.pause(),
                    tokio::time::timeout(Duration::from_secs(10), &mut server_task),
                );
                    let _ = authority_thread.join();
                    let _ = handover_acceptor.join();
                    return Ok(());
                }
                result = &mut server_task => {
                    return result?.context("HTTP server stopped unexpectedly");
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct InitializationRetry {
    timeout: Duration,
    initial_delay: Duration,
    max_delay: Duration,
}

const ROLLBACK_INITIALIZATION_RETRY: InitializationRetry = InitializationRetry {
    timeout: Duration::from_secs(5 * 60),
    initial_delay: Duration::from_secs(1),
    max_delay: Duration::from_secs(10),
};

/// Cold starts (including replacements) fail immediately. Only a process that
/// previously served may wait for shared state to become usable again.
async fn initialize_with_retry<T>(
    retry: Option<InitializationRetry>,
    mut initialize: impl FnMut() -> Result<T>,
) -> Result<T> {
    let started = tokio::time::Instant::now();
    let mut delay = retry.map_or(Duration::ZERO, |policy| policy.initial_delay);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match initialize() {
            Ok(state) => {
                if attempt > 1 {
                    eprintln!(
                        "rollback initialization recovered after {attempt} attempts ({:?})",
                        started.elapsed()
                    );
                }
                return Ok(state);
            }
            Err(error) => {
                let Some(policy) = retry else {
                    return Err(error);
                };
                let remaining = policy.timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    eprintln!("rollback initialization attempt {attempt} failed; retry limit reached: {error:#}");
                    return Err(error).context("rollback initialization retry limit reached");
                }
                let wait = delay.min(remaining);
                eprintln!("rollback initialization attempt {attempt} failed; retrying in {wait:?}: {error:#}");
                tokio::time::sleep(wait).await;
                // Do not start another potentially slow operation at the deadline.
                if started.elapsed() >= policy.timeout {
                    return Err(error).context("rollback initialization retry limit reached");
                }
                delay = delay.saturating_mul(2).min(policy.max_delay);
            }
        }
    }
}

struct InitializedGeneration {
    state: AppState,
    reconciliation_snapshot: sm_server::sessions::SeatSessionReconciliationSnapshot,
    poller: Option<Arc<IdentityPoller>>,
    local_model: Option<Arc<sm_server::local_model::ModelHost>>,
    #[cfg(target_os = "macos")]
    local_walls: Option<Arc<sm_server::local_wall::recovery::GenerationWalls>>,
}

fn initialize_generation(
    config: &AppConfig,
    port: u16,
    queue_state_dir: &std::path::Path,
    shutdown: &Shutdown,
    btw_workers: &BtwWorkers,
) -> Result<InitializedGeneration> {
    #[cfg(target_os = "macos")]
    let local_walls = if config.rust_core.runtime_enabled {
        let directory = sm_server::config::test_isolation_root_from_environment()?
            .map(|root| root.join("local-egress"))
            .unwrap_or_else(|| expand_home("~/.local/share/claude-sessions/local-egress"));
        let walls = Arc::new(sm_server::local_wall::recovery::GenerationWalls::new(
            queue_state_dir.to_path_buf(),
            expand_home(&config.local_judge.python),
            SocketAddr::from(([127, 0, 0, 1], port)),
            &config.local_host.base_url,
            sm_server::local_egress::ServiceClient::new(directory, std::env::current_exe()?),
            sm_server::local_judge::LocalJudgeRuntime::from_config(config),
        )?);
        Some(walls)
    } else {
        None
    };

    let state = AppState::try_new(config.clone())
        .context("failed to initialize server state")?
        .with_listen_port(port)
        .with_shutdown(shutdown.clone())
        .with_btw_workers(btw_workers.clone());
    #[cfg(target_os = "macos")]
    let state = if let Some(walls) = &local_walls {
        state.with_local_walls(walls.clone())
    } else {
        state
    };
    let local_model = if config.rust_core.runtime_enabled {
        state.register_local_model()?;
        Some(
            sm_server::local_model::live(queue_state_dir)
                .context("local model registration missing")?,
        )
    } else {
        None
    };
    // Capture the artifact boundary before serving. The background scan may run
    // alongside live creates, but it must not attribute their new artifacts
    // against this startup snapshot of the seat registry.
    let reconciliation_cutoff_ns = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    let reconciliation_snapshot = state
        .prepare_seat_session_reconciliation(reconciliation_cutoff_ns)
        .context("failed to snapshot sessions for usage ledger reconciliation")?;

    let poller = if state.config().usage.enabled {
        Some(Arc::new(IdentityPoller::new(
            expand_home(&state.config().usage.db_path),
            expand_home("~/.claude.json"),
            expand_home("~/.codex/auth.json"),
        )?))
    } else {
        None
    };
    Ok(InitializedGeneration {
        state,
        reconciliation_snapshot,
        poller,
        local_model,
        #[cfg(target_os = "macos")]
        local_walls,
    })
}

fn write_active_slot(state_dir: &std::path::Path) -> Result<()> {
    let label = std::env::var("XPC_SERVICE_NAME").context("missing launchd service label")?;
    let slot = if label.ends_with(".blue") {
        "blue"
    } else if label.ends_with(".green") {
        "green"
    } else {
        anyhow::bail!("launchd service is not a blue or green slot");
    };
    let path = state_dir.join("active-slot");
    let temporary = state_dir.join(format!("active-slot.{}.tmp", std::process::id()));
    std::fs::write(&temporary, format!("{slot}\n"))?;
    std::fs::rename(&temporary, &path)?;
    Ok(())
}

/// Raise this process's RLIMIT_NOFILE soft limit toward
/// OPEN_FILE_SOFT_LIMIT_TARGET, never above the hard limit and never lowering
/// it. Children inherit the result. A failure only leaves the old limit.
fn raise_open_file_soft_limit() {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};
    let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) else {
        return;
    };
    let Some(raised) = raised_open_file_soft_limit(soft, hard, OPEN_FILE_SOFT_LIMIT_TARGET) else {
        return;
    };
    match setrlimit(Resource::RLIMIT_NOFILE, raised, hard) {
        Ok(()) => eprintln!("sm-server open-file soft limit raised from {soft} to {raised}"),
        Err(error) => {
            eprintln!("sm-server could not raise open-file soft limit from {soft}: {error}")
        }
    }
}

/// The soft limit to set, or None when the current one already meets the
/// target. RLIM_INFINITY compares as the largest value, so an unlimited soft
/// limit is left alone and an unlimited hard limit does not cap the target.
fn raised_open_file_soft_limit(soft: u64, hard: u64, target: u64) -> Option<u64> {
    let raised = target.min(hard);
    (raised > soft).then_some(raised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn rollback_initialization_failure_stops_at_five_minutes() {
        let started = tokio::time::Instant::now();
        let mut attempts = Vec::new();
        let result: Result<()> = initialize_with_retry(Some(ROLLBACK_INITIALIZATION_RETRY), || {
            attempts.push(started.elapsed().as_secs());
            anyhow::bail!("persistent shared-state failure")
        })
        .await;
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("rollback initialization retry limit reached"));
        assert!(error.contains("persistent shared-state failure"));
        assert_eq!(started.elapsed(), Duration::from_secs(300));
        assert_eq!(&attempts[..6], &[0, 1, 3, 7, 15, 25]);
        assert_eq!(attempts.last(), Some(&295));
        assert!(attempts.windows(2).all(|pair| pair[1] - pair[0] <= 10));
    }

    #[tokio::test(start_paused = true)]
    async fn cold_start_and_replacement_initialization_fail_without_retry() {
        let started = tokio::time::Instant::now();
        let mut attempts = 0;
        let result: Result<()> = initialize_with_retry(None, || {
            attempts += 1;
            anyhow::bail!("cold startup failure")
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), "cold startup failure");
        assert_eq!(attempts, 1);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    // This exercises real AppState recovery and HTTP, without launchd or live
    // stores. The listener survives stopping the old generation and two failed
    // initializations, then answers a request queued during the outage.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rollback_initialization_recovers_on_retained_listener() {
        use std::{
            fs,
            io::{Read, Write},
            net::TcpStream,
        };

        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let root = sm_server::config::test_isolation_root_from_environment()
            .unwrap()
            .expect("run through scripts/test-rust-isolated.sh")
            .join("rollback-initialization");
        fs::create_dir_all(&root).unwrap();
        let _fixture = Fixture(root.clone());
        let mut config = AppConfig::default();
        config.paths.state_file = root.join("sessions.json").to_string_lossy().into_owned();
        config.rust_core.runtime_enabled = false;
        config.usage.enabled = false;
        config.terminal_direct.lan.enabled = false;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let btw_workers = BtwWorkers::default();

        fn serve(
            listener: &std::net::TcpListener,
            state: AppState,
            shutdown: &Shutdown,
        ) -> tokio::task::JoinHandle<std::io::Result<()>> {
            let serving = TcpListener::from_std(listener.try_clone().unwrap()).unwrap();
            let mut stopped = shutdown.subscribe();
            tokio::spawn(async move {
                axum::serve(
                    serving,
                    router(state).into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let _ = stopped.changed().await;
                })
                .await
            })
        }

        let old_shutdown = Shutdown::default();
        let old =
            initialize_generation(&config, address.port(), &root, &old_shutdown, &btw_workers)
                .unwrap();
        let old_server = serve(&listener, old.state, &old_shutdown);
        tokio::task::spawn_blocking(move || handover::probe_health(address))
            .await
            .unwrap()
            .unwrap();
        old_shutdown.stop();
        old_server.await.unwrap().unwrap();

        // The replacement left state temporarily unreadable. Both retry failures
        // must come from real initialization, rather than a synthetic error.
        let state_file = PathBuf::from(&config.paths.state_file);
        fs::write(&state_file, "invalid session state").unwrap();
        let mut queued = TcpStream::connect(address).unwrap();
        queued
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        queued
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let response = tokio::task::spawn_blocking(move || {
            let mut response = String::new();
            queued.read_to_string(&mut response).unwrap();
            response
        });
        let shutdown = Shutdown::default();
        let mut attempts = 0;
        let recovered = initialize_with_retry(
            Some(InitializationRetry {
                timeout: Duration::from_secs(2),
                initial_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(20),
            }),
            || {
                attempts += 1;
                let bind_error = std::net::TcpListener::bind(address).unwrap_err();
                assert_eq!(bind_error.kind(), std::io::ErrorKind::AddrInUse);
                if attempts == 3 {
                    fs::remove_file(&state_file).unwrap();
                }
                let result =
                    initialize_generation(&config, address.port(), &root, &shutdown, &btw_workers);
                if attempts < 3 {
                    assert!(
                        result.is_err(),
                        "corrupt session state must fail initialization"
                    );
                }
                result
            },
        )
        .await
        .unwrap();
        assert_eq!(attempts, 3);
        let server = serve(&listener, recovered.state, &shutdown);
        assert!(response.await.unwrap().starts_with("HTTP/1.1 200"));
        tokio::task::spawn_blocking(move || handover::probe_health(address))
            .await
            .unwrap()
            .unwrap();
        shutdown.stop();
        server.await.unwrap().unwrap();
    }

    const UNLIMITED: u64 = nix::libc::RLIM_INFINITY;

    #[test]
    fn raises_launchd_default_to_target_under_unlimited_hard_limit() {
        assert_eq!(
            raised_open_file_soft_limit(256, UNLIMITED, 8192),
            Some(8192)
        );
    }

    #[test]
    fn caps_raise_at_hard_limit() {
        assert_eq!(raised_open_file_soft_limit(256, 4096, 8192), Some(4096));
    }

    #[test]
    fn never_lowers_a_higher_or_unlimited_soft_limit() {
        assert_eq!(raised_open_file_soft_limit(8192, UNLIMITED, 8192), None);
        assert_eq!(
            raised_open_file_soft_limit(1_048_576, UNLIMITED, 8192),
            None
        );
        assert_eq!(
            raised_open_file_soft_limit(UNLIMITED, UNLIMITED, 8192),
            None
        );
        assert_eq!(raised_open_file_soft_limit(256, 256, 8192), None);
    }
}
