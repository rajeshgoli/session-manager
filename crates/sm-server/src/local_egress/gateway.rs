//! Signed, fixed-destination HTTP gateways. The shared signing key never travels
//! over HTTP. #2008 consumes `StampVerifier` before route-specific authorization.
use super::Registration;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    io::{self, Read, Write},
    net::SocketAddr,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpStream, sync::watch};

pub const FIRST_PORT: u16 = 18600;
pub const LAST_PORT: u16 = 18699;
pub const AGENT_HEADER: &str = "x-sm-local-agent";
pub const SIGNATURE_HEADER: &str = "x-sm-gateway-signature";
const MAX_REQUEST: usize = 8 * 1024 * 1024;
const MAX_RESPONSE: usize = 64 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayRegistration {
    pub port: u16,
    pub upstream: SocketAddr,
}
impl GatewayRegistration {
    pub(super) fn validate(&self) -> io::Result<()> {
        if !(FIRST_PORT..=LAST_PORT).contains(&self.port)
            || !self.upstream.ip().is_loopback()
            || self.upstream.port() == 0
            || (FIRST_PORT..=LAST_PORT).contains(&self.upstream.port())
            || (super::FIRST_PORT..=super::LAST_PORT).contains(&self.upstream.port())
        {
            return Err(io::Error::other("invalid gateway port or sm upstream"));
        }
        Ok(())
    }
}

/// Only the verifier can construct this identity. A header alone is never one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedLocalAgent(String);
impl VerifiedLocalAgent {
    #[cfg(test)]
    pub(crate) fn test_identity(agent: &str) -> Self {
        Self(agent.into())
    }
    pub fn agent_id(&self) -> &str {
        &self.0
    }
}

