//! Eligibility (spec 1821 E1): one exclusion at a time from an agent that
//! qualifies.

use super::*;

const NOW: &str = "2026-09-30T13:05:00Z";
const COMPLETED: &str = "2026-09-30T12:05:00Z";

fn now() -> OffsetDateTime {
    OffsetDateTime::parse(NOW, &Rfc3339).unwrap()
}

/// far-1855 at 13:05: started by sm, finished at 12:05, idle since.
fn agent() -> SessionRecord {
    serde_json::from_value(json!({
        "id": "far01855", "name": "far-1855", "working_dir": "/repo",
        "tmux_session": "claude-far01855", "provider": "claude", "status": "running",
        "created_at": "2026-09-30T09:00:00Z", "last_activity": COMPLETED,
        "agent_task_completed_at": COMPLETED, "started_by_sm": true,
    }))
    .unwrap()
}

fn context() -> Context {
    Context {
        spawned: BTreeSet::new(),
        roles: BTreeSet::new(),
        followed: BTreeSet::new(),
        parents_of_live: BTreeSet::new(),
        watch: BTreeMap::from([(
            "far01855".to_owned(),
            json!({"state": "idle", "facts": {"you": null,
                "jobs": {"running": 0, "waiting": 0, "review": null}}}),
        )]),
    }
}

fn check(session: &SessionRecord, context: &Context) -> Option<&'static str> {
    why_not(session, context, now(), 60)
}

#[test]
fn a_finished_agent_sm_started_retires_after_sixty_idle_minutes() {
    assert_eq!(check(&agent(), &context()), None);
    // A minute earlier it has not waited long enough, measured from the
    // later of task-complete and the last activity.
    let early = now() - time::Duration::minutes(1);
    assert_eq!(
        why_not(&agent(), &context(), early, 60),
        Some("not idle long enough")
    );
    let mut active = agent();
    active.last_activity = "2026-09-30T12:30:00Z".to_owned();
    assert_eq!(check(&active, &context()), Some("not idle long enough"));
    assert_eq!(why_not(&active, &context(), now(), 15), None);
}

#[test]
fn legacy_spawns_and_handoff_successors_count_as_started_by_sm() {
    let mut mine = agent();
    mine.started_by_sm = false;
    assert_eq!(check(&mine, &context()), Some("not started by sm"));
    let mut spawned = context();
    spawned.spawned.insert("far01855".to_owned());
    assert_eq!(check(&mine, &spawned), None);
    mine.predecessor_session_id = Some("far01800".to_owned());
    assert_eq!(check(&mine, &context()), None);
}

#[test]
fn each_exclusion_keeps_it() {
    let context_with = |edit: &dyn Fn(&mut Context)| {
        let mut context = context();
        edit(&mut context);
        check(&agent(), &context)
    };
    assert_eq!(
        context_with(&|c| {
            c.roles.insert("far01855".to_owned());
        }),
        Some("registered in a role")
    );
    assert_eq!(
        context_with(&|c| {
            c.followed.insert("far01855".to_owned());
        }),
        Some("followed")
    );
    assert_eq!(
        context_with(&|c| {
            c.parents_of_live.insert("far01855".to_owned());
        }),
        Some("has live child agents")
    );
    let facts = |facts: Value| {
        move |c: &mut Context| {
            c.watch.get_mut("far01855").unwrap()["facts"] = facts.clone();
        }
    };
    let jobs = |running: u64, waiting: u64, review: Value| json!({"you": null, "jobs": {"running": running, "waiting": waiting, "review": review}});
    assert_eq!(
        context_with(&facts(json!({"you": {"kind": "message"}, "jobs": {}}))),
        Some("asked you a question")
    );
    assert_eq!(
        context_with(&facts(json!({"you": {"kind": "doc_review"}, "jobs": {}}))),
        Some("waits on your doc review")
    );
    assert_eq!(
        context_with(&facts(jobs(1, 0, Value::Null))),
        Some("has queue jobs")
    );
    assert_eq!(
        context_with(&facts(jobs(0, 1, Value::Null))),
        Some("has queue jobs")
    );
    assert_eq!(
        context_with(&facts(jobs(0, 0, json!({"pr_number": 7})))),
        Some("waits on a review")
    );
    assert_eq!(
        context_with(&|c| {
            c.watch.get_mut("far01855").unwrap()["state"] = json!("working");
        }),
        Some("not idle")
    );

    let with = |edit: &dyn Fn(&mut SessionRecord)| {
        let mut session = agent();
        edit(&mut session);
        check(&session, &context())
    };
    assert_eq!(
        with(&|s| s.node = "laptop".to_owned()),
        Some("on another machine")
    );
    assert_eq!(
        with(&|s| s.provider = "codex-app".to_owned()),
        Some("provider cannot be restored")
    );
    assert_eq!(
        with(&|s| s.agent_task_completed_at = None),
        Some("not finished")
    );
    assert_eq!(with(&|s| s.status = "stopped".to_owned()), Some("stopped"));
}

/// A fixture store holding `sessions`.
fn state_with(sessions: Value) -> (AppState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "sm-auto-retire-unit-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let state_file = dir.join("sessions.json");
    fs::write(&state_file, json!({"sessions": sessions}).to_string()).unwrap();
    let mut config = AppConfig::default();
    config.paths.state_file = state_file.display().to_string();
    config.sm_send.db_path = dir.join("message_queue.db").display().to_string();
    config.push.db_path = dir.join("owner_push.db").display().to_string();
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.log_dir = Some(dir.join("logs").display().to_string());
    (AppState::new(config), dir)
}

#[test]
fn a_doc_review_wakes_its_auto_retired_author_by_restoring_it() {
    let retired = |id: &str, source: &str| {
        json!({"id": id, "name": id, "working_dir": "/repo", "tmux_session": id,
            "provider": "claude", "status": "stopped", "completion_status": "retired",
            "created_at": COMPLETED, "last_activity": NOW, "completed_at": NOW,
            "parent_session_id": "parent01",
            "terminal_provenance": {"cause": "explicit_retire", "observed_at": NOW,
                "authority": "server_lifecycle", "source": source}})
    };
    let parent = json!({"id": "parent01", "name": "parent", "working_dir": "/repo",
        "tmux_session": "parent01", "provider": "claude", "status": "running",
        "created_at": COMPLETED, "last_activity": NOW});
    let (state, dir) = state_with(json!([
        parent,
        retired("author01", crate::sessions::AUTO_RETIRE_SOURCE),
        retired("author02", "operator"),
    ]));
    let doc = |author: &str| crate::owner_docs::OwnerDoc {
        id: "doc1".to_owned(),
        repo: "acme/far".to_owned(),
        path: "docs/memo.html".to_owned(),
        pr_number: None,
        author_session_id: author.to_owned(),
        author_session_name: None,
        title: "Memo".to_owned(),
        note: None,
        retracted_at: None,
        created_at: NOW.to_owned(),
        updated_at: NOW.to_owned(),
    };
    assert_eq!(
        docs::review_wake_target(&state, &doc("author01")).as_deref(),
        Some("author01")
    );
    let author = state
        .session_store
        .get_session("author01")
        .unwrap()
        .unwrap();
    assert!(!author.is_stopped(), "restored");
    // Retired on purpose: the review goes to its parent, as before.
    assert_eq!(
        docs::review_wake_target(&state, &doc("author02")).as_deref(),
        Some("parent01")
    );
    let _ = fs::remove_dir_all(dir);
}
