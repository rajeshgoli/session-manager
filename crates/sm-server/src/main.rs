use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use sm_server::{
    config::AppConfig,
    http::{router, AppState},
    queue::{QueueAdmissionPolicy, QueueRecoverySummary, RetainedQueueStore},
    queue_authority::{QueueAuthorityServer, QueueAuthorityServiceIdentity},
    sessions::expand_home,
    studio_ssh,
    usage_identity::IdentityPoller,
};
use tokio::net::TcpListener;

/// How often the Studio SSH reconcile loop repairs toward the desired state.
const STUDIO_SSH_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const QUEUE_COMPLETION_RETRY_INTERVAL: Duration = Duration::from_secs(5);

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
}

#[tokio::main]
async fn main() -> Result<()> {
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
        return Ok(());
    }
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind {address}"))?;

    let queue_state_dir_config = config.queue_runner_state_dir();
    let queue_state_dir = expand_home(&queue_state_dir_config.to_string_lossy());
    let authority_server =
        QueueAuthorityServer::bind(&queue_state_dir, QueueAuthorityServiceIdentity::current()?)?;
    eprintln!(
        "sm-server queue authority on {}",
        authority_server.socket_path().display()
    );
    authority_server.spawn();

    if config.rust_core.runtime_enabled {
        let message_queue_db_path = expand_home(&config.sm_send.db_path);
        let cancel_grace_seconds = config.queue_runner.cancel_grace_seconds;
        let admission_policy = QueueAdmissionPolicy {
            max_running_jobs: config.queue_runner.max_running_jobs,
            perf_cooldown_seconds: config.queue_runner.perf_cooldown_seconds,
            tests_max_concurrent: config.queue_runner.types.tests.max_concurrent,
            perf_max_concurrent: config.queue_runner.types.perf.max_concurrent,
            background_max_concurrent: config.queue_runner.types.background.max_concurrent,
        };
        thread::spawn(move || {
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
            loop {
                thread::sleep(QUEUE_COMPLETION_RETRY_INTERVAL);
                if let Err(error) =
                    RetainedQueueStore::retry_unnotified_queue_job_completions_in_state_dir(
                        &queue_state_dir,
                        &message_queue_db_path,
                    )
                {
                    eprintln!("queue completion wake retry failed: {error:#}");
                }
            }
        });
    }

    let state = AppState::try_new(config).context("failed to initialize server state")?;
    if state.config().rust_core.runtime_enabled {
        let queue_delivery_state = state.clone();
        thread::spawn(move || loop {
            if let Err(error) = queue_delivery_state.drain_queue_completion_wakes() {
                eprintln!("queue completion delivery retry failed: {error:#}");
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
        if let Err(error) = reconciliation_state.reconcile_seat_sessions(reconciliation_snapshot) {
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
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(poll_interval));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
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
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(scan_interval));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
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
    }

    // Repair the Studio SSH LaunchAgents toward the desired state every 30s while
    // the toggle is on. launchctl is synchronous, so run it on a blocking thread.
    let studio_ssh_flag = state.studio_ssh_enabled_flag();
    let studio_ssh_config = state.config().external_access.studio_ssh.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(STUDIO_SSH_RECONCILE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            // Drive toward the desired state in BOTH directions so "off" is
            // enforced too (a stray enable that raced a disable gets corrected).
            let desired = studio_ssh_flag.load(Ordering::SeqCst);
            let config = studio_ssh_config.clone();
            match tokio::task::spawn_blocking(move || studio_ssh::reconcile(&config, desired)).await
            {
                Ok(status) if status.status == "error" => {
                    eprintln!("studio-ssh reconcile error: {:?}", status.error);
                }
                Ok(_) => {}
                Err(error) => eprintln!("studio-ssh reconcile task failed: {error}"),
            }
        }
    });

    eprintln!("sm-server listening on http://{address}");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
