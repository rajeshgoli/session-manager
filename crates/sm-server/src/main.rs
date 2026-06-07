use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use sm_server::{
    config::AppConfig,
    http::{router, AppState},
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

    eprintln!("sm-server listening on http://{address}");
    axum::serve(
        listener,
        router(AppState::new(config)).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
