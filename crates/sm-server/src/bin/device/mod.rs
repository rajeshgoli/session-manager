//! Computer certificate enrollment. Security.framework owns the private key;
//! cloudflared supplies the existing owner browser login for the signing call.
use super::*;
use std::process::{Command as ProcessCommand, Stdio};

#[derive(Args)]
pub(super) struct DeviceArgs {
    #[command(subcommand)]
    command: DeviceCommand,
}

#[derive(Subcommand)]
enum DeviceCommand {
    /// Install a non-exportable Mac certificate for browser sign-in.
    Enroll { name: String },
}

pub(super) fn run(args: DeviceArgs, api_url: Option<String>) -> Result<()> {
    let DeviceCommand::Enroll { name } = args.command;
    if !cfg!(target_os = "macos") {
        bail!("sm device enroll requires macOS");
    }
    if !mobile_devices::valid_computer_name(&name) {
        bail!("Device name must match [a-z0-9-]{{1,32}}");
    }
    let url = api_url.unwrap_or_else(|| "https://sm.rajeshgo.li".into());
    let client = ApiClient::parse(&url)?.with_timeout(Duration::from_secs(90));
    let local = client.scheme == "http"
        && matches!(client.host.as_str(), "localhost" | "127.0.0.1" | "::1");
    if !local && client.scheme != "https" {
        bail!("Enrollment requires HTTPS or localhost");
    }
    let assertion = if local {
        None
    } else {
        Some(owner_assertion(&url)?)
    };
    let temp = env::temp_dir().join(format!(
        "sm-device-{}-{}",
        process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    fs::create_dir(&temp)?;
    let result = (|| -> Result<()> {
        let helper = temp.join("device.swift");
        fs::write(&helper, include_str!("device_keychain.swift"))?;
        let csr = helper_call(&helper, "prepare", &name, None)?;
        let origin = format!("{}://{}", client.scheme, client.authority);
        let cookie = assertion
            .as_ref()
            .map(|token| format!("CF_Authorization={token}"));
        let mut headers = vec![("Origin", origin.as_str())];
        if let Some(token) = assertion.as_deref() {
            headers.push(("cf-access-jwt-assertion", token));
        }
        if let Some(cookie) = cookie.as_deref() {
            headers.push(("Cookie", cookie));
        }
        let response = client
            .request_with_headers(
                "POST",
                "/client/devices/enroll",
                Some(json!({ "name": name, "csr_pem": csr })),
                &headers,
            )?
            .into_json()?;
        let chain = response["certificate_chain_pem"]
            .as_str()
            .context("Server returned no certificate")?;
        helper_call(&helper, "import", &name, Some(chain))?;
        let selection =
            json!({ "pattern": "https://sm.rajeshgo.li", "filter": { "SUBJECT": { "CN": name } } })
                .to_string();
        let status = ProcessCommand::new("/usr/bin/defaults")
            .args([
                "write",
                "com.google.Chrome",
                "AutoSelectCertificateForUrls",
                "-array-add",
                &selection,
            ])
            .status()?;
        if !status.success() {
            bail!("Certificate installed, but Chrome preference could not be saved");
        }
        println!("Enrolled {name}. The private key stays in your login keychain.");
        println!("Open https://sm.rajeshgo.li in Chrome. If Chrome asks for a certificate, choose {name}.");
        Ok(())
    })();
    let _ = fs::remove_dir_all(temp);
    result
}

fn helper_call(helper: &Path, operation: &str, name: &str, input: Option<&str>) -> Result<String> {
    let mut child = ProcessCommand::new("/usr/bin/swift")
        .arg(helper)
        .args([operation, name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Run macOS Keychain helper (requires Apple command line tools)")?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("Helper stdin")?
            .write_all(input.as_bytes())?;
    }
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "Keychain enrollment failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("Keychain helper output was not UTF-8")
}

fn owner_assertion(url: &str) -> Result<String> {
    let cached = ProcessCommand::new("cloudflared")
        .args(["access", "token", "--app", url])
        .output()
        .context("Install cloudflared to use your owner browser login")?;
    if cached.status.success() {
        let token = String::from_utf8(cached.stdout)?.trim().to_owned();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    eprintln!("Sign in as the owner in the browser window opened by cloudflared.");
    let login = ProcessCommand::new("cloudflared")
        .args(["access", "login", url])
        .stdout(Stdio::null())
        .status()?;
    if !login.success() {
        bail!("Owner browser login failed");
    }
    let output = ProcessCommand::new("cloudflared")
        .args(["access", "token", "--app", url])
        .output()?;
    if !output.status.success() {
        bail!("Could not read owner browser login");
    }
    let token = String::from_utf8(output.stdout)?.trim().to_owned();
    if token.is_empty() {
        bail!("Owner browser login returned an empty token");
    }
    Ok(token)
}
