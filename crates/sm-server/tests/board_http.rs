//! sm board over HTTP (sm#1665, tickets #1681 and #1682): lane adds, link
//! writes, the owner-only routes, Needs you from messages and review
//! requests, board alerts, and the queue's lane fields.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sm_server::{
    board::{
        model::{Key, PrRef},
        sync::{
            BoardSource, IssueConnection, IssueNode, IssuesPage, LinkMutation, RefNode,
            ResolvedIssue, WriteError,
        },
    },
    config::{AppConfig, EmailConfig, PathsConfig, QueueRunnerConfig, SmSendConfig},
    http::{router, AppState, DocFetchError, DocPullRequest, OwnerDocSource},
    owner_docs::git_blob_sha,
    owner_push::{PushError, PushSender},
    work_claims::WorkClaimStore,
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

/// An issue: open, the numbers it is blocked by, its parent.
type FakeIssue = (bool, Vec<i64>, Option<i64>);

/// GitHub as open/closed issues with blocked-by and parent links.
#[derive(Clone, Default)]
struct FakeBoard {
    issues: Arc<Mutex<BTreeMap<i64, FakeIssue>>>,
    /// PRs GitHub links to each issue.
    prs: Arc<Mutex<BTreeMap<i64, Vec<PrRef>>>>,
}

impl FakeBoard {
    fn node(&self, number: i64) -> RefNode {
        let open = self.issues.lock().unwrap()[&number].0;
        RefNode {
            repo: REPO.into(),
            number,
            title: format!("Ticket {number}"),
            url: format!("https://github.com/{REPO}/issues/{number}"),
            state: if open { "open" } else { "closed" }.into(),
            state_reason: None,
            closed_at: None,
        }
    }
}

impl BoardSource for FakeBoard {
    fn issues_page(&self, repo: &str, _cursor: Option<&str>) -> Result<IssuesPage, String> {
        if repo != REPO {
            return Ok(IssuesPage::default());
        }
        let issues = self.issues.lock().unwrap().clone();
        let prs = self.prs.lock().unwrap().clone();
        let nodes = issues
            .iter()
            .filter(|(_, (open, _, _))| *open)
            .map(|(number, (_, blocked_by, parent))| IssueNode {
                number: *number,
                title: format!("Ticket {number}"),
                body: None,
                url: String::new(),
                updated_at: None,
                state_reason: None,
                parent: parent.map(|parent| self.node(parent)),
                blocked_by: blocked_by.iter().map(|n| self.node(*n)).collect(),
                blocked_by_more: None,
                sub_issues: issues
                    .iter()
                    .filter(|(_, (_, _, p))| *p == Some(*number))
                    .map(|(child, _)| self.node(*child))
                    .collect(),
                sub_issues_more: None,
                prs: prs.get(number).cloned().unwrap_or_default(),
            })
            .collect();
        Ok(IssuesPage {
            nodes,
            ..IssuesPage::default()
        })
    }

    fn connection_page(
        &self,
        _repo: &str,
        _number: i64,
        _connection: IssueConnection,
        _cursor: &str,
    ) -> Result<(Vec<RefNode>, Option<String>), String> {
        Ok((Vec::new(), None))
    }

    fn items(
        &self,
        _repo: &str,
        numbers: &[i64],
    ) -> Result<BTreeMap<i64, Option<RefNode>>, String> {
        Ok(numbers
            .iter()
            .map(|number| {
                let exists = self.issues.lock().unwrap().contains_key(number);
                (*number, exists.then(|| self.node(*number)))
            })
            .collect())
    }

    fn resolve(&self, issues: &[Key]) -> Result<Vec<Option<ResolvedIssue>>, String> {
        Ok(issues
            .iter()
            .map(|(_, number)| {
                let (_, blocked_by, parent) = self.issues.lock().unwrap().get(number).cloned()?;
                Some(ResolvedIssue {
                    id: number.to_string(),
                    node: self.node(*number),
                    parent: parent.map(|p| (REPO.to_owned(), p)),
                    blocked_by: blocked_by.iter().map(|n| (REPO.to_owned(), *n)).collect(),
                })
            })
            .collect())
    }

