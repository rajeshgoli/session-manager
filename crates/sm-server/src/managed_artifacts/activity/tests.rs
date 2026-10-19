use super::*;
use std::{fs, path::PathBuf};

struct Fixture {
    base: PathBuf,
    manager: ManagedArtifacts,
    root: ArtifactRoot,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "sm-activity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(base.join("root")).unwrap();
        fs::create_dir_all(base.join("cold")).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let root = ArtifactRoot {
            path: base.join("root").display().to_string(),
            cold_store: base.join("cold").display().to_string(),
            repo: "owner/repo".into(),
            ..Default::default()
        };
        let manager = ManagedArtifacts::new(&config::ManagedArtifactsConfig {
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
        let flags = RecordFlags {
            note: "fixture".into(),
            consumers: vec!["#1".into()],
            ..Default::default()
        };
        self.manager
            .create(&self.root, name, &flags, "maker", "Maker")
            .unwrap();
        fs::write(
            Path::new(&self.root.path).join(name).join("data"),
            b"original bytes",
        )
        .unwrap();
        self.manager.done(&self.root, name, "maker").unwrap();
        // Freezes measure the folder; give the record its size like the nightly walk.
        self.manager.measure_one(&self.root, name).unwrap();
    }
    fn request(&self, name: &str, kind: &str) {
        ops::request(&self.manager, &self.root, name, kind, "far-1").unwrap();
    }
    fn exec(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) {
        self.manager
            .store
            .open()
            .unwrap()
            .execute(sql, args)
            .unwrap();
    }
    fn activity(&self) -> Value {
        activity(&self.manager, OffsetDateTime::now_utc()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        moves::TEST_MOUNTED.set(false);
        let _ = fs::remove_dir_all(&self.base);
    }
}
use std::path::Path;

#[test]
fn managed_artifacts_activity_shows_queue_order_then_done_with_sizes_and_savings() {
    let f = Fixture::new();
    f.create("first");
    f.create("second");
    f.request("first", "freezing");
    f.request("second", "freezing");
    let a = f.activity();
    let ops = a["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0]["names"], json!(["first"]));
    assert_eq!(ops[0]["state"], "waiting");
    assert_eq!(ops[0]["phase"], "queued");
    assert_eq!(ops[0]["ahead"], 0);
    assert_eq!(ops[0]["behind"], 1);
    assert_eq!(ops[1]["ahead"], 1);
    assert_eq!(ops[0]["actor"], "far-1");
    assert_eq!(ops[0]["data_bytes"], 14);
    // No freeze has been measured yet, so there is no ETA to offer.
    assert!(ops[0]["remaining_seconds"].is_null());

    ops::resume(&f.manager).unwrap();
    let a = f.activity();
    let ops = a["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2, "finished operations stay visible as done");
    assert!(ops.iter().all(|o| o["state"] == "done"));
    let history = a["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["names"], json!(["second"]), "newest first");
    let frozen = f.manager.store.detail(&f.root.path, "second").unwrap()["frozen_bytes"]
        .as_u64()
        .unwrap();
    assert_eq!(history[0]["data_bytes"], 14);
    assert_eq!(history[0]["frozen_bytes"], frozen);
    let s = &a["savings"];
    assert_eq!(s["folders"], 2);
    assert_eq!(s["data_bytes"], 28);
    assert_eq!(s["saved_bytes"], 2 * 14u64.saturating_sub(frozen));
    assert_eq!(s["roots"][0]["root"], f.root.path);

    // A later thaw leaves the freeze's recorded sizes in history.
    f.request("second", "thawing");
    ops::resume(&f.manager).unwrap();
    let a = f.activity();
    let freeze = a["history"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "freezing" && h["names"] == json!(["second"]))
        .unwrap()
        .clone();
    assert_eq!(freeze["frozen_bytes"], frozen);
    assert_eq!(a["history"][0]["kind"], "thawing");
    assert_eq!(a["savings"]["folders"], 1);
}

#[test]
fn managed_artifacts_activity_estimates_from_measured_rates_and_queue_position() {
    let f = Fixture::new();
    f.create("measured");
    f.create("running");
    f.create("waiting");
    f.request("measured", "freezing");
    ops::resume(&f.manager).unwrap();
    // The finished freeze took 10 s for 14 bytes: 1.4 bytes a second.
    let finished = OffsetDateTime::parse(
        f.manager.store.detail(&f.root.path, "measured").unwrap()["operation"]["finished_at"]
            .as_str()
            .unwrap(),
        &Rfc3339,
    )
    .unwrap();
    let started = (finished - time::Duration::seconds(10))
        .unix_timestamp_nanos()
        .to_string();
    f.exec(
        "UPDATE artifact_meta SET value=?1 WHERE key='operation_started:1'",
        &[&started],
    );

    f.request("running", "freezing");
    f.request("waiting", "freezing");
    let now = OffsetDateTime::now_utc();
    let begun = (now - time::Duration::seconds(4))
        .unix_timestamp_nanos()
        .to_string();
    f.exec(
        "UPDATE artifact_operations SET phase='copying' WHERE id=2",
        &[],
    );
    f.exec(
        "INSERT INTO artifact_meta(key,value) VALUES('operation_started:2',?1)",
        &[&begun],
    );
    let a = activity(&f.manager, now).unwrap();
    let ops: Vec<&Value> = a["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["phase"] != "done")
        .collect();
    let close = |v: &Value, want: f64| (v.as_f64().unwrap() - want).abs() < 0.01;
    assert_eq!(ops[0]["state"], "running");
    assert_eq!(ops[0]["behind"], 1);
    assert!(close(&ops[0]["expected_seconds"], 10.0));
    assert!(close(&ops[0]["elapsed_seconds"], 4.0));
    assert!(close(&ops[0]["remaining_seconds"], 6.0));
    assert_eq!(ops[0]["over_estimate"], false);
    assert_eq!(ops[1]["state"], "waiting");
    assert_eq!(ops[1]["ahead"], 1);
    assert!(close(&ops[1]["starts_in_seconds"], 6.0));
    assert!(close(&ops[1]["remaining_seconds"], 16.0));
}

#[test]
fn managed_artifacts_activity_history_without_recorded_sizes_uses_only_the_latest_operation() {
    let f = Fixture::new();
    f.create("sample");
    f.request("sample", "freezing");
    ops::resume(&f.manager).unwrap();
    f.request("sample", "thawing");
    ops::resume(&f.manager).unwrap();
    // Events written before sizes were recorded carry only the operation id.
    f.exec("UPDATE artifact_events SET detail_json=json_object('operation_id',json_extract(detail_json,'$.operation_id')) WHERE kind IN ('frozen','thawed')", &[]);
    let a = f.activity();
    let history = a["history"].as_array().unwrap();
    assert_eq!(history[0]["kind"], "thawing");
    assert_eq!(
        history[0]["data_bytes"], 14,
        "latest operation reads the folder"
    );
    assert!(
        history[1]["data_bytes"].is_null(),
        "an older one has no sizes to show"
    );
}

#[test]
fn managed_artifacts_enrich_reports_placement_savings_and_last_thaw() {
    let f = Fixture::new();
    f.create("sample");
    let mut live = f.manager.store.detail(&f.root.path, "sample").unwrap();
    enrich(&f.root, &mut live);
    assert_eq!(live["placement"]["disk"], "root");
    assert_eq!(live["placement"]["path"], format!("{}/sample", f.root.path));
    assert!(live["saved_bytes"].is_null());
    f.request("sample", "freezing");
    ops::resume(&f.manager).unwrap();
    let mut frozen = f.manager.store.detail(&f.root.path, "sample").unwrap();
    enrich(&f.root, &mut frozen);
    assert_eq!(frozen["placement"]["disk"], "cold");
    assert_eq!(frozen["placement"]["disk_path"], f.root.cold_store);
    assert_eq!(
        frozen["placement"]["path"],
        format!("{}/.frozen/sample", f.root.cold_store)
    );
    assert_eq!(
        frozen["saved_bytes"],
        14u64.saturating_sub(frozen["frozen_bytes"].as_u64().unwrap())
    );
    assert!(frozen["last_thaw"].is_null());
    f.request("sample", "thawing");
    ops::resume(&f.manager).unwrap();
    let mut thawed = f.manager.store.detail(&f.root.path, "sample").unwrap();
    enrich(&f.root, &mut thawed);
    assert_eq!(
        thawed["placement"]["path"],
        format!("{}/sample", f.root.cold_store)
    );
    assert_eq!(thawed["last_thaw"]["data_bytes"], 14);
    assert!(thawed["last_thaw"]["seconds"].as_f64().unwrap() > 0.0);
}

#[test]
fn managed_artifacts_activity_skipped_waiter_does_not_delay_the_running_operation() {
    let f = Fixture::new();
    f.create("measured");
    f.create("skipped");
    f.create("running");
    f.request("measured", "freezing");
    ops::resume(&f.manager).unwrap();
    let finished = OffsetDateTime::parse(
        f.manager.store.detail(&f.root.path, "measured").unwrap()["operation"]["finished_at"]
            .as_str()
            .unwrap(),
        &Rfc3339,
    )
    .unwrap();
    let started = (finished - time::Duration::seconds(10))
        .unix_timestamp_nanos()
        .to_string();
    f.exec(
        "UPDATE artifact_meta SET value=?1 WHERE key='operation_started:1'",
        &[&started],
    );
    // The runner passed over the older request for want of cold-disk capacity.
    f.request("skipped", "freezing");
    f.request("running", "freezing");
    let now = OffsetDateTime::now_utc();
    let begun = (now - time::Duration::seconds(4))
        .unix_timestamp_nanos()
        .to_string();
    f.exec("UPDATE artifact_operations SET error='Waiting for cold-store capacity above the floor and reservations' WHERE id=2", &[]);
    f.exec(
        "UPDATE artifact_operations SET phase='copying' WHERE id=3",
        &[],
    );
    f.exec(
        "INSERT INTO artifact_meta(key,value) VALUES('operation_started:3',?1)",
        &[&begun],
    );
    let a = activity(&f.manager, now).unwrap();
    let ops = a["operations"].as_array().unwrap();
    let close = |v: &Value, want: f64| (v.as_f64().unwrap() - want).abs() < 0.01;
    assert_eq!(ops[0]["names"], json!(["skipped"]));
    assert_eq!(ops[0]["state"], "waiting");
    assert_eq!(ops[0]["ahead"], 1);
    assert!(close(&ops[0]["starts_in_seconds"], 6.0));
    assert!(close(&ops[0]["remaining_seconds"], 16.0));
    assert_eq!(ops[1]["state"], "running");
    assert_eq!(ops[1]["behind"], 1);
    assert!(
        close(&ops[1]["remaining_seconds"], 6.0),
        "only its own copy is left"
    );
}

#[test]
fn managed_artifacts_activity_lists_a_failed_operation_with_its_finish_time() {
    let f = Fixture::new();
    f.create("sample");
    f.request("sample", "freezing");
    // The folder vanishes before the copy: the freeze fails and is aborted.
    fs::remove_dir_all(Path::new(&f.root.path).join("sample")).unwrap();
    ops::resume(&f.manager).unwrap();
    let a = f.activity();
    let failed = &a["operations"][0];
    assert_eq!(failed["state"], "failed");
    assert!(failed["finished_at"].is_string());
    assert!(failed["error"].is_string());
    assert_eq!(a["history"][0]["state"], "failed");
}
