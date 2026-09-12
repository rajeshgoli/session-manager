use super::*;
use std::io::Read;
use std::net::TcpListener;

fn args() -> WatchArgs {
    WatchArgs {
        repo: None,
        role: None,
        interval: 0.2,
        restore: false,
        top_level: false,
        sort: "retired".into(),
        node: None,
        all_nodes: false,
    }
}
fn job(id: &str, owner: &str, state: &str) -> Value {
    json!({"id":id,"requester_session_id":owner,"notify_session_id":owner,"state":state,
        "queued_at":"2026-09-10T10:00:00Z","started_at":"2026-09-10T10:07:00Z","label":"unit tests"})
}
fn session(id: &str, parent: &str, repo: &str) -> Value {
    json!({"id":id,"parent_session_id":parent,"working_dir":repo,"friendly_name":id,"provider":"claude","status":"running","activity_state":"idle"})
}
fn ids(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .filter_map(|r| match &r.target {
            Some(Target::Session(id)) => Some(id.clone()),
            _ => None,
        })
        .collect()
}
fn fake_worker() -> (Worker, mpsc::Receiver<Work>) {
    let (tx, rx) = mpsc::channel();
    let (_, replies) = mpsc::channel();
    (
        Worker {
            tx,
            state: Arc::new(Mutex::new(Snapshot::default())),
            replies,
        },
        rx,
    )
}

#[test]
fn ages_distinguish_wait_start_and_terminal_duration() {
    let now = stamp("2026-09-10T10:10:00Z").unwrap();
    assert_eq!(job_age(&job("j", "a", "pending"), now), "10m");
    assert_eq!(job_age(&job("j", "a", "running"), now), "3m");
    let mut done = job("j", "a", "completed");
    done["finished_at"] = json!("2026-09-10T10:08:00Z");
    assert_eq!(job_age(&done, now), "1m");
    done["finished_at"] = Value::Null;
    assert_eq!(job_age(&done, now), "?");
    assert_eq!(age("invalid", now), "?");
    assert_eq!(duration(7260), "2h 1m");
    assert_eq!(duration(90000), "1d 1h");
    assert_eq!(age("2026-09-10T11:00:00Z", now), "0s");
}

#[test]
fn jobs_expand_inline_then_open_full_height_live_output() {
    let a = args();
    let mut view = View::new(&a);
    let snap = Snapshot {
        sessions: vec![session("a", "", "/repo")],
        jobs: vec![job("j", "a", "running")],
        ..Default::default()
    };
    let rows = view.rows(&snap, &a, 0);
    assert!(!rows.iter().any(|r| r.text.contains("1 job running")));
    assert_eq!(
        rows.iter()
            .filter(|r| r.text.contains("unit tests · running"))
            .count(),
        1
    );
    let target = Target::SessionJob("a".into(), "j".into());
    assert!(rows
        .iter()
        .any(|r| r.target.as_ref() == Some(&target) && r.style == "\x1b[32m"));
    view.selected = Some(target);
    let (worker, _) = fake_worker();
    handle_key(Key::Tab, &mut view, &worker, &snap, &a).unwrap();
    assert!(view.jobs_for.is_none());
    assert_eq!(view.interest().log_lines, 6);
    assert!(view
        .rows(&snap, &a, 0)
        .iter()
        .any(|r| r.text.contains("LIVE OUTPUT")));
    handle_key(Key::Tab, &mut view, &worker, &snap, &a).unwrap();
    assert_eq!(view.jobs_for.as_deref(), Some("a"));
    assert_eq!(view.interest().log, "j");
    assert_eq!(view.interest().log_lines, 200);
    assert!(view.interest().details.is_empty());
    let rows = view.rows(&snap, &a, 0);
    assert_eq!(rows[0].style, "\x1b[32m");
    let snap = Snapshot {
        log_id: "j".into(),
        log: (1..=8).map(|n| format!("unique-line-{n}\n")).collect(),
        ..snap
    };
    let text = clean(&frame(&mut view, &snap, &a, &rows, 40, 100));
    assert!(text.contains("unique-line-3"));
    assert!(text.contains("unique-line-4"));
    assert!(text.contains("unique-line-8"));
    assert!(text.contains("following"));
}

