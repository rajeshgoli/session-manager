//! `sm board` (sm#1665): prints the lanes, records ticket order on GitHub,
//! and adds lanes. Spec: `docs/working/1665_sm_board.html`, appendix E.

use super::*;
use crate::claims::resolve_claim_target;
use crate::git_repo::ProcessTools;

/// GitHub or sm refused.
const EXIT_REFUSED: i32 = 1;
/// A usage error.
const EXIT_USAGE: i32 = 2;

#[derive(Args)]
pub(crate) struct BoardArgs {
    #[command(subcommand)]
    command: Option<BoardCommand>,
    /// Only this lane: its goal ticket
    #[arg(long, value_name = "REF")]
    lane: Option<String>,
    /// Print the board JSON unchanged
    #[arg(long)]
    json: bool,
    /// owner/name, or a bare repo name with the cwd repo's owner: the repo
    /// of tickets written as plain numbers (default: the cwd repo)
    #[arg(long, global = true)]
    repo: Option<String>,
}

#[derive(Subcommand)]
enum BoardCommand {
    /// Record that TICKET starts after each BLOCKER
    After {
        ticket: String,
        #[arg(required = true)]
        blockers: Vec<String>,
        /// Remove the links instead
        #[arg(long)]
        remove: bool,
    },
    /// Make TICKET a sub-issue of PARENT
    Under {
        ticket: String,
        parent: String,
        /// Remove the link instead
        #[arg(long)]
        remove: bool,
    },
    /// Add a lane
    Lane {
        #[command(subcommand)]
        command: LaneCommand,
    },
}

#[derive(Subcommand)]
enum LaneCommand {
    /// Add a lane with goal GOAL at the bottom of the list
    Add { goal: String },
}

/// Ticket references resolve against `--repo`, else the cwd repo, found
/// only when a reference needs it.
struct Refs {
    repo_flag: Option<String>,
    default: Option<Result<String, String>>,
}

impl Refs {
    fn default_repo(&mut self) -> Result<String, String> {
        if self.default.is_none() {
            let resolved = env::current_dir()
                .map_err(|error| error.to_string())
                .and_then(|cwd| {
                    resolve_claim_target(&ProcessTools, &cwd, self.repo_flag.as_deref())
                        .map(|target| target.repo)
                        .map_err(|error| error.to_string())
                });
            self.default = Some(resolved);
        }
        self.default.clone().unwrap_or_else(|| Err(String::new()))
    }

    fn parse(&mut self, text: &str) -> Result<(String, i64), String> {
        let full = text.contains('/');
        let default = if full {
            String::new()
        } else {
            self.default_repo()?
        };
        parse_ticket_ref(text, &default).ok_or_else(|| format!("not a ticket: {text}"))
    }
}

/// `1654`, `#1654`, `name#1654` (the default repo's owner) or
/// `owner/name#1654`.
pub(crate) fn parse_ticket_ref(text: &str, default_repo: &str) -> Option<(String, i64)> {
    sm_server::board::parse_ticket_ref(text, default_repo)
}

fn usage(message: &str) -> ! {
    eprintln!("{message}");
    process::exit(EXIT_USAGE);
}

pub(crate) fn run_board(client: &ApiClient, args: BoardArgs) -> Result<()> {
    let mut refs = Refs {
        repo_flag: args.repo.clone(),
        default: None,
    };
    let session_id = optional_current_session_id();
    match args.command {
        None => {
            let mut path = "/board?format=json".to_owned();
            if let Some(lane) = args.lane.as_deref() {
                let (repo, number) = refs.parse(lane).unwrap_or_else(|error| usage(&error));
                path.push_str(&format!(
                    "&lane={}",
                    encode_query_component(&format!("{repo}#{number}"))
                ));
            }
            let response = client.request("GET", &path, None)?;
            let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
            if response.status != 200 {
                refused(response.status, &body);
            }
            if args.json {
                println!("{}", serde_json::to_string_pretty(&body)?);
            } else {
                for line in board_lines(&body, OffsetDateTime::now_utc()) {
                    println!("{line}");
                }
            }
        }
        Some(BoardCommand::After {
            ticket,
            blockers,
            remove,
        }) => {
            let ticket = refs.parse(&ticket).unwrap_or_else(|error| usage(&error));
            let blockers: Vec<_> = blockers
                .iter()
                .map(|blocker| refs.parse(blocker).unwrap_or_else(|error| usage(&error)))
                .collect();
            for blocker in blockers {
                post_link(client, &ticket, &blocker, "after", remove, &session_id)?;
            }
        }
        Some(BoardCommand::Under {
            ticket,
            parent,
            remove,
        }) => {
            let ticket = refs.parse(&ticket).unwrap_or_else(|error| usage(&error));
            let parent = refs.parse(&parent).unwrap_or_else(|error| usage(&error));
            post_link(client, &ticket, &parent, "under", remove, &session_id)?;
        }
        Some(BoardCommand::Lane {
            command: LaneCommand::Add { goal },
        }) => {
            let (repo, number) = refs.parse(&goal).unwrap_or_else(|error| usage(&error));
            let response = client.request(
                "POST",
                "/board/lanes",
                Some(json!({ "repo": repo, "number": number, "session_id": session_id })),
            )?;
            let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
            if response.status != 200 {
                refused(response.status, &body);
            }
            println!("{}", body["message"].as_str().unwrap_or_default());
        }
    }
    Ok(())
}

