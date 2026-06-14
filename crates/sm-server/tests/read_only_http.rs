use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode},
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use futures_util::{future::join, StreamExt as _};
use hmac::{Hmac, Mac};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use sm_server::config::RustShadowConfig;
use sm_server::queue::{QueueAdmissionPolicy, RetainedQueueStore};
use sm_server::{
    config::{
        AppArtifactsConfig, AppConfig, BugReportsConfig, CodexEventsConfig, CodexForkLaunchConfig,
        CodexObservabilityConfig, CodexRequestsConfig, CodexRolloutConfig, EmailConfig,
        ExternalAccessConfig, GoogleAuthConfig, MobileAnalyticsConfig, MobileTerminalConfig,
        MobileTerminalDeviceKeyConfig, MobileTerminalUserConfig, PathsConfig, QueueRunnerConfig,
        RustCoreConfig, SmSendConfig, ToolLoggingConfig,
    },
    http::{router, AppState, GitHubReviewComment, GitHubReviewMatch, GitHubReviewPoster},
};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tower::ServiceExt;

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let (status, _headers, body) = get_response(app, uri).await;
    (status, serde_json::from_slice(&body).unwrap())
}

async fn get_response(app: axum::Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, body.to_vec())
}

async fn post_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, Value) {
    json_request_with_headers_and_peer(
        app,
        "POST",
        uri,
        payload,
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await
}

async fn post_json_with_headers_and_peer(
    app: axum::Router,
    uri: &str,
    payload: Value,
    headers: &[(&str, &str)],
    peer_addr: Option<SocketAddr>,
) -> (StatusCode, Value) {
    json_request_with_headers_and_peer(app, "POST", uri, payload, headers, peer_addr).await
}

async fn put_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, Value) {
    json_request_with_headers_and_peer(
        app,
        "PUT",
        uri,
        payload,
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await
}

async fn delete_json(app: axum::Router, uri: &str, payload: Value) -> (StatusCode, Value) {
    json_request_with_headers_and_peer(
        app,
        "DELETE",
        uri,
        payload,
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await
}

async fn wait_for_queue_job_state(app: axum::Router, job_id: &str, states: &[&str]) -> Value {
    for _ in 0..80 {
        let (status, payload) = get_json(app.clone(), &format!("/queue-jobs/{job_id}")).await;
        assert_eq!(status, StatusCode::OK);
        if payload["state"]
            .as_str()
            .is_some_and(|state| states.contains(&state))
        {
            return payload;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("queue job {job_id} did not reach one of {states:?}");
}

fn queued_message_texts(db_path: &PathBuf, target_session_id: &str) -> Vec<String> {
    let mut last_texts = Vec::new();
    let mut stable_reads = 0;
    for attempt in 0..50 {
        if let Ok(conn) = Connection::open(db_path) {
            if let Ok(mut statement) = conn.prepare(
                "SELECT text FROM message_queue WHERE target_session_id = ?1 ORDER BY queued_at, id",
            ) {
                let texts = statement
                    .query_map([target_session_id], |row| row.get::<_, String>(0))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                if texts == last_texts {
                    stable_reads += 1;
                } else {
                    stable_reads = 0;
                    last_texts = texts;
                }
                if (!last_texts.is_empty() && stable_reads >= 2) || attempt == 49 {
                    return last_texts;
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    last_texts
}

fn queue_job_completion_notified_at(queue_state_dir: &PathBuf, job_id: &str) -> Option<String> {
    for attempt in 0..50 {
        let conn = Connection::open(queue_state_dir.join("queue_runner.db")).unwrap();
        let completion_notified_at = conn
            .query_row(
                "SELECT completion_notified_at FROM queue_jobs WHERE id = ?1",
                [job_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap();
        if completion_notified_at.is_some() || attempt == 49 {
            return completion_notified_at;
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

#[derive(Debug, Clone)]
struct StubGitHubReviewPoster {
    result: Arc<Mutex<Result<GitHubReviewComment, String>>>,
    calls: Arc<Mutex<Vec<(String, i64, Option<String>)>>>,
    fresh_review: Arc<Mutex<Option<GitHubReviewMatch>>>,
}

impl StubGitHubReviewPoster {
    fn successful() -> Self {
        Self {
            result: Arc::new(Mutex::new(Ok(GitHubReviewComment {
                comment_id: Some(4701290334),
                comment_url: Some(
                    "https://github.com/rajeshgoli/session-manager/pull/967#issuecomment-4701290334"
                        .to_owned(),
                ),
                posted_at: "2026-06-14T02:30:00Z".to_owned(),
            }))),
            calls: Arc::new(Mutex::new(Vec::new())),
            fresh_review: Arc::new(Mutex::new(None)),
        }
    }

    fn failing(message: &str) -> Self {
        Self {
            result: Arc::new(Mutex::new(Err(message.to_owned()))),
            calls: Arc::new(Mutex::new(Vec::new())),
            fresh_review: Arc::new(Mutex::new(None)),
        }
    }

    fn calls(&self) -> Vec<(String, i64, Option<String>)> {
        self.calls.lock().unwrap().clone()
    }

    fn with_fresh_review(self, review_match: GitHubReviewMatch) -> Self {
        *self.fresh_review.lock().unwrap() = Some(review_match);
        self
    }
}

impl GitHubReviewPoster for StubGitHubReviewPoster {
    fn post_initial_review_request(
        &self,
        repo: &str,
        pr_number: i64,
        steer: Option<&str>,
    ) -> Result<GitHubReviewComment, String> {
        self.calls
            .lock()
            .unwrap()
            .push((repo.to_owned(), pr_number, steer.map(ToOwned::to_owned)));
        self.result.lock().unwrap().clone()
    }

    fn find_fresh_codex_review_or_comment(
        &self,
        _repo: &str,
        _pr_number: i64,
        _since: &str,
    ) -> Result<Option<GitHubReviewMatch>, String> {
        Ok(self.fresh_review.lock().unwrap().clone())
    }
}

fn queue_job_text_column(queue_state_dir: &PathBuf, job_id: &str, column: &str) -> String {
    assert!(matches!(column, "exit_code_path" | "log_path"));
    let conn = Connection::open(queue_state_dir.join("queue_runner.db")).unwrap();
    conn.query_row(
        &format!("SELECT {column} FROM queue_jobs WHERE id = ?1"),
        [job_id],
        |row| row.get::<_, String>(0),
    )
    .unwrap()
}

fn set_queue_job_text_column_null(queue_state_dir: &PathBuf, job_id: &str, column: &str) {
    assert!(matches!(column, "wrapper_path" | "log_path"));
    let conn = Connection::open(queue_state_dir.join("queue_runner.db")).unwrap();
    conn.execute(
        &format!("UPDATE queue_jobs SET {column} = NULL WHERE id = ?1"),
        [job_id],
    )
    .unwrap();
}

fn set_queue_job_holding_reason(queue_state_dir: &PathBuf, job_id: &str, holding_reason: &str) {
    let conn = Connection::open(queue_state_dir.join("queue_runner.db")).unwrap();
    conn.execute(
        "UPDATE queue_jobs SET holding_reason = ?2 WHERE id = ?1",
        (job_id, holding_reason),
    )
    .unwrap();
}

fn mark_queue_job_terminal(
    queue_state_dir: &PathBuf,
    job_id: &str,
    state: &str,
    started_at: &str,
    finished_at: &str,
    exit_code: i64,
) {
    assert!(matches!(
        state,
        "succeeded" | "failed" | "cancelled" | "timed_out"
    ));
    let conn = Connection::open(queue_state_dir.join("queue_runner.db")).unwrap();
    conn.execute(
        r#"
        UPDATE queue_jobs
        SET state = ?2,
            started_at = ?3,
            finished_at = ?4,
            exit_code = ?5,
            holding_reason = NULL
        WHERE id = ?1
        "#,
        (job_id, state, started_at, finished_at, exit_code),
    )
    .unwrap();
}

fn mark_queue_job_running(
    queue_state_dir: &PathBuf,
    job_id: &str,
    started_at: &str,
    pid: i64,
    process_group_id: i64,
) {
    mark_queue_job_running_with_holding(
        queue_state_dir,
        job_id,
        started_at,
        pid,
        process_group_id,
        None,
    );
}

fn mark_queue_job_running_with_holding(
    queue_state_dir: &PathBuf,
    job_id: &str,
    started_at: &str,
    pid: i64,
    process_group_id: i64,
    holding_reason: Option<&str>,
) {
    let conn = Connection::open(queue_state_dir.join("queue_runner.db")).unwrap();
    conn.execute(
        r#"
        UPDATE queue_jobs
        SET state = 'running',
            started_at = ?2,
            pid = ?3,
            process_group_id = ?4,
            holding_reason = ?5
        WHERE id = ?1
        "#,
        (job_id, started_at, pid, process_group_id, holding_reason),
    )
    .unwrap();
}

fn test_now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

fn queue_runtime_test_app(
    state_file: &PathBuf,
    queue_state_dir: &PathBuf,
    message_queue_db: &PathBuf,
    runtime_enabled: bool,
    fixture_writes_enabled: bool,
) -> axum::Router {
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: message_queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.runtime_enabled = runtime_enabled;
    config.rust_core.fixture_writes_enabled = fixture_writes_enabled;
    router(AppState::new(config))
}

async fn create_pending_queue_job(
    app: axum::Router,
    working_dir: &PathBuf,
    label: &str,
    script: &str,
    timeout_seconds: i64,
) -> String {
    create_pending_queue_job_of_type(app, working_dir, "tests", label, script, timeout_seconds)
        .await
}

async fn create_pending_queue_job_of_type(
    app: axum::Router,
    working_dir: &PathBuf,
    job_type: &str,
    label: &str,
    script: &str,
    timeout_seconds: i64,
) -> String {
    let (status, payload) = post_json(
        app,
        "/queue-jobs",
        json!({
            "type": job_type,
            "label": label,
            "script": script,
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": timeout_seconds
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["state"], "pending");
    payload["id"].as_str().unwrap().to_owned()
}

async fn json_request_with_headers_and_peer(
    app: axum::Router,
    method: &str,
    uri: &str,
    payload: Value,
    headers: &[(&str, &str)],
    peer_addr: Option<SocketAddr>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();
    if let Some(peer_addr) = peer_addr {
        request.extensions_mut().insert(ConnectInfo(peer_addr));
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn post_multipart_with_host_and_peer(
    app: axum::Router,
    uri: &str,
    host: &str,
    body: Vec<u8>,
    boundary: &str,
    headers: &[(&str, String)],
    peer_addr: Option<SocketAddr>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", host)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        );
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    let mut request = builder.body(Body::from(body)).unwrap();
    if let Some(peer_addr) = peer_addr {
        request.extensions_mut().insert(ConnectInfo(peer_addr));
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, body.to_vec())
}

fn multipart_app_upload(
    boundary: &str,
    file_bytes: &[u8],
    version_code: Option<&str>,
    version_name: Option<&str>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"app-debug.apk\"\r\nContent-Type: application/vnd.android.package-archive\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(b"\r\n");
    if let Some(version_code) = version_code {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"version_code\"\r\n\r\n{version_code}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some(version_name) = version_name {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"version_name\"\r\n\r\n{version_name}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

async fn get_json_with_host(app: axum::Router, uri: &str, host: &str) -> (StatusCode, Value) {
    get_json_with_host_and_peer(app, uri, host, None).await
}

async fn get_json_with_host_and_headers(
    app: axum::Router,
    uri: &str,
    host: &str,
    headers: &[(&str, String)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(uri).header("host", host);
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn get_json_with_host_and_peer(
    app: axum::Router,
    uri: &str,
    host: &str,
    peer_addr: Option<SocketAddr>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .uri(uri)
        .header("host", host)
        .body(Body::empty())
        .unwrap();
    if let Some(peer_addr) = peer_addr {
        request.extensions_mut().insert(ConnectInfo(peer_addr));
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn health_matches_python_basic_shape() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/health").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "status": "healthy" }));
}

#[tokio::test]
async fn detailed_health_has_required_top_level_fields() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/health/detailed").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "healthy");
    assert!(payload["checks"].is_object());
    assert!(payload["resources"].is_object());
    assert!(payload["timestamp"].is_string());
}

#[tokio::test]
async fn nodes_list_defaults_to_implicit_primary_node() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/nodes").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["default"], "primary");
    assert_eq!(
        payload["nodes"],
        json!([
            {
                "id": "primary",
                "primary": true,
                "ssh": null,
                "api_url": null,
                "hook_base_url": null,
                "projects_root": null,
                "log_dir": null,
                "codex_fork_node_agent": false
            }
        ])
    );
}

#[tokio::test]
async fn nodes_list_preserves_configured_metadata_and_redacts_secrets() {
    let config_path = unique_temp_path();
    let local_env_path = unique_temp_path();
    fs::write(
        &config_path,
        r#"
nodes:
  default: macbook
  restore_inventory_cache_seconds: 42
  registry:
    macbook:
      ssh: macbook.local
      ssh_proxy_command: "cloudflared access ssh --hostname macbook.example.com"
      control_path: "~/Library/Caches/sm/macbook.sock"
      api_url: "http://macbook.local:8420"
      hook_base_url: "https://macbook.example.com/hooks"
      hook_secret: "secret-hook-value"
      node_token: "secret-node-token"
      projects_root: "/Users/rajesh/projects"
      log_dir: "/tmp/sm-node"
    worker:
      ssh: " worker.example.com "
    empty-value: "not a mapping"
"#,
    )
    .unwrap();
    let config =
        AppConfig::load_from_path_with_local_env(&config_path, Some(&local_env_path)).unwrap();
    let app = router(AppState::new(config));

    let (status, payload) = get_json(app, "/nodes").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["default"], "macbook");
    let nodes = payload["nodes"].as_array().unwrap();
    let ids = nodes
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["empty-value", "macbook", "primary", "worker"]);

    let macbook = nodes.iter().find(|entry| entry["id"] == "macbook").unwrap();
    assert_eq!(macbook["primary"], false);
    assert_eq!(macbook["ssh"], "macbook.local");
    assert_eq!(macbook["api_url"], "http://macbook.local:8420");
    assert_eq!(
        macbook["hook_base_url"],
        "https://macbook.example.com/hooks"
    );
    assert_eq!(macbook["projects_root"], "/Users/rajesh/projects");
    assert_eq!(macbook["log_dir"], "/tmp/sm-node");
    assert_eq!(macbook["codex_fork_node_agent"], false);

    let worker = nodes.iter().find(|entry| entry["id"] == "worker").unwrap();
    assert_eq!(worker["ssh"], "worker.example.com");

    let empty_value = nodes
        .iter()
        .find(|entry| entry["id"] == "empty-value")
        .unwrap();
    assert_eq!(empty_value["ssh"], Value::Null);
    assert_eq!(empty_value["api_url"], Value::Null);

    let serialized = payload.to_string();
    assert!(!serialized.contains("secret-hook-value"));
    assert!(!serialized.contains("secret-node-token"));
    assert!(!serialized.contains("cloudflared access ssh"));
    assert!(!serialized.contains("macbook.sock"));
}

#[tokio::test]
async fn nodes_list_falls_back_to_primary_when_default_is_unknown() {
    let config_path = unique_temp_path();
    let local_env_path = unique_temp_path();
    fs::write(
        &config_path,
        r#"
nodes:
  default: missing-node
  registry:
    remote:
      ssh: remote.example.com
"#,
    )
    .unwrap();
    let config =
        AppConfig::load_from_path_with_local_env(&config_path, Some(&local_env_path)).unwrap();
    let app = router(AppState::new(config));

    let (status, payload) = get_json(app, "/nodes").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["default"], "primary");
    let ids = payload["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["primary", "remote"]);
}

#[tokio::test]
async fn node_ping_preserves_primary_unknown_and_remote_error_contracts() {
    let config_path = unique_temp_path();
    let local_env_path = unique_temp_path();
    fs::write(
        &config_path,
        r#"
nodes:
  registry:
    macbook:
      ssh: macbook.local
      node_token: secret-node-token
"#,
    )
    .unwrap();
    let config =
        AppConfig::load_from_path_with_local_env(&config_path, Some(&local_env_path)).unwrap();
    let app = router(AppState::new(config));

    let (status, payload) = post_json(app.clone(), "/nodes/primary/ping", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "node": "primary",
            "ok": true,
            "error": null
        })
    );

    let (status, payload) = post_json(app.clone(), "/nodes/missing/ping", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Unknown node: missing");

    let (status, payload) = post_json(app, "/nodes/macbook/ping", json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        payload["detail"],
        "node-agent not connected for node macbook"
    );
    assert!(!payload.to_string().contains("secret-node-token"));
}

#[tokio::test]
async fn node_restore_candidates_project_primary_stopped_sessions() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/nodes/primary/restore-candidates?refresh=true").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["node"], "primary");
    let sessions = payload["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let candidate = &sessions[0];
    assert_eq!(candidate["id"], "stop1234");
    assert_eq!(candidate["status"], "stopped");
    assert_eq!(candidate["node"], "primary");
    assert_eq!(candidate["origin_node"], "primary");
    assert_eq!(candidate["source_session_id"], "stop1234");
    assert_eq!(candidate["restore_source"], "server_state");
    assert_eq!(candidate["activity_state"], "stopped");
    assert_eq!(candidate["provider"], "claude");
}

#[tokio::test]
async fn node_restore_candidates_preserve_unknown_and_remote_errors() {
    let config_path = unique_temp_path();
    let local_env_path = unique_temp_path();
    fs::write(
        &config_path,
        r#"
nodes:
  registry:
    macbook:
      ssh: macbook.local
      node_token: secret-node-token
"#,
    )
    .unwrap();
    let config =
        AppConfig::load_from_path_with_local_env(&config_path, Some(&local_env_path)).unwrap();
    let app = router(AppState::new(config));

    let (status, payload) = get_json(app.clone(), "/nodes/missing/restore-candidates").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Unknown node: missing");

    let (status, payload) = get_json(app, "/nodes/macbook/restore-candidates").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        payload["detail"],
        "node-agent not connected for node macbook"
    );
    assert!(!payload.to_string().contains("secret-node-token"));
}

#[tokio::test]
async fn node_restore_candidate_restore_round_trips_primary_fixture() {
    let state_file = write_session_fixture();
    let mut config = config_with_state_file(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/nodes/primary/restore-candidates/stop1234/restore",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "stop1234");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["stopped_at"], Value::Null);

    let (status, payload) = post_json(
        app.clone(),
        "/nodes/primary/restore-candidates/run12345/restore",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(payload["detail"], "Session is not stopped");

    let (status, payload) = post_json(
        app,
        "/nodes/primary/restore-candidates/missing/restore",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["detail"], "Session not found");
}

#[tokio::test]
async fn client_analytics_summary_reports_live_metrics_from_state_queue_and_logs() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("analytics-message-queue.db");
    let server_log = state_file.with_extension("analytics-server.log");
    let now = time::OffsetDateTime::now_utc();
    let session_a_created = now - time::Duration::hours(2);
    let session_b_created = now - time::Duration::hours(1);
    let send_a = now - time::Duration::minutes(90);
    let send_b = now - time::Duration::minutes(30);
    let send_previous = now - time::Duration::hours(25);
    let track_send = now - time::Duration::minutes(15);
    let restart = now - time::Duration::hours(2);
    let spawn_current = now - time::Duration::minutes(80);
    let spawn_previous = now - time::Duration::hours(26);
    let self_heal = now - time::Duration::minutes(70);

    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "analyticsa",
                    "name": "claude-analyticsa",
                    "working_dir": "/tmp/repo-a",
                    "tmux_session": "claude-analyticsa",
                    "log_file": "/tmp/analyticsa.log",
                    "status": "running",
                    "created_at": rfc3339(session_a_created),
                    "last_activity": rfc3339(now),
                    "provider": "claude",
                    "tokens_used": 1200,
                    "friendly_name": "agent-a"
                },
                {
                    "id": "analyticsb",
                    "name": "codex-fork-analyticsb",
                    "working_dir": "/tmp/repo-b",
                    "tmux_session": "codex-fork-analyticsb",
                    "log_file": "/tmp/analyticsb.log",
                    "status": "thinking",
                    "created_at": rfc3339(session_b_created),
                    "last_activity": rfc3339(now),
                    "provider": "codex-fork",
                    "tokens_used": 800,
                    "friendly_name": "agent-b"
                },
                {
                    "id": "analyticsstopped",
                    "name": "claude-analyticsstopped",
                    "working_dir": "/tmp/repo-c",
                    "tmux_session": "claude-analyticsstopped",
                    "log_file": "/tmp/analyticsstopped.log",
                    "status": "stopped",
                    "created_at": rfc3339(now - time::Duration::hours(5)),
                    "last_activity": rfc3339(now),
                    "stopped_at": rfc3339(now)
                }
            ]
        })
        .to_string(),
    )
    .unwrap();

    {
        let conn = Connection::open(&queue_db).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE message_queue (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL,
                text TEXT NOT NULL,
                delivery_mode TEXT DEFAULT 'sequential',
                from_sm_send INTEGER DEFAULT 0,
                queued_at TIMESTAMP NOT NULL,
                message_category TEXT DEFAULT NULL
            );
            CREATE TABLE remind_registrations (
                id TEXT PRIMARY KEY,
                target_session_id TEXT NOT NULL UNIQUE,
                soft_threshold_seconds INTEGER NOT NULL,
                hard_threshold_seconds INTEGER NOT NULL,
                registered_at TIMESTAMP NOT NULL,
                last_reset_at TIMESTAMP NOT NULL,
                soft_fired INTEGER DEFAULT 0,
                is_active INTEGER DEFAULT 1,
                cancel_on_reply_session_id TEXT
            );
            "#,
        )
        .unwrap();
        conn.execute(
            r#"
            INSERT INTO message_queue
                (id, target_session_id, text, queued_at, message_category, from_sm_send)
            VALUES
                ('send-a', 'analyticsa', 'msg', ?1, NULL, 1),
                ('send-b', 'analyticsa', 'msg', ?2, NULL, 1),
                ('send-prev', 'analyticsb', 'msg', ?3, NULL, 1),
                ('track-a', 'analyticsb', 'track', ?4, 'track_remind', 0)
            "#,
            (
                rfc3339(send_a),
                rfc3339(send_b),
                rfc3339(send_previous),
                rfc3339(track_send),
            ),
        )
        .unwrap();
        conn.execute(
            r#"
            INSERT INTO remind_registrations
                (id, target_session_id, soft_threshold_seconds, hard_threshold_seconds,
                 registered_at, last_reset_at, soft_fired, is_active, cancel_on_reply_session_id)
            VALUES
                ('track-active', 'analyticsa', 300, 600, ?1, ?1, 1, 1, 'owner-a'),
                ('track-waiting', 'analyticsb', 300, 600, ?1, ?1, 0, 1, 'owner-b'),
                ('track-unowned', 'ignored', 300, 600, ?1, ?1, 1, 1, NULL)
            "#,
            [rfc3339(now - time::Duration::hours(3))],
        )
        .unwrap();
    }

    fs::write(
        &server_log,
        [
            format!(
                "{} - __main__ - INFO - Starting Claude Session Manager...",
                log_timestamp(restart)
            ),
            format!(
                "{} - src.session_manager - INFO - Created session claude-analyticsa (id=analyticsa)",
                log_timestamp(spawn_current)
            ),
            format!(
                "{} - src.session_manager - INFO - Created session with CLI prompt should be ignored",
                log_timestamp(now - time::Duration::minutes(75))
            ),
            format!(
                "{} - src.session_manager - INFO - Created session codex-fork-old (id=old)",
                log_timestamp(spawn_previous)
            ),
            format!(
                "{} - src.infra_supervisor - WARNING - Recovered android attach sshd via launchctl",
                log_timestamp(self_heal)
            ),
        ]
        .join("\n"),
    )
    .unwrap();

    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        mobile_analytics: MobileAnalyticsConfig {
            message_queue_db: queue_db.display().to_string(),
            server_log_file: server_log.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/client/analytics/summary").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["window_hours"], 24);
    assert!(payload["generated_at"].is_string());
    assert_eq!(payload["kpis"]["active_sessions"]["value"], 2);
    assert_eq!(payload["kpis"]["sends_24h"]["value"], 2);
    assert_eq!(payload["kpis"]["sends_24h"]["delta_pct"], 100.0);
    assert_eq!(payload["kpis"]["spawns_24h"]["value"], 1);
    assert_eq!(payload["kpis"]["spawns_24h"]["delta_pct"], 0.0);
    assert_eq!(payload["kpis"]["active_tracks"]["value"], 2);
    assert_eq!(payload["kpis"]["overdue_tracks"]["value"], 1);
    assert_eq!(payload["kpis"]["incidents_24h"]["value"], 2);
    assert_eq!(payload["totals"]["tokens_live"], 2000);
    assert_eq!(payload["totals"]["track_reminders_24h"], 1);
    assert_eq!(payload["reliability"]["restart_count_24h"], 1);
    assert_eq!(payload["reliability"]["self_heal_count_24h"], 1);
    assert_eq!(
        payload["state_distribution"],
        json!([
            {"key": "working", "label": "working", "count": 1},
            {"key": "thinking", "label": "thinking", "count": 1},
            {"key": "waiting", "label": "waiting", "count": 0},
            {"key": "idle", "label": "idle", "count": 0}
        ])
    );
    assert_eq!(
        payload["provider_distribution"].as_array().unwrap().len(),
        2
    );
    assert_eq!(payload["repo_distribution"].as_array().unwrap().len(), 2);
    assert_eq!(payload["longest_running"][0]["id"], "analyticsa");
    assert_eq!(payload["throughput"].as_array().unwrap().len(), 12);
    assert_eq!(payload["health_checks"], json!([]));
    assert_eq!(payload["attach_available"], true);
}

#[tokio::test]
async fn client_analytics_summary_uses_zero_fallbacks_for_missing_telemetry_files() {
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        mobile_analytics: MobileAnalyticsConfig {
            message_queue_db: state_file
                .with_extension("missing-queue.db")
                .display()
                .to_string(),
            server_log_file: state_file
                .with_extension("missing.log")
                .display()
                .to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/client/analytics/summary").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["kpis"]["active_sessions"]["value"], 0);
    assert_eq!(payload["kpis"]["sends_24h"]["value"], 0);
    assert_eq!(payload["kpis"]["active_tracks"]["value"], 0);
    assert_eq!(payload["reliability"]["restart_count_24h"], 0);
    assert_eq!(payload["totals"]["tokens_live"], 0);
    assert_eq!(payload["throughput"].as_array().unwrap().len(), 12);
}

#[tokio::test]
async fn client_analytics_summary_rejects_public_host_without_auth() {
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));

    let (status, payload) =
        get_json_with_host(app, "/client/analytics/summary", "sm.example.com").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["detail"], "Authentication required");
    assert_eq!(
        payload["login_url"],
        "/auth/google/login?next=%2Fclient%2Fanalytics%2Fsummary"
    );
}

#[tokio::test]
async fn client_request_status_prompts_live_sessions() {
    let state_file = unique_temp_path();
    let first_log = unique_temp_path();
    let second_log = unique_temp_path();
    let codex_app_log = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "mobileone",
                    "name": "claude-mobileone",
                    "working_dir": "/repo",
                    "tmux_session": "claude-mobileone",
                    "log_file": first_log.display().to_string(),
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                },
                {
                    "id": "mobiletwo",
                    "name": "claude-mobiletwo",
                    "working_dir": "/repo",
                    "tmux_session": "claude-mobiletwo",
                    "log_file": second_log.display().to_string(),
                    "status": "waiting_permission",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                },
                {
                    "id": "mobilecodexapp",
                    "name": "codex-app-mobile",
                    "working_dir": "/repo",
                    "tmux_session": "codex-app-mobile",
                    "provider": "codex-app",
                    "log_file": codex_app_log.display().to_string(),
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                },
                {
                    "id": "mobilestopped",
                    "name": "claude-mobilestopped",
                    "working_dir": "/repo",
                    "tmux_session": "claude-mobilestopped",
                    "status": "stopped",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let mut config = config_with_state_file(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(app, "/client/request-status", json!({})).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "requested");
    assert_eq!(
        payload["prompt"],
        "[sm] user requests status, please update now using sm status"
    );
    assert_eq!(payload["targeted_count"], 3);
    assert_eq!(payload["delivered_count"], 2);
    assert_eq!(payload["queued_count"], 0);
    assert_eq!(payload["failed_count"], 1);
    assert_eq!(
        payload["targeted_session_ids"],
        json!(["mobileone", "mobiletwo", "mobilecodexapp"])
    );
    let first_output = fs::read_to_string(first_log).unwrap();
    assert!(first_output.contains("[sm] user requests status"));
    let second_output = fs::read_to_string(second_log).unwrap();
    assert!(second_output.contains("[sm] user requests status"));
    let codex_app_output = fs::read_to_string(codex_app_log).unwrap_or_default();
    assert!(!codex_app_output.contains("[sm] user requests status"));
}

#[tokio::test]
async fn client_bug_report_persists_sqlite_row_and_debug_state() {
    let state_file = write_session_fixture();
    let bug_db = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        bug_reports: BugReportsConfig {
            db_path: bug_db.display().to_string(),
            max_reports: 30,
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app,
        "/client/bug-reports",
        json!({
            "report_text": "mobile bug report",
            "include_debug_state": true,
            "selected_session_id": "run12345",
            "client_state": {"route": "/watch/", "screen": "sessions"},
            "app_version": "0.3.0",
            "artifact_hash": "deadbeef"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "submitted");
    assert_eq!(payload["maintainer_notified"], false);
    let bug_id = payload["bug_id"].as_str().unwrap();
    assert!(bug_id.starts_with("BR-"));
    let conn = Connection::open(&bug_db).unwrap();
    let row: (
        String,
        Option<String>,
        String,
        String,
        i64,
        String,
        String,
        Option<String>,
    ) = conn
        .query_row(
            r#"
            SELECT report_text, reported_by, route, app_version, include_debug_state,
                   client_state_json, server_state_json, maintainer_delivery_result
            FROM bug_reports
            WHERE id = ?
            "#,
            [bug_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0, "mobile bug report");
    assert_eq!(row.1, None);
    assert_eq!(row.2, "/watch/");
    assert_eq!(row.3, "0.3.0");
    assert_eq!(row.4, 1);
    assert!(row.5.contains("\"screen\":\"sessions\""));
    assert!(row.6.contains("\"selected_session\""));
    assert_eq!(row.7.as_deref(), Some("maintainer_not_found"));
}

#[tokio::test]
async fn client_bug_report_notifies_registered_maintainer() {
    let state_file = unique_temp_path();
    let maintainer_log = unique_temp_path();
    let selected_log = unique_temp_path();
    let bug_db = unique_temp_path();
    fs::write(&maintainer_log, "").unwrap();
    fs::write(&selected_log, "").unwrap();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "maint01",
                    "name": "claude-maint01",
                    "working_dir": "/repo",
                    "tmux_session": "claude-maint01",
                    "tmux_socket_name": null,
                    "node": "primary",
                    "provider": "claude",
                    "log_file": maintainer_log.display().to_string(),
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "friendly_name": "maintainer"
                },
                {
                    "id": "run12345",
                    "name": "claude-run12345",
                    "working_dir": "/repo",
                    "tmux_session": "claude-run12345",
                    "node": "primary",
                    "provider": "claude",
                    "log_file": selected_log.display().to_string(),
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ],
            "agent_registrations": [
                {
                    "role": "maintainer",
                    "session_id": "maint01",
                    "created_at": "2026-06-01T00:02:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        bug_reports: BugReportsConfig {
            db_path: bug_db.display().to_string(),
            max_reports: 30,
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app,
        "/client/bug-reports",
        json!({
            "report_text": "important   mobile\nfailure",
            "include_debug_state": false,
            "selected_session_id": "run12345"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["maintainer_notified"], true);
    let bug_id = payload["bug_id"].as_str().unwrap();
    let maintainer_output = fs::read_to_string(&maintainer_log).unwrap();
    assert!(maintainer_output.contains(&format!("[app bug] {bug_id}")));
    assert!(maintainer_output.contains("report: important mobile failure"));
    assert!(maintainer_output.contains("session: run12345"));
    assert!(maintainer_output.contains(&format!("db: {}", bug_db.display())));
    let conn = Connection::open(&bug_db).unwrap();
    let delivery_result: String = conn
        .query_row(
            "SELECT maintainer_delivery_result FROM bug_reports WHERE id = ?",
            [bug_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delivery_result, "delivered");
}

#[tokio::test]
async fn client_bug_report_enforces_auth_and_payload_bounds() {
    let state_file = write_session_fixture();
    let bug_db = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        bug_reports: BugReportsConfig {
            db_path: bug_db.display().to_string(),
            max_reports: 30,
        },
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            client_id: Some("web-client-id".to_owned()),
            android_client_id: Some("android-client-id".to_owned()),
            client_secret: Some("web-client-secret".to_owned()),
            redirect_uri: Some("https://sm.example.com/auth/google/callback".to_owned()),
            allowlist_emails: vec!["rajesh@example.com".to_owned()],
            session_cookie_secret: Some("session-cookie-secret".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json_with_headers_and_peer(
        app.clone(),
        "/client/bug-reports",
        json!({ "report_text": "public unauth" }),
        &[("host", "sm.example.com")],
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["detail"], "Authentication required");

    let token = device_access_token("session-cookie-secret", "rajesh@example.com", "Rajesh");
    let (status, payload) = post_json_with_headers_and_peer(
        app,
        "/client/bug-reports",
        json!({
            "report_text": "ok",
            "include_debug_state": true,
            "client_state": {"blob": "x".repeat(100_001)}
        }),
        &[
            ("host", "sm.example.com"),
            ("authorization", &format!("Bearer {token}")),
        ],
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(payload["detail"]
        .as_str()
        .unwrap()
        .contains("client_state exceeds"));
}

#[tokio::test]
async fn human_lookup_lists_capabilities_without_email_addresses() {
    let state_file = write_session_fixture();
    let bridge_config = write_email_bridge_config(None, None);
    let app = router(AppState::new(config_with_state_file_and_email(
        &state_file,
        &bridge_config,
        false,
    )));

    let (status, payload) = get_json(app.clone(), "/humans").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["humans"][0]["recipient"], "operator");
    assert_eq!(payload["humans"][0]["available_channels"], json!(["email"]));
    assert_eq!(payload["humans"][0]["email_use"], "fallback_only");
    assert!(
        !serde_json::to_string(&payload)
            .unwrap()
            .contains("operator@example.com"),
        "human listing must not expose private email addresses"
    );

    let (status, payload) = get_json(app, "/humans/owner").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["recipient"], "operator");
    assert!(
        !serde_json::to_string(&payload)
            .unwrap()
            .contains("operator@example.com"),
        "human lookup must not expose private email addresses"
    );
}

#[tokio::test]
async fn registered_email_send_posts_resend_payload_with_routing_footer() {
    let (resend_url, request_rx) = spawn_resend_server(200, r#"{"id":"email_123"}"#);
    let state_file = write_session_fixture();
    let bridge_config = write_email_bridge_config(Some(&resend_url), None);
    let app = router(AppState::new(config_with_state_file_and_email(
        &state_file,
        &bridge_config,
        false,
    )));

    let (status, payload) = post_json(
        app,
        "/email/send",
        json!({
            "requester_session_id": "run12345",
            "recipients": ["teammate"],
            "subject": "Status",
            "body_text": "hello from rust"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["message_id"], "email_123");
    assert_eq!(payload["to"][0]["username"], "teammate");
    assert_eq!(payload["to"][0]["email"], "teammate@example.com");
    let request = request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(request["path"], "/emails");
    assert_eq!(request["authorization"], "Bearer test-api-key");
    assert_eq!(request["body"]["to"], json!(["teammate@example.com"]));
    assert_eq!(request["body"]["reply_to"], "reply@example.com");
    assert!(request["body"]["text"]
        .as_str()
        .unwrap()
        .contains("SM: Runner Native run12345 claude"));
    assert_eq!(request["body"]["headers"]["X-SM-Session-ID"], "run12345");
}

#[tokio::test]
async fn explicit_human_email_is_required_for_human_recipients() {
    let state_file = write_session_fixture();
    let bridge_config = write_email_bridge_config(None, None);
    let app = router(AppState::new(config_with_state_file_and_email(
        &state_file,
        &bridge_config,
        false,
    )));

    let (status, payload) = post_json(
        app.clone(),
        "/email/send",
        json!({
            "requester_session_id": "run12345",
            "recipients": ["owner"],
            "subject": "Status",
            "body_text": "hello"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(payload["detail"]
        .as_str()
        .unwrap()
        .contains("must use explicit human email delivery"));

    let (resend_url, request_rx) = spawn_resend_server(200, r#"{"id":"human_123"}"#);
    fs::write(&bridge_config, email_bridge_yaml(Some(&resend_url), None)).unwrap();
    let (status, payload) = post_json(
        app,
        "/humans/operator/email",
        json!({
            "requester_session_id": "run12345",
            "text": "human fallback body"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "sent");
    assert_eq!(payload["recipient"], "operator");
    assert_eq!(payload["to"], json!([{ "username": "operator" }]));
    let request = request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(request["body"]["to"], json!(["operator@example.com"]));
    assert_eq!(request["body"]["subject"], "human fallback body");
}

#[tokio::test]
async fn inbound_email_requires_worker_proof_and_delivers_to_session() {
    let state_file = write_session_fixture();
    let bridge_config = write_email_bridge_config(None, Some("worker-secret"));
    let app = router(AppState::new(config_with_state_file_and_email(
        &state_file,
        &bridge_config,
        true,
    )));

    let (status, payload) = post_json(
        app.clone(),
        "/api/email-inbound",
        json!({
            "from_address": "operator@example.com",
            "body": "reply\n\n--\nSM: Runner run12345 claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["detail"], "Invalid email worker secret");

    let (status, payload) = post_json_with_headers_and_peer(
        app.clone(),
        "/api/email-inbound",
        json!({
            "from_address": "intruder@example.com",
            "body": "reply\n\n--\nSM: Runner run12345 claude"
        }),
        &[("x-email-worker-secret", "worker-secret")],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(payload["detail"], "Inbound sender is not authorized");

    let (status, payload) = post_json_with_headers_and_peer(
        app.clone(),
        "/api/email-inbound",
        json!({
            "from_address": "operator@example.com",
            "body": "no routing footer"
        }),
        &[("x-email-worker-secret", "worker-secret")],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "ignored");
    assert_eq!(payload["reason"], "missing_routing_footer");

    let (status, payload) = post_json_with_headers_and_peer(
        app.clone(),
        "/api/email-inbound",
        json!({
            "from_address": "operator@example.com",
            "raw_email": concat!(
                "Subject: Re: status\r\n",
                "Content-Type: multipart/alternative; boundary=\"sm-reply-boundary\"\r\n",
                "\r\n",
                "--sm-reply-boundary\r\n",
                "Content-Type: text/html; charset=utf-8\r\n",
                "\r\n",
                "<p>html fallback should not be delivered</p>\r\n",
                "--sm-reply-boundary\r\n",
                "Content-Type: text/plain; charset=utf-8\r\n",
                "Content-Transfer-Encoding: quoted-printable\r\n",
                "\r\n",
                "Here=20is=20the=20reply=0A=0A--=0ASM:=20Runner=20run12345=20claude\r\n",
                "--sm-reply-boundary--\r\n",
            )
        }),
        &[("x-email-worker-secret", "worker-secret")],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "sent");
    assert_eq!(payload["session_id"], "run12345");
    assert_eq!(payload["restored"], false);
    assert_eq!(payload["delivery_result"], "delivered");

    let (_status, output) = get_json(app.clone(), "/sessions/run12345/output?lines=5").await;
    assert!(output["output"]
        .as_str()
        .unwrap()
        .contains("{sm email from operator@example.com subj: Re: status}\nHere is the reply"));

    let (status, payload) = post_json_with_headers_and_peer(
        app.clone(),
        "/api/email-inbound",
        json!({
            "from_address": "operator@example.com",
            "raw_email": "Subject: Re: parsed fallback\r\n\r\n",
            "body": "Parsed fallback body\n\n--\nSM: Runner run12345 claude"
        }),
        &[("x-email-worker-secret", "worker-secret")],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "sent");
    assert_eq!(payload["session_id"], "run12345");

    let (_status, output) = get_json(app, "/sessions/run12345/output?lines=10").await;
    assert!(output["output"].as_str().unwrap().contains(
        "{sm email from operator@example.com subj: Re: parsed fallback}\nParsed fallback body"
    ));
}

#[tokio::test]
async fn app_artifact_upload_metadata_and_downloads_are_auth_gated() {
    let artifact_root = unique_short_temp_dir("sm-rust-app-artifacts");
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        app_artifacts: AppArtifactsConfig {
            root_dir: artifact_root.display().to_string(),
        },
        ..AppConfig::default()
    }));
    let boundary = "sm-rust-boundary";
    let body = multipart_app_upload(boundary, b"apk-bytes", Some("7"), Some("0.1.7"));

    let (status, _headers, response_body) = post_multipart_with_host_and_peer(
        app.clone(),
        "/deploy/session-manager-android",
        "localhost",
        body,
        boundary,
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_slice(&response_body).unwrap();
    let artifact_hash = Sha256::digest(b"apk-bytes")
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        payload,
        json!({
            "ok": true,
            "app": "session-manager-android",
            "size_bytes": 9,
            "download_url": "/apps/session-manager-android/latest.apk",
            "artifact_hash": artifact_hash
        })
    );
    let app_dir = artifact_root.join("session-manager-android");
    assert_eq!(fs::read(app_dir.join("latest.apk")).unwrap(), b"apk-bytes");
    assert_eq!(
        fs::read(app_dir.join(format!("{artifact_hash}.apk"))).unwrap(),
        b"apk-bytes"
    );
    let metadata: Value =
        serde_json::from_str(&fs::read_to_string(app_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(metadata["artifact_hash"], artifact_hash);
    assert_eq!(metadata["uploaded_by"], "local_bypass");
    assert_eq!(metadata["version_code"], 7);
    assert_eq!(metadata["version_name"], "0.1.7");

    let (status, headers, body) =
        get_response(app.clone(), "/apps/session-manager-android/latest.apk").await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(
        headers
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some(format!("/apps/session-manager-android/{artifact_hash}.apk").as_str())
    );
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );
    assert!(body.is_empty());

    let (status, headers, body) = get_response(
        app.clone(),
        &format!("/apps/session-manager-android/{artifact_hash}.apk"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"apk-bytes");
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(
        headers
            .get("content-disposition")
            .and_then(|value| value.to_str().ok()),
        Some("attachment; filename=\"session-manager-android.apk\"")
    );

    let (status, metadata_payload) =
        get_json(app.clone(), "/apps/session-manager-android/meta.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(metadata_payload["artifact_hash"], artifact_hash);

    let (status, headers, _) = get_response(app, "/apk").await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(
        headers
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("/apps/session-manager-android/latest.apk")
    );
    let _ = fs::remove_dir_all(artifact_root);
}

#[tokio::test]
async fn app_artifact_upload_rejects_encoded_whitespace_app_name() {
    let artifact_root = unique_short_temp_dir("sm-rust-app-artifacts-invalid");
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        app_artifacts: AppArtifactsConfig {
            root_dir: artifact_root.display().to_string(),
        },
        ..AppConfig::default()
    }));
    let boundary = "sm-rust-boundary-invalid";
    let body = multipart_app_upload(boundary, b"apk-bytes", None, None);

    let (status, _headers, response_body) = post_multipart_with_host_and_peer(
        app,
        "/deploy/session-manager-android%20",
        "localhost",
        body,
        boundary,
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let payload: Value = serde_json::from_slice(&response_body).unwrap();
    assert_eq!(payload["detail"], "Invalid app name");
    assert!(!artifact_root.join("session-manager-android ").exists());
    let _ = fs::remove_dir_all(artifact_root);
}

#[tokio::test]
async fn app_artifacts_reject_public_unauthenticated_access_when_auth_enabled() {
    let artifact_root = unique_short_temp_dir("sm-rust-app-artifacts-auth");
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        app_artifacts: AppArtifactsConfig {
            root_dir: artifact_root.display().to_string(),
        },
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            client_id: Some("web-client-id".to_owned()),
            android_client_id: Some("android-client-id".to_owned()),
            client_secret: Some("web-client-secret".to_owned()),
            redirect_uri: Some("https://sm.example.com/auth/google/callback".to_owned()),
            allowlist_emails: vec!["rajesh@example.com".to_owned()],
            session_cookie_secret: Some("session-cookie-secret".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json_with_host(
        app.clone(),
        "/apps/session-manager-android/meta.json",
        "sm.example.com",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["detail"], "Authentication required");

    let boundary = "sm-rust-boundary-denied";
    let body = multipart_app_upload(boundary, b"apk-bytes", None, None);
    let (status, _headers, body) = post_multipart_with_host_and_peer(
        app,
        "/deploy/session-manager-android",
        "sm.example.com",
        body,
        boundary,
        &[],
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload["detail"], "Authentication required");
    let _ = fs::remove_dir_all(artifact_root);
}

#[tokio::test]
async fn codex_review_requests_missing_db_returns_empty_requests() {
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: state_file
                .with_extension("missing-codex-review.db")
                .display()
                .to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app.clone(), "/codex-review-requests").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "requests": [] }));

    let (status, payload) = get_json(app, "/codex-review-requests/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Codex review request not found");
}

#[tokio::test]
async fn codex_review_requests_lists_rows_with_filters_and_session_names() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-requests.db");
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "requester1",
                    "name": "codex-fork-requester1",
                    "working_dir": "/repo/requester",
                    "tmux_session": "codex-fork-requester1",
                    "log_file": "/tmp/requester1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "provider": "codex-fork",
                    "friendly_name": "stale requester",
                    "friendly_name_updated_at_ns": 10,
                    "native_title": "native requester",
                    "native_title_updated_at_ns": 20
                },
                {
                    "id": "notify1",
                    "name": "codex-fork-notify1",
                    "working_dir": "/repo/notify",
                    "tmux_session": "codex-fork-notify1",
                    "log_file": "/tmp/notify1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "native_title": "native notify"
                },
                {
                    "id": "notify2",
                    "name": "codex-fork-notify2",
                    "working_dir": "/repo/notify",
                    "tmux_session": "codex-fork-notify2",
                    "log_file": "/tmp/notify2.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }
            ],
            "agent_registrations": [
                {
                    "role": "reviewer",
                    "session_id": "notify1",
                    "created_at": "2026-06-01T00:02:00Z"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    create_codex_review_request_fixture_db(&queue_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app.clone(), "/codex-review-requests").await;
    assert_eq!(status, StatusCode::OK);
    let requests = payload["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["id"], "active-old");
    assert_eq!(requests[0]["repo"], "rajeshgoli/session-manager");
    assert_eq!(requests[0]["pr_number"], 830);
    assert_eq!(requests[0]["requester_name"], "native requester");
    assert_eq!(requests[0]["notify_name"], "reviewer");
    assert_eq!(requests[0]["latest_request_comment_id"], 111);
    assert_eq!(requests[0]["review_comment_id"], 222);
    assert_eq!(requests[0]["is_active"], true);
    assert_eq!(requests[1]["id"], "active-new");
    assert_eq!(requests[1]["review_comment_id"], "R_kw123");

    let (status, payload) = get_json(app.clone(), "/codex-review-requests/active-old").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "active-old");
    assert_eq!(payload["repo"], "rajeshgoli/session-manager");
    assert_eq!(payload["pr_number"], 830);
    assert_eq!(payload["requester_name"], "native requester");
    assert_eq!(payload["notify_name"], "reviewer");
    assert_eq!(payload["latest_request_comment_id"], 111);
    assert_eq!(payload["review_comment_id"], 222);
    assert_eq!(payload["is_active"], true);

    let (status, payload) = get_json(app.clone(), "/codex-review-requests/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Codex review request not found");

    let (status, payload) = get_json(
        app.clone(),
        "/codex-review-requests?repo=rajeshgoli/session-manager&pr_number=830&notify_target=notify1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["requests"].as_array().unwrap().len(), 1);
    assert_eq!(payload["requests"][0]["id"], "active-old");

    let (status, payload) = get_json(app.clone(), "/registry/reviewer").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "notify1");

    let (status, payload) = get_json(
        app.clone(),
        "/codex-review-requests?repo=rajeshgoli/session-manager&pr_number=830&notify_target=reviewer",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["requests"].as_array().unwrap().len(), 1);
    assert_eq!(payload["requests"][0]["id"], "active-old");

    let (status, payload) =
        get_json(app.clone(), "/codex-review-requests?include_inactive=true").await;
    assert_eq!(status, StatusCode::OK);
    let ids = payload["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["active-old", "inactive", "active-new"]);

    let (status, payload) = get_json(app, "/codex-review-requests/inactive").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "inactive");
    assert_eq!(payload["state"], "cancelled");
    assert_eq!(payload["is_active"], false);
}

#[tokio::test]
async fn codex_review_request_cancel_updates_active_row_and_preserves_inactive_row() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-cancel.db");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    create_codex_review_request_fixture_db(&queue_db);
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) =
        delete_json(app.clone(), "/codex-review-requests/active-old", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "active-old");
    assert_eq!(payload["state"], "cancelled");
    assert_eq!(payload["is_active"], false);
    assert_eq!(payload["last_error"], Value::Null);

    let conn = Connection::open(&queue_db).unwrap();
    let (state, is_active): (String, i64) = conn
        .query_row(
            "SELECT state, is_active FROM codex_review_request_registrations WHERE id = 'active-old'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "cancelled");
    assert_eq!(is_active, 0);

    let (status, payload) = get_json(app.clone(), "/codex-review-requests").await;
    assert_eq!(status, StatusCode::OK);
    let active_ids = payload["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(active_ids, vec!["active-new"]);

    let (status, payload) = delete_json(app, "/codex-review-requests/inactive", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "inactive");
    assert_eq!(payload["state"], "cancelled");
    assert_eq!(payload["is_active"], false);
}

#[tokio::test]
async fn codex_review_request_create_posts_and_persists_active_row() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-create.db");
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "requester1",
                    "name": "codex-fork-requester1",
                    "working_dir": "/repo/requester",
                    "tmux_session": "codex-fork-requester1",
                    "log_file": "/tmp/requester1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "provider": "codex-fork",
                    "native_title": "native requester"
                },
                {
                    "id": "notify1",
                    "name": "codex-fork-notify1",
                    "working_dir": "/repo/notify",
                    "tmux_session": "codex-fork-notify1",
                    "log_file": "/tmp/notify1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }
            ],
            "agent_registrations": [
                {
                    "role": "reviewer",
                    "session_id": "notify1",
                    "created_at": "2026-06-01T00:02:00Z"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let poster = StubGitHubReviewPoster::successful();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config).with_github_review_poster(Arc::new(poster.clone())));

    let (status, payload) = post_json(
        app.clone(),
        "/codex-review-requests",
        json!({
            "pr_number": 967,
            "repo": "rajeshgoli/session-manager",
            "steer": "focus create",
            "requester_session_id": "requester1",
            "notify_target": "reviewer",
            "poll_interval_seconds": 45,
            "retry_interval_seconds": 900
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let request_id = payload["id"].as_str().unwrap();
    assert_eq!(request_id.len(), 12);
    assert!(request_id.chars().all(|ch| ch.is_ascii_hexdigit()));
    assert_eq!(payload["repo"], "rajeshgoli/session-manager");
    assert_eq!(payload["pr_number"], 967);
    assert_eq!(payload["requester_session_id"], "requester1");
    assert_eq!(payload["requester_name"], "native requester");
    assert_eq!(payload["notify_session_id"], "notify1");
    assert_eq!(payload["notify_name"], "reviewer");
    assert_eq!(payload["steer"], "focus create");
    assert_eq!(payload["latest_request_comment_id"], 4701290334_i64);
    assert_eq!(
        payload["latest_request_comment_url"],
        "https://github.com/rajeshgoli/session-manager/pull/967#issuecomment-4701290334"
    );
    assert_eq!(payload["attempt_count"], 1);
    assert_eq!(payload["poll_interval_seconds"], 45);
    assert_eq!(payload["retry_interval_seconds"], 900);
    assert_eq!(payload["state"], "active");
    assert_eq!(payload["is_active"], true);
    assert_eq!(
        poster.calls(),
        vec![(
            "rajeshgoli/session-manager".to_owned(),
            967,
            Some("focus create".to_owned())
        )]
    );

    let conn = Connection::open(&queue_db).unwrap();
    let row: (String, i64, String, i64, String, String, i64) = conn
        .query_row(
            r#"
            SELECT id, latest_request_comment_id, latest_request_comment_url,
                   attempt_count, state, next_retry_at, is_active
            FROM codex_review_request_registrations
            WHERE id = ?1
            "#,
            [request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0, request_id);
    assert_eq!(row.1, 4701290334);
    assert_eq!(
        row.2,
        "https://github.com/rajeshgoli/session-manager/pull/967#issuecomment-4701290334"
    );
    assert_eq!(row.3, 1);
    assert_eq!(row.4, "active");
    assert_eq!(row.5, "2026-06-14T02:45:00Z");
    assert_eq!(row.6, 1);

    let (status, payload) = get_json(app, &format!("/codex-review-requests/{request_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], request_id);
}

#[tokio::test]
async fn pr_review_route_posts_comment_and_returns_python_shape() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "caller1",
                    "name": "codex-fork-caller1",
                    "working_dir": "/repo/caller",
                    "tmux_session": "codex-fork-caller1",
                    "log_file": "/tmp/caller1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "provider": "codex-fork"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let queue_db = state_file.with_extension("pr-review-route-message-queue.db");
    let poster = StubGitHubReviewPoster::successful().with_fresh_review(GitHubReviewMatch {
        source: "pull_review".to_owned(),
        created_at: "2026-06-14T02:30:01Z".to_owned(),
        id: Some(json!("PRR_kw123")),
        url: Some(
            "https://github.com/rajeshgoli/session-manager/pull/967#pullrequestreview-1".to_owned(),
        ),
    });
    let mut config = AppConfig::default();
    config.paths.state_file = state_file.display().to_string();
    config.sm_send.db_path = queue_db.display().to_string();
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config).with_github_review_poster(Arc::new(poster.clone())));

    let (status, payload) = post_json(
        app,
        "/reviews/pr",
        json!({
            "pr_number": 967,
            "repo": "rajeshgoli/session-manager",
            "steer": "focus create",
            "wait": 600,
            "caller_session_id": "caller1"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["repo"], "rajeshgoli/session-manager");
    assert_eq!(payload["pr_number"], 967);
    assert_eq!(payload["posted_at"], "2026-06-14T02:30:00Z");
    assert_eq!(payload["comment_id"], 4701290334_i64);
    assert_eq!(payload["comment_body"], "@codex review for focus create");
    assert_eq!(payload["status"], "posted");
    assert_eq!(payload["server_polling"], true);
    assert_eq!(
        poster.calls(),
        vec![(
            "rajeshgoli/session-manager".to_owned(),
            967,
            Some("focus create".to_owned())
        )]
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        queued_message_texts(&queue_db, "caller1"),
        vec!["Review --pr 967 (rajeshgoli/session-manager) completed: Codex posted review on PR #967"]
    );
}

#[tokio::test]
async fn pr_review_route_returns_error_payloads_and_preserves_write_gate() {
    let poster = StubGitHubReviewPoster::failing("PR #999 not found in owner/repo");
    let mut config = AppConfig::default();
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config).with_github_review_poster(Arc::new(poster.clone())));

    let (status, payload) = post_json(
        app.clone(),
        "/reviews/pr",
        json!({
            "pr_number": 999,
            "repo": "owner/repo"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["error"], "PR #999 not found in owner/repo");
    assert_eq!(poster.calls(), vec![("owner/repo".to_owned(), 999, None)]);

    let (status, payload) = post_json(app.clone(), "/reviews/pr", json!({ "pr_number": 42 })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["error"],
        "Could not determine repo. Provide --repo or run from a git directory."
    );

    let gated_app = router(AppState::new(AppConfig::default()));
    let (status, payload) = post_json(
        gated_app,
        "/reviews/pr",
        json!({
            "pr_number": 42,
            "repo": "owner/repo"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(payload["detail"]
        .as_str()
        .unwrap()
        .contains("Rust core writes are disabled"));
}

#[tokio::test]
async fn codex_review_request_watcher_completes_and_queues_wake() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-create-watch.db");
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "notify1",
                    "name": "codex-fork-notify1",
                    "working_dir": "/repo/notify",
                    "tmux_session": "codex-fork-notify1",
                    "log_file": "/tmp/notify1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let poster = StubGitHubReviewPoster::successful().with_fresh_review(GitHubReviewMatch {
        source: "comment".to_owned(),
        created_at: "2026-06-14T02:31:00Z".to_owned(),
        id: Some(json!(4701300000_i64)),
        url: Some(
            "https://github.com/rajeshgoli/session-manager/pull/967#issuecomment-4701300000"
                .to_owned(),
        ),
    });
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config).with_github_review_poster(Arc::new(poster)));

    let (status, payload) = post_json(
        app,
        "/codex-review-requests",
        json!({
            "pr_number": 967,
            "repo": "rajeshgoli/session-manager",
            "notify_target": "notify1",
            "poll_interval_seconds": 1,
            "retry_interval_seconds": 900
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request_id = payload["id"].as_str().unwrap().to_owned();

    let mut completed = None;
    for _ in 0..30 {
        let conn = Connection::open(&queue_db).unwrap();
        let row: (String, i64, Option<String>) = conn
            .query_row(
                r#"
                SELECT state, is_active, review_url
                FROM codex_review_request_registrations
                WHERE id = ?1
                "#,
                [&request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        if row.0 == "completed" {
            completed = Some(row);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let completed = completed.expect("watcher should complete retained request");
    assert_eq!(completed.1, 0);
    assert_eq!(
        completed.2.as_deref(),
        Some("https://github.com/rajeshgoli/session-manager/pull/967#issuecomment-4701300000")
    );

    let conn = Connection::open(&queue_db).unwrap();
    let message: String = conn
        .query_row(
            "SELECT text FROM message_queue WHERE target_session_id = 'notify1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        message,
        "[sm review] Codex comment for PR #967 is here. https://github.com/rajeshgoli/session-manager/pull/967#issuecomment-4701300000"
    );
}

#[tokio::test]
async fn codex_review_request_recovery_spawns_active_watchers() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-recover-watch.db");
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "notify1",
                    "name": "codex-fork-notify1",
                    "working_dir": "/repo/notify",
                    "tmux_session": "codex-fork-notify1",
                    "log_file": "/tmp/notify1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    create_active_codex_review_request_fixture_db(&queue_db, "recovered-watch", "notify1", 1);
    let poster = StubGitHubReviewPoster::successful().with_fresh_review(GitHubReviewMatch {
        source: "pull_review".to_owned(),
        created_at: "2026-06-14T02:31:00Z".to_owned(),
        id: Some(json!("PRR_kwRecovered")),
        url: Some(
            "https://github.com/rajeshgoli/session-manager/pull/971#pullrequestreview-1".to_owned(),
        ),
    });
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.runtime_enabled = true;
    let _app = router(AppState::new(config).with_github_review_poster(Arc::new(poster)));

    let mut completed = None;
    for _ in 0..30 {
        let conn = Connection::open(&queue_db).unwrap();
        let row: (String, i64, Option<String>) = conn
            .query_row(
                r#"
                SELECT state, is_active, review_url
                FROM codex_review_request_registrations
                WHERE id = 'recovered-watch'
                "#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        if row.0 == "completed" {
            completed = Some(row);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let completed = completed.expect("recovered watcher should complete retained request");
    assert_eq!(completed.1, 0);
    assert_eq!(
        completed.2.as_deref(),
        Some("https://github.com/rajeshgoli/session-manager/pull/971#pullrequestreview-1")
    );

    let conn = Connection::open(&queue_db).unwrap();
    let message: String = conn
        .query_row(
            "SELECT text FROM message_queue WHERE target_session_id = 'notify1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        message,
        "[sm review] Codex review for PR #971 is here. https://github.com/rajeshgoli/session-manager/pull/971#pullrequestreview-1"
    );
}

#[tokio::test]
async fn codex_review_request_recovery_cancels_missing_notify_session() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-recover-missing-notify.db");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    create_active_codex_review_request_fixture_db(&queue_db, "missing-notify", "notify-missing", 1);
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.runtime_enabled = true;
    let _app = router(AppState::new(config));

    let conn = Connection::open(&queue_db).unwrap();
    let row: (String, i64, Option<String>) = conn
        .query_row(
            r#"
            SELECT state, is_active, last_error
            FROM codex_review_request_registrations
            WHERE id = 'missing-notify'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row.0, "cancelled");
    assert_eq!(row.1, 0);
    assert_eq!(row.2.as_deref(), Some("Notify session no longer exists"));
}

#[tokio::test]
async fn codex_review_request_create_rejects_duplicates_before_github_post() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-create-duplicate.db");
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "notify1",
                    "name": "codex-fork-notify1",
                    "working_dir": "/repo/notify",
                    "tmux_session": "codex-fork-notify1",
                    "log_file": "/tmp/notify1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    create_codex_review_request_fixture_db(&queue_db);
    let poster = StubGitHubReviewPoster::successful();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config).with_github_review_poster(Arc::new(poster.clone())));

    let (status, payload) = post_json(
        app,
        "/codex-review-requests",
        json!({
            "pr_number": 830,
            "repo": "rajeshgoli/session-manager",
            "notify_target": "notify1"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload["detail"],
        "Active Codex review request already exists for rajeshgoli/session-manager PR #830"
    );
    assert!(poster.calls().is_empty());
}

#[tokio::test]
async fn codex_review_request_create_preserves_validation_errors_and_write_gate() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-create-errors.db");
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "notify1",
                    "name": "codex-fork-notify1",
                    "working_dir": "/repo/notify",
                    "tmux_session": "codex-fork-notify1",
                    "log_file": "/tmp/notify1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let disabled_app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    }));
    let (status, payload) = post_json(
        disabled_app,
        "/codex-review-requests",
        json!({
            "pr_number": 967,
            "repo": "rajeshgoli/session-manager",
            "notify_target": "notify1"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(payload["detail"], "Rust core writes are disabled");

    let poster = StubGitHubReviewPoster::failing("gh pr comment failed: denied");
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config).with_github_review_poster(Arc::new(poster.clone())));

    let (status, payload) = post_json(
        app.clone(),
        "/codex-review-requests",
        json!({
            "pr_number": 967,
            "repo": "rajeshgoli/session-manager",
            "notify_target": "missing"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Notify target not found");
    assert!(poster.calls().is_empty());

    let (status, payload) = post_json(
        app.clone(),
        "/codex-review-requests",
        json!({
            "pr_number": 967,
            "repo": "rajeshgoli/session-manager",
            "notify_target": "notify1",
            "poll_interval_seconds": 0
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(payload["detail"], "poll_interval_seconds must be > 0");
    assert!(poster.calls().is_empty());

    let (status, payload) = post_json(
        app.clone(),
        "/codex-review-requests",
        json!({
            "pr_number": 0,
            "repo": "rajeshgoli/session-manager",
            "notify_target": "notify1"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(payload["detail"], "pr_number must be > 0");
    assert!(poster.calls().is_empty());

    let (status, payload) = post_json(
        app,
        "/codex-review-requests",
        json!({
            "pr_number": 967,
            "repo": "rajeshgoli/session-manager",
            "notify_target": "notify1"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["detail"], "gh pr comment failed: denied");
    assert_eq!(
        poster.calls(),
        vec![("rajeshgoli/session-manager".to_owned(), 967, None)]
    );
    assert!(!queue_db.exists());
}

#[tokio::test]
async fn codex_review_request_cancel_preserves_missing_and_write_gate_errors() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-cancel-gates.db");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    create_codex_review_request_fixture_db(&queue_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) =
        delete_json(app.clone(), "/codex-review-requests/active-old", json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(payload["detail"], "Rust core writes are disabled");
    let (status, payload) = get_json(app.clone(), "/codex-review-requests/active-old").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["state"], "completed");
    assert_eq!(payload["is_active"], true);

    let mut write_config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    write_config.rust_core.fixture_writes_enabled = true;
    let write_app = router(AppState::new(write_config));
    let (status, payload) =
        delete_json(write_app, "/codex-review-requests/missing", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Codex review request not found");
}

#[tokio::test]
async fn codex_review_requests_unknown_notify_target_returns_404() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-requests-empty.db");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    create_codex_review_request_fixture_db(&queue_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/codex-review-requests?notify_target=missing").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Notify target not found");
}

#[tokio::test]
async fn codex_review_requests_rejects_public_host_without_auth() {
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));

    let (status, payload) =
        get_json_with_host(app.clone(), "/codex-review-requests", "sm.example.com").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["detail"], "Authentication required");
    assert_eq!(
        payload["login_url"],
        "/auth/google/login?next=%2Fcodex-review-requests"
    );

    let (status, payload) = get_json_with_host(
        app.clone(),
        "/codex-review-requests/active-old",
        "sm.example.com",
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["detail"], "Authentication required");
    assert_eq!(
        payload["login_url"],
        "/auth/google/login?next=%2Fcodex-review-requests%2Factive-old"
    );

    let (status, payload) = json_request_with_headers_and_peer(
        app,
        "DELETE",
        "/codex-review-requests/active-old",
        json!({}),
        &[("host", "sm.example.com")],
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["detail"], "Authentication required");
    assert_eq!(
        payload["login_url"],
        "/auth/google/login?next=%2Fcodex-review-requests%2Factive-old"
    );
}

#[tokio::test]
async fn queue_jobs_missing_db_returns_empty_jobs() {
    let state_file = unique_temp_path();
    let queue_state_dir = state_file.with_extension("missing-queue-runner");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/queue-jobs").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "jobs": [] }));

    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    }));
    let (status, payload) = get_json(app, "/queue-jobs/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Queue job not found");
}

#[tokio::test]
async fn queue_jobs_lists_rows_with_filters_and_session_names() {
    let state_file = unique_temp_path();
    let queue_state_dir = state_file.with_extension("queue-runner");
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "requester1",
                    "name": "codex-fork-requester1",
                    "working_dir": "/repo/requester",
                    "tmux_session": "codex-fork-requester1",
                    "log_file": "/tmp/requester1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "provider": "codex-fork",
                    "friendly_name": "stale requester",
                    "friendly_name_updated_at_ns": 10,
                    "native_title": "native requester",
                    "native_title_updated_at_ns": 20
                },
                {
                    "id": "notify1",
                    "name": "codex-fork-notify1",
                    "working_dir": "/repo/notify",
                    "tmux_session": "codex-fork-notify1",
                    "log_file": "/tmp/notify1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "native_title": "native notify"
                },
                {
                    "id": "notify2",
                    "name": "codex-fork-notify2",
                    "working_dir": "/repo/notify",
                    "tmux_session": "codex-fork-notify2",
                    "log_file": "/tmp/notify2.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }
            ],
            "agent_registrations": [
                {
                    "role": "reviewer",
                    "session_id": "notify1",
                    "created_at": "2026-06-01T00:02:00Z"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    create_queue_jobs_fixture_db(&queue_state_dir);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app.clone(), "/queue-jobs").await;
    assert_eq!(status, StatusCode::OK);
    let jobs = payload["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0]["id"], "job-pending");
    assert_eq!(jobs[0]["type"], "tests");
    assert_eq!(jobs[0]["requester_name"], "native requester");
    assert_eq!(jobs[0]["notify_name"], "reviewer");
    assert_eq!(jobs[0]["argv"], json!(["cargo", "test"]));
    assert_eq!(jobs[0]["script_path"], Value::Null);
    assert_eq!(jobs[0]["holding_reason"], "memory");
    assert_eq!(jobs[1]["id"], "job-running");
    assert_eq!(jobs[1]["notify_name"], "codex-fork-notify2");
    assert_eq!(jobs[1]["script_path"], "/tmp/run-perf.sh");
    assert_eq!(jobs[1]["pid"], 4242);

    let (status, payload) = get_json(app.clone(), "/queue-jobs/job-pending").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "job-pending");
    assert_eq!(payload["type"], "tests");
    assert_eq!(payload["requester_name"], "native requester");
    assert_eq!(payload["notify_name"], "reviewer");
    assert_eq!(payload["argv"], json!(["cargo", "test"]));
    assert_eq!(payload["holding_reason"], "memory");

    let (status, payload) = get_json(app.clone(), "/queue-jobs/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Queue job not found");

    let (status, payload) =
        get_json(app.clone(), "/queue-jobs?notify_target=notify1&type=tests").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(payload["jobs"][0]["id"], "job-pending");

    let (status, payload) =
        get_json(app.clone(), "/queue-jobs?notify_target=reviewer&type=tests").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(payload["jobs"][0]["id"], "job-pending");

    let (status, payload) = get_json(app.clone(), "/queue-jobs?state=done").await;
    assert_eq!(status, StatusCode::OK);
    let failed_job = payload["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "job-failed")
        .unwrap();
    assert_eq!(failed_job["requester_session_id"], "missing-requester");
    assert_eq!(failed_job["requester_name"], Value::Null);
    let done_ids = payload["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(done_ids, vec!["job-succeeded", "job-failed"]);

    let (status, payload) = get_json(app.clone(), "/queue-jobs?state=succeeded").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(payload["jobs"][0]["id"], "job-succeeded");

    let (status, payload) = get_json(app, "/queue-jobs?include_terminal=true").await;
    assert_eq!(status, StatusCode::OK);
    let all_ids = payload["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        all_ids,
        vec!["job-pending", "job-running", "job-succeeded", "job-failed"]
    );
}

#[tokio::test]
async fn queue_jobs_uses_custom_state_file_relative_dir_by_default() {
    let base_dir = unique_temp_path().with_extension("queue-job-state-dir");
    fs::create_dir_all(&base_dir).unwrap();
    let state_file = base_dir.join("sessions.json");
    let config_file = base_dir.join("config.yaml");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    fs::write(
        &config_file,
        format!("paths:\n  state_file: \"{}\"\n", state_file.display()),
    )
    .unwrap();
    create_queue_jobs_fixture_db(&base_dir.join("queue-runner"));
    let app = router(AppState::new(
        AppConfig::load_from_path(&config_file).unwrap(),
    ));

    let (status, payload) = get_json(app, "/queue-jobs").await;

    assert_eq!(status, StatusCode::OK);
    let jobs = payload["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0]["id"], "job-pending");
    assert_eq!(jobs[1]["id"], "job-running");
}

#[tokio::test]
async fn queue_jobs_derives_state_dir_for_direct_custom_state_config() {
    let base_dir = unique_temp_path().with_extension("queue-job-direct-state-dir");
    fs::create_dir_all(&base_dir).unwrap();
    let state_file = base_dir.join("sessions.json");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    create_queue_jobs_fixture_db(&base_dir.join("queue-runner"));
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/queue-jobs").await;

    assert_eq!(status, StatusCode::OK);
    let jobs = payload["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0]["id"], "job-pending");
    assert_eq!(jobs[1]["id"], "job-running");
}

#[tokio::test]
async fn queue_jobs_unknown_notify_target_returns_404() {
    let state_file = unique_temp_path();
    let queue_state_dir = state_file.with_extension("queue-runner-empty");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    create_queue_jobs_fixture_db(&queue_state_dir);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/queue-jobs?notify_target=missing").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Notify target not found");
}

#[tokio::test]
async fn queue_jobs_rejects_public_host_without_auth() {
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));

    let (status, payload) = get_json_with_host(app, "/queue-jobs", "sm.example.com").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["detail"], "Authentication required");
    assert_eq!(
        payload["login_url"],
        "/auth/google/login?next=%2Fqueue-jobs"
    );

    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));
    let (status, payload) =
        get_json_with_host(app, "/queue-jobs/job-pending", "sm.example.com").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(payload["detail"], "Authentication required");
    assert_eq!(
        payload["login_url"],
        "/auth/google/login?next=%2Fqueue-jobs%2Fjob-pending"
    );
}

#[tokio::test]
async fn queue_job_create_is_disabled_by_default() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-create-disabled");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app,
        "/queue-jobs",
        json!({
            "type": "tests",
            "argv": ["echo", "disabled"],
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(payload["detail"], "Rust core writes are disabled");
    assert!(!queue_state_dir.join("queue_runner.db").exists());
}

#[tokio::test]
async fn queue_job_create_persists_pending_job_and_files() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-create");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "api queue",
            "argv": ["echo", "hello queue"],
            "cwd": working_dir.display().to_string(),
            "env": {"EXTRA": "1"},
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 5
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let job_id = payload["id"].as_str().unwrap();
    assert!(job_id.starts_with("job_"));
    assert_eq!(payload["type"], "tests");
    assert_eq!(payload["label"], "api queue");
    assert_eq!(payload["requester_session_id"], "run12345");
    assert_eq!(payload["notify_session_id"], "run12345");
    assert_eq!(payload["notify_name"], "Runner Native");
    assert_eq!(payload["argv"], json!(["echo", "hello queue"]));
    assert_eq!(payload["script_path"], Value::Null);
    assert_eq!(payload["timeout_seconds"], 5);
    assert_eq!(payload["state"], "pending");
    assert_eq!(payload["holding_reason"], Value::Null);
    assert!(payload["queued_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(payload["started_at"], Value::Null);
    assert_eq!(payload["finished_at"], Value::Null);
    assert!(payload["log_path"]
        .as_str()
        .unwrap()
        .contains(&format!("logs/{job_id}.log")));

    let db_path = queue_state_dir.join("queue_runner.db");
    let conn = Connection::open(&db_path).unwrap();
    let row = conn
        .query_row(
            "SELECT id, type, label, notify_session_id, argv_json, env_json, state, exit_code_path, wrapper_path FROM queue_jobs WHERE id = ?1",
            [&job_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0, job_id);
    assert_eq!(row.1, "tests");
    assert_eq!(row.2, "api queue");
    assert_eq!(row.3, "run12345");
    assert_eq!(row.4, r#"["echo","hello queue"]"#);
    assert_eq!(row.5, r#"{"EXTRA":"1"}"#);
    assert_eq!(row.6, "pending");
    assert!(PathBuf::from(&row.7).ends_with(format!("{job_id}/exit.code")));
    let wrapper_path = PathBuf::from(row.8);
    let wrapper = fs::read_to_string(&wrapper_path).unwrap();
    assert!(wrapper.contains("cd "));
    assert!(wrapper.contains("export 'EXTRA'='1'"));
    assert!(wrapper.contains("'echo' 'hello queue'"));
    assert!(wrapper.contains("exit.code"));

    let (status, detail) = get_json(app.clone(), &format!("/queue-jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["id"], job_id);
    assert_eq!(detail["state"], "pending");

    let (status, list) = get_json(app, "/queue-jobs?notify_target=run12345").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["jobs"][0]["id"], job_id);
}

#[tokio::test]
async fn queue_job_create_validates_request_shape() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-create-validation");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "argv": ["echo", "missing notify"],
            "cwd": working_dir.display().to_string()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload["detail"],
        "notify_target or requester_session_id is required"
    );

    let (status, payload) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "unknown",
            "argv": ["echo", "bad type"],
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["detail"], "unknown queue job type: unknown");

    let (status, payload) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "argv": ["echo", "both"],
            "script": "echo both",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload["detail"],
        "exactly one of argv or script is required"
    );

    let (status, payload) = post_json(
        app,
        "/queue-jobs",
        json!({
            "type": "tests",
            "script": "echo missing cwd",
            "cwd": working_dir.join("missing").display().to_string(),
            "notify_target": "run12345"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(payload["detail"]
        .as_str()
        .unwrap()
        .starts_with("cwd does not exist or is not a directory:"));
}

#[tokio::test]
async fn queue_job_create_runs_when_runtime_enabled() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-runtime");
    let message_queue_db = state_file.with_extension("queue-runtime-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: message_queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.runtime_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "runtime queue",
            "argv": ["/bin/zsh", "-lc", "printf queue-runtime-ok"],
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 5
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let job_id = payload["id"].as_str().unwrap().to_owned();
    assert!(matches!(
        payload["state"].as_str(),
        Some("running" | "succeeded")
    ));
    let final_payload = wait_for_queue_job_state(app, &job_id, &["succeeded"]).await;
    assert_eq!(final_payload["exit_code"], 0);
    assert!(final_payload["started_at"].as_str().is_some());
    assert!(final_payload["finished_at"].as_str().is_some());
    assert!(final_payload["pid"].as_i64().unwrap_or_default() > 0);
    assert_eq!(
        fs::read_to_string(queue_state_dir.join(format!("logs/{job_id}.log"))).unwrap(),
        "queue-runtime-ok"
    );
    assert!(queue_job_completion_notified_at(&queue_state_dir, &job_id).is_some());
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains(&format!("[sm queue] {job_id} completed: succeeded")));
    assert!(notifications[0].contains(" exit=0 "));
    assert!(notifications[0].contains("log tail:\nqueue-runtime-ok"));
}

#[tokio::test]
async fn queue_job_runtime_respects_configured_max_running_jobs() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-configured-cap");
    let message_queue_db = state_file.with_extension("queue-configured-cap-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            max_running_jobs: 1,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: message_queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.runtime_enabled = true;
    let app = router(AppState::new(config));

    let (status, first) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "configured cap first",
            "script": "printf first-start; sleep 1; printf first-end",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 5
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first_id = first["id"].as_str().unwrap().to_owned();
    let first_running = wait_for_queue_job_state(app.clone(), &first_id, &["running"]).await;
    assert_eq!(first_running["state"], "running");

    let (status, second) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "configured cap second",
            "script": "printf second-started",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 5
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second_id = second["id"].as_str().unwrap().to_owned();
    assert_eq!(second["state"], "pending");
    assert_eq!(second["holding_reason"], "concurrency_cap");

    let first_final = wait_for_queue_job_state(app.clone(), &first_id, &["succeeded"]).await;
    assert_eq!(first_final["exit_code"], 0);
    let second_final = wait_for_queue_job_state(app, &second_id, &["succeeded"]).await;
    assert_eq!(second_final["exit_code"], 0);
    assert_eq!(second_final["holding_reason"], Value::Null);
}

#[tokio::test]
async fn queue_job_runtime_persists_failure_and_timeout() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-terminal");
    let message_queue_db = state_file.with_extension("queue-terminal-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: message_queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.runtime_enabled = true;
    let app = router(AppState::new(config));

    let (status, failed) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "runtime failure",
            "argv": ["/bin/zsh", "-lc", "printf failed-output; exit 7"],
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 5
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let failed_id = failed["id"].as_str().unwrap().to_owned();
    let failed_final = wait_for_queue_job_state(app.clone(), &failed_id, &["failed"]).await;
    assert_eq!(failed_final["exit_code"], 7);
    assert!(failed_final["finished_at"].as_str().is_some());
    assert_eq!(
        fs::read_to_string(queue_state_dir.join(format!("logs/{failed_id}.log"))).unwrap(),
        "failed-output"
    );
    assert!(queue_job_completion_notified_at(&queue_state_dir, &failed_id).is_some());

    let (status, timed_out) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "runtime timeout",
            "script": "printf 'before-timeout\\n'; sleep 5; printf 'after-timeout\\n'",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let timeout_id = timed_out["id"].as_str().unwrap().to_owned();
    let timeout_final = wait_for_queue_job_state(app.clone(), &timeout_id, &["timed_out"]).await;
    assert!(timeout_final["finished_at"].as_str().is_some());
    let log = fs::read_to_string(queue_state_dir.join(format!("logs/{timeout_id}.log"))).unwrap();
    assert!(log.contains("before-timeout"));
    assert!(!log.contains("after-timeout"));
    assert!(queue_job_completion_notified_at(&queue_state_dir, &timeout_id).is_some());
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 2);
    assert!(notifications[0].contains(&format!("[sm queue] {failed_id} completed: failed")));
    assert!(notifications[0].contains(" exit=7 "));
    assert!(notifications[1].contains(&format!("[sm queue] {timeout_id} completed: timed_out")));
}

#[tokio::test]
async fn queue_runtime_recovery_requeues_held_pending_job() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-held-pending");
    let message_queue_db = state_file.with_extension("queue-recover-held-pending-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover held pending",
        "printf recovered-held",
        5,
    )
    .await;
    set_queue_job_holding_reason(&queue_state_dir, &job_id, "memory_pressure");

    let summary =
        RetainedQueueStore::recover_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
            .unwrap();

    assert_eq!(summary.requeued_pending, 1);
    assert_eq!(summary.started_pending, 1);
    assert_eq!(summary.held_pending, 0);
    let final_payload = wait_for_queue_job_state(app, &job_id, &["succeeded"]).await;
    assert_eq!(final_payload["holding_reason"], Value::Null);
    assert!(final_payload["started_at"].as_str().is_some());
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains(&format!("[sm queue] {job_id} completed: succeeded")));
}

#[tokio::test]
async fn queue_runtime_recovery_admits_pending_jobs_through_concurrency_cap() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-pending-cap");
    let message_queue_db = state_file.with_extension("queue-recover-pending-cap-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let first_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover cap first",
        "sleep 0.5; printf first",
        5,
    )
    .await;
    let second_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover cap second",
        "sleep 0.5; printf second",
        5,
    )
    .await;
    let third_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover cap third",
        "printf third",
        5,
    )
    .await;
    set_queue_job_holding_reason(&queue_state_dir, &third_id, "memory_pressure");

    let summary =
        RetainedQueueStore::recover_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
            .unwrap();

    assert_eq!(summary.started_pending, 2);
    assert_eq!(summary.requeued_pending, 1);
    assert!(summary.held_pending >= 1);
    let (status, third_pending) = get_json(app.clone(), &format!("/queue-jobs/{third_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(third_pending["state"], "pending");
    assert_eq!(third_pending["holding_reason"], "concurrency_cap");

    let first_final = wait_for_queue_job_state(app.clone(), &first_id, &["succeeded"]).await;
    let second_final = wait_for_queue_job_state(app.clone(), &second_id, &["succeeded"]).await;
    let third_final = wait_for_queue_job_state(app, &third_id, &["succeeded"]).await;
    assert_eq!(first_final["exit_code"], 0);
    assert_eq!(second_final["exit_code"], 0);
    assert_eq!(third_final["exit_code"], 0);
}

#[tokio::test]
async fn queue_runtime_admission_displaces_background_for_ready_perf_job() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-displace-background");
    let message_queue_db = state_file.with_extension("queue-displace-background-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 2,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: message_queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.runtime_enabled = true;
    let app = router(AppState::new(config));

    let (status, first_background) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "background",
            "label": "first background",
            "script": "trap 'exit 0' TERM; printf first-start; while true; do sleep 1; done",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 10
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first_background_id = first_background["id"].as_str().unwrap().to_owned();
    let (status, second_background) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "background",
            "label": "second background",
            "script": "printf second-start; sleep 2; printf second-end",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 10
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second_background_id = second_background["id"].as_str().unwrap().to_owned();
    let first_running =
        wait_for_queue_job_state(app.clone(), &first_background_id, &["running"]).await;
    let second_running =
        wait_for_queue_job_state(app.clone(), &second_background_id, &["running"]).await;
    assert!(first_running["pid"].as_i64().unwrap_or_default() > 0);
    assert!(second_running["pid"].as_i64().unwrap_or_default() > 0);

    let (status, perf) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "perf",
            "label": "ready perf",
            "script": "printf perf-started",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 10
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let perf_id = perf["id"].as_str().unwrap().to_owned();

    let first_final =
        wait_for_queue_job_state(app.clone(), &first_background_id, &["displaced"]).await;
    assert_eq!(first_final["state"], "displaced");
    let perf_final = wait_for_queue_job_state(app.clone(), &perf_id, &["succeeded"]).await;
    assert_eq!(perf_final["exit_code"], 0);
    assert_eq!(perf_final["holding_reason"], Value::Null);
    let second_final = wait_for_queue_job_state(app, &second_background_id, &["succeeded"]).await;
    assert_eq!(second_final["exit_code"], 0);
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert!(notifications.iter().any(|text| text.contains(&format!(
        "[sm queue] {first_background_id} completed: displaced"
    ))));
    assert!(notifications
        .iter()
        .any(|text| text.contains(&format!("[sm queue] {perf_id} completed: succeeded"))));
}

#[tokio::test]
async fn queue_runtime_recovery_retries_perf_after_cooldown_expires() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-perf-cooldown");
    let message_queue_db =
        state_file.with_extension("queue-recover-perf-cooldown-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let seed_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "cooldown seed",
        "printf cooldown-seed",
        5,
    )
    .await;
    let now = time::OffsetDateTime::now_utc();
    let started_at = (now - time::Duration::seconds(29))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let finished_at = (now - time::Duration::seconds(28))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    mark_queue_job_terminal(
        &queue_state_dir,
        &seed_id,
        "succeeded",
        &started_at,
        &finished_at,
        0,
    );
    let perf_id = create_pending_queue_job_of_type(
        app.clone(),
        &working_dir,
        "perf",
        "perf after cooldown",
        "printf perf-after-cooldown",
        5,
    )
    .await;

    RetainedQueueStore::admit_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
        .unwrap();

    let (status, held) = get_json(app.clone(), &format!("/queue-jobs/{perf_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(held["state"], "pending");
    assert_eq!(held["holding_reason"], "perf_cooldown");

    let final_payload = wait_for_queue_job_state(app, &perf_id, &["succeeded"]).await;
    assert_eq!(final_payload["holding_reason"], Value::Null);
    assert_eq!(final_payload["exit_code"], 0);
    assert_eq!(
        fs::read_to_string(queue_state_dir.join(format!("logs/{perf_id}.log"))).unwrap(),
        "perf-after-cooldown"
    );
    assert!(queue_job_completion_notified_at(&queue_state_dir, &perf_id).is_some());
}

#[tokio::test]
async fn queue_runtime_admission_respects_configured_perf_cooldown() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-configured-perf-cooldown");
    let message_queue_db =
        state_file.with_extension("queue-configured-perf-cooldown-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let seed_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "configured cooldown seed",
        "printf configured-cooldown-seed",
        5,
    )
    .await;
    let now = time::OffsetDateTime::now_utc();
    let started_at = (now - time::Duration::seconds(4))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let finished_at = (now - time::Duration::seconds(3))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    mark_queue_job_terminal(
        &queue_state_dir,
        &seed_id,
        "succeeded",
        &started_at,
        &finished_at,
        0,
    );
    let perf_id = create_pending_queue_job_of_type(
        app.clone(),
        &working_dir,
        "perf",
        "configured cooldown perf",
        "printf configured-perf",
        5,
    )
    .await;

    RetainedQueueStore::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(
        &queue_state_dir,
        &message_queue_db,
        0,
        QueueAdmissionPolicy {
            max_running_jobs: 2,
            perf_cooldown_seconds: 5,
        },
    )
    .unwrap();

    let (status, held) = get_json(app.clone(), &format!("/queue-jobs/{perf_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(held["state"], "pending");
    assert_eq!(held["holding_reason"], "perf_cooldown");

    RetainedQueueStore::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(
        &queue_state_dir,
        &message_queue_db,
        0,
        QueueAdmissionPolicy {
            max_running_jobs: 2,
            perf_cooldown_seconds: 1,
        },
    )
    .unwrap();

    let final_payload = wait_for_queue_job_state(app, &perf_id, &["succeeded"]).await;
    assert_eq!(final_payload["exit_code"], 0);
    assert_eq!(final_payload["holding_reason"], Value::Null);
}

#[tokio::test]
async fn queue_runtime_admission_serializes_concurrent_start_claims() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-concurrent-admission");
    let message_queue_db = state_file.with_extension("queue-concurrent-admission-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let marker_path = working_dir.join("started.txt");
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "concurrent admission",
        &format!("printf started >> {}; sleep 0.1", marker_path.display()),
        5,
    )
    .await;

    let mut handles = Vec::new();
    for _ in 0..8 {
        let queue_state_dir = queue_state_dir.clone();
        let message_queue_db = message_queue_db.clone();
        handles.push(thread::spawn(move || {
            RetainedQueueStore::admit_queue_jobs_in_state_dir(
                &queue_state_dir,
                &message_queue_db,
                0,
            )
            .unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let final_payload = wait_for_queue_job_state(app, &job_id, &["succeeded"]).await;
    assert_eq!(final_payload["exit_code"], 0);
    assert_eq!(
        fs::read_to_string(marker_path)
            .unwrap()
            .matches("started")
            .count(),
        1
    );
    assert!(queue_job_completion_notified_at(&queue_state_dir, &job_id).is_some());
}

#[tokio::test]
async fn queue_runtime_recovery_starts_pending_job() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-pending");
    let message_queue_db = state_file.with_extension("queue-recover-pending-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover pending",
        "printf recovered-pending",
        5,
    )
    .await;

    let summary =
        RetainedQueueStore::recover_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
            .unwrap();

    assert_eq!(summary.started_pending, 1);
    let final_payload = wait_for_queue_job_state(app, &job_id, &["succeeded"]).await;
    assert_eq!(final_payload["exit_code"], 0);
    assert!(queue_job_completion_notified_at(&queue_state_dir, &job_id).is_some());
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains(&format!("[sm queue] {job_id} completed: succeeded")));
    assert!(notifications[0].contains("log tail:\nrecovered-pending"));
}

#[tokio::test]
async fn queue_runtime_recovery_continues_after_bad_pending_job() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-bad-pending");
    let message_queue_db = state_file.with_extension("queue-recover-bad-pending-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let bad_job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover bad pending",
        "printf should-not-start",
        5,
    )
    .await;
    let good_job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover good pending",
        "printf recovered-after-bad-pending",
        5,
    )
    .await;
    set_queue_job_text_column_null(&queue_state_dir, &bad_job_id, "wrapper_path");

    let summary =
        RetainedQueueStore::recover_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
            .unwrap();

    assert_eq!(summary.finished_failed, 1);
    assert_eq!(summary.started_pending, 1);
    let (bad_status, bad_detail) =
        get_json(app.clone(), &format!("/queue-jobs/{bad_job_id}")).await;
    assert_eq!(bad_status, StatusCode::OK);
    assert_eq!(bad_detail["state"], "failed");
    let good_final = wait_for_queue_job_state(app, &good_job_id, &["succeeded"]).await;
    assert_eq!(good_final["exit_code"], 0);
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 2);
    assert!(notifications[0].contains(&format!("[sm queue] {bad_job_id} completed: failed")));
    assert!(notifications[1].contains(&format!("[sm queue] {good_job_id} completed: succeeded")));
}

#[tokio::test]
async fn queue_job_create_continues_after_bad_existing_pending_job() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-create-after-bad-pending");
    let message_queue_db = state_file.with_extension("queue-create-after-bad-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let seed_app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let bad_job_id = create_pending_queue_job(
        seed_app,
        &working_dir,
        "bad existing pending",
        "printf should-not-start",
        5,
    )
    .await;
    set_queue_job_text_column_null(&queue_state_dir, &bad_job_id, "wrapper_path");

    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        true,
        false,
    );
    let (status, created) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "created after bad pending",
            "script": "printf created-after-bad",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 5
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let created_id = created["id"].as_str().unwrap().to_owned();

    let bad_final = wait_for_queue_job_state(app.clone(), &bad_job_id, &["failed"]).await;
    assert_eq!(bad_final["state"], "failed");
    let created_final = wait_for_queue_job_state(app, &created_id, &["succeeded"]).await;
    assert_eq!(created_final["exit_code"], 0);
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 2);
    assert!(notifications[0].contains(&format!("[sm queue] {bad_job_id} completed: failed")));
    assert!(notifications[1].contains(&format!("[sm queue] {created_id} completed: succeeded")));
}

#[tokio::test]
async fn queue_runtime_recovery_finishes_running_job_with_exit_code() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-exit");
    let message_queue_db = state_file.with_extension("queue-recover-exit-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover exit",
        "printf recovered-exit",
        5,
    )
    .await;
    let exit_code_path = queue_job_text_column(&queue_state_dir, &job_id, "exit_code_path");
    let log_path = queue_job_text_column(&queue_state_dir, &job_id, "log_path");
    fs::write(&exit_code_path, "0\n").unwrap();
    fs::write(&log_path, "recovered-exit").unwrap();
    mark_queue_job_running(
        &queue_state_dir,
        &job_id,
        &test_now_rfc3339(),
        9_999_999,
        9_999_999,
    );

    let summary =
        RetainedQueueStore::recover_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
            .unwrap();

    assert_eq!(summary.recovered_running, 1);
    assert_eq!(summary.finished_succeeded, 1);
    let (status, detail) = get_json(app, &format!("/queue-jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["state"], "succeeded");
    assert_eq!(detail["exit_code"], 0);
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains(&format!("[sm queue] {job_id} completed: succeeded")));
}

#[tokio::test]
async fn queue_runtime_recovery_marks_dead_running_job_failed() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-dead");
    let message_queue_db = state_file.with_extension("queue-recover-dead-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover dead",
        "printf never-ran",
        5,
    )
    .await;
    mark_queue_job_running(
        &queue_state_dir,
        &job_id,
        &test_now_rfc3339(),
        9_999_999,
        9_999_999,
    );

    let summary =
        RetainedQueueStore::recover_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
            .unwrap();

    assert_eq!(summary.recovered_running, 1);
    assert_eq!(summary.finished_failed, 1);
    let (status, detail) = get_json(app, &format!("/queue-jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["state"], "failed");
    assert!(queue_job_completion_notified_at(&queue_state_dir, &job_id).is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn queue_runtime_recovery_honors_persisted_running_cancellation() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-cancelling");
    let message_queue_db = state_file.with_extension("queue-recover-cancelling-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover cancelling",
        "printf never-completes",
        30,
    )
    .await;
    let ready_path = queue_state_dir.join("recover-cancelling.ready");
    let mut child = Command::new("/bin/zsh")
        .arg("-lc")
        .arg("printf ready > \"$READY_PATH\"; while true; do sleep 1; done")
        .env("READY_PATH", &ready_path)
        .process_group(0)
        .spawn()
        .unwrap();
    let _ready = wait_for_file_contains(&ready_path, "ready").await;
    let pid = i64::from(child.id());
    mark_queue_job_running_with_holding(
        &queue_state_dir,
        &job_id,
        &test_now_rfc3339(),
        pid,
        pid,
        Some("cancelling"),
    );

    let summary =
        RetainedQueueStore::recover_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
            .unwrap();

    assert_eq!(summary.recovered_running, 1);
    assert_eq!(summary.finished_cancelled, 1);
    let mut exited = false;
    for _ in 0..50 {
        if child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !exited {
        let _ = Command::new("/bin/kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .status();
        let _ = child.wait();
    }
    assert!(exited, "recovered cancelling process was not stopped");
    let (status, detail) = get_json(app, &format!("/queue-jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["state"], "cancelled");
    assert!(queue_job_completion_notified_at(&queue_state_dir, &job_id).is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn queue_runtime_recovery_polls_live_running_job_to_completion() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-live");
    let message_queue_db = state_file.with_extension("queue-recover-live-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover live",
        "printf placeholder",
        5,
    )
    .await;
    let exit_code_path = queue_job_text_column(&queue_state_dir, &job_id, "exit_code_path");
    let log_path = queue_job_text_column(&queue_state_dir, &job_id, "log_path");
    let ready_path = queue_state_dir.join("recover-live.ready");
    let mut child = Command::new("/bin/zsh")
        .arg("-lc")
        .arg("printf ready > \"$READY_PATH\"; sleep 0.3; printf recovered-live > \"$LOG_PATH\"; printf '0\\n' > \"$EXIT_CODE_PATH\"")
        .env("READY_PATH", &ready_path)
        .env("LOG_PATH", &log_path)
        .env("EXIT_CODE_PATH", &exit_code_path)
        .process_group(0)
        .spawn()
        .unwrap();
    let _ready = wait_for_file_contains(&ready_path, "ready").await;
    let pid = i64::from(child.id());
    mark_queue_job_running(&queue_state_dir, &job_id, &test_now_rfc3339(), pid, pid);

    let summary =
        RetainedQueueStore::recover_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
            .unwrap();
    assert_eq!(summary.recovered_running, 1);
    assert_eq!(summary.polling_running, 1);
    let final_payload = wait_for_queue_job_state(app, &job_id, &["succeeded"]).await;
    assert_eq!(final_payload["exit_code"], 0);
    let _ = child.wait();
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains("log tail:\nrecovered-live"));
}

#[cfg(unix)]
#[tokio::test]
async fn queue_runtime_recovery_times_out_and_stops_live_running_job() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-recover-timeout");
    let message_queue_db = state_file.with_extension("queue-recover-timeout-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        false,
        true,
    );
    let job_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "recover timeout",
        "printf never-completes",
        1,
    )
    .await;
    let ready_path = queue_state_dir.join("recover-timeout.ready");
    let mut child = Command::new("/bin/zsh")
        .arg("-lc")
        .arg("trap '' TERM; printf ready > \"$READY_PATH\"; while true; do sleep 1; done")
        .env("READY_PATH", &ready_path)
        .process_group(0)
        .spawn()
        .unwrap();
    let _ready = wait_for_file_contains(&ready_path, "ready").await;
    let pid = i64::from(child.id());
    mark_queue_job_running(&queue_state_dir, &job_id, "2026-06-14T00:00:00Z", pid, pid);

    let summary =
        RetainedQueueStore::recover_queue_jobs_in_state_dir(&queue_state_dir, &message_queue_db, 0)
            .unwrap();

    assert_eq!(summary.recovered_running, 1);
    assert_eq!(summary.finished_timed_out, 1);
    let mut exited = false;
    for _ in 0..50 {
        if child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !exited {
        let _ = Command::new("/bin/kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .status();
        let _ = child.wait();
    }
    assert!(exited, "recovered timed-out process was not stopped");
    let (status, detail) = get_json(app, &format!("/queue-jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["state"], "timed_out");
    assert!(queue_job_completion_notified_at(&queue_state_dir, &job_id).is_some());
}

#[tokio::test]
async fn queue_job_cancel_persists_pending_cancel() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-cancel");
    let message_queue_db = state_file.with_extension("queue-cancel-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: message_queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "cancel queue",
            "argv": ["echo", "cancel"],
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 5
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["state"], "pending");
    let job_id = payload["id"].as_str().unwrap();

    let (status, cancelled) =
        delete_json(app.clone(), &format!("/queue-jobs/{job_id}"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["id"], job_id);
    assert_eq!(cancelled["state"], "cancelled");
    assert!(cancelled["finished_at"].as_str().is_some());

    let (status, detail) = get_json(app, &format!("/queue-jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["state"], "cancelled");
    assert!(queue_job_completion_notified_at(&queue_state_dir, job_id).is_some());
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains(&format!("[sm queue] {job_id} completed: cancelled")));
}

#[tokio::test]
async fn queue_job_cancel_does_not_admit_pending_jobs_without_runtime_ownership() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-cancel-no-runtime-admission");
    let message_queue_db = state_file.with_extension("queue-cancel-no-runtime-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: message_queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let first_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "cancel fixture first",
        "printf first-should-not-run",
        5,
    )
    .await;
    let second_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "cancel fixture second",
        "printf second-should-not-run",
        5,
    )
    .await;

    let (status, cancelled) =
        delete_json(app.clone(), &format!("/queue-jobs/{first_id}"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["state"], "cancelled");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, second_detail) = get_json(app, &format!("/queue-jobs/{second_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second_detail["state"], "pending");
    assert_eq!(second_detail["pid"], Value::Null);
    assert!(!queue_state_dir
        .join(format!("logs/{second_id}.log"))
        .exists());
}

#[tokio::test]
async fn queue_job_cancel_terminates_running_job() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-cancel-running");
    let message_queue_db = state_file.with_extension("queue-cancel-running-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: message_queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.runtime_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "cancel running queue",
            "script": "printf 'before-cancel\\n'; sleep 5; printf 'after-cancel\\n'",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 20
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let job_id = payload["id"].as_str().unwrap().to_owned();
    let running = wait_for_queue_job_state(app.clone(), &job_id, &["running"]).await;
    assert!(running["pid"].as_i64().unwrap_or_default() > 0);
    let log_path = queue_state_dir.join(format!("logs/{job_id}.log"));
    let log = wait_for_file_contains(&log_path, "before-cancel").await;
    assert!(!log.contains("after-cancel"));

    let (status, cancelled) =
        delete_json(app.clone(), &format!("/queue-jobs/{job_id}"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["state"], "cancelled");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, detail) = get_json(app, &format!("/queue-jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["state"], "cancelled");
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("before-cancel"));
    assert!(!log.contains("after-cancel"));
    assert!(queue_job_completion_notified_at(&queue_state_dir, &job_id).is_some());
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains(&format!("[sm queue] {job_id} completed: cancelled")));
    assert!(notifications[0].contains("log tail:\nbefore-cancel"));
}

#[tokio::test]
async fn queue_job_cancel_admits_next_pending_job() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-cancel-admits-next");
    let message_queue_db = state_file.with_extension("queue-cancel-admits-next-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let app = queue_runtime_test_app(
        &state_file,
        &queue_state_dir,
        &message_queue_db,
        true,
        false,
    );

    let (status, first) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "cancel admits first",
            "script": "printf first-start; sleep 2; printf first-end",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 10
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first_id = first["id"].as_str().unwrap().to_owned();
    let (status, second) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "cancel admits second",
            "script": "printf second-start; sleep 2; printf second-end",
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 10
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second_id = second["id"].as_str().unwrap().to_owned();
    let first_running = wait_for_queue_job_state(app.clone(), &first_id, &["running"]).await;
    let second_running = wait_for_queue_job_state(app.clone(), &second_id, &["running"]).await;
    assert!(first_running["pid"].as_i64().unwrap_or_default() > 0);
    assert!(second_running["pid"].as_i64().unwrap_or_default() > 0);

    let third_id = create_pending_queue_job(
        app.clone(),
        &working_dir,
        "cancel admits third",
        "printf third-started",
        10,
    )
    .await;
    let (status, third_pending) = get_json(app.clone(), &format!("/queue-jobs/{third_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(third_pending["state"], "pending");
    assert_eq!(third_pending["holding_reason"], "concurrency_cap");

    let (status, cancelled) =
        delete_json(app.clone(), &format!("/queue-jobs/{first_id}"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["state"], "cancelled");
    let third_final = wait_for_queue_job_state(app.clone(), &third_id, &["succeeded"]).await;
    assert_eq!(third_final["exit_code"], 0);
    assert_eq!(third_final["holding_reason"], Value::Null);
    assert_eq!(
        fs::read_to_string(queue_state_dir.join(format!("logs/{third_id}.log"))).unwrap(),
        "third-started"
    );
    let second_final = wait_for_queue_job_state(app, &second_id, &["succeeded"]).await;
    assert_eq!(second_final["exit_code"], 0);
    assert!(queue_job_completion_notified_at(&queue_state_dir, &first_id).is_some());
    assert!(queue_job_completion_notified_at(&queue_state_dir, &third_id).is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn queue_job_cancel_force_stops_unmonitored_running_process_group() {
    let state_file = write_session_fixture();
    let queue_state_dir = state_file.with_extension("queue-runner-cancel-unmonitored");
    let message_queue_db = state_file.with_extension("queue-cancel-unmonitored-message-queue.db");
    let working_dir = unique_temp_path().with_extension("queue-cwd");
    fs::create_dir_all(&working_dir).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: message_queue_db.display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/queue-jobs",
        json!({
            "type": "tests",
            "label": "cancel unmonitored queue",
            "argv": ["echo", "cancel"],
            "cwd": working_dir.display().to_string(),
            "notify_target": "run12345",
            "requester_session_id": "run12345",
            "timeout_seconds": 30
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let job_id = payload["id"].as_str().unwrap().to_owned();
    let ready_path = queue_state_dir.join("unmonitored.ready");
    let mut child = Command::new("/bin/zsh")
        .arg("-lc")
        .arg("trap '' TERM; printf ready > \"$READY_PATH\"; while true; do sleep 1; done")
        .env("READY_PATH", &ready_path)
        .process_group(0)
        .spawn()
        .unwrap();
    let _ready = wait_for_file_contains(&ready_path, "ready").await;
    let pid = i64::from(child.id());
    let conn = Connection::open(queue_state_dir.join("queue_runner.db")).unwrap();
    conn.execute(
        r#"
        UPDATE queue_jobs
        SET state = 'running',
            started_at = '2026-06-14T00:00:00Z',
            pid = ?2,
            process_group_id = ?2
        WHERE id = ?1
        "#,
        (&job_id, pid),
    )
    .unwrap();

    let (status, cancelled) =
        delete_json(app.clone(), &format!("/queue-jobs/{job_id}"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["state"], "cancelled");

    let mut exited = false;
    for _ in 0..50 {
        if child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !exited {
        let _ = Command::new("/bin/kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .status();
        let _ = child.wait();
    }
    assert!(exited, "unmonitored queue process was not force-stopped");
    assert!(queue_job_completion_notified_at(&queue_state_dir, &job_id).is_some());
    let notifications = queued_message_texts(&message_queue_db, "run12345");
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains(&format!("[sm queue] {job_id} completed: cancelled")));
}

#[tokio::test]
async fn auth_session_reports_disabled_bypass_when_google_auth_not_requested() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/auth/session").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": false,
            "authenticated": true,
            "bypass": true,
            "email": null,
            "name": null
        })
    );
}

#[tokio::test]
async fn device_google_auth_route_preserves_validation_and_config_errors() {
    let app = router(AppState::new(AppConfig::default()));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/device/google")
                .header("content-type", "application/json")
                .body(Body::from(b"{}".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        String::from_utf8_lossy(&body).contains("id_token"),
        "body={}",
        String::from_utf8_lossy(&body)
    );

    let (status, payload) = post_json(
        app,
        "/auth/device/google",
        json!({"id_token": "google-id-token"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(payload["detail"], "Google auth is not configured");
}

#[tokio::test]
async fn tmux_client_hook_preserves_local_only_and_event_validation() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json_with_headers_and_peer(
        app.clone(),
        "/hooks/tmux-client?event=unsupported",
        json!({}),
        &[("host", "127.0.0.1:8421")],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["detail"], "unsupported tmux client event");

    let (status, payload) = post_json_with_headers_and_peer(
        app.clone(),
        "/hooks/tmux-client?event=client-session-changed&session=tmux-test",
        json!({}),
        &[("host", "127.0.0.1:8421")],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["tmux_client_event_version"], 0);

    let (status, payload) = post_json_with_headers_and_peer(
        app,
        "/hooks/tmux-client?event=client-session-changed",
        json!({}),
        &[("host", "127.0.0.1:8421")],
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "tmux client hooks must originate locally"
    );
}

#[tokio::test]
async fn shadow_http_reports_match_for_stable_read_only_route() {
    let app = router(AppState::new(AppConfig::default()));
    let python_body = serde_json::to_vec(&json!({ "status": "healthy" })).unwrap();

    let (status, payload) = post_json(
        app.clone(),
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["would_write"], false);
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["body_sha256_match"], true);
}

#[tokio::test]
async fn shadow_http_treats_events_state_as_status_only() {
    let app = router(AppState::new(AppConfig::default()));
    let python_body = serde_json::to_vec(&json!({
        "tmux_client_event_version": 42,
        "last_tmux_client_event": {
            "type": "tmux_client_event",
            "event": "client-attached",
            "tmux_session": "codex-fork-live",
            "version": 42
        }
    }))
    .unwrap();

    let (status, payload) = post_json(
        app.clone(),
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/events/state",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["would_write"], false);
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
}

#[tokio::test]
async fn shadow_http_treats_nodes_as_status_only_until_node_agents_are_ported() {
    let app = router(AppState::new(AppConfig::default()));
    let python_body = br#"{"default":"primary","nodes":[{"id":"primary","primary":true,"ssh":null,"api_url":null,"hook_base_url":null,"projects_root":null,"log_dir":null,"codex_fork_node_agent":true}]}"#;

    let (status, payload) = post_json(
        app.clone(),
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/nodes",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["would_write"], false);
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);

    let python_body = br#"{"node":"primary","ok":true,"error":null}"#;
    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "POST",
                "path": "/nodes/primary/ping",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["would_write"], false);
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
}

#[tokio::test]
async fn shadow_http_treats_live_session_lists_as_status_only() {
    let app = router(AppState::new(config_with_state_file(
        &write_session_fixture(),
    )));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"sessions\":[{\"activity_state\":\"working\"}]}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
}

#[tokio::test]
async fn shadow_http_treats_live_session_detail_as_status_only() {
    let app = router(AppState::new(config_with_state_file(
        &write_session_fixture(),
    )));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/run12345",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"id\":\"run12345\",\"activity_state\":\"working\"}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
}

#[tokio::test]
async fn shadow_http_reports_codex_review_request_detail_200_as_status_only() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-requests.db");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    create_codex_review_request_fixture_db(&queue_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    }));
    let python_body = br#"{"id":"active-old","repo":"rajeshgoli/session-manager","pr_number":830,"requester_session_id":"requester1","requester_name":"requester1","notify_session_id":"notify1","notify_name":"notify1","steer":"focus nodes","requested_at":"2026-06-01T00:00:00","latest_request_comment_id":111,"latest_request_comment_url":"https://example.com/comment/111","latest_request_posted_at":"2026-06-01T00:00:01","attempt_count":2,"next_retry_at":"2026-06-01T00:10:00","poll_interval_seconds":30,"retry_interval_seconds":600,"pickup_detected_at":"2026-06-01T00:02:00","pickup_source":"issue_comment","review_landed_at":"2026-06-01T00:03:00","review_source":"pull_review","review_comment_id":222,"review_url":"https://example.com/review/222","last_polled_at":"2026-06-01T00:04:00","last_error":null,"state":"completed","is_active":true}"#;

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/codex-review-requests/active-old",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
}

#[tokio::test]
async fn shadow_http_reports_codex_review_request_detail_404() {
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: state_file
                .with_extension("missing-codex-review.db")
                .display()
                .to_string(),
        },
        ..AppConfig::default()
    }));
    let python_body =
        serde_json::to_vec(&json!({ "detail": "Codex review request not found" })).unwrap();

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/codex-review-requests/missing",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 404,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["predicted_status"], 404);
    assert_eq!(payload["body_sha256_match"], true);
}

#[tokio::test]
async fn shadow_http_reports_codex_review_request_cancel_as_status_only_without_writing() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-requests-shadow-delete.db");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    create_codex_review_request_fixture_db(&queue_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    }));
    let python_body = br#"{"id":"active-old","state":"cancelled","is_active":false}"#;

    let (status, payload) = post_json(
        app.clone(),
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "DELETE",
                "path": "/codex-review-requests/active-old",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["support_status"],
        "implemented_retained_write_status_only"
    );
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["would_write"], false);
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);

    let (status, detail) = get_json(app, "/codex-review-requests/active-old").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["state"], "completed");
    assert_eq!(detail["is_active"], true);
}

#[tokio::test]
async fn shadow_http_reports_codex_review_request_create_as_status_only_without_writing() {
    let state_file = unique_temp_path();
    let queue_db = state_file.with_extension("codex-review-requests-shadow-create.db");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "POST",
                "path": "/codex-review-requests",
                "query_string": "",
                "headers": {},
                "body_sha256": sha256_hex(b"{\"pr_number\":967}")
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"id\":\"python-owned\"}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["support_status"],
        "implemented_retained_write_status_only"
    );
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["would_write"], false);
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
    assert!(!queue_db.exists());
}

#[tokio::test]
async fn shadow_http_reports_pr_review_create_as_status_only_without_writing() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "POST",
                "path": "/reviews/pr",
                "query_string": "",
                "headers": {},
                "body_sha256": sha256_hex(b"{\"pr_number\":967}")
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"status\":\"posted\"}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["support_status"],
        "implemented_retained_write_status_only"
    );
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["would_write"], false);
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
}

#[tokio::test]
async fn shadow_http_reports_tool_calls_200_as_status_only() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let python_body = br#"{"session_id":"run12345","tool_calls":[{"timestamp":"2026-06-01 00:02:00","tool_name":"Bash","hook_type":"PreToolUse"}]}"#;

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/run12345/tool-calls",
                "query_string": "limit=10",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
}

#[tokio::test]
async fn shadow_http_reports_tool_calls_404() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let python_body = serde_json::to_vec(&json!({ "detail": "Session not found" })).unwrap();

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/missing/tool-calls",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 404,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["predicted_status"], 404);
    assert_eq!(payload["body_sha256_match"], true);
}

#[tokio::test]
async fn shadow_http_reports_activity_actions_200_as_status_only() {
    let state_file = write_codex_app_session_fixture("codexproj");
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let python_body = br#"{"actions":[]}"#;

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/codexproj/activity-actions",
                "query_string": "limit=%32&limit=3",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["body_sha256"], Value::Null);
}

#[tokio::test]
async fn shadow_http_reports_activity_actions_stable_failures() {
    let state_file = write_codex_app_session_fixture("codexproj");
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = post_json(
        app.clone(),
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/missing/activity-actions",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 404,
                "body_sha256": sha256_hex(br#"{"detail":"Session not found"}"#)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["predicted_status"], 404);

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/codexproj/activity-actions",
                "query_string": "limit=0",
                "headers": {}
            },
            "python_response": {
                "status": 422,
                "body_sha256": sha256_hex(b"{}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 422);
}

#[tokio::test]
async fn shadow_http_reports_codex_events_200_as_status_only() {
    let state_file = write_codex_app_session_fixture("codexapp1");
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let python_body = br#"{"events":[{"session_id":"codexapp1","seq":1,"timestamp":"2026-06-01T00:01:00+00:00","event_type":"turn_started","turn_id":"turn-a","payload_preview":{"message":"start"},"persisted":true}],"earliest_seq":1,"latest_seq":1,"next_seq":2,"history_gap":false,"gap_reason":null}"#;

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/codexapp1/codex-events",
                "query_string": "limit=200",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
}

#[tokio::test]
async fn shadow_http_reports_codex_events_404() {
    let state_file = write_codex_app_session_fixture("codexapp1");
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let python_body = serde_json::to_vec(&json!({ "detail": "Session not found" })).unwrap();

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/missing/codex-events",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 404,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["predicted_status"], 404);
    assert_eq!(payload["body_sha256_match"], true);
}

#[tokio::test]
async fn shadow_http_reports_codex_events_invalid_query_as_status_only_422() {
    let state_file = write_codex_app_session_fixture("codexapp1");
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let python_body = br#"{"detail":[{"type":"greater_than_equal","loc":["query","limit"],"msg":"Input should be greater than or equal to 1","input":"0","ctx":{"ge":1}}]}"#;

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/codexapp1/codex-events",
                "query_string": "limit=0",
                "headers": {}
            },
            "python_response": {
                "status": 422,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 422);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
}

#[tokio::test]
async fn shadow_http_uses_decoded_last_codex_events_query_value() {
    let state_file = write_codex_app_session_fixture("codexapp1");
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let python_body = br#"{"events":[],"earliest_seq":null,"latest_seq":null,"next_seq":1,"history_gap":false,"gap_reason":null}"#;

    let (status, payload) = post_json(
        app.clone(),
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/codexapp1/codex-events",
                "query_string": "limit=0&limit=%32",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/codexapp1/codex-events",
                "query_string": "limit=2&limit=0",
                "headers": {}
            },
            "python_response": {
                "status": 422,
                "body_sha256": sha256_hex(b"{}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 422);

    let app = router(AppState::new(config_with_state_file(&state_file)));
    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/codexpending/codex-pending-requests",
                "query_string": "include_orphaned=%20true%20",
                "headers": {}
            },
            "python_response": {
                "status": 422,
                "body_sha256": sha256_hex(b"{}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 422);
}

#[tokio::test]
async fn shadow_http_reports_codex_pending_requests_200_as_status_only() {
    let state_file = write_codex_app_session_fixture("codexpending");
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let python_body = br#"{"requests":[]}"#;

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/codexpending/codex-pending-requests",
                "query_string": "include_orphaned=%74&include_orphaned=true",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["body_sha256"], Value::Null);
}

#[tokio::test]
async fn shadow_http_reports_codex_pending_requests_stable_failures() {
    let state_file = write_codex_app_session_fixture("codexpending");
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = post_json(
        app.clone(),
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/missing/codex-pending-requests",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 404,
                "body_sha256": sha256_hex(br#"{"detail":"Session not found"}"#)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["predicted_status"], 404);

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/codexpending/codex-pending-requests",
                "query_string": "include_orphaned=maybe",
                "headers": {}
            },
            "python_response": {
                "status": 422,
                "body_sha256": sha256_hex(b"{}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 422);
}

#[tokio::test]
async fn shadow_http_reports_queue_job_detail_404() {
    let state_file = unique_temp_path();
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: state_file
                .with_extension("missing-queue-runner")
                .display()
                .to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    }));
    let python_body = serde_json::to_vec(&json!({ "detail": "Queue job not found" })).unwrap();

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/queue-jobs/missing",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 404,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["predicted_status"], 404);
    assert_eq!(payload["body_sha256_match"], true);
}

#[tokio::test]
async fn shadow_http_reports_queue_job_detail_200_as_status_only() {
    let state_file = unique_temp_path();
    let queue_state_dir = state_file.with_extension("queue-runner");
    fs::write(&state_file, json!({ "sessions": [] }).to_string()).unwrap();
    create_queue_jobs_fixture_db(&queue_state_dir);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            cancel_grace_seconds: 0,
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    }));
    let python_body = br#"{"id":"job-pending","type":"tests","label":"cargo tests","requester_session_id":"requester1","requester_name":null,"notify_session_id":"notify1","notify_name":"notify1","cwd":"/repo","argv":["cargo","test"],"script_path":null,"env":{},"timeout_seconds":900,"state":"pending","holding_reason":"memory","queued_at":"2026-06-01T00:00:00","started_at":null,"finished_at":null,"pid":null,"process_group_id":null,"exit_code":null,"log_path":"/tmp/job-pending.log","exit_code_path":null,"wrapper_path":null,"queued_notified_at":null,"started_notified_at":null,"completion_notified_at":null}"#;

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/queue-jobs/job-pending",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(python_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
    assert_eq!(payload["body_sha256_match"], Value::Null);
}

#[tokio::test]
async fn shadow_http_does_not_predict_malformed_queue_job_detail_paths() {
    let app = router(AppState::new(AppConfig::default()));

    for path in ["/queue-jobs/", "/queue-jobs/job-pending/extra"] {
        let (status, payload) = post_json(
            app.clone(),
            "/__shadow/http",
            json!({
                "schema_version": 1,
                "request": {
                    "method": "GET",
                    "path": path,
                    "query_string": "",
                    "headers": {}
                },
                "python_response": {
                    "status": 404,
                    "body_sha256": sha256_hex(b"{\"detail\":\"Not Found\"}")
                }
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["support_status"], "unsupported");
        assert_eq!(payload["comparison"], "not_compared");
        assert_eq!(payload["predicted_status"], Value::Null);
        assert_eq!(payload["body_sha256_match"], Value::Null);
    }
}

#[tokio::test]
async fn shadow_http_reports_body_mismatch_for_stable_read_only_route() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"different")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "body_mismatch");
    assert_eq!(payload["body_sha256_match"], false);
}

#[tokio::test]
async fn shadow_http_classifies_core_writes_without_side_effects() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json(
        app.clone(),
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "POST",
                "path": "/sessions",
                "query_string": "",
                "headers": {},
                "body_sha256": sha256_hex(b"{\"working_dir\":\"~\"}")
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"id\":\"python-owned\"}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "unsupported_retained_write");
    assert_eq!(payload["comparison"], "not_compared");
    assert_eq!(payload["would_write"], false);
    assert!(payload["detail"]
        .as_str()
        .unwrap()
        .contains("never performs retained write side effects"));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "POST",
                "path": "/nodes/primary/restore-candidates/stop1234/restore",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"id\":\"stop1234\"}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "unsupported_retained_write");
    assert_eq!(payload["comparison"], "not_compared");
    assert_eq!(payload["would_write"], false);
    assert!(payload["detail"]
        .as_str()
        .unwrap()
        .contains("never performs retained write side effects"));
}

#[tokio::test]
async fn shadow_http_classifies_native_mobile_writes_without_side_effects() {
    let app = router(AppState::new(AppConfig::default()));
    for path in [
        "/client/request-status",
        "/client/bug-reports",
        "/deploy/session-manager-android",
    ] {
        let (status, payload) = post_json(
            app.clone(),
            "/__shadow/http",
            json!({
                "schema_version": 1,
                "request": {
                    "method": "POST",
                    "path": path,
                    "query_string": "",
                    "headers": {},
                    "body_sha256": sha256_hex(b"{}")
                },
                "python_response": {
                    "status": 200,
                    "body_sha256": sha256_hex(b"{\"status\":\"python-owned\"}")
                }
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(
            payload["support_status"], "unsupported_retained_write",
            "{path}"
        );
        assert_eq!(payload["comparison"], "not_compared", "{path}");
        assert_eq!(payload["would_write"], false, "{path}");
    }
}

#[tokio::test]
async fn shadow_http_preserves_python_auth_denial_for_app_artifact_reads() {
    let app = router(AppState::new(AppConfig::default()));

    for path in ["/apk", "/apps/session-manager-android/meta.json"] {
        let (status, payload) = post_json(
            app.clone(),
            "/__shadow/http",
            json!({
                "schema_version": 1,
                "request": {
                    "method": "GET",
                    "path": path,
                    "query_string": "",
                    "headers": {}
                },
                "python_response": {
                    "status": 401,
                    "body_sha256": sha256_hex(b"{\"detail\":\"Authentication required\"}")
                }
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(payload["support_status"], "python_auth_denial", "{path}");
        assert_eq!(payload["comparison"], "status_match", "{path}");
        assert_eq!(payload["would_write"], false, "{path}");
    }
}

#[tokio::test]
async fn shadow_http_rejects_remote_without_shadow_secret() {
    let app = router(AppState::new(AppConfig::default()));
    let python_body = serde_json::to_vec(&json!({ "status": "healthy" })).unwrap();

    let (status, payload) = post_json_with_headers_and_peer(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
        &[],
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Rust shadow endpoint requires local peer or shadow secret"
    );
}

#[tokio::test]
async fn shadow_http_allows_remote_with_configured_shadow_secret() {
    let app = router(AppState::new(AppConfig {
        rust_shadow: RustShadowConfig {
            secret: Some("shared-shadow-secret".to_owned()),
        },
        ..AppConfig::default()
    }));
    let python_body = serde_json::to_vec(&json!({ "status": "healthy" })).unwrap();

    let (status, payload) = post_json_with_headers_and_peer(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
        &[("x-sm-rust-shadow-secret", "shared-shadow-secret")],
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["comparison"], "match");
}

#[tokio::test]
async fn shadow_http_requires_configured_secret_even_from_loopback() {
    let app = router(AppState::new(AppConfig {
        rust_shadow: RustShadowConfig {
            secret: Some("shared-shadow-secret".to_owned()),
        },
        ..AppConfig::default()
    }));
    let python_body = serde_json::to_vec(&json!({ "status": "healthy" })).unwrap();

    let (status, payload) = post_json_with_headers_and_peer(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/health",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&python_body)
            }
        }),
        &[],
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Rust shadow endpoint requires local peer or shadow secret"
    );
}

#[tokio::test]
async fn shadow_http_preserves_python_auth_denial_for_protected_reads() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 401,
                "body_sha256": sha256_hex(b"{\"detail\":\"Authentication required\"}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "python_auth_denial");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 401);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
}

#[tokio::test]
async fn shadow_http_treats_auth_session_as_status_only() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            client_id: Some("web-client".to_owned()),
            client_secret: Some("web-secret".to_owned()),
            redirect_uri: Some("https://sm.example.com/auth/google/callback".to_owned()),
            allowlist_emails: vec!["user@example.com".to_owned()],
            session_cookie_secret: Some("cookie-secret".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/auth/session",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"authenticated\":true}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
}

#[tokio::test]
async fn shadow_http_treats_bootstrap_as_status_only_for_mobile_terminal_metadata() {
    let app = router(AppState::new(AppConfig {
        mobile_terminal: MobileTerminalConfig {
            enabled: true,
            ..MobileTerminalConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/client/bootstrap",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"external_access\":{\"mobile_terminal_supported\":true}}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read_status_only");
    assert_eq!(payload["comparison"], "status_match");
    assert_eq!(payload["predicted_status"], 200);
    assert_eq!(payload["predicted_body_sha256"], Value::Null);
}

#[tokio::test]
async fn shadow_http_does_not_treat_static_sessions_route_as_session_id() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/context-monitor",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(b"{\"enabled\":true}")
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "body_mismatch");
    assert_eq!(payload["predicted_status"], 200);
}

#[tokio::test]
async fn auth_session_reports_local_bypass_for_localhost_when_auth_is_misconfigured() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json_with_host_and_peer(
        app,
        "/auth/session",
        "localhost:8421",
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": true,
            "authenticated": true,
            "bypass": true,
            "email": null,
            "name": null
        })
    );
}

#[tokio::test]
async fn auth_session_reports_misconfigured_on_public_host_without_ready_google_auth() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json_with_host(app, "/auth/session", "sm.example.com").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": true,
            "authenticated": false,
            "bypass": false,
            "email": null,
            "name": null,
            "error": "misconfigured"
        })
    );
}

#[tokio::test]
async fn auth_session_does_not_bypass_for_spoofed_localhost_host_from_remote_peer() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json_with_host_and_peer(
        app,
        "/auth/session",
        "localhost:8421",
        Some(SocketAddr::from(([203, 0, 113, 10], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": true,
            "authenticated": false,
            "bypass": false,
            "email": null,
            "name": null,
            "error": "misconfigured"
        })
    );
}

#[tokio::test]
async fn bootstrap_preserves_native_schema_without_termux_or_terminal_when_disabled() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            client_id: Some("web-client-id".to_owned()),
            ..GoogleAuthConfig::default()
        },
        external_access: ExternalAccessConfig {
            public_http_host: Some("sm.example.com".to_owned()),
            public_ssh_host: Some("ssh.sm.example.com".to_owned()),
            ssh_username: Some("rajesh".to_owned()),
            ..ExternalAccessConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/client/bootstrap").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "auth": {
                "mode": "browser_session_cookie",
                "session_endpoint": "/auth/session",
                "login_endpoint": "/auth/google/login",
                "logout_endpoint": "/auth/logout",
                "device_auth_endpoint": "/auth/device/google",
                "device_auth_token_type": "Bearer",
                "google_server_client_id": "web-client-id"
            },
            "external_access": {
                "public_http_host": "sm.example.com",
                "public_ssh_host": "ssh.sm.example.com",
                "ssh_username": "rajesh",
                "termux_attach_supported": false,
                "mobile_terminal_supported": false,
                "mobile_terminal_ws_url": null
            },
            "session_open_defaults": {
                "preferred_action": "details",
                "termux_package": "com.termux"
            }
        })
    );
    assert!(payload["external_access"]
        .get("ssh_proxy_command")
        .is_none());
}

#[tokio::test]
async fn bootstrap_advertises_configured_mobile_terminal() {
    let app = router(AppState::new(AppConfig {
        google_auth: GoogleAuthConfig {
            client_id: Some("web-client-id".to_owned()),
            ..GoogleAuthConfig::default()
        },
        external_access: ExternalAccessConfig {
            public_http_host: Some("sm.example.com".to_owned()),
            ..ExternalAccessConfig::default()
        },
        mobile_terminal: MobileTerminalConfig {
            enabled: true,
            ..MobileTerminalConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/client/bootstrap").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["external_access"]["mobile_terminal_supported"],
        true
    );
    assert_eq!(
        payload["external_access"]["mobile_terminal_ws_url"],
        "wss://sm.example.com/client/terminal"
    );
    assert_eq!(
        payload["session_open_defaults"],
        json!({
            "preferred_action": "mobile_terminal",
            "termux_package": "com.termux"
        })
    );
}

#[tokio::test]
async fn bootstrap_does_not_advertise_mobile_terminal_without_public_ws_url() {
    let app = router(AppState::new(AppConfig {
        mobile_terminal: MobileTerminalConfig {
            enabled: true,
            ..MobileTerminalConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/client/bootstrap").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["external_access"]["mobile_terminal_supported"],
        false
    );
    assert_eq!(
        payload["external_access"]["mobile_terminal_ws_url"],
        Value::Null
    );
    assert_eq!(
        payload["session_open_defaults"],
        json!({
            "preferred_action": "details",
            "termux_package": "com.termux"
        })
    );
}

#[tokio::test]
async fn absent_routes_are_not_implemented() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/api/sessions").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Not Found" }));
}

#[tokio::test]
async fn events_state_returns_fallback_snapshot() {
    let app = router(AppState::new(AppConfig::default()));

    let (status, payload) = get_json(app, "/events/state").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "tmux_client_event_version": 0,
            "last_tmux_client_event": null
        })
    );
}

#[tokio::test]
async fn events_stream_emits_hello_frame_with_sse_headers() {
    let app = router(AppState::new(AppConfig::default()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let mut body_stream = response.into_body().into_data_stream();
    let first_chunk = body_stream.next().await.unwrap().unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );
    assert_eq!(
        headers
            .get("x-accel-buffering")
            .and_then(|value| value.to_str().ok()),
        Some("no")
    );
    assert_eq!(
        String::from_utf8(first_chunk.to_vec()).unwrap(),
        "event: hello\ndata: {\"tmux_client_event_version\":0,\"last_tmux_client_event\":null}\n\n"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), body_stream.next())
            .await
            .is_err(),
        "SSE stream closed before the keepalive interval"
    );
}

#[tokio::test]
async fn events_state_rejects_public_host_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));

    let (status, payload) = get_json_with_host(app, "/events/state", "sm.example.com").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        payload,
        json!({
            "detail": "Authentication required",
            "login_url": "/auth/google/login?next=%2Fevents%2Fstate"
        })
    );
}

#[tokio::test]
async fn sessions_lists_running_sessions_and_filters_stopped_by_default() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(payload["sessions"][0]["id"], "run12345");
    assert_eq!(payload["sessions"][0]["friendly_name"], "Runner Native");
    assert_eq!(payload["sessions"][0]["activity_state"], "working");
    assert_eq!(payload["sessions"][0]["provider"], "claude");
    assert_eq!(payload["sessions"][1]["id"], "oldstate");
    assert_eq!(payload["sessions"][1]["friendly_name"], "claude-oldstate");
    assert_eq!(payload["sessions"][1]["status"], "idle");
    assert_eq!(payload["sessions"][1]["activity_state"], "idle");
    assert!(payload["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["id"] != "stop1234"));
}

#[tokio::test]
async fn sessions_can_include_stopped_sessions() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions?include_stopped=true").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 3);
    assert_eq!(payload["sessions"][2]["id"], "stop1234");
    assert_eq!(payload["sessions"][2]["activity_state"], "stopped");
}

#[tokio::test]
async fn client_sessions_adds_attach_descriptor_without_termux_when_mobile_disabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/client/sessions").await;

    assert_eq!(status, StatusCode::OK);
    let first = &payload["sessions"][0];
    assert_eq!(first["id"], "run12345");
    assert_eq!(first["attach_descriptor"]["attach_supported"], true);
    assert_eq!(
        first["attach_descriptor"]["tmux_session"],
        "claude-run12345"
    );
    assert_eq!(first["attach_descriptor"]["message"], Value::Null);
    assert_eq!(first["termux_attach"], Value::Null);
    assert_eq!(first["mobile_terminal"]["supported"], false);
    assert_eq!(
        first["mobile_terminal"]["reason"],
        "mobile terminal attach is disabled"
    );
    assert_eq!(first["primary_action"]["type"], "details");
    assert!(payload["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["id"] != "stop1234"));
}

#[tokio::test]
async fn client_sessions_advertises_mobile_terminal_for_authorized_local_user() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_mobile_terminal(
        &state_file,
    )));

    let (status, payload) = get_json_with_host_and_peer(
        app,
        "/client/sessions",
        "localhost:8421",
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let first = &payload["sessions"][0];
    assert_eq!(first["id"], "run12345");
    assert_eq!(first["termux_attach"], Value::Null);
    assert_eq!(
        first["mobile_terminal"],
        json!({
            "supported": true,
            "transport": "sm-https-tmux",
            "ticket_endpoint": "/client/sessions/run12345/attach-ticket",
            "ws_url": "wss://sm.example.com/client/terminal",
            "tmux_session": "claude-run12345",
            "tmux_socket_name": null,
            "runtime_mode": "detached_runtime",
            "requires_device_key": true
        })
    );
    assert_eq!(
        first["primary_action"],
        json!({
            "type": "mobile_terminal",
            "label": "Attach"
        })
    );
}

#[tokio::test]
async fn session_detail_returns_one_projected_session() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions/run12345").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "run12345");
    assert_eq!(payload["friendly_name"], "Runner Native");
    assert_eq!(payload["activity_state"], "working");
    assert_eq!(payload["provider"], "claude");
}

#[tokio::test]
async fn session_detail_returns_404_for_unknown_session() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions/missing-session").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Session not found" }));
}

#[tokio::test]
async fn client_session_detail_returns_mobile_metadata_for_one_session() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/client/sessions/run12345").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "run12345");
    assert_eq!(payload["attach_descriptor"]["attach_supported"], true);
    assert_eq!(payload["termux_attach"], Value::Null);
    assert_eq!(payload["mobile_terminal"]["supported"], false);
    assert_eq!(
        payload["mobile_terminal"]["reason"],
        "mobile terminal attach is disabled"
    );
    assert_eq!(payload["primary_action"]["type"], "details");
}

#[tokio::test]
async fn client_session_detail_explains_mobile_terminal_auth_gap_without_actor() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_mobile_terminal(
        &state_file,
    )));

    let (status, payload) = get_json(app, "/client/sessions/run12345").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "run12345");
    assert_eq!(payload["attach_descriptor"]["attach_supported"], true);
    assert_eq!(payload["mobile_terminal"]["supported"], false);
    assert_eq!(
        payload["mobile_terminal"]["reason"],
        "authenticated mobile terminal user is required"
    );
    assert_eq!(payload["primary_action"]["type"], "details");
}

#[tokio::test]
async fn client_session_detail_preserves_attach_unsupported_primary_action_reason() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/client/sessions/stop1234").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "stop1234");
    assert_eq!(payload["attach_descriptor"]["attach_supported"], false);
    assert_eq!(
        payload["attach_descriptor"]["message"],
        "Session is stopped"
    );
    assert_eq!(
        payload["primary_action"],
        json!({
            "type": "details",
            "label": "View details",
            "reason": "Session is stopped"
        })
    );
}

#[tokio::test]
async fn session_detail_prunes_stale_role_aliases() {
    let state_file = write_registry_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app.clone(), "/sessions/stale-role").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Session not found" }));

    let (status, payload) = get_json(app.clone(), "/client/sessions/stale-role").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Session not found" }));

    let (status, payload) = get_json(app, "/sessions/reviewer").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "child001");
}

