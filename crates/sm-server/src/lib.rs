/// The server's `eprintln!`, in scope for every module below and imported by
/// `main.rs`. Std's panics when stderr cannot be written, and the server's
/// stderr is a log file: when the disk filled, every background loop that
/// logged an error panicked and stopped for good (#1995, #1994, #1996). This
/// one drops the line instead. Tests keep std's so the harness captures them.
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {
        if cfg!(test) {
            ::std::eprintln!($($arg)*)
        } else {
            $crate::write_stderr_line(format_args!($($arg)*))
        }
    };
}

/// Write one line to stderr, ignoring a failed write rather than panicking.
#[doc(hidden)]
pub fn write_stderr_line(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let mut stderr = std::io::stderr().lock();
    let _ = stderr
        .write_fmt(args)
        .and_then(|()| stderr.write_all(b"\n"));
}

pub mod activity_ledger;
pub mod agent_notes;
pub mod analytics_spend;
pub mod analytics_time;
pub mod app_artifacts;
pub mod board;
pub mod btw;
pub mod bug_reports;
pub mod child_output;
pub mod claude_remote_control;
pub mod cloudflare_access;
pub mod codex_activity;
pub mod codex_events;
pub mod codex_requests;
pub mod config;
pub mod doc_markdown;
pub mod email;
pub mod google_auth;
pub mod guestbook;
pub mod handoff;
pub mod handover;
pub mod http;
pub mod local_egress;
pub mod local_identity;
pub mod local_judge;
pub mod local_model;
pub mod local_sockets;
#[cfg(target_os = "macos")]
pub mod local_wall;
pub mod mobile_devices;
pub mod notes;
pub mod opencode;
pub mod owner_doc_render;
pub mod owner_docs;
pub mod owner_inbox;
pub mod owner_messages;
pub mod owner_push;
pub mod owner_settings;
pub mod push_fcm;
pub mod queue;
pub mod queue_authority;
pub mod quota_rates;
pub mod review;
pub mod runtime;
pub mod seat_sessions;
pub mod sessions;
pub mod studio_ssh;
pub mod terminal_lan;
pub mod tool_usage;
pub mod turn_messages;
pub mod usage_burn;
mod usage_db;
pub mod usage_identity;
pub mod usage_ledger;
pub mod usage_meters;
pub mod usage_report;
pub mod utilization;
pub mod watch_view;
pub mod work_attribution;
pub mod work_claims;
pub mod work_history;

pub mod host_status;

#[cfg(test)]
mod stderr_tests {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};

    const CHILD: &str = "SM_STDERR_BROKEN_PIPE_CHILD";

    /// The child half runs with stderr on a pipe whose reader is gone, so every
    /// write fails with EPIPE, as a log write did when the disk filled.
    #[test]
    fn a_failed_stderr_write_does_not_panic() {
        if std::env::var_os(CHILD).is_some() {
            let std_result = std::panic::catch_unwind(|| ::std::eprintln!("lost"));
            assert!(std_result.is_err(), "std's eprintln! must panic here");
            super::write_stderr_line(format_args!("lost {}", 1));
            std::process::exit(0);
        }
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // The child must not inherit the read end, or the pipe stays readable.
        let (reader, writer) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        drop(reader);
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stderr_tests::a_failed_stderr_write_does_not_panic",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::from(writer))
            .status()
            .unwrap();
        assert!(status.success(), "child failed: {status}");
    }
}
