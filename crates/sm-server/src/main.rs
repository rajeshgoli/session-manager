use std::{net::SocketAddr, path::PathBuf, sync::atomic::Ordering, thread, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use sm_server::{
    config::AppConfig,
    http::{router, AppState},
    queue::{QueueAdmissionPolicy, QueueRecoverySummary, RetainedQueueStore},
    sessions::expand_home,
    studio_ssh,
};
use tokio::net::TcpListener;

/// How often the Studio SSH reconcile loop repairs toward the desired state.
const STUDIO_SSH_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = AppConfig::load_from_path_with_local_env(&args.config, args.local_env.as_deref())?;
    let address: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .with_context(|| format!("invalid listen address {}:{}", args.host, args.port))?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind {address}"))?;

    if config.rust_core.runtime_enabled {
        let queue_state_dir_config = config.queue_runner_state_dir();
        let queue_state_dir = expand_home(&queue_state_dir_config.to_string_lossy());
        let message_queue_db_path = expand_home(&config.sm_send.db_path);
        let cancel_grace_seconds = config.queue_runner.cancel_grace_seconds;
        let admission_policy = QueueAdmissionPolicy {
            max_running_jobs: config.queue_runner.max_running_jobs,
            perf_cooldown_seconds: config.queue_runner.perf_cooldown_seconds,
        };
        thread::spawn(
            move || match RetainedQueueStore::recover_queue_jobs_in_state_dir_with_policy(
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
            },
        );
    }

    let state = AppState::new(config);

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
            match tokio::task::spawn_blocking(move || studio_ssh::reconcile(&config, desired)).await {
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