#[tokio::test]
async fn session_output_tails_fixture_log_file() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions/run12345/output?lines=2").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "run12345");
    assert_eq!(
        payload["output"],
        "fixture log line 2\nfixture log line 3\n"
    );
}

#[tokio::test]
async fn session_tool_calls_reads_pre_tool_use_rows() {
    let state_file = write_session_fixture();
    let tool_db = unique_temp_path();
    create_tool_usage_fixture_db(&tool_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        tool_logging: ToolLoggingConfig {
            db_path: tool_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/sessions/run12345/tool-calls?limit=2").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "run12345");
    assert_eq!(
        payload["tool_calls"],
        json!([
            {
                "timestamp": "2026-06-01 00:02:00",
                "tool_name": "Bash",
                "hook_type": "PreToolUse"
            },
            {
                "timestamp": "2026-06-01 00:01:00",
                "tool_name": "Read",
                "hook_type": "PreToolUse"
            }
        ])
    );
}

#[tokio::test]
async fn session_tool_calls_projects_codex_fork_observability_rows() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "forktools",
                    "name": "codex-fork-forktools",
                    "working_dir": "/repo",
                    "tmux_session": "codex-fork-forktools",
                    "tmux_socket_name": null,
                    "node": "primary",
                    "provider": "codex-fork",
                    "log_file": "/tmp/forktools.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let observability_db = unique_temp_path();
    create_codex_observability_fixture_db(&observability_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_observability: CodexObservabilityConfig {
            db_path: observability_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/sessions/forktools/tool-calls?limit=2").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "forktools");
    assert_eq!(
        payload["tool_calls"],
        json!([
            {
                "timestamp": "2026-06-01T00:03:00+00:00",
                "tool_name": "Bash",
                "hook_type": "CodexForkToolCall"
            },
            {
                "timestamp": "2026-06-01T00:05:00+00:00",
                "tool_name": "Edit",
                "hook_type": "CodexForkToolCall"
            }
        ])
    );
}

#[tokio::test]
async fn session_tool_calls_codex_fork_missing_observability_db_returns_empty_rows() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "forktools",
                    "name": "codex-fork-forktools",
                    "working_dir": "/repo",
                    "tmux_session": "codex-fork-forktools",
                    "provider": "codex-fork",
                    "log_file": "/tmp/forktools.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_observability: CodexObservabilityConfig {
            db_path: state_file
                .with_extension("missing-codex-observability.db")
                .display()
                .to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/sessions/forktools/tool-calls").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({ "session_id": "forktools", "tool_calls": [] })
    );
}

#[tokio::test]
async fn session_activity_actions_projects_codex_observability_rows() {
    let state_file = write_codex_app_session_fixture("codexproj");
    let observability_db = unique_temp_path();
    create_codex_activity_fixture_db(&observability_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_observability: CodexObservabilityConfig {
            db_path: observability_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(
        app,
        "/sessions/codexproj/activity-actions?limit=%32&limit=3",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["actions"].as_array().unwrap().len(), 3);
    assert_eq!(
        payload["actions"][0],
        json!({
            "source_provider": "codex-app",
            "action_kind": "command",
            "summary_text": "Started: pytest -q",
            "status": "running",
            "started_at": "2026-02-21T10:00:00+00:00",
            "ended_at": null,
            "session_id": "codexproj",
            "turn_id": "turn-1",
            "item_id": "item-1"
        })
    );
    assert_eq!(
        payload["actions"][1]["summary_text"],
        "Failed pytest -q: non-zero exit"
    );
    assert_eq!(
        payload["actions"][1]["started_at"],
        "2026-02-21T10:00:00+00:00"
    );
    assert_eq!(
        payload["actions"][1]["ended_at"],
        "2026-02-21T10:00:05+00:00"
    );
    assert_eq!(payload["actions"][2]["action_kind"], "approval");
    assert_eq!(
        payload["actions"][2]["summary_text"],
        "Approval decision: accept"
    );
    assert_eq!(payload["actions"][2]["status"], "completed");
}

#[tokio::test]
async fn session_activity_actions_handles_empty_and_gating_errors() {
    let state_file = write_codex_app_session_fixture("codexproj");
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app.clone(), "/sessions/codexproj/activity-actions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "actions": [] }));

    let (status, payload) = get_json(app.clone(), "/sessions/missing/activity-actions").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Session not found");

    let non_codex_state = write_session_fixture();
    let non_codex_app = router(AppState::new(config_with_state_file(&non_codex_state)));
    let (status, payload) = get_json(non_codex_app, "/sessions/run12345/activity-actions").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload["detail"],
        "activity actions supported only for provider=codex-app"
    );

    let disabled_app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_rollout: CodexRolloutConfig {
            enable_observability_projection: false,
            ..CodexRolloutConfig::default()
        },
        ..AppConfig::default()
    }));
    let (status, payload) = get_json(disabled_app, "/sessions/codexproj/activity-actions").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        payload["detail"],
        "codex activity projection disabled by rollout flag"
    );

    let (status, payload) = get_json(app, "/sessions/codexproj/activity-actions?limit=201").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(payload["detail"], "limit must be between 1 and 200");
}

