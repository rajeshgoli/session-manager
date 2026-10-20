use super::super::{
    config::{ArtifactRoot, ManagedArtifactsConfig},
    moves, ManagedArtifacts,
};
use super::*;
use rusqlite::params;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, io::Read, os::unix::fs::symlink, path::PathBuf};
struct Fixture {
    dir: PathBuf,
    manager: ManagedArtifacts,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sm-backup-{}-{}-{serial}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        for path in ["root", "cold", "data", "projects"] {
            fs::create_dir_all(dir.join(path)).unwrap();
        }
        let dir = fs::canonicalize(dir).unwrap();
        let mut config = ManagedArtifactsConfig {
            db_path: dir.join("backup.db").display().to_string(),
            roots: vec![ArtifactRoot {
                path: dir.join("root").display().to_string(),
                cold_store: dir.join("cold").display().to_string(),
                repo: "owner/repo".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.backup.outbox = dir.join("outbox").display().to_string();
        config.backup.static_paths = vec![];
        config.backup.plain_paths = vec![dir.join("data").display().to_string()];
        config.backup.outbox_part_bytes = 5;
        moves::TEST_MOUNTED.set(true);
        Self {
            manager: ManagedArtifacts::new(&config).unwrap(),
            dir,
        }
    }
    fn scopes(&self) -> Vec<Scope> {
        ["data", "projects"]
            .into_iter()
            .map(|name| Scope {
                source: self.dir.join(name),
                segment: name.into(),
                plain: name == "data",
                holding_days: if name == "data" { 30 } else { 60 },
                estimated_bytes: 100,
                exclude: Exclude::default(),
            })
            .collect()
    }
    fn prepare(&self, id: &str, base: Option<&Manifest>) -> Prepared {
        prepare(&self.manager, &self.scopes(), base, id).unwrap()
    }
    fn artifact(&self, name: &str, state: &str, recipe: bool, gpu: bool, seconds: i64) {
        let root = &self.manager.config.roots[0];
        let path = if state == "frozen" {
            PathBuf::from(&root.cold_store).join(".frozen").join(name)
        } else {
            PathBuf::from(&root.path).join(name)
        };
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("content"), b"abcdefg").unwrap();
        let conn = self.manager.store.open().unwrap();
        conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at,recipe_complete,gpu_hours) VALUES(?1,?2,?3,'root',?2,'hand','owner/repo','now',?4,?5)",params![root.path,name,state,recipe,if gpu {1} else {0}]).unwrap();
        conn.execute("INSERT INTO artifact_jobs(root,name,job_id,state,wall_seconds) VALUES(?1,?2,?2,'succeeded',?3)",params![root.path,name,seconds]).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        moves::TEST_MOUNTED.set(false);
        let _ = fs::remove_dir_all(&self.dir);
    }
}
/// The one seed staging directory a failed seed left, if any.
fn seed_staging(dest: &super::client::Destination) -> Option<PathBuf> {
    let mut staging: Vec<_> = fs::read_dir(dest.root.join(".incoming"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(staging.len() <= 1, "{staging:?}");
    staging.pop()
}

#[test]
fn coverage_preserves_unchanged_live_frozen_and_symlink_versions() {
    use crate::managed_artifacts::scan;
    let f = Fixture::new();
    let root = &f.manager.config.roots[0];
    let conn = f.manager.store.open().unwrap();
    for state in ["live", "unrecorded", "frozen"] {
        f.artifact(state, state, false, false, 0);
        let source = if state == "frozen" {
            f.dir.join("cold/.frozen/frozen")
        } else {
            f.dir.join("root").join(state)
        };
        let measured = scan::measure(&source).unwrap().stamp();
        // A frozen row retains the original tree's stamp, not its archive stamp.
        let stored = if state == "frozen" {
            "original-tree"
        } else {
            &measured
        };
        conn.execute(
            "UPDATE artifacts SET content_stamp=?1 WHERE name=?2",
            params![stored, state],
        )
        .unwrap();
        let request =
            json!({"coverage_json": json!([{"source":source,"stamp":measured}]).to_string()});
        lifecycle::mark_covered(&conn, &f.manager, &request).unwrap();
        let a = f.manager.store.detail(&root.path, state).unwrap();
        assert_eq!(a["backup_stamp"], stored);
        assert_eq!(a["backup_due"], false);
    }
    fs::write(f.dir.join("data/content"), b"external target").unwrap();
    for (name, target) in [
        ("alias", f.dir.join("root/live")),
        ("far", f.dir.join("data")),
    ] {
        symlink(&target, f.dir.join("root").join(name)).unwrap();
        conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,?2,'live','root',?2,'hand','owner/repo','now')", params![root.path, name]).unwrap();
        f.manager.measure_one(root, name).unwrap();
        // Both a link-only scope and a scope reading a link's external target.
        let source = if name == "alias" {
            f.dir.join("root/alias")
        } else {
            target
        };
        let stamp = scan::measure(&source).unwrap().stamp();
        let request =
            json!({"coverage_json": json!([{"source":source,"stamp":stamp}]).to_string()});
        lifecycle::mark_covered(&conn, &f.manager, &request).unwrap();
        let a = f.manager.store.detail(&root.path, name).unwrap();
        assert_eq!(a["backup_stamp"], "0:0:0");
        assert_eq!(a["backup_due"], false);
    }
}

#[test]
fn coverage_does_not_stamp_an_artifact_updated_after_verification() {
    use crate::managed_artifacts::scan;
    let f = Fixture::new();
    let root = &f.manager.config.roots[0];
    let conn = f.manager.store.open().unwrap();
    for state in ["live", "frozen"] {
        f.artifact(state, state, false, false, 0);
        let source = if state == "frozen" {
            f.dir.join("cold/.frozen/frozen")
        } else {
            f.dir.join("root/live")
        };
        let stamp = scan::measure(&source).unwrap().stamp();
        conn.execute(
            "UPDATE artifacts SET content_stamp=?1 WHERE name=?2",
            params![stamp, state],
        )
        .unwrap();
        let request =
            json!({"coverage_json": json!([{"source":source,"stamp":stamp}]).to_string()});
        let coverage = lifecycle::measure_coverage(&conn, &f.manager, &request).unwrap();
        // A separate connection publishes a new measurement in the exact gap.
        // This also proves verification leaves the artifact writer available.
        if state == "live" {
            fs::write(source.join("content"), b"updated after backup verification").unwrap();
            f.manager.measure_one(root, state).unwrap();
        } else {
            let writer = f.manager.store.open().unwrap();
            writer
                .execute(
                    "UPDATE artifacts SET content_stamp='new-version' WHERE name=?1",
                    [state],
                )
                .unwrap();
        }
        coverage.persist(&conn).unwrap();
        let a = f.manager.store.detail(&root.path, state).unwrap();
        assert!(a["backed_up_at"].is_null());
        assert!(a["backup_stamp"].is_null());
        assert_eq!(a["backup_due"], true);
    }
}

#[test]
fn coverage_checks_lifecycle_state_and_does_not_copy_a_stale_live_stamp() {
    use crate::managed_artifacts::scan;
    let f = Fixture::new();
    let root = &f.manager.config.roots[0];
    f.artifact("live", "live", false, false, 0);
    let conn = f.manager.store.open().unwrap();
    let source = f.dir.join("root/live");
    let stamp = scan::measure(&source).unwrap().stamp();
    let request = json!({"coverage_json": json!([{"source":source,"stamp":stamp}]).to_string()});
    for change in ["state='deleted'", "op='freezing'", "location='cold'"] {
        conn.execute(
            "UPDATE artifacts SET state='live',op=NULL,location='root'",
            [],
        )
        .unwrap();
        let coverage = lifecycle::measure_coverage(&conn, &f.manager, &request).unwrap();
        conn.execute(&format!("UPDATE artifacts SET {change}"), [])
            .unwrap();
        coverage.persist(&conn).unwrap();
        assert!(f.manager.store.detail(&root.path, "live").unwrap()["backed_up_at"].is_null());
    }
    conn.execute(
        "UPDATE artifacts SET state='live',op=NULL,location='root',content_stamp='stale'",
        [],
    )
    .unwrap();
    lifecycle::mark_covered(&conn, &f.manager, &request).unwrap();
    let a = f.manager.store.detail(&root.path, "live").unwrap();
    assert_eq!(a["backup_stamp"], stamp);
    assert_eq!(a["backup_due"], true);
}

#[test]
fn coverage_does_not_stamp_a_replacement_artifact_with_an_equal_stamp() {
    let f = Fixture::new();
    let root = &f.manager.config.roots[0];
    f.artifact("live", "live", false, false, 0);
    let conn = f.manager.store.open().unwrap();
    let source = f.dir.join("root/live");
    let stamp = crate::managed_artifacts::scan::measure(&source)
        .unwrap()
        .stamp();
    f.manager.measure_one(root, "live").unwrap();
    let request = json!({"coverage_json": json!([{"source":source,"stamp":stamp}]).to_string()});
    let coverage = lifecycle::measure_coverage(&conn, &f.manager, &request).unwrap();
    conn.execute("DELETE FROM artifacts WHERE name='live'", [])
        .unwrap();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at,content_stamp) VALUES(?1,'live','live','root','live','hand','owner/repo','replacement',?2)", params![root.path, stamp]).unwrap();
    coverage.persist(&conn).unwrap();
    let a = f.manager.store.detail(&root.path, "live").unwrap();
    assert!(a["backed_up_at"].is_null());
    assert_eq!(a["backup_due"], true);
}

#[test]
fn acknowledgement_confirms_before_coverage_can_cross_the_expiry_deadline() {
    use super::lifecycle::{self as l, RequestResult};
    let mut f = Fixture::new();
    f.manager.config.backup.enabled = true;
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/content"), b"backup").unwrap();
    let RequestResult::Open(first) = l::request(&f.manager, "mini", None).unwrap() else {
        panic!()
    };
    l::prepare_request(&f.manager, &first.id).unwrap();
    let manager = f.manager.clone();
    let id = first.id.clone();
    l::BEFORE_COVERAGE_MEASURE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let future = time::OffsetDateTime::now_utc().unix_timestamp()
                + i64::try_from(manager.config.backup.ready_ttl_hours).unwrap() * 3600
                + 1;
            l::expire(&manager, future).unwrap();
            assert_eq!(l::status(&manager, "mini", &id).unwrap().state, "acked");
        }))
    });
    let ack = l::Ack {
        applied: true,
        fetched_bytes: 0,
        fetch_seconds: 0.0,
        apply_seconds: 1.0,
        held_deletions: 0,
        check: None,
        error: None,
        update: None,
        disk: None,
    };
    l::acknowledge(&f.manager, "mini", &first.id, &ack).unwrap();
    let conn = f.manager.store.open().unwrap();
    assert_eq!(
        confirmed(&conn, "mini").unwrap().unwrap().manifest_id,
        first.id
    );
    assert!(matches!(
        l::request(&f.manager, "mini", Some(&first.id)).unwrap(),
        RequestResult::Open(_)
    ));
}

#[test]
fn rate_defaults_medians_windows_and_device_isolation() {
    let f = Fixture::new();
    let conn = f.manager.store.open().unwrap();
    let default = parse_rate("18.75MB").unwrap();
    assert_eq!(default, 18_750_000.0);
    for s in ["0", "-1MB", "NaN", "inf", "garbage"] {
        assert!(parse_rate(s).is_err());
    }
    assert_eq!(rates(&conn, "mini", default).unwrap().effective, default);
    record_rate(&conn, "other", "fetch", 2_000_000_000, 1000.0).unwrap();
    record_rate(&conn, "mini", "fetch", 1_000_000_000, 1000.0).unwrap(); // threshold is strict
    assert_eq!(rates(&conn, "mini", default).unwrap().fetch, default);
    for _ in 0..10 {
        record_rate(&conn, "mini", "fetch", 2_000_000_000, 100.0).unwrap();
    }
    assert_eq!(rates(&conn, "mini", default).unwrap().fetch, 20_000_000.0);
    for _ in 0..10 {
        record_rate(&conn, "mini", "fetch", 2_000_000_000, 200.0).unwrap();
    }
    record_rate(&conn, "mini", "apply", 2_000_000_000, 400.0).unwrap();
    record_rate(&conn, "mini", "probe", 268_435_456, 268.435456).unwrap();
    assert_eq!(
        rates(&conn, "mini", default).unwrap().effective,
        1_000_000.0
    );
    for _ in 0..4 {
        record_rate(&conn, "mini", "probe", 268_435_456, 26.8435456).unwrap();
    }
    let r = rates(&conn, "mini", default).unwrap();
    assert_eq!(r.fetch, 10_000_000.0);
    assert_eq!(r.effective, 5_000_000.0);
    record_rate(&conn, "even", "apply", 2_000_000_000, 100.0).unwrap();
    record_rate(&conn, "even", "apply", 2_000_000_000, 200.0).unwrap();
    assert_eq!(rates(&conn, "even", default).unwrap().apply, 15_000_000.0);
    assert!(record_rate(&conn, "mini", "fetch", 1, f64::NAN).is_err());
}

