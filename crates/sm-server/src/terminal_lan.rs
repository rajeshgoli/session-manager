//! A certificate-backed terminal listener for another computer on the home LAN.
use std::{
    fs::{self, OpenOptions},
    net::{Ipv4Addr, SocketAddr},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    process::Command,
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use instant_acme::{
    Account, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    RetryPolicy,
};
use serde_json::{json, Value};

use crate::{
    config::{AppConfig, TerminalDirectLanConfig},
    http::{terminal_lan_router, AppState},
    sessions::expand_home,
};

const DNS_INTERVAL: Duration = Duration::from_secs(600);
const CERT_INTERVAL: Duration = Duration::from_secs(86_400);
const CERT_RENEW_BEFORE_SECONDS: u64 = 30 * 86_400;
const DNS_PERMISSION: &str = "Cloudflare token needs Zone > DNS > Edit on rajeshgo.li";

/// Keep the listener off unless its DNS record and certificate are both ready.
/// Reconcile immediately, then every ten minutes; certificate checks run daily.
pub async fn run(state: Arc<AppState>) {
    let lan = state.config().terminal_direct.lan.clone();
    if !lan.enabled {
        return;
    }
    let config = state.config().clone();
    let mut serving: Option<tokio::task::JoinHandle<()>> = None;
    let mut last_cert_check: Option<tokio::time::Instant> = None;
    let mut ticker = tokio::time::interval(DNS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let dns = tokio::task::spawn_blocking({
            let config = config.clone();
            let lan = lan.clone();
            move || sync_address_record(&config, &lan)
        })
        .await;
        let dns_ok = match dns {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                eprintln!("terminal LAN DNS unavailable: {error:#}");
                false
            }
            Err(error) => {
                eprintln!("terminal LAN DNS task failed: {error}");
                false
            }
        };
        if !dns_ok {
            stop(&mut serving).await;
            continue;
        }
        if last_cert_check.is_none_or(|at| at.elapsed() >= CERT_INTERVAL) {
            let certificate = ensure_certificate(&config, &lan).await;
            match certificate {
                Ok(renewed) => {
                    last_cert_check = Some(tokio::time::Instant::now());
                    if renewed {
                        stop(&mut serving).await;
                    }
                }
                Err(error) => {
                    eprintln!("terminal LAN certificate unavailable: {error:#}");
                    stop(&mut serving).await;
                    continue;
                }
            }
        }
        if serving.as_ref().is_some_and(|task| task.is_finished()) {
            stop(&mut serving).await;
        }
        if serving.is_none() {
            match start_listener(state.clone(), &lan).await {
                Ok(task) => serving = Some(task),
                Err(error) => eprintln!("terminal LAN listener unavailable: {error:#}"),
            }
        }
    }
}

async fn stop(serving: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(task) = serving.take() {
        task.abort();
        let _ = task.await;
    }
}

async fn start_listener(
    state: Arc<AppState>,
    lan: &TerminalDirectLanConfig,
) -> Result<tokio::task::JoinHandle<()>> {
    let dir = expand_home(&lan.cert_dir);
    let cert = fs::read(dir.join("fullchain.pem"))?;
    let key = fs::read(dir.join("privkey.pem"))?;
    let tls = RustlsConfig::from_pem(cert, key).await?;
    let addr: SocketAddr = format!("0.0.0.0:{}", lan.port).parse()?;
    let listener = std::net::TcpListener::bind(addr)
        .with_context(|| format!("cannot bind terminal LAN listener on {addr}"))?;
    listener.set_nonblocking(true)?;
    eprintln!(
        "terminal LAN listening on https://{}:{}",
        lan.hostname, lan.port
    );
    let server = axum_server::from_tcp_rustls(listener, tls)?;
    Ok(tokio::spawn(async move {
        let server = server
            .serve(terminal_lan_router(state).into_make_service_with_connect_info::<SocketAddr>());
        if let Err(error) = server.await {
            eprintln!("terminal LAN listener stopped: {error:#}");
        }
    }))
}

fn certificate_is_fresh(dir: &Path, hostname: &str) -> Result<bool> {
    let cert = dir.join("fullchain.pem");
    if !cert.exists() || !dir.join("privkey.pem").exists() {
        return Ok(false);
    }
    let valid = openssl_command()
        .args(["x509", "-in"])
        .arg(&cert)
        .args([
            "-noout",
            "-checkend",
            &CERT_RENEW_BEFORE_SECONDS.to_string(),
        ])
        .status()
        .context("cannot inspect terminal LAN certificate expiry")?;
    let matching = openssl_command()
        .args(["x509", "-in"])
        .arg(&cert)
        .args(["-noout", "-checkhost", hostname])
        .status()
        .context("cannot inspect terminal LAN certificate hostname")?;
    if !valid.success() || !matching.success() {
        return Ok(false);
    }
    let certificate_key = openssl_command()
        .args(["x509", "-in"])
        .arg(&cert)
        .args(["-pubkey", "-noout"])
        .output()
        .context("cannot read terminal LAN certificate public key")?;
    let private_key = openssl_command()
        .args(["pkey", "-in"])
        .arg(dir.join("privkey.pem"))
        .arg("-pubout")
        .output()
        .context("cannot read terminal LAN private key")?;
    Ok(certificate_key.status.success()
        && private_key.status.success()
        && certificate_key.stdout == private_key.stdout)
}

