use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture() -> (PathBuf, WorkClaimStore, OffsetDateTime) {
    let root = std::env::temp_dir().join(format!(
        "sm-loose-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let store = WorkClaimStore::new(root.with_extension("db"));
    store.ensure_schema().unwrap();
    (root, store, OffsetDateTime::now_utc())
}
fn ctx(root: &Path, now: OffsetDateTime) -> Context<'_> {
    Context {
        root,
        sessions: &[],
        jobs: &[],
        pending: &[],
        now,
    }
}
fn age(path: &Path, now: OffsetDateTime) {
    let old = UNIX_EPOCH
        + std::time::Duration::from_secs(
            (now - RETENTION - time::Duration::days(1)).unix_timestamp() as u64,
        );
    if path.is_dir() {
        for child in fs::read_dir(path).unwrap() {
            age(&child.unwrap().path(), now);
        }
    }
    fs::File::open(path).unwrap().set_modified(old).unwrap();
}
fn enroll(root: &Path, store: &WorkClaimStore, now: OffsetDateTime) {
    sweep(store, &ctx(root, now - RETENTION - time::Duration::days(1))).unwrap();
}
#[test]
fn first_pass_recent_nested_changes_cap_and_ignored_entries() {
    let (root, store, now) = fixture();
    let deep = root.join("scripts/a/b/c/d/e");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("log"), "live").unwrap();
    fs::create_dir(root.join("checkout")).unwrap();
    fs::write(root.join("checkout/.git"), "gitdir").unwrap();
    fs::create_dir(root.join("main")).unwrap();
    fs::create_dir(root.join("main/.git")).unwrap();
    fs::write(root.join(".hidden"), "").unwrap();
    sweep(&store, &ctx(&root, now)).unwrap();
    let rows = list(&store, &ctx(&root, now)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].blocked_by.as_deref(), Some("recent change"));
    // Old ancestors do not conceal a recent file five folders deep.
    let mut dir = deep.clone();
    while dir != root {
        fs::File::open(&dir)
            .unwrap()
            .set_modified(SystemTime::from(now - RETENTION - time::Duration::days(1)))
            .unwrap();
        dir.pop();
    }
    assert_eq!(
        walk(
            &root.join("scripts"),
            (now - RETENTION).unix_timestamp(),
            WALK_LIMIT
        )
        .unwrap()
        .reason,
        Some("recent change")
    );
    age(&root.join("scripts"), now);
    assert_eq!(
        walk(&root.join("scripts"), (now - RETENTION).unix_timestamp(), 3)
            .unwrap()
            .reason,
        Some("too large to check")
    );
    sweep(&store, &ctx(&root, now)).unwrap();
    assert!(root.join("scripts").exists());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn retention_keeps_and_symlinks() {
    let (root, store, now) = fixture();
    let file = root.join("note.md");
    fs::write(&file, "note").unwrap();
    age(&file, now);
    let outside = root.with_extension("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious"), "keep").unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    // Advancing the clock ages the symlink itself without touching its target.
    let later = now + RETENTION + time::Duration::days(1);
    sweep(&store, &ctx(&root, now)).unwrap();
    assert!(file.exists());
    store
        .set_worktree_keep(file.to_str().unwrap(), "owner", "keep")
        .unwrap();
    sweep(&store, &ctx(&root, later)).unwrap();
    assert!(file.exists());
    assert!(!exists(&link));
    assert!(outside.join("precious").exists());
    assert_eq!(
        list(&store, &ctx(&root, later)).unwrap()[0]
            .blocked_by
            .as_deref(),
        Some("keep")
    );
    store.clear_worktree_keep(file.to_str().unwrap()).unwrap();
    sweep(&store, &ctx(&root, later)).unwrap();
    assert!(!file.exists());
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}
#[test]
fn disappearance_and_inode_replacement_get_a_full_grace_period() {
    let (root, store, now) = fixture();
    let file = root.join("note");
    fs::write(&file, "one").unwrap();
    age(&file, now);
    enroll(&root, &store, now);
    let replacement = root.join(".new");
    fs::write(&replacement, "two").unwrap();
    age(&replacement, now);
    fs::rename(&replacement, &file).unwrap();
    sweep(&store, &ctx(&root, now)).unwrap();
    assert!(file.exists());
    assert_eq!(
        list(&store, &ctx(&root, now)).unwrap()[0].first_seen_at,
        now.format(&Rfc3339).unwrap()
    );
    fs::remove_file(&file).unwrap();
    sweep(&store, &ctx(&root, now)).unwrap();
    fs::write(&file, "three").unwrap();
    age(&file, now);
    sweep(&store, &ctx(&root, now + time::Duration::hours(1))).unwrap();
    assert!(file.exists());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn pending_and_live_sessions_block_automatic_and_manual_removal() {
    let (root, store, now) = fixture();
    let path = root.join("scripts");
    fs::create_dir(&path).unwrap();
    age(&path, now);
    enroll(&root, &store, now);
    let pending = [path.join("handoff.md")];
    let mut context = ctx(&root, now);
    context.pending = &pending;
    sweep(&store, &context).unwrap();
    assert!(path.exists());
    assert_eq!(
        list(&store, &context).unwrap()[0].blocked_by.as_deref(),
        Some("pending handoff note")
    );
    assert!(delete(&store, &context, &path).unwrap().unwrap().is_err());
    let sessions = [CleanupSession {
        id: "s".into(),
        name: "worker".into(),
        working_dir: path.join("nested").display().to_string(),
        retired: false,
        stopped: false,
        retired_at: None,
        local: true,
    }];
    context.pending = &[];
    context.sessions = &sessions;
    sweep(&store, &context).unwrap();
    assert!(path.exists());
    assert_eq!(
        list(&store, &context).unwrap()[0].blocked_by.as_deref(),
        Some("session worker")
    );
    context.sessions = &[];
    assert!(
        delete(&store, &context, &path)
            .unwrap()
            .unwrap()
            .unwrap()
            .removed
    );
    assert!(!path.exists());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn argv_paths_in_options_shell_strings_and_boundaries() {
    let p = "/Users/rajesh/worktrees/results";
    for arg in [
        format!("--output={p}/file"),
        format!("cat {p}/file; done"),
        format!("'{p}'"),
        format!("{p}:log"),
    ] {
        assert!(argv_mentions(&arg, p), "{arg}");
    }
    assert!(!argv_mentions(&format!("{p}-other"), p));
    assert!(argv_mentions(
        "cat ~/worktrees/results/log",
        "~/worktrees/results"
    ));
}
#[test]
fn a_replacement_during_removal_is_restored_and_its_row_reset() {
    let (root, store, now) = fixture();
    let path = root.join("note");
    fs::write(&path, "old").unwrap();
    age(&path, now);
    enroll(&root, &store, now);
    let old = fs::symlink_metadata(&path).unwrap();
    let replacement = root.join(".replacement");
    fs::write(&replacement, "new").unwrap();
    fs::rename(&replacement, &path).unwrap();
    let conn = store.open_write().unwrap();
    assert!(!remove(&conn, &ctx(&root, now), &path, old.dev(), old.ino(), |_| {}).unwrap());
    assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    assert_eq!(
        list(&store, &ctx(&root, now)).unwrap()[0].first_seen_at,
        now.format(&Rfc3339).unwrap()
    );
    // If another entry takes the old name, retain and list the moved replacement.
    let meta = fs::symlink_metadata(&path).unwrap();
    assert!(!remove(
        &conn,
        &ctx(&root, now),
        &path,
        meta.dev(),
        old.ino(),
        |_| {
            fs::write(&path, "third").unwrap();
        }
    )
    .unwrap());
    assert_eq!(fs::read_to_string(&path).unwrap(), "third");
    let rows = list(&store, &ctx(&root, now)).unwrap();
    assert!(rows[0].path.contains(".sm-quarantine"));
    assert_eq!(
        rows[0].blocked_by.as_deref(),
        Some("replaced during removal")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn active_queue_cwd_argv_and_tilde_references_block_removal() {
    use crate::queue::{CreateQueueJob, RetainedQueueStore};
    let (root, store, now) = fixture();
    let path = root.join("scripts");
    fs::create_dir(&path).unwrap();
    age(&path, now);
    enroll(&root, &store, now);
    let request = CreateQueueJob {
        local_submitter: None,
        job_type: "tests".into(),
        label: "guard".into(),
        requester_session_id: None,
        notify_session_id: "s".into(),
        cwd: path.display().to_string(),
        argv: Some(vec!["true".into()]),
        script: None,
        env: BTreeMap::new(),
        timeout_seconds: 60,
        cpu_percent: None,
        gpu_percent: None,
        memory_bytes: None,
        rank_tickets: None,
    };
    let mut job =
        RetainedQueueStore::create_queue_job_in_state_dir(&root.join(".queue"), request).unwrap();
    let variants = [
        (
            path.join("nested").display().to_string(),
            vec!["true".into()],
        ),
        (
            "/elsewhere".into(),
            vec![format!("--output={}/log", path.display())],
        ),
        (
            "/elsewhere".into(),
            vec![
                "bash".into(),
                "-c".into(),
                format!("cat {}/log", path.display()),
            ],
        ),
    ];
    for (cwd, argv) in variants {
        job.cwd = cwd;
        job.argv = Some(argv);
        let jobs = [job.clone()];
        let mut context = ctx(&root, now);
        context.jobs = &jobs;
        sweep(&store, &context).unwrap();
        assert!(path.exists());
        assert_eq!(
            list(&store, &context).unwrap()[0].blocked_by,
            Some(format!("queue job {}", job.id))
        );
        assert!(delete(&store, &context, &path).unwrap().unwrap().is_err());
    }
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn interrupted_quarantine_is_restored_and_manual_delete_can_remove_a_conflict() {
    let (root, store, now) = fixture();
    let path = root.join("note");
    fs::write(&path, "precious").unwrap();
    age(&path, now);
    enroll(&root, &store, now);
    let q = root.join(".sm-quarantine");
    fs::create_dir(&q).unwrap();
    let q = q.join("1-note");
    let conn = store.open_write().unwrap();
    conn.execute(
        "UPDATE loose_entries SET quarantine_path=?1",
        [q.to_str().unwrap()],
    )
    .unwrap();
    fs::rename(&path, &q).unwrap();
    sweep(&store, &ctx(&root, now)).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "precious");
    assert!(!q.exists());
    conn.execute(
        "UPDATE loose_entries SET quarantine_path=?1",
        [q.to_str().unwrap()],
    )
    .unwrap();
    fs::rename(&path, &q).unwrap();
    fs::write(&path, "new").unwrap();
    sweep(&store, &ctx(&root, now)).unwrap();
    assert!(q.exists());
    assert_eq!(list(&store, &ctx(&root, now)).unwrap().len(), 2);
    assert!(
        delete(&store, &ctx(&root, now), &q)
            .unwrap()
            .unwrap()
            .unwrap()
            .removed
    );
    assert!(path.exists());
    fs::remove_dir_all(root).unwrap();
}
