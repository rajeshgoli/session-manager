//! A PR's open review findings (#2605): every P0 or P1 finding a reviewer
//! posted that the PR's author has not answered, on whichever commit it was
//! posted. `sm pr findings` lists them, `sm pr merge` refuses while any is
//! open, and the claims sync pass reports a PR that merged with one open.
//!
//! A finding is answered when any of these holds:
//! - its review thread is resolved;
//! - the thread has a reply from the PR's author;
//! - the author posted a PR comment after the finding that names the commit
//!   the finding was reviewed at (its first 7 characters), the way a triage
//!   comment says "Review of 4a6a22a — one P1, valid";
//! - an `sm pr merge` override comment lists the finding.
//!
//! P2 and lower findings, and comments with no severity, never count: they
//! are routinely deferred.
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

/// Hidden marker that opens an override comment; the finding ids follow.
pub const OVERRIDE_MARKER: &str = "<!-- sm-findings-override:v1 ";
/// sm's own review bodies carry this marker and name their commit; they are
/// never a triage comment.
const SM_REVIEW_MARKER: &str = "<!-- sm-review-request:";

pub const PR_QUERY: &str = "query($owner:String!,$name:String!,$pr:Int!,$after:String){
 repository(owner:$owner,name:$name){pullRequest(number:$pr){
  author{login} state mergedAt url
  comments(last:100){nodes{author{login} body createdAt}}
  reviews(first:100){nodes{databaseId author{login} body url submittedAt commit{oid}}}
  reviewThreads(first:100,after:$after){pageInfo{hasNextPage endCursor}
   nodes{isResolved path line originalLine
    comments(first:50){nodes{databaseId author{login} body url createdAt originalCommit{oid}}}}}}}}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The finding comment's database id, or `review-<id>-<n>` for the n-th
    /// finding in a review body. The key an override reason is given under.
    pub id: String,
    pub severity: u8,
    pub path: String,
    pub line: Option<i64>,
    pub title: String,
    pub url: String,
    /// The commit the finding was reviewed at.
    pub commit: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFindings {
    pub author: String,
    /// GitHub's state: OPEN, CLOSED or MERGED.
    pub state: String,
    pub merged_at: Option<String>,
    pub url: String,
    /// Open P0/P1 findings, oldest first.
    pub open: Vec<Finding>,
    /// Open findings below P1, counted and not listed.
    pub lower_open: usize,
}

struct Comment {
    author: String,
    body: String,
    created_at: String,
}

/// Fetches PR `number` of `repo` (`owner/name`) through `graphql(query,
/// variables)` and returns its findings. Review threads are paged; the PR's
/// last 100 comments and first 100 reviews are read.
pub fn fetch(
    repo: &str,
    number: i64,
    graphql: &dyn Fn(&str, Value) -> Result<Value, String>,
) -> Result<PrFindings, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("repo must be owner/name, got {repo}"))?;
    let mut pages = Vec::new();
    let mut after = Value::Null;
    loop {
        let page = graphql(
            PR_QUERY,
            json!({"owner": owner, "name": name, "pr": number, "after": after}),
        )?;
        let pr = page["data"]["repository"]["pullRequest"].clone();
        if pr.is_null() {
            return Err(format!("{repo}#{number} is not a pull request"));
        }
        let info = &pr["reviewThreads"]["pageInfo"];
        let next = (info["hasNextPage"] == true)
            .then(|| info["endCursor"].clone())
            .filter(Value::is_string);
        pages.push(pr);
        match next {
            Some(cursor) => after = cursor,
            None => break,
        }
    }
    Ok(evaluate(&pages))
}

