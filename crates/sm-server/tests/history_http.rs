//! Owner pages over HTTP (sm#1452): the history pages (ticket #1488,
//! `/history` and `/t/<repo-name>/<n>`, HTML and JSON, filters, paging and
//! the timeline) and the web watch (ticket #1489, `/`, `/watch` and
//! `/watch/state`), on one claims fixture.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header::CONTENT_TYPE, Request, StatusCode},
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

#[derive(Clone, Default)]
struct StubItems {
    items: Arc<Mutex<BTreeMap<i64, GhItem>>>,
}

impl StubItems {
    fn put(&self, number: i64, kind: WorkKind, state: &str, closes: &[i64]) {
        let path = if kind == WorkKind::Pr {
            "pull"
        } else {
            "issues"
        };
        self.items.lock().unwrap().insert(
            number,
            GhItem {
                kind,
                title: format!("Item <{number}>"),
                state: state.into(),
                state_reason: None,
                url: format!("https://github.com/{REPO}/{path}/{number}"),
                head_ref: None,
                head_sha: None,
                closed_at: None,
                merged_at: None,
                closing_refs: (kind == WorkKind::Pr)
                    .then(|| closes.iter().map(|n| (REPO.to_owned(), *n)).collect()),
            },
        );
    }
}

impl WorkItemSource for StubItems {
    fn fetch(&self, _repo: &str, numbers: &[i64]) -> Result<BatchFetch, String> {
        let items = self.items.lock().unwrap();
        Ok(numbers
            .iter()
            .map(|n| {
                let fetch = items
                    .get(n)
                    .cloned()
                    .map_or(ItemFetch::NotFound, |item| ItemFetch::Found(Box::new(item)));
                (*n, fetch)
            })
            .collect())
    }
}

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
            // Now, so the request orders after the claims made before it.
            posted_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        })
    }

    fn current_open_pr_head(&self, _repo: &str, _pr_number: i64) -> Result<String, String> {
        Ok("1".repeat(40))
    }
}

struct Fixture {
    app: axum::Router,
    state: AppState,
    items: StubItems,
    dir: PathBuf,
}

