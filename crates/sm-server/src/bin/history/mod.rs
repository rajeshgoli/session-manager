//! `sm history`: the history page in the terminal (sm#1452, ticket #1488).
//! The list reads `GET /history?format=json`, one item's timeline reads
//! `GET /t/<repo-name>/<n>?format=json`; `--json` prints either verbatim.

use super::*;
use crate::git_repo::{resolve_repo_slug, run_ok, DocTools, ProcessTools};
use std::collections::BTreeSet;

#[derive(Args)]
pub(crate) struct HistoryArgs {
    /// Only tickets this agent worked on: session id, name, or role
    #[arg(long, conflicts_with = "item")]
    agent: Option<String>,
    /// Repo name or owner/name; with --item, the item's repo (default: the cwd's)
    #[arg(long)]
    repo: Option<String>,
    /// Only open tickets and PRs
    #[arg(long, conflicts_with = "item")]
    open: bool,
    /// Print the server's JSON verbatim
    #[arg(long)]
    json: bool,
    /// Rows to show (1-200)
    #[arg(long, default_value_t = 50, conflicts_with = "item")]
    limit: usize,
    /// One ticket or PR's timeline instead of the list
    #[arg(long, value_name = "N")]
    item: Option<i64>,
}

pub(crate) fn run_history(client: &ApiClient, args: HistoryArgs) -> Result<()> {
    match args.item {
        Some(number) => {
            let cwd = env::current_dir()?;
            let name = item_repo_name(&ProcessTools, &cwd, args.repo.as_deref())?;
            let path = format!("/t/{}/{number}", encode_query_component(&name));
            let response = client.request("GET", &format!("{path}?format=json"), None)?;
            if response.status == 404 {
                eprintln!("Not tracked: sm has no record of {name} #{number}.");
                process::exit(1);
            }
            if args.json {
                println!("{}", response.body.trim_end());
                return Ok(());
            }
            let payload = response.into_json()?;
            for line in timeline_lines(&payload) {
                println!("{line}");
            }
            println!("{}", page_url(&payload, || client.url_for(&path)));
        }
        None => {
            let query = list_query(&args);
            let response = client.request("GET", &format!("/history?format=json{query}"), None)?;
            if args.json {
                if !(200..300).contains(&response.status) {
                    return Err(response.into_json().unwrap_err());
                }
                println!("{}", response.body.trim_end());
                return Ok(());
            }
            let payload = response.into_json()?;
            for line in list_lines(&payload) {
                println!("{line}");
            }
            let page_path = match query.strip_prefix('&') {
                Some(query) => format!("/history?{query}"),
                None => "/history".to_owned(),
            };
            println!("{}", page_url(&payload, || client.url_for(&page_path)));
        }
    }
    Ok(())
}

/// `&agent=…&repo=…&open=1&limit=N`, the list filters.
fn list_query(args: &HistoryArgs) -> String {
    let mut query = String::new();
    for (key, value) in [("agent", &args.agent), ("repo", &args.repo)] {
        if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            query.push_str(&format!("&{key}={}", encode_query_component(value)));
        }
    }
    if args.open {
        query.push_str("&open=1");
    }
    if args.limit != 50 {
        query.push_str(&format!("&limit={}", args.limit));
    }
    query
}

/// The page URL: the browser address when the server has one, else this
/// client's.
fn page_url(payload: &Value, fallback: impl FnOnce() -> String) -> String {
    payload["page_url"]
        .as_str()
        .filter(|url| !url.is_empty())
        .map_or_else(fallback, str::to_owned)
}

/// The repo name `/t/<repo-name>/<n>` takes: `--repo` (name or
/// owner/name), else the cwd checkout's.
pub(crate) fn item_repo_name(
    tools: &dyn DocTools,
    cwd: &Path,
    repo_flag: Option<&str>,
) -> Result<String> {
    if let Some(repo) = repo_flag.map(str::trim).filter(|repo| !repo.is_empty()) {
        return Ok(repo.rsplit('/').next().unwrap_or(repo).to_owned());
    }
    let slug = run_ok(tools, "git", cwd, &["rev-parse", "--show-toplevel"])
        .and_then(|root| resolve_repo_slug(tools, Path::new(&root)))
        .map_err(|_| anyhow!("run this from the repo's checkout, or pass --repo owner/name"))?;
    Ok(slug.rsplit('/').next().unwrap_or(&slug).to_owned())
}

