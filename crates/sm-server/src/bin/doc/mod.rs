//! `sm doc`: publish agent-written docs for the owner (sm#1447 / #1449).
//!
//! Resolution runs here, in the agent's cwd: the repo, the repo-relative
//! path and the commit the owner will read. The server only stores pointers.

use super::*;

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
    if let Some(publishes) = object.get_mut("publishes").and_then(Value::as_array_mut) {
        for publish in publishes {
            if let Some(object) = publish.as_object_mut() {
                object.remove("doc_id");
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
pub(crate) struct ToolOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// git and gh, behind a seam so resolution is testable without a repo.
pub(crate) trait DocTools {
    fn run(&self, program: &str, cwd: &Path, args: &[&str]) -> Result<ToolOutput>;
}

struct ProcessTools;

impl DocTools for ProcessTools {
    fn run(&self, program: &str, cwd: &Path, args: &[&str]) -> Result<ToolOutput> {
        let output = process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("failed to run {program}"))?;
        Ok(ToolOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn run_ok(tools: &dyn DocTools, program: &str, cwd: &Path, args: &[&str]) -> Result<String> {
    let output = tools.run(program, cwd, args)?;
    if !output.success {
        bail!("{program} {} failed: {}", args.join(" "), output.stderr);
    }
    Ok(output.stdout)
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

/// `owner/name` from an `origin` URL: SSH, scp-style, or HTTPS. The host
/// must be exactly `github.com`; anything else falls back to `gh repo view`.
pub(crate) fn parse_github_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let (authority, rest) = if let Some((_, after_scheme)) = url.split_once("://") {
        after_scheme.split_once('/')?
    } else {
        // scp-style: [user@]host:owner/name
        url.split_once(':')?
    };
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !name.is_empty())
        .then(|| format!("{owner}/{name}"))
}

fn resolve_repo_slug(tools: &dyn DocTools, root: &Path) -> Result<String> {
    if let Ok(url) = run_ok(tools, "git", root, &["remote", "get-url", "origin"]) {
        if let Some(repo) = parse_github_remote(&url) {
            return Ok(repo);
        }
    }
    let repo = run_ok(
        tools,
        "gh",
        root,
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ],
    )
    .context("could not determine the GitHub repo for this checkout")?;
    if repo.is_empty() {
        bail!("could not determine the GitHub repo for this checkout");
    }
    Ok(repo)
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
        let payload = run_ok(
            tools,
            "gh",
            &root,
            &[
                "pr",
                "view",
                &pr.to_string(),
                "--repo",
                &repo,
                "--json",
                "headRefOid,state",
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
        let local_head = run_ok(tools, "git", &root, &["rev-parse", "HEAD"]).unwrap_or_default();
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
        }),
    )?;
    println!(
        "Published \"{}\" ({}) at {} → {}",
        json_string(&doc, "title"),
        json_string(&doc, "name"),
        &resolved.commit_sha[..7],
        reader_url(client, &doc)
    );
    Ok(())
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
            "  {}  blob {}  {}",
            short_sha(publish["commit_sha"].as_str().unwrap_or("")),
            short_sha(publish["blob_sha"].as_str().unwrap_or("")),
            json_string(publish, "published_at")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

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
    fn json_output_drops_the_internal_doc_id() {
        let doc = without_doc_ids(json!({
            "id": "d0c00001",
            "name": "widgets/memo.md",
            "publishes": [{"id": 1, "doc_id": "d0c00001", "commit_sha": "a"}],
        }));
        assert!(!doc.to_string().contains("d0c00001"), "{doc}");
        assert_eq!(doc["name"], "widgets/memo.md");
        assert_eq!(doc["publishes"][0]["commit_sha"], "a");
        let listed = without_doc_ids(json!({"id": "d0c00001", "name": "widgets/memo.md"}));
        assert_eq!(listed, json!({"name": "widgets/memo.md"}));
    }

    /// Scripted git/gh: `(program, args)` → output. Unscripted calls fail.
    #[derive(Default)]
    struct FakeTools {
        responses: BTreeMap<String, ToolOutput>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeTools {
        fn with(mut self, command: &str, success: bool, stdout: &str) -> Self {
            self.responses.insert(
                command.to_owned(),
                ToolOutput {
                    success,
                    stdout: stdout.to_owned(),
                    stderr: if success {
                        String::new()
                    } else {
                        "failed".into()
                    },
                },
            );
            self
        }
    }

    impl DocTools for FakeTools {
        fn run(&self, program: &str, _cwd: &Path, args: &[&str]) -> Result<ToolOutput> {
            let key = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(key.clone());
            Ok(self.responses.get(&key).cloned().unwrap_or(ToolOutput {
                success: false,
                stdout: String::new(),
                stderr: format!("unscripted: {key}"),
            }))
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
                "gh pr view 42 --repo acme/widgets --json headRefOid,state",
                true,
                &format!(r#"{{"headRefOid":"{HEAD}","state":"OPEN"}}"#),
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

    #[test]
    fn pr_head_mismatch_warns_and_pins_the_pushed_head() {
        let root = repo_dir();
        let tools = base_tools(&root)
            .with(
                "gh pr view 7 --repo acme/widgets --json headRefOid,state",
                true,
                &format!(r#"{{"headRefOid":"{PR_HEAD}","state":"OPEN"}}"#),
            )
            .with("git hash-object -- specs/memo.html", true, "local")
            .with(
                &format!("git rev-parse {PR_HEAD}:specs/memo.html"),
                true,
                "pushed",
            );
        let mut request = request(Path::new("specs/memo.html"));
        request.pr = Some(7);
        let resolved = resolve_doc_publish(&tools, &root, &request).unwrap();
        assert_eq!(resolved.commit_sha, PR_HEAD);
        assert_eq!(resolved.pr_number, Some(7));
        assert_eq!(resolved.messages.len(), 1);
        assert!(resolved.messages[0].contains("pushed version at 2222222"));
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
