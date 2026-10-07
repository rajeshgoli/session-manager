//! Harness-independent judge runtime. Call `register` before launching a local
//! agent and `unregister` on final retirement or failed launch. No HTTP owner
//! writes are exposed here: the Unix control socket is outside every agent wall.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalJudgeConfig {
    pub port: u16,
    pub proxy_port: u16,
    pub timeout_seconds: u64,
    pub python: String,
}
impl Default for LocalJudgeConfig {
    fn default() -> Self {
        Self {
            port: 8431,
            proxy_port: 8432,
            timeout_seconds: 30,
            python: "python3".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub name: String,
    pub ticket: i64,
    pub title: String,
    pub branch: String,
    pub checkout: PathBuf,
    pub tmp: PathBuf,
    pub parent: String,
    pub sm_url: String,
    /// Assigned listener from the host local-egress registration.
    pub proxy_port: u16,
}

/// Provider hook configuration returned only to the host at registration.
/// The hook sends both X-Local-Agent and X-Local-Judge-Token with each request.
#[derive(Debug, Clone, Deserialize)]
pub struct JudgeEndpoint {
    pub url: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct LocalJudgeRuntime {
    config: LocalJudgeConfig,
    root: PathBuf,
    model_url: String,
    model_auth_token: String,
}
impl LocalJudgeRuntime {
    #[cfg(test)]
    pub(crate) fn isolated(config: &crate::config::AppConfig, root: PathBuf) -> Self {
        let mut runtime = Self::from_config(config);
        runtime.root = root;
        runtime
    }

    pub fn from_config(config: &crate::config::AppConfig) -> Self {
        // The production location is part of the durable-state contract, not
        // a configurable per-deployment path. Test launchers isolate it only.
        let root = std::env::var_os("SM_TEST_ISOLATION_ROOT")
            .map(|root| PathBuf::from(root).join("local-judge"))
            .unwrap_or_else(|| {
                crate::sessions::expand_home("~/.local/share/claude-sessions/local-judge")
            });
        Self {
            config: config.local_judge.clone(),
            model_auth_token: config.local_host.auth_token.clone(),
            root,
            model_url: config
                .local_host
                .base_url
                .trim_end_matches("/v1")
                .trim_end_matches('/')
                .into(),
        }
    }

    fn control(&self, request: Value) -> Result<Value> {
        let mut socket = UnixStream::connect(self.root.join("control.sock"))?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        serde_json::to_writer(&mut socket, &request)?;
        socket.write_all(b"\n")?;
        let mut response = String::new();
        BufReader::new(socket).take_line(&mut response)?;
        let response: Value = serde_json::from_str(&response)?;
        if let Some(error) = response.get("error") {
            bail!("local judge: {error}");
        }
        response
            .get("result")
            .cloned()
            .context("missing judge result")
    }

    /// Idempotent across concurrent starts and sm blue/green handover. The
    /// daemon's lifetime flock selects one process, not a remembered PID.
    fn generation(&self, port: u16) -> String {
        use sha2::{Digest, Sha256};
        let identity = json!({
            "service": include_str!("../../../scripts/local-judge/service.py"),
            "policy": include_str!("../../../scripts/local-judge/policy.md"),
            "config": self.config, "model_url": self.model_url,
            "model_auth_token": self.model_auth_token, "effective_port": port,
        });
        format!("{:x}", Sha256::digest(identity.to_string().as_bytes()))
    }

    fn effective_port(&self) -> Result<u16> {
        let agents = self.root.join("agents.json");
        if agents.exists() {
            let registrations: serde_json::Map<String, Value> =
                serde_json::from_slice(&fs::read(agents)?)?;
            if !registrations.is_empty() {
                let endpoint = self.root.join("endpoint.json");
                let saved: Value = if endpoint.exists() {
                    serde_json::from_slice(&fs::read(endpoint)?)?
                } else {
                    self.control(json!({"op": "health"}))
                        .context("active judge is missing its saved endpoint")?
                };
                let port = saved["port"]
                    .as_u64()
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port != 0)
                    .context("invalid saved judge port")?;
                return Ok(port);
            }
        }
        Ok(self.config.port)
    }

    fn save_endpoint(&self, port: u16) -> Result<()> {
        let staged = self.root.join("endpoint.json.new");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&staged)?;
        serde_json::to_writer(&mut file, &json!({"port": port}))?;
        file.sync_all()?;
        fs::rename(staged, self.root.join("endpoint.json"))?;
        fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    fn matching_service(&self, generation: &str) -> bool {
        self.control(json!({"op": "health"}))
            .is_ok_and(|health| health["generation"] == generation)
    }

    pub fn ensure_running(&self) -> Result<()> {
        if !self.root.is_absolute() {
            bail!("local judge test isolation root must be absolute");
        }
        if self.config.port == 0 {
            bail!("local_judge.port must be nonzero so existing hook URLs survive recovery");
        }
        let generation = self.generation(self.effective_port()?);
        if self.matching_service(&generation) {
            return Ok(());
        }
        if self.config.timeout_seconds == 0 || self.config.timeout_seconds > 30 {
            bail!("local_judge.timeout_seconds must be between 1 and 30");
        }
        fs::create_dir_all(&self.root)?;
        fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        // Serialize source installation and process readiness across host
        // threads and overlapping sm instances. flock releases on host death.
        use std::os::fd::AsRawFd;
        let startup_lock = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("startup.lock"))?;
        if unsafe { libc::flock(startup_lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let generation = self.generation(self.effective_port()?);
        if self.matching_service(&generation) {
            return Ok(());
        }
        if self.control(json!({"op": "health"})).is_ok() {
            self.control(json!({"op": "shutdown"}))
                .context("drain outdated local judge")?;
        }
        // A draining/idle daemon may already have unlinked its socket. Wait
        // on its lifetime lock, never signal a self-reported PID. The existing
        // model deadline bounds drain; failures remain closed to new actions.
        let lifetime = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("service.lock"))?;
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if unsafe { libc::flock(lifetime.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error.into());
            }
            if Instant::now() >= deadline {
                bail!("local judge did not finish draining");
            }
            thread::sleep(Duration::from_millis(25));
        }
        drop(lifetime);
        // Recompute after draining: a registration may have won admission
        // just before shutdown. Active hook URLs retain their durable port.
        let port = self.effective_port()?;
        self.save_endpoint(port)?;
        let generation = self.generation(port);
        // Content-addressed installed sources outlive temporary worktrees and
        // cannot be overwritten under a running Python process by a new build.
        let service = install_source(
            &self.root,
            "service",
            include_str!("../../../scripts/local-judge/service.py"),
            "py",
        )?;
        let policy = install_source(
            &self.root,
            "policy",
            include_str!("../../../scripts/local-judge/policy.md"),
            "md",
        )?;
        let connection = install_source(
            &self.root,
            "connection",
            &json!({"auth_token": self.model_auth_token}).to_string(),
            "json",
        )?;
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("service.log"))?;
        let mut command = Command::new(&self.config.python);
        command
            .arg(service)
            .arg("--generation")
            .arg(&generation)
            .arg("--root")
            .arg(&self.root)
            .arg("--port")
            .arg(port.to_string())
            .arg("--proxy-port")
            .arg(self.config.proxy_port.to_string())
            .arg("--timeout")
            .arg(self.config.timeout_seconds.to_string())
            .arg("--model-auth-file")
            .arg(connection)
            .arg("--model-url")
            .arg(&self.model_url)
            .arg("--policy")
            .arg(policy)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        // Detach from sm's process group. A restart may signal the old group.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("start local judge")?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.matching_service(&generation) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        bail!(
            "local judge did not become ready; see {}",
            self.root.join("service.log").display()
        )
    }

    fn ensure_and_control(&self, request: Value) -> Result<Value> {
        let mut last_error = None;
        // Idle shutdown or replacement can begin after a readiness check.
        // All control writes used here are idempotent, including spent grants.
        for _ in 0..3 {
            if let Err(error) = self.ensure_running() {
                last_error = Some(error);
                continue;
            }
            match self.control(request.clone()) {
                Ok(result) => return Ok(result),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.expect("control attempted"))
    }

    pub fn directory(&self) -> &std::path::Path {
        &self.root
    }

    /// Authoritative saved port, including registrations surviving a config change.
    pub fn port(&self) -> Result<u16> {
        self.ensure_running()?;
        self.effective_port()
    }

    pub fn register(&self, session_id: &str, agent: &Registration) -> Result<JudgeEndpoint> {
        serde_json::from_value(self.ensure_and_control(
            json!({"op": "register", "session_id": session_id, "agent": agent}),
        )?)
        .context("judge endpoint")
    }

    pub fn unregister(&self, session_id: &str) -> Result<()> {
        self.ensure_and_control(json!({"op": "unregister", "session_id": session_id}))?;
        Ok(())
    }

    /// Owner-only host interface. Never wire this to an agent-facing endpoint.
    pub fn allow_once(&self, denial_id: &str) -> Result<Value> {
        self.ensure_and_control(json!({"op": "allow", "denial_id": denial_id}))
    }

    /// Restart recovery: registrations, grants and logs are daemon-owned and
    /// durable. Pending pre-launch registrations are retained, even before a
    /// provider/session record exists. Explicit unregister rolls those back.
    pub fn reconcile(&self) -> Result<()> {
        let agents_path = self.root.join("agents.json");
        if agents_path.exists() {
            let agents: serde_json::Map<String, Value> =
                serde_json::from_slice(&fs::read(agents_path)?)?;
            if !agents.is_empty() {
                self.ensure_running()?;
            }
        }
        Ok(())
    }
}