#[test]
fn scope_reacts_to_rates_and_preserves_irreplaceable_sources() {
    let mut f = Fixture::new();
    f.artifact("fast", "live", true, false, 0);
    f.artifact("slow", "live", true, false, 100);
    f.artifact("gpu", "live", true, true, 0);
    f.artifact("unknown", "unrecorded", false, false, 0);
    f.artifact("no_recipe", "live", false, false, 0);
    f.artifact("frozen", "frozen", false, false, 0);
    let data = f.dir.join("data").display().to_string();
    f.manager.config.backup.static_paths = vec![data.clone()];
    f.manager.config.backup.holding_days.insert(data, 300);
    let scopes = collect_scope(&f.manager, "mini").unwrap();
    assert!(!scopes.iter().any(|s| s.segment == "fast"));
    for name in ["slow", "gpu", "unknown", "no_recipe", "frozen", "data"] {
        assert!(scopes.iter().any(|s| s.segment == name), "{name}");
    }
    let frozen = scopes.iter().find(|s| s.segment == "frozen").unwrap();
    assert!(frozen.source.ends_with("cold/.frozen/frozen"));
    let data = scopes.iter().find(|s| s.segment == "data").unwrap();
    assert!(data.plain);
    assert_eq!(data.holding_days, 300);
    record_rate(&f.manager.store.open().unwrap(), "mini", "probe", 1, 1000.0).unwrap();
    assert!(!collect_scope(&f.manager, "mini")
        .unwrap()
        .iter()
        .any(|s| s.segment == "slow"));
    fs::remove_dir_all(f.dir.join("data")).unwrap();
    assert!(collect_scope(&f.manager, "mini").is_err());
}

#[test]
fn outbox_layout_original_hashes_parts_delta_and_symlinks() {
    let f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    fs::write(f.dir.join("projects/code"), b"source code").unwrap();
    fs::write(f.dir.join("data/.DS_Store"), b"ignored").unwrap();
    fs::write(f.dir.join("data/._prices"), b"ignored").unwrap();
    symlink("code", f.dir.join("projects/link")).unwrap();
    let first = f.prepare("first", None);
    assert_eq!(first.transfer.changed.len(), 3);
    assert_eq!(first.parts.len(), 2);
    assert_eq!(first.manifest.holding_days["packed/projects"], 60);
    assert_eq!(
        first.manifest.files["packed/projects/link"]
            .target
            .as_deref(),
        Some("code")
    );
    let mut payloads = BTreeMap::new();
    for part in &first.parts {
        let path = first.directory.join(&part.file);
        assert_eq!(outbox::hash_file(&path).unwrap(), part.sha256);
        assert_eq!(fs::metadata(&path).unwrap().len(), part.bytes);
        let mut archive = tar::Archive::new(
            zstd::stream::read::Decoder::new(fs::File::open(path).unwrap()).unwrap(),
        );
        for file in archive.entries().unwrap() {
            let mut file = file.unwrap();
            let path = file.path().unwrap().to_str().unwrap().to_string();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            if path.starts_with("packed/") {
                bytes = zstd::stream::decode_all(bytes.as_slice()).unwrap();
            }
            assert_eq!(
                first.manifest.files[&path].sha256.as_deref(),
                Some(format!("{:x}", Sha256::digest(&bytes)).as_str())
            );
            payloads.insert(path, bytes);
        }
    }
    assert_eq!(payloads["plain/data/prices"], b"prices");
    assert_eq!(payloads["packed/projects/code.zst"], b"source code");
    let second = f.prepare("second", Some(&first.manifest));
    assert!(second.parts.is_empty());
    assert!(second.transfer.changed.is_empty());
    fs::remove_file(f.dir.join("data/prices")).unwrap();
    fs::write(f.dir.join("projects/code"), b"new source code").unwrap();
    let third = f.prepare("third", Some(&first.manifest));
    assert_eq!(third.transfer.deleted, vec!["plain/data/prices"]);
    assert_eq!(third.transfer.changed.len(), 1);
    assert!(prepare(&f.manager, &f.scopes(), None, "../escape").is_err());
    assert!(prepare(&f.manager, &f.scopes(), None, "first").is_err());
}

#[test]
fn changing_file_keeps_old_manifest_and_retries_next_time() {
    let f = Fixture::new();
    let path = f.dir.join("data/prices");
    fs::write(&path, b"old").unwrap();
    let first = f.prepare("first", None);
    fs::write(&path, b"changed").unwrap();
    outbox::AFTER_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|p| {
            fs::write(p, b"changed while reading").unwrap()
        }))
    });
    let second = f.prepare("second", Some(&first.manifest));
    assert_eq!(second.deferred, vec!["plain/data/prices"]);
    assert!(second.parts.is_empty());
    assert!(second.transfer.deleted.is_empty());
    assert_eq!(second.manifest.files, first.manifest.files);
    let third = f.prepare("third", Some(&second.manifest));
    assert_eq!(third.transfer.changed.len(), 1);
    assert_eq!(third.transfer.changed[0].size, 21);
}

