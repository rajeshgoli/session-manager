use std::{
    env, fs,
    io::{self, IsTerminal, Read, Write},
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
    #[command(name = "subagent-start")]
    SubagentStart(EmptyArgs),
    #[command(name = "subagent-stop")]
    SubagentStop(EmptyArgs),
    Subagents(SessionIdArgs),
    Claude(ProviderLaunchArgs),
    Codex(ProviderLaunchArgs),
    #[command(name = "codex-app")]
    CodexApp(ProviderLaunchArgs),
    #[command(name = "codex-fork", alias = "codex_fork")]
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
    #[arg(long)]
    node: Option<String>,
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
    message: Option<String>,
    #[arg(long)]
    subject: Option<String>,
    #[arg(long)]
    body: Option<String>,
    #[arg(long)]
    text: Option<String>,
    #[arg(long)]
    html: Option<String>,
    #[arg(long)]
    cc: Option<String>,
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
    #[arg(long)]
    node: Option<String>,
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
        Command::New(args) => launch_provider_session(
            &client,
            launch_provider_for_alias("new")?,
            args.working_dir,
            args.node,
        )?,
        Command::Claude(args) => launch_provider_session(
            &client,
            launch_provider_for_alias("claude")?,
            args.working_dir,
            args.node,
        )?,
        Command::Codex(args) => launch_provider_session(
            &client,
            launch_provider_for_alias("codex")?,
            args.working_dir,
            args.node,
        )?,
        Command::CodexFork(args) => launch_provider_session(
            &client,
            launch_provider_for_alias("codex-fork")?,
            args.working_dir,
            args.node,
        )?,
        Command::Codex2(args) => launch_provider_session(
            &client,
            launch_provider_for_alias("codex-2")?,
            args.working_dir,
            args.node,
        )?,
        Command::CodexApp(args) => launch_provider_session(
            &client,
            launch_provider_for_alias("codex-app")?,
            args.working_dir,
            args.node,
        )?,
        Command::Send(args) => {
            let text = args.text.join(" ");
            if text.trim().is_empty() {
                bail!("send text is required");
            }
            let delivery_mode = if args.urgent { "urgent" } else { "sequential" };
            let targets = split_send_targets(&args.session_id);
            let mut payload = send_input_payload(text, delivery_mode, args.wait);
            if targets.len() > 1 {
                payload["recipients"] = json!(targets);
                let payload = client.post_json("/sessions/input-batch", payload)?;
                print_batch_send_result(&payload)?;
                if payload["failure_count"].as_u64().unwrap_or(0) > 0 {
                    bail!("one or more sends failed");
                }
            } else {
                let target = targets
                    .first()
                    .map(String::as_str)
                    .unwrap_or(args.session_id.as_str());
                let payload = client.post_json(&format!("/sessions/{target}/input"), payload)?;
                println!(
                    "{}",
                    if payload["delivered"].as_bool().unwrap_or(false) {
                        "delivered"
                    } else {
                        "not delivered"
                    }
                );
            }
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
        Command::Email(args) => run_email(&client, args)?,
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
        Command::SubagentStart(_) => run_subagent_start(&client)?,
        Command::SubagentStop(_) => run_subagent_stop(&client)?,
        Command::Subagents(args) => print_subagents(&client, &args.session_id)?,
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

fn run_email(client: &ApiClient, args: EmailArgs) -> Result<()> {
    let requester_session_id = current_session_id()?;
    let recipient_raw = required_positional(args.recipient, "recipient")?;
    let recipients = split_email_targets(&recipient_raw);
    if recipients.is_empty() {
        bail!("at least one recipient is required");
    }
    let cc = split_email_targets(args.cc.as_deref().unwrap_or(""));
    let body = email_body_from_args(args.message, args.body, args.text, args.html)?;
    let subject = args.subject;

    let mut human_match = None;
    for target in recipients.iter().chain(cc.iter()) {
        let human_response = client.request(
            "GET",
            &format!("/humans/{}", encode_path_segment(target)),
            None,
        )?;
        if (200..300).contains(&human_response.status) {
            human_match = Some((target.clone(), human_response.into_json()?));
            break;
        }
        if human_response.status != 404 {
            return Err(human_response.into_status_error());
        }
    }

    if let Some((target, human)) = human_match {
        if recipients.len() != 1 || !cc.is_empty() {
            bail!("sm email to human recipients supports exactly one recipient and no --cc");
        }
        let Some(text) = body.text.clone() else {
            bail!("sm email to human recipients supports plain text or markdown bodies only");
        };
        if body.html.is_some() {
            bail!("sm email to human recipients supports plain text or markdown bodies only");
        }
        let canonical = human["recipient"].as_str().unwrap_or(&target);
        let payload = client.post_json(
            &format!("/humans/{}/email", encode_path_segment(canonical)),
            json!({
                "requester_session_id": requester_session_id,
                "text": text,
                "subject": subject,
                "body_markdown": body.markdown,
            }),
        )?;
        println!(
            "Email sent to {}",
            payload["recipient"].as_str().unwrap_or(canonical)
        );
        return Ok(());
    }
    let request_payload =
        registered_email_payload(requester_session_id, recipients, cc, subject, body)?;
    let payload = client.post_json("/email/send", request_payload)?;
    let to_summary = payload["to"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["username"].as_str().or_else(|| item["email"].as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "recipient".to_owned());
    println!("Email sent to {to_summary}");
    Ok(())
}

fn split_email_targets(raw_value: &str) -> Vec<String> {
    let mut identifiers = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for part in raw_value.split(',') {
        let identifier = part.trim();
        if identifier.is_empty() || !seen.insert(identifier.to_owned()) {
            continue;
        }
        identifiers.push(identifier.to_owned());
    }
    identifiers
}

fn registered_email_payload(
    requester_session_id: String,
    recipients: Vec<String>,
    cc: Vec<String>,
    subject: Option<String>,
    body: EmailBody,
) -> Result<Value> {
    if subject
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        bail!("--subject is required for non-human registered email");
    }
    Ok(json!({
        "requester_session_id": requester_session_id,
        "recipients": recipients,
        "cc": cc,
        "subject": subject,
        "body_text": body.text,
        "body_html": body.html,
        "body_markdown": body.markdown,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EmailBody {
    text: Option<String>,
    html: Option<String>,
    markdown: bool,
}

fn email_body_from_args(
    message: Option<String>,
    body: Option<String>,
    text_file: Option<String>,
    html_file: Option<String>,
) -> Result<EmailBody> {
    if message.is_some() && body.is_some() {
        bail!("use either positional message or --body, not both");
    }

    let source_count = usize::from(message.is_some())
        + usize::from(body.is_some())
        + usize::from(text_file.is_some())
        + usize::from(html_file.is_some());
    if source_count > 1 {
        bail!("Provide exactly one of positional message, --body, --text, --html, or stdin");
    }

    if let Some(body) = body
        .or(message)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return Ok(EmailBody {
            text: Some(body),
            html: None,
            markdown: false,
        });
    }
    if let Some(text_file) = text_file {
        let path = Path::new(&text_file);
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read email text file {}", path.display()))?;
        let markdown = path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| matches!(value.to_ascii_lowercase().as_str(), "md" | "markdown"))
            .unwrap_or(false);
        return Ok(EmailBody {
            text: Some(text),
            html: None,
            markdown,
        });
    }
    if let Some(html_file) = html_file {
        let path = Path::new(&html_file);
        let html = fs::read_to_string(path)
            .with_context(|| format!("failed to read email HTML file {}", path.display()))?;
        return Ok(EmailBody {
            text: None,
            html: Some(html),
            markdown: false,
        });
    }
    if !io::stdin().is_terminal() {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        let input = input.trim().to_owned();
        if !input.is_empty() {
            return Ok(EmailBody {
                text: Some(input),
                html: None,
                markdown: true,
            });
        }
    }
    bail!("Email body is required");
}

fn print_output(client: &ApiClient, session_id: &str, lines: usize) -> Result<()> {
    let payload = client.get_json(&format!("/sessions/{session_id}/output?lines={lines}"))?;
    if let Some(output) = payload["output"].as_str() {
        print!("{output}");
    }
    Ok(())
}

fn print_batch_send_result(payload: &Value) -> Result<()> {
    let Some(results) = payload["results"].as_array() else {
        bail!("batch send response missing results");
    };
    for item in results {
        let identifier = item["identifier"].as_str().unwrap_or("<unknown>");
        let status = item["status"].as_str().unwrap_or("failed");
        let session_id = item["session_id"].as_str().unwrap_or(identifier);
        let target_name = item["target_name"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(session_id);
        match status {
            "delivered" => println!("Input sent to {target_name} ({session_id})"),
            "queued" => println!("Input queued for {target_name} ({session_id})"),
            _ => {
                let detail = item["detail"].as_str().unwrap_or("Failed to send input");
                eprintln!("Error: {identifier}: {detail}");
            }
        }
    }
    Ok(())
}

fn run_subagent_start(client: &ApiClient) -> Result<()> {
    let Some(session_id) = optional_current_session_id() else {
        let mut ignored = String::new();
        io::stdin().read_to_string(&mut ignored)?;
        return Ok(());
    };
    let payload = read_json_stdin()?;
    let agent_id = json_value_string(&payload, "agent_id")
        .ok_or_else(|| anyhow!("Missing agent_id in hook payload"))?;
    let agent_type = json_value_string(&payload, "agent_type")
        .or_else(|| json_value_string(&payload, "subagent_type"))
        .unwrap_or_else(|| "unknown".to_owned());
    let transcript_path = json_value_string(&payload, "agent_transcript_path");
    client.post_json(
        &format!("/sessions/{}/subagents", encode_path_segment(&session_id)),
        json!({
            "agent_id": agent_id,
            "agent_type": agent_type,
            "transcript_path": transcript_path
        }),
    )?;
    Ok(())
}

fn run_subagent_stop(client: &ApiClient) -> Result<()> {
    let Some(session_id) = optional_current_session_id() else {
        let mut ignored = String::new();
        io::stdin().read_to_string(&mut ignored)?;
        return Ok(());
    };
    let payload = read_json_stdin()?;
    let agent_id = json_value_string(&payload, "agent_id")
        .ok_or_else(|| anyhow!("Missing agent_id in hook payload"))?;
    let transcript_path = json_value_string(&payload, "agent_transcript_path");
    let summary = subagent_stop_summary(&payload);
    client.post_json(
        &format!(
            "/sessions/{}/subagents/{}/stop",
            encode_path_segment(&session_id),
            encode_path_segment(&agent_id)
        ),
        json!({
            "summary": summary,
            "transcript_path": transcript_path
        }),
    )?;
    Ok(())
}

fn print_subagents(client: &ApiClient, session_id: &str) -> Result<()> {
    let session = client.get_json(&format!("/sessions/{}", encode_path_segment(session_id)))?;
    let payload = client.get_json(&format!(
        "/sessions/{}/subagents",
        encode_path_segment(session_id)
    ))?;
    let name = session["friendly_name"]
        .as_str()
        .or_else(|| session["name"].as_str())
        .unwrap_or(session_id);
    let subagents = payload["subagents"].as_array().cloned().unwrap_or_default();
    if subagents.is_empty() {
        println!("{name} has no subagents");
        return Ok(());
    }
    println!("{name} ({session_id}) subagents:");
    for subagent in subagents {
        let agent_id = subagent["agent_id"].as_str().unwrap_or("unknown");
        let short_id = agent_id.chars().take(6).collect::<String>();
        let agent_type = subagent["agent_type"].as_str().unwrap_or("unknown");
        let status = subagent["status"].as_str().unwrap_or("unknown");
        let started_at = subagent["started_at"].as_str().unwrap_or("");
        println!("  {agent_type} ({short_id}) | {status} | {started_at}");
        if let Some(summary) = subagent["summary"]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            println!("     {summary}");
        }
    }
    Ok(())
}

fn read_json_stdin() -> Result<Value> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    serde_json::from_str(&input).with_context(|| "Failed to parse hook payload")
}

fn json_value_string(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn subagent_stop_summary(payload: &Value) -> Option<String> {
    json_value_string(payload, "last_assistant_message")
        .or_else(|| json_value_string(payload, "summary"))
}

fn split_send_targets(raw_value: &str) -> Vec<String> {
    let mut identifiers = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for part in raw_value.split(',') {
        let identifier = part.trim();
        if identifier.is_empty() || !seen.insert(identifier.to_owned()) {
            continue;
        }
        identifiers.push(identifier.to_owned());
    }
    identifiers
}

fn send_input_payload(text: String, delivery_mode: &str, wait: Option<u64>) -> Value {
    let mut payload = json!({
        "text": text,
        "delivery_mode": delivery_mode,
        "notify_after_seconds": wait,
        "from_sm_send": true
    });
    if let Some(sender_session_id) = optional_current_session_id() {
        payload["sender_session_id"] = json!(sender_session_id);
    }
    payload
}

fn launch_provider_for_alias(alias: &str) -> Result<&'static str> {
    match alias {
        "new" | "claude" => Ok("claude"),
        "codex" | "codex-fork" | "codex_fork" | "codex-2" => Ok("codex-fork"),
        "codex-app" => Ok("codex-app"),
        _ => bail!("unsupported launch alias {alias}"),
    }
}

fn launch_provider_session(
    client: &ApiClient,
    provider: &str,
    working_dir: Option<String>,
    node: Option<String>,
) -> Result<()> {
    let working_dir = resolve_launch_working_dir(working_dir, node.as_deref())?;
    if let Some(node) = node
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        println!("Creating session on node {node} in {working_dir}...");
    } else {
        println!("Creating session in {working_dir}...");
    }
    let parent_session_id = optional_current_session_id();
    let payload = create_launch_session_payload(
        provider,
        &working_dir,
        parent_session_id.as_deref(),
        node.as_deref(),
    );
    let response = client.post_json("/sessions", payload)?;
    if let Some(error) = response["error"]
        .as_str()
        .or_else(|| response["detail"].as_str())
    {
        bail!("{error}");
    }
    let session_id = response["id"]
        .as_str()
        .or_else(|| response["session_id"].as_str())
        .ok_or_else(|| anyhow!("create response missing id"))?;
    let response_provider = response["provider"].as_str().unwrap_or(provider);
    if response_provider == "codex-app" {
        println!("Codex app session created: {session_id}");
        println!("No tmux attach for Codex app sessions.");
        return Ok(());
    }
    println!("Session created: {session_id}");
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        println!(
            "Automatic attach skipped: current shell is not interactive. Run `sm attach {session_id}` from an interactive terminal."
        );
        return Ok(());
    }
    let tmux_session = response["tmux_session"].as_str().unwrap_or(session_id);
    println!("Attaching to {tmux_session}...");
    attach_session(client, session_id)
}

fn create_launch_session_payload(
    provider: &str,
    working_dir: &str,
    parent_session_id: Option<&str>,
    node: Option<&str>,
) -> Value {
    json!({
        "provider": provider,
        "working_dir": working_dir,
        "parent_session_id": parent_session_id,
        "node": node
    })
}

fn resolve_launch_working_dir(working_dir: Option<String>, node: Option<&str>) -> Result<String> {
    let raw = match working_dir {
        Some(value) if !value.trim().is_empty() => value.trim().to_owned(),
        _ => env::current_dir()
            .with_context(|| "failed to resolve current directory")?
            .display()
            .to_string(),
    };
    if node.map(is_primary_node_alias) == Some(false) {
        return Ok(raw);
    }
    let path = expand_home_path(&raw);
    if !path.exists() {
        bail!("Directory does not exist: {raw}");
    }
    if !path.is_dir() {
        bail!("Not a directory: {raw}");
    }
    Ok(path
        .canonicalize()
        .with_context(|| format!("Invalid path: {}", path.display()))?
        .display()
        .to_string())
}

fn is_primary_node_alias(node: &str) -> bool {
    matches!(
        node.trim(),
        "" | "primary" | "local" | "localhost" | "studio"
    )
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

    #[test]
    fn send_input_payload_includes_sm_send_sender_metadata() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SESSION_MANAGER_ID", "CLAUDE_SESSION_MANAGER_ID"]);
        env::set_var("SESSION_MANAGER_ID", "sender001");

        let payload = send_input_payload("hello".to_owned(), "sequential", Some(7));

        assert_eq!(payload["text"], "hello");
        assert_eq!(payload["delivery_mode"], "sequential");
        assert_eq!(payload["notify_after_seconds"], 7);
        assert_eq!(payload["from_sm_send"], true);
        assert_eq!(payload["sender_session_id"], "sender001");
    }

    #[test]
    fn email_cli_accepts_positional_message_and_cc() {
        let cli = Cli::try_parse_from([
            "sm",
            "email",
            "alice,bob",
            "hello from rust",
            "--subject",
            "Status",
            "--cc",
            "carol,dave",
        ])
        .unwrap();

        let Command::Email(args) = cli.command else {
            panic!("expected email command");
        };
        assert_eq!(args.recipient.as_deref(), Some("alice,bob"));
        assert_eq!(args.message.as_deref(), Some("hello from rust"));
        assert_eq!(args.subject.as_deref(), Some("Status"));
        assert_eq!(args.cc.as_deref(), Some("carol,dave"));
    }

    #[test]
    fn email_cli_accepts_file_backed_body_flags() {
        let text_cli = Cli::try_parse_from([
            "sm",
            "email",
            "alice",
            "--subject",
            "Status",
            "--text",
            "body.md",
        ])
        .unwrap();
        let Command::Email(text_args) = text_cli.command else {
            panic!("expected email command");
        };
        assert_eq!(text_args.text.as_deref(), Some("body.md"));

        let html_cli = Cli::try_parse_from([
            "sm",
            "email",
            "alice",
            "--subject",
            "Status",
            "--html",
            "body.html",
        ])
        .unwrap();
        let Command::Email(html_args) = html_cli.command else {
            panic!("expected email command");
        };
        assert_eq!(html_args.html.as_deref(), Some("body.html"));
    }

    #[test]
    fn split_email_targets_dedupes_comma_lists() {
        assert_eq!(
            split_email_targets(" alice, bob ,,alice,carol "),
            vec!["alice", "bob", "carol"]
        );
    }

    #[test]
    fn registered_email_payload_preserves_recipient_and_cc_lists() {
        let payload = registered_email_payload(
            "sender001".to_owned(),
            vec!["alice".to_owned(), "bob".to_owned()],
            vec!["carol".to_owned()],
            Some("Status".to_owned()),
            EmailBody {
                text: Some("hello".to_owned()),
                html: None,
                markdown: false,
            },
        )
        .unwrap();

        assert_eq!(payload["requester_session_id"], "sender001");
        assert_eq!(payload["recipients"], json!(["alice", "bob"]));
        assert_eq!(payload["cc"], json!(["carol"]));
        assert_eq!(payload["subject"], "Status");
        assert_eq!(payload["body_text"], "hello");
        assert_eq!(payload["body_html"], Value::Null);
        assert_eq!(payload["body_markdown"], false);
    }

    #[test]
    fn registered_email_payload_preserves_html_body() {
        let payload = registered_email_payload(
            "sender001".to_owned(),
            vec!["alice".to_owned()],
            Vec::new(),
            Some("Status".to_owned()),
            EmailBody {
                text: None,
                html: Some("<p>hello</p>".to_owned()),
                markdown: false,
            },
        )
        .unwrap();

        assert_eq!(payload["body_text"], Value::Null);
        assert_eq!(payload["body_html"], "<p>hello</p>");
    }

    #[test]
    fn email_body_rejects_positional_message_with_body_flag() {
        let error = email_body_from_args(
            Some("positional".to_owned()),
            Some("flag".to_owned()),
            None,
            None,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("use either positional message or --body"));
    }

    #[test]
    fn email_body_loads_text_and_html_files() {
        let markdown_path = write_temp_file("sm-rust-email-body", ".md", "# Summary\n\n- one\n");
        let markdown_body =
            email_body_from_args(None, None, Some(markdown_path.display().to_string()), None)
                .unwrap();
        assert_eq!(markdown_body.text.as_deref(), Some("# Summary\n\n- one\n"));
        assert_eq!(markdown_body.html, None);
        assert!(markdown_body.markdown);

        let html_path = write_temp_file("sm-rust-email-body", ".html", "<p>Summary</p>\n");
        let html_body =
            email_body_from_args(None, None, None, Some(html_path.display().to_string())).unwrap();
        assert_eq!(html_body.text, None);
        assert_eq!(html_body.html.as_deref(), Some("<p>Summary</p>\n"));
        assert!(!html_body.markdown);
    }

    #[test]
    fn subagent_stop_summary_prefers_current_hook_field() {
        let payload = json!({
            "last_assistant_message": "done from hook",
            "summary": "legacy summary"
        });

        assert_eq!(
            subagent_stop_summary(&payload).as_deref(),
            Some("done from hook")
        );

        let legacy_payload = json!({ "summary": "legacy summary" });
        assert_eq!(
            subagent_stop_summary(&legacy_payload).as_deref(),
            Some("legacy summary")
        );
    }

    #[test]
    fn launch_provider_aliases_match_retained_surface() {
        assert_eq!(launch_provider_for_alias("new").unwrap(), "claude");
        assert_eq!(launch_provider_for_alias("claude").unwrap(), "claude");
        assert_eq!(launch_provider_for_alias("codex").unwrap(), "codex-fork");
        assert_eq!(
            launch_provider_for_alias("codex-fork").unwrap(),
            "codex-fork"
        );
        assert_eq!(
            launch_provider_for_alias("codex_fork").unwrap(),
            "codex-fork"
        );
        assert_eq!(launch_provider_for_alias("codex-2").unwrap(), "codex-fork");
        assert_eq!(launch_provider_for_alias("codex-app").unwrap(), "codex-app");
        assert!(launch_provider_for_alias("codex-legacy").is_err());
    }

    #[test]
    fn launch_create_payload_preserves_parent_and_node() {
        let payload =
            create_launch_session_payload("claude", "/repo", Some("parent001"), Some("worker"));

        assert_eq!(payload["provider"], "claude");
        assert_eq!(payload["working_dir"], "/repo");
        assert_eq!(payload["parent_session_id"], "parent001");
        assert_eq!(payload["node"], "worker");

        let top_level = create_launch_session_payload("codex-fork", "/repo", None, None);
        assert_eq!(top_level["provider"], "codex-fork");
        assert_eq!(top_level["parent_session_id"], Value::Null);
        assert_eq!(top_level["node"], Value::Null);
    }

    #[test]
    fn launch_working_dir_validates_local_but_preserves_remote_paths() {
        let local_dir = env::temp_dir().join(format!(
            "sm-rust-launch-dir-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&local_dir).unwrap();

        let resolved =
            resolve_launch_working_dir(Some(local_dir.display().to_string()), None).unwrap();
        assert_eq!(PathBuf::from(resolved), local_dir.canonicalize().unwrap());

        let missing_local = local_dir.join("missing");
        assert!(
            resolve_launch_working_dir(Some(missing_local.display().to_string()), None).is_err()
        );

        let remote_path = "/remote/node/project";
        assert_eq!(
            resolve_launch_working_dir(Some(remote_path.to_owned()), Some("worker")).unwrap(),
            remote_path
        );
    }

    fn write_temp_config(content: &str) -> PathBuf {
        write_temp_file("sm-rust-client-config", ".yaml", content)
    }

    fn write_temp_file(prefix: &str, suffix: &str, content: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("{prefix}-{}-{nonce}{suffix}", std::process::id()));
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