fn repo_name_of(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn flag_label(id: &str) -> &str {
    match id {
        "two_agents" => "2 agents",
        "open_after_merge" => "open after merge",
        "no_live_holder" => "no live holder",
        "worktree_left" => "worktree left",
        other => other,
    }
}

fn row_line(row: &Value, show_repo: bool) -> String {
    let repo = row["repo"].as_str().unwrap_or_default();
    let number = row["number"].as_i64().unwrap_or_default();
    let kind = if row["kind"].as_str() == Some("pr") {
        "PR "
    } else {
        ""
    };
    let id = if show_repo {
        format!("{kind}{}#{number}", repo_name_of(repo))
    } else {
        format!("{kind}#{number}")
    };
    let title = row["title"].as_str().unwrap_or_default();
    let title = if title.is_empty() {
        "(not fetched yet)".to_owned()
    } else {
        truncate(title, 48)
    };
    let mut parts = vec![
        id,
        row["state"].as_str().unwrap_or_default().to_owned(),
        title,
    ];
    let flags: Vec<&str> = row["flags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(flag_label)
        .collect();
    if !flags.is_empty() {
        parts.push(format!("[{}]", flags.join(", ")));
    }
    if row["kind"].as_str() != Some("pr") {
        for pr in row["prs"].as_array().into_iter().flatten() {
            let mut text = format!(
                "PR #{} {}",
                pr["number"].as_i64().unwrap_or_default(),
                pr["state"].as_str().unwrap_or_default()
            );
            let codex = pr["codex_requested"].as_u64().unwrap_or_default();
            if codex > 0 {
                text.push_str(&format!(" {codex} Codex"));
            }
            parts.push(text);
        }
    } else if let Some(pr) = row["prs"].as_array().and_then(|prs| prs.first()) {
        let codex = pr["codex_requested"].as_u64().unwrap_or_default();
        if codex > 0 {
            parts.push(format!("{codex} Codex"));
        }
    }
    let docs = row["docs"].as_array().map_or(0, Vec::len);
    if docs > 0 {
        parts.push(format!("docs {docs}"));
    }
    let agents: Vec<String> = row["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|agent| {
            let name = agent["name"].as_str().unwrap_or_default();
            match agent["state"].as_str() {
                Some("retired") => format!("{name} (retired)"),
                Some("stopped") => format!("{name} (stopped)"),
                _ => name.to_owned(),
            }
        })
        .collect();
    if !agents.is_empty() {
        parts.push(agents.join(", "));
    }
    if let Some(at) = row["last_activity"].as_str() {
        parts.push(claims::coarse_age(at));
    }
    parts.join("  ")
}

/// One line per row, as the page's cards; repo names when rows span repos.
pub(crate) fn list_lines(payload: &Value) -> Vec<String> {
    let rows = payload["rows"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return vec!["No tracked tickets.".to_owned()];
    }
    let repos: BTreeSet<&str> = rows.iter().filter_map(|r| r["repo"].as_str()).collect();
    let mut lines: Vec<String> = rows.iter().map(|r| row_line(r, repos.len() > 1)).collect();
    if payload["next_before"].is_string() {
        lines.push("(more on the page)".to_owned());
    }
    lines
}

/// `2026-09-24 14:02`, local when the offset is known, else UTC.
fn local_stamp(at: &str) -> String {
    let Ok(utc) = OffsetDateTime::parse(at, &Rfc3339) else {
        return at.to_owned();
    };
    let local = time::UtcOffset::current_local_offset()
        .map(|offset| utc.to_offset(offset))
        .unwrap_or(utc);
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        local.year(),
        u8::from(local.month()),
        local.day(),
        local.hour(),
        local.minute()
    )
}

/// The item line, its PRs, then one line per timeline entry.
pub(crate) fn timeline_lines(payload: &Value) -> Vec<String> {
    let item = &payload["item"];
    let mut lines = vec![row_line(item, true)];
    for ticket in item["linked_tickets"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  links ticket #{}",
            ticket.as_i64().unwrap_or_default()
        ));
    }
    for doc in item["docs"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  doc  {}  {}",
            doc["state"].as_str().unwrap_or_default().replace('_', " "),
            doc["title"].as_str().unwrap_or_default()
        ));
    }
    let events = payload["events"].as_array().cloned().unwrap_or_default();
    if events.is_empty() {
        lines.push("  Nothing recorded yet.".to_owned());
    }
    for event in &events {
        let name = event["name"].as_str().unwrap_or("-");
        let mut line = format!(
            "  {}  {name}  {}",
            local_stamp(event["at"].as_str().unwrap_or_default()),
            event["text"].as_str().unwrap_or_default()
        );
        if let Some(link) = event["link"].as_str().filter(|l| l.starts_with("https://")) {
            line.push_str(&format!("  {link}"));
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests;
