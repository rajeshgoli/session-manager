use super::*;

/// A fixture owns these advisory locks even while admission or restore needs
/// its TCP ports unbound. Other test processes use the same lock files.
pub(crate) struct PortRun {
    pub(crate) first: u16,
    _locks: Vec<nix::fcntl::Flock<std::fs::File>>,
}

impl PortRun {
    fn claim(first: u16, count: u16) -> std::io::Result<Self> {
        use nix::fcntl::{Flock, FlockArg};
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        // Deliberately outside each invocation's isolation root: concurrent
        // worktrees must lock the same inodes. Never unlink a lock file while
        // another process may hold it open.
        // SAFETY: getuid has no arguments or memory preconditions.
        let uid = unsafe { libc::getuid() };
        let root = std::path::PathBuf::from(format!("/tmp/sm-opencode-test-ports-{uid}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
        let mut locks = Vec::new();
        for port in first..first + count {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(root.join(port.to_string()))?;
            locks.push(
                Flock::lock(file, FlockArg::LockExclusiveNonblock)
                    .map_err(|(_, error)| std::io::Error::from_raw_os_error(error as i32))?,
            );
        }
        Ok(Self {
            first,
            _locks: locks,
        })
    }
}

/// Use 30000–39999, below macOS's ephemeral range and away from the fixed
/// control-port tests (21000). Probe only after acquiring cross-process locks.
/// The issued set also avoids reusing a recently closed port in this process.
pub(crate) fn free_port_run(count: u16) -> PortRun {
    assert!((1..=10_000).contains(&count));
    static ISSUED: std::sync::Mutex<std::collections::BTreeSet<u16>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let mut issued = ISSUED.lock().unwrap();
    for _ in 0..100 {
        let port = 30_000 + (OsRng.next_u32() % u32::from(10_001 - count)) as u16;
        if (port..port + count).any(|p| issued.contains(&p)) {
            continue;
        }
        let lease = match PortRun::claim(port, count) {
            Ok(lease) => lease,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => panic!("could not reserve fixture port {port}: {error}"),
        };
        if probe_in_child(port, count) {
            issued.extend(port..port + count);
            return lease;
        }
    }
    panic!("no run of {count} free local ports");
}

fn port_child(port: u16, count: u16, mode: &str) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "opencode::tests::test_ports::fixture_port_reservations_cross_process",
            "--nocapture",
        ])
        .env("SM_OPENCODE_PORT_TEST_CHILD", port.to_string())
        .env("SM_OPENCODE_PORT_TEST_COUNT", count.to_string())
        .env("SM_OPENCODE_PORT_TEST_MODE", mode);
    command
}

