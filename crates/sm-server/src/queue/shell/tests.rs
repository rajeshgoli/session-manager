use super::*;

use crate::queue::{
    get_queue_job_conn, list_queue_job_runtime_records_conn, running_queue_job_count,
    RetainedQueueStore,
};

#[test]
fn reservation_tiers_follow_headroom() {
    assert_eq!(reservation_for_headroom(100 * GIB), Some(16 * GIB));
    assert_eq!(reservation_for_headroom(100 * GIB - 1), Some(8 * GIB));
    assert_eq!(reservation_for_headroom(20 * GIB), Some(8 * GIB));
    assert_eq!(reservation_for_headroom(20 * GIB - 1), None);
    assert_eq!(reservation_for_headroom(-5 * GIB), None);
}

#[test]
fn headroom_subtracts_what_running_jobs_reserved_but_have_not_used() {
    // The memo's worked example: 140 free, 8 reserve, a perf job reserving
    // 40 and using 15, then a fresh 16 GiB shell job using about 1.
    let first = headroom_bytes(140 * GIB, 8 * GIB, &[(40 * GIB, 15 * GIB)]);
    assert_eq!(first, 107 * GIB);
    assert_eq!(reservation_for_headroom(first), Some(16 * GIB));
    let second = headroom_bytes(140 * GIB, 8 * GIB, &[(40 * GIB, 15 * GIB), (16 * GIB, GIB)]);
    assert_eq!(second, 92 * GIB);
    assert_eq!(reservation_for_headroom(second), Some(8 * GIB));
    // A job over its reservation leaves nothing unspent, not a credit.
    assert_eq!(
        headroom_bytes(50 * GIB, 8 * GIB, &[(5 * GIB, 9 * GIB)]),
        42 * GIB
    );
}

#[test]
fn a_declared_ceiling_counts_in_full_until_the_job_has_history() {
    // Under ten minutes old, or never sampled, a job may reach its ceiling.
    assert_eq!(
        expected_bytes(192 * GIB, GIB, Some(GIB), EXPECTATION_WARMUP_MS - 1, &[]),
        192 * GIB
    );
    assert_eq!(
        expected_bytes(192 * GIB, GIB, None, EXPECTATION_WARMUP_MS * 10, &[]),
        192 * GIB
    );
}

#[test]
fn a_flat_job_is_expected_at_its_peak_not_its_ceiling() {
    // sm#2490: 2305-reach-full-r2 declared 192 GiB, peaked at 39, sat near 29.
    let minute = 60 * 1000;
    let flat: Vec<(i64, i64)> = (0..15).map(|m| (m * minute, 29 * GIB)).collect();
    let expected = expected_bytes(192 * GIB, 29 * GIB, Some(39 * GIB), 7 * 60 * minute, &flat);
    assert_eq!(expected, 39 * GIB);
    // The whole incident: 170 free, 8 reserve, this job and a 16 GiB
    // ceiling at 2.4 GiB (seven hours old, flat) leave enough to adopt.
    let headroom = headroom_bytes(
        170 * GIB,
        8 * GIB,
        &[(expected, 29 * GIB), (16 * GIB, 2 * GIB)],
    );
    assert_eq!(headroom, 138 * GIB);
    assert_eq!(reservation_for_headroom(headroom), Some(16 * GIB));
}

#[test]
fn a_growing_job_is_expected_where_its_trend_reaches_capped_at_its_ceiling() {
    // 1 GiB a minute for the last 15 minutes, now at 40: 55 in 15 minutes.
    let minute = 60 * 1000;
    let growing: Vec<(i64, i64)> = (0..=15).map(|m| (m * minute, (25 + m) * GIB)).collect();
    let age = 60 * minute;
    assert_eq!(
        expected_bytes(192 * GIB, 40 * GIB, Some(40 * GIB), age, &growing),
        55 * GIB
    );
    assert_eq!(
        expected_bytes(50 * GIB, 40 * GIB, Some(40 * GIB), age, &growing),
        50 * GIB
    );
    // A shrinking job is not credited below its peak.
    let shrinking: Vec<(i64, i64)> = (0..=15).map(|m| (m * minute, (40 - m) * GIB)).collect();
    assert_eq!(
        expected_bytes(192 * GIB, 25 * GIB, Some(40 * GIB), age, &shrinking),
        40 * GIB
    );
}

