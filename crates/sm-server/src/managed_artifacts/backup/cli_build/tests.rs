use super::*;
use std::os::unix::fs::PermissionsExt;

/// Stands in for scripts/build-backup-cli.sh: records what it was asked to build
/// and publishes a "binary" naming the commit, or fails when told to.
const SCRIPT: &str = r#"#!/bin/bash
set -euo pipefail
mode="$1"; shift
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target-dir) target="$2"; shift 2 ;;
    --expect-version) expected="$2"; shift 2 ;;
    --dir) dir="$2"; shift 2 ;;
  esac
done
if [[ "$mode" == publish ]]; then
  mv -f "$dir/sm-x86_64-apple-darwin.staging" "$dir/sm-x86_64-apple-darwin"
  mv -f "$dir/sm-x86_64-apple-darwin.json.staging" "$dir/sm-x86_64-apple-darwin.json"
  exit 0
fi
[[ -f FAIL ]] && { echo "error[E0425]: cannot find value in this scope" >&2; exit 101; }
mkdir -p "$target"
printf '%s' "$SM_BUILD_SHA" > "$dir/sm-x86_64-apple-darwin.staging"
printf '{"version":"%s","sha256":"00","bytes":40}' "$expected" > "$dir/sm-x86_64-apple-darwin.json.staging"
"#;

struct Repo {
    dir: PathBuf,
}
impl Repo {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sm-cli-build-{}-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("source/scripts")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let repo = Self { dir };
        repo.git(&["init", "--quiet", "-b", "main"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "user.name", "test"]);
        let script = repo.source().join("scripts/build-backup-cli.sh");
        fs::write(&script, SCRIPT).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        repo
    }
    fn source(&self) -> PathBuf {
        self.dir.join("source")
    }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.source())
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }
    fn commit(&self, file: &str) -> String {
        fs::write(self.source().join(file), file).unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "-m", file]);
        self.git(&["rev-parse", "HEAD"])
    }
    fn cli(&self) -> PathBuf {
        self.dir.join("cli")
    }
    fn queue(&self) -> PathBuf {
        self.dir.join("queue")
    }
    fn set_job(&self, commit: &str, state: &str, finished: OffsetDateTime) {
        let job = RetainedQueueStore::owned_queue_job(&self.queue(), &owner(commit))
            .unwrap()
            .unwrap();
        let conn = crate::queue::open_queue_jobs_connection(&self.queue().join("queue_runner.db"))
            .unwrap();
        conn.execute(
            "UPDATE queue_jobs SET state=?2, finished_at=?3 WHERE id=?1",
            rusqlite::params![job.id, state, finished.format(&Rfc3339).unwrap()],
        )
        .unwrap();
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn build<'a>(
    dir: &'a Path,
    queue: &'a Path,
    exe: &'a Path,
    config: &'a CliBuildConfig,
    commit: &'a str,
) -> Build<'a> {
    Build {
        dir,
        config,
        queue_dir: queue,
        executable: exe,
        commit,
        version: "sm 0.2.0 (abcdef012345)",
    }
}

#[test]
fn a_request_queues_one_build_of_the_servers_commit_and_retries_failures_later() {
    let repo = Repo::new();
    let commit = repo.commit("first");
    let (dir, queue) = (repo.cli(), repo.queue());
    // sm-server installed inside the deployed checkout, as the deploy does.
    fs::create_dir_all(repo.source().join(".local/bin")).unwrap();
    let exe = repo.source().join(".local/bin/sm-server");
    let config = CliBuildConfig::default();
    let b = build(&dir, &queue, &exe, &config, &commit);
    let now = OffsetDateTime::now_utc();

    let job = request(&b, now)
        .unwrap()
        .expect("first request queues a build");
    let argv = job.argv.unwrap();
    assert_eq!(argv[0], exe.display().to_string());
    assert_eq!(&argv[1..3], ["--backup-cli-build", commit.as_str()]);
    assert_eq!(argv[6], repo.source().display().to_string());
    assert_eq!(argv[8], "sm 0.2.0 (abcdef012345)");
    assert_eq!(job.job_type, "background");
    // Queued or running: no second build.
    assert!(request(&b, now).unwrap().is_none());

    repo.set_job(&commit, "failed", now);
    assert!(request(&b, now + Duration::hours(1)).unwrap().is_none());
    assert!(request(&b, now + Duration::hours(13)).unwrap().is_some());

    // Published already: nothing to build, whatever the job history.
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(update::CLI_JSON),
        r#"{"version":"sm 0.2.0 (abcdef012345)","sha256":"00","bytes":1}"#,
    )
    .unwrap();
    repo.set_job(&commit, "failed", now - Duration::days(2));
    assert!(request(&b, now).unwrap().is_none());

    // Another commit published since: a succeeded build is redone at once.
    fs::write(
        dir.join(update::CLI_JSON),
        r#"{"version":"sm 0.2.0 (999999999999)","sha256":"00","bytes":1}"#,
    )
    .unwrap();
    repo.set_job(&commit, "succeeded", now);
    assert!(request(&b, now).unwrap().is_some());
}

