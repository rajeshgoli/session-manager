//! Owner doc review additions (sm#1580): a `--review` publish notifies the
//! owner, No review needed clears a request, and Assign starts a new agent
//! on a review nobody was left to take.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use sm_server::{
    config::{AppConfig, PathsConfig, SmSendConfig},
    http::{router, AppState, DocFetchError, DocPullRequest, OwnerDocSource},
    owner_docs::{git_blob_sha, OwnerDocStore, OwnerDocVerdict, PostedOwnerDocReview},
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

const REPO: &str = "acme/widgets";
const MEMO: &[u8] = b"# Decision memo\n\nBuy.\n";

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "sm-owner-doc-http-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Files by `(path, sha)`; every PR is open.
#[derive(Default)]
struct Source {
    files: Mutex<BTreeMap<(String, String), Vec<u8>>>,
}

impl OwnerDocSource for Source {
    fn fetch_doc(&self, _repo: &str, path: &str, sha: &str) -> Result<Vec<u8>, DocFetchError> {
        self.files
            .lock()
            .unwrap()
            .get(&(path.to_owned(), sha.to_owned()))
            .cloned()
            .ok_or_else(|| DocFetchError::NotFound("HTTP 404".to_owned()))
    }

    fn fetch_doc_with_blob_sha(
        &self,
        repo: &str,
        path: &str,
        sha: &str,
    ) -> Result<(Vec<u8>, String), DocFetchError> {
        let bytes = self.fetch_doc(repo, path, sha)?;
        let blob = git_blob_sha(&bytes);
        Ok((bytes, blob))
    }

    fn pull_request(&self, repo: &str, pr_number: i64) -> Result<DocPullRequest, String> {
        Ok(DocPullRequest {
            node_id: format!("PR_{pr_number}"),
            state: "open".to_owned(),
            head_sha: "a".repeat(40),
            url: format!("https://github.com/{repo}/pull/{pr_number}"),
        })
    }
}

struct Fixture {
    app: axum::Router,
    dir: PathBuf,
    state_file: PathBuf,
}