#[test]
fn manifests_are_device_scoped_and_confirmation_is_atomic() {
    let f = Fixture::new();
    let mut conn = f.manager.store.open().unwrap();
    let first = Manifest {
        manifest_id: "one".into(),
        ..Default::default()
    };
    save_manifest(&conn, "mini", &first).unwrap();
    assert!(confirmed(&conn, "mini").unwrap().is_none());
    assert!(load_manifest(&conn, "other", "one").is_err());
    {
        let tx = conn.transaction().unwrap();
        confirm(&tx, "mini", "one").unwrap();
    } // rollback
    assert!(confirmed(&conn, "mini").unwrap().is_none());
    {
        let tx = conn.transaction().unwrap();
        confirm(&tx, "mini", "one").unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(confirmed(&conn, "mini").unwrap(), Some(first));
    let second = Manifest {
        manifest_id: "two".into(),
        base: Some("one".into()),
        ..Default::default()
    };
    let stale = Manifest {
        manifest_id: "stale".into(),
        base: Some("one".into()),
        ..Default::default()
    };
    save_manifest(&conn, "mini", &second).unwrap();
    save_manifest(&conn, "mini", &stale).unwrap();
    {
        let tx = conn.transaction().unwrap();
        confirm(&tx, "mini", "two").unwrap();
        confirm(&tx, "mini", "two").unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = conn.transaction().unwrap();
        assert!(confirm(&tx, "mini", "stale").is_err());
        assert!(confirm(&tx, "mini", "one").is_err());
    }
    assert_eq!(confirmed(&conn, "mini").unwrap(), Some(second));
}

#[test]
fn rejects_ambiguous_layout_unavailable_mount_and_capacity() {
    let mut f = Fixture::new();
    let scopes = f.scopes();
    let mut duplicate = scopes.clone();
    duplicate[1].segment = "DATA".into();
    assert!(prepare(&f.manager, &duplicate, None, "collision").is_err());
    moves::TEST_MOUNTED.set(false);
    assert!(prepare(&f.manager, &scopes, None, "unmounted").is_err());
    moves::TEST_MOUNTED.set(true);
    f.manager.config.floor_percent = 100;
    assert!(prepare(&f.manager, &scopes, None, "full").is_err());
    assert!(!f.dir.join("outbox/full").exists());
    f.manager.config.floor_percent = 5;
    fs::write(f.dir.join("projects/code"), b"source").unwrap();
    symlink("code", f.dir.join("projects/code.zst")).unwrap();
    assert!(prepare(&f.manager, &scopes, None, "alias").is_err());
    assert!(!f.dir.join("outbox/alias").exists());
}

#[test]
fn scope_counts_each_job_once_across_overlapping_output_sets() {
    let mut f = Fixture::new();
    for name in ["a", "b", "c"] {
        f.artifact(name, "live", true, false, 10);
    }
    let conn = f.manager.store.open().unwrap();
    conn.execute("UPDATE artifacts SET set_id='a'", []).unwrap();
    conn.execute("DELETE FROM artifact_jobs", []).unwrap();
    for (name, job) in [
        ("a", "first"),
        ("b", "first"),
        ("b", "second"),
        ("c", "second"),
    ] {
        conn.execute("INSERT INTO artifact_jobs(root,name,job_id,state,wall_seconds) VALUES(?1,?2,?3,'succeeded',10)", params![f.manager.config.roots[0].path,name,job]).unwrap();
    }
    // Each folder packs to 16 bytes. Rebuilding any member runs both 10s jobs.
    f.manager.config.backup.backup_rate_default = "1B".into();
    assert_eq!(collect_scope(&f.manager, "mini").unwrap().len(), 3);
    // At half the rate the return takes 32s, exceeding the actual 20s rebuild.
    // Counting shared jobs twice would incorrectly retain all three folders.
    f.manager.config.backup.backup_rate_default = "0.5B".into();
    assert!(collect_scope(&f.manager, "mini").unwrap().is_empty());
}

#[test]
fn request_lifecycle_is_device_scoped_and_reuses_open_requests() {
    use super::lifecycle::{self as l, RequestResult};
    let mut f = Fixture::new();
    f.manager.config.backup.enabled = true;
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let RequestResult::Open(first) = l::request(&f.manager, "mini", None).unwrap() else {
        panic!()
    };
    let RequestResult::Open(repeated) = l::request(&f.manager, "mini", Some("stale")).unwrap()
    else {
        panic!()
    };
    assert_eq!(first.id, repeated.id);
    assert!(l::status(&f.manager, "other", &first.id).is_err());
    l::prepare_request(&f.manager, &first.id).unwrap();
    let ready = l::status(&f.manager, "mini", &first.id).unwrap();
    assert_eq!(ready.state, "ready");
    assert_eq!(ready.manifest.as_ref().unwrap().changed.len(), 1);
    l::allow_deletes(&f.manager, "mini").unwrap();
    assert!(
        l::status(&f.manager, "mini", &first.id)
            .unwrap()
            .allow_deletes
    );
    assert!(
        l::status(&f.manager, "mini", &first.id)
            .unwrap()
            .allow_deletes
    );
    let full_part_bytes: u64 = ready.parts.iter().map(|part| part.bytes).sum();
    let ack = l::Ack {
        applied: true,
        fetched_bytes: 6,
        fetch_seconds: 1.0,
        apply_seconds: 1.0,
        held_deletions: 0,
        check: None,
        error: None,
        update: None,
        disk: None,
    };
    l::acknowledge(&f.manager, "mini", &first.id, &ack).unwrap();
    let conn = f.manager.store.open().unwrap();
    let apply_bytes: u64 = conn
        .query_row(
            "SELECT bytes FROM backup_rate_samples WHERE device='mini' AND kind='apply'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let fetch_bytes: u64 = conn
        .query_row(
            "SELECT bytes FROM backup_rate_samples WHERE device='mini' AND kind='fetch'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(apply_bytes, full_part_bytes);
    assert_eq!(fetch_bytes, ack.fetched_bytes);
    assert_ne!(apply_bytes, fetch_bytes);

    l::acknowledge(&f.manager, "mini", &first.id, &ack).unwrap();
    assert!(!f.dir.join("outbox").join(&first.id).exists());
    assert_eq!(
        l::status(&f.manager, "mini", &first.id).unwrap().state,
        "acked"
    );
    assert!(matches!(
        l::request(&f.manager, "mini", None).unwrap(),
        RequestResult::Mismatch(Some(_))
    ));
    let RequestResult::Open(next) = l::request(&f.manager, "mini", Some(&first.id)).unwrap() else {
        panic!()
    };
    l::prepare_request(&f.manager, &next.id).unwrap();
    let ready = l::status(&f.manager, "mini", &next.id).unwrap();
    assert!(ready.manifest.unwrap().changed.is_empty());
    assert!(!ready.allow_deletes);
    let failed = l::Ack {
        applied: false,
        error: Some("disk disconnected".into()),
        ..ack
    };
    l::acknowledge(&f.manager, "mini", &next.id, &failed).unwrap();
    assert_eq!(
        confirmed(&f.manager.store.open().unwrap(), "mini")
            .unwrap()
            .unwrap()
            .manifest_id,
        first.id
    );
}

#[test]
fn expired_request_retries_delta_and_alerts_recover_after_confirmation() {
    use super::lifecycle::{self as l, RequestResult};
    let mut f = Fixture::new();
    f.manager.config.backup.enabled = true;
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    l::missed_alerts(&f.manager, now - 3 * 86400).unwrap();
    l::missed_alerts(&f.manager, now).unwrap();
    l::missed_alerts(&f.manager, now).unwrap();
    let conn = f.manager.store.open().unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM backup_alerts", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let RequestResult::Open(first) = l::request(&f.manager, "mini", None).unwrap() else {
        panic!()
    };
    l::prepare_request(&f.manager, &first.id).unwrap();
    l::expire(&f.manager, now + 49 * 3600).unwrap();
    assert_eq!(
        l::status(&f.manager, "mini", &first.id).unwrap().state,
        "expired"
    );
    assert!(!f.dir.join("outbox").join(&first.id).exists());
    let RequestResult::Open(next) = l::request(&f.manager, "mini", None).unwrap() else {
        panic!()
    };
    l::prepare_request(&f.manager, &next.id).unwrap();
    assert_eq!(
        l::status(&f.manager, "mini", &next.id)
            .unwrap()
            .manifest
            .unwrap()
            .changed
            .len(),
        1
    );
    let ack = l::Ack {
        applied: true,
        fetched_bytes: 6,
        fetch_seconds: 1.0,
        apply_seconds: 1.0,
        held_deletions: 1,
        check: Some(l::Check {
            ok: false,
            files_checked: 1,
            bad: vec!["plain/data/prices".into()],
        }),
        error: None,
        update: None,
        disk: None,
    };
    // The fixture's configured device is backup-mini; recovery is per device.
    crate::managed_artifacts::store::set_meta(&conn, "backup_missed:mini", "1").unwrap();
    l::acknowledge(&f.manager, "mini", &next.id, &ack).unwrap();
    // Held deletions need the owner; a failed check is agent work (sm#2414).
    assert_eq!(
        conn.query_row(
            "SELECT title FROM backup_alerts WHERE blocking=1",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "Backup deletions need confirmation"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM backup_alerts WHERE blocking=0 AND title='Backup integrity check failed'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM backup_alerts WHERE title='Backup is confirming again'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn incremental_capacity_estimate_counts_only_changed_files() {
    let f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let first = f.prepare("first", None);
    let mut scopes = f.scopes();
    scopes[0].estimated_bytes = u64::MAX;
    assert_eq!(
        outbox::pending_input(&scopes, Some(&first.manifest)).unwrap(),
        (0, 0)
    );
    assert!(
        prepare(&f.manager, &scopes, Some(&first.manifest), "unchanged")
            .unwrap()
            .parts
            .is_empty()
    );
    fs::write(f.dir.join("data/new"), b"new").unwrap();
    scopes[0].estimated_bytes = 90;
    assert_eq!(
        outbox::pending_input(&scopes, Some(&first.manifest)).unwrap(),
        (3, 30)
    );
}

#[test]
fn client_round_trip_history_deletion_guard_and_lost_ack_replay() {
    use super::client::Destination;
    let f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    fs::write(f.dir.join("projects/code"), b"source code").unwrap();
    symlink("code", f.dir.join("projects/link")).unwrap();
    let first = f.prepare("first", None);
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let staging = dest
        .unpack("first", &first.transfer, &first.parts, &first.directory)
        .unwrap();
    let applied = dest
        .apply("first", &first.transfer, &staging, false, 0.1)
        .unwrap();
    assert_eq!(applied.files, 3);
    assert!(dest.state().unwrap().confirmed.is_none());
    assert_eq!(
        dest.apply("first", &first.transfer, &staging, false, 0.1)
            .unwrap()
            .files,
        3
    );
    dest.confirm("first").unwrap();
    fs::write(f.dir.join("data/prices"), b"new prices").unwrap();
    fs::remove_file(f.dir.join("projects/code")).unwrap();
    let second = f.prepare("second", Some(&first.manifest));
    let staging = dest
        .unpack("second", &second.transfer, &second.parts, &second.directory)
        .unwrap();
    assert_eq!(
        dest.apply("second", &second.transfer, &staging, false, 0.1)
            .unwrap()
            .held_deletions,
        1
    );
    assert!(dest.root.join("packed/projects/code.zst").exists());
    dest.restore(&f.dir.join("restored"), None, &[]).unwrap();
    assert_eq!(
        fs::read(f.dir.join("restored/data/prices")).unwrap(),
        b"new prices"
    );
    assert_eq!(
        fs::read(f.dir.join("restored/projects/code")).unwrap(),
        b"source code"
    );
    assert_eq!(
        fs::read_link(f.dir.join("restored/projects/link")).unwrap(),
        PathBuf::from("code")
    );
    let third = f.prepare("third", Some(&second.manifest));
    let staging = dest
        .unpack("third", &third.transfer, &third.parts, &third.directory)
        .unwrap();
    assert_eq!(
        dest.apply("third", &third.transfer, &staging, true, 0.1)
            .unwrap()
            .held_deletions,
        0
    );
    assert!(!dest.root.join("packed/projects/code.zst").exists());
    // Choose a time immediately before the replacement, ignoring initial creation.
    dest.conn
        .execute("UPDATE history SET held_at=100 WHERE run LIKE '%first'", [])
        .unwrap();
    dest.conn
        .execute(
            "UPDATE history SET held_at=300 WHERE run NOT LIKE '%first'",
            [],
        )
        .unwrap();
    dest.restore(&f.dir.join("historical"), Some(200), &[])
        .unwrap();
    assert_eq!(
        fs::read(f.dir.join("historical/data/prices")).unwrap(),
        b"prices"
    );
    assert_eq!(
        fs::read(f.dir.join("historical/projects/code")).unwrap(),
        b"source code"
    );
    dest.expire_history(31 * 86400 + 300).unwrap();
    assert_eq!(
        dest.conn
            .query_row(
                "SELECT COUNT(*) FROM history WHERE path LIKE 'plain/%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert!(
        dest.conn
            .query_row(
                "SELECT COUNT(*) FROM history WHERE path LIKE 'packed/%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap()
            > 0
    );
    dest.expire_history(61 * 86400 + 300).unwrap();
    assert_eq!(
        dest.conn
            .query_row("SELECT COUNT(*) FROM history", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn destination_open_waits_out_a_briefly_held_lock() {
    use super::client::Destination;
    use std::os::unix::io::AsRawFd;
    let f = Fixture::new();
    let root = f.dir.join("remote");
    fs::create_dir_all(&root).unwrap();
    // Acquire the fixture's lock first. Opening and dropping a Destination here
    // races unrelated forks that can briefly inherit that first descriptor.
    let held = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(root.join(".sm-backup.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let started = std::time::Instant::now();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(held);
    });
    let opened = Destination::open(&root);
    let elapsed = started.elapsed();
    release.join().unwrap();
    opened.unwrap();
    assert!(elapsed >= std::time::Duration::from_millis(200));
}

#[test]
fn client_detects_corruption_and_refuses_untracked_file_loss_and_symlink_escape() {
    use super::client::Destination;
    let f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let first = f.prepare("first", None);
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    assert!(Destination::open(&dest.root).is_err());
    let staging = dest
        .unpack("first", &first.transfer, &first.parts, &first.directory)
        .unwrap();
    fs::create_dir_all(dest.root.join("plain/data")).unwrap();
    fs::write(dest.root.join("plain/data/prices"), b"untracked").unwrap();
    assert!(dest
        .apply("first", &first.transfer, &staging, false, 0.1)
        .is_err());
    assert_eq!(
        fs::read(dest.root.join("plain/data/prices")).unwrap(),
        b"untracked"
    );
    fs::remove_file(dest.root.join("plain/data/prices")).unwrap();
    dest.apply("first", &first.transfer, &staging, false, 0.1)
        .unwrap();
    assert!(dest.check().unwrap().ok);
    fs::write(dest.root.join("plain/data/prices"), b"broken").unwrap();
    assert!(!dest.check().unwrap().ok);
    fs::write(first.directory.join(&first.parts[0].file), b"bad part").unwrap();
    assert!(dest
        .unpack("corrupt", &first.transfer, &first.parts, &first.directory)
        .is_err());
    let other = Destination::open(&f.dir.join("other")).unwrap();
    symlink(f.dir.join("data"), other.root.join("plain")).unwrap();
    let second = f.prepare("second", None);
    let mut other = other;
    let stage = other
        .unpack("second", &second.transfer, &second.parts, &second.directory)
        .unwrap();
    assert!(other
        .apply("second", &second.transfer, &stage, false, 0.1)
        .is_err());
    assert_eq!(fs::read(f.dir.join("data/prices")).unwrap(), b"prices");
}

#[test]
fn client_resumes_interrupted_download_and_recovers_a_lost_ack_response() {
    use super::{
        client::Destination,
        lifecycle::RequestStatus,
        pull::{self, Remote},
    };
    struct Fake {
        status: RequestStatus,
        bytes: Vec<u8>,
        interrupt: bool,
        lose_ack: bool,
        acked: bool,
        offsets: Vec<u64>,
    }
    impl Remote for Fake {
        fn json(
            &mut self,
            method: &str,
            path: &str,
            _body: Option<serde_json::Value>,
        ) -> Result<(u16, serde_json::Value)> {
            if method == "POST" && path.ends_with("/ack") {
                self.acked = true;
                if self.lose_ack {
                    self.lose_ack = false;
                    anyhow::bail!("lost acknowledgement response");
                }
                return Ok((200, serde_json::json!({"ok":true})));
            }
            let mut status = self.status.clone();
            if self.acked {
                status.state = "acked".into();
            }
            Ok((200, serde_json::to_value(status)?))
        }
        fn download(
            &mut self,
            _id: &str,
            _part: &Part,
            offset: u64,
            writer: &mut dyn std::io::Write,
        ) -> Result<()> {
            self.offsets.push(offset);
            if self.interrupt {
                self.interrupt = false;
                writer.write_all(&self.bytes[..7])?;
                anyhow::bail!("connection lost");
            }
            writer.write_all(&self.bytes[offset as usize..])?;
            Ok(())
        }
        fn probe(&mut self) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
    }
    let f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let first = f.prepare("first", None);
    let bytes = fs::read(first.directory.join(&first.parts[0].file)).unwrap();
    let mut fake = Fake {
        status: RequestStatus {
            id: "first".into(),
            state: "ready".into(),
            retry_after_seconds: None,
            estimate_bytes: 6,
            manifest_id: Some("first".into()),
            parts: first.parts,
            manifest: Some(first.transfer),
            deletions: vec![],
            allow_deletes: false,
            max_delete_fraction: 0.1,
            error: None,
            space_floor_bytes: Some(0),
        },
        bytes,
        interrupt: true,
        lose_ack: true,
        acked: false,
        offsets: vec![],
    };
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    fake.status.manifest.as_mut().unwrap().base = Some("another-disk".into());
    let error = pull::pull(&mut dest, &mut fake).unwrap_err();
    assert!(error.to_string().contains("delta base does not match"));
    assert!(fake.offsets.is_empty());
    assert!(dest.records().unwrap().is_empty());
    assert!(!fake.acked);
    fake.status.manifest.as_mut().unwrap().base = None;
    assert!(pull::pull(&mut dest, &mut fake).is_err());
    assert!(dest.state().unwrap().confirmed.is_none());
    assert!(pull::pull(&mut dest, &mut fake).is_err());
    assert_eq!(fake.offsets, vec![0, 7]);
    assert!(fake.acked);
    assert!(dest.state().unwrap().confirmed.is_none());
    let history: i64 = dest
        .conn
        .query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))
        .unwrap();
    assert!(pull::pull(&mut dest, &mut fake).unwrap().applied);
    assert_eq!(dest.state().unwrap().confirmed.as_deref(), Some("first"));
    assert!(dest.meta("pending_request").unwrap().is_none());
    assert!(dest.meta("pending_ack").unwrap().is_none());
    assert!(!dest.root.join(".downloads/first").exists());
    assert!(!dest.root.join(".incoming/first").exists());
    assert_eq!(
        dest.conn
            .query_row("SELECT COUNT(*) FROM history", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        history
    );
}

#[test]
fn seed_adopts_plain_copy_confirms_full_scope_and_next_request_is_empty() {
    use super::{client::Destination, lifecycle, seed};
    let mut f = Fixture::new();
    f.manager.config.backup.enabled = true;
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    fs::create_dir_all(f.dir.join("remote/existing-data")).unwrap();
    fs::copy(
        f.dir.join("data/prices"),
        f.dir.join("remote/existing-data/prices"),
    )
    .unwrap();
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let id = seed::seed(
        &f.manager,
        "backup-mini",
        &mut dest,
        Some(&f.dir.join("remote/existing-data")),
        false,
    )
    .unwrap();
    assert!(!f.dir.join("remote/existing-data").exists());
    assert_eq!(
        fs::read(dest.root.join("plain/data/prices")).unwrap(),
        b"prices"
    );
    assert_eq!(
        dest.state().unwrap().confirmed.as_deref(),
        Some(id.as_str())
    );
    assert_eq!(
        confirmed(&f.manager.store.open().unwrap(), "backup-mini")
            .unwrap()
            .unwrap()
            .manifest_id,
        id
    );
    let parts: String = f
        .manager
        .store
        .open()
        .unwrap()
        .query_row(
            "SELECT parts_json FROM backup_requests WHERE id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(parts, "[]", "adoption must not stage another payload copy");
    let lifecycle::RequestResult::Open(request) =
        lifecycle::request(&f.manager, "backup-mini", Some(&id)).unwrap()
    else {
        panic!()
    };
    lifecycle::prepare_request(&f.manager, &request.id).unwrap();
    let status = lifecycle::status(&f.manager, "backup-mini", &request.id).unwrap();
    assert!(status.parts.is_empty());
    assert!(status.manifest.unwrap().changed.is_empty());
}

#[test]
fn seed_rejects_mismatched_adoption_without_moving_original() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    fs::create_dir(f.dir.join("existing-data")).unwrap();
    fs::write(f.dir.join("existing-data/prices"), b"old").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    assert!(super::seed::seed(
        &f.manager,
        "backup-mini",
        &mut dest,
        Some(&f.dir.join("existing-data")),
        false
    )
    .is_err());
    assert_eq!(
        fs::read(f.dir.join("existing-data/prices")).unwrap(),
        b"old"
    );
    assert!(confirmed(&f.manager.store.open().unwrap(), "backup-mini")
        .unwrap()
        .is_none());
    assert_eq!(
        fs::read_dir(f.dir.join("outbox"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0
    );
}

#[test]
fn seed_rejects_managed_adoption_and_cleans_outbox() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let source = dest.directory("packed/existing").unwrap();
    assert!(super::seed::seed(&f.manager, "backup-mini", &mut dest, Some(&source), false).is_err());
    assert!(source.exists());
    assert_eq!(
        fs::read_dir(f.dir.join("outbox"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0
    );
}

#[test]
fn new_files_create_no_holding_folders_and_replaced_ones_are_held() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::create_dir_all(f.dir.join("data/a/b")).unwrap();
    fs::write(f.dir.join("data/a/b/prices"), b"prices").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    let held = |dest: &super::client::Destination| -> Vec<PathBuf> {
        walkdir(&dest.root.join(".holding"))
    };
    // Nothing was replaced, so no folder tree mirrors the new files in holding.
    assert!(held(&dest)
        .iter()
        .all(|p| !p.ends_with("a") && !p.ends_with("b")));
    fs::write(f.dir.join("data/a/b/prices"), b"new prices").unwrap();
    super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    let kept = held(&dest);
    let old = kept
        .iter()
        .find(|p| p.ends_with("plain/data/a/b/prices"))
        .unwrap();
    assert_eq!(fs::read(old).unwrap(), b"prices");
}
fn walkdir(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walkdir(&path));
        }
        out.push(path);
    }
    out
}

#[test]
fn reseed_removes_files_absent_from_full_source() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    fs::remove_file(f.dir.join("data/prices")).unwrap();
    super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    assert!(dest.records().unwrap().is_empty());
    assert!(!dest.root.join("plain/data/prices").exists());
}

#[test]
fn expired_applied_request_rolls_back_before_requesting_another_delta() {
    use super::{
        client::Destination,
        pull::{self, Remote},
    };
    struct Expired;
    impl Remote for Expired {
        fn json(
            &mut self,
            method: &str,
            _path: &str,
            body: Option<serde_json::Value>,
        ) -> Result<(u16, serde_json::Value)> {
            if method == "POST" {
                assert_eq!(body.unwrap()["have"], "confirmed");
                anyhow::bail!("stop after rollback before next download");
            }
            Ok((
                200,
                serde_json::json!({"id":"expired","state":"expired","estimate_bytes":0,"parts":[],"deletions":[],"allow_deletes":false,"max_delete_fraction":0.1}),
            ))
        }
        fn download(
            &mut self,
            _: &str,
            _: &Part,
            _: u64,
            _: &mut dyn std::io::Write,
        ) -> Result<()> {
            panic!("no download expected")
        }
        fn probe(&mut self) -> Result<serde_json::Value> {
            panic!("no probe expected")
        }
    }
    let f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"A").unwrap();
    fs::write(f.dir.join("data/deleted"), b"keep").unwrap();
    let first = f.prepare("confirmed", None);
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let staging = dest
        .unpack("confirmed", &first.transfer, &first.parts, &first.directory)
        .unwrap();
    dest.apply("confirmed", &first.transfer, &staging, true, 1.0)
        .unwrap();
    dest.confirm("confirmed").unwrap();
    let original = dest.records().unwrap();
    fs::write(f.dir.join("data/prices"), b"BBBB").unwrap();
    fs::remove_file(f.dir.join("data/deleted")).unwrap();
    fs::write(f.dir.join("data/created"), b"new").unwrap();
    let next = f.prepare("expired", Some(&first.manifest));
    let staging = dest
        .unpack("expired", &next.transfer, &next.parts, &next.directory)
        .unwrap();
    dest.apply("expired", &next.transfer, &staging, true, 1.0)
        .unwrap();
    dest.set_meta("pending_request", "expired").unwrap();
    let downloads = dest.directory(".downloads/expired").unwrap();
    fs::write(downloads.join("partial"), b"partial").unwrap();
    let error = pull::pull(&mut dest, &mut Expired).unwrap_err();
    assert!(
        error.to_string().contains("stop after rollback"),
        "{error:#}"
    );
    assert_eq!(dest.records().unwrap(), original);
    assert_eq!(fs::read(dest.root.join("plain/data/prices")).unwrap(), b"A");
    assert_eq!(
        fs::read(dest.root.join("plain/data/deleted")).unwrap(),
        b"keep"
    );
    assert!(!dest.root.join("plain/data/created").exists());
    assert!(!dest.root.join(".incoming/expired").exists());
    assert!(!downloads.exists());
    assert!(dest.meta("pending_request").unwrap().is_none());
    // A crash after rollback but before clearing pending must be repeatable.
    dest.rollback("expired").unwrap();
    assert_eq!(dest.records().unwrap(), original);
}

#[test]
fn seed_refuses_active_pull_before_changing_destination() {
    let mut f = Fixture::new();
    f.manager.config.backup.enabled = true;
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"A").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let first = super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    let records = dest.records().unwrap();
    lifecycle::request(&f.manager, "backup-mini", Some(&first)).unwrap();
    fs::write(f.dir.join("data/prices"), b"BBBB").unwrap();
    let error = super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap_err();
    assert!(error
        .to_string()
        .contains("remote backup request is active"));
    assert_eq!(dest.records().unwrap(), records);
    assert_eq!(fs::read(dest.root.join("plain/data/prices")).unwrap(), b"A");
    assert_eq!(
        dest.state().unwrap().confirmed.as_deref(),
        Some(first.as_str())
    );
    // A seed stages on the destination and leaves nothing in the outbox.
    let outbox = f.dir.join("outbox");
    assert!(!outbox.exists() || fs::read_dir(outbox).unwrap().count() == 0);
}

#[test]
fn seed_allows_thaw_and_input_submission_during_destination_work() {
    use crate::managed_artifacts::{ops, submission};
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    f.artifact("input", "frozen", false, false, 0);
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let root = &f.manager.config.roots[0];
    let request = serde_json::from_value(serde_json::json!({
        "job_type":"background", "label":"reader", "notify_session_id":"reader",
        "cwd":f.dir, "argv":["/usr/bin/true"], "env":{}, "timeout_seconds":60
    }))
    .unwrap();
    let prepared = submission::PreparedSubmission::prepare(
        &f.manager,
        root,
        submission::Declarations {
            inputs: vec!["input".into()],
            ..Default::default()
        },
        &request,
    )
    .unwrap();
    let make_writer = |id: &'static str| -> Box<dyn FnOnce()> {
        let manager = f.manager.clone();
        let prepared = prepared.clone();
        let queue_path = f.dir.join("queue.db");
        Box::new(move || {
            // This independent connection must acquire the writer immediately
            // while seed is paused at either end of destination application.
            let mut queue = rusqlite::Connection::open(queue_path).unwrap();
            queue.busy_timeout(std::time::Duration::ZERO).unwrap();
            submission::attach_store(&queue, &manager.store).unwrap();
            let tx = queue
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .expect("seed must leave the artifact writer available");
            prepared.persist(&tx, id).unwrap();
            tx.commit().unwrap();
            ops::request(
                &manager,
                &manager.config.roots[0],
                "input",
                "thawing",
                "reader",
            )
            .unwrap();
        })
    };
    seed::BEFORE_APPLY.with(|hook| *hook.borrow_mut() = Some(make_writer("before")));
    seed::AFTER_APPLY.with(|hook| *hook.borrow_mut() = Some(make_writer("after")));
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let id = seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    assert_eq!(
        dest.state().unwrap().confirmed.as_deref(),
        Some(id.as_str())
    );
    let conn = f.manager.store.open().unwrap();
    let reservations: i64 = conn
        .query_row("SELECT COUNT(*) FROM artifact_job_reservations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(reservations, 2);
    assert_eq!(
        f.manager.store.detail(&root.path, "input").unwrap()["op"],
        "thawing"
    );
}

#[test]
fn seed_rolls_back_when_a_pull_starts_or_head_changes_during_application() {
    for advance_head in [false, true] {
        let mut f = Fixture::new();
        f.manager.config.backup.enabled = true;
        f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
        fs::write(f.dir.join("data/prices"), b"original").unwrap();
        let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
        let first = seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
        let records = dest.records().unwrap();
        fs::write(f.dir.join("data/prices"), b"replacement").unwrap();
        let manager = f.manager.clone();
        let base = first.clone();
        seed::AFTER_APPLY.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                if advance_head {
                    let mut conn = manager.store.open().unwrap();
                    let tx = conn.transaction().unwrap();
                    save_manifest(
                        &tx,
                        "backup-mini",
                        &Manifest {
                            manifest_id: "competing-seed".into(),
                            base: Some(base),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                    confirm(&tx, "backup-mini", "competing-seed").unwrap();
                    tx.commit().unwrap();
                } else {
                    assert!(matches!(
                        lifecycle::request(&manager, "backup-mini", Some(&base)).unwrap(),
                        lifecycle::RequestResult::Open(_)
                    ));
                }
            }));
        });
        let error = seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap_err();
        assert!(
            error.to_string().contains(if advance_head {
                "backup head changed"
            } else {
                "remote backup request is active"
            }),
            "{error:#}"
        );
        assert_eq!(dest.records().unwrap(), records);
        assert_eq!(
            fs::read(dest.root.join("plain/data/prices")).unwrap(),
            b"original"
        );
        assert_eq!(
            dest.state().unwrap().confirmed.as_deref(),
            Some(first.as_str())
        );
        assert!(dest.meta("pending_seed").unwrap().is_none());
        let staging = seed_staging(&dest).unwrap();
        assert_eq!(
            fs::read(staging.join("plain/data/prices")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            confirmed(&f.manager.store.open().unwrap(), "backup-mini")
                .unwrap()
                .unwrap()
                .manifest_id,
            if advance_head {
                "competing-seed"
            } else {
                &first
            }
        );
    }
}

#[test]
fn failed_seed_application_keeps_destination_staging() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"source").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let plain = dest.directory("plain/data").unwrap();
    fs::write(plain.join("prices"), b"untracked").unwrap();
    assert!(super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).is_err());
    let staging = seed_staging(&dest).unwrap();
    assert_eq!(
        fs::read(staging.join("plain/data/prices")).unwrap(),
        b"source"
    );
    assert_eq!(fs::read(plain.join("prices")).unwrap(), b"untracked");
    assert!(!f.dir.join("outbox").exists());
}