#[test]
fn agent_expansion_and_job_tail_are_independent() {
    let a = args();
    let mut view = View::new(&a);
    let snap = Snapshot {
        sessions: vec![session("a", "", "/repo")],
        jobs: vec![job("j", "a", "running")],
        details: BTreeMap::from([(
            "a".into(),
            vec![
                "Agent output (last 10 lines):".into(),
                "agent-only-output".into(),
            ],
        )]),
        ..Default::default()
    };
    let rows = view.rows(&snap, &a, 0);
    assert!(rows
        .iter()
        .any(|r| r.target == Some(Target::SessionJob("a".into(), "j".into()))));
    assert!(!rows.iter().any(|r| r.text.contains("agent-only-output")));
    view.expanded.insert("a".into());
    let rows = view.rows(&snap, &a, 0);
    let agent_output = rows
        .iter()
        .position(|r| r.text.contains("agent-only-output"))
        .unwrap();
    let job_target = Target::SessionJob("a".into(), "j".into());
    assert!(
        agent_output
            < rows
                .iter()
                .position(|r| r.target.as_ref() == Some(&job_target))
                .unwrap()
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r.target.as_ref() == Some(&job_target))
            .count(),
        1
    );
    let (worker, _) = fake_worker();
    view.selected = Some(job_target);
    handle_key(Key::Tab, &mut view, &worker, &snap, &a).unwrap();
    assert!(view.interest().details.contains("a"));
    handle_key(Key::Tab, &mut view, &worker, &snap, &a).unwrap();
    assert!(view.interest().details.is_empty());
    let rows = view.rows(&snap, &a, 0);
    assert!(!frame(&mut view, &snap, &a, &rows, 40, 100).contains("agent-only-output"));
}
#[test]
fn agent_queue_only_shows_its_requesters_jobs_and_hold_reasons() {
    let mut pending = job("p", "a", "pending");
    pending["holding_reason"] = json!("concurrency_cap");
    let running = job("r", "b", "running");
    let mut earlier = job("e", "c", "pending");
    earlier["queued_at"] = json!("2026-09-10T09:59:00Z");
    let mut later = job("l", "d", "pending");
    later["queued_at"] = json!("2026-09-10T10:01:00Z");
    let jobs = vec![pending, running, earlier, later];
    let a = args();
    let mut view = View::new(&a);
    let snap = Snapshot {
        sessions: vec![session("a", "", "/repo")],
        jobs,
        ..Default::default()
    };
    let rows = view.rows(&snap, &a, 0);
    let job_rows: Vec<_> = rows
        .iter()
        .filter(|row| matches!(row.target, Some(Target::SessionJob(_, _))))
        .collect();
    assert_eq!(job_rows.len(), 1);
    assert!(job_rows[0].text.contains("pending"));
    assert!(job_rows[0].text.contains("slots full"));
    let mut delegated = job("d", "a", "running");
    delegated["notify_session_id"] = json!("b");
    assert!(owns(&delegated, "a") && !owns(&delegated, "b"));
    delegated["requester_session_id"] = Value::Null;
    assert!(owns(&delegated, "b"));
}
#[test]
fn repo_filter_keeps_cross_repo_tree_but_role_and_text_do_not() {
    let mut a = args();
    a.repo = Some("/project".into());
    let sessions = vec![
        session("root", "", "/elsewhere"),
        session("child", "root", "/project"),
        session("grandchild", "child", "/third"),
    ];
    assert_eq!(filtered(&sessions, &a, "").len(), 3);
    assert_eq!(filtered(&sessions, &a, "grandchild").len(), 0);
    a.role = Some("engineer".into());
    let mut sessions = sessions;
    sessions[1]["role"] = json!("engineer");
    assert_eq!(filtered(&sessions, &a, "").len(), 1);
}
#[test]
fn tree_groups_children_preserves_cycles_and_expands_jobs() {
    let a = args();
    let mut view = View::new(&a);
    let snap = Snapshot {
        sessions: vec![
            session("child", "root", "/other"),
            session("root", "", "/repo"),
            session("cycle", "cycle", "/cycle"),
        ],
        jobs: vec![job("j", "root", "running")],
        ..Default::default()
    };
    view.expanded.insert("root".into());
    let rows = view.rows(&snap, &a, 0);
    let names = ids(&rows);
    assert!(names.iter().position(|v| v == "root") < names.iter().position(|v| v == "child"));
    assert!(names.contains(&"cycle".into()));
    assert!(rows.iter().any(|r| r.text.contains("unit tests · running")));
    assert!(rows.iter().any(|r| r.text.contains("/other")));
}
#[test]
fn restore_collapses_expands_hides_and_sorts_without_losing_headers() {
    let mut a = args();
    a.restore = true;
    a.top_level = true;
    let mut snap = Snapshot {
        sessions: vec![
            session("child", "root", "/repo"),
            session("root", "", "/repo"),
            session("other", "", "/repo"),
        ],
        ..Default::default()
    };
    for v in &mut snap.sessions {
        v["status"] = json!("stopped");
    }
    snap.sessions[1]["retired_at"] = json!("2026-09-10T10:00:00Z");
    snap.sessions[2]["retired_at"] = json!("2026-09-10T11:00:00Z");
    let mut view = View::new(&a);
    assert_eq!(ids(&view.rows(&snap, &a, 0)), vec!["other", "root"]);
    view.expanded.insert("root".into());
    assert_eq!(
        ids(&view.rows(&snap, &a, 0)),
        vec!["other", "root", "child"]
    );
    view.hidden.insert("/repo".into());
    let rows = view.rows(&snap, &a, 0);
    assert!(ids(&rows).is_empty());
    assert!(rows
        .iter()
        .any(|r| r.target == Some(Target::Repo("/repo".into()))));
}
#[test]
fn job_browser_keeps_completed_jobs_and_selection_across_refresh() {
    let a = args();
    let mut view = View::new(&a);
    view.jobs_for = Some("a".into());
    let mut snap = Snapshot {
        jobs: vec![job("j", "a", "running"), job("other", "b", "pending")],
        ..Default::default()
    };
    assert_eq!(view.rows(&snap, &a, 0).len(), 1);
    view.job_selected = Some(Target::Job("j".into()));
    view.tail = true;
    assert_eq!(view.interest().log, "j");
    snap.jobs.clear();
    let rows = view.rows(&snap, &a, 0);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].text.contains("left active queue"));
    view.global = true;
    assert_eq!(view.rows(&snap, &a, 0).len(), 2);
}

