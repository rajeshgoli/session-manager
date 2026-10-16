use super::*;
struct Fixture {
    base: PathBuf,
    manager: ManagedArtifacts,
    root: ArtifactRoot,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "sm-ops-{}-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        fs::create_dir(base.join("root")).unwrap();
        fs::create_dir(base.join("cold")).unwrap();
        let root = ArtifactRoot {
            path: base.join("root").display().to_string(),
            cold_store: base.join("cold").display().to_string(),
            repo: "owner/repo".into(),
            ..Default::default()
        };
        let manager = ManagedArtifacts::new(&ManagedArtifactsConfig {
            db_path: base.join("db").display().to_string(),
            roots: vec![root.clone()],
            freeze_part_bytes: 32,
            ..Default::default()
        })
        .unwrap();
        moves::TEST_MOUNTED.set(true);
        Self {
            base,
            manager,
            root,
        }
    }
    fn create(&self, name: &str) {
        self.manager
            .create(
                &self.root,
                name,
                &RecordFlags {
                    note: "fixture".into(),
                    consumers: vec!["#1".into()],
                    ..Default::default()
                },
                "maker",
                "Maker",
            )
            .unwrap();
        fs::write(
            Path::new(&self.root.path).join(name).join("data"),
            b"original bytes",
        )
        .unwrap();
        self.manager.done(&self.root, name, "maker").unwrap();
    }
    fn op(&self, name: &str, kind: &str) -> Operation {
        request(&self.manager, &self.root, name, kind, "maker").unwrap();
        let row = store::query(
            &self.manager.store.open().unwrap(),
            "SELECT * FROM artifact_operations ORDER BY id DESC LIMIT 1",
            &[],
        )
        .unwrap()
        .pop()
        .unwrap();
        Operation {
            id: row["id"].as_i64().unwrap(),
            root: self.root.clone(),
            kind: kind.into(),
            phase: "queued".into(),
            members: serde_json::from_str(row["members_json"].as_str().unwrap()).unwrap(),
            actor: "maker".into(),
        }
    }
    fn detail(&self, name: &str) -> Value {
        self.manager.store.detail(&self.root.path, name).unwrap()
    }
    fn assert_bytes(&self, name: &str) {
        assert_eq!(
            fs::read(Path::new(&self.root.path).join(name).join("data")).unwrap(),
            b"original bytes"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        moves::TEST_MOUNTED.set(false);
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn managed_artifacts_ops_freeze_thaw_round_trip_and_placeholder() {
    let f = Fixture::new();
    f.create("sample");
    f.op("sample", "freezing");
    resume(&f.manager).unwrap();
    assert_eq!(f.detail("sample")["state"], "frozen");
    assert!(Path::new(&f.root.path).join("sample/FROZEN.md").is_file());
    assert!(!Path::new(&f.root.path).join("sample/data").exists());
    f.op("sample", "thawing");
    resume(&f.manager).unwrap();
    assert_eq!(f.detail("sample")["state"], "live");
    assert_eq!(f.detail("sample")["location"], "cold");
    assert!(f.detail("sample")["last_used_at"].is_string());
    assert!(f.detail("sample")["op"].is_null());
    f.assert_bytes("sample");
    assert!(fs::read_link(Path::new(&f.root.path).join("sample")).is_ok());
    // Freezing a moved artifact must remove its physical live copy too.
    f.op("sample", "freezing");
    resume(&f.manager).unwrap();
    assert!(!Path::new(&f.root.cold_store).join("sample").exists());
    assert_eq!(f.detail("sample")["state"], "frozen");
}

#[test]
fn managed_artifacts_ops_recovers_prepared_freeze_and_partial_swap() {
    let f = Fixture::new();
    f.create("sample");
    let op = f.op("sample", "freezing");
    stage(&f.manager, &op).unwrap();
    let (path, staging, finished) = paths(&f.root, &op.members[0], "freezing");
    rename(&staging, &finished).unwrap();
    rename(&path, &Path::new(&f.root.path).join(".sample.old")).unwrap();
    let placeholder = Path::new(&f.root.path).join(".sample.placeholder");
    fs::create_dir(&placeholder).unwrap();
    fs::write(placeholder.join("FROZEN.md"), "partial").unwrap();
    f.manager.scan(&f.root).unwrap();
    assert_eq!(f.detail("sample")["state"], "frozen");
    resume(&f.manager).unwrap();
    assert!(path.join("FROZEN.md").exists());
    assert!(f.detail("sample")["op"].is_null());
    f.op("sample", "thawing");
    resume(&f.manager).unwrap();
    f.assert_bytes("sample");
}

#[test]
fn managed_artifacts_ops_recovers_thaw_after_destination_rename() {
    let f = Fixture::new();
    f.create("sample");
    f.op("sample", "freezing");
    resume(&f.manager).unwrap();
    let op = f.op("sample", "thawing");
    stage(&f.manager, &op).unwrap();
    let (_, staging, finished) = paths(&f.root, &op.members[0], "thawing");
    rename(&staging, &finished).unwrap();
    resume(&f.manager).unwrap();
    f.assert_bytes("sample");
    assert!(f.detail("sample")["op"].is_null());
    assert!(!Path::new(&f.root.cold_store)
        .join(".frozen/sample")
        .exists());
}

#[test]
fn managed_artifacts_ops_recovers_move_and_disposable_partial_copy() {
    let f = Fixture::new();
    f.create("sample");
    let op = f.op("sample", "moving");
    set_phase(&f.manager, op.id, "copying").unwrap();
    let (_, staging, _) = paths(&f.root, &op.members[0], "moving");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("partial"), "incomplete").unwrap();
    resume(&f.manager).unwrap();
    f.assert_bytes("sample");
    assert_eq!(f.detail("sample")["location"], "cold");
    assert!(!Path::new(&f.root.cold_store)
        .join("sample/partial")
        .exists());
    f.create("second");
    let op = f.op("second", "moving");
    stage(&f.manager, &op).unwrap();
    let (_, staging, finished) = paths(&f.root, &op.members[0], "moving");
    rename(&staging, &finished).unwrap();
    swap(&op, &op.members[0], Some(&finished)).unwrap();
    resume(&f.manager).unwrap();
    f.assert_bytes("second");
    assert!(!Path::new(&f.root.path).join(".second.old").exists());
}

#[test]
fn managed_artifacts_ops_set_claim_is_atomic_and_in_use_blocks() {
    let f = Fixture::new();
    f.create("one");
    f.create("two");
    let conn = f.manager.store.open().unwrap();
    conn.execute("UPDATE artifacts SET set_id='set'", [])
        .unwrap();
    conn.execute("INSERT INTO artifact_signals(root,name,kind,evidence) VALUES(?1,'two','in_use','running job')",[&f.root.path]).unwrap();
    assert!(request(&f.manager, &f.root, "one", "freezing", "maker").is_err());
    assert!(f.detail("one")["op"].is_null());
    conn.execute("DELETE FROM artifact_signals", []).unwrap();
    f.op("one", "freezing");
    resume(&f.manager).unwrap();
    for name in ["one", "two"] {
        assert_eq!(f.detail(name)["state"], "frozen");
    }
    f.op("two", "thawing");
    resume(&f.manager).unwrap();
    for name in ["one", "two"] {
        f.assert_bytes(name);
    }
}

#[test]
fn managed_artifacts_ops_missing_cold_store_does_nothing() {
    let f = Fixture::new();
    f.create("sample");
    moves::TEST_MOUNTED.set(false);
    assert!(request(&f.manager, &f.root, "sample", "freezing", "maker").is_err());
    f.assert_bytes("sample");
    assert!(f.detail("sample")["op"].is_null());
}

#[test]
fn managed_artifacts_ops_missing_notice_is_limited_to_six_hours() {
    let f = Fixture::new();
    f.create("sample");
    f.op("sample", "freezing");
    resume(&f.manager).unwrap();
    moves::TEST_MOUNTED.set(false);
    let at = OffsetDateTime::now_utc();
    let mut count = 0;
    for elapsed in [0, 1, 21599, 21600] {
        notify_missing(
            &f.manager,
            &f.root,
            at + time::Duration::seconds(elapsed),
            |_, text| {
                assert!(text.contains("1 frozen and 0 moved"));
                count += 1;
                Ok(())
            },
        )
        .unwrap();
    }
    assert_eq!(count, 2);
}

#[test]
fn managed_artifacts_ops_durable_queue_request_triggers_thaw() {
    let f = Fixture::new();
    f.create("sample");
    f.op("sample", "freezing");
    resume(&f.manager).unwrap();
    let conn = f.manager.store.open().unwrap();
    conn.execute("INSERT INTO artifact_job_reservations(job_id,root,outputs_json,inputs_json,disk_bytes) VALUES('reader',?1,'[]','[\"sample\"]',0)",[&f.root.path]).unwrap();
    queue::request_operation(&conn, &f.root.path, "sample", "thaw_requested", "reader").unwrap();
    resume(&f.manager).unwrap();
    f.assert_bytes("sample");
    assert_eq!(f.detail("sample")["state"], "live");
}

#[test]
fn managed_artifacts_ops_frozen_delete_and_interrupted_delete_recovery() {
    let f = Fixture::new();
    f.create("sample");
    f.op("sample", "freezing");
    resume(&f.manager).unwrap();
    f.manager
        .delete(&f.root, "sample", "maker", "test")
        .unwrap();
    assert_eq!(f.detail("sample")["state"], "deleted");
    assert!(!Path::new(&f.root.cold_store)
        .join(".frozen/sample")
        .exists());
    assert!(!Path::new(&f.root.path).join("sample").exists());
    f.create("interrupted");
    f.op("interrupted", "freezing");
    resume(&f.manager).unwrap();
    f.manager.store.open().unwrap().execute("UPDATE artifacts SET op='deleting',deleted_by='maker',delete_reason='agent' WHERE name='interrupted'",[]).unwrap();
    fs::remove_dir_all(Path::new(&f.root.cold_store).join(".frozen/interrupted")).unwrap();
    resume(&f.manager).unwrap();
    assert_eq!(f.detail("interrupted")["state"], "deleted");
    assert!(!Path::new(&f.root.path).join("interrupted").exists());
}

#[test]
fn managed_artifacts_thaw_rate_uses_last_five_measurements() {
    let f = Fixture::new();
    let conn = f.manager.store.open().unwrap();
    for rate in [100, 10, 50, 20] {
        store::event(
            &conn,
            &f.root.path,
            "sample",
            "thaw_measured",
            "sm",
            json!({"data_bytes":rate,"seconds":1}),
        )
        .unwrap();
    }
    assert_eq!(
        store::thaw_rate(&conn, &f.root.path).unwrap(),
        100_000_000_000.0 / 60.0
    );
    store::event(
        &conn,
        &f.root.path,
        "sample",
        "thaw_measured",
        "sm",
        json!({"data_bytes":30,"seconds":1}),
    )
    .unwrap();
    assert_eq!(store::thaw_rate(&conn, &f.root.path).unwrap(), 30.0);
}

#[test]
fn managed_artifacts_ops_rechecks_keep_mark_and_rejects_archive_parent_symlink() {
    let f = Fixture::new();
    f.create("sample");
    f.op("sample", "freezing");
    f.manager
        .edit(
            &f.root,
            "sample",
            "marks",
            &json!({"project":true}),
            "maker",
        )
        .unwrap();
    resume(&f.manager).unwrap();
    f.assert_bytes("sample");
    assert_eq!(f.detail("sample")["state"], "live");
    assert_eq!(f.detail("sample")["operation"]["phase"], "failed");
    fs::remove_dir(Path::new(&f.root.cold_store).join(".frozen")).unwrap();
    let outside = f.base.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, Path::new(&f.root.cold_store).join(".frozen")).unwrap();
    f.manager
        .edit(
            &f.root,
            "sample",
            "marks",
            &json!({"project":false}),
            "maker",
        )
        .unwrap();
    assert!(request(&f.manager, &f.root, "sample", "freezing", "maker").is_err());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn managed_artifacts_ops_low_space_thaw_retains_one_pending_operation() {
    let mut f = Fixture::new();
    f.create("sample");
    f.op("sample", "freezing");
    resume(&f.manager).unwrap();
    let op = f.op("sample", "thawing");
    f.manager.config.floor_percent = 100;
    for _ in 0..3 {
        resume(&f.manager).unwrap();
    }
    assert_eq!(f.detail("sample")["op"], "thawing");
    assert_eq!(f.detail("sample")["operation"]["id"], op.id);
    assert_eq!(f.detail("sample")["operation"]["phase"], "queued");
    let conn = f.manager.store.open().unwrap();
    let failures: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_events WHERE kind='op_failed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failures, 0);
    f.manager.config.floor_percent = 5;
    resume(&f.manager).unwrap();
    f.assert_bytes("sample");
}
