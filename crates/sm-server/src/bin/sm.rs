use std::{
    env, fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process, thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

const DEFAULT_API_URL: &str = "http://127.0.0.1:8420";
const CLIENT_CONFIG_ENV: &str = "SM_CLIENT_CONFIG";
const CLIENT_CONFIG_SUBPATH: &str = "session-manager/client.yaml";

#[derive(Parser)]
#[command(name = "sm", version, about = "Session Manager Rust CLI")]
struct Cli {
    #[arg(long, global = true, value_name = "URL")]
    api_url: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status(StatusArgs),
    Me(EmptyArgs),
    Who(EmptyArgs),
    All(EmptyArgs),
    Send(SendArgs),
    Wait(WaitArgs),
    Spawn(SpawnArgs),
    Fork(ForkArgs),
    New(NewArgs),
    Children(ChildrenArgs),
    Tail(TailArgs),
    Retire(SessionIdArgs),
    Restore(SessionIdArgs),
    Attach(SessionIdArgs),
    Output(OutputArgs),
    Clear(ClearArgs),
    Handoff(HandoffArgs),
    #[command(name = "task-complete")]
    TaskComplete(EmptyArgs),
    #[command(name = "turn-complete")]
    TurnComplete(EmptyArgs),
    #[command(name = "context-monitor")]
    ContextMonitor(ContextMonitorArgs),
    Email(EmailArgs),
    Maintainer(MaintainerArgs),
    Register(RegisterArgs),
    Unregister(RegisterArgs),
    Lookup(LookupArgs),
    Roster(EmptyArgs),
    Queue(QueueArgs),
    Review(ReviewArgs),
    #[command(name = "request-codex-review")]
    RequestCodexReview(RequestCodexReviewArgs),
    Claude(ProviderLaunchArgs),
    Codex(ProviderLaunchArgs),
    #[command(name = "codex-app")]
    CodexApp(ProviderLaunchArgs),
    #[command(name = "codex-fork")]
    CodexFork(ProviderLaunchArgs),
    #[command(name = "codex-2")]
    Codex2(ProviderLaunchArgs),
    Watch(EmptyArgs),
}

#[derive(Args)]
struct EmptyArgs {}

#[derive(Args)]
struct StatusArgs {
    text: Vec<String>,
}

#[derive(Args)]
struct SendArgs {
    session_id: String,
    text: Vec<String>,
    #[arg(long)]
    urgent: bool,
    #[arg(long, value_name = "SECONDS")]
    wait: Option<u64>,
}

#[derive(Args)]
struct WaitArgs {
    session_id: String,
    seconds: u64,
}

#[derive(Args)]
struct SpawnArgs {
    provider: String,
    prompt: Vec<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long, value_name = "SECONDS")]
    wait: Option<u64>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    working_dir: Option<String>,
    #[arg(long)]
    node: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long, hide = true)]
    id: Option<String>,
}

#[derive(Args)]
struct ForkArgs {
    #[arg(long = "self")]
    self_: bool,
    #[arg(long)]
    attach: bool,
}

#[derive(Args)]
struct NewArgs {
    working_dir: Option<String>,
}