    fn write_link(&self, mutation: &LinkMutation) -> Result<(), WriteError> {
        let mut issues = self.issues.lock().unwrap();
        match mutation {
            LinkMutation::AddBlockedBy {
                issue_id,
                blocking_id,
            } => issues
                .get_mut(&issue_id.parse().unwrap())
                .unwrap()
                .1
                .push(blocking_id.parse().unwrap()),
            LinkMutation::AddSubIssue {
                parent_id,
                child_id,
            } => issues.get_mut(&child_id.parse().unwrap()).unwrap().2 = parent_id.parse().ok(),
            _ => return Err(WriteError::Refused("not in this fake".into())),
        }
        Ok(())
    }
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

const BRIDGE: &str = r#"humans:
  owner:
    display_name: "Human operator"
    aliases: ["ownergh"]
    default_channel: "email"
    channels:
      email:
        enabled: true
        address: "operator@example.com"
"#;

struct Fixture {
    app: axum::Router,
    state: AppState,
    board: FakeBoard,
    dir: PathBuf,
}

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "sm-board-http-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Tickets: 1 the goal; 2, 3, 4 under it. Sessions: eng00001 and
/// eng00002 live, gone0001 retired.
fn fixture() -> Fixture {
    fixture_with_push(None)
}

fn fixture_with_push(sender: Option<Arc<RecordingSender>>) -> Fixture {
    fixture_options(sender, |_| {})
}

fn fixture_config(configure: impl FnOnce(&mut AppConfig)) -> Fixture {
    fixture_options(None, configure)
}

fn fixture_options(
    sender: Option<Arc<RecordingSender>>,
    configure: impl FnOnce(&mut AppConfig),
) -> Fixture {
    let dir = temp_dir();
    let state_file = dir.join("sessions.json");
    let session = |id: &str, completion: Option<&str>| {
        let mut record = json!({
            "id": id, "name": format!("claude-{id}"), "friendly_name": format!("{id}-agent"),
            "working_dir": "/repo", "tmux_session": format!("claude-{id}"),
            "log_file": dir.join(format!("{id}.log")).display().to_string(),
            "status": if completion.is_some() { "stopped" } else { "running" },
            "created_at": "2026-09-24T00:00:00Z", "last_activity": "2026-09-24T00:01:00Z",
        });
        if let Some(completion) = completion {
            record["completion_status"] = json!(completion);
        }
        record
    };
    let mut idle = session("idle0001", None);
    idle["status"] = json!("idle");
    fs::write(
        &state_file,
        json!({"sessions": [
            session("eng00001", None),
            session("eng00002", None),
            session("gone0001", Some("retired")),
            idle,
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
        queue_runner: QueueRunnerConfig {
            state_dir: dir.join("queue").display().to_string(),
            configured: true,
            ..QueueRunnerConfig::default()
        },
        ..AppConfig::default()
    };
    config.board.repos = vec![REPO.to_owned()];
    config.push.db_path = dir.join("owner_push.db").display().to_string();
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.log_dir = Some(dir.join("logs").display().to_string());
    let board = FakeBoard::default();
    {
        let mut issues = board.issues.lock().unwrap();
        issues.insert(1, (true, Vec::new(), None));
        for number in [2, 3, 4] {
            issues.insert(number, (true, Vec::new(), Some(1)));
        }
        issues.insert(9, (false, Vec::new(), None));
    }
    configure(&mut config);
    let state = AppState::new(config)
        .with_work_item_source(Arc::new(BoardClaimItems))
        .with_board_source(Arc::new(board.clone()))
        .with_owner_doc_source(Arc::new(StubDocs))
        .with_push_sender(sender.map(|sender| sender as Arc<dyn PushSender>));
    WorkClaimStore::new(dir.join("message_queue.db"))
        .ensure_schema()
        .unwrap();
    Fixture {
        app: router(state.clone()),
        state,
        board,
        dir,
    }
}

impl Fixture {
    fn claim(&self, number: i64, session_id: &str) {
        Connection::open(self.dir.join("message_queue.db"))
            .unwrap()
            .execute(
                "INSERT INTO work_claims (id, repo, number, kind, session_id, source, claimed_at)
                 VALUES (?1, ?2, ?3, 'ticket', ?4, 'explicit', '2026-09-29T10:00:00Z')",
                params![format!("c{number}{session_id}"), REPO, number, session_id],
            )
            .unwrap();
    }

    fn link_pr(&self, pr: i64, ticket: i64) {
        Connection::open(self.dir.join("message_queue.db"))
            .unwrap()
            .execute(
                "INSERT INTO work_links (repo, pr_number, ticket_number, source, created_at)
                 VALUES (?1, ?2, ?3, 'closing_ref', '2026-09-29T10:00:00Z')",
                params![REPO, pr, ticket],
            )
            .unwrap();
    }

    async fn ticket(&self, number: i64) -> Value {
        let (status, board) = request(&self.app, "GET", "/board?format=json", None).await;
        assert_eq!(status, StatusCode::OK, "{board}");
        board["lanes"][0]["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|ticket| ticket["number"] == number)
            .cloned()
            .unwrap()
    }

    async fn badge(&self) -> i64 {
        let (status, body) = request(&self.app, "GET", "/client/board/badge", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["count"].as_i64().unwrap()
    }

    async fn message(&self, sender: &str, text: &str) -> String {
        let (status, body) = request(
            &self.app,
            "POST",
            "/humans/ownergh/messages",
            Some(json!({"sender_session_id": sender, "text": text, "blocking": true})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    async fn publish(&self, session: &str, sha: &str, review: bool) {
        let (status, body) = request(
            &self.app,
            "POST",
            "/docs",
            Some(json!({
                "repo": REPO, "path": "specs/memo.md", "pr_number": 12,
                "commit_sha": sha.repeat(40), "session_id": session, "review": review,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    fn board_notices(&self) -> usize {
        let path = self.dir.join("owner_push.db");
        if !path.exists() {
            return 0;
        }
        Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM owner_notices WHERE kind LIKE 'board%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap() as usize
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

async fn add_goal(f: &Fixture) -> Value {
    let (status, body) = request(
        &f.app,
        "POST",
        "/board/lanes",
        Some(json!({"repo": REPO, "number": 1, "session_id": "eng00001"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn lane_add_duplicate_409() {
    let f = fixture();
    let body = add_goal(&f).await;
    assert_eq!(
        body["message"],
        "Lane 1: acme/widgets#1 Ticket 1. Added at the bottom; Owner sets the order."
    );
    assert_eq!(body["lane"]["rank"], 1);
    assert_eq!(body["lane"]["added_by_name"], "eng00001-agent");
    assert_eq!(body["lane"]["counts"]["ready"], 3);
    let (status, body) = request(
        &f.app,
        "POST",
        "/board/lanes",
        Some(json!({"repo": REPO, "number": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["detail"], "Lane 1 already has goal acme/widgets#1");
    assert_eq!(body["lane"]["goal"]["number"], 1);
    let (status, body) = request(
        &f.app,
        "POST",
        "/board/lanes",
        Some(json!({"repo": REPO, "number": 9})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["detail"], "#9 is closed");
    let (status, _) = request(
        &f.app,
        "POST",
        "/board/lanes",
        Some(json!({"repo": REPO, "number": 77})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn link_route_outcomes() {
    let f = fixture();
    add_goal(&f).await;
    let body = json!({"repo": REPO, "number": 3, "target_repo": REPO, "target_number": 2,
                      "kind": "after", "session_id": "eng00002"});
    let (status, response) = request(&f.app, "POST", "/board/links", Some(body.clone())).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["outcome"], "recorded");
    assert_eq!(response["message"], "Recorded: #3 starts after #2.");
    assert_eq!(f.board.issues.lock().unwrap()[&3].1, vec![2]);
    // The board shows it at once.
    assert_eq!(f.ticket(3).await["state"], "blocked");
    let (_, response) = request(&f.app, "POST", "/board/links", Some(body)).await;
    assert_eq!(response["outcome"], "already");
    let (_, board) = request(&f.app, "GET", "/board?format=json", None).await;
    assert_eq!(
        board["lanes"][0]["changes"][0]["text"],
        "#3 now starts after #2 (eng00002-agent)"
    );
    let (status, response) = request(
        &f.app,
        "POST",
        "/board/links",
        Some(
            json!({"repo": REPO, "number": 3, "target_repo": REPO, "target_number": 2,
                    "kind": "after", "remove": true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{response}");
    assert_eq!(response["detail"], "not in this fake");
    let (status, _) = request(
        &f.app,
        "POST",
        "/board/links",
        Some(
            json!({"repo": REPO, "number": 3, "target_repo": REPO, "target_number": 2,
                    "kind": "sideways"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn owner_routes_reject_agent_callers() {
    let f = fixture();
    add_goal(&f).await;
    for (method, uri, body) in [
        ("PUT", "/client/board/order", Some(json!({"lane_ids": [1]}))),
        (
            "POST",
            "/client/board/lanes",
            Some(json!({"repo": REPO, "number": 2})),
        ),
        ("DELETE", "/client/board/lanes/1", None),
    ] {
        let (status, body) = request(&f.app, method, uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
    }
    // Reads and the refresh are not writes to the order.
    let (status, board) = request(&f.app, "GET", "/client/board", None).await;
    assert_eq!(status, StatusCode::OK, "{board}");
    assert_eq!(board["lanes"][0]["rank"], 1);
    let (status, _) = request(&f.app, "POST", "/client/board/refresh", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn blocking_message_from_holder_is_needs_you() {
    let f = fixture();
    add_goal(&f).await;
    f.claim(2, "eng00001");
    assert_eq!(f.ticket(2).await["state"], "in_progress");
    let id = f.message("eng00001", "# Which bound?\n150 or 128?").await;
    let ticket = f.ticket(2).await;
    assert_eq!(ticket["state"], "needs_you");
    assert_eq!(ticket["needs_you"]["kind"], "message");
    assert_eq!(ticket["needs_you"]["text"], "Which bound?");
    assert_eq!(ticket["needs_you"]["url"], format!("/messages/{id}"));
    // Only a holder's messages count.
    f.message("eng00002", "# Unrelated?\nx").await;
    assert_eq!(f.ticket(3).await["state"], "ready");

    // The Board count follows the recompute and clears when seen; no
    // notice is created for it.
    f.state.run_board_pass().unwrap();
    assert_eq!(f.badge().await, 1);
    let (_, board) = request(&f.app, "GET", "/board?format=json", None).await;
    assert_eq!(board["unseen"]["count"], 1);
    assert_eq!(board["lanes"][0]["unseen"], true);
    let (status, _) = request(&f.app, "POST", "/client/board/seen", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(f.badge().await, 0);
    assert_eq!(f.board_notices(), 0);
}

#[tokio::test]
async fn replied_or_handled_message_clears() {
    let f = fixture();
    add_goal(&f).await;
    f.claim(2, "eng00001");
    let first = f.message("eng00001", "# Keep it?\nx").await;
    assert_eq!(f.ticket(2).await["state"], "needs_you");
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/messages/{first}/reply"),
        Some(json!({"submission_id": "sub-board-01", "body": "Keep it."})),
    )
    .await;
    assert!(status.is_success(), "{status}");
    assert_eq!(f.ticket(2).await["state"], "in_progress");
    let second = f.message("eng00001", "# Drop it?\nx").await;
    assert_eq!(f.ticket(2).await["state"], "needs_you");
    let (status, _) = request(&f.app, "POST", &format!("/messages/{second}/handled"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(f.ticket(2).await["state"], "in_progress");
}

#[tokio::test]
async fn ended_sender_clears() {
    let f = fixture();
    add_goal(&f).await;
    f.claim(3, "gone0001");
    f.message("gone0001", "# Still there?\nx").await;
    let ticket = f.ticket(3).await;
    // A retired sender waits on nobody, and holds nothing.
    assert_eq!(ticket["state"], "ready", "{ticket}");
}

#[tokio::test]
async fn review_request_by_holder_is_needs_you() {
    let f = fixture();
    add_goal(&f).await;
    f.claim(2, "eng00001");
    f.publish("eng00001", "a", true).await;
    let ticket = f.ticket(2).await;
    assert_eq!(ticket["state"], "needs_you");
    assert_eq!(ticket["needs_you"]["kind"], "review");
    assert_eq!(ticket["needs_you"]["text"], "PR #12 waits for your review");
}

#[tokio::test]
async fn review_request_via_pr_link_is_needs_you() {
    let f = fixture();
    add_goal(&f).await;
    f.link_pr(12, 4);
    f.publish("eng00002", "b", true).await;
    assert_eq!(f.ticket(4).await["state"], "needs_you");
    assert_eq!(f.ticket(3).await["state"], "ready");
}

#[tokio::test]
async fn superseded_or_reviewed_publish_clears() {
    let f = fixture();
    add_goal(&f).await;
    f.claim(2, "eng00001");
    f.publish("eng00001", "a", true).await;
    assert_eq!(f.ticket(2).await["state"], "needs_you");
    // A later revision published without --review is the latest publish.
    f.publish("eng00001", "c", false).await;
    assert_eq!(f.ticket(2).await["state"], "in_progress");
}

// ---- Alerts and the queue (ticket #1682) -----------------------------------

async fn pass(f: &Fixture) {
    let state = f.state.clone();
    tokio::task::spawn_blocking(move || state.run_board_pass().map(|_| ()))
        .await
        .unwrap()
        .unwrap();
}

async fn follow_pass(f: &Fixture) {
    let state = f.state.clone();
    tokio::task::spawn_blocking(move || state.run_follow_pass(false))
        .await
        .unwrap()
        .unwrap();
}

fn close(f: &Fixture, number: i64) {
    f.board.issues.lock().unwrap().get_mut(&number).unwrap().0 = false;
}

fn sent_kinds(sender: &RecordingSender) -> Vec<(String, String)> {
    sender
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|data| (data["kind"].clone(), data["notice_id"].clone()))
        .collect()
}

#[tokio::test]
async fn board_seen_acks_and_withdraws() {
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
    // #3 starts after #2.
    f.board
        .issues
        .lock()
        .unwrap()
        .get_mut(&3)
        .unwrap()
        .1
        .push(2);
    f.claim(2, "eng00001");
    let lane = add_goal(&f).await["lane"]["id"].as_i64().unwrap();
    assert_eq!(f.badge().await, 0);
    close(&f, 2);
    pass(&f).await;
    // One notice: the Board count and the lane's edge show it.
    assert_eq!(f.board_notices(), 1);
    assert_eq!(f.badge().await, 1);
    let (_, board) = request(&f.app, "GET", "/client/board", None).await;
    assert_eq!(board["unseen"]["lane_ids"], json!([lane]));
    assert_eq!(board["lanes"][0]["unseen"], true);
    // The follow worker pushes it; the phone shows it.
    follow_pass(&f).await;
    let kinds = sent_kinds(&sender);
    assert_eq!(kinds.len(), 1);
    assert_eq!(kinds[0].0, "board_ready");
    let notice_id = kinds[0].1.clone();
    let data = sender.sent.lock().unwrap()[0].clone();
    assert_eq!(data["title"], "Ready in lane 1, Ticket 1");
    assert_eq!(data["body"], "#3 can start — #2 closed");
    assert_eq!(data["reader_path"], format!("/board#lane-{lane}"));
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/client/notices/{notice_id}/ack"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    follow_pass(&f).await;
    assert_eq!(sent_kinds(&sender).len(), 1, "shown, not opened: it stays");
    // Opening the board takes it off the phone.
    let (status, _) = request(&f.app, "POST", "/client/board/seen", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(f.badge().await, 0);
    follow_pass(&f).await;
    follow_pass(&f).await;
    assert_eq!(
        sent_kinds(&sender),
        vec![
            ("board_ready".to_owned(), notice_id.clone()),
            ("withdraw".to_owned(), notice_id),
        ]
    );
}

#[tokio::test]
async fn unseen_count_matches_notices() {
    let f = fixture();
    f.board
        .issues
        .lock()
        .unwrap()
        .get_mut(&3)
        .unwrap()
        .1
        .push(2);
    f.board
        .issues
        .lock()
        .unwrap()
        .get_mut(&4)
        .unwrap()
        .1
        .push(2);
    f.claim(2, "eng00001");
    add_goal(&f).await;
    close(&f, 2);
    pass(&f).await;
    // #3 and #4 in one alert; a Needs-you ticket adds one more.
    assert_eq!(f.board_notices(), 1);
    f.claim(3, "eng00002");
    f.message("eng00002", "# Which fills table?\nx").await;
    pass(&f).await;
    assert_eq!(f.board_notices(), 1, "Needs you creates no notice");
    assert_eq!(f.badge().await, 2);
    let (status, _) = request(&f.app, "POST", "/client/board/seen", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(f.badge().await, 0);
}

#[tokio::test]
async fn lane_done_notice_survives_lane_end() {
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
    add_goal(&f).await;
    close(&f, 1);
    pass(&f).await;
    let (_, board) = request(&f.app, "GET", "/client/board", None).await;
    assert_eq!(board["lanes"], json!([]), "the lane ended");
    follow_pass(&f).await;
    let sent = sender.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["kind"], "board_lane_done");
    assert_eq!(sent[0]["title"], "Lane done: Ticket 1");
    assert_eq!(sent[0]["body"], "widgets#1 closed · lanes below move up");
    assert_eq!(f.badge().await, 1);
}

#[tokio::test]
async fn queue_job_carries_its_lane() {
    let f = fixture();
    add_goal(&f).await;
    f.claim(3, "eng00001");
    let submit = |requester: &str| {
        json!({
            "type": "tests", "argv": ["true"], "cwd": f.dir.display().to_string(),
            "requester_session_id": requester, "timeout_seconds": 60,
        })
    };
    let (status, job) = request(&f.app, "POST", "/queue-jobs", Some(submit("eng00001"))).await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["lane_rank"], 1);
    assert_eq!(
        job["lane_goal"],
        json!({"repo": REPO, "number": 1, "title": "Ticket 1"})
    );
    // No claim, and a cwd in no claimed worktree: no lane.
    let (status, job) = request(&f.app, "POST", "/queue-jobs", Some(submit("eng00002"))).await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["lane_rank"], Value::Null);
    assert_eq!(job["lane_goal"], Value::Null);
}

#[tokio::test]
async fn clocked_tickets_carry_ball_text_and_segments() {
    let f = fixture();
    add_goal(&f).await;
    f.claim(2, "eng00001");
    f.claim(3, "idle0001");
    let (status, body) = request(&f.app, "GET", "/client/board?clock_hours=5", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["detail"], "clock_hours must be 3, 6 or 24");

    let (status, board) = request(&f.app, "GET", "/client/board?clock_hours=6", None).await;
    assert_eq!(status, StatusCode::OK, "{board}");
    let ticket = |number: i64| {
        board["lanes"][0]["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|ticket| ticket["number"] == number)
            .cloned()
            .unwrap()
    };
    // A working holder with no turn-start hook: since its last activity.
    let working = ticket(2)["clock"].clone();
    assert_eq!(working["ball"], "working", "{working}");
    assert_eq!(working["since"], "2026-09-24T00:01:00Z");
    assert!(working["text"]
        .as_str()
        .unwrap()
        .starts_with("Agent working "));
    // Its open turn is not recorded yet, so it fills the six-hour strip.
    let segments = working["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 1, "{working}");
    assert_eq!(segments[0]["kind"], "working");
    let at = |value: &Value| {
        time::OffsetDateTime::parse(
            value.as_str().unwrap(),
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap()
    };
    assert_eq!(
        at(&segments[0]["to"]) - at(&segments[0]["from"]),
        time::Duration::hours(6)
    );
    // Idle for days with nothing running: stalled.
    let idle = ticket(3)["clock"].clone();
    assert_eq!(idle["ball"], "stalled", "{idle}");
    assert!(idle["text"]
        .as_str()
        .unwrap()
        .ends_with(": agent idle, nothing running"));
    // Ready tickets have no clock.
    assert_eq!(ticket(4)["state"], "ready");
    assert!(ticket(4).get("clock").is_none());

    // The idle holder's job is the ticket's by its claim.
    let (status, job) = request(
        &f.app,
        "POST",
        "/queue-jobs",
        Some(json!({
            "type": "tests", "argv": ["true"], "cwd": f.dir.display().to_string(),
            "requester_session_id": "idle0001", "timeout_seconds": 3600,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    let clock = f.ticket(3).await["clock"].clone();
    assert_eq!(clock["ball"], "queue", "{clock}");
    assert_eq!(clock["text"], "Job waiting 0m · 1st in line");
}

struct BoardClaimItems;
impl sm_server::work_claims::WorkItemSource for BoardClaimItems {
    fn fetch(
        &self,
        repo: &str,
        numbers: &[i64],
    ) -> Result<sm_server::work_claims::BatchFetch, String> {
        use sm_server::work_claims::{GhItem, ItemFetch, WorkKind};
        Ok(numbers
            .iter()
            .map(|n| {
                (
                    *n,
                    ItemFetch::Found(Box::new(GhItem {
                        kind: WorkKind::Ticket,
                        title: format!("Ticket {n}"),
                        state: "open".into(),
                        state_reason: None,
                        url: format!("https://github.com/{repo}/issues/{n}"),
                        head_ref: None,
                        head_sha: None,
                        closed_at: None,
                        merged_at: None,
                        closing_refs: None,
                        is_draft: false,
                    })),
                )
            })
            .collect())
    }
}

fn start_fixture() -> Fixture {
    fixture_config(|config| {
        config.google_auth.session_cookie_secret = Some("board-test-secret".into());
        config.google_auth.allowlist_emails = vec!["owner@example.com".into()];
        config.board.checkouts.insert(REPO.into(), "/tmp".into());
    })
}

fn owner_token() -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let body = URL_SAFE_NO_PAD.encode(json!({"v":1,"type":"device_access","email":"owner@example.com","name":"Owner","iat":now,"exp":now+3600}).to_string());
    let mut mac = Hmac::<Sha256>::new_from_slice(b"board-test-secret").unwrap();
    mac.update(body.as_bytes());
    format!(
        "smat_{body}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

async fn owner_request(
    f: &Fixture,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {}", owner_token()))
        .header("content-type", "application/json")
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 49152))))
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .unwrap();
    let response = f.app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn start_body(number: i64) -> Value {
    json!({"repo":REPO,"number":number,"provider":"claude","model":"opus[1m]","reasoning_effort":"high"})
}

#[tokio::test]
async fn board_html_is_the_web_apps_alone() {
    let f = fixture();
    add_goal(&f).await;
    let page = |uri: &str| {
        Request::builder()
            .uri(uri)
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 49152))))
            .body(Body::empty())
            .unwrap()
    };
    // Off the browser hostname `/board` has no page; the phone has its own tab.
    let response = f.app.clone().oneshot(page("/board")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // The remaining server pages no longer link to a Board page.
    let response = f.app.clone().oneshot(page("/history")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!html.contains("href=\"/board\""));
    assert!(!html.contains("board-badge"));
    let (_, json) = request_json_board(&f).await;
    assert_eq!(json["lanes"][0]["goal"]["number"], 1);
}

async fn request_json_board(f: &Fixture) -> (StatusCode, Value) {
    request(&f.app, "GET", "/board?format=json", None).await
}

#[tokio::test]
async fn start_refuses_needs_you_and_held_ticket() {
    let f = start_fixture();
    add_goal(&f).await;
    f.claim(2, "eng00001");
    let (status, body) =
        owner_request(&f, "POST", "/client/board/start", Some(start_body(2))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("eng00001-agent"));
    f.message("eng00001", "Please decide").await;
    let (status, body) =
        owner_request(&f, "POST", "/client/board/start", Some(start_body(2))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["detail"], "#2 is waiting on you");
}

#[tokio::test]
async fn auto_start_routes_store_blocked_choice_and_lane_is_all_or_nothing() {
    let f = start_fixture();
    add_goal(&f).await;
    f.board
        .issues
        .lock()
        .unwrap()
        .get_mut(&2)
        .unwrap()
        .1
        .push(3);
    pass(&f).await;
    assert_eq!(f.ticket(2).await["state"], "blocked");
    let choice = |number| {
        json!({"repo":REPO,"number":number,"agent_type":"Mid",
        "provider":"claude","model":"opus[1m]","reasoning_effort":"high"})
    };
    let (status, _) = request(&f.app, "PUT", "/client/board/auto-start", Some(choice(2))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) =
        owner_request(&f, "PUT", "/client/board/auto-start", Some(choice(2))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(f.ticket(2).await["auto_start"]["state"], "waiting");
    assert_eq!(f.ticket(2).await["auto_start"]["agent_type"], "Mid");
    assert!(f.ticket(2).await["auto_start"]["brief"].is_null());
    let mut edited = choice(2);
    edited["brief"] = json!("Read the spec first.");
    let (status, body) = owner_request(&f, "PUT", "/client/board/auto-start", Some(edited)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        f.ticket(2).await["auto_start"]["brief"],
        "Read the spec first."
    );
    let (status, _) = owner_request(
        &f,
        "DELETE",
        "/client/board/auto-start?repo=acme/widgets&number=2",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.ticket(2).await["auto_start"].is_null());
    f.claim(3, "eng00001");
    let lane = json!({"goal_repo":REPO,"goal_number":1,"tickets":[choice(2),choice(3)]});
    let (status, body) =
        owner_request(&f, "PUT", "/client/board/auto-start/lane", Some(lane)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(f.ticket(2).await["auto_start"].is_null());
}

#[tokio::test]
async fn start_reserves_claim_and_creates_root_session_and_holder_can_setup() {
    let f = start_fixture();
    add_goal(&f).await;
    let (status, body) =
        owner_request(&f, "POST", "/client/board/start", Some(start_body(2))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "widgets-2");
    let id = body["session_id"].as_str().unwrap();
    let (_, session) = request(&f.app, "GET", &format!("/sessions/{id}"), None).await;
    assert!(session["parent_session_id"].is_null(), "{session}");
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    let (holder,source): (String,String) = conn.query_row("SELECT session_id,source FROM work_claims WHERE repo=?1 AND number=2 AND ended_at IS NULL",[REPO],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(holder, id);
    assert_eq!(source, "spawn");
    // The CLI starts --setup-worktree by claiming again. It must get AlreadyHeld,
    // with the same claim id, then be allowed to record its worktree.
    let (status, claim) = request(
        &f.app,
        "POST",
        "/claims",
        Some(json!({"requester_session_id":id,"repo":REPO,"number":2,"kind":"ticket"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    assert_eq!(claim["outcome"], "already_held");
    assert!(claim["worktree_naming"]["root"].is_string(), "{claim}");
    for state in ["intent", "created"] {
        let (status, recorded) = request(
            &f.app,
            "POST",
            "/claims/worktree",
            Some(json!({
                "requester_session_id": id, "claim_id": claim["claim"]["id"], "state": state,
                "worktree_path": f.dir.join("worktrees/widgets-2").to_string_lossy(),
                "branch": "2-ticket", "base_sha": "a".repeat(40),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
    }

    let (status, again) =
        owner_request(&f, "POST", "/client/board/start", Some(start_body(2))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    let (_, board) = request_json_board(&f).await;
    assert!(board["lanes"][0]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["text"] == "Started widgets-2 on #2"));
}

#[tokio::test]
async fn start_drops_reservation_on_failure() {
    let f = start_fixture();
    add_goal(&f).await;
    // Make the log directory a file: session creation fails after reservation.
    fs::write(f.dir.join("logs"), "not a directory").unwrap();
    let body = start_body(2);
    let (status, result) = owner_request(&f, "POST", "/client/board/start", Some(body)).await;
    assert!(!status.is_success(), "{result}");
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM work_claims WHERE repo=?1 AND number=2 AND ended_at IS NULL",
            [REPO],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn start_rejects_missing_closed_and_unsigned() {
    let f = start_fixture();
    add_goal(&f).await;
    let (status, _) = request(&f.app, "POST", "/client/board/start", Some(start_body(2))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = owner_request(&f, "POST", "/client/board/start", Some(start_body(999))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    f.board.issues.lock().unwrap().get_mut(&2).unwrap().0 = false;
    f.state.run_board_pass().unwrap();
    let (status, body) =
        owner_request(&f, "POST", "/client/board/start", Some(start_body(2))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn start_rejects_new_blockers_cycles_and_merged_unclosed_tickets() {
    let f = start_fixture();
    add_goal(&f).await;
    for cycle in [false, true] {
        f.board.issues.lock().unwrap().get_mut(&2).unwrap().1 = vec![3];
        if cycle {
            f.board.issues.lock().unwrap().get_mut(&3).unwrap().1 = vec![2];
        }
        pass(&f).await;
        let (status, body) =
            owner_request(&f, "POST", "/client/board/start", Some(start_body(2))).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
    f.board
        .issues
        .lock()
        .unwrap()
        .get_mut(&2)
        .unwrap()
        .1
        .clear();
    f.board
        .issues
        .lock()
        .unwrap()
        .get_mut(&3)
        .unwrap()
        .1
        .clear();
    pass(&f).await;
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    conn.execute("INSERT INTO board_prs (repo,issue_number,pr_repo,pr_number,pr_state,url) VALUES (?1,2,?1,99,'MERGED','https://github.com/acme/widgets/pull/99')",[REPO]).unwrap();
    let (status, body) =
        owner_request(&f, "POST", "/client/board/start", Some(start_body(2))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM work_claims", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn blocked_ticket_starts_only_with_early_start_flag_and_brief() {
    let f = start_fixture();
    add_goal(&f).await;
    f.board.issues.lock().unwrap().get_mut(&2).unwrap().1 = vec![3];
    pass(&f).await;
    let (status, _) = owner_request(&f, "POST", "/client/board/start", Some(start_body(2))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, options) = owner_request(
        &f,
        "GET",
        "/client/board/start-options?repo=acme/widgets&number=2&start_blocked=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{options}");
    assert!(options["brief"]
        .as_str()
        .unwrap()
        .contains("#3 is not done yet"));
    let mut body = start_body(2);
    body["start_blocked"] = json!(true);
    let (status, started) = owner_request(&f, "POST", "/client/board/start", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let (_, board) = request_json_board(&f).await;
    let ticket = board["lanes"][0]["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ticket| ticket["number"] == 2)
        .unwrap();
    assert_eq!(ticket["started_early"], true);
}

#[tokio::test]
async fn early_start_flag_on_ready_ticket_does_not_mark_it_early() {
    let f = start_fixture();
    add_goal(&f).await;
    let mut body = start_body(2);
    body["start_blocked"] = json!(true);
    let (status, started) = owner_request(&f, "POST", "/client/board/start", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(f.ticket(2).await["started_early"], false);
}

#[tokio::test]
async fn close_requires_finished_container() {
    let f = start_fixture();
    add_goal(&f).await;
    let (status, body) = owner_request(
        &f,
        "POST",
        "/client/board/close",
        Some(json!({"repo": REPO, "number": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    for child in [2, 3, 4] {
        f.board.issues.lock().unwrap().get_mut(&child).unwrap().0 = false;
    }
    pass(&f).await;
    let (_, board) = request_json_board(&f).await;
    let goal = board["lanes"][0]["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ticket| ticket["number"] == 1)
        .unwrap();
    assert_eq!(goal["state"], "close_ready");
    assert_eq!(goal["sub_issues"], json!({"total": 3, "done": 3}));
}

#[tokio::test]
async fn board_includes_pr_claimed_by_ticket_holder_without_github_reference() {
    let f = start_fixture();
    add_goal(&f).await;
    f.claim(2, "eng00001");
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    conn.execute("INSERT INTO work_items(repo, number, kind, title, state, url, synced_at)
        VALUES (?1, 77, 'pr', 'Unlinked PR', 'OPEN', 'https://github.com/acme/widgets/pull/77', '2026-09-29T11:00:00Z')", [REPO]).unwrap();
    conn.execute(
        "INSERT INTO work_claims(id, repo, number, kind, session_id, source, claimed_at)
        VALUES ('pr77', ?1, 77, 'pr', 'eng00001', 'explicit', '2026-09-29T11:00:00Z')",
        [REPO],
    )
    .unwrap();
    conn.execute(
        "UPDATE work_claims SET ended_at = '2026-09-29T12:00:00Z' WHERE id IN ('pr77', 'c2eng00001')",
        [],
    ).unwrap();
    let ticket = f.ticket(2).await;
    assert_eq!(ticket["prs"][0]["number"], 77);
    assert_eq!(ticket["prs"][0]["state"], "OPEN");
    assert_eq!(ticket["prs"][0]["review"], Value::Null);
}

#[tokio::test]
async fn start_rechecks_readiness_after_github_fetch_before_claiming() {
    use sm_server::work_claims::{BatchFetch, WorkItemSource};
    struct LateBlocker(PathBuf);
    impl WorkItemSource for LateBlocker {
        fn fetch(&self, repo: &str, numbers: &[i64]) -> Result<BatchFetch, String> {
            Connection::open(&self.0).unwrap().execute("INSERT INTO board_edges (waiter_repo,waiter_number,blocker_repo,blocker_number,kind,source,first_seen_at) VALUES (?1,2,?1,3,'after','github','2026-09-29T00:00:00Z')",[REPO]).unwrap();
            BoardClaimItems.fetch(repo, numbers)
        }
    }
    let mut f = start_fixture();
    add_goal(&f).await;
    f.state = f
        .state
        .clone()
        .with_work_item_source(Arc::new(LateBlocker(f.dir.join("message_queue.db"))));
    f.app = router(f.state.clone());
    let (status, body) =
        owner_request(&f, "POST", "/client/board/start", Some(start_body(2))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM work_claims", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

/// Claims PR 77 for `session`, with its sm-recorded state.
fn claim_pr_77(f: &Fixture, session: &str, state: &str) {
    let conn = Connection::open(f.dir.join("message_queue.db")).unwrap();
    conn.execute("INSERT INTO work_items(repo, number, kind, title, state, url, synced_at)
        VALUES (?1, 77, 'pr', 'PR 77', ?2, 'https://github.com/acme/widgets/pull/77', '2026-09-29T11:00:00Z')", params![REPO, state]).unwrap();
    conn.execute(
        "INSERT INTO work_claims(id, repo, number, kind, session_id, source, claimed_at)
        VALUES ('pr77', ?1, 77, 'pr', ?2, 'explicit', '2026-09-29T11:00:00Z')",
        params![REPO, session],
    )
    .unwrap();
}

#[tokio::test]
async fn job_with_empty_rank_tickets_links_to_tickets_of_requesters_claimed_pr() {
    let f = fixture();
    add_goal(&f).await;
    // A PR claim with no ticket link yet: the job is queued with no tickets.
    claim_pr_77(&f, "eng00001", "OPEN");
    let (status, job) = request(
        &f.app,
        "POST",
        "/queue-jobs",
        Some(json!({
            "type": "tests", "argv": ["true"], "cwd": f.dir.display().to_string(),
            "requester_session_id": "eng00001", "timeout_seconds": 60,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    let rank_tickets: Option<String> = Connection::open(f.dir.join("queue/queue_runner.db"))
        .unwrap()
        .query_row(
            "SELECT rank_tickets FROM queue_jobs WHERE id = ?1",
            [job["id"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rank_tickets.as_deref(), Some("[]"));
    f.link_pr(77, 3);
    let ticket = f.ticket(3).await;
    assert_eq!(ticket["jobs"].as_array().unwrap().len(), 1, "{ticket}");
    assert_eq!(ticket["jobs"][0]["id"], job["id"], "{ticket}");
    assert_eq!(ticket["jobs"][0]["state"], "waiting");
    // Claiming the PR's ticket too names that ticket once.
    f.claim(3, "eng00001");
    let ticket = f.ticket(3).await;
    assert_eq!(ticket["jobs"].as_array().unwrap().len(), 1, "{ticket}");
}

#[tokio::test]
async fn claimed_pr_takes_its_state_from_the_board_reference() {
    let f = start_fixture();
    f.board.prs.lock().unwrap().insert(
        2,
        vec![PrRef {
            repo: REPO.into(),
            number: 77,
            state: "MERGED".into(),
            url: format!("https://github.com/{REPO}/pull/77"),
        }],
    );
    add_goal(&f).await;
    f.claim(2, "eng00001");
    claim_pr_77(&f, "eng00001", "OPEN");
    let ticket = f.ticket(2).await;
    assert_eq!(ticket["prs"].as_array().unwrap().len(), 1, "{ticket}");
    assert_eq!(ticket["prs"][0]["number"], 77);
    assert_eq!(ticket["prs"][0]["state"], "MERGED");
}