#[tokio::test]
async fn session_codex_events_reads_recent_events_with_python_cursor_shape() {
    let state_file = write_codex_app_session_fixture("codexapp1");
    let events_db = unique_temp_path();
    create_codex_events_fixture_db(&events_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_events: CodexEventsConfig {
            db_path: events_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app, "/sessions/codexapp1/codex-events?limit=2").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["earliest_seq"], 1);
    assert_eq!(payload["latest_seq"], 4);
    assert_eq!(payload["next_seq"], 5);
    assert_eq!(payload["history_gap"], false);
    assert_eq!(payload["gap_reason"], Value::Null);
    assert_eq!(
        payload["events"],
        json!([
            {
                "session_id": "codexapp1",
                "seq": 3,
                "timestamp": "2026-06-01T00:03:00+00:00",
                "event_type": "item_completed",
                "turn_id": "turn-a",
                "payload_preview": {"raw": "not-json"},
                "persisted": true
            },
            {
                "session_id": "codexapp1",
                "seq": 4,
                "timestamp": "2026-06-01T00:04:00+00:00",
                "event_type": "turn_completed",
                "turn_id": null,
                "payload_preview": null,
                "persisted": true
            }
        ])
    );
}

#[tokio::test]
async fn session_codex_events_since_seq_reports_retention_gap() {
    let state_file = write_codex_app_session_fixture("codexapp1");
    let events_db = unique_temp_path();
    create_codex_events_fixture_db(&events_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_events: CodexEventsConfig {
            db_path: events_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) =
        get_json(app, "/sessions/codexapp1/codex-events?since_seq=0&limit=2").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["history_gap"], false);
    assert_eq!(payload["gap_reason"], Value::Null);
    assert_eq!(payload["events"][0]["seq"], 1);
    assert_eq!(payload["next_seq"], 3);

    let state_file = write_codex_app_session_fixture("retainedgap");
    let events_db = unique_temp_path();
    create_codex_events_retention_gap_fixture_db(&events_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_events: CodexEventsConfig {
            db_path: events_db.display().to_string(),
        },
        ..AppConfig::default()
    }));
    let (status, payload) = get_json(app, "/sessions/retainedgap/codex-events?since_seq=1").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["earliest_seq"], 4);
    assert_eq!(payload["latest_seq"], 5);
    assert_eq!(payload["history_gap"], true);
    assert_eq!(payload["gap_reason"], "retention");
    assert_eq!(payload["events"][0]["seq"], 4);
}

