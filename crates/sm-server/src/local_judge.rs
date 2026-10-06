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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LocalJudgeConfig {
    pub root_dir: String,
    pub port: u16,
    pub proxy_port: u16,
    pub timeout_seconds: u64,
    pub python: String,
}
impl Default for LocalJudgeConfig {
    fn default() -> Self {
        Self {
            root_dir: "~/.local/share/claude-sessions/local-judge".into(),
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
}
impl LocalJudgeRuntime {
    pub fn from_config(config: &crate::config::AppConfig) -> Self {
        Self {
            config: config.local_judge.clone(),
            root: crate::sessions::expand_home(&config.local_judge.root_dir),
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
    pub fn ensure_running(&self) -> Result<()> {
        if !self.root.is_absolute() {
            bail!("local_judge.root_dir must be absolute (or start with ~/)");
        }
        if self.control(json!({"op": "health"})).is_ok() {
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
        if self.control(json!({"op": "health"})).is_ok() {
            return Ok(());
        }
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
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("service.log"))?;
        let mut command = Command::new(&self.config.python);
        command
            .arg(service)
            .arg("--root")
            .arg(&self.root)
            .arg("--port")
            .arg(self.config.port.to_string())
            .arg("--proxy-port")
            .arg(self.config.proxy_port.to_string())
            .arg("--timeout")
            .arg(self.config.timeout_seconds.to_string())
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
            if self.control(json!({"op": "health"})).is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        bail!(
            "local judge did not become ready; see {}",
            self.root.join("service.log").display()
        )
    }

    pub fn register(&self, session_id: &str, agent: &Registration) -> Result<JudgeEndpoint> {
        self.ensure_running()?;
        serde_json::from_value(
            self.control(json!({"op": "register", "session_id": session_id, "agent": agent}))?,
        )
        .context("judge endpoint")
    }

    pub fn unregister(&self, session_id: &str) -> Result<()> {
        self.ensure_running()?;
        self.control(json!({"op": "unregister", "session_id": session_id}))?;
        Ok(())
    }

    /// Owner-only host interface. Never wire this to an agent-facing endpoint.
    pub fn allow_once(&self, denial_id: &str) -> Result<Value> {
        self.ensure_running()?;
        self.control(json!({"op": "allow", "denial_id": denial_id}))
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
    #[test]
    fn runtime_register_restart_reconcile_and_unregister() {
        let root = PathBuf::from(format!("/tmp/j77-r-{}", std::process::id()));
        fs::create_dir_all(root.join("wt")).unwrap();
        fs::create_dir_all(root.join("tmp")).unwrap();
        let mut config = crate::config::AppConfig::default();
        config.local_judge.root_dir = root.join("state").to_string_lossy().into();
        config.local_judge.port = 0;
        let runtime = LocalJudgeRuntime::from_config(&config);
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
        let restarted = LocalJudgeRuntime::from_config(&config);
        restarted.reconcile().unwrap();
        assert_eq!(
            restarted.control(json!({"op": "health"})).unwrap()["pid"],
            pid
        );
        assert_eq!(
            restarted.register("a", &agent).unwrap().token,
            endpoint.token
        );
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
        restarted.unregister("a").unwrap();
        restarted.unregister("b").unwrap();
        assert!(restarted
            .control(json!({"op": "registrations"}))
            .unwrap()
            .as_object()
            .unwrap()
            .is_empty());
    }
}