#[derive(Args)]
struct ChildrenArgs {
    session_id: Option<String>,
    #[arg(long)]
    recursive: bool,
    #[arg(long)]
    terminated: bool,
    #[arg(long, value_parser = ["running", "completed", "error", "all"])]
    status: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct SessionIdArgs {
    session_id: String,
}

#[derive(Args)]
struct ClearArgs {
    session_id: String,
    prompt: Vec<String>,
}

#[derive(Args)]
struct OutputArgs {
    session_id: String,
    #[arg(long, default_value_t = 50)]
    lines: usize,
}

#[derive(Args)]
struct TailArgs {
    session_id: String,
    #[arg(long)]
    raw: bool,
    #[arg(long, default_value_t = 50)]
    lines: usize,
}

#[derive(Args)]
struct HandoffArgs {
    file_path: Option<String>,
}

#[derive(Args)]
struct ContextMonitorArgs {
    #[command(subcommand)]
    command: Option<ContextMonitorCommand>,
}

#[derive(Subcommand)]
enum ContextMonitorCommand {
    Enable { target: Option<String> },
    Disable { target: Option<String> },
    Status,
}

#[derive(Args)]
struct EmailArgs {
    recipient: Option<String>,
    #[arg(long)]
    subject: Option<String>,
    #[arg(long)]
    body: Option<String>,
}

#[derive(Args)]
struct MaintainerArgs {
    #[arg(long)]
    clear: bool,
}

#[derive(Args)]
struct RegisterArgs {
    role: Option<String>,
    session_id: Option<String>,
}

#[derive(Args)]
struct LookupArgs {
    role: Option<String>,
}

#[derive(Args)]
struct QueueArgs {
    #[command(subcommand)]
    command: QueueCommand,
}

#[derive(Subcommand)]
enum QueueCommand {
    Run(QueueRunArgs),
    List(QueueListArgs),
    Status(QueueStatusArgs),
    Cancel(QueueCancelArgs),
}

#[derive(Args)]
struct QueueRunArgs {
    #[arg(long)]
    script_file: Option<String>,
    #[arg(long)]
    notify: Option<String>,
}

#[derive(Args)]
struct QueueListArgs {
    #[arg(long)]
    state: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct QueueStatusArgs {
    job_id: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct QueueCancelArgs {
    job_id: String,
}

#[derive(Args)]
struct ReviewArgs {
    #[arg(long)]
    base: Option<String>,
    #[arg(long)]
    uncommitted: bool,
    #[arg(long)]
    commit: Option<String>,
    #[arg(long)]
    custom: Option<String>,
    #[arg(long)]
    new: bool,
    #[arg(long)]
    pr: Option<u64>,
}

#[derive(Args)]
struct RequestCodexReviewArgs {
    #[arg(long)]
    notify: Option<String>,
    #[arg(long)]
    repo: Option<String>,
    #[command(subcommand)]
    command: Option<RequestCodexReviewCommand>,
}

#[derive(Subcommand)]
enum RequestCodexReviewCommand {
    List,
    Status { request_id: Option<String> },
    Cancel { request_id: Option<String> },
}

#[derive(Args)]
struct ProviderLaunchArgs {
    working_dir: Option<String>,
}

struct ApiClient {
    scheme: String,
    authority: String,
    host: String,
    port: u16,
    path_prefix: String,
}

struct ApiResponse {
    status: u16,
    body: String,
}

fn main() {
    retire_removed_surface_if_requested();
    if let Err(error) = run() {
        eprintln!("{error:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let api_url = resolve_api_url(cli.api_url)?;
    let client = ApiClient::parse(&api_url)?;

    match cli.command {
        Command::Status(args) => {
            if !args.text.is_empty() {
                let session_id = current_session_id()?;
                let text = args.text.join(" ");
                client.post_json(
                    &format!("/sessions/{session_id}/agent-status"),
                    json!({ "text": text }),
                )?;
                println!("Status set: {text}");
            } else {
                let payload = client.get_json("/sessions")?;
                let sessions = payload["sessions"].as_array().cloned().unwrap_or_default();
                if sessions.is_empty() {
                    println!("No active sessions");
                } else {
                    for session in sessions {
                        let id = session["id"].as_str().unwrap_or("unknown");
                        let status = session["status"].as_str().unwrap_or("unknown");
                        let name = session["friendly_name"]
                            .as_str()
                            .or_else(|| session["name"].as_str())
                            .unwrap_or(id);
                        println!("{id} {status} {name}");
                    }
                }
            }
        }
        Command::Me(_) => {
            let session_id = current_session_id()?;
            let session = client.get_json(&format!("/sessions/{session_id}"))?;
            println!("{}", format_session_line(&session, true));
        }
        Command::Who(_) => {
            let session_id = current_session_id()?;
            let current = client.get_json(&format!("/sessions/{session_id}"))?;
            let working_dir = current["working_dir"].as_str().unwrap_or_default();
            let payload = client.get_json("/sessions")?;
            let sessions = payload["sessions"].as_array().cloned().unwrap_or_default();
            let mut found = false;
            for session in sessions {
                if session["id"].as_str() == Some(session_id.as_str()) {
                    continue;
                }
                if session["working_dir"].as_str() != Some(working_dir) {
                    continue;
                }
                if !matches!(
                    session["status"].as_str().unwrap_or_default(),
                    "running" | "waiting_permission" | "idle"
                ) {
                    continue;
                }
                println!("{}", format_session_line(&session, false));
                found = true;
            }
            if !found {
                process::exit(1);
            }
        }
        Command::All(_) => {
            let payload = client.get_json("/sessions")?;
            let sessions = payload["sessions"].as_array().cloned().unwrap_or_default();
            if sessions.is_empty() {
                println!("No active sessions");
                process::exit(1);
            }
            for session in sessions {
                println!("{}", format_session_line(&session, true));
            }
        }
        Command::Spawn(args) => {
            let prompt = args.prompt.join(" ");
            let parent_session_id = optional_current_session_id();
            let payload = if let Some(parent_session_id) = parent_session_id {
                client.post_json(
                    "/sessions/spawn",
                    json!({
                        "id": args.id,
                        "parent_session_id": parent_session_id,
                        "prompt": prompt,
                        "name": args.name,
                        "wait": args.wait,
                        "model": args.model,
                        "working_dir": args.working_dir,
                        "provider": args.provider,
                        "node": args.node
                    }),
                )?
            } else {
                client.post_json(
                    "/sessions",
                    json!({
                        "id": args.id,
                        "name": args.name,
                        "working_dir": args.working_dir,
                        "provider": args.provider,
                        "node": args.node,
                        "initial_message": prompt,
                        "model": args.model,
                        "wait": args.wait
                    }),
                )?
            };
            if let Some(error) = payload["error"]
                .as_str()
                .or_else(|| payload["detail"].as_str())
            {
                bail!("{error}");
            }
            if args.json {
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!(
                    "{}",
                    payload["session_id"]
                        .as_str()
                        .or_else(|| payload["id"].as_str())
                        .ok_or_else(|| anyhow!("spawn response missing id"))?
                );
            }
        }
        Command::Send(args) => {
            let text = args.text.join(" ");
            if text.trim().is_empty() {
                bail!("send text is required");
            }
            let payload = client.post_json(
                &format!("/sessions/{}/input", args.session_id),
                json!({
                    "text": text,
                    "delivery_mode": if args.urgent { "urgent" } else { "sequential" },
                    "notify_after_seconds": args.wait
                }),
            )?;
            println!(
                "{}",
                if payload["delivered"].as_bool().unwrap_or(false) {
                    "delivered"
                } else {
                    "not delivered"
                }
            );
        }
        Command::Output(args) => print_output(&client, &args.session_id, args.lines)?,
        Command::Tail(args) => print_output(&client, &args.session_id, args.lines)?,
        Command::Children(args) => {
            let parent_session_id = match args.session_id {
                Some(target) => {
                    let session = client.get_json(&format!("/sessions/{target}"))?;
                    session["id"]
                        .as_str()
                        .ok_or_else(|| anyhow!("session response missing id"))?
                        .to_owned()
                }
                None => current_session_id()?,
            };
            let mut query = Vec::new();
            if args.recursive {
                query.push("recursive=true".to_owned());
            }
            if args.terminated {
                query.push("include_terminated=true".to_owned());
            }
            if let Some(status) = args.status {
                query.push(format!("status={status}"));
            }
            let path = if query.is_empty() {
                format!("/sessions/{parent_session_id}/children")
            } else {
                format!("/sessions/{parent_session_id}/children?{}", query.join("&"))
            };
            let payload = client.get_json(&path)?;
            let children = payload["children"].as_array().cloned().unwrap_or_default();
            if args.json {
                println!("{}", serde_json::to_string_pretty(&children)?);
            } else if children.is_empty() {
                println!("No child sessions");
            } else {
                for child in children {
                    println!("{}", format_child_line(&child));
                }
            }
        }
        Command::Retire(args) => {
            let payload =
                client.post_json(&format!("/sessions/{}/kill", args.session_id), json!({}))?;
            println!("{}", payload["status"].as_str().unwrap_or("stopped"));
        }
        Command::Restore(args) => {
            let payload =
                client.post_json(&format!("/sessions/{}/restore", args.session_id), json!({}))?;
            println!(
                "Session restored: {}",
                payload["id"].as_str().unwrap_or(&args.session_id)
            );
        }
        Command::Attach(args) => attach_session(&client, &args.session_id)?,
        Command::Clear(args) => {
            let requester_session_id = optional_current_session_id();
            ensure_clear_authorized(&client, &args.session_id, requester_session_id.as_deref())?;
            let prompt = args.prompt.join(" ");
            let prompt = (!prompt.trim().is_empty()).then_some(prompt);
            let payload = client.post_json(
                &format!("/sessions/{}/clear", args.session_id),
                json!({
                    "prompt": prompt,
                    "requester_session_id": requester_session_id
                }),
            )?;
            println!(
                "{} {}",
                payload["status"].as_str().unwrap_or("cleared"),
                payload["session_id"].as_str().unwrap_or(&args.session_id)
            );
        }
        Command::Handoff(args) => {
            let session_id = current_session_id()?;
            let file_path = args.file_path.unwrap_or_else(|| "HANDOFF.md".to_owned());
            let absolute = fs::canonicalize(&file_path)
                .with_context(|| format!("File not found: {file_path}"))?;
            let payload = client.post_json(
                &format!("/sessions/{session_id}/handoff"),
                json!({
                    "requester_session_id": session_id,
                    "file_path": absolute.display().to_string()
                }),
            )?;
            if let Some(error) = payload["error"].as_str() {
                bail!("{error}");
            }
            match payload["status"].as_str() {
                Some("executed") => println!("Handoff executed"),
                Some("recorded") => println!("Handoff recorded"),
                _ => println!("Handoff scheduled - will execute after current turn completes"),
            }
        }
        Command::TaskComplete(_) => {
            let Some(session_id) = optional_current_session_id() else {
                eprintln!(
                    "Error: SESSION_MANAGER_ID not set. sm task-complete can only be called from within a session."
                );
                process::exit(2);
            };
            let payload = client.post_json(
                &format!("/sessions/{session_id}/task-complete"),
                json!({ "requester_session_id": session_id }),
            )?;
            if payload["error"].as_str().is_some() {
                bail!("Failed to mark task complete");
            }
            if payload["em_notified"].as_bool().unwrap_or(false) {
                println!("Task complete. Remind cancelled. EM notified.");
            } else {
                println!(
                    "Task complete. Remind cancelled. (No EM registered - no notification sent.)"
                );
            }
        }
        Command::TurnComplete(_) => {
            let Some(session_id) = optional_current_session_id() else {
                eprintln!(
                    "Error: SESSION_MANAGER_ID not set. sm turn-complete can only be called from within a session."
                );
                process::exit(2);
            };
            let payload = client.post_json(
                &format!("/sessions/{session_id}/turn-complete"),
                json!({ "requester_session_id": session_id }),
            )?;
            if payload["error"].as_str().is_some() {
                bail!("Failed to mark turn complete");
            }
            println!("Turn complete. Remind cancelled until new work is assigned.");
        }
        Command::ContextMonitor(args) => {
            run_context_monitor(&client, args)?;
        }
        Command::Maintainer(args) => {
            let session_id = current_session_id()?;
            let body = json!({ "requester_session_id": session_id });
            if args.clear {
                client.delete_json(&format!("/sessions/{session_id}/maintainer"), body)?;
                println!("Maintainer alias cleared");
            } else {
                client.put_json(&format!("/sessions/{session_id}/maintainer"), body)?;
                println!("Maintainer alias registered: maintainer -> {session_id}");
            }
        }
        Command::Register(args) => {
            let session_id = current_session_id()?;
            if args.session_id.is_some() {
                bail!("sm register is self-directed; pass only the role");
            }
            let role = required_positional(args.role, "role")?;
            let payload = client.post_json(
                &format!("/sessions/{session_id}/registry"),
                json!({ "requester_session_id": session_id, "role": role }),
            )?;
            let role_name = payload["role"].as_str().unwrap_or(&role);
            let target_session = payload["session_id"].as_str().unwrap_or(&session_id);
            println!("Registered: {role_name} -> {target_session}");
        }
        Command::Unregister(args) => {
            let session_id = current_session_id()?;
            if args.session_id.is_some() {
                bail!("sm unregister is self-directed; pass only the role");
            }
            let role = required_positional(args.role, "role")?;
            let payload = client.delete_json(
                &format!("/sessions/{session_id}/registry"),
                json!({ "requester_session_id": session_id, "role": role }),
            )?;
            let role_name = payload["role"].as_str().unwrap_or(&role);
            println!("Unregistered: {role_name}");
        }
        Command::Lookup(args) => {
            let identifier = required_positional(args.role, "role")?;
            if let Some(session_id) = lookup_identifier(&client, &identifier)? {
                println!("{session_id}");
            } else {
                bail!("Role not registered");
            }
        }
        Command::Roster(_) => print_roster(&client)?,
        Command::Wait(args) => wait_for_session(&client, &args.session_id, args.seconds)?,
        _ => bail!("this retained command is not implemented in the Rust core slice yet"),
    }
    Ok(())
}

fn required_positional(value: Option<String>, label: &str) -> Result<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("{label} is required"))
}

fn print_output(client: &ApiClient, session_id: &str, lines: usize) -> Result<()> {
    let payload = client.get_json(&format!("/sessions/{session_id}/output?lines={lines}"))?;
    if let Some(output) = payload["output"].as_str() {
        print!("{output}");
    }
    Ok(())
}

fn wait_for_session(client: &ApiClient, session_id: &str, seconds: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let payload = client.get_json(&format!("/sessions/{session_id}"))?;
        let status = payload["status"].as_str().unwrap_or("unknown");
        if matches!(status, "idle" | "completed" | "stopped") {
            println!("{status}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {session_id}; current status {status}");
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn run_context_monitor(client: &ApiClient, args: ContextMonitorArgs) -> Result<()> {
    match args.command.unwrap_or(ContextMonitorCommand::Status) {
        ContextMonitorCommand::Status => {
            let payload = client.get_json("/sessions/context-monitor")?;
            let monitored = payload["monitored"].as_array().cloned().unwrap_or_default();
            if monitored.is_empty() {
                println!("No sessions currently registered for context monitoring.");
                return Ok(());
            }
            println!("{:<12} {:<24} Notify Target", "Session", "Name");
            println!("{}", "-".repeat(52));
            for entry in monitored {
                let session_id = entry["session_id"].as_str().unwrap_or("unknown");
                let name = entry["friendly_name"].as_str().unwrap_or("");
                let notify = entry["notify_session_id"].as_str().unwrap_or("(none)");
                println!("{session_id:<12} {name:<24} {notify}");
            }
        }
        ContextMonitorCommand::Enable { target } => {
            let requester = current_session_id()?;
            let target = target.unwrap_or_else(|| requester.clone());
            client.post_json(
                &format!("/sessions/{target}/context-monitor"),
                json!({
                    "enabled": true,
                    "requester_session_id": requester,
                    "notify_session_id": requester
                }),
            )?;
            if target == requester {
                println!("Context monitoring enabled - notifications -> self ({requester})");
            } else {
                println!("Context monitoring enabled for {target} - notifications -> {requester}");
            }
        }
        ContextMonitorCommand::Disable { target } => {
            let requester = current_session_id()?;
            let target = target.unwrap_or_else(|| requester.clone());
            client.post_json(
                &format!("/sessions/{target}/context-monitor"),
                json!({
                    "enabled": false,
                    "requester_session_id": requester,
                    "notify_session_id": null
                }),
            )?;
            println!("Context monitoring disabled for {target}");
        }
    }
    Ok(())
}

fn lookup_identifier(client: &ApiClient, identifier: &str) -> Result<Option<String>> {
    let registry_path = format!("/registry/{}", encode_path_segment(identifier));
    let response = client.request("GET", &registry_path, None)?;
    if (200..300).contains(&response.status) {
        let payload = response.into_json()?;
        if let Some(session_id) = payload["session_id"].as_str() {
            return Ok(Some(session_id.to_owned()));
        }
        bail!("Role lookup returned no session ID");
    }
    if response.status != 404 {
        return Err(response.into_status_error());
    }

    let session_path = format!("/sessions/{}", encode_path_segment(identifier));
    let response = client.request("GET", &session_path, None)?;
    if (200..300).contains(&response.status) {
        let payload = response.into_json()?;
        if let Some(session_id) = payload["id"].as_str() {
            return Ok(Some(session_id.to_owned()));
        }
    } else if response.status != 404 {
        return Err(response.into_status_error());
    }

    let payload = client.get_json("/sessions")?;
    let sessions = payload["sessions"].as_array().cloned().unwrap_or_default();
    let exact_matches = sessions
        .iter()
        .filter(|session| {
            session["aliases"].as_array().is_some_and(|aliases| {
                aliases
                    .iter()
                    .any(|alias| alias.as_str() == Some(identifier))
            }) || session["friendly_name"].as_str() == Some(identifier)
                || session["name"].as_str() == Some(identifier)
        })
        .cloned()
        .collect::<Vec<_>>();
    if exact_matches.len() == 1 {
        return Ok(exact_matches[0]["id"].as_str().map(ToOwned::to_owned));
    }
    if exact_matches.len() > 1 {
        return bail_ambiguous_lookup(identifier, &exact_matches);
    }

    let needle = identifier.to_ascii_lowercase();
    let matches = sessions
        .iter()
        .filter(|session| {
            ["friendly_name", "name"].iter().any(|field| {
                session[*field]
                    .as_str()
                    .is_some_and(|value| value.to_ascii_lowercase().contains(&needle))
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.len() {
        1 => Ok(matches[0]["id"].as_str().map(ToOwned::to_owned)),
        count if count > 1 => bail_ambiguous_lookup(identifier, &matches),
        _ => Ok(None),
    }
}

fn bail_ambiguous_lookup(identifier: &str, matches: &[Value]) -> Result<Option<String>> {
    let labels = matches
        .iter()
        .take(5)
        .map(|session| {
            let id = session["id"].as_str().unwrap_or("unknown");
            let name = session["friendly_name"]
                .as_str()
                .or_else(|| session["name"].as_str())
                .unwrap_or(id);
            format!("{name} ({id})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if matches.len() > 5 {
        format!(", +{} more", matches.len() - 5)
    } else {
        String::new()
    };
    bail!("Multiple sessions match '{identifier}': {labels}{suffix}");
}

fn print_roster(client: &ApiClient) -> Result<()> {
    let payload = client.get_json("/registry")?;
    let registrations = payload["registrations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if registrations.is_empty() {
        println!("No registered roles or humans.");
        return Ok(());
    }

    println!("Agents");
    let rows = registrations
        .iter()
        .map(|entry| {
            vec![
                json_string(entry, "role"),
                json_string(entry, "session_id"),
                json_string(entry, "friendly_name"),
                json_string(entry, "provider"),
                entry["activity_state"]
                    .as_str()
                    .or_else(|| entry["status"].as_str())
                    .unwrap_or("")
                    .to_owned(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["Role", "Session ID", "Name", "Provider", "State"], &rows);
    Ok(())
}

fn json_string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or("").to_owned()
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let widths = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .map(|row| row.get(index).map(String::len).unwrap_or(0))
                .fold(header.len(), usize::max)
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        headers
            .iter()
            .enumerate()
            .map(|(index, header)| format!("{header:<width$}", width = widths[index]))
            .collect::<Vec<_>>()
            .join("  ")
    );
    println!(
        "{}",
        widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for row in rows {
        println!(
            "{}",
            row.iter()
                .enumerate()
                .map(|(index, value)| format!("{value:<width$}", width = widths[index]))
                .collect::<Vec<_>>()
                .join("  ")
        );
    }
}

fn attach_session(client: &ApiClient, session_id: &str) -> Result<()> {
    let response = client.get_json(&format!("/sessions/{session_id}/attach-descriptor"))?;
    let descriptor = response.get("attach").unwrap_or(&response);
    if descriptor["attach_supported"].as_bool() == Some(false) {
        let message = descriptor["message"]
            .as_str()
            .unwrap_or("Attach not supported for this session");
        bail!("{message}");
    }
    let tmux_session = descriptor["tmux_session"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("Session has no tmux session"))?;
    let mut command = process::Command::new("tmux");
    if let Some(socket_name) = descriptor["tmux_socket_name"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
    {
        command.arg("-L").arg(socket_name);
    }
    let status = command
        .arg("attach")
        .arg("-t")
        .arg(tmux_session)
        .status()
        .with_context(|| "failed to run tmux attach")?;
    if !status.success() {
        bail!("tmux attach exited with {status}");
    }
    Ok(())
}

fn ensure_clear_authorized(
    client: &ApiClient,
    target_session_id: &str,
    requester_session_id: Option<&str>,
) -> Result<()> {
    let session = client.get_json(&format!("/sessions/{target_session_id}"))?;
    let parent_id = session["parent_session_id"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(requester_session_id) = requester_session_id {
        if parent_id != Some(requester_session_id) {
            bail!(
                "Not authorized. You can only clear your child sessions.\nTarget session parent: {}",
                parent_id.unwrap_or("none")
            );
        }
    } else if parent_id.is_none() {
        bail!("Can only clear child sessions. Target session has no parent.");
    }
    Ok(())
}

fn format_session_line(session: &Value, show_working_dir: bool) -> String {
    let id = session["id"].as_str().unwrap_or("unknown");
    let name = session["friendly_name"]
        .as_str()
        .or_else(|| session["name"].as_str())
        .unwrap_or(id);
    let provider = session["provider"].as_str().unwrap_or("claude");
    let status = session["activity_state"]
        .as_str()
        .or_else(|| session["status"].as_str())
        .unwrap_or("unknown");
    let mut line = format!("{name} ({id}) | {provider} | {status}");
    if show_working_dir {
        if let Some(working_dir) = session["working_dir"].as_str() {
            line.push_str(" | ");
            line.push_str(working_dir);
        }
    }
    line
}

fn format_child_line(child: &Value) -> String {
    let id = child["id"].as_str().unwrap_or("unknown");
    let name = child["friendly_name"]
        .as_str()
        .or_else(|| child["name"].as_str())
        .unwrap_or(id);
    let provider = child["provider"].as_str().unwrap_or("claude");
    let status = child["completion_status"]
        .as_str()
        .or_else(|| child["activity_state"].as_str())
        .or_else(|| child["status"].as_str())
        .unwrap_or("unknown");
    format!("{name} ({id}) | {provider} | {status}")
}

impl ApiClient {
    fn parse(base_url: &str) -> Result<Self> {
        let (scheme, rest, default_port) = if let Some(rest) = base_url.strip_prefix("http://") {
            ("http", rest, 80)
        } else if let Some(rest) = base_url.strip_prefix("https://") {
            ("https", rest, 443)
        } else {
            bail!("only http(s):// API URLs are supported in this core slice");
        };
        let (authority, path_prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.trim().is_empty() {
            bail!("API URL is missing host");
        }
        let (host, port) = parse_authority(authority, default_port)?;
        Ok(Self {
            scheme: scheme.to_owned(),
            authority: authority.to_owned(),
            host,
            port,
            path_prefix: if path_prefix.is_empty() {
                String::new()
            } else {
                format!("/{path_prefix}")
            },
        })
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let response = self.request("GET", path, None)?;
        response.into_json()
    }

    fn post_json(&self, path: &str, body: Value) -> Result<Value> {
        let response = self.request("POST", path, Some(body))?;
        response.into_json()
    }

    fn put_json(&self, path: &str, body: Value) -> Result<Value> {
        let response = self.request("PUT", path, Some(body))?;
        response.into_json()
    }

    fn delete_json(&self, path: &str, body: Value) -> Result<Value> {
        let response = self.request("DELETE", path, Some(body))?;
        response.into_json()
    }

    fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<ApiResponse> {
        if self.scheme == "https" {
            return self.request_https(method, path, body);
        }
        let body_bytes = match body {
            Some(value) => serde_json::to_vec(&value)?,
            None => Vec::new(),
        };
        let full_path = format!("{}{}", self.path_prefix, path);
        let mut stream = TcpStream::connect((self.host.as_str(), self.port))
            .with_context(|| format!("failed to connect to {}", self.authority))?;
        let request = format!(
            "{method} {full_path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.authority,
            body_bytes.len()
        );
        stream.write_all(request.as_bytes())?;
        stream.write_all(&body_bytes)?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        parse_response(&raw)
    }

    fn request_https(&self, method: &str, path: &str, body: Option<Value>) -> Result<ApiResponse> {
        let body_bytes = match body {
            Some(value) => serde_json::to_vec(&value)?,
            None => Vec::new(),
        };
        let url = format!(
            "{}://{}{}{}",
            self.scheme, self.authority, self.path_prefix, path
        );
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = match method {
            "GET" => agent
                .get(&url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .call(),
            "POST" => agent
                .post(&url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .send(body_bytes.as_slice()),
            "PUT" => agent
                .put(&url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .send(body_bytes.as_slice()),
            "DELETE" => agent
                .delete(&url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .force_send_body()
                .send(body_bytes.as_slice()),
            _ => bail!("unsupported HTTP method {method}"),
        }
        .with_context(|| format!("failed to request {url}"))?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string()?;
        Ok(ApiResponse { status, body })
    }
}

impl ApiResponse {
    fn into_json(self) -> Result<Value> {
        if !(200..300).contains(&self.status) {
            return Err(self.into_status_error());
        }
        serde_json::from_str(&self.body)
            .with_context(|| format!("response body was not JSON: {}", self.body))
    }

    fn into_status_error(self) -> anyhow::Error {
        anyhow!("HTTP {}: {}", self.status, self.body)
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn parse_authority(authority: &str, default_port: u16) -> Result<(String, u16)> {
    let default_port = default_port.to_string();
    let (host, port) = authority
        .rsplit_once(':')
        .unwrap_or((authority, default_port.as_str()));
    let port = port
        .parse::<u16>()
        .with_context(|| format!("invalid API URL port: {port}"))?;
    Ok((host.to_owned(), port))
}

fn parse_response(raw: &[u8]) -> Result<ApiResponse> {
    let response = String::from_utf8_lossy(raw);
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let status_line = headers
        .lines()
        .next()
        .ok_or_else(|| anyhow!("missing HTTP status line"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("missing HTTP status code"))?
        .parse::<u16>()?;
    let is_chunked = headers.lines().any(|line| {
        line.to_ascii_lowercase()
            .starts_with("transfer-encoding: chunked")
    });
    let body = if is_chunked {
        decode_chunked_body(body)?
    } else {
        body.to_owned()
    };
    Ok(ApiResponse { status, body })
}

fn decode_chunked_body(body: &str) -> Result<String> {
    let mut remaining = body;
    let mut decoded = String::new();
    loop {
        let Some((size_hex, rest)) = remaining.split_once("\r\n") else {
            bail!("malformed chunked response");
        };
        let size = usize::from_str_radix(size_hex.trim(), 16)?;
        if size == 0 {
            return Ok(decoded);
        }
        if rest.len() < size + 2 {
            bail!("truncated chunked response");
        }
        decoded.push_str(&rest[..size]);
        remaining = &rest[size + 2..];
    }
}

fn current_session_id() -> Result<String> {
    optional_current_session_id()
        .ok_or_else(|| anyhow!("SESSION_MANAGER_ID is required to report status"))
}

fn optional_current_session_id() -> Option<String> {
    env::var("SESSION_MANAGER_ID")
        .or_else(|_| env::var("CLAUDE_SESSION_MANAGER_ID"))
        .map(|value| value.trim().to_owned())
        .ok()
        .filter(|value| !value.is_empty())
}

fn resolve_api_url(explicit_api_url: Option<String>) -> Result<String> {
    if let Some(api_url) = explicit_api_url {
        return coerce_api_url(&api_url).ok_or_else(|| {
            anyhow!("Invalid Session Manager API URL: explicit api_url must be http(s)")
        });
    }

    if let Ok(api_url) = env::var("SM_API_URL") {
        return coerce_api_url(&api_url)
            .ok_or_else(|| anyhow!("Invalid Session Manager API URL: SM_API_URL must be http(s)"));
    }

    if let Some(api_url) = read_client_config_api_url()? {
        return Ok(api_url);
    }

    Ok(DEFAULT_API_URL.to_owned())
}

fn coerce_api_url(value: &str) -> Option<String> {
    let api_url = value.trim().trim_end_matches('/').to_owned();
    if api_url.starts_with("http://") || api_url.starts_with("https://") {
        Some(api_url)
    } else {
        None
    }
}

fn read_client_config_api_url() -> Result<Option<String>> {
    let path = client_config_path();
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Invalid Session Manager client config {}", path.display())
            })
        }
    };
    let payload: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("Invalid Session Manager client config {}", path.display()))?;
    let mapping = payload.as_mapping().ok_or_else(|| {
        anyhow!(
            "Invalid Session Manager client config {}: expected a YAML mapping",
            path.display()
        )
    })?;

    if let Some(value) = yaml_mapping_get(mapping, "api_url") {
        return coerce_yaml_api_url(value, "api_url");
    }

    if let Some(client_payload) = yaml_mapping_get(mapping, "client") {
        let client_mapping = client_payload.as_mapping().ok_or_else(|| {
            anyhow!("Invalid Session Manager client config: client must be a mapping")
        })?;
        if let Some(value) = yaml_mapping_get(client_mapping, "api_url") {
            return coerce_yaml_api_url(value, "client.api_url");
        }
    }

    Ok(None)
}

fn coerce_yaml_api_url(value: &serde_yaml::Value, label: &str) -> Result<Option<String>> {
    let Some(raw) = value.as_str() else {
        return Err(anyhow!(
            "Invalid Session Manager client config: {label} must be http(s)"
        ));
    };
    coerce_api_url(raw)
        .map(Some)
        .ok_or_else(|| anyhow!("Invalid Session Manager client config: {label} must be http(s)"))
}

fn yaml_mapping_get<'a>(
    mapping: &'a serde_yaml::Mapping,
    key: &str,
) -> Option<&'a serde_yaml::Value> {
    mapping.get(serde_yaml::Value::String(key.to_owned()))
}

fn client_config_path() -> PathBuf {
    if let Ok(path) = env::var(CLIENT_CONFIG_ENV) {
        return expand_home_path(&path);
    }
    if let Ok(xdg_config_home) = env::var("XDG_CONFIG_HOME") {
        return expand_home_path(&xdg_config_home).join(CLIENT_CONFIG_SUBPATH);
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join(CLIENT_CONFIG_SUBPATH)
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    Path::new(path).to_path_buf()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn retire_removed_surface_if_requested() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let command = command_tokens_after_globals(&args);
    let retired = match command.as_slice() {
        ["kill", ..] => Some("removed: use sm retire instead of sm kill"),
        ["what", ..] => Some("removed: use sm tail --raw or sm send for explicit status"),
        ["dispatch", ..] => Some("removed: dispatch is not part of the Rust cutover scope"),
        ["remind", ..] => Some("removed: reminders are not part of the Rust cutover scope"),
        ["watch-job", ..] => Some("removed: watch-job is not part of the Rust cutover scope"),
        ["telegram", ..] | ["tg", ..] => {
            Some("removed: Telegram control is not part of the Rust cutover scope")
        }
        ["codex-legacy", ..] | ["codex-server", ..] => {
            Some("removed: legacy Codex surfaces are not part of the Rust cutover scope")
        }
        ["queue", "ci-run", ..] | ["queue", "ci-status", ..] | ["queue", "ci-history", ..] => {
            Some("removed: queue policy CI commands are not part of the Rust cutover scope")
        }
        _ => None,
    };
    if let Some(message) = retired {
        eprintln!("{message}");
        process::exit(2);
    }
}

fn command_tokens_after_globals(args: &[String]) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--api-url" {
            index += 2;
            continue;
        }
        if arg.starts_with("--api-url=") {
            index += 1;
            continue;
        }
        tokens.push(arg);
        index += 1;
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolve_api_url_uses_existing_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SM_API_URL", CLIENT_CONFIG_ENV, "XDG_CONFIG_HOME"]);

        assert_eq!(resolve_api_url(None).unwrap(), DEFAULT_API_URL);
    }

    #[test]
    fn resolve_api_url_prefers_explicit_then_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SM_API_URL", CLIENT_CONFIG_ENV, "XDG_CONFIG_HOME"]);
        env::set_var("SM_API_URL", "http://127.0.0.1:9999");

        assert_eq!(
            resolve_api_url(Some("http://127.0.0.1:8888/".to_owned())).unwrap(),
            "http://127.0.0.1:8888"
        );
        assert_eq!(resolve_api_url(None).unwrap(), "http://127.0.0.1:9999");
    }

    #[test]
    fn resolve_api_url_reads_top_level_client_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SM_API_URL", CLIENT_CONFIG_ENV, "XDG_CONFIG_HOME"]);
        let config = write_temp_config("api_url: \"http://127.0.0.1:7777/\"\n");
        env::set_var(CLIENT_CONFIG_ENV, &config);

        assert_eq!(resolve_api_url(None).unwrap(), "http://127.0.0.1:7777");
    }

    #[test]
    fn resolve_api_url_reads_nested_client_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SM_API_URL", CLIENT_CONFIG_ENV, "XDG_CONFIG_HOME"]);
        let config = write_temp_config("client:\n  api_url: \"http://127.0.0.1:6666\"\n");
        env::set_var(CLIENT_CONFIG_ENV, &config);

        assert_eq!(resolve_api_url(None).unwrap(), "http://127.0.0.1:6666");
    }

    #[test]
    fn resolve_api_url_preserves_https_client_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SM_API_URL", CLIENT_CONFIG_ENV, "XDG_CONFIG_HOME"]);
        let config = write_temp_config("client:\n  api_url: \"https://sm.example.test/api/\"\n");
        env::set_var(CLIENT_CONFIG_ENV, &config);

        assert_eq!(
            resolve_api_url(None).unwrap(),
            "https://sm.example.test/api"
        );
    }

    #[test]
    fn api_client_parse_supports_http_and_https_defaults() {
        let http = ApiClient::parse("http://127.0.0.1/api").unwrap();
        assert_eq!(http.scheme, "http");
        assert_eq!(http.host, "127.0.0.1");
        assert_eq!(http.port, 80);
        assert_eq!(http.path_prefix, "/api");

        let https = ApiClient::parse("https://sm.example.test/client").unwrap();
        assert_eq!(https.scheme, "https");
        assert_eq!(https.host, "sm.example.test");
        assert_eq!(https.port, 443);
        assert_eq!(https.path_prefix, "/client");
    }

    fn write_temp_config(content: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "sm-rust-client-config-{}-{nonce}.yaml",
            std::process::id()
        ));
        fs::write(&path, content).unwrap();
        path
    }

    struct EnvRestore {
        values: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvRestore {
        fn new(keys: &[&'static str]) -> Self {
            let values = keys
                .iter()
                .map(|key| {
                    let value = env::var_os(key);
                    env::remove_var(key);
                    (*key, value)
                })
                .collect();
            Self { values }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in &self.values {
                match value {
                    Some(value) => env::set_var(key, value),
                    None => env::remove_var(key),
                }
            }
        }
    }
}
