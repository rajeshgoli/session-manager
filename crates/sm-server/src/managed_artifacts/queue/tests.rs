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
fn job_without_artifact_flags_thaws_the_frozen_folder_its_argv_names() {
    let f = Fixture::new();
    let conn = f.manager.store.open().unwrap();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'chains','frozen','cold','chains','queue','owner/repo',?2)",params![f.root.path,now()]).unwrap();
    let mut request = f.request();
    request.argv = Some(vec![
        "lanes.sh".into(),
        format!("{}/chains/nk5", f.root.path),
    ]);
    let p = PreparedSubmission::detect_undeclared(&f.manager, &request)
        .unwrap()
        .expect("recorded folder is detected");
    let job = RetainedQueueStore::create_queue_job_with_artifacts(
        &f.state(),
        request.clone(),
        300,
        None,
        Some((&f.manager, &p)),
    )
    .unwrap();
    assert_eq!(
        admit(&f.conn(), &job.id).unwrap().as_deref(),
        Some("thawing")
    );
    assert!(f.detail("chains")["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "thaw_requested"));

    // Paths under the root that match no record leave the job untouched.
    request.argv = Some(vec![format!("{}/scratch/out", f.root.path)]);
    assert!(PreparedSubmission::detect_undeclared(&f.manager, &request)
        .unwrap()
        .is_none());
    request.argv = Some(vec![
        "sm".into(),
        "artifact".into(),
        "thaw".into(),
        "chains".into(),
    ]);
    assert!(PreparedSubmission::detect_undeclared(&f.manager, &request)
        .unwrap()
        .is_none());
}
#[test]
fn submission_waits_out_an_artifacts_writer_longer_than_busy_timeout() {
    // sm#2259: a plain job naming a recorded folder failed with "database is
    // locked" whenever another artifacts writer held or committed meanwhile.
    let f = Fixture::new();
    let conn = f.manager.store.open().unwrap();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'chains','live','root','chains','queue','owner/repo',?2)",params![f.root.path,now()]).unwrap();
    let mut request = f.request();
    request.argv = Some(vec![format!("{}/chains/nk5", f.root.path)]);
    let p = PreparedSubmission::detect_undeclared(&f.manager, &request)
        .unwrap()
        .expect("recorded folder is detected");
    let (locked, wait_locked) = std::sync::mpsc::channel();
    let store = f.manager.store.clone();
    let root = f.root.path.clone();
    let writer = std::thread::spawn(move || {
        let conn = store.open().unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.execute(
            "UPDATE artifacts SET last_used_at=?2 WHERE root=?1 AND name='chains'",
            params![root, now()],
        )
        .unwrap();
        locked.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(6));
        conn.execute_batch("COMMIT").unwrap();
    });
    wait_locked.recv().unwrap();
    let job = RetainedQueueStore::create_queue_job_with_artifacts(
        &f.state(),
        request,
        300,
        None,
        Some((&f.manager, &p)),
    )
    .unwrap();
    writer.join().unwrap();
    assert_eq!(job.state, "pending");
    let reserved: i64 = f
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM artifact_job_reservations WHERE job_id=?1",
            [&job.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reserved, 1);
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

#[test]
fn failed_set_member_prevents_recording_the_whole_set() {
    let f = Fixture::new();
    let mut d = f.declarations("a");
    d.outputs.push("b".into());
    let first = f.submit(d);
    admit(&f.conn(), &first.id).unwrap();
    f.finish(&first.id, "failed");
    let second = f.submit(f.declarations("a"));
    admit(&f.conn(), &second.id).unwrap();
    f.finish(&second.id, "succeeded");
    assert_eq!(f.detail("a")["state"], "building");
    assert_eq!(f.detail("b")["state"], "building");
}

#[test]
fn input_only_job_starts_below_the_output_capacity_floor() {
    let mut f = Fixture::new();
    let mut d = f.declarations("out");
    d.record = None;
    let build = f.submit(d);
    admit(&f.conn(), &build.id).unwrap();
    f.finish(&build.id, "succeeded");
    f.manager.config.floor_percent = 100;
    let reader = f.submit(submission::Declarations {
        inputs: vec!["out".into()],
        ..Default::default()
    });
    assert_eq!(admit(&f.conn(), &reader.id).unwrap(), None);
}