// A listener probed in the multithreaded test host can be inherited by an
// unrelated Command::pre_exec fork. CLOEXEC releases it only at that child's
// exec, so dropping our copy does not prove that the port is free. This child
// performs no other spawns, and waiting for its exit closes every probe copy.
fn probe_in_child(port: u16, count: u16) -> bool {
    let output = port_child(port, count, "probe").output().unwrap();
    if output.status.code() == Some(42) {
        return false;
    }
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("fixture probe bound all ports"),
        "port probe failed: {}\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

pub(crate) fn wait_until_port_free(port: u16) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !probe_in_child(port, 1) {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture port {port} remained bound after release"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

pub(crate) fn report_bind_failure(port: u16, error: &std::io::Error) {
    eprintln!(
        "OpenCode admission: pid={} port={port} bind error={error:?}",
        std::process::id()
    );
    match std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}")])
        .output()
    {
        Ok(output) => eprintln!(
            "OpenCode port {port} owners (status={}):\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => eprintln!("OpenCode port {port} owner lookup failed: {error}"),
    }
}

#[test]
fn fixture_port_reservations_cross_process() {
    const CHILD_PORT: &str = "SM_OPENCODE_PORT_TEST_CHILD";
    const CHILD_MODE: &str = "SM_OPENCODE_PORT_TEST_MODE";
    if let Ok(port) = std::env::var(CHILD_PORT) {
        let port = port.parse().unwrap();
        let count = std::env::var("SM_OPENCODE_PORT_TEST_COUNT")
            .unwrap()
            .parse::<u16>()
            .unwrap();
        match std::env::var(CHILD_MODE).unwrap().as_str() {
            "inherit" => {
                use std::os::{fd::AsRawFd, unix::process::CommandExt};
                let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
                let (mut gate, child_gate) = std::os::unix::net::UnixStream::pair().unwrap();
                gate.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let worker = thread::spawn(move || {
                    let fd = child_gate.as_raw_fd();
                    let mut command = std::process::Command::new("/usr/bin/true");
                    // SAFETY: only async-signal-safe read/write and errno
                    // access run between fork and exec. The gate remains live.
                    unsafe {
                        command.pre_exec(move || {
                            let mut byte = b'R';
                            if libc::write(fd, (&byte as *const u8).cast(), 1) != 1
                                || libc::read(fd, (&mut byte as *mut u8).cast(), 1) != 1
                            {
                                return Err(std::io::Error::last_os_error());
                            }
                            Ok(())
                        });
                    }
                    let status = command.status().unwrap();
                    drop(child_gate);
                    status
                });
                gate.read_exact(&mut [0]).unwrap();
                drop(listener);
                let config = OpencodeConfig {
                    port_range: [port, port],
                    ..OpencodeConfig::default()
                };
                let refusal = launch::lowest_port(&config, &Default::default());
                let probe_was_blocked = !probe_in_child(port, 1);
                gate.write_all(b"R").unwrap();
                assert!(worker.join().unwrap().success());
                let error = refusal.unwrap_err();
                assert_eq!(
                    error
                        .downcast_ref::<std::io::Error>()
                        .unwrap()
                        .raw_os_error(),
                    Some(libc::EADDRINUSE)
                );
                assert!(probe_was_blocked);
                wait_until_port_free(port);
                assert_eq!(
                    launch::lowest_port(&config, &Default::default()).unwrap(),
                    port
                );
            }
            "probe" => {
                let _listeners: Vec<_> = (port..port + count)
                    .map(
                        |candidate| match TcpListener::bind(("127.0.0.1", candidate)) {
                            Ok(listener) => listener,
                            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                                std::process::exit(42)
                            }
                            Err(error) => panic!("probe port {candidate}: {error:?}"),
                        },
                    )
                    .collect();
                // Prevent a stale --exact test path from silently treating
                // "0 tests" as a successful bind probe.
                println!("fixture probe bound all ports");
            }
            "blocked" => {
                for candidate in port..port + count {
                    assert_eq!(
                        PortRun::claim(candidate, 1).err().unwrap().kind(),
                        std::io::ErrorKind::WouldBlock
                    );
                }
                // No TCP socket is held: only the fixture reservation prevents
                // another cooperating process from choosing this port.
                let _listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
            }
            "free" => {
                let _ports = PortRun::claim(port, count).unwrap();
            }
            "bind" => {
                // Model a competitor using the old bind-then-drop allocator,
                // which ignores reservations held by another test process.
                let _listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
                println!("competing listener ready");
                std::io::stdout().flush().unwrap();
                let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
            }
            mode => panic!("unknown child mode {mode}"),
        }
        return;
    }
    let ports = free_port_run(2);
    let child = |mode: &str| port_child(ports.first, 2, mode);
    assert!(child("blocked").status().unwrap().success());
    let mut competitor = child("bind")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = std::io::BufReader::new(competitor.stdout.take().unwrap());
    let mut line = String::new();
    use std::io::BufRead;
    loop {
        assert_ne!(output.read_line(&mut line).unwrap(), 0, "{line}");
        if line.contains("competing listener ready") {
            break;
        }
    }
    eprintln!(
        "Competing fixture: pid={} port={}",
        competitor.id(),
        ports.first
    );
    let config = OpencodeConfig {
        port_range: [ports.first, ports.first],
        ..OpencodeConfig::default()
    };
    let error = launch::lowest_port(&config, &Default::default()).unwrap_err();
    // Release the child before asserting so failures cannot strand its socket.
    drop(competitor.stdin.take());
    assert!(competitor.wait().unwrap().success());
    assert_eq!(error.to_string(), "no local port free");
    assert!(error.downcast_ref::<launch::AdmissionError>().is_some());
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(libc::EADDRINUSE)
    );
    assert!(format!("{error:#}").contains(&format!("127.0.0.1:{}", ports.first)));
    wait_until_port_free(ports.first);
    assert_eq!(
        launch::lowest_port(&config, &Default::default()).unwrap(),
        ports.first
    );
    let mut released = child("free");
    drop(ports);
    assert!(released.status().unwrap().success());
}

#[test]
fn forked_probe_retains_port_until_exec() {
    let ports = free_port_run(1);
    let output = port_child(ports.first, 1, "inherit").output().unwrap();
    eprintln!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{}", output.status);
}