#[test]
fn automatic_job_reselection_refreshes_tail_interest() {
    let a = args();
    let (worker, rx) = fake_worker();
    let mut view = View::new(&a);
    view.jobs_for = Some("a".into());
    view.global = true;
    view.tail = true;
    view.job_selected = Some(Target::Job("other".into()));
    let snap = Snapshot {
        jobs: vec![job("local", "a", "running"), job("other", "b", "running")],
        ..Default::default()
    };
    view.rows(&snap, &a, 0);
    let mut interest = view.interest();
    assert_eq!(interest.log, "other");

    view.global = false;
    let rows = view.rows(&snap, &a, 0);
    let selection = view.active_selection();
    if !rows
        .iter()
        .any(|row| row.target.is_some() && &row.target == selection)
    {
        navigation(&rows, selection, 0);
    }
    refresh_interest(&worker, &view, &mut interest).unwrap();

    assert_eq!(view.job_selected, Some(Target::Job("local".into())));
    assert!(matches!(
        rx.try_recv().unwrap(),
        Work::Refresh(Interest { log, .. }) if log == "local"
    ));
}

#[test]
fn create_paths_expand_home_before_canonicalization() {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    assert_eq!(expand_home("~"), home);
    assert_eq!(expand_home("~/project"), home.join("project"));
    assert_eq!(
        expand_home("/tmp/project"),
        std::path::PathBuf::from("/tmp/project")
    );
}

