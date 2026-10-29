use super::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/pr_findings_timeline.json")).unwrap()
}

fn pr(page: &Value) -> Value {
    page["data"]["repository"]["pullRequest"].clone()
}

fn thread(id: i64, body: &str, commit: &str, at: &str, replies: &[(&str, &str)]) -> Value {
    let mut comments = vec![
        json!({"databaseId": id, "author": {"login": "codex"}, "body": body,
        "url": format!("https://x/{id}"), "createdAt": at, "originalCommit": {"oid": commit}}),
    ];
    for (who, when) in replies {
        comments.push(
            json!({"databaseId": id + 1, "author": {"login": who}, "body": "ok",
            "url": "", "createdAt": when, "originalCommit": {"oid": commit}}),
        );
    }
    json!({"isResolved": false, "path": "a.rs", "line": 7, "originalLine": 7,
        "comments": {"nodes": comments}})
}

fn synthetic(threads: Vec<Value>, comments: Vec<Value>, reviews: Vec<Value>) -> Value {
    json!({"author": {"login": "me"}, "state": "OPEN", "mergedAt": null, "url": "https://x/pr",
        "comments": {"nodes": comments}, "reviews": {"nodes": reviews},
        "reviewThreads": {"pageInfo": {"hasNextPage": false}, "nodes": threads}})
}

fn comment(body: &str, at: &str) -> Value {
    json!({"author": {"login": "me"}, "body": body, "createdAt": at})
}

const P1: &str = "**<sub><sub>![P1 Badge](https://img.shields.io/badge/P1-orange?style=flat)</sub></sub>  Fills are wrong**\n\nbody";
const P2: &str = "**<sub><sub>![P2 Badge](https://img.shields.io/badge/P2-yellow?style=flat)</sub></sub>  Minor**";

#[test]
fn a_superseded_round_stays_open_while_triaged_rounds_close() {
    let found = evaluate(&[pr(&fixture())]);
    assert_eq!(found.state, "MERGED");
    let ids: Vec<&str> = found.open.iter().map(|f| f.id.as_str()).collect();
    // The first round, on the superseded aaaa111, was never answered; the
    // later two were triaged in PR comments naming bbbb222 and cccc333.
    assert_eq!(ids, ["1001", "1002"]);
    assert!(found.open.iter().all(|f| f.commit.starts_with("aaaa111")));
    let first = &found.open[0];
    assert_eq!(first.severity, 1);
    assert_eq!(first.title, "Never-filled orders are read as filled");
    assert_eq!(
        finding_line(first),
        "P1 src/store/reader.py:683 — Never-filled orders are read as filled"
    );
}

#[test]
fn reply_resolve_and_triage_close_a_finding() {
    let sha = "abcdef1234";
    let open = |threads, comments| evaluate(&[synthetic(threads, comments, vec![])]).open.len();
    let t = || thread(1, P1, sha, "2026-10-28T10:00:00Z", &[]);
    assert_eq!(open(vec![t()], vec![]), 1);
    // A threaded reply from the author closes it; one from anyone else does not.
    let replied = thread(
        1,
        P1,
        sha,
        "2026-10-28T10:00:00Z",
        &[("me", "2026-10-28T10:05:00Z")],
    );
    assert_eq!(open(vec![replied], vec![]), 0);
    let other = thread(
        1,
        P1,
        sha,
        "2026-10-28T10:00:00Z",
        &[("codex", "2026-10-28T10:05:00Z")],
    );
    assert_eq!(open(vec![other], vec![]), 1);
    let mut resolved = t();
    resolved["isResolved"] = json!(true);
    assert_eq!(open(vec![resolved], vec![]), 0);
    // A later comment that does not name the reviewed commit leaves it open.
    let vague = comment("Addressed review feedback.", "2026-10-28T10:05:00Z");
    assert_eq!(open(vec![t()], vec![vague]), 1);
    // A triage comment naming the commit closes it, but only if it is later.
    let triage = comment(
        "**Review of abcdef1 — one P1, invalid.**",
        "2026-10-28T10:05:00Z",
    );
    assert_eq!(open(vec![t()], vec![triage]), 0);
    let early = comment("Pushed abcdef1.", "2026-10-28T09:55:00Z");
    assert_eq!(open(vec![t()], vec![early]), 1);
}

