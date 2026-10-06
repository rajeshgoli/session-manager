//! Host-owned HTTPS CONNECT proxy. The listener port, never client input,
//! identifies the agent. Call `ServiceClient` before constructing a wall.
mod networks;
mod service;
pub use service::{run_service, ServiceClient};

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub const FIRST_PORT: u16 = 18700;
pub const LAST_PORT: u16 = 18799;
const REQUEST_LIMIT: usize = 8192;
const IO_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registration {
    pub agent_id: String,
    pub port: u16,
    pub active: bool,
}
impl Registration {
    /// Apply to a cleared launch environment. No token is present. Empty first
    /// credential.helper resets inherited helpers before gh's helper is added.
    pub fn environment(&self) -> BTreeMap<String, String> {
        let proxy = format!("http://127.0.0.1:{}", self.port);
        let mut env = BTreeMap::new();
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            env.insert(key.into(), proxy.clone());
        }
        for key in ["NO_PROXY", "no_proxy"] {
            env.insert(key.into(), "localhost,127.0.0.1,::1".into());
        }
        for (key, value) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "2"),
            ("GIT_CONFIG_KEY_0", "credential.helper"),
            ("GIT_CONFIG_VALUE_0", ""),
            ("GIT_CONFIG_KEY_1", "credential.helper"),
            ("GIT_CONFIG_VALUE_1", "!gh auth git-credential"),
            ("CARGO_NET_OFFLINE", "false"),
        ] {
            env.insert(key.into(), value.into());
        }
        env
    }
}

#[derive(Debug, Serialize)]
struct ConnectionLog {
    time: String,
    agent_id: String,
    host: Option<String>,
    resolved_address: Option<IpAddr>,
    port: Option<u16>,
    bytes_to_host: u64,
    bytes_to_agent: u64,
    duration_ms: u128,
    outcome: String,
    #[serde(skip)]
    established: bool,
}

/// Reject non-public destinations, including IPv4 encoded in IPv6. The extra
/// reserved ranges prevent NAT64/transition addresses from reaching private IPs.
fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_address(IpAddr::V4(v4));
            }
            let s = ip.segments();
            // Only global unicast, excluding documentation and transition space.
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
        }
    }
}

fn parse_request(request: &[u8]) -> Result<(String, u16), &'static str> {
    let text = std::str::from_utf8(request).map_err(|_| "malformed_request")?;
    let mut lines = text.split("\r\n");
    let line = lines.next().ok_or("malformed_request")?;
    let fields: Vec<_> = line.split(' ').collect();
    if fields.first() != Some(&"CONNECT") {
        return Err("connect_only");
    }
    if fields.len() != 3 || !matches!(fields[2], "HTTP/1.0" | "HTTP/1.1") {
        return Err("malformed_request");
    }
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("malformed_request")?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b < 32 && b != b'\t')
        {
            return Err("malformed_request");
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
        {
            return Err("malformed_request");
        }
    }
    let authority = fields[1];
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, port) = bracketed.split_once("]:").ok_or("malformed_request")?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("malformed_request");
        }
        (host, port)
    } else {
        let (host, port) = authority.rsplit_once(':').ok_or("malformed_request")?;
        if host.is_empty()
            || host.len() > 253
            || host.contains(':')
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        {
            return Err("malformed_request");
        }
        (host, port)
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err("malformed_request");
    }
    let port = port.parse::<u16>().map_err(|_| "malformed_request")?;
    if port != 443 {
        return Err("port_not_443");
    }
    Ok((host.into(), port))
}

trait Resolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>>;
}
struct SystemResolver;
impl Resolver for SystemResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>>
    {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host, 443))
                .await?
                .map(|s| s.ip())
                .collect())
        })
    }
}

