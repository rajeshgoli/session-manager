//! Host-only immutable queue launch registration. No HTTP request can create,
//! replace or select this state. #1974 installs/restores it before admission.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WallSpec {
    pub agent_state: PathBuf,
    pub checkout: PathBuf,
    pub profile: PathBuf,
    /// Host-staged, agent-immutable shell. An adapted native shell preserves
    /// the broker adapter for ordinary child processes.
    pub shell: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub gateway_port: u16,
    pub egress_port: u16,
    /// Optional for legacy host primitives; production recovery requires it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_authority_sha256: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RegisteredWall {
    agent: String,
    spec: WallSpec,
    profile_sha256: String,
    shell_sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct JobBinding {
    agent: String,
    wall_sha256: String,
    command_sha256: String,
    command: PathBuf,
    cwd: PathBuf,
}

fn valid_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        bail!("invalid local queue identity");
    }
    Ok(())
}
fn physical_directory(path: &Path) -> Result<PathBuf> {
    let physical = path.canonicalize()?;
    if physical != path || !physical.is_dir() {
        bail!("local wall directory must be physical: {}", path.display());
    }
    Ok(physical)
}
fn read_file(path: &Path) -> Result<Vec<u8>> {
    if path.canonicalize()? != path {
        bail!("local wall file must be physical");
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no arguments.
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!("local wall file must be an unshared host-owned regular file");
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(bytes)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn registry_path(state_dir: &Path, agent: &str) -> Result<PathBuf> {
    valid_id(agent)?;
    Ok(state_dir
        .canonicalize()?
        .join("local-walls")
        .join(format!("{agent}.json")))
}
fn fingerprint(wall: &RegisteredWall) -> Result<String> {
    Ok(hash(&serde_json::to_vec(wall)?))
}

/// Register once, outside the wall. The host must generate the production
/// profile for these exact services, exclude the queue state directory from
/// every wall, and deny writes to agent_state outside its mutable subtrees.
/// Restores must use the saved profile/environment rather than regenerate them.
pub fn register(state_dir: &Path, agent: &str, mut spec: WallSpec) -> Result<String> {
    valid_id(agent)?;
    fs::create_dir_all(state_dir)?;
    let state_root = state_dir.canonicalize()?;
    let state_dir = state_root.as_path();
    physical_directory(&spec.agent_state)?;
    physical_directory(&spec.checkout)?;
    if spec.agent_state.starts_with(&spec.checkout)
        || spec.checkout.starts_with(&spec.agent_state)
        || state_dir.starts_with(&spec.checkout)
        || state_dir.starts_with(&spec.agent_state)
        || spec.checkout.starts_with(state_dir)
        || spec.agent_state.starts_with(state_dir)
    {
        bail!("queue service state, checkout and agent state must not overlap");
    }
    if !spec.profile.starts_with(&spec.agent_state) || !spec.shell.starts_with(&spec.agent_state) {
        bail!("profile and shell must be staged in immutable agent state");
    }
    for mutable in ["tmp", "xdg/data", "xdg/cache", "xdg/state"] {
        if spec.profile.starts_with(spec.agent_state.join(mutable))
            || spec.shell.starts_with(spec.agent_state.join(mutable))
        {
            bail!("wall launch artifacts cannot be in mutable agent state");
        }
    }
    if !(18600..=18699).contains(&spec.gateway_port) || !(18700..=18799).contains(&spec.egress_port)
    {
        bail!("invalid local gateway/egress assignment");
    }
    for (key, value) in &spec.environment {
        if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
            bail!("invalid host wall environment");
        }
    }
    // Identity and networking always come from host registration, not a job's
    // request.env (including an empty or forged requester environment).
    for key in [
        "SESSION_MANAGER_ID",
        "CLAUDE_SESSION_MANAGER_ID",
        "LOCAL_AGENT_ID",
    ] {
        spec.environment.insert(key.into(), agent.into());
    }
    spec.environment.insert(
        "SM_API_URL".into(),
        format!("http://127.0.0.1:{}", spec.gateway_port),
    );
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        spec.environment
            .insert(key.into(), format!("http://127.0.0.1:{}", spec.egress_port));
    }
    for key in ["NO_PROXY", "no_proxy"] {
        spec.environment
            .insert(key.into(), "localhost,127.0.0.1,::1".into());
    }
    spec.environment.remove("SM_WALL_RECOVERY_FD");
    spec.environment.remove("DYLD_INSERT_LIBRARIES"); // supplied by the trusted launcher
    let profile = read_file(&spec.profile)?;
    if !crate::local_sockets::has_launch_session_confinement(&profile) {
        bail!("profile lacks descendant confinement");
    }
    let wall = RegisteredWall {
        agent: agent.into(),
        profile_sha256: hash(&profile),
        shell_sha256: hash(&read_file(&spec.shell)?),
        spec,
    };
    let path = registry_path(state_dir, agent)?;
    let directory = path.parent().unwrap();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    physical_directory(directory)?;
    let metadata = directory.metadata()?;
    if metadata.mode() & 0o077 != 0 {
        bail!("local wall registry must be private");
    }
    let bytes = serde_json::to_vec(&wall)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            std::io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()?;
            fs::File::open(directory)?.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let old: RegisteredWall = serde_json::from_slice(&read_file(&path)?)?;
            if old != wall {
                bail!("local wall registration is immutable");
            }
        }
        Err(e) => return Err(e.into()),
    }
    fingerprint(&wall)
}
pub fn validate(state_dir: &Path, agent: &str) -> Result<()> {
    load(state_dir, agent).map(|_| ())
}
/// Host-only restoration snapshot; existing artifacts must still match.
pub fn registered_spec(state_dir: &Path, agent: &str) -> Result<Option<WallSpec>> {
    let path = registry_path(state_dir, agent)?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => Ok(Some(load(state_dir, agent)?.spec)),
    }
}

