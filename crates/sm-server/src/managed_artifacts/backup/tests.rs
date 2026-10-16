use super::super::{
    config::{ArtifactRoot, ManagedArtifactsConfig},
    moves, ManagedArtifacts,
};
use super::*;
use rusqlite::params;
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
    let ack = l::Ack {
        applied: true,
        fetched_bytes: 6,
        fetch_seconds: 1.0,
        apply_seconds: 1.0,
        held_deletions: 0,
        check: None,
        error: None,
    };
    l::acknowledge(&f.manager, "mini", &first.id, &ack).unwrap();
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
    };
    // The fixture's configured device is backup-mini; recovery is per device.
    crate::managed_artifacts::store::set_meta(&conn, "backup_missed:mini", "1").unwrap();
    l::acknowledge(&f.manager, "mini", &next.id, &ack).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM backup_alerts WHERE blocking=1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
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
