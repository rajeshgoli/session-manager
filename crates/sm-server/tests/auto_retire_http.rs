//! Auto-retire and restore on message (sm#1839; spec
//! `docs/working/1821_fit_and_finish_2.html`, appendix E3 tests), on the
//! fixture store: the sweep retires a finished idle agent sm started, and
//! anything addressed to an auto-retired agent restores it.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sm_server::{
    config::{AppConfig, EmailConfig, PathsConfig, SmSendConfig},
    http::{router, sweep_auto_retire, AppState},
};
use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tower::ServiceExt;

const RETIRED_AT: &str = "2026-09-30T13:05:00Z";
const FINISHED_AT: &str = "2026-09-30T12:05:00Z";

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "sm-auto-retire-http-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    app: axum::Router,
    state: AppState,
    dir: PathBuf,
}

const BRIDGE: &str = r#"humans:
  rajesh:
    display_name: "Human operator"
    aliases: ["rajeshgoli"]
    default_channel: "email"
    channels:
      email:
        enabled: true
        address: "operator@example.com"
"#;

/// `eng00001` live; under it `auto0001` auto-retired at 13:05, `manual01`
/// retired by `sm retire`, and `done0001`, which sm started and which
/// finished at 12:05 and has been idle since.
fn fixture(owner_settings: Value) -> Fixture {
    let dir = temp_dir();
    let state_file = dir.join("sessions.json");
    let session = |id: &str, parent: Option<&str>| {
        json!({
            "id": id, "name": format!("claude-{id}"), "friendly_name": format!("{id}-agent"),
            "working_dir": "/repo", "tmux_session": format!("claude-{id}"),
            "log_file": dir.join(format!("{id}.log")).display().to_string(),
            "status": "running", "provider": "claude",
            "created_at": "2026-09-30T09:00:00Z", "last_activity": "2026-09-30T09:01:00Z",
            "parent_session_id": parent,
        })
    };
    let retired = |id: &str, source: &str| {
        let mut record = session(id, Some("eng00001"));
        record["status"] = json!("stopped");
        record["completion_status"] = json!("retired");
        record["completed_at"] = json!(RETIRED_AT);
        record["stopped_at"] = json!(RETIRED_AT);
        record["terminal_provenance"] = json!({"cause": "explicit_retire",
            "observed_at": RETIRED_AT, "authority": "server_lifecycle", "source": source});
        record
    };
    let mut done = session("done0001", Some("eng00001"));
    done["started_by_sm"] = json!(true);
    done["agent_task_completed_at"] = json!(FINISHED_AT);
    done["last_activity"] = json!(FINISHED_AT);
    fs::write(
        &state_file,
        json!({"sessions": [
            session("eng00001", None),
            retired("auto0001", "auto_retire"),
            retired("manual01", "operator"),
            done,
        ], "owner_settings": owner_settings})
        .to_string(),
    )
    .unwrap();
    let bridge = dir.join("email_send.yaml");
    fs::write(&bridge, BRIDGE).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: dir.join("message_queue.db").display().to_string(),
        },
        email: EmailConfig {
            bridge_config: bridge.display().to_string(),
        },
        owner_name: "Rajesh".to_owned(),
        ..AppConfig::default()
    };
    config.push.db_path = dir.join("owner_push.db").display().to_string();
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.log_dir = Some(dir.join("logs").display().to_string());
    let state = AppState::new(config);
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

