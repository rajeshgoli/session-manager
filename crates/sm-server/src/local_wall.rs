//! Host-only production wall preparation. No request handler accepts these
//! structures from an agent. Queue admission consumes this authority in #1974.

use crate::{
    local_egress::{Registration as EgressRegistration, ServiceClient},
    local_judge::{JudgeEndpoint, LocalJudgeRuntime, Registration as JudgeRegistration},
    local_sockets::{
        launch::{LaunchBinding, RegisteredChild},
        pool::{PortConfiguration, PortPolicy},
        service::BrokerHub,
        IpVersion,
    },
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    net::SocketAddr,
    ops::RangeInclusive,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{ChildStderr, ChildStdout, Command, ExitStatus},
    sync::{Arc, Mutex},
};

/// All values come from sm's host configuration, never a tool or queue body.
#[derive(Clone, PartialEq, Eq)]
pub struct HostConfiguration {
    pub home: PathBuf,
    pub state_root: PathBuf,
    pub alias_root: PathBuf,
    pub python: PathBuf,
    pub read_only_roots: Vec<PathBuf>,
    pub executable_roots: Vec<PathBuf>,
    pub control_ports: RangeInclusive<u16>,
    pub model_port: u16,
    pub sm_upstream: SocketAddr,
}

/// The host selects sources; copies are independently staged and ad-hoc signed.
#[derive(Serialize)]
pub struct StageTool {
    pub name: String,
    pub source: PathBuf,
}

#[derive(Serialize)]
pub struct AgentRegistration {
    pub id: String,
    pub name: String,
    pub ticket: i64,
    pub title: String,
    pub branch: String,
    pub checkout: PathBuf,
    pub parent: String,
    pub control_port: u16,
    pub tools: Vec<StageTool>,
}

#[derive(Debug, Deserialize)]
pub struct WallArtifacts {
    pub profile: PathBuf,
    pub adapter: PathBuf,
    pub supervisor: PathBuf,
    pub tmp: PathBuf,
    pub gh: PathBuf,
    pub cargo: PathBuf,
    pub executables: PathBuf,
    pub tools: BTreeMap<String, PathBuf>,
}

pub struct LocalWallRuntime {
    config: HostConfiguration,
    egress: ServiceClient,
    judge: LocalJudgeRuntime,
    judge_port: u16,
    hub: BrokerHub,
}

impl LocalWallRuntime {
    pub fn new(
        config: HostConfiguration,
        egress: ServiceClient,
        judge: LocalJudgeRuntime,
    ) -> Result<Self> {
        physical_directory(&config.home)?;
        if config.home == Path::new("/")
            || config.state_root == config.home
            || !config.state_root.starts_with(&config.home)
        {
            bail!("agent state root must be a narrow directory inside the physical home");
        }
        if config.alias_root == Path::new("/private/tmp")
            || !config.alias_root.starts_with("/private/tmp")
            || config.alias_root.as_os_str().as_encoded_bytes().len() + 19 >= 104
        {
            bail!("broker aliases require a short private directory below /private/tmp");
        }
        private_directory(&config.state_root)?;
        private_directory(&config.alias_root)?;
        let judge_port = judge.port()?;
        let policy = PortPolicy::new(PortConfiguration {
            agent_control: config.control_ports.clone(),
            gateway: crate::local_egress::gateway::FIRST_PORT
                ..=crate::local_egress::gateway::LAST_PORT,
            egress: crate::local_egress::FIRST_PORT..=crate::local_egress::LAST_PORT,
            model: config.model_port,
            judge: judge_port,
        })?;
        Ok(Self {
            config,
            egress,
            judge,
            judge_port,
            hub: BrokerHub::new(policy),
        })
    }

    /// Call only after all previous launches using this state have exited.
    /// A live broker/host preparation lock causes refusal, never replacement.
    pub fn prepare(&self, agent: &AgentRegistration) -> Result<Arc<PreparedWall>> {
        self.prepare_inner(agent, None)
    }

    /// The caller pauses queue admission during preparation and restoration.
    /// Existing pending jobs retain their original immutable registration.
    pub fn prepare_for_queue(
        &self,
        agent: &AgentRegistration,
        queue_state: &Path,
    ) -> Result<Arc<PreparedWall>> {
        private_directory(queue_state)?;
        crate::queue::local_wall::ensure_no_running_jobs(queue_state, &agent.id)?;
        self.prepare_inner(agent, Some(queue_state))
    }