#[test]
fn retire_requires_second_press_same_session_and_unexpired_confirmation() {
    let a = args();
    let (worker, rx) = fake_worker();
    let mut view = View::new(&a);
    view.selected = Some(Target::Session("a".into()));
    handle_key(Key::Char('K'), &mut view, &worker, &Snapshot::default(), &a).unwrap();
    assert!(rx.try_recv().is_err());
    view.selected = Some(Target::Session("b".into()));
    handle_key(Key::Char('K'), &mut view, &worker, &Snapshot::default(), &a).unwrap();
    assert!(rx.try_recv().is_err());
    view.retire = Some(("b".into(), Instant::now() - Duration::from_secs(6)));
    handle_key(Key::Char('K'), &mut view, &worker, &Snapshot::default(), &a).unwrap();
    assert!(rx.try_recv().is_err());
    handle_key(Key::Char('K'), &mut view, &worker, &Snapshot::default(), &a).unwrap();
    assert!(
        matches!(rx.try_recv().unwrap(),Work::Action{method:"POST",path,..} if path=="/sessions/b/retire")
    );
}

#[test]
fn unsupported_fork_key_does_not_enqueue_an_action() {
    let a = args();
    let (worker, rx) = fake_worker();
    let mut view = View::new(&a);
    view.selected = Some(Target::Session("a".into()));

    handle_key(Key::Char('F'), &mut view, &worker, &Snapshot::default(), &a).unwrap();

    assert!(rx.try_recv().is_err());
}

#[test]
fn restore_and_attach_use_correct_endpoints_and_preserve_remote_argv() {
    let mut a = args();
    a.restore = true;
    a.node = Some("studio".into());
    let (worker, rx) = fake_worker();
    let mut view = View::new(&a);
    view.selected = Some(Target::Session("a".into()));
    handle_key(Key::Enter, &mut view, &worker, &Snapshot::default(), &a).unwrap();
    assert!(
        matches!(rx.try_recv().unwrap(),Work::Action{method:"POST",path,attach:true,..} if path=="/nodes/studio/restore-candidates/a/restore")
    );
    assert_eq!(
        attach_command(&json!({"tmux_session":"target","tmux_socket_name":"socket"})).unwrap(),
        vec!["tmux", "-L", "socket", "attach-session", "-t", "target"]
    );
    assert_eq!(
        attach_command(
            &json!({"attach_command":["ssh","-tt","remote","tmux attach-session -t a"]})
        )
        .unwrap(),
        vec!["ssh", "-tt", "remote", "tmux attach-session -t a"]
    );
    assert!(
        attach_command(&json!({"attach_supported":false,"message":"headless"}))
            .unwrap_err()
            .to_string()
            .contains("headless")
    );
}
#[test]
fn reparent_guards_block_unsafe_rollback_and_allow_human_decisions() {
    let a = args();
    let (worker, rx) = fake_worker();
    let mut view = View::new(&a);
    view.selected = Some(Target::Request("req".into()));
    let mut snap = Snapshot {
        requests: vec![json!({"id":"req","status":"failed","apply_stage":"committed"})],
        ..Default::default()
    };
    assert!(handle_key(Key::Char('B'), &mut view, &worker, &snap, &a).is_err());
    assert!(rx.try_recv().is_err());
    snap.requests[0] = json!({"id":"req","status":"pending","required_human_approval":true});
    handle_key(Key::Char('A'), &mut view, &worker, &snap, &a).unwrap();
    assert!(
        matches!(rx.try_recv().unwrap(),Work::Action{path,..} if path=="/reparent-requests/req/human-approve")
    );
}
#[test]
fn logs_cannot_inject_terminal_commands() {
    assert_eq!(
        clean("\x1b[31mred\x1b[0m\n\x1b]52;c;clipboard\x07safe\x1b]0;title\x1b\\\0"),
        "red\nsafe"
    );
    assert_eq!(clipped("éhello", 2), "éh");
}

#[test]
fn narrow_frames_wrap_queue_context_and_allow_scrolling_details() {
    let a = args();
    let mut view = View::new(&a);
    let snap = Snapshot {
        sessions: vec![session("agent", "", "/repo")],
        ..Default::default()
    };
    let mut rows = view.rows(&snap, &a, 0);
    rows.extend((0..40).map(|n| {
        Row::plain(format!(
            "detail line {n} with long queue owner names and hold reasons"
        ))
    }));
    let rows = wrap_rows(rows, 37);
    view.selected = Some(Target::Session("agent".into()));
    for (h, w) in [(4, 10), (10, 40), (24, 80), (40, 160)] {
        let text = frame(&mut view, &snap, &a, &rows, h, w);
        assert!(clean(&text).lines().all(|line| line.chars().count() < w));
    }
    view.free_scroll = true;
    view.offset = 25;
    frame(&mut view, &snap, &a, &rows, 10, 40);
    assert_eq!(view.offset, 25);
}

