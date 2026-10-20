use super::*;

const GIB: i64 = 1024 * 1024 * 1024;

/// A process leading its own group, as each harness starts its commands.
fn row(pid: i32, ppid: i32, gib: i64, command: &str) -> ProcessRow {
    grouped(pid, ppid, pid, gib, command)
}

fn grouped(pid: i32, ppid: i32, pgid: i32, gib: i64, command: &str) -> ProcessRow {
    ProcessRow {
        pid,
        ppid,
        pgid,
        rss_kib: gib * 1024 * 1024,
        command: command.to_owned(),
    }
}

fn pane(session_id: &str, pane_pid: i32) -> AgentPane {
    AgentPane {
        session_id: session_id.to_owned(),
        session_name: format!("{session_id}-name"),
        pane_pid,
    }
}

fn owner(session_id: &str) -> GroupOwner {
    GroupOwner {
        session_id: session_id.to_owned(),
        session_name: format!("{session_id}-name"),
    }
}

/// (session, root pid, sorted pids, detached) of every command root.
fn roots(
    rows: &[ProcessRow],
    panes: &[AgentPane],
    groups: &HashMap<i32, GroupOwner>,
) -> Vec<(String, i32, Vec<i32>, bool)> {
    let mut found: Vec<_> = command_roots(rows, panes, groups)
        .0
        .into_iter()
        .map(|root| {
            let mut pids = root.pids;
            pids.sort_unstable();
            (root.session_id, root.pid, pids, root.detached)
        })
        .collect();
    found.sort();
    found
}

#[test]
fn a_claude_command_is_the_shell_under_the_harness_with_everything_below_it() {
    // claude -> zsh -c …eval 'cargo test' -> cargo -> rustc, plus caffeinate.
    let rows = [
        row(100, 1, 0, "/opt/homebrew/bin/claude"),
        row(101, 100, 0, "/bin/zsh"),
        grouped(102, 101, 101, 0, "/Users/x/.cargo/bin/cargo"),
        grouped(103, 102, 101, 2, "/Users/x/.rustup/rustc"),
        row(104, 100, 0, "/usr/bin/caffeinate"),
    ];
    assert_eq!(
        roots(&rows, &[pane("s1", 100)], &HashMap::new()),
        vec![
            ("s1".to_owned(), 101, vec![101, 102, 103], false),
            ("s1".to_owned(), 104, vec![104], false),
        ]
    );
}

#[test]
fn a_codex_command_runs_bare_under_codex_fork_beside_its_helpers() {
    let rows = [
        row(200, 1, 0, "/Users/x/codex-fork/bin/codex-fork"),
        row(201, 200, 0, "/bin/sleep"),
        row(202, 200, 0, "/Applications/ChatGPT.app/cua_node/bin/node"),
        grouped(
            203,
            202,
            202,
            0,
            "/Applications/ChatGPT.app/cua_node/bin/node_repl",
        ),
    ];
    assert_eq!(
        roots(&rows, &[pane("s1", 200)], &HashMap::new()),
        vec![
            ("s1".to_owned(), 201, vec![201], false),
            ("s1".to_owned(), 202, vec![202, 203], false),
        ]
    );
}

#[test]
fn a_local_agent_runs_commands_in_its_serve_window_under_opencode_serve() {
    // agent window: launch-attach.sh -> opencode attach; serve window:
    // launch-serve.sh -> owner -> {judge, supervisor -> opencode serve -> pytest}.
    let rows = [
        row(300, 1, 0, "/bin/bash"),
        grouped(301, 300, 300, 0, "/opt/homebrew/bin/opencode"),
        row(310, 1, 0, "/bin/bash"),
        grouped(311, 310, 310, 0, "/x/local-wall-owners/a/owner-08"),
        row(312, 311, 0, "/opt/homebrew/Python"),
        row(313, 311, 0, "/x/executables/supervisor"),
        grouped(314, 313, 313, 0, "/x/executables/opencode"),
        row(315, 314, 3, "/x/executables/pytest"),
    ];
    let panes = [pane("s1", 300), pane("s1", 310)];
    assert_eq!(
        roots(&rows, &panes, &HashMap::new()),
        vec![("s1".to_owned(), 315, vec![315], false)]
    );
    let (_, harnesses) = command_roots(&rows, &panes, &HashMap::new());
    let mut harness_pids: Vec<i32> = harnesses.iter().map(|(pid, _)| *pid).collect();
    harness_pids.sort_unstable();
    assert_eq!(harness_pids, vec![301, 314]);
}

#[test]
fn the_harness_under_a_node_wrapper_is_found_wherever_it_sits() {
    let rows = [
        row(100, 1, 0, "/usr/local/bin/node"),
        row(101, 100, 0, "/Users/x/codex-fork/bin/codex-fork"),
        row(102, 101, 9, "/Users/x/.cargo/bin/rustc"),
        row(103, 102, 9, "/usr/bin/cc"),
    ];
    assert_eq!(
        roots(&rows, &[pane("s1", 100)], &HashMap::new()),
        vec![("s1".to_owned(), 102, vec![102, 103], false)]
    );
}

