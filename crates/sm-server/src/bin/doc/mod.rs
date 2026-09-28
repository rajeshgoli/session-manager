//! `sm doc`: publish agent-written docs for the owner (sm#1447 / #1449).
//!
//! Resolution runs here, in the agent's cwd: the repo, the repo-relative
//! path and the commit the owner will read. The server only stores pointers.

use super::*;
#[cfg(test)]
use crate::git_repo::{parse_github_remote, ToolOutput};
use crate::git_repo::{resolve_repo_slug, run_ok, DocTools, ProcessTools};

#[derive(Args)]
pub(crate) struct DocArgs {
    #[command(subcommand)]
    command: DocCommand,
}

#[derive(Subcommand)]
enum DocCommand {
    /// Publish a committed, pushed doc for the owner to read
    Publish(DocPublishArgs),
    /// List docs (default: your session and its descendants)
    List(DocListArgs),
    /// Show a doc's metadata, revisions and reader URL
    Show(DocShowArgs),
    /// Hide a doc from the session Docs row (git is untouched)
    Retract(DocRetractArgs),
}

#[derive(Args)]
struct DocPublishArgs {
    path: PathBuf,
    /// Tie the doc to this PR; the pinned commit is the PR head
    #[arg(long, conflicts_with_all = ["commit", "no_pr"])]
    pr: Option<i64>,
    /// Pin this commit instead of HEAD or a PR head
    #[arg(long, conflicts_with = "no_pr")]
    commit: Option<String>,
    /// Don't use the current branch's open PR; publish a commit-only doc
    #[arg(long)]
    no_pr: bool,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    note: Option<String>,
    /// Ask the owner to review this revision (needs an open PR); the review
    /// arrives as a GitHub PR review and wakes you with `[sm review]`
    #[arg(long, conflicts_with_all = ["commit", "no_pr"])]
    review: bool,
}

#[derive(Args)]
struct DocListArgs {
    /// Session whose docs (and descendants' docs) to list
    #[arg(long)]
    session: Option<String>,
    /// List every doc, not only one session tree
    #[arg(long, conflicts_with = "session")]
    all: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct DocShowArgs {
    /// `<repo-name>/<path in repo>`, or the doc's URL as printed
    doc: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct DocRetractArgs {
    /// `<repo-name>/<path in repo>`, or the doc's URL as printed
    doc: String,
}

pub(crate) fn run_doc(client: &ApiClient, args: DocArgs) -> Result<()> {
    match args.command {
        DocCommand::Publish(args) => run_doc_publish(client, args),
        DocCommand::List(args) => run_doc_list(client, args),
        DocCommand::Show(args) => run_doc_show(client, args),
        DocCommand::Retract(args) => {
            // The id is the internal key: resolve the readable name to it,
            // and never print it.
            let found = client.get_json(&doc_metadata_path(&args.doc)?)?;
            let doc = client.post_json(
                &format!("/docs/{}/retract", url_segment(&json_string(&found, "id"))),
                json!({}),
            )?;
            println!(
                "Retracted \"{}\" ({}). The file in git is untouched; publish again to restore it.",
                json_string(&doc, "title"),
                json_string(&doc, "name")
            );
            Ok(())
        }
    }
}

/// The metadata request for a doc named as `<repo-name>/<path in repo>` or
/// as a reader URL (`https://<host>/docs/<repo-name>/<path>?version=<sha>`,
/// or its `/docs/...` path). A URL's `?version=` is kept, so `show` describes
/// the same doc the link opens.
fn doc_metadata_path(doc: &str) -> Result<String> {
    let doc = doc.trim();
    let invalid = || anyhow!("expected <repo-name>/<path in repo> or a doc URL, got {doc:?}");
    let url_path = match doc.split_once("://") {
        Some((_, rest)) => Some(rest.find('/').map_or("", |slash| &rest[slash..])),
        None => doc.starts_with("/docs/").then_some(doc),
    };
    let (encoded, version) = match url_path {
        // Already percent-encoded as printed.
        Some(url_path) => {
            let url_path = url_path.split('#').next().unwrap_or_default();
            let (path, query) = url_path.split_once('?').unwrap_or((url_path, ""));
            let encoded = path.strip_prefix("/docs/").ok_or_else(invalid)?.to_owned();
            let version = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("version="))
                .filter(|version| !version.is_empty())
                .map(ToOwned::to_owned);
            (encoded, version)
        }
        None => (
            doc.split('/')
                .map(url_segment)
                .collect::<Vec<_>>()
                .join("/"),
            None,
        ),
    };
    let (name, path) = encoded.split_once('/').ok_or_else(invalid)?;
    if name.is_empty() || path.is_empty() || path.ends_with('/') {
        return Err(invalid());
    }
    let mut request = format!("/docs/{encoded}?format=json");
    if let Some(version) = version {
        request.push_str(&format!("&version={}", url_segment(&version)));
    }
    Ok(request)
}

/// `--json` output keeps the internal doc id out, like every other output.
fn without_doc_ids(mut doc: Value) -> Value {
    let Some(object) = doc.as_object_mut() else {
        return doc;
    };
    object.remove("id");
    // `get_mut`, not indexing: a list record has no `publishes` to add.
    for key in ["publishes", "reviews"] {
        if let Some(rows) = object.get_mut(key).and_then(Value::as_array_mut) {
            for row in rows {
                if let Some(object) = row.as_object_mut() {
                    object.remove("doc_id");
                }
            }
        }
    }
    doc
}

fn url_segment(value: &str) -> String {
    value
        .trim()
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedDoc {
    pub repo: String,
    pub path: String,
    pub pr_number: Option<i64>,
    pub commit_sha: String,
    /// Printed before publishing (auto-PR notice, stale-push warnings).
    pub messages: Vec<String>,
}

pub(crate) struct PublishRequest<'a> {
    pub path: &'a Path,
    pub pr: Option<i64>,
    pub commit: Option<&'a str>,
    pub no_pr: bool,
}

/// Repo root and repo-relative path. The file itself may be absent locally
/// (for example `--pr` on another branch), but its directory must exist.
fn resolve_repo_path(tools: &dyn DocTools, cwd: &Path, path: &Path) -> Result<(PathBuf, String)> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let file_name = absolute
        .file_name()
        .ok_or_else(|| anyhow!("{} is not a file path", path.display()))?
        .to_owned();
    let parent = absolute
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let parent = fs::canonicalize(parent)
        .with_context(|| format!("directory {} does not exist", parent.display()))?;
    let root = run_ok(tools, "git", &parent, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("{} is not inside a git repository", path.display()))?;
    let root = fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(&root));
    let relative = parent
        .join(&file_name)
        .strip_prefix(&root)
        .map_err(|_| {
            anyhow!(
                "{} is outside the repository at {}",
                path.display(),
                root.display()
            )
        })?
        .to_string_lossy()
        .replace('\\', "/");
    Ok((root, relative))
}