    fn prepare_inner(
        &self,
        agent: &AgentRegistration,
        queue_state: Option<&Path>,
    ) -> Result<Arc<PreparedWall>> {
        if agent.id.is_empty()
            || agent.id.len() > 64
            || !agent
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
        {
            bail!("invalid registered local agent id");
        }
        physical_directory(&agent.checkout)?;
        if self.config.alias_root.starts_with(&agent.checkout) {
            bail!("broker alias directory must be outside the writable checkout");
        }
        let state = self.config.state_root.join(&agent.id);
        for name in ["xdg/config", "xdg/data", "xdg/cache", "xdg/state", "tmp/b"] {
            private_directory(&state.join(name))?;
        }
        let config = state.join("xdg/config");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(config.join("host.lock"))?;
        if lock.metadata()?.nlink() != 1
            || unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
        {
            bail!("agent wall is already prepared or its lock has aliases");
        }
        let saved = queue_state
            .map(|root| crate::queue::local_wall::registered_spec(root, &agent.id))
            .transpose()?
            .flatten();
        let broker = state.join("tmp/b");
        let alias_name = format!("{:x}", Sha256::digest(agent.id.as_bytes()));
        let alias = self.config.alias_root.join(&alias_name[..16]);
        if alias.is_symlink() {
            if fs::read_link(&alias)? != broker {
                bail!("unexpected broker alias target");
            }
        } else {
            std::os::unix::fs::symlink(&broker, &alias)?;
        }
        let endpoint = alias.join("s");
        remove_stale_socket(&endpoint)?;
        let service =
            Arc::new(
                self.hub
                    .register_agent(&agent.id, agent.control_port, &broker, &endpoint)?,
            );
        let control = service.control_listener(IpVersion::V4)?;
        let result = (|| {
            // A lost reply can hide a successful durable registration. Include
            // the call itself in rollback, retaining its assigned ports.
            let egress = self
                .egress
                .register_gateway(&agent.id, self.config.sm_upstream)?;
            let temporary = run_python(
                &self.config.python,
                &install_sources(&config)?.join("wall_profile.py"),
                &[
                    OsString::from("--prepare-tmp"),
                    state.as_os_str().to_owned(),
                ],
            )?;
            let tmp = PathBuf::from(String::from_utf8(temporary)?.trim());
            let registration = JudgeRegistration {
                name: agent.name.clone(),
                ticket: agent.ticket,
                title: agent.title.clone(),
                branch: agent.branch.clone(),
                checkout: agent.checkout.clone(),
                tmp,
                parent: agent.parent.clone(),
                sm_url: format!("http://{}", self.config.sm_upstream),
                proxy_port: egress.port,
            };
            let judge = if let Some(saved) = &saved {
                self.judge.register_restored(
                    &agent.id,
                    &registration,
                    saved
                        .environment
                        .get("LOCAL_JUDGE_TOKEN")
                        .context("saved judge credential missing")?,
                )?
            } else {
                self.judge.register(&agent.id, &registration)?
            };
            if loopback_port(&judge.url)? != self.judge_port {
                bail!("judge endpoint changed during preparation");
            }
            let gateway = egress
                .gateway
                .as_ref()
                .context("missing registered sm gateway")?;
            let mut tools = serde_json::to_value(&agent.tools)?;
            if queue_state.is_some() {
                if agent.tools.iter().any(|tool| tool.name == "queue-zsh") {
                    bail!("queue-zsh is reserved for the host shell");
                }
                tools
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"name": "queue-zsh", "source": "/bin/zsh"}));
            }
            let mut service_roots = vec![self.egress.directory(), self.judge.directory()];
            if let Some(root) = queue_state {
                service_roots.push(root);
            }
            let mut request = json!({
                "home": self.config.home, "state_root": self.config.state_root, "state": state,
                "checkout": agent.checkout, "broker_dir": broker, "endpoint": endpoint,
                "peer_token": service.peer_token().0, "tools": tools,
                "ports": {"agent": agent.control_port, "gateway": gateway.port, "egress": egress.port,
                    "model": self.config.model_port, "judge": self.judge_port},
                "ranges": {"agent": format!("{}-{}", self.config.control_ports.start(), self.config.control_ports.end()),
                    "gateway": "18600-18699", "egress": "18700-18799"},
                "service_roots": service_roots,
                "read_only_roots": self.config.read_only_roots, "executable_roots": self.config.executable_roots,
            });
            let mut authority = request.clone();
            authority.as_object_mut().unwrap().remove("peer_token");
            authority["registration"] = serde_json::to_value(agent)?;
            // Pin trusted source contents, not just their locations.
            let mut hashes = BTreeMap::new();
            for tool in tools.as_array().unwrap() {
                let source = tool["source"].as_str().unwrap();
                hashes.insert(source, format!("{:x}", Sha256::digest(fs::read(source)?)));
            }
            authority["tool_hashes"] = serde_json::to_value(hashes)?;
            authority["generator_sha256"] = json!(format!(
                "{:x}",
                Sha256::digest(include_bytes!(
                    "../../../scripts/local-wall/wall_profile.py"
                ))
            ));
            let authority = serde_json::to_vec(&authority)?;
            let authority_path = config.join("queue-authority.json");
            if let Some(saved) = &saved {
                if saved.agent_state != state
                    || saved.checkout != agent.checkout
                    || saved.profile != config.join("wall.sb")
                    || saved.shell != config.join("executables/queue-zsh")
                    || fs::read(&authority_path)? != authority
                {
                    bail!("saved queue authority differs from current host configuration");
                }
                request["preserve_profile"] = json!(true);
            }
            let request_path = config.join("preparation.json");
            atomic_write(&request_path, serde_json::to_vec(&request)?.as_slice())?;
            let output = run_python(
                &self.config.python,
                &config.join("sources/prepare.py"),
                &[request_path.as_os_str().to_owned()],
            );
            fs::remove_file(request_path)?;
            let artifacts: WallArtifacts = serde_json::from_slice(&output?)?;
            let environment = environment(&self.config, agent, &state, &artifacts, &egress, &judge);
            let provider = LaunchBinding::new(
                service.clone(),
                Some(control),
                &artifacts.profile,
                &artifacts.adapter,
                &artifacts.supervisor,
            )?;
            let queue = Arc::new(LaunchBinding::new(
                service.clone(),
                None,
                &artifacts.profile,
                &artifacts.adapter,
                &artifacts.supervisor,
            )?);
            if let Some(root) = queue_state {
                // Persist host authority before publishing a manifest that can
                // admit commands. An interrupted first prepare can then retry.
                atomic_write(&authority_path, &authority)?;
                crate::queue::local_wall::register(
                    root,
                    &agent.id,
                    crate::queue::local_wall::WallSpec {
                        host_authority_sha256: Some(format!("{:x}", Sha256::digest(&authority))),
                        agent_state: state,
                        checkout: agent.checkout.clone(),
                        profile: artifacts.profile.clone(),
                        shell: artifacts.tools["queue-zsh"].clone(),
                        environment: environment.clone(),
                        gateway_port: gateway.port,
                        egress_port: egress.port,
                    },
                )?;
                crate::queue::local_wall::attach(root, &agent.id, queue.clone())?;
            }
            Ok(Arc::new(PreparedWall {
                artifacts,
                environment,
                provider,
                queue,
                queue_state: queue_state.map(Path::to_path_buf),
                broker_peer: service.peer_token(),
                checkout: agent.checkout.clone(),
                id: agent.id.clone(),
                egress: self.egress.clone(),
                judge: self.judge.clone(),
                _lock: lock,
                life: Mutex::new(Lifecycle {
                    active: true,
                    children: 0,
                }),
            }))
        })();
        if result.is_err() {
            let judge = self.judge.unregister(&agent.id);
            let egress = self.egress.unregister_agent(&agent.id);
            if let Err(error) = judge {
                return result.context(format!("judge rollback failed: {error:#}"));
            }
            if let Err(error) = egress {
                return result.context(format!("egress rollback failed: {error}"));
            }
        }
        result
    }
}

