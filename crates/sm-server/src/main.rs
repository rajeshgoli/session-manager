use std::{net::SocketAddr, path::PathBuf, thread};

use anyhow::{Context, Result};
use clap::Parser;
use sm_server::{
    config::AppConfig,
    http::{router, AppState},
    queue::{QueueAdmissionPolicy, QueueRecoverySummary, RetainedQueueStore},
    sessions::expand_home,
};
use tokio::net::TcpListener;

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

    eprintln!("sm-server listening on http://{address}");
    axum::serve(
        listener,
        router(AppState::new(config)).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
