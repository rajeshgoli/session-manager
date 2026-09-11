//! Exercise the actual binary in a PTY; the API fixture never touches live state.
use nix::{
    libc,
    pty::{openpty, Winsize},
    sys::termios,
};
use serde_json::json;
use std::{
    fs::File,
    io::{Read, Write},
    net::TcpListener,
    os::fd::AsRawFd,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn until(master: &mut File, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    while Instant::now() < deadline {
        let mut p = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut p, 1, 100) } > 0 {
            let mut bytes = [0; 16384];
            match master.read(&mut bytes) {
                Ok(n) if n > 0 => output.push_str(&String::from_utf8_lossy(&bytes[..n])),
                _ => break,
            }
            if output.contains(needle) {
                return output;
            }
        }
    }
    panic!("did not see {needle:?}; terminal output:\n{output}");
}

#[test]
fn native_watch_handles_key_bursts_live_logs_resize_and_signal_cleanup() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let peer = thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut raw = Vec::new();
            let mut b = [0];
            while !raw.ends_with(b"\r\n\r\n") {
                if stream.read_exact(&mut b).is_err() {
                    break;
                }
                raw.push(b[0]);
            }
            let request = String::from_utf8_lossy(&raw);
            if request.is_empty() {
                continue;
            }
            assert!(
                request.starts_with("GET "),
                "unexpected mutation: {request}"
            );
            let path = request.split_whitespace().nth(1).unwrap();
            assert!(
                !path.contains("/output") && !path.contains("/tool-calls"),
                "job tail must not fetch agent output: {path}"
            );
            let body=match path {
            "/sessions"=>json!({"sessions":[{"id":"agent001","friendly_name":"needle","working_dir":"/repo","status":"running","activity_state":"idle","provider":"claude"}]}),
            "/queue-jobs"=>json!({"jobs":[{"id":"job001","requester_session_id":"agent001","notify_session_id":"agent001","state":"running","label":"fixture-job","queued_at":"2026-09-10T10:00:00Z","started_at":"2026-09-10T10:00:01Z"}]}),
            "/reparent-requests"=>json!({"requests":[]}),
            "/session-obligations"=>json!({"sessions":[{"session_id":"agent001","waiting_on":[{"kind":"queue_job","id":"job001","label":"fixture-job","since":"2026-09-10T10:00:00Z"}]}]}),
            "/queue-jobs/job001/log?lines=200"=>json!({"text":"fixture-live-output\n"}),
            "/queue-jobs/job001/log?lines=6"=>json!({"text":"five-line-output\n"}),
            _=>json!({}),
        }.to_string();
            let _=write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
        }
    });
    let pty = openpty(
        Some(&Winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None,
    )
    .unwrap();
    let slave = File::from(pty.slave);
    let before = termios::tcgetattr(&slave).unwrap();
    let mut master = File::from(pty.master);
    let mut child = Process(
        Command::new(env!("CARGO_BIN_EXE_sm"))
            .args([
                "--api-url",
                &format!("http://{addr}"),
                "watch",
                "--interval",
                "0.2",
            ])
            .env_remove("SESSION_MANAGER_ID")
            .env_remove("CLAUDE_SESSION_MANAGER_ID")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .spawn()
            .unwrap(),
    );
    let initial = until(&mut master, "◷  needle");
    assert!(!initial.contains("fixture-job · waiting"));
    // The job is directly selectable without expanding the agent or pressing J.
    master.write_all(b"/needle\rj\t").unwrap();
    until(&mut master, "five-line-output");
    master.write_all(b"\t").unwrap();
    until(&mut master, "fixture-live-output");
    master.write_all(b"g").unwrap();
    until(&mut master, "all agents");
    let size = Winsize {
        ws_row: 10,
        ws_col: 42,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    master.write_all(b"q").unwrap();
    until(&mut master, "sm watch");
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "watch did not exit on SIGTERM");
        thread::sleep(Duration::from_millis(10));
    }
    let after = termios::tcgetattr(&slave).unwrap();
    // PENDIN is kernel bookkeeping for pending input, not a terminal mode.
    let modes = termios::LocalFlags::ICANON
        | termios::LocalFlags::ECHO
        | termios::LocalFlags::ISIG
        | termios::LocalFlags::IEXTEN;
    assert_eq!(before.local_flags & modes, after.local_flags & modes);
    assert_eq!(before.input_flags, after.input_flags);
    stop.store(true, Ordering::Relaxed);
    peer.join().unwrap();
}
