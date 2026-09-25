//! `sm ticket` and `sm pr`: tell sm which ticket or PR this session works on
//! (sm#1452). Resolution runs here, in the agent's cwd: the repo, the
//! worktree and the branch; the server checks GitHub and applies the rules.

use super::*;
use crate::git_repo::{resolve_repo_slug, run_ok, DocTools, ProcessTools};

pub(crate) mod worktree;

/// A refused claim: another live agent holds the item.
pub(crate) const EXIT_COLLISION: i32 = 3;

#[derive(Args)]
pub(crate) struct TicketArgs {
    /// Ticket number to claim; without one, list your active claims
    number: Option<i64>,
    /// owner/name, or a bare repo name with the cwd repo's owner
    #[arg(long)]
    repo: Option<String>,
    /// Take the ticket from an unrelated live holder (only on the owner's word)
    #[arg(long, conflicts_with = "release")]
    take: bool,
    /// End your claim on this ticket
    #[arg(long, value_name = "N", conflicts_with = "number")]
    release: Option<i64>,
    /// Then create the ticket's worktree (optionally named by SLUG)
    #[arg(
        long = "setup-worktree",
        value_name = "SLUG",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "",
        requires = "number",
        conflicts_with = "release"
    )]
    setup_worktree: Option<String>,
    /// The commit the new branch starts from (default: origin's default branch)
    #[arg(long, value_name = "REF", requires = "setup_worktree")]
    base: Option<String>,
}

#[derive(Args)]
pub(crate) struct PrArgs {
    /// PR number to claim; without one, the current branch's open PR
    number: Option<i64>,
    /// owner/name, or a bare repo name with the cwd repo's owner
    #[arg(long)]
    repo: Option<String>,
    /// Take the PR from an unrelated live holder (only on the owner's word)
    #[arg(long, conflicts_with = "release")]
    take: bool,
    /// End your claim on this PR
    #[arg(long, value_name = "N", conflicts_with_all = ["number", "ticket"])]
    release: Option<i64>,
    /// A ticket this PR is for (repeatable); links it, does not claim it
    #[arg(long = "ticket", value_name = "T")]
    ticket: Vec<i64>,
}

/// What a claim command sends and prints, resolved in the cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimTarget {
    pub repo: String,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
}

/// The cwd checkout's top level and `owner/name`, when inside one.
fn cwd_checkout(tools: &dyn DocTools, cwd: &Path) -> Option<(String, String)> {
    let root = run_ok(tools, "git", cwd, &["rev-parse", "--show-toplevel"]).ok()?;
    let repo = resolve_repo_slug(tools, Path::new(&root)).ok()?;
    Some((root, repo))
}

/// The repo a claim names (appendix B): `--repo owner/name`, a bare
/// `--repo name` with the cwd repo's owner, or the cwd repo. The worktree
/// and branch are sent only when the cwd is inside the claimed repo.
pub(crate) fn resolve_claim_target(
    tools: &dyn DocTools,
    cwd: &Path,
    repo_flag: Option<&str>,
) -> Result<ClaimTarget> {
    let checkout = cwd_checkout(tools, cwd);
    let repo = match repo_flag.map(str::trim).filter(|repo| !repo.is_empty()) {
        Some(repo) if repo.contains('/') => repo.to_owned(),
        Some(name) => match &checkout {
            Some((_, cwd_repo)) => {
                let owner = cwd_repo.split('/').next().unwrap_or_default();
                format!("{owner}/{name}")
            }
            None => bail!("--repo {name} needs an owner outside a checkout: pass owner/name"),
        },
        None => match &checkout {
            Some((_, cwd_repo)) => cwd_repo.clone(),
            None => bail!("run this from the repo's checkout, or pass --repo owner/name"),
        },
    };
    let (worktree_path, branch) = match &checkout {
        Some((root, cwd_repo)) if cwd_repo.eq_ignore_ascii_case(&repo) => {
            let branch = run_ok(tools, "git", Path::new(root), &["branch", "--show-current"])
                .ok()
                .filter(|branch| !branch.is_empty());
            (Some(root.clone()), branch)
        }
        _ => (None, None),
    };
    Ok(ClaimTarget {
        repo,
        worktree_path,
        branch,
    })
}

/// `sm pr` with no number: the current branch's open PR, as `sm doc
/// publish` finds it.
pub(crate) fn current_branch_pr(tools: &dyn DocTools, cwd: &Path) -> Result<i64> {
    let current = tools.run("gh", cwd, &["pr", "view", "--json", "number,state"])?;
    if current.success {
        let payload: Value = serde_json::from_str(&current.stdout).unwrap_or(Value::Null);
        if payload["state"].as_str() == Some("OPEN") {
            if let Some(number) = payload["number"].as_i64() {
                return Ok(number);
            }
        }
    }
    let branch = run_ok(tools, "git", cwd, &["branch", "--show-current"]).unwrap_or_default();
    let branch = if branch.is_empty() {
        "(detached HEAD)".to_owned()
    } else {
        branch
    };
    bail!("No open PR for branch {branch}. Open one first (gh pr create) or pass the PR number.")
}