#[test]
fn deferred_record_request_goes_to_successful_producer() {
    let f = Fixture::new();
    let mut d = f.declarations("out");
    d.record = None;
    let build = f.submit(d);
    let mut request = f.request();
    request.notify_session_id = "later-writer".into();
    let prepared =
        PreparedSubmission::prepare(&f.manager, &f.root, f.declarations("out"), &request).unwrap();
    let later = RetainedQueueStore::create_queue_job_with_artifacts(
        &f.state(),
        request,
        300,
        None,
        Some((&f.manager, &prepared)),
    )
    .unwrap();
    let conn = f.conn();
    admit(&conn, &build.id).unwrap();
    f.finish(&build.id, "succeeded");
    conn.execute(
        "UPDATE queue_jobs SET state='cancelled',finished_at=?2 WHERE id=?1",
        params![later.id, now()],
    )
    .unwrap();
    complete(&conn, &later.id, Some(&f.messages())).unwrap();
    let messages = RetainedQueueStore::new(f.messages());
    let producer = messages
        .pending_messages_for_target_by_category("maker", "queue-completion", 10)
        .unwrap();
    assert!(producer
        .iter()
        .any(|m| m.text.contains("built successfully with no record")));
    assert!(messages
        .pending_messages_for_target_by_category("later-writer", "queue-completion", 10)
        .unwrap()
        .is_empty());
}

#[test]
fn untracked_build_input_prevents_complete_recipe() {
    let f = Fixture::new();
    f.init_git();
    let mut d = f.declarations("out");
    d.record.as_mut().unwrap().recipe_complete = true;
    let job = f.submit(d);
    fs::write(
        Path::new(&f.root.repo_checkout).join("untracked.conf"),
        "build input",
    )
    .unwrap();
    admit(&f.conn(), &job.id).unwrap();
    f.finish(&job.id, "succeeded");
    assert_eq!(f.detail("out")["jobs"][0]["dirty"], 1);
    assert_eq!(f.detail("out")["recipe_complete"], 0);
    assert!(f.git(&["ls-remote", "--tags", "origin"]).is_empty());
}