#[test]
fn failed_seed_returns_more_than_one_batch_of_placed_files_to_staging() {
    // Recovery moves files back 512 at a time under one barrier each (#2403).
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    for i in 0..1100 {
        let dir = f.dir.join(format!("data/a/{}", i % 7));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("f{i}")), format!("source {i}")).unwrap();
    }
    fs::write(f.dir.join("data/zzz"), b"source").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    fs::write(
        dest.directory("plain/data").unwrap().join("zzz"),
        b"untracked",
    )
    .unwrap();
    let error = super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap_err();
    assert!(error
        .to_string()
        .contains("untracked destination file differs"));
    let staging = seed_staging(&dest).unwrap();
    for i in 0..1100 {
        let path = format!("plain/data/a/{}/f{i}", i % 7);
        assert_eq!(
            fs::read(staging.join(&path)).unwrap(),
            format!("source {i}").as_bytes()
        );
        assert!(!dest.root.join(&path).exists(), "{path} left in place");
    }
    assert!(dest.records().unwrap().is_empty());
    assert_eq!(
        fs::read(dest.root.join("plain/data/zzz")).unwrap(),
        b"untracked"
    );
    fs::remove_file(dest.root.join("plain/data/zzz")).unwrap();
    super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    assert_eq!(
        fs::read(dest.root.join("plain/data/a/3/f10")).unwrap(),
        b"source 10"
    );
}