/// Host-only directory, excluded from *every* agent wall. Reloads the active
/// registrations per request so suspension/release revokes stamps immediately.
/// This synchronous disk check belongs on a blocking worker in HTTP middleware.
#[derive(Clone)]
pub struct StampVerifier {
    directory: PathBuf,
}
impl StampVerifier {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
    pub fn verify(
        &self,
        peer: SocketAddr,
        headers: &HeaderMap,
        method: &Method,
        uri: &Uri,
        body: &[u8],
    ) -> io::Result<Option<VerifiedLocalAgent>> {
        if !peer.ip().is_loopback() {
            return Ok(None);
        }
        let (Some(agent), Some(stamp)) = (
            single_header(headers, AGENT_HEADER),
            single_header(headers, SIGNATURE_HEADER),
        ) else {
            return Ok(None);
        };
        if super::service::validate_agent(agent).is_err() {
            return Ok(None);
        }
        let Some((timestamp, signature)) = stamp.split_once(':') else {
            return Ok(None);
        };
        let Ok(timestamp) = timestamp.parse::<u64>() else {
            return Ok(None);
        };
        let now = seconds()?;
        if timestamp > now.saturating_add(5) || now.saturating_sub(timestamp) > 120 {
            return Ok(None);
        }
        let Ok(signature) = URL_SAFE_NO_PAD.decode(signature) else {
            return Ok(None);
        };
        // Absence of state is not authority; malformed/unreadable state is an
        // explicit error for the caller to fail closed, never a bypass.
        let key = match read_private(&self.directory.join("gateway.key")) {
            Ok(key) if key.len() == 32 => key,
            Ok(_) => return Err(io::Error::other("invalid gateway key")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mac = request_mac(&key, agent, timestamp, method, uri, body);
        if mac.verify_slice(&signature).is_err() {
            return Ok(None);
        }
        let records: BTreeMap<String, Registration> =
            serde_json::from_slice(&read_private(&self.directory.join("registrations.json"))?)?;
        match records.get(agent) {
            Some(record)
                if record.agent_id == agent && record.active && record.gateway.is_some() =>
            {
                record.gateway.as_ref().unwrap().validate()?;
                Ok(Some(VerifiedLocalAgent(agent.into())))
            }
            _ => Ok(None),
        }
    }
}
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        None
    } else {
        Some(value)
    }
}
fn seconds() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.as_secs())
        .map_err(io::Error::other)
}
fn request_mac(
    key: &[u8],
    agent: &str,
    timestamp: u64,
    method: &Method,
    uri: &Uri,
    body: &[u8],
) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    // Length-prefix every component; no delimiter or ambiguous encoding can
    // move a signature between agents, methods, targets or payloads.
    let timestamp = timestamp.to_be_bytes();
    let digest = Sha256::digest(body);
    let target = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    for part in [
        b"sm-local-gateway-v1".as_slice(),
        agent.as_bytes(),
        &timestamp,
        method.as_str().as_bytes(),
        target.as_bytes(),
        digest.as_slice(),
    ] {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part);
    }
    mac
}
fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no arguments.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::Error::other(
            "gateway state must be a private host-owned regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[derive(Clone)]
pub(super) struct Gateway {
    key: Arc<Vec<u8>>,
    capacity: Arc<tokio::sync::Semaphore>,
}
impl Gateway {
    /// Called while the service's exclusive lifetime lock is held.
    pub(super) fn open(directory: &Path) -> io::Result<Self> {
        let path = directory.join("gateway.key");
        let key = match read_private(&path) {
            Ok(key) => key,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut key = vec![0; 32];
                OsRng.fill_bytes(&mut key);
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                file.write_all(&key)?;
                file.sync_all()?;
                fs::File::open(directory)?.sync_all()?;
                key
            }
            Err(e) => return Err(e),
        };
        if key.len() != 32 {
            return Err(io::Error::other("invalid gateway key"));
        }
        Ok(Self {
            key: Arc::new(key),
            capacity: Arc::new(tokio::sync::Semaphore::new(128)),
        })
    }
    pub(super) async fn serve(
        &self,
        stream: TcpStream,
        agent: String,
        upstream: SocketAddr,
        mut stopped: watch::Receiver<bool>,
    ) {
        let Ok(_permit) = self.capacity.try_acquire() else {
            return;
        };
        let gateway = self.clone();
        let service = service_fn(move |request: Request<Incoming>| {
            let gateway = gateway.clone();
            let agent = agent.clone();
            async move {
                Ok::<_, Infallible>(
                    gateway
                        .forward(request.map(Body::new), &agent, upstream)
                        .await,
                )
            }
        });
        if *stopped.borrow() {
            return;
        }
        // The connection limit and whole-connection deadline bound slow clients;
        // unregister cancels in-flight requests and response streams too.
        tokio::select! {
            _ = stopped.changed() => {},
            _ = tokio::time::timeout(Duration::from_secs(300), hyper::server::conn::http1::Builder::new().keep_alive(false).serve_connection(TokioIo::new(stream), service)) => {},
        }
    }
    async fn forward(
        &self,
        request: Request<Body>,
        agent: &str,
        upstream: SocketAddr,
    ) -> Response<Body> {
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.forward_inner(request, agent, upstream),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(status)) => response(status),
            Err(_) => response(StatusCode::GATEWAY_TIMEOUT),
        }
    }
    async fn forward_inner(
        &self,
        request: Request<Body>,
        agent: &str,
        upstream: SocketAddr,
    ) -> Result<Response<Body>, StatusCode> {
        let (mut parts, body) = request.into_parts();
        if !matches!(
            parts.method,
            Method::GET
                | Method::HEAD
                | Method::POST
                | Method::PUT
                | Method::PATCH
                | Method::DELETE
                | Method::OPTIONS
        ) || parts.uri.scheme().is_some()
            || parts.uri.authority().is_some()
            || !parts.uri.path().starts_with('/')
            || parts.uri.path().starts_with("//")
            || parts.headers.contains_key("upgrade")
        {
            return Err(StatusCode::BAD_REQUEST);
        }
        let body = to_bytes(body, MAX_REQUEST)
            .await
            .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
        // Allowlist metadata. This drops all identity, auth, cookie, forwarded,
        // node-secret and hop-by-hop headers, including unknown future aliases.
        parts.headers = allowed_headers(&parts.headers, false);
        parts.headers.insert(
            "host",
            HeaderValue::from_str(&upstream.to_string()).map_err(|_| StatusCode::BAD_GATEWAY)?,
        );
        parts.headers.insert(
            AGENT_HEADER,
            HeaderValue::from_str(agent).map_err(|_| StatusCode::BAD_GATEWAY)?,
        );
        let timestamp = seconds().map_err(|_| StatusCode::BAD_GATEWAY)?;
        let signature = request_mac(
            &self.key,
            agent,
            timestamp,
            &parts.method,
            &parts.uri,
            &body,
        )
        .finalize()
        .into_bytes();
        let stamp = format!("{timestamp}:{}", URL_SAFE_NO_PAD.encode(signature));
        parts.headers.insert(
            SIGNATURE_HEADER,
            HeaderValue::from_str(&stamp).map_err(|_| StatusCode::BAD_GATEWAY)?,
        );
        let stream = TcpStream::connect(upstream)
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        let driver = AbortOnDrop(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let response = sender
            .send_request(Request::from_parts(parts, Body::from(body)))
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        // Never tunnel protocols, follow redirects, or pass owner cookies/auth
        // headers back to a local agent. Streaming drops trailers as well.
        if (response.status().is_redirection() && response.status() != StatusCode::NOT_MODIFIED)
            || response.status() == StatusCode::SWITCHING_PROTOCOLS
        {
            return Err(StatusCode::BAD_GATEWAY);
        }
        let (mut parts, body) = response.into_parts();
        parts.headers = allowed_headers(&parts.headers, true);
        let stream = futures_util::stream::unfold(
            (body.into_data_stream(), 0usize, driver),
            |(mut stream, total, driver)| async move {
                if total > MAX_RESPONSE {
                    return None;
                }
                let chunk = match tokio::time::timeout(Duration::from_secs(120), stream.next())
                    .await
                {
                    Ok(Some(Ok(chunk))) if total.saturating_add(chunk.len()) <= MAX_RESPONSE => {
                        chunk
                    }
                    Ok(None) => return None,
                    _ => {
                        return Some((
                            Err(io::Error::other(
                                "gateway response stream failed or exceeded limit",
                            )),
                            (stream, MAX_RESPONSE + 1, driver),
                        ))
                    }
                };
                let total = total + chunk.len();
                Some((Ok(chunk), (stream, total, driver)))
            },
        );
        Ok(Response::from_parts(parts, Body::from_stream(stream)))
    }
}
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn allowed_headers(source: &HeaderMap, response: bool) -> HeaderMap {
    let mut result = HeaderMap::new();
    let names: &[&str] = if response {
        &[
            "content-type",
            "content-length",
            "content-disposition",
            "cache-control",
            "etag",
            "last-modified",
            "retry-after",
            "accept-ranges",
            "content-range",
            "x-sm-build",
        ]
    } else {
        &[
            "content-type",
            "accept",
            "user-agent",
            "range",
            "if-none-match",
            "if-modified-since",
        ]
    };
    // Connection can nominate otherwise innocuous metadata as hop-by-hop.
    let nominated: Vec<_> = source
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect();
    for &name in names {
        if nominated.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            continue;
        }
        for value in source.get_all(name) {
            result.append(axum::http::HeaderName::from_static(name), value.clone());
        }
    }
    result
}
fn response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

#[cfg(test)]
mod tests;