struct Lifecycle {
    active: bool,
    children: usize,
}
pub struct PreparedWall {
    pub artifacts: WallArtifacts,
    environment: BTreeMap<String, String>,
    provider: LaunchBinding,
    queue: Arc<LaunchBinding>,
    queue_state: Option<PathBuf>,
    broker_peer: crate::local_sockets::identity::PeerToken,
    checkout: PathBuf,
    id: String,
    egress: ServiceClient,
    judge: LocalJudgeRuntime,
    _lock: File,
    life: Mutex<Lifecycle>,
}

impl PreparedWall {
    pub(crate) fn stop_admission(&self) -> Result<()> {
        self.life
            .lock()
            .map_err(|_| anyhow::anyhow!("wall lifecycle lock poisoned"))?
            .active = false;
        Ok(())
    }
    pub fn broker_peer_token(&self) -> crate::local_sockets::identity::PeerToken {
        self.broker_peer
    }
    /// After suspension and all pending/running queue work ends, permit a new
    /// configuration. The caller still owns immutable command-file cleanup.
    pub fn retire_queue(&self) -> Result<()> {
        let life = self
            .life
            .lock()
            .map_err(|_| anyhow::anyhow!("wall lifecycle lock poisoned"))?;
        if life.active || life.children != 0 {
            bail!("suspend the wall before retiring queue authority");
        }
        if let Some(root) = &self.queue_state {
            crate::queue::local_wall::retire_registration(root, &self.id)?;
        }
        Ok(())
    }
    /// Caller pauses admission first; pending jobs remain durable for restart.
    pub fn detach_queue(&self) -> Result<()> {
        if let Some(root) = &self.queue_state {
            crate::queue::local_wall::ensure_no_running_jobs(root, &self.id)?;
            crate::queue::local_wall::detach(root, &self.id)?;
        }
        Ok(())
    }
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub fn spawn_provider(
        self: &Arc<Self>,
        tool: &str,
        arguments: &[OsString],
    ) -> Result<WallChild> {
        self.spawn(tool, arguments, true)
    }

