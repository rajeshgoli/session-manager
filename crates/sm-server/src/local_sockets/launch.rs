//! Trusted host launch binding. A persistent supervisor is the registered
//! process root, so application exec cannot invalidate its kernel identity.

use super::{identity::ProcessIdentity, service::AgentService};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Read, Write},
    net::TcpListener,
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio},
    sync::Arc,
};

pub const CONTROL_FD: i32 = 198;
const GATE_FD: i32 = 199;

/// Host-staged paths must be immutable to the agent, including every ancestor.
/// Generate the profile with the service's broker directory and the same
/// immutable executable roots compiled into the adapter. The service must be
/// shared across provider and queue launches for this agent, not reconstructed
/// from caller environment variables.
pub struct LaunchBinding {
    service: Arc<AgentService>,
    control: Option<TcpListener>,
    profile: PathBuf,
    adapter: PathBuf,
    supervisor: PathBuf,
}

impl LaunchBinding {
    pub fn new(
        service: Arc<AgentService>,
        control: Option<TcpListener>,
        profile: &Path,
        adapter: &Path,
        supervisor: &Path,
    ) -> io::Result<Self> {
        if let Some(control) = &control {
            let address = control.local_addr()?;
            if ![
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ]
            .contains(&address.ip())
                || address.port() != service.control_port()
            {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
        }
        let profile = host_file(profile)?;
        // Reject accidental composition with a standalone file/network wall.
        // The immutable host-generated profile must also confine descendants.
        if !std::fs::read_to_string(&profile)?.lines().any(|line| {
            line == "(deny syscall-unix (syscall-number SYS_setsid SYS_setpgid SYS_posix_spawn))"
        }) {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        Ok(Self {
            service,
            control,
            profile,
            adapter: host_file(adapter)?,
            supervisor: host_file(supervisor)?,
        })
    }

    /// Environment entries are a host-owned allowlist, not inherited host
    /// state. In particular credentials, requester IDs and loader overrides
    /// cannot select an agent or endpoint. The adapter path is always ours.
    pub fn spawn(
        &self,
        executable: &Path,
        arguments: &[OsString],
        environment: &[(OsString, OsString)],
        checkout: &Path,
    ) -> io::Result<RegisteredChild> {
        let (gate, reader) = UnixStream::pair()?;
        // Reserve separate copies above the target descriptors. This avoids
        // dup2 overwriting a source when the host already has many open FDs.
        let control = self
            .control
            .as_ref()
            .map(|listener| duplicate_high(listener.as_raw_fd()))
            .transpose()?;
        let reader = duplicate_high(reader.as_raw_fd())?;
        let control_fd = control.as_ref().map(AsRawFd::as_raw_fd);
        let reader_fd = reader.as_raw_fd();
        let mut command = Command::new(&self.supervisor);
        command.env_clear().current_dir(checkout).process_group(0);
        for (key, value) in environment {
            if key != OsStr::new("DYLD_INSERT_LIBRARIES")
                && key != OsStr::new("SM_WALL_RECOVERY_FD")
            {
                command.env(key, value);
            }
        }
        let mut loader = OsString::from("DYLD_INSERT_LIBRARIES=");
        loader.push(&self.adapter);
        command
            .arg(if control_fd.is_some() {
                "--control"
            } else {
                "--no-control"
            })
            .args([OsStr::new("sandbox-exec"), OsStr::new("-f")])
            .arg(&self.profile)
            .arg("/usr/bin/env")
            .arg(loader)
            .arg(executable)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: only async-signal-safe dup2 is called in the forked child.
        unsafe {
            command.pre_exec(move || {
                if control_fd.is_some_and(|fd| libc::dup2(fd, CONTROL_FD) == -1)
                    || libc::dup2(reader_fd, GATE_FD) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let mut registered = RegisteredChild {
            child,
            gate,
            service: self.service.clone(),
            root: None,
            finished: false,
            status: None,
        };
        // Command::spawn has completed exec of the trusted supervisor. It is
        // blocked on the gate, and it never execs again after registration.
        let root = ProcessIdentity::capture(registered.child.id())?;
        self.service.add_root(root)?;
        registered.root = Some(root);
        registered.gate.write_all(b"R")?;
        Ok(registered)
    }
}

fn host_file(path: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let path = path.canonicalize()?;
    let metadata = path.metadata()?;
    // Shared inode aliases could bypass path-based immutable write denials.
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    Ok(path)
}

fn duplicate_high(fd: i32) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: fcntl returns a new exclusively owned descriptor on success.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, GATE_FD + 1) };
    if copy == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(copy) })
    }
}

/// Host-owned lifetime for a provider or queue process tree. Dropping it kills
/// the process group, reaps the supervisor and revokes the registered root.
pub struct RegisteredChild {
    child: Child,
    gate: UnixStream,
    service: Arc<AgentService>,
    root: Option<ProcessIdentity>,
    finished: bool,
    status: Option<ExitStatus>,
}

impl RegisteredChild {
    pub fn id(&self) -> u32 {
        self.child.id()
    }
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        use std::os::unix::process::ExitStatusExt;
        if let Some(status) = self.status {
            return Ok(status);
        }
        let mut status = [0u8; 4];
        if self.gate.read_exact(&mut status).is_err() {
            kill_group(self.child.id());
            let status = self.child.wait()?;
            if let Some(root) = self.root.take() {
                let _ = self.service.remove_root(root);
            }
            self.finished = true;
            self.status = Some(status);
            return Ok(status);
        }
        self.finish();
        let status = ExitStatus::from_raw(i32::from_ne_bytes(status));
        self.status = Some(status);
        Ok(status)
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        let mut bytes = [0u8; 4];
        // SAFETY: recv writes only into the live four-byte buffer; peek leaves
        // it available for the blocking completion path once all bytes arrive.
        let count = unsafe {
            libc::recv(
                self.gate.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if count == 4 || count == 0 {
            return self.wait().map(Some);
        }
        if count == -1 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
        }
        Ok(None)
    }
    fn finish(&mut self) {
        if !self.finished {
            // Descendants may have outlived the main application. They must
            // not retain socket capabilities or survive a queue completion.
            kill_group(self.child.id());
            let _ = self.child.wait();
            if let Some(root) = self.root.take() {
                let _ = self.service.remove_root(root);
            }
            self.finished = true;
        }
    }
}

fn kill_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: the supervisor's private process group was created at spawn.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

impl Drop for RegisteredChild {
    fn drop(&mut self) {
        if !self.finished {
            self.finish();
        }
    }
}