/// The findings in one PR's query pages (the first page carries the PR's
/// comments and reviews; every page carries threads).
pub fn evaluate(pages: &[Value]) -> PrFindings {
    let first = &pages[0];
    let author = first["author"]["login"].as_str().unwrap_or("").to_owned();
    let by_author = |node: &Value| !author.is_empty() && node["author"]["login"] == author.as_str();
    let comments: Vec<Comment> = nodes(&first["comments"])
        .filter(|c| by_author(c))
        .map(|c| Comment {
            author: author.clone(),
            body: text(c, "body"),
            created_at: text(c, "createdAt"),
        })
        .collect();
    let overridden: Vec<String> = comments
        .iter()
        .filter_map(|c| override_ids(&c.body))
        .flatten()
        .collect();
    let triaged = |finding: &Finding| {
        let short = &finding.commit[..finding.commit.len().min(7)];
        overridden.contains(&finding.id)
            || (short.len() == 7
                && comments.iter().any(|c| {
                    c.author == author
                        && c.created_at > finding.created_at
                        && !c.body.contains(SM_REVIEW_MARKER)
                        && c.body.contains(short)
                }))
    };
    let mut open = Vec::new();
    let mut lower_open = 0;
    let mut consider = |finding: Finding, answered: bool| {
        if answered || triaged(&finding) {
            return;
        }
        if finding.severity <= 1 {
            open.push(finding);
        } else {
            lower_open += 1;
        }
    };
    for thread in pages.iter().flat_map(|p| nodes(&p["reviewThreads"])) {
        let mut thread_comments = nodes(&thread["comments"]);
        let Some(head) = thread_comments.next() else {
            continue;
        };
        let body = text(head, "body");
        let Some(severity) = severity(&body) else {
            continue;
        };
        let replied = thread_comments.any(&by_author);
        let finding = Finding {
            id: head["databaseId"].as_i64().unwrap_or(0).to_string(),
            severity,
            path: text(thread, "path"),
            line: thread["line"]
                .as_i64()
                .or_else(|| thread["originalLine"].as_i64()),
            title: title(&body),
            url: text(head, "url"),
            commit: head["originalCommit"]["oid"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            created_at: text(head, "createdAt"),
        };
        consider(finding, thread["isResolved"] == true || replied);
    }
    for review in nodes(&first["reviews"]) {
        for (n, (severity, title, path, line)) in
            body_findings(&text(review, "body")).into_iter().enumerate()
        {
            let finding = Finding {
                id: format!(
                    "review-{}-{}",
                    review["databaseId"].as_i64().unwrap_or(0),
                    n + 1
                ),
                severity,
                path,
                line,
                title,
                url: text(review, "url"),
                commit: review["commit"]["oid"].as_str().unwrap_or("").to_owned(),
                created_at: text(review, "submittedAt"),
            };
            consider(finding, false);
        }
    }
    open.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    PrFindings {
        author,
        state: text(first, "state"),
        merged_at: first["mergedAt"].as_str().map(str::to_owned),
        url: text(first, "url"),
        open,
        lower_open,
    }
}

fn nodes(connection: &Value) -> impl Iterator<Item = &Value> {
    connection["nodes"].as_array().into_iter().flatten()
}

fn text(node: &Value, key: &str) -> String {
    node[key].as_str().unwrap_or("").to_owned()
}

/// `P<n>` from a Codex badge (`![P1 Badge]`) or an sm tag (`**[P1] …`).
fn severity(body: &str) -> Option<u8> {
    let head = body.lines().find(|l| !l.trim().is_empty())?;
    for (prefix, suffix) in [("![P", " Badge]"), ("[P", "]")] {
        if let Some(at) = head.find(prefix) {
            let rest = &head[at + prefix.len()..];
            let digit = rest.chars().next()?;
            if digit.is_ascii_digit() && rest[1..].starts_with(suffix) {
                return Some(digit as u8 - b'0');
            }
        }
    }
    None
}

/// The finding's one-line title: its first bold run, without the severity.
fn title(body: &str) -> String {
    let head = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let bold = head
        .rsplit_once("</sub></sub>")
        .map(|(_, rest)| rest)
        .unwrap_or(head);
    let mut title = bold.trim().trim_matches('*').trim();
    if title.starts_with("[P") {
        if let Some((_, rest)) = title.split_once(']') {
            title = rest.trim();
        }
    }
    cut(title.trim_matches('*').trim(), 120)
}

/// Findings an sm review could not place on a changed line, which it lists
/// in the review body as `#### [P1] title` followed by `` `path:a-b` ``.
fn body_findings(body: &str) -> Vec<(u8, String, String, Option<i64>)> {
    let lines: Vec<&str> = body.lines().collect();
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(heading) = line.strip_prefix("#### ") else {
            continue;
        };
        let Some(severity) = severity(heading) else {
            continue;
        };
        let location = lines
            .get(i + 1)
            .map(|l| l.trim().trim_matches('`'))
            .unwrap_or("");
        let (path, line) = match location.rsplit_once(':') {
            Some((path, range)) => (
                path.to_owned(),
                range.split('-').next().and_then(|n| n.parse().ok()),
            ),
            None => (String::new(), None),
        };
        found.push((severity, title(heading), path, line));
    }
    found
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

/// The finding ids an override comment lists.
pub fn override_ids(body: &str) -> Option<Vec<String>> {
    let rest = &body[body.find(OVERRIDE_MARKER)? + OVERRIDE_MARKER.len()..];
    let list = &rest[..rest.find("-->")?];
    Some(
        list.split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// `P1 path:line — title` and its link on the next line.
pub fn finding_line(f: &Finding) -> String {
    let at = match (f.path.is_empty(), f.line) {
        (true, _) => String::new(),
        (false, Some(line)) => format!(" {}:{line}", f.path),
        (false, None) => format!(" {}", f.path),
    };
    format!("P{}{at} — {}", f.severity, f.title)
}

/// The comment `sm pr merge` posts before merging over open findings:
/// each finding's link and the reason it is left open, signed by the
/// session that merged.
pub fn override_comment(session: &str, reasons: &[(&Finding, &str)]) -> String {
    let ids: Vec<&str> = reasons.iter().map(|(f, _)| f.id.as_str()).collect();
    let mut body = format!(
        "{OVERRIDE_MARKER}{} -->\n**Merging with {} review finding{} left open** — `{session}`\n",
        ids.join(","),
        reasons.len(),
        if reasons.len() == 1 { "" } else { "s" }
    );
    for (finding, reason) in reasons {
        body.push_str(&format!(
            "\n- [{}]({}): {}",
            finding_line(finding),
            finding.url,
            reason.trim()
        ));
    }
    body.push('\n');
    body
}

/// Where Check D reads a merged PR's findings. The live server uses `gh`;
/// tests substitute a fixture.
pub trait FindingsSource: Send + Sync {
    fn findings(&self, repo: &str, pr: i64) -> Result<PrFindings, String>;
}

pub struct GhFindingsSource;

impl FindingsSource for GhFindingsSource {
    fn findings(&self, repo: &str, pr: i64) -> Result<PrFindings, String> {
        fetch(repo, pr, &gh_graphql)
    }
}

/// `gh api graphql` with the request on stdin, so variables keep their JSON
/// types. Read-only queries only.
pub fn gh_graphql(query: &str, variables: Value) -> Result<Value, String> {
    let input = serde_json::to_vec(&json!({"query": query, "variables": variables}))
        .map_err(|e| e.to_string())?;
    let mut command = Command::new("gh");
    command.args(["api", "graphql", "--input", "-"]);
    let output =
        crate::child_output::output_with_timeout_input(command, &input, Duration::from_secs(60))?;
    let payload: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        format!(
            "gh api graphql failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;
    if let Some(errors) = payload["errors"].as_array().filter(|e| !e.is_empty()) {
        return Err(errors
            .iter()
            .map(|e| e["message"].as_str().unwrap_or("GraphQL error"))
            .collect::<Vec<_>>()
            .join("; "));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests;