#[tokio::test]
async fn session_codex_events_handles_missing_db_session_and_provider_errors() {
    let state_file = write_codex_app_session_fixture("codexapp1");
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_events: CodexEventsConfig {
            db_path: state_file
                .with_extension("missing-codex-events.db")
                .display()
                .to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app.clone(), "/sessions/codexapp1/codex-events").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "events": [],
            "earliest_seq": null,
            "latest_seq": null,
            "next_seq": 1,
            "history_gap": false,
            "gap_reason": null
        })
    );

    let (status, payload) = get_json(app.clone(), "/sessions/missing/codex-events").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Session not found");

    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let (status, payload) = get_json(app, "/sessions/run12345/codex-events").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload["detail"],
        "codex-events supported only for provider=codex-app"
    );

    let state_file = write_codex_app_session_fixture("codexapp1");
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_rollout: CodexRolloutConfig {
            enable_durable_events: false,
            ..CodexRolloutConfig::default()
        },
        ..AppConfig::default()
    }));
    let (status, payload) = get_json(app, "/sessions/codexapp1/codex-events").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        payload["detail"],
        "codex durable events disabled by rollout flag"
    );
}

#[tokio::test]
async fn session_codex_events_rejects_invalid_query_values_before_session_lookup() {
    let state_file = write_codex_app_session_fixture("codexapp1");
    let app = router(AppState::new(config_with_state_file(&state_file)));

    for (uri, detail) in [
        (
            "/sessions/codexapp1/codex-events?limit=0",
            "limit must be between 1 and 500",
        ),
        (
            "/sessions/codexapp1/codex-events?limit=abc",
            "limit must be between 1 and 500",
        ),
        (
            "/sessions/codexapp1/codex-events?since_seq=-1",
            "since_seq must be >= 0 and less than 9223372036854775807",
        ),
        (
            "/sessions/codexapp1/codex-events?since_seq=9223372036854775807",
            "since_seq must be >= 0 and less than 9223372036854775807",
        ),
        (
            "/sessions/missing/codex-events?limit=0",
            "limit must be between 1 and 500",
        ),
    ] {
        let (status, payload) = get_json(app.clone(), uri).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "uri={uri}");
        assert_eq!(payload["detail"], detail, "uri={uri}");
    }
}

