use super::*;
use crate::git_repo::ToolOutput;
use std::cell::RefCell;

/// Scripted git/gh: `program args` → stdout. Unscripted calls fail.
#[derive(Default)]
struct FakeTools {
    responses: BTreeMap<String, (bool, String)>,
    calls: RefCell<Vec<String>>,
}

impl FakeTools {
    fn with(mut self, command: &str, success: bool, stdout: &str) -> Self {
        self.responses
            .insert(command.to_owned(), (success, stdout.to_owned()));
        self
    }
}

impl DocTools for FakeTools {
    fn run(&self, program: &str, _cwd: &Path, args: &[&str]) -> Result<ToolOutput> {
        let key = format!("{program} {}", args.join(" "));
        self.calls.borrow_mut().push(key.clone());
        let (success, stdout) = self
            .responses
            .get(&key)
            .cloned()
            .unwrap_or((false, String::new()));
        Ok(ToolOutput {
            success,
            stdout,
            stderr: if success {
                String::new()
            } else {
                format!("failed: {key}")
            },
        })
    }
}

fn checkout(remote: &str) -> FakeTools {
    FakeTools::default()
        .with("git rev-parse --show-toplevel", true, "/wt/sm-1452")
        .with("git remote get-url origin", true, remote)
        .with("git branch --show-current", true, "1452-claims")
}

fn cwd() -> PathBuf {
    PathBuf::from("/wt/sm-1452/src")
}

#[test]
fn repo_resolution_ssh_https_bare_name_and_flag() {
    for remote in [
        "git@github.com:rajeshgoli/session-manager.git",
        "https://github.com/rajeshgoli/session-manager",
    ] {
        let target = resolve_claim_target(&checkout(remote), &cwd(), None).unwrap();
        assert_eq!(
            target,
            ClaimTarget {
                repo: "rajeshgoli/session-manager".into(),
                worktree_path: Some("/wt/sm-1452".into()),
                branch: Some("1452-claims".into()),
            }
        );
    }
    let tools = checkout("git@github.com:rajeshgoli/session-manager.git");
    // A bare name takes the cwd repo's owner; another repo sends no worktree.
    let other = resolve_claim_target(&tools, &cwd(), Some("fractal-algo-rust")).unwrap();
    assert_eq!(other.repo, "rajeshgoli/fractal-algo-rust");
    assert_eq!((other.worktree_path, other.branch), (None, None));
    let explicit = resolve_claim_target(&tools, &cwd(), Some("acme/widgets")).unwrap();
    assert_eq!(explicit.repo, "acme/widgets");
    // The cwd repo named explicitly still sends its worktree.
    let same = resolve_claim_target(&tools, &cwd(), Some("session-manager")).unwrap();
    assert_eq!(same.worktree_path.as_deref(), Some("/wt/sm-1452"));

    // Outside a checkout.
    let outside = FakeTools::default();
    assert_eq!(
        resolve_claim_target(&outside, &cwd(), None)
            .unwrap_err()
            .to_string(),
        "run this from the repo's checkout, or pass --repo owner/name"
    );
    assert!(resolve_claim_target(&outside, &cwd(), Some("widgets"))
        .unwrap_err()
        .to_string()
        .contains("pass owner/name"));
    let flagged = resolve_claim_target(&outside, &cwd(), Some("acme/widgets")).unwrap();
    assert_eq!(
        (flagged.repo.as_str(), flagged.worktree_path),
        ("acme/widgets", None)
    );

    // A detached HEAD sends no branch.
    let detached =
        checkout("git@github.com:acme/widgets.git").with("git branch --show-current", true, "");
    assert_eq!(
        resolve_claim_target(&detached, &cwd(), None)
            .unwrap()
            .branch,
        None
    );
}

#[test]
fn pr_without_a_number_uses_the_branch_pr_or_errors() {
    let open = checkout("git@github.com:acme/widgets.git").with(
        "gh pr view --json number,state",
        true,
        r#"{"number":1470,"state":"OPEN"}"#,
    );
    assert_eq!(current_branch_pr(&open, &cwd()).unwrap(), 1470);
    let merged = checkout("git@github.com:acme/widgets.git").with(
        "gh pr view --json number,state",
        true,
        r#"{"number":1467,"state":"MERGED"}"#,
    );
    assert_eq!(
        current_branch_pr(&merged, &cwd()).unwrap_err().to_string(),
        "No open PR for branch 1452-claims. Open one first (gh pr create) or pass the PR number."
    );
    let none = checkout("git@github.com:acme/widgets.git");
    assert!(current_branch_pr(&none, &cwd()).is_err());
}

#[test]
fn spawn_ticket_fields_default_to_the_working_directory_repo() {
    let tools = checkout("git@github.com:rajeshgoli/session-manager.git");
    assert_eq!(
        spawn_ticket_fields(&tools, &cwd(), 1485, None).unwrap(),
        json!({"ticket": 1485, "ticket_repo": "rajeshgoli/session-manager",
               "ticket_worktree_path": "/wt/sm-1452", "ticket_branch": "1452-claims"})
    );
    assert_eq!(
        spawn_ticket_fields(&tools, &cwd(), 7, Some("acme/widgets")).unwrap()
            ["ticket_worktree_path"],
        Value::Null
    );
}

fn url_for(path: &str) -> String {
    format!("http://127.0.0.1:8420{path}")
}