pub(crate) fn run_ticket(client: &ApiClient, args: TicketArgs) -> Result<()> {
    let session_id = managed_session_id("sm ticket")?;
    let cwd = env::current_dir()?;
    if let Some(number) = args.release {
        let target = resolve_claim_target(&ProcessTools, &cwd, args.repo.as_deref())?;
        return release(client, &session_id, "ticket", &target.repo, number);
    }
    let Some(number) = args.number else {
        return list_claims(client, &session_id);
    };
    let target = resolve_claim_target(&ProcessTools, &cwd, args.repo.as_deref())?;
    if let Some(slug) = args.setup_worktree.as_deref() {
        return worktree::run_setup(
            client,
            &session_id,
            number,
            &target,
            args.take,
            slug,
            args.base.as_deref(),
        );
    }
    claim(
        client,
        &session_id,
        "ticket",
        number,
        &target,
        args.take,
        &[],
    )
}

pub(crate) fn run_pr(client: &ApiClient, args: PrArgs) -> Result<()> {
    let session_id = managed_session_id("sm pr")?;
    let cwd = env::current_dir()?;
    let target = resolve_claim_target(&ProcessTools, &cwd, args.repo.as_deref())?;
    if let Some(number) = args.release {
        return release(client, &session_id, "pr", &target.repo, number);
    }
    let number = match args.number {
        Some(number) => number,
        None => current_branch_pr(&ProcessTools, &cwd)?,
    };
    claim(
        client,
        &session_id,
        "pr",
        number,
        &target,
        args.take,
        &args.ticket,
    )
}

fn managed_session_id(command: &str) -> Result<String> {
    optional_current_session_id().ok_or_else(|| {
        anyhow!("{command} must run inside a managed session (CLAUDE_SESSION_MANAGER_ID)")
    })
}

fn claim(
    client: &ApiClient,
    session_id: &str,
    kind: &str,
    number: i64,
    target: &ClaimTarget,
    take: bool,
    tickets: &[i64],
) -> Result<()> {
    let response = client.request(
        "POST",
        "/claims",
        Some(claim_body(session_id, kind, number, target, take, tickets)),
    )?;
    let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    let printed = claim_output(kind, number, response.status, &body, |path| {
        client.url_for(path)
    });
    finish(printed)
}

fn claim_body(
    session_id: &str,
    kind: &str,
    number: i64,
    target: &ClaimTarget,
    take: bool,
    tickets: &[i64],
) -> Value {
    json!({
        "requester_session_id": session_id,
        "kind": kind,
        "repo": target.repo,
        "number": number,
        "take": take,
        "worktree_path": target.worktree_path,
        "branch": target.branch,
        "tickets": tickets,
    })
}

fn release(
    client: &ApiClient,
    session_id: &str,
    kind: &str,
    repo: &str,
    number: i64,
) -> Result<()> {
    let response = client.request(
        "POST",
        "/claims/release",
        Some(json!({
            "requester_session_id": session_id,
            "kind": kind,
            "repo": repo,
            "number": number,
        })),
    )?;
    let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    finish(release_output(kind, number, response.status, &body))
}

fn list_claims(client: &ApiClient, session_id: &str) -> Result<()> {
    let payload = client.get_json(&format!(
        "/claims?session={}&active=true",
        encode_query_component(session_id)
    ))?;
    for line in claim_list_lines(&payload, |path| client.url_for(path)) {
        println!("{line}");
    }
    Ok(())
}

/// What a claim command prints and how it exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Printed {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub exit: i32,
}

fn finish(printed: Printed) -> Result<()> {
    for line in &printed.stdout {
        println!("{line}");
    }
    for line in &printed.stderr {
        eprintln!("{line}");
    }
    if printed.exit != 0 {
        io::stdout().flush().ok();
        process::exit(printed.exit);
    }
    Ok(())
}

fn noun(kind: &str) -> &'static str {
    if kind == "pr" {
        "PR"
    } else {
        "ticket"
    }
}

fn history_link(claim: &Value, url_for: impl Fn(&str) -> String) -> String {
    match claim["history_url"].as_str().filter(|url| !url.is_empty()) {
        Some(url) => url.to_owned(),
        None => url_for(claim["history_path"].as_str().unwrap_or("")),
    }
}

/// `Claimed ticket #1452 "Agent work claims…" (session-manager). History: <url>`
pub(crate) fn claimed_line(claim: &Value, url_for: impl Fn(&str) -> String) -> String {
    let kind = claim["kind"].as_str().unwrap_or("ticket");
    let number = claim["number"].as_i64().unwrap_or_default();
    let repo = claim["repo"].as_str().unwrap_or_default();
    let repo_name = repo.rsplit('/').next().unwrap_or(repo);
    let title = claim["title"].as_str().unwrap_or_default();
    let title = if title.is_empty() {
        String::new()
    } else {
        format!(" \"{title}\"")
    };
    format!(
        "Claimed {} #{number}{title} ({repo_name}). History: {}",
        noun(kind),
        history_link(claim, url_for)
    )
}

