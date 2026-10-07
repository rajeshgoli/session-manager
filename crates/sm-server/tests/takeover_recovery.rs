//! Exercise the real startup path used by launchd's retained --take-over args.
use std::{
    fs,
    net::TcpListener,
    os::unix::{fs::MetadataExt, net::UnixListener},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(format!(
            "/tmp/sm1993-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("config.yaml"),
            format!(
                "paths:\n  state_file: {}/sessions.json\nrust_core:\n  runtime_enabled: false\nusage:\n  enabled: false\n",
                root.display()
            ),
        )
        .unwrap();
        Self { root, child: None }
    }

    fn start(&mut self, port: u16) {
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_sm-server"))
                .args(["--take-over", "--port", &port.to_string(), "--config"])
                .arg(self.root.join("config.yaml"))
                .env("SM_TEST_ISOLATION_ROOT", self.root.join("isolated"))
                .env_remove("SM_LABEL")
                .stdout(Stdio::null())
                .stderr(fs::File::create(self.root.join("stderr")).unwrap())
                .spawn()
                .unwrap(),
        );
    }

    fn wait_healthy(&mut self, port: u16) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "server exited: {}",
                fs::read_to_string(self.root.join("stderr")).unwrap()
            );
            if sm_server::handover::probe_health(([127, 0, 0, 1], port).into()).is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "server did not become healthy");
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.kill();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn takeover_recovers_without_predecessor_and_after_crash() {
    let mut fixture = Fixture::new();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    fixture.start(port);
    fixture.wait_healthy(port);
    fixture.kill();
    assert!(fixture.root.join("handover.sock").exists());
    // Replay exactly the same arguments, as launchd does after a crash.
    fixture.start(port);
    fixture.wait_healthy(port);
}

#[test]
fn takeover_cold_bind_failure_preserves_shared_sockets() {
    let mut fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let path = fixture.root.join("handover.sock");
    drop(UnixListener::bind(&path).unwrap());
    let inode = fs::metadata(&path).unwrap().ino();
    fixture.start(occupied.local_addr().unwrap().port());
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = fixture.child.as_mut().unwrap().try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "startup did not fail promptly");
        thread::sleep(Duration::from_millis(25));
    };
    assert!(!status.success());
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert!(fs::read_to_string(fixture.root.join("stderr"))
        .unwrap()
        .contains("failed to bind"));
}

#[test]
fn takeover_connects_to_live_predecessor_and_rejects_other_errors() {
    let fixture = Fixture::new();
    let listener = UnixListener::bind(fixture.root.join("handover.sock")).unwrap();
    assert!(sm_server::handover::connect_serving(&fixture.root)
        .unwrap()
        .is_some());
    drop(listener);
    let file = fixture.root.join("not-a-directory");
    fs::write(&file, "x").unwrap();
    assert!(sm_server::handover::connect_serving(&file).is_err());
}