/// Stop new admission first. Pending jobs may survive a host restart; running
/// jobs must finish before their launcher and immutable artifacts are replaced.
pub fn ensure_no_running_jobs(state_dir: &Path, agent: &str) -> Result<()> {
    let connection = open_queue_jobs_connection(&state_dir.join("queue_runner.db"))?;
    init_queue_jobs_schema(&connection)?;
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM queue_jobs WHERE local_agent_id = ? AND state = 'running'",
        [agent],
        |row| row.get(0),
    )?;
    if count != 0 {
        bail!("local agent still has running queue work");
    }
    Ok(())
}

/// Host-only cleanup after owner admission stops. Preserve running jobs until
/// their process teardown is proved; cancel only this agent's waiting work.
pub(crate) fn cancel_pending_jobs_for_restaging(state_dir: &Path, agent: &str) -> Result<()> {
    valid_id(agent)?;
    let _admission = admission_guard();
    let connection = open_queue_jobs_connection(&state_dir.join("queue_runner.db"))?;
    init_queue_jobs_schema(&connection)?;
    let transaction = connection.unchecked_transaction()?;
    for job in list_queue_job_runtime_records_conn(&transaction)?
        .into_iter()
        .filter(|job| job.local_agent_id.as_deref() == Some(agent) && job.state == "pending")
    {
        transaction.execute(
            "UPDATE queue_jobs SET termination_detail_json = ?2 WHERE id = ?1 AND state = 'pending'",
            params![job.id, r#"{"kind":"cancel","note":"Local agent launch settings are being replaced."}"#],
        )?;
        // The normal completion path retains a durable notification for the
        // queue's next delivery pass, without starting any replacement work.
        finish_queue_job_conn(&transaction, &job, "cancelled", None, None)?;
    }
    transaction.commit()?;
    Ok(())
}

/// Host-only retirement after admission stops and all pending/running work ends.
pub fn retire_registration(state_dir: &Path, agent: &str) -> Result<()> {
    valid_id(agent)?;
    let connection = open_queue_jobs_connection(&state_dir.join("queue_runner.db"))?;
    init_queue_jobs_schema(&connection)?;
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM queue_jobs WHERE local_agent_id = ? AND state IN ('pending', 'running')",
        [agent], |row| row.get(0),
    )?;
    if count != 0 {
        bail!("local agent still has pending or running queue work");
    }
    #[cfg(target_os = "macos")]
    detach(state_dir, agent)?;
    if registered_spec(state_dir, agent)?.is_some() {
        let path = registry_path(state_dir, agent)?;
        fs::remove_file(&path)?;
        fs::File::open(path.parent().unwrap())?.sync_all()?;
    }
    Ok(())
}
fn load(state_dir: &Path, agent: &str) -> Result<RegisteredWall> {
    let path = registry_path(state_dir, agent)?;
    let wall: RegisteredWall = serde_json::from_slice(&read_file(&path)?)?;
    if wall.agent != agent
        || hash(&read_file(&wall.spec.profile)?) != wall.profile_sha256
        || hash(&read_file(&wall.spec.shell)?) != wall.shell_sha256
    {
        bail!("local wall registration or launch artifact changed");
    }
    if let Some(expected) = &wall.spec.host_authority_sha256 {
        if &hash(&read_file(
            &wall
                .spec
                .agent_state
                .join("xdg/config/queue-authority.json"),
        )?) != expected
        {
            bail!("local host preparation authority changed");
        }
    }
    physical_directory(&wall.spec.agent_state)?;
    physical_directory(&wall.spec.checkout)?;
    Ok(wall)
}

