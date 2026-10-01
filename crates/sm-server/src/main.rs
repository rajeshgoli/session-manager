use std::{
    net::SocketAddr,
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use sm_server::{
    activity_ledger::ActivityRecorder,
    config::AppConfig,
    handover::{self, Shutdown},
    http::{router, AppState, BtwWorkers},
    owner_settings,
    queue::{QueueRecoverySummary, RetainedQueueStore},
    queue_authority::{QueueAuthorityServer, QueueAuthorityServiceIdentity},
    sessions::{expand_home, SessionStore},
    studio_ssh, terminal_lan,
    usage_identity::IdentityPoller,
};
use tokio::net::TcpListener;

/// How often the Studio SSH reconcile loop repairs toward the desired state.
const STUDIO_SSH_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const QUEUE_COMPLETION_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const REPARENT_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
/// Owner follows deliver every 5s and sweep for finished targets every
/// third pass (sm#1569).
const FOLLOW_DELIVERY_INTERVAL: Duration = Duration::from_secs(5);
const FOLLOW_SWEEP_EVERY_PASSES: u64 = 3;
/// How often Finished rows are checked for the 10-minute text fallback.
const FINISHED_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
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
    /// Load and validate the configuration, then exit without binding a port or
    /// touching any state. scripts/restart-rust-server.sh uses this to reject a
    /// bad config while the old server is still running, rather than discovering
    /// it after the service has been stopped.
    #[arg(long)]
    check_config: bool,
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
    let (listener, authority_server, handover_listener, mut takeover_stream, mut inherited_lan) =
        if args.take_over {
            let mut stream = UnixStream::connect(handover::socket_path(handover_dir))
                .context("cannot connect to serving slot for handover")?;
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
    'generations: loop {
        let shutdown = Shutdown::default();
        sm_server::queue::set_live_queue_shutdown(shutdown.clone());
        let authority_thread = authority_server.spawn(shutdown.clone())?;
        let (handover_tx, mut handover_rx) = tokio::sync::mpsc::channel(1);
        let handover_acceptor =
            handover::spawn_acceptor(&handover_listener, shutdown.clone(), handover_tx)?;

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
            match RetainedQueueStore::recover_queue_jobs_in_state_dir_with_policy(
                &queue_state_dir,
                &message_queue_db_path,
                cancel_grace_seconds,
                admission_policy,
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
        }

        let state = AppState::try_new(config.clone())
            .context("failed to initialize server state")?
            .with_listen_port(args.port)
            .with_shutdown(shutdown.clone())
            .with_btw_workers(btw_workers.clone());
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
            if let Err(error) = reparent_state.reconcile_reparent_background() {
                eprintln!("reparent background reconciliation failed: {error:#}");
            }
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
                    match follow_state.run_follow_pass(pass % FOLLOW_SWEEP_EVERY_PASSES == 0) {
                        Ok(problems) => {
                            for problem in problems {
                                eprintln!("owner follow: {problem}");
                            }
                        }
                        Err(error) => eprintln!("owner follow pass failed: {error:#}"),
                    }
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
                if let Err(error) = turns.sweep(time::OffsetDateTime::now_utc()) {
                    eprintln!("finished row sweep failed: {error:#}");
                }
            });
            let queue_delivery_state = state.clone();
            let queue_delivery_shutdown = shutdown.clone();
            thread::spawn(move || loop {
                if queue_delivery_shutdown.is_stopped() {
                    break;
                }
                if let Err(error) = queue_delivery_state.drain_background_retry_wakes() {
                    eprintln!("background wake delivery retry failed: {error:#}");
                }
                thread::sleep(QUEUE_COMPLETION_RETRY_INTERVAL);
            });
        }
        // Capture the artifact boundary before serving. The background scan may run
        // alongside live creates, but it must not attribute their new artifacts
        // against this startup snapshot of the seat registry.
        let reconciliation_cutoff_ns = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        let reconciliation_snapshot = state
            .prepare_seat_session_reconciliation(reconciliation_cutoff_ns)
            .context("failed to snapshot sessions for usage ledger reconciliation")?;
        let reconciliation_state = state.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) =
                reconciliation_state.reconcile_seat_sessions(reconciliation_snapshot)
            {
                eprintln!("usage ledger session reconciliation failed: {error:#}");
            }
        });

        if state.config().usage.enabled {
            let poll_interval = state.config().usage.poll_interval_secs.max(1);
            let scan_interval = state.config().usage.scan_interval_secs.max(1);
            let poller = Arc::new(IdentityPoller::new(
                expand_home(&state.config().usage.db_path),
                expand_home("~/.claude.json"),
                expand_home("~/.codex/auth.json"),
            )?);
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

        // Repair the Studio SSH LaunchAgents toward the desired state every 30s while
        // the toggle is on. launchctl is synchronous, so run it on a blocking thread.
        let studio_ssh_flag = state.studio_ssh_enabled_flag();
        let studio_ssh_config = state.config().external_access.studio_ssh.clone();
        let studio_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(STUDIO_SSH_RECONCILE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if studio_shutdown.is_stopped() {
                    break;
                }
                // Drive toward the desired state in BOTH directions so "off" is
                // enforced too (a stray enable that raced a disable gets corrected).
                let desired = studio_ssh_flag.load(Ordering::SeqCst);
                let config = studio_ssh_config.clone();
                match tokio::task::spawn_blocking(move || studio_ssh::reconcile(&config, desired))
                    .await
                {
                    Ok(status) if status.status == "error" => {
                        eprintln!("studio-ssh reconcile error: {:?}", status.error);
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("studio-ssh reconcile task failed: {error}"),
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
        loop {
            tokio::select! {
                decision = async {
                    match decision_rx.as_mut() {
                        Some(receiver) => receiver.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    decision_rx = None;
                    match decision {
                        Some((handover::Decision::Commit | handover::Decision::PeerExited, _connection)) => authority_server.claim_socket(),
                        Some((handover::Decision::Rollback, _connection)) => {
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
                        write_active_slot(handover_dir)?;
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
                            return Ok(());
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
                    write_active_slot(handover_dir)?;
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
    use super::raised_open_file_soft_limit;

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