#[derive(Clone)]
struct Proxy {
    log: Arc<Mutex<std::fs::File>>,
    resolver: Arc<dyn Resolver>,
    capacity: Arc<tokio::sync::Semaphore>,
    networks: Arc<dyn networks::Networks>,
}
impl Proxy {
    fn new(directory: &Path) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("connections.jsonl"))?;
        Ok(Self {
            log: Arc::new(Mutex::new(log)),
            resolver: Arc::new(SystemResolver),
            capacity: Arc::new(tokio::sync::Semaphore::new(256)),
            networks: Arc::new(networks::HostNetworks),
        })
    }
    fn log(&self, record: &ConnectionLog) -> io::Result<()> {
        use std::io::Write;
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.log
            .lock()
            .map_err(|_| io::Error::other("log lock poisoned"))?
            .write_all(&line)
    }
    async fn serve(
        &self,
        mut client: TcpStream,
        agent: String,
        mut stopped: tokio::sync::watch::Receiver<bool>,
    ) {
        let start = Instant::now();
        let mut record = ConnectionLog {
            time: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            agent_id: agent,
            host: None,
            resolved_address: None,
            port: None,
            bytes_to_host: 0,
            bytes_to_agent: 0,
            duration_ms: 0,
            outcome: String::new(),
            established: false,
        };
        let permit = self.capacity.try_acquire();
        let result = if permit.is_err() {
            Err("connection_limit")
        } else if *stopped.borrow() {
            Err("registration_revoked")
        } else {
            tokio::select! {
                result = self.tunnel(&mut client, &mut record) => result,
                _ = stopped.changed() => Err("registration_revoked"),
            }
        };
        record.outcome = match result {
            Ok(()) => "allowed".into(),
            Err(reason) => reason.into(),
        };
        if !record.established {
            let _ = tokio::time::timeout(
                IO_TIMEOUT,
                client.write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                ),
            )
            .await;
        }
        record.duration_ms = start.elapsed().as_millis();
        if let Err(error) = self.log(&record) {
            eprintln!("local egress log failed: {error}");
        }
    }
    async fn tunnel(
        &self,
        client: &mut TcpStream,
        record: &mut ConnectionLog,
    ) -> Result<(), &'static str> {
        let request = tokio::time::timeout(IO_TIMEOUT, async {
            let mut request = Vec::new();
            // Read exactly the header: never consume pipelined TLS bytes.
            while request.len() < REQUEST_LIMIT {
                request.push(client.read_u8().await.map_err(|_| "malformed_request")?);
                if request.ends_with(b"\r\n\r\n") {
                    return Ok(request);
                }
            }
            Err("request_too_large")
        })
        .await
        .map_err(|_| "request_timeout")??;
        // Preserve only a bounded authority for refused requests, never headers.
        if let Ok(text) = std::str::from_utf8(&request) {
            if let Some(authority) = text
                .lines()
                .next()
                .filter(|line| line.starts_with("CONNECT "))
                .and_then(|line| line.split(' ').nth(1))
                .filter(|authority| {
                    authority.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')
                    })
                })
            {
                if let Some((host, port)) = authority.rsplit_once(':') {
                    record.host = Some(host.trim_matches(['[', ']']).chars().take(253).collect());
                    record.port = port.parse().ok();
                }
            }
        }
        let (host, port) = parse_request(&request)?;
        record.host = Some(host.clone());
        record.port = Some(port);
        let addresses = tokio::time::timeout(IO_TIMEOUT, self.resolver.resolve(&host))
            .await
            .map_err(|_| "dns_timeout")?
            .map_err(|_| "dns_failed")?;
        if addresses.is_empty() {
            return Err("dns_empty");
        }
        if let Some(ip) = addresses.iter().find(|ip| !public_address(**ip)) {
            record.resolved_address = Some(*ip);
            return Err("non_public_address");
        }
        let networks = self
            .networks
            .current()
            .map_err(|_| "interface_lookup_failed")?;
        if let Some(ip) = addresses
            .iter()
            .find(|ip| networks.iter().any(|network| network.contains(**ip)))
        {
            record.resolved_address = Some(*ip);
            return Err("local_network_address");
        }
        let mut remote = None;
        for ip in addresses {
            record.resolved_address = Some(ip);
            // Use the checked IP, never resolve the hostname a second time.
            if let Ok(Ok(stream)) =
                tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(SocketAddr::new(ip, port)))
                    .await
            {
                remote = Some(stream);
                break;
            }
        }
        let mut remote = remote.ok_or("connect_failed")?;
        tokio::time::timeout(
            IO_TIMEOUT,
            client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n"),
        )
        .await
        .map_err(|_| "response_timeout")?
        .map_err(|_| "response_failed")?;
        record.established = true;
        // Count partial transfers too, rather than losing counts on a reset.
        let (mut cr, mut cw) = client.split();
        let (mut rr, mut rw) = remote.split();
        let activity = tokio::sync::watch::channel(tokio::time::Instant::now()).0;
        let (up, down) = tokio::join!(
            copy_counted(
                &mut cr,
                &mut rw,
                &mut record.bytes_to_host,
                &activity,
                Duration::from_secs(300)
            ),
            copy_counted(
                &mut rr,
                &mut cw,
                &mut record.bytes_to_agent,
                &activity,
                Duration::from_secs(300)
            )
        );
        if up.is_err() || down.is_err() {
            return Err("tunnel_io_error");
        }
        Ok(())
    }
}
async fn copy_counted<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    count: &mut u64,
    activity: &tokio::sync::watch::Sender<tokio::time::Instant>,
    idle: Duration,
) -> io::Result<()> {
    async {
        let mut buffer = [0; 16384];
        loop {
            let n = with_activity(reader.read(&mut buffer), activity, idle).await?;
            if n == 0 {
                return with_activity(writer.shutdown(), activity, idle).await;
            }
            activity.send_replace(tokio::time::Instant::now());
            let mut offset = 0;
            while offset < n {
                let written =
                    with_activity(writer.write(&buffer[offset..n]), activity, idle).await?;
                if written == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                activity.send_replace(tokio::time::Instant::now());
                *count += written as u64;
                offset += written;
            }
        }
    }
    .await
}

async fn with_activity<T>(
    operation: impl std::future::Future<Output = io::Result<T>>,
    activity: &tokio::sync::watch::Sender<tokio::time::Instant>,
    idle: Duration,
) -> io::Result<T> {
    let mut changes = activity.subscribe();
    tokio::pin!(operation);
    loop {
        let deadline = *changes.borrow_and_update() + idle;
        tokio::select! {
            biased;
            result = &mut operation => return result,
            _ = changes.changed() => {},
            _ = tokio::time::sleep_until(deadline) => {
                // A write can race the timer. Check the shared timestamp again.
                if tokio::time::Instant::now() >= *changes.borrow() + idle {
                    return Err(io::ErrorKind::TimedOut.into());
                }
            }
        }
    }
}
#[cfg(test)]
mod tests;
