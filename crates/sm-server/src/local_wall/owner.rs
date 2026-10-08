//! Host tmux launcher. This process, rather than an sm server generation, owns
//! the broker, provider root and queued roots. No HTTP route exposes control.
use super::*;
use crate::local_sockets::identity::PeerToken;
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};
use std::{
    io::{self, IoSlice, IoSliceMut, Read, Write},
    os::{
        fd::{FromRawFd, RawFd},
        unix::net::{UnixListener, UnixStream},
    },
    process::ExitStatus,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

mod retirement;
pub use retirement::retire_for_restaging;

#[derive(Serialize, Deserialize)]
pub struct ProviderLaunch {
    pub tool: String,
    pub arguments: Vec<OsString>,
    pub settings: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct Configuration {
    host: HostConfiguration,
    agent: AgentRegistration,
    queue: PathBuf,
    egress: ServiceClient,
    judge: LocalJudgeRuntime,
    provider: ProviderLaunch,
}

/// Execute this exact argv in the host tmux serve window. The installed binary
/// is copied into private queue state so blue/green upgrades cannot replace it.
pub struct HostLaunch {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub client: OwnerClient,
}

#[derive(Clone)]
pub struct OwnerClient {
    socket: PathBuf,
    agent: String,
    profile: PathBuf,
}

#[derive(Serialize, Deserialize)]
enum Request {
    Info,
    Spawn {
        binding: String,
        ceiling: Option<u64>,
    },
    Status(u64),
    Release(u64),
    Retire,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    value: serde_json::Value,
    error: Option<String>,
}

fn directory(queue: &Path, agent: &str) -> Result<PathBuf> {
    if agent.is_empty()
        || agent.len() > 64
        || !agent
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        bail!("invalid durable wall agent");
    }
    Ok(queue.join("local-wall-owners").join(agent))
}
fn socket(config: &Configuration) -> PathBuf {
    let hash = format!("{:x}", Sha256::digest(config.agent.id.as_bytes()));
    config.host.alias_root.join(format!("{}.host", &hash[..16]))
}
fn read_configuration(path: &Path) -> Result<Configuration> {
    if path.canonicalize()? != path {
        bail!("owner configuration has aliases");
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("owner configuration must be an independent private host file");
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let config: Configuration = serde_json::from_slice(&bytes)?;
    if path != directory(&config.queue, &config.agent.id)?.join("launch.json") {
        bail!("owner configuration identity/location changed");
    }
    Ok(config)
}

/// Called only with host configuration, while queue admission is paused.
pub fn stage(
    queue: &Path,
    host: HostConfiguration,
    agent: AgentRegistration,
    egress: ServiceClient,
    judge: LocalJudgeRuntime,
    provider: ProviderLaunch,
    installed_executable: &Path,
) -> Result<HostLaunch> {
    physical_directory(queue)?;
    let root = directory(queue, &agent.id)?;
    if queue.starts_with(&agent.checkout)
        || agent.checkout.starts_with(queue)
        || queue.starts_with(&host.state_root)
        || host.state_root.starts_with(queue)
    {
        bail!("durable host state must be outside agent writable roots");
    }
    private_directory(&root)?;
    let config = Configuration {
        host,
        agent,
        queue: queue.into(),
        egress,
        judge,
        provider,
    };
    let source = installed_executable.canonicalize()?;
    if source.components().any(|part| part.as_os_str() == "target") {
        bail!("durable owner requires an installed host executable");
    }
    // Serialize staging across old/new server processes without changing the
    // preparation lock that the live owner holds for its entire lifetime.
    let _stage = retirement::stage_lock(&root, libc::LOCK_EX | libc::LOCK_NB)?;
    let bytes = serde_json::to_vec(&config)?;
    let path = root.join("launch.json");
    if path.exists() {
        read_configuration(&path)?;
        if fs::read(&path)? != bytes {
            bail!("durable provider launch is immutable");
        }
    } else {
        retirement::record_staged(&config)?;
        atomic_write(&path, &bytes)?;
    }
    let contents = fs::read(&source)?;
    let executable = root.join(format!("owner-{:x}", Sha256::digest(&contents)));
    if !executable.exists() {
        atomic_write(&executable, &contents)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o500))?;
    }
    let metadata = fs::symlink_metadata(&executable)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.mode() & 0o222 != 0
        || fs::read(&executable)? != contents
    {
        bail!("staged host executable changed");
    }
    Ok(HostLaunch {
        executable,
        arguments: vec!["--local-wall-owner".into(), path.into_os_string()],
        client: client(&config),
    })
}
fn client(config: &Configuration) -> OwnerClient {
    OwnerClient {
        socket: socket(config),
        agent: config.agent.id.clone(),
        profile: config
            .host
            .state_root
            .join(&config.agent.id)
            .join("xdg/config/wall.sb"),
    }
}