#[test]
fn restore_recency_uses_stopped_at_and_includes_cross_repo_descendants() {
    let mut a = args();
    a.restore = true;
    let mut view = View::new(&a);
    let mut root = session("oldroot", "", "/a");
    root["status"] = json!("stopped");
    root["stopped_at"] = json!("2026-09-10T10:00:00Z");
    let mut child = session("newchild", "oldroot", "/child");
    child["status"] = json!("stopped");
    child["stopped_at"] = json!("2026-09-10T12:00:00Z");
    let mut other = session("other", "", "/b");
    other["status"] = json!("stopped");
    other["stopped_at"] = json!("2026-09-10T11:00:00Z");
    let snap = Snapshot {
        sessions: vec![root, child, other],
        ..Default::default()
    };
    assert_eq!(
        ids(&view.rows(&snap, &a, 0)),
        vec!["oldroot", "newchild", "other"]
    );
    let config: serde_yaml::Value =
        serde_yaml::from_str("default_node: primary\nclient:\n  local_node: studio\n").unwrap();
    assert_eq!(configured_node(&config).as_deref(), Some("studio"));
}

// A real HTTP peer verifies endpoint shape, bounded tails, terminal job fetches,
// and failure handling without accessing production state.
fn server(responses: Vec<(&'static str, u16, Value)>) -> (Client, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for (path, status, value) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut raw = Vec::new();
            let mut b = [0];
            while !raw.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut b).unwrap();
                raw.push(b[0]);
            }
            assert!(String::from_utf8_lossy(&raw).starts_with(&format!("GET {path} HTTP/1.1")));
            let body = value.to_string();
            write!(stream,"HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    (Client::new(&format!("http://{addr}")), handle)
}
#[test]
fn refresh_retains_snapshot_on_failure_and_reads_final_remote_tail() {
    let done = job("j", "a", "completed");
    let (client, peer) = server(vec![
        ("/sessions", 200, json!({"sessions":[]})),
        ("/queue-jobs", 200, json!({"jobs":[]})),
        ("/reparent-requests", 200, json!({"requests":[]})),
        ("/session-obligations", 200, json!({"sessions":[]})),
        ("/queue-jobs/j", 200, done.clone()),
        (
            "/queue-jobs/j/log?lines=200",
            200,
            json!({"text":"\x1b[32mdone\x1b[0m"}),
        ),
        ("/queue-jobs", 503, json!({"detail":"offline"})),
    ]);
    let state = Arc::new(Mutex::new(Snapshot::default()));
    refresh(
        &client,
        &state,
        &Interest {
            log: "j".into(),
            ..Default::default()
        },
        false,
        "primary",
        false,
    );
    assert_eq!(state.lock().unwrap().log, "done");
    assert_eq!(state.lock().unwrap().jobs, vec![done]);
    update_list(&client, &state, "/queue-jobs", "jobs", "queue");
    assert_eq!(state.lock().unwrap().jobs.len(), 1);
    assert!(state.lock().unwrap().errors["queue"].contains("offline"));
    peer.join().unwrap();
}

#[test]
fn pending_job_skips_missing_log_then_follows_when_it_starts() {
    let (client, peer) = server(vec![
        ("/sessions", 200, json!({"sessions":[]})),
        ("/queue-jobs", 200, json!({"jobs":[job("j","a","pending")]})),
        ("/reparent-requests", 200, json!({"requests":[]})),
        ("/session-obligations", 200, json!({"sessions":[]})),
        ("/sessions", 200, json!({"sessions":[]})),
        ("/queue-jobs", 200, json!({"jobs":[job("j","a","running")]})),
        ("/reparent-requests", 200, json!({"requests":[]})),
        ("/session-obligations", 200, json!({"sessions":[]})),
        (
            "/queue-jobs/j/log?lines=5",
            200,
            json!({"text":"started output"}),
        ),
    ]);
    let state = Arc::new(Mutex::new(Snapshot::default()));
    let interest = Interest {
        log: "j".into(),
        log_lines: 5,
        ..Default::default()
    };
    refresh(&client, &state, &interest, false, "primary", false);
    assert!(state.lock().unwrap().log.contains("Waiting to start"));
    assert!(!state.lock().unwrap().log.contains("404"));
    refresh(&client, &state, &interest, false, "primary", false);
    assert_eq!(state.lock().unwrap().log, "started output");
    peer.join().unwrap();
}