fn post_link(
    client: &ApiClient,
    ticket: &(String, i64),
    target: &(String, i64),
    kind: &str,
    remove: bool,
    session_id: &Option<String>,
) -> Result<()> {
    let response = client.request(
        "POST",
        "/board/links",
        Some(json!({
            "repo": ticket.0,
            "number": ticket.1,
            "target_repo": target.0,
            "target_number": target.1,
            "kind": kind,
            "remove": remove,
            "session_id": session_id,
        })),
    )?;
    let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    if response.status != 200 {
        refused(response.status, &body);
    }
    println!("{}", body["message"].as_str().unwrap_or_default());
    Ok(())
}

fn refused(status: u16, body: &Value) -> ! {
    let detail = body["detail"]
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("HTTP {status}: {body}"));
    eprintln!("{detail}");
    io::stdout().flush().ok();
    process::exit(EXIT_REFUSED);
}

fn short_ref(repo: &str, number: i64, base: &str) -> String {
    if repo == base {
        format!("#{number}")
    } else {
        format!("{}#{number}", repo.rsplit('/').next().unwrap_or(repo))
    }
}

fn warning_words(warning: &str) -> &str {
    match warning {
        "working_while_blocked" => "working while blocked",
        "holder_stopped" => "agent stopped",
        "merged_not_closed" => "PR merged — close the ticket",
        "cycle" => "waits in a loop",
        "stale" => "stale",
        other => other,
    }
}

