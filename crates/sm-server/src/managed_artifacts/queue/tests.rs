use super::*;
use crate::queue::{CreateQueueJob, RetainedQueueStore};
use std::{collections::BTreeMap, fs, path::PathBuf};
struct Fixture {
    dir: PathBuf,
    manager: ManagedArtifacts,
    root: ArtifactRoot,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sm-artifact-queue-{}-{}-{serial}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(dir.join("root")).unwrap();
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = ArtifactRoot {
            path: dir.join("root").display().to_string(),
            cold_store: dir.join("unmounted").display().to_string(),
            repo: "owner/repo".into(),
            repo_checkout: dir.join("repo").display().to_string(),
            ..Default::default()
        };
        let manager = ManagedArtifacts::new(&ManagedArtifactsConfig {
            db_path: dir.join("artifacts.db").display().to_string(),
            roots: vec![root.clone()],
            ..Default::default()
        })
        .unwrap();
        Self { dir, manager, root }
    }
    fn state(&self) -> PathBuf {
        self.dir.join("queue")
    }
    fn messages(&self) -> PathBuf {
        self.dir.join("messages.db")
    }
    fn request(&self) -> CreateQueueJob {
        CreateQueueJob {
            local_submitter: None,
            job_type: "background".into(),
            label: "fixture build".into(),
            requester_session_id: Some("maker".into()),
            notify_session_id: "maker".into(),
            cwd: self.root.repo_checkout.clone(),
            argv: Some(vec!["/usr/bin/true".into()]),
            script: None,
            env: BTreeMap::new(),
            timeout_seconds: 60,
            cpu_percent: None,
            gpu_percent: None,
            memory_bytes: None,
            rank_tickets: None,
        }
    }
    fn declarations(&self, name: &str) -> submission::Declarations {
        submission::Declarations {
            outputs: vec![name.into()],
            disk_bytes: Some(1024 * 1024),
            record: Some(RecordFlags {
                note: format!("Result {name}"),
                consumers: vec!["#123".into()],
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    fn submit(&self, d: submission::Declarations) -> crate::queue::QueueJobRecord {
        let request = self.request();
        let p = PreparedSubmission::prepare(&self.manager, &self.root, d, &request).unwrap();
        RetainedQueueStore::create_queue_job_with_artifacts(
            &self.state(),
            request,
            300,
            None,
            Some((&self.manager, &p)),
        )
        .unwrap()
    }
    fn conn(&self) -> Connection {
        let conn = crate::queue::open_queue_jobs_connection(&self.state().join("queue_runner.db"))
            .unwrap();
        attach_store(&conn, &self.manager.store).unwrap();
        conn
    }
    fn finish(&self, id: &str, state: &str) {
        let conn = self.conn();
        conn.execute(
            "UPDATE queue_jobs SET state=?2,exit_code=?3,started_at=?4,finished_at=?4 WHERE id=?1",
            params![id, state, if state == "succeeded" { 0 } else { 1 }, now()],
        )
        .unwrap();
        complete(&conn, id, Some(&self.messages())).unwrap();
    }
    fn detail(&self, name: &str) -> Value {
        self.manager.store.detail(&self.root.path, name).unwrap()
    }
    fn git(&self, args: &[&str]) -> String {
        submission::git_in(Path::new(&self.root.repo_checkout), args).unwrap()
    }
    fn init_git(&self) {
        self.git(&["init", "-q"]);
        fs::write(
            Path::new(&self.root.repo_checkout).join("build.txt"),
            "input",
        )
        .unwrap();
        self.git(&["add", "build.txt"]);
        self.git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "build",
        ]);
        let remote = self.dir.join("remote.git");
        self.git(&["init", "--bare", "-q", remote.to_str().unwrap()]);
        self.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
#[test]
fn admission_arithmetic_accounts_for_floor_and_outstanding_bytes() {
    assert!(fits(300, 180, 60, 60));
    assert!(!fits(300, 180, 60, 61));
    assert!(!fits(u64::MAX, u64::MAX, 1, 1));
}
#[test]
fn output_is_created_only_at_admission_and_recorded_on_success() {
    let f = Fixture::new();
    let job = f.submit(f.declarations("out"));
    assert!(!Path::new(&f.root.path).join("out").exists());
    assert_eq!(admit(&f.conn(), &job.id).unwrap(), None);
    assert!(Path::new(&f.root.path).join("out").is_dir());
    fs::write(Path::new(&f.root.path).join("out/data"), "data").unwrap();
    f.finish(&job.id, "succeeded");
    assert_eq!(f.detail("out")["state"], "live");
    assert_eq!(f.detail("out")["note"], "Result out");
    let conn = f.conn();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM artifact_job_reservations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    complete(&conn, &job.id, Some(&f.messages())).unwrap();
    assert_eq!(f.detail("out")["jobs"].as_array().unwrap().len(), 1);
}
#[test]
fn failure_and_success_without_record_leave_building_outputs() {
    for state in ["failed", "succeeded"] {
        let f = Fixture::new();
        let mut d = f.declarations("out");
        if state == "succeeded" {
            d.record = None;
        }
        let job = f.submit(d);
        admit(&f.conn(), &job.id).unwrap();
        f.finish(&job.id, state);
        assert_eq!(f.detail("out")["state"], "building");
        assert_eq!(f.detail("out")["jobs"][0]["state"], state);
    }
}
#[test]
fn pending_records_merge_only_after_the_whole_set_stops_writing() {
    let f = Fixture::new();
    let first = f.submit(f.declarations("out"));
    let mut d = f.declarations("out");
    d.record.as_mut().unwrap().note = "Second description".into();
    d.record.as_mut().unwrap().consumers = vec!["#456".into()];
    let second = f.submit(d);
    admit(&f.conn(), &first.id).unwrap();
    f.finish(&first.id, "succeeded");
    assert_eq!(f.detail("out")["state"], "building");
    admit(&f.conn(), &second.id).unwrap();
    f.finish(&second.id, "succeeded");
    let a = f.detail("out");
    assert_eq!(a["note"], "Second description");
    assert_eq!(a["consumers"].as_array().unwrap().len(), 2);
}
#[test]
fn disk_and_thaw_holds_keep_the_first_twenty_four_hour_deadline() {
    let f = Fixture::new();
    let mut d = f.declarations("out");
    d.disk_bytes = Some(i64::MAX);
    let job = f.submit(d);
    assert_eq!(
        admit(&f.conn(), &job.id).unwrap().as_deref(),
        Some("disk_space")
    );
    let conn = f.conn();
    let past = (OffsetDateTime::now_utc() - time::Duration::hours(23))
        .format(&Rfc3339)
        .unwrap();
    conn.execute(
        "UPDATE queue_artifact_submissions SET first_hold_at=?2 WHERE job_id=?1",
        params![job.id, past],
    )
    .unwrap();
    assert!(
        remaining(&conn, &job.id, Some("disk_space"))
            .unwrap()
            .unwrap()
            <= 3600
    );
    assert!(remaining(&conn, &job.id, Some("thawing")).unwrap().unwrap() > 3500);
    assert!(
        remaining(&conn, &job.id, Some("concurrency_cap"))
            .unwrap()
            .unwrap()
            > 3500
    );
    admit(&conn, &job.id).unwrap();
    assert!(
        remaining(&conn, &job.id, Some("disk_space"))
            .unwrap()
            .unwrap()
            <= 3600
    );
}
#[test]
fn frozen_inputs_request_thaw_and_do_not_start() {
    let f = Fixture::new();
    let conn = f.manager.store.open().unwrap();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'input','frozen','cold','input','queue','owner/repo',?2)",params![f.root.path,now()]).unwrap();
    let mut d = f.declarations("out");
    d.inputs.push("input".into());
    let job = f.submit(d);
    assert_eq!(
        admit(&f.conn(), &job.id).unwrap().as_deref(),
        Some("thawing")
    );
    assert!(!Path::new(&f.root.path).join("out").exists());
}
#[test]
fn overrun_message_and_event_are_emitted_once() {
    let f = Fixture::new();
    let mut d = f.declarations("out");
    d.disk_bytes = Some(0);
    let job = f.submit(d);
    let conn = f.conn();
    admit(&conn, &job.id).unwrap();
    fs::write(Path::new(&f.root.path).join("out/data"), vec![1u8; 65536]).unwrap();
    check_overrun(&conn, &job.id, &f.messages()).unwrap();
    check_overrun(&conn, &job.id, &f.messages()).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_events WHERE kind='overrun'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}
#[test]
fn successful_recipe_preserves_commit_tag_and_failed_recipe_jobs_do_not_count() {
    let f = Fixture::new();
    f.init_git();
    let first = f.submit(f.declarations("out"));
    admit(&f.conn(), &first.id).unwrap();
    f.finish(&first.id, "failed");
    let mut d = f.declarations("out");
    d.record.as_mut().unwrap().recipe_complete = true;
    let second = f.submit(d);
    admit(&f.conn(), &second.id).unwrap();
    f.finish(&second.id, "succeeded");
    assert_eq!(f.detail("out")["recipe_complete"], 1);
    let tags = f.git(&["ls-remote", "--tags", "origin"]);
    assert!(tags.contains("refs/tags/artifact/out/"));
}
#[test]
fn lost_artifact_side_of_submission_is_repaired_before_admission() {
    let f = Fixture::new();
    let job = f.submit(f.declarations("out"));
    let conn = f.conn();
    conn.execute_batch(
        "DELETE FROM artifact_job_reservations; DELETE FROM artifact_jobs; DELETE FROM artifacts;",
    )
    .unwrap();
    repair(&conn, Some(&f.messages())).unwrap();
    assert_eq!(f.detail("out")["state"], "building");
    assert_eq!(admit(&conn, &job.id).unwrap(), None);
}

#[test]
fn completion_recovery_preserves_measured_recipe_bytes() {
    let f = Fixture::new();
    let job = f.submit(f.declarations("out"));
    let conn = f.conn();
    admit(&conn, &job.id).unwrap();
    fs::write(Path::new(&f.root.path).join("out/data"), vec![1u8; 65536]).unwrap();
    f.finish(&job.id, "succeeded");
    let before = f.detail("out")["jobs"][0]["bytes_written"].clone();
    assert!(before.as_i64().unwrap() > 0);
    conn.execute(
        "UPDATE queue_artifact_submissions SET completed=0 WHERE job_id=?1",
        [&job.id],
    )
    .unwrap();
    complete(&conn, &job.id, Some(&f.messages())).unwrap();
    assert_eq!(f.detail("out")["jobs"][0]["bytes_written"], before);
}
#[test]
fn orphan_reservations_do_not_keep_outputs_in_use_forever() {
    let f = Fixture::new();
    let job = f.submit(f.declarations("out"));
    let conn = f.conn();
    conn.execute("DELETE FROM queue_jobs WHERE id=?1", [&job.id])
        .unwrap();
    conn.execute(
        "DELETE FROM queue_artifact_submissions WHERE job_id=?1",
        [&job.id],
    )
    .unwrap();
    repair(&conn, Some(&f.messages())).unwrap();
    super::super::ensure_not_in_use(&conn, &f.root.path, "out").unwrap();
    assert_eq!(f.detail("out")["jobs"][0]["state"], "failed");
}
#[test]
fn outstanding_reservations_shrink_as_outputs_grow() {
    let f = Fixture::new();
    let job = f.submit(f.declarations("out"));
    let conn = f.conn();
    admit(&conn, &job.id).unwrap();
    let device = fs::metadata(&f.root.path).unwrap().dev();
    let before = outstanding(&conn, &f.manager, device).unwrap();
    fs::write(Path::new(&f.root.path).join("out/data"), vec![1u8; 65536]).unwrap();
    conn.execute(
        "DELETE FROM artifact_meta WHERE key=?1",
        [format!("queue-size:{}", job.id)],
    )
    .unwrap();
    let after = outstanding(&conn, &f.manager, device).unwrap();
    assert!(after < before);
    assert!(after > 0);
}

#[test]
fn successful_build_without_pending_record_requests_recording() {
    let f = Fixture::new();
    let mut d = f.declarations("out");
    d.record = None;
    let job = f.submit(d);
    admit(&f.conn(), &job.id).unwrap();
    f.finish(&job.id, "succeeded");
    assert!(f.detail("out")["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "request_sent"));
}
#[test]
fn older_job_finishing_later_does_not_regress_last_use() {
    let f = Fixture::new();
    let job = f.submit(f.declarations("out"));
    let conn = f.conn();
    admit(&conn, &job.id).unwrap();
    let future = (OffsetDateTime::now_utc() + time::Duration::hours(1))
        .format(&Rfc3339)
        .unwrap();
    conn.execute("UPDATE artifacts SET last_used_at=?1", [&future])
        .unwrap();
    f.finish(&job.id, "succeeded");
    assert_eq!(f.detail("out")["last_used_at"], future);
}

#[test]
fn shared_outputs_wait_for_the_active_writer_but_unrelated_outputs_can_start() {
    let f = Fixture::new();
    let first = f.submit(f.declarations("shared"));
    let second = f.submit(f.declarations("SHARED"));
    let other = f.submit(f.declarations("other"));
    let conn = f.conn();
    assert_eq!(admit(&conn, &first.id).unwrap(), None);
    assert_eq!(
        admit(&conn, &second.id).unwrap().as_deref(),
        Some("artifact_writer")
    );
    assert_eq!(admit(&conn, &other.id).unwrap(), None);
    f.finish(&first.id, "succeeded");
    assert_eq!(admit(&conn, &second.id).unwrap(), None);
}
#[test]
fn failed_thaw_worker_leaves_a_retryable_hold_with_a_deadline() {
    struct Failing;
    impl Operations for Failing {
        fn thaw(&self, _: &ManagedArtifacts, _: &ArtifactRoot, _: &str) -> Result<()> {
            bail!("temporary thaw failure")
        }
        fn pressure(&self, _: &ManagedArtifacts, _: &ArtifactRoot, _: u64) -> Result<()> {
            Ok(())
        }
    }
    let f = Fixture::new();
    let conn = f.manager.store.open().unwrap();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'input','frozen','cold','input','queue','owner/repo',?2)",params![f.root.path,now()]).unwrap();
    install_operations(&f.root.path, std::sync::Arc::new(Failing));
    let mut d = f.declarations("out");
    d.inputs.push("input".into());
    let job = f.submit(d);
    let conn = f.conn();
    assert_eq!(admit(&conn, &job.id).unwrap().as_deref(), Some("thawing"));
    assert!(remaining(&conn, &job.id, Some("thawing")).unwrap().unwrap() > 86000);
}

#[test]
fn successful_command_with_invalid_output_does_not_record_or_preserve_recipe() {
    for replacement in ["missing", "file", "symlink"] {
        let f = Fixture::new();
        f.init_git();
        let mut d = f.declarations("out");
        d.record.as_mut().unwrap().recipe_complete = true;
        let job = f.submit(d);
        let conn = f.conn();
        admit(&conn, &job.id).unwrap();
        let path = Path::new(&f.root.path).join("out");
        fs::remove_dir(&path).unwrap();
        let external = f.dir.join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("keep"), "untouched").unwrap();
        match replacement {
            "file" => fs::write(&path, "not a directory").unwrap(),
            "symlink" => std::os::unix::fs::symlink(&external, &path).unwrap(),
            _ => {}
        }
        f.finish(&job.id, "succeeded");
        assert_eq!(f.detail("out")["state"], "building");
        assert_eq!(f.detail("out")["recipe_complete"], 0);
        assert_eq!(f.detail("out")["jobs"][0]["state"], "failed");
        super::super::ensure_not_in_use(&conn, &f.root.path, "out").unwrap();
        complete(&conn, &job.id, Some(&f.messages())).unwrap();
        assert_eq!(
            fs::read_to_string(external.join("keep")).unwrap(),
            "untouched"
        );
        assert!(f.git(&["ls-remote", "--tags", "origin"]).is_empty());
    }
}

#[test]
fn valid_artifact_names_are_encoded_in_preserved_tags() {
    let f = Fixture::new();
    f.init_git();
    for (name, encoded) in [
        ("data.lock", "data%2Elock"),
        ("data..v2", "data%2E%2Ev2"),
        ("data.", "data%2E"),
    ] {
        let mut d = f.declarations(name);
        d.record.as_mut().unwrap().recipe_complete = true;
        let job = f.submit(d);
        admit(&f.conn(), &job.id).unwrap();
        f.finish(&job.id, "succeeded");
        assert_eq!(f.detail(name)["recipe_complete"], 1);
        assert!(f
            .git(&["ls-remote", "--tags", "origin"])
            .contains(&format!("refs/tags/artifact/{encoded}/")));
    }
}

#[test]
fn final_record_revalidates_earlier_outputs_in_the_set() {
    let f = Fixture::new();
    let mut d = f.declarations("first");
    d.outputs.push("second".into());
    let first = f.submit(d);
    let second = f.submit(f.declarations("second"));
    admit(&f.conn(), &first.id).unwrap();
    f.finish(&first.id, "succeeded");
    assert_eq!(f.detail("first")["state"], "building");
    fs::remove_dir(Path::new(&f.root.path).join("first")).unwrap();
    admit(&f.conn(), &second.id).unwrap();
    f.finish(&second.id, "succeeded");
    assert_eq!(f.detail("first")["state"], "building");
    assert_eq!(f.detail("second")["state"], "building");
}

#[test]
fn cold_output_requires_the_expected_logical_symlink() {
    let f = Fixture::new();
    let job = f.submit(f.declarations("out"));
    let conn = f.conn();
    admit(&conn, &job.id).unwrap();
    fs::create_dir_all(Path::new(&f.root.cold_store).join("out")).unwrap();
    conn.execute("UPDATE artifacts SET location='cold'", [])
        .unwrap();
    assert!(validate_output(&conn, &f.root, "out").is_err());
    let logical = Path::new(&f.root.path).join("out");
    fs::remove_dir(&logical).unwrap();
    std::os::unix::fs::symlink(Path::new(&f.root.cold_store).join("out"), &logical).unwrap();
    assert!(validate_output(&conn, &f.root, "out").is_ok());
}

#[test]
fn completion_checks_short_build_overrun_once() {
    let f = Fixture::new();
    let mut d = f.declarations("out");
    d.disk_bytes = Some(0);
    let job = f.submit(d);
    let conn = f.conn();
    admit(&conn, &job.id).unwrap();
    check_overrun(&conn, &job.id, &f.messages()).unwrap();
    fs::write(Path::new(&f.root.path).join("out/data"), vec![1u8; 65536]).unwrap();
    f.finish(&job.id, "succeeded");
    complete(&conn, &job.id, Some(&f.messages())).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_events WHERE kind='overrun'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(f.detail("out")["state"], "live");
}

#[test]
fn pending_artifact_cancellation_commits_and_releases_reservation() {
    let f = Fixture::new();
    let job = f.submit(f.declarations("out"));
    let cancelled = RetainedQueueStore::cancel_queue_job_with_detail_in_state_dir(
        &f.state(),
        &f.messages(),
        &job.id,
        0,
        crate::queue::QueueAdmissionPolicy::default(),
        false,
        Some(&json!({"cause":"owner_cancel"})),
    )
    .unwrap()
    .unwrap();
    assert_eq!(cancelled.state, "cancelled");
    let conn = f.conn();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_job_reservations WHERE job_id=?1",
            [&job.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(f.detail("out")["jobs"][0]["state"], "cancelled");
    assert!(!Path::new(&f.root.path).join("out").exists());
    let notices = RetainedQueueStore::new(f.messages())
        .pending_messages_for_target_by_category("maker", "queue-completion", 10)
        .unwrap();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].text.contains("cancelled"));
}

#[test]
fn writer_waits_for_active_reader_but_readers_can_share_inputs() {
    let f = Fixture::new();
    let mut d = f.declarations("out");
    d.record = None;
    let build = f.submit(d);
    admit(&f.conn(), &build.id).unwrap();
    f.finish(&build.id, "succeeded");
    let reader = f.submit(submission::Declarations {
        inputs: vec!["out".into()],
        ..Default::default()
    });
    let other_reader = f.submit(submission::Declarations {
        inputs: vec!["out".into()],
        ..Default::default()
    });
    let conn = f.conn();
    assert_eq!(admit(&conn, &reader.id).unwrap(), None);
    assert_eq!(admit(&conn, &other_reader.id).unwrap(), None);
    let writer = f.submit(f.declarations("OUT"));
    assert_eq!(
        admit(&conn, &writer.id).unwrap().as_deref(),
        Some("artifact_writer")
    );
    f.finish(&reader.id, "succeeded");
    assert_eq!(
        admit(&conn, &writer.id).unwrap().as_deref(),
        Some("artifact_writer")
    );
    f.finish(&other_reader.id, "succeeded");
    assert_eq!(admit(&conn, &writer.id).unwrap(), None);
    f.finish(&writer.id, "succeeded");
    assert_eq!(f.detail("out")["state"], "live");
    assert_eq!(
        f.detail("out")["jobs"].as_array().unwrap().last().unwrap()["state"],
        "succeeded"
    );
}

#[test]
fn recipe_push_does_not_hold_queue_or_artifact_database_write_locks() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.init_git();
    let mut d = f.declarations("out");
    d.record.as_mut().unwrap().recipe_complete = true;
    let job = f.submit(d);
    admit(&f.conn(), &job.id).unwrap();
    let hook = f.dir.join("remote.git/hooks/pre-receive");
    let script = format!(
        "#!/usr/bin/env python3\nimport sqlite3, sys\nfor path, sql in {:?}:\n    conn = sqlite3.connect(path, timeout=0.1)\n    conn.execute(sql)\n    conn.commit()\nsys.stdin.read()\n",
        vec![
            (f.state().join("queue_runner.db").display().to_string(), "UPDATE queue_jobs SET label=label"),
            (f.dir.join("artifacts.db").display().to_string(), "UPDATE artifacts SET note=note"),
        ]
    );
    fs::write(&hook, script).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    f.finish(&job.id, "succeeded");
    assert_eq!(f.detail("out")["recipe_complete"], 1);
    assert_eq!(f.detail("out")["state"], "live");
    complete(&f.conn(), &job.id, Some(&f.messages())).unwrap();
    assert_eq!(f.detail("out")["recipe_complete"], 1);
}

