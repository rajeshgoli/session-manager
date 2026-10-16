use super::*;
use crate::work_claims::{BatchFetch, GhItem, WorkKind};
struct Source(bool);
impl WorkItemSource for Source {
    fn fetch(&self, _repo: &str, numbers: &[i64]) -> std::result::Result<BatchFetch, String> {
        if !self.0 {
            return Err("offline".into());
        }
        Ok(numbers
            .iter()
            .map(|n| {
                (
                    *n,
                    ItemFetch::Found(Box::new(GhItem {
                        is_draft: false,
                        kind: WorkKind::Ticket,
                        title: "consumer".into(),
                        state: "closed".into(),
                        state_reason: None,
                        url: String::new(),
                        head_ref: None,
                        head_sha: None,
                        closed_at: None,
                        merged_at: None,
                        closing_refs: None,
                    })),
                )
            })
            .collect())
    }
}
struct Fixture {
    _files: super::super::tests::Fixture,
    manager: ManagedArtifacts,
    root: ArtifactRoot,
}
impl Fixture {
    fn new() -> Self {
        let files = super::super::tests::Fixture::new();
        let root = ArtifactRoot {
            path: files.path("").display().to_string(),
            cold_store: files.path("cold").display().to_string(),
            repo: "owner/repo".into(),
            ..Default::default()
        };
        fs::create_dir(&root.cold_store).unwrap();
        let manager = ManagedArtifacts::new(&ManagedArtifactsConfig {
            db_path: files.path("db").display().to_string(),
            mirror_dir: files.path("mirrors").display().to_string(),
            roots: vec![root.clone()],
            nightly_time: "00:00".into(),
            ..Default::default()
        })
        .unwrap();
        Self {
            _files: files,
            manager,
            root,
        }
    }
    fn add(&self, name: &str) {
        self.manager
            .create(
                &self.root,
                name,
                &RecordFlags {
                    note: "data".into(),
                    consumers: vec!["#1".into()],
                    ..Default::default()
                },
                "maker",
                "Maker",
            )
            .unwrap();
        self.manager.done(&self.root, name, "maker").unwrap();
    }
    fn evaluate(&self) {
        evaluate_with_named(
            &self.manager,
            &self.root,
            &Source(true),
            Ok(BTreeMap::new()),
        )
        .unwrap();
    }
    fn hot(&self, name: &str) -> bool {
        is_hot(
            &self.manager.store.open().unwrap(),
            &self.root.path,
            name,
            14,
            OffsetDateTime::now_utc(),
        )
        .unwrap()
    }
}
#[test]
fn managed_artifacts_hotcold_each_retention_condition() {
    let f = Fixture::new();
    f.add("sample");
    assert!(f.hot("sample"));
    f.evaluate();
    assert!(!f.hot("sample"));
    let conn = f.manager.store.open().unwrap();
    for (field, value) in [
        ("project", json!(1)),
        ("keep_until", json!("2099-01-01")),
        ("keep_live_until", json!("2099-01-01T00:00:00Z")),
        ("last_used_at", json!(now())),
        ("thawed_at", json!(now())),
        ("state", json!("unrecorded")),
    ] {
        let original = artifact(&conn, &f.root.path, "sample").unwrap()[field].clone();
        let sql = format!("UPDATE artifacts SET {field}=?1 WHERE name='sample'");
        let encode = |v: &Value| -> rusqlite::types::Value {
            match v {
                Value::Null => rusqlite::types::Value::Null,
                Value::Number(n) => rusqlite::types::Value::Integer(n.as_i64().unwrap()),
                Value::String(s) => rusqlite::types::Value::Text(s.clone()),
                _ => panic!(),
            }
        };
        conn.execute(&sql, [encode(&value)]).unwrap();
        assert!(f.hot("sample"), "{field}");
        conn.execute(&sql, [encode(&original)]).unwrap();
        assert!(!f.hot("sample"), "reset {field}");
    }
    conn.execute("UPDATE artifact_consumers SET last_state='OPEN'", [])
        .unwrap();
    assert!(f.hot("sample"));
    f.evaluate();
    conn.execute(
        "INSERT INTO artifact_signals(root,name,kind,evidence) VALUES(?1,'sample','named','spec')",
        [&f.root.path],
    )
    .unwrap();
    assert!(f.hot("sample"));
    conn.execute("DELETE FROM artifact_signals", []).unwrap();
    conn.execute("INSERT INTO artifact_job_reservations(job_id,root,inputs_json,outputs_json,disk_bytes) VALUES('job',?1,'[\"sample\"]','[]',0)",[&f.root.path]).unwrap();
    assert!(f.hot("sample"));
    conn.execute("DELETE FROM artifact_job_reservations", [])
        .unwrap();
    conn.execute("INSERT INTO artifact_signals(root,name,kind,evidence) VALUES(?1,'sample','input_of','other')",[&f.root.path]).unwrap();
    assert!(!f.hot("sample"));
}
#[test]
fn managed_artifacts_hotcold_failures_are_conservative() {
    let f = Fixture::new();
    f.add("sample");
    f.evaluate();
    assert!(!f.hot("sample"));
    evaluate_with_named(
        &f.manager,
        &f.root,
        &Source(false),
        Err(anyhow::anyhow!("offline")),
    )
    .unwrap();
    assert!(f.hot("sample"));
    evaluate_with_named(
        &f.manager,
        &f.root,
        &Source(true),
        Ok(BTreeMap::from([("sample".into(), vec!["spec.md".into()])])),
    )
    .unwrap();
    evaluate_with_named(
        &f.manager,
        &f.root,
        &Source(true),
        Err(anyhow::anyhow!("offline")),
    )
    .unwrap();
    assert!(f.hot("sample"));
    let conn = f.manager.store.open().unwrap();
    assert_eq!(
        store::query(
            &conn,
            "SELECT evidence FROM artifact_signals WHERE kind='named'",
            &[]
        )
        .unwrap()[0]["evidence"],
        "spec.md"
    );
}
#[test]
fn managed_artifacts_hotcold_mirror_exclusions_aliases_and_case() {
    let f = Fixture::new();
    f.add("sample");
    f.add("excluded");
    let checkout = Path::new(&f.root.path).join("repo");
    fs::create_dir(&checkout).unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
    ] {
        git(
            Some(&checkout),
            &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            false,
        )
        .unwrap();
    }
    fs::create_dir_all(checkout.join("docs/working")).unwrap();
    fs::write(checkout.join("spec.md"), "ROOT/SAMPLE root/alias\n").unwrap();
    fs::write(checkout.join("docs/working/draft.md"), "root/excluded").unwrap();
    git(Some(&checkout), &["add".into(), ".".into()], false).unwrap();
    git(
        Some(&checkout),
        &["commit".into(), "-m".into(), "fixture".into()],
        false,
    )
    .unwrap();
    let conn = f.manager.store.open().unwrap();
    conn.execute("INSERT INTO artifact_aliases(root,alias,target_name,target_rel) VALUES(?1,'alias','sample','')",[&f.root.path]).unwrap();
    let found = named_from_mirror(&f.manager, &f.root, &checkout).unwrap();
    assert_eq!(found.get("sample").unwrap(), &vec!["spec.md".to_owned()]);
    assert!(!found.contains_key("excluded"));
}
#[test]
fn managed_artifacts_hotcold_whole_sets_order_and_grace() {
    let f = Fixture::new();
    for n in ["small", "large", "kept", "mate", "unrecorded"] {
        f.add(n);
    }
    f.evaluate();
    let conn = f.manager.store.open().unwrap();
    conn.execute(
        "UPDATE artifacts SET disk_bytes=CASE name WHEN 'large' THEN 100 ELSE 1 END",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE artifacts SET set_id='together' WHERE name IN ('kept','mate')",
        [],
    )
    .unwrap();
    conn.execute("UPDATE artifacts SET project=1 WHERE name='kept'", [])
        .unwrap();
    conn.execute("UPDATE artifacts SET state='unrecorded',grace_until='2000-01-01T00:00:00Z' WHERE name='unrecorded'",[]).unwrap();
    moves::TEST_MOUNTED.set(true);
    assert_eq!(
        freeze_candidates(&f.manager, &f.root, OffsetDateTime::now_utc()).unwrap(),
        vec!["large", "small", "unrecorded"]
    );
    moves::TEST_MOUNTED.set(false);
    assert!(
        freeze_candidates(&f.manager, &f.root, OffsetDateTime::now_utc())
            .unwrap()
            .is_empty()
    );
    assert!(nightly_due(&f.manager, &f.root, OffsetDateTime::now_utc()).unwrap());
    let (today, _) = attribution::local_calendar(OffsetDateTime::now_utc()).unwrap();
    store::set_meta(&conn, &format!("cold_night:{}", f.root.path), &today).unwrap();
    assert!(!nightly_due(&f.manager, &f.root, OffsetDateTime::now_utc()).unwrap());
}