#[test]
fn waiting_is_cyan_only_for_idle_agents_and_review_history_is_visible() {
    let a = args();
    let mut view = View::new(&a);
    let mut snap = Snapshot {
        sessions: vec![session("a", "", "/repo")],
        obligations: vec![
            json!({"session_id":"a", "waiting_on":[{"kind":"review", "repo":"repo", "pr_number":42, "label":"Review · repo #42", "since":"2026-09-10T10:00:00Z"}], "review_history":[{"repo":"repo","pr_number":42,"landed_count":3,"landed_requested_by_agent":1,"requested_by_agent":2}]}),
        ],
        ..Default::default()
    };
    let rows = view.rows(&snap, &a, stamp("2026-09-10T10:05:00Z").unwrap());
    let agent = rows
        .iter()
        .find(|r| r.target == Some(Target::Session("a".into())))
        .unwrap();
    assert!(agent.text.contains("waiting"));
    assert!(agent.text.contains("◷  a"));
    assert_eq!(agent.style, "\x1b[36m");
    assert!(rows.iter().any(|r| r.text.contains("waiting 5m")));
    assert!(rows.iter().any(|r| r.text.contains("3 reviews · 1 yours")));
    snap.sessions[0]["activity_state"] = json!("working");
    let rows = view.rows(&snap, &a, 0);
    assert!(rows
        .iter()
        .any(|r| r.target == Some(Target::Session("a".into()))
            && r.text.contains("working")
            && r.style == "\x1b[32m"));
}

#[test]
fn full_screen_tail_uses_height_and_scroll_follow_survives_resize() {
    let a = args();
    let mut view = View::new(&a);
    view.jobs_for = Some("a".into());
    view.job_selected = Some(Target::Job("j".into()));
    view.tail = true;
    let snap = Snapshot {
        jobs: vec![job("j", "a", "running")],
        log_id: "j".into(),
        log: (1..=100).map(|n| format!("output-{n:03}\n")).collect(),
        ..Default::default()
    };
    let rows = view.rows(&snap, &a, 0);
    let large = clean(&frame(&mut view, &snap, &a, &rows, 45, 100));
    assert!(large.contains("output-070") && large.contains("output-100"));
    assert!(!large.contains("held:") && !large.contains("null"));
    view.log_scroll = 20;
    let scrolled = clean(&frame(&mut view, &snap, &a, &rows, 20, 100));
    assert!(scrolled.contains("output-080") && !scrolled.contains("output-100"));
    assert!(scrolled.contains("End to follow"));
    view.log_scroll = 0;
    let resized = clean(&frame(&mut view, &snap, &a, &rows, 12, 60));
    assert!(resized.contains("output-100"));
}

#[test]
fn obligations_do_not_repeat_visible_jobs_but_preserve_other_results() {
    let jobs = vec![job("j", "a", "running")];
    let single = json!({"waiting_on":[{"kind":"queue_job", "id":"j", "label":"unit tests", "since":"2026-09-10T10:00:00Z"}]});
    assert!(obligation_context(&single, &jobs, "a", false, 0).is_empty());
    assert!(obligation_context(&single, &jobs, "a", true, 0).is_empty());
    // A delegated result or a missing job snapshot still needs its own line.
    assert_eq!(obligation_context(&single, &jobs, "b", false, 0).len(), 1);
    assert_eq!(obligation_context(&single, &[], "a", false, 0).len(), 1);
    let multiple = json!({"waiting_on":[
        {"kind":"queue_job", "id":"j", "label":"unit tests"},
        {"kind":"queue_job", "id":"j2", "label":"integration tests"},
        {"kind":"review", "id":"r", "repo":"repo", "pr_number":42, "label":"Review · repo #42", "since":"2026-09-10T10:00:00Z"}
    ]});
    let mut jobs = jobs;
    jobs.push(job("j2", "a", "pending"));
    assert_eq!(
        obligation_context(&multiple, &jobs, "a", false, 0),
        vec!["Waiting for 2 jobs and 1 review"]
    );
    let expanded = obligation_context(&multiple, &jobs, "a", true, 0);
    assert_eq!(expanded, vec!["Waiting for 2 jobs and 1 review"]);
}