#[cfg(target_os = "macos")]
enum Launcher {
    Owned(Arc<crate::local_sockets::launch::LaunchBinding>),
    Durable(crate::local_wall::owner::OwnerClient),
}
#[cfg(target_os = "macos")]
impl Launcher {
    fn queue_identity(&self) -> std::io::Result<(&str, &Path)> {
        match self {
            Self::Owned(binding) => binding.queue_identity(),
            Self::Durable(owner) => Ok(owner.queue_identity()),
        }
    }
}
#[cfg(target_os = "macos")]
fn launchers() -> &'static Mutex<BTreeMap<PathBuf, Launcher>> {
    static LAUNCHERS: OnceLock<Mutex<BTreeMap<PathBuf, Launcher>>> = OnceLock::new();
    LAUNCHERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}
/// Attach the host's live socket-service binding, after registration or restart.
/// It must be the submitting agent's service and carry no provider listener.
#[cfg(target_os = "macos")]
pub fn attach(
    state_dir: &Path,
    agent: &str,
    launcher: Arc<crate::local_sockets::launch::LaunchBinding>,
) -> Result<()> {
    let wall = load(state_dir, agent)?;
    let (launch_agent, profile) = launcher.queue_identity()?;
    if launch_agent != agent || profile != wall.spec.profile {
        bail!("queue launcher does not match registered agent/profile");
    }
    launchers()
        .lock()
        .map_err(|_| anyhow::anyhow!("local launch lock poisoned"))?
        .insert(registry_path(state_dir, agent)?, Launcher::Owned(launcher));
    Ok(())
}
/// Reconnect to the live host tmux owner without replacing its broker/profile.
#[cfg(target_os = "macos")]
pub fn attach_durable(
    state_dir: &Path,
    agent: &str,
    owner: crate::local_wall::owner::OwnerClient,
) -> Result<()> {
    owner.ready(state_dir)?;
    if owner.queue_identity().0 != agent {
        bail!("durable owner agent differs");
    }
    launchers()
        .lock()
        .map_err(|_| anyhow::anyhow!("local launch lock poisoned"))?
        .insert(registry_path(state_dir, agent)?, Launcher::Durable(owner));
    Ok(())
}
#[cfg(target_os = "macos")]
pub fn detach(state_dir: &Path, agent: &str) -> Result<()> {
    launchers()
        .lock()
        .map_err(|_| anyhow::anyhow!("local launch lock poisoned"))?
        .remove(&registry_path(state_dir, agent)?);
    Ok(())
}

