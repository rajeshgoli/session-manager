//! Real key-only, non-root SSH, including selective live revocation.
use sm_server::{
    config::StudioSshConfig,
    device_ssh::Store,
    studio_ssh::transport::{self, Runtime},
};
use std::{
    fs,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    dir: PathBuf,
    children: Vec<Child>,
    port: u16,
    user: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}
impl Fixture {
    fn new() -> Self {
        let dir = PathBuf::from(format!(
            "/tmp/sm-2334-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        Self {
            dir,
            children: Vec::new(),
            port,
            user,
        }
    }
    fn key(&self, name: &str) -> String {
        assert!(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
            .arg(self.dir.join(name))
            .status()
            .unwrap()
            .success());
        fs::read_to_string(self.dir.join(format!("{name}.pub")))
            .unwrap()
            .trim()
            .into()
    }
    fn ssh(&self, name: &str, command: &str) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "ConnectTimeout=2",
            "-p",
            &self.port.to_string(),
            "-i",
        ])
        .arg(self.dir.join(name))
        .arg(format!("{}@127.0.0.1", self.user))
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        cmd
    }
}

#[test]
fn device_keys_real_sshd_selective_revocation_restart_and_linger() {
    let mut f = Fixture::new();
    f.key("host");
    let a = f.key("phone");
    let b = f.key("laptop");
    f.key("unregistered");
    let cfg = StudioSshConfig {
        assets_dir: f.dir.display().to_string(),
        device_keys_file: f.dir.join("device_keys").display().to_string(),
        local_sshd_port: f.port,
        ..StudioSshConfig::default()
    };
    let store = Store::new(f.dir.join("usage.db"));
    store.ensure_schema().unwrap();
    let a = store.add("phone", &a).unwrap();
    store.add("laptop", &b).unwrap();
    assert!(store.add("another-device", &a.public_key).is_err());
    store
        .project(&PathBuf::from(&cfg.device_keys_file))
        .unwrap();
    assert_eq!(
        fs::metadata(&cfg.device_keys_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(fs::read_to_string(&cfg.device_keys_file)
        .unwrap()
        .lines()
        .all(|l| l.starts_with("restrict,pty ssh-ed25519 ")));
    fs::write(f.dir.join("sshd_config"),format!("Port {}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nAuthorizedKeysFile /must/not/read/authorized_keys\nAllowUsers {}\nPermitRootLogin no\nUsePAM no\nStrictModes no\nPerSourcePenalties no\nLogLevel VERBOSE\n",f.port,f.dir.join("host").display(),f.dir.join("sshd.pid").display(),f.user)).unwrap();
    assert!(transport::configure(&cfg, None).unwrap());
    assert!(!transport::configure(&cfg, None).unwrap());
    let server = Command::new("/usr/sbin/sshd")
        .args(["-D", "-E"])
        .arg(f.dir.join("sshd.err.log"))
        .arg("-f")
        .arg(f.dir.join("sshd_config"))
        .spawn()
        .unwrap();
    f.children.push(server);
    let start = Instant::now();
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", f.port)).is_ok() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "sshd failed: {}",
            fs::read_to_string(f.dir.join("sshd.err.log")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(30));
    }
    let login = f
        .ssh("phone", "true")
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        login.status.success(),
        "client: {}\nsshd: {}",
        String::from_utf8_lossy(&login.stderr),
        fs::read_to_string(f.dir.join("sshd.err.log")).unwrap()
    );
    assert!(!f.ssh("unregistered", "true").status().unwrap().success());
    // Options must precede the host, so use a fresh client for the password-only denial.
    assert!(!Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-p",
            &f.port.to_string()
        ])
        .arg(format!("{}@127.0.0.1", f.user))
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success());
    let first = f.ssh("phone", "sleep 90").spawn().unwrap();
    f.children.push(first);
    let second = f.ssh("laptop", "sleep 90").spawn().unwrap();
    f.children.push(second);
    let mut runtime = Runtime::default();
    runtime.requested_at = Some(100);
    let start = Instant::now();
    loop {
        runtime.read_log(&cfg, 101).unwrap();
        if runtime.connections.len() == 2 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "No PID-tagged accepted logins: {}",
            fs::read_to_string(f.dir.join("sshd.err.log")).unwrap()
        );
        thread::sleep(Duration::from_millis(30));
    }
    assert!(runtime.connections.values().all(|c| c.away));
    runtime.save(&cfg).unwrap();
    runtime = Runtime::load(&cfg).unwrap();
    assert_eq!(runtime.connections.len(), 2);
    assert!(
        runtime.tunnel_needed(10000, false, true, 1),
        "active away login holds the tunnel"
    );
    let start = Instant::now();
    store.revoke("phone", Some(&a.fingerprint)).unwrap();
    store
        .project(&PathBuf::from(&cfg.device_keys_file))
        .unwrap();
    runtime.revoke(&store.list().unwrap()).unwrap();
    while f.children[1].try_wait().unwrap().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "Revoked key still connected"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        f.children[2].try_wait().unwrap().is_none(),
        "Other device was disconnected"
    );
    assert!(!f.ssh("phone", "true").status().unwrap().success());
    assert!(f.ssh("laptop", "true").status().unwrap().success());
    assert!(!fs::read_to_string(&cfg.device_keys_file)
        .unwrap()
        .contains(&a.public_key));
    f.children[2].kill().unwrap();
    f.children[2].wait().unwrap();
    let start = Instant::now();
    loop {
        runtime.read_log(&cfg, 10001).unwrap();
        if runtime.connections.is_empty() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(20));
    }
    assert!(runtime.tunnel_needed(10060, false, true, 1));
    assert!(!runtime.tunnel_needed(10061, false, true, 1));
    runtime.requested_at = Some(11000);
    assert!(
        !runtime.tunnel_needed(11001, false, false, 15),
        "last shell closes tunnel immediately"
    );
    assert!(
        runtime.tunnel_needed(11001, true, false, 15),
        "manual override survives kill switch"
    );
}