async fn session(f: &Fixture, id: &str) -> Value {
    let (status, body) = request(&f.app, "GET", &format!("/sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn message_from(f: &Fixture, sender: &str) -> String {
    let (status, body) = request(
        &f.app,
        "POST",
        "/humans/rajeshgoli/messages",
        Some(json!({"sender_session_id": sender, "text": "Ran the views on ES."})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}

#[tokio::test]
async fn a_reply_to_an_auto_retired_agent_restores_it_and_reaches_it() {
    let f = fixture(json!({}));
    let message = message_from(&f, "auto0001").await;
    let (_, page) = request(
        &f.app,
        "GET",
        &format!("/messages/{message}?format=json"),
        None,
    )
    .await;
    assert_eq!(page["reply_to_session_id"], "auto0001");
    assert_eq!(page["reply_restores"], true);

    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/messages/{message}/reply"),
        Some(json!({"submission_id": "sub-auto-001", "body": "Run the same views on YM."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivered_to_session_id"], "auto0001");
    let restored = session(&f, "auto0001").await;
    assert_eq!(restored["status"], "running", "{restored}");
    assert!(restored["terminal_provenance"].is_null(), "{restored}");
}

#[tokio::test]
async fn a_reply_to_an_agent_retired_on_purpose_still_goes_to_its_parent() {
    let f = fixture(json!({}));
    let message = message_from(&f, "manual01").await;
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/messages/{message}/reply"),
        Some(json!({"submission_id": "sub-manual-1", "body": "ok"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivered_to_session_id"], "eng00001");
    assert_eq!(session(&f, "manual01").await["status"], "stopped");
}

#[tokio::test]
async fn sm_send_to_an_auto_retired_agent_restores_it() {
    let f = fixture(json!({}));
    let (status, body) = request(
        &f.app,
        "POST",
        "/sessions/auto0001/input",
        Some(json!({"text": "Are the YM views done?", "delivery_mode": "sequential"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivered"], true, "{body}");
    assert_eq!(session(&f, "auto0001").await["status"], "running");
    // Retired on purpose: the send reports it stopped, as before.
    let (_, body) = request(
        &f.app,
        "POST",
        "/sessions/manual01/input",
        Some(json!({"text": "hello", "delivery_mode": "sequential"})),
    )
    .await;
    assert_eq!(body["delivered"], false, "{body}");
}

#[tokio::test]
async fn restore_reopens_the_claims_its_retire_ended_unless_another_agent_took_one() {
    let f = fixture(json!({}));
    // The router created the claims schema.
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    let claim = |id: &str, session: &str, number: i64, ended_at: Option<&str>| {
        conn.execute(
            "INSERT INTO work_claims (id, repo, number, kind, session_id, source, claimed_at,
                 ended_at, end_reason)
             VALUES (?1, 'acme/far', ?2, 'ticket', ?3, 'explicit', '2026-09-30T10:00:00Z', ?4, ?5)",
            params![id, number, session, ended_at, ended_at.map(|_| "retired")],
        )
        .unwrap();
    };
    claim("c-mine", "auto0001", 1855, Some("2026-09-30T13:05:01Z"));
    claim("c-taken", "auto0001", 1856, Some("2026-09-30T13:05:01Z"));
    claim("c-other", "eng00001", 1856, None);
    claim("c-older", "auto0001", 1700, Some("2026-09-29T08:00:00Z"));

    let (status, body) = request(&f.app, "POST", "/sessions/auto0001/restore", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = |id: &str| -> Option<String> {
        conn.query_row(
            "SELECT ended_at FROM work_claims WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(ended("c-mine"), None);
    assert!(ended("c-taken").is_some(), "another agent holds #1856 now");
    assert!(ended("c-older").is_some(), "ended before this retire");
    let reopened: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'claim.reopened'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reopened, 1);
}

#[tokio::test]
async fn the_sweep_retires_after_the_delay_and_marks_it_automatic() {
    let f = fixture(json!({}));
    let state = Arc::new(f.state.clone());
    // 59 minutes after it finished: not yet.
    let retired = sweep_auto_retire(state.clone(), at("2026-09-30T13:04:00Z"))
        .await
        .unwrap();
    assert!(retired.is_empty(), "{retired:?}");
    let retired = sweep_auto_retire(state, at(RETIRED_AT)).await.unwrap();
    assert_eq!(retired, vec!["done0001".to_owned()]);
    let done = session(&f, "done0001").await;
    assert_eq!(done["status"], "stopped", "{done}");
    assert_eq!(
        done["terminal_provenance"]["source"], "auto_retire",
        "{done}"
    );
    let (status, history) = request(&f.app, "GET", "/history/agents?q=done0001", None).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(
        history["agents"][0]["retired_automatically"], true,
        "{history}"
    );
    // Anything addressed to it now restores it.
    let (status, body) = request(
        &f.app,
        "POST",
        "/sessions/done0001/input",
        Some(json!({"text": "One more thing", "delivery_mode": "sequential"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(session(&f, "done0001").await["status"], "running");
}

#[tokio::test]
async fn the_sweep_does_nothing_when_the_owner_turned_it_off() {
    let f = fixture(json!({"auto_retire": {"value": {"enabled": false}, "updated_at": "t"}}));
    let retired = sweep_auto_retire(Arc::new(f.state.clone()), at("2026-10-01T00:00:00Z"))
        .await
        .unwrap();
    assert!(retired.is_empty(), "{retired:?}");
    assert_eq!(session(&f, "done0001").await["status"], "running");
}
