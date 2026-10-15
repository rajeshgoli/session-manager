use super::*;
use std::{fs, path::PathBuf};

struct Fixture {
    dir: PathBuf,
    manager: ManagedArtifacts,
    root: ArtifactRoot,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "sm-submission-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(dir.join("root")).unwrap();
        fs::create_dir_all(dir.join("cold")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = ArtifactRoot {
            path: dir.join("root").display().to_string(),
            cold_store: dir.join("cold").display().to_string(),
            repo: "owner/repo".into(),
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
    fn request(&self) -> CreateQueueJob {
        CreateQueueJob {
            local_submitter: None,
            job_type: "background".into(),
            label: "build".into(),
            requester_session_id: Some("maker".into()),
            notify_session_id: "maker".into(),
            cwd: self.dir.display().to_string(),
            argv: Some(vec!["true".into()]),
            script: None,
            env: BTreeMap::new(),
            timeout_seconds: 60,
            cpu_percent: None,
            gpu_percent: None,
            memory_bytes: None,
            rank_tickets: None,
        }
    }
    fn declarations(&self, outputs: &[&str]) -> Declarations {
        Declarations {
            outputs: outputs.iter().map(|s| (*s).into()).collect(),
            disk_bytes: Some(100),
            ..Default::default()
        }
    }
    fn flags(&self) -> RecordFlags {
        RecordFlags {
            note: "Dataset for evaluation".into(),
            consumers: vec!["#42".into()],
            ..Default::default()
        }
    }
    fn prepare(&self, d: Declarations) -> PreparedSubmission {
        PreparedSubmission::prepare(&self.manager, &self.root, d, &self.request()).unwrap()
    }
    fn queue(&self) -> Connection {
        let conn = Connection::open(self.dir.join("queue.db")).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS queue_jobs(id TEXT PRIMARY KEY);")
            .unwrap();
        attach_store(&conn, &self.manager.store).unwrap();
        conn
    }
    fn persist(&self, id: &str, d: Declarations) -> Result<Vec<String>> {
        let prepared = self.prepare(d);
        let mut conn = self.queue();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO queue_jobs VALUES(?1)", [id])?;
        let warnings = prepared.persist(&tx, id)?;
        tx.commit()?;
        Ok(warnings)
    }
    fn seed(&self, name: &str, state: &str) {
        let conn = self.manager.store.open().unwrap();
        conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,?2,?3,'root',?2,'queue',?4,?5)",params![self.root.path,name,state,self.root.repo,now()]).unwrap();
    }
    fn detail(&self, name: &str) -> Value {
        self.manager.store.detail(&self.root.path, name).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn record_flags_are_validated_before_submission() {
    let f = Fixture::new();
    let mut d = f.declarations(&["dataset"]);
    d.disk_bytes = None;
    assert!(d.validate(&f.manager, &f.root).is_err());
    d.disk_bytes = Some(-1);
    assert!(d.validate(&f.manager, &f.root).is_err());
    d.disk_bytes = Some(0);
    d.record = Some(f.flags());
    d.validate(&f.manager, &f.root).unwrap();
    let mut bad = Vec::new();
    let mut flags = f.flags();
    flags.note = "".into();
    bad.push(flags);
    let mut flags = f.flags();
    flags.note = "two\nlines".into();
    bad.push(flags);
    let mut flags = f.flags();
    flags.note = "x".repeat(141);
    bad.push(flags);
    let mut flags = f.flags();
    flags.describe = Some("x".repeat(8001));
    bad.push(flags);
    let mut flags = f.flags();
    flags.consumers.clear();
    bad.push(flags);
    let mut flags = f.flags();
    flags.consumers = vec!["#0".into()];
    bad.push(flags);
    let mut flags = f.flags();
    flags.keep_until = Some("2000-01-01".into());
    bad.push(flags);
    let mut flags = f.flags();
    flags.ticket = Some(-1);
    bad.push(flags);
    let mut flags = f.flags();
    flags.gpu_hours = Some(0.0);
    bad.push(flags);
    let mut flags = f.flags();
    flags.gpu_cost = Some(0.0);
    bad.push(flags);
    for flags in bad {
        d.record = Some(flags);
        assert!(d.validate(&f.manager, &f.root).is_err());
    }
    d.record = Some(f.flags());
    d.outputs.clear();
    assert!(d
        .validate(&f.manager, &f.root)
        .unwrap_err()
        .to_string()
        .contains("require --artifact"));
    d.record = None;
    d.inputs = vec!["../bad".into()];
    assert!(d.validate(&f.manager, &f.root).is_err());
}

#[test]
fn detection_covers_three_spellings_cold_store_and_full_segment_boundaries() {
    let home = std::env::var("HOME").unwrap();
    let root = ArtifactRoot {
        path: format!("{home}/Artifacts"),
        cold_store: "/Volumes/Cold/Artifacts".into(),
        ..Default::default()
    };
    let texts=vec![format!("{home}/Artifacts/One/data ~/ARTIFACTS/Two, $HOME/artifacts/Three; /Volumes/Cold/Artifacts/Four) ~/Artifacts/one-long ~/Artifacts/one.ext ~/Artifacts/{}", "x".repeat(129))];
    assert_eq!(
        detect_names(&root, &texts),
        vec!["four", "one", "one-long", "one.ext", "three", "two"]
    );
}

#[test]
fn submission_resolves_aliases_and_excludes_detected_outputs() {
    let f = Fixture::new();
    f.seed("Input", "live");
    f.manager.store.open().unwrap().execute("INSERT INTO artifact_aliases(root,alias,target_name,target_rel) VALUES(?1,'alias','Input','data')",[&f.root.path]).unwrap();
    let mut request = f.request();
    request.argv = Some(vec![
        format!("{}/alias/file", f.root.path),
        format!("{}/Output/file", f.root.path),
        format!("{}/Input-long", f.root.path),
    ]);
    request.script = Some(format!("read {}/Input/data", f.root.cold_store));
    let mut d = f.declarations(&["Output", "output"]);
    d.inputs = vec!["ALIAS".into()];
    let prepared = PreparedSubmission::prepare(&f.manager, &f.root, d, &request).unwrap();
    let mut conn = f.queue();
    let tx = conn.transaction().unwrap();
    assert!(prepared.persist(&tx, "job1").unwrap().is_empty());
    tx.commit().unwrap();
    let detail = f.detail("output");
    assert_eq!(detail["inputs"].as_array().unwrap().len(), 1);
    assert_eq!(detail["inputs"][0]["input"], "Input");
    assert_eq!(detail["inputs"][0]["source"], "declared");
    assert_eq!(detail["jobs"].as_array().unwrap().len(), 1);
}

#[test]
fn deleted_detected_input_warns_but_explicit_input_refuses() {
    let f = Fixture::new();
    f.seed("gone", "deleted");
    let mut request = f.request();
    request.script = Some(format!("cat {}/gone/file", f.root.path));
    let prepared =
        PreparedSubmission::prepare(&f.manager, &f.root, f.declarations(&["out"]), &request)
            .unwrap();
    let mut conn = f.queue();
    let tx = conn.transaction().unwrap();
    let warnings = prepared.persist(&tx, "job1").unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("deleted artifact gone"));
    tx.commit().unwrap();
    let mut d = f.declarations(&[]);
    d.inputs = vec!["gone".into()];
    assert!(f
        .persist("job2", d.clone())
        .unwrap_err()
        .to_string()
        .contains("not rebuildable"));
    f.manager
        .store
        .open()
        .unwrap()
        .execute("UPDATE artifacts SET rebuildable=1 WHERE name='gone'", [])
        .unwrap();
    assert!(f
        .persist("job3", d)
        .unwrap_err()
        .to_string()
        .contains("run sm artifact rebuild gone first"));
}

#[test]
fn input_state_is_rechecked_in_transaction_and_writer_blocks_building_input() {
    let f = Fixture::new();
    f.seed("input", "live");
    let mut d = f.declarations(&["out"]);
    d.inputs = vec!["input".into()];
    let prepared = f.prepare(d.clone());
    f.manager
        .store
        .open()
        .unwrap()
        .execute(
            "UPDATE artifacts SET state='deleted' WHERE name='input'",
            [],
        )
        .unwrap();
    let mut conn = f.queue();
    let tx = conn.transaction().unwrap();
    assert!(prepared.persist(&tx, "late").is_err());
    drop(tx);
    f.manager
        .store
        .open()
        .unwrap()
        .execute(
            "UPDATE artifacts SET state='building' WHERE name='input'",
            [],
        )
        .unwrap();
    f.persist("reader", d.clone()).unwrap();
    f.persist("writer", f.declarations(&["input"])).unwrap();
    assert!(f
        .persist("reader2", d)
        .unwrap_err()
        .to_string()
        .contains("being written"));
}

#[test]
fn inputs_only_protects_frozen_inputs_without_creating_recipe_rows() {
    let f = Fixture::new();
    f.seed("input", "frozen");
    let d = Declarations {
        inputs: vec!["input".into()],
        ..Default::default()
    };
    f.persist("reader", d).unwrap();
    let conn = f.manager.store.open().unwrap();
    assert!(ensure_not_in_use(&conn, &f.root.path, "input").is_err());
    assert!(f.detail("input")["jobs"].as_array().unwrap().is_empty());
}

#[test]
fn transaction_rolls_back_queue_row_outputs_and_reservation_together() {
    let f = Fixture::new();
    let mut conn = f.queue();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute("INSERT INTO queue_jobs VALUES('rollback')", [])
        .unwrap();
    f.prepare(f.declarations(&["out"]))
        .persist(&tx, "rollback")
        .unwrap();
    tx.rollback().unwrap();
    for table in [
        "queue_jobs",
        "artifacts",
        "artifact_jobs",
        "artifact_job_reservations",
        "artifact_events",
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    assert!(!Path::new(&f.root.path).join("out").exists());
}

#[test]
fn output_refusals_leave_existing_records_unchanged() {
    let f = Fixture::new();
    for state in ["live", "frozen", "deleted", "unrecorded"] {
        f.seed(state, state);
        assert!(f.persist(state, f.declarations(&["new", state])).is_err());
        assert_eq!(f.detail(state)["state"], state);
        assert!(f.manager.store.detail(&f.root.path, "new").is_err());
    }
    fs::create_dir(Path::new(&f.root.path).join("unscanned")).unwrap();
    assert!(f
        .persist("unscanned", f.declarations(&["unscanned"]))
        .is_err());
}

#[test]
fn set_union_is_transitive_and_pending_record_and_recipe_are_preserved() {
    let f = Fixture::new();
    f.persist("ab", f.declarations(&["a", "b"])).unwrap();
    f.persist("cd", f.declarations(&["c", "d"])).unwrap();
    let mut d = f.declarations(&["b", "c"]);
    d.record = Some(f.flags());
    f.persist("bc", d).unwrap();
    for name in ["a", "b", "c", "d"] {
        assert_eq!(f.detail(name)["set_id"], "a");
    }
    let conn = f.manager.store.open().unwrap();
    let pending: String = conn
        .query_row(
            "SELECT pending_record_json FROM artifact_job_reservations WHERE job_id='bc'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<RecordFlags>(&pending).unwrap().note,
        f.flags().note
    );
    let detail = f.detail("b");
    assert_eq!(detail["jobs"][1]["seq"], 3);
    assert_eq!(detail["jobs"][1]["env_json"], "{}");
    assert_eq!(detail["jobs"][1]["state"], "queued");
}

#[test]
fn git_capture_ignores_untracked_files_and_records_dirty_tracked_files() {
    let f = Fixture::new();
    git_in(&f.dir, &["init", "-q"]).unwrap();
    fs::write(f.dir.join("tracked"), "before").unwrap();
    git_in(&f.dir, &["add", "tracked"]).unwrap();
    git_in(
        &f.dir,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "initial",
        ],
    )
    .unwrap();
    fs::create_dir(f.dir.join("subdir")).unwrap();
    let clean = GitState::capture(&f.dir.join("subdir"));
    assert!(!clean.dirty);
    assert_eq!(clean.cwd_rel.as_deref(), Some("subdir"));
    assert_eq!(clean.commit_sha.as_ref().unwrap().len(), 40);
    fs::write(f.dir.join("tracked"), "after").unwrap();
    assert!(GitState::capture(&f.dir).dirty);
    let other = Fixture::new();
    let absent = GitState::capture(&other.dir);
    assert!(absent.dirty);
    assert!(absent.commit_sha.is_none());
}

#[test]
fn oversized_script_is_rejected_before_any_persistence() {
    let f = Fixture::new();
    let mut request = f.request();
    request.script = Some("x".repeat(MAX_SCRIPT_BYTES + 1));
    assert!(
        PreparedSubmission::prepare(&f.manager, &f.root, f.declarations(&["out"]), &request)
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
    );
}

#[test]
fn attach_is_idempotent_and_refuses_another_store() {
    let f = Fixture::new();
    let conn = f.queue();
    attach_store(&conn, &f.manager.store).unwrap();
    let other = Fixture::new();
    assert!(attach_store(&conn, &other.manager.store).is_err());
}

#[test]
fn scan_preserves_outputs_waiting_for_admission_but_detects_vanished_started_output() {
    let f = Fixture::new();
    f.persist("waiting", f.declarations(&["out"])).unwrap();
    f.manager.scan(&f.root).unwrap();
    assert_eq!(f.detail("out")["state"], "building");
    f.manager
        .store
        .open()
        .unwrap()
        .execute(
            "UPDATE artifact_job_reservations SET start_bytes=0 WHERE job_id='waiting'",
            [],
        )
        .unwrap();
    f.manager.scan(&f.root).unwrap();
    assert_eq!(f.detail("out")["state"], "deleted");
    assert_eq!(f.detail("out")["delete_reason"], "vanished");
}