    pub fn spawn_queue(self: &Arc<Self>, tool: &str, arguments: &[OsString]) -> Result<WallChild> {
        self.spawn(tool, arguments, false)
    }

    fn spawn(
        self: &Arc<Self>,
        tool: &str,
        arguments: &[OsString],
        provider: bool,
    ) -> Result<WallChild> {
        let mut life = self
            .life
            .lock()
            .map_err(|_| anyhow::anyhow!("wall lifecycle lock poisoned"))?;
        if !life.active {
            bail!("agent wall is suspended");
        }
        let executable = self
            .artifacts
            .tools
            .get(tool)
            .context("tool is not host-staged")?;
        let environment: Vec<_> = self
            .environment
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();
        let binding = if provider {
            &self.provider
        } else {
            self.queue.as_ref()
        };
        let child = binding.spawn(executable, arguments, &environment, &self.checkout)?;
        life.children += 1;
        Ok(WallChild {
            child: Some(child),
            wall: self.clone(),
        })
    }

    /// Stops admission, retaining durable ports. Caller must also cancel any
    /// pending queue work before calling ServiceClient::release_agent later.
    pub fn suspend(&self) -> Result<()> {
        let mut life = self
            .life
            .lock()
            .map_err(|_| anyhow::anyhow!("wall lifecycle lock poisoned"))?;
        if life.children != 0 {
            bail!("agent wall still has running children");
        }
        self.detach_queue()?;
        life.active = false;
        let judge = self.judge.unregister(&self.id);
        let egress = self.egress.unregister_agent(&self.id);
        judge?;
        egress?;
        Ok(())
    }
}

pub struct WallChild {
    child: Option<RegisteredChild>,
    wall: Arc<PreparedWall>,
}
impl WallChild {
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.take_stdout()
    }
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.take_stderr()
    }
    pub fn wait(&mut self) -> Result<ExitStatus> {
        let status = self
            .child
            .as_mut()
            .context("child already reaped")?
            .wait()?;
        self.finish();
        Ok(status)
    }
    fn finish(&mut self) {
        if self.child.take().is_some() {
            if let Ok(mut life) = self.wall.life.lock() {
                life.children -= 1;
            }
        }
    }
}
impl Drop for WallChild {
    fn drop(&mut self) {
        self.finish();
    }
}