#[test]
fn failed_seed_rolls_back_files_already_replaced() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"original").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let first = super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    let original = dest.records().unwrap();
    fs::write(f.dir.join("data/prices"), b"replacement").unwrap();
    fs::write(f.dir.join("data/zzz"), b"source").unwrap();
    fs::write(dest.root.join("plain/data/zzz"), b"untracked").unwrap();
    let error = super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap_err();
    assert!(error
        .to_string()
        .contains("untracked destination file differs"));
    assert_eq!(dest.records().unwrap(), original);
    assert_eq!(
        fs::read(dest.root.join("plain/data/prices")).unwrap(),
        b"original"
    );
    assert_eq!(
        fs::read(dest.root.join("plain/data/zzz")).unwrap(),
        b"untracked"
    );
    assert_eq!(
        dest.state().unwrap().confirmed.as_deref(),
        Some(first.as_str())
    );
    assert_eq!(
        confirmed(&f.manager.store.open().unwrap(), "backup-mini")
            .unwrap()
            .unwrap()
            .manifest_id,
        first
    );
    // The replacement apply had already moved into place is staged again.
    let staging = seed_staging(&dest).unwrap();
    assert_eq!(
        fs::read(staging.join("plain/data/prices")).unwrap(),
        b"replacement"
    );
    assert_eq!(fs::read(staging.join("plain/data/zzz")).unwrap(), b"source");
}

#[test]
fn failed_weekly_integrity_check_never_confirms_and_blocks_unrepaired_retry() {
    use super::{
        client::Destination,
        pull::{self, Remote},
    };
    struct Fake {
        transfer: outbox::TransferManifest,
        ack: Option<lifecycle::Ack>,
        requests: usize,
    }
    impl Remote for Fake {
        fn json(
            &mut self,
            _: &str,
            path: &str,
            body: Option<serde_json::Value>,
        ) -> Result<(u16, serde_json::Value)> {
            if path.ends_with("/ack") {
                self.ack = Some(serde_json::from_value(body.unwrap())?);
                return Ok((200, serde_json::json!({})));
            }
            self.requests += 1;
            Ok((
                200,
                serde_json::json!({"id":"weekly","state":"ready","estimate_bytes":0,"parts":[],"manifest":self.transfer,"deletions":[],"allow_deletes":false,"max_delete_fraction":0.1}),
            ))
        }
        fn download(
            &mut self,
            _: &str,
            _: &Part,
            _: u64,
            _: &mut dyn std::io::Write,
        ) -> Result<()> {
            panic!("empty delta")
        }
        fn probe(&mut self) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
    }
    let f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"original").unwrap();
    let first = f.prepare("confirmed", None);
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let staging = dest
        .unpack("confirmed", &first.transfer, &first.parts, &first.directory)
        .unwrap();
    dest.apply("confirmed", &first.transfer, &staging, true, 1.0)
        .unwrap();
    dest.confirm("confirmed").unwrap();
    fs::write(dest.root.join("plain/data/prices"), b"damaged").unwrap();
    let next = f.prepare("weekly", Some(&first.manifest));
    let mut fake = Fake {
        transfer: next.transfer,
        ack: None,
        requests: 0,
    };
    assert!(pull::pull_with_check(&mut dest, &mut fake, true).is_err());
    let ack = fake.ack.as_ref().unwrap();
    assert!(!ack.applied);
    assert!(!ack.check.as_ref().unwrap().ok);
    assert_eq!(
        dest.state().unwrap().confirmed.as_deref(),
        Some("confirmed")
    );
    assert!(dest.meta("pending_request").unwrap().is_none());
    let requests = fake.requests;
    assert!(pull::pull_with_check(&mut dest, &mut fake, false).is_err());
    assert_eq!(fake.requests, requests);
    fs::write(dest.root.join("plain/data/prices"), b"original").unwrap();
    assert!(
        pull::pull_with_check(&mut dest, &mut fake, true)
            .unwrap()
            .applied
    );
    assert_eq!(dest.state().unwrap().confirmed.as_deref(), Some("weekly"));
}

#[test]
fn failed_adoption_returns_original_folder_without_registering_records() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![
        f.dir.join("data").display().to_string(),
        f.dir.join("projects").display().to_string(),
    ];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    fs::write(f.dir.join("projects/code"), b"source").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let source = dest.directory("existing-data").unwrap();
    fs::copy(f.dir.join("data/prices"), source.join("prices")).unwrap();
    let packed = dest.directory("packed/projects").unwrap();
    fs::write(packed.join("code.zst"), b"untracked").unwrap();
    assert!(super::seed::seed(&f.manager, "backup-mini", &mut dest, Some(&source), false).is_err());
    assert!(dest.records().unwrap().is_empty());
    assert!(dest.state().unwrap().confirmed.is_none());
    assert_eq!(fs::read(source.join("prices")).unwrap(), b"prices");
    assert!(!dest.root.join("plain/data").exists());
    let staging = seed_staging(&dest).unwrap();
    assert!(staging.join("packed/projects/code.zst").is_file());
    assert!(
        !staging.join("plain").exists(),
        "adopted files are never staged"
    );
}

#[test]
fn adoption_sync_failure_prevents_confirmation_and_preserves_recovery_journal() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let source = dest.directory("existing-data").unwrap();
    fs::copy(f.dir.join("data/prices"), source.join("prices")).unwrap();

    seed::FAIL_ADOPTION_SYNC.set(true);
    let result = seed::seed_inner(&f.manager, "backup-mini", &mut dest, Some(&source));
    seed::FAIL_ADOPTION_SYNC.set(false);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("directory sync failure"));
    assert!(dest.state().unwrap().confirmed.is_none());
    assert!(confirmed(&f.manager.store.open().unwrap(), "backup-mini")
        .unwrap()
        .is_none());
    assert!(dest.records().unwrap().is_empty());
    assert_eq!(fs::read(source.join("prices")).unwrap(), b"prices");
    assert!(!dest.root.join("plain/data").exists());
    assert!(dest.meta("pending_seed").unwrap().is_some());

    // The guard already returned the folder, but its sync failed too. Recovery
    // must retry the sync even though no reverse rename remains to perform.
    seed::FAIL_ADOPTION_SYNC.set(true);
    let result = seed::seed(&f.manager, "unknown-device", &mut dest, None, false);
    seed::FAIL_ADOPTION_SYNC.set(false);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("directory sync failure"));
    assert!(dest.meta("pending_seed").unwrap().is_some());
    drop(dest);
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let error = seed::seed(&f.manager, "unknown-device", &mut dest, None, false).unwrap_err();
    assert!(error.to_string().contains("unknown backup device"));
    assert!(dest.meta("pending_seed").unwrap().is_none());
    assert_eq!(fs::read(source.join("prices")).unwrap(), b"prices");
}

#[test]
fn adoption_recovery_keeps_journal_until_reverse_rename_is_synced() {
    let f = Fixture::new();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let source = dest.root.join("existing-data");
    let target = dest.directory("plain/data").unwrap();
    fs::write(target.join("prices"), b"prices").unwrap();
    dest.set_meta(
        "pending_seed",
        &serde_json::json!({
            "id": "seed-interrupted", "manifest": Manifest::default(),
            "adoption": [source, target]
        })
        .to_string(),
    )
    .unwrap();
    seed::FAIL_ADOPTION_SYNC.set(true);
    let result = seed::seed(&f.manager, "unknown-device", &mut dest, None, false);
    seed::FAIL_ADOPTION_SYNC.set(false);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("directory sync failure"));
    assert_eq!(fs::read(source.join("prices")).unwrap(), b"prices");
    assert!(!target.exists());
    assert!(dest.meta("pending_seed").unwrap().is_some());
    drop(dest);
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let error = seed::seed(&f.manager, "unknown-device", &mut dest, None, false).unwrap_err();
    assert!(error.to_string().contains("unknown backup device"));
    assert!(dest.meta("pending_seed").unwrap().is_none());
    assert_eq!(fs::read(source.join("prices")).unwrap(), b"prices");
}

#[test]
fn failed_seed_confirmation_rolls_back_applied_destination() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"original").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let first = super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    let original = dest.records().unwrap();
    f.manager.store.open().unwrap().execute_batch("CREATE TRIGGER fail_seed_confirmation BEFORE INSERT ON backup_requests BEGIN SELECT RAISE(ABORT, 'injected confirmation failure'); END;").unwrap();
    fs::write(f.dir.join("data/prices"), b"replacement").unwrap();
    let error = super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap_err();
    assert!(
        error.to_string().contains("injected confirmation failure"),
        "{error:#}"
    );
    assert_eq!(dest.records().unwrap(), original);
    assert_eq!(
        fs::read(dest.root.join("plain/data/prices")).unwrap(),
        b"original"
    );
    assert_eq!(
        dest.state().unwrap().confirmed.as_deref(),
        Some(first.as_str())
    );
    assert_eq!(
        confirmed(&f.manager.store.open().unwrap(), "backup-mini")
            .unwrap()
            .unwrap()
            .manifest_id,
        first
    );
    let staging = seed_staging(&dest).unwrap();
    assert_eq!(
        fs::read(staging.join("plain/data/prices")).unwrap(),
        b"replacement"
    );
}

#[test]
fn adoption_rejects_file_removed_during_preparation() {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![
        f.dir.join("data").display().to_string(),
        f.dir.join("projects").display().to_string(),
    ];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    fs::write(f.dir.join("projects/code"), b"code").unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    let source = dest.directory("existing-data").unwrap();
    fs::copy(f.dir.join("data/prices"), source.join("prices")).unwrap();
    let removed = source.join("prices");
    outbox::AFTER_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |_| {
            fs::remove_file(removed).unwrap();
        }))
    });
    let error =
        super::seed::seed(&f.manager, "backup-mini", &mut dest, Some(&source), false).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("adopted file missing or changed"),
        "{error:#}"
    );
    assert!(dest.records().unwrap().is_empty());
    assert!(dest.state().unwrap().confirmed.is_none());
    assert!(confirmed(&f.manager.store.open().unwrap(), "backup-mini")
        .unwrap()
        .is_none());
    assert!(source.exists());
    assert!(!dest.root.join("plain/data").exists());
}

#[test]
fn restore_requires_initialized_source_and_preserves_file_mtimes() {
    use super::client::Destination;
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let missing = f.dir.join("missing");
    assert!(Destination::open_existing(&missing).is_err());
    assert!(!missing.exists());
    assert!(Destination::open_existing(&f.dir.join("data")).is_err());
    assert!(!f.dir.join("data/state.db").exists());
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    fs::write(f.dir.join("projects/code"), b"code").unwrap();
    let timestamp = std::time::UNIX_EPOCH + std::time::Duration::new(1_600_000_000, 123_456_789);
    for path in ["data/prices", "projects/code"] {
        fs::File::open(f.dir.join(path))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(timestamp))
            .unwrap();
    }
    let first = f.prepare("restore", None);
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let staging = dest
        .unpack("restore", &first.transfer, &first.parts, &first.directory)
        .unwrap();
    dest.apply("restore", &first.transfer, &staging, true, 1.0)
        .unwrap();
    dest.confirm("restore").unwrap();
    drop(dest);
    let dest = Destination::open_existing(&f.dir.join("remote")).unwrap();
    dest.restore(&f.dir.join("restored"), None, &[]).unwrap();
    for path in ["data/prices", "projects/code"] {
        let source = fs::metadata(f.dir.join(path)).unwrap();
        let restored = fs::metadata(f.dir.join("restored").join(path)).unwrap();
        assert_eq!(
            (restored.mtime(), restored.mtime_nsec()),
            (source.mtime(), source.mtime_nsec())
        );
    }
}