pub(super) fn prepare(
    state_dir: &Path,
    id: &str,
    request: &mut CreateQueueJob,
) -> Result<Option<JobBinding>> {
    let Some(identity) = &request.local_submitter else {
        return Ok(None);
    };
    let agent = identity.agent_id();
    let wall = load(state_dir, agent)?;
    let cwd = physical_directory(Path::new(&request.cwd))?;
    if !cwd.starts_with(&wall.spec.checkout) {
        bail!("local queue cwd must be inside its registered checkout");
    }
    let directory = wall.spec.agent_state.join("queue-inputs");
    fs::create_dir_all(&directory)?;
    physical_directory(&directory)?;
    let command = directory.join(format!("{id}.zsh"));
    let mut source = format!("cd {} || exit 127\n", shell_quote(&cwd.to_string_lossy()));
    match (&request.argv, &request.script) {
        (Some(argv), None) if !argv.is_empty() => {
            source.push_str("exec -- ");
            source.push_str(
                &argv
                    .iter()
                    .map(|a| shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            source.push('\n');
        }
        (None, Some(script)) => {
            source.push_str(script);
            source.push('\n');
        }
        _ => bail!("exactly one local queue argv or script is required"),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(&command)?;
    std::io::Write::write_all(&mut file, source.as_bytes())?;
    file.sync_all()?;
    fs::File::open(&directory)?.sync_all()?;
    request.env = wall.spec.environment.clone();
    Ok(Some(JobBinding {
        agent: agent.into(),
        wall_sha256: fingerprint(&wall)?,
        command_sha256: hash(source.as_bytes()),
        command,
        cwd,
    }))
}
impl JobBinding {
    pub(super) fn command(&self) -> &Path {
        &self.command
    }
}

fn validated_binding(
    state_dir: &Path,
    agent: &str,
    binding_json: &str,
) -> Result<(RegisteredWall, JobBinding)> {
    let binding: JobBinding = serde_json::from_str(binding_json)?;
    let wall = load(state_dir, agent)?;
    if binding.agent != agent
        || fingerprint(&wall)? != binding.wall_sha256
        || binding.command.parent() != Some(wall.spec.agent_state.join("queue-inputs").as_path())
        || hash(&read_file(&binding.command)?) != binding.command_sha256
        || !physical_directory(&binding.cwd)?.starts_with(&wall.spec.checkout)
    {
        bail!("local queue binding changed or invalid");
    }
    Ok((wall, binding))
}

/// A valid durable command may wait for host restoration, but changed authority
/// is an error and follows the ordinary failed-start path.
pub(super) fn admission_ready(state_dir: &Path, agent: &str, binding: &str) -> Result<bool> {
    let (wall, _) = validated_binding(state_dir, agent, binding)?;
    #[cfg(target_os = "macos")]
    {
        let launchers = launchers()
            .lock()
            .map_err(|_| anyhow::anyhow!("local launch lock poisoned"))?;
        let Some(launcher) = launchers.get(&registry_path(state_dir, agent)?) else {
            return Ok(false);
        };
        let (launch_agent, profile) = launcher.queue_identity()?;
        if launch_agent != agent || profile != wall.spec.profile {
            bail!("local queue launcher identity changed");
        }
        if let Launcher::Durable(owner) = launcher {
            return Ok(owner.ready(state_dir).is_ok());
        }
        Ok(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = wall;
        bail!("local queue walls require macOS");
    }
}

pub(super) fn spawn(
    state_dir: &Path,
    agent: &str,
    binding_json: &str,
    output: (fs::File, fs::File, Option<u64>),
) -> Result<QueueChild> {
    let (wall, binding) = validated_binding(state_dir, agent, binding_json)?;
    #[cfg(target_os = "macos")]
    {
        // Keep the registry locked through spawn: detachment cannot return
        // while a previously obtained binding can still launch a child.
        let launchers = launchers()
            .lock()
            .map_err(|_| anyhow::anyhow!("local launch lock poisoned"))?;
        let launcher = launchers.get(&registry_path(state_dir, agent)?).context(
            "local queue launcher is unavailable; restore host binding before admission",
        )?;
        let (launch_agent, profile) = launcher.queue_identity()?;
        if launch_agent != agent || profile != wall.spec.profile {
            bail!("local queue launcher identity changed");
        }
        if let Launcher::Durable(owner) = launcher {
            return Ok(QueueChild::DurableLocal(
                owner.spawn_queue(binding_json, output)?,
            ));
        }
        let Launcher::Owned(launcher) = launcher else {
            unreachable!()
        };
        let environment = wall
            .spec
            .environment
            .iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect::<Vec<_>>();
        let child = launcher.spawn_queue(
            &wall.spec.shell,
            &["-df".into(), binding.command.into_os_string()],
            &environment,
            &binding.cwd,
            output,
        )?;
        Ok(QueueChild::Local(child))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = output;
        bail!("local queue walls require macOS");
    }
}

/// Executed only inside the durable owner. It revalidates the immutable command
/// instead of accepting executable, cwd or environment from the control caller.
#[cfg(target_os = "macos")]
pub(crate) fn spawn_owned(
    state_dir: &Path,
    agent: &str,
    binding_json: &str,
    output: (fs::File, fs::File, Option<u64>),
) -> Result<crate::local_sockets::launch::RegisteredChild> {
    let (wall, binding) = validated_binding(state_dir, agent, binding_json)?;
    let launchers = launchers()
        .lock()
        .map_err(|_| anyhow::anyhow!("local launch lock poisoned"))?;
    let Some(Launcher::Owned(launcher)) = launchers.get(&registry_path(state_dir, agent)?) else {
        bail!("owner's queue binding unavailable");
    };
    let (id, profile) = launcher.queue_identity()?;
    if id != agent || profile != wall.spec.profile {
        bail!("owner queue identity changed");
    }
    let environment = wall
        .spec
        .environment
        .iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect::<Vec<_>>();
    Ok(launcher.spawn_queue(
        &wall.spec.shell,
        &["-df".into(), binding.command.into_os_string()],
        &environment,
        &binding.cwd,
        output,
    )?)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::local_egress::gateway::VerifiedLocalAgent;

    #[test]
    fn durable_queue_launch_confines_scripts_and_restores_only_registered_wall() {
        let fixture = crate::local_sockets::service::prepare_launch();
        let profile = fixture.path("profile");
        let agent_state = profile.parent().unwrap().to_path_buf();
        let root = agent_state.parent().unwrap().parent().unwrap();
        let state_dir = root.join("queue-service");
        fs::create_dir_all(&state_dir).unwrap();
        let shell = fixture.path("application").with_file_name("queue-zsh");
        fs::copy("/bin/zsh", &shell).unwrap();
        assert!(Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&shell)
            .status()
            .unwrap()
            .success());
        let spec = WallSpec {
            host_authority_sha256: None,
            agent_state: agent_state.clone(),
            checkout: fixture.path("checkout"),
            profile: profile.clone(),
            shell,
            environment: fixture
                .environment
                .iter()
                .map(|(k, v)| (k.to_str().unwrap().into(), v.to_str().unwrap().into()))
                .collect(),
            gateway_port: 18600,
            egress_port: 18700,
        };
        register(&state_dir, "launch", spec.clone()).unwrap();
        let outside = root.join("forbidden-write");
        let script = format!(
            "[[ $CLAUDE_SESSION_MANAGER_ID == launch ]] || exit 10\n\
             [[ $SM_API_URL == http://127.0.0.1:18600 ]] || exit 11\n\
             [[ $HTTP_PROXY == http://127.0.0.1:18700 ]] || exit 12\n\
             print confined > {} || exit 13\n\
             if (print escape > {}); then exit 14; fi\n\
             if (print changed >> {}); then exit 15; fi\n\
             {} -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\",0)); s.listen(); c=socket.create_connection(s.getsockname()); c.sendall(b\"ok\"); a,_=s.accept(); assert a.recv(2)==b\"ok\"' || exit 16\n\
             print queue-wall-ok\n",
            shell_quote(&spec.checkout.join("result").to_string_lossy()),
            shell_quote(&outside.to_string_lossy()),
            shell_quote(&profile.to_string_lossy()),
            shell_quote(&fixture.path("python").to_string_lossy()),
        );
        let request = CreateQueueJob {
            local_submitter: Some(VerifiedLocalAgent::test_identity("launch")),
            job_type: "tests".into(),
            label: "wall".into(),
            requester_session_id: Some("forged".into()),
            notify_session_id: "another-agent".into(),
            cwd: spec.checkout.display().to_string(),
            argv: None,
            script: Some(script),
            env: BTreeMap::from([
                ("CLAUDE_SESSION_MANAGER_ID".into(), "forged".into()),
                ("SM_API_URL".into(), "http://127.0.0.1:8420".into()),
            ]),
            timeout_seconds: 30,
            cpu_percent: None,
            gpu_percent: None,
            memory_bytes: None,
            rank_tickets: None,
        };
        let job =
            RetainedQueueStore::create_queue_job_in_state_dir(&state_dir, request.clone()).unwrap();
        let conn = open_queue_jobs_connection(&state_dir.join("queue_runner.db")).unwrap();
        let runtime = get_queue_job_runtime_conn(&conn, &job.id).unwrap().unwrap();
        // A server that has not restored the trusted socket binding cannot
        // silently run a persisted local command as an ordinary host job.
        assert!(spawn_queue_job_process(&runtime, None, &state_dir).is_err());
        attach(&state_dir, "launch", Arc::new(fixture.queue_binding())).unwrap();
        detach(&state_dir, "launch").unwrap();
        assert!(spawn_queue_job_process(&runtime, None, &state_dir).is_err());
        attach(&state_dir, "launch", Arc::new(fixture.queue_binding())).unwrap();
        let mut child = spawn_queue_job_process(&runtime, None, &state_dir).unwrap();
        let status = child.wait().unwrap();
        let log = fs::read_to_string(job.log_path.as_ref().unwrap()).unwrap();
        assert!(status.success(), "{status}: {log}");
        assert!(log.contains("queue-wall-ok"), "{log}");
        assert_eq!(
            fs::read_to_string(spec.checkout.join("result")).unwrap(),
            "confined\n"
        );
        assert!(!outside.exists());
        let mut argv_request = request;
        argv_request.script = None;
        argv_request.argv = Some(vec![
            fixture.path("python").display().to_string(),
            "-c".into(),
            "import time; time.sleep(60)".into(),
        ]);
        let argv_job =
            RetainedQueueStore::create_queue_job_in_state_dir(&state_dir, argv_request).unwrap();
        let argv_runtime = get_queue_job_runtime_conn(&conn, &argv_job.id)
            .unwrap()
            .unwrap();
        let mut argv_child = spawn_queue_job_process(&argv_runtime, None, &state_dir).unwrap();
        let pgid = i64::from(argv_child.id());
        terminate_child_process_group_with_grace(&mut argv_child, pgid, 0);
        assert!(argv_child.try_wait().unwrap().is_some());
        assert!(!process_group_exists(pgid));
        let mut incomplete = runtime.clone();
        incomplete.local_binding_json = None;
        assert!(spawn_queue_job_process(&incomplete, None, &state_dir).is_err());
        // Replacing the persisted profile fails before executing any command.
        fs::write(&profile, "changed").unwrap();
        assert!(spawn_queue_job_process(&runtime, None, &state_dir).is_err());
        detach(&state_dir, "launch").unwrap();
    }
}
