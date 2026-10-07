//! Host restart recovery (sm#2054) on the fixture store: a cohort recorded
//! after a boot change is listed, restored in one action through the ordinary
//! restore path, and each restored agent gets the restart notice once.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sm_server::{
    config::{AppConfig, PathsConfig, SmSendConfig},
    host_restart::{BootIdentity, CohortMember, HostRestartStore},
    http::{router, AppState},
};
use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use time::macros::datetime;
use tower::ServiceExt;

const MEMBERS: [&str; 3] = ["far01936", "sm020200", "far01978"];

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "sm-host-restart-http-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    app: axum::Router,
    dir: PathBuf,
    restart_id: String,
}

/// Three agents the restart stopped (the middle one idle, the others
/// mid-turn), one that was already stopped before it, and the cohort sm
/// recorded at startup.
fn fixture() -> Fixture {
    let dir = temp_dir();
    let state_file = dir.join("sessions.json");
    let session = |id: &str, status: &str| {
        json!({
            "id": id, "name": format!("claude-{id}"), "friendly_name": format!("{id}-agent"),
            "working_dir": "/repo", "tmux_session": format!("claude-{id}"),
            "log_file": dir.join(format!("{id}.log")).display().to_string(),
            "status": status, "provider": "claude",
            "created_at": "2026-10-07T09:00:00Z", "last_activity": "2026-10-07T18:40:00Z",
            "stopped_at": "2026-10-07T19:10:10Z",
            "error_message": "Interrupted when the Mac restarted at 11:49 am; `sm recover` restores it",
        })
    };
    let mut sessions: Vec<Value> = MEMBERS.iter().map(|id| session(id, "stopped")).collect();
    sessions.push(session("oldstop1", "stopped"));
    fs::write(&state_file, json!({ "sessions": sessions }).to_string()).unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
            ..PathsConfig::default()
        },
        sm_send: SmSendConfig {
            db_path: dir.join("message_queue.db").display().to_string(),
        },
        ..AppConfig::default()
    };
    config.push.db_path = dir.join("owner_push.db").display().to_string();
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.log_dir = Some(dir.join("logs").display().to_string());
    config.queue_runner.state_dir = dir.join("queue-runner").display().to_string();
    let state = AppState::new(config);

    let store = HostRestartStore::beside_state_file(&state_file);
    let boot = |id: &str, at| BootIdentity {
        id: id.to_owned(),
        booted_at: at,
    };
    store
        .detect(
            &boot("OLD", datetime!(2026-09-12 08:00 UTC)),
            datetime!(2026-09-12 08:01 UTC),
        )
        .unwrap();
    let restart = store
        .detect(
            &boot("NEW", datetime!(2026-10-07 18:49:48 UTC)),
            datetime!(2026-10-07 19:10:10 UTC),
        )
        .unwrap()
        .unwrap();
    let members = MEMBERS
        .iter()
        .map(|id| {
            let prior = if *id == "sm020200" { "idle" } else { "running" };
            CohortMember::new(
                *id,
                format!("{id}-agent"),
                "claude",
                prior,
                Some("2026-10-07T18:40:00Z".to_owned()),
                "/repo",
                vec![format!("ticket acme/far#{}", &id[3..])],
            )
        })
        .collect::<Vec<_>>();
    store.add_members(&restart.id, &members).unwrap();
    Fixture {
        app: router(state),
        dir,
        restart_id: restart.id,
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

fn notices(f: &Fixture, session_id: &str) -> Vec<String> {
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT text FROM message_queue
             WHERE target_session_id = ?1 AND message_category = 'host_restart'",
        )
        .unwrap();
    statement
        .query_map(params![session_id], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn recover_lists_the_cohort_and_restores_it_in_one_action_with_one_notice_each() {
    let f = fixture();
    let (status, body) = request(&f.app, "GET", "/host-restarts/latest", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let restart = &body["restart"];
    assert_eq!(restart["id"], f.restart_id.as_str());
    assert_eq!(restart["open_count"], 3, "{restart}");
    let members = restart["members"].as_array().unwrap();
    assert_eq!(
        members.len(),
        3,
        "the agent stopped before the restart is not in it"
    );
    let idle = members
        .iter()
        .find(|m| m["session_id"] == "sm020200")
        .unwrap();
    assert_eq!(idle["mid_turn"], false);
    assert_eq!(idle["claims"], json!(["ticket acme/far#20200"]));

    let restore = format!("/host-restarts/{}/restore", f.restart_id);
    let (status, body) = request(&f.app, "POST", &restore, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let outcomes = body["results"].as_array().unwrap();
    assert_eq!(outcomes.len(), 3, "{body}");
    assert!(
        outcomes.iter().all(|o| o["outcome"] == "restored"),
        "{body}"
    );
    assert_eq!(body["restart"]["open_count"], 0, "{body}");
    for id in MEMBERS {
        let (_, session) = request(&f.app, "GET", &format!("/sessions/{id}"), None).await;
        assert_eq!(session["status"], "running", "{session}");
        let sent = notices(&f, id);
        assert_eq!(sent.len(), 1, "{id}: {sent:?}");
        assert!(sent[0].contains("The Mac restarted"), "{}", sent[0]);
    }
    assert!(notices(&f, "sm020200")[0].contains("You were idle"));
    assert!(notices(&f, "far01936")[0].contains("mid-turn"));
    assert!(notices(&f, "oldstop1").is_empty());

    // A second restore all finds nothing left and sends nothing more.
    let (status, body) = request(&f.app, "POST", &restore, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["results"].as_array().unwrap().is_empty(), "{body}");
    for id in MEMBERS {
        assert_eq!(notices(&f, id).len(), 1, "{id}");
    }
}

#[tokio::test]
async fn a_plain_restore_of_a_member_also_sends_the_notice_and_closes_it() {
    let f = fixture();
    let (status, body) = request(&f.app, "POST", "/sessions/far01978/restore", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(notices(&f, "far01978").len(), 1);
    let (_, body) = request(&f.app, "GET", "/host-restarts/latest", None).await;
    assert_eq!(body["restart"]["open_count"], 2, "{body}");
    // Restoring only one named member leaves the rest waiting.
    let (_, body) = request(
        &f.app,
        "POST",
        &format!("/host-restarts/{}/restore", f.restart_id),
        Some(json!({"session_ids": ["sm020200"]})),
    )
    .await;
    assert_eq!(body["results"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["restart"]["open_count"], 1, "{body}");
}

#[tokio::test]
async fn leaving_a_member_retires_it_and_takes_it_out_of_the_cohort() {
    let f = fixture();
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/host-restarts/{}/members/far01936/leave", f.restart_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let left = body["restart"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["session_id"] == "far01936")
        .unwrap()
        .clone();
    assert_eq!(left["decision"], "left", "{left}");
    let (_, session) = request(&f.app, "GET", "/sessions/far01936", None).await;
    assert_eq!(
        session["terminal_provenance"]["cause"], "explicit_retire",
        "{session}"
    );
    assert!(notices(&f, "far01936").is_empty());
    // One retired the ordinary way is not brought back by restore all.
    let (status, body) =
        request(&f.app, "POST", "/sessions/far01978/retire", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = request(
        &f.app,
        "POST",
        &format!("/host-restarts/{}/restore", f.restart_id),
        Some(json!({})),
    )
    .await;
    let outcome = |id: &str| {
        body["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["session_id"] == id)
            .unwrap()["outcome"]
            .clone()
    };
    assert_eq!(outcome("far01978"), "left", "{body}");
    assert_eq!(outcome("sm020200"), "restored", "{body}");
    let (_, session) = request(&f.app, "GET", "/sessions/far01978", None).await;
    assert_eq!(session["status"], "stopped", "{session}");
    // Leaving it twice is refused.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/host-restarts/{}/members/far01936/leave", f.restart_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn no_restart_recorded_reads_as_null() {
    let dir = temp_dir();
    let state_file = dir.join("sessions.json");
    fs::write(&state_file, json!({"sessions": []}).to_string()).unwrap();
    let mut config = AppConfig::default();
    config.paths.state_file = state_file.display().to_string();
    config.sm_send.db_path = dir.join("message_queue.db").display().to_string();
    let app = router(AppState::new(config));
    let (status, body) = request(&app, "GET", "/host-restarts/latest", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"restart": null}));
}
