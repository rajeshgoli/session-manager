use super::{
    gateway::{Gateway, GatewayRegistration},
    Proxy, Registration, FIRST_PORT, LAST_PORT,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixListener},
    sync::watch,
};

#[derive(Serialize, Deserialize)]
enum Request {
    Register(String),
    RegisterGateway(String, std::net::SocketAddr),
    Unregister(String),
    Get(String),
    Release(String),
}
#[derive(Serialize, Deserialize)]
struct Reply {
    registration: Option<Registration>,
    error: Option<String>,
}

/// Host-only runtime interface, independent of opencode. The wall must exclude
/// `directory` from reads/writes and the control socket from agent connections.
/// `executable` is the installed sm-server copy, never a Cargo target artifact.
#[derive(Clone, Serialize, Deserialize)]
pub struct ServiceClient {
    directory: PathBuf,
    executable: PathBuf,
}
impl ServiceClient {
    pub fn new(directory: PathBuf, executable: PathBuf) -> Self {
        Self {
            directory,
            executable,
        }
    }
    pub fn production(executable: PathBuf) -> Self {
        Self::new(
            crate::sessions::expand_home("~/.local/share/claude-sessions/local-egress"),
            executable,
        )
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn register_agent(&self, agent: &str) -> io::Result<Registration> {
        validate_agent(agent)?;
        self.ensure_running()?;
        self.request(Request::Register(agent.into()))?
            .ok_or_else(|| io::Error::other("missing registration"))
    }
    /// Host-only: the upstream is sm's fixed loopback listener. Existing
    /// gateways are immutable until the wall is fully released.
    pub fn register_gateway(
        &self,
        agent: &str,
        upstream: std::net::SocketAddr,
    ) -> io::Result<Registration> {
        validate_agent(agent)?;
        GatewayRegistration {
            port: super::gateway::FIRST_PORT,
            upstream,
        }
        .validate()?;
        self.ensure_running()?;
        self.request(Request::RegisterGateway(agent.into(), upstream))?
            .ok_or_else(|| io::Error::other("missing gateway registration"))
    }
    pub fn stamp_verifier(&self) -> super::gateway::StampVerifier {
        super::gateway::StampVerifier::new(self.directory.clone())
    }
    pub fn unregister_agent(&self, agent: &str) -> io::Result<()> {
        validate_agent(agent)?;
        self.ensure_running()?;
        self.request(Request::Unregister(agent.into()))?;
        Ok(())
    }
    /// Only call after every process using this agent's wall has exited. Unlike
    /// unregister (suspend), release makes its reservation available again.
    pub fn release_agent(&self, agent: &str) -> io::Result<()> {
        validate_agent(agent)?;
        self.ensure_running()?;
        self.request(Request::Release(agent.into()))?;
        Ok(())
    }
    pub fn registration(&self, agent: &str) -> io::Result<Option<Registration>> {
        validate_agent(agent)?;
        self.ensure_running()?;
        self.request(Request::Get(agent.into()))
    }
    fn request(&self, request: Request) -> io::Result<Option<Registration>> {
        let mut stream = UnixStream::connect(self.directory.join("control.sock"))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        stream.write_all(&bytes)?;
        let mut bytes = Vec::new();
        stream.take(4096).read_to_end(&mut bytes)?;
        let reply: Reply = serde_json::from_slice(&bytes)?;
        match reply.error {
            Some(error) => Err(io::Error::other(error)),
            None => Ok(reply.registration),
        }
    }
    /// launchd supervises this service separately from both sm server slots.
    /// Successful idle exit stops it; a crash restarts it with saved listeners.
    pub fn ensure_running(&self) -> io::Result<()> {
        private_directory(&self.directory)?;
        if UnixStream::connect(self.directory.join("control.sock")).is_ok() {
            return Ok(());
        }
        if !cfg!(target_os = "macos") {
            return Err(io::Error::other("local egress service requires launchd"));
        }
        let label = "com.rajeshgoli.sm-local-egress";
        let plist = self.directory.join("service.plist");
        let executable = self.executable.canonicalize()?;
        if executable
            .components()
            .any(|part| part.as_os_str() == "target")
        {
            return Err(io::Error::other(
                "egress service requires an installed executable",
            ));
        }
        let xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>--local-egress-service</string><string>{}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>StandardErrorPath</key><string>{}</string></dict></plist>", xml_escape(&executable.to_string_lossy()), xml_escape(&self.directory.to_string_lossy()), xml_escape(&self.directory.join("service.stderr").to_string_lossy()));
        atomic_write(&plist, xml.as_bytes())?;
        // SAFETY: geteuid has no arguments.
        let domain = format!("gui/{}", unsafe { libc::geteuid() });
        let target = format!("{domain}/{label}");
        let boot = Command::new("/bin/launchctl")
            .args(["bootstrap", &domain])
            .arg(&plist)
            .output()?;
        if !boot.status.success() {
            let start = Command::new("/bin/launchctl")
                .args(["kickstart", &target])
                .output()?;
            if !start.status.success() {
                return Err(io::Error::other(format!(
                    "egress launch failed: {} {}",
                    String::from_utf8_lossy(&boot.stderr),
                    String::from_utf8_lossy(&start.stderr)
                )));
            }
        }
        for _ in 0..50 {
            if UnixStream::connect(self.directory.join("control.sock")).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(io::Error::other("egress service did not become ready"))
    }
}
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub(super) fn validate_agent(agent: &str) -> io::Result<()> {
    if agent.is_empty()
        || agent.len() > 128
        || !agent
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid agent id",
        ));
    }
    Ok(())
}
fn private_directory(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other("service directory must be absolute"));
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no arguments.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "service directory must be private and host owned",
        ));
    }
    Ok(())
}
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(path.parent().unwrap())?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
struct ListenerTask {
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl ListenerTask {
    async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for ListenerTask {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

struct Registry {
    directory: PathBuf,
    records: BTreeMap<String, Registration>,
    listeners: BTreeMap<String, ListenerTask>,
    proxy: Proxy,
    gateway: Gateway,
    gateway_listeners: BTreeMap<String, ListenerTask>,
}
impl Registry {
    async fn open(directory: &Path) -> io::Result<Self> {
        let path = directory.join("registrations.json");
        let records: BTreeMap<String, Registration> = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        let mut ports = std::collections::BTreeSet::new();
        let mut gateway_ports = std::collections::BTreeSet::new();
        for (agent, record) in &records {
            validate_agent(agent)?;
            if let Some(gateway) = &record.gateway {
                gateway.validate()?;
                if !gateway_ports.insert(gateway.port) {
                    return Err(io::Error::other("duplicate saved gateway port"));
                }
            }
            if record.agent_id != *agent
                || !(FIRST_PORT..=LAST_PORT).contains(&record.port)
                || !ports.insert(record.port)
            {
                return Err(io::Error::other("invalid saved egress registration"));
            }
        }
        let mut registry = Self {
            directory: directory.into(),
            records,
            listeners: BTreeMap::new(),
            proxy: Proxy::new(directory)?,
            gateway: Gateway::open(directory)?,
            gateway_listeners: BTreeMap::new(),
        };
        for record in registry.records.values().filter(|r| r.active) {
            if let Some(gateway) = &record.gateway {
                registry.gateway_listeners.insert(
                    record.agent_id.clone(),
                    start_gateway_listener(&record.agent_id, gateway, registry.gateway.clone())
                        .await?,
                );
            }
            registry.listeners.insert(
                record.agent_id.clone(),
                start_listener(record, registry.proxy.clone()).await?,
            );
        }
        Ok(registry)
    }
    fn save(&self) -> io::Result<()> {
        atomic_write(
            &self.directory.join("registrations.json"),
            &serde_json::to_vec(&self.records)?,
        )
    }
    async fn request(&mut self, request: Request) -> io::Result<Option<Registration>> {
        let agent = match &request {
            Request::Register(a)
            | Request::RegisterGateway(a, _)
            | Request::Unregister(a)
            | Request::Get(a)
            | Request::Release(a) => a,
        };
        validate_agent(agent)?;
        match request {
            Request::Get(agent) => Ok(self.records.get(&agent).cloned()),
            Request::Release(agent) => {
                if self.records.get(&agent).is_some_and(|r| r.active) {
                    return Err(io::Error::other(
                        "unregister before releasing an exited wall",
                    ));
                }
                if let Some(old) = self.records.remove(&agent) {
                    if let Err(error) = self.save() {
                        self.records.insert(agent, old);
                        return Err(error);
                    }
                }
                Ok(None)
            }
            Request::Register(agent) => self.register(agent, None).await,
            Request::RegisterGateway(agent, upstream) => self.register(agent, Some(upstream)).await,
            Request::Unregister(agent) => {
                if let Some(old) = self.records.get(&agent).cloned() {
                    self.records.get_mut(&agent).unwrap().active = false;
                    if let Err(error) = self.save() {
                        self.records.insert(agent, old);
                        return Err(error);
                    }
                    if let Some(listener) = self.gateway_listeners.remove(&agent) {
                        listener.shutdown().await;
                    }
                    if let Some(listener) = self.listeners.remove(&agent) {
                        listener.shutdown().await;
                    }
                }
                Ok(None)
            }
        }
    }
    async fn register(
        &mut self,
        agent: String,
        upstream: Option<std::net::SocketAddr>,
    ) -> io::Result<Option<Registration>> {
        let old = self.records.get(&agent).cloned();
        let port = match &old {
            Some(record) => record.port,
            None => (FIRST_PORT..=LAST_PORT)
                .find(|port| !self.records.values().any(|r| r.port == *port))
                .ok_or_else(|| io::Error::other("egress port reservations exhausted"))?,
        };
        let mut gateway = old.as_ref().and_then(|r| r.gateway.clone());
        if let Some(upstream) = upstream {
            if let Some(existing) = &gateway {
                if existing.upstream != upstream {
                    return Err(io::Error::other(
                        "gateway upstream is immutable until release",
                    ));
                }
            } else {
                let port = (super::gateway::FIRST_PORT..=super::gateway::LAST_PORT)
                    .find(|port| {
                        !self
                            .records
                            .values()
                            .any(|r| r.gateway.as_ref().is_some_and(|g| g.port == *port))
                    })
                    .ok_or_else(|| io::Error::other("gateway port reservations exhausted"))?;
                gateway = Some(GatewayRegistration { port, upstream });
            }
        }
        if let Some(gateway) = &gateway {
            gateway.validate()?;
        }
        let record = Registration {
            agent_id: agent.clone(),
            port,
            active: true,
            gateway,
        };
        if old.as_ref() == Some(&record) {
            return Ok(Some(record));
        }
        let proxy_listener = if old.as_ref().is_some_and(|r| r.active) {
            None
        } else {
            Some(start_listener(&record, self.proxy.clone()).await?)
        };
        let gateway_listener = if old
            .as_ref()
            .is_some_and(|r| r.active && r.gateway.is_some())
        {
            None
        } else if let Some(gateway) = &record.gateway {
            Some(start_gateway_listener(&agent, gateway, self.gateway.clone()).await?)
        } else {
            None
        };
        self.records.insert(agent.clone(), record.clone());
        if let Err(error) = self.save() {
            self.records.remove(&agent);
            if let Some(old) = old {
                self.records.insert(agent, old);
            }
            return Err(error);
        }
        if let Some(listener) = proxy_listener {
            self.listeners.insert(agent.clone(), listener);
        }
        if let Some(listener) = gateway_listener {
            self.gateway_listeners.insert(agent, listener);
        }
        Ok(Some(record))
    }
}
async fn start_gateway_listener(
    agent: &str,
    registration: &GatewayRegistration,
    gateway: Gateway,
) -> io::Result<ListenerTask> {
    registration.validate()?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, registration.port)).await?;
    let agent = agent.to_owned();
    let upstream = registration.upstream;
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut workers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stopped.changed() => break,
                Some(_) = workers.join_next(), if !workers.is_empty() => {},
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let gateway = gateway.clone(); let agent = agent.clone(); let stopped = stopped.clone();
                            workers.spawn(async move { gateway.serve(stream, agent, upstream, stopped).await; });
                        }
                        Err(_) => tokio::select! {
                            _ = stopped.changed() => break,
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                        },
                    }
                }
            }
        }
        drop(listener);
        while workers.join_next().await.is_some() {}
    });
    Ok(ListenerTask {
        stop,
        task: Some(task),
    })
}
async fn start_listener(record: &Registration, proxy: Proxy) -> io::Result<ListenerTask> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, record.port)).await?;
    let agent = record.agent_id.clone();
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut workers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stopped.changed() => break,
                Some(_) = workers.join_next(), if !workers.is_empty() => {},
                result = listener.accept() => {
                    let (stream, _) = match result {
                        Ok(connection) => connection,
                        Err(error) => {
                            eprintln!("local egress accept failed for {agent}: {error}");
                            // Descriptor/resource pressure and transient accept
                            // failures must not strand a saved registration.
                            tokio::select! {
                                _ = stopped.changed() => break,
                                _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                            }
                            continue;
                        }
                    };
                    let proxy = proxy.clone(); let agent = agent.clone();
                    let stopped = stopped.clone();
                    workers.spawn(async move { proxy.serve(stream, agent, stopped).await; });
                }
            }
        }
        drop(listener);
        while workers.join_next().await.is_some() {}
    });
    Ok(ListenerTask {
        stop,
        task: Some(task),
    })
}

