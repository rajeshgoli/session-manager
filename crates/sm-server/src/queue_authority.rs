use std::{
    env, fs,
    io::{ErrorKind, Read, Write},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process, thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::queue::{QueueJobRecord, RetainedQueueStore};

pub const AUTHORITY_SOCKET_FILE: &str = "authority.sock";
pub const REQUEST_SCHEMA: &str = "sm.queue_authority.request.v1";
pub const RESPONSE_SCHEMA: &str = "sm.queue_authority.response.v1";
pub const CODE_SIGN_IDENTIFIER: &str = "com.rajeshgoli.sm-server";
const MAX_REQUEST_BYTES: usize = 1024;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueAuthorityServiceIdentity {
    pub pid: u32,
    pub launchd_label: String,
    pub executable_path: String,
    pub code_sign_identifier: String,
}

impl QueueAuthorityServiceIdentity {
    pub fn current() -> Result<Self> {
        let executable_path = env::current_exe()
            .context("failed to resolve the queue authority executable")?
            .to_string_lossy()
            .into_owned();
        Ok(Self {
            pid: process::id(),
            launchd_label: env::var("XPC_SERVICE_NAME").unwrap_or_else(|_| "unmanaged".to_owned()),
            executable_path,
            code_sign_identifier: CODE_SIGN_IDENTIFIER.to_owned(),
        })
    }
}

pub struct QueueAuthorityServer {
    listener: UnixListener,
    socket_path: PathBuf,
    queue_db_path: PathBuf,
    identity: QueueAuthorityServiceIdentity,
}

impl QueueAuthorityServer {
    pub fn bind(queue_state_dir: &Path, identity: QueueAuthorityServiceIdentity) -> Result<Self> {
        fs::create_dir_all(queue_state_dir).with_context(|| {
            format!(
                "failed to create queue authority directory {}",
                queue_state_dir.display()
            )
        })?;
        let socket_path = queue_state_dir.join(AUTHORITY_SOCKET_FILE);
        remove_stale_socket(&socket_path)?;
        let listener = UnixListener::bind(&socket_path).with_context(|| {
            format!(
                "failed to bind queue authority socket {}",
                socket_path.display()
            )
        })?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).with_context(
            || {
                format!(
                    "failed to secure queue authority socket {}",
                    socket_path.display()
                )
            },
        )?;
        Ok(Self {
            listener,
            socket_path,
            queue_db_path: queue_state_dir.join("queue_runner.db"),
            identity,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn spawn(self) {
        thread::spawn(move || loop {
            if let Err(error) = self.accept_once() {
                eprintln!("queue authority request failed: {error:#}");
            }
        });
    }

    fn accept_once(&self) -> Result<()> {
        let (stream, _) = self
            .listener
            .accept()
            .context("failed to accept queue authority connection")?;
        handle_connection(stream, &self.queue_db_path, &self.identity)
    }
}

impl Drop for QueueAuthorityServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityRequest {
    schema: String,
    job_id: String,
}

#[derive(Debug, Serialize)]
struct AuthorityResponse<'a> {
    schema: &'static str,
    ok: bool,
    service: &'a QueueAuthorityServiceIdentity,
    job: Option<QueueJobRecord>,
    error: Option<AuthorityError>,
}

#[derive(Debug, Serialize)]
struct AuthorityError {
    code: &'static str,
    message: String,
}

fn handle_connection(
    mut stream: UnixStream,
    queue_db_path: &Path,
    identity: &QueueAuthorityServiceIdentity,
) -> Result<()> {
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let request_bytes = read_request(&mut stream)?;
    let response = match serde_json::from_slice::<AuthorityRequest>(&request_bytes) {
        Ok(request) if request.schema != REQUEST_SCHEMA => error_response(
            identity,
            "unsupported_schema",
            format!("unsupported request schema {}", request.schema),
        ),
        Ok(request) if !valid_job_id(&request.job_id) => error_response(
            identity,
            "invalid_job_id",
            "job_id must match job_<12 lowercase hex characters>".to_owned(),
        ),
        Ok(request) => {
            match RetainedQueueStore::get_queue_job_from_path(queue_db_path, &request.job_id) {
                Ok(Some(job)) => AuthorityResponse {
                    schema: RESPONSE_SCHEMA,
                    ok: true,
                    service: identity,
                    job: Some(job),
                    error: None,
                },
                Ok(None) => error_response(
                    identity,
                    "not_found",
                    format!("queue job {} was not found", request.job_id),
                ),
                Err(error) => error_response(identity, "queue_read_failed", error.to_string()),
            }
        }
        Err(error) => error_response(identity, "invalid_request", error.to_string()),
    };
    let mut body = serde_json::to_vec(&response)?;
    body.push(b'\n');
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

fn error_response<'a>(
    identity: &'a QueueAuthorityServiceIdentity,
    code: &'static str,
    message: String,
) -> AuthorityResponse<'a> {
    AuthorityResponse {
        schema: RESPONSE_SCHEMA,
        ok: false,
        service: identity,
        job: None,
        error: Some(AuthorityError { code, message }),
    }
}

fn read_request(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut request = Vec::with_capacity(256);
    let mut chunk = [0_u8; 256];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            bail!("queue authority request ended before newline");
        }
        let bytes = &chunk[..read];
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            request.extend_from_slice(&bytes[..newline]);
            if bytes[newline + 1..]
                .iter()
                .any(|byte| !byte.is_ascii_whitespace())
            {
                bail!("queue authority accepts exactly one request per connection");
            }
            break;
        }
        request.extend_from_slice(bytes);
        if request.len() > MAX_REQUEST_BYTES {
            bail!("queue authority request exceeds {MAX_REQUEST_BYTES} bytes");
        }
    }
    if request.is_empty() {
        bail!("queue authority request is empty");
    }
    if request.len() > MAX_REQUEST_BYTES {
        bail!("queue authority request exceeds {MAX_REQUEST_BYTES} bytes");
    }
    Ok(request)
}