#[test]
fn seed_crash_after_source_commit_recovers_adopted_records_remotely() {
    use super::{client::Destination, pull::Remote};
    struct Server<'a>(&'a ManagedArtifacts);
    impl Remote for Server<'_> {
        fn json(
            &mut self,
            _: &str,
            path: &str,
            _: Option<serde_json::Value>,
        ) -> Result<(u16, serde_json::Value)> {
            let id = path.rsplit('/').next().unwrap();
            Ok((
                200,
                serde_json::to_value(lifecycle::status(self.0, "backup-mini", id)?)?,
            ))
        }
        fn download(
            &mut self,
            _: &str,
            _: &Part,
            _: u64,
            _: &mut dyn std::io::Write,
        ) -> Result<()> {
            panic!()
        }
        fn probe(&mut self) -> Result<serde_json::Value> {
            panic!()
        }
    }
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let source = dest.directory("existing-data").unwrap();
    fs::copy(f.dir.join("data/prices"), source.join("prices")).unwrap();
    seed::FAIL_AFTER_SOURCE_COMMIT.set(true);
    assert!(
        seed::seed_inner(&f.manager, "backup-mini", &mut dest, Some(&source))
            .unwrap_err()
            .to_string()
            .contains("injected crash")
    );
    assert!(dest.state().unwrap().confirmed.is_none());
    assert!(dest.records().unwrap().is_empty());
    drop(dest);
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    seed::recover_remote(&mut dest, &mut Server(&f.manager)).unwrap();
    let confirmed = confirmed(&f.manager.store.open().unwrap(), "backup-mini")
        .unwrap()
        .unwrap();
    assert_eq!(
        dest.state().unwrap().confirmed.as_deref(),
        Some(confirmed.manifest_id.as_str())
    );
    assert_eq!(dest.records().unwrap(), confirmed.files);
    assert!(dest.meta("pending_seed").unwrap().is_none());
    assert_eq!(
        fs::read(dest.root.join("plain/data/prices")).unwrap(),
        b"prices"
    );
}

#[test]
fn uncommitted_seed_is_rolled_back_on_remote_recovery() {
    use super::{client::Destination, pull::Remote};
    struct Missing;
    impl Remote for Missing {
        fn json(
            &mut self,
            _: &str,
            _: &str,
            _: Option<serde_json::Value>,
        ) -> Result<(u16, serde_json::Value)> {
            Ok((
                404,
                serde_json::json!({"detail":"backup request not found"}),
            ))
        }
        fn download(
            &mut self,
            _: &str,
            _: &Part,
            _: u64,
            _: &mut dyn std::io::Write,
        ) -> Result<()> {
            panic!()
        }
        fn probe(&mut self) -> Result<serde_json::Value> {
            panic!()
        }
    }
    let f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"original").unwrap();
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let first = f.prepare("confirmed", None);
    let staging = dest
        .unpack("confirmed", &first.transfer, &first.parts, &first.directory)
        .unwrap();
    dest.apply("confirmed", &first.transfer, &staging, true, 1.0)
        .unwrap();
    dest.confirm("confirmed").unwrap();
    let original = dest.records().unwrap();
    fs::write(f.dir.join("data/prices"), b"replacement").unwrap();
    let next = f.prepare("seed-interrupted", None);
    dest.set_meta(
        "pending_seed",
        &serde_json::json!({"id":"seed-interrupted","manifest":next.manifest,"adoption":null})
            .to_string(),
    )
    .unwrap();
    let staging = dest
        .unpack(
            "seed-interrupted",
            &next.transfer,
            &next.parts,
            &next.directory,
        )
        .unwrap();
    dest.apply("seed-interrupted", &next.transfer, &staging, true, 1.0)
        .unwrap();
    drop(dest);
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    seed::recover_remote(&mut dest, &mut Missing).unwrap();
    assert_eq!(dest.records().unwrap(), original);
    assert_eq!(
        fs::read(dest.root.join("plain/data/prices")).unwrap(),
        b"original"
    );
    assert_eq!(
        dest.state().unwrap().confirmed.as_deref(),
        Some("confirmed")
    );
    assert!(dest.meta("pending_seed").unwrap().is_none());
}

#[test]
fn excluded_static_trees_are_absent_unstamped_and_later_deleted() {
    let mut f = Fixture::new();
    let projects = f.dir.join("projects");
    let p = projects.display().to_string();
    for dir in ["app/src", "app/target/debug", "comfyui/models"] {
        fs::create_dir_all(projects.join(dir)).unwrap();
    }
    fs::write(projects.join("app/src/main.rs"), b"fn main() {}").unwrap();
    fs::write(projects.join("app/targets.txt"), b"not a build folder").unwrap();
    fs::write(projects.join("app/target/debug/bin"), b"built").unwrap();
    fs::write(projects.join("comfyui/models/weights"), b"downloadable").unwrap();
    f.manager.config.backup.static_paths = vec![p.clone()];
    f.manager.config.backup.exclude = vec![format!("{p}/comfyui"), format!("{p}/**/target")];
    let scopes = collect_scope(&f.manager, "mini").unwrap();
    let first = prepare(&f.manager, &scopes, None, "one").unwrap();
    assert_eq!(
        first.manifest.files.keys().collect::<Vec<_>>(),
        [
            "packed/projects/app/src/main.rs.zst",
            "packed/projects/app/targets.txt.zst"
        ]
    );
    // Build churn under an excluded tree leaves the coverage stamp unchanged.
    let stamp = scopes[0].measure().unwrap().stamp();
    fs::write(projects.join("app/target/debug/bin"), b"rebuilt and longer").unwrap();
    assert_eq!(scopes[0].measure().unwrap().stamp(), stamp);
    // Excluding a file that is already backed up reports it deleted.
    f.manager
        .config
        .backup
        .exclude
        .push(format!("{p}/app/targets.txt"));
    let scopes = collect_scope(&f.manager, "mini").unwrap();
    let second = prepare(&f.manager, &scopes, Some(&first.manifest), "two").unwrap();
    assert_eq!(
        second.transfer.deleted,
        ["packed/projects/app/targets.txt.zst"]
    );
    assert!(second.transfer.changed.is_empty());
}

#[test]
fn invalid_backup_exclude_fails_config_validation() {
    let mut config = ManagedArtifactsConfig::default();
    assert!(exclude_set(&config.backup)
        .unwrap()
        .is_match(crate::sessions::expand_home(
            "~/projects/fractal-algo-rust/target"
        )));
    assert!(!exclude_set(&config.backup)
        .unwrap()
        .is_match(crate::sessions::expand_home("~/projects/app/targets.txt")));
    config.enabled = true;
    config.validate().unwrap();
    config.backup.exclude = vec!["~/projects/[unclosed".into()];
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("invalid backup exclude"), "{error}");
}

#[test]
fn loose_root_file_is_a_one_file_scope_and_round_trips() {
    use super::client::Destination;
    let f = Fixture::new();
    let root = &f.manager.config.roots[0];
    fs::write(PathBuf::from(&root.path).join("notes.md"), b"cleanup notes").unwrap();
    f.manager.store.open().unwrap().execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'notes.md','unrecorded','root','notes.md','scan','owner/repo','now')",params![root.path]).unwrap();
    let scopes = collect_scope(&f.manager, "mini").unwrap();
    let notes = scopes.iter().find(|s| s.segment == "notes.md").unwrap();
    assert_eq!(notes.measure().unwrap().data_bytes, 13);
    let first = prepare(&f.manager, &scopes, None, "first").unwrap();
    assert!(first
        .manifest
        .files
        .contains_key("packed/notes.md/.sm-file.zst"));
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let staging = dest
        .unpack("first", &first.transfer, &first.parts, &first.directory)
        .unwrap();
    dest.apply("first", &first.transfer, &staging, false, 0.1)
        .unwrap();
    dest.restore(&f.dir.join("restored"), None, &[]).unwrap();
    assert_eq!(
        fs::read(f.dir.join("restored/notes.md")).unwrap(),
        b"cleanup notes"
    );
    // A folder may not use the reserved inner name.
    fs::write(f.dir.join("projects").join(FILE_ENTRY), b"x").unwrap();
    assert!(prepare(&f.manager, &f.scopes(), None, "second").is_err());
}

#[test]
fn artifact_linking_into_another_is_kept_as_the_link_and_round_trips() {
    use super::client::Destination;
    let f = Fixture::new();
    let root = PathBuf::from(&f.manager.config.roots[0].path);
    f.artifact("chains", "live", false, false, 0);
    // An alias of another artifact, and a link to data no other source reads.
    symlink(root.join("chains"), root.join("alias")).unwrap();
    fs::create_dir_all(f.dir.join("elsewhere")).unwrap();
    fs::write(f.dir.join("elsewhere/content"), b"far").unwrap();
    symlink(f.dir.join("elsewhere"), root.join("far")).unwrap();
    let conn = f.manager.store.open().unwrap();
    for name in ["alias", "far"] {
        conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,?2,'live','root',?2,'hand','owner/repo','now')",params![root.display().to_string(),name]).unwrap();
    }
    let scopes = collect_scope(&f.manager, "mini").unwrap();
    assert_eq!(scopes.len(), 3);
    let first = prepare(&f.manager, &scopes, None, "first").unwrap();
    let files = &first.manifest.files;
    assert_eq!(
        files["packed/alias/.sm-file"].target.as_deref(),
        root.join("chains").to_str()
    );
    assert!(files.contains_key("packed/chains/content.zst"));
    assert!(files.contains_key("packed/far/content.zst"));
    assert_eq!(files.len(), 3);
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let staging = dest
        .unpack("first", &first.transfer, &first.parts, &first.directory)
        .unwrap();
    dest.apply("first", &first.transfer, &staging, false, 0.1)
        .unwrap();
    let restored = f.dir.join("restored");
    dest.restore(&restored, None, &[]).unwrap();
    assert_eq!(
        fs::read_link(restored.join("alias")).unwrap(),
        root.join("chains")
    );
    assert_eq!(
        fs::read(restored.join("chains/content")).unwrap(),
        b"abcdefg"
    );
    assert_eq!(fs::read(restored.join("far/content")).unwrap(), b"far");
    // An unchanged link is not sent again.
    let second = prepare(&f.manager, &scopes, Some(&first.manifest), "second").unwrap();
    assert!(second.transfer.changed.is_empty());
}

#[test]
fn link_into_an_excluded_artifact_is_marked_backed_up() {
    let f = Fixture::new();
    let root = PathBuf::from(&f.manager.config.roots[0].path);
    // Rebuildable and cheap, so out of scope; the link to it is still backed up.
    f.artifact("chains", "live", true, false, 0);
    symlink(root.join("chains"), root.join("alias")).unwrap();
    let conn = f.manager.store.open().unwrap();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'alias','live','root','alias','hand','owner/repo','now')",params![root.display().to_string()]).unwrap();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    super::seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    let backed_up = |name: &str| -> bool {
        conn.query_row(
            "SELECT backed_up_at IS NOT NULL FROM artifacts WHERE name=?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(backed_up("alias"));
    assert!(!backed_up("chains"));
}

#[test]
fn source_moved_after_scope_collection_waits_and_is_never_deleted() {
    let f = Fixture::new();
    fs::write(f.dir.join("projects/a"), b"abc").unwrap();
    fs::write(f.dir.join("data/b"), b"def").unwrap();
    let first = f.prepare("first", None);
    // packed/projects is prepared before plain/data; while it is read, a thaw
    // or freeze moves the data folder away.
    let moved = |f: &Fixture| {
        let data = f.dir.join("data");
        outbox::AFTER_READ.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |_| fs::remove_dir_all(&data).unwrap()))
        });
    };
    fs::write(f.dir.join("projects/a"), b"abcd").unwrap();
    moved(&f);
    let second = f.prepare("second", Some(&first.manifest));
    assert!(second.transfer.deleted.is_empty());
    assert!(second.manifest.files.contains_key("plain/data/b"));
    assert_eq!(second.deferred, ["plain/data/", "plain/data/b"]);
    // A seed (no confirmed manifest) leaves it for the first request.
    fs::create_dir_all(f.dir.join("data")).unwrap();
    fs::write(f.dir.join("data/b"), b"def").unwrap();
    moved(&f);
    let fresh = f.prepare("fresh", None);
    assert!(fresh.manifest.files.contains_key("packed/projects/a.zst"));
    assert!(!fresh
        .manifest
        .files
        .keys()
        .any(|p| p.starts_with("plain/data/")));
    // ...and its coverage is dropped, so it is not marked backed up.
    assert_eq!(fresh.deferred, ["plain/data/"]);
}

