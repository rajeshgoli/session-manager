//! The control channel shared by the serving and replacement processes.
use std::{
    fs,
    io::{IoSlice, IoSliceMut, Read, Write},
    net::{SocketAddr, TcpStream},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use nix::{
    cmsg_space,
    poll::{poll, PollFd, PollFlags, PollTimeout},
    sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags},
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

pub const SCHEMA: &str = "sm.handover.v1";
pub const VERSION: u32 = 1;
pub const SOCKET_FILE: &str = "handover.sock";
const MAX_REQUEST_BYTES: usize = 1024;

fn readable_before(stream: &UnixStream, deadline: std::time::Instant) -> Result<bool> {
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let mut descriptors = [PollFd::new(stream.as_fd(), PollFlags::POLLIN)];
        match poll(&mut descriptors, PollTimeout::try_from(remaining)?) {
            Ok(ready) => return Ok(ready > 0),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn read_exact_before(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: std::time::Instant,
) -> Result<()> {
    while !bytes.is_empty() {
        if !readable_before(stream, deadline)? {
            bail!("handover read timed out");
        }
        let count = stream.read(bytes)?;
        if count == 0 {
            bail!("handover peer closed connection");
        }
        bytes = &mut bytes[count..];
    }
    Ok(())
}

#[derive(Clone)]
pub struct Shutdown {
    stopped: Arc<AtomicBool>,
    tx: watch::Sender<bool>,
}

impl Default for Shutdown {
    fn default() -> Self {
        let (tx, _) = watch::channel(false);
        Self {
            stopped: Arc::new(AtomicBool::new(false)),
            tx,
        }
    }
}

impl Shutdown {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.tx.send_replace(true);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.tx.subscribe()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema: String,
    pub version: u32,
    pub pid: u32,
}

impl Request {
    pub fn current() -> Self {
        Self {
            schema: SCHEMA.to_owned(),
            version: VERSION,
            pid: std::process::id(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA || self.version != VERSION {
            bail!(
                "unsupported handover protocol {} version {}",
                self.schema,
                self.version
            );
        }
        if self.pid == 0 {
            bail!("handover peer pid must be positive");
        }
        Ok(())
    }
}

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SOCKET_FILE)
}

/// A new server only reads the existing path. Binding and unlinking are done
/// by the serving server, so `--take-over` does not mutate shared state.
pub fn bind_socket(state_dir: &Path) -> Result<UnixListener> {
    fs::create_dir_all(state_dir)?;
    let path = socket_path(state_dir);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                bail!("handover path is not a socket: {}", path.display());
            }
            if UnixStream::connect(&path).is_ok() {
                bail!("another server owns handover socket {}", path.display());
            }
            fs::remove_file(&path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Connects to the serving slot for `--take-over`. `None` means no server is
/// serving: the socket is missing, or nothing listens on it because the slot
/// that bound it died. launchd relaunches a slot with the arguments it was
/// installed with, so after a crash or reboot the only slot left still asks
/// for a handover; it must cold-start rather than fail forever.
pub fn connect_serving(state_dir: &Path) -> Result<Option<UnixStream>> {
    let path = socket_path(state_dir);
    match UnixStream::connect(&path) {
        Ok(stream) => Ok(Some(stream)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error)
            .with_context(|| format!("cannot connect to serving slot at {}", path.display())),
    }
}

pub fn send_request(stream: &mut UnixStream) -> Result<()> {
    let mut request = serde_json::to_vec(&Request::current())?;
    request.push(b'\n');
    stream.write_all(&request)?;
    Ok(())
}

pub fn read_request(stream: &mut UnixStream) -> Result<Request> {
    let mut body = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        let mut byte = [0];
        read_exact_before(stream, &mut byte, deadline)?;
        if byte[0] == b'\n' {
            break;
        }
        if body.len() >= MAX_REQUEST_BYTES {
            bail!("handover request is too large");
        }
        body.push(byte[0]);
    }
    let request: Request = serde_json::from_slice(&body)?;
    request.validate()?;
    Ok(request)
}

pub fn validate_peer_with<F>(
    stream: &UnixStream,
    request: &Request,
    expected_job_pid: F,
) -> Result<()>
where
    F: FnOnce(&str) -> Result<u32>,
{
    let current_label =
        std::env::var("XPC_SERVICE_NAME").context("handover requires a launchd service label")?;
    validate_peer_for_label(stream, request, &current_label, expected_job_pid)
}

fn validate_peer_for_label<F>(
    stream: &UnixStream,
    request: &Request,
    current_label: &str,
    expected_job_pid: F,
) -> Result<()>
where
    F: FnOnce(&str) -> Result<u32>,
{
    let (uid, pid) = peer_identity(stream)?;
    if uid != unsafe { nix::libc::geteuid() } || pid != request.pid {
        bail!("handover peer identity does not match its request");
    }
    let other_label = match current_label.strip_suffix(".blue") {
        Some(prefix) => format!("{prefix}.green"),
        None => match current_label.strip_suffix(".green") {
            Some(prefix) => format!("{prefix}.blue"),
            None => bail!("handover requires a blue or green launchd slot"),
        },
    };
    if expected_job_pid(&other_label)? != pid {
        bail!("handover peer is not the other launchd slot");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn peer_identity(stream: &UnixStream) -> Result<(u32, u32)> {
    let mut uid = 0;
    let mut gid = 0;
    let result = unsafe { nix::libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut pid = 0_i32;
    let mut len = std::mem::size_of::<i32>() as nix::libc::socklen_t;
    let result = unsafe {
        nix::libc::getsockopt(
            stream.as_raw_fd(),
            nix::libc::SOL_LOCAL,
            nix::libc::LOCAL_PEERPID,
            (&mut pid as *mut i32).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((uid, u32::try_from(pid)?))
}

pub fn peer_pid(stream: &UnixStream) -> Result<u32> {
    Ok(peer_identity(stream)?.1)
}

/// Keep rollback recovery parked until every thread in the replacement is gone.
pub fn wait_peer_exit(pid: u32) -> Result<()> {
    let pid = i32::try_from(pid)?;
    loop {
        if unsafe { nix::libc::kill(pid, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(nix::libc::ESRCH) {
                return Ok(());
            }
            return Err(error.into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "linux")]
fn peer_identity(stream: &UnixStream) -> Result<(u32, u32)> {
    let mut credentials: nix::libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&credentials) as nix::libc::socklen_t;
    let result = unsafe {
        nix::libc::getsockopt(
            stream.as_raw_fd(),
            nix::libc::SOL_SOCKET,
            nix::libc::SO_PEERCRED,
            (&mut credentials as *mut nix::libc::ucred).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((credentials.uid, u32::try_from(credentials.pid)?))
}

#[cfg(target_os = "macos")]
pub fn launchd_job_pid(label: &str) -> Result<u32> {
    macos_launchd::job_pid(label)
}

#[cfg(not(target_os = "macos"))]
pub fn launchd_job_pid(_label: &str) -> Result<u32> {
    bail!("launchd handover is only available on macOS")
}

pub fn spawn_acceptor(
    listener: &UnixListener,
    shutdown: Shutdown,
    sender: mpsc::Sender<UnixStream>,
) -> Result<thread::JoinHandle<()>> {
    let listener = listener.try_clone()?;
    listener.set_nonblocking(true)?;
    Ok(thread::spawn(move || {
        while !shutdown.is_stopped() {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let result = (|| -> Result<()> {
                        let request = read_request(&mut stream)?;
                        validate_peer_with(&stream, &request, launchd_job_pid)?;
                        sender
                            .try_send(stream)
                            .context("handover receiver is busy")?;
                        Ok(())
                    })();
                    if let Err(error) = result {
                        eprintln!("handover request refused: {error:#}");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => eprintln!("handover accept failed: {error}"),
            }
        }
    }))
}

pub fn send_serving(stream: &mut UnixStream) -> Result<()> {
    stream.write_all(b"{\"serving\":true}\n")?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Commit,
    Rollback,
    PeerExited,
}

pub fn send_decision(stream: &mut UnixStream, decision: Decision) -> Result<()> {
    match decision {
        Decision::Commit => stream.write_all(b"{\"commit\":true}\n")?,
        Decision::Rollback => stream.write_all(b"{\"rollback\":true}\n")?,
        Decision::PeerExited => bail!("peer exit is observed, not a decision to send"),
    }
    Ok(())
}

pub fn read_decision(stream: &mut UnixStream) -> Result<Decision> {
    let mut line = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(35);
    loop {
        let mut byte = [0];
        if !readable_before(stream, deadline)? {
            bail!("handover decision timed out");
        }
        if stream.read(&mut byte)? == 0 {
            return Ok(Decision::PeerExited);
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if line.len() > 64 {
            bail!("handover decision is too large");
        }
    }
    match line.as_slice() {
        b"{\"commit\":true}\n" => Ok(Decision::Commit),
        b"{\"rollback\":true}\n" => Ok(Decision::Rollback),
        _ => bail!("invalid handover decision"),
    }
}

pub fn await_peer_close(stream: &mut UnixStream) -> Result<()> {
    if !readable_before(stream, std::time::Instant::now() + Duration::from_secs(2))? {
        bail!("replacement did not close after rollback");
    }
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => Ok(()),
        Ok(_) => bail!("replacement sent data after rollback"),
        Err(error) => Err(error.into()),
    }
}

pub fn await_serving(stream: &mut UnixStream) -> Result<()> {
    let mut response = [0; 17];
    read_exact_before(
        stream,
        &mut response,
        std::time::Instant::now() + Duration::from_secs(30),
    )
    .context("reading serving reply")?;
    if response != *b"{\"serving\":true}\n" {
        bail!("replacement did not confirm serving");
    }
    Ok(())
}

pub fn probe_health(address: SocketAddr) -> Result<()> {
    let probe = SocketAddr::new(
        if address.ip().is_unspecified() {
            "127.0.0.1".parse()?
        } else {
            address.ip()
        },
        address.port(),
    );
    let mut stream = TcpStream::connect_timeout(&probe, Duration::from_secs(1))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")?;
    let mut response = [0; 64];
    let bytes = stream.read(&mut response)?;
    if !response[..bytes].starts_with(b"HTTP/1.1 200") {
        bail!("replacement health check failed");
    }
    Ok(())
}

pub fn await_stable_serving(stream: &mut UnixStream, address: SocketAddr) -> Result<()> {
    await_stable_serving_for(stream, address, Duration::from_secs(30))
}

fn await_stable_serving_for(
    stream: &mut UnixStream,
    address: SocketAddr,
    duration: Duration,
) -> Result<()> {
    await_serving(stream).context("waiting for replacement serving reply")?;
    let deadline = std::time::Instant::now() + duration;
    let mut consecutive_timeouts = 0;
    // Scheduling delays must not let an unchecked replacement pass the window.
    let mut first_probe = true;
    while first_probe || std::time::Instant::now() < deadline {
        first_probe = false;
        let mut byte = [0];
        let check_deadline = std::time::Instant::now() + Duration::from_secs(1);
        if readable_before(stream, check_deadline.min(deadline))? {
            match stream.read(&mut byte) {
                Ok(0) => bail!("replacement closed handover connection"),
                Ok(_) => bail!("replacement sent unexpected handover data"),
                Err(error) => return Err(error).context("reading replacement handover connection"),
            }
        }
        match probe_health(address) {
            Ok(()) => consecutive_timeouts = 0,
            Err(error) => {
                // SO_RCVTIMEO reports WouldBlock on macOS and TimedOut on
                // other platforms. A busy replacement may miss one probe.
                let timed_out = error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
                });
                consecutive_timeouts += 1;
                if !timed_out || consecutive_timeouts >= 3 || std::time::Instant::now() >= deadline
                {
                    return Err(error).context("probing replacement health");
                }
                eprintln!(
                    "replacement health probe timed out ({consecutive_timeouts}/3); \
                     retrying within stability window: {error:#}"
                );
            }
        }
    }
    // The deadline can expire between iterations after a tolerated timeout.
    if consecutive_timeouts != 0 {
        bail!("replacement health did not recover before stability window ended");
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub handed_over: bool,
    pub lan_listener: bool,
}

/// Send the stable listeners in a single message. The LAN listener is optional
/// because certificate provisioning can leave it unavailable. The control
/// socket itself moves too, so a second restart can begin immediately.
pub fn send_listeners(stream: &UnixStream, fds: &[RawFd], lan_listener: bool) -> Result<()> {
    if fds.len() != 3 + usize::from(lan_listener) {
        bail!("handover listener count does not match LAN availability");
    }
    let body = serde_json::to_vec(&Reply {
        handed_over: true,
        lan_listener,
    })?;
    let iov = [IoSlice::new(&body)];
    let sent = sendmsg::<()>(
        stream.as_raw_fd(),
        &iov,
        &[ControlMessage::ScmRights(fds)],
        MsgFlags::empty(),
        None,
    )
    .context("failed to pass handover listeners")?;
    if sent != body.len() {
        bail!("short handover listener message");
    }
    Ok(())
}

/// Receive listeners before starting workers that can spawn child processes.
/// macOS has no atomic close-on-exec receive flag, so startup ordering keeps
/// children from racing the descriptor receipt and the fcntl below.
pub fn receive_listeners(stream: &UnixStream) -> Result<(Vec<OwnedFd>, bool)> {
    if !readable_before(stream, std::time::Instant::now() + Duration::from_secs(15))? {
        bail!("timed out waiting for handover listeners");
    }
    let mut body = [0_u8; 1024];
    let mut iov = [IoSliceMut::new(&mut body)];
    let mut control = cmsg_space!([RawFd; 4]);
    #[cfg(target_os = "linux")]
    let receive_flags = MsgFlags::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let receive_flags = MsgFlags::empty();
    let message = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut control),
        receive_flags,
    )
    .context("failed to receive handover listeners")?;
    if message.flags.contains(MsgFlags::MSG_CTRUNC) {
        bail!("handover listener descriptors were truncated");
    }
    let length = message.bytes;
    let mut fds = Vec::new();
    for item in message
        .cmsgs()
        .context("invalid handover control message")?
    {
        if let ControlMessageOwned::ScmRights(rights) = item {
            for fd in rights {
                // SAFETY: SCM_RIGHTS installs a fresh owned descriptor in this process.
                fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    // SCM_RIGHTS transfers the socket, not its descriptor flags. Own every
    // received descriptor before a fallible operation so errors close them all.
    for fd in &fds {
        nix::fcntl::fcntl(
            fd.as_raw_fd(),
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )
        .context("failed to mark handover listener close-on-exec")?;
    }
    let reply: Reply = serde_json::from_slice(&body[..length])?;
    if !reply.handed_over || fds.len() != 3 + usize::from(reply.lan_listener) {
        bail!("invalid handover listener reply");
    }
    Ok((fds, reply.lan_listener))
}

#[cfg(target_os = "macos")]
mod macos_launchd {
    use anyhow::{bail, Result};
    use std::{
        ffi::{c_void, CString},
        ptr,
    };

    const UTF8: u32 = 0x0800_0100;
    const NUMBER_SINT64: i32 = 4;

    #[link(name = "ServiceManagement", kind = "framework")]
    unsafe extern "C" {
        static kSMDomainUserLaunchd: *const c_void;
        fn SMJobCopyDictionary(domain: *const c_void, label: *const c_void) -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            string: *const i8,
            encoding: u32,
        ) -> *const c_void;
        fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        fn CFGetTypeID(value: *const c_void) -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFNumberGetValue(value: *const c_void, number_type: i32, result: *mut c_void) -> u8;
        fn CFRelease(value: *const c_void);
    }

    pub(super) fn job_pid(label: &str) -> Result<u32> {
        let encoded = CString::new(label)?;
        // SAFETY: CoreFoundation owns the returned references. Every non-null
        // created reference is released after the dictionary lookup.
        unsafe {
            let domain = kSMDomainUserLaunchd;
            if domain.is_null() {
                bail!("user launchd domain is unavailable");
            }
            let label_ref = CFStringCreateWithCString(ptr::null(), encoded.as_ptr(), UTF8);
            if label_ref.is_null() {
                bail!("cannot encode launchd label");
            }
            let job = SMJobCopyDictionary(domain, label_ref);
            CFRelease(label_ref);
            if job.is_null() {
                bail!("launchd job {label} is not registered");
            }
            let key = CFStringCreateWithCString(ptr::null(), c"PID".as_ptr(), UTF8);
            if key.is_null() {
                CFRelease(job);
                bail!("cannot encode launchd PID key");
            }
            let value = CFDictionaryGetValue(job, key);
            CFRelease(key);
            let mut pid: i64 = 0;
            let valid = !value.is_null()
                && CFGetTypeID(value) == CFNumberGetTypeID()
                && CFNumberGetValue(value, NUMBER_SINT64, (&mut pid as *mut i64).cast()) != 0;
            CFRelease(job);
            if !valid || pid <= 0 {
                bail!("launchd job {label} has no valid PID");
            }
            Ok(u32::try_from(pid)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn passes_listeners_and_rejects_wrong_count() {
        let (old, new) = UnixStream::pair().unwrap();
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (authority, _authority_peer) = UnixStream::pair().unwrap();
        let (control, _peer) = UnixStream::pair().unwrap();
        send_listeners(
            &old,
            &[tcp.as_raw_fd(), authority.as_raw_fd(), control.as_raw_fd()],
            false,
        )
        .unwrap();
        let (fds, lan) = receive_listeners(&new).unwrap();
        assert_eq!(fds.len(), 3);
        assert!(!lan);
        for fd in &fds {
            let flags = nix::fcntl::fcntl(fd.as_raw_fd(), nix::fcntl::FcntlArg::F_GETFD).unwrap();
            assert_ne!(
                flags & nix::libc::FD_CLOEXEC,
                0,
                "handed-over listener is inheritable"
            );
        }
        assert!(send_listeners(&old, &[tcp.as_raw_fd()], false).is_err());
    }

    #[test]
    fn take_over_without_a_serving_slot_cold_starts() {
        // Short path: Unix socket paths are limited to 104 bytes on macOS.
        let dir = PathBuf::from(format!("/tmp/sm-ho-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(connect_serving(&dir).unwrap().is_none());

        // A dead slot leaves its socket file behind with nobody listening.
        drop(UnixListener::bind(socket_path(&dir)).unwrap());
        assert!(connect_serving(&dir).unwrap().is_none());

        let listener = bind_socket(&dir).unwrap();
        assert!(connect_serving(&dir).unwrap().is_some());
        drop(listener);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shutdown_notifies_existing_and_future_receivers() {
        let shutdown = Shutdown::default();
        let receiver = shutdown.subscribe();
        shutdown.stop();
        assert!(*receiver.borrow());
        assert!(*shutdown.subscribe().borrow());
        assert!(shutdown.is_stopped());
    }

    #[test]
    fn refuses_a_peer_whose_pid_is_not_the_other_slot() {
        let (server, _client) = UnixStream::pair().unwrap();
        let request = Request::current();
        validate_peer_for_label(&server, &request, "com.example.sm.blue", |label| {
            assert_eq!(label, "com.example.sm.green");
            Ok(std::process::id())
        })
        .unwrap();
        assert!(
            validate_peer_for_label(&server, &request, "com.example.sm.blue", |_| {
                Ok(std::process::id() + 1)
            })
            .is_err()
        );
        let invalid = Request {
            pid: request.pid + 1,
            ..request
        };
        assert!(
            validate_peer_for_label(&server, &invalid, "com.example.sm.blue", |_| {
                Ok(invalid.pid)
            })
            .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_flight_request_finishes_and_waiting_request_reaches_successor() {
        use axum::{routing::get, Router};
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            sync::Notify,
        };

        async fn get_response(address: SocketAddr, path: &str) -> String {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            stream
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut body = Vec::new();
            stream.read_to_end(&mut body).await.unwrap();
            String::from_utf8(body).unwrap()
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let old_router = Router::new()
            .route(
                "/slow",
                get({
                    let entered = entered.clone();
                    let release = release.clone();
                    move || {
                        let entered = entered.clone();
                        let release = release.clone();
                        async move {
                            entered.notify_one();
                            release.notified().await;
                            "old slow"
                        }
                    }
                }),
            )
            .route("/health", get(|| async { "old" }));
        let shutdown = Shutdown::default();
        let mut stopped = shutdown.subscribe();
        let old_listener =
            tokio::net::TcpListener::from_std(listener.try_clone().unwrap()).unwrap();
        let old = tokio::spawn(async move {
            axum::serve(old_listener, old_router)
                .with_graceful_shutdown(async move {
                    let _ = stopped.changed().await;
                })
                .await
                .unwrap();
        });
        let slow = tokio::spawn(get_response(address, "/slow"));
        entered.notified().await;
        shutdown.stop();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let waiting = tokio::spawn(get_response(address, "/health"));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiting.is_finished());
        let new_listener =
            tokio::net::TcpListener::from_std(listener.try_clone().unwrap()).unwrap();
        let new = tokio::spawn(async move {
            axum::serve(
                new_listener,
                Router::new().route("/health", get(|| async { "new" })),
            )
            .await
            .unwrap();
        });
        release.notify_one();
        let slow = tokio::time::timeout(Duration::from_secs(2), slow)
            .await
            .unwrap()
            .unwrap();
        let waiting = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(slow.contains("old slow"), "{slow}");
        assert!(waiting.ends_with("new"), "{waiting}");
        old.await.unwrap();
        new.abort();
    }

    #[test]
    fn slow_health_probe_recovers_without_rollback() {
        let (result, probes) = check_health_sequence(
            &[true, true, false, true, true, false],
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            Duration::from_secs(12),
        );
        result.unwrap();
        assert!(
            probes >= 6,
            "successful probes must reset the timeout count"
        );
    }

    #[test]
    fn three_consecutive_health_timeouts_request_rollback() {
        let (result, probes) = check_health_sequence(
            &[true, true, true],
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            Duration::from_secs(15),
        );
        assert!(result.is_err());
        assert_eq!(probes, 3);
    }

    #[test]
    fn health_timeout_at_window_end_requests_rollback() {
        let (result, probes) = check_health_sequence(
            &[true],
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            Duration::from_millis(10),
        );
        assert!(result.is_err());
        assert_eq!(probes, 1);
    }

    #[test]
    fn unhealthy_response_requests_immediate_rollback() {
        let (result, probes) = check_health_sequence(
            &[],
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            Duration::from_secs(15),
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("health check failed"));
        assert_eq!(probes, 1);
    }

    // Exercise real socket timeouts, including macOS's WouldBlock result.
    // Entries mark responses delayed beyond the production one-second timeout.
    fn check_health_sequence(
        slow: &[bool],
        response: &'static [u8],
        duration: Duration,
    ) -> (Result<()>, usize) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Shutdown::default();
        let stopped = stop.clone();
        let slow = slow.to_vec();
        let server = thread::spawn(move || {
            let mut probes = 0;
            let mut responses = Vec::new();
            while !stopped.is_stopped() {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let delayed = slow.get(probes).copied().unwrap_or(false);
                        // A timed-out response must not delay accepting the next probe.
                        responses.push(thread::spawn(move || {
                            socket
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            socket
                                .set_write_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let mut request = [0; 1];
                            socket.read_exact(&mut request).unwrap();
                            if delayed {
                                thread::sleep(Duration::from_millis(1300));
                            }
                            // The client may already have timed out and closed.
                            let _ = socket.write_all(response);
                        }));
                        probes += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accepting health probe: {error}"),
                }
            }
            for response in responses {
                response.join().unwrap();
            }
            probes
        });
        let (mut old, mut new) = UnixStream::pair().unwrap();
        send_serving(&mut new).unwrap();
        let result = await_stable_serving_for(&mut old, address, duration);
        stop.stop();
        (result, server.join().unwrap())
    }

    #[test]
    fn successor_connection_loss_requests_rollback() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (mut old, mut new) = UnixStream::pair().unwrap();
        send_serving(&mut new).unwrap();
        drop(new);
        let error = await_stable_serving_for(
            &mut old,
            listener.local_addr().unwrap(),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.to_string().contains("closed"), "{error:#}");
    }

    #[test]
    fn loop_from_old_generation_stops_before_new_generation_runs() {
        use std::sync::atomic::AtomicUsize;
        let old = Shutdown::default();
        let new = Shutdown::default();
        let old_passes = Arc::new(AtomicUsize::new(0));
        let old_count = old_passes.clone();
        let old_signal = old.clone();
        let old_worker = thread::spawn(move || {
            while !old_signal.is_stopped() {
                old_count.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(1));
            }
        });
        old.stop();
        old_worker.join().unwrap();
        let before = old_passes.load(Ordering::SeqCst);
        let new_passes = Arc::new(AtomicUsize::new(0));
        let new_count = new_passes.clone();
        let new_signal = new.clone();
        let new_worker = thread::spawn(move || {
            while !new_signal.is_stopped() {
                new_count.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(1));
            }
        });
        thread::sleep(Duration::from_millis(10));
        new.stop();
        new_worker.join().unwrap();
        assert_eq!(old_passes.load(Ordering::SeqCst), before);
        assert!(new_passes.load(Ordering::SeqCst) > 0);
    }

    /// Run with SM_HANDOVER_STATE_COPY pointing at a trimmed, disposable copy
    /// of the live registry. This measures the accept pause including startup
    /// recovery with thirty real session records, without opening live stores.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires SM_HANDOVER_STATE_COPY and reports scratch restart timing"]
    async fn scratch_pause_with_thirty_sessions() {
        use crate::{
            config::AppConfig,
            http::{router, AppState},
        };
        use std::net::TcpListener;

        fn wait_for_health(
            address: SocketAddr,
            queued: Option<std::sync::mpsc::Sender<()>>,
        ) -> Result<Duration> {
            let started = std::time::Instant::now();
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.write_all(
                b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )?;
            if let Some(queued) = queued {
                let _ = queued.send(());
            }
            let mut response = [0; 64];
            let bytes = stream.read(&mut response)?;
            if !response[..bytes].starts_with(b"HTTP/1.1 200") {
                bail!("scratch health request did not succeed");
            }
            Ok(started.elapsed())
        }

        let path = PathBuf::from(std::env::var("SM_HANDOVER_STATE_COPY").unwrap());
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["sessions"].as_array().unwrap().len(), 30);
        let mut config = AppConfig::default();
        config.paths.state_file = path.to_string_lossy().into_owned();
        config.usage.enabled = false;
        config.terminal_direct.lan.enabled = false;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let mut pauses = Vec::new();
        let mut previous: Option<(Shutdown, tokio::task::JoinHandle<()>)> = None;
        for generation in 0..6 {
            let waiting = if let Some((shutdown, task)) = previous.take() {
                shutdown.stop();
                task.await.unwrap();
                let (queued_tx, queued_rx) = std::sync::mpsc::channel();
                let requests = (0..3)
                    .map(|_| {
                        let queued = queued_tx.clone();
                        std::thread::spawn(move || wait_for_health(address, Some(queued)))
                    })
                    .collect::<Vec<_>>();
                for _ in 0..3 {
                    queued_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                }
                Some(requests)
            } else {
                None
            };
            let shutdown = Shutdown::default();
            let state = AppState::try_new(config.clone())
                .unwrap()
                .with_listen_port(address.port())
                .with_shutdown(shutdown.clone());
            let serving = listener.try_clone().unwrap();
            serving.set_nonblocking(true).unwrap();
            let serving = tokio::net::TcpListener::from_std(serving).unwrap();
            let mut stopped = shutdown.subscribe();
            let task = tokio::spawn(async move {
                axum::serve(
                    serving,
                    router(state).into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let _ = stopped.changed().await;
                })
                .await
                .unwrap();
            });
            if let Some(waiting) = waiting {
                for request in waiting {
                    pauses.push(request.join().unwrap().unwrap());
                }
            } else {
                assert!(
                    wait_for_health(address, None).is_ok(),
                    "initial scratch server did not start"
                );
            }
            previous = Some((shutdown, task));
            assert_eq!(generation + 1, pauses.len() / 3 + 1);
        }
        let (shutdown, task) = previous.unwrap();
        shutdown.stop();
        task.await.unwrap();
        pauses.sort();
        let median = pauses[pauses.len() / 2];
        let maximum = *pauses.last().unwrap();
        eprintln!("scratch handover pause: 15 queued requests, median {median:?}, maximum {maximum:?}, failures 0");
        assert!(median < Duration::from_secs(2), "median {median:?}");
        assert!(maximum < Duration::from_secs(4), "maximum {maximum:?}");
    }
}
