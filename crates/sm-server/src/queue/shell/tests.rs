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
    assert!(recover_conn(&conn, &job, &messages, false).unwrap());
    // Same live group number, but the recorded root was another process.
    let mut reused = live.clone();
    reused.record.root_identity = Some("an earlier process".to_owned());
    let stale = adopt_conn(&conn, &reused).unwrap();
    let job = get_queue_job_runtime_conn(&conn, &stale).unwrap().unwrap();
    assert!(!recover_conn(&conn, &job, &messages, false).unwrap());
    assert_eq!(
        get_queue_job_conn(&conn, &stale).unwrap().unwrap().state,
        "ended"
    );
    // Adopted before the Mac last booted: it died with the host.
    let job = get_queue_job_runtime_conn(&conn, &kept).unwrap().unwrap();
    assert!(!recover_conn(&conn, &job, &messages, true).unwrap());
    assert_eq!(
        get_queue_job_conn(&conn, &kept).unwrap().unwrap().state,
        "host_restart"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