// Bounded line parsing keeps a faulty control peer from growing sm's memory.
trait TakeLine: BufRead {
    fn take_line(&mut self, output: &mut String) -> std::io::Result<()> {
        let mut bytes = Vec::new();
        self.take(2_000_001).read_until(b'\n', &mut bytes)?;
        if bytes.len() > 2_000_000 {
            return Err(std::io::Error::other("judge response too large"));
        }
        *output = String::from_utf8(bytes).map_err(std::io::Error::other)?;
        Ok(())
    }
}
impl<T: BufRead> TakeLine for T {}

fn install_source(
    root: &std::path::Path,
    stem: &str,
    content: &str,
    extension: &str,
) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
    let path = root.join(format!("{stem}-{digest}.{extension}"));
    if path.exists() {
        if fs::read(&path)? != content.as_bytes() {
            bail!("judge source mismatch: {}", path.display());
        }
    } else {
        // Caller holds startup.lock. A crashed install leaves only a staging
        // file, never a partial content-addressed source used at next startup.
        let staged = root.join(format!(".install-{stem}"));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&staged)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(staged, &path)?;
        fs::File::open(root)?.sync_all()?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn runtime_for(config: &crate::config::AppConfig, root: &std::path::Path) -> LocalJudgeRuntime {
        let mut runtime = LocalJudgeRuntime::from_config(config);
        runtime.root = root.join("state");
        runtime
    }

    #[test]
    fn judge_state_root_cannot_change_through_configuration() {
        assert!(serde_yaml::from_str::<LocalJudgeConfig>("root_dir: /tmp/another-root").is_err());
        assert!(
            serde_yaml::from_str::<LocalJudgeConfig>("port: 8431\ntimeout_seconds: 30").is_ok()
        );
    }

    #[test]
    fn ephemeral_judge_ports_are_rejected() {
        let mut config = crate::config::AppConfig::default();
        config.local_judge.port = 0;
        assert!(LocalJudgeRuntime::from_config(&config)
            .ensure_running()
            .unwrap_err()
            .to_string()
            .contains("nonzero"));
    }

    #[test]
    fn runtime_register_restart_reconcile_and_unregister() {
        let root = PathBuf::from(format!("/tmp/j77-r-{}", std::process::id()));
        fs::create_dir_all(root.join("wt")).unwrap();
        fs::create_dir_all(root.join("tmp")).unwrap();
        let mut config = crate::config::AppConfig::default();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        config.local_judge.port = port.local_addr().unwrap().port();
        drop(port);
        let runtime = runtime_for(&config, &root);
        struct Cleanup(LocalJudgeRuntime, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Ok(health) = self.0.control(json!({"op": "health"})) {
                    if let Some(pid) = health["pid"].as_i64() {
                        unsafe {
                            libc::kill(pid as i32, libc::SIGTERM);
                        }
                    }
                }
                let _ = fs::remove_dir_all(&self.1);
            }
        }
        let _cleanup = Cleanup(runtime.clone(), root.clone());
        // Two concurrent callers must see one ready process, including when
        // the embedded sources do not yet exist on disk.
        let calls: Vec<_> = (0..4)
            .map(|_| {
                let runtime = runtime.clone();
                thread::spawn(move || runtime.ensure_running().unwrap())
            })
            .collect();
        for call in calls {
            call.join().unwrap();
        }
        let agent = Registration {
            name: "test-local".into(),
            ticket: 1977,
            title: "judge".into(),
            branch: "1977-local".into(),
            checkout: root.join("wt"),
            tmp: root.join("tmp"),
            parent: "parent".into(),
            sm_url: "http://127.0.0.1:8420".into(),
            proxy_port: 18700,
        };
        let endpoint = runtime.register("a", &agent).unwrap();
        runtime.register("b", &agent).unwrap();
        assert_eq!(
            runtime
                .control(json!({"op": "registrations"}))
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            2
        );
        let pid = runtime.control(json!({"op": "health"})).unwrap()["pid"]
            .as_i64()
            .unwrap();
        // Reconstructing sm's runtime keeps the existing daemon, its process
        // identity and its registration token.
        let restarted = runtime_for(&config, &root);
        restarted.reconcile().unwrap();
        assert_eq!(
            restarted.control(json!({"op": "health"})).unwrap()["pid"],
            pid
        );
        assert_eq!(
            restarted.register("a", &agent).unwrap().token,
            endpoint.token
        );
        assert_eq!(restarted.register("a", &agent).unwrap().url, endpoint.url);
        // Daemon crash: the next reconcile rebinds using durable registrations.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while runtime.control(json!({"op": "health"})).is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        restarted.reconcile().unwrap();
        assert_eq!(
            restarted.register("a", &agent).unwrap().token,
            endpoint.token
        );
        // Configuration changes replace a healthy detached process; this is
        // also the path used when the embedded service or policy hash changes.
        assert_eq!(restarted.register("a", &agent).unwrap().url, endpoint.url);
        let old_pid = restarted.control(json!({"op": "health"})).unwrap()["pid"].clone();
        let mut changed_config = config.clone();
        changed_config.local_judge.timeout_seconds = 29;
        changed_config.local_judge.proxy_port = 8433;
        changed_config.local_host.base_url = "http://127.0.0.1:8001".into();
        changed_config.local_host.auth_token = "fixture-upgraded-key".into();
        let original_port = changed_config.local_judge.port;
        changed_config.local_judge.port = if original_port == 60000 { 60001 } else { 60000 };
        let upgraded = runtime_for(&changed_config, &root);
        upgraded.reconcile().unwrap();
        let health = upgraded.control(json!({"op": "health"})).unwrap();
        assert_ne!(health["pid"], old_pid);
        assert_eq!(health["port"], original_port);
        assert_eq!(upgraded.register("a", &agent).unwrap().url, endpoint.url);
        assert_eq!(
            health["generation"],
            upgraded.generation(upgraded.effective_port().unwrap())
        );
        assert_eq!(
            upgraded.register("a", &agent).unwrap().token,
            endpoint.token
        );
        assert_eq!(
            upgraded
                .control(json!({"op": "registrations"}))
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            2
        );
        upgraded.unregister("a").unwrap();
        upgraded.unregister("b").unwrap();
        // Kill the empty daemon, then simulate an idle-shutdown response
        // precisely between ensure's successful health check and register.
        upgraded.control(json!({"op": "shutdown"})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while root.join("state/control.sock").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        let socket = root.join("state/control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let generation = upgraded.generation(upgraded.effective_port().unwrap());
        let stub = thread::spawn(move || {
            for result in [
                json!({"result": {"generation": generation}}),
                json!({"error": "judge stopping"}),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                serde_json::to_writer(&mut stream, &result).unwrap();
                stream.write_all(b"\n").unwrap();
            }
            fs::remove_file(socket).unwrap();
        });
        upgraded.register("a", &agent).unwrap();
        stub.join().unwrap();
        upgraded.unregister("a").unwrap();
        // The generation-mismatch shutdown request can itself race idle
        // shutdown. Readiness failure must also retry the whole operation.
        upgraded.control(json!({"op": "shutdown"})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while root.join("state/control.sock").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        let socket = root.join("state/control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let stub = thread::spawn(move || {
            for result in [
                json!({"result": {"generation": "outdated"}}),
                json!({"result": {"generation": "outdated"}}),
                json!({"result": {"generation": "outdated"}}),
                json!({"error": "judge stopping"}),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                serde_json::to_writer(&mut stream, &result).unwrap();
                stream.write_all(b"\n").unwrap();
            }
            fs::remove_file(socket).unwrap();
        });
        upgraded.register("a", &agent).unwrap();
        stub.join().unwrap();
        upgraded.unregister("a").unwrap();
        // Final retirement must also survive a shutdown between readiness
        // and its control write, leaving no durable orphan registration.
        upgraded.register("a", &agent).unwrap();
        upgraded.control(json!({"op": "shutdown"})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while root.join("state/control.sock").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        let socket = root.join("state/control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let generation = upgraded.generation(upgraded.effective_port().unwrap());
        let stub = thread::spawn(move || {
            for result in [
                json!({"result": {"generation": generation}}),
                json!({"error": "judge stopping"}),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                serde_json::to_writer(&mut stream, &result).unwrap();
                stream.write_all(b"\n").unwrap();
            }
            fs::remove_file(socket).unwrap();
        });
        upgraded.unregister("a").unwrap();
        stub.join().unwrap();
        assert!(upgraded
            .control(json!({"op": "registrations"}))
            .unwrap()
            .as_object()
            .unwrap()
            .is_empty());
    }
}
