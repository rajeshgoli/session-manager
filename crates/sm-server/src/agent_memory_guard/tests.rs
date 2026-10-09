use super::*;

const GIB: i64 = 1024 * 1024 * 1024;

fn row(pid: i32, ppid: i32, gib: i64, command: &str) -> ProcessRow {
    ProcessRow {
        pid,
        ppid,
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

fn victims(
    rows: &[ProcessRow],
    panes: &[AgentPane],
    exempt: &[&str],
) -> Vec<(String, i32, Vec<i32>)> {
    let exempt: Vec<String> = exempt.iter().map(|c| (*c).to_owned()).collect();
    agent_tree_victims(rows, panes, GIB, &exempt, |_| None)
        .into_iter()
        .map(|v| {
            let mut pids = v.pids;
            pids.sort_unstable();
            (v.session_id, v.pid, pids)
        })
        .collect()
}

#[test]
fn kills_an_over_limit_process_with_its_descendants_and_spares_the_harness() {
    // The 2026-10-08 shape: claude -> zsh -> python (135 GB) -> worker.
    let rows = [
        row(100, 1, 3, "/opt/homebrew/bin/claude"),
        row(101, 100, 0, "/bin/zsh"),
        row(102, 101, 135, "/usr/local/bin/python3.12"),
        row(103, 102, 0, "/usr/local/bin/python3.12"),
        row(104, 101, 0, "/bin/sleep"),
    ];
    assert_eq!(
        victims(&rows, &[pane("s1", 100)], &[]),
        vec![("s1".to_owned(), 102, vec![102, 103])]
    );
}

#[test]
fn finds_processes_that_left_the_pane_process_group() {
    // setsid changes the group, not the parent: the walk is by parent PID.
    let rows = [
        row(100, 1, 0, "/opt/homebrew/bin/claude"),
        row(101, 100, 0, "/bin/zsh"),
        row(102, 101, 2, "/usr/bin/setsid-child"),
    ];
    assert_eq!(victims(&rows, &[pane("s1", 100)], &[]).len(), 1);
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
    assert!(victims(&rows, &[pane("s1", 100)], &[]).is_empty());
}

#[test]
fn harness_and_exempt_executables_are_spared_but_their_children_are_checked() {
    let rows = [
        // The pane root itself is the harness, whatever its name.
        row(100, 1, 9, "/usr/local/bin/node"),
        row(101, 100, 9, "/Users/x/codex-fork/bin/codex-fork"),
        row(102, 101, 9, "/Users/x/.cargo/bin/rustc"),
        row(103, 102, 9, "/usr/bin/cc"),
    ];
    assert_eq!(
        victims(&rows, &[pane("s1", 100)], &["rustc"]),
        vec![("s1".to_owned(), 103, vec![103])]
    );
}

#[test]
fn processes_at_or_under_the_limit_survive() {
    let rows = [
        row(100, 1, 0, "/opt/homebrew/bin/claude"),
        row(101, 100, 1, "/bin/at-limit"),
    ];
    assert!(victims(&rows, &[pane("s1", 100)], &[]).is_empty());
}

#[test]
fn footprint_beats_resident_size_when_available() {
    // MLX buffers count in the footprint but not in RSS.
    let rows = [
        row(100, 1, 0, "/opt/homebrew/bin/claude"),
        row(101, 100, 0, "/usr/local/bin/python3.12"),
    ];
    let found = agent_tree_victims(&rows, &[pane("s1", 100)], GIB, &[], |pid| {
        (pid == 101).then_some(40 * GIB)
    });
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].memory_bytes, 40 * GIB);
}

#[test]
fn each_session_owns_its_own_tree() {
    let rows = [
        row(100, 1, 0, "/opt/homebrew/bin/claude"),
        row(101, 100, 2, "/bin/a"),
        row(200, 1, 0, "/opt/homebrew/bin/claude"),
        row(201, 200, 2, "/bin/b"),
    ];
    let mut found = victims(&rows, &[pane("s1", 100), pane("s2", 200)], &[]);
    found.sort();
    assert_eq!(
        found,
        vec![
            ("s1".to_owned(), 101, vec![101]),
            ("s2".to_owned(), 201, vec![201])
        ]
    );
}

#[test]
fn parses_padded_ps_rows_with_spaces_in_the_command() {
    let rows = parse_process_listing(
        "  100     1  2048 /opt/homebrew/bin/claude\n 4974  101 141557760 /Applications/Google Chrome.app/x\ngarbage\n",
    );
    assert_eq!(
        rows,
        vec![
            ProcessRow {
                pid: 100,
                ppid: 1,
                rss_kib: 2048,
                command: "/opt/homebrew/bin/claude".to_owned()
            },
            ProcessRow {
                pid: 4974,
                ppid: 101,
                rss_kib: 141_557_760,
                command: "/Applications/Google Chrome.app/x".to_owned()
            },
        ]
    );
}

#[test]
fn parses_tmux_pane_listing() {
    let panes = parse_pane_listing("sm-rust-claude-abc 8755\n__sm_server_anchor 1458\nbad\n");
    assert_eq!(panes.get("sm-rust-claude-abc"), Some(&8755));
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
fn notice_names_pid_command_memory_and_the_queue() {
    let text = kill_notice_text(&kill("s1", "2026-10-08T21:37:05Z"));
    assert!(
        text.contains("PID 4974 and its 2 child processes"),
        "{text}"
    );
    assert!(
        text.contains("`python3.12 train.py --heads` reached 135.0 GiB"),
        "{text}"
    );
    assert!(text.contains("over the 1.0 GiB limit"), "{text}");
    assert!(text.contains("Do not rerun it from your shell"), "{text}");
    assert!(text.contains("sm queue run"), "{text}");
    assert!(text.contains("--memory"), "{text}");
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
fn truncates_long_commands() {
    assert_eq!(truncate_chars("abcdef", 3), "abc…");
    assert_eq!(truncate_chars("abc", 3), "abc");
}
