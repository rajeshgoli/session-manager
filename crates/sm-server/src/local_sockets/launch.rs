//! Trusted host launch binding. A persistent supervisor is the registered
//! process root, so application exec cannot invalidate its kernel identity.

use super::{
    identity::{snapshot_for_cleanup, ProcessIdentity},
    service::AgentService,
};
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
                || line == "(deny syscall-unix (syscall-number SYS_setsid SYS_posix_spawn))"
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
        self.spawn_inner(executable, arguments, environment, checkout, None)
    }

    pub(crate) fn queue_identity(&self) -> io::Result<(&str, &Path)> {
        if self.control.is_some() {
            return Err(io::Error::other(
                "queue launch must not carry a provider control listener",
            ));
        }
        Ok((self.service.agent_id(), &self.profile))
    }

    pub(crate) fn spawn_queue(
        &self,
        executable: &Path,
        arguments: &[OsString],
        environment: &[(OsString, OsString)],
        checkout: &Path,
        output: (std::fs::File, std::fs::File, Option<u64>),
    ) -> io::Result<RegisteredChild> {
        self.queue_identity()?;
        self.spawn_inner(executable, arguments, environment, checkout, Some(output))
    }

    fn spawn_inner(
        &self,
        executable: &Path,
        arguments: &[OsString],
        environment: &[(OsString, OsString)],
        checkout: &Path,
        output: Option<(std::fs::File, std::fs::File, Option<u64>)>,
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
        command.env_clear().current_dir(checkout);
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
        let ceiling = if let Some((stdout, stderr, ceiling)) = output {
            command
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr));
            ceiling
        } else {
            None
        };
        // SAFETY: setsid, dup2 and setrlimit are async-signal-safe. The private
        // session is created before exec and before applying the guest wall.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1
                    || control_fd.is_some_and(|fd| libc::dup2(fd, CONTROL_FD) == -1)
                    || libc::dup2(reader_fd, GATE_FD) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                if let Some(ceiling) = ceiling {
                    let limit = libc::rlimit {
                        rlim_cur: ceiling,
                        rlim_max: ceiling,
                    };
                    if libc::setrlimit(libc::RLIMIT_NPROC, &limit) != 0 {
                        return Err(io::Error::last_os_error());
                    }
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
/// the private session, reaps the supervisor and revokes the registered root.
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
    #[cfg(test)]
    pub(crate) fn disconnect_host_for_test(mut self) -> io::Result<ExitStatus> {
        self.gate.shutdown(std::net::Shutdown::Both)?;
        let status = self.child.wait()?;
        if let Some(root) = self.root.take() {
            self.service.remove_root(root)?;
        }
        self.finished = true;
        Ok(status)
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        use std::os::unix::process::ExitStatusExt;
        if let Some(status) = self.status {
            self.finish()?;
            return Ok(status);
        }
        let mut status = [0u8; 4];
        if self.gate.read_exact(&mut status).is_err() {
            kill_session(self.child.id())?;
            let status = self.child.wait()?;
            if let Some(root) = self.root.take() {
                let _ = self.service.remove_root(root);
            }
            self.finished = true;
            self.status = Some(status);
            return Ok(status);
        }
        let status = ExitStatus::from_raw(i32::from_ne_bytes(status));
        // Retain the consumed receipt if cleanup fails, so a retry completes
        // cleanup rather than waiting for a second application exit message.
        self.status = Some(status);
        self.finish()?;
        Ok(status)
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_some() {
            return self.wait().map(Some);
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
    fn finish(&mut self) -> io::Result<()> {
        if !self.finished {
            // Descendants may have outlived the main application. They must
            // not retain socket capabilities or survive a queue completion.
            kill_session(self.child.id())?;
            self.child.wait()?;
            if let Some(root) = self.root.take() {
                let _ = self.service.remove_root(root);
            }
            self.finished = true;
        }
        Ok(())
    }
}

fn kill_session(pid: u32) -> io::Result<()> {
    let session = i32::try_from(pid).map_err(|_| io::Error::other("invalid launch PID"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    // The unreaped supervisor reserves the session ID. Freeze its only fork
    // before enumerating, including when cancellation happens during startup.
    // SAFETY: pid belongs to the Child handle and has not been reaped.
    unsafe { libc::kill(session, libc::SIGSTOP) };
    let result = (|| {
        loop {
            let mut receipt: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // WNOWAIT proves our child exited without releasing its PID/session.
            // SAFETY: receipt is initialized C-layout storage; pid is our child.
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    &mut receipt,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
            if receipt.si_pid == session {
                break;
            }
            let root = snapshot_for_cleanup(pid)?;
            if root.exited || root.stopped {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::other("launch supervisor did not stop"));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        loop {
            let mut pids = vec![0_i32; 4096];
            loop {
                // SAFETY: the kernel writes only within this allocated array.
                let count = unsafe {
                    libc::proc_listallpids(
                        pids.as_mut_ptr().cast(),
                        (pids.len() * std::mem::size_of::<i32>()) as i32,
                    )
                };
                if count <= 0 {
                    return Err(io::Error::last_os_error());
                }
                if count as usize >= pids.len() {
                    pids.resize(pids.len() * 2, 0);
                    continue;
                }
                pids.truncate(count as usize);
                break;
            }
            let mut alive = false;
            for member in pids {
                if member <= 0 || member == session {
                    continue;
                }
                // SAFETY: getsid only reads kernel session membership.
                if unsafe { libc::getsid(member) } != session {
                    continue;
                }
                let current = match snapshot_for_cleanup(member as u32) {
                    Ok(current) => current,
                    Err(error) => {
                        if unsafe { libc::getsid(member) } == session {
                            return Err(error);
                        }
                        continue;
                    }
                };
                if current.exited {
                    continue;
                }
                alive = true;
                if current.identity.is_live() && unsafe { libc::getsid(member) } == session {
                    // SAFETY: this live kernel identity is in our private session.
                    if unsafe { libc::kill(member, libc::SIGKILL) } == -1 {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            return Err(error);
                        }
                    }
                }
            }
            if !alive {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::other("launch session did not terminate"));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // SAFETY: the unreaped supervisor still reserves this PID/session ID.
        if unsafe { libc::kill(session, libc::SIGKILL) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    })();
    if result.is_err() {
        // Restore the trusted supervisor so its host-disconnect cleanup can
        // run on Drop. An error never revokes the root as completed.
        unsafe { libc::kill(session, libc::SIGCONT) };
    }
    result
}

impl Drop for RegisteredChild {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Err(error) = self.finish() {
            eprintln!("launch session {} cleanup failed: {error}", self.child.id());
        }
    }
}