#[test]
fn file_removed_after_inventory_waits_a_night_then_is_deleted() {
    let f = Fixture::new();
    fs::write(f.dir.join("projects/a"), b"a").unwrap();
    fs::write(f.dir.join("projects/b"), b"b").unwrap();
    let first = f.prepare("first", None);
    fs::write(f.dir.join("projects/a"), b"a, edited").unwrap();
    let b = f.dir.join("projects/b");
    outbox::AFTER_READ
        .with(|hook| *hook.borrow_mut() = Some(Box::new(move |_| fs::remove_file(&b).unwrap())));
    let second = f.prepare("second", Some(&first.manifest));
    assert_eq!(second.deferred, vec!["packed/projects/b.zst"]);
    assert_eq!(
        second.manifest.files.get("packed/projects/b.zst"),
        first.manifest.files.get("packed/projects/b.zst")
    );
    assert!(second.transfer.deleted.is_empty());
    let third = f.prepare("third", Some(&second.manifest));
    assert_eq!(third.transfer.deleted, vec!["packed/projects/b.zst"]);
}

#[test]
fn seed_leaves_files_changed_while_packing_for_the_first_request() {
    use super::{client::Destination, seed};
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    fs::write(f.dir.join("data/stable"), b"stable").unwrap();
    outbox::AFTER_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|p| {
            fs::write(p, b"written during the seed").unwrap()
        }))
    });
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    let confirmed = confirmed(&f.manager.store.open().unwrap(), "backup-mini")
        .unwrap()
        .unwrap();
    assert!(!confirmed.files.contains_key("plain/data/prices"));
    assert!(confirmed.files.contains_key("plain/data/stable"));
    let next = f.prepare("next", Some(&confirmed));
    assert_eq!(
        next.transfer
            .changed
            .iter()
            .map(|r| r.path.as_str())
            .collect::<Vec<_>>(),
        ["plain/data/prices"]
    );
}

#[test]
fn diskutil_smart_status_maps_to_health() {
    use super::disk::parse_smart;
    let info = |status: &str| {
        format!("   Device Node:              /dev/disk4s1\n   SMART Status:             {status}\n   Volume Free Space:        1.2 TB\n")
    };
    assert_eq!(parse_smart(&info("Verified")), "verified");
    assert_eq!(parse_smart(&info("Failing")), "failing");
    assert_eq!(parse_smart(&info("Not Supported")), "not_reported");
    assert_eq!(
        parse_smart("   Device Node:              /dev/disk4s1\n"),
        "not_reported"
    );
    assert_eq!(parse_smart(""), "not_reported");
}

/// A remote disk with three dated holding folders of 100 bytes each, and one
/// current file applied through the ordinary path.
fn holding_fixture(f: &Fixture) -> super::client::Destination {
    use super::client::Destination;
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let first = f.prepare("first", None);
    let dest = Destination::open(&f.dir.join("remote")).unwrap();
    let staging = dest
        .unpack("first", &first.transfer, &first.parts, &first.directory)
        .unwrap();
    let mut dest = dest;
    dest.apply("first", &first.transfer, &staging, false, 0.1)
        .unwrap();
    dest.confirm("first").unwrap();
    // A first apply replaces nothing, so it creates no holding folders (#2377).
    assert!(!dest.root.join(".holding").exists());
    dest.conn.execute("DELETE FROM history", []).unwrap();
    for (n, run) in ["2026-01-03-c", "2026-01-01-a", "2026-01-02-b"]
        .iter()
        .enumerate()
    {
        let dir = dest.root.join(".holding").join(run).join("plain/data");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("prices"), vec![b'x'; 100]).unwrap();
        dest.conn
            .execute(
                "INSERT INTO history(run,path,held_at,days,body) VALUES(?1,'plain/data/prices',?2,30,NULL)",
                params![run, 1000 + n as i64],
            )
            .unwrap();
    }
    dest
}
/// A 1000-byte disk whose only other contents are its holding folders.
fn simulated_free(root: &std::path::Path) -> anyhow::Result<u64> {
    let mut used = 0;
    for run in ["2026-01-01-a", "2026-01-02-b", "2026-01-03-c"] {
        if root.join(".holding").join(run).exists() {
            used += 100;
        }
    }
    Ok(1000 - used)
}

#[test]
fn pruning_deletes_oldest_holding_first_and_never_current_files() {
    use super::disk::Pruned;
    let f = Fixture::new();
    let dest = holding_fixture(&f);
    let report = dest.disk_report(None, "not_reported").unwrap();
    assert_eq!(report.holding_bytes, 300);
    assert_eq!(report.holding_oldest.as_deref(), Some("2026-01-01"));
    assert_eq!(report.pruned, None);
    // Free 700 against a floor of 750: one folder, the oldest, goes.
    let mut pruned = Pruned::default();
    let short = dest
        .prune_holding(0, 750, None, simulated_free, &mut pruned)
        .unwrap();
    assert_eq!(short, 0);
    assert_eq!(
        pruned,
        Pruned {
            folders: 1,
            bytes: 100
        }
    );
    assert!(!dest.root.join(".holding/2026-01-01-a").exists());
    assert!(dest.root.join(".holding/2026-01-02-b").exists());
    assert!(dest.root.join(".holding/2026-01-03-c").exists());
    let runs: Vec<String> = dest
        .conn
        .prepare("SELECT run FROM history ORDER BY run")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(runs, ["2026-01-02-b", "2026-01-03-c"]);
    assert_eq!(
        fs::read(dest.root.join("plain/data/prices")).unwrap(),
        b"prices"
    );
    let report = dest.disk_report(Some(pruned.clone()), "verified").unwrap();
    assert_eq!(report.holding_oldest.as_deref(), Some("2026-01-02"));
    assert_eq!(
        report.pruned,
        Some(Pruned {
            folders: 1,
            bytes: 100
        })
    );
    // Tonight's folder (c) is kept even when that leaves too little room;
    // the shortfall is what remains.
    let mut pruned = Pruned::default();
    let short = dest
        .prune_holding(500, 750, Some("c"), simulated_free, &mut pruned)
        .unwrap();
    assert_eq!(pruned.folders, 1);
    assert!(dest.root.join(".holding/2026-01-03-c").exists());
    assert_eq!(short, 500 + 750 - 900);
    assert_eq!(
        fs::read(dest.root.join("plain/data/prices")).unwrap(),
        b"prices"
    );
    assert!(dest.root.join("state.db").exists() && dest.root.join("state.json").exists());
}

#[test]
fn an_impossible_apply_acknowledges_disk_full_without_applying() {
    use super::{
        client::Destination,
        lifecycle::RequestStatus,
        pull::{self, Remote},
    };
    struct Fake {
        status: RequestStatus,
        acks: Vec<serde_json::Value>,
        downloads: usize,
    }
    impl Remote for Fake {
        fn json(
            &mut self,
            method: &str,
            path: &str,
            body: Option<serde_json::Value>,
        ) -> Result<(u16, serde_json::Value)> {
            if method == "POST" && path.ends_with("/ack") {
                self.acks.push(body.unwrap());
                return Ok((200, serde_json::json!({"ok":true})));
            }
            Ok((200, serde_json::to_value(&self.status)?))
        }
        fn download(
            &mut self,
            _: &str,
            _: &Part,
            _: u64,
            _: &mut dyn std::io::Write,
        ) -> Result<()> {
            self.downloads += 1;
            anyhow::bail!("no download expected")
        }
        fn probe(&mut self) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
    }
    let f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let first = f.prepare("first", None);
    let mut fake = Fake {
        status: RequestStatus {
            id: "first".into(),
            state: "ready".into(),
            retry_after_seconds: None,
            estimate_bytes: 6,
            manifest_id: Some("first".into()),
            parts: first.parts,
            manifest: Some(first.transfer),
            deletions: vec![],
            allow_deletes: false,
            max_delete_fraction: 0.1,
            error: None,
            // More than any disk holds.
            space_floor_bytes: Some(u64::MAX / 2),
        },
        acks: vec![],
        downloads: 0,
    };
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let error = pull::pull(&mut dest, &mut fake).unwrap_err();
    assert!(
        error.to_string().starts_with("backup disk full: needs "),
        "{error}"
    );
    assert_eq!(fake.downloads, 0);
    assert_eq!(fake.acks.len(), 1);
    let ack = &fake.acks[0];
    assert_eq!(ack["applied"], false);
    assert!(ack["error"]
        .as_str()
        .unwrap()
        .starts_with("backup disk full: needs "));
    assert!(ack["disk"]["total_bytes"].as_u64().unwrap() > 0);
    assert!(super::disk::HEALTH.contains(&ack["disk"]["health"].as_str().unwrap()));
    // Nothing applied, nothing pending: the next night asks afresh.
    assert!(dest.records().unwrap().is_empty());
    assert!(dest.state().unwrap().confirmed.is_none());
    assert!(dest.meta("pending_request").unwrap().is_none());
    assert_eq!(
        dest.meta("space_floor_bytes").unwrap().as_deref(),
        Some((u64::MAX / 2).to_string().as_str())
    );
}