fn environment(
    config: &HostConfiguration,
    agent: &AgentRegistration,
    state: &Path,
    artifacts: &WallArtifacts,
    egress: &EgressRegistration,
    judge: &JudgeEndpoint,
) -> BTreeMap<String, String> {
    let mut values = egress.environment();
    for (key, value) in [
        ("HOME", config.home.display().to_string()),
        (
            "PATH",
            format!(
                "{}:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
                artifacts.executables.display()
            ),
        ),
        ("TMPDIR", artifacts.tmp.display().to_string()),
        ("TMUX_TMPDIR", artifacts.tmp.display().to_string()),
        ("CARGO_HOME", artifacts.cargo.display().to_string()),
        ("GH_CONFIG_DIR", artifacts.gh.display().to_string()),
        ("SESSION_MANAGER_ID", agent.id.clone()),
        ("CLAUDE_SESSION_MANAGER_ID", agent.id.clone()),
        ("LOCAL_AGENT_ID", agent.id.clone()),
        ("LOCAL_JUDGE_URL", judge.url.clone()),
        ("LOCAL_JUDGE_TOKEN", judge.token.clone()),
        ("GIT_CONFIG_COUNT", "4".into()),
        ("GIT_CONFIG_KEY_2", "user.name".into()),
        ("GIT_CONFIG_VALUE_2", agent.name.clone()),
        ("GIT_CONFIG_KEY_3", "user.email".into()),
        (
            "GIT_CONFIG_VALUE_3",
            format!("{}@local-agent.invalid", agent.id),
        ),
        ("LANG", "en_US.UTF-8".into()),
        ("TERM", "xterm-256color".into()),
    ] {
        values.insert(key.into(), value);
    }
    for name in ["config", "data", "cache", "state"] {
        values.insert(
            format!("XDG_{}_HOME", name.to_uppercase()),
            state.join("xdg").join(name).display().to_string(),
        );
    }
    values
}

fn physical_directory(path: &Path) -> Result<()> {
    if !path.is_absolute() || !path.is_dir() || path.canonicalize()? != path {
        bail!(
            "directory must be physical and absolute: {}",
            path.display()
        );
    }
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("private directory must be absolute");
    }
    let mut parent = Some(path);
    while let Some(path) = parent {
        if path.is_symlink() {
            bail!("private directory has a symlink component");
        }
        parent = path.parent();
    }
    fs::create_dir_all(path)?;
    physical_directory(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if path.is_symlink() || (path.exists() && path.metadata()?.nlink() != 1) {
        bail!("host artifact has aliases");
    }
    use rand_core::{OsRng, RngCore};
    let mut nonce = [0u8; 16];
    OsRng.fill_bytes(&mut nonce);
    let temporary = path.with_extension(format!("{:x}.tmp", Sha256::digest(nonce)));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}
fn remove_stale_socket(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(metadata) if metadata.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(path)?;
                    Ok(())
                }
                _ => bail!("broker endpoint is still active or cannot be verified stale"),
            }
        }
        _ => bail!("broker endpoint is not a socket"),
    }
}
fn loopback_port(url: &str) -> Result<u16> {
    let url: axum::http::Uri = url.parse()?;
    if url.scheme_str() != Some("http") || !matches!(url.host(), Some("127.0.0.1" | "[::1]")) {
        bail!("service endpoint is not exact loopback HTTP");
    }
    url.port_u16().context("missing service port")
}
fn run_python(python: &Path, script: &Path, arguments: &[OsString]) -> Result<Vec<u8>> {
    let output = Command::new(python)
        .arg(script)
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .output()?;
    if !output.status.success() {
        bail!(
            "wall preparation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

fn install_sources(config: &Path) -> Result<PathBuf> {
    let root = config.join("sources");
    private_directory(&root.join("native"))?;
    macro_rules! source {
        ($path:literal) => {
            atomic_write(
                &root.join($path),
                include_bytes!(concat!("../../../scripts/local-wall/", $path)),
            )?;
        };
    }
    source!("prepare.py");
    source!("wall_profile.py");
    source!("build_adapter.py");
    source!("native/adapter.c");
    source!("native/wire_client.c");
    source!("native/wire_client.h");
    source!("native/launch_supervisor.c");
    source!("native/exec_guards.c");
    source!("native/exec_image.c");
    source!("native/exec_recovery.c");
    source!("native/fork_lifecycle.c");
    source!("native/spawn_actions.c");
    source!("native/spawn_directory.c");
    source!("native/spawn_lifecycle.c");
    source!("native/spawn_contained.c");
    Ok(root)
}

#[cfg(all(test, target_os = "macos"))]
mod tests;

pub mod recovery;