#[test]
fn admission_captures_the_commit_and_dirty_state_that_will_run() {
    for dirty in [false, true] {
        let f = Fixture::new();
        f.init_git();
        let mut d = f.declarations("out");
        d.record.as_mut().unwrap().recipe_complete = true;
        let job = f.submit(d);
        let original = f.git(&["rev-parse", "HEAD"]);
        fs::write(
            Path::new(&f.root.repo_checkout).join("build.txt"),
            "changed",
        )
        .unwrap();
        if !dirty {
            f.git(&["add", "build.txt"]);
            f.git(&[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "changed before admission",
            ]);
        }
        let actual = f.git(&["rev-parse", "HEAD"]);
        admit(&f.conn(), &job.id).unwrap();
        f.finish(&job.id, "succeeded");
        let a = f.detail("out");
        assert_eq!(a["jobs"][0]["commit_sha"], actual);
        assert_eq!(a["jobs"][0]["dirty"], i64::from(dirty));
        assert_eq!(a["recipe_complete"], i64::from(!dirty));
        if !dirty {
            assert_ne!(actual, original);
            assert!(f
                .git(&["ls-remote", "--tags", "origin"])
                .starts_with(&actual));
        }
    }
}

#[test]
fn unstarted_writer_departure_records_the_prior_successful_build() {
    for terminal in ["cancelled", "wait_expired"] {
        let f = Fixture::new();
        f.init_git();
        let mut d = f.declarations("out");
        d.record.as_mut().unwrap().recipe_complete = true;
        let first = f.submit(d);
        let later = f.submit(f.declarations("out"));
        let conn = f.conn();
        admit(&conn, &first.id).unwrap();
        f.finish(&first.id, "succeeded");
        assert_eq!(f.detail("out")["state"], "building");
        conn.execute(
            "UPDATE queue_jobs SET state=?2,finished_at=?3 WHERE id=?1",
            params![later.id, terminal, now()],
        )
        .unwrap();
        complete(&conn, &later.id, Some(&f.messages())).unwrap();
        assert_eq!(f.detail("out")["state"], "live");
        assert_eq!(f.detail("out")["recipe_complete"], 1);
        assert_eq!(f.detail("out")["jobs"][1]["state"], terminal);
    }
}

