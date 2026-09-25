//! Work claims over HTTP (sm#1452, ticket #1485): `/claims`, `sm spawn
//! --ticket`, retire, implicit claims and the session feed.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use sm_server::{
    config::{AppConfig, PathsConfig, SmSendConfig},
    http::{
        router, AppState, DocFetchError, DocPullRequest, GitHubReviewComment, GitHubReviewPoster,
        OwnerDocSource,
    },
    owner_docs::git_blob_sha,
    work_claims::{BatchFetch, GhItem, ItemFetch, WorkClaimStore, WorkItemSource, WorkKind},
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

const REPO: &str = "acme/widgets";

/// GitHub as a map; `acme/down` is unreachable.
#[derive(Clone, Default)]
struct StubItems {
    items: Arc<Mutex<BTreeMap<i64, GhItem>>>,
}

impl StubItems {
    fn put(&self, number: i64, kind: WorkKind, state: &str) {
        self.items.lock().unwrap().insert(
            number,
            GhItem {
                kind,
                title: format!("Item {number}"),
                state: state.into(),
                state_reason: None,
                url: format!("https://github.com/{REPO}/issues/{number}"),
                head_ref: None,
                head_sha: None,
                closed_at: None,
                merged_at: None,
                closing_refs: (kind == WorkKind::Pr).then(Vec::new),
            },
        );
    }
}

impl WorkItemSource for StubItems {
    fn fetch(&self, repo: &str, numbers: &[i64]) -> Result<BatchFetch, String> {
        if repo == "acme/down" {
            return Err("gh api graphql failed: timed out after 30s".into());
        }
        let items = self.items.lock().unwrap();
        Ok(numbers
            .iter()
            .map(|n| {
                (
                    *n,
                    items
                        .get(n)
                        .cloned()
                        .map_or(ItemFetch::NotFound, |item| ItemFetch::Found(Box::new(item))),
                )
            })
            .collect())
    }
}

#[derive(Default)]
struct StubDocs;

impl OwnerDocSource for StubDocs {
    fn fetch_doc(&self, _repo: &str, _path: &str, _sha: &str) -> Result<Vec<u8>, DocFetchError> {
        Ok(b"<html><title>Memo</title></html>".to_vec())
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
            state: "open".into(),
            head_sha: "a".repeat(40),
            url: format!("https://github.com/{repo}/pull/{pr_number}"),
        })
    }
}

struct StubPoster;

impl GitHubReviewPoster for StubPoster {
    fn post_initial_review_request(
        &self,
        _repo: &str,
        _pr_number: i64,
        _steer: Option<&str>,
    ) -> Result<GitHubReviewComment, String> {
        Ok(GitHubReviewComment {
            comment_id: Some(1),
            comment_url: Some("https://github.com/acme/widgets/pull/9#issuecomment-1".into()),
            posted_at: "2026-09-24T00:00:00Z".into(),
        })
    }

    fn current_open_pr_head(&self, _repo: &str, _pr_number: i64) -> Result<String, String> {
        Ok("1".repeat(40))
    }
}

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "sm-work-claims-http-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    app: axum::Router,
    state: AppState,
    items: StubItems,
    dir: PathBuf,
}

impl Fixture {
    fn store(&self) -> WorkClaimStore {
        WorkClaimStore::new(self.dir.join("message_queue.db"))
    }
}

