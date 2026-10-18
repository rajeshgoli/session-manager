//! Builds the mini's `sm` when it asks for one (#2264). Deploys do not build it:
//! when the mini calls `GET /backup/cli` and the published build is not this
//! server's commit, sm queues a background job that checks the commit out into
//! its own clone and runs that commit's `scripts/build-backup-cli.sh`. The mini
//! keeps its current `sm` that night and picks up the build on the next. A failed
//! build changes nothing on the mini; sm files one ticket for the commit.
use super::{update, *};
use crate::{
    managed_artifacts::config::CliBuildConfig,
    queue::{is_terminal_queue_state, CreateQueueJob, QueueJobRecord, RetainedQueueStore},
};
use anyhow::{bail, Context};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

/// The commit this server was built from (`SM_BUILD_SHA`, set by the deploy
/// script). A build without it cannot say what the mini should run.
pub fn server_commit() -> Option<&'static str> {
    option_env!("SM_BUILD_SHA")
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
}
pub fn owner(commit: &str) -> String {
    format!("backup-cli:{commit}")
}

pub struct Build<'a> {
    pub dir: &'a Path,
    pub config: &'a CliBuildConfig,
    pub queue_dir: &'a Path,
    /// sm-server, which runs the build as `--backup-cli-build`.
    pub executable: &'a Path,
    pub commit: &'a str,
    /// What the built `sm --version` must print: this server's `CLI_VERSION`.
    pub version: &'a str,
}

/// Queues a build of `commit` unless the published `sm` already is `version`, a
/// build of it is queued or running, or one failed within `retry_hours`.
pub fn request(build: &Build, now: OffsetDateTime) -> Result<Option<QueueJobRecord>> {
    if update::published(build.dir)?.is_some_and(|info| info.version == build.version) {
        return Ok(None);
    }
    if let Some(job) = RetainedQueueStore::owned_queue_job(build.queue_dir, &owner(build.commit))? {
        if !is_terminal_queue_state(&job.state) {
            return Ok(None);
        }
        let finished = job
            .finished_at
            .as_deref()
            .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok());
        let retry = Duration::hours(i64::try_from(build.config.retry_hours).unwrap_or(i64::MAX));
        // A succeeded build that is no longer published (a later commit replaced
        // it, then the server went back) is rebuilt at once.
        if job.state != "succeeded" && finished.is_some_and(|at| now - at < retry) {
            return Ok(None);
        }
    }
    let source = if build.config.source.is_empty() {
        checkout_of(build.executable)?
    } else {
        build.config.source.clone()
    };
    fs::create_dir_all(build.dir)?;
    let job = RetainedQueueStore::create_owned_queue_job(
        build.queue_dir,
        CreateQueueJob {
            local_submitter: None,
            job_type: "background".into(),
            label: format!("Backup mini sm build {}", &build.commit[..12]),
            requester_session_id: None,
            notify_session_id: String::new(),
            cwd: build.dir.display().to_string(),
            argv: Some(vec![
                build.executable.display().to_string(),
                "--backup-cli-build".into(),
                build.commit.into(),
                "--backup-cli-dir".into(),
                build.dir.display().to_string(),
                "--backup-cli-source".into(),
                source,
                "--backup-cli-version".into(),
                build.version.into(),
            ]),
            script: None,
            env: BTreeMap::from([(
                "PATH".to_owned(),
                build
                    .config
                    .path
                    .split(':')
                    .map(|p| crate::sessions::expand_home(p).display().to_string())
                    .collect::<Vec<_>>()
                    .join(":"),
            )]),
            timeout_seconds: 3600,
            cpu_percent: None,
            gpu_percent: None,
            memory_bytes: None,
            rank_tickets: None,
        },
        &owner(build.commit),
    )?;
    Ok(Some(job))
}

/// The git checkout holding `executable`: the deployed checkout installs
/// sm-server under its own `.local/bin`.
fn checkout_of(executable: &Path) -> Result<String> {
    let dir = executable.parent().context("sm-server has no directory")?;
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("run git to find the deployed checkout")?;
    if !output.status.success() {
        bail!(
            "{} is not in a git checkout; set managed_artifacts.backup.cli_build.source",
            dir.display()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A finished, failed build of `commit`, with the end of its log.
pub struct Failure {
    pub job_id: String,
    pub state: String,
    pub log_tail: String,
}
pub fn failure(queue_dir: &Path, commit: &str) -> Result<Option<Failure>> {
    let Some(job) = RetainedQueueStore::owned_queue_job(queue_dir, &owner(commit))? else {
        return Ok(None);
    };
    if !is_terminal_queue_state(&job.state) || job.state == "succeeded" {
        return Ok(None);
    }
    let log = job
        .log_path
        .as_deref()
        .and_then(|path| fs::read(path).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    let log_tail = lines[lines.len().saturating_sub(60)..].join("\n");
    Ok(Some(Failure {
        job_id: job.id,
        state: job.state,
        log_tail,
    }))
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .context("run git")?;
    if !status.success() {
        bail!(
            "git {} failed in {}: {status}",
            args.join(" "),
            dir.display()
        );
    }
    Ok(())
}

/// The queue job: check `commit` out into `<dir>/build/src`, build it into the
/// persistent `<dir>/build/target`, verify, and publish into `dir`.
pub fn run_worker(commit: &str, dir: &Path, source: &str, version: &str) -> Result<()> {
    let build = dir.join("build");
    let src = build.join("src");
    fs::create_dir_all(&build)?;
    if !src.join(".git").exists() {
        if src.exists() {
            fs::remove_dir_all(&src)?;
        }
        git(
            &build,
            &["clone", "--quiet", "--no-checkout", source, "src"],
        )?;
    }
    git(
        &src,
        &[
            "fetch",
            "--quiet",
            source,
            "+refs/heads/*:refs/remotes/source/*",
        ],
    )?;
    git(
        &src,
        &["checkout", "--quiet", "--force", "--detach", commit],
    )?;
    git(&src, &["clean", "-qfdx"])?;
    let script = src.join("scripts/build-backup-cli.sh");
    let target: PathBuf = build.join("target");
    let stage = Command::new("bash")
        .arg(&script)
        .arg("stage")
        .arg("--target-dir")
        .arg(&target)
        .arg("--expect-version")
        .arg(version)
        .arg("--dir")
        .arg(dir)
        .current_dir(&src)
        .env("SM_BUILD_SHA", commit)
        .stdin(Stdio::null())
        .status()
        .context("run the backup mini build script")?;
    if !stage.success() {
        bail!("building the backup mini's sm for {commit} failed: {stage}");
    }
    let publish = Command::new("bash")
        .arg(&script)
        .arg("publish")
        .arg("--dir")
        .arg(dir)
        .current_dir(&src)
        .stdin(Stdio::null())
        .status()?;
    if !publish.success() {
        bail!("publishing the backup mini's sm failed: {publish}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
