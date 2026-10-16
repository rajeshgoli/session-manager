use super::*;
use anyhow::ensure;
use sm_server::managed_artifacts::{
    backup::{
        client::Destination,
        pull::{self, HttpRemote, Remote},
        seed,
    },
    ManagedArtifacts,
};
#[derive(Args)]
pub(super) struct BackupArgs {
    #[command(subcommand)]
    command: BackupCommand,
}
#[derive(Subcommand)]
enum BackupCommand {
    Pull {
        #[arg(long)]
        dest: PathBuf,
        #[arg(long)]
        check: bool,
        #[arg(long, default_value = "~/.config/session-manager/device")]
        identity: PathBuf,
    },
    Seed {
        #[arg(long)]
        device: String,
        #[arg(long)]
        dest: PathBuf,
        #[arg(long)]
        adopt_plain: Option<PathBuf>,
        #[arg(long, default_value = "~/.config/session-manager/config.yaml")]
        config: PathBuf,
    },
    Restore {
        #[arg(long)]
        from: PathBuf,
        #[arg(long)]
        to: PathBuf,
        #[arg(long)]
        as_of: Option<String>,
        paths: Vec<String>,
    },
    AllowDeletes {
        #[arg(long)]
        device: Option<String>,
    },
}
fn expanded(path: &Path) -> PathBuf {
    sm_server::sessions::expand_home(&path.to_string_lossy())
}
pub(super) fn run(args: BackupArgs, api_url: Option<String>) -> Result<()> {
    match args.command {
        BackupCommand::Restore {
            from,
            to,
            as_of,
            paths,
        } => {
            let at = as_of
                .map(|date| -> Result<i64> {
                    let format = time::format_description::parse("[year]-[month]-[day]")?;
                    Ok(time::Date::parse(&date, &format)?
                        .midnight()
                        .assume_utc()
                        .unix_timestamp())
                })
                .transpose()?;
            let dest = Destination::open(&expanded(&from))?;
            println!(
                "Restored {} paths.",
                dest.restore(&expanded(&to), at, &paths)?
            );
        }
        BackupCommand::Seed {
            device,
            dest,
            adopt_plain,
            config,
        } => {
            let config = AppConfig::load_from_path(expanded(&config))?;
            let manager = ManagedArtifacts::new(&config.managed_artifacts)?;
            let mut destination = Destination::open(&expanded(&dest))?;
            let adopt = adopt_plain.as_ref().map(|p| expanded(p));
            let id = seed::seed(&manager, &device, &mut destination, adopt.as_deref())?;
            println!("Seed verified and confirmed: {id}");
        }
        BackupCommand::Pull {
            dest,
            check,
            identity,
        } => {
            let url = resolve_api_url(api_url)?;
            let identity = expanded(&identity);
            let mut remote = HttpRemote::new(&url, &identity)?;
            let mut destination = Destination::open(&expanded(&dest))?;
            if check {
                let (status, body) = remote.json("GET", "/backup/status", None)?;
                ensure!(
                    (200..300).contains(&status),
                    "backup hostname rejected this device: {status} {body}"
                );
                let metadata: Value = serde_json::from_slice(
                    &fs::read(identity.join("identity.json"))
                        .context("device identity metadata missing; enroll with --headless")?,
                )?;
                let disk = nix::sys::statvfs::statvfs(&destination.root)?;
                println!("Device certificate accepted by {url}");
                println!(
                    "Certificate expiry: {}",
                    metadata["expires_at"].as_str().unwrap_or("unknown")
                );
                println!(
                    "Last confirmed manifest: {}",
                    destination.state()?.confirmed.as_deref().unwrap_or("none")
                );
                println!(
                    "Destination free bytes: {}",
                    u64::from(disk.blocks_available()) * disk.fragment_size()
                );
                println!("Speed probe: {}", remote.probe()?);
            } else {
                let ack = pull::pull(&mut destination, &mut remote)?;
                println!(
                    "Backup confirmed; fetched {} bytes; held {} deletions.",
                    ack.fetched_bytes, ack.held_deletions
                );
            }
        }
        BackupCommand::AllowDeletes { device } => {
            let url = resolve_api_url(api_url)?;
            let local = ApiClient::parse(&url)?;
            let url = if local.scheme == "http"
                && matches!(local.host.as_str(), "localhost" | "127.0.0.1" | "::1")
            {
                let config = AppConfig::load_from_path(expanded(Path::new(
                    "~/.config/session-manager/config.yaml",
                )))?;
                format!(
                    "https://{}",
                    config
                        .cloudflare_access
                        .browser
                        .hostname
                        .context("browser hostname is not configured")?
                )
            } else {
                url
            };
            let client = ApiClient::parse(&url)?;
            ensure!(
                client.scheme == "https",
                "deletion approval requires the owner browser hostname"
            );
            let assertion = device::owner_assertion(&url)?;
            let cookie = format!("CF_Authorization={assertion}");
            let origin = format!("{}://{}", client.scheme, client.authority);
            client
                .request_with_headers(
                    "POST",
                    "/backup/allow-deletes",
                    Some(json!({"device":device})),
                    &[
                        ("Origin", &origin),
                        ("Cookie", &cookie),
                        ("cf-access-jwt-assertion", &assertion),
                    ],
                )?
                .into_json()?;
            println!("Deletion approval recorded for the next ready backup request.");
        }
    }
    Ok(())
}