#[test]
fn command_text_unwraps_each_harness_shell() {
    let claude = "/bin/zsh -c source /Users/r/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true && setopt NO_EXTENDED_GLOB 2>/dev/null || true && eval 'cargo test -p sm-server -- it'\\''s' < /dev/null && pwd -P >| /tmp/claude-cb45-cwd";
    assert_eq!(command_text(claude), "cargo test -p sm-server -- it's");
    assert_eq!(command_text("/bin/zsh -lc npm run dev"), "npm run dev");
    assert_eq!(command_text("bash -c sleep 180"), "sleep 180");
    assert_eq!(command_text("sleep 180"), "sleep 180");
    assert_eq!(
        command_text("/usr/bin/python3 markets.py --replay"),
        "/usr/bin/python3 markets.py --replay"
    );
}

#[test]
fn labels_are_one_line_and_cut() {
    assert_eq!(
        label_text("cargo   test\n  --workspace"),
        "cargo test --workspace"
    );
    let long = "x".repeat(130);
    assert_eq!(
        label_text("/opt/homebrew/Cellar/python@3.14/3.14.5/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python -c 'x=1'"),
        "Python -c 'x=1'"
    );
    assert_eq!(label_text("./run.sh --fast"), "./run.sh --fast");
    assert_eq!(label_text(&long).chars().count(), 121);
    assert!(label_text(&long).ends_with('…'));
}

#[test]
fn only_plain_test_runs_and_build_checks_get_a_type() {
    for command in [
        "cargo test --workspace",
        "cargo nextest run",
        "cargo build -p sm-server",
        "cargo clippy --all-targets",
        "pytest -q tests",
        "python -m pytest tests",
        "python3 -m pytest",
        ".venv/bin/pytest -x",
        "npx jest",
        "./gradlew assembleDebug",
        "scripts/test-rust-isolated.sh",
        "cd crates && cargo test",
    ] {
        assert_eq!(type_hint(command), Some("tests"), "{command}");
    }
    for command in [
        "python markets.py --replay",
        "npm run dev",
        "cargo run --release",
        "make",
        "sleep 180",
    ] {
        assert_eq!(type_hint(command), None, "{command}");
    }
}

#[test]
fn the_resubmit_line_quotes_shell_syntax_and_guides_an_unknown_type() {
    let plain = resubmit_text("cargo test --workspace", 10 * GIB, None);
    assert_eq!(
        plain,
        "  sm queue run --type tests --label cargo-test --memory 15G -- cargo test --workspace"
    );
    let piped = resubmit_text("python markets.py | tee out.log", 11 * GIB + GIB / 2, None);
    assert!(
        piped.starts_with(
            "  sm queue run --type <type> --label python-markets-py --memory 18G -- zsh -lc 'python markets.py | tee out.log'"
        ),
        "{piped}"
    );
    assert!(
        piped.contains("Pick the type: tests for a test suite"),
        "{piped}"
    );
    let quoted = resubmit_text("echo 'hi'", GIB, Some("/tmp/x y"));
    assert!(quoted.contains("--cwd '/tmp/x y'"), "{quoted}");
    assert!(quoted.contains("--memory 2G"), "{quoted}");
    assert!(
        quoted.contains("-- zsh -lc 'echo '\\''hi'\\'''"),
        "{quoted}"
    );
}

