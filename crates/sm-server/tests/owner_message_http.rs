//! Owner messages over HTTP (sm#1580): `sm send <person>` creates a message,
//! the owner reads it, comments and replies, and the reply is queued for
//! the agent once. Spec: `specs/1580_app_messages_replace_email.html`.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sm_server::{
    config::{AppConfig, EmailConfig, PathsConfig, SmSendConfig},
    http::{router, AppState},
    owner_push::{PushError, PushSender},
};
use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    path::PathBuf,
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
        "sm-owner-message-http-{}-{nanos}-{}",
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

#[derive(Default)]
struct RecordingSender {
    sent: Mutex<Vec<BTreeMap<String, String>>>,
}

impl PushSender for RecordingSender {
    fn send(&self, _token: &str, data: &BTreeMap<String, String>) -> Result<(), PushError> {
        self.sent.lock().unwrap().push(data.clone());
        Ok(())
    }
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

/// Sessions: `eng00001` live; `child001` retired under it; `orphan01`
/// retired with no parent; `killed01` killed under `orphan01`.
fn fixture() -> Fixture {
    fixture_with_push(None)
}

fn fixture_with_push(sender: Option<Arc<RecordingSender>>) -> Fixture {
    let dir = temp_dir();
    let state_file = dir.join("sessions.json");
    let session = |id: &str, parent: Option<&str>, completion: Option<&str>| {
        let mut record = json!({
            "id": id, "name": format!("claude-{id}"), "friendly_name": format!("{id}-agent"),
            "working_dir": "/repo", "tmux_session": format!("claude-{id}"),
            "log_file": dir.join(format!("{id}.log")).display().to_string(),
            "status": if completion.is_some() { "stopped" } else { "running" },
            "created_at": "2026-09-24T00:00:00Z", "last_activity": "2026-09-24T00:01:00Z",
            "parent_session_id": parent,
        });
        if let Some(completion) = completion {
            record["completion_status"] = json!(completion);
        }
        record
    };
    fs::write(
        &state_file,
        json!({"sessions": [
            session("eng00001", None, None),
            session("child001", Some("eng00001"), Some("retired")),
            session("orphan01", None, Some("retired")),
            session("killed01", Some("orphan01"), Some("killed")),
        ]})
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
    let state =
        AppState::new(config).with_push_sender(sender.map(|sender| sender as Arc<dyn PushSender>));
    Fixture {
        app: router(state.clone()),
        state,
        dir,
    }
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
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
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, text) = send(app, method, uri, body).await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn create(f: &Fixture, sender: &str, text: &str, extra: Value) -> (StatusCode, Value) {
    let mut body = json!({"sender_session_id": sender, "text": text});
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    request(&f.app, "POST", "/humans/rajeshgoli/messages", Some(body)).await
}

async fn created_id(f: &Fixture, sender: &str, text: &str, extra: Value) -> String {
    let (status, body) = create(f, sender, text, extra).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// `(target, text, delivery_mode, sender)` of queued messages whose id
/// starts with `prefix`.
fn queued(f: &Fixture, prefix: &str) -> Vec<(String, String, String, Option<String>)> {
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT target_session_id, text, delivery_mode, sender_session_id FROM message_queue \
             WHERE id LIKE ?1 ORDER BY queued_at",
        )
        .unwrap();
    statement
        .query_map(params![format!("{prefix}%")], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

async fn session_feed(f: &Fixture, session_id: &str) -> Value {
    let (status, feed) = request(&f.app, "GET", "/session-obligations", None).await;
    assert_eq!(status, StatusCode::OK, "{feed}");
    feed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["session_id"] == session_id)
        .cloned()
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn create_validates_input() {
    let f = fixture();
    let (status, body) = create(
        &f,
        "eng00001",
        "# Keep the old fills table?\nBody",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap();
    assert!(id.starts_with("msg_") && id.len() == 12, "{id}");
    assert_eq!(body["title"], "Keep the old fills table?");
    assert_eq!(body["reader_path"], format!("/messages/{id}"));
    assert_eq!(body["blocking"], false);

    // An explicit title is trimmed.
    let (_, body) = create(
        &f,
        "eng00001",
        "text",
        json!({"title": "  Ship it?  ", "blocking": true}),
    )
    .await;
    assert_eq!(body["title"], "Ship it?");
    assert_eq!(body["blocking"], true);

    // 25,000 characters is the limit, counted in characters, not bytes.
    let (status, _) = create(&f, "eng00001", &"é".repeat(25_000), json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = create(&f, "eng00001", &"a".repeat(25_001), json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["detail"],
        "too long for a message (25,000 characters); publish it as a doc with sm doc publish"
    );

    for (text, extra) in [
        ("   \n ", json!({})),
        ("text", json!({"title": "   "})),
        ("text", json!({"title": "a".repeat(121)})),
    ] {
        let (status, body) = create(&f, "eng00001", text, extra).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    let (status, _) = create(&f, "nobody01", "text", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = request(
        &f.app,
        "POST",
        "/humans/teammate/messages",
        Some(json!({"sender_session_id": "eng00001", "text": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unread_cap_refuses_sixth_and_resets_on_view() {
    let f = fixture();
    let mut ids = Vec::new();
    for n in 0..5 {
        ids.push(created_id(&f, "eng00001", &format!("update {n}"), json!({})).await);
    }
    let (status, body) = create(&f, "eng00001", "sixth", json!({})).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body["detail"],
        "Rajesh has 5 unread messages from you; wait for them to be read"
    );
    // Reading JSON is not viewing.
    request(
        &f.app,
        "GET",
        &format!("/messages/{}?format=json", ids[0]),
        None,
    )
    .await;
    let (status, _) = create(&f, "eng00001", "sixth", json!({})).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    // Opening the sender's thread is.
    let (status, _) = send(&f.app, "GET", "/inbox/agent/eng00001", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = create(&f, "eng00001", "sixth", json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn page_marks_viewed_json_does_not() {
    let f = fixture();
    let id = created_id(
        &f,
        "eng00001",
        "# Title\n\nFirst paragraph.\n\n- a list",
        json!({}),
    )
    .await;
    let json_uri = format!("/messages/{id}?format=json");
    let (_, body) = request(&f.app, "GET", &json_uri, None).await;
    assert_eq!(body["state"], "new");
    assert_eq!(body["first_viewed_at"], Value::Null);
    assert_eq!(body["reply_to_session_id"], "eng00001");
    assert_eq!(body["sender_session_name"], "eng00001-agent");
    assert_eq!(body["replies"], json!([]));

    // The message page is the sender's thread, scrolled to the message,
    // served at the message's own path so the app's reader keeps it.
    let (status, page) = send(&f.app, "GET", &format!("/messages/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<title>eng00001-agent</title>"), "{page}");
    assert!(page.contains(&format!(r#"id="{id}""#)), "{page}");
    assert!(page.contains("<h3>Title</h3>"), "{page}");
    assert!(page.contains("data-sm-line=\"3\""), "{page}");
    assert!(page.contains(&format!(r#""at":"{id}""#)), "{page}");
    // After a send the page reloads at the newest item.
    let (_, page) = send(&f.app, "GET", &format!("/messages/{id}?bottom=1"), None).await;
    assert!(page.contains(r#""at":null"#), "{page}");
    let (_, body) = request(&f.app, "GET", &json_uri, None).await;
    assert_eq!(body["state"], "read");
    assert!(body["first_viewed_at"].is_string());

    let (status, _) = send(&f.app, "GET", "/messages/msg_00000000", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&f.app, "GET", "/messages/not-an-id", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn thread_reply_box_follows_recipient() {
    let f = fixture();
    created_id(&f, "eng00001", "x", json!({"blocking": true})).await;
    let (_, page) = send(&f.app, "GET", "/inbox/agent/eng00001", None).await;
    assert!(page.contains(r#""canSend":true"#), "{page}");
    assert!(page.contains("Write to eng00001-agent"), "{page}");
    assert!(page.contains("NEEDS YOU"), "{page}");
    // A retired sender with a live parent: the parent answers, and an ended
    // sender needs nobody.
    created_id(&f, "child001", "x", json!({"blocking": true})).await;
    let (_, page) = send(&f.app, "GET", "/inbox/agent/child001", None).await;
    assert!(page.contains(r#""canSend":true"#));
    assert!(page.contains("a reply goes to eng00001-agent"), "{page}");
    assert!(!page.contains("NEEDS YOU"), "an ended sender needs nobody");
    // Nobody left: no reply box, Done still offered.
    for sender in ["orphan01", "killed01"] {
        created_id(&f, sender, "x", json!({})).await;
        let (_, page) = send(&f.app, "GET", &format!("/inbox/agent/{sender}"), None).await;
        assert!(page.contains(r#""canSend":false"#), "{sender}");
        assert!(page.contains("No agent is left to reply to"), "{sender}");
        assert!(page.contains(r#"id="done""#), "{sender}");
    }
    let (status, _) = send(&f.app, "GET", "/inbox/agent/nobody01", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reply_is_idempotent_and_delivers_once() {
    let f = fixture();
    let id = created_id(
        &f,
        "eng00001",
        "# Keep the old fills table or drop it?\nThe migration copies every row.\n\nNothing reads `fills` after the cutover.",
        json!({}),
    )
    .await;
    let drafts = format!("/messages/{id}/drafts");
    let (status, late) = request(
        &f.app,
        "POST",
        &drafts,
        Some(json!({"line": 4, "quote": "Nothing reads `fills` after the cutover.", "body": "The month-end recon does. Check it first."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{late}");
    let (_, early) = request(
        &f.app,
        "POST",
        &drafts,
        Some(json!({"line": 2, "quote": "The migration copies every row.", "body": "typo"})),
    )
    .await;
    let (status, edited) = request(
        &f.app,
        "PATCH",
        &format!("{drafts}/{}", early["id"].as_str().unwrap()),
        Some(json!({"body": "Every row?"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["body"], "Every row?");
    let (_, listed) = request(&f.app, "GET", &drafts, None).await;
    assert_eq!(listed["drafts"].as_array().unwrap().len(), 2);

    let reply = json!({"submission_id": "sub-00000001", "body": "  Otherwise yes, drop it.  "});
    let (status, first) = request(
        &f.app,
        "POST",
        &format!("/messages/{id}/reply"),
        Some(reply.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let expected = format!(
        "[Input from: Rajesh via sm app] Re: \"Keep the old fills table or drop it?\" ({id})\n\
         Otherwise yes, drop it.\n\
         \n\
         > The migration copies every row.\n\
         Every row?\n\
         \n\
         > Nothing reads `fills` after the cutover.\n\
         The month-end recon does. Check it first."
    );
    assert_eq!(first["delivered_text"], expected);
    assert_eq!(first["delivered_to_session_id"], "eng00001");
    assert_eq!(first["delivered_to_session_name"], "eng00001-agent");
    // The retry after a lost response returns the same reply, delivers nothing new.
    let (status, again) = request(
        &f.app,
        "POST",
        &format!("/messages/{id}/reply"),
        Some(reply),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["delivered_text"], expected);
    let delivered = queued(&f, "owner-reply-");
    assert_eq!(
        delivered,
        vec![(
            "eng00001".to_owned(),
            expected.clone(),
            "sequential".to_owned(),
            None
        )]
    );
    // Drafts went with the reply; the page lists it.
    let (_, listed) = request(&f.app, "GET", &drafts, None).await;
    assert_eq!(listed["drafts"], json!([]));
    let (_, body) = request(&f.app, "GET", &format!("/messages/{id}?format=json"), None).await;
    assert_eq!(body["state"], "replied");
    assert_eq!(body["replies"][0]["comments"].as_array().unwrap().len(), 2);

    // The same submission id on another message is refused.
    let other = created_id(&f, "eng00001", "other", json!({})).await;
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/messages/{other}/reply"),
        Some(json!({"submission_id": "sub-00000001", "body": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Nothing to send.
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/messages/{other}/reply"),
        Some(json!({"submission_id": "sub-00000002", "body": "   "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["detail"], "Nothing to send");
    // Only overall text: no blank line after the header.
    let (_, body) = request(
        &f.app,
        "POST",
        &format!("/messages/{other}/reply"),
        Some(json!({"submission_id": "sub-00000003", "body": "Yes."})),
    )
    .await;
    assert_eq!(
        body["delivered_text"],
        format!("[Input from: Rajesh via sm app] Re: \"other\" ({other})\nYes.")
    );
    assert_eq!(queued(&f, "owner-reply-").len(), 2);
}

#[tokio::test]
async fn reply_routes_like_review_wakes() {
    let f = fixture();
    // A retired sender's reply goes to its live parent.
    let child = created_id(&f, "child001", "from the child", json!({})).await;
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/messages/{child}/reply"),
        Some(json!({"submission_id": "sub-child-01", "body": "ok"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivered_to_session_id"], "eng00001");
    // Nobody left: 409, and the drafts stay.
    let orphan = created_id(&f, "killed01", "from the killed one", json!({})).await;
    let drafts = format!("/messages/{orphan}/drafts");
    request(
        &f.app,
        "POST",
        &drafts,
        Some(json!({"line": 1, "quote": "from", "body": "?"})),
    )
    .await;
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/messages/{orphan}/reply"),
        Some(json!({"submission_id": "sub-orphan-1", "body": "ok"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["detail"], "No agent is left to reply to");
    let (_, listed) = request(&f.app, "GET", &drafts, None).await;
    assert_eq!(listed["drafts"].as_array().unwrap().len(), 1);
    assert_eq!(queued(&f, "owner-reply-").len(), 1);
}

#[tokio::test]
async fn blocking_message_waits_until_reply_or_handled() {
    let f = fixture();
    // An unmarked message waits on nobody, but is listed on the card.
    let plain = created_id(&f, "eng00001", "# FYI\nDone.", json!({})).await;
    let entry = session_feed(&f, "eng00001").await;
    assert_eq!(entry["waiting_on"], json!([]));
    assert_eq!(entry["messages"][0]["id"], plain);
    assert_eq!(entry["messages"][0]["state"], "new");
    assert_eq!(
        entry["messages"][0]["reader_path"],
        format!("/messages/{plain}")
    );
    let (status, _) = request(&f.app, "POST", &format!("/messages/{plain}/handled"), None).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Handled needs a blocking message"
    );

    // A blocking message waits until a reply.
    let first = created_id(&f, "eng00001", "# Keep it?\nx", json!({"blocking": true})).await;
    let entry = session_feed(&f, "eng00001").await;
    assert_eq!(
        entry["waiting_on"],
        json!([{"kind": "owner_message", "id": first, "label": "Rajesh · Keep it?",
                "since": entry["messages"][0]["created_at"]}])
    );
    assert_eq!(entry["messages"][0]["state"], "needs_you");
    assert!(entry["waiting_since"].is_string());
    request(
        &f.app,
        "POST",
        &format!("/messages/{first}/reply"),
        Some(json!({"submission_id": "sub-block-01", "body": "Keep it."})),
    )
    .await;
    assert_eq!(session_feed(&f, "eng00001").await["waiting_on"], json!([]));

    // ...or until Handled, which sends nothing.
    let second = created_id(&f, "eng00001", "# Drop it?\nx", json!({"blocking": true})).await;
    assert_eq!(
        session_feed(&f, "eng00001").await["waiting_on"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (status, _) = request(&f.app, "POST", &format!("/messages/{second}/handled"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let entry = session_feed(&f, "eng00001").await;
    assert_eq!(entry["waiting_on"], json!([]));
    assert_eq!(entry["messages"][0]["state"], "handled");
    assert_eq!(queued(&f, "owner-reply-").len(), 1);

    // An ended sender waits on nobody.
    created_id(&f, "orphan01", "# Late\nx", json!({"blocking": true})).await;
    let entry = session_feed(&f, "orphan01").await;
    assert_eq!(entry["waiting_on"], json!([]));
    assert_eq!(entry["messages"][0]["state"], "new");
}

#[tokio::test]
async fn opening_a_message_withdraws_its_phone_notification() {
    let sender = Arc::new(RecordingSender::default());
    let f = fixture_with_push(Some(sender.clone()));
    let (status, _) = request(
        &f.app,
        "PUT",
        "/client/push-token",
        Some(json!({"token": "tok1", "device_name": "Pixel"})),
    )
    .await;
    assert!(status.is_success(), "{status}");
    let id = created_id(
        &f,
        "eng00001",
        "# Keep the old fills table?\nBody",
        json!({}),
    )
    .await;
    let pass = |f: &Fixture| {
        let state = f.state.clone();
        async move {
            tokio::task::spawn_blocking(move || state.run_follow_pass(false))
                .await
                .unwrap()
                .unwrap();
        }
    };
    let kinds = || {
        sender
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|data| (data["kind"].clone(), data["notice_id"].clone()))
            .collect::<Vec<_>>()
    };
    pass(&f).await;
    let notice_id = kinds()[0].1.clone();
    assert_eq!(kinds(), vec![("message".to_owned(), notice_id.clone())]);

    // Shown on the phone but not opened: it stays.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/client/notices/{notice_id}/ack"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    pass(&f).await;
    assert_eq!(kinds().len(), 1);

    // The notification opens the message's page, which is the sender's
    // thread; opening it removes the notification, once.
    assert_eq!(
        sender.sent.lock().unwrap()[0]["reader_path"],
        format!("/messages/{id}")
    );
    let (status, _) = send(&f.app, "GET", "/inbox/agent/eng00001", None).await;
    assert_eq!(status, StatusCode::OK);
    pass(&f).await;
    pass(&f).await;
    assert_eq!(
        kinds(),
        vec![
            ("message".to_owned(), notice_id.clone()),
            ("withdraw".to_owned(), notice_id),
        ]
    );
}

// ---- Inbox (sm#1647) -------------------------------------------------------

async fn inbox(f: &Fixture, filter: &str) -> Value {
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("/inbox?format=json&filter={filter}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn keys(listing: &Value) -> Vec<String> {
    listing["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["thread_key"].as_str().unwrap().to_owned())
        .collect()
}

fn row(listing: &Value, key: &str) -> Value {
    listing["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["thread_key"] == key)
        .cloned()
        .unwrap_or(Value::Null)
}

fn publish_doc(f: &Fixture, session: &str, path: &str, review: bool) -> String {
    use sm_server::owner_docs::{OwnerDocStore, PublishOwnerDoc};
    OwnerDocStore::new(f.dir.join("message_queue.db"))
        .publish(
            PublishOwnerDoc {
                repo: "acme/widgets".into(),
                path: path.into(),
                pr_number: Some(12),
                session_id: session.into(),
                session_name: Some(format!("{session}-agent")),
                title: "Queue memo".into(),
                note: None,
                commit_sha: "a".repeat(40),
                blob_sha: "b".repeat(40),
                review_requested: review,
                checkout_root: None,
            },
            |_| true,
        )
        .unwrap()
        .doc
        .id
}

#[tokio::test]
async fn inbox_groups_threads_by_what_they_need() {
    let f = fixture();
    // Read, so earlier.
    created_id(&f, "orphan01", "# Old news\nx", json!({})).await;
    send(&f.app, "GET", "/inbox/agent/orphan01", None).await;
    // Unread note from an ended agent: new.
    created_id(&f, "child001", "# Inventory done\nx", json!({})).await;
    // A blocking question: needs you, with the question as the preview.
    created_id(&f, "eng00001", "# Copying fills\nx", json!({})).await;
    created_id(&f, "eng00001", "# Keep it?\nx", json!({"blocking": true})).await;
    created_id(&f, "eng00001", "# Progress\nx", json!({})).await;
    // A review request from a live agent needs you; a plain publish is new.
    let review = publish_doc(&f, "eng00001", "docs/memo.md", true);
    let plain = publish_doc(&f, "eng00001", "docs/readout.md", false);

    let listing = inbox(&f, "open").await;
    assert_eq!(listing["needs_you_count"], 2);
    assert_eq!(listing["has_new"], true);
    let order = keys(&listing);
    assert_eq!(order.len(), 5, "{listing}");
    assert!(order[..2].contains(&"agent:eng00001".to_owned()));
    assert!(order[..2].contains(&format!("doc:{review}")));
    assert!(order[2..4].contains(&"agent:child001".to_owned()));
    assert!(order[2..4].contains(&format!("doc:{plain}")));
    assert_eq!(order[4], "agent:orphan01");

    let eng = row(&listing, "agent:eng00001");
    assert_eq!(eng["group"], "needs_you");
    assert_eq!(eng["preview"], "Keep it?");
    assert_eq!(eng["message_count"], 3);
    assert_eq!(eng["open_asks"], 1);
    assert_eq!(eng["status"], "live");
    assert_eq!(eng["url"], "/inbox/agent/eng00001");
    let doc = row(&listing, &format!("doc:{review}"));
    assert_eq!(doc["kind"], "doc");
    assert_eq!(doc["group"], "needs_you");
    assert_eq!(doc["status"], "review_requested");
    assert_eq!(doc["preview"], "Review requested · revision 1");
    assert_eq!(doc["pr_number"], 12);
    assert_eq!(doc["author"], "eng00001-agent");
    assert!(doc["url"]
        .as_str()
        .unwrap()
        .starts_with("/docs/widgets/docs/memo.md"));
    assert_eq!(
        row(&listing, &format!("doc:{plain}"))["preview"],
        "Published · for your information"
    );
    assert_eq!(row(&listing, "agent:child001")["status"], "ended");

    // The Docs filter lists every doc; the page shows the groups.
    assert_eq!(keys(&inbox(&f, "docs").await).len(), 2);
    let (status, page) = send(&f.app, "GET", "/inbox", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Needs you · 2"), "{page}");
    assert!(page.contains("New · 2"), "{page}");
    assert!(page.contains("Earlier · 1"), "{page}");
    assert!(page.contains(r#"class="tab on" href="/inbox""#), "{page}");
    let (status, _) = send(&f.app, "GET", "/inbox?filter=bogus", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_verdict_shows_only_while_the_latest_revision_has_it() {
    use sm_server::owner_docs::{
        git_blob_sha, OwnerDocStore, OwnerDocVerdict, PostedOwnerDocReview, PublishOwnerDoc,
    };
    let f = fixture();
    let doc = publish_doc(&f, "eng00001", "docs/memo.md", true);
    let store = OwnerDocStore::new(f.dir.join("message_queue.db"));
    store
        .begin_review(
            "sub-verdict-1",
            &doc,
            &"a".repeat(40),
            &"b".repeat(40),
            OwnerDocVerdict::Approve,
            None,
        )
        .unwrap();
    store
        .finish_review(
            "sub-verdict-1",
            &PostedOwnerDocReview {
                github_review_id: Some(1),
                github_review_url: "https://github.com/acme/widgets/pull/12#r1".into(),
                posted_at: None,
                line_comment_count: 0,
                file_comment_count: 0,
                draft_ids: Vec::new(),
                wake: None,
            },
        )
        .unwrap();
    let key = format!("doc:{doc}");
    let reviewed = row(&inbox(&f, "docs").await, &key);
    assert_eq!(reviewed["status"], "reviewed");
    assert_eq!(reviewed["verdict"], "approve");
    assert_eq!(reviewed["preview"], "You reviewed · Approved");
    // A newer revision, then read: no verdict for it.
    store
        .publish(
            PublishOwnerDoc {
                repo: "acme/widgets".into(),
                path: "docs/memo.md".into(),
                pr_number: Some(12),
                session_id: "eng00001".into(),
                session_name: Some("eng00001-agent".into()),
                title: "Queue memo".into(),
                note: None,
                commit_sha: "c".repeat(40),
                blob_sha: git_blob_sha(b"v2"),
                review_requested: false,
                checkout_root: None,
            },
            |_| true,
        )
        .unwrap();
    store.record_view(&doc, &git_blob_sha(b"v2")).unwrap();
    let read = row(&inbox(&f, "docs").await, &key);
    assert_eq!(read["status"], "read");
    assert_eq!(read["verdict"], Value::Null);
    assert_eq!(read["preview"], "Read");
}

#[tokio::test]
async fn a_review_request_from_an_ended_agent_is_new_not_needs_you() {
    let f = fixture();
    let doc = publish_doc(&f, "orphan01", "docs/memo.md", true);
    let listing = inbox(&f, "open").await;
    assert_eq!(listing["needs_you_count"], 0);
    assert_eq!(row(&listing, &format!("doc:{doc}"))["group"], "new");
}

#[tokio::test]
async fn thread_send_answers_the_open_question_once() {
    let f = fixture();
    let note = created_id(
        &f,
        "eng00001",
        "# Copying\n\nNothing reads fills.",
        json!({}),
    )
    .await;
    let ask = created_id(&f, "eng00001", "# Keep it?\nx", json!({"blocking": true})).await;
    let body = json!({
        "submission_id": "sub-thread-01",
        "body": "The month-end recon does.",
        "quotes": [{"message_id": note, "quote": "Nothing reads fills."}],
    });
    let (status, sent) = request(
        &f.app,
        "POST",
        "/inbox/agent/eng00001/send",
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["kind"], "reply");
    assert_eq!(sent["message_id"], ask, "the open question is answered");
    assert_eq!(sent["delivered_to_session_name"], "eng00001-agent");
    let (status, again) = request(&f.app, "POST", "/inbox/agent/eng00001/send", Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], sent["id"]);
    let queued = queued(&f, "owner-reply-");
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].1,
        format!(
            "[Input from: Rajesh via sm app] Re: \"Keep it?\" ({ask})\n\
             > Nothing reads fills.\n\nThe month-end recon does."
        )
    );
    let (_, message) = request(&f.app, "GET", &format!("/messages/{ask}?format=json"), None).await;
    assert_eq!(message["state"], "replied");
    let listing = inbox(&f, "open").await;
    assert_eq!(listing["needs_you_count"], 0);
    assert_eq!(
        row(&listing, "agent:eng00001")["preview"],
        "You: The month-end recon does."
    );
    let (_, page) = send(&f.app, "GET", "/inbox/agent/eng00001", None).await;
    assert!(
        page.contains("<blockquote>Nothing reads fills.</blockquote>"),
        "{page}"
    );

    // A quote from another agent's message, an empty send, nobody to reply to.
    let other = created_id(&f, "orphan01", "# Other\nx", json!({})).await;
    let (status, _) = request(
        &f.app,
        "POST",
        "/inbox/agent/eng00001/send",
        Some(json!({"submission_id": "sub-thread-02",
                    "quotes": [{"message_id": other, "quote": "x"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &f.app,
        "POST",
        "/inbox/agent/eng00001/send",
        Some(json!({"submission_id": "sub-thread-03", "body": "  "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &f.app,
        "POST",
        "/inbox/agent/orphan01/send",
        Some(json!({"submission_id": "sub-thread-04", "body": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn writing_first_sends_a_note_without_a_re_line() {
    let f = fixture();
    let (status, sent) = request(
        &f.app,
        "POST",
        "/inbox/agent/eng00001/send",
        Some(json!({"submission_id": "sub-note-0001", "body": "Pause the copy."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["kind"], "note");
    assert_eq!(sent["message_id"], Value::Null);
    let queued = queued(&f, "owner-note-");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, "eng00001");
    assert_eq!(
        queued[0].1,
        "[Input from: Rajesh via sm app]\nPause the copy."
    );
    let listing = inbox(&f, "open").await;
    let thread = row(&listing, "agent:eng00001");
    assert_eq!(thread["preview"], "You: Pause the copy.");
    assert_eq!(thread["group"], "earlier");
    let (_, page) = send(&f.app, "GET", "/inbox/agent/eng00001", None).await;
    assert!(page.contains("Pause the copy."), "{page}");
}

#[tokio::test]
async fn done_clears_the_ask_and_anything_new_reopens_the_thread() {
    let f = fixture();
    let ask = created_id(&f, "eng00001", "# Keep it?\nx", json!({"blocking": true})).await;
    let doc = publish_doc(&f, "eng00001", "docs/memo.md", true);
    for key in ["agent:eng00001".to_owned(), format!("doc:{doc}")] {
        let (status, body) = request(
            &f.app,
            "POST",
            "/inbox/done",
            Some(json!({"thread_key": key})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let listing = inbox(&f, "open").await;
    assert_eq!(keys(&listing), Vec::<String>::new());
    assert_eq!(listing["needs_you_count"], 0);
    assert_eq!(keys(&inbox(&f, "done").await).len(), 2);
    let (_, message) = request(&f.app, "GET", &format!("/messages/{ask}?format=json"), None).await;
    assert_eq!(message["state"], "handled");
    let docs = inbox(&f, "docs").await;
    assert_eq!(row(&docs, &format!("doc:{doc}"))["status"], "new");
    assert_eq!(row(&docs, &format!("doc:{doc}"))["done"], true);
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    let queue_exists: bool = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE name = 'message_queue'")
        .unwrap()
        .exists([])
        .unwrap();
    assert!(
        !queue_exists || queued(&f, "owner-").is_empty(),
        "Done messages nobody"
    );

    // The agent writes again: the thread is back in Open.
    created_id(&f, "eng00001", "# One more thing\nx", json!({})).await;
    let listing = inbox(&f, "open").await;
    assert_eq!(keys(&listing), vec!["agent:eng00001".to_owned()]);
    assert_eq!(row(&listing, "agent:eng00001")["group"], "new");

    for (key, status) in [
        ("agent:nobody01", StatusCode::NOT_FOUND),
        ("doc:zzzzzzzz", StatusCode::NOT_FOUND),
        ("bogus", StatusCode::BAD_REQUEST),
    ] {
        let (got, _) = request(
            &f.app,
            "POST",
            "/inbox/done",
            Some(json!({"thread_key": key})),
        )
        .await;
        assert_eq!(got, status, "{key}");
    }
}

#[tokio::test]
async fn a_fired_follow_is_new_until_its_thread_is_read() {
    use sm_server::owner_push::{FollowTarget, OwnerPushStore, REASON_TASK_COMPLETE};
    let f = fixture();
    let store = OwnerPushStore::new(f.dir.join("owner_push.db"));
    let now = time::OffsetDateTime::now_utc();
    let (follow, _) = store
        .create_follow(
            "operator@example.com",
            &FollowTarget::Session {
                session_id: "eng00001".into(),
                session_name: "eng00001-agent".into(),
            },
            None,
            now,
        )
        .unwrap();
    store
        .fire(&follow.id, REASON_TASK_COMPLETE, now, None)
        .unwrap();
    let listing = inbox(&f, "open").await;
    let thread = row(&listing, "agent:eng00001");
    assert_eq!(thread["group"], "new");
    assert_eq!(
        thread["preview"],
        "eng00001-agent finished · No completion report published"
    );
    let (_, page) = send(&f.app, "GET", "/inbox/agent/eng00001", None).await;
    assert!(
        page.contains(r#"<div class="ev">eng00001-agent finished"#),
        "{page}"
    );
    assert_eq!(
        row(&inbox(&f, "open").await, "agent:eng00001")["group"],
        "earlier"
    );
    // A follow that fires in the same second as the read is still new.
    let (job, _) = store
        .create_follow(
            "operator@example.com",
            &FollowTarget::QueueJob {
                job_id: "job_1".into(),
                job_label: "copy-fills".into(),
                session_id: "eng00001".into(),
                session_name: "eng00001-agent".into(),
            },
            None,
            now,
        )
        .unwrap();
    store
        .fire(&job.id, REASON_TASK_COMPLETE, now, None)
        .unwrap();
    assert_eq!(
        row(&inbox(&f, "open").await, "agent:eng00001")["group"],
        "new"
    );
}
