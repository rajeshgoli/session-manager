//! Owner follows over HTTP (sm#1569): following agents and queue jobs, the
//! `[sm follow]` message, push tokens, acks, and one worker pass end to end.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sm_server::{
    config::{AppConfig, PathsConfig, QueueRunnerConfig, SmSendConfig},
    http::{router, AppState},
    owner_push::{PushError, PushSender},
};
use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tower::ServiceExt;

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "sm-owner-follow-http-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[derive(Default)]
struct RecordingSender {
    sent: Mutex<Vec<(String, BTreeMap<String, String>)>>,
}

impl PushSender for RecordingSender {
    fn send(&self, token: &str, data: &BTreeMap<String, String>) -> Result<(), PushError> {
        self.sent
            .lock()
            .unwrap()
            .push((token.to_owned(), data.clone()));
        Ok(())
    }
}

struct Fixture {
    app: axum::Router,
    state: AppState,
    dir: PathBuf,
}

fn create_queue_db(state_dir: &Path) {
    fs::create_dir_all(state_dir).unwrap();
    let conn = Connection::open(state_dir.join("queue_runner.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE queue_jobs (
            id TEXT PRIMARY KEY, type TEXT NOT NULL, label TEXT NOT NULL,
            requester_session_id TEXT, notify_session_id TEXT NOT NULL, cwd TEXT NOT NULL,
            argv_json TEXT, script_path TEXT, env_json TEXT NOT NULL,
            timeout_seconds INTEGER NOT NULL, state TEXT NOT NULL, holding_reason TEXT,
            queued_at TEXT NOT NULL, started_at TEXT, finished_at TEXT, pid INTEGER,
            process_group_id INTEGER, exit_code INTEGER, log_path TEXT, exit_code_path TEXT,
            wrapper_path TEXT, queued_notified_at TEXT, started_notified_at TEXT,
            completion_notified_at TEXT
        );
        "#,
    )
    .unwrap();
    for (id, state) in [("job-running", "running"), ("job-done", "succeeded")] {
        conn.execute(
            "INSERT INTO queue_jobs (id, type, label, requester_session_id, notify_session_id, \
               cwd, env_json, timeout_seconds, state, queued_at, started_at) \
             VALUES (?1, 'background', ?2, 'eng00001', 'eng00001', '/repo', '{}', 900, ?3, \
               '2026-09-25T10:00:00Z', '2026-09-25T10:00:00Z')",
            params![id, format!("{id}-label"), state],
        )
        .unwrap();
    }
}

fn fixture(sender: Option<Arc<RecordingSender>>) -> Fixture {
    let dir = temp_dir();
    let state_file = dir.join("sessions.json");
    let log_dir = dir.clone();
    let session = |id: &str, status: &str| {
        json!({
            "id": id, "name": format!("claude-{id}"), "friendly_name": format!("{id}-agent"),
            "working_dir": "/repo", "tmux_session": format!("claude-{id}"),
            "log_file": log_dir.join(format!("{id}.log")).display().to_string(), "status": status,
            "created_at": "2026-09-24T00:00:00Z", "last_activity": "2026-09-24T00:01:00Z",
        })
    };
    fs::write(
        &state_file,
        json!({"sessions": [session("eng00001", "running"), session("asleep01", "stopped")]})
            .to_string(),
    )
    .unwrap();
    let queue_state_dir = dir.join("queue");
    create_queue_db(&queue_state_dir);
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: dir.join("message_queue.db").display().to_string(),
        },
        queue_runner: QueueRunnerConfig {
            state_dir: queue_state_dir.display().to_string(),
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    };
    config.push.db_path = dir.join("owner_push.db").display().to_string();
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.log_dir = Some(dir.join("logs").display().to_string());
    let state =
        AppState::new(config).with_push_sender(sender.map(|sender| sender as Arc<dyn PushSender>));
    Fixture {
        app: router(state.clone()),
        state,
        dir,
    }
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 49152))));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Messages delivered to a session. Without a tmux runtime, delivery
/// appends the text to the session's log file.
fn delivered_texts(f: &Fixture, session_id: &str) -> Vec<String> {
    fs::read_to_string(f.dir.join(format!("{session_id}.log")))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("[sm follow]"))
        .map(|line| line.trim().to_owned())
        .collect()
}

