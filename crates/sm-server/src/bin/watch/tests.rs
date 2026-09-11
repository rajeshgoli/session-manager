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
    assert_eq!(age("2026-09-10T11:00:00Z", now), "0s");
}

#[test]
fn running_jobs_appear_once_are_green_and_tab_follows_five_lines() {
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
            .filter(|r| r.text.contains("job j running"))
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
    assert_eq!(view.jobs_for.as_deref(), Some("a"));
    assert_eq!(view.interest().log, "j");
    assert_eq!(view.interest().log_lines, 5);
    assert!(view.interest().details.is_empty());
    let rows = view.rows(&snap, &a, 0);
    assert_eq!(rows[0].style, "\x1b[32m");
    let snap = Snapshot {
        log_id: "j".into(),
        log: (1..=8).map(|n| format!("unique-line-{n}\n")).collect(),
        ..snap
    };
    let text = clean(&frame(&mut view, &snap, &a, &rows, 40, 100));
    assert!(!text.contains("unique-line-3"));
    assert!(text.contains("unique-line-4"));
    assert!(text.contains("unique-line-8"));
    assert!(text.contains("last 5 lines"));
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
    assert!(view.interest().details.is_empty());
    let rows = view.rows(&snap, &a, 0);
    assert!(!frame(&mut view, &snap, &a, &rows, 40, 100).contains("agent-only-output"));
}
#[test]
fn queue_reports_global_contention_not_false_fifo_dependencies() {
    let mut pending = job("p", "a", "pending");
    pending["holding_reason"] = json!("concurrency_cap");
    let running = job("r", "b", "running");
    let mut earlier = job("e", "c", "pending");
    earlier["queued_at"] = json!("2026-09-10T09:59:00Z");
    let mut later = job("l", "d", "pending");
    later["queued_at"] = json!("2026-09-10T10:01:00Z");
    let jobs = vec![pending, running, earlier, later];
    let text = queue_context(&jobs, "a").join("\n");
    assert!(!text.contains("job waiting"));
    assert!(text.contains("held: concurrency cap"));
    assert!(text.contains("1 running globally by b"));
    assert!(text.contains("1 earlier queued globally by c"));
    assert!(!text.contains("by d"));
    assert!(!text.contains("behind"));
    assert!(queue_context(&jobs, "missing").is_empty());
    let mut delegated = job("d", "a", "running");
    delegated["notify_session_id"] = json!("b");
    assert!(owns(&delegated, "a") && owns(&delegated, "b"));
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
    assert!(rows.iter().any(|r| r.text.contains("job j running")));
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
