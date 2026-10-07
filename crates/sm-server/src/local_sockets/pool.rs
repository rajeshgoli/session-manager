//! Loopback allocations owned by one agent's trusted service.

use super::{IpVersion, MAX_BACKLOG};
use std::{
    collections::{BTreeSet, HashMap},
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream},
    ops::RangeInclusive,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

pub const MAX_LISTENERS: usize = 16;

/// Host-assigned port ranges and single service ports. These never become
/// test-listener capabilities, including ranges allocated to future agents.
pub struct PortConfiguration {
    pub agent_control: RangeInclusive<u16>,
    pub gateway: RangeInclusive<u16>,
    pub egress: RangeInclusive<u16>,
    pub model: u16,
    pub judge: u16,
}

pub struct PortPolicy {
    agent_control: RangeInclusive<u16>,
    reserved: BTreeSet<u16>,
}

impl PortPolicy {
    pub fn new(configuration: PortConfiguration) -> io::Result<Self> {
        let mut reserved = BTreeSet::from([8420, 8443]);
        for range in [
            &configuration.agent_control,
            &configuration.gateway,
            &configuration.egress,
        ] {
            if range.is_empty() || *range.start() == 0 {
                return Err(error(libc::EINVAL));
            }
            for port in range.clone() {
                if [1234, 1235, 1236, 8000].contains(&port) || !reserved.insert(port) {
                    return Err(error(libc::EINVAL));
                }
            }
        }
        for port in [configuration.model, configuration.judge] {
            if port == 0 || !reserved.insert(port) {
                return Err(error(libc::EINVAL));
            }
        }
        // Model/judge services may use their established ports (8000 and
        // 1234-1236); these stay forbidden to test allocations either way.
        reserved.extend([1234, 1235, 1236, 8000]);
        Ok(Self {
            agent_control: configuration.agent_control,
            reserved,
        })
    }

    pub fn permits_control(&self, port: u16) -> bool {
        self.agent_control.contains(&port)
    }

    fn permits_test(&self, port: u16) -> bool {
        port != 0 && !self.reserved.contains(&port)
    }
}

#[derive(Default)]
struct Allocations {
    next_lease: u64,
    sockets: HashMap<(IpVersion, u16), Weak<SocketLease>>,
}

/// One pool per agent. Only this pool's retained live test leases can be used
/// as connection destinations; a port number alone grants no access.
pub struct SocketPool {
    policy: Arc<PortPolicy>,
    allocations: Mutex<Allocations>,
}

pub struct SocketLease {
    listener: TcpListener,
    ip: IpVersion,
    port: u16,
    id: u64,
    listening: AtomicBool,
}

impl SocketPool {
    pub fn new(policy: Arc<PortPolicy>) -> Self {
        Self {
            policy,
            allocations: Mutex::new(Allocations::default()),
        }
    }

    pub fn bind(
        &self,
        ip: IpVersion,
        port: u16,
        reuse_address: bool,
    ) -> io::Result<Arc<SocketLease>> {
        if port != 0 && !self.policy.permits_test(port) {
            return Err(error(libc::EACCES));
        }
        let mut allocations = self.allocations.lock().map_err(|_| error(libc::EACCES))?;
        allocations
            .sockets
            .retain(|_, socket| socket.strong_count() != 0);
        if allocations.sockets.len() >= MAX_LISTENERS {
            return Err(error(libc::ENOSPC));
        }
        // The kernel selects ephemeral ports without consulting the host's
        // service map. Retry boundedly rather than loan a reserved port.
        for _ in 0..32 {
            let listener = bound_socket(ip, port, reuse_address)?;
            let assigned = listener.local_addr()?.port();
            if !self.policy.permits_test(assigned) {
                continue;
            }
            if allocations.sockets.contains_key(&(ip, assigned)) {
                return Err(error(libc::EADDRINUSE));
            }
            allocations.next_lease = allocations
                .next_lease
                .checked_add(1)
                .ok_or_else(|| error(libc::ENOSPC))?;
            let lease = Arc::new(SocketLease {
                listener,
                ip,
                port: assigned,
                id: allocations.next_lease,
                listening: AtomicBool::new(false),
            });
            allocations
                .sockets
                .insert((ip, assigned), Arc::downgrade(&lease));
            return Ok(lease);
        }
        Err(error(libc::EADDRINUSE))
    }

    pub fn connect(&self, ip: IpVersion, port: u16) -> io::Result<TcpStream> {
        let lease = self.retain(ip, port)?;
        if !lease.listening.load(Ordering::Acquire) {
            return Err(error(libc::ECONNREFUSED));
        }
        // Keep the actual listener alive throughout connect. A client cannot
        // release/rebind the port and redirect this request to a new service.
        let connection = TcpStream::connect_timeout(&address(ip, port), Duration::from_secs(1));
        drop(lease);
        connection
    }