fn state_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sm-shell-jobs-{name}-{}-{}",
        std::process::id(),
        generate_queue_job_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn adoption(pgid: i64) -> Adoption {
    Adoption {
        session_id: "agent1".to_owned(),
        root_pid: pgid,
        process_group_id: pgid,
        label: "python markets.py --replay".to_owned(),
        command: "python markets.py --replay".to_owned(),
        cwd: "/tmp".to_owned(),
        reservation_bytes: 8 * GIB,
        process_started_at: Some("2026-10-19T14:02:00Z".to_owned()),
        record: AdoptionRecord {
            adopted_at: "2026-10-19T14:05:00Z".to_owned(),
            trigger_pid: pgid + 1,
            trigger_bytes: 2 * GIB,
            headroom_bytes: 60 * GIB,
            root_identity: None,
            detached: false,
        },
    }
}

#[test]
fn an_adopted_job_runs_without_a_slot_and_ends_without_a_message() {
    let dir = state_dir("ended");
    let conn = open_queue_jobs_connection(&dir.join("queue_runner.db")).unwrap();
    let id = adopt_conn(&conn, &adoption(999_999)).unwrap();
    let job = get_queue_job_conn(&conn, &id).unwrap().unwrap();
    assert_eq!(job.job_type, SHELL_JOB_TYPE);
    assert_eq!(job.state, "running");
    assert_eq!(job.queued_at, "2026-10-19T14:02:00Z");
    assert_eq!(job.started_at.as_deref(), Some("2026-10-19T14:05:00Z"));
    assert_eq!(job.memory_bytes, Some(8 * GIB));
    assert_eq!(job.notify_session_id.as_deref(), Some("agent1"));
    let records = list_queue_job_runtime_records_conn(&conn).unwrap();
    assert_eq!(running_queue_job_count(&records, None), 0);
    assert_eq!(
        adoption_record_conn(&conn, &id)
            .unwrap()
            .unwrap()
            .headroom_bytes,
        60 * GIB
    );
    assert_eq!(running_shell_jobs_conn(&conn).unwrap().len(), 1);

    let messages = dir.join("messages.db");
    finish_gone_conn(&conn, &id, &messages).unwrap();
    let job = get_queue_job_conn(&conn, &id).unwrap().unwrap();
    assert_eq!(job.state, "ended");
    assert!(running_shell_jobs_conn(&conn).unwrap().is_empty());
    let sent = RetainedQueueStore::new(messages)
        .pending_messages_for_target_by_category("agent1", "queue-completion", 10)
        .unwrap();
    assert!(sent.is_empty(), "{sent:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_host_pressure_stop_tells_the_agent_how_to_resubmit() {
    let dir = state_dir("pressure");
    let conn = open_queue_jobs_connection(&dir.join("queue_runner.db")).unwrap();
    let id = adopt_conn(&conn, &adoption(999_998)).unwrap();
    record_usage(&id, 11 * GIB + GIB / 2);
    assert!(over_reservation(&id, Some(8 * GIB)));
    let (summary, detail) =
        holding_text(&id, "far-1955", Some(8 * GIB), Some("2026-10-19T14:05:00Z"));
    assert_eq!(
        summary.as_deref(),
        Some("Over its reservation · stops first if memory runs short")
    );
    assert_eq!(
        detail,
        "far-1955 started this from its own shell. At 14:05:00 UTC it passed 1 GB and sm took it into the queue with 8.0 GiB set aside."
    );
    conn.execute(
        "UPDATE queue_jobs SET holding_reason = 'memory_terminating', termination_detail_json = ?2 WHERE id = ?1",
        params![
            id,
            serde_json::json!({
                "cause": MEMORY_GUARD_HOST_PRESSURE,
                "host_available_bytes": 6 * GIB,
                "effective_reserve_bytes": 8 * GIB,
                "process_group_rss_bytes": 11 * GIB,
            })
            .to_string()
        ],
    )
    .unwrap();
    let messages = dir.join("messages.db");
    finish_gone_conn(&conn, &id, &messages).unwrap();
    assert_eq!(
        get_queue_job_conn(&conn, &id).unwrap().unwrap().state,
        "memory_exceeded"
    );
    let sent = RetainedQueueStore::new(messages)
        .pending_messages_for_target_by_category("agent1", "queue-completion", 10)
        .unwrap();
    assert_eq!(sent.len(), 1);
    let text = &sent[0].text;
    assert!(text.contains("sm stopped `python markets.py --replay` (PID 999998), which you started from your shell at 14:02:00 UTC."), "{text}");
    assert!(text.contains("The Mac ran short of memory (6.0 GiB free; it keeps 8.0 GiB in reserve), and this command was using 11.5 GiB, above the 8.0 GiB sm had set aside for it."), "{text}");
    assert!(
        text.contains(
            "--type <type> --label python-markets-py --memory 18G -- python markets.py --replay"
        ),
        "{text}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_owner_stop_names_the_owner() {
    let dir = state_dir("owner");
    let conn = open_queue_jobs_connection(&dir.join("queue_runner.db")).unwrap();
    let id = adopt_conn(&conn, &adoption(999_997)).unwrap();
    conn.execute(
        "UPDATE queue_jobs SET holding_reason = 'cancelling', termination_detail_json = ?2 WHERE id = ?1",
        params![id, r#"{"kind":"cancel","cancelled_by":"Rajesh from the sm app","note":"the replay can wait"}"#],
    )
    .unwrap();
    let messages = dir.join("messages.db");
    finish_gone_conn(&conn, &id, &messages).unwrap();
    let sent = RetainedQueueStore::new(messages)
        .pending_messages_for_target_by_category("agent1", "queue-completion", 10)
        .unwrap();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0]
            .text
            .contains("It was stopped by Rajesh from the sm app. Note: the replay can wait. Rerun"),
        "{}",
        sent[0].text
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_stop_message_resubmits_the_full_command_not_the_cut_label() {
    let dir = state_dir("long");
    let conn = open_queue_jobs_connection(&dir.join("queue_runner.db")).unwrap();
    let command = format!("python train.py {}", "--flag ".repeat(30).trim_end());
    let mut long = adoption(999_996);
    long.label = label_text(&command);
    long.command = command.clone();
    let id = adopt_conn(&conn, &long).unwrap();
    conn.execute(
        "UPDATE queue_jobs SET holding_reason = 'cancelling', termination_detail_json = '{\"kind\":\"cancel\"}' WHERE id = ?1",
        params![id],
    )
    .unwrap();
    let messages = dir.join("messages.db");
    finish_gone_conn(&conn, &id, &messages).unwrap();
    let sent = RetainedQueueStore::new(messages)
        .pending_messages_for_target_by_category("agent1", "queue-completion", 10)
        .unwrap();
    assert!(
        sent[0].text.contains(&format!("-- {command}")),
        "{}",
        sent[0].text
    );
    assert!(
        sent[0]
            .text
            .contains("It was cancelled with sm queue cancel."),
        "{}",
        sent[0].text
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recovery_resumes_a_stop_the_restart_interrupted() {
    use std::os::unix::process::CommandExt;
    let dir = state_dir("resume-stop");
    let conn = open_queue_jobs_connection(&dir.join("queue_runner.db")).unwrap();
    let messages = dir.join("messages.db");
    // A command that ignores SIGTERM, leading its own group.
    let mut child = std::process::Command::new("sh")
        .args(["-c", "trap '' TERM; sleep 60"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = i64::from(child.id());
    std::thread::sleep(std::time::Duration::from_millis(200));
    let mut live = adoption(pid);
    live.root_pid = pid;
    live.record.root_identity = crate::agent_memory_guard::process_identity_text(pid as i32);
    let id = adopt_conn(&conn, &live).unwrap();
    conn.execute(
        "UPDATE queue_jobs SET holding_reason = 'cancelling' WHERE id = ?1",
        params![id],
    )
    .unwrap();
    let job = get_queue_job_runtime_conn(&conn, &id).unwrap().unwrap();
    assert!(recover_conn(&conn, &job, &messages, false, 1).unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the interrupted stop was not resumed"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recovery_keeps_a_job_only_while_its_root_is_the_adopted_process() {
    let dir = state_dir("recover");
    let conn = open_queue_jobs_connection(&dir.join("queue_runner.db")).unwrap();
    let messages = dir.join("messages.db");
    // This test process stands in for a live command root leading a live group.
    let pid = std::process::id() as i64;
    // SAFETY: getpgrp has no failure mode.
    let pgid = i64::from(unsafe { libc::getpgrp() });
    let mut live = adoption(pgid);
    live.root_pid = pid;
    live.record.root_identity = crate::agent_memory_guard::process_identity_text(pid as i32);
    let kept = adopt_conn(&conn, &live).unwrap();
    let job = get_queue_job_runtime_conn(&conn, &kept).unwrap().unwrap();
    assert!(recover_conn(&conn, &job, &messages, false, 10).unwrap());
    // Same live group number, but the recorded root was another process.
    let mut reused = live.clone();
    reused.record.root_identity = Some("an earlier process".to_owned());
    let stale = adopt_conn(&conn, &reused).unwrap();
    let job = get_queue_job_runtime_conn(&conn, &stale).unwrap().unwrap();
    assert!(!recover_conn(&conn, &job, &messages, false, 10).unwrap());
    assert_eq!(
        get_queue_job_conn(&conn, &stale).unwrap().unwrap().state,
        "ended"
    );
    // Adopted before the Mac last booted: it died with the host.
    let job = get_queue_job_runtime_conn(&conn, &kept).unwrap().unwrap();
    assert!(!recover_conn(&conn, &job, &messages, true, 10).unwrap());
    assert_eq!(
        get_queue_job_conn(&conn, &kept).unwrap().unwrap().state,
        "host_restart"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn command_from_argv_keeps_argument_boundaries() {
    let argv = |items: &[&str]| {
        items
            .iter()
            .map(|item| (*item).to_owned())
            .collect::<Vec<_>>()
    };
    // Codex runs a program directly; its -c source is one argument.
    assert_eq!(
        command_from_argv(&argv(&["python3", "-c", "import time; time.sleep(9)"])),
        "python3 -c 'import time; time.sleep(9)'"
    );
    assert_eq!(command_from_argv(&argv(&["sleep", "180"])), "sleep 180");
    assert_eq!(
        command_from_argv(&argv(&["bash", "-c", r#"printf "%s" "$1""#, "_", "value"])),
        r#"bash -c 'printf "%s" "$1"' _ value"#
    );
    assert_eq!(command_from_argv(&argv(&["echo", ""])), "echo ''");
    assert_eq!(
        command_from_argv(&argv(&["/bin/zsh", "-lc", "npm run dev | tee x"])),
        "npm run dev | tee x"
    );
    let claude = r#"source /x/snapshot.sh 2>/dev/null || true && eval 'cargo test -- it'\''s' < /dev/null && pwd -P >| /tmp/c"#;
    assert_eq!(
        command_from_argv(&argv(&["/bin/zsh", "-c", claude])),
        "cargo test -- it's"
    );
    // The resubmit line passes the quoted command through zsh -lc intact.
    let line = resubmit_text(
        &command_from_argv(&argv(&["python3", "-c", "print(1)"])),
        GIB,
        None,
    );
    assert!(
        line.lines()
            .next()
            .unwrap()
            .ends_with(r#"-- zsh -lc 'python3 -c '\''print(1)'\'''"#),
        "{line}"
    );
}

#[test]
fn process_argv_reads_this_process_with_its_boundaries() {
    let argv = crate::queue::process_argv(i64::from(std::process::id())).unwrap();
    assert_eq!(argv, std::env::args().collect::<Vec<_>>());
}