fn claim(kind: &str, number: i64) -> Value {
    json!({"kind": kind, "number": number, "repo": "rajeshgoli/session-manager",
           "title": "Agent work claims", "state": "open",
           "claimed_at": "2020-01-01T00:00:00Z",
           "history_path": format!("/t/session-manager/{number}")})
}

#[test]
fn each_outcome_prints_its_row_and_exits() {
    let claimed = claim_output(
        "ticket",
        1452,
        201,
        &json!({"outcome": "claimed", "claim": claim("ticket", 1452),
                "notes": ["Also held by your parent sm-1452-lead (1a2b3c4d)."]}),
        url_for,
    );
    assert_eq!(
        claimed.stdout,
        vec![
            "Claimed ticket #1452 \"Agent work claims\" (session-manager). History: http://127.0.0.1:8420/t/session-manager/1452",
            "Also held by your parent sm-1452-lead (1a2b3c4d).",
        ]
    );
    assert_eq!(claimed.exit, 0);

    let mut browser = claim("pr", 1470);
    browser["history_url"] = json!("https://sm.example.com/t/session-manager/1470");
    let taken = claim_output(
        "pr",
        1470,
        201,
        &json!({"outcome": "taken", "claim": browser,
                "notes": ["Took PR #1470 from sm-1451-owner-doc-review (4d2a91c0)."]}),
        url_for,
    );
    assert_eq!(taken.exit, 0);
    assert!(taken.stdout[0].ends_with("History: https://sm.example.com/t/session-manager/1470"));
    assert_eq!(
        taken.stdout[1],
        "Took PR #1470 from sm-1451-owner-doc-review (4d2a91c0)."
    );

    let held = claim_output(
        "ticket",
        1452,
        200,
        &json!({"outcome": "already_held", "claim": claim("ticket", 1452), "notes": []}),
        url_for,
    );
    assert_eq!(held.stdout, vec!["You already hold ticket #1452."]);

    let refused = claim_output(
        "ticket",
        1451,
        409,
        &json!({"outcome": "collision", "holders": [
            {"session_id": "4d2a91c0", "name": "sm-1451-owner-doc-review", "state": "working",
             "claimed_at": "2020-01-01T00:00:00Z", "worktree_path": "/wt/sm-1451"},
            {"session_id": "9f9f9f9f", "name": "sm-1451-other", "state": "idle",
             "claimed_at": "2020-01-01T00:00:00Z", "worktree_path": null}]}),
        url_for,
    );
    assert_eq!(refused.exit, EXIT_COLLISION);
    assert!(refused.stdout.is_empty());
    assert!(refused.stderr[0].starts_with(
        "Refused: ticket #1451 is held by sm-1451-owner-doc-review (4d2a91c0), working, claimed "
    ));
    assert!(refused.stderr[0].ends_with(" ago, worktree /wt/sm-1451."));
    assert!(
        refused.stderr[1].ends_with(" ago."),
        "no worktree: {}",
        refused.stderr[1]
    );

    for (status, detail) in [
        (422, "#1467 is a pull request; use sm pr 1467."),
        (422, "Ticket #1450 is closed."),
        (422, "PR #1467 is merged."),
        (422, "No ticket or PR #99999 in rajeshgoli/session-manager."),
        (
            502,
            "Could not reach GitHub to check #1452: timed out. Nothing was recorded.",
        ),
    ] {
        let failed = claim_output("ticket", 1, status, &json!({ "detail": detail }), url_for);
        assert_eq!((failed.stderr, failed.exit), (vec![detail.to_owned()], 1));
    }

    assert_eq!(
        release_output("ticket", 1452, 200, &json!({})).stdout,
        vec!["Released ticket #1452."]
    );
    let none = release_output(
        "ticket",
        1452,
        404,
        &json!({"detail": "You don't hold ticket #1452."}),
    );
    assert_eq!(
        (none.stderr, none.exit),
        (vec!["You don't hold ticket #1452.".into()], 1)
    );
}

#[test]
fn listing_prints_one_line_per_claim() {
    assert_eq!(
        claim_list_lines(&json!({"claims": []}), url_for),
        vec!["No active claims."]
    );
    let lines = claim_list_lines(
        &json!({"claims": [claim("ticket", 1452), claim("pr", 1470)]}),
        url_for,
    );
    assert!(lines[0].starts_with("ticket #1452  open  Agent work claims  since "));
    assert!(lines[0].ends_with("  http://127.0.0.1:8420/t/session-manager/1452"));
    assert!(lines[1].starts_with("PR #1470  open"));
}

#[test]
fn release_and_take_together_is_a_usage_error() {
    let parse = |args: &[&str]| Cli::try_parse_from(args.iter().copied());
    assert!(parse(&["sm", "ticket", "--release", "5", "--take"]).is_err());
    assert!(parse(&["sm", "pr", "--release", "5", "--take"]).is_err());
    assert!(parse(&["sm", "ticket", "5", "--take"]).is_ok());
    assert!(parse(&["sm", "pr", "--ticket", "1", "--ticket", "2"]).is_ok());
    assert!(parse(&["sm", "spawn", "claude", "go", "--ticket", "1485"]).is_ok());
    assert!(parse(&["sm", "spawn", "claude", "go", "--ticket-repo", "x/y"]).is_err());
}