impl OwnerClient {
    /// A durable marker forbids fallback to a generation-owned broker, including
    /// while the tmux launcher is starting, unavailable or retired.
    pub fn registered(queue: &Path, agent: &str) -> Result<Option<Self>> {
        let path = directory(queue, agent)?.join("launch.json");
        if !path.try_exists()? {
            return Ok(None);
        }
        let config = read_configuration(&path)?;
        if config.queue != queue || config.agent.id != agent {
            bail!("owner registration changed");
        }
        Ok(Some(client(&config)))
    }
    fn connect(&self) -> Result<UnixStream> {
        let stream = UnixStream::connect(&self.socket)?;
        if PeerToken::read(&stream)?.process()?.identity.uid != unsafe { libc::geteuid() } {
            bail!("owner peer is not the host user");
        }
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        Ok(stream)
    }
    fn exchange(
        &self,
        stream: &mut UnixStream,
        request: Request,
        files: &[RawFd],
    ) -> Result<serde_json::Value> {
        send_files(stream, files)?;
        write_frame(stream, &request)?;
        let reply: Reply = read_frame(stream)?;
        if let Some(error) = reply.error {
            bail!("durable owner: {error}");
        }
        Ok(reply.value)
    }
    fn request(&self, request: Request, files: &[RawFd]) -> Result<serde_json::Value> {
        self.exchange(&mut self.connect()?, request, files)
    }
    pub fn ready(&self, queue: &Path) -> Result<()> {
        let info = self.request(Request::Info, &[])?;
        let remote: crate::queue::local_wall::WallSpec =
            serde_json::from_value(info["spec"].clone())?;
        let saved = crate::queue::local_wall::registered_spec(queue, &self.agent)?
            .context("missing wall registration")?;
        if saved != remote || saved.profile != self.profile {
            bail!("owner wall differs from registered authority");
        }
        Ok(())
    }
    pub(crate) fn queue_identity(&self) -> (&str, &Path) {
        (&self.agent, &self.profile)
    }
    pub(crate) fn spawn_queue(
        &self,
        binding: &str,
        output: (File, File, Option<u64>),
    ) -> io::Result<RemoteChild> {
        let mut lease = self.connect().map_err(io::Error::other)?;
        let value = self
            .exchange(
                &mut lease,
                Request::Spawn {
                    binding: binding.into(),
                    ceiling: output.2,
                },
                &[output.0.as_raw_fd(), output.1.as_raw_fd()],
            )
            .map_err(io::Error::other)?;
        let pid = value["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or_else(|| io::Error::other("missing child pid"))?;
        let key = value["key"]
            .as_u64()
            .ok_or_else(|| io::Error::other("missing child key"))?;
        Ok(RemoteChild {
            owner: self.clone(),
            pid,
            key,
            status: None,
            _lease: lease,
        })
    }
    /// Stop provider and all queued descendants, revoke their roots and suspend
    /// services. Durable manifests remain until pending queue work is cancelled.
    pub fn retire(&self) -> Result<()> {
        self.request(Request::Retire, &[]).map(|_| ())
    }
    #[cfg(test)]
    pub(super) fn info_for_test(&self) -> serde_json::Value {
        self.request(Request::Info, &[]).unwrap()
    }
}

pub struct RemoteChild {
    owner: OwnerClient,
    pid: u32,
    key: u64,
    status: Option<ExitStatus>,
    // EOF tells the owner to kill/revoke this queue tree if the sm process
    // crashes or loses a launch reply. Provider authority has no such lease.
    _lease: UnixStream,
}
impl RemoteChild {
    pub fn id(&self) -> u32 {
        self.pid
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        if self.status.is_none() {
            let value = self
                .owner
                .request(Request::Status(self.key), &[])
                .map_err(io::Error::other)?;
            if let Some(raw) = value.as_i64() {
                self.status = Some(ExitStatus::from_raw(raw as i32));
            }
        }
        Ok(self.status)
    }
    #[cfg(test)]
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for RemoteChild {
    fn drop(&mut self) {
        let _ = self.owner.request(Request::Release(self.key), &[]);
    }
}

fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > 65536 {
        bail!("owner frame too large");
    }
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}
fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size > 65536 {
        bail!("owner frame too large");
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
fn send_files(stream: &UnixStream, files: &[RawFd]) -> Result<()> {
    let controls = if files.is_empty() {
        vec![]
    } else {
        vec![ControlMessage::ScmRights(files)]
    };
    if sendmsg::<()>(
        stream.as_raw_fd(),
        &[IoSlice::new(b"F")],
        &controls,
        MsgFlags::empty(),
        None,
    )? != 1
    {
        bail!("incomplete owner descriptor frame");
    }
    Ok(())
}
fn receive_files(stream: &UnixStream) -> Result<Vec<File>> {
    let mut byte = [0];
    let mut space = nix::cmsg_space!([RawFd; 2]);
    let mut slices = [IoSliceMut::new(&mut byte)];
    let message = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut slices,
        Some(&mut space),
        MsgFlags::empty(),
    )?;
    let mut files = Vec::new();
    for control in message.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = control {
            for fd in fds {
                // SAFETY: SCM_RIGHTS supplies newly owned descriptors.
                let file = unsafe { File::from_raw_fd(fd) };
                if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                    return Err(io::Error::last_os_error().into());
                }
                files.push(file);
            }
        }
    }
    if message.bytes != 1 || message.flags.contains(MsgFlags::MSG_CTRUNC) || files.len() > 2 {
        bail!("invalid owner descriptor frame");
    }
    Ok(files)
}

