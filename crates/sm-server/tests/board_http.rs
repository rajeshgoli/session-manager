//! sm board over HTTP (sm#1665, ticket #1681): lane adds, link writes, the
//! owner-only routes, and Needs you from messages and review requests.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sm_server::{
    board::{
        model::Key,
        sync::{
            BoardSource, IssueConnection, IssueNode, IssuesPage, LinkMutation, RefNode,
            ResolvedIssue, WriteError,
        },
    },
    config::{AppConfig, EmailConfig, PathsConfig, SmSendConfig},
    http::{router, AppState, DocFetchError, DocPullRequest, OwnerDocSource},
    owner_docs::git_blob_sha,
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
        let nodes = issues
            .iter()
            .filter(|(_, (open, _, _))| *open)
            .map(|(number, (_, blocked_by, parent))| IssueNode {
                number: *number,
                title: format!("Ticket {number}"),
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
                prs: Vec::new(),
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
    fs::write(
        &state_file,
        json!({"sessions": [
            session("eng00001", None),
            session("eng00002", None),
            session("gone0001", Some("retired")),
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
    let state = AppState::new(config)
        .with_board_source(Arc::new(board.clone()))
        .with_owner_doc_source(Arc::new(StubDocs));
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