#[test]
fn an_executable_outside_a_checkout_needs_a_configured_source() {
    let repo = Repo::new();
    let commit = repo.commit("first");
    let (dir, queue) = (repo.cli(), repo.queue());
    let exe = std::env::temp_dir().join("sm-server");
    let mut config = CliBuildConfig::default();
    let error = request(
        &build(&dir, &queue, &exe, &config, &commit),
        OffsetDateTime::now_utc(),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("cli_build.source"), "{error}");
    config.source = repo.source().display().to_string();
    let job = request(
        &build(&dir, &queue, &exe, &config, &commit),
        OffsetDateTime::now_utc(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(job.argv.unwrap()[6], repo.source().display().to_string());
}

#[test]
fn the_worker_builds_each_commit_in_its_own_clone_and_publishes() {
    let repo = Repo::new();
    let first = repo.commit("first");
    let source = repo.source().display().to_string();
    let dir = repo.cli();
    run_worker(&first, &dir, &source, "sm 0.2.0 (first)").unwrap();
    assert_eq!(
        fs::read_to_string(dir.join(update::CLI_FILE)).unwrap(),
        first
    );
    assert_eq!(
        update::published(&dir).unwrap().unwrap().version,
        "sm 0.2.0 (first)"
    );
    assert!(dir.join("build/target").is_dir());

    // A later commit is fetched into the same clone; leftovers are cleaned.
    fs::write(dir.join("build/src/stray"), b"left by a crash").unwrap();
    let second = repo.commit("second");
    run_worker(&second, &dir, &source, "sm 0.2.0 (second)").unwrap();
    assert_eq!(
        fs::read_to_string(dir.join(update::CLI_FILE)).unwrap(),
        second
    );
    assert!(!dir.join("build/src/stray").exists());

    // A failing build leaves the published pair alone.
    let broken = repo.commit("FAIL");
    let error = run_worker(&broken, &dir, &source, "sm 0.2.0 (broken)").unwrap_err();
    assert!(error.to_string().contains(&broken), "{error}");
    assert_eq!(
        fs::read_to_string(dir.join(update::CLI_FILE)).unwrap(),
        second
    );
    assert_eq!(
        update::published(&dir).unwrap().unwrap().version,
        "sm 0.2.0 (second)"
    );
    assert!(run_worker("0".repeat(40).as_str(), &dir, &source, "x").is_err());
}

#[test]
fn a_failed_build_reports_its_job_and_log_tail() {
    let repo = Repo::new();
    let commit = repo.commit("first");
    let (dir, queue) = (repo.cli(), repo.queue());
    let exe = repo.source().join("sm-server");
    let config = CliBuildConfig::default();
    assert!(failure(&queue, &commit).unwrap().is_none());
    let job = request(
        &build(&dir, &queue, &exe, &config, &commit),
        OffsetDateTime::now_utc(),
    )
    .unwrap()
    .unwrap();
    assert!(failure(&queue, &commit).unwrap().is_none());
    let log = (1..=100).map(|n| format!("line {n}\n")).collect::<String>();
    fs::write(job.log_path.as_ref().unwrap(), log).unwrap();
    repo.set_job(&commit, "failed", OffsetDateTime::now_utc());
    let failed = failure(&queue, &commit).unwrap().unwrap();
    assert_eq!(
        (failed.job_id.as_str(), failed.state.as_str()),
        (job.id.as_str(), "failed")
    );
    assert!(failed.log_tail.starts_with("line 41\n") && failed.log_tail.ends_with("line 100"));
    repo.set_job(&commit, "succeeded", OffsetDateTime::now_utc());
    assert!(failure(&queue, &commit).unwrap().is_none());
}