    /// Acquire an existing allocation for a forked or restored descriptor.
    /// This never binds a new socket and never reaches a provider control.
    pub fn retain(&self, ip: IpVersion, port: u16) -> io::Result<Arc<SocketLease>> {
        if !self.policy.permits_test(port) {
            return Err(error(libc::EACCES));
        }
        let lease = self
            .allocations
            .lock()
            .map_err(|_| error(libc::EACCES))?
            .sockets
            .get(&(ip, port))
            .and_then(Weak::upgrade)
            .ok_or_else(|| error(libc::EACCES))?;
        Ok(lease)
    }

    /// Host-only control listener; never inserted in the test allocation map.
    pub fn control_listener(&self, ip: IpVersion, port: u16) -> io::Result<TcpListener> {
        if !self.policy.permits_control(port) {
            return Err(error(libc::EACCES));
        }
        // Restore must survive closed connections in TIME_WAIT. Exact loopback
        // binding without SO_REUSEPORT still excludes another active listener.
        let listener = bound_socket(ip, port, true)?;
        activate(&listener, 128)?;
        Ok(listener)
    }
}

impl SocketLease {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn ip(&self) -> IpVersion {
        self.ip
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn descriptor(&self) -> &TcpListener {
        &self.listener
    }

    pub fn listen(&self, backlog: u16) -> io::Result<()> {
        if backlog > MAX_BACKLOG {
            return Err(error(libc::EINVAL));
        }
        activate(&self.listener, backlog)?;
        self.listening.store(true, Ordering::Release);
        Ok(())
    }
}

fn address(ip: IpVersion, port: u16) -> SocketAddr {
    SocketAddr::new(
        match ip {
            IpVersion::V4 => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpVersion::V6 => IpAddr::V6(Ipv6Addr::LOCALHOST),
        },
        port,
    )
}

fn error(errno: i32) -> io::Error {
    io::Error::from_raw_os_error(errno)
}

fn activate(listener: &TcpListener, backlog: u16) -> io::Result<()> {
    // SAFETY: a live TCP descriptor and bounded integer backlog are passed.
    if unsafe { libc::listen(listener.as_raw_fd(), i32::from(backlog)) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn bound_socket(ip: IpVersion, port: u16, reuse_address: bool) -> io::Result<TcpListener> {
    let family = match ip {
        IpVersion::V4 => libc::AF_INET,
        IpVersion::V6 => libc::AF_INET6,
    };
    // SAFETY: socket has no pointer arguments. Ownership is acquired exactly
    // once after checking the returned descriptor.
    let raw = unsafe { libc::socket(family, libc::SOCK_STREAM, libc::IPPROTO_TCP) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: fcntl takes a live descriptor and the defined descriptor flag.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    set_option(
        &fd,
        libc::SOL_SOCKET,
        libc::SO_REUSEADDR,
        i32::from(reuse_address),
    )?;
    let result = match ip {
        IpVersion::V4 => {
            let socket_address = libc::sockaddr_in {
                sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                sin_family: libc::AF_INET as u8,
                sin_port: port.to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes([127, 0, 0, 1]),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: a correctly initialized C-layout IPv4 record and exact
            // size remain live throughout bind.
            unsafe {
                libc::bind(
                    raw,
                    (&socket_address as *const libc::sockaddr_in).cast(),
                    std::mem::size_of_val(&socket_address) as libc::socklen_t,
                )
            }
        }
        IpVersion::V6 => {
            set_option(&fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 1)?;
            let socket_address = libc::sockaddr_in6 {
                sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                sin6_family: libc::AF_INET6 as u8,
                sin6_port: port.to_be(),
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr {
                    s6_addr: Ipv6Addr::LOCALHOST.octets(),
                },
                sin6_scope_id: 0,
            };
            // SAFETY: the IPv6 record and exact size remain live during bind.
            unsafe {
                libc::bind(
                    raw,
                    (&socket_address as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of_val(&socket_address) as libc::socklen_t,
                )
            }
        }
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(TcpListener::from(fd))
}

fn set_option(fd: &OwnedFd, level: i32, option: i32, value: i32) -> io::Result<()> {
    // SAFETY: value is an initialized integer of the exact option size.
    if unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            option,
            (&value as *const i32).cast(),
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn configuration() -> PortConfiguration {
        PortConfiguration {
            agent_control: 21000..=21010,
            gateway: 22000..=22010,
            egress: 23000..=23010,
            model: 24000,
            judge: 24001,
        }
    }
    fn pool() -> SocketPool {
        SocketPool::new(Arc::new(PortPolicy::new(configuration()).unwrap()))
    }

    #[test]
    fn control_restore_reuses_closed_connections_but_excludes_live_listeners() {
        let pool = pool();
        let listener = pool.control_listener(IpVersion::V4, 21000).unwrap();
        assert!(pool.control_listener(IpVersion::V4, 21000).is_err());
        let mut client = TcpStream::connect(address(IpVersion::V4, 21000)).unwrap();
        let (server, _) = listener.accept().unwrap();
        drop(server);
        let mut byte = [0];
        assert_eq!(client.read(&mut byte).unwrap(), 0);
        drop(client);
        drop(listener);
        // EOF precedes completion of the TCP close handshake. Rebinding is
        // allowed once it settles, without waiting for TIME_WAIT to expire.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let restored = loop {
            match pool.control_listener(IpVersion::V4, 21000) {
                Ok(listener) => break listener,
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::EADDRINUSE));
                    assert!(std::time::Instant::now() < deadline, "{error}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        assert!(pool.control_listener(IpVersion::V4, 21000).is_err());
        drop(restored);
    }

    #[test]
    fn bind_listen_connect_and_release_ipv4_ipv6() {
        for ip in [IpVersion::V4, IpVersion::V6] {
            let pool = pool();
            let lease = pool.bind(ip, 0, false).unwrap();
            let port = lease.port();
            assert_ne!(port, 0);
            assert!(pool.connect(ip, port).is_err());
            assert!(
                TcpStream::connect_timeout(&address(ip, port), Duration::from_millis(50)).is_err()
            );
            lease.listen(8).unwrap();
            let mut client = pool.connect(ip, port).unwrap();
            let (mut server, _) = lease.descriptor().accept().unwrap();
            client.write_all(b"own").unwrap();
            let mut bytes = [0; 3];
            server.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"own");
            drop(client);
            drop(server);
            drop(lease);
            assert!(pool.connect(ip, port).is_err());
            let rebound = pool.bind(ip, port, true).unwrap();
            assert_eq!(rebound.port(), port);
        }
    }

    #[test]
    fn reserved_ports_cross_agent_and_unregistered_listener_are_denied() {
        let one = pool();
        let two = pool();
        for port in [8420, 8443, 1234, 8000, 21000, 22005, 23010, 24000, 24001] {
            assert_eq!(
                one.bind(IpVersion::V4, port, true)
                    .err()
                    .unwrap()
                    .raw_os_error(),
                Some(libc::EACCES)
            );
            assert_eq!(
                one.connect(IpVersion::V4, port).unwrap_err().raw_os_error(),
                Some(libc::EACCES)
            );
        }
        let lease = one.bind(IpVersion::V4, 0, false).unwrap();
        lease.listen(8).unwrap();
        assert!(two.connect(IpVersion::V4, lease.port()).is_err());
        let unrelated = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(one
            .connect(IpVersion::V4, unrelated.local_addr().unwrap().port())
            .is_err());
    }

    #[test]
    fn live_allocation_limit_recovers_after_release() {
        let pool = pool();
        let mut leases = (0..MAX_LISTENERS)
            .map(|_| pool.bind(IpVersion::V4, 0, false).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            pool.bind(IpVersion::V4, 0, false)
                .err()
                .unwrap()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );
        leases.pop();
        assert!(pool.bind(IpVersion::V4, 0, false).is_ok());
    }

    #[test]
    fn retaining_existing_listener_preserves_lifetime_without_allocating_ports() {
        let pool = pool();
        let foreign = SocketPool::new(pool.policy.clone());
        let initial = pool.bind(IpVersion::V4, 0, false).unwrap();
        let port = initial.port();
        let id = initial.id();
        assert!(foreign.retain(IpVersion::V4, port).is_err());
        let retained = pool.retain(IpVersion::V4, port).unwrap();
        assert_eq!(retained.id(), id);
        drop(initial);
        assert!(pool.bind(IpVersion::V4, port, false).is_err());
        retained.listen(8).unwrap();
        assert!(pool.connect(IpVersion::V4, port).is_ok());
        drop(retained);
        assert!(pool.retain(IpVersion::V4, port).is_err());
        assert!(pool.retain(IpVersion::V4, 8420).is_err());
    }

    #[test]
    fn port_policy_rejects_overlaps_and_invalid_ranges() {
        let mut default_model = configuration();
        default_model.model = 8000;
        default_model.judge = 8441;
        let default_policy = PortPolicy::new(default_model).unwrap();
        assert!(!default_policy.permits_test(8000));
        assert!(!default_policy.permits_test(8441));
        let mut lm_studio = configuration();
        lm_studio.model = 1234;
        assert!(PortPolicy::new(lm_studio).is_ok());
        let mut config = configuration();
        config.egress = 22010..=23000;
        assert!(PortPolicy::new(config).is_err());
        let mut config = configuration();
        config.model = config.judge;
        assert!(PortPolicy::new(config).is_err());
        let mut config = configuration();
        config.gateway = 0..=1;
        assert!(PortPolicy::new(config).is_err());
        let mut config = configuration();
        config.agent_control = 8400..=8500;
        assert!(PortPolicy::new(config).is_err());
    }
}