#[test]
fn pending_job_rows_explain_each_hold_and_clear_it_when_running() {
    let a = args();
    let mut view = View::new(&a);
    let mut j = job("j", "a", "pending");
    for (reason, expected) in [
        ("perf_cooldown", "perf cooldown"),
        ("perf_running", "perf in progress"),
        ("awaiting_tests", "tests ahead"),
        ("", "reason unknown"),
        ("future_hold", "future hold"),
    ] {
        j["holding_reason"] = json!(reason);
        let snap = Snapshot {
            sessions: vec![session("a", "", "/repo")],
            jobs: vec![j.clone()],
            ..Default::default()
        };
        let rows = view.rows(&snap, &a, 0);
        assert!(rows.iter().any(
            |r| matches!(r.target, Some(Target::SessionJob(_, _))) && r.text.contains(expected)
        ));
        view.jobs_for = Some("a".into());
        let rows = view.rows(&snap, &a, 0);
        assert!(rows
            .iter()
            .any(|r| matches!(r.target, Some(Target::Job(_))) && r.text.contains(expected)));
        view.jobs_for = None;
    }
    j["state"] = json!("running");
    assert!(pending_reason(&j).is_empty());
}

#[test]
fn running_job_pid_appears_in_tree_browser_and_details_only_while_running() {
    let a = args();
    let mut view = View::new(&a);
    let mut j = job("j", "a", "running");
    j["pid"] = json!(4321);
    let snap = Snapshot {
        sessions: vec![session("a", "", "/repo")],
        jobs: vec![j.clone()],
        ..Default::default()
    };
    assert!(
        view.rows(&snap, &a, 0)
            .iter()
            .any(|r| matches!(r.target, Some(Target::SessionJob(_, _)))
                && r.text.contains("PID 4321"))
    );
    view.jobs_for = Some("a".into());
    assert!(view
        .rows(&snap, &a, 0)
        .iter()
        .any(|r| matches!(r.target, Some(Target::Job(_))) && r.text.contains("PID 4321")));
    assert!(job_metadata(&j, 0).join("\n").contains("PID  4321"));
    j["state"] = json!("succeeded");
    assert!(running_job_pid(&j).is_none());
    assert!(!job_metadata(&j, 0).join("\n").contains("PID"));
    j["state"] = json!("running");
    j["pid"] = Value::Null;
    assert!(!job_row_context(&j).contains("PID"));
    j["pid"] = json!(0);
    assert!(running_job_pid(&j).is_none());
}

#[test]
fn review_rows_show_pr_history_collapsed_and_tab_opens_the_watch_details() {
    let a = args();
    let mut view = View::new(&a);
    let now = stamp("2026-09-10T10:05:00Z").unwrap();
    let mut j = job("j", "a", "pending");
    j["type"] = json!("perf");
    j["holding_reason"] = json!("awaiting_tests");
    let snap = Snapshot {
        sessions: vec![session("a", "", "/repo")],
        jobs: vec![j],
        obligations: vec![json!({
            "session_id":"a", "waiting_on":[{"kind":"review", "id":"r", "repo":"owner/repo", "pr_number":42, "state":"requested", "since":"2026-09-10T10:00:00Z", "last_polled_at":"2026-09-10T10:04:00Z"}],
            "review_history":[{"repo":"owner/repo","pr_number":42,"landed_count":3,"landed_requested_by_agent":1,"requested_by_agent":2}, {"repo":"owner/repo","pr_number":41,"landed_count":2,"landed_requested_by_agent":0,"requested_by_agent":0}]
        })],
        ..Default::default()
    };
    let rows = view.rows(&snap, &a, now);
    assert!(rows
        .iter()
        .any(|r| r.text.contains("[perf] unit tests") && r.text.contains("tests ahead")));
    let reviews: Vec<_> = rows
        .iter()
        .filter(|r| matches!(r.target, Some(Target::Review(_, _, _))))
        .collect();
    assert_eq!(reviews.len(), 2);
    assert!(reviews[0]
        .text
        .contains("owner/repo#42 · waiting 5m · 3 reviews · 1 yours"));
    assert!(reviews[1].text.contains("owner/repo#41 · history"));
    assert_eq!(rows.iter().filter(|r| r.text.contains("#42")).count(), 1);
    view.selected = reviews[0].target.clone();
    let (worker, _) = fake_worker();
    handle_key(Key::Tab, &mut view, &worker, &snap, &a).unwrap();
    assert!(view.interest().details.is_empty());
    let rows = view.rows(&snap, &a, now);
    assert!(rows
        .iter()
        .any(|r| r.text.contains("2 requests by this agent")));
    assert!(rows
        .iter()
        .any(|r| r.text.contains("Last checked · 1m ago")));
    assert!(rows
        .iter()
        .any(|r| r.text.contains("https://github.com/owner/repo/pull/42")));
    handle_key(Key::Tab, &mut view, &worker, &snap, &a).unwrap();
    assert!(view.expanded_review.is_none());
}