/// Runs in a separately supervised sm-server process, outside every wall.
/// The exclusive file lock makes stale socket cleanup safe on crash recovery.
pub async fn run_service(directory: &Path) -> io::Result<()> {
    private_directory(directory)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("service.lock"))?;
    // SAFETY: flock receives a valid live descriptor; held until service exits.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = directory.join("control.sock");
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut registry = Registry::open(directory).await?;
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    loop {
        // Give an initial registration and simultaneous host launch time to arrive.
        let connection = tokio::time::timeout(Duration::from_secs(60), listener.accept()).await;
        let (mut stream, _) = match connection {
            Ok(result) => result?,
            Err(_) if registry.listeners.is_empty() => break,
            Err(_) => continue,
        };
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            while bytes.len() < 4096 {
                let b = stream.read_u8().await?;
                if b == b'\n' {
                    return serde_json::from_slice::<Request>(&bytes).map_err(io::Error::from);
                }
                bytes.push(b);
            }
            Err(io::Error::other("control request too large"))
        })
        .await;
        let result = match request {
            Ok(Ok(request)) => registry.request(request).await,
            _ => Err(io::Error::other("invalid control request")),
        };
        let reply = match result {
            Ok(registration) => Reply {
                registration,
                error: None,
            },
            Err(e) => Reply {
                registration: None,
                error: Some(e.to_string()),
            },
        };
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            stream.write_all(&serde_json::to_vec(&reply)?),
        )
        .await;
    }
    fs::remove_file(socket)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reservations_survive_restart_and_retirement_and_environment_is_stable() {
        let dir = super::super::tests::directory();
        let mut registry = Registry::open(&dir).await.unwrap();
        let a = registry
            .request(Request::Register("a".into()))
            .await
            .unwrap()
            .unwrap();
        let b = registry
            .request(Request::Register("b".into()))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(a.port, b.port);
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = upstream_listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(axum::routing::any(
            |headers: axum::http::HeaderMap| async move {
                headers[super::super::gateway::AGENT_HEADER]
                    .to_str()
                    .unwrap()
                    .to_owned()
            },
        ));
        let server = tokio::spawn(async move {
            axum::serve(upstream_listener, app).await.unwrap();
        });
        let a = registry
            .request(Request::RegisterGateway("a".into(), upstream))
            .await
            .unwrap()
            .unwrap();
        let b = registry
            .request(Request::RegisterGateway("b".into(), upstream))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            a.gateway.as_ref().unwrap().port,
            b.gateway.as_ref().unwrap().port
        );
        let occupied = TcpListener::bind((
            std::net::Ipv4Addr::LOCALHOST,
            super::super::gateway::FIRST_PORT + 2,
        ))
        .await
        .unwrap();
        assert!(registry
            .request(Request::RegisterGateway("conflict".into(), upstream))
            .await
            .is_err());
        assert!(!registry.records.contains_key("conflict"));
        drop(occupied);
        let original_key = fs::read(dir.join("gateway.key")).unwrap();
        for record in [&a, &b] {
            let mut stream = tokio::net::TcpStream::connect((
                std::net::Ipv4Addr::LOCALHOST,
                record.gateway.as_ref().unwrap().port,
            ))
            .await
            .unwrap();
            stream.write_all(b"GET /health HTTP/1.1\r\nHost: forged\r\nX-SM-Local-Agent: forged\r\nConnection: close\r\n\r\n").await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            assert!(String::from_utf8(bytes)
                .unwrap()
                .ends_with(&record.agent_id));
        }
        assert_eq!(
            a,
            registry
                .request(Request::Register("a".into()))
                .await
                .unwrap()
                .unwrap()
        );
        for (_, listener) in std::mem::take(&mut registry.listeners) {
            listener.shutdown().await;
        }
        for (_, listener) in std::mem::take(&mut registry.gateway_listeners) {
            listener.shutdown().await;
        }
        drop(registry);
        let mut registry = Registry::open(&dir).await.unwrap();
        assert_eq!(original_key, fs::read(dir.join("gateway.key")).unwrap());
        assert_eq!(
            a,
            registry
                .request(Request::Get("a".into()))
                .await
                .unwrap()
                .unwrap()
        );
        registry
            .request(Request::Unregister("a".into()))
            .await
            .unwrap();
        assert!(tokio::net::TcpStream::connect((
            std::net::Ipv4Addr::LOCALHOST,
            a.gateway.as_ref().unwrap().port
        ))
        .await
        .is_err());
        assert!(registry
            .request(Request::RegisterGateway(
                "a".into(),
                "127.0.0.1:8421".parse().unwrap()
            ))
            .await
            .is_err());
        let c = registry
            .request(Request::Register("c".into()))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(a.port, c.port);
        assert_eq!(
            a,
            registry
                .request(Request::Register("a".into()))
                .await
                .unwrap()
                .unwrap()
        );
        assert!(registry
            .request(Request::Release("a".into()))
            .await
            .is_err());
        registry
            .request(Request::Unregister("a".into()))
            .await
            .unwrap();
        registry
            .request(Request::Release("a".into()))
            .await
            .unwrap();
        let d = registry
            .request(Request::Register("d".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.port, d.port);
        let env = a.environment();
        assert_eq!(env["HTTPS_PROXY"], format!("http://127.0.0.1:{}", a.port));
        assert_eq!(env["GIT_CONFIG_VALUE_0"], "");
        assert_eq!(env["GIT_CONFIG_VALUE_1"], "!gh auth git-credential");
        assert_eq!(env["CARGO_NET_OFFLINE"], "false");
        assert_eq!(
            env["SM_API_URL"],
            format!("http://127.0.0.1:{}", a.gateway.as_ref().unwrap().port)
        );
        server.abort();
        drop(registry);
        fs::remove_dir_all(dir).unwrap();
    }
}