struct ChildRecord {
    child: RegisteredChild,
    status: Option<ExitStatus>,
    lease: UnixStream,
}
fn lease_connected(stream: &UnixStream) -> bool {
    let mut byte = 0u8;
    // SAFETY: recv writes at most one byte; peek leaves the protocol untouched.
    let count = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    count == -1 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock
}

struct Lifetime<'a> {
    config: &'a Configuration,
    wall: Arc<PreparedWall>,
    provider: Option<WallChild>,
    children: BTreeMap<u64, ChildRecord>,
}
impl Lifetime<'_> {
    fn retire(&mut self) -> Result<()> {
        self.wall.stop_admission()?;
        self.provider.take();
        self.children.clear();
        crate::queue::local_wall::detach(&self.config.queue, &self.config.agent.id)?;
        // Attempt both revocations even if one service is unavailable.
        let judge = self.config.judge.unregister(&self.config.agent.id);
        let egress = self.config.egress.unregister_agent(&self.config.agent.id);
        judge?;
        egress?;
        retirement::record_retired(self.config)?;
        Ok(())
    }
}
impl Drop for Lifetime<'_> {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

/// Host launcher entry point; run in tmux, independently of sm blue/green slots.
pub async fn run(path: PathBuf) -> Result<()> {
    let stopping = Arc::new(AtomicBool::new(false));
    let worker_stop = stopping.clone();
    let mut worker = tokio::task::spawn_blocking(move || run_inner(&path, worker_stop));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = &mut worker => return result?,
        _ = tokio::signal::ctrl_c() => {},
        _ = terminate.recv() => {},
    }
    stopping.store(true, Ordering::Release);
    worker.await?
}
fn run_inner(path: &Path, stopping: Arc<AtomicBool>) -> Result<()> {
    use std::os::unix::process::ExitStatusExt;
    let _lifetime = retirement::stage_lock(
        path.parent()
            .context("owner configuration missing directory")?,
        libc::LOCK_SH | libc::LOCK_NB,
    )?;
    let config = read_configuration(path)?;
    let runtime = LocalWallRuntime::new(
        config.host.clone(),
        config.egress.clone(),
        config.judge.clone(),
    )?;
    let wall = runtime.prepare_for_queue(&config.agent, &config.queue)?;
    let mut life = Lifetime {
        config: &config,
        wall: wall.clone(),
        provider: None,
        children: BTreeMap::new(),
    };
    retirement::record_started(&config)?;
    let mut provider = wall.spawn_provider_with_environment(
        &config.provider.tool,
        &config.provider.arguments,
        &config.provider.settings,
    )?;
    let stdout = provider.take_stdout().context("missing provider stdout")?;
    let stderr = provider.take_stderr().context("missing provider stderr")?;
    life.provider = Some(provider);
    let out = thread::spawn(move || {
        let _ = io::copy(&mut { stdout }, &mut io::stdout());
    });
    let err = thread::spawn(move || {
        let _ = io::copy(&mut { stderr }, &mut io::stderr());
    });
    let endpoint = socket(&config);
    remove_stale_socket(&endpoint)?;
    let listener = UnixListener::bind(&endpoint)?;
    fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let mut next = 0u64;
    let result = (|| {
        while !stopping.load(Ordering::Acquire) {
            if let Some(status) = life
                .provider
                .as_mut()
                .context("provider retired")?
                .try_wait()?
            {
                if !status.success() {
                    bail!("provider exited with {status}");
                }
                break;
            }
            // Retain the supervisor's PID until the caller consumes status;
            // this keeps process-group cancellation attached to the same tree.
            life.children
                .retain(|_, record| lease_connected(&record.lease));
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let response = (|| {
                if PeerToken::read(&stream)?.process()?.identity.uid != unsafe { libc::geteuid() } {
                    bail!("host control peer required");
                }
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                let mut files = receive_files(&stream)?;
                let request: Request = read_frame(&mut stream)?;
                if !matches!(request, Request::Spawn { .. }) && !files.is_empty() {
                    bail!("unexpected owner descriptors");
                }
                match request {
                    Request::Info => Ok(
                        json!({"spec": crate::queue::local_wall::registered_spec(&config.queue, &config.agent.id)?.context("missing owner wall")?, "broker_peer": wall.broker_peer_token().0}),
                    ),
                    Request::Spawn { binding, ceiling } => {
                        if life.children.len() >= 1024 {
                            bail!("owner child limit reached");
                        }
                        if files.len() != 2 {
                            bail!("queue output descriptors required");
                        }
                        let stderr = files.pop().unwrap();
                        let stdout = files.pop().unwrap();
                        let child = crate::queue::local_wall::spawn_owned(
                            &config.queue,
                            &config.agent.id,
                            &binding,
                            (stdout, stderr, ceiling),
                        )?;
                        next = next.checked_add(1).context("child key overflow")?;
                        let pid = child.id();
                        life.children.insert(
                            next,
                            ChildRecord {
                                child,
                                status: None,
                                lease: stream.try_clone()?,
                            },
                        );
                        Ok(json!({"pid": pid, "key": next}))
                    }
                    Request::Status(key) => {
                        let record = life.children.get_mut(&key).context("unknown owner child")?;
                        if record.status.is_none() {
                            record.status = record.child.try_wait()?;
                        }
                        Ok(json!(record.status.map(|status| status.into_raw())))
                    }
                    Request::Release(key) => {
                        life.children.remove(&key);
                        Ok(json!(null))
                    }
                    Request::Retire => {
                        stopping.store(true, Ordering::Release);
                        life.retire()?;
                        Ok(json!(null))
                    }
                }
            })();
            let reply = match response {
                Ok(value) => Reply { value, error: None },
                Err(error) => Reply {
                    value: json!(null),
                    error: Some(format!("{error:#}")),
                },
            };
            let _ = write_frame(&mut stream, &reply);
        }
        Ok(())
    })();
    // Kill/revoke all roots before suspending egress and judge. Queue database
    // lifecycle remains sm's responsibility; no result is fabricated here.
    let retirement = life.retire();
    drop(listener);
    let _ = fs::remove_file(endpoint);
    let _ = out.join();
    let _ = err.join();
    result.and(retirement)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "spawned by the production wall integration fixture"]
    async fn owner_child() {
        run(PathBuf::from(
            std::env::var_os("SM_WALL_OWNER_FIXTURE").unwrap(),
        ))
        .await
        .unwrap();
    }

    #[test]
    #[ignore = "spawned by the production wall integration fixture"]
    fn recovered_generation_child() {
        let path = PathBuf::from(std::env::var_os("SM_WALL_OWNER_FIXTURE").unwrap());
        let config = read_configuration(&path).unwrap();
        let walls = recovery::GenerationWalls::new(
            config.queue.clone(),
            config.host.python.clone(),
            config.host.sm_upstream,
            &format!("http://127.0.0.1:{}", config.host.model_port),
            config.egress.clone(),
            config.judge.clone(),
        )
        .unwrap();
        assert!(walls.reconcile().unwrap().is_empty());
        assert!(walls.get_durable(&config.agent.id).unwrap().is_some());
        let job = std::env::var("SM_WALL_OWNER_JOB").unwrap();
        crate::queue::RetainedQueueStore::start_queue_job_in_state_dir(
            &config.queue,
            &config.queue.join("messages.db"),
            &job,
            0,
        )
        .unwrap();
        if std::env::var_os("SM_WALL_CRASH_AFTER_LAUNCH").is_some() {
            // Exit skips destructors, as a killed sm process would. Only the
            // kernel closing the per-job lease can tell the owner to revoke it.
            std::process::exit(0);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let result = crate::queue::RetainedQueueStore::get_queue_job_strict_from_path(
                &config.queue.join("queue_runner.db"),
                &job,
            )
            .unwrap()
            .unwrap();
            if result.state != "running" && result.state != "pending" {
                let output = fs::read_to_string(result.log_path.unwrap()).unwrap();
                assert_eq!(result.state, "succeeded", "{output}");
                assert!(output.contains("owner-queue-ok"), "{output}");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        walls.stop().unwrap();
    }
}