#[test]
fn p2_and_untagged_comments_never_block() {
    let threads = vec![
        thread(1, P2, "abcdef1", "2026-10-28T10:00:00Z", &[]),
        thread(
            3,
            "Why not a map here?",
            "abcdef1",
            "2026-10-28T10:00:00Z",
            &[],
        ),
    ];
    let found = evaluate(&[synthetic(threads, vec![], vec![])]);
    assert!(found.open.is_empty());
    assert_eq!(found.lower_open, 1);
}

#[test]
fn override_comment_closes_the_findings_it_lists() {
    let page = synthetic(
        vec![thread(1, P1, "abcdef1", "2026-10-28T10:00:00Z", &[])],
        vec![],
        vec![],
    );
    let finding = evaluate(std::slice::from_ref(&page)).open.remove(0);
    let body = override_comment(
        "abc123",
        &[(&finding, "Deferred to #12; no live run reads it.")],
    );
    assert!(body.starts_with("<!-- sm-findings-override:v1 1 -->"));
    assert!(body.contains("— `abc123`"));
    assert!(body.contains("- [P1 a.rs:7 — Fills are wrong](https://x/1): Deferred to #12"));
    assert_eq!(override_ids(&body), Some(vec!["1".to_owned()]));
    let mut overridden = page;
    overridden["comments"]["nodes"] = json!([comment(&body, "2026-10-28T09:00:00Z")]);
    assert!(evaluate(&[overridden]).open.is_empty());
}

#[test]
fn sm_review_body_findings_count_and_do_not_triage_themselves() {
    let body = "<!-- sm-review-request:r1 -->\n**sm review** · x\n\n**Reviewed commit:** `abcdef1234`\n\n### Findings not on a changed line\n\n#### [P1] Store reader inverts fills\n`src/store.rs:10-12`\n\nbody\n\n#### [P3] Nit\n`src/a.rs:1-1`\n";
    let review = json!({"databaseId": 9, "author": {"login": "me"}, "body": body,
        "url": "https://x/r9", "submittedAt": "2026-10-28T10:00:00Z", "commit": {"oid": "abcdef1234"}});
    let echo = comment(body, "2026-10-28T10:01:00Z");
    let found = evaluate(&[synthetic(vec![], vec![echo], vec![review.clone()])]);
    assert_eq!(found.open.len(), 1);
    assert_eq!(found.lower_open, 1);
    let f = &found.open[0];
    assert_eq!(
        (f.id.as_str(), f.path.as_str(), f.line),
        ("review-9-1", "src/store.rs", Some(10))
    );
    assert_eq!(f.title, "Store reader inverts fills");
    let triage = comment("Review of abcdef1: P1 fixed.", "2026-10-28T10:02:00Z");
    assert!(evaluate(&[synthetic(vec![], vec![triage], vec![review])])
        .open
        .is_empty());
}

#[test]
fn sm_inline_tag_is_a_severity() {
    let t = thread(
        1,
        "**[P0] Orders sent twice**\n\nbody",
        "abcdef1",
        "2026-10-28T10:00:00Z",
        &[],
    );
    let found = evaluate(&[synthetic(vec![t], vec![], vec![])]);
    assert_eq!(found.open[0].severity, 0);
    assert_eq!(found.open[0].title, "Orders sent twice");
}

#[test]
fn fetch_pages_review_threads() {
    let calls = std::cell::RefCell::new(Vec::new());
    let graphql = |_: &str, vars: Value| {
        calls.borrow_mut().push(vars["after"].clone());
        let (threads, info) = if vars["after"].is_null() {
            (
                vec![thread(1, P1, "abcdef1", "2026-10-28T10:00:00Z", &[])],
                json!({"hasNextPage": true, "endCursor": "c1"}),
            )
        } else {
            (
                vec![thread(5, P1, "abcdef1", "2026-10-28T10:01:00Z", &[])],
                json!({"hasNextPage": false}),
            )
        };
        let mut page = synthetic(threads, vec![], vec![]);
        page["reviewThreads"]["pageInfo"] = info;
        Ok(json!({"data": {"repository": {"pullRequest": page}}}))
    };
    let found = fetch("o/r", 3, &graphql).unwrap();
    assert_eq!(found.open.len(), 2);
    assert_eq!(*calls.borrow(), vec![Value::Null, json!("c1")]);
    let missing = |_: &str, _: Value| Ok(json!({"data": {"repository": {"pullRequest": null}}}));
    assert!(fetch("o/r", 3, &missing).is_err());
    assert!(fetch("bare", 3, &missing).is_err());
}