#[tokio::test]
async fn session_codex_events_decodes_query_and_uses_last_duplicate_value() {
    let state_file = write_codex_app_session_fixture("codexapp1");
    let events_db = unique_temp_path();
    create_codex_events_fixture_db(&events_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_events: CodexEventsConfig {
            db_path: events_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(
        app.clone(),
        "/sessions/codexapp1/codex-events?limit=%32&since_seq=%31",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["events"][0]["seq"], 2);
    assert_eq!(payload["events"].as_array().unwrap().len(), 2);

    let (status, payload) = get_json(
        app.clone(),
        "/sessions/codexapp1/codex-events?limit=0&limit=2",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["events"].as_array().unwrap().len(), 2);

    let (status, payload) = get_json(app, "/sessions/codexapp1/codex-events?limit=2&limit=0").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(payload["detail"], "limit must be between 1 and 500");
}

#[tokio::test]
async fn session_codex_pending_requests_reads_pending_and_optional_orphaned_rows() {
    let state_file = write_codex_app_session_fixture("codexpending");
    let requests_db = unique_temp_path();
    create_codex_pending_requests_fixture_db(&requests_db);
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_requests: CodexRequestsConfig {
            db_path: requests_db.display().to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) =
        get_json(app.clone(), "/sessions/codexpending/codex-pending-requests").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "requests": [
                {
                    "request_id": "req-pending",
                    "session_id": "codexpending",
                    "thread_id": "thread-a",
                    "turn_id": "turn-a",
                    "item_id": "item-a",
                    "request_type": "request_approval",
                    "request_method": "item/commandExecution/requestApproval",
                    "status": "pending",
                    "requested_at": "2026-06-01T00:00:00+00:00",
                    "expires_at": "2026-06-01T00:05:00+00:00",
                    "resolved_payload": null,
                    "resolved_at": null,
                    "resolution_source": null,
                    "error_code": null,
                    "error_message": null
                }
            ]
        })
    );

    let (status, payload) = get_json(
        app,
        "/sessions/codexpending/codex-pending-requests?include_orphaned=false&include_orphaned=%74",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let requests = payload["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["request_id"], "req-pending");
    assert_eq!(requests[1]["request_id"], "req-orphaned");
    assert_eq!(requests[1]["status"], "orphaned");
    assert_eq!(requests[1]["resolution_source"], "policy");
    assert_eq!(requests[1]["error_code"], "server_restarted");
}

#[tokio::test]
async fn session_codex_pending_requests_handles_empty_and_gating_errors() {
    let state_file = write_codex_app_session_fixture("codexpending");
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) =
        get_json(app.clone(), "/sessions/codexpending/codex-pending-requests").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "requests": [] }));

    let (status, payload) = get_json(app.clone(), "/sessions/missing/codex-pending-requests").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Session not found");

    let non_codex_state = write_session_fixture();
    let non_codex_app = router(AppState::new(config_with_state_file(&non_codex_state)));
    let (status, payload) =
        get_json(non_codex_app, "/sessions/run12345/codex-pending-requests").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload["detail"],
        "codex requests supported only for provider=codex-app"
    );

    let disabled_app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_rollout: CodexRolloutConfig {
            enable_structured_requests: false,
            ..CodexRolloutConfig::default()
        },
        ..AppConfig::default()
    }));
    let (status, payload) = get_json(
        disabled_app,
        "/sessions/codexpending/codex-pending-requests",
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        payload["detail"],
        "codex structured requests disabled by rollout flag"
    );

    let (status, payload) = get_json(
        app.clone(),
        "/sessions/codexpending/codex-pending-requests?include_orphaned=maybe",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(payload["detail"], "include_orphaned must be a boolean");

    let (status, payload) = get_json(
        app,
        "/sessions/codexpending/codex-pending-requests?include_orphaned=%20true%20",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(payload["detail"], "include_orphaned must be a boolean");
}

#[tokio::test]
async fn session_tool_calls_handles_missing_db_and_invalid_limit() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        tool_logging: ToolLoggingConfig {
            db_path: state_file
                .with_extension("missing-tool-usage.db")
                .display()
                .to_string(),
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app.clone(), "/sessions/run12345/tool-calls").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({ "session_id": "run12345", "tool_calls": [] })
    );

    let (status, payload) = get_json(app.clone(), "/sessions/missing/tool-calls").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Session not found");

    let (status, payload) = get_json(app, "/sessions/run12345/tool-calls?limit=0").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(payload["detail"], "limit must be between 1 and 100");
}

