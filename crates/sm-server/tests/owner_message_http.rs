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
    // Opening the page is.
    let (status, _) = send(&f.app, "GET", &format!("/messages/{}", ids[0]), None).await;
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

    let (status, page) = send(&f.app, "GET", &format!("/messages/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<title>Title</title>"), "{page}");
    assert!(page.contains("data-sm-line=\"3\""), "{page}");
    assert!(page.contains("eng00001-agent · just now"), "{page}");
    assert!(page.contains("sm-doc-client"));
    let (_, body) = request(&f.app, "GET", &json_uri, None).await;
    assert_eq!(body["state"], "read");
    assert!(body["first_viewed_at"].is_string());

    let (status, _) = send(&f.app, "GET", "/messages/msg_00000000", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&f.app, "GET", "/messages/not-an-id", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn page_mode_follows_recipient() {
    let f = fixture();
    let live = created_id(&f, "eng00001", "x", json!({"blocking": true})).await;
    let (_, page) = send(&f.app, "GET", &format!("/messages/{live}"), None).await;
    assert!(page.contains(r#""mode":"reply""#), "{page}");
    assert!(page.contains(r#""replyTo":"eng00001-agent""#), "{page}");
    assert!(
        page.contains("eng00001-agent · just now · needs you"),
        "{page}"
    );
    // A retired sender with a live parent: the parent answers.
    let child = created_id(&f, "child001", "x", json!({"blocking": true})).await;
    let (_, page) = send(&f.app, "GET", &format!("/messages/{child}"), None).await;
    assert!(page.contains(r#""mode":"reply""#));
    assert!(page.contains(r#""replyTo":"eng00001-agent""#));
    assert!(!page.contains("needs you"), "an ended sender needs nobody");
    // Nobody left: read mode.
    for sender in ["orphan01", "killed01"] {
        let id = created_id(&f, sender, "x", json!({})).await;
        let (_, page) = send(&f.app, "GET", &format!("/messages/{id}"), None).await;
        assert!(page.contains(r#""mode":"read""#), "{sender}");
        assert!(page.contains(r#""replyTo":null"#), "{sender}");
    }
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

    // The owner opens the message: the phone is told to remove it, once.
    let (status, _) = send(&f.app, "GET", &format!("/messages/{id}"), None).await;
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
