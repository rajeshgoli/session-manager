//! Resumable pull orchestration, separate from HTTP for failure-injection tests.
use super::{
    client::Destination,
    lifecycle::{Ack, RequestStatus},
    *,
};
use anyhow::{bail, ensure, Context};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    time::{Duration, Instant},
};

pub trait Remote {
    fn json(&mut self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value)>;
    /// Append exactly the requested suffix. Reject a server ignoring Range.
    fn download(
        &mut self,
        id: &str,
        part: &Part,
        offset: u64,
        output: &mut dyn Write,
    ) -> Result<()>;
    fn probe(&mut self) -> Result<Value>;
}
fn successful((status, body): (u16, Value)) -> Result<Value> {
    ensure!(
        (200..300).contains(&status),
        "backup server returned {status}: {body}"
    );
    Ok(body)
}
fn clear_update(dest: &Destination) -> Result<()> {
    dest.conn.execute(
        "DELETE FROM metadata WHERE key=?1",
        [super::update::PENDING_UPDATE],
    )?;
    Ok(())
}
fn clear_pending(dest: &Destination) -> Result<()> {
    dest.conn.execute(
        "DELETE FROM metadata WHERE key IN ('pending_request', 'pending_ack')",
        [],
    )?;
    Ok(())
}
fn finish(dest: &mut Destination, remote: &mut impl Remote, id: &str, ack: &Ack) -> Result<()> {
    successful(remote.json(
        "POST",
        &format!("/backup/requests/{id}/ack"),
        Some(serde_json::to_value(ack)?),
    )?)?;
    if !ack.applied {
        dest.rollback(id)?;
        cleanup_staging(dest, id)?;
        clear_pending(dest)?;
        clear_update(dest)?;
        bail!("backup integrity check failed; repair the reported files or reseed the destination");
    }
    complete_local(dest, id)
}
fn complete_local(dest: &Destination, id: &str) -> Result<()> {
    safe_relative(id)?;
    ensure!(!id.contains('/'), "invalid backup request id");
    dest.confirm(id)?;
    cleanup_staging(dest, id)?;
    clear_pending(dest)?;
    clear_update(dest)?;
    dest.expire_history(time::OffsetDateTime::now_utc().unix_timestamp())
}
fn cleanup_staging(dest: &Destination, id: &str) -> Result<()> {
    safe_relative(id)?;
    ensure!(!id.contains('/'), "invalid backup request id");
    // Keep no obsolete transit/staging copy on the remote disk.
    for base in [".downloads", ".incoming"] {
        let path = dest.root.join(base).join(id);
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}
pub fn pull(dest: &mut Destination, remote: &mut impl Remote) -> Result<Ack> {
    let local =
        time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    pull_with_check(dest, remote, local.weekday() == time::Weekday::Sunday)
}
/// Where tonight's request stands once the server has finished preparing it.
pub enum Ready {
    /// The server already confirmed it (a lost ack response); completed locally.
    Acked(Ack),
    Ready(RequestStatus),
}
/// Requests tonight's backup, or resumes the pending request, and waits until
/// the server has prepared it. The request stays pending in the destination, so
/// a later `pull` (also in a newly installed sm) resumes the same request.
pub fn wait_ready(dest: &mut Destination, remote: &mut impl Remote) -> Result<Ready> {
    super::seed::recover_remote(dest, remote)?;
    let deadline = Instant::now() + Duration::from_secs(3 * 3600);
    let pending = dest.meta("pending_request")?.filter(|s| !s.is_empty());
    let mut status = if let Some(id) = pending {
        let response = remote.json("GET", &format!("/backup/requests/{id}"), None)?;
        let status: RequestStatus = serde_json::from_value(successful(response)?)?;
        if status.state == "acked" {
            let ack: Ack = serde_json::from_str(
                &dest
                    .meta("pending_ack")?
                    .filter(|s| !s.is_empty())
                    .context(
                        "server confirmed a request without a local acknowledgement journal",
                    )?,
            )?;
            complete_local(dest, &id)?;
            return Ok(Ready::Acked(ack));
        }
        if matches!(status.state.as_str(), "failed" | "expired") {
            dest.rollback(&id)?;
            cleanup_staging(dest, &id)?;
            clear_pending(dest)?;
            new_request(dest, remote)?
        } else {
            status
        }
    } else {
        new_request(dest, remote)?
    };
    while status.state == "preparing" {
        let delay = Duration::from_secs(status.retry_after_seconds.unwrap_or(30).max(1));
        ensure!(
            Instant::now() + delay < deadline,
            "backup preparation exceeded three hours; retry next run"
        );
        std::thread::sleep(delay);
        status = serde_json::from_value(successful(remote.json(
            "GET",
            &format!("/backup/requests/{}", status.id),
            None,
        )?)?)?;
    }
    ensure!(
        status.state == "ready",
        "backup preparation {}: {}",
        status.state,
        status.error.as_deref().unwrap_or("unknown error")
    );
    Ok(Ready::Ready(status))
}
pub(super) fn pull_with_check(
    dest: &mut Destination,
    remote: &mut impl Remote,
    sunday: bool,
) -> Result<Ack> {
    let status = match wait_ready(dest, remote)? {
        Ready::Acked(ack) => return Ok(ack),
        Ready::Ready(status) => status,
    };
    let id = &status.id;
    if let Some(body) = dest.meta("pending_ack")?.filter(|s| !s.is_empty()) {
        let ack: Ack = serde_json::from_str(&body)?;
        finish(dest, remote, id, &ack)?;
        return Ok(ack);
    }
    let manifest = status
        .manifest
        .as_ref()
        .context("ready backup has no manifest")?;
    ensure!(
        manifest.manifest_id == *id,
        "backup manifest/request mismatch"
    );
    ensure!(
        manifest.base == dest.state()?.confirmed,
        "backup delta base does not match this destination's confirmed manifest"
    );
    safe_relative(id)?;
    ensure!(!id.contains('/'), "invalid backup request id");
    let download = dest.directory(&format!(".downloads/{id}"))?;
    let mut fetch_seconds = 0.0;
    let mut fetched = 0u64;
    for part in &status.parts {
        safe_relative(&part.file)?;
        ensure!(!part.file.contains('/'), "invalid backup part filename");
        let file = download.join(&part.file);
        let mut output = OpenOptions::new()
            .create(true)
            .truncate(false)
            .append(true)
            .read(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&file)?;
        let offset = output.metadata()?.len();
        if offset > part.bytes
            || (offset == part.bytes && super::outbox::hash_file(&file)? != part.sha256)
        {
            output.set_len(0)?;
        }
        let offset = output.metadata()?.len();
        if offset < part.bytes {
            let fetch = Instant::now();
            remote.download(id, part, offset, &mut output)?;
            fetch_seconds += fetch.elapsed().as_secs_f64();
            output.sync_all()?;
            fetched = fetched.saturating_add(part.bytes - offset);
        }
        ensure!(
            output.metadata()?.len() == part.bytes
                && super::outbox::hash_file(&file)? == part.sha256,
            "downloaded backup part failed SHA-256: {}",
            part.file
        );
    }
    let fetch_seconds = fetch_seconds.max(0.001);
    let apply = Instant::now();
    let staging = dest.unpack(id, manifest, &status.parts, &download)?;
    let applied = dest.apply(
        id,
        manifest,
        &staging,
        status.allow_deletes,
        status.max_delete_fraction,
    )?;
    let apply_seconds = apply.elapsed().as_secs_f64().max(0.001);
    let local =
        time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let week = local.date().to_string();
    let check = if sunday && dest.meta("checked_sunday")?.as_deref() != Some(&week) {
        Some(dest.check()?)
    } else {
        None
    };
    if sunday && dest.meta("probed_sunday")?.as_deref() != Some(&week) {
        remote.probe()?;
        dest.set_meta("probed_sunday", &week)?;
    }
    let healthy = check.as_ref().is_none_or(|check| check.ok);
    if let Some(check) = check.as_ref().filter(|check| !check.ok) {
        dest.set_meta("integrity_bad", &serde_json::to_string(&check.bad)?)?;
    }
    let ack = Ack {
        applied: healthy,
        fetched_bytes: fetched,
        fetch_seconds,
        apply_seconds,
        held_deletions: applied.held_deletions,
        check,
        error: (!healthy).then(|| "destination integrity check failed".into()),
        update: dest
            .meta(super::update::PENDING_UPDATE)?
            .map(|body| serde_json::from_str(&body))
            .transpose()?,
    };
    // Persist the exact ack before sending it: a lost response can be retried
    // without changing timings, rechecking different files, or moving history.
    dest.set_meta("pending_ack", &serde_json::to_string(&ack)?)?;
    if healthy && ack.check.is_some() {
        dest.set_meta("checked_sunday", &week)?;
    }
    finish(dest, remote, id, &ack)?;
    Ok(ack)
}
fn new_request(dest: &Destination, remote: &mut impl Remote) -> Result<RequestStatus> {
    if let Some(body) = dest.meta("integrity_bad")? {
        let paths: Vec<String> = serde_json::from_str(&body)?;
        let records = dest.records()?;
        for path in paths {
            if let Some(record) = records.get(&path) {
                super::client::verify(&dest.root.join(&path), record)
                    .with_context(|| format!("backup integrity failure remains at {path}; repair or reseed before pulling"))?;
            }
        }
        dest.conn
            .execute("DELETE FROM metadata WHERE key='integrity_bad'", [])?;
    }
    let (code, body) = remote.json(
        "POST",
        "/backup/requests",
        Some(json!({"have":dest.state()?.confirmed})),
    )?;
    if code == 409 {
        bail!("destination manifest differs from server (expected {}); use the seeded destination for this device",body["expected"]);
    }
    let status: RequestStatus = serde_json::from_value(successful((code, body))?)?;
    safe_relative(&status.id)?;
    ensure!(!status.id.contains('/'), "invalid backup request id");
    dest.set_meta("pending_request", &status.id)?;
    Ok(status)
}

pub struct HttpRemote {
    pub agent: ureq::Agent,
    pub base: String,
}
impl HttpRemote {
    pub fn new(base: &str, identity: &Path) -> Result<Self> {
        ensure!(
            base.starts_with("https://") && !base.contains(['?', '#', '@']),
            "backup requires an HTTPS API URL without credentials or query"
        );
        use ureq::tls::{ClientCert, PemItem, PrivateKey, TlsConfig};
        let cert = fs::read(identity.join("certificate.pem"))
            .context("read backup device certificate; enroll with --headless")?;
        let key =
            fs::read(identity.join("private-key.pem")).context("read backup device private key")?;
        let mut chain = Vec::new();
        for item in ureq::tls::parse_pem(&cert) {
            if let PemItem::Certificate(cert) = item? {
                chain.push(cert);
            }
        }
        ensure!(!chain.is_empty(), "device certificate chain is empty");
        let tls = TlsConfig::builder()
            .client_cert(Some(ClientCert::new_with_certs(
                &chain,
                PrivateKey::from_pem(&key)?,
            )))
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(3 * 3600)))
            .timeout_connect(Some(Duration::from_secs(30)))
            .build()
            .into();
        Ok(Self {
            agent,
            base: base.trim_end_matches('/').into(),
        })
    }
}
impl Remote for HttpRemote {
    fn json(&mut self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value)> {
        let url = format!("{}{}", self.base, path);
        let mut response = match method {
            "GET" => self.agent.get(&url).call()?,
            "POST" => self
                .agent
                .post(&url)
                .header("Content-Type", "application/json")
                .send(serde_json::to_vec(&body.unwrap_or(Value::Null))?.as_slice())?,
            _ => bail!("unsupported backup HTTP method"),
        };
        let code = response.status().as_u16();
        let text = response.body_mut().read_to_string()?;
        Ok((
            code,
            serde_json::from_str(&text).context("backup server did not return JSON")?,
        ))
    }
    fn download(
        &mut self,
        id: &str,
        part: &Part,
        offset: u64,
        output: &mut dyn Write,
    ) -> Result<()> {
        let url = format!("{}/backup/requests/{id}/parts/{}", self.base, part.n);
        let mut request = self.agent.get(url);
        if offset > 0 {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let mut response = request.call()?;
        ensure!(
            response.status().as_u16() == if offset == 0 { 200 } else { 206 },
            "backup server refused resumable download"
        );
        if offset > 0 {
            ensure!(
                response
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    == Some(format!("bytes {offset}-{}/{}", part.bytes - 1, part.bytes).as_str()),
                "backup range response mismatch"
            );
        }
        let remaining = part
            .bytes
            .checked_sub(offset)
            .context("invalid resume offset")?;
        let mut reader = response.body_mut().as_reader();
        let copied = io::copy(&mut reader.by_ref().take(remaining), output)?;
        ensure!(copied == remaining, "backup download interrupted");
        let mut extra = [0];
        ensure!(
            reader.read(&mut extra)? == 0,
            "oversized backup part response"
        );
        Ok(())
    }
    fn probe(&mut self) -> Result<Value> {
        let mut body = io::repeat(0).take(256 * 1024 * 1024);
        let mut response = self
            .agent
            .post(format!("{}/backup/probe", self.base))
            .header("Content-Length", "268435456")
            .send(ureq::SendBody::from_reader(&mut body))?;
        ensure!(
            response.status().is_success(),
            "backup speed probe failed: {}",
            response.status()
        );
        Ok(serde_json::from_str(
            &response.body_mut().read_to_string()?,
        )?)
    }
}
impl super::update::CliSource for HttpRemote {
    fn cli(&mut self) -> Result<super::update::Advert> {
        let mut response = self.agent.get(format!("{}/backup/cli", self.base)).call()?;
        let code = response.status().as_u16();
        let text = response.body_mut().read_to_string()?;
        // A server without this route answers 404 with no JSON: nothing published.
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let building = body["building"].as_bool().unwrap_or(false);
        if code == 404 {
            return Ok(super::update::Advert {
                published: None,
                building,
            });
        }
        ensure!(
            (200..300).contains(&code),
            "backup server returned {code} for the published sm: {text}"
        );
        Ok(super::update::Advert {
            published: Some(
                serde_json::from_value(body).context("published sm description is malformed")?,
            ),
            building,
        })
    }
    fn cli_download(
        &mut self,
        info: &super::update::CliInfo,
        offset: u64,
        output: &mut dyn Write,
    ) -> Result<()> {
        let mut request = self.agent.get(format!("{}/backup/cli/binary", self.base));
        if offset > 0 {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let mut response = request.call()?;
        ensure!(
            response.status().as_u16() == if offset == 0 { 200 } else { 206 },
            "backup server refused the sm download: {}",
            response.status()
        );
        if offset > 0 {
            ensure!(
                response
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    == Some(format!("bytes {offset}-{}/{}", info.bytes - 1, info.bytes).as_str()),
                "sm download range response mismatch"
            );
        }
        let remaining = info
            .bytes
            .checked_sub(offset)
            .context("invalid resume offset")?;
        let mut reader = response.body_mut().as_reader();
        let copied = io::copy(&mut reader.by_ref().take(remaining), output)?;
        ensure!(copied == remaining, "sm download interrupted");
        Ok(())
    }
}