#[test]
fn disk_reports_are_stored_rendered_and_a_failing_drive_alerts_weekly() {
    use super::{
        disk::{Disk, Pruned},
        lifecycle::{self as l, RequestResult},
    };
    let mut f = Fixture::new();
    f.manager.config.backup.enabled = true;
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    let conn = f.manager.store.open().unwrap();
    let failing_alerts = || -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM backup_alerts WHERE key LIKE 'backup-failing:mini:%' AND blocking=1",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    let mut have: Option<String> = None;
    let mut night = |content: &str, disk: Option<Disk>| -> String {
        fs::write(f.dir.join("data/prices"), content).unwrap();
        let RequestResult::Open(req) = l::request(&f.manager, "mini", have.as_deref()).unwrap()
        else {
            panic!()
        };
        l::prepare_request(&f.manager, &req.id).unwrap();
        let ack = l::Ack {
            applied: true,
            fetched_bytes: 6,
            fetch_seconds: 1.0,
            apply_seconds: 1.0,
            held_deletions: 0,
            check: Some(l::Check {
                ok: true,
                files_checked: 3,
                bad: vec![],
            }),
            error: None,
            update: None,
            disk,
        };
        l::acknowledge(&f.manager, "mini", &req.id, &ack).unwrap();
        have = Some(req.id.clone());
        req.id
    };
    // Tonight's run under way has nothing to report: no night is shown.
    fs::write(f.dir.join("data/prices"), "v0").unwrap();
    assert!(matches!(
        l::request(&f.manager, "mini", None).unwrap(),
        RequestResult::Open(_)
    ));
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let card = l::card(&conn, "mini", now).unwrap();
    assert_eq!(card["nights"], serde_json::json!([]));
    assert_eq!(card["measured"]["fetch"], false);
    // An older mini sends no disk report: the card says not reported, no floor.
    let first = night("v1", None);
    let card = l::card(&conn, "mini", now).unwrap();
    assert!(card["disk"].is_null());
    assert_eq!(
        card["nights"].as_array().unwrap().last().unwrap()["ok"],
        true
    );
    assert_eq!(card["nights"].as_array().unwrap().len(), 1);
    // A 6-byte transfer measures no speed: the defaults stay unshown.
    assert_eq!(card["measured"]["fetch"], false);
    assert_eq!(card["last_night"]["bytes"], 6);
    assert_eq!(card["weekly_check"]["files_checked"], 3);
    assert_eq!(card["weekly_check"]["bad"], 0);
    assert_eq!(
        l::status(&f.manager, "mini", &first)
            .unwrap()
            .space_floor_bytes,
        None
    );
    // A new mini reports its disk and tonight's pruning; the drive is failing.
    let disk = Disk {
        total_bytes: 4_000_000_000_000,
        free_bytes: 1_000_000_000_000,
        holding_bytes: 30_000_000_000,
        holding_oldest: Some("2026-09-17".into()),
        health: "failing".into(),
        pruned: Some(Pruned {
            folders: 2,
            bytes: 5_000_000_000,
        }),
    };
    let second = night("v2", Some(disk.clone()));
    let card = l::card(&conn, "mini", now).unwrap();
    assert_eq!(card["disk"]["total_bytes"], 4_000_000_000_000u64);
    assert_eq!(card["disk"]["health"], "failing");
    assert_eq!(card["disk"]["holding_oldest"], "2026-09-17");
    assert!(card["disk"]["pruned_on"].is_string());
    // 10% of 4 TB is above the 50 GB minimum.
    assert_eq!(
        l::status(&f.manager, "mini", &second)
            .unwrap()
            .space_floor_bytes,
        Some(400_000_000_000)
    );
    assert_eq!(failing_alerts(), 1);
    let body: String = conn
        .query_row(
            "SELECT body FROM backup_alerts WHERE key LIKE 'backup-failing:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(body, "The backup drive reports it is failing (SMART). Replace it, then reseed with `sm backup seed`.");
    // Still failing the next night: no second message within seven days.
    night(
        "v3",
        Some(Disk {
            pruned: None,
            ..disk.clone()
        }),
    );
    assert_eq!(failing_alerts(), 1);
    // Seven days after the first message, a reminder.
    let stamp: i64 = crate::managed_artifacts::store::meta(&conn, "backup_drive_failing:mini")
        .unwrap()
        .unwrap()
        .parse()
        .unwrap();
    crate::managed_artifacts::store::set_meta(
        &conn,
        "backup_drive_failing:mini",
        &(stamp - 7 * 86400).to_string(),
    )
    .unwrap();
    night(
        "v4",
        Some(Disk {
            pruned: None,
            ..disk.clone()
        }),
    );
    assert_eq!(failing_alerts(), 2);
    // A small disk gets the 50 GB minimum; a healthy report clears the state.
    let last = night(
        "v5",
        Some(Disk {
            total_bytes: 100_000_000_000,
            free_bytes: 0,
            health: "verified".into(),
            pruned: None,
            ..disk.clone()
        }),
    );
    assert_eq!(
        l::status(&f.manager, "mini", &last)
            .unwrap()
            .space_floor_bytes,
        Some(50_000_000_000)
    );
    assert_eq!(
        crate::managed_artifacts::store::meta(&conn, "backup_drive_failing:mini")
            .unwrap()
            .as_deref(),
        Some("")
    );
    // An invalid health is refused.
    fs::write(f.dir.join("data/prices"), "v6").unwrap();
    let RequestResult::Open(req) = l::request(&f.manager, "mini", Some(&last)).unwrap() else {
        panic!()
    };
    l::prepare_request(&f.manager, &req.id).unwrap();
    let bad = l::Ack {
        applied: true,
        fetched_bytes: 6,
        fetch_seconds: 1.0,
        apply_seconds: 1.0,
        held_deletions: 0,
        check: None,
        error: None,
        update: None,
        disk: Some(Disk {
            health: "melting".into(),
            ..disk
        }),
    };
    let error = l::acknowledge(&f.manager, "mini", &req.id, &bad).unwrap_err();
    assert_eq!(error.to_string(), "invalid backup disk report");
}

#[test]
fn seed_stages_on_destination_refuses_past_floor_and_resumes_staging() {
    use super::{client::Destination, seed};
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![f.dir.join("data").display().to_string()];
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    fs::create_dir_all(dest.root.join(".incoming/seed-stale/plain")).unwrap();
    outbox::DIRECT_FLOOR.set(Some(u128::MAX / 2));
    let error = seed::seed(&f.manager, "backup-mini", &mut dest, None, false)
        .unwrap_err()
        .to_string();
    outbox::DIRECT_FLOOR.set(None);
    assert!(error.contains("would cross its floor"), "{error}");
    // The earlier staging is resumed under the new seed's id, never deleted.
    let staging = seed_staging(&dest).unwrap();
    assert_ne!(staging.file_name().unwrap(), "seed-stale");
    assert!(staging.join(outbox::INDEX).is_file());
    assert!(dest.state().unwrap().confirmed.is_none());
    // No outbox is created on the cold store for a seed.
    seed::seed(&f.manager, "backup-mini", &mut dest, None, false).unwrap();
    assert!(!f.dir.join("outbox").exists());
    assert_eq!(
        fs::read(dest.root.join("plain/data/prices")).unwrap(),
        b"prices"
    );
    assert_eq!(
        fs::read_dir(dest.root.join(".incoming")).unwrap().count(),
        0
    );
}

fn two_scope_seed_fixture() -> Fixture {
    let mut f = Fixture::new();
    f.manager.config.backup.static_paths = vec![
        f.dir.join("data").display().to_string(),
        f.dir.join("projects").display().to_string(),
    ];
    for (path, body) in [
        ("data/a", "alpha"),
        ("data/b", "bravo"),
        ("data/c", "charlie"),
        ("projects/x", "x-ray"),
        ("projects/y", "yankee"),
    ] {
        fs::write(f.dir.join(path), body).unwrap();
    }
    f
}
/// Run a seed, returning its result and every source file it read.
fn seed_reading(
    f: &Fixture,
    dest: &mut super::client::Destination,
    fresh: bool,
) -> (Result<String>, Vec<PathBuf>) {
    outbox::READS.with(|r| *r.borrow_mut() = Some(Vec::new()));
    let result = super::seed::seed(&f.manager, "backup-mini", dest, None, fresh);
    (
        result,
        outbox::READS.with(|r| r.borrow_mut().take().unwrap()),
    )
}

#[test]
fn stopped_seed_keeps_staging_and_next_seed_reuses_it() {
    let f = two_scope_seed_fixture();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    outbox::FAIL_AFTER_PLACED.set(Some(2));
    let (result, first_reads) = seed_reading(&f, &mut dest, false);
    outbox::FAIL_AFTER_PLACED.set(None);
    assert!(result.unwrap_err().to_string().contains("injected stop"));
    assert_eq!(first_reads.len(), 2);
    let staging = seed_staging(&dest).unwrap();
    assert_eq!(outbox::staged_index(&staging).unwrap().len(), 2);
    assert!(dest.state().unwrap().confirmed.is_none());

    let (result, second_reads) = seed_reading(&f, &mut dest, false);
    let id = result.unwrap();
    assert!(
        second_reads.iter().all(|p| !first_reads.contains(p)),
        "reused files were read again: {second_reads:?}"
    );
    assert_eq!(second_reads.len(), 3);
    // The confirmed manifest is the one an uninterrupted preparation gives.
    let confirmed = confirmed(&f.manager.store.open().unwrap(), "backup-mini")
        .unwrap()
        .unwrap();
    assert_eq!(confirmed.manifest_id, id);
    assert_eq!(
        confirmed.files,
        f.prepare("uninterrupted", None).manifest.files
    );
    for record in confirmed.files.values() {
        super::client::verify(&dest.root.join(&record.path), record).unwrap();
    }
    assert_eq!(dest.records().unwrap(), confirmed.files);
    assert!(seed_staging(&dest).is_none());
}

#[test]
fn resumed_seed_restages_changed_sources_and_drops_removed_ones() {
    let f = two_scope_seed_fixture();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    outbox::FAIL_AFTER_PLACED.set(Some(5));
    assert!(seed_reading(&f, &mut dest, false).0.is_err());
    outbox::FAIL_AFTER_PLACED.set(None);
    fs::write(f.dir.join("data/b"), b"bravo, changed").unwrap();
    fs::remove_file(f.dir.join("data/c")).unwrap();
    // Fail after apply, so the swept and returned staging can be inspected.
    let conn = f.manager.store.open().unwrap();
    conn.execute_batch("CREATE TRIGGER fail_seed_confirmation BEFORE INSERT ON backup_requests BEGIN SELECT RAISE(ABORT, 'injected confirmation failure'); END;").unwrap();
    let (result, reads) = seed_reading(&f, &mut dest, false);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("injected confirmation failure"));
    assert_eq!(reads, [f.dir.join("data/b")]);
    let staging = seed_staging(&dest).unwrap();
    assert_eq!(
        fs::read(staging.join("plain/data/b")).unwrap(),
        b"bravo, changed"
    );
    assert!(staging.join("plain/data/a").is_file());
    assert!(!staging.join("plain/data/c").exists(), "orphan left staged");
    assert!(dest.records().unwrap().is_empty());
    assert!(!dest.root.join("plain/data/a").exists());

    conn.execute_batch("DROP TRIGGER fail_seed_confirmation")
        .unwrap();
    let (result, reads) = seed_reading(&f, &mut dest, false);
    result.unwrap();
    assert!(reads.is_empty(), "{reads:?}");
    assert_eq!(
        fs::read(dest.root.join("plain/data/b")).unwrap(),
        b"bravo, changed"
    );
    assert!(!dest.root.join("plain/data/c").exists());
    assert!(!dest.records().unwrap().contains_key("plain/data/c"));
    assert_eq!(dest.records().unwrap().len(), 4);
}

#[test]
fn fresh_seed_discards_earlier_staging() {
    let f = two_scope_seed_fixture();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    outbox::FAIL_AFTER_PLACED.set(Some(3));
    assert!(seed_reading(&f, &mut dest, false).0.is_err());
    outbox::FAIL_AFTER_PLACED.set(None);
    let (result, reads) = seed_reading(&f, &mut dest, true);
    result.unwrap();
    assert_eq!(reads.len(), 5);
    assert!(seed_staging(&dest).is_none());
}

#[test]
fn torn_index_line_is_cut_and_its_file_restaged() {
    let f = two_scope_seed_fixture();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    outbox::FAIL_AFTER_PLACED.set(Some(2));
    assert!(seed_reading(&f, &mut dest, false).0.is_err());
    outbox::FAIL_AFTER_PLACED.set(None);
    let index = seed_staging(&dest).unwrap().join(outbox::INDEX);
    let body = fs::read(&index).unwrap();
    // A crash mid-append leaves the second line without its newline.
    fs::write(&index, &body[..body.len() - 10]).unwrap();
    let (result, reads) = seed_reading(&f, &mut dest, false);
    result.unwrap();
    assert_eq!(reads.len(), 4);
}

#[test]
fn staged_copy_other_than_the_indexed_one_is_restaged() {
    let f = two_scope_seed_fixture();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    outbox::FAIL_AFTER_PLACED.set(Some(5));
    assert!(seed_reading(&f, &mut dest, false).0.is_err());
    outbox::FAIL_AFTER_PLACED.set(None);
    // An older same-size copy where the indexed one should be, as a lost
    // rename after a power cut would leave it.
    let staged = seed_staging(&dest).unwrap().join("plain/data/a");
    std::thread::sleep(std::time::Duration::from_millis(10));
    fs::write(&staged, b"stale").unwrap();
    let (result, reads) = seed_reading(&f, &mut dest, false);
    result.unwrap();
    assert_eq!(reads, [f.dir.join("data/a")]);
    assert_eq!(fs::read(dest.root.join("plain/data/a")).unwrap(), b"alpha");
}

#[test]
fn crashed_seed_reuses_flushed_batches_and_restages_the_rest() {
    let f = two_scope_seed_fixture();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    outbox::BATCH_FILES.set(Some(2));
    outbox::CRASH_AFTER_PLACED.set(Some(3));
    let (result, first_reads) = seed_reading(&f, &mut dest, false);
    outbox::CRASH_AFTER_PLACED.set(None);
    outbox::BATCH_FILES.set(None);
    assert!(result.unwrap_err().to_string().contains("injected crash"));
    assert_eq!(first_reads.len(), 3);
    let staging = seed_staging(&dest).unwrap();
    // Only the flushed batch is vouched for; the third file's line never landed.
    assert_eq!(outbox::staged_index(&staging).unwrap().len(), 2);
    let (result, second_reads) = seed_reading(&f, &mut dest, false);
    result.unwrap();
    assert!(second_reads.iter().all(|p| !first_reads[..2].contains(p)));
    assert_eq!(second_reads.len(), 3);
    let confirmed = confirmed(&f.manager.store.open().unwrap(), "backup-mini")
        .unwrap()
        .unwrap();
    assert_eq!(
        confirmed.files,
        f.prepare("uninterrupted", None).manifest.files
    );
    assert_eq!(dest.records().unwrap(), confirmed.files);
}

#[test]
fn files_an_interrupted_apply_moved_are_reused_in_place() {
    let f = two_scope_seed_fixture();
    let mut dest = super::client::Destination::open(&f.dir.join("remote")).unwrap();
    outbox::FAIL_AFTER_PLACED.set(Some(5));
    let (result, _) = seed_reading(&f, &mut dest, false);
    outbox::FAIL_AFTER_PLACED.set(None);
    assert!(result.is_err());
    // As if apply had moved two staged files into place and then crashed.
    let staging = seed_staging(&dest).unwrap();
    let index = outbox::staged_index(&staging).unwrap();
    let moved: Vec<_> = index.keys().take(2).cloned().collect();
    for path in &moved {
        let target = dest.root.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::rename(staging.join(path), &target).unwrap();
    }
    let (result, reads) = seed_reading(&f, &mut dest, false);
    result.unwrap();
    assert!(reads.is_empty(), "moved files were restaged: {reads:?}");
    let confirmed = confirmed(&f.manager.store.open().unwrap(), "backup-mini")
        .unwrap()
        .unwrap();
    for record in confirmed.files.values() {
        super::client::verify(&dest.root.join(&record.path), record).unwrap();
    }
    assert_eq!(dest.records().unwrap(), confirmed.files);
}