/// lead → eng1; other unrelated; gone stopped.
fn fixture() -> Fixture {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "sm-history-http-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    let state_file = dir.join("sessions.json");
    let session = |id: &str, parent: Option<&str>, status: &str| {
        json!({
            "id": id, "name": format!("claude-{id}"), "friendly_name": format!("{id}-agent"),
            "working_dir": "/repo", "tmux_session": format!("claude-{id}"),
            "log_file": "/tmp/history.log", "status": status,
            "created_at": "2026-09-24T00:00:00Z", "last_activity": "2026-09-24T00:01:00Z",
            "parent_session_id": parent,
        })
    };
    fs::write(
        &state_file,
        json!({"sessions": [
            session("lead0001", None, "idle"),
            session("eng00001", Some("lead0001"), "running"),
            session("other001", None, "running"),
            session("gone0001", None, "stopped"),
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
    items.put(1, WorkKind::Ticket, "open", &[]);
    items.put(2, WorkKind::Ticket, "open", &[]);
    items.put(3, WorkKind::Ticket, "open", &[]);
    items.put(9, WorkKind::Pr, "open", &[]);
    let state = AppState::new(config)
        .with_work_item_source(Arc::new(items.clone()))
        .with_owner_doc_source(Arc::new(StubDocs))
        .with_github_review_poster(Arc::new(StubPoster));
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

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String, String) {
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
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let (status, _, body) = send(app, "GET", uri, None).await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> Value {
    let (status, _, text) = send(app, "POST", uri, Some(body)).await;
    assert!(status.is_success(), "{uri}: {status} {text}");
    serde_json::from_str(&text).unwrap()
}

async fn claim(f: &Fixture, who: &str, kind: &str, number: i64, extra: Value) {
    let mut body = json!({"requester_session_id": who, "kind": kind, "repo": REPO,
                          "number": number, "worktree_path": "/wt", "branch": "b"});
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    post(&f.app, "/claims", body).await;
}

/// eng00001 on ticket 1 and PR 9 (which closes #1), a Codex review and a
/// doc on PR 9; other001 on ticket 2.
async fn seeded() -> Fixture {
    let f = fixture();
    claim(&f, "eng00001", "ticket", 1, json!({})).await;
    claim(&f, "eng00001", "pr", 9, json!({})).await;
    post(
        &f.app,
        "/codex-review-requests",
        json!({"pr_number": 9, "repo": REPO, "requester_session_id": "eng00001"}),
    )
    .await;
    post(
        &f.app,
        "/docs",
        json!({"repo": REPO, "path": "specs/memo.html", "commit_sha": "c".repeat(40),
               "pr_number": 9, "session_id": "eng00001", "title": "Memo <draft>",
               "review": true}),
    )
    .await;
    claim(&f, "other001", "ticket", 2, json!({})).await;
    f
}

#[tokio::test]
async fn history_json_rows_carry_agents_prs_reviews_and_docs() {
    let f = seeded().await;
    let (status, body) = get_json(&f.app, "/history?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["next_before"], Value::Null);
    let rows = body["rows"].as_array().unwrap();
    // Ticket 2's claim is the latest activity. PR 9 links to #1 through
    // its claim, so it is not a row of its own; ticket 3 was never claimed
    // and is not tracked.
    assert_eq!(
        rows.iter()
            .map(|r| r["number"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    let one = &rows[1];
    assert_eq!(one["repo"], REPO);
    assert_eq!(one["kind"], "ticket");
    assert_eq!(one["title"], "Item <1>");
    assert_eq!(one["state"], "open");
    assert_eq!(one["url"], "https://github.com/acme/widgets/issues/1");
    assert_eq!(one["history_path"], "/t/widgets/1");
    assert_eq!(one["flags"], json!([]));
    assert_eq!(one["agents"].as_array().unwrap().len(), 1);
    assert_eq!(one["agents"][0]["session_id"], "eng00001");
    assert_eq!(one["agents"][0]["name"], "eng00001-agent");
    assert_eq!(one["agents"][0]["state"], "working");
    assert_eq!(one["agents"][0]["ended_at"], Value::Null);
    assert_eq!(
        one["prs"],
        json!([{"number": 9, "title": "Item <9>", "state": "open",
                "url": "https://github.com/acme/widgets/pull/9",
                "codex_requested": 1, "codex_landed": 0}])
    );
    assert_eq!(one["docs"][0]["title"], "Memo <draft>");
    assert_eq!(one["docs"][0]["state"], "review_requested");
    assert_eq!(one["docs"][0]["name"], "widgets/specs/memo.html");
    assert_eq!(
        one["docs"][0]["reader_path"],
        "/docs/widgets/specs/memo.html?version=cccccccccccc"
    );
    assert_eq!(one["docs"][0]["owner_reviews"], 0);
    assert!(one["last_activity"].is_string());
}

#[tokio::test]
async fn history_html_is_a_card_page_with_escaped_text() {
    let f = seeded().await;
    let (status, content_type, html) = send(&f.app, "GET", "/history", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/html; charset=utf-8");
    assert!(html.contains("<title>sm · History</title>"));
    assert!(html.contains(r#"<a class="tab on" href="/history">History</a>"#));
    assert!(html.contains(r#"<a class="tab" href="/">Watch</a>"#));
    assert!(html.contains(r#"href="/t/widgets/1""#));
    assert!(html.contains("Item &lt;1&gt;"), "titles are escaped");
    assert!(!html.contains("Item <1>"));
    assert!(html
        .contains(r#"<a class="mt lk" href="https://github.com/acme/widgets/pull/9">PR #9 ↗</a>"#));
    assert!(html.contains("1 Codex"));
    assert!(html.contains("Memo &lt;draft&gt;"));
    assert!(html.contains(r#"href="/history?agent=eng00001""#));
    assert!(!html.contains("<script"), "no script needed");
}

#[tokio::test]
async fn history_filters_by_agent_repo_and_open() {
    let f = seeded().await;
    let numbers = |body: &Value| {
        body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["number"].as_i64().unwrap())
            .collect::<Vec<_>>()
    };
    for agent in ["eng00001", "eng00001-agent"] {
        let (_, body) = get_json(&f.app, &format!("/history?format=json&agent={agent}")).await;
        assert_eq!(numbers(&body), vec![1], "{agent}");
    }
    let (_, body) = get_json(&f.app, "/history?format=json&agent=nobody").await;
    assert_eq!(numbers(&body), Vec::<i64>::new());
    // A purged agent, by an 8+ character prefix of its stored id.
    let db = rusqlite::Connection::open(f.dir.join("message_queue.db")).unwrap();
    db.execute(
        "UPDATE work_claims SET session_id = 'purged00aa11' WHERE session_id = 'other001'",
        [],
    )
    .unwrap();
    let (_, body) = get_json(&f.app, "/history?format=json&agent=purged00").await;
    assert_eq!(numbers(&body), vec![2]);
    let (_, body) = get_json(&f.app, "/history?format=json&agent=purged0").await;
    assert_eq!(
        numbers(&body),
        Vec::<i64>::new(),
        "under 8 characters is not a prefix"
    );
    db.execute(
        "UPDATE work_claims SET session_id = 'other001' WHERE session_id = 'purged00aa11'",
        [],
    )
    .unwrap();
    let (_, body) = get_json(&f.app, "/history?format=json&repo=WIDGETS").await;
    assert_eq!(numbers(&body), vec![2, 1]);
    let (_, body) = get_json(&f.app, "/history?format=json&repo=gadgets").await;
    assert_eq!(numbers(&body), Vec::<i64>::new());

    // Ticket 2 closes on GitHub: the sync pass records it.
    f.items.put(2, WorkKind::Ticket, "closed", &[]);
    f.state.run_work_claims_sync_pass().unwrap();
    let (_, body) = get_json(&f.app, "/history?format=json&open=1").await;
    assert_eq!(numbers(&body), vec![1]);
    let (_, body) = get_json(&f.app, "/history?format=json").await;
    assert_eq!(body["rows"][0]["state"], "closed");
}

#[tokio::test]
async fn history_pages_with_a_cursor() {
    let f = seeded().await;
    claim(&f, "other001", "ticket", 3, json!({})).await;
    let (_, first) = get_json(&f.app, "/history?format=json&limit=2").await;
    assert_eq!(first["rows"].as_array().unwrap().len(), 2);
    let cursor = first["next_before"].as_str().unwrap().to_owned();
    let (_, second) = get_json(
        &f.app,
        &format!("/history?format=json&limit=2&before={cursor}"),
    )
    .await;
    assert_eq!(second["rows"].as_array().unwrap().len(), 1);
    assert_eq!(second["rows"][0]["number"], 1);
    assert_eq!(second["next_before"], Value::Null);
    let (_, _, html) = send(&f.app, "GET", "/history?limit=2", None).await;
    assert!(
        html.contains(&format!("/history?before={cursor}&amp;limit=2")),
        "{html}"
    );
    let (status, body) = get_json(&f.app, "/history?format=json&before=%%%").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn timeline_merges_claims_reviews_and_doc_publishes() {
    let f = seeded().await;
    let (status, body) = get_json(&f.app, "/t/widgets/1?format=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["item"]["number"], 1);
    assert_eq!(body["item"]["prs"][0]["number"], 9);
    let events = body["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        // The PR claim writes its link first, in the same transaction.
        vec![
            "claim.taken",
            "link.added",
            "claim.taken",
            "codex_review.requested",
            "doc.published"
        ]
    );
    assert_eq!(events[0]["text"], "claimed the ticket");
    assert_eq!(events[0]["name"], "eng00001-agent");
    assert_eq!(events[1]["text"], "PR #9 linked (by the PR's claim)");
    assert_eq!(events[2]["text"], "claimed PR #9");
    assert_eq!(events[3]["text"], "requested Codex review on #9");
    assert_eq!(
        events[4]["text"],
        "published Memo <draft> (review requested)"
    );
    assert_eq!(
        events[4]["link"],
        "/docs/widgets/specs/memo.html?version=cccccccccccc"
    );
    for event in events {
        for key in ["at", "session_id", "name", "kind", "text", "link"] {
            assert!(event.get(key).is_some(), "{key} in {event}");
        }
    }

    // Repo names match case-insensitively; a PR has its own page.
    let (status, pr) = get_json(&f.app, "/t/Widgets/9?format=json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pr["item"]["kind"], "pr");
    assert_eq!(pr["item"]["linked_tickets"], json!([1]));

    let (status, content_type, html) = send(&f.app, "GET", "/t/widgets/1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/html; charset=utf-8");
    assert!(html.contains("t / widgets / 1"));
    assert!(html.contains(r#"href="https://github.com/acme/widgets/issues/1">GitHub ↗</a>"#));
    assert!(html.contains("published Memo &lt;draft&gt; (review requested)"));
    assert!(html.contains("Timeline"));
}

#[tokio::test]
async fn timeline_of_an_untracked_item_is_404_not_tracked() {
    let f = seeded().await;
    for uri in [
        "/t/widgets/3?format=json",
        "/t/gadgets/1?format=json",
        "/t/widgets/x?format=json",
    ] {
        let (status, body) = get_json(&f.app, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body["detail"], "Not tracked", "{uri}");
    }
    let (status, content_type, html) = send(&f.app, "GET", "/t/widgets/3", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(content_type, "text/html; charset=utf-8");
    assert!(html.contains("Not tracked"));
}

// ---- web watch (#1489) -----------------------------------------------------

async fn watch_state(app: &axum::Router, query: &str) -> Value {
    let (status, body) = get_json(app, &format!("/watch/state{query}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn listed(state: &Value) -> Vec<String> {
    state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_owned())
        .collect()
}

/// `/watch/state` against what `sm watch` reads and computes from
/// `/sessions` and `/session-obligations` for the same fixture.
#[tokio::test]
async fn watch_state_matches_what_sm_watch_shows() {
    use sm_server::watch_view::{display_state, filter_sessions, tree_order};
    let f = seeded().await;
    let (_, sessions) = get_json(&f.app, "/sessions").await;
    let (_, feed) = get_json(&f.app, "/session-obligations").await;
    let state = watch_state(&f.app, "").await;
    assert_eq!(state["schema_version"], 1);
    assert!(state["generated_at"].is_string());

    let sessions = filter_sessions(sessions["sessions"].as_array().unwrap(), None, None, "");
    let order = tree_order(&sessions);
    let watched = state["sessions"].as_array().unwrap();
    assert_eq!(watched.len(), order.len());
    for (entry, web) in order.iter().zip(watched) {
        let tui = &sessions[entry.index];
        let id = tui["id"].as_str().unwrap();
        assert_eq!(web["id"], id);
        assert_eq!(web["depth"], entry.depth);
        assert_eq!(web["group"], json!(entry.group));
        let obligation = feed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["session_id"] == id);
        let expected = match display_state(tui, obligation) {
            "working" => "working",
            "waiting" => "waiting",
            "stopped" => "stopped",
            _ => "idle",
        };
        assert_eq!(web["state"], expected, "{id}");
        for key in ["claims", "docs", "waiting_on", "review_history"] {
            assert_eq!(
                web[key],
                obligation.map_or(json!([]), |o| o[key].clone()),
                "{id} {key}"
            );
        }
    }
    // lead → eng (child), then other; the stopped session is left out.
    assert_eq!(listed(&state), ["lead0001", "eng00001", "other001"]);
    let eng = &state["sessions"][1];
    assert_eq!(eng["name"], "eng00001-agent");
    assert_eq!(eng["parent_session_id"], "lead0001");
    assert_eq!(eng["state"], "waiting");
    assert_eq!(eng["repo"], "/repo");
    assert_eq!(eng["attach"], "sm attach eng00001-agent");
    assert_eq!(eng["collision"], false);
    assert_eq!(eng["jobs"], json!([]));
    assert_eq!(eng["claims"][0]["worktree_path"], "/wt");
    assert_eq!(
        eng["claims"][0]["url"],
        "https://github.com/acme/widgets/issues/1"
    );
    assert_eq!(state["counts"], json!({"live": 3, "waiting_on_owner": 1}));
}

#[tokio::test]
async fn watch_state_filters_mirror_sm_watch_flags() {
    let f = seeded().await;
    assert_eq!(
        listed(&watch_state(&f.app, "?stopped=1").await),
        ["gone0001", "lead0001", "eng00001", "other001"]
    );
    let stopped = watch_state(&f.app, "?stopped=1").await;
    assert_eq!(stopped["sessions"][0]["state"], "stopped");
    assert_eq!(stopped["counts"]["live"], 3);
    assert_eq!(
        listed(&watch_state(&f.app, "?top_level=1").await),
        ["lead0001", "other001"]
    );
    assert_eq!(listed(&watch_state(&f.app, "?repo=/repo").await).len(), 3);
    assert!(listed(&watch_state(&f.app, "?repo=/elsewhere").await).is_empty());
    assert_eq!(listed(&watch_state(&f.app, "?node=primary").await).len(), 3);
    assert!(listed(&watch_state(&f.app, "?node=studio").await).is_empty());
    assert!(listed(&watch_state(&f.app, "?role=reviewer").await).is_empty());
}

fn watch_cards(html: &str) -> &str {
    let start = html.find(r#"<div id="w""#).expect("cards");
    let end = html[start..].find("<script>").expect("script") + start;
    &html[start..end]
}

#[tokio::test]
async fn watch_page_paints_cards_without_scripts() {
    let f = seeded().await;
    let (status, content_type, root) = send(&f.app, "GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "text/html; charset=utf-8");
    let (status, _, watch) = send(&f.app, "GET", "/watch", None).await;
    assert_eq!(status, StatusCode::OK);
    // One page at both addresses.
    let ids = |html: &str| -> Vec<String> {
        html.split(r#"data-id=""#)
            .skip(1)
            .map(|rest| rest[..rest.find('"').unwrap()].to_owned())
            .collect()
    };
    assert_eq!(
        ids(watch_cards(&root)),
        ["lead0001", "eng00001", "other001"]
    );
    assert_eq!(ids(watch_cards(&root)), ids(watch_cards(&watch)));
    assert!(root.contains(r#"<a class="tab on" href="/">Watch</a>"#));
    assert!(root.contains(r#"<span class="m" id="ws">3 live · 1 waiting on you</span>"#));

    let cards = watch_cards(&root);
    let eng = &cards[cards.find(r#"data-id="eng00001""#).unwrap()..];
    let eng = &eng[..eng.find("</details>").unwrap()];
    // Collapsed line: amber edge for the owner's review, indented child,
    // claim and docs chips.
    assert!(cards.contains(r#"<div class="grp">/repo</div>"#));
    assert!(
        cards.contains(r#"<details class="card a" data-id="eng00001" style="margin-left:14px">"#)
    );
    assert!(eng
        .contains(r#"<span class="dot waiting"></span><span class="mt nm">eng00001-agent</span>"#));
    assert!(eng.contains(r#"<span class="chip c">#1 +1</span>"#));
    assert!(eng.contains(r#"<span class="chip v">docs 1 · review requested</span>"#));
    assert!(eng.contains(r#"<span class="chip a">waiting on you</span>"#));
    // Expanded rows: Work, Docs, Reviews, Attach.
    assert!(eng.contains(
        r#"<a class="mt lk" href="/t/widgets/1">ticket #1</a> <span class="chip c">open</span>"#
    ));
    assert!(eng.contains(r#"<a class="mt lk" href="https://github.com/acme/widgets/pull/9">PR #9 ↗</a> <span class="chip c">open</span> <span class="m">1 Codex</span>"#));
    assert!(eng.contains(r#"<span class="m">worktree /wt</span>"#));
    assert!(eng.contains(r#"<a class="lk" href="/docs/widgets/specs/memo.html?version=cccccccccccc">Memo &lt;draft&gt;</a> <span class="chip a">review requested</span>"#));
    assert!(eng.contains(r#"<span class="amb">Owner review · Memo &lt;draft&gt;</span>"#));
    assert!(eng.contains(
        r#"<span class="mt">widgets#9</span> <span class="m">0 landed · 1 requested</span>"#
    ));
    assert!(eng.contains(r#"$ sm attach eng00001-agent ⧉</code>"#));
    let labels: Vec<&str> = eng
        .split(r#"<span class="lbl">"#)
        .skip(1)
        .map(|rest| &rest[..rest.find('<').unwrap()])
        .collect();
    assert_eq!(labels, ["Work", "Docs", "Reviews", "Attach"]);

    // The refresh script is inline, under 10 KB, and reads /watch/state.
    let script = &root[root.find("<script>").unwrap() + 8..root.find("</script>").unwrap()];
    assert!(script.len() < 10 * 1024, "{} bytes", script.len());
    assert!(script.contains("'/watch/state'"));
    assert!(root.contains(r#"<div id="w" data-refresh="3">"#));
}

#[tokio::test]
async fn watch_flags_a_collision_on_both_cards() {
    let f = seeded().await;
    // other001 publishes a doc on eng00001's PR: an implicit claim by an
    // agent outside eng00001's line.
    post(
        &f.app,
        "/docs",
        json!({"repo": REPO, "path": "specs/notes.md", "commit_sha": "d".repeat(40),
               "pr_number": 9, "session_id": "other001", "title": "Notes"}),
    )
    .await;
    let state = watch_state(&f.app, "").await;
    let collision = |id: &str| {
        state["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == id)
            .unwrap()["collision"]
            .clone()
    };
    assert_eq!(collision("eng00001"), true);
    assert_eq!(collision("other001"), true);
    assert_eq!(collision("lead0001"), false);
    let (_, _, html) = send(&f.app, "GET", "/watch", None).await;
    assert!(html.contains(r#"<details class="card r" data-id="other001""#));
    assert!(html.contains(r#"<span class="chip r">2 agents</span>"#));
}