fn rebuild_execution(f: &Fixture) -> super::super::rebuild::Execution {
    super::super::rebuild::Execution {
        queue_dir: f.state(),
        messages: f.messages(),
        worktrees: f.dir.join("worktrees"),
        policy: crate::queue::QueueAdmissionPolicy {
            memory_min_free_bytes: 0,
            ..Default::default()
        },
        cancel_grace: 1,
    }
}
fn rebuild_fixture(f: &Fixture, script: &str) {
    f.init_git();
    let mut d = f.declarations("out");
    d.record.as_mut().unwrap().recipe_complete = true;
    let mut request = f.request();
    request.argv = None;
    request.script = Some(script.into());
    let prepared = PreparedSubmission::prepare(&f.manager, &f.root, d, &request).unwrap();
    let job = RetainedQueueStore::create_queue_job_with_artifacts(
        &f.state(),
        request,
        300,
        None,
        Some((&f.manager, &prepared)),
    )
    .unwrap();
    admit(&f.conn(), &job.id).unwrap();
    f.finish(&job.id, "succeeded");
    assert_eq!(f.detail("out")["recipe_complete"], 1);
    fs::remove_dir(Path::new(&f.root.path).join("out")).unwrap();
    f.conn()
        .execute("UPDATE artifacts SET state='deleted',rebuildable=1", [])
        .unwrap();
}
fn await_rebuild(f: &Fixture, id: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        super::super::rebuild::resume(&f.manager).unwrap();
        let state: String = f
            .conn()
            .query_row(
                "SELECT state FROM artifact_rebuilds WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        let notified: bool = f
            .conn()
            .query_row(
                "SELECT notified FROM artifact_rebuilds WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        if matches!(state.as_str(), "succeeded" | "failed") && notified {
            return state;
        }
        assert!(std::time::Instant::now() < deadline, "rebuild timed out");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
#[test]
fn rebuild_replays_a_preserved_recipe_and_retains_records() {
    let f = Fixture::new();
    rebuild_fixture(
        &f,
        &format!("printf rebuilt > '{}/out/result'", f.root.path),
    );
    let before = f.detail("out");
    let result =
        super::super::rebuild::start(&f.manager, &f.root, "out", "maker", rebuild_execution(&f))
            .unwrap();
    let id = result["rebuild_id"].as_str().unwrap();
    assert!(f
        .manager
        .record(
            &f.root,
            &["out".into()],
            &RecordFlags {
                note: "Premature record".into(),
                project: true,
                ..Default::default()
            },
            "maker",
            "maker"
        )
        .is_err());
    f.manager.scan(&f.root).unwrap();
    assert_eq!(f.detail("out")["state"], "building");
    assert_eq!(await_rebuild(&f, id), "succeeded");
    super::super::rebuild::resume(&f.manager).unwrap();
    let after = f.detail("out");
    assert_eq!(after["state"], "live");
    assert_eq!(after["consumers"], before["consumers"]);
    assert_eq!(after["note"], before["note"]);
    assert_eq!(after["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(Path::new(&f.root.path).join("out/result")).unwrap(),
        "rebuilt"
    );
    assert_eq!(fs::read_dir(f.dir.join("worktrees")).unwrap().count(), 0);
}
#[test]
fn rebuild_failure_keeps_diagnostics_and_blocks_deletion() {
    let f = Fixture::new();
    rebuild_fixture(&f, "exit 9");
    let result =
        super::super::rebuild::start(&f.manager, &f.root, "out", "maker", rebuild_execution(&f))
            .unwrap();
    assert_eq!(
        await_rebuild(&f, result["rebuild_id"].as_str().unwrap()),
        "failed"
    );
    assert_eq!(f.detail("out")["state"], "building");
    assert_eq!(f.detail("out")["needs_owner"], true);
    assert!(f
        .manager
        .delete(&f.root, "out", "maker", "failed build")
        .is_err());
    assert!(fs::read_dir(f.dir.join("worktrees")).unwrap().count() > 0);
}
#[test]
fn rebuild_refuses_live_artifacts_and_deleted_external_inputs() {
    let f = Fixture::new();
    rebuild_fixture(&f, "true");
    let conn = f.conn();
    conn.execute("UPDATE artifacts SET state='live'", [])
        .unwrap();
    assert!(super::super::rebuild::plan(&conn, &f.root, "out", "maker").is_err());
    conn.execute("UPDATE artifacts SET state='deleted'", [])
        .unwrap();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'lost','deleted','root','lost','queue','owner/repo',?2)",params![f.root.path,now()]).unwrap();
    conn.execute("INSERT INTO artifact_inputs(root,name,input,source,job_id) SELECT root,name,'lost','declared',job_id FROM artifact_jobs",[]).unwrap();
    assert!(super::super::rebuild::plan(&conn, &f.root, "out", "maker")
        .unwrap_err()
        .to_string()
        .contains("input lost is deleted"));
}

#[test]
fn failed_rebuild_waits_for_output_reservation_release_before_notifying() {
    let f = Fixture::new();
    rebuild_fixture(&f, "exit 9");
    let result =
        super::super::rebuild::start(&f.manager, &f.root, "out", "maker", rebuild_execution(&f))
            .unwrap();
    let id = result["rebuild_id"].as_str().unwrap();
    // Install a replay job without launching a worker so the terminal-state /
    // artifact-completion boundary remains under the test's control.
    let request = f.request();
    let mut prepared = PreparedSubmission::prepare(
        &f.manager,
        &f.root,
        submission::Declarations {
            outputs: vec!["out".into()],
            disk_bytes: Some(1024),
            ..Default::default()
        },
        &request,
    )
    .unwrap();
    prepared.rebuild = Some((id.into(), 0));
    let job = RetainedQueueStore::create_queue_job_with_artifacts(
        &f.state(),
        request,
        300,
        None,
        Some((&f.manager, &prepared)),
    )
    .unwrap();
    let conn = f.conn();
    admit(&conn, &job.id).unwrap();
    conn.execute(
        "UPDATE artifact_rebuilds SET state='running' WHERE id=?1",
        [id],
    )
    .unwrap();
    conn.execute(
        "UPDATE queue_jobs SET state='failed',exit_code=9,finished_at=?2 WHERE id=?1",
        params![job.id, now()],
    )
    .unwrap();

    super::super::rebuild::resume(&f.manager).unwrap();
    let status: (String, bool) = conn
        .query_row(
            "SELECT state,notified FROM artifact_rebuilds WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, ("running".into(), false));
    assert!(super::super::rebuild::start(
        &f.manager,
        &f.root,
        "out",
        "maker",
        rebuild_execution(&f)
    )
    .is_err());

    complete(&conn, &job.id, Some(&f.messages())).unwrap();
    assert_eq!(await_rebuild(&f, id), "failed");
    assert!(super::super::rebuild::start(
        &f.manager,
        &f.root,
        "out",
        "maker",
        rebuild_execution(&f)
    )
    .is_ok());
}

#[test]
fn failed_rebuild_can_retry_without_reusing_partial_output() {
    let f = Fixture::new();
    let permit = f.dir.join("permit");
    rebuild_fixture(
        &f,
        &format!(
            "printf attempt >> '{}/out/result'; test -f '{}'",
            f.root.path,
            permit.display()
        ),
    );
    let first =
        super::super::rebuild::start(&f.manager, &f.root, "out", "maker", rebuild_execution(&f))
            .unwrap();
    assert_eq!(
        await_rebuild(&f, first["rebuild_id"].as_str().unwrap()),
        "failed"
    );
    fs::write(&permit, "allowed").unwrap();
    let second =
        super::super::rebuild::start(&f.manager, &f.root, "out", "maker", rebuild_execution(&f))
            .unwrap();
    assert_eq!(
        await_rebuild(&f, second["rebuild_id"].as_str().unwrap()),
        "succeeded"
    );
    assert_eq!(
        fs::read_to_string(Path::new(&f.root.path).join("out/result")).unwrap(),
        "attempt"
    );
    assert_eq!(f.detail("out")["needs_owner"], false);
    assert!(Path::new(&f.root.path)
        .join(format!(
            ".sm-{}-out",
            second["rebuild_id"].as_str().unwrap()
        ))
        .join("result")
        .exists());
}

#[test]
fn rebuild_replays_distinct_commits_in_order_without_failed_jobs() {
    let f = Fixture::new();
    f.init_git();
    let mut request = f.request();
    request.argv = None;
    request.script = Some(format!("cat build.txt >> '{}/out/result'", f.root.path));
    let submit = |f: &Fixture, request: CreateQueueJob| {
        let mut d = f.declarations("out");
        d.record.as_mut().unwrap().recipe_complete = true;
        let p = PreparedSubmission::prepare(&f.manager, &f.root, d, &request).unwrap();
        RetainedQueueStore::create_queue_job_with_artifacts(
            &f.state(),
            request,
            300,
            None,
            Some((&f.manager, &p)),
        )
        .unwrap()
    };
    let first = submit(&f, request.clone());
    admit(&f.conn(), &first.id).unwrap();
    fs::write(Path::new(&f.root.repo_checkout).join("build.txt"), "second").unwrap();
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
        "second commit",
    ]);
    let second = submit(&f, request);
    f.finish(&first.id, "succeeded");
    admit(&f.conn(), &second.id).unwrap();
    f.finish(&second.id, "succeeded");
    let conn = f.conn();
    conn.execute("INSERT INTO artifact_jobs(root,name,job_id,seq,state) VALUES(?1,'out','failed-original',0,'failed')",[&f.root.path]).unwrap();
    fs::remove_dir(Path::new(&f.root.path).join("out")).unwrap();
    conn.execute("UPDATE artifacts SET state='deleted',rebuildable=1", [])
        .unwrap();
    let plan = super::super::rebuild::plan(&conn, &f.root, "out", "maker").unwrap();
    assert_eq!(plan.steps.len(), 2);
    assert_ne!(plan.steps[0].commit, plan.steps[1].commit);
    let run =
        super::super::rebuild::start(&f.manager, &f.root, "out", "maker", rebuild_execution(&f))
            .unwrap();
    assert_eq!(
        await_rebuild(&f, run["rebuild_id"].as_str().unwrap()),
        "succeeded"
    );
    assert_eq!(
        fs::read_to_string(Path::new(&f.root.path).join("out/result")).unwrap(),
        "inputsecond"
    );
    assert_eq!(f.detail("out")["jobs"].as_array().unwrap().len(), 3);
}

#[test]
fn rebuild_recovers_a_submitted_step_without_submitting_it_twice() {
    let f = Fixture::new();
    rebuild_fixture(&f, &format!("printf once >> '{}/out/result'", f.root.path));
    let mut execution = rebuild_execution(&f);
    execution.policy.max_running_jobs = 0;
    let run = super::super::rebuild::start(&f.manager, &f.root, "out", "maker", execution.clone())
        .unwrap();
    let id = run["rebuild_id"].as_str().unwrap();
    super::super::rebuild::resume(&f.manager).unwrap();
    let conn = f.conn();
    let first: String = conn
        .query_row(
            "SELECT job_id FROM artifact_rebuild_steps WHERE run_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    let restored = ManagedArtifacts::new(&f.manager.config).unwrap();
    super::super::rebuild::resume(&restored).unwrap();
    let second: String = conn
        .query_row(
            "SELECT job_id FROM artifact_rebuild_steps WHERE run_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(first, second);
    execution.policy.max_running_jobs = 4;
    conn.execute(
        "UPDATE artifact_rebuilds SET execution_json=?2 WHERE id=?1",
        params![id, serde_json::to_string(&execution).unwrap()],
    )
    .unwrap();
    assert_eq!(await_rebuild(&f, id), "succeeded");
    assert_eq!(
        fs::read_to_string(Path::new(&f.root.path).join("out/result")).unwrap(),
        "once"
    );
}

#[test]
fn rebuild_waits_for_frozen_input_thaw() {
    struct Thaw;
    impl Operations for Thaw {
        fn thaw(&self, manager: &ManagedArtifacts, root: &ArtifactRoot, name: &str) -> Result<()> {
            manager.store.open()?.execute(
                "UPDATE artifacts SET state='live',location='root' WHERE root=?1 AND name=?2",
                params![root.path, name],
            )?;
            Ok(())
        }
        fn pressure(&self, _: &ManagedArtifacts, _: &ArtifactRoot, _: u64) -> Result<()> {
            Ok(())
        }
    }
    let f = Fixture::new();
    rebuild_fixture(&f, "true");
    fs::create_dir(Path::new(&f.root.path).join("input")).unwrap();
    let conn = f.conn();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'input','frozen','root','input','hand','owner/repo',?2)",params![f.root.path,now()]).unwrap();
    conn.execute("INSERT INTO artifact_inputs(root,name,input,source,job_id) SELECT root,name,'input','declared',job_id FROM artifact_jobs",[]).unwrap();
    install_operations(&f.root.path, std::sync::Arc::new(Thaw));
    let run =
        super::super::rebuild::start(&f.manager, &f.root, "out", "maker", rebuild_execution(&f))
            .unwrap();
    assert_eq!(
        await_rebuild(&f, run["rebuild_id"].as_str().unwrap()),
        "succeeded"
    );
    assert_eq!(f.detail("input")["state"], "live");
    assert!(f.detail("input")["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "thaw_requested"));
}

#[test]
fn disk_hold_is_cleared_when_readmission_waits_for_a_writer() {
    let f = Fixture::new();
    let first = f.submit(f.declarations("out"));
    let second = f.submit(f.declarations("out"));
    let conn = f.conn();
    assert_eq!(admit(&conn, &first.id).unwrap(), None);
    conn.execute(
        "INSERT INTO artifact_disk_holds VALUES(?1,?2,'root','old disk hold')",
        params![second.id, f.root.path],
    )
    .unwrap();
    assert_eq!(
        admit(&conn, &second.id).unwrap().as_deref(),
        Some("artifact_writer")
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_disk_holds WHERE job_id=?1",
            [&second.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}
#[test]
fn a_long_artifact_store_writer_holds_only_artifact_jobs() {
    // sm#2378: a backup seed held the artifact store's write lock for hours;
    // every admission pass failed and no queue job started or gave up.
    let f = Fixture::new();
    let artifact = f.submit(f.declarations("out"));
    let plain = RetainedQueueStore::create_queue_job_in_state_dir_with_max_wait(
        &f.state(),
        f.request(),
        300,
    )
    .unwrap();
    // A live server finished the one-time history import long ago.
    repair(&f.conn(), Some(&f.messages())).unwrap();
    let (locked, wait_locked) = std::sync::mpsc::channel();
    let (release, wait_release) = std::sync::mpsc::channel::<()>();
    let store = f.manager.store.clone();
    let writer = std::thread::spawn(move || {
        let conn = store.open().unwrap();
        conn.execute_batch("BEGIN IMMEDIATE; UPDATE artifact_meta SET value=value")
            .unwrap();
        locked.send(()).unwrap();
        wait_release.recv().unwrap();
        conn.execute_batch("COMMIT").unwrap();
    });
    wait_locked.recv().unwrap();
    // Constructing a manager and repairing read only while nothing changed.
    ManagedArtifacts::new(&f.manager.config).unwrap();
    repair(&f.conn(), Some(&f.messages())).unwrap();
    RetainedQueueStore::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(
        &f.state(),
        &f.messages(),
        0,
        crate::queue::QueueAdmissionPolicy::default(),
    )
    .unwrap();
    let job = |id: &str| {
        RetainedQueueStore::get_queue_job_from_path(&f.state().join("queue_runner.db"), id)
            .unwrap()
            .unwrap()
    };
    assert_ne!(job(&plain.id).state, "pending");
    let held = job(&artifact.id);
    assert_eq!(held.state, "pending");
    assert_eq!(
        held.holding_reason.as_deref(),
        Some(crate::queue::ARTIFACT_STORE_HOLD)
    );
    assert!(held.termination_detail.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("locked"));

    release.send(()).unwrap();
    writer.join().unwrap();
    RetainedQueueStore::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(
        &f.state(),
        &f.messages(),
        0,
        crate::queue::QueueAdmissionPolicy::default(),
    )
    .unwrap();
    assert_ne!(job(&artifact.id).state, "pending");
    assert!(Path::new(&f.root.path).join("out").is_dir());
}
