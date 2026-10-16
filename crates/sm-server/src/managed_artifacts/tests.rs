use super::*;
use std::os::unix::fs::symlink;
use std::{fs, path::PathBuf};
struct TempDir(PathBuf);
impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub(super) struct Fixture {
    _dir: TempDir,
    svc: ManagedArtifacts,
    root: ArtifactRoot,
}
impl Fixture {
    pub(super) fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = TempDir(std::env::temp_dir().join(format!(
            "sm-artifacts-test-{}-{}-{serial}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        )));
        fs::create_dir(&dir.0).unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(path.join("root")).unwrap();
        fs::create_dir(path.join("cold")).unwrap();
        let root = ArtifactRoot {
            path: path.join("root").display().to_string(),
            cold_store: path.join("cold").display().to_string(),
            repo: "owner/research".into(),
            ..Default::default()
        };
        let config = ManagedArtifactsConfig {
            db_path: path.join("artifacts.db").display().to_string(),
            roots: vec![root.clone()],
            ..Default::default()
        };
        Self {
            _dir: dir,
            svc: ManagedArtifacts::new(&config).unwrap(),
            root,
        }
    }
    pub(super) fn path(&self, name: &str) -> PathBuf {
        PathBuf::from(&self.root.path).join(name)
    }
    fn flags(&self) -> RecordFlags {
        RecordFlags {
            note: "Useful dataset".into(),
            consumers: vec!["#123".into()],
            ..Default::default()
        }
    }
    fn create(&self, name: &str) -> Value {
        self.svc
            .create(&self.root, name, &self.flags(), "maker", "Maker")
            .unwrap()
    }
}
#[test]
fn owner_delete_preserves_recipes_reports_dependents_and_refuses_in_use() {
    let f = Fixture::new();
    f.create("input");
    f.create("output");
    let conn = f.svc.store.open().unwrap();
    conn.execute("UPDATE artifacts SET project=1 WHERE name='input'", [])
        .unwrap();
    conn.execute(
        "UPDATE artifacts SET recipe_complete=1 WHERE name='output'",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO artifact_inputs(root,name,input,source,job_id) VALUES(?1,'output','input','declared','job')", [&f.root.path]).unwrap();
    conn.execute(
        "INSERT INTO artifact_signals(root,name,kind,evidence) VALUES(?1,'input','in_use','job')",
        [&f.root.path],
    )
    .unwrap();
    assert!(f
        .svc
        .delete_owner(&f.root, "input", "owner@example.com")
        .is_err());
    conn.execute("DELETE FROM artifact_signals", []).unwrap();
    assert!(f.svc.delete(&f.root, "input", "maker", "owner").is_err());
    let result = f
        .svc
        .delete_owner(&f.root, "input", "owner@example.com")
        .unwrap();
    assert_eq!(result["dependents"][0]["name"], "output");
    assert!(!f.path("input").exists());
    let output = f.svc.store.detail(&f.root.path, "output").unwrap();
    assert_eq!(output["recipe_complete"], 1);
    assert_eq!(output["rebuildable"], false);
    assert_eq!(result["delete_reason"], "owner");
}

#[test]
fn names_and_notes() {
    for name in ["a", "123_data", "a.b-c_9"] {
        validate_name(name).unwrap();
    }
    for name in ["", "../a", "/tmp/a", ".hidden", "a/b", "a b", "é"] {
        assert!(validate_name(name).is_err(), "{name}");
    }
    assert!(validate_name(&"a".repeat(129)).is_err());
    validate_note(&"é".repeat(140), 140).unwrap();
    for note in ["", "   ", "two\nlines", "two\rlines"] {
        assert!(validate_note(note, 140).is_err());
    }
    assert!(validate_note(&"é".repeat(141), 140).is_err());
}
#[test]
fn root_resolution_requires_unambiguous_repo() {
    let f = Fixture::new();
    assert_eq!(
        f.svc.config.resolve_root(None, None).unwrap().path,
        f.root.path
    );
    let mut c = f.svc.config.clone();
    c.roots.push(ArtifactRoot {
        path: "/other".into(),
        repo: "owner/other".into(),
        ..Default::default()
    });
    assert!(c.resolve_root(None, None).is_err());
    assert_eq!(
        c.resolve_root(None, Some("owner/research")).unwrap().path,
        f.root.path
    );
    assert!(c.resolve_root(Some("/not/a/root"), None).is_err());
}
#[test]
fn record_validation() {
    let f = Fixture::new();
    let mut flags = f.flags();
    flags.consumers.clear();
    assert!(flags.validate(&f.svc.config, &f.root).is_err());
    flags.project = true;
    flags.keep_until = Some("2020-01-01".into());
    assert!(flags.validate(&f.svc.config, &f.root).is_err());
    flags.keep_until = None;
    flags.gpu_hours = Some(-1.0);
    assert!(flags.validate(&f.svc.config, &f.root).is_err());
    assert_eq!(
        consumer("#123", &f.root.repo).unwrap(),
        ("owner/research".into(), 123)
    );
    assert!(consumer("#0", &f.root.repo).is_err());
}
#[test]
fn new_refuses_existing_record_and_entry() {
    let f = Fixture::new();
    f.create("123_data");
    assert!(f
        .svc
        .create(&f.root, "123_DATA", &f.flags(), "maker", "Maker")
        .is_err());
    fs::create_dir(f.path("456_data")).unwrap();
    assert!(f
        .svc
        .create(&f.root, "456_data", &f.flags(), "maker", "Maker")
        .is_err());
}
#[test]
fn done_requires_creating_session_and_hand_origin() {
    let f = Fixture::new();
    f.create("123_data");
    assert!(f.svc.done(&f.root, "123_data", "other").is_err());
    f.svc
        .store
        .open()
        .unwrap()
        .execute("UPDATE artifacts SET origin='queue'", [])
        .unwrap();
    assert!(f.svc.done(&f.root, "123_data", "maker").is_err());
    f.svc
        .store
        .open()
        .unwrap()
        .execute("UPDATE artifacts SET origin='hand'", [])
        .unwrap();
    assert_eq!(
        f.svc.done(&f.root, "123_data", "maker").unwrap()["state"],
        "live"
    );
}
#[test]
fn record_is_atomic_and_adopts_without_recipe() {
    let f = Fixture::new();
    fs::create_dir(f.path("123_data")).unwrap();
    f.svc.scan(&f.root).unwrap();
    assert!(f
        .svc
        .record(
            &f.root,
            &["123_data".into(), "missing".into()],
            &f.flags(),
            "maker"
        )
        .is_err());
    assert_eq!(
        f.svc.store.detail(&f.root.path, "123_data").unwrap()["state"],
        "unrecorded"
    );
    let result = f
        .svc
        .record(&f.root, &["123_data".into()], &f.flags(), "maker")
        .unwrap();
    assert_eq!(result[0]["origin"], "adopted");
    assert_eq!(result[0]["rebuildable"], false);
    assert!(f
        .svc
        .record(&f.root, &["123_data".into()], &f.flags(), "maker")
        .is_err());
}
#[test]
fn scan_known_directory_and_cold_link_are_unchanged() {
    let f = Fixture::new();
    f.create("123_data");
    let before = f.svc.store.detail(&f.root.path, "123_data").unwrap();
    f.svc.scan(&f.root).unwrap();
    assert_eq!(
        before,
        f.svc.store.detail(&f.root.path, "123_data").unwrap()
    );
    let cold = PathBuf::from(&f.root.cold_store).join("456_data");
    fs::create_dir(&cold).unwrap();
    symlink(&cold, f.path("456_data")).unwrap();
    f.svc.scan(&f.root).unwrap();
    let a = f.svc.store.detail(&f.root.path, "456_data").unwrap();
    assert_eq!(a["location"], "cold");
    assert_eq!(a["state"], "unrecorded");
    f.svc.scan(&f.root).unwrap();
    assert_eq!(a, f.svc.store.detail(&f.root.path, "456_data").unwrap());
}
#[test]
fn scan_frozen_placeholder_unchanged() {
    let f = Fixture::new();
    f.create("123_data");
    fs::write(f.path("123_data/FROZEN.md"), "frozen").unwrap();
    fs::write(f.path("123_data/.DS_Store"), "finder").unwrap();
    f.svc
        .store
        .open()
        .unwrap()
        .execute("UPDATE artifacts SET state='frozen'", [])
        .unwrap();
    f.svc.scan(&f.root).unwrap();
    assert_eq!(
        f.svc.store.detail(&f.root.path, "123_data").unwrap()["state"],
        "frozen"
    );
}
#[test]
fn scan_alias_and_unknown_entry_and_hidden_entries() {
    let f = Fixture::new();
    fs::create_dir_all(f.path("123_data/subdir")).unwrap();
    symlink(f.path("123_data/subdir"), f.path("alias")).unwrap();
    fs::write(f.path("other"), "x").unwrap();
    fs::write(f.path(".DS_Store"), "x").unwrap();
    f.svc.scan(&f.root).unwrap();
    let rows = f.svc.store.list(&f.root.path).unwrap();
    assert_eq!(rows.len(), 2);
    let a = f.svc.store.detail(&f.root.path, "123_data").unwrap();
    assert_eq!(a["aliases"][0]["alias"], "alias");
    assert_eq!(
        f.svc.store.detail(&f.root.path, "other").unwrap()["state"],
        "unrecorded"
    );
    fs::remove_file(f.path("alias")).unwrap();
    f.svc.scan(&f.root).unwrap();
    assert_eq!(
        f.svc.store.detail(&f.root.path, "123_data").unwrap()["aliases"],
        json!([])
    );
}
#[test]
fn scan_missing_live_record_is_vanished() {
    let f = Fixture::new();
    f.create("123_data");
    f.svc.done(&f.root, "123_data", "maker").unwrap();
    fs::remove_dir(f.path("123_data")).unwrap();
    f.svc.scan(&f.root).unwrap();
    let a = f.svc.store.detail(&f.root.path, "123_data").unwrap();
    assert_eq!(a["delete_reason"], "vanished");
    assert_eq!(a["needs_owner"], true);
}
#[test]
fn initial_grace_is_persisted_once() {
    let f = Fixture::new();
    fs::create_dir(f.path("123_data")).unwrap();
    f.svc.scan(&f.root).unwrap();
    fs::create_dir(f.path("456_data")).unwrap();
    f.svc.scan(&f.root).unwrap();
    let get = |name| {
        OffsetDateTime::parse(
            f.svc.store.detail(&f.root.path, name).unwrap()["grace_until"]
                .as_str()
                .unwrap(),
            &Rfc3339,
        )
        .unwrap()
    };
    assert!(
        (get("123_data") - get("456_data"))
            .whole_seconds()
            .abs_diff(7 * 86400)
            < 5
    );
}
#[test]
fn size_walk_deduplicates_hardlinks_and_skips_symlinks_and_finder() {
    let f = Fixture::new();
    fs::create_dir(f.path("123_data")).unwrap();
    fs::write(f.path("123_data/a"), "hello").unwrap();
    fs::hard_link(f.path("123_data/a"), f.path("123_data/b")).unwrap();
    symlink(f.path("123_data/a"), f.path("123_data/link")).unwrap();
    let before = scan::measure(&f.path("123_data")).unwrap();
    assert_eq!(before.files, 1);
    assert_eq!(before.data_bytes, 5);
    fs::write(f.path("123_data/.DS_Store"), "finder").unwrap();
    let after = scan::measure(&f.path("123_data")).unwrap();
    assert_eq!(before.stamp(), after.stamp());
}
#[test]
fn delete_refuses_project_inputs_and_jobs() {
    let f = Fixture::new();
    f.create("123_data");
    f.svc.done(&f.root, "123_data", "maker").unwrap();
    f.svc
        .edit(
            &f.root,
            "123_data",
            "marks",
            &json!({"project":true}),
            "maker",
        )
        .unwrap();
    assert!(f
        .svc
        .delete(&f.root, "123_data", "maker", "cleanup")
        .is_err());
    f.svc
        .edit(
            &f.root,
            "123_data",
            "marks",
            &json!({"project":false}),
            "maker",
        )
        .unwrap();
    let conn = f.svc.store.open().unwrap();
    conn.execute(
        "INSERT INTO artifact_jobs(root,name,job_id,state) VALUES(?1,'123_data','job','running')",
        [&f.root.path],
    )
    .unwrap();
    assert!(f
        .svc
        .delete(&f.root, "123_data", "maker", "cleanup")
        .is_err());
    conn.execute("DELETE FROM artifact_jobs", []).unwrap();
    f.create("456_data");
    conn.execute("INSERT INTO artifact_inputs(root,name,input,job_id) VALUES(?1,'456_data','123_data','job')",[&f.root.path]).unwrap();
    assert!(f
        .svc
        .delete(&f.root, "123_data", "maker", "cleanup")
        .is_err());
    assert!(f.path("123_data").exists());
}
#[test]
fn deletion_preserves_symlink_targets_and_removes_aliases() {
    let f = Fixture::new();
    f.create("123_data");
    let outside = f._dir.path().join("outside");
    fs::write(&outside, "precious").unwrap();
    symlink(&outside, f.path("123_data/link")).unwrap();
    symlink(f.path("123_data"), f.path("alias")).unwrap();
    f.svc.scan(&f.root).unwrap();
    let a = f
        .svc
        .delete(&f.root, "123_data", "maker", "cleanup")
        .unwrap();
    assert_eq!(a["state"], "deleted");
    assert_eq!(fs::read_to_string(outside).unwrap(), "precious");
    assert!(f.path("alias").symlink_metadata().is_err());
    assert!(f.svc.delete(&f.root, "123_data", "maker", "again").is_err());
}
#[test]
fn deletion_refuses_replaced_root() {
    let f = Fixture::new();
    f.create("123_data");
    let root = PathBuf::from(&f.root.path);
    let moved = root.with_file_name("moved");
    fs::rename(&root, &moved).unwrap();
    symlink(&moved, &root).unwrap();
    assert!(f
        .svc
        .delete(&f.root, "123_data", "maker", "cleanup")
        .is_err());
    assert!(moved.join("123_data").exists());
    assert_eq!(
        f.svc.store.detail(&f.root.path, "123_data").unwrap()["state"],
        "building"
    );
}
#[test]
fn attribution_ticket_prefix_is_bounded() {
    for name in ["1_a", "1234567_a", "abc_a", "12a_a", "123"] {
        assert_eq!(scan::ticket_prefix(name), None);
    }
    assert_eq!(scan::ticket_prefix("12_a"), Some(12));
    assert_eq!(scan::ticket_prefix("123456-data"), Some(123456));
}

#[test]
fn orphan_cleanup_report_only_then_enforcing() {
    let mut f = Fixture::new();
    f.create("123_data");
    assert!(f.svc.delete_orphan(&f.root, "123_data").is_err());
    f.svc
        .store
        .open()
        .unwrap()
        .execute("UPDATE artifacts SET created_at='2020-01-01T00:00:00Z'", [])
        .unwrap();
    assert_eq!(
        f.svc.delete_orphan(&f.root, "123_data").unwrap()["state"],
        "building"
    );
    f.svc.delete_orphan(&f.root, "123_data").unwrap();
    let a = f.svc.store.detail(&f.root.path, "123_data").unwrap();
    assert_eq!(
        a["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "would_delete")
            .count(),
        1
    );
    assert!(f.path("123_data").exists());
    f.svc.config.deletion_mode = "enforcing".into();
    let deleted = f.svc.delete_orphan(&f.root, "123_data").unwrap();
    assert_eq!(deleted["delete_reason"], "orphan");
    assert!(!f.path("123_data").exists());
}

#[test]
fn record_refuses_vanished_entry_without_partial_updates() {
    let f = Fixture::new();
    for name in ["123_data", "456_data"] {
        fs::create_dir(f.path(name)).unwrap();
    }
    f.svc.scan(&f.root).unwrap();
    fs::remove_dir(f.path("456_data")).unwrap();
    assert!(f
        .svc
        .record(
            &f.root,
            &["123_data".into(), "456_data".into()],
            &f.flags(),
            "maker"
        )
        .is_err());
    for name in ["123_data", "456_data"] {
        assert_eq!(
            f.svc.store.detail(&f.root.path, name).unwrap()["state"],
            "unrecorded"
        );
    }
}
#[test]
fn keep_until_compares_against_the_local_calendar() {
    for timestamp in ["2026-10-16T01:00:00Z", "2026-10-16T23:00:00Z"] {
        let now = OffsetDateTime::parse(timestamp, &Rfc3339).unwrap();
        let (local, _) = attribution::local_calendar(now).unwrap();
        let format = time::format_description::parse("[year]-[month]-[day]").unwrap();
        let today = time::Date::parse(&local, &format).unwrap();
        assert!(validate_date_at(&today.to_string(), now).is_err());
        assert!(validate_date_at(&today.next_day().unwrap().to_string(), now).is_ok());
    }
}