fn full_commit_sha(tools: &dyn DocTools, root: &Path, rev: &str) -> Result<String> {
    let rev = rev.trim();
    if rev.len() == 40 && rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(rev.to_ascii_lowercase());
    }
    run_ok(
        tools,
        "git",
        root,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
    .with_context(|| format!("unknown commit {rev}"))
}

/// Seconds between re-reads of a PR's head while GitHub catches up with a
/// push: about 15 s in all.
const PR_HEAD_POLL_DELAYS: [u64; 5] = [1, 2, 3, 4, 5];

/// A PR's head commit and head branch, as GitHub reports them now.
fn pr_head(tools: &dyn DocTools, root: &Path, repo: &str, pr: i64) -> Result<(String, String)> {
    let payload = run_ok(
        tools,
        "gh",
        root,
        &[
            "pr",
            "view",
            &pr.to_string(),
            "--repo",
            repo,
            "--json",
            "headRefOid,headRefName,state",
        ],
    )
    .with_context(|| format!("could not read PR #{pr} in {repo}"))?;
    let payload: Value =
        serde_json::from_str(&payload).context("gh pr view returned invalid JSON")?;
    let head = payload["headRefOid"]
        .as_str()
        .filter(|head| !head.is_empty())
        .ok_or_else(|| anyhow!("PR #{pr} in {repo} has no head commit"))?
        .to_ascii_lowercase();
    let branch = payload["headRefName"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    Ok((head, branch))
}

/// Whether this checkout pushed HEAD to `branch`: `git push` moves the
/// matching remote-tracking ref (`refs/remotes/<remote>/<branch>`) to HEAD.
fn pushed_to(tools: &dyn DocTools, root: &Path, branch: &str) -> bool {
    if branch.is_empty() {
        return false;
    }
    let suffix = format!("/{branch}");
    run_ok(
        tools,
        "git",
        root,
        &[
            "for-each-ref",
            "--points-at",
            "HEAD",
            "--format=%(refname)",
            "refs/remotes",
        ],
    )
    .is_ok_and(|refs| refs.lines().any(|name| name.trim().ends_with(&suffix)))
}

pub(crate) fn resolve_doc_publish(
    tools: &dyn DocTools,
    cwd: &Path,
    request: &PublishRequest<'_>,
) -> Result<ResolvedDoc> {
    let (root, path) = resolve_repo_path(tools, cwd, request.path)?;
    let repo = resolve_repo_slug(tools, &root)?;
    let mut messages = Vec::new();

    if let Some(commit) = request.commit {
        return Ok(ResolvedDoc {
            commit_sha: full_commit_sha(tools, &root, commit)?,
            repo,
            path,
            pr_number: None,
            messages,
        });
    }

    let mut pr_number = request.pr;
    if pr_number.is_none() && !request.no_pr {
        // Keep a republish on the same doc when the agent forgets --pr.
        let current = tools.run("gh", &root, &["pr", "view", "--json", "number,state"])?;
        if current.success {
            let payload: Value = serde_json::from_str(&current.stdout).unwrap_or(Value::Null);
            if payload["state"].as_str() == Some("OPEN") {
                if let Some(number) = payload["number"].as_i64() {
                    messages.push(format!(
                        "Using PR #{number} for the current branch (pass --no-pr for a commit-only doc)"
                    ));
                    pr_number = Some(number);
                }
            }
        }
    }

    if let Some(pr) = pr_number {
        let (mut head, branch) = pr_head(tools, &root, &repo, pr)?;
        let local_head = run_ok(tools, "git", &root, &["rev-parse", "HEAD"]).unwrap_or_default();
        // GitHub moves a PR's head a few seconds after `git push` returns
        // (sm#1599). When this checkout just pushed HEAD to the PR's branch,
        // wait for GitHub rather than publish the previous commit.
        if !local_head.is_empty() && local_head != head && pushed_to(tools, &root, &branch) {
            for delay in PR_HEAD_POLL_DELAYS {
                tools.sleep(Duration::from_secs(delay));
                head = pr_head(tools, &root, &repo, pr)?.0;
                if head == local_head {
                    break;
                }
            }
            if head != local_head {
                bail!(
                    "you pushed {} to {branch}, but after {}s GitHub still shows {} as PR #{pr}'s head; \
                     publish again in a minute, or pull {branch} first if someone pushed to it since",
                    short_sha(&local_head),
                    PR_HEAD_POLL_DELAYS.iter().sum::<u64>(),
                    short_sha(&head)
                );
            }
        }
        let stale_file = run_ok(tools, "git", &root, &["hash-object", "--", &path])
            .ok()
            .zip(
                run_ok(
                    tools,
                    "git",
                    &root,
                    &["rev-parse", &format!("{head}:{path}")],
                )
                .ok(),
            )
            .is_some_and(|(working, pushed)| working != pushed);
        if local_head != head || stale_file {
            messages.push(format!(
                "Warning: the owner will see the pushed version at {}; push first if that's not what you want",
                &head[..7.min(head.len())]
            ));
        }
        return Ok(ResolvedDoc {
            repo,
            path,
            pr_number: Some(pr),
            commit_sha: head,
            messages,
        });
    }

    let status = run_ok(tools, "git", &root, &["status", "--porcelain", "--", &path])?;
    if !status.is_empty() {
        bail!("{path} has uncommitted changes; commit and push first");
    }
    let remote_branches = run_ok(tools, "git", &root, &["branch", "-r", "--contains", "HEAD"])?;
    if remote_branches.is_empty() {
        bail!("HEAD is not on any remote branch; commit and push first");
    }
    Ok(ResolvedDoc {
        commit_sha: full_commit_sha(tools, &root, "HEAD")?,
        repo,
        path,
        pr_number: None,
        messages,
    })
}

/// The browser-hostname link when the server has one, so the owner can open
/// it off the studio; otherwise the API base this CLI talks to.
fn reader_url(client: &ApiClient, doc: &Value) -> String {
    if let Some(url) = doc["browser_url"].as_str().filter(|url| !url.is_empty()) {
        return url.to_owned();
    }
    match doc["reader_path"].as_str() {
        Some(path) => client.url_for(path),
        None => json_string(doc, "reader_url"),
    }
}

fn run_doc_publish(client: &ApiClient, args: DocPublishArgs) -> Result<()> {
    let session_id = optional_current_session_id().ok_or_else(|| {
        anyhow!("sm doc publish must run inside a managed session (CLAUDE_SESSION_MANAGER_ID)")
    })?;
    let cwd = env::current_dir()?;
    let resolved = resolve_doc_publish(
        &ProcessTools,
        &cwd,
        &PublishRequest {
            path: &args.path,
            pr: args.pr,
            commit: args.commit.as_deref(),
            no_pr: args.no_pr,
        },
    )?;
    for message in &resolved.messages {
        eprintln!("{message}");
    }
    if args.review && resolved.pr_number.is_none() {
        bail!("--review needs an open PR: open one containing the file, then pass --pr <N>");
    }
    let file = if args.path.is_absolute() {
        args.path.clone()
    } else {
        cwd.join(&args.path)
    };
    let checkout_root = file
        .parent()
        .and_then(|dir| main_checkout_root(&ProcessTools, dir));
    let doc = client.post_json(
        "/docs",
        json!({
            "repo": resolved.repo,
            "path": resolved.path,
            "pr_number": resolved.pr_number,
            "commit_sha": resolved.commit_sha,
            "session_id": session_id,
            "title": args.title,
            "note": args.note,
            "review": args.review,
            "checkout_root": checkout_root,
        }),
    )?;
    println!(
        "Published \"{}\" ({}) at {} → {}",
        json_string(&doc, "title"),
        json_string(&doc, "name"),
        &resolved.commit_sha[..7],
        reader_url(client, &doc)
    );
    if let Some(warning) = doc["claim_warning"].as_str() {
        eprintln!("{warning}");
    }
    if args.review {
        println!(
            "Review requested. {} is notified in the sm app; the review arrives as a GitHub PR review on #{}, and sm wakes you with [sm review].",
            doc["owner_name"].as_str().unwrap_or("The owner"),
            resolved.pr_number.unwrap_or_default()
        );
    }
    Ok(())
}

/// The repo's main checkout: the parent of git's common dir, run in `dir`.
/// For a file in a linked worktree this is the checkout the worktree was
/// made from, which outlives it (sm#1580). `None` when git can't say.
pub(crate) fn main_checkout_root(tools: &dyn DocTools, dir: &Path) -> Option<String> {
    let common = run_ok(
        tools,
        "git",
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let root = Path::new(common.trim()).parent()?;
    Some(
        fs::canonicalize(root)
            .unwrap_or_else(|_| root.to_path_buf())
            .display()
            .to_string(),
    )
}

fn doc_location(doc: &Value) -> String {
    let mut location = format!("{}:{}", json_string(doc, "repo"), json_string(doc, "path"));
    if let Some(pr) = doc["pr_number"].as_i64() {
        location.push_str(&format!(" #{pr}"));
    }
    location
}

fn short_sha(value: &str) -> &str {
    &value[..value.len().min(7)]
}

fn run_doc_list(client: &ApiClient, args: DocListArgs) -> Result<()> {
    let session = if args.all {
        None
    } else {
        args.session.or_else(optional_current_session_id)
    };
    let path = match &session {
        Some(session) => format!("/docs?session={}&tree=true", url_segment(session)),
        None => "/docs".to_owned(),
    };
    let payload = client.get_json(&path)?;
    let docs = payload["docs"].as_array().cloned().unwrap_or_default();
    if args.json {
        let docs: Vec<_> = docs.into_iter().map(without_doc_ids).collect();
        println!("{}", serde_json::to_string_pretty(&docs)?);
        return Ok(());
    }
    if docs.is_empty() {
        println!("No docs.");
        return Ok(());
    }
    let rows = docs
        .iter()
        .map(|doc| {
            let mut name = json_string(doc, "name");
            if let Some(pr) = doc["pr_number"].as_i64() {
                name.push_str(&format!(" #{pr}"));
            }
            vec![
                name,
                json_string(doc, "state"),
                json_string(doc, "title"),
                short_sha(doc["latest_commit_sha"].as_str().unwrap_or("")).to_owned(),
                doc["author_session_name"]
                    .as_str()
                    .unwrap_or_else(|| doc["author_session_id"].as_str().unwrap_or(""))
                    .to_owned(),
                reader_url(client, doc),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &["Doc", "State", "Title", "Version", "Author", "URL"],
        &rows,
    );
    Ok(())
}

fn run_doc_show(client: &ApiClient, args: DocShowArgs) -> Result<()> {
    let doc = without_doc_ids(client.get_json(&doc_metadata_path(&args.doc)?)?);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }
    println!(
        "Doc: {} ({})",
        json_string(&doc, "title"),
        json_string(&doc, "name")
    );
    println!("State: {}", json_string(&doc, "state"));
    println!("Where: {}", doc_location(&doc));
    println!(
        "Author: {} ({})",
        doc["author_session_name"].as_str().unwrap_or("-"),
        json_string(&doc, "author_session_id")
    );
    if let Some(note) = doc["note"].as_str() {
        println!("Note: {note}");
    }
    if let Some(retracted_at) = doc["retracted_at"].as_str() {
        println!("Retracted: {retracted_at}");
    }
    println!("Reader: {}", reader_url(client, &doc));
    let publishes = doc["publishes"].as_array().cloned().unwrap_or_default();
    println!("Revisions ({}):", publishes.len());
    for publish in publishes.iter().rev() {
        println!(
            "  {}  blob {}  {}{}",
            short_sha(publish["commit_sha"].as_str().unwrap_or("")),
            short_sha(publish["blob_sha"].as_str().unwrap_or("")),
            json_string(publish, "published_at"),
            if publish["review_requested"].as_bool() == Some(true) {
                "  review requested"
            } else {
                ""
            }
        );
    }
    let reviews = doc["reviews"].as_array().cloned().unwrap_or_default();
    if !reviews.is_empty() {
        println!("Reviews ({}):", reviews.len());
        for review in reviews.iter().rev() {
            println!("  {}", review_line(review));
        }
    }
    if doc["review_undelivered"].as_bool() == Some(true) {
        println!("Review not delivered: author retired");
    }
    Ok(())
}

/// One review submission as `sm doc show` prints it.
fn review_line(review: &Value) -> String {
    let status = json_string(review, "status");
    let mut line = format!(
        "{}  {}  {}  {}",
        short_sha(review["commit_sha"].as_str().unwrap_or("")),
        json_string(review, "verdict").replace('_', " "),
        status,
        json_string(review, "submitted_at")
    );
    if status == "posted" {
        line.push_str(&format!(
            "  {} line / {} file comments  {}",
            review["line_comment_count"].as_i64().unwrap_or(0),
            review["file_comment_count"].as_i64().unwrap_or(0),
            json_string(review, "github_review_url")
        ));
        if review["delivered_to_session_id"].is_null() {
            line.push_str("  (not delivered)");
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};

    #[test]
    fn checkout_root_is_the_main_checkout() {
        let base = std::env::temp_dir().join(format!(
            "sm-doc-checkout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let main = base.join("repo");
        let linked = base.join("linked");
        fs::create_dir_all(main.join("docs")).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let output = process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
        };
        git(&main, &["init", "-q"]);
        fs::write(main.join("docs/memo.md"), "# Memo\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "memo"]);
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wt",
                linked.to_str().unwrap(),
            ],
        );
        let expected = fs::canonicalize(&main).unwrap().display().to_string();
        assert_eq!(
            main_checkout_root(&ProcessTools, &main.join("docs")).as_deref(),
            Some(expected.as_str())
        );
        // From a linked worktree: the checkout it was made from.
        assert_eq!(
            main_checkout_root(&ProcessTools, &linked.join("docs")).as_deref(),
            Some(expected.as_str())
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn reader_url_prefers_the_browser_hostname_link() {
        let client = ApiClient::parse("http://127.0.0.1:8420").unwrap();
        let path = "/docs/widgets/memo.md?version=aaaaaaaaaaaa";
        let doc = json!({"reader_path": path});
        assert_eq!(
            reader_url(&client, &doc),
            format!("http://127.0.0.1:8420{path}")
        );
        let doc = json!({
            "reader_path": path,
            "browser_url": format!("https://sm.example.com{path}"),
        });
        assert_eq!(
            reader_url(&client, &doc),
            format!("https://sm.example.com{path}")
        );
    }

    #[test]
    fn docs_are_named_by_repo_and_path_or_by_url() {
        for (doc, expected) in [
            (
                "fractal-algo-rust/docs/working/ticket-title.html",
                "/docs/fractal-algo-rust/docs/working/ticket-title.html?format=json",
            ),
            (
                "widgets/notes/my memo#1.md",
                "/docs/widgets/notes/my%20memo%231.md?format=json",
            ),
            (
                "https://sm.example.com/docs/widgets/notes/my%20memo%231.md?version=aaaaaaaaaaaa",
                "/docs/widgets/notes/my%20memo%231.md?format=json&version=aaaaaaaaaaaa",
            ),
            (
                "http://127.0.0.1:8420/docs/widgets/memo.md",
                "/docs/widgets/memo.md?format=json",
            ),
            (
                "/docs/widgets/memo.md?version=cccccccccccc",
                "/docs/widgets/memo.md?format=json&version=cccccccccccc",
            ),
        ] {
            assert_eq!(doc_metadata_path(doc).unwrap(), expected, "{doc}");
        }
        for bad in [
            "d0c00001",
            "widgets/",
            "/widgets",
            "https://sm.example.com/sessions/abc",
            "https://sm.example.com/docs/d0c00001",
        ] {
            assert!(doc_metadata_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn review_lines_show_verdict_counts_and_delivery() {
        let review = json!({
            "commit_sha": "abcdef0123", "verdict": "changes_requested", "status": "posted",
            "submitted_at": "2026-09-24T10:00:00Z", "line_comment_count": 2,
            "file_comment_count": 1, "github_review_url": "https://github.com/a/b/pull/1#r",
            "delivered_to_session_id": null,
        });
        assert_eq!(
            review_line(&review),
            "abcdef0  changes requested  posted  2026-09-24T10:00:00Z  2 line / 1 file comments  https://github.com/a/b/pull/1#r  (not delivered)"
        );
        let failed = json!({"commit_sha": "abcdef0123", "verdict": "comment", "status": "failed",
                            "submitted_at": "t"});
        assert_eq!(review_line(&failed), "abcdef0  comment  failed  t");
    }

    #[test]
    fn json_output_drops_the_internal_doc_id() {
        let doc = without_doc_ids(json!({
            "id": "d0c00001",
            "name": "widgets/memo.md",
            "publishes": [{"id": 1, "doc_id": "d0c00001", "commit_sha": "a"}],
            "reviews": [{"id": "sub-1", "doc_id": "d0c00001"}],
        }));
        assert!(!doc.to_string().contains("d0c00001"), "{doc}");
        assert_eq!(doc["name"], "widgets/memo.md");
        assert_eq!(doc["publishes"][0]["commit_sha"], "a");
        let listed = without_doc_ids(json!({"id": "d0c00001", "name": "widgets/memo.md"}));
        assert_eq!(listed, json!({"name": "widgets/memo.md"}));
    }

    /// Scripted git/gh: `(program, args)` → output. Unscripted calls fail.
    /// `then` queues further answers; the last one repeats.
    #[derive(Default)]
    struct FakeTools {
        responses: RefCell<BTreeMap<String, VecDeque<ToolOutput>>>,
        calls: RefCell<Vec<String>>,
        sleeps: RefCell<Vec<Duration>>,
    }

    impl FakeTools {
        /// Scripts `command`, replacing any earlier answers.
        fn with(self, command: &str, success: bool, stdout: &str) -> Self {
            self.responses.borrow_mut().remove(command);
            self.then(command, success, stdout)
        }

        fn then(self, command: &str, success: bool, stdout: &str) -> Self {
            self.responses
                .borrow_mut()
                .entry(command.to_owned())
                .or_default()
                .push_back(ToolOutput {
                    success,
                    stdout: stdout.to_owned(),
                    stderr: if success {
                        String::new()
                    } else {
                        "failed".into()
                    },
                });
            self
        }
    }

    impl DocTools for FakeTools {
        fn run(&self, program: &str, _cwd: &Path, args: &[&str]) -> Result<ToolOutput> {
            let key = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(key.clone());
            let mut responses = self.responses.borrow_mut();
            let answer = responses.get_mut(&key).and_then(|queue| {
                if queue.len() > 1 {
                    queue.pop_front()
                } else {
                    queue.front().cloned()
                }
            });
            Ok(answer.unwrap_or(ToolOutput {
                success: false,
                stdout: String::new(),
                stderr: format!("unscripted: {key}"),
            }))
        }

        fn sleep(&self, duration: Duration) {
            self.sleeps.borrow_mut().push(duration);
        }
    }

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const PR_HEAD: &str = "2222222222222222222222222222222222222222";

    /// A real directory standing in for the repo root.
    fn repo_dir() -> PathBuf {
        let dir = env::temp_dir().join(format!("sm-doc-cli-{}", process::id()));
        fs::create_dir_all(dir.join("specs")).unwrap();
        fs::canonicalize(dir).unwrap()
    }

    fn base_tools(root: &Path) -> FakeTools {
        FakeTools::default()
            .with(
                "git rev-parse --show-toplevel",
                true,
                &root.to_string_lossy(),
            )
            .with(
                "git remote get-url origin",
                true,
                "git@github.com:acme/widgets.git",
            )
            .with("git rev-parse HEAD", true, HEAD)
            .with("git rev-parse --verify HEAD^{commit}", true, HEAD)
    }

    fn request(path: &Path) -> PublishRequest<'_> {
        PublishRequest {
            path,
            pr: None,
            commit: None,
            no_pr: false,
        }
    }

    #[test]
    fn repo_slug_parses_ssh_and_https_remotes() {
        for url in [
            "git@github.com:acme/widgets.git",
            "ssh://git@github.com/acme/widgets.git",
            "https://github.com/acme/widgets",
            "https://github.com/acme/widgets.git/",
            "https://x-access-token:t@github.com/acme/widgets.git",
        ] {
            assert_eq!(
                parse_github_remote(url).as_deref(),
                Some("acme/widgets"),
                "{url}"
            );
        }
        assert_eq!(parse_github_remote("https://gitlab.com/acme/widgets"), None);
        assert_eq!(
            parse_github_remote("https://notgithub.com/acme/widgets.git"),
            None
        );
        assert_eq!(
            parse_github_remote("git@notgithub.com:acme/widgets.git"),
            None
        );
        assert_eq!(
            parse_github_remote("https://github.com.evil.io/acme/widgets"),
            None
        );
        assert_eq!(
            parse_github_remote("ssh://git@github.com:22/acme/widgets.git").as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(parse_github_remote("https://github.com/acme"), None);
    }

    #[test]
    fn open_pr_for_the_branch_is_used_without_flags() {
        let root = repo_dir();
        let tools = base_tools(&root)
            .with(
                "gh pr view --json number,state",
                true,
                r#"{"number":42,"state":"OPEN"}"#,
            )
            .with(
                "gh pr view 42 --repo acme/widgets --json headRefOid,headRefName,state",
                true,
                &pr_view(HEAD),
            )
            .with("git hash-object -- specs/memo.html", true, "b1")
            .with(&format!("git rev-parse {HEAD}:specs/memo.html"), true, "b1");
        let resolved =
            resolve_doc_publish(&tools, &root, &request(Path::new("specs/memo.html"))).unwrap();
        assert_eq!(resolved.repo, "acme/widgets");
        assert_eq!(resolved.path, "specs/memo.html");
        assert_eq!(resolved.pr_number, Some(42));
        assert_eq!(resolved.commit_sha, HEAD);
        assert_eq!(
            resolved.messages,
            vec!["Using PR #42 for the current branch (pass --no-pr for a commit-only doc)"]
        );
    }

    #[test]
    fn no_pr_opts_out_of_the_branch_pr() {
        let root = repo_dir();
        let tools = base_tools(&root)
            .with("git status --porcelain -- specs/memo.html", true, "")
            .with("git branch -r --contains HEAD", true, "origin/topic");
        let mut request = request(Path::new("specs/memo.html"));
        request.no_pr = true;
        let resolved = resolve_doc_publish(&tools, &root, &request).unwrap();
        assert_eq!(resolved.pr_number, None);
        assert_eq!(resolved.commit_sha, HEAD);
        assert!(!tools
            .calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("gh pr view")));
    }

    fn pr_view(head: &str) -> String {
        format!(r#"{{"headRefOid":"{head}","headRefName":"topic","state":"OPEN"}}"#)
    }

    const PR_7_VIEW: &str = "gh pr view 7 --repo acme/widgets --json headRefOid,headRefName,state";
    const REMOTE_REFS_AT_HEAD: &str =
        "git for-each-ref --points-at HEAD --format=%(refname) refs/remotes";

    fn pr_7_request(path: &Path) -> PublishRequest<'_> {
        let mut request = request(path);
        request.pr = Some(7);
        request
    }

    #[test]
    fn pr_head_mismatch_warns_and_pins_the_pushed_head() {
        let root = repo_dir();
        // HEAD is not what origin/topic points at: this checkout never
        // pushed it, so there is nothing to wait for.
        let tools = base_tools(&root)
            .with(PR_7_VIEW, true, &pr_view(PR_HEAD))
            .with(REMOTE_REFS_AT_HEAD, true, "refs/remotes/origin/other-topic")
            .with("git hash-object -- specs/memo.html", true, "local")
            .with(
                &format!("git rev-parse {PR_HEAD}:specs/memo.html"),
                true,
                "pushed",
            );
        let resolved =
            resolve_doc_publish(&tools, &root, &pr_7_request(Path::new("specs/memo.html")))
                .unwrap();
        assert_eq!(resolved.commit_sha, PR_HEAD);
        assert_eq!(resolved.pr_number, Some(7));
        assert_eq!(resolved.messages.len(), 1);
        assert!(resolved.messages[0].contains("pushed version at 2222222"));
        assert!(tools.sleeps.borrow().is_empty());
    }

    #[test]
    fn a_push_github_has_not_seen_yet_is_waited_for() {
        let root = repo_dir();
        // `git push` returned, but GitHub reports the old head twice more.
        let tools = base_tools(&root)
            .with(PR_7_VIEW, true, &pr_view(PR_HEAD))
            .then(PR_7_VIEW, true, &pr_view(PR_HEAD))
            .then(PR_7_VIEW, true, &pr_view(PR_HEAD))
            .then(PR_7_VIEW, true, &pr_view(HEAD))
            .with(
                REMOTE_REFS_AT_HEAD,
                true,
                "refs/remotes/origin/HEAD\nrefs/remotes/origin/topic",
            )
            .with("git hash-object -- specs/memo.html", true, "b1")
            .with(&format!("git rev-parse {HEAD}:specs/memo.html"), true, "b1");
        let resolved =
            resolve_doc_publish(&tools, &root, &pr_7_request(Path::new("specs/memo.html")))
                .unwrap();
        assert_eq!(resolved.commit_sha, HEAD);
        assert!(resolved.messages.is_empty(), "{:?}", resolved.messages);
        assert_eq!(
            *tools.sleeps.borrow(),
            [1, 2, 3].map(Duration::from_secs).to_vec()
        );
    }

    #[test]
    fn a_push_github_never_shows_is_an_error_not_the_old_head() {
        let root = repo_dir();
        let tools = base_tools(&root)
            .with(PR_7_VIEW, true, &pr_view(PR_HEAD))
            .with(REMOTE_REFS_AT_HEAD, true, "refs/remotes/origin/topic");
        let error = resolve_doc_publish(&tools, &root, &pr_7_request(Path::new("specs/memo.html")))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("you pushed 1111111 to topic, but after 15s GitHub still shows 2222222 as PR #7's head"),
            "{error}"
        );
        assert_eq!(tools.sleeps.borrow().len(), PR_HEAD_POLL_DELAYS.len());
    }

    #[test]
    fn missing_pr_is_an_error() {
        let root = repo_dir();
        let mut request = request(Path::new("specs/memo.html"));
        request.pr = Some(9);
        let error = resolve_doc_publish(&base_tools(&root), &root, &request).unwrap_err();
        assert!(format!("{error:#}").contains("could not read PR #9"));
    }

    #[test]
    fn dirty_file_without_a_pr_is_an_error() {
        let root = repo_dir();
        let tools = base_tools(&root)
            .with("gh pr view --json number,state", false, "")
            .with(
                "git status --porcelain -- specs/memo.html",
                true,
                " M specs/memo.html",
            );
        let error =
            resolve_doc_publish(&tools, &root, &request(Path::new("specs/memo.html"))).unwrap_err();
        assert!(error.to_string().contains("commit and push first"));
    }

    #[test]
    fn unpushed_head_without_a_pr_is_an_error() {
        let root = repo_dir();
        let tools = base_tools(&root)
            // A merged PR for the branch doesn't count as the branch's open PR.
            .with(
                "gh pr view --json number,state",
                true,
                r#"{"number":3,"state":"MERGED"}"#,
            )
            .with("git status --porcelain -- specs/memo.html", true, "")
            .with("git branch -r --contains HEAD", true, "");
        let error =
            resolve_doc_publish(&tools, &root, &request(Path::new("specs/memo.html"))).unwrap_err();
        assert!(error.to_string().contains("not on any remote branch"));
    }

    #[test]
    fn explicit_commit_is_used_as_given() {
        let root = repo_dir();
        let mut request = request(Path::new("specs/memo.html"));
        request.commit = Some(PR_HEAD);
        let resolved = resolve_doc_publish(&base_tools(&root), &root, &request).unwrap();
        assert_eq!(resolved.commit_sha, PR_HEAD);
        assert_eq!(resolved.pr_number, None);
    }

    #[test]
    fn repo_slug_falls_back_to_gh_and_paths_are_repo_relative() {
        let root = repo_dir();
        let mut tools = base_tools(&root)
            .with("git remote get-url origin", true, "/srv/mirror.git")
            .with(
                "gh repo view --json nameWithOwner --jq .nameWithOwner",
                true,
                "acme/mirror",
            )
            .with("git status --porcelain -- specs/memo.md", true, "")
            .with("git branch -r --contains HEAD", true, "origin/main");
        tools = tools.with("gh pr view --json number,state", false, "");
        // An absolute path from a cwd elsewhere still resolves repo-relative.
        let resolved = resolve_doc_publish(
            &tools,
            &env::temp_dir(),
            &request(&root.join("specs/memo.md")),
        )
        .unwrap();
        assert_eq!(resolved.repo, "acme/mirror");
        assert_eq!(resolved.path, "specs/memo.md");
    }
}