#[tokio::test]
async fn follow_is_idempotent_and_messages_once() {
    let f = fixture(None);
    let (status, first) = request(
        &f.app,
        "POST",
        "/sessions/eng00001/follow",
        Some(json!({"message": "  please publish a report  "})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["state"], "active");
    assert_eq!(first["target_kind"], "session");
    assert_eq!(first["session_id"], "eng00001");
    assert_eq!(first["session_name"], "eng00001-agent");
    assert!(first.get("message_text").is_none() && first.get("user_id").is_none());
    let delivered = delivered_texts(&f, "eng00001");
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert!(delivered[0].contains("[sm follow] please publish a report"));
    assert!(!delivered[0].contains("[sm follow] [sm follow]"));

    let (status, again) = request(
        &f.app,
        "POST",
        "/sessions/eng00001-agent/follow",
        Some(json!({"message": "second"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["id"], first["id"]);
    assert_eq!(delivered_texts(&f, "eng00001").len(), 1);

    let (status, listed) = request(&f.app, "GET", "/client/follows", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["push_configured"], false);
    assert_eq!(listed["follows"].as_array().unwrap().len(), 1);

    let (status, _) = request(&f.app, "DELETE", "/sessions/eng00001/follow", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = request(&f.app, "GET", "/client/follows", None).await;
    assert!(listed["follows"].as_array().unwrap().is_empty());
    let (status, _) = request(&f.app, "DELETE", "/sessions/eng00001/follow", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn follow_without_message_tells_the_agent_nothing() {
    let f = fixture(None);
    let (status, body) = request(
        &f.app,
        "POST",
        "/sessions/eng00001/follow",
        Some(json!({"message": "   "})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(delivered_texts(&f, "eng00001").is_empty());
}

#[tokio::test]
async fn follow_refuses_stopped_and_unknown_sessions() {
    let f = fixture(None);
    let (status, body) = request(
        &f.app,
        "POST",
        "/sessions/asleep01/follow",
        Some(json!({"message": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["detail"], "session stopped");
    let (status, _) = request(
        &f.app,
        "POST",
        "/sessions/nobody01/follow",
        Some(json!({"message": null})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(delivered_texts(&f, "asleep01").is_empty());
}

#[tokio::test]
async fn job_follow_rules() {
    let f = fixture(None);
    let (status, first) = request(&f.app, "POST", "/queue-jobs/job-running/follow", None).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["target_kind"], "queue_job");
    assert_eq!(first["job_id"], "job-running");
    assert_eq!(first["job_label"], "job-running-label");
    assert_eq!(first["session_id"], "eng00001");
    assert_eq!(first["session_name"], "eng00001-agent");
    let (status, again) =
        request(&f.app, "POST", "/queue-jobs/job-running-label/follow", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], first["id"]);
    let (status, body) = request(&f.app, "POST", "/queue-jobs/job-done/follow", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["detail"], "job finished");
    let (status, _) = request(&f.app, "POST", "/queue-jobs/missing/follow", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(delivered_texts(&f, "eng00001").is_empty());

    let (status, _) = request(&f.app, "DELETE", "/queue-jobs/job-running/follow", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = request(&f.app, "GET", "/client/follows", None).await;
    assert!(listed["follows"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn push_token_and_test_push_routes() {
    let f = fixture(None);
    let (status, _) = request(
        &f.app,
        "PUT",
        "/client/push-token",
        Some(json!({"token": " "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &f.app,
        "PUT",
        "/client/push-token",
        Some(json!({"token": "tok1", "device_name": "Pixel", "app_version": "abc"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = request(&f.app, "POST", "/client/push/test", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["detail"], "push not configured");
    let (status, _) = request(
        &f.app,
        "DELETE",
        "/client/push-token",
        Some(json!({"token": "tok1"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let sender = Arc::new(RecordingSender::default());
    let f = fixture(Some(sender.clone()));
    request(
        &f.app,
        "PUT",
        "/client/push-token",
        Some(json!({"token": "tok1", "device_name": "Pixel"})),
    )
    .await;
    let (status, body) = request(&f.app, "POST", "/client/push/test", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sent"], 1);
    let sent = sender.sent.lock().unwrap().clone();
    assert_eq!(sent[0].0, "tok1");
    assert_eq!(sent[0].1["kind"], "test");
}

#[tokio::test]
async fn ack_requires_a_known_follow() {
    let f = fixture(None);
    let (status, _) = request(&f.app, "POST", "/client/follows/fol_missing/ack", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, follow) = request(&f.app, "POST", "/queue-jobs/job-running/follow", None).await;
    let id = follow["id"].as_str().unwrap();
    let (status, _) = request(&f.app, "POST", &format!("/client/follows/{id}/ack"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn worker_pass_fires_and_pushes_a_finished_job() {
    let sender = Arc::new(RecordingSender::default());
    let f = fixture(Some(sender.clone()));
    request(
        &f.app,
        "PUT",
        "/client/push-token",
        Some(json!({"token": "tok1", "device_name": "Pixel"})),
    )
    .await;
    let (_, follow) = request(&f.app, "POST", "/queue-jobs/job-running/follow", None).await;
    let state = f.state.clone();
    assert!(
        tokio::task::spawn_blocking(move || state.run_follow_pass(true))
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
    assert!(sender.sent.lock().unwrap().is_empty(), "job still running");

    Connection::open(f.dir.join("queue").join("queue_runner.db"))
        .unwrap()
        .execute(
            "UPDATE queue_jobs SET state = 'failed', exit_code = 2, \
             finished_at = '2026-09-25T10:41:00Z' WHERE id = 'job-running'",
            [],
        )
        .unwrap();
    let state = f.state.clone();
    tokio::task::spawn_blocking(move || state.run_follow_pass(true))
        .await
        .unwrap()
        .unwrap();
    let sent = sender.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let data = &sent[0].1;
    assert_eq!(data["kind"], "job_finished");
    assert_eq!(data["title"], "job-running-label failed (exit 2)");
    assert_eq!(data["body"], "eng00001-agent · ran 41m");
    assert_eq!(data["follow_id"], follow["id"].as_str().unwrap());
    assert_eq!(data["session_id"], "eng00001");
    assert_eq!(data["job_id"], "job-running");

    let (_, listed) = request(&f.app, "GET", "/client/follows", None).await;
    assert_eq!(listed["follows"][0]["state"], "notified");
    assert_eq!(listed["follows"][0]["notified_via"], "push");
}

#[tokio::test]
async fn task_complete_fires_an_agent_follow_that_waits_for_a_report() {
    let f = fixture(None);
    let (_, follow) = request(
        &f.app,
        "POST",
        "/sessions/eng00001/follow",
        Some(json!({"message": null})),
    )
    .await;
    let (status, body) = request(
        &f.app,
        "POST",
        "/sessions/eng00001/task-complete",
        Some(json!({"requester_session_id": "eng00001"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let state = f.state.clone();
    tokio::task::spawn_blocking(move || state.run_follow_pass(false))
        .await
        .unwrap()
        .unwrap();
    let (_, listed) = request(&f.app, "GET", "/client/follows", None).await;
    let fired = &listed["follows"][0];
    assert_eq!(fired["id"], follow["id"]);
    assert_eq!(fired["fire_reason"], "task_complete");
    // No report yet: delivery waits out the report grace.
    assert_eq!(fired["state"], "fired");
    assert_ne!(fired["notify_after"], fired["fired_at"]);
}