#[tokio::test]
async fn fixture_core_writes_are_disabled_by_default() {
    let state_file = write_session_fixture();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let app = router(AppState::new(config_with_state_file_and_queue(&state_file)));

    let (status, payload) = post_json(
        app,
        "/sessions",
        json!({
            "id": "rustcore",
            "name": "rust-core",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        payload,
        json!({ "detail": "Rust core writes are disabled" })
    );
    assert!(
        !queue_db_path.exists(),
        "disabled Rust core writes must not create the retained queue DB"
    );
}

#[test]
fn runtime_core_inherits_existing_tmux_socket_config() {
    let config_path = unique_temp_path();
    let missing_env_path = unique_temp_path();

    fs::write(
        &config_path,
        r#"
tmux:
  socket_name: "session-manager"
timeouts:
  tmux:
    send_keys_settle_seconds: 0.25
    send_keys_settle_max_seconds: 1.25
    send_keys_settle_per_ki_chars: 0.07
    send_keys_settle_per_extra_line: 0.02
    send_keys_max_chunk_chars: 2048
rust_core:
  runtime_enabled: true
"#,
    )
    .unwrap();
    let config =
        AppConfig::load_from_path_with_local_env(&config_path, Some(&missing_env_path)).unwrap();
    assert_eq!(
        config.rust_core.tmux_socket_name.as_deref(),
        Some("session-manager")
    );
    assert_eq!(config.rust_core.send_keys_settle_ms, Some(250.0));
    assert_eq!(config.rust_core.send_keys_settle_max_ms, Some(1250.0));
    assert_eq!(config.rust_core.send_keys_settle_per_ki_ms, Some(70.0));
    assert_eq!(
        config.rust_core.send_keys_settle_per_extra_line_ms,
        Some(20.0)
    );
    assert_eq!(config.rust_core.send_keys_max_chunk_chars, Some(2048));

    fs::write(
        &config_path,
        r#"
tmux:
  socket_name: "session-manager"
rust_core:
  runtime_enabled: true
  tmux_socket_name: "rust-core-only"
"#,
    )
    .unwrap();
    let config =
        AppConfig::load_from_path_with_local_env(&config_path, Some(&missing_env_path)).unwrap();
    assert_eq!(
        config.rust_core.tmux_socket_name.as_deref(),
        Some("rust-core-only")
    );
}

#[tokio::test]
async fn fixture_core_lifecycle_creates_sends_outputs_and_retires() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "rustcore",
            "name": "rust-core",
            "working_dir": "/repo",
            "provider": "claude",
            "initial_message": "initial fixture prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "rustcore");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["friendly_name"], "rust-core");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "rustchild",
            "name": "rust-child",
            "parent_session_id": "rustcore",
            "provider": "claude",
            "initial_message": "child fixture prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "rustchild");
    assert_eq!(payload["parent_session_id"], "rustcore");
    assert_eq!(payload["working_dir"], "/repo");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustcore/input",
        json!({
            "text": "hello from rust fixture",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    assert_eq!(payload["delivery_mode"], "sequential");
    assert_eq!(payload["notify_after_seconds"], Value::Null);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustcore/input",
        json!({
            "text": "urgent fixture note",
            "delivery_mode": "urgent",
            "notify_after_seconds": 7
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    assert_eq!(payload["delivery_mode"], "urgent");
    assert_eq!(payload["notify_after_seconds"], 7);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustcore/agent-status",
        json!({
            "text": "writing Rust status"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "updated");
    assert_eq!(payload["agent_status_text"], "writing Rust status");

    let (status, payload) = get_json(app.clone(), "/sessions/rustcore/output?lines=4").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["output"]
        .as_str()
        .unwrap()
        .contains("hello from rust fixture"));

    let (status, payload) = get_json(app.clone(), "/sessions/rustcore").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["agent_status_text"], "writing Rust status");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustchild/kill",
        json!({ "requester_session_id": "otherparent" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["error"],
        "Cannot kill session rustchild - not your child session"
    );

    let (status, payload) = get_json(app.clone(), "/sessions/rustchild").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "running");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/rustchild/kill",
        json!({ "requester_session_id": "rustcore" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert_eq!(payload["session_id"], "rustchild");

    let (status, payload) = get_json(app.clone(), "/sessions/rustchild").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "stopped");

    let (status, payload) = post_json(app.clone(), "/sessions/missingcore/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["error"], "Session missingcore not found");

    let (status, payload) = post_json(app.clone(), "/sessions/rustcore/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = get_json(app, "/sessions?include_stopped=true").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"][0]["id"], "rustcore");
    assert_eq!(payload["sessions"][0]["status"], "stopped");
}

#[tokio::test]
async fn fixture_core_input_batch_reports_per_recipient_results() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    for (id, name) in [
        ("batchfixturea", "batch-fixture-a"),
        ("batchfixtureb", "batch-fixture-b"),
        ("batchstopped", "batch-stopped"),
    ] {
        let (status, payload) = post_json(
            app.clone(),
            "/sessions",
            json!({
                "id": id,
                "name": name,
                "working_dir": "/repo",
                "provider": "claude"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["id"], id);
    }
    let (status, payload) = post_json(app.clone(), "/sessions/batchstopped/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/input-batch",
        json!({
            "recipients": ["batchfixturea, batchfixtureb", "batchstopped", "missingbatch", "batchfixturea"],
            "text": "fixture batch payload",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["requested_count"], 4);
    assert_eq!(payload["success_count"], 2);
    assert_eq!(payload["failure_count"], 2);
    assert_eq!(payload["delivery_mode"], "sequential");
    let results = payload["results"].as_array().unwrap();
    assert_eq!(results[0]["identifier"], "batchfixturea");
    assert_eq!(results[0]["status"], "delivered");
    assert_eq!(results[0]["delivery_kind"], "session");
    assert_eq!(results[0]["session_id"], "batchfixturea");
    assert_eq!(results[0]["target_name"], "batch-fixture-a");
    assert_eq!(results[1]["identifier"], "batchfixtureb");
    assert_eq!(results[1]["status"], "delivered");
    assert_eq!(results[2]["identifier"], "batchstopped");
    assert_eq!(results[2]["status"], "failed");
    assert_eq!(results[2]["delivery_kind"], "none");
    assert_eq!(results[2]["detail"], "Session batchstopped is stopped");
    assert_eq!(results[3]["identifier"], "missingbatch");
    assert_eq!(results[3]["status"], "failed");
    assert_eq!(results[3]["delivery_kind"], "none");
    assert_eq!(results[3]["detail"], "Session 'missingbatch' not found");

    let (status, payload) = get_json(app.clone(), "/sessions/batchfixturea/output?lines=5").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["output"]
        .as_str()
        .unwrap()
        .contains("fixture batch payload"));
    let (status, payload) = get_json(app, "/sessions/batchfixtureb/output?lines=5").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["output"]
        .as_str()
        .unwrap()
        .contains("fixture batch payload"));
}

#[tokio::test]
async fn fixture_core_session_graph_endpoints_round_trip_state() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphparent",
            "name": "graph-parent",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphchild",
            "name": "graph-child",
            "parent_session_id": "graphparent",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphgrandchild",
            "name": "graph-grandchild",
            "parent_session_id": "graphchild",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphgreatgrandchild",
            "name": "graph-great-grandchild",
            "parent_session_id": "graphgrandchild",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, payload) = get_json(app.clone(), "/sessions/graphparent/children").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["parent_session_id"], "graphparent");
    assert_eq!(payload["children"].as_array().unwrap().len(), 1);
    assert_eq!(payload["children"][0]["id"], "graphchild");
    assert_eq!(payload["children"][0]["friendly_name"], "graph-child");

    let (status, payload) =
        get_json(app.clone(), "/sessions/graphparent/children?recursive=true").await;
    assert_eq!(status, StatusCode::OK);
    let child_ids = payload["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|child| child["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        child_ids,
        vec!["graphchild", "graphgrandchild", "graphgreatgrandchild"]
    );

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild/attach-descriptor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["attach"]["attach_supported"], true);
    assert_eq!(payload["attach"]["tmux_session"], "sm-rust-graphchild");

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "graphcodexapp",
            "name": "graph-codex-app",
            "provider": "codex-app"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, payload) =
        get_json(app.clone(), "/sessions/graphcodexapp/attach-descriptor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["attach"]["attach_supported"], false);
    assert_eq!(
        payload["attach"]["message"],
        "Attach not supported for Codex app sessions"
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/context-monitor",
        json!({
            "enabled": true,
            "requester_session_id": "graphparent",
            "notify_session_id": "graphparent"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "status": "ok", "enabled": true }));

    let (status, payload) = get_json(app.clone(), "/sessions/context-monitor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["monitored"][0]["session_id"], "graphchild");
    assert_eq!(payload["monitored"][0]["notify_session_id"], "graphparent");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/agent-status",
        json!({ "text": "old task state" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["agent_status_text"], "old task state");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphparent/clear",
        json!({ "prompt": "root reset denied" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Can only clear child sessions. Target session has no parent."
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/clear",
        json!({
            "prompt": "sibling reset denied",
            "requester_session_id": "graphgrandchild"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Not authorized. You can only clear your child sessions. Target session parent: graphparent"
    );

    let mut state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let child_entry = state["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|session| session["id"] == "graphchild")
        .unwrap();
    child_entry["completion_status"] = json!("completed");
    child_entry["completion_message"] = json!("stale completed message");
    child_entry["completed_at"] = json!("2026-06-01T00:02:00Z");
    child_entry["agent_task_completed_at"] = json!("2026-06-01T00:03:00Z");
    fs::write(&state_file, state.to_string()).unwrap();

    let (status, payload) = get_json(
        app.clone(),
        "/sessions/graphparent/children?status=completed",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["children"].as_array().unwrap().len(), 1);
    assert_eq!(payload["children"][0]["completion_status"], "completed");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/clear",
        json!({ "prompt": "new task after clear" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({ "status": "cleared", "session_id": "graphchild" })
    );

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["agent_status_text"], Value::Null);

    let (status, payload) = get_json(app.clone(), "/sessions/graphparent/children").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["children"][0]["completion_status"], Value::Null);
    assert_eq!(payload["children"][0]["completion_message"], Value::Null);

    let (status, payload) = get_json(
        app.clone(),
        "/sessions/graphparent/children?status=completed",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["children"].as_array().unwrap().len(), 0);

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild/output?lines=5").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["output"]
        .as_str()
        .unwrap()
        .contains("new task after clear"));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/graphchild/handoff",
        json!({
            "requester_session_id": "graphchild",
            "file_path": "/tmp/handoff.md"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "status": "recorded" }));

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["last_handoff_path"], Value::Null);
    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let graph_child = raw_state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "graphchild")
        .unwrap();
    assert_eq!(graph_child["pending_handoff_path"], "/tmp/handoff.md");

    let (status, payload) = post_json(app.clone(), "/sessions/graphchild/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = get_json(app.clone(), "/sessions/graphparent/children").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["children"]
        .as_array()
        .unwrap()
        .iter()
        .all(|child| child["id"] != "graphchild"));

    let (status, payload) = get_json(
        app.clone(),
        "/sessions/graphparent/children?include_terminated=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let graphchild = payload["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["id"] == "graphchild")
        .unwrap();
    assert_eq!(graphchild["completion_status"], "killed");
    assert_eq!(graphchild["completion_message"], "Terminated via sm kill");

    let (status, payload) = get_json(app.clone(), "/sessions/graphchild/attach-descriptor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["attach"]["attach_supported"], false);
    assert_eq!(payload["attach"]["message"], "Session is stopped");

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let child = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "graphchild")
        .unwrap()
        .clone();
    let mut state = state;
    let sessions = state["sessions"].as_array_mut().unwrap();
    let child_entry = sessions
        .iter_mut()
        .find(|session| session["id"] == "graphchild")
        .unwrap();
    child_entry["completion_status"] = json!("killed");
    child_entry["completion_message"] = json!("stale killed message");
    child_entry["completed_at"] = json!("2026-06-01T00:02:00Z");
    child_entry["agent_task_completed_at"] = json!("2026-06-01T00:03:00Z");
    assert_ne!(child, *child_entry);
    fs::write(&state_file, state.to_string()).unwrap();

    let (status, payload) = post_json(app.clone(), "/sessions/graphchild/restore", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "graphchild");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["stopped_at"], Value::Null);
    assert_eq!(payload["completion_status"], Value::Null);
    assert_eq!(payload["completion_message"], Value::Null);
    assert_eq!(payload["completed_at"], Value::Null);
    assert_eq!(payload["agent_task_completed_at"], Value::Null);

    let (status, payload) = post_json(app, "/sessions/graphchild/restore", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(payload, json!({ "detail": "Session is not stopped" }));
}

#[tokio::test]
async fn fixture_subagent_endpoints_round_trip_python_state_shape() {
    let state_file = write_session_fixture();
    let mut config = config_with_state_file(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/missing/subagents",
        json!({
            "agent_id": "agent-missing",
            "agent_type": "engineer"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Session not found");

    let (status, payload) = get_json(app.clone(), "/sessions/run12345/subagents").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({ "session_id": "run12345", "subagents": [] })
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/run12345/subagents",
        json!({
            "agent_id": "agent456789",
            "agent_type": "engineer"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["agent_id"], "agent456789");
    assert_eq!(payload["agent_type"], "engineer");
    assert_eq!(payload["parent_session_id"], "run12345");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["stopped_at"], Value::Null);
    assert_eq!(payload["summary"], Value::Null);
    assert_python_naive_timestamp(payload["started_at"].as_str().unwrap_or_default());

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/run12345/subagents/not-there/stop",
        json!({ "summary": "ignored" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Subagent not-there not found");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/run12345/subagents/agent456789/stop",
        json!({
            "summary": "Finished useful work",
            "transcript_path": "/tmp/agent456789.jsonl"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "session_id": "run12345",
            "agent_id": "agent456789",
            "status": "stopped",
            "summary": "Finished useful work"
        })
    );

    let (status, payload) = get_json(app.clone(), "/sessions/run12345/subagents").await;
    assert_eq!(status, StatusCode::OK);
    let subagents = payload["subagents"].as_array().unwrap();
    assert_eq!(subagents.len(), 1);
    assert_eq!(subagents[0]["agent_id"], "agent456789");
    assert_eq!(subagents[0]["status"], "completed");
    assert_eq!(subagents[0]["summary"], "Finished useful work");
    assert_python_naive_timestamp(subagents[0]["stopped_at"].as_str().unwrap_or_default());

    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let session = raw_state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "run12345")
        .unwrap();
    let stored = &session["subagents"][0];
    assert_eq!(stored["agent_id"], "agent456789");
    assert_eq!(stored["agent_type"], "engineer");
    assert_eq!(stored["parent_session_id"], "run12345");
    assert_eq!(stored["transcript_path"], "/tmp/agent456789.jsonl");
    assert_eq!(stored["status"], "completed");
    assert_eq!(stored["summary"], "Finished useful work");
    assert_python_naive_timestamp(stored["started_at"].as_str().unwrap_or_default());
    assert_python_naive_timestamp(stored["stopped_at"].as_str().unwrap_or_default());
}

#[tokio::test]
async fn fixture_completion_endpoints_preserve_python_compatible_state() {
    let state_file = write_completion_fixture();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let mut config = config_with_state_file_and_queue(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/task-complete",
        json!({ "requester_session_id": "other001" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload["error"].as_str().unwrap().contains("self-directed"));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/task-complete",
        json!({ "requester_session_id": "child001" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "completed");
    assert_eq!(payload["session_id"], "child001");
    assert_eq!(payload["em_notified"], true);
    assert!(payload["agent_task_completed_at"].is_string());

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let child = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "child001")
        .unwrap();
    assert!(child["agent_task_completed_at"].is_string());
    assert_eq!(
        state["retained_remind_registrations"][0]["is_active"],
        false
    );
    assert_eq!(
        state["retained_parent_wake_registrations"][0]["is_active"],
        false
    );
    assert_eq!(
        state["retained_pending_messages"][0]["text"],
        "[sm task-complete] agent child001(worker-1) completed its task."
    );
    assert_eq!(
        state["retained_pending_messages"][0]["target_session_id"],
        "em001"
    );
    assert_eq!(
        state["retained_pending_messages"][0]["delivery_mode"],
        "important"
    );
    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let queued_message: (String, String, String) = queue_conn
        .query_row(
            "SELECT target_session_id, text, delivery_mode FROM message_queue WHERE message_category = 'task_complete'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        queued_message,
        (
            "em001".to_owned(),
            "[sm task-complete] agent child001(worker-1) completed its task.".to_owned(),
            "important".to_owned()
        )
    );

    let state_file = write_completion_fixture();
    let mut config = config_with_state_file_and_queue(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/turn-complete",
        json!({ "requester_session_id": "child001" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({ "status": "turn_completed", "session_id": "child001" })
    );

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let child = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "child001")
        .unwrap();
    assert_eq!(child["agent_task_completed_at"], Value::Null);
    assert_eq!(
        state["retained_remind_registrations"][0]["is_active"],
        false
    );
    assert_eq!(
        state["retained_parent_wake_registrations"][0]["is_active"],
        true
    );
}

#[tokio::test]
async fn fixture_notify_on_stop_preserves_authorization_and_state_contract() {
    let state_file = write_completion_fixture();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let mut config = config_with_state_file_and_queue(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/missing/notify-on-stop",
        json!({ "sender_session_id": "em001", "requester_session_id": "em001" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload["detail"], "Session not found");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/notify-on-stop",
        json!({ "sender_session_id": "child001", "requester_session_id": "child001" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Only EM sessions (is_em=True) may arm stop notifications"
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/notify-on-stop",
        json!({ "sender_session_id": "em002", "requester_session_id": "em002" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["detail"],
        "Cannot arm stop notify — not the parent of target session"
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/notify-on-stop",
        json!({ "sender_session_id": "missing", "requester_session_id": "em001" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(payload["detail"], "sender_session_id \"missing\" not found");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/fork001/notify-on-stop",
        json!({ "sender_session_id": "em001", "requester_session_id": "em001" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "suppressed");
    assert_eq!(
        payload["reason"],
        "notify_on_stop disabled for codex-fork sessions"
    );

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/notify-on-stop",
        json!({
            "sender_session_id": "em001",
            "requester_session_id": "em001",
            "delay_seconds": 8
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({ "status": "ok", "session_id": "child001", "sender_session_id": "em001" })
    );

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(
        state["retained_stop_notify_states"][0]["session_id"],
        "child001"
    );
    assert_eq!(
        state["retained_stop_notify_states"][0]["sender_session_id"],
        "em001"
    );
    assert_eq!(state["retained_stop_notify_states"][0]["sender_name"], "em");
    assert_eq!(state["retained_stop_notify_states"][0]["delay_seconds"], 8);
    assert_eq!(
        state["retained_stop_notify_states"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let stop_notify: (String, String, i64) = queue_conn
        .query_row(
            "SELECT sender_session_id, sender_name, delay_seconds FROM rust_stop_notify_states WHERE session_id = 'child001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(stop_notify, ("em001".to_owned(), "em".to_owned(), 8));
}

#[tokio::test]
async fn fixture_retire_honors_delayed_stop_notify_without_runtime() {
    let state_file = write_completion_fixture();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let mut config = config_with_state_file_and_queue(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/child001/notify-on-stop",
        json!({
            "sender_session_id": "em001",
            "requester_session_id": "em001",
            "delay_seconds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "ok");

    let (status, payload) = post_json(app.clone(), "/sessions/child001/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let immediate_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = 'em001' AND message_category = 'stop_notify'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(immediate_count, 0);

    tokio::time::sleep(Duration::from_millis(1200)).await;
    let delayed: (String, String, Option<String>) = queue_conn
        .query_row(
            r#"
            SELECT text, delivery_mode, message_category
            FROM message_queue
            WHERE target_session_id = 'em001' AND message_category = 'stop_notify'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        delayed,
        (
            "[sm] worker-1 (child001) completed (Stop hook fired)".to_owned(),
            "important".to_owned(),
            Some("stop_notify".to_owned())
        )
    );
    let remaining_stop_notify_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM rust_stop_notify_states WHERE session_id = 'child001'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining_stop_notify_count, 0);
}

#[tokio::test]
async fn fixture_registry_and_maintainer_endpoints_round_trip_state() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "registryowner",
            "name": "registry-owner",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "otherowner",
            "name": "other-owner",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, payload) = put_json(
        app.clone(),
        "/sessions/registryowner/maintainer",
        json!({ "requester_session_id": "otherowner" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["detail"], "sm maintainer is self-directed only");

    let (status, payload) = put_json(
        app.clone(),
        "/sessions/registryowner/maintainer",
        json!({ "requester_session_id": "registryowner" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "registryowner");
    assert_eq!(payload["aliases"], json!(["maintainer"]));
    assert_eq!(payload["is_maintainer"], true);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/registryowner/registry",
        json!({
            "requester_session_id": "registryowner",
            "role": "Review Owner"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["role"], "review-owner");
    assert_eq!(payload["session_id"], "registryowner");
    assert_eq!(payload["friendly_name"], "maintainer");
    assert_eq!(payload["provider"], "claude");
    assert_eq!(payload["activity_state"], "working");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/otherowner/registry",
        json!({
            "requester_session_id": "otherowner",
            "role": "review-owner"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        payload["detail"],
        "Role \"review-owner\" is already registered to registryowner"
    );

    let (status, payload) = get_json(app.clone(), "/registry/review-owner").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["role"], "review-owner");
    assert_eq!(payload["session_id"], "registryowner");

    let (status, payload) = get_json(app.clone(), "/registry").await;
    assert_eq!(status, StatusCode::OK);
    let roles = payload["registrations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(roles, vec!["maintainer", "review-owner"]);

    let (status, payload) = get_json(app.clone(), "/sessions/registryowner").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["aliases"], json!(["maintainer", "review-owner"]));

    let (status, payload) = delete_json(
        app.clone(),
        "/sessions/otherowner/registry",
        json!({
            "requester_session_id": "otherowner",
            "role": "review-owner"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(payload["detail"], "Role is not owned by this session");

    let (status, payload) = delete_json(
        app.clone(),
        "/sessions/registryowner/registry",
        json!({
            "requester_session_id": "registryowner",
            "role": "review-owner"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["role"], "review-owner");
    assert_eq!(payload["session_id"], "registryowner");

    let (status, payload) = get_json(app.clone(), "/registry/review-owner").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Role not registered" }));

    let (status, payload) = delete_json(
        app.clone(),
        "/sessions/registryowner/maintainer",
        json!({ "requester_session_id": "registryowner" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "registryowner");
    assert_eq!(payload["aliases"], json!([]));
    assert_eq!(payload["is_maintainer"], false);
}

#[tokio::test]
async fn fixture_registry_prunes_stale_roles_and_updates_maintainer_alias() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "liveagent",
                    "name": "claude-liveagent",
                    "working_dir": "/repo",
                    "tmux_session": "claude-liveagent",
                    "log_file": "/tmp/liveagent.log",
                    "status": "running",
                    "provider": "claude",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                },
                {
                    "id": "restorable",
                    "name": "claude-restorable",
                    "working_dir": "/repo",
                    "tmux_session": "claude-restorable",
                    "log_file": "/tmp/restorable.log",
                    "status": "stopped",
                    "provider": "claude",
                    "provider_resume_id": "resume-restorable",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                },
                {
                    "id": "staleagent",
                    "name": "claude-staleagent",
                    "working_dir": "/repo",
                    "tmux_session": "claude-staleagent",
                    "log_file": "/tmp/staleagent.log",
                    "status": "stopped",
                    "provider": "claude",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ],
            "maintainer_session_id": "staleagent",
            "agent_registrations": [
                {
                    "role": "Live Role",
                    "session_id": "liveagent",
                    "created_at": "2026-06-01T00:03:00"
                },
                {
                    "role": "Restorable Role",
                    "session_id": "restorable",
                    "created_at": "2026-06-01T00:03:01"
                },
                {
                    "role": "Stale Role",
                    "session_id": "staleagent",
                    "created_at": "2026-06-01T00:03:02"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app.clone(), "/registry").await;
    assert_eq!(status, StatusCode::OK);
    let roles = payload["registrations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(roles, vec!["live-role", "restorable-role"]);

    let (status, payload) = get_json(app.clone(), "/registry/stale-role").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Role not registered" }));

    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(raw_state["maintainer_session_id"], Value::Null);
    assert_eq!(
        raw_state["agent_role_last_session_ids"]["stale-role"],
        "staleagent"
    );
    assert!(raw_state["agent_registrations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["role"] != "stale-role"));
}

#[tokio::test]
async fn fixture_registry_clears_stale_legacy_maintainer_without_registration() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "staleonly",
                    "name": "claude-staleonly",
                    "working_dir": "/repo",
                    "tmux_session": "claude-staleonly",
                    "log_file": "/tmp/staleonly.log",
                    "status": "stopped",
                    "provider": "claude",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ],
            "maintainer_session_id": "staleonly",
            "agent_registrations": []
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/registry").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "registrations": [] }));

    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(raw_state["maintainer_session_id"], Value::Null);
    assert_eq!(
        raw_state["agent_role_last_session_ids"]["maintainer"],
        "staleonly"
    );
}

#[tokio::test]
async fn fixture_registry_clear_removes_recovered_maintainer_history() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "maintainer1",
                    "name": "claude-maintainer1",
                    "working_dir": "/repo",
                    "tmux_session": "claude-maintainer1",
                    "log_file": "/tmp/maintainer1.log",
                    "status": "running",
                    "provider": "claude",
                    "friendly_name": "maintainer",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ],
            "maintainer_session_id": null,
            "agent_role_last_session_ids": {
                "maintainer": "maintainer1"
            },
            "agent_registrations": []
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = get_json(app.clone(), "/registry").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["registrations"][0]["role"], "maintainer");
    assert_eq!(payload["registrations"][0]["session_id"], "maintainer1");

    let (status, payload) = delete_json(
        app.clone(),
        "/sessions/maintainer1/maintainer",
        json!({ "requester_session_id": "maintainer1" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["aliases"], json!([]));
    assert_eq!(payload["is_maintainer"], false);

    let (status, payload) = get_json(app.clone(), "/registry").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "registrations": [] }));
    let (status, payload) = get_json(app, "/registry/maintainer").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(payload, json!({ "detail": "Role not registered" }));

    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(raw_state["maintainer_session_id"], Value::Null);
    assert!(raw_state["agent_role_last_session_ids"]
        .as_object()
        .is_some_and(|last| !last.contains_key("maintainer")));
}

#[tokio::test]
async fn shadow_attach_descriptor_reuses_real_attach_support_rules() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));
    let expected_body = serde_json::to_vec(&json!({
        "attach": {
            "session_id": "stop1234",
            "provider": "claude",
            "attach_supported": false,
            "tmux_session": "claude-stop1234",
            "tmux_socket_name": null,
            "runtime_id": null,
            "lifecycle_state": "stopped",
            "message": "Session is stopped"
        }
    }))
    .unwrap();

    let (status, payload) = post_json(
        app,
        "/__shadow/http",
        json!({
            "schema_version": 1,
            "request": {
                "method": "GET",
                "path": "/sessions/stop1234/attach-descriptor",
                "query_string": "",
                "headers": {}
            },
            "python_response": {
                "status": 200,
                "body_sha256": sha256_hex(&expected_body)
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["support_status"], "implemented_read");
    assert_eq!(payload["comparison"], "match");
    assert_eq!(payload["body_sha256_match"], true);
}

#[tokio::test]
async fn fixture_core_spawn_endpoint_inherits_parent_fields() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let parent_dir = unique_temp_path();
    fs::create_dir_all(&parent_dir).unwrap();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "parentfixture",
                    "name": "claude-parentfixture",
                    "working_dir": parent_dir.display().to_string(),
                    "tmux_session": "claude-parentfixture",
                    "node": "primary",
                    "provider": "claude",
                    "log_file": "/tmp/parentfixture.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ],
            "agent_registrations": [
                {
                    "role": "parent-alias",
                    "session_id": "parentfixture"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "trackedchildfixture",
            "parent_session_id": "parentfixture",
            "prompt": "tracked child fixture prompt",
            "name": "tracked-child-fixture",
            "track_seconds": 300
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust core spawn does not support track_seconds yet" })
    );
    let (status, _payload) = get_json(app.clone(), "/sessions/trackedchildfixture").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "childfixture",
            "parent_session_id": "parent-alias",
            "prompt": "child fixture prompt",
            "name": "child-fixture"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "childfixture");
    assert_eq!(payload["friendly_name"], "child-fixture");
    assert_eq!(payload["parent_session_id"], "parentfixture");
    assert_eq!(payload["working_dir"], parent_dir.display().to_string());
    assert_eq!(payload["node"], "primary");
    assert_eq!(payload["provider"], "claude");

    let (status, payload) = get_json(app, "/sessions/childfixture").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["parent_session_id"], "parentfixture");
    assert_eq!(payload["working_dir"], parent_dir.display().to_string());
}

#[tokio::test]
async fn fixture_core_spawn_auto_arms_em_stop_notify_for_retained_children() {
    let state_file = write_completion_fixture();
    let log_dir = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let mut config = config_with_state_file_and_queue(&state_file);
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.log_dir = Some(log_dir.display().to_string());
    let app = router(AppState::new(config));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "spawnchild",
            "parent_session_id": "em001",
            "prompt": "spawn child prompt",
            "name": "spawn-child",
            "provider": "claude"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "spawnchild");

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let stop_notify = state["retained_stop_notify_states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["session_id"] == "spawnchild")
        .unwrap();
    assert_eq!(stop_notify["sender_session_id"], "em001");
    assert_eq!(stop_notify["sender_name"], "em");
    assert_eq!(stop_notify["delay_seconds"], 8);
    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let db_stop_notify: (String, String, i64) = queue_conn
        .query_row(
            "SELECT sender_session_id, sender_name, delay_seconds FROM rust_stop_notify_states WHERE session_id = 'spawnchild'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(db_stop_notify, ("em001".to_owned(), "em".to_owned(), 8));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "spawnfork",
            "parent_session_id": "em001",
            "prompt": "spawn codex fork prompt",
            "name": "spawn-fork",
            "provider": "codex-fork"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "spawnfork");

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert!(state["retained_stop_notify_states"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["session_id"] != "spawnfork"));
}

#[tokio::test]
async fn fixture_core_writes_preserve_concurrent_session_updates() {
    let state_file = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(unique_temp_path().display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let first = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "rustcore1",
            "name": "rust-core-1",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    );
    let second = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "rustcore2",
            "name": "rust-core-2",
            "working_dir": "/repo",
            "provider": "claude"
        }),
    );

    let ((status1, _payload1), (status2, _payload2)) = join(first, second).await;
    assert_eq!(status1, StatusCode::OK);
    assert_eq!(status2, StatusCode::OK);

    let (status, payload) = get_json(app, "/sessions").await;
    assert_eq!(status, StatusCode::OK);
    let mut ids = payload["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, vec!["rustcore1", "rustcore2"]);
}

#[tokio::test]
async fn fixture_core_logs_do_not_collide_for_sanitized_session_ids() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, _first) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "ab",
            "name": "rust-core-ab",
            "initial_message": "first prompt only"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _second) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "a/b",
            "name": "rust-core-slash",
            "initial_message": "second prompt only"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let log_files = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["log_file"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(log_files.len(), 2);
    assert_ne!(log_files[0], log_files[1]);

    let (status, payload) = get_json(app, "/sessions/ab/output?lines=10").await;
    assert_eq!(status, StatusCode::OK);
    let output = payload["output"].as_str().unwrap();
    assert!(output.contains("first prompt only"));
    assert!(!output.contains("second prompt only"));
}

#[tokio::test]
async fn runtime_core_lifecycle_uses_tmux_backend_when_enabled() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db_path_for_state_file(&state_file)
                .display()
                .to_string(),
        },
        rust_core: RustCoreConfig {
            runtime_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            tmux_socket_name: Some(tmux_socket.clone()),
            runtime_command: Some(
                r#"/bin/sh -lc 'while IFS= read -r line; do printf "argv:%s\nids:%s:%s:%s\nruntime:%s\n" "$*" "$SESSION_MANAGER_ID" "$CLAUDE_SESSION_MANAGER_ID" "$ENABLE_TOOL_SEARCH" "$line"; done' runtime-sh"#
                    .to_owned(),
            ),
            runtime_prompt_mode: Some("stdin".to_owned()),
            runtime_start_settle_ms: Some(100),
            send_keys_settle_ms: Some(10.0),
            send_keys_settle_max_ms: Some(50.0),
            send_keys_max_chunk_chars: Some(128),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimecore",
            "name": "runtime-core",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "initial runtime prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "runtimecore");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["tmux_socket_name"], tmux_socket);
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();

    let payload =
        wait_for_output_contains(app.clone(), "runtimecore", "runtime:initial runtime prompt")
            .await;
    assert_eq!(payload["session_id"], "runtimecore");
    wait_for_output_contains(
        app.clone(),
        "runtimecore",
        "ids:runtimecore:runtimecore:false",
    )
    .await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimecore/input",
        json!({
            "text": "second runtime message",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);

    wait_for_output_contains(app.clone(), "runtimecore", "runtime:second runtime message").await;
    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let delivered_at: Option<String> = queue_conn
        .query_row(
            "SELECT delivered_at FROM message_queue WHERE target_session_id = 'runtimecore' AND text = 'second runtime message'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(delivered_at.is_some());

    let long_runtime_message = format!("{}chunked-runtime-tail", "x".repeat(500));
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimecore/input",
        json!({
            "text": long_runtime_message,
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(app.clone(), "runtimecore", "chunked-runtime-tail").await;

    assert!(tmux_enter_copy_mode(&tmux_socket, &tmux_session));
    assert_eq!(tmux_pane_in_mode(&tmux_socket, &tmux_session), Some(1));
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimecore/input",
        json!({
            "text": "copy mode runtime message",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app.clone(),
        "runtimecore",
        "runtime:copy mode runtime message",
    )
    .await;

    let (status, payload) = post_json(app.clone(), "/sessions/runtimecore/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = get_json(app.clone(), "/sessions/runtimecore").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "stopped");
    assert!(!tmux_session_exists(&tmux_socket, &tmux_session));

    let (status, payload) =
        post_json(app.clone(), "/sessions/runtimecore/restore", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "runtimecore");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["completion_status"], Value::Null);
    assert!(tmux_session_exists(&tmux_socket, &tmux_session));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimecore/input",
        json!({
            "text": "restored runtime message",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(app, "runtimecore", "runtime:restored runtime message").await;
}

#[tokio::test]
async fn runtime_core_delivers_sm_send_metadata_rows() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-side-effects-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimesmsend",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "sm send initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimesender",
            "name": "runtime-sender",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "sender initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_output_contains(app.clone(), "runtimesmsend", "runtime:sm send initial").await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimesmsend/input",
        json!({
            "text": "ordinary sm send metadata delivered",
            "sender_session_id": "runtimesender",
            "delivery_mode": "sequential",
            "from_sm_send": true,
            "timeout_seconds": 60,
            "notify_on_stop": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    assert_eq!(payload["status"], "running");
    wait_for_output_contains(
        app.clone(),
        "runtimesmsend",
        "runtime:ordinary sm send metadata delivered",
    )
    .await;

    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let pending: (
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        i64,
        Option<i64>,
        i64,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = queue_conn
        .query_row(
            r#"
            SELECT text, sender_session_id, sender_name, from_sm_send, timeout_at,
                   notify_on_delivery, notify_after_seconds, notify_on_stop,
                   remind_soft_threshold, remind_hard_threshold,
                   remind_cancel_on_reply_session_id, parent_session_id,
                   response_relay_source, delivered_at
            FROM message_queue
            WHERE target_session_id = 'runtimesmsend'
            "#,
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .unwrap();
    let timeout_at = pending.4.clone();
    assert!(timeout_at.is_some());
    assert_eq!(
        pending.0,
        "[Input from: runtime-sender (runtimes) via sm send]\nordinary sm send metadata delivered"
    );
    assert_eq!(pending.1.as_deref(), Some("runtimesender"));
    assert_eq!(pending.2.as_deref(), Some("runtime-sender"));
    assert_eq!(pending.3, 1);
    assert_eq!(pending.4, timeout_at);
    assert_eq!(pending.5, 0);
    assert_eq!(pending.6, None);
    assert_eq!(pending.7, 0);
    assert_eq!(pending.8, None);
    assert_eq!(pending.9, None);
    assert_eq!(pending.10, None);
    assert_eq!(pending.11, None);
    assert_eq!(pending.12.as_deref(), Some("sm-send"));
    assert!(pending.13.is_some());
}

#[tokio::test]
async fn runtime_core_input_batch_delivers_to_multiple_sessions() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-input-batch-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    for (id, prompt) in [
        ("runtimebatcha", "runtime batch a initial"),
        ("runtimebatchb", "runtime batch b initial"),
    ] {
        let (status, payload) = post_json(
            app.clone(),
            "/sessions",
            json!({
                "id": id,
                "working_dir": working_dir.display().to_string(),
                "provider": "claude",
                "initial_message": prompt
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["id"], id);
        wait_for_output_contains(app.clone(), id, &format!("runtime:{prompt}")).await;
    }

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/input-batch",
        json!({
            "recipients": ["runtimebatcha,runtimebatchb", "missingruntimebatch"],
            "text": "runtime batch payload",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["requested_count"], 3);
    assert_eq!(payload["success_count"], 2);
    assert_eq!(payload["failure_count"], 1);
    let results = payload["results"].as_array().unwrap();
    assert_eq!(results[0]["identifier"], "runtimebatcha");
    assert_eq!(results[0]["status"], "delivered");
    assert_eq!(results[0]["delivery_kind"], "session");
    assert_eq!(results[1]["identifier"], "runtimebatchb");
    assert_eq!(results[1]["status"], "delivered");
    assert_eq!(results[2]["identifier"], "missingruntimebatch");
    assert_eq!(results[2]["status"], "failed");
    assert_eq!(
        results[2]["detail"],
        "Session 'missingruntimebatch' not found"
    );
    wait_for_output_contains(
        app.clone(),
        "runtimebatcha",
        "runtime:runtime batch payload",
    )
    .await;
    wait_for_output_contains(app, "runtimebatchb", "runtime:runtime batch payload").await;
}

#[tokio::test]
async fn runtime_core_materializes_send_delivery_side_effects() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-side-effects-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeem",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "em side effect initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimechild",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "parent_session_id": "runtimeem",
            "initial_message": "child side effect initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut raw_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let em = raw_state["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|session| session["id"] == "runtimeem")
        .unwrap();
    em["is_em"] = Value::Bool(true);
    em["friendly_name"] = Value::String("runtime-em".to_owned());
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&raw_state).unwrap(),
    )
    .unwrap();
    wait_for_output_contains(
        app.clone(),
        "runtimechild",
        "runtime:child side effect initial",
    )
    .await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimechild/input",
        json!({
            "text": "side effect delivery should be materialized",
            "delivery_mode": "sequential",
            "sender_session_id": "runtimeem",
            "from_sm_send": true,
            "notify_on_delivery": true,
            "notify_after_seconds": 1,
            "notify_on_stop": true,
            "remind_soft_threshold": 30,
            "remind_hard_threshold": 45,
            "remind_cancel_on_reply_session_id": "runtimeem",
            "parent_session_id": "runtimeem"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app.clone(),
        "runtimechild",
        "runtime:side effect delivery should be materialized",
    )
    .await;
    wait_for_output_contains(
        app.clone(),
        "runtimeem",
        "[sm] Message delivered to runtimechild",
    )
    .await;

    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let original: (
        Option<String>,
        i64,
        Option<i64>,
        i64,
        Option<i64>,
        Option<String>,
    ) = queue_conn
        .query_row(
            r#"
                SELECT delivered_at, notify_on_delivery, notify_after_seconds,
                       notify_on_stop, remind_soft_threshold, parent_session_id
                FROM message_queue
                WHERE target_session_id = 'runtimechild'
                "#,
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert!(original.0.is_some());
    assert_eq!(original.1, 1);
    assert_eq!(original.2, Some(1));
    assert_eq!(original.3, 1);
    assert_eq!(original.4, Some(30));
    assert_eq!(original.5.as_deref(), Some("runtimeem"));

    let delivery_notification_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = 'runtimeem' AND text LIKE '[sm] Message delivered%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delivery_notification_count, 1);

    tokio::time::sleep(Duration::from_millis(1200)).await;
    wait_for_output_contains(
        app.clone(),
        "runtimeem",
        "[sm] Reminder: 1s since your message to runtimechild was delivered",
    )
    .await;
    let followup_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = 'runtimeem' AND text LIKE '[sm] Reminder: 1s since%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(followup_count, 1);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimechild/input",
        json!({
            "text": "stale sender still reaches target",
            "delivery_mode": "sequential",
            "sender_session_id": "missing-runtime-sender",
            "from_sm_send": true,
            "notify_on_delivery": true,
            "notify_after_seconds": 1,
            "notify_on_stop": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app.clone(),
        "runtimechild",
        "runtime:stale sender still reaches target",
    )
    .await;
    let stale_sender_row: (Option<String>, i64, Option<i64>, i64) = queue_conn
        .query_row(
            r#"
            SELECT sender_session_id, notify_on_delivery, notify_after_seconds, notify_on_stop
            FROM message_queue
            WHERE target_session_id = 'runtimechild'
              AND text = 'stale sender still reaches target'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(stale_sender_row, (None, 0, None, 0));
    let stale_sender_notification_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = 'missing-runtime-sender'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stale_sender_notification_count, 0);

    let remind: (i64, i64, Option<String>, i64) = queue_conn
        .query_row(
            "SELECT soft_threshold_seconds, hard_threshold_seconds, cancel_on_reply_session_id, is_active FROM remind_registrations WHERE target_session_id = 'runtimechild'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(remind, (30, 45, Some("runtimeem".to_owned()), 1));
    let parent_wake: (String, i64, i64) = queue_conn
        .query_row(
            "SELECT parent_session_id, period_seconds, is_active FROM parent_wake_registrations WHERE child_session_id = 'runtimechild'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(parent_wake, ("runtimeem".to_owned(), 600, 1));
    let stop_notify: (String, String, i64) = queue_conn
        .query_row(
            "SELECT sender_session_id, sender_name, delay_seconds FROM rust_stop_notify_states WHERE session_id = 'runtimechild'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        stop_notify,
        ("runtimeem".to_owned(), "runtime-em".to_owned(), 0)
    );

    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(
        raw_state["retained_stop_notify_states"][0]["sender_session_id"],
        "runtimeem"
    );
    assert_eq!(
        raw_state["retained_remind_registrations"][0]["target_session_id"],
        "runtimechild"
    );
    assert_eq!(
        raw_state["retained_parent_wake_registrations"][0]["parent_session_id"],
        "runtimeem"
    );
}

#[tokio::test]
async fn runtime_core_retire_delivers_stop_notify_side_effects() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-stop-notify-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimestopem",
            "name": "runtime-stop-em",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "stop em initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimestopchild",
            "name": "runtime-stop-child",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "parent_session_id": "runtimestopem",
            "initial_message": "stop child initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut raw_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let em = raw_state["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|session| session["id"] == "runtimestopem")
        .unwrap();
    em["is_em"] = Value::Bool(true);
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&raw_state).unwrap(),
    )
    .unwrap();
    wait_for_output_contains(
        app.clone(),
        "runtimestopchild",
        "runtime:stop child initial",
    )
    .await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimestopchild/notify-on-stop",
        json!({
            "sender_session_id": "runtimestopem",
            "requester_session_id": "runtimestopem",
            "delay_seconds": 0
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "ok");

    let (status, payload) =
        post_json(app.clone(), "/sessions/runtimestopchild/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    wait_for_output_contains(
        app.clone(),
        "runtimestopem",
        "[sm] runtime-stop-child (runtimes) completed (Stop hook fired)",
    )
    .await;

    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let notification: (i64, String, String) = queue_conn
        .query_row(
            r#"
            SELECT delivered_at IS NOT NULL, delivery_mode, COALESCE(message_category, '')
            FROM message_queue
            WHERE target_session_id = 'runtimestopem'
              AND text LIKE '[sm] runtime-stop-child%'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        notification,
        (1, "important".to_owned(), "stop_notify".to_owned())
    );
    let remaining_stop_notify_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM rust_stop_notify_states WHERE session_id = 'runtimestopchild'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining_stop_notify_count, 0);
    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert!(raw_state["retained_stop_notify_states"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["session_id"] != "runtimestopchild"));

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimegoneem",
            "name": "runtime-gone-em",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "gone em initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeghostchild",
            "name": "runtime-ghost-child",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "parent_session_id": "runtimegoneem",
            "initial_message": "ghost child initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut raw_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let sessions = raw_state["sessions"].as_array_mut().unwrap();
    let gone_em = sessions
        .iter_mut()
        .find(|session| session["id"] == "runtimegoneem")
        .unwrap();
    gone_em["is_em"] = Value::Bool(true);
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&raw_state).unwrap(),
    )
    .unwrap();
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimeghostchild/notify-on-stop",
        json!({
            "sender_session_id": "runtimegoneem",
            "requester_session_id": "runtimegoneem",
            "delay_seconds": 0
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "ok");

    let mut raw_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    raw_state["sessions"]
        .as_array_mut()
        .unwrap()
        .retain(|session| session["id"] != "runtimegoneem");
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&raw_state).unwrap(),
    )
    .unwrap();

    let (status, payload) =
        post_json(app.clone(), "/sessions/runtimeghostchild/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    let stale_sender_notification_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = 'runtimegoneem'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stale_sender_notification_count, 0);
    let remaining_ghost_stop_notify_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM rust_stop_notify_states WHERE session_id = 'runtimeghostchild'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining_ghost_stop_notify_count, 0);
}

#[tokio::test]
async fn runtime_core_task_complete_wakes_parent_runtime() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-task-complete-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimetaskparent",
            "name": "runtime-task-parent",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "task parent initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimetaskchild",
            "name": "runtime-task-child",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "parent_session_id": "runtimetaskparent",
            "initial_message": "task child initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_output_contains(
        app.clone(),
        "runtimetaskchild",
        "runtime:task child initial",
    )
    .await;
    let mut raw_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    raw_state["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|session| session["id"] == "runtimetaskparent")
        .unwrap()["agent_task_completed_at"] = json!("2026-06-09T00:01:00Z");
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&raw_state).unwrap(),
    )
    .unwrap();

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimetaskchild/task-complete",
        json!({ "requester_session_id": "runtimetaskchild" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "completed");
    assert_eq!(payload["em_notified"], true);

    let notification =
        "[sm task-complete] agent runtimetaskchild(runtime-task-child) completed its task.";
    wait_for_output_contains(app.clone(), "runtimetaskparent", notification).await;

    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let queued: (i64, String, Option<String>) = queue_conn
        .query_row(
            r#"
            SELECT delivered_at IS NOT NULL, delivery_mode, message_category
            FROM message_queue
            WHERE target_session_id = 'runtimetaskparent'
              AND message_category = 'task_complete'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        queued,
        (1, "important".to_owned(), Some("task_complete".to_owned()))
    );
    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let parent = raw_state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "runtimetaskparent")
        .unwrap();
    assert_eq!(parent["agent_task_completed_at"], Value::Null);
    let child = raw_state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "runtimetaskchild")
        .unwrap();
    assert!(child["agent_task_completed_at"].is_string());
    assert_eq!(
        raw_state["retained_pending_messages"][0]["text"],
        notification
    );

    let remote_parent_log = unique_temp_path();
    let child_log = unique_temp_path();
    let remote_state = json!({
        "sessions": [
            {
                "id": "remotetaskparent",
                "name": "remote-task-parent",
                "friendly_name": "remote-parent",
                "working_dir": working_dir.display().to_string(),
                "tmux_session": "remote-task-parent",
                "log_file": remote_parent_log.display().to_string(),
                "status": "running",
                "node": "macbook",
                "created_at": "2026-06-09T00:00:00Z",
                "last_activity": "2026-06-09T00:00:00Z",
                "agent_task_completed_at": "2026-06-09T00:01:00Z"
            },
            {
                "id": "remotetaskchild",
                "name": "remote-task-child",
                "friendly_name": "remote-child",
                "working_dir": working_dir.display().to_string(),
                "tmux_session": "remote-task-child",
                "log_file": child_log.display().to_string(),
                "status": "running",
                "node": "primary",
                "parent_session_id": "remotetaskparent",
                "created_at": "2026-06-09T00:00:00Z",
                "last_activity": "2026-06-09T00:00:00Z"
            }
        ],
        "retained_pending_messages": [],
        "retained_remind_registrations": [],
        "retained_parent_wake_registrations": [],
        "retained_stop_notify_states": []
    });
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&remote_state).unwrap(),
    )
    .unwrap();
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/remotetaskchild/task-complete",
        json!({ "requester_session_id": "remotetaskchild" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "completed");
    assert_eq!(payload["em_notified"], true);
    let remote_queued: (i64, String, Option<String>) = queue_conn
        .query_row(
            r#"
            SELECT delivered_at IS NOT NULL, delivery_mode, message_category
            FROM message_queue
            WHERE target_session_id = 'remotetaskparent'
              AND message_category = 'task_complete'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        remote_queued,
        (0, "important".to_owned(), Some("task_complete".to_owned()))
    );
    let remote_raw_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let remote_parent = remote_raw_state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "remotetaskparent")
        .unwrap();
    assert!(remote_parent["agent_task_completed_at"].is_string());

    let missing_parent_child_log = unique_temp_path();
    let missing_parent_state = json!({
        "sessions": [
            {
                "id": "missingparentchild",
                "name": "missing-parent-child",
                "friendly_name": "missing-parent-child",
                "working_dir": working_dir.display().to_string(),
                "tmux_session": "missing-parent-child",
                "log_file": missing_parent_child_log.display().to_string(),
                "status": "running",
                "node": "primary",
                "parent_session_id": "deletedparent",
                "created_at": "2026-06-09T00:00:00Z",
                "last_activity": "2026-06-09T00:00:00Z"
            }
        ],
        "retained_pending_messages": [],
        "retained_remind_registrations": [],
        "retained_parent_wake_registrations": [],
        "retained_stop_notify_states": []
    });
    fs::write(
        &state_file,
        serde_json::to_string_pretty(&missing_parent_state).unwrap(),
    )
    .unwrap();
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/missingparentchild/task-complete",
        json!({ "requester_session_id": "missingparentchild" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "completed");
    assert_eq!(payload["em_notified"], true);
    let missing_parent_queued: (i64, String, Option<String>) = queue_conn
        .query_row(
            r#"
            SELECT delivered_at IS NOT NULL, delivery_mode, message_category
            FROM message_queue
            WHERE target_session_id = 'deletedparent'
              AND message_category = 'task_complete'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        missing_parent_queued,
        (0, "important".to_owned(), Some("task_complete".to_owned()))
    );
    let missing_parent_raw_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let missing_parent_child = missing_parent_raw_state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "missingparentchild")
        .unwrap();
    assert!(missing_parent_child["agent_task_completed_at"].is_string());
}

#[tokio::test]
async fn runtime_core_priority_sends_bypass_sequential_queue_backlog() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-priority-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimepriority",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "priority initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_output_contains(app.clone(), "runtimepriority", "runtime:priority initial").await;

    let queue = RetainedQueueStore::new(queue_db_path.clone());
    for index in 0..12 {
        queue
            .enqueue_message(
                "runtimepriority",
                &format!("stale sequential backlog {index}"),
                "sequential",
                None,
            )
            .unwrap();
    }

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimepriority/input",
        json!({
            "text": "sequential after large backlog",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app.clone(),
        "runtimepriority",
        "runtime:sequential after large backlog",
    )
    .await;

    for index in 0..12 {
        queue
            .enqueue_message(
                "runtimepriority",
                &format!("second stale sequential backlog {index}"),
                "sequential",
                None,
            )
            .unwrap();
    }

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimepriority/input",
        json!({
            "text": "important priority message",
            "delivery_mode": "important"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app.clone(),
        "runtimepriority",
        "runtime:important priority message",
    )
    .await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimepriority/input",
        json!({
            "text": "urgent priority message",
            "delivery_mode": "urgent"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(app.clone(), "runtimepriority", "urgent priority message").await;

    let queue_conn = Connection::open(&queue_db_path).unwrap();
    let delivered_priority_count: i64 = queue_conn
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM message_queue
            WHERE target_session_id = 'runtimepriority'
                AND text IN (
                    'sequential after large backlog',
                    'important priority message',
                    'urgent priority message'
                )
                AND delivered_at IS NOT NULL
            "#,
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delivered_priority_count, 3);
    let delivered_backlog_count: i64 = queue_conn
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM message_queue
            WHERE target_session_id = 'runtimepriority'
                AND text LIKE 'second stale sequential backlog%'
                AND delivered_at IS NOT NULL
            "#,
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delivered_backlog_count, 0);
}

#[tokio::test]
async fn runtime_core_replays_retained_urgent_rows_with_interrupt_semantics() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-retained-urgent-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeurgentreplay",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "urgent replay initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_output_contains(
        app.clone(),
        "runtimeurgentreplay",
        "runtime:urgent replay initial",
    )
    .await;

    let queue = RetainedQueueStore::new(queue_db_path.clone());
    let urgent_id = queue
        .enqueue_message(
            "runtimeurgentreplay",
            "retained urgent replay",
            "urgent",
            None,
        )
        .unwrap();

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimeurgentreplay/input",
        json!({
            "text": "sequential trigger after retained urgent",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    let payload = wait_for_output_contains(
        app.clone(),
        "runtimeurgentreplay",
        "sequential trigger after retained urgent",
    )
    .await;
    let output = payload["output"].as_str().unwrap_or_default();
    assert!(
        output.contains("runtime:\u{2}\u{1b}retained urgent replay"),
        "retained urgent row was not replayed through the interrupt path: {output:?}",
    );
    assert!(queue.message_delivered(&urgent_id).unwrap());
}

#[tokio::test]
async fn runtime_core_handoff_records_without_interrupting_active_turn() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-handoff-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket);
    let app = runtime_app_with_command(
        &state_file,
        &log_dir,
        _tmux_guard.0.as_str(),
        r#"/bin/sh -lc 'printf ">\n"; while IFS= read -r line; do printf "runtime:%s\n>\n" "$line"; done' runtime-sh"#,
    );

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimehandoff",
            "name": "runtime-handoff",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "initial handoff prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_output_contains(
        app.clone(),
        "runtimehandoff",
        "runtime:initial handoff prompt",
    )
    .await;

    let handoff_path = unique_temp_path();
    fs::write(&handoff_path, "handoff body").unwrap();
    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimehandoff/handoff",
        json!({
            "requester_session_id": "runtimehandoff",
            "file_path": handoff_path.display().to_string()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "recorded");

    let (status, output) = get_json(app.clone(), "/sessions/runtimehandoff/output?lines=20").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!output["output"]
        .as_str()
        .unwrap()
        .contains("continue from where you left off"));

    let (status, payload) = get_json(app, "/sessions/runtimehandoff").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["last_handoff_path"], Value::Null);
    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let runtime_handoff = raw_state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "runtimehandoff")
        .unwrap();
    assert_eq!(
        runtime_handoff["pending_handoff_path"],
        handoff_path.display().to_string()
    );
}

#[tokio::test]
async fn runtime_core_spawn_endpoint_uses_tmux_and_parent_fields() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let parent_dir = unique_temp_path();
    fs::create_dir_all(&parent_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-spawn-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, parent_payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeparent",
            "name": "runtime-parent",
            "working_dir": parent_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "parent runtime prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parent_tmux_session = parent_payload["tmux_session"].as_str().unwrap().to_owned();
    wait_for_output_contains(
        app.clone(),
        "runtimeparent",
        "runtime:parent runtime prompt",
    )
    .await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "runtimechild",
            "parent_session_id": "runtimeparent",
            "prompt": "spawn endpoint runtime prompt",
            "name": "runtime-child",
            "model": "opus",
            "wait": 5
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["session_id"], "runtimechild");
    assert_eq!(payload["friendly_name"], "runtime-child");
    assert_eq!(payload["parent_session_id"], "runtimeparent");
    assert_eq!(payload["working_dir"], parent_dir.display().to_string());
    assert_eq!(payload["node"], "primary");
    assert_eq!(payload["provider"], "claude");
    assert_eq!(payload["model"], "opus");
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    assert!(tmux_session.starts_with("sm-rust-claude-runtimechild-"));

    wait_for_output_contains(
        app.clone(),
        "runtimechild",
        "runtime:spawn endpoint runtime prompt",
    )
    .await;
    wait_for_output_contains(app.clone(), "runtimechild", "argv:--model opus").await;

    let (status, payload) = get_json(app.clone(), "/sessions/runtimechild").await;
    assert_eq!(status, StatusCode::OK);
    assert!(payload.get("model").is_none());
    let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    let runtime_child = state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "runtimechild")
        .unwrap();
    assert_eq!(runtime_child["model"], "opus");

    let (status, payload) = post_json(app.clone(), "/sessions/runtimechild/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&tmux_socket, &tmux_session));

    wait_for_output_contains(
        app.clone(),
        "runtimeparent",
        "Child runtime-child (runtimec) completed: Session exited",
    )
    .await;

    let (status, payload) = post_json(app.clone(), "/sessions/runtimeparent/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&tmux_socket, &parent_tmux_session));
}

#[tokio::test]
async fn runtime_core_spawn_wait_detects_naturally_exited_tmux_child() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let parent_dir = unique_temp_path();
    fs::create_dir_all(&parent_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-spawn-exit-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app_with_command(
        &state_file,
        &log_dir,
        &tmux_socket,
        r#"/bin/sh -lc 'while IFS= read -r line; do printf "runtime:%s\n" "$line"; case "$line" in *natural-child-prompt*) exit 0;; esac; done' runtime-sh"#,
    );

    let (status, parent_payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "naturalparent",
            "name": "natural-parent",
            "working_dir": parent_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "parent prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parent_tmux_session = parent_payload["tmux_session"].as_str().unwrap().to_owned();
    wait_for_output_contains(app.clone(), "naturalparent", "runtime:parent prompt").await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "naturalchild",
            "parent_session_id": "naturalparent",
            "prompt": "natural-child-prompt",
            "name": "natural-child",
            "wait": 10
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let child_tmux_session = payload["tmux_session"].as_str().unwrap();

    wait_for_output_contains(
        app.clone(),
        "naturalparent",
        "Child natural-child (naturalc) completed: Session exited",
    )
    .await;
    assert!(!tmux_session_exists(&tmux_socket, child_tmux_session));

    let (status, payload) = post_json(app.clone(), "/sessions/naturalparent/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&tmux_socket, &parent_tmux_session));
}

#[tokio::test]
async fn runtime_core_spawn_wait_uses_runtime_output_as_activity() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let parent_dir = unique_temp_path();
    fs::create_dir_all(&parent_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-spawn-active-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app_with_command(
        &state_file,
        &log_dir,
        &tmux_socket,
        r#"/bin/sh -lc 'while IFS= read -r line; do case "$line" in *active-child-prompt*) for i in 1 2 3 4 5 6 7 8; do printf "runtime:heartbeat-%s\n" "$i"; sleep 0.2; done; exit 0;; *) printf "runtime:%s\n" "$line";; esac; done' runtime-sh"#,
    );

    let (status, parent_payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "activeparent",
            "name": "active-parent",
            "working_dir": parent_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "parent prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parent_tmux_session = parent_payload["tmux_session"].as_str().unwrap().to_owned();
    wait_for_output_contains(app.clone(), "activeparent", "runtime:parent prompt").await;

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/spawn",
        json!({
            "id": "activechild",
            "parent_session_id": "activeparent",
            "prompt": "active-child-prompt",
            "name": "active-child",
            "wait": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let child_tmux_session = payload["tmux_session"].as_str().unwrap();

    let parent_output = wait_for_output_contains(
        app.clone(),
        "activeparent",
        "Child active-child (activech) completed: Session exited",
    )
    .await;
    assert!(!parent_output["output"]
        .as_str()
        .unwrap_or_default()
        .contains("Idle for"));
    assert!(!tmux_session_exists(&tmux_socket, child_tmux_session));

    let (status, payload) = post_json(app.clone(), "/sessions/activeparent/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&tmux_socket, &parent_tmux_session));
}

#[tokio::test]
async fn runtime_core_send_and_retire_use_persisted_tmux_socket() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let socket_a = format!(
        "sm-rust-test-a-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let socket_b = format!("{socket_a}-b");
    let _guard_a = TestTmuxSocket(socket_a.clone());
    let _guard_b = TestTmuxSocket(socket_b.clone());
    let app_a = runtime_app(&state_file, &log_dir, &socket_a);
    let app_b = runtime_app(&state_file, &log_dir, &socket_b);

    let (status, payload) = post_json(
        app_a.clone(),
        "/sessions",
        json!({
            "id": "runtimepersisted",
            "name": "runtime-persisted",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "persisted socket initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["tmux_socket_name"], socket_a);
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    wait_for_output_contains(
        app_a.clone(),
        "runtimepersisted",
        "runtime:persisted socket initial",
    )
    .await;

    let (status, payload) = post_json(
        app_b.clone(),
        "/sessions/runtimepersisted/input",
        json!({ "text": "sent through changed config socket" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], true);
    wait_for_output_contains(
        app_b.clone(),
        "runtimepersisted",
        "runtime:sent through changed config socket",
    )
    .await;

    let (status, payload) = post_json(app_b, "/sessions/runtimepersisted/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&socket_a, &tmux_session));
}

#[tokio::test]
async fn runtime_core_expands_bare_home_working_dir_for_tmux() {
    if !tmux_available() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let tmux_socket = format!(
        "sm-rust-test-home-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimehome",
            "name": "runtime-home",
            "working_dir": "~",
            "provider": "claude",
            "initial_message": "home runtime prompt"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["working_dir"], "~");
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    let home_path = PathBuf::from(home);
    assert_eq!(
        tmux_pane_current_path(&tmux_socket, &tmux_session).as_deref(),
        Some(home_path.as_path())
    );

    let (status, payload) = post_json(app.clone(), "/sessions/runtimehome/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = post_json(app, "/sessions/runtimehome/restore", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], "runtimehome");
    assert_eq!(payload["status"], "running");
    assert_eq!(
        tmux_pane_current_path(&tmux_socket, &tmux_session).as_deref(),
        Some(home_path.as_path())
    );
}

#[tokio::test]
async fn runtime_core_marks_missing_tmux_stopped_on_send_and_retire() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-missing-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, _payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimemissingsender",
            "name": "runtime-missing-sender",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "missing sender initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimemissingsend",
            "name": "runtime-missing-send",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "missing send initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    assert!(tmux_kill_session(&tmux_socket, &tmux_session));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimemissingsend/input",
        json!({
            "text": "after external kill",
            "delivery_mode": "sequential",
            "sender_session_id": "runtimemissingsender",
            "from_sm_send": true,
            "notify_on_delivery": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], false);
    assert_eq!(payload["status"], "stopped");
    let queue_conn = Connection::open(queue_db_path_for_state_file(&state_file)).unwrap();
    let pending: (String, Option<String>, i64) = queue_conn
        .query_row(
            r#"
            SELECT text, delivered_at, notify_on_delivery
            FROM message_queue
            WHERE target_session_id = 'runtimemissingsend'
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        pending,
        (
            "[Input from: runtime-missing-sender (runtimem) via sm send]\nafter external kill"
                .to_owned(),
            None,
            1
        )
    );
    let sender_notification_count: i64 = queue_conn
        .query_row(
            "SELECT COUNT(*) FROM message_queue WHERE target_session_id = 'runtimemissingsender' AND text LIKE '[sm] Message delivered%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sender_notification_count, 0);

    let (status, payload) = get_json(app.clone(), "/sessions/runtimemissingsend").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "stopped");
    assert!(payload["stopped_at"].as_str().is_some());

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimemissingretire",
            "name": "runtime-missing-retire",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "missing retire initial"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    assert!(tmux_kill_session(&tmux_socket, &tmux_session));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/runtimemissingretire/kill",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");

    let (status, payload) = get_json(app, "/sessions/runtimemissingretire").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "stopped");
    assert!(payload["stopped_at"].as_str().is_some());
}

#[tokio::test]
async fn runtime_core_does_not_queue_sends_to_already_stopped_sessions() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "runtimestopped",
                    "name": "runtime-stopped",
                    "working_dir": "/repo",
                    "tmux_session": "claude-runtimestopped",
                    "node": "primary",
                    "provider": "claude",
                    "log_file": "/tmp/runtimestopped.log",
                    "status": "stopped",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let log_dir = unique_temp_path();
    let app = runtime_app(&state_file, &log_dir, "sm-rust-test-stopped-send");

    let (status, payload) = post_json(
        app,
        "/sessions/runtimestopped/input",
        json!({
            "text": "do not retain for stopped session",
            "delivery_mode": "sequential"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["delivered"], false);
    assert_eq!(payload["status"], "stopped");
    assert!(!queue_db_path_for_state_file(&state_file).exists());
}

#[tokio::test]
async fn runtime_core_rejects_unsupported_provider() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-provider-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = runtime_app(&state_file, &log_dir, &tmux_socket);

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimecodex",
            "working_dir": working_dir.display().to_string(),
            "provider": "codex",
            "initial_message": "codex should not launch claude"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support provider codex" })
    );
    let (status, _) = get_json(app, "/sessions/runtimecodex").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn runtime_core_lifecycle_uses_codex_fork_launch_config() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let queue_db_path = queue_db_path_for_state_file(&state_file);
    let log_dir = unique_short_temp_dir("smrf");
    let working_dir = unique_temp_path();
    fs::create_dir_all(&log_dir).unwrap();
    fs::create_dir_all(&working_dir).unwrap();
    let codex_bin_dir = working_dir.join("codex fork bin");
    fs::create_dir_all(&codex_bin_dir).unwrap();
    let codex_binary = codex_bin_dir.join("fake-codex-fork");
    fs::write(
        &codex_binary,
        r#"#!/bin/sh
event_stream=""
previous=""
for arg in "$@"; do
  if [ "$previous" = "--event-stream" ]; then
    event_stream="$arg"
  fi
  previous="$arg"
done
if [ -n "$event_stream" ]; then
  printf '{"event_type":"thread/started","payload":{"thread":{"id":"provider-thread-123"}}}\n' >> "$event_stream"
  printf '{"event_type":"turn_started","payload":{}}\n' >> "$event_stream"
fi
sleep 0.2
printf 'argv:%s\n' "$*"
printf 'ids:%s:%s:%s\n' "$SESSION_MANAGER_ID" "$CLAUDE_SESSION_MANAGER_ID" "$ENABLE_TOOL_SEARCH"
if [ -n "$event_stream" ]; then
  printf '{"event_type":"turn_complete","payload":{}}\n' >> "$event_stream"
fi
while true; do sleep 1; done
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&codex_binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&codex_binary, permissions).unwrap();
    }
    let (event_path, control_path) = codex_fork_artifact_paths(&log_dir, "runtimefork");
    fs::write(&event_path, "stale event").unwrap();
    fs::write(&control_path, "stale control").unwrap();
    let tmux_socket = format!(
        "sm-rust-test-codex-fork-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db_path.display().to_string(),
        },
        codex_fork: CodexForkLaunchConfig {
            command: codex_binary.display().to_string(),
            args: vec![
                "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                "-c".to_owned(),
                "check_for_update_on_startup=false".to_owned(),
            ],
            default_model: Some("gpt-default".to_owned()),
            event_schema_version: 7,
        },
        rust_core: RustCoreConfig {
            runtime_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            tmux_socket_name: Some(tmux_socket.clone()),
            runtime_prompt_mode: Some("argv".to_owned()),
            runtime_start_settle_ms: Some(100),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimefork",
            "working_dir": working_dir.display().to_string(),
            "provider": "codex-fork",
            "initial_message": "hello from codex fork"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["provider"], "codex-fork");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["tmux_socket_name"], tmux_socket);
    assert_eq!(payload["provider_resume_id"], "provider-thread-123");
    assert!(payload["tmux_session"]
        .as_str()
        .unwrap()
        .contains("codex-fork"));
    let event_text = fs::read_to_string(&event_path).unwrap();
    assert!(!event_text.contains("stale event"));
    assert!(event_text.contains("provider-thread-123"));
    assert!(
        !control_path.exists(),
        "Rust runtime should remove stale codex-fork control sockets before launch"
    );

    let output = wait_for_output_contains(app.clone(), "runtimefork", "--event-stream").await;
    let output_text = output["output"].as_str().unwrap();
    assert!(output_text.contains("--dangerously-bypass-approvals-and-sandbox"));
    assert!(output_text.contains("-c check_for_update_on_startup=false"));
    assert!(output_text.contains(&event_path.display().to_string()));
    assert!(output_text.contains("--event-schema-version 7"));
    assert!(output_text.contains(&control_path.display().to_string()));
    assert!(output_text.contains("--model gpt-default"));
    assert!(output_text.contains("-- hello from codex fork"));
    assert!(output_text.contains("ids:runtimefork:runtimefork:false"));
    let mut lifecycle_status = String::new();
    for _ in 0..30 {
        let (_, session) = get_json(app.clone(), "/sessions/runtimefork").await;
        lifecycle_status = session["status"].as_str().unwrap().to_owned();
        if session["status"] == "idle" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(lifecycle_status, "idle");

    #[cfg(unix)]
    {
        let control_requests =
            spawn_codex_fork_control_socket(control_path.clone(), "epoch-runtimefork");
        let (status, payload) = post_json(
            app.clone(),
            "/sessions/runtimefork/input",
            json!({
                "text": "control socket message",
                "delivery_mode": "direct"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["delivered"], true);

        let epoch_request = control_requests
            .recv_timeout(Duration::from_secs(2))
            .expect("codex-fork get_epoch request");
        assert_eq!(epoch_request["command"], "get_epoch");
        let submit_request = control_requests
            .recv_timeout(Duration::from_secs(2))
            .expect("codex-fork submit_message request");
        assert_eq!(submit_request["command"], "submit_message");
        assert_eq!(submit_request["expected_epoch"], "epoch-runtimefork");
        assert_eq!(submit_request["message"], "control socket message");

        let stale_requests = spawn_codex_fork_stale_epoch_control_socket(
            control_path.clone(),
            "epoch-runtimefork-stale",
            "epoch-runtimefork-fresh",
        );
        let (status, payload) = post_json(
            app.clone(),
            "/sessions/runtimefork/input",
            json!({
                "text": "control socket retry message",
                "delivery_mode": "direct"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["delivered"], true);

        let first_epoch_request = stale_requests
            .recv_timeout(Duration::from_secs(2))
            .expect("first codex-fork get_epoch request");
        assert_eq!(first_epoch_request["command"], "get_epoch");
        let stale_submit_request = stale_requests
            .recv_timeout(Duration::from_secs(2))
            .expect("stale codex-fork submit_message request");
        assert_eq!(stale_submit_request["command"], "submit_message");
        assert_eq!(
            stale_submit_request["expected_epoch"],
            "epoch-runtimefork-stale"
        );
        let refresh_epoch_request = stale_requests
            .recv_timeout(Duration::from_secs(2))
            .expect("refreshed codex-fork get_epoch request");
        assert_eq!(refresh_epoch_request["command"], "get_epoch");
        let retry_submit_request = stale_requests
            .recv_timeout(Duration::from_secs(2))
            .expect("retried codex-fork submit_message request");
        assert_eq!(retry_submit_request["command"], "submit_message");
        assert_eq!(
            retry_submit_request["expected_epoch"],
            "epoch-runtimefork-fresh"
        );
        assert_eq!(
            retry_submit_request["message"],
            "control socket retry message"
        );
    }

    let tmux_session = payload["tmux_session"].as_str().unwrap().to_owned();
    let (status, payload) = post_json(app.clone(), "/sessions/runtimefork/kill", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "killed");
    assert!(!tmux_session_exists(&tmux_socket, &tmux_session));
    fs::write(&event_path, "stale event after stop").unwrap();
    let _ = fs::remove_file(&control_path);
    fs::write(&control_path, "stale control after stop").unwrap();

    let mut state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    state["sessions"][0]["provider_resume_id"] = Value::Null;
    fs::write(&state_file, state.to_string()).unwrap();

    let (status, payload) =
        post_json(app.clone(), "/sessions/runtimefork/restore", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        payload,
        json!({ "detail": "Cannot restore codex-fork session without provider_resume_id" })
    );
    assert!(!tmux_session_exists(&tmux_socket, &tmux_session));

    let mut state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    state["sessions"][0]["provider_resume_id"] = json!("provider-thread-123");
    fs::write(&state_file, state.to_string()).unwrap();

    let (status, payload) =
        post_json(app.clone(), "/sessions/runtimefork/restore", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["provider"], "codex-fork");
    assert_eq!(payload["status"], "running");
    assert!(tmux_session_exists(&tmux_socket, &tmux_session));
    let restore_event_text = wait_for_file_contains(&event_path, "provider-thread-123").await;
    assert!(!restore_event_text.contains("stale event after stop"));
    assert!(
        !control_path.exists(),
        "Rust runtime should remove stale codex-fork control sockets before restore"
    );
    let mut restored_text = String::new();
    for _ in 0..30 {
        let output = wait_for_output_contains(app.clone(), "runtimefork", "--event-stream").await;
        restored_text = output["output"].as_str().unwrap().to_owned();
        if restored_text
            .matches("ids:runtimefork:runtimefork:false")
            .count()
            >= 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        restored_text
            .matches("ids:runtimefork:runtimefork:false")
            .count()
            >= 2,
        "restore should launch a second codex-fork process; output={restored_text:?}"
    );
    assert!(restored_text.contains("--dangerously-bypass-approvals-and-sandbox"));
    assert!(restored_text.contains("resume provider-thread-123"));
    assert!(restored_text.contains("--event-schema-version 7"));
    assert!(restored_text.contains("--model gpt-default"));
}

#[tokio::test]
async fn runtime_core_codex_fork_sanitizes_artifact_paths() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&log_dir).unwrap();
    fs::create_dir_all(&working_dir).unwrap();
    let codex_binary = working_dir.join("fake-codex-fork");
    fs::write(
        &codex_binary,
        r#"#!/bin/sh
event_stream=""
previous=""
for arg in "$@"; do
  if [ "$previous" = "--event-stream" ]; then
    event_stream="$arg"
  fi
  previous="$arg"
done
if [ -n "$event_stream" ]; then
  printf '{"event_type":"thread/started","payload":{"thread_id":"safe-provider-thread"}}\n' >> "$event_stream"
fi
sleep 0.2
printf 'argv:%s\n' "$*"
while true; do sleep 1; done
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&codex_binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&codex_binary, permissions).unwrap();
    }
    let raw_session_id = "../runtimefork";
    let (event_path, control_path) = codex_fork_artifact_paths(&log_dir, raw_session_id);
    let unsafe_event_path = log_dir.join("../runtimefork.codex-fork.events.jsonl");
    let tmux_socket = format!(
        "sm-rust-test-codex-fork-safe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        codex_fork: CodexForkLaunchConfig {
            command: codex_binary.display().to_string(),
            args: vec![
                "-c".to_owned(),
                "check_for_update_on_startup=false".to_owned(),
            ],
            default_model: None,
            event_schema_version: 2,
        },
        rust_core: RustCoreConfig {
            runtime_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            tmux_socket_name: Some(tmux_socket),
            runtime_prompt_mode: Some("argv".to_owned()),
            runtime_start_settle_ms: Some(100),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app,
        "/sessions",
        json!({
            "id": raw_session_id,
            "working_dir": working_dir.display().to_string(),
            "provider": "codex-fork"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["id"], raw_session_id);
    let log_file = core_log_file_path(&log_dir, raw_session_id);
    let output_text = wait_for_file_contains(&log_file, "--event-stream").await;
    assert!(output_text.contains(&event_path.display().to_string()));
    assert!(output_text.contains(&control_path.display().to_string()));
    assert!(
        !output_text.contains("../runtimefork.codex-fork.events.jsonl"),
        "codex-fork artifact path must not include caller-controlled path separators"
    );
    assert!(
        !unsafe_event_path.exists(),
        "codex-fork launch must not create artifacts outside the configured log directory"
    );
}

#[tokio::test]
async fn runtime_core_rejects_remote_node_create_before_local_tmux_launch() {
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let app = runtime_app(&state_file, &log_dir, "sm-rust-test-remote-create");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeremote",
            "working_dir": ".",
            "provider": "claude",
            "node": "macbook",
            "initial_message": "remote node should not launch locally"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support remote node macbook" })
    );
    let (status, _) = get_json(app, "/sessions/runtimeremote").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn runtime_core_rejects_child_create_that_inherits_remote_parent_node() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "remoteparent",
                    "name": "claude-remoteparent",
                    "working_dir": "/remote/repo",
                    "tmux_session": "claude-remoteparent",
                    "node": "macbook",
                    "provider": "claude",
                    "log_file": "/tmp/remoteparent.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let log_dir = unique_temp_path();
    let app = runtime_app(&state_file, &log_dir, "sm-rust-test-remote-child");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "remotechild",
            "parent_session_id": "remoteparent",
            "provider": "claude",
            "initial_message": "child should inherit remote node and reject"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support remote node macbook" })
    );
    let (status, _) = get_json(app, "/sessions/remotechild").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn runtime_core_rejects_remote_node_send_and_retire_without_mutating_state() {
    let state_file = unique_temp_path();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "remoteruntime",
                    "name": "claude-remoteruntime",
                    "working_dir": "/repo",
                    "tmux_session": "claude-remoteruntime",
                    "node": "macbook",
                    "provider": "claude",
                    "log_file": "/tmp/remoteruntime.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let log_dir = unique_temp_path();
    let app = runtime_app(&state_file, &log_dir, "sm-rust-test-remote-existing");

    let (status, payload) = post_json(
        app.clone(),
        "/sessions/remoteruntime/input",
        json!({ "text": "do not send to local tmux" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support remote node macbook" })
    );

    let (status, payload) = post_json(app.clone(), "/sessions/remoteruntime/kill", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        payload,
        json!({ "detail": "Rust runtime does not support remote node macbook" })
    );

    let (status, payload) = get_json(app, "/sessions/remoteruntime").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["node"], "macbook");
    assert_eq!(payload["status"], "running");
    assert_eq!(payload["stopped_at"], Value::Null);
}

#[tokio::test]
async fn runtime_core_fails_create_when_stdin_prompt_cannot_be_delivered() {
    if !tmux_available() {
        return;
    }
    let state_file = unique_temp_path();
    let log_dir = unique_temp_path();
    let working_dir = unique_temp_path();
    fs::create_dir_all(&working_dir).unwrap();
    let tmux_socket = format!(
        "sm-rust-test-exit-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _tmux_guard = TestTmuxSocket(tmux_socket.clone());
    let app = router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db_path_for_state_file(&state_file)
                .display()
                .to_string(),
        },
        rust_core: RustCoreConfig {
            runtime_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            tmux_socket_name: Some(tmux_socket.clone()),
            runtime_command: Some("/bin/sh -lc 'exit 0'".to_owned()),
            runtime_prompt_mode: Some("stdin".to_owned()),
            runtime_start_settle_ms: Some(100),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }));

    let (status, payload) = post_json(
        app.clone(),
        "/sessions",
        json!({
            "id": "runtimeexited",
            "working_dir": working_dir.display().to_string(),
            "provider": "claude",
            "initial_message": "cannot be delivered"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        payload,
        json!({ "detail": "tmux session exited before initial prompt could be delivered" })
    );
    let (status, _) = get_json(app, "/sessions/runtimeexited").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn session_output_tails_large_log_file_from_end() {
    let state_file = unique_temp_path();
    let log_file = unique_temp_path();
    fs::write(
        &log_file,
        format!(
            "{}\nlast retained line\nfinal retained line\n",
            "x".repeat(2 * 1024 * 1024)
        ),
    )
    .unwrap();
    fs::write(
        &state_file,
        json!({
            "sessions": [
                {
                    "id": "largeout",
                    "name": "claude-largeout",
                    "working_dir": "/repo",
                    "tmux_session": "claude-largeout",
                    "node": "primary",
                    "provider": "claude",
                    "log_file": log_file.display().to_string(),
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions/largeout/output?lines=2").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["output"],
        "last retained line\nfinal retained line\n"
    );
}

#[tokio::test]
async fn sessions_missing_state_file_returns_empty_list() {
    let state_file = unique_temp_path();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app, "/sessions").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload, json!({ "sessions": [] }));
}

#[tokio::test]
async fn sessions_reject_public_host_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));

    let (status, payload) = get_json_with_host(app, "/sessions", "sm.example.com").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        payload,
        json!({
            "detail": "Authentication required",
            "login_url": "/auth/google/login?next=%2Fsessions"
        })
    );
}

#[tokio::test]
async fn sessions_allow_public_device_bearer_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));
    let token = device_access_token("session-cookie-secret", "rajesh@example.com", "Rajesh");

    let (status, payload) = get_json_with_host_and_headers(
        app,
        "/sessions",
        "sm.example.com",
        &[("authorization", format!("Bearer {token}"))],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn client_sessions_allow_public_browser_cookie_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));
    let cookie = starlette_session_cookie(
        "session-cookie-secret",
        json!({
            "google_authenticated": true,
            "google_email": "rajesh@example.com",
            "google_name": "Rajesh"
        }),
    );

    let (status, payload) = get_json_with_host_and_headers(
        app,
        "/client/sessions",
        "sm.example.com",
        &[("cookie", format!("sm_auth={cookie}"))],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn auth_session_reports_device_bearer_when_google_auth_enabled() {
    let token = device_access_token("session-cookie-secret", "rajesh@example.com", "Rajesh");
    let app = router(AppState::new(config_with_state_file_and_auth(
        &unique_temp_path(),
    )));

    let (status, payload) = get_json_with_host_and_headers(
        app,
        "/auth/session",
        "sm.example.com",
        &[("authorization", format!("Bearer {token}"))],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload,
        json!({
            "enabled": true,
            "authenticated": true,
            "bypass": false,
            "email": "rajesh@example.com",
            "name": "Rajesh",
            "auth_type": "device_bearer"
        })
    );
}

#[tokio::test]
async fn client_sessions_allow_local_bypass_when_google_auth_enabled() {
    let state_file = write_session_fixture();
    let app = router(AppState::new(config_with_state_file_and_auth(&state_file)));

    let (status, payload) = get_json_with_host_and_peer(
        app,
        "/client/sessions",
        "localhost:8421",
        Some(SocketAddr::from(([127, 0, 0, 1], 49152))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["sessions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn sessions_project_top_level_registry_and_adoption_state() {
    let state_file = write_registry_fixture();
    let app = router(AppState::new(config_with_state_file(&state_file)));

    let (status, payload) = get_json(app.clone(), "/sessions").await;

    assert_eq!(status, StatusCode::OK);
    let sessions = payload["sessions"].as_array().unwrap();
    let maintainer = sessions
        .iter()
        .find(|session| session["id"] == "em123456")
        .unwrap();
    let child = sessions
        .iter()
        .find(|session| session["id"] == "child001")
        .unwrap();
    assert_eq!(maintainer["aliases"], json!(["maintainer"]));
    assert_eq!(maintainer["is_maintainer"], true);
    assert_eq!(maintainer["friendly_name"], "maintainer");
    assert_eq!(child["aliases"], json!(["reviewer"]));
    assert_eq!(
        child["pending_adoption_proposals"],
        json!([
            {
                "id": "proposal1",
                "proposer_session_id": "em123456",
                "proposer_name": "maintainer",
                "target_session_id": "child001",
                "created_at": "2026-06-01T00:03:00",
                "status": "pending",
                "decided_at": null
            }
        ])
    );

    let (status, payload) = get_json(app, "/registry").await;
    assert_eq!(status, StatusCode::OK);
    let roles = payload["registrations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(roles, vec!["maintainer", "reviewer"]);
    let raw_state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert!(raw_state["agent_registrations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["role"] == "maintainer" && entry["session_id"] == "em123456"));
}

fn config_with_state_file(state_file: &PathBuf) -> AppConfig {
    AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        ..AppConfig::default()
    }
}

fn config_with_state_file_and_mobile_terminal(state_file: &PathBuf) -> AppConfig {
    let mut config = config_with_state_file(state_file);
    config.external_access.public_http_host = Some("sm.example.com".to_owned());
    config.mobile_terminal.enabled = true;
    config.mobile_terminal.allowed_users.insert(
        "local_bypass".to_owned(),
        MobileTerminalUserConfig {
            interactive_shell_access: true,
            registered_device_keys: vec![MobileTerminalDeviceKeyConfig {
                id: "android-1".to_owned(),
                public_key: "-----BEGIN PUBLIC KEY-----\nfixture\n-----END PUBLIC KEY-----"
                    .to_owned(),
                enabled: true,
            }],
            ..MobileTerminalUserConfig::default()
        },
    );
    config
}

fn config_with_state_file_and_email(
    state_file: &PathBuf,
    bridge_config: &PathBuf,
    fixture_writes_enabled: bool,
) -> AppConfig {
    AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        email: EmailConfig {
            bridge_config: bridge_config.display().to_string(),
        },
        rust_core: RustCoreConfig {
            fixture_writes_enabled,
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }
}

fn config_with_state_file_and_queue(state_file: &PathBuf) -> AppConfig {
    AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db_path_for_state_file(state_file)
                .display()
                .to_string(),
        },
        ..AppConfig::default()
    }
}

fn queue_db_path_for_state_file(state_file: &PathBuf) -> PathBuf {
    state_file.with_extension("message_queue.db")
}

fn create_codex_review_request_fixture_db(path: &PathBuf) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE codex_review_request_registrations (
            id TEXT PRIMARY KEY,
            repo TEXT NOT NULL,
            pr_number INTEGER NOT NULL,
            requester_session_id TEXT,
            notify_session_id TEXT NOT NULL,
            steer TEXT,
            requested_at TIMESTAMP NOT NULL,
            latest_request_comment_id INTEGER,
            latest_request_comment_url TEXT,
            latest_request_posted_at TIMESTAMP,
            attempt_count INTEGER NOT NULL,
            next_retry_at TIMESTAMP,
            poll_interval_seconds INTEGER NOT NULL,
            retry_interval_seconds INTEGER NOT NULL,
            pickup_detected_at TIMESTAMP,
            pickup_source TEXT,
            review_landed_at TIMESTAMP,
            review_source TEXT,
            review_comment_id INTEGER,
            review_url TEXT,
            last_polled_at TIMESTAMP,
            last_error TEXT,
            state TEXT NOT NULL,
            is_active INTEGER DEFAULT 1
        );
        "#,
    )
    .unwrap();
    conn.execute(
        r#"
        INSERT INTO codex_review_request_registrations
            (id, repo, pr_number, requester_session_id, notify_session_id, steer,
             requested_at, latest_request_comment_id, latest_request_comment_url,
             latest_request_posted_at, attempt_count, next_retry_at,
             poll_interval_seconds, retry_interval_seconds, pickup_detected_at,
             pickup_source, review_landed_at, review_source, review_comment_id,
             review_url, last_polled_at, last_error, state, is_active)
        VALUES
            ('active-old', 'rajeshgoli/session-manager', 830, 'requester1', 'notify1', 'focus nodes',
             '2026-06-01T00:00:00', 111, 'https://example.com/comment/111',
             '2026-06-01T00:00:01', 2, '2026-06-01T00:10:00',
             30, 600, '2026-06-01T00:02:00',
             'issue_comment', '2026-06-01T00:03:00', 'pull_review', 222,
             'https://example.com/review/222', '2026-06-01T00:04:00', NULL, 'completed', 1),
            ('inactive', 'rajeshgoli/session-manager', 831, NULL, 'notify1', NULL,
             '2026-06-01T00:01:00', NULL, NULL,
             NULL, 1, NULL,
             30, 600, NULL,
             NULL, NULL, NULL, NULL,
             NULL, NULL, 'cancelled', 'cancelled', 0),
            ('active-new', 'rajeshgoli/other', 7, NULL, 'notify2', NULL,
             '2026-06-01T00:02:00', NULL, NULL,
             NULL, 1, NULL,
             45, 900, NULL,
             NULL, '2026-06-01T00:03:30', 'pull_review', 'R_kw123',
             'https://example.com/review/R_kw123', NULL, NULL, 'completed', 1)
        "#,
        [],
    )
    .unwrap();
}

fn create_active_codex_review_request_fixture_db(
    path: &PathBuf,
    request_id: &str,
    notify_session_id: &str,
    poll_interval_seconds: i64,
) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE codex_review_request_registrations (
            id TEXT PRIMARY KEY,
            repo TEXT NOT NULL,
            pr_number INTEGER NOT NULL,
            requester_session_id TEXT,
            notify_session_id TEXT NOT NULL,
            steer TEXT,
            requested_at TIMESTAMP NOT NULL,
            latest_request_comment_id INTEGER,
            latest_request_comment_url TEXT,
            latest_request_posted_at TIMESTAMP,
            attempt_count INTEGER NOT NULL,
            next_retry_at TIMESTAMP,
            poll_interval_seconds INTEGER NOT NULL,
            retry_interval_seconds INTEGER NOT NULL,
            pickup_detected_at TIMESTAMP,
            pickup_source TEXT,
            review_landed_at TIMESTAMP,
            review_source TEXT,
            review_comment_id INTEGER,
            review_url TEXT,
            last_polled_at TIMESTAMP,
            last_error TEXT,
            state TEXT NOT NULL,
            is_active INTEGER DEFAULT 1
        );
        "#,
    )
    .unwrap();
    conn.execute(
        r#"
        INSERT INTO codex_review_request_registrations
            (id, repo, pr_number, requester_session_id, notify_session_id, steer,
             requested_at, latest_request_comment_id, latest_request_comment_url,
             latest_request_posted_at, attempt_count, next_retry_at,
             poll_interval_seconds, retry_interval_seconds, pickup_detected_at,
             pickup_source, review_landed_at, review_source, review_comment_id,
             review_url, last_polled_at, last_error, state, is_active)
        VALUES
            (?1, 'rajeshgoli/session-manager', 971, 'requester1', ?2, 'recover watchers',
             '2026-06-14T02:30:00Z', 4701290334,
             'https://github.com/rajeshgoli/session-manager/pull/971#issuecomment-4701290334',
             '2026-06-14T02:30:00Z', 1, '2026-06-14T02:45:00Z',
             ?3, 600, NULL,
             NULL, NULL, NULL, NULL,
             NULL, NULL, NULL, 'active', 1)
        "#,
        rusqlite::params![request_id, notify_session_id, poll_interval_seconds],
    )
    .unwrap();
}

fn create_queue_jobs_fixture_db(state_dir: &PathBuf) {
    fs::create_dir_all(state_dir).unwrap();
    let conn = Connection::open(state_dir.join("queue_runner.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE queue_jobs (
            id TEXT PRIMARY KEY,
            type TEXT NOT NULL,
            label TEXT NOT NULL,
            requester_session_id TEXT,
            notify_session_id TEXT NOT NULL,
            cwd TEXT NOT NULL,
            argv_json TEXT,
            script_path TEXT,
            env_json TEXT NOT NULL,
            timeout_seconds INTEGER NOT NULL,
            state TEXT NOT NULL,
            holding_reason TEXT,
            queued_at TEXT NOT NULL,
            started_at TEXT,
            finished_at TEXT,
            pid INTEGER,
            process_group_id INTEGER,
            exit_code INTEGER,
            log_path TEXT,
            exit_code_path TEXT,
            wrapper_path TEXT,
            queued_notified_at TEXT,
            started_notified_at TEXT,
            completion_notified_at TEXT
        );
        "#,
    )
    .unwrap();
    conn.execute(
        r#"
        INSERT INTO queue_jobs
            (id, type, label, requester_session_id, notify_session_id, cwd,
             argv_json, script_path, env_json, timeout_seconds, state,
             holding_reason, queued_at, started_at, finished_at, pid,
             process_group_id, exit_code, log_path, exit_code_path, wrapper_path,
             queued_notified_at, started_notified_at, completion_notified_at)
        VALUES
            ('job-pending', 'tests', 'cargo tests', 'requester1', 'notify1', '/repo',
             '["cargo","test"]', NULL, '{}', 900, 'pending',
             'memory', '2026-06-01T00:00:00', NULL, NULL, NULL,
             NULL, NULL, '/tmp/job-pending.log', NULL, NULL,
             NULL, NULL, NULL),
            ('job-running', 'perf', 'perf run', NULL, 'notify2', '/repo/perf',
             NULL, '/tmp/run-perf.sh', '{}', 2700, 'running',
             NULL, '2026-06-01T00:01:00', '2026-06-01T00:01:30', NULL, 4242,
             4242, NULL, '/tmp/job-running.log', NULL, NULL,
             NULL, NULL, NULL),
            ('job-succeeded', 'tests', 'done tests', 'requester1', 'notify1', '/repo',
             '["true"]', NULL, '{}', 900, 'succeeded',
             NULL, '2026-06-01T00:02:00', '2026-06-01T00:02:10', '2026-06-01T00:02:20', 4343,
             4343, 0, '/tmp/job-succeeded.log', NULL, NULL,
             NULL, NULL, NULL),
            ('job-failed', 'background', 'failed background', 'missing-requester', 'notify2', '/repo/bg',
             NULL, '/tmp/fail.sh', '{}', 3600, 'failed',
             NULL, '2026-06-01T00:03:00', '2026-06-01T00:03:10', '2026-06-01T00:03:20', 4444,
             4444, 2, '/tmp/job-failed.log', NULL, NULL,
             NULL, NULL, NULL)
        "#,
        [],
    )
    .unwrap();
}

fn create_tool_usage_fixture_db(path: &PathBuf) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE tool_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
            session_id TEXT,
            hook_type TEXT NOT NULL,
            tool_name TEXT NOT NULL
        );
        INSERT INTO tool_usage (timestamp, session_id, hook_type, tool_name)
        VALUES
            ('2026-06-01 00:00:00', 'run12345', 'PreToolUse', 'Write'),
            ('2026-06-01 00:01:00', 'run12345', 'PreToolUse', 'Read'),
            ('2026-06-01 00:02:00', 'run12345', 'PreToolUse', 'Bash'),
            ('2026-06-01 00:03:00', 'run12345', 'PostToolUse', 'Bash'),
            ('2026-06-01 00:04:00', 'oldstate', 'PreToolUse', 'Glob');
        "#,
    )
    .unwrap();
}

fn create_codex_events_fixture_db(path: &PathBuf) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE codex_session_events (
            session_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            timestamp TEXT NOT NULL,
            event_type TEXT NOT NULL,
            turn_id TEXT,
            payload_preview_json TEXT,
            PRIMARY KEY (session_id, seq)
        );
        INSERT INTO codex_session_events
            (session_id, seq, timestamp, event_type, turn_id, payload_preview_json)
        VALUES
            ('codexapp1', 1, '2026-06-01T00:01:00+00:00', 'turn_started', 'turn-a', '{"message":"start"}'),
            ('codexapp1', 2, '2026-06-01T00:02:00+00:00', 'item_started', 'turn-a', '{"tool_name":"Read"}'),
            ('codexapp1', 3, '2026-06-01T00:03:00+00:00', 'item_completed', 'turn-a', 'not-json'),
            ('codexapp1', 4, '2026-06-01T00:04:00+00:00', 'turn_completed', NULL, NULL),
            ('othercodex', 1, '2026-06-01T00:05:00+00:00', 'turn_started', 'turn-b', '{"ignored":true}');
        "#,
    )
    .unwrap();
}

fn create_codex_events_retention_gap_fixture_db(path: &PathBuf) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE codex_session_events (
            session_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            timestamp TEXT NOT NULL,
            event_type TEXT NOT NULL,
            turn_id TEXT,
            payload_preview_json TEXT,
            PRIMARY KEY (session_id, seq)
        );
        INSERT INTO codex_session_events
            (session_id, seq, timestamp, event_type, turn_id, payload_preview_json)
        VALUES
            ('retainedgap', 4, '2026-06-01T00:04:00+00:00', 'item_completed', 'turn-r', '{"tool_name":"Bash"}'),
            ('retainedgap', 5, '2026-06-01T00:05:00+00:00', 'turn_completed', 'turn-r', '{"ok":true}');
        "#,
    )
    .unwrap();
}

fn create_codex_pending_requests_fixture_db(path: &PathBuf) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE codex_pending_requests (
            request_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            process_generation TEXT NOT NULL,
            rpc_request_id INTEGER,
            thread_id TEXT,
            turn_id TEXT,
            item_id TEXT,
            request_type TEXT NOT NULL,
            request_method TEXT NOT NULL,
            requested_at TEXT NOT NULL,
            expires_at TEXT,
            status TEXT NOT NULL,
            request_payload_json TEXT,
            resolved_payload_json TEXT,
            resolved_at TEXT,
            resolution_source TEXT,
            error_code TEXT,
            error_message TEXT
        );
        INSERT INTO codex_pending_requests
            (request_id, session_id, process_generation, rpc_request_id, thread_id,
             turn_id, item_id, request_type, request_method, requested_at, expires_at,
             status, request_payload_json, resolved_payload_json, resolved_at,
             resolution_source, error_code, error_message)
        VALUES
            ('req-pending', 'codexpending', 'gen-a', 7, 'thread-a',
             'turn-a', 'item-a', 'request_approval', 'item/commandExecution/requestApproval',
             '2026-06-01T00:00:00+00:00', '2026-06-01T00:05:00+00:00',
             'pending', '{"turnId":"turn-a"}', NULL, NULL,
             NULL, NULL, NULL),
            ('req-orphaned', 'codexpending', 'gen-old', 8, 'thread-b',
             'turn-b', 'item-b', 'request_user_input', 'item/tool/requestUserInput',
             '2026-06-01T00:01:00+00:00', '2026-06-01T00:06:00+00:00',
             'orphaned', '{"turnId":"turn-b"}', NULL, '2026-06-01T00:02:00+00:00',
             'policy', 'server_restarted', 'server restarted before request resolution'),
            ('req-resolved', 'codexpending', 'gen-a', 9, NULL,
             NULL, NULL, 'request_approval', 'item/fileChange/requestApproval',
             '2026-06-01T00:02:00+00:00', '2026-06-01T00:07:00+00:00',
             'resolved', '{}', '{"decision":"accept"}', '2026-06-01T00:03:00+00:00',
             'api', NULL, NULL),
            ('req-expired', 'codexpending', 'gen-a', 10, NULL,
             NULL, NULL, 'request_user_input', 'item/tool/requestUserInput',
             '2026-06-01T00:03:00+00:00', '2026-06-01T00:08:00+00:00',
             'expired', '{}', NULL, '2026-06-01T00:04:00+00:00',
             'policy', 'request_expired', 'request expired before explicit response'),
            ('req-other', 'othercodex', 'gen-a', 11, NULL,
             NULL, NULL, 'request_approval', 'item/commandExecution/requestApproval',
             '2026-06-01T00:04:00+00:00', '2026-06-01T00:09:00+00:00',
             'pending', '{}', NULL, NULL,
             NULL, NULL, NULL);
        "#,
    )
    .unwrap();
}

fn create_codex_observability_fixture_db(path: &PathBuf) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE codex_tool_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            raw_payload_json TEXT,
            created_at TEXT NOT NULL
        );
        INSERT INTO codex_tool_events (session_id, raw_payload_json, created_at)
        VALUES
            ('forktools', '{"tool_name":"Read"}', '2026-06-01T00:00:00+00:00'),
            ('forktools', '{"event_type":"no-tool"}', '2026-06-01T00:01:00+00:00'),
            ('forktools', 'not-json', '2026-06-01T00:02:00+00:00'),
            ('forktools', '{"tool_name":"Bash"}', '2026-06-01T00:03:00+00:00'),
            ('otherfork', '{"tool_name":"Ignore"}', '2026-06-01T00:04:00+00:00'),
            ('forktools', NULL, '2026-06-01T00:04:30+00:00'),
            ('forktools', '{"tool_name":"Edit"}', '2026-06-01T00:05:00+00:00');
        "#,
    )
    .unwrap();
}

fn create_codex_activity_fixture_db(path: &PathBuf) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE codex_tool_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            thread_id TEXT,
            turn_id TEXT,
            item_id TEXT,
            request_id TEXT,
            event_type TEXT NOT NULL,
            item_type TEXT,
            phase TEXT,
            command TEXT,
            cwd TEXT,
            exit_code INTEGER,
            file_path TEXT,
            diff_summary TEXT,
            approval_decision TEXT,
            latency_ms INTEGER,
            final_status TEXT,
            error_code TEXT,
            error_message TEXT,
            raw_payload_json TEXT,
            provider TEXT NOT NULL DEFAULT 'codex-app',
            schema_version INTEGER,
            created_at TEXT NOT NULL
        );
        INSERT INTO codex_tool_events
            (session_id, thread_id, turn_id, item_id, request_id, event_type,
             item_type, phase, command, cwd, exit_code, file_path, diff_summary,
             approval_decision, latency_ms, final_status, error_code, error_message,
             raw_payload_json, provider, schema_version, created_at)
        VALUES
            ('codexproj', 'thread-1', 'turn-1', 'item-1', NULL, 'started',
             'commandExecution', 'pre', 'pytest -q', '/repo', NULL, NULL, NULL,
             NULL, NULL, NULL, NULL, NULL,
             '{}', 'codex-app', 2, '2026-02-21T10:00:00+00:00'),
            ('codexproj', 'thread-1', 'turn-1', 'item-1', NULL, 'failed',
             'commandExecution', 'post', 'pytest -q', '/repo', 1, NULL, NULL,
             NULL, 5000, 'failed', 'nonzero', 'non-zero exit',
             '{}', 'codex-app', 2, '2026-02-21T10:00:05+00:00'),
            ('codexproj', 'thread-2', 'turn-2', 'item-2', 'req-1', 'approval_decision',
             'fileChange', 'post', NULL, '/repo', NULL, 'src/main.py', NULL,
             'accept', NULL, NULL, NULL, NULL,
             '{}', 'codex-app', 2, '2026-02-21T10:01:00+00:00'),
            ('otherproj', 'thread-x', 'turn-x', 'item-x', NULL, 'started',
             'commandExecution', 'pre', 'ignored', '/repo', NULL, NULL, NULL,
             NULL, NULL, NULL, NULL, NULL,
             '{}', 'codex-app', 2, '2026-02-21T10:02:00+00:00');
        "#,
    )
    .unwrap();
}

fn rfc3339(value: time::OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

fn log_timestamp(value: time::OffsetDateTime) -> String {
    format!(
        "{},000",
        value
            .format(time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second]"
            ))
            .unwrap()
    )
}

fn assert_python_naive_timestamp(value: &str) {
    assert!(
        value.contains('T'),
        "timestamp should use ISO separator: {value}"
    );
    assert!(!value.ends_with('Z'), "timestamp should be naive: {value}");
    assert!(
        value.len() > 10,
        "timestamp should include a date and time: {value}"
    );
    assert!(
        !value[10..].contains('+') && !value[10..].contains('-'),
        "timestamp should not include an offset: {value}"
    );
}

fn config_with_state_file_and_auth(state_file: &PathBuf) -> AppConfig {
    AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        google_auth: GoogleAuthConfig {
            enabled: true,
            public_host: Some("sm.example.com".to_owned()),
            client_id: Some("web-client-id".to_owned()),
            android_client_id: Some("android-client-id".to_owned()),
            client_secret: Some("web-client-secret".to_owned()),
            redirect_uri: Some("https://sm.example.com/auth/google/callback".to_owned()),
            allowlist_emails: vec!["rajesh@example.com".to_owned()],
            session_cookie_secret: Some("session-cookie-secret".to_owned()),
            ..GoogleAuthConfig::default()
        },
        ..AppConfig::default()
    }
}

fn write_session_fixture() -> PathBuf {
    let path = unique_temp_path();
    let log_file = unique_temp_path();
    fs::write(
        &log_file,
        "fixture log line 1\nfixture log line 2\nfixture log line 3\n",
    )
    .unwrap();
    fs::write(
        &path,
        json!({
            "sessions": [
                {
                    "id": "run12345",
                    "name": "claude-run12345",
                    "working_dir": "/repo",
                    "tmux_session": "claude-run12345",
                    "tmux_socket_name": null,
                    "node": "primary",
                    "provider": "claude",
                    "log_file": log_file.display().to_string(),
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "friendly_name": "Runner",
                    "friendly_name_is_explicit": false,
                    "friendly_name_updated_at_ns": 10,
                    "native_title": "Runner Native",
                    "native_title_updated_at_ns": 20,
                    "current_task": "Working",
                    "tokens_used": 42,
                    "context_monitor_enabled": true
                },
                {
                    "id": "oldstate",
                    "name": "claude-oldstate",
                    "working_dir": "/repo",
                    "tmux_session": "claude-oldstate",
                    "log_file": "/tmp/oldstate.log",
                    "status": "waiting_permission",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                },
                {
                    "id": "stop1234",
                    "name": "claude-stop1234",
                    "working_dir": "/repo",
                    "tmux_session": "claude-stop1234",
                    "log_file": "/tmp/stop1234.log",
                    "status": "stopped",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn write_codex_app_session_fixture(session_id: &str) -> PathBuf {
    let path = unique_temp_path();
    fs::write(
        &path,
        json!({
            "sessions": [
                {
                    "id": session_id,
                    "name": format!("codex-app-{session_id}"),
                    "working_dir": "/repo",
                    "tmux_session": format!("codex-app-{session_id}"),
                    "tmux_socket_name": null,
                    "node": "primary",
                    "provider": "codex-app",
                    "log_file": format!("/tmp/{session_id}.log"),
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn write_completion_fixture() -> PathBuf {
    let path = unique_temp_path();
    fs::write(
        &path,
        json!({
            "sessions": [
                {
                    "id": "em001",
                    "name": "claude-em001",
                    "working_dir": "/repo",
                    "tmux_session": "claude-em001",
                    "log_file": "/tmp/em001.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "friendly_name": "em",
                    "is_em": true
                },
                {
                    "id": "em002",
                    "name": "claude-em002",
                    "working_dir": "/repo",
                    "tmux_session": "claude-em002",
                    "log_file": "/tmp/em002.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "friendly_name": "other-em",
                    "is_em": true
                },
                {
                    "id": "child001",
                    "name": "claude-child001",
                    "working_dir": "/repo",
                    "tmux_session": "claude-child001",
                    "log_file": "/tmp/child001.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "friendly_name": "worker-1",
                    "parent_session_id": "em001",
                    "agent_task_completed_at": null
                },
                {
                    "id": "fork001",
                    "name": "codex-fork-fork001",
                    "working_dir": "/repo",
                    "tmux_session": "codex-fork-fork001",
                    "log_file": "/tmp/fork001.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z",
                    "provider": "codex-fork",
                    "parent_session_id": "em001"
                }
            ],
            "retained_remind_registrations": [
                {
                    "session_id": "child001",
                    "is_active": true
                }
            ],
            "retained_parent_wake_registrations": [
                {
                    "child_session_id": "child001",
                    "parent_session_id": "em001",
                    "period_seconds": 600,
                    "is_active": true
                }
            ],
            "retained_pending_messages": [],
            "retained_stop_notify_states": []
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn write_registry_fixture() -> PathBuf {
    let path = unique_temp_path();
    fs::write(
        &path,
        json!({
            "sessions": [
                {
                    "id": "em123456",
                    "name": "claude-em123456",
                    "working_dir": "/repo",
                    "tmux_session": "claude-em123456",
                    "log_file": "/tmp/em123456.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "friendly_name": "em-ops",
                    "is_em": true
                },
                {
                    "id": "child001",
                    "name": "claude-child001",
                    "working_dir": "/repo",
                    "tmux_session": "claude-child001",
                    "log_file": "/tmp/child001.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                },
                {
                    "id": "deadrole",
                    "name": "claude-deadrole",
                    "working_dir": "/repo",
                    "tmux_session": "claude-deadrole",
                    "log_file": "/tmp/deadrole.log",
                    "status": "stopped",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00",
                    "stopped_at": "2026-06-01T00:02:00"
                }
            ],
            "maintainer_session_id": "em123456",
            "agent_registrations": [
                {
                    "role": "Reviewer",
                    "session_id": "child001",
                    "created_at": "2026-06-01T00:02:00"
                },
                {
                    "role": "Stale Role",
                    "session_id": "deadrole",
                    "created_at": "2026-06-01T00:02:30"
                }
            ],
            "adoption_proposals": [
                {
                    "id": "proposal1",
                    "proposer_session_id": "em123456",
                    "target_session_id": "child001",
                    "created_at": "2026-06-01T00:03:00",
                    "status": "pending",
                    "decided_at": null
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn write_email_bridge_config(api_base_url: Option<&str>, worker_secret: Option<&str>) -> PathBuf {
    let path = unique_temp_path();
    fs::write(&path, email_bridge_yaml(api_base_url, worker_secret)).unwrap();
    path
}

fn email_bridge_yaml(api_base_url: Option<&str>, worker_secret: Option<&str>) -> String {
    let api_base_url = api_base_url.unwrap_or("http://127.0.0.1:9");
    let worker_secret_yaml = worker_secret
        .map(|secret| format!("  worker_secret: \"{secret}\"\n"))
        .unwrap_or_default();
    format!(
        r#"resend:
  api_key: "test-api-key"
  domain: "example.com"
  reply_address: "reply@example.com"
  api_base_url: "{api_base_url}"
humans:
  operator:
    display_name: "Human operator"
    aliases: ["owner"]
    default_channel: "email"
    channels:
      email:
        enabled: true
        address: "operator@example.com"
        use: "fallback_only"
users:
  operator:
    email: "operator@example.com"
    name: "Human operator"
    aliases: ["owner"]
  teammate:
    email: "teammate@example.com"
    name: "Team Mate"
    aliases: ["tm"]
email_bridge:
  authorized_senders: ["operator@example.com"]
{worker_secret_yaml}  worker_secret_header: "x-email-worker-secret"
  session_id_header: "x-email-session-id"
  webhook_path: "/api/email-inbound"
"#
    )
}

fn spawn_resend_server(
    status: u16,
    response_body: &'static str,
) -> (String, mpsc::Receiver<Value>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_owned();
        let mut authorization = String::new();
        let mut content_length = 0_usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.eq_ignore_ascii_case("authorization") {
                    authorization = value.trim().to_owned();
                }
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
        }
        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body).unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        sender
            .send(json!({
                "path": path,
                "authorization": authorization,
                "body": body_json,
            }))
            .unwrap();
        let response = format!(
            "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    (format!("http://{address}"), receiver)
}

fn unique_temp_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "sm-rust-read-only-sessions-{}-{nanos}-{}.json",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn unique_short_temp_dir(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    PathBuf::from(format!(
        "/tmp/{}-{}-{}",
        prefix,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn runtime_app(state_file: &PathBuf, log_dir: &PathBuf, tmux_socket: &str) -> axum::Router {
    runtime_app_with_command(
        state_file,
        log_dir,
        tmux_socket,
        r#"/bin/sh -lc 'while IFS= read -r line; do printf "argv:%s\nids:%s:%s:%s\nruntime:%s\n" "$*" "$SESSION_MANAGER_ID" "$CLAUDE_SESSION_MANAGER_ID" "$ENABLE_TOOL_SEARCH" "$line"; done' runtime-sh"#,
    )
}

fn runtime_app_with_command(
    state_file: &PathBuf,
    log_dir: &PathBuf,
    tmux_socket: &str,
    runtime_command: &str,
) -> axum::Router {
    router(AppState::new(AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: queue_db_path_for_state_file(state_file)
                .display()
                .to_string(),
        },
        rust_core: RustCoreConfig {
            runtime_enabled: true,
            log_dir: Some(log_dir.display().to_string()),
            tmux_socket_name: Some(tmux_socket.to_owned()),
            runtime_command: Some(runtime_command.to_owned()),
            runtime_prompt_mode: Some("stdin".to_owned()),
            runtime_start_settle_ms: Some(100),
            send_keys_settle_ms: Some(10.0),
            send_keys_settle_max_ms: Some(50.0),
            send_keys_max_chunk_chars: Some(128),
            ..RustCoreConfig::default()
        },
        ..AppConfig::default()
    }))
}

async fn wait_for_output_contains(app: axum::Router, session_id: &str, needle: &str) -> Value {
    for _ in 0..30 {
        let (status, payload) = get_json(
            app.clone(),
            &format!("/sessions/{session_id}/output?lines=20"),
        )
        .await;
        if status == StatusCode::OK
            && payload["output"]
                .as_str()
                .unwrap_or_default()
                .contains(needle)
        {
            return payload;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for output containing {needle:?}");
}

async fn wait_for_file_contains(path: &PathBuf, needle: &str) -> String {
    for _ in 0..30 {
        let text = fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "timed out waiting for {} to contain {needle:?}",
        path.display()
    );
}

#[cfg(unix)]
fn spawn_codex_fork_control_socket(path: PathBuf, epoch: &'static str) -> mpsc::Receiver<Value> {
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw_request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut raw_request)
                .unwrap();
            let request: Value = serde_json::from_str(&raw_request).unwrap();
            sender.send(request.clone()).unwrap();
            let response = match request.get("command").and_then(Value::as_str) {
                Some("get_epoch") => json!({
                    "ok": true,
                    "epoch": epoch,
                    "result": { "epoch": epoch }
                }),
                Some("submit_message") => json!({
                    "ok": true,
                    "epoch": epoch,
                    "result": {}
                }),
                Some(command) => json!({
                    "ok": false,
                    "error": {
                        "code": "unknown_command",
                        "message": format!("unknown command {command}")
                    }
                }),
                None => json!({
                    "ok": false,
                    "error": {
                        "code": "missing_command",
                        "message": "missing command"
                    }
                }),
            };
            let mut raw_response = serde_json::to_string(&response).unwrap();
            raw_response.push('\n');
            stream.write_all(raw_response.as_bytes()).unwrap();
        }
    });
    receiver
}

#[cfg(unix)]
fn spawn_codex_fork_stale_epoch_control_socket(
    path: PathBuf,
    stale_epoch: &'static str,
    fresh_epoch: &'static str,
) -> mpsc::Receiver<Value> {
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for index in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw_request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut raw_request)
                .unwrap();
            let request: Value = serde_json::from_str(&raw_request).unwrap();
            sender.send(request.clone()).unwrap();
            let response = match (index, request.get("command").and_then(Value::as_str)) {
                (0, Some("get_epoch")) => json!({
                    "ok": true,
                    "epoch": stale_epoch,
                    "result": { "epoch": stale_epoch }
                }),
                (1, Some("submit_message")) => json!({
                    "ok": false,
                    "error": {
                        "code": "stale_epoch",
                        "message": "stale epoch"
                    }
                }),
                (2, Some("get_epoch")) => json!({
                    "ok": true,
                    "epoch": fresh_epoch,
                    "result": { "epoch": fresh_epoch }
                }),
                (3, Some("submit_message")) => json!({
                    "ok": true,
                    "epoch": fresh_epoch,
                    "result": {}
                }),
                (_, Some(command)) => json!({
                    "ok": false,
                    "error": {
                        "code": "unexpected_command",
                        "message": format!("unexpected command {command}")
                    }
                }),
                (_, None) => json!({
                    "ok": false,
                    "error": {
                        "code": "missing_command",
                        "message": "missing command"
                    }
                }),
            };
            let mut raw_response = serde_json::to_string(&response).unwrap();
            raw_response.push('\n');
            stream.write_all(raw_response.as_bytes()).unwrap();
        }
    });
    receiver
}

fn core_log_file_path(log_dir: &PathBuf, session_id: &str) -> PathBuf {
    log_dir.join(format!("{}.log", safe_session_basename(session_id)))
}

fn codex_fork_artifact_paths(log_dir: &PathBuf, session_id: &str) -> (PathBuf, PathBuf) {
    let basename = safe_session_basename(session_id);
    (
        log_dir.join(format!("{basename}.codex-fork.events.jsonl")),
        log_dir.join(format!("{basename}.codex-fork.control.sock")),
    )
}

fn safe_session_basename(session_id: &str) -> String {
    format!(
        "{}-{}",
        sanitize_path_component(session_id),
        stable_session_id_hash(session_id)
    )
}

fn sanitize_path_component(value: &str) -> String {
    let mut safe = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .collect::<String>();
    if safe.is_empty() {
        safe = "session".to_owned();
    }
    safe
}

fn stable_session_id_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut hash = String::with_capacity(12);
    for byte in &digest[..6] {
        hash.push_str(&format!("{byte:02x}"));
    }
    hash
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_session_exists(socket: &str, session: &str) -> bool {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("has-session")
        .arg("-t")
        .arg(session)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_kill_session(socket: &str, session: &str) -> bool {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("kill-session")
        .arg("-t")
        .arg(session)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_enter_copy_mode(socket: &str, session: &str) -> bool {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("copy-mode")
        .arg("-t")
        .arg(session)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_pane_in_mode(socket: &str, session: &str) -> Option<i32> {
    let output = Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("display-message")
        .arg("-p")
        .arg("-t")
        .arg(session)
        .arg("#{pane_in_mode}")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "0" => Some(0),
        "1" => Some(1),
        _ => None,
    }
}

fn tmux_pane_current_path(socket: &str, session: &str) -> Option<PathBuf> {
    let output = Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .arg("display-message")
        .arg("-p")
        .arg("-t")
        .arg(session)
        .arg("#{pane_current_path}")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

struct TestTmuxSocket(String);

impl Drop for TestTmuxSocket {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-L")
            .arg(&self.0)
            .arg("kill-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn device_access_token(secret: &str, email: &str, name: &str) -> String {
    let exp = unix_timestamp() + 3600;
    let payload = json!({
        "v": 1,
        "type": "device_access",
        "email": email,
        "name": name,
        "iat": unix_timestamp(),
        "exp": exp
    });
    let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
    let signature = hmac_sha256_urlsafe(secret.as_bytes(), payload_b64.as_bytes());
    format!("smat_{payload_b64}.{signature}")
}

fn starlette_session_cookie(secret: &str, payload: Value) -> String {
    let payload_b64 = STANDARD.encode(serde_json::to_vec(&payload).unwrap());
    let timestamp_b64 = URL_SAFE_NO_PAD.encode(int_to_bytes(unix_timestamp() as u64));
    let value = format!("{payload_b64}.{timestamp_b64}");
    let derived_key = itsdangerous_django_concat_key(secret);
    let signature = hmac_sha1_urlsafe(&derived_key, value.as_bytes());
    format!("{value}.{signature}")
}

fn hmac_sha256_urlsafe(key: &[u8], value: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(value);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac_sha1_urlsafe(key: &[u8], value: &[u8]) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).unwrap();
    mac.update(value);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn itsdangerous_django_concat_key(secret: &str) -> Vec<u8> {
    let mut hasher = Sha1::new();
    hasher.update(b"itsdangerous.Signersigner");
    hasher.update(secret.as_bytes());
    hasher.finalize().to_vec()
}

fn int_to_bytes(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first_nonzero = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len() - 1);
    bytes[first_nonzero..].to_vec()
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