fn valid_job_id(job_id: &str) -> bool {
    job_id.strip_prefix("job_").is_some_and(|suffix| {
        suffix.len() == 12
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn remove_stale_socket(socket_path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to replace non-socket queue authority path {}",
            socket_path.display()
        );
    }
    match UnixStream::connect(socket_path) {
        Ok(_) => bail!(
            "queue authority socket {} is already active",
            socket_path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(socket_path).with_context(|| {
                format!(
                    "failed to remove stale queue authority socket {}",
                    socket_path.display()
                )
            })?;
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to verify queue authority socket {}",
                socket_path.display()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        os::fd::AsRawFd,
        sync::atomic::{AtomicU64, Ordering},
    };

    use serde_json::{json, Value};

    use super::*;
    use crate::queue::CreateQueueJob;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn authority_returns_exact_queue_record_and_kernel_peer_pid() {
        let state_dir = unique_temp_dir("response");
        let job = RetainedQueueStore::create_queue_job_in_state_dir(
            &state_dir,
            CreateQueueJob {
                job_type: "tests".to_owned(),
                label: "authority fixture".to_owned(),
                requester_session_id: Some("requester1".to_owned()),
                notify_session_id: "notify1".to_owned(),
                cwd: "/tmp/authority-fixture".to_owned(),
                argv: Some(vec!["/usr/bin/true".to_owned()]),
                script: None,
                env: BTreeMap::new(),
                timeout_seconds: 60,
            },
        )
        .unwrap();
        let identity = QueueAuthorityServiceIdentity {
            pid: process::id(),
            launchd_label: "com.rajeshgoli.session-manager-rust".to_owned(),
            executable_path: "/approved/sm-server".to_owned(),
            code_sign_identifier: CODE_SIGN_IDENTIFIER.to_owned(),
        };
        let server = QueueAuthorityServer::bind(&state_dir, identity.clone()).unwrap();
        assert_eq!(
            fs::metadata(server.socket_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let socket_path = server.socket_path().to_path_buf();
        let server_thread = thread::spawn(move || server.accept_once().unwrap());

        let mut stream = UnixStream::connect(socket_path).unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(macos_peer_pid(&stream), process::id());
        writeln!(
            stream,
            "{}",
            json!({ "schema": REQUEST_SCHEMA, "job_id": job.id })
        )
        .unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).unwrap();
        server_thread.join().unwrap();

        let payload: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(payload["schema"], RESPONSE_SCHEMA);
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["service"], serde_json::to_value(identity).unwrap());
        assert_eq!(payload["job"]["id"], job.id);
        assert_eq!(payload["job"]["type"], "tests");
        assert_eq!(payload["job"]["cwd"], "/tmp/authority-fixture");
        assert_eq!(payload["job"]["argv"], json!(["/usr/bin/true"]));
    }

    #[test]
    fn authority_rejects_bad_schema_job_id_and_missing_job() {
        let state_dir = unique_temp_dir("errors");
        let identity = fixture_identity();

        let payload = round_trip(
            &state_dir,
            &identity,
            json!({ "schema": "wrong", "job_id": "job_0123456789ab" }),
        );
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["error"]["code"], "unsupported_schema");

        let payload = round_trip(
            &state_dir,
            &identity,
            json!({ "schema": REQUEST_SCHEMA, "job_id": "job_ABC" }),
        );
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["error"]["code"], "invalid_job_id");

        let payload = round_trip(
            &state_dir,
            &identity,
            json!({ "schema": REQUEST_SCHEMA, "job_id": "job_0123456789ab" }),
        );
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["error"]["code"], "not_found");
        assert!(payload["job"].is_null());
    }

    #[test]
    fn authority_socket_refuses_active_and_non_socket_paths_but_replaces_stale_socket() {
        let active_dir = unique_temp_dir("active");
        let active_path = active_dir.join(AUTHORITY_SOCKET_FILE);
        let _active_listener = UnixListener::bind(&active_path).unwrap();
        let error = QueueAuthorityServer::bind(&active_dir, fixture_identity())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("already active"));

        let file_dir = unique_temp_dir("file");
        fs::write(file_dir.join(AUTHORITY_SOCKET_FILE), "not a socket").unwrap();
        let error = QueueAuthorityServer::bind(&file_dir, fixture_identity())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("refusing to replace non-socket"));

        let stale_dir = unique_temp_dir("stale");
        let stale_path = stale_dir.join(AUTHORITY_SOCKET_FILE);
        UnixListener::bind(&stale_path).unwrap();
        let server = QueueAuthorityServer::bind(&stale_dir, fixture_identity()).unwrap();
        assert!(server.socket_path().exists());
        drop(server);
        assert!(!stale_path.exists());
    }

    #[test]
    fn authority_request_reader_is_bounded_and_requires_one_line() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client
            .write_all(format!("{}\n", "x".repeat(MAX_REQUEST_BYTES + 1)).as_bytes())
            .unwrap();
        let error = read_request(&mut server).unwrap_err().to_string();
        assert!(error.contains("exceeds"));

        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(b"{}\nnot-another-request").unwrap();
        let error = read_request(&mut server).unwrap_err().to_string();
        assert!(error.contains("exactly one request"));
    }

    fn round_trip(
        state_dir: &Path,
        identity: &QueueAuthorityServiceIdentity,
        request: Value,
    ) -> Value {
        let server = QueueAuthorityServer::bind(state_dir, identity.clone()).unwrap();
        let socket_path = server.socket_path().to_path_buf();
        let server_thread = thread::spawn(move || server.accept_once().unwrap());
        let mut stream = UnixStream::connect(socket_path).unwrap();
        writeln!(stream, "{request}").unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).unwrap();
        server_thread.join().unwrap();
        serde_json::from_str(&body).unwrap()
    }

    fn fixture_identity() -> QueueAuthorityServiceIdentity {
        QueueAuthorityServiceIdentity {
            pid: process::id(),
            launchd_label: "test-launchd".to_owned(),
            executable_path: "/test/sm-server".to_owned(),
            code_sign_identifier: CODE_SIGN_IDENTIFIER.to_owned(),
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "sm-queue-authority-{label}-{}-{}",
            process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[cfg(target_os = "macos")]
    fn macos_peer_pid(stream: &UnixStream) -> u32 {
        let mut peer_pid = 0_i32;
        let mut length = std::mem::size_of::<i32>() as nix::libc::socklen_t;
        let result = unsafe {
            nix::libc::getsockopt(
                stream.as_raw_fd(),
                nix::libc::SOL_LOCAL,
                nix::libc::LOCAL_PEERPID,
                (&mut peer_pid as *mut i32).cast(),
                &mut length,
            )
        };
        assert_eq!(result, 0);
        u32::try_from(peer_pid).unwrap()
    }
}
