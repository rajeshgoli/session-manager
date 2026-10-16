use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
};
struct Fixture {
    dir: PathBuf,
    manager: ManagedArtifacts,
    root: ArtifactRoot,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "sm-pressure-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(dir.join("root")).unwrap();
        fs::create_dir(dir.join("cold")).unwrap();
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
        let conn = manager.store.open().unwrap();
        store::set_meta(&conn, &format!("evaluated:{}", root.path), &now()).unwrap();
        moves::TEST_MOUNTED.set(true);
        Self { dir, manager, root }
    }
    fn conn(&self) -> Connection {
        self.manager.store.open().unwrap()
    }
    fn live(&self, name: &str, bytes: usize) {
        self.manager
            .create(
                &self.root,
                name,
                &RecordFlags {
                    note: "test data".into(),
                    consumers: vec!["#1".into()],
                    ..Default::default()
                },
                "maker",
                "maker",
            )
            .unwrap();
        fs::write(
            Path::new(&self.root.path).join(name).join("data"),
            vec![b'a'; bytes],
        )
        .unwrap();
        self.manager.done(&self.root, name, "maker").unwrap();
        self.conn()
            .execute(
                "UPDATE artifacts SET recipe_complete=1 WHERE name=?1",
                [name],
            )
            .unwrap();
        self.conn().execute("INSERT INTO artifact_jobs(root,name,job_id,state,wall_seconds) VALUES(?1,?2,?2,'succeeded',60)",params![self.root.path,name]).unwrap();
        self.manager.measure_root(&self.root).unwrap();
    }
    fn frozen(&self, name: &str, bytes: u64, data: u64) {
        let path = Path::new(&self.root.cold_store).join(".frozen").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("part"), vec![b'x'; bytes as usize]).unwrap();
        fs::create_dir(Path::new(&self.root.path).join(name)).unwrap();
        self.conn().execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at,recorded_at,recipe_complete,frozen_bytes,data_bytes) VALUES(?1,?2,'frozen','cold',?2,'queue','owner/repo',?3,?3,1,?4,?5)",params![self.root.path,name,now(),bytes,data]).unwrap();
        self.conn().execute("INSERT INTO artifact_jobs(root,name,job_id,state,wall_seconds) VALUES(?1,?2,?2,'succeeded',60)",params![self.root.path,name]).unwrap();
    }
    fn disks(&self, root_free: u64, cold_free: u64) {
        let root = self.root.path.clone();
        queue::TEST_DISKS.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path| {
                Ok(if path == root {
                    (root_free, 10, 1)
                } else {
                    (cold_free, 10, 2)
                })
            }))
        });
    }
    fn history(&self, name: &str, bytes: u64, days: i64, state: &str) {
        self.conn()
            .execute(
                "INSERT INTO artifact_completions VALUES(?1,?2,?3,?4,?5,'root')",
                params![
                    name,
                    self.root.path,
                    state,
                    (OffsetDateTime::now_utc() - time::Duration::days(days))
                        .format(&Rfc3339)
                        .unwrap(),
                    bytes
                ],
            )
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        queue::TEST_DISKS.with(|h| *h.borrow_mut() = None);
        moves::TEST_MOUNTED.set(false);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn enough_is_per_filesystem_and_ignores_old_or_failed_jobs() {
    let f = Fixture::new();
    f.disks(100, 100);
    f.history("recent", 900, 1, "succeeded");
    f.history("old", 9000, 31, "succeeded");
    f.history("failed", 9900, 1, "failed");
    f.frozen("frozen", 50, 700);
    assert_eq!(enough(&f.conn(), &f.manager, 1, 100).unwrap(), 900);
    assert_eq!(enough(&f.conn(), &f.manager, 2, 100).unwrap(), 700);
    assert_eq!(enough(&f.conn(), &f.manager, 3, 100).unwrap(), 100);
    assert_eq!(target(100, 10, 150), 160);
    assert!(!satisfied(200, 50, 160, 0));
    assert!(satisfied(200, 50, 160, 10));
}
#[test]
fn ranking_moves_infinity_wait_floor_and_ties() {
    let f = Fixture::new();
    let make = |name: &str, action, freed, wait_minutes| score::Candidate {
        root: f.root.clone(),
        name: name.into(),
        set_id: name.into(),
        action,
        freed,
        wait_minutes,
    };
    let mut candidates = vec![
        make("z", "deleting", 100, 1.0),
        make("b", "freezing", 200, 2.0),
        make("a", "freezing", 200, 2.0),
        make("move", "moving", 1, 0.0),
        make("tiny", "deleting", 10, 0.0),
    ];
    score::sort(&mut candidates);
    assert_eq!(
        candidates
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["move", "a", "b", "z", "tiny"]
    );
}
#[test]
fn all_deletion_protections_and_internal_set_inputs() {
    let f = Fixture::new();
    f.live("a", 8192);
    f.live("b", 8192);
    let check = || {
        let a = artifact(&f.conn(), &f.root.path, "a").unwrap();
        score::deletion_eligible(&f.conn(), &f.root.path, &[a]).is_ok()
    };
    assert!(check());
    for (column, value, reset) in [
        ("recipe_complete", "0", "1"),
        ("gpu_hours", "1", "NULL"),
        ("gpu_cost_usd", "1", "NULL"),
        ("project", "1", "0"),
        ("keep_live_until", "'2999-01-01'", "NULL"),
        ("state", "'building'", "'live'"),
    ] {
        f.conn()
            .execute(
                &format!("UPDATE artifacts SET {column}={value} WHERE name='a'"),
                [],
            )
            .unwrap();
        assert!(!check(), "{column}");
        f.conn()
            .execute(
                &format!("UPDATE artifacts SET {column}={reset} WHERE name='a'"),
                [],
            )
            .unwrap();
    }
    f.conn()
        .execute(
            "UPDATE artifact_jobs SET state='running' WHERE name='a'",
            [],
        )
        .unwrap();
    assert!(!check());
    f.conn()
        .execute(
            "UPDATE artifact_jobs SET state='succeeded' WHERE name='a'",
            [],
        )
        .unwrap();
    f.conn()
        .execute(
            "INSERT INTO artifact_inputs(root,name,input,job_id) VALUES(?1,'b','a','input')",
            [&f.root.path],
        )
        .unwrap();
    assert!(!check());
    f.conn()
        .execute("UPDATE artifacts SET set_id='a' WHERE name='b'", [])
        .unwrap();
    assert!(check());
    f.conn()
        .execute("UPDATE artifact_inputs SET name='a',input='b'", [])
        .unwrap();
    f.conn()
        .execute("UPDATE artifacts SET state='deleted' WHERE name='b'", [])
        .unwrap();
    assert!(!check());
    f.conn().execute("DELETE FROM artifact_inputs", []).unwrap();
    f.conn().execute("INSERT INTO artifact_rebuilds(id,root,set_id,plan_json,execution_json,state,started_at) VALUES('failed',?1,'a','{}','{}','failed',?2)",params![f.root.path,now()]).unwrap();
    assert!(!check());
}
#[test]
fn move_freeze_exclusions_and_set_rebuild_cost_deduplicates_jobs() {
    let f = Fixture::new();
    f.live("a", 16384);
    f.live("b", 16384);
    f.disks(0, 1_000_000);
    f.conn()
        .execute("UPDATE artifacts SET set_id='a'", [])
        .unwrap();
    f.conn()
        .execute("UPDATE artifact_jobs SET job_id='shared'", [])
        .unwrap();
    let list = score::candidates(&f.manager, 1).unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].action, "moving");
    assert_eq!(
        list.iter()
            .find(|c| c.action == "deleting")
            .unwrap()
            .wait_minutes,
        1.0
    );
    f.conn()
        .execute("UPDATE artifacts SET project=1 WHERE name='b'", [])
        .unwrap();
    assert_eq!(
        score::candidates(&f.manager, 1)
            .unwrap()
            .iter()
            .map(|c| c.action)
            .collect::<Vec<_>>(),
        ["moving"]
    );
    f.conn()
        .execute(
            "UPDATE artifacts SET project=0,keep_live_until='2999-01-01' WHERE name='b'",
            [],
        )
        .unwrap();
    assert_eq!(score::candidates(&f.manager, 1).unwrap().len(), 1);
    f.conn()
        .execute(
            "UPDATE artifacts SET keep_live_until=NULL,op='rebuilding:job' WHERE name='b'",
            [],
        )
        .unwrap();
    assert!(score::candidates(&f.manager, 1).unwrap().is_empty());
    f.conn()
        .execute("UPDATE artifacts SET op=NULL", [])
        .unwrap();
    moves::TEST_MOUNTED.set(false);
    assert_eq!(
        score::candidates(&f.manager, 1)
            .unwrap()
            .iter()
            .map(|c| c.action)
            .collect::<Vec<_>>(),
        ["deleting"]
    );
}
#[test]
fn ratio_sample_cached_until_stamp_changes() {
    let f = Fixture::new();
    f.live("a", 8192);
    let get = || artifact(&f.conn(), &f.root.path, "a").unwrap();
    let ratio = score::ratio(&f.manager, &f.root, &get()).unwrap();
    assert!(ratio > 10.0);
    f.conn()
        .execute(
            "UPDATE artifacts SET ratio_sampled_at='cached' WHERE name='a'",
            [],
        )
        .unwrap();
    assert_eq!(score::ratio(&f.manager, &f.root, &get()).unwrap(), ratio);
    assert_eq!(get()["ratio_sampled_at"], "cached");
    fs::write(Path::new(&f.root.path).join("a/new"), b"changed size").unwrap();
    score::ratio(&f.manager, &f.root, &get()).unwrap();
    assert_ne!(get()["ratio_sampled_at"], "cached");
}
#[test]
fn forced_low_disk_moves_real_folders_in_score_order_and_stops() {
    let f = Fixture::new();
    f.live("small", 8192);
    f.live("large", 32768);
    let large = artifact(&f.conn(), &f.root.path, "large").unwrap()["disk_bytes"]
        .as_u64()
        .unwrap();
    f.history("target", large, 0, "succeeded");
    let root = f.root.path.clone();
    let db = f.manager.store.clone();
    queue::TEST_DISKS.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |path| {
            if path != root {
                return Ok((10_000_000, 10, 2));
            }
            let free = db
                .list(&root)?
                .iter()
                .filter(|a| a["location"] == "cold")
                .map(|a| a["disk_bytes"].as_u64().unwrap_or(0))
                .sum();
            Ok((free, 10, 1))
        }))
    });
    pass(&f.manager, &f.root.path, 0).unwrap();
    assert_eq!(
        artifact(&f.conn(), &f.root.path, "large").unwrap()["location"],
        "cold"
    );
    assert_eq!(
        artifact(&f.conn(), &f.root.path, "small").unwrap()["location"],
        "root"
    );
    assert_eq!(
        fs::read(Path::new(&f.root.path).join("large/data")).unwrap(),
        vec![b'a'; 32768]
    );
    let events = store::query(
        &f.conn(),
        "SELECT name FROM artifact_events WHERE kind='pressure_action' ORDER BY id",
        &[],
    )
    .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["name"], "large");
}
#[test]
fn report_only_and_enforcing_choose_same_frozen_deletions() {
    for report in [true, false] {
        let mut f = Fixture::new();
        f.frozen("a", 100, 300);
        f.frozen("b", 100, 300);
        f.frozen("c", 100, 300);
        if !report {
            f.manager.config.deletion_mode = "enforcing".into();
        }
        let root = f.root.path.clone();
        let db = f.manager.store.clone();
        queue::TEST_DISKS.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |path| {
                if path == root {
                    return Ok((1_000_000, 10, 1));
                }
                let removed = db
                    .list(&root)?
                    .iter()
                    .filter(|a| a["state"] == "deleted")
                    .count() as u64;
                Ok((100 + removed * 100, 10, 2))
            }))
        });
        pass(&f.manager, &f.root.cold_store, 0).unwrap();
        let events = store::query(
            &f.conn(),
            "SELECT name FROM artifact_events WHERE kind='pressure_action' ORDER BY id",
            &[],
        )
        .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|a| a["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        let a = f.manager.store.detail(&f.root.path, "a").unwrap();
        assert_eq!(a["state"], if report { "frozen" } else { "deleted" });
        assert_eq!(a["needs_owner"], report);
        if report {
            pass(&f.manager, &f.root.cold_store, 0).unwrap();
            let count: i64 = f
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM artifact_events WHERE kind='would_delete'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 2);
        }
    }
}
#[test]
fn pressure_notifications_only_for_exhaustion_rate_limit_and_retry() {
    let f = Fixture::new();
    f.disks(0, 1_000_000);
    f.history("need", 100, 0, "succeeded");
    let at = OffsetDateTime::now_utc();
    let mut sent = 0;
    notify(&f.manager, at, |_, _| {
        sent += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(sent, 0);
    pass(&f.manager, &f.root.path, 0).unwrap();
    assert!(notify(&f.manager, at, |_, _| bail!("inbox down")).is_err());
    for time in [
        at,
        at + time::Duration::hours(1),
        at + time::Duration::hours(6),
    ] {
        notify(&f.manager, time, |_, body| {
            assert!(body.contains("cannot free more"));
            sent += 1;
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(sent, 2);
}
#[test]
fn needs_you_outlives_recent_events_and_clears_with_cause() {
    let f = Fixture::new();
    f.live("a", 4096);
    store::event(
        &f.conn(),
        &f.root.path,
        "a",
        "project_added",
        "maker",
        json!({}),
    )
    .unwrap();
    f.conn()
        .execute("UPDATE artifacts SET project=1", [])
        .unwrap();
    for _ in 0..60 {
        store::event(&f.conn(), &f.root.path, "a", "viewed", "maker", json!({})).unwrap();
    }
    assert_eq!(
        f.manager.store.detail(&f.root.path, "a").unwrap()["needs_owner"],
        true
    );
    f.conn()
        .execute("UPDATE artifacts SET project=0", [])
        .unwrap();
    assert_eq!(
        f.manager.store.detail(&f.root.path, "a").unwrap()["needs_owner"],
        false
    );
    fs::remove_dir_all(Path::new(&f.root.path).join("a")).unwrap();
    f.manager.scan(&f.root).unwrap();
    assert_eq!(
        f.manager.store.detail(&f.root.path, "a").unwrap()["needs_owner"],
        true
    );
    store::event(
        &f.conn(),
        &f.root.path,
        "a",
        "owner_opened",
        "owner",
        json!({}),
    )
    .unwrap();
    assert_eq!(
        f.manager.store.detail(&f.root.path, "a").unwrap()["needs_owner"],
        false
    );
}

#[test]
fn report_only_still_freezes_real_hot_cold_store_folders() {
    let f = Fixture::new();
    f.live("live", 65536);
    f.disks(1_000_000, 1_000_000);
    ops::request(&f.manager, &f.root, "live", "moving", "maker").unwrap();
    ops::resume(&f.manager).unwrap();
    f.frozen("reserve", 100, 70000);
    let root = f.root.path.clone();
    let db = f.manager.store.clone();
    queue::TEST_DISKS.with(|h| {
        *h.borrow_mut() = Some(Box::new(move |path| {
            if path == root {
                return Ok((1_000_000, 10, 1));
            }
            let a = artifact(&db.open()?, &root, "live")?;
            Ok((if a["state"] == "frozen" { 75000 } else { 9000 }, 10, 2))
        }))
    });
    pass(&f.manager, &f.root.cold_store, 0).unwrap();
    let a = f.manager.store.detail(&f.root.path, "live").unwrap();
    assert_eq!(a["state"], "frozen");
    assert!(Path::new(&f.root.path).join("live/FROZEN.md").is_file());
    assert!(a["ratio_estimate"].as_f64().unwrap() > 10.0);
    let actions = store::query(
        &f.conn(),
        "SELECT detail_json FROM artifact_events WHERE kind='pressure_action'",
        &[],
    )
    .unwrap();
    assert_eq!(actions.len(), 1);
    assert!(actions[0]["detail_json"]
        .as_str()
        .unwrap()
        .contains("freezing"));
}

#[test]
fn waiting_jobs_are_scoped_to_their_filesystem_and_exclude_thaw_holds() {
    let f = Fixture::new();
    f.disks(100, 100);
    f.conn().execute("INSERT INTO artifact_job_reservations(job_id,root,outputs_json,inputs_json,disk_bytes,first_disk_hold_at) VALUES('thaw',?1,'[]','[]',99999,?2),('disk',?1,'[]','[]',300,?2)",params![f.root.path,now()]).unwrap();
    f.conn()
        .execute(
            "INSERT INTO artifact_disk_holds VALUES('disk',?1,'cold','cold job')",
            [&f.root.path],
        )
        .unwrap();
    assert_eq!(waiting(&f.conn(), &f.manager, 1).unwrap(), 0);
    assert_eq!(waiting(&f.conn(), &f.manager, 2).unwrap(), 300);
    f.conn()
        .execute(
            "UPDATE artifact_job_reservations SET start_bytes=0 WHERE job_id='disk'",
            [],
        )
        .unwrap();
    assert_eq!(waiting(&f.conn(), &f.manager, 2).unwrap(), 0);
}

#[test]
fn cold_and_unknown_completion_placement_do_not_inflate_root_target() {
    let f = Fixture::new();
    f.disks(100, 100);
    f.history("root-job", 200, 0, "succeeded");
    f.history("cold-job", 9000, 0, "succeeded");
    f.history("unknown-old-job", 9900, 0, "succeeded");
    f.conn()
        .execute(
            "UPDATE artifact_completions SET placement='cold' WHERE job_id='cold-job'",
            [],
        )
        .unwrap();
    f.conn()
        .execute(
            "UPDATE artifact_completions SET placement=NULL WHERE job_id='unknown-old-job'",
            [],
        )
        .unwrap();
    assert_eq!(enough(&f.conn(), &f.manager, 1, 100).unwrap(), 200);
    // Cold space targets are driven by the largest thaw, not output history.
    assert_eq!(enough(&f.conn(), &f.manager, 2, 100).unwrap(), 100);
}

#[test]
fn report_only_renews_acknowledged_warning_without_repeating_current_warning() {
    let f = Fixture::new();
    f.frozen("a", 100, 300);
    f.manager.delete_pressure(&f.root, "a").unwrap();
    for activity in [
        "owner_opened",
        "owner_investigate",
        "owner_keep_live",
        "marks",
        "recorded",
    ] {
        store::event(&f.conn(), &f.root.path, "a", activity, "owner", json!({})).unwrap();
        assert_eq!(
            f.manager.store.detail(&f.root.path, "a").unwrap()["needs_owner"],
            false,
            "{activity}"
        );
        f.manager.delete_pressure(&f.root, "a").unwrap();
        assert_eq!(
            f.manager.store.detail(&f.root.path, "a").unwrap()["needs_owner"],
            true,
            "{activity}"
        );
        f.manager.delete_pressure(&f.root, "a").unwrap();
    }
    let count: i64 = f
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM artifact_events WHERE kind='would_delete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 6);
    assert_eq!(
        artifact(&f.conn(), &f.root.path, "a").unwrap()["state"],
        "frozen"
    );
}

#[test]
fn flexible_disk_hold_targets_only_root_even_before_readmission() {
    let f = Fixture::new();
    f.disks(100, 100);
    f.conn().execute("INSERT INTO artifact_job_reservations(job_id,root,outputs_json,inputs_json,disk_bytes,first_disk_hold_at) VALUES('job',?1,'[]','[]',300,?2)",params![f.root.path,now()]).unwrap();
    // Read compatibility for a hold persisted by the earlier build.
    for role in ["either", "root"] {
        f.conn()
            .execute(
                "INSERT OR REPLACE INTO artifact_disk_holds VALUES('job',?1,?2,'flexible job')",
                params![f.root.path, role],
            )
            .unwrap();
        assert_eq!(waiting(&f.conn(), &f.manager, 1).unwrap(), 300);
        assert_eq!(waiting(&f.conn(), &f.manager, 2).unwrap(), 0);
        // Root is now large enough, but the job has not been re-admitted yet.
        f.disks(1000, 100);
        assert_eq!(waiting(&f.conn(), &f.manager, 2).unwrap(), 0);
    }
}

#[test]
fn rejected_final_delete_check_cancels_intent_and_cannot_recover_later() {
    for protection in ["project=1", "keep_live_until='2999-01-01'"] {
        let mut f = Fixture::new();
        f.live("a", 4096);
        f.manager.config.deletion_mode = "enforcing".into();
        let db = f.manager.store.clone();
        delete::BEFORE_REMOVAL.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                db.open()
                    .unwrap()
                    .execute(
                        &format!("UPDATE artifacts SET {protection} WHERE name='a'"),
                        [],
                    )
                    .unwrap();
            }))
        });
        assert!(f.manager.delete_pressure(&f.root, "a").is_err());
        let a = artifact(&f.conn(), &f.root.path, "a").unwrap();
        assert!(a["op"].is_null());
        assert!(a["delete_reason"].is_null());
        f.conn()
            .execute("UPDATE artifacts SET project=0,keep_live_until=NULL", [])
            .unwrap();
        f.conn()
            .execute(
                "UPDATE artifact_events SET at='2000-01-01T00:00:00Z' WHERE kind='op_failed'",
                [],
            )
            .unwrap();
        f.manager.recover_deletions(&f.root).unwrap();
        assert_eq!(
            artifact(&f.conn(), &f.root.path, "a").unwrap()["state"],
            "live"
        );
        assert_eq!(
            fs::read(Path::new(&f.root.path).join("a/data")).unwrap(),
            vec![b'a'; 4096]
        );
    }
}

#[test]
fn disappeared_device_notices_are_resolved_and_never_republished() {
    let f = Fixture::new();
    f.disks(1000, 1000);
    for device in [2, 999] {
        store::set_meta(
            &f.conn(),
            &format!("pressure_pending:{device}"),
            &json!({"body":"old pressure"}).to_string(),
        )
        .unwrap();
    }
    moves::TEST_MOUNTED.set(false);
    periodic(&f.manager).unwrap();
    let mut resolved = Vec::new();
    resolve_notices(&f.manager, |key| {
        resolved.push(key.to_owned());
        Ok(())
    })
    .unwrap();
    assert!(resolved.contains(&"pressure_pending:2:".into()));
    assert!(resolved.contains(&"pressure_pending:999:".into()));
    let mut sent = 0;
    notify(
        &f.manager,
        OffsetDateTime::now_utc() + time::Duration::hours(7),
        |_, _| {
            sent += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(sent, 0);
}