#[test]
fn a_pane_without_any_harness_treats_the_pane_process_as_one() {
    let rows = [row(100, 1, 0, "/bin/zsh"), row(101, 100, 2, "/bin/a")];
    assert_eq!(
        roots(&rows, &[pane("s1", 100)], &HashMap::new()),
        vec![("s1".to_owned(), 101, vec![101], false)]
    );
}

#[test]
fn a_detached_command_is_traced_through_its_recorded_group() {
    // `( nohup sleep 306 & )` left sleep with parent 1 and the dead shell's group.
    let rows = [
        row(100, 1, 0, "/opt/homebrew/bin/claude"),
        grouped(34026, 1, 34017, 0, "/bin/sleep"),
        grouped(34030, 34026, 34017, 0, "/bin/child"),
        grouped(500, 1, 499, 0, "/bin/stranger"),
    ];
    let groups = HashMap::from([(34017, owner("s1"))]);
    assert_eq!(
        roots(&rows, &[pane("s1", 100)], &groups),
        vec![("s1".to_owned(), 34026, vec![34026, 34030], true)]
    );
}

#[test]
fn never_touches_processes_outside_agent_trees() {
    let rows = [
        row(100, 1, 0, "/opt/homebrew/bin/claude"),
        row(
            200,
            1,
            50,
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ),
        row(300, 1, 50, "/usr/local/bin/python3.12"),
    ];
    assert!(roots(&rows, &[pane("s1", 100)], &HashMap::new()).is_empty());
}

#[test]
fn each_session_owns_its_own_tree() {
    let rows = [
        row(100, 1, 0, "/opt/homebrew/bin/claude"),
        row(101, 100, 2, "/bin/a"),
        row(200, 1, 0, "/opt/homebrew/bin/claude"),
        row(201, 200, 2, "/bin/b"),
    ];
    assert_eq!(
        roots(&rows, &[pane("s1", 100), pane("s2", 200)], &HashMap::new()),
        vec![
            ("s1".to_owned(), 101, vec![101], false),
            ("s2".to_owned(), 201, vec![201], false),
        ]
    );
}

#[test]
fn a_group_tree_holds_members_and_descendants_that_left_the_group() {
    let rows = [
        row(101, 100, 0, "/bin/zsh"),
        grouped(102, 101, 101, 0, "/bin/cargo"),
        grouped(103, 102, 900, 0, "/bin/setsid-child"),
        row(104, 100, 0, "/bin/other"),
    ];
    let mut pids = group_tree(&rows, 101);
    pids.sort_unstable();
    assert_eq!(pids, vec![101, 102, 103]);
}

fn facts(started_at: Option<OffsetDateTime>, args: &str) -> ProcessFacts {
    ProcessFacts {
        identity: None,
        started_at,
        args: Some(args.to_owned()),
        command: None,
    }
}

#[test]
fn harness_helpers_are_not_background_jobs() {
    let harness = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
    let later = harness + time::Duration::minutes(5);
    let row = |command: &str| grouped(1, 0, 1, 0, command);
    assert!(is_helper(
        &row("/usr/bin/caffeinate"),
        &facts(Some(later), "caffeinate -i"),
        Some(harness),
        &[]
    ));
    assert!(is_helper(
        &row("/bin/node"),
        &facts(Some(later), "node /x/cua-repl/bin/cua-repl.mjs"),
        Some(harness),
        &[]
    ));
    // An MCP server starts with its harness.
    assert!(is_helper(
        &row("/bin/node"),
        &facts(Some(harness + time::Duration::seconds(3)), "node mcp.js"),
        Some(harness),
        &[]
    ));
    assert!(is_helper(
        &row("/bin/mcp-x"),
        &facts(Some(later), "mcp-x"),
        Some(harness),
        &["mcp-x".to_owned()]
    ));
    assert!(!is_helper(
        &row("/bin/zsh"),
        &facts(Some(later), "zsh -c npm run dev"),
        Some(harness),
        &[]
    ));
    assert!(!is_helper(
        &row("/bin/sleep"),
        &facts(None, "sleep 180"),
        None,
        &[]
    ));
}

#[test]
fn parses_padded_ps_rows_with_spaces_in_the_command() {
    let rows = parse_process_listing(
        "  100     1   100  2048 /opt/homebrew/bin/claude\n 4974  101  101 141557760 /Applications/Google Chrome.app/x\ngarbage\n",
    );
    let mut claude = grouped(100, 1, 100, 0, "/opt/homebrew/bin/claude");
    claude.rss_kib = 2048;
    let mut chrome = grouped(4974, 101, 101, 0, "/Applications/Google Chrome.app/x");
    chrome.rss_kib = 141_557_760;
    assert_eq!(rows, vec![claude, chrome]);
}