/// `author01` live (claude, opus, high); `retired1` retired with no parent
/// (codex-fork, gpt-5, xhigh).
fn fixture() -> Fixture {
    let dir = temp_dir();
    let state_file = dir.join("sessions.json");
    let session = |id: &str, provider: &str, model: &str, effort: &str, retired: bool| {
        let mut record = json!({
            "id": id, "name": format!("{provider}-{id}"), "friendly_name": format!("{id}-writer"),
            "working_dir": "/repo", "tmux_session": format!("{provider}-{id}"),
            "log_file": dir.join(format!("{id}.log")).display().to_string(),
            "status": if retired { "stopped" } else { "running" },
            "provider": provider, "model": model, "reasoning_effort": effort,
            "created_at": "2026-09-24T00:00:00Z", "last_activity": "2026-09-24T00:01:00Z",
        });
        if retired {
            record["completion_status"] = json!("retired");
        }
        record
    };
    fs::write(
        &state_file,
        json!({"sessions": [
            session("author01", "claude", "opus", "high", false),
            session("retired1", "codex-fork", "gpt-5", "xhigh", true),
        ]})
        .to_string(),
    )
    .unwrap();
    let source = Source::default();
    for sha in ["a", "b", "c"] {
        source
            .files
            .lock()
            .unwrap()
            .insert(("specs/memo.md".to_owned(), sha.repeat(40)), MEMO.to_vec());
    }
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: dir.join("message_queue.db").display().to_string(),
        },
        owner_name: "Rajesh".to_owned(),
        ..AppConfig::default()
    };
    config.push.db_path = dir.join("owner_push.db").display().to_string();
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.log_dir = Some(dir.join("logs").display().to_string());
    Fixture {
        app: router(AppState::new(config).with_owner_doc_source(Arc::new(source))),
        dir,
        state_file,
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

async fn publish(
    f: &Fixture,
    session: &str,
    sha: &str,
    review: bool,
    checkout_root: Option<&Path>,
) -> Value {
    let (status, body) = request(
        &f.app,
        "POST",
        "/docs",
        Some(json!({
            "repo": REPO, "path": "specs/memo.md", "pr_number": 12,
            "commit_sha": sha.repeat(40), "session_id": session, "review": review,
            "checkout_root": checkout_root.map(|root| root.display().to_string()),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// `(kind, subject_id, title, body, reader_path)` of every notice.
fn notices(f: &Fixture) -> Vec<(String, String, String, String, String)> {
    let path = f.dir.join("owner_push.db");
    if !path.exists() {
        return Vec::new();
    }
    let conn = Connection::open(path).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT kind, subject_id, title, body, reader_path FROM owner_notices ORDER BY rowid",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

async fn waiting_on(f: &Fixture, session_id: &str) -> Value {
    let (_, feed) = request(&f.app, "GET", "/session-obligations", None).await;
    feed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["session_id"] == session_id)
        .map(|entry| entry["waiting_on"].clone())
        .unwrap_or(Value::Null)
}

/// A posted review of the doc's latest publish, delivered to `delivered_to`.
fn post_review(f: &Fixture, doc_id: &str, submission: &str, delivered_to: Option<&str>) {
    let store = OwnerDocStore::new(f.dir.join("message_queue.db"));
    store
        .begin_review(
            submission,
            doc_id,
            &"a".repeat(40),
            &git_blob_sha(MEMO),
            OwnerDocVerdict::ChangesRequested,
            Some("Tighten it."),
        )
        .unwrap();
    store
        .finish_review(
            submission,
            &PostedOwnerDocReview {
                github_review_id: Some(7),
                github_review_url: "https://github.com/acme/widgets/pull/12#pullrequestreview-7"
                    .into(),
                line_comment_count: 1,
                file_comment_count: 0,
                draft_ids: Vec::new(),
                wake: delivered_to.map(|id| (id.to_owned(), "[sm review] ...".to_owned())),
            },
        )
        .unwrap();
}

#[tokio::test]
async fn review_request_creates_notice() {
    let f = fixture();
    let plain = publish(&f, "author01", "a", false, None).await;
    assert_eq!(plain["owner_name"], "Rajesh");
    assert!(notices(&f).is_empty(), "a plain publish notifies nobody");
    let reviewed = publish(&f, "author01", "b", true, Some(&f.dir)).await;
    assert_eq!(
        reviewed["publish"]["checkout_root"],
        f.dir.display().to_string()
    );
    let publish_id = reviewed["publish"]["id"].as_i64().unwrap();
    assert_eq!(
        notices(&f),
        vec![(
            "review_requested".to_owned(),
            publish_id.to_string(),
            "author01-writer asks for your review".to_owned(),
            "Decision memo".to_owned(),
            "/docs/widgets/specs/memo.md?version=bbbbbbbbbbbb".to_owned(),
        )]
    );
    // Each --review publish gets its own notice.
    publish(&f, "author01", "c", true, None).await;
    assert_eq!(notices(&f).len(), 2);
    // A relative checkout root is dropped.
    let relative = publish(&f, "author01", "a", false, Some(Path::new("repo"))).await;
    assert_eq!(relative["publish"]["checkout_root"], Value::Null);
}

#[tokio::test]
async fn dismiss_review_clears_waiting() {
    let f = fixture();
    let doc = publish(&f, "author01", "a", true, None).await;
    let id = doc["id"].as_str().unwrap();
    let waiting = waiting_on(&f, "author01").await;
    assert_eq!(waiting[0]["kind"], "owner_review");
    let (status, _) = request(&f.app, "POST", &format!("/docs/{id}/dismiss-review"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(waiting_on(&f, "author01").await, json!([]));
    let (_, meta) = request(&f.app, "GET", &format!("/docs/{id}?format=json"), None).await;
    assert_eq!(meta["state"], "new");
    let (status, _) = request(&f.app, "POST", &format!("/docs/{id}/dismiss-review"), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "nothing left to dismiss");

    // A retired author's request is omitted from waiting on its own.
    let f = fixture();
    let retired = publish(&f, "retired1", "b", true, None).await;
    let (_, meta) = request(
        &f.app,
        "GET",
        &format!("/docs/{}?format=json", retired["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(meta["state"], "review_requested");
    assert_eq!(meta["author_session_id"], "retired1");
    assert_eq!(waiting_on(&f, "retired1").await, json!([]));
}

#[tokio::test]
async fn assign_undelivered_review_spawns_agent() {
    let f = fixture();
    let checkout = f.dir.join("checkout");
    fs::create_dir_all(&checkout).unwrap();
    let gone = f.dir.join("deleted-worktree");
    publish(&f, "retired1", "a", false, Some(&checkout)).await;
    let doc = publish(&f, "retired1", "a", true, Some(&gone)).await;
    let id = doc["id"].as_str().unwrap().to_owned();
    post_review(&f, &id, "sub-assign-1", None);
    let assign = format!("/docs/{id}/assign");

    let (status, body) = request(
        &f.app,
        "POST",
        &assign,
        Some(json!({"review_id": "sub-assign-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let session_id = body["session_id"].as_str().unwrap().to_owned();
    // The new agent: same provider, model and effort as the ended author,
    // in the newest checkout that still exists, no parent.
    let sessions: Value =
        serde_json::from_str(&fs::read_to_string(&f.state_file).unwrap()).unwrap();
    let session = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == session_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(session["provider"], "codex-fork");
    assert_eq!(session["model"], "gpt-5");
    assert_eq!(session["reasoning_effort"], "xhigh");
    assert_eq!(session["working_dir"], checkout.display().to_string());
    assert_eq!(session["parent_session_id"], Value::Null);
    let log = fs::read_to_string(session["log_file"].as_str().unwrap()).unwrap();
    assert!(
        log.contains(
            "[sm review] Your task is Rajesh's latest review of \"Decision memo\" on PR #12 in acme/widgets. \
             The agent that wrote the doc has ended, so the review is yours.\n\
             [sm review] Rajesh's review of \"Decision memo\" (PR #12 @ aaaaaaa) is here: \
             https://github.com/acme/widgets/pull/12#pullrequestreview-7\n\
             Verdict: changes requested · 1 line comment\n\
             Rajesh wrote:\n\
             > Tighten it.\n\
             Check out the PR branch in your own worktree, run `sm pr` from it to claim the PR, \
             address the review, push, and republish with `sm doc publish specs/memo.md --pr 12 --review`."
        ),
        "{log}"
    );
    // The review is now delivered: the banner goes, a second tap is a 409.
    let (_, meta) = request(&f.app, "GET", &format!("/docs/{id}?format=json"), None).await;
    assert_eq!(meta["review_undelivered"], false);
    assert_eq!(
        meta["reviews"][0]["delivered_to_session_id"],
        session_id.as_str()
    );
    let (status, body) = request(
        &f.app,
        "POST",
        &assign,
        Some(json!({"review_id": "sub-assign-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .starts_with("Review is already with "),
        "{body}"
    );
}

/// sm#1606: a review nobody was woken for stops waiting once a revision is
/// published after it, since some agent took it up. The flag clears and
/// Assign refuses, even if that agent has since ended too.
#[tokio::test]
async fn republish_after_undelivered_review_clears_it() {
    let f = fixture();
    let doc = publish(&f, "retired1", "a", true, Some(&f.dir)).await;
    let id = doc["id"].as_str().unwrap().to_owned();
    post_review(&f, &id, "sub-answer-1", None);
    let (_, meta) = request(&f.app, "GET", &format!("/docs/{id}?format=json"), None).await;
    assert_eq!(meta["review_undelivered"], true, "{meta}");

    publish(&f, "retired1", "b", true, Some(&f.dir)).await;
    let (_, meta) = request(&f.app, "GET", &format!("/docs/{id}?format=json"), None).await;
    assert_eq!(meta["review_undelivered"], false, "{meta}");
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/docs/{id}/assign"),
        Some(json!({"review_id": "sub-answer-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["detail"],
        "A newer revision was published after this review; reload"
    );
}

#[tokio::test]
async fn assign_refuses_delivered_or_unknown_checkout() {
    let f = fixture();
    // No checkout recorded.
    let doc = publish(&f, "retired1", "a", true, None).await;
    let id = doc["id"].as_str().unwrap().to_owned();
    let assign = format!("/docs/{id}/assign");
    post_review(&f, &id, "sub-old-0001", None);
    post_review(&f, &id, "sub-new-0001", None);
    let (status, body) = request(
        &f.app,
        "POST",
        &assign,
        Some(json!({"review_id": "sub-new-0001"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["detail"],
        "No local checkout is known for widgets; start the agent yourself."
    );
    // Not the latest review; not this doc's review.
    let (_, body) = request(
        &f.app,
        "POST",
        &assign,
        Some(json!({"review_id": "sub-old-0001"})),
    )
    .await;
    assert_eq!(body["detail"], "A newer review exists; reload");
    let (_, body) = request(
        &f.app,
        "POST",
        &assign,
        Some(json!({"review_id": "sub-nope-001"})),
    )
    .await;
    assert_eq!(body["detail"], "Review not found");

    // Another agent took the doc over and republished: it has an agent again.
    publish(&f, "author01", "b", false, Some(&f.dir)).await;
    let (_, body) = request(
        &f.app,
        "POST",
        &assign,
        Some(json!({"review_id": "sub-new-0001"})),
    )
    .await;
    assert_eq!(
        body["detail"],
        "The doc has an agent again: author01-writer"
    );

    // A delivered review.
    let other = fixture();
    let doc = publish(&other, "author01", "a", true, Some(&other.dir)).await;
    let id = doc["id"].as_str().unwrap().to_owned();
    post_review(&other, &id, "sub-deliv-01", Some("author01"));
    let (status, body) = request(
        &other.app,
        "POST",
        &format!("/docs/{id}/assign"),
        Some(json!({"review_id": "sub-deliv-01"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["detail"], "Review is already with author01-writer");
}
