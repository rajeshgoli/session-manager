//! Private Unix service and bounded descriptor-transfer workers.

use super::{
    authority::{Authority, RootCapability},
    identity::{PeerToken, ProcessIdentity},
    pool::{PortPolicy, SocketLease, SocketPool},
    IpVersion, Operation, Reply, Request, REQUEST_SIZE,
};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use std::{
    ffi::CString,
    fs,
    io::{self, IoSlice, Read},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const MAX_CONNECTIONS: usize = 32;
const TICK: Duration = Duration::from_millis(50);
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// Share one hub across every agent. The host owns registration and the service
/// directory; the wall must exclude that directory from agent writes.
pub struct BrokerHub {
    authority: Authority,
    policy: Arc<PortPolicy>,
}

struct AgentState {
    active: AtomicBool,
    authority: Authority,
    agent: String,
    pool: SocketPool,
    control: Mutex<Vec<std::net::TcpListener>>,
    control_port: u16,
}

/// Dropping this handle revokes roots, stops workers and removes the endpoint.
/// Restore creates a new handle, peer token and process registrations.
pub struct AgentService {
    state: Arc<AgentState>,
    worker: Option<JoinHandle<()>>,
    endpoint: Endpoint,
    token: PeerToken,
}

struct Endpoint {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl BrokerHub {
    pub fn new(policy: PortPolicy) -> Self {
        Self {
            authority: Authority::default(),
            policy: Arc::new(policy),
        }
    }

    /// `directory` must be a host-owned, agent-write-denied directory below
    /// private state/tmp. A host-owned short alias may point to that directory
    /// to meet macOS's Unix socket path limit. No submitted wire path is used.
    pub fn register_agent(
        &self,
        agent: &str,
        control_port: u16,
        directory: &Path,
        endpoint_path: &Path,
    ) -> io::Result<AgentService> {
        if !self.policy.permits_control(control_port) {
            return Err(errno(libc::EACCES));
        }
        let directory = directory.canonicalize()?;
        let metadata = fs::metadata(&directory)?;
        // SAFETY: geteuid has no pointer arguments.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(errno(libc::EACCES));
        }
        let parent = endpoint_path
            .parent()
            .ok_or_else(|| errno(libc::EINVAL))?
            .canonicalize()?;
        if parent != directory || endpoint_path.file_name().is_none() {
            return Err(errno(libc::EACCES));
        }
        self.authority.register_agent(agent, control_port)?;
        let result = self.start(agent, control_port, endpoint_path);
        if result.is_err() {
            self.authority.unregister_agent(agent)?;
        }
        result
    }

    fn start(&self, agent: &str, control_port: u16, path: &Path) -> io::Result<AgentService> {
        // UnixListener::bind refuses an existing path; never unlink it blindly.
        let listener = UnixListener::bind(path)?;
        let metadata = fs::symlink_metadata(path)?;
        let endpoint = Endpoint {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let path_string =
            CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| errno(libc::EINVAL))?;
        // SAFETY: path_string is NUL-terminated. NOFOLLOW prevents changing a
        // symlink target's permissions if the endpoint is replaced.
        if unsafe {
            libc::fchmodat(
                libc::AT_FDCWD,
                path_string.as_ptr(),
                0o600,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        listener.set_nonblocking(true)?;
        // A host connection captures the kernel identity of the binder before
        // any client is authorized. It sends no request and is closed here.
        let token = PeerToken::read(&UnixStream::connect(path)?)?;
        let state = Arc::new(AgentState {
            active: AtomicBool::new(true),
            authority: self.authority.clone(),
            agent: agent.to_owned(),
            pool: SocketPool::new(self.policy.clone()),
            control: Mutex::new(Vec::new()),
            control_port,
        });
        let worker_state = state.clone();
        let worker = thread::Builder::new()
            .name(format!("socket-{agent}"))
            .spawn(move || accept_loop(listener, worker_state))?;
        Ok(AgentService {
            state,
            worker: Some(worker),
            endpoint,
            token,
        })
    }
}

impl AgentService {
    pub fn endpoint(&self) -> &Path {
        &self.endpoint.path
    }
    pub fn peer_token(&self) -> PeerToken {
        self.token
    }
    pub fn add_root(&self, root: ProcessIdentity) -> io::Result<()> {
        self.state.authority.add_root(&self.state.agent, root)
    }
    pub fn remove_root(&self, root: ProcessIdentity) -> io::Result<()> {
        self.state.authority.remove_root(&self.state.agent, root)
    }

    /// Control capabilities are supplied only by the host launch path and
    /// retained until shutdown. Test connection requests cannot reach them.
    pub fn control_listener(&self, ip: IpVersion) -> io::Result<std::net::TcpListener> {
        let mut control = self.state.control.lock().map_err(|_| errno(libc::EACCES))?;
        if control.len() >= 2 {
            return Err(errno(libc::ENOSPC));
        }
        let listener = self
            .state
            .pool
            .control_listener(ip, self.state.control_port)?;
        let supplied = listener.try_clone()?;
        control.push(listener);
        Ok(supplied)
    }
}

impl Drop for AgentService {
    fn drop(&mut self) {
        self.state.active.store(false, Ordering::Release);
        let _ = self.state.authority.unregister_agent(&self.state.agent);
        if let Ok(mut control) = self.state.control.lock() {
            control.clear();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn accept_loop(listener: UnixListener, state: Arc<AgentState>) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while state.active.load(Ordering::Acquire) {
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if workers.len() >= MAX_CONNECTIONS {
                    continue;
                }
                let Ok(capability) = state.authority.authorize(&state.agent, &stream) else {
                    continue;
                };
                if stream.set_read_timeout(Some(TICK)).is_err()
                    || stream.set_write_timeout(Some(TICK)).is_err()
                {
                    continue;
                }
                let worker_state = state.clone();
                if let Ok(worker) = thread::Builder::new()
                    .name("socket-client".to_owned())
                    .spawn(move || {
                        let _ = serve(stream, worker_state, capability);
                    })
                {
                    workers.push(worker);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
            Err(_) => break,
        }
    }
    state.active.store(false, Ordering::Release);
    for worker in workers {
        let _ = worker.join();
    }
}

fn live(state: &AgentState, capability: &RootCapability) -> bool {
    state.active.load(Ordering::Acquire) && capability.is_live()
}

fn request(
    stream: &mut UnixStream,
    state: &AgentState,
    capability: &RootCapability,
    idle_lease: bool,
) -> io::Result<Request> {
    let mut frame = [0; REQUEST_SIZE];
    let mut offset = 0;
    let mut deadline = (!idle_lease).then(|| Instant::now() + FRAME_TIMEOUT);
    while live(state, capability) {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(errno(libc::ETIMEDOUT));
        }
        match stream.read(&mut frame[offset..]) {
            Ok(0) => return Err(errno(libc::ECONNRESET)),
            Ok(count) => {
                if deadline.is_none() {
                    deadline = Some(Instant::now() + FRAME_TIMEOUT);
                }
                offset += count;
                if offset == frame.len() {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        return Err(errno(libc::ETIMEDOUT));
                    }
                    return Request::decode(&frame);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(errno(libc::ETIMEDOUT));
        }
    }
    Err(errno(libc::EACCES))
}

fn serve(
    mut stream: UnixStream,
    state: Arc<AgentState>,
    capability: RootCapability,
) -> io::Result<()> {
    let initial = request(&mut stream, &state, &capability, false)?;
    let lease = match initial {
        Request::Bind {
            ip,
            port,
            reuse_address,
        } => match state.pool.bind(ip, port, reuse_address) {
            Ok(lease) => {
                send(
                    &stream,
                    &state,
                    &capability,
                    Reply {
                        operation: Operation::Bind,
                        errno: 0,
                        ip: Some(ip),
                        port: lease.port(),
                        lease: lease.id(),
                        descriptors: 1,
                    },
                    Some(lease.descriptor().as_raw_fd()),
                )?;
                lease
            }
            Err(error) => {
                send_error(&stream, &state, &capability, Operation::Bind, error)?;
                return Ok(());
            }
        },
        Request::Connect { ip, port } => {
            match state.pool.connect(ip, port) {
                Ok(connection) => send(
                    &stream,
                    &state,
                    &capability,
                    Reply {
                        operation: Operation::Connect,
                        errno: 0,
                        ip: Some(ip),
                        port,
                        lease: 0,
                        descriptors: 1,
                    },
                    Some(connection.as_raw_fd()),
                )?,
                Err(error) => send_error(&stream, &state, &capability, Operation::Connect, error)?,
            }
            return Ok(());
        }
        Request::Listen { .. } | Request::Release { .. } => {
            send_error(
                &stream,
                &state,
                &capability,
                operation(initial),
                errno(libc::EINVAL),
            )?;
            return Ok(());
        }
    };
    serve_lease(&mut stream, &state, &capability, lease)
}

fn serve_lease(
    stream: &mut UnixStream,
    state: &AgentState,
    capability: &RootCapability,
    lease: Arc<SocketLease>,
) -> io::Result<()> {
    loop {
        let next = request(stream, state, capability, true)?;
        match next {
            Request::Listen { lease: id, backlog } if id == lease.id() => {
                match lease.listen(backlog) {
                    Ok(()) => send(
                        stream,
                        state,
                        capability,
                        Reply {
                            operation: Operation::Listen,
                            errno: 0,
                            ip: None,
                            port: 0,
                            lease: id,
                            descriptors: 0,
                        },
                        None,
                    )?,
                    Err(error) => send_error(stream, state, capability, Operation::Listen, error)?,
                }
            }
            Request::Release { lease: id } if id == lease.id() => {
                send(
                    stream,
                    state,
                    capability,
                    Reply {
                        operation: Operation::Release,
                        errno: 0,
                        ip: None,
                        port: 0,
                        lease: id,
                        descriptors: 0,
                    },
                    None,
                )?;
                return Ok(());
            }
            _ => {
                send_error(
                    stream,
                    state,
                    capability,
                    operation(next),
                    errno(libc::EINVAL),
                )?;
                return Ok(());
            }
        }
    }
}

fn operation(request: Request) -> Operation {
    match request {
        Request::Bind { .. } => Operation::Bind,
        Request::Listen { .. } => Operation::Listen,
        Request::Connect { .. } => Operation::Connect,
        Request::Release { .. } => Operation::Release,
    }
}

fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

fn send_error(
    stream: &UnixStream,
    state: &AgentState,
    capability: &RootCapability,
    operation: Operation,
    error: io::Error,
) -> io::Result<()> {
    send(
        stream,
        state,
        capability,
        Reply::error(
            operation,
            error
                .raw_os_error()
                .filter(|code| *code > 0)
                .unwrap_or(libc::EIO),
        )?,
        None,
    )
}

fn send(
    stream: &UnixStream,
    state: &AgentState,
    capability: &RootCapability,
    reply: Reply,
    descriptor: Option<i32>,
) -> io::Result<()> {
    if !live(state, capability) {
        return Err(errno(libc::EACCES));
    }
    let frame = reply.encode()?;
    let iov = [IoSlice::new(&frame)];
    let descriptors = descriptor.into_iter().collect::<Vec<_>>();
    let controls = if descriptors.is_empty() {
        Vec::new()
    } else {
        vec![ControlMessage::ScmRights(&descriptors)]
    };
    let sent = sendmsg::<()>(
        stream.as_raw_fd(),
        &iov,
        &controls,
        MsgFlags::from_bits_retain(libc::MSG_NOSIGNAL),
        None,
    )
    .map_err(io::Error::from)?;
    if sent != frame.len() {
        return Err(errno(libc::EIO));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{pool::PortConfiguration, REPLY_SIZE};
    use super::*;
    use nix::sys::socket::{recvmsg, ControlMessageOwned};
    use std::{
        io::{IoSliceMut, Write},
        net::{Ipv4Addr, TcpListener, TcpStream},
        os::fd::{FromRawFd, OwnedFd, RawFd},
    };

    struct Fixture {
        service: AgentService,
        _hub: BrokerHub,
        _directory: TestDirectory,
        root: ProcessIdentity,
        control_port: u16,
    }

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = PathBuf::from(format!("/tmp/sms-{}-{nonce}", std::process::id()));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> Fixture {
        // Use a short private path even when the isolated test launcher has a
        // long TMPDIR. The host may provide an equivalent immutable alias.
        let directory = TestDirectory::new();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let control_port = loop {
            let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = probe.local_addr().unwrap().port();
            if ![22000, 23000, 24000, 24001].contains(&port) {
                break port;
            }
        };
        let policy = PortPolicy::new(PortConfiguration {
            agent_control: control_port..=control_port,
            gateway: 22000..=22000,
            egress: 23000..=23000,
            model: 24000,
            judge: 24001,
        })
        .unwrap();
        let hub = BrokerHub::new(policy);
        let service = hub
            .register_agent(
                "one",
                control_port,
                directory.path(),
                &directory.path().join("s"),
            )
            .unwrap();
        let root = ProcessIdentity::capture(std::process::id()).unwrap();
        service.add_root(root).unwrap();
        Fixture {
            service,
            _hub: hub,
            _directory: directory,
            root,
            control_port,
        }
    }

    fn client(service: &AgentService) -> UnixStream {
        let stream = UnixStream::connect(service.endpoint()).unwrap();
        service.peer_token().verify(&stream).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
    }

    fn exchange(stream: &mut UnixStream, request: Request) -> (Reply, Vec<OwnedFd>) {
        stream.write_all(&request.encode().unwrap()).unwrap();
        receive(stream)
    }

    fn receive(stream: &mut UnixStream) -> (Reply, Vec<OwnedFd>) {
        let mut frame = [0; REPLY_SIZE];
        let mut iov = [IoSliceMut::new(&mut frame)];
        let mut control = nix::cmsg_space!([RawFd; 4]);
        let message = recvmsg::<()>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut control),
            MsgFlags::empty(),
        )
        .unwrap();
        assert!(!message.flags.contains(MsgFlags::MSG_CTRUNC));
        let count = message.bytes;
        assert_ne!(count, 0);
        let mut descriptors = Vec::new();
        for control in message.cmsgs().unwrap() {
            if let ControlMessageOwned::ScmRights(fds) = control {
                for fd in fds {
                    // SAFETY: SCM_RIGHTS installed a new owned descriptor.
                    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
                    // SAFETY: set a valid flag on this live descriptor.
                    assert_ne!(
                        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                        -1
                    );
                    descriptors.push(owned);
                }
            }
        }
        stream.read_exact(&mut frame[count..]).unwrap();
        let reply = Reply::decode(&frame).unwrap();
        assert_eq!(usize::from(reply.descriptors), descriptors.len());
        (reply, descriptors)
    }

    #[test]
    fn transferred_sockets_activate_and_connect_only_after_listen() {
        let fixture = fixture();
        for ip in [IpVersion::V4, IpVersion::V6] {
            let mut control = client(&fixture.service);
            let (bound, mut fds) = exchange(
                &mut control,
                Request::Bind {
                    ip,
                    port: 0,
                    reuse_address: false,
                },
            );
            assert_eq!(bound.errno, 0);
            let listener = TcpListener::from(fds.pop().unwrap());
            let mut premature = client(&fixture.service);
            let (reply, fds) = exchange(
                &mut premature,
                Request::Connect {
                    ip,
                    port: bound.port,
                },
            );
            assert_eq!(reply.errno, libc::ECONNREFUSED);
            assert!(fds.is_empty());
            let (reply, _) = exchange(
                &mut control,
                Request::Listen {
                    lease: bound.lease,
                    backlog: 8,
                },
            );
            assert_eq!(reply.errno, 0);
            let mut connection = client(&fixture.service);
            let (reply, mut fds) = exchange(
                &mut connection,
                Request::Connect {
                    ip,
                    port: bound.port,
                },
            );
            assert_eq!(reply.errno, 0);
            let mut outgoing = TcpStream::from(fds.pop().unwrap());
            outgoing.write_all(b"proof").unwrap();
            let (mut incoming, _) = listener.accept().unwrap();
            incoming
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut proof = [0; 5];
            incoming.read_exact(&mut proof).unwrap();
            assert_eq!(&proof, b"proof");
            drop(outgoing);
            drop(incoming);
            drop(listener);
            let (reply, fds) = exchange(&mut control, Request::Release { lease: bound.lease });
            assert_eq!(reply.errno, 0);
            assert!(fds.is_empty());
            assert_eq!(control.read(&mut [0]).unwrap(), 0);
            // EOF is after worker lease destruction, so port reuse is no race.
            let mut rebound = client(&fixture.service);
            let (reply, fds) = exchange(
                &mut rebound,
                Request::Bind {
                    ip,
                    port: bound.port,
                    reuse_address: true,
                },
            );
            assert_eq!(reply.errno, 0);
            drop(fds);
        }
    }

    #[test]
    fn failures_carry_no_descriptors_and_control_is_host_only() {
        let fixture = fixture();
        let _control = fixture.service.control_listener(IpVersion::V4).unwrap();
        for request in [
            Request::Bind {
                ip: IpVersion::V4,
                port: fixture.control_port,
                reuse_address: true,
            },
            Request::Connect {
                ip: IpVersion::V4,
                port: fixture.control_port,
            },
            Request::Connect {
                ip: IpVersion::V4,
                port: 24000,
            },
            Request::Listen {
                lease: 1,
                backlog: 1,
            },
        ] {
            let (reply, fds) = exchange(&mut client(&fixture.service), request);
            assert_ne!(reply.errno, 0);
            assert!(fds.is_empty());
        }
        let mut stream = client(&fixture.service);
        let (bound, fds) = exchange(
            &mut stream,
            Request::Bind {
                ip: IpVersion::V4,
                port: 0,
                reuse_address: false,
            },
        );
        let (reply, extra) = exchange(
            &mut stream,
            Request::Listen {
                lease: bound.lease + 1,
                backlog: 1,
            },
        );
        assert_eq!(reply.errno, libc::EINVAL);
        assert!(extra.is_empty());
        drop(fds);
    }

    #[test]
    fn revocation_closes_idle_lease_and_shutdown_removes_endpoint() {
        let fixture = fixture();
        let path = fixture.service.endpoint().to_owned();
        let mut stream = client(&fixture.service);
        let (_, fds) = exchange(
            &mut stream,
            Request::Bind {
                ip: IpVersion::V4,
                port: 0,
                reuse_address: false,
            },
        );
        fixture.service.remove_root(fixture.root).unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        drop(fds);
        drop(fixture);
        assert!(!path.exists());
    }

    #[test]
    fn malformed_frames_close_without_reply_and_cleanup_preserves_replacement() {
        let fixture = fixture();
        let mut stream = client(&fixture.service);
        stream.write_all(&[0; REQUEST_SIZE]).unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        let path = fixture.service.endpoint().to_owned();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        drop(fixture.service);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn incomplete_requests_time_out_and_unregistered_peers_are_closed() {
        let fixture = fixture();
        let mut stream = client(&fixture.service);
        stream.write_all(b"S").unwrap();
        let start = Instant::now();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        assert!(start.elapsed() < Duration::from_secs(3));
        fixture.service.remove_root(fixture.root).unwrap();
        let mut unregistered = client(&fixture.service);
        assert_eq!(unregistered.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn connection_limit_refuses_excess_peers_and_shutdown_unblocks_workers() {
        let fixture = fixture();
        let mut clients = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            let mut stream = client(&fixture.service);
            stream.write_all(b"S").unwrap();
            clients.push(stream);
        }
        let mut excess = client(&fixture.service);
        assert_eq!(excess.read(&mut [0]).unwrap(), 0);
        let start = Instant::now();
        drop(fixture.service);
        assert!(start.elapsed() < Duration::from_secs(1));
        for mut stream in clients {
            // Partial unread client input can make Unix close report RESET.
            match stream.read(&mut [0]) {
                Ok(count) => assert_eq!(count, 0),
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::ConnectionReset),
            }
        }
    }

    #[test]
    fn existing_endpoint_is_preserved_and_failed_start_releases_registration() {
        let directory = TestDirectory::new();
        let path = directory.path().join("s");
        let hub = BrokerHub::new(
            PortPolicy::new(PortConfiguration {
                agent_control: 21000..=21001,
                gateway: 22000..=22000,
                egress: 23000..=23000,
                model: 24000,
                judge: 24001,
            })
            .unwrap(),
        );
        fs::write(&path, b"existing").unwrap();
        assert!(hub
            .register_agent("one", 21000, directory.path(), &path)
            .is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing");
        fs::remove_file(&path).unwrap();
        let service = hub
            .register_agent("one", 21000, directory.path(), &path)
            .unwrap();
        let mode = fs::symlink_metadata(service.endpoint())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(hub
            .register_agent(
                "two",
                21000,
                directory.path(),
                &directory.path().join("other")
            )
            .is_err());
    }
}
