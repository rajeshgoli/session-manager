//! Run a child process to completion under a wall-clock timeout, capturing its
//! stdout and stderr.
//!
//! Both pipes are drained on reader threads while the child runs. Reading them
//! only after exit deadlocks as soon as the child writes more than the OS pipe
//! buffer (64 KB on macOS): the child blocks on write and never exits (#1471).

use std::{
    io::{self, Read},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Spawn `command` with piped stdout/stderr and wait up to `timeout` for it to
/// exit and for both pipes to reach EOF. On timeout the child is killed and an
/// error of the form `timed out after {N}s` is returned.
pub fn output_with_timeout(mut command: Command, timeout: Duration) -> Result<Output, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout_reader = child.stdout.take().map(spawn_reader);
    let stderr_reader = child.stderr.take().map(spawn_reader);
    let started = Instant::now();
    let timed_out = || format!("timed out after {}s", timeout.as_secs());
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(timed_out());
        }
        thread::sleep(POLL_INTERVAL);
    };
    // A grandchild that inherited a pipe can hold it open after the child
    // exits, so collecting output is bounded by the same deadline.
    let collect = |reader: Option<mpsc::Receiver<io::Result<Vec<u8>>>>| match reader {
        None => Ok(Vec::new()),
        Some(receiver) => match receiver.recv_timeout(timeout.saturating_sub(started.elapsed())) {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(timed_out()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("output reader stopped unexpectedly".to_owned())
            }
        },
    };
    let stdout = collect(stdout_reader)?;
    let stderr = collect(stderr_reader)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn spawn_reader(mut stream: impl Read + Send + 'static) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stream.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_larger_than_the_pipe_buffer() {
        // 1 MiB on stdout and 256 KiB on stderr: far past the 64 KB pipe
        // buffer, which deadlocked the read-after-exit helpers.
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "head -c 1048576 /dev/zero; head -c 262144 /dev/zero >&2",
        ]);
        let output = output_with_timeout(command, Duration::from_secs(10)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1_048_576);
        assert_eq!(output.stderr.len(), 262_144);
    }

    #[test]
    fn kills_a_child_that_outlives_the_timeout() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = Instant::now();
        let error = output_with_timeout(command, Duration::from_millis(200)).unwrap_err();
        assert_eq!(error, "timed out after 0s");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn reports_exit_status_and_small_output() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf out; printf err >&2; exit 3"]);
        let output = output_with_timeout(command, Duration::from_secs(10)).unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }
}