/// lead → (eng1, eng2); other unrelated; asleep stopped.
fn fixture() -> Fixture {
    let dir = temp_dir();
    let state_file = dir.join("sessions.json");
    let session = |id: &str, parent: Option<&str>, status: &str| {
        json!({
            "id": id, "name": format!("claude-{id}"), "friendly_name": format!("{id}-agent"),
            "working_dir": "/repo", "tmux_session": format!("claude-{id}"),
            "log_file": "/tmp/claims.log", "status": status,
            "created_at": "2026-09-24T00:00:00Z", "last_activity": "2026-09-24T00:01:00Z",
            "parent_session_id": parent,
        })
    };
    fs::write(
        &state_file,
        json!({"sessions": [
            session("lead0001", None, "idle"),
            session("eng00001", Some("lead0001"), "running"),
            session("eng00002", Some("lead0001"), "running"),
            session("other001", None, "running"),
            session("asleep01", None, "stopped"),
        ]})
        .to_string(),
    )
    .unwrap();
    let mut config = AppConfig {
        paths: PathsConfig {
            state_file: state_file.display().to_string(),
        },
        sm_send: SmSendConfig {
            db_path: dir.join("message_queue.db").display().to_string(),
        },
        ..AppConfig::default()
    };
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.log_dir = Some(dir.join("logs").display().to_string());
    let items = StubItems::default();
    items.put(1, WorkKind::Ticket, "open");
    items.put(2, WorkKind::Ticket, "closed");
    items.put(9, WorkKind::Pr, "open");
    items.put(10, WorkKind::Pr, "merged");
    let state = AppState::new(config)
        .with_work_item_source(Arc::new(items.clone()))
        .with_owner_doc_source(Arc::new(StubDocs))
        .with_github_review_poster(Arc::new(StubPoster));
    // The DB exists before the router starts, as it does on the live server.
    WorkClaimStore::new(dir.join("message_queue.db"))
        .ensure_schema()
        .unwrap();
    Fixture {
        app: router(state.clone()),
        state,
        items,
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

async fn claim(
    f: &Fixture,
    who: &str,
    kind: &str,
    number: i64,
    extra: Value,
) -> (StatusCode, Value) {
    let mut body = json!({"requester_session_id": who, "kind": kind, "repo": REPO,
                          "number": number, "worktree_path": "/wt", "branch": "b"});
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    request(&f.app, "POST", "/claims", Some(body)).await
}

fn queued_texts(f: &Fixture, target: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(f.dir.join("message_queue.db")).unwrap();
    let mut statement = conn
        .prepare("SELECT text FROM message_queue WHERE target_session_id = ?1")
        .unwrap();
    statement
        .query_map([target], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

#[tokio::test]
async fn claims_status_codes_listing_and_release() {
    let f = fixture();
    let (status, body) = claim(&f, "eng00001", "ticket", 1, json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["outcome"], "claimed");
    assert_eq!(body["claim"]["title"], "Item 1");
    assert_eq!(body["claim"]["history_path"], "/t/widgets/1");
    assert_eq!(body["claim"]["worktree_path"], "/wt");

    let (status, body) = claim(&f, "eng00001", "ticket", 1, json!({})).await;
    assert_eq!(
        (status, body["outcome"].clone()),
        (StatusCode::OK, json!("already_held"))
    );

    // A sibling is not in the same line.
    let (status, body) = claim(&f, "eng00002", "ticket", 1, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["outcome"], "collision");
    assert_eq!(body["holders"][0]["session_id"], "eng00001");
    assert_eq!(body["holders"][0]["name"], "eng00001-agent");
    assert_eq!(body["holders"][0]["state"], "working");

    // The parent shares.
    let (status, body) = claim(&f, "lead0001", "ticket", 1, json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        body["notes"],
        json!(["Also held by your child eng00001-agent (eng00001)."])
    );

    // --take moves it and tells the holders.
    let (status, body) = claim(&f, "other001", "ticket", 1, json!({"take": true})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["outcome"], "taken");
    assert_eq!(
        queued_texts(&f, "eng00001"),
        vec!["[sm claim] other001-agent (other001) claimed ticket #1. Your claim on it ended."]
    );

    for (kind, number, detail) in [
        ("ticket", 9, "#9 is a pull request; use sm pr 9."),
        ("pr", 1, "#1 is a ticket; use sm ticket 1."),
        ("ticket", 2, "Ticket #2 is closed."),
        ("pr", 10, "PR #10 is merged."),
        ("ticket", 404, "No ticket or PR #404 in acme/widgets."),
    ] {
        let (status, body) = claim(&f, "eng00002", kind, number, json!({})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{number}");
        assert_eq!(body["detail"], detail);
    }
    let (status, body) = request(
        &f.app,
        "POST",
        "/claims",
        Some(
            json!({"requester_session_id": "eng00002", "kind": "ticket", "repo": "acme/down",
                    "number": 1}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        body["detail"],
        "Could not reach GitHub to check #1: gh api graphql failed: timed out after 30s. Nothing was recorded."
    );

    let (status, body) = request(&f.app, "GET", "/claims?session=other001&active=true", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["claims"].as_array().unwrap().len(), 1);
    assert_eq!(body["claims"][0]["number"], 1);

    let release = |who: &str| json!({"requester_session_id": who, "kind": "ticket", "repo": REPO, "number": 1});
    let (status, body) =
        request(&f.app, "POST", "/claims/release", Some(release("eng00002"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["detail"], "You don't hold ticket #1.");
    let (status, body) =
        request(&f.app, "POST", "/claims/release", Some(release("other001"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["claim"]["end_reason"], "released");

    let (status, _) = request(
        &f.app,
        "POST",
        "/claims",
        Some(json!({"requester_session_id": "nobody00", "kind": "ticket", "repo": REPO, "number": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn spawn_with_a_ticket_claims_before_the_first_turn_or_creates_nothing() {
    let f = fixture();
    claim(&f, "eng00001", "ticket", 1, json!({})).await;
    let spawn = |parent: &str, id: &str| {
        json!({"id": id, "parent_session_id": parent, "prompt": "work #1", "name": id,
               "provider": "claude", "ticket": 1, "ticket_repo": REPO,
               "ticket_worktree_path": "/wt"})
    };
    // An unrelated live holder refuses the spawn; no session is created.
    let (status, body) = request(
        &f.app,
        "POST",
        "/sessions/spawn",
        Some(spawn("other001", "kid00001")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["outcome"], "collision");
    let (status, _) = request(&f.app, "GET", "/sessions/kid00001", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(f
        .store()
        .claims_for_session("kid00001", false)
        .unwrap()
        .is_empty());

    // The spawner holds the ticket: its child shares it, source spawn.
    let (status, body) = request(
        &f.app,
        "POST",
        "/sessions/spawn",
        Some(spawn("eng00001", "kid00002")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["session_id"], "kid00002");
    assert_eq!(body["ticket_claim"]["claim"]["source"], "spawn");
    assert_eq!(
        body["ticket_claim"]["notes"],
        json!(["Also held by your parent eng00001-agent (eng00001)."])
    );
    let claims = f.store().claims_for_session("kid00002", true).unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].claim.reserved_at, None);
    assert_eq!(claims[0].claim.worktree_path.as_deref(), Some("/wt"));

    // Retire ends the claims.
    let (status, body) =
        request(&f.app, "POST", "/sessions/kid00002/retire", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ended = f.store().claims_for_session("kid00002", false).unwrap();
    assert_eq!(ended[0].claim.end_reason.as_deref(), Some("retired"));

    // A failed session creation (the id is taken) deletes the reservation.
    let (status, body) = request(
        &f.app,
        "POST",
        "/sessions/spawn",
        Some(spawn("eng00001", "eng00002")),
    )
    .await;
    assert!(!status.is_success(), "{body}");
    assert!(f
        .store()
        .claims_for_item(REPO, 1)
        .unwrap()
        .iter()
        .all(|c| c.session_id != "eng00002"));
}

#[tokio::test]
async fn implicit_claims_from_doc_publish_and_codex_review_and_the_feed() {
    let f = fixture();
    claim(&f, "eng00001", "pr", 9, json!({})).await;
    let (status, body) = request(
        &f.app,
        "POST",
        "/docs",
        Some(
            json!({"repo": REPO, "path": "specs/memo.html", "commit_sha": "c".repeat(40),
                    "pr_number": 9, "session_id": "other001"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["claim_warning"],
        "Warning: PR #9 is also held by eng00001-agent (eng00001), working."
    );
    let pr9 = f.store().claims_for_item(REPO, 9).unwrap();
    assert_eq!(pr9[1].source, "doc_publish");
    assert_eq!(pr9[1].session_id, "other001");

    let (status, body) = request(
        &f.app,
        "POST",
        "/codex-review-requests",
        Some(json!({"pr_number": 9, "repo": REPO, "requester_session_id": "lead0001"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // lead0001 shares with its child and collides with other001.
    assert_eq!(
        body["claim_warning"],
        "Warning: PR #9 is also held by other001-agent (other001), working."
    );
    assert!(f
        .store()
        .claims_for_item(REPO, 9)
        .unwrap()
        .iter()
        .any(|c| c.session_id == "lead0001" && c.source == "codex_review"));

    claim(&f, "eng00001", "ticket", 1, json!({})).await;
    let (status, feed) = request(&f.app, "GET", "/session-obligations", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(feed["schema_version"], 3);
    let eng = feed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == "eng00001")
        .unwrap();
    assert_eq!(
        eng["claims"],
        json!([
            {"kind": "pr", "repo": REPO, "number": 9, "title": "Item 9", "state": "open",
             "claimed_at": eng["claims"][0]["claimed_at"], "source": "explicit",
             "history_path": "/t/widgets/9"},
            {"kind": "ticket", "repo": REPO, "number": 1, "title": "Item 1", "state": "open",
             "claimed_at": eng["claims"][1]["claimed_at"], "source": "explicit",
             "history_path": "/t/widgets/1"},
        ])
    );
}

#[tokio::test]
async fn the_sync_pass_backfills_ends_claims_and_skips_closed_items() {
    let f = fixture();
    claim(&f, "eng00001", "ticket", 1, json!({})).await;
    claim(&f, "eng00002", "pr", 9, json!({})).await;
    f.items.put(1, WorkKind::Ticket, "closed");
    f.items.put(9, WorkKind::Pr, "merged");
    f.state.run_work_claims_sync_pass().unwrap();
    let store = f.store();
    assert_eq!(
        store.claims_for_item(REPO, 1).unwrap()[0]
            .end_reason
            .as_deref(),
        Some("closed")
    );
    assert_eq!(
        store.claims_for_item(REPO, 9).unwrap()[0]
            .end_reason
            .as_deref(),
        Some("merged")
    );
    assert!(store.tracked_items().unwrap().is_empty());
    // A second pass is a no-op: backfill ran once.
    f.state.run_work_claims_sync_pass().unwrap();
    assert_eq!(store.claims_for_item(REPO, 1).unwrap().len(), 1);
}