/// The refusal: one line per holder, exit 3.
pub(crate) fn refusal_lines(kind: &str, number: i64, holders: &[Value]) -> Vec<String> {
    holders
        .iter()
        .map(|holder| {
            let mut line = format!(
                "Refused: {} #{number} is held by {} ({}), {}, claimed {} ago",
                noun(kind),
                holder["name"].as_str().unwrap_or_default(),
                holder["session_id"].as_str().unwrap_or_default(),
                holder["state"].as_str().unwrap_or_default(),
                coarse_age(holder["claimed_at"].as_str().unwrap_or_default()),
            );
            if let Some(path) = holder["worktree_path"].as_str().filter(|p| !p.is_empty()) {
                line.push_str(&format!(", worktree {}", home_relative(path)));
            }
            line.push('.');
            line
        })
        .collect()
}

pub(crate) fn claim_output(
    kind: &str,
    number: i64,
    status: u16,
    body: &Value,
    url_for: impl Fn(&str) -> String,
) -> Printed {
    let notes = || -> Vec<String> {
        body["notes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|note| note.as_str().map(ToOwned::to_owned))
            .collect()
    };
    match (status, body["outcome"].as_str()) {
        (200 | 201, Some("claimed" | "taken")) => {
            let mut stdout = vec![claimed_line(&body["claim"], url_for)];
            stdout.extend(notes());
            Printed {
                stdout,
                stderr: Vec::new(),
                exit: 0,
            }
        }
        (200, Some("already_held")) => {
            let mut stdout = vec![format!("You already hold {} #{number}.", noun(kind))];
            stdout.extend(notes());
            Printed {
                stdout,
                stderr: Vec::new(),
                exit: 0,
            }
        }
        (409, Some("collision")) => Printed {
            stdout: Vec::new(),
            stderr: refusal_lines(
                kind,
                number,
                body["holders"].as_array().map(Vec::as_slice).unwrap_or(&[]),
            ),
            exit: EXIT_COLLISION,
        },
        _ => Printed {
            stdout: Vec::new(),
            stderr: vec![api_detail(status, body)],
            exit: 1,
        },
    }
}

pub(crate) fn release_output(kind: &str, number: i64, status: u16, body: &Value) -> Printed {
    if status == 200 {
        return Printed {
            stdout: vec![format!("Released {} #{number}.", noun(kind))],
            stderr: Vec::new(),
            exit: 0,
        };
    }
    Printed {
        stdout: Vec::new(),
        stderr: vec![api_detail(status, body)],
        exit: 1,
    }
}

fn api_detail(status: u16, body: &Value) -> String {
    body["detail"]
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("HTTP {status}: {body}"))
}

/// `sm ticket` with no number: `ticket #1452  open  Title  since 2h  <url>`.
pub(crate) fn claim_list_lines(payload: &Value, url_for: impl Fn(&str) -> String) -> Vec<String> {
    let claims = payload["claims"].as_array().cloned().unwrap_or_default();
    if claims.is_empty() {
        return vec!["No active claims.".to_owned()];
    }
    claims
        .iter()
        .map(|claim| {
            format!(
                "{} #{}  {}  {}  since {}  {}",
                noun(claim["kind"].as_str().unwrap_or("ticket")),
                claim["number"].as_i64().unwrap_or_default(),
                claim["state"].as_str().unwrap_or_default(),
                claim["title"].as_str().unwrap_or_default(),
                coarse_age(claim["claimed_at"].as_str().unwrap_or_default()),
                history_link(claim, &url_for),
            )
        })
        .collect()
}

/// `45s`, `12m`, `3h`, `2d`.
pub(crate) fn coarse_age(timestamp: &str) -> String {
    let Ok(at) = OffsetDateTime::parse(timestamp, &Rfc3339) else {
        return "?".to_owned();
    };
    let seconds = (OffsetDateTime::now_utc() - at).whole_seconds().max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

pub(crate) fn home_relative(path: &str) -> String {
    match env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

/// `sm spawn --ticket N [--ticket-repo R]`: the fields added to the spawn
/// request. The repo defaults to the spawn's working directory's repo.
pub(crate) fn spawn_ticket_fields(
    tools: &dyn DocTools,
    working_dir: &Path,
    ticket: i64,
    ticket_repo: Option<&str>,
) -> Result<Value> {
    let target = resolve_claim_target(tools, working_dir, ticket_repo)?;
    Ok(json!({
        "ticket": ticket,
        "ticket_repo": target.repo,
        "ticket_worktree_path": target.worktree_path,
        "ticket_branch": target.branch,
    }))
}

#[cfg(test)]
mod tests;