#[test]
fn recent_output_uses_rendered_screen_instead_of_raw_terminal_recording() {
    let (client, peer) = server(vec![
        (
            "/sessions",
            200,
            json!({"sessions":[session("a", "", "/repo")]}),
        ),
        ("/queue-jobs", 200, json!({"jobs":[]})),
        ("/reparent-requests", 200, json!({"requests":[]})),
        ("/session-obligations", 200, json!({"sessions":[]})),
        (
            "/sessions/a/tool-calls?limit=10",
            200,
            json!({"tool_calls":[]}),
        ),
        (
            "/sessions/a/output?lines=10&rendered=true",
            200,
            json!({"output":"PR #1382 is open.\nIndependent review is pending."}),
        ),
    ]);
    let state = Arc::new(Mutex::new(Snapshot::default()));
    refresh(
        &client,
        &state,
        &Interest {
            details: BTreeSet::from(["a".into()]),
            ..Default::default()
        },
        false,
        "primary",
        false,
    );
    let text = state.lock().unwrap().details["a"].join("\n");
    assert!(text.contains("PR #1382 is open."));
    assert!(!text.contains("2026l"));
    peer.join().unwrap();
}

#[test]
fn escape_keys_support_normal_and_application_cursor_modes() {
    for (sequence, expected) in [
        (b"[A".as_slice(), Key::Up),
        (b"OA".as_slice(), Key::Up),
        (b"[B".as_slice(), Key::Down),
        (b"OB".as_slice(), Key::Down),
        (b"[5~".as_slice(), Key::PageUp),
        (b"[6~".as_slice(), Key::PageDown),
        (b"[F".as_slice(), Key::End),
        (b"OF".as_slice(), Key::End),
        (b"[4~".as_slice(), Key::End),
        (b"".as_slice(), Key::Esc),
        (b"[C".as_slice(), Key::None),
        (b"OD".as_slice(), Key::None),
        (b"[1;2B".as_slice(), Key::None),
        (b"[".as_slice(), Key::None),
        (b"O".as_slice(), Key::None),
    ] {
        let mut bytes = sequence.iter().copied();
        assert_eq!(escape_key(|| Ok(bytes.next())).unwrap(), expected);
        assert_eq!(bytes.next(), None, "must consume the entire sequence");
    }
}

#[test]
fn escape_key_stops_before_the_next_key_in_a_burst() {
    let mut bytes = b"OBj".iter().copied();
    assert_eq!(escape_key(|| Ok(bytes.next())).unwrap(), Key::Down);
    assert_eq!(bytes.next(), Some(b'j'));
}

#[test]
fn active_row_does_not_display_stale_idle_lifecycle_status() {
    let a = args();
    let mut view = View::new(&a);
    let mut agent = session("agent", "", "/repo");
    agent["status"] = json!("idle");
    agent["activity_state"] = json!("working");
    let snap = Snapshot {
        sessions: vec![agent],
        ..Snapshot::default()
    };
    let rows = view.rows(&snap, &a, 0);
    let row = rows
        .iter()
        .find(|row| row.target == Some(Target::Session("agent".into())))
        .unwrap();
    assert!(row.text.contains("working"));
    assert!(!row.text.contains("idle"));
    assert_eq!(row.style, "\x1b[32m");
}