fn openssl_command() -> Command {
    // macOS ships LibreSSL, whose x509 command lacks -checkhost.
    #[cfg(target_os = "macos")]
    for path in [
        "/opt/homebrew/opt/openssl@3/bin/openssl",
        "/usr/local/opt/openssl@3/bin/openssl",
    ] {
        if Path::new(path).is_file() {
            return Command::new(path);
        }
    }
    Command::new("openssl")
}

async fn ensure_certificate(config: &AppConfig, lan: &TerminalDirectLanConfig) -> Result<bool> {
    let dir = expand_home(&lan.cert_dir);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    if certificate_is_fresh(&dir, &lan.hostname)? {
        return Ok(false);
    }
    let cloudflare = Cloudflare::new(config, &lan.hostname)?;
    let credentials_path = dir.join("acme-account.json");
    let account = if credentials_path.exists() {
        let credentials = serde_json::from_slice(&fs::read(&credentials_path)?)?;
        Account::builder()?.from_credentials(credentials).await?
    } else {
        let (account, credentials) = Account::builder()?
            .create(
                &NewAccount {
                    contact: &[],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                LetsEncrypt::Production.url().to_owned(),
                None,
            )
            .await?;
        write_private(&credentials_path, &serde_json::to_vec(&credentials)?)?;
        account
    };
    let mut order = account
        .new_order(&NewOrder::new(&[Identifier::Dns(lan.hostname.clone())]))
        .await?;
    let mut challenge_ids = Vec::new();
    let result: Result<(String, String)> = async {
        let mut authorizations = order.authorizations();
        while let Some(authorization) = authorizations.next().await {
            let mut authorization = authorization?;
            if authorization.status == AuthorizationStatus::Valid {
                continue;
            }
            let mut challenge = authorization
                .challenge(ChallengeType::Dns01)
                .context("Let's Encrypt did not offer DNS-01")?;
            let value = challenge.key_authorization().dns_value();
            let name = format!("_acme-challenge.{}", lan.hostname);
            let provider = cloudflare.clone();
            let published_value = value.clone();
            let id =
                tokio::task::spawn_blocking(move || provider.create_txt(&name, &published_value))
                    .await??;
            challenge_ids.push(id);
            wait_for_txt(&format!("_acme-challenge.{}", lan.hostname), &value).await?;
            challenge.set_ready().await?;
        }
        let policy = RetryPolicy::new().timeout(Duration::from_secs(180));
        order.poll_ready(&policy).await?;
        let key = order.finalize().await?;
        let cert = order.poll_certificate(&policy).await?;
        Ok((cert, key))
    }
    .await;
    for id in challenge_ids {
        let provider = cloudflare.clone();
        match tokio::task::spawn_blocking(move || provider.delete_record(&id)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("terminal LAN challenge cleanup failed: {error:#}"),
            Err(error) => eprintln!("terminal LAN challenge cleanup task failed: {error}"),
        }
    }
    let (cert, key) = result?;
    write_private(&dir.join("privkey.pem"), key.as_bytes())?;
    write_private(&dir.join("fullchain.pem"), cert.as_bytes())?;
    if !certificate_is_fresh(&dir, &lan.hostname)? {
        bail!("Let's Encrypt returned a certificate without the expected hostname or lifetime");
    }
    Ok(true)
}

async fn wait_for_txt(name: &str, value: &str) -> Result<()> {
    for _ in 0..24 {
        let name = name.to_owned();
        let output = tokio::task::spawn_blocking(move || {
            Command::new("dig")
                .args(["@1.1.1.1", "+short", "+time=2", "+tries=1", "TXT", &name])
                .output()
        })
        .await??;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(value) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    bail!("DNS-01 TXT record did not propagate within two minutes")
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("new");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

#[derive(Clone)]
struct Cloudflare {
    token: String,
    zone_id: String,
    agent: ureq::Agent,
}

impl Cloudflare {
    fn new(config: &AppConfig, hostname: &str) -> Result<Self> {
        let token = config
            .cloudflare_access
            .api_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .context("cloudflare_access.api_token is required for terminal LAN DNS")?
            .to_owned();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .into();
        let mut provider = Self {
            token,
            zone_id: config.cloudflare_access.zone_id.clone().unwrap_or_default(),
            agent,
        };
        if provider.zone_id.is_empty() {
            let zone = hostname
                .split_once('.')
                .map(|(_, zone)| zone)
                .context("terminal LAN hostname must include a DNS zone")?;
            let response = provider.request("GET", &format!("/zones?name={zone}"), None)?;
            provider.zone_id = response["result"]
                .as_array()
                .and_then(|zones| zones.first())
                .and_then(|zone| zone["id"].as_str())
                .context("Cloudflare could not find the terminal LAN DNS zone")?
                .to_owned();
        }
        Ok(provider)
    }

    fn records(&self, name: &str, kind: &str) -> Result<Vec<Value>> {
        let path = format!(
            "/zones/{}/dns_records?type={kind}&name={name}",
            self.zone_id
        );
        self.request("GET", &path, None)?["result"]
            .as_array()
            .cloned()
            .context("Cloudflare DNS record response has no result array")
    }

    fn sync_a(&self, name: &str, ip: Ipv4Addr) -> Result<()> {
        let records = self.records(name, "A")?;
        if records.len() > 1 {
            bail!("multiple A records exist for {name}; refusing an ambiguous LAN address");
        }
        let expected = ip.to_string();
        if records
            .first()
            .is_some_and(|record| record["content"] == expected && record["proxied"] == false)
        {
            return Ok(());
        }
        let payload =
            json!({"type":"A", "name":name, "content":expected, "ttl":120, "proxied":false});
        if let Some(record) = records.first() {
            let id = record["id"]
                .as_str()
                .context("Cloudflare A record has no id")?;
            self.request(
                "PUT",
                &format!("/zones/{}/dns_records/{id}", self.zone_id),
                Some(payload),
            )?;
        } else {
            self.request(
                "POST",
                &format!("/zones/{}/dns_records", self.zone_id),
                Some(payload),
            )?;
        }
        Ok(())
    }

    fn create_txt(&self, name: &str, value: &str) -> Result<String> {
        let response = self.request(
            "POST",
            &format!("/zones/{}/dns_records", self.zone_id),
            Some(json!({"type":"TXT", "name":name, "content":value, "ttl":120})),
        )?;
        response["result"]["id"]
            .as_str()
            .map(str::to_owned)
            .context("Cloudflare challenge response has no record id")
    }

    fn delete_record(&self, id: &str) -> Result<()> {
        self.request(
            "DELETE",
            &format!("/zones/{}/dns_records/{id}", self.zone_id),
            None,
        )?;
        Ok(())
    }

    fn request(&self, method: &str, path: &str, payload: Option<Value>) -> Result<Value> {
        let url = format!("https://api.cloudflare.com/client/v4{path}");
        let authorization = format!("Bearer {}", self.token);
        let mut response = match method {
            "GET" => self
                .agent
                .get(&url)
                .header("Authorization", &authorization)
                .call()?,
            "DELETE" => self
                .agent
                .delete(&url)
                .header("Authorization", &authorization)
                .call()?,
            "POST" => self
                .agent
                .post(&url)
                .header("Authorization", &authorization)
                .header("Content-Type", "application/json")
                .send(
                    payload
                        .context("POST requires a payload")?
                        .to_string()
                        .as_bytes(),
                )?,
            "PUT" => self
                .agent
                .put(&url)
                .header("Authorization", &authorization)
                .header("Content-Type", "application/json")
                .send(
                    payload
                        .context("PUT requires a payload")?
                        .to_string()
                        .as_bytes(),
                )?,
            _ => bail!("unsupported Cloudflare method {method}"),
        };
        let status = response.status().as_u16();
        let body: Value = serde_json::from_str(&response.body_mut().read_to_string()?)?;
        if status >= 400 || body["success"] != true {
            bail!(
                "Cloudflare DNS API returned HTTP {status}: {}. {DNS_PERMISSION}",
                body["errors"]
            );
        }
        Ok(body)
    }
}

fn sync_address_record(config: &AppConfig, lan: &TerminalDirectLanConfig) -> Result<()> {
    let ip = default_route_ipv4()?;
    Cloudflare::new(config, &lan.hostname)?.sync_a(&lan.hostname, ip)
}

fn default_route_ipv4() -> Result<Ipv4Addr> {
    let output = Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .context("cannot find the default-route network interface")?;
    if !output.status.success() {
        bail!("cannot find the default-route network interface");
    }
    let text = String::from_utf8(output.stdout)?;
    let interface = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:").map(str::trim))
        .context("default route has no interface")?;
    let output = Command::new("ipconfig")
        .args(["getifaddr", interface])
        .output()
        .context("cannot find an IPv4 address for the default-route interface")?;
    if !output.status.success() {
        bail!("default-route interface {interface} has no IPv4 address");
    }
    let ip: Ipv4Addr = String::from_utf8(output.stdout)?.trim().parse()?;
    if !ip.is_private() {
        bail!("default-route interface {interface} has no private LAN IPv4 address");
    }
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_must_match_hostname_key_and_renewal_window() {
        let dir = std::env::temp_dir().join(format!(
            "sm-terminal-lan-cert-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let cert = dir.join("fullchain.pem");
        let key = dir.join("privkey.pem");
        let output = openssl_command()
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes"])
            .args(["-days", "90", "-subj", "/CN=studio-lan.example.com"])
            .args(["-addext", "subjectAltName=DNS:studio-lan.example.com"])
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(certificate_is_fresh(&dir, "studio-lan.example.com").unwrap());
        assert!(!certificate_is_fresh(&dir, "wrong.example.com").unwrap());
        write_private(&key, b"invalid key").unwrap();
        assert!(!certificate_is_fresh(&dir, "studio-lan.example.com").unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
}
