use super::*;
use crate::git_repo::ToolOutput;

struct FakeTools(Vec<(&'static str, &'static str)>);

impl DocTools for FakeTools {
    fn run(&self, program: &str, _cwd: &Path, args: &[&str]) -> Result<ToolOutput> {
        let key = format!("{program} {}", args.join(" "));
        let found = self.0.iter().find(|(command, _)| *command == key);
        Ok(ToolOutput {
            success: found.is_some(),
            stdout: found.map(|(_, out)| (*out).to_owned()).unwrap_or_default(),
            stderr: String::new(),
        })
    }
}

fn args(item: Option<i64>) -> HistoryArgs {
    HistoryArgs {
        agent: None,
        repo: None,
        open: false,
        json: false,
        limit: 50,
        item,
    }
}

#[test]
fn item_repo_comes_from_the_flag_or_the_cwd_checkout() {
    let cwd = Path::new("/wt");
    let none = FakeTools(Vec::new());
    assert_eq!(
        item_repo_name(&none, cwd, Some("acme/widgets")).unwrap(),
        "widgets"
    );
    assert_eq!(
        item_repo_name(&none, cwd, Some("widgets")).unwrap(),
        "widgets"
    );
    assert_eq!(
        item_repo_name(&none, cwd, None).unwrap_err().to_string(),
        "run this from the repo's checkout, or pass --repo owner/name"
    );
    let checkout = FakeTools(vec![
        ("git rev-parse --show-toplevel", "/wt"),
        (
            "git remote get-url origin",
            "git@github.com:acme/widgets.git",
        ),
    ]);
    assert_eq!(item_repo_name(&checkout, cwd, None).unwrap(), "widgets");
}

#[test]
fn list_query_carries_only_the_filters_given() {
    assert_eq!(list_query(&args(None)), "");
    let mut filtered = args(None);
    filtered.agent = Some("sm 1449 engineer".into());
    filtered.repo = Some("widgets".into());
    filtered.open = true;
    filtered.limit = 10;
    assert_eq!(
        list_query(&filtered),
        "&agent=sm%201449%20engineer&repo=widgets&open=1&limit=10"
    );
}

#[test]
fn list_lines_mirror_the_page_cards() {
    let payload = json!({
        "schema_version": 1,
        "rows": [
            {"repo": "acme/widgets", "number": 1449, "kind": "ticket",
             "title": "Owner docs: publish, storage, reader", "state": "open",
             "flags": ["open_after_merge"],
             "agents": [{"name": "sm-1449-engineer", "state": "retired"}],
             "prs": [{"number": 1460, "state": "merged", "codex_requested": 3}],
             "docs": [{"title": "Spec"}], "last_activity": null},
            {"repo": "acme/widgets", "number": 1481, "kind": "pr", "title": "",
             "state": "open", "flags": [], "agents": [],
             "prs": [{"number": 1481, "state": "open", "codex_requested": 0}], "docs": []}
        ],
        "next_before": "abc"
    });
    assert_eq!(
        list_lines(&payload),
        vec![
            "#1449  open  Owner docs: publish, storage, reader  [open after merge]  \
             PR #1460 merged 3 Codex  docs 1  sm-1449-engineer (retired)",
            "PR #1481  open  (not fetched yet)",
            "(more on the page)",
        ]
    );
    // Rows from several repos name them.
    let mixed = json!({"rows": [
        {"repo": "acme/a", "number": 1, "kind": "ticket", "title": "A", "state": "closed"},
        {"repo": "acme/b", "number": 2, "kind": "ticket", "title": "B", "state": "open"}
    ]});
    assert_eq!(list_lines(&mixed), vec!["a#1  closed  A", "b#2  open  B"]);
    assert_eq!(
        list_lines(&json!({"rows": []})),
        vec!["No tracked tickets."]
    );
}

#[test]
fn timeline_lines_list_the_item_then_each_entry() {
    let payload = json!({
        "item": {"repo": "acme/widgets", "number": 1449, "kind": "ticket", "title": "Docs",
                 "state": "open", "flags": [], "prs": [], "docs": [], "agents": []},
        "events": [
            {"at": "not a time", "session_id": "a1", "name": "sm-1449-engineer",
             "kind": "claim.taken", "text": "claimed the ticket", "link": null},
            {"at": "x", "session_id": null, "name": null, "kind": "github.state_changed",
             "text": "PR #1460 merged", "link": "https://github.com/acme/widgets/pull/1460"}
        ]
    });
    assert_eq!(
        timeline_lines(&payload),
        vec![
            "widgets#1449  open  Docs",
            "  not a time  sm-1449-engineer  claimed the ticket",
            "  x  -  PR #1460 merged  https://github.com/acme/widgets/pull/1460",
        ]
    );
}

#[test]
fn page_url_prefers_the_browser_address() {
    assert_eq!(
        page_url(
            &json!({"page_url": "https://sm.example.com/history"}),
            || "x".into()
        ),
        "https://sm.example.com/history"
    );
    assert_eq!(
        page_url(&json!({}), || "http://127.0.0.1:8420/history".into()),
        "http://127.0.0.1:8420/history"
    );
}