fn state_words(state: &str) -> &str {
    match state {
        "needs_you" => "needs you",
        "close_ready" => "all parts done: close",
        "in_progress" => "in progress",
        other => other,
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let cut: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn age(timestamp: &str, now: OffsetDateTime) -> Option<String> {
    let then = OffsetDateTime::parse(timestamp, &Rfc3339).ok()?;
    let seconds = (now - then).whole_seconds().max(0);
    Some(match seconds {
        0..=119 => format!("{seconds}s ago"),
        120..=7199 => format!("{}m ago", seconds / 60),
        _ => format!("{}h ago", seconds / 3600),
    })
}

/// One ticket's line: state, reference, title, detail, then warnings.
fn ticket_line(ticket: &Value, base: &str) -> String {
    let repo = ticket["repo"].as_str().unwrap_or_default();
    let number = ticket["number"].as_i64().unwrap_or_default();
    let state = ticket["state"].as_str().unwrap_or_default();
    let mut detail: Vec<String> = Vec::new();
    match state {
        "needs_you" => {
            if let Some(text) = ticket["needs_you"]["text"].as_str() {
                detail.push(text.to_owned());
            }
        }
        "in_progress" => {
            if let Some(name) = ticket["holder"]["name"].as_str() {
                detail.push(format!(
                    "{name} ({})",
                    ticket["holder"]["state"].as_str().unwrap_or_default()
                ));
            }
        }
        "done" => {
            if let Some(reason) = ticket["done_reason"].as_str() {
                detail.push(reason.replace('_', " "));
            }
        }
        _ => {}
    }
    if state != "done" {
        let open: Vec<String> = ticket["waits_on"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|blocker| blocker["state"].as_str() != Some("done"))
            .map(|blocker| {
                short_ref(
                    blocker["repo"].as_str().unwrap_or_default(),
                    blocker["number"].as_i64().unwrap_or_default(),
                    base,
                )
            })
            .collect();
        if !open.is_empty() && state != "ready" {
            detail.push(format!("waits on {}", open.join(" ")));
        }
        let prs: Vec<String> = ticket["prs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|pr| pr["state"].as_str() == Some("OPEN"))
            .map(|pr| format!("PR #{}", pr["number"].as_i64().unwrap_or_default()))
            .collect();
        if !prs.is_empty() {
            detail.push(prs.join(" "));
        }
    }
    if ticket["sub_issues_done"].as_bool() == Some(true) && state != "done" {
        detail.push("all sub-issues done".to_owned());
    }
    if ticket["new"].as_bool() == Some(true) {
        detail.push("new".to_owned());
    }
    for lane in ticket["also_in"].as_array().into_iter().flatten() {
        detail.push(format!("also in lane {}", lane["rank"]));
    }
    let title = ticket["title"].as_str().unwrap_or_default();
    let mut line = format!(
        "  {:<12} {:<6} ",
        state_words(state),
        short_ref(repo, number, base)
    );
    if detail.is_empty() {
        line.push_str(&truncate(title, 60));
    } else {
        line.push_str(&format!(
            "{:<40} {}",
            truncate(title, 40),
            detail.join("  ")
        ));
    }
    let warnings: Vec<&str> = ticket["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(warning_words)
        .collect();
    if !warnings.is_empty() {
        line.push_str(&format!("  ! {}", warnings.join(", ")));
    }
    line.trim_end().to_owned()
}

/// The `sm board` text (appendix E).
pub(crate) fn board_lines(payload: &Value, now: OffsetDateTime) -> Vec<String> {
    let mut lines = Vec::new();
    let count = payload["unseen"]["count"].as_u64().unwrap_or(0);
    lines.push(match count {
        0 => "Board: no unseen alerts".to_owned(),
        1 => "Board: 1 unseen alert".to_owned(),
        n => format!("Board: {n} unseen alerts"),
    });
    let repos: BTreeMap<&str, &Value> = payload["repos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|repo| Some((repo["repo"].as_str()?, repo)))
        .collect();
    let lanes = payload["lanes"].as_array().cloned().unwrap_or_default();
    if lanes.is_empty() {
        lines.push("No lanes. Add one with sm board lane add <goal>.".to_owned());
    }
    for lane in &lanes {
        let goal_repo = lane["goal"]["repo"].as_str().unwrap_or_default();
        let read = repos.get(goal_repo).map(|repo| {
            let last_ok = repo["last_ok_at"].as_str().unwrap_or_default();
            if repo["stale"].as_bool() == Some(true) {
                format!(
                    "stale since {}",
                    if last_ok.is_empty() { "never" } else { last_ok }
                )
            } else {
                age(last_ok, now)
                    .map(|age| format!("read {age}"))
                    .unwrap_or_else(|| "not read yet".to_owned())
            }
        });
        lines.push(format!(
            "Lane {}  {}#{}  {}   ({})",
            lane["rank"],
            goal_repo,
            lane["goal"]["number"],
            lane["goal"]["title"].as_str().unwrap_or_default(),
            read.unwrap_or_else(|| "not read yet".to_owned())
        ));
        let counts = &lane["counts"];
        let chain: Vec<String> = lane["longest_chain"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|key| {
                short_ref(
                    key["repo"].as_str().unwrap_or_default(),
                    key["number"].as_i64().unwrap_or_default(),
                    goal_repo,
                )
            })
            .collect();
        lines.push(format!(
            "  {} needs you · {} all parts done · {} ready · {} in progress · {} blocked · {} done · longest chain {}: {}",
            counts["needs_you"],
            counts["close_ready"],
            counts["ready"],
            counts["in_progress"],
            counts["blocked"],
            counts["done"],
            chain.len(),
            chain.join(" → ")
        ));
        for cycle in lane["cycles"].as_array().into_iter().flatten() {
            let refs: Vec<String> = cycle
                .as_array()
                .into_iter()
                .flatten()
                .map(|key| {
                    short_ref(
                        key["repo"].as_str().unwrap_or_default(),
                        key["number"].as_i64().unwrap_or_default(),
                        goal_repo,
                    )
                })
                .collect();
            lines.push(format!("  ! {} wait on each other", refs.join(", ")));
        }
        for ticket in lane["tickets"].as_array().into_iter().flatten() {
            lines.push(ticket_line(ticket, goal_repo));
        }
    }
    let other: Vec<&Value> = payload["other"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|repo| repo["tickets"].as_array().is_some_and(|t| !t.is_empty()))
        .collect();
    if !other.is_empty() {
        lines.push("Not in any lane".to_owned());
        for repo in other {
            let name = repo["repo"].as_str().unwrap_or_default();
            for ticket in repo["tickets"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "  {name}: {} #{} {}",
                    state_words(ticket["state"].as_str().unwrap_or_default()),
                    ticket["number"],
                    truncate(ticket["title"].as_str().unwrap_or_default(), 60)
                ));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests;