#[test]
fn foreign_build_commit_is_preserved_in_the_artifact_roots_repository() {
    let owner = Fixture::new();
    owner.init_git();
    let foreign = Fixture::new();
    foreign.init_git();
    fs::write(
        Path::new(&foreign.root.repo_checkout).join("foreign.txt"),
        "foreign source",
    )
    .unwrap();
    foreign.git(&["add", "foreign.txt"]);
    foreign.git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.test",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "foreign build",
    ]);
    let sha = foreign.git(&["rev-parse", "HEAD"]);
    let mut request = owner.request();
    request.cwd = foreign.root.repo_checkout.clone();
    let mut d = owner.declarations("out");
    d.record.as_mut().unwrap().recipe_complete = true;
    let prepared = PreparedSubmission::prepare(&owner.manager, &owner.root, d, &request).unwrap();
    let job = RetainedQueueStore::create_queue_job_with_artifacts(
        &owner.state(),
        request,
        300,
        None,
        Some((&owner.manager, &prepared)),
    )
    .unwrap();
    admit(&owner.conn(), &job.id).unwrap();
    owner.finish(&job.id, "succeeded");
    assert_eq!(owner.detail("out")["recipe_complete"], 1);
    assert!(owner
        .git(&["ls-remote", "--tags", "origin"])
        .starts_with(&sha));
    assert!(foreign.git(&["ls-remote", "--tags", "origin"]).is_empty());
    let tag = owner.detail("out")["jobs"][0]["tag"]
        .as_str()
        .unwrap()
        .to_owned();
    owner.git(&["fetch", "origin", &format!("refs/tags/{tag}")]);
    assert_eq!(owner.git(&["rev-parse", "FETCH_HEAD"]), sha);
}

#[test]
fn queued_reader_waits_when_a_later_writer_starts_first() {
    let f = Fixture::new();
    let mut d = f.declarations("out");
    d.record = None;
    let build = f.submit(d);
    let conn = f.conn();
    admit(&conn, &build.id).unwrap();
    f.finish(&build.id, "succeeded");
    let reader = f.submit(submission::Declarations {
        inputs: vec!["OUT".into()],
        ..Default::default()
    });
    let writer = f.submit(f.declarations("out"));
    assert_eq!(admit(&conn, &writer.id).unwrap(), None);
    assert_eq!(
        admit(&conn, &reader.id).unwrap().as_deref(),
        Some("artifact_writer")
    );
    let unrelated = f.submit(f.declarations("other"));
    assert_eq!(admit(&conn, &unrelated.id).unwrap(), None);
    f.finish(&writer.id, "succeeded");
    assert_eq!(admit(&conn, &reader.id).unwrap(), None);
}
