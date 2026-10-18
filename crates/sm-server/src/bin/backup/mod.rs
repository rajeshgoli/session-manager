use super::*;
use anyhow::ensure;
use sm_server::managed_artifacts::{
    backup::{
        client::Destination,
        pull::{self, HttpRemote, Remote},
        seed,
        update::{self, Outcome, CLI_VERSION},
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
        /// Run the pull with this binary even if the server advertises another sm.
        #[arg(long)]
        no_update: bool,
        /// Put back the sm this binary replaced, and skip this version.
        #[arg(long, conflicts_with_all = ["check", "no_update"])]
        rollback: bool,
    },
    /// Exit 0 only if the backup hostname accepts this device; the gate a new sm
    /// passes before it replaces the running one.
    SelfTest {
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
/// Updates this binary when the server advertises another sm, then execs the new
/// one with `--no-update` so it runs tonight's pull. Returns when the current
/// binary should run the pull itself.
fn self_update(dest: &Path, url: &str, identity: &Path, remote: &mut HttpRemote) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let destination = Destination::open(dest)?;
    let binary = fs::canonicalize(std::env::current_exe()?)?;
    let gate = update::Gate {
        api_url: url.into(),
        identity: identity.into(),
        timeout: std::time::Duration::from_secs(60),
    };
    match update::update(&binary, CLI_VERSION, &destination, remote, &gate) {
        Ok(Outcome::Unchanged) => Ok(()),
        Ok(Outcome::Failed(report)) => {
            eprintln!(
                "Update to {} failed: {}. Tonight's pull runs on {CLI_VERSION}.",
                report.to,
                report.error.as_deref().unwrap_or("unknown error")
            );
            Ok(())
        }
        Ok(Outcome::Installed(mut report)) => {
            // The destination lock is released here and by exec (close-on-exec).
            drop(destination);
            eprintln!("Updated sm to {}; it runs tonight's pull.", report.to);
            let error = std::process::Command::new(&binary)
                .args(std::env::args_os().skip(1))
                .arg("--no-update")
                .exec();
            report.ok = false;
            report.error = Some(format!(
                "installed, but starting it failed: {error}; tonight's pull ran on {CLI_VERSION}"
            ));
            Destination::open(dest)?
                .set_meta(update::PENDING_UPDATE, &serde_json::to_string(&report)?)
        }
        Err(error) => {
            eprintln!("Could not check for a newer sm: {error:#}");
            Ok(())
        }
    }
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
            let dest = Destination::open_existing(&expanded(&from))?;
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
        BackupCommand::SelfTest { identity } => {
            let url = resolve_api_url(api_url)?;
            let identity = expanded(&identity);
            sm_server::device_identity::Identity::load(&identity)
                .with_context(|| format!("load the device identity from {}", identity.display()))?;
            let (status, body) =
                HttpRemote::new(&url, &identity)?.json("GET", "/backup/status", None)?;
            ensure!(
                (200..300).contains(&status),
                "backup hostname rejected this device: {status} {body}"
            );
            println!("Self-test passed: {url} accepts this device.");
        }
        BackupCommand::Pull {
            dest,
            check,
            identity,
            no_update,
            rollback,
        } => {
            if rollback {
                let binary = fs::canonicalize(std::env::current_exe()?)?;
                let destination = Destination::open(&expanded(&dest))?;
                update::rollback(&binary, CLI_VERSION, &destination)?;
                println!(
                    "Restored the previous sm at {}. {CLI_VERSION} stays skipped until the server advertises another version.",
                    binary.display()
                );
                return Ok(());
            }
            let url = resolve_api_url(api_url)?;
            let identity = expanded(&identity);
            let device = sm_server::device_identity::Identity::load(&identity)?;
            let mut remote = HttpRemote::new(&url, &identity)?;
            if !check && !no_update {
                self_update(&expanded(&dest), &url, &identity, &mut remote)?;
            }
            if device.renewal_due()? {
                let (status, body) = remote.json(
                    "POST",
                    "/backup/renew",
                    Some(json!({
                        "csr_pem": device.csr()?, "certificate_pem": device.certificate()?
                    })),
                )?;
                ensure!(
                    (200..300).contains(&status),
                    "device certificate renewal failed: {status} {body}"
                );
                device.install(
                    body["certificate_chain_pem"]
                        .as_str()
                        .context("renewal response has no certificate")?,
                )?;
                remote = HttpRemote::new(&url, &identity)?;
            }

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
