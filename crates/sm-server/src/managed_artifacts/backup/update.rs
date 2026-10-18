//! The mini updates its own `sm` at the start of a nightly pull (#2264). The mini
//! accepts no inbound connections, so the server only advertises a build; the mini
//! downloads it, verifies it, proves it runs, and swaps it in. Every failure
//! leaves a runnable binary at the original path.
use super::{client::Destination, *};
use anyhow::{bail, Context};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Exactly what `sm --version` prints, without the trailing newline.
pub const CLI_VERSION: &str = concat!("sm ", env!("SM_CLI_VERSION"));
/// The published binary and its `{version, sha256, bytes}` description.
pub const CLI_FILE: &str = "sm-x86_64-apple-darwin";
pub const CLI_JSON: &str = "sm-x86_64-apple-darwin.json";
const DOWNLOAD_ATTEMPTS: usize = 3;
/// Metadata key in the destination's state.db carrying tonight's report into the ack.
pub const PENDING_UPDATE: &str = "pending_update";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliInfo {
    pub version: String,
    pub sha256: String,
    pub bytes: u64,
}
/// Reported in the ack as `update`, and stored on the `backup_runs` row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateReport {
    pub from: String,
    pub to: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The installed description, read by the server; `None` when nothing is published.
pub fn published(dir: &Path) -> Result<Option<CliInfo>> {
    match fs::read(dir.join(CLI_JSON)) {
        Ok(body) => Ok(Some(
            serde_json::from_slice(&body).context("published sm description is malformed")?,
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub trait CliSource {
    /// `GET /backup/cli`; `None` on 404.
    fn cli(&mut self) -> Result<Option<CliInfo>>;
    /// `GET /backup/cli/binary`, appending exactly the bytes after `offset`.
    fn cli_download(&mut self, info: &CliInfo, offset: u64, output: &mut dyn Write) -> Result<()>;
}

/// The candidate must pass these before it replaces the running binary.
pub struct Gate {
    pub api_url: String,
    pub identity: PathBuf,
    pub timeout: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing published, already current, or the advertised version is skipped.
    Unchanged,
    /// The candidate was rejected; the running binary stays and runs the pull.
    Failed(UpdateReport),
    /// The candidate is now at the binary's path; exec it with `--no-update`.
    Installed(UpdateReport),
}

pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_BETWEEN_RENAMES: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Steps 1-5 of the update. `binary` is the running binary's canonical path and
/// `own` its version. Records the report for tonight's ack; never fails the pull
/// except when the advertisement cannot be read at all (an `Err`).
pub fn update(
    binary: &Path,
    own: &str,
    dest: &Destination,
    source: &mut impl CliSource,
    gate: &Gate,
) -> Result<Outcome> {
    let Some(info) = source.cli()? else {
        return Ok(Outcome::Unchanged);
    };
    let mut state = dest.state()?;
    let skipped = state.skip_versions.contains(&info.version);
    if !skipped && !state.skip_versions.is_empty() {
        state.skip_versions.clear();
        dest.write_state(&state)?;
    }
    if skipped || info.version == own {
        return Ok(Outcome::Unchanged);
    }
    let candidate = sibling(binary, ".new");
    let result = install(binary, &candidate, &info, source, gate);
    let report = UpdateReport {
        from: own.into(),
        to: info.version.clone(),
        ok: result.is_ok(),
        error: result
            .as_ref()
            .err()
            .map(|e| format!("{e:#}").chars().take(1000).collect()),
    };
    if result.is_err() {
        let _ = fs::remove_file(&candidate);
    }
    dest.set_meta(PENDING_UPDATE, &serde_json::to_string(&report)?)?;
    Ok(if report.ok {
        Outcome::Installed(report)
    } else {
        Outcome::Failed(report)
    })
}

fn install(
    binary: &Path,
    candidate: &Path,
    info: &CliInfo,
    source: &mut impl CliSource,
    gate: &Gate,
) -> Result<()> {
    download(candidate, info, source)?;
    fs::set_permissions(candidate, fs::Permissions::from_mode(0o755))?;
    remove_quarantine(candidate)?;
    let version = run(candidate, &["--version"], gate.timeout)?;
    let printed = version.trim_end_matches('\n');
    if printed != info.version {
        bail!(
            "new binary reports version {printed:?}, expected {:?}",
            info.version
        );
    }
    let identity = gate.identity.to_string_lossy();
    run(
        candidate,
        &[
            "backup",
            "self-test",
            "--api-url",
            &gate.api_url,
            "--identity",
            &identity,
        ],
        gate.timeout,
    )
    .context("new binary failed its self-test")?;
    swap(binary, candidate)
}

fn download(candidate: &Path, info: &CliInfo, source: &mut impl CliSource) -> Result<()> {
    let mut output = OpenOptions::new()
        .create(true)
        .truncate(false)
        .append(true)
        .read(true)
        .mode(0o700)
        .custom_flags(libc::O_NOFOLLOW)
        .open(candidate)?;
    if output.metadata()?.len() > info.bytes {
        output.set_len(0)?;
    }
    let mut last = None;
    for _ in 0..DOWNLOAD_ATTEMPTS {
        let offset = output.metadata()?.len();
        if offset == info.bytes {
            break;
        }
        match source.cli_download(info, offset, &mut output) {
            Ok(()) => last = None,
            Err(e) => last = Some(e),
        }
        output.sync_all()?;
    }
    if let Some(e) = last {
        return Err(e.context("download of the new sm failed"));
    }
    let bytes = output.metadata()?.len();
    if bytes != info.bytes {
        bail!(
            "downloaded {bytes} bytes of the new sm, expected {}",
            info.bytes
        );
    }
    let sha256 = super::outbox::hash_file(candidate)?;
    if sha256 != info.sha256 {
        bail!("downloaded sm failed SHA-256 check");
    }
    Ok(())
}

fn remove_quarantine(path: &Path) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let name = CString::new("com.apple.quarantine")?;
    // SAFETY: both arguments are NUL-terminated strings that outlive the call.
    let rc = unsafe { libc::removexattr(path.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) };
    if rc != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOATTR) {
            return Err(error).context("remove the quarantine attribute from the new sm");
        }
    }
    Ok(())
}

/// Runs the candidate with a deadline and returns its stdout.
fn run(program: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {}", program.display()))?;
    let mut stdout = child.stdout.take().context("child stdout")?;
    let mut stderr = child.stderr.take().context("child stderr")?;
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{} {}` did not finish within {}s",
                program.display(),
                args.join(" "),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        bail!(
            "`{}` exited with {status}: {}",
            args.join(" "),
            stderr.trim()
        );
    }
    Ok(stdout)
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("binary has no directory")?)?.sync_all()?;
    Ok(())
}

/// A crash at any point leaves a runnable binary at `binary`: the old one until
/// the final rename, the verified new one after it.
fn swap(binary: &Path, candidate: &Path) -> Result<()> {
    let previous = sibling(binary, ".prev");
    let staged = sibling(binary, ".prev.tmp");
    {
        let mut from = File::open(binary)?;
        let mut to = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&staged)?;
        io::copy(&mut from, &mut to)?;
        to.sync_all()?;
    }
    fs::rename(&staged, &previous)?;
    sync_parent(binary)?;
    #[cfg(test)]
    if FAIL_BETWEEN_RENAMES.get() {
        bail!("injected crash between the two renames");
    }
    fs::rename(candidate, binary)?;
    sync_parent(binary)
}

/// `sm backup pull --rollback`: put the previous binary back and skip the version
/// it replaced until the server advertises a different one.
pub fn rollback(binary: &Path, own: &str, dest: &Destination) -> Result<String> {
    let previous = sibling(binary, ".prev");
    if !previous.is_file() {
        bail!("no previous sm at {}", previous.display());
    }
    let mut state = dest.state()?;
    if !state.skip_versions.iter().any(|v| v == own) {
        state.skip_versions.push(own.into());
        dest.write_state(&state)?;
    }
    fs::rename(&previous, binary)?;
    sync_parent(binary)?;
    Ok(own.into())
}

#[cfg(test)]
mod tests;