#[test]
fn parses_every_pane_of_a_tmux_session() {
    let panes = parse_pane_listing(
        "sm-rust-opencode-abc 61410\nsm-rust-opencode-abc 58441\n__sm_server_anchor 1458\nbad\n",
    );
    assert_eq!(panes.get("sm-rust-opencode-abc"), Some(&vec![61410, 58441]));
    assert_eq!(panes.len(), 2);
}

fn kill(session_id: &str, killed_at: &str) -> AgentProcessKill {
    AgentProcessKill {
        killed_at: killed_at.to_owned(),
        session_id: session_id.to_owned(),
        session_name: "far-2010".to_owned(),
        pid: 4974,
        command: "python3.12 train.py --heads".to_owned(),
        memory_bytes: 135 * GIB,
        limit_bytes: GIB,
        descendants: 2,
    }
}

#[test]
fn a_no_headroom_notice_says_why_and_how_to_resubmit() {
    let text = kill_notice_text(
        &kill("s1", "2026-10-08T21:37:05Z"),
        "python3.12 train.py --heads",
        StopCause::NoHeadroom { headroom: 12 * GIB },
    );
    assert!(
        text.contains("At 21:37:05 UTC sm stopped `python3.12 train.py --heads`"),
        "{text}"
    );
    assert!(
        text.contains("PID 4974 and its 2 child processes"),
        "{text}"
    );
    assert!(text.contains("only 12.0 GiB of memory to spare"), "{text}");
    assert!(text.contains("It was using 135.0 GiB"), "{text}");
    assert!(
        text.contains("sm queue run --type <type> --label python3-12-train-py --memory 203G -- python3.12 train.py --heads"),
        "{text}"
    );
    assert!(text.contains("Pick the type"), "{text}");
}

#[test]
fn an_untrackable_notice_names_the_shared_group() {
    let text = kill_notice_text(
        &kill("s1", "2026-10-08T21:37:05Z"),
        "cargo test --workspace",
        StopCause::Untrackable,
    );
    assert!(
        text.contains("shares a process group with your agent program"),
        "{text}"
    );
    assert!(text.contains("--type tests --label cargo-test"), "{text}");
    assert!(!text.contains("Pick the type"), "{text}");
}

#[test]
fn records_kills_and_lists_recent_ones_per_session() {
    let conn = Connection::open_in_memory().unwrap();
    insert_kill(&conn, &kill("s1", "2026-10-07T00:00:00Z")).unwrap();
    insert_kill(&conn, &kill("s1", "2026-10-08T21:37:05Z")).unwrap();
    insert_kill(&conn, &kill("s2", "2026-10-08T21:38:00Z")).unwrap();
    let since = "2026-10-08T00:00:00Z";
    let all = list_kills_since(&conn, None, since).unwrap();
    assert_eq!(
        all.iter()
            .map(|k| k.session_id.as_str())
            .collect::<Vec<_>>(),
        ["s2", "s1"]
    );
    let mine = list_kills_since(&conn, Some("s1"), since).unwrap();
    assert_eq!(mine, vec![kill("s1", "2026-10-08T21:37:05Z")]);
}

#[test]
fn reads_the_session_id_from_a_linux_environment_block() {
    let environ = b"PATH=/bin\0CLAUDE_SESSION_MANAGER_ID=cbf8008e\0HOME=/x\0";
    assert_eq!(
        environ_value(environ, "CLAUDE_SESSION_MANAGER_ID").as_deref(),
        Some("cbf8008e")
    );
    assert_eq!(
        environ_value(b"CLAUDE_SESSION_MANAGER_ID=\0", "CLAUDE_SESSION_MANAGER_ID"),
        None
    );
    assert_eq!(
        environ_value(b"OTHER=1\0", "CLAUDE_SESSION_MANAGER_ID"),
        None
    );
}

#[test]
fn truncates_long_commands() {
    assert_eq!(truncate_chars("abcdef", 3), "abc…");
    assert_eq!(truncate_chars("abc", 3), "abc");
}

/// Kernel fork notices see the shell behind a detached command before it
/// exits, so the command stays attributable after it is reparented.
#[cfg(target_os = "macos")]
#[test]
fn fork_notices_record_the_group_of_a_short_lived_child() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let groups = Arc::new(Mutex::new(HashMap::new()));
    let watcher = fork_watch::ForkWatcher::spawn(groups.clone());
    // This test process stands in for a harness.
    let me = std::process::id() as i32;
    watcher.watch(&[(me, owner("s1"))]);
    thread::sleep(Duration::from_millis(700));
    let mut shell = Command::new("/bin/sh");
    shell
        .args(["-c", "( sleep 5 >/dev/null 2>&1 & ); sleep 0.3; exit 0"])
        .stdin(Stdio::null())
        .process_group(0);
    let mut child = shell.spawn().unwrap();
    let shell_pid = child.id() as i32;
    child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !groups.lock().unwrap().contains_key(&shell_pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(groups.lock().unwrap().get(&shell_pid), Some(&owner("s1")));
}
