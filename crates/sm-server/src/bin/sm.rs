use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, IsTerminal, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process, thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Map, Value};
use sm_server::{config::AppConfig, mobile_devices};

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
    Restore(RestoreArgs),
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
    #[command(name = "enroll-device")]
    EnrollDevice(EnrollDeviceArgs),
    #[command(name = "list-devices")]
    ListDevices(ListDevicesArgs),
    #[command(name = "remove-device")]
    RemoveDevice(RemoveDeviceArgs),
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
struct RestoreArgs {
    session_id: String,
    #[arg(long)]
    node: Option<String>,
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
    #[arg(long = "type", value_parser = ["tests", "perf", "background"], default_value = "tests")]
    job_type: String,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    cwd: Option<String>,
    #[arg(long)]
    timeout: Option<String>,
    #[arg(long = "env")]
    env_pairs: Vec<String>,
    #[arg(long)]
    script_file: Option<String>,
    #[arg(long)]
    notify: Option<String>,
    #[arg(trailing_var_arg = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct QueueListArgs {
    #[arg(long)]
    notify: Option<String>,
    #[arg(long)]
    all: bool,
    #[arg(long = "type", value_parser = ["tests", "perf", "background"])]
    job_type: Option<String>,
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
struct EnrollDeviceArgs {
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,
    #[arg(long = "user-id")]
    user_id: Option<String>,
    #[arg(long, default_value_t = 15)]
    expires_in_minutes: u64,
    #[arg(long, default_value = "0.0.0.0:19192")]
    listen: SocketAddr,
    #[arg(long = "url-base")]
    url_base: Option<String>,
    #[arg(long = "device-ca-cert")]
    device_ca_cert: Option<PathBuf>,
    #[arg(long = "device-ca-key")]
    device_ca_key: Option<PathBuf>,
    #[arg(long)]
    no_qr: bool,
}

#[derive(Args)]
struct ListDevicesArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct RemoveDeviceArgs {
    device_id: String,
    #[arg(long = "user-id")]
    user_id: Option<String>,
}

#[derive(Args)]
struct ReviewArgs {
    session: Option<String>,
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
    name: Option<String>,
    #[arg(long, value_name = "SECONDS")]
    wait: Option<u64>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    working_dir: Option<String>,
    #[arg(long)]
    steer: Option<String>,
    #[arg(long)]
    pr: Option<u64>,
    #[arg(long)]
    repo: Option<String>,
}

#[derive(Debug)]
struct ReviewModeSelection {
    mode: &'static str,
    base_branch: Option<String>,
    commit_sha: Option<String>,
    custom_prompt: Option<String>,
}

#[derive(Args)]
struct RequestCodexReviewArgs {
    #[arg(value_name = "PR_NUMBER")]
    action_or_pr: Option<String>,
    #[arg(long, global = true)]
    notify: Option<String>,
    #[arg(long, global = true)]
    repo: Option<String>,
    #[arg(long, global = true)]
    steer: Option<String>,
    #[arg(long, global = true)]
    all: bool,
    #[arg(long, global = true)]
    inactive: bool,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long = "pr", global = true)]
    pr_number: Option<i64>,
    #[arg(long = "poll-interval", global = true, default_value_t = 30)]
    poll_interval_seconds: i64,
    #[arg(long = "retry-interval", global = true, default_value_t = 600)]
    retry_interval_seconds: i64,
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
    let command = cli.command;
    let command = match command {
        Command::EnrollDevice(args) => return run_enroll_device(args),
        command => command,
    };
    let api_url = resolve_api_url(cli.api_url)?;
    let client = ApiClient::parse(&api_url)?;

    match command {
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
            restore_session(&client, args)?;
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
        Command::ListDevices(args) => run_list_devices(&client, args)?,
        Command::RemoveDevice(args) => run_remove_device(&client, args)?,
        Command::Wait(args) => wait_for_session(&client, &args.session_id, args.seconds)?,
        Command::SubagentStart(_) => run_subagent_start(&client)?,
        Command::SubagentStop(_) => run_subagent_stop(&client)?,
        Command::Subagents(args) => print_subagents(&client, &args.session_id)?,
        Command::Queue(args) => run_queue(&client, args)?,
        Command::Review(args) => run_review(&client, args)?,
        Command::RequestCodexReview(args) => run_request_codex_review(&client, args)?,
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

fn run_list_devices(client: &ApiClient, args: ListDevicesArgs) -> Result<()> {
    let payload = client.get_json("/client/mobile-terminal/devices")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    print_mobile_devices(&payload)
}

fn run_enroll_device(args: EnrollDeviceArgs) -> Result<()> {
    let user_id = resolve_enroll_device_user_id(&args)?;
    mobile_devices::run_enroll_device(mobile_devices::EnrollDeviceOptions {
        config_path: args.config,
        user_id,
        expires_in_minutes: args.expires_in_minutes,
        listen: args.listen,
        advertised_base_url: args.url_base,
        device_ca_cert: args.device_ca_cert,
        device_ca_key: args.device_ca_key,
        no_qr: args.no_qr,
    })
}

fn resolve_enroll_device_user_id(args: &EnrollDeviceArgs) -> Result<String> {
    if let Some(user_id) = args
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(user_id.to_owned());
    }
    let config = AppConfig::load_from_path(&args.config)?;
    let mut allowed = config
        .mobile_terminal
        .allowed_users
        .iter()
        .filter(|(_, user_config)| user_config.interactive_shell_access)
        .map(|(user_id, _)| user_id.clone())
        .collect::<Vec<_>>();
    allowed.sort();
    match allowed.as_slice() {
        [user_id] => Ok(user_id.clone()),
        [] => bail!("no mobile_terminal.allowed_users have interactive_shell_access; pass --user-id after configuring a user"),
        _ => bail!("multiple mobile terminal users are configured; pass --user-id"),
    }
}

fn run_queue(client: &ApiClient, args: QueueArgs) -> Result<()> {
    match args.command {
        QueueCommand::List(args) => run_queue_list(client, args),
        QueueCommand::Status(args) => run_queue_status(client, args),
        QueueCommand::Run(args) => run_queue_run(client, args),
        QueueCommand::Cancel(args) => run_queue_cancel(client, args),
    }
}

fn run_queue_run(client: &ApiClient, args: QueueRunArgs) -> Result<()> {
    let notify_target = args
        .notify
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(optional_current_session_id)
        .ok_or_else(|| anyhow!("No session context. Use --notify or SESSION_MANAGER_ID."))?;
    let requester_session_id = optional_current_session_id();
    let cwd = match args.cwd {
        Some(value) => value,
        None => env::current_dir()?.display().to_string(),
    };
    let mut command = args.command;
    if command.first().is_some_and(|value| value == "--") {
        command.remove(0);
    }
    let script = match args.script_file {
        Some(path) if path == "-" => {
            let mut script = String::new();
            io::stdin()
                .read_to_string(&mut script)
                .context("failed to read queue script from stdin")?;
            Some(script)
        }
        Some(path) => Some(
            fs::read_to_string(&path)
                .with_context(|| format!("failed to read queue script file {}", path))?,
        ),
        None => None,
    };
    let argv = (!command.is_empty()).then_some(command);
    if argv.is_some() == script.is_some() {
        bail!("exactly one of command or --script-file is required");
    }
    let mut env_values = BTreeMap::new();
    for pair in args.env_pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --env value {pair:?}; expected KEY=VALUE"))?;
        if key.trim().is_empty() {
            bail!("invalid --env value {pair:?}; key is empty");
        }
        env_values.insert(key.to_owned(), value.to_owned());
    }
    let timeout_seconds = args
        .timeout
        .as_deref()
        .map(parse_duration_seconds)
        .transpose()?;
    let mut body = json!({
        "type": args.job_type,
        "label": args.label,
        "cwd": cwd,
        "env": env_values,
        "notify_target": notify_target,
        "requester_session_id": requester_session_id,
    });
    if let Some(argv) = argv {
        body["argv"] = json!(argv);
    }
    if let Some(script) = script {
        body["script"] = json!(script);
    }
    if let Some(timeout_seconds) = timeout_seconds {
        body["timeout_seconds"] = json!(timeout_seconds);
    }
    let payload = client.post_json("/queue-jobs", body)?;
    let id = payload["id"].as_str().unwrap_or("unknown");
    let label = payload["label"].as_str().unwrap_or("-");
    let state = payload["state"].as_str().unwrap_or("-");
    println!("Queued job {id}: {label} [{state}]");
    if let Some(log_path) = payload["log_path"].as_str() {
        println!("Log: {log_path}");
    }
    Ok(())
}

fn run_queue_list(client: &ApiClient, args: QueueListArgs) -> Result<()> {
    let mut query = Vec::new();
    let effective_notify = args
        .notify
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            if args.all {
                None
            } else {
                optional_current_session_id()
            }
        });
    if !args.all && effective_notify.is_none() {
        bail!("No session context. Use --notify or --all.");
    }
    if let Some(notify) = effective_notify {
        query.push(format!("notify_target={}", encode_query_component(&notify)));
    }
    if let Some(job_type) = args
        .job_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        query.push(format!("type={}", encode_query_component(job_type)));
    }
    if let Some(state) = args
        .state
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        query.push(format!("state={}", encode_query_component(state)));
    }
    if args.all || args.state.is_some() {
        query.push("include_terminal=true".to_owned());
    }
    let path = if query.is_empty() {
        "/queue-jobs".to_owned()
    } else {
        format!("/queue-jobs?{}", query.join("&"))
    };
    let payload = client.get_json(&path)?;
    let jobs = payload["jobs"].as_array().cloned().unwrap_or_default();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
        return Ok(());
    }
    print_queue_jobs(&jobs);
    Ok(())
}

fn run_queue_status(client: &ApiClient, args: QueueStatusArgs) -> Result<()> {
    let job_id = args.job_id.trim();
    if job_id.is_empty() {
        bail!("job id is required");
    }
    let payload = client.get_json(&format!("/queue-jobs/{}", encode_path_segment(job_id)))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    println!("Job: {}", payload["id"].as_str().unwrap_or(job_id));
    println!("Type: {}", payload["type"].as_str().unwrap_or("-"));
    println!("State: {}", payload["state"].as_str().unwrap_or("-"));
    println!(
        "Holding: {}",
        payload["holding_reason"].as_str().unwrap_or("-")
    );
    println!(
        "Exit: {}",
        payload["exit_code"]
            .as_i64()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    println!("Log: {}", payload["log_path"].as_str().unwrap_or("-"));
    Ok(())
}

fn run_queue_cancel(client: &ApiClient, args: QueueCancelArgs) -> Result<()> {
    let job_id = args.job_id.trim();
    if job_id.is_empty() {
        bail!("job id is required");
    }
    let payload = client.delete_json(
        &format!("/queue-jobs/{}", encode_path_segment(job_id)),
        json!({}),
    )?;
    println!(
        "Cancelled queue job: {} ({})",
        payload["id"].as_str().unwrap_or(job_id),
        payload["state"].as_str().unwrap_or("-")
    );
    Ok(())
}

fn run_review(client: &ApiClient, args: ReviewArgs) -> Result<()> {
    if let Some(pr_number) = args.pr {
        return run_review_pr(client, &args, pr_number);
    }

    let selection = review_mode_selection(&args)?;
    let parent_session_id = optional_current_session_id();
    let wait = effective_review_wait(args.wait, parent_session_id.as_deref());

    if args.new {
        let parent_session_id = parent_session_id.ok_or_else(|| {
            anyhow!("Error: --new requires session context (CLAUDE_SESSION_MANAGER_ID must be set)")
        })?;
        let payload = review_spawn_payload(
            &parent_session_id,
            &selection,
            args.steer.as_deref(),
            args.name.as_deref(),
            wait,
            args.model.as_deref(),
            args.working_dir.as_deref(),
        );
        let response = client.post_json("/sessions/review", payload)?;
        bail_review_error(&response)?;

        let child_id = response["session_id"].as_str().unwrap_or("unknown");
        let child_name = response["friendly_name"]
            .as_str()
            .or_else(|| response["name"].as_str())
            .unwrap_or(child_id);
        println!(
            "Review started on {child_name} ({child_id}) — mode={}",
            selection.mode
        );
        if let Some(wait) = wait {
            println!("  Watching for completion (timeout={wait}s)");
        }
        return Ok(());
    }

    let session = args
        .session
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Error: Must specify a session or use --new"))?;
    let session_id = lookup_identifier(client, session)?
        .ok_or_else(|| anyhow!("Error: Session '{session}' not found"))?;
    let session_info =
        client.get_json(&format!("/sessions/{}", encode_path_segment(&session_id)))?;
    let payload = review_existing_payload(
        &selection,
        args.steer.as_deref(),
        wait,
        parent_session_id.as_deref(),
    );
    let response = client.post_json(
        &format!("/sessions/{}/review", encode_path_segment(&session_id)),
        payload,
    )?;
    bail_review_error(&response)?;

    let session_name = session_info["friendly_name"]
        .as_str()
        .or_else(|| session_info["name"].as_str())
        .unwrap_or(&session_id);
    println!(
        "Review started on {session_name} ({session_id}) — mode={}",
        selection.mode
    );
    if let Some(steer) = trimmed_string(args.steer.as_deref()) {
        let preview = steer.chars().take(60).collect::<String>();
        println!("  Steer queued: {preview}...");
    }
    if let Some(wait) = wait {
        println!("  Watching for completion (timeout={wait}s)");
    }
    Ok(())
}

fn run_review_pr(client: &ApiClient, args: &ReviewArgs, pr_number: u64) -> Result<()> {
    if args
        .session
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
        || args.new
    {
        bail!("Error: --pr is mutually exclusive with session/--new");
    }
    if !review_tui_mode_names(args).is_empty() {
        bail!("Error: --pr is mutually exclusive with --base/--uncommitted/--commit/--custom");
    }

    let parent_session_id = optional_current_session_id();
    let wait = effective_review_wait(args.wait, parent_session_id.as_deref());
    let payload = review_pr_payload(
        pr_number,
        args.repo.as_deref(),
        args.steer.as_deref(),
        wait,
        parent_session_id.as_deref(),
    );
    let response = client.post_json("/reviews/pr", payload)?;
    bail_review_error(&response)?;

    let resolved_repo = response["repo"]
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| trimmed_string(args.repo.as_deref()))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("Posted @codex review on PR #{pr_number} ({resolved_repo})");
    if response["server_polling"].as_bool().unwrap_or(false) {
        if let Some(wait) = wait {
            println!("  Server polling for completion (timeout={wait}s)");
        }
    }
    Ok(())
}

fn review_mode_selection(args: &ReviewArgs) -> Result<ReviewModeSelection> {
    let modes = review_tui_mode_names(args);
    if modes.is_empty() {
        bail!("Error: Must specify one of --base, --uncommitted, --commit, --custom, or --pr");
    }
    if modes.len() > 1 {
        bail!(
            "Error: Modes are mutually exclusive. Got: {}",
            modes.join(", ")
        );
    }

    match modes[0] {
        "base" => Ok(ReviewModeSelection {
            mode: "branch",
            base_branch: trimmed_string(args.base.as_deref()),
            commit_sha: None,
            custom_prompt: None,
        }),
        "uncommitted" => Ok(ReviewModeSelection {
            mode: "uncommitted",
            base_branch: None,
            commit_sha: None,
            custom_prompt: None,
        }),
        "commit" => Ok(ReviewModeSelection {
            mode: "commit",
            base_branch: None,
            commit_sha: trimmed_string(args.commit.as_deref()),
            custom_prompt: None,
        }),
        "custom" => Ok(ReviewModeSelection {
            mode: "custom",
            base_branch: None,
            commit_sha: None,
            custom_prompt: trimmed_string(args.custom.as_deref()),
        }),
        _ => unreachable!("review_tui_mode_names returned an unknown mode"),
    }
}

fn review_tui_mode_names(args: &ReviewArgs) -> Vec<&'static str> {
    let mut modes = Vec::new();
    if trimmed_string(args.base.as_deref()).is_some() {
        modes.push("base");
    }
    if args.uncommitted {
        modes.push("uncommitted");
    }
    if trimmed_string(args.commit.as_deref()).is_some() {
        modes.push("commit");
    }
    if trimmed_string(args.custom.as_deref()).is_some() {
        modes.push("custom");
    }
    modes
}

fn effective_review_wait(
    explicit_wait: Option<u64>,
    parent_session_id: Option<&str>,
) -> Option<u64> {
    explicit_wait.or_else(|| parent_session_id.map(|_| 600))
}

fn review_existing_payload(
    selection: &ReviewModeSelection,
    steer: Option<&str>,
    wait: Option<u64>,
    watcher_session_id: Option<&str>,
) -> Value {
    let mut payload = review_mode_payload(selection);
    insert_trimmed(&mut payload, "steer", steer);
    insert_u64(&mut payload, "wait", wait);
    insert_trimmed(&mut payload, "watcher_session_id", watcher_session_id);
    Value::Object(payload)
}

fn review_spawn_payload(
    parent_session_id: &str,
    selection: &ReviewModeSelection,
    steer: Option<&str>,
    name: Option<&str>,
    wait: Option<u64>,
    model: Option<&str>,
    working_dir: Option<&str>,
) -> Value {
    let mut payload = review_mode_payload(selection);
    payload.insert(
        "parent_session_id".to_owned(),
        Value::String(parent_session_id.to_owned()),
    );
    insert_trimmed(&mut payload, "steer", steer);
    insert_trimmed(&mut payload, "name", name);
    insert_u64(&mut payload, "wait", wait);
    insert_trimmed(&mut payload, "model", model);
    insert_trimmed(&mut payload, "working_dir", working_dir);
    Value::Object(payload)
}

fn review_pr_payload(
    pr_number: u64,
    repo: Option<&str>,
    steer: Option<&str>,
    wait: Option<u64>,
    caller_session_id: Option<&str>,
) -> Value {
    let mut payload = Map::new();
    payload.insert("pr_number".to_owned(), json!(pr_number));
    insert_trimmed(&mut payload, "repo", repo);
    insert_trimmed(&mut payload, "steer", steer);
    insert_u64(&mut payload, "wait", wait);
    insert_trimmed(&mut payload, "caller_session_id", caller_session_id);
    Value::Object(payload)
}

fn review_mode_payload(selection: &ReviewModeSelection) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("mode".to_owned(), Value::String(selection.mode.to_owned()));
    insert_trimmed(
        &mut payload,
        "base_branch",
        selection.base_branch.as_deref(),
    );
    insert_trimmed(&mut payload, "commit_sha", selection.commit_sha.as_deref());
    insert_trimmed(
        &mut payload,
        "custom_prompt",
        selection.custom_prompt.as_deref(),
    );
    payload
}

fn insert_trimmed(payload: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = trimmed_string(value) {
        payload.insert(key.to_owned(), Value::String(value));
    }
}

fn insert_u64(payload: &mut Map<String, Value>, key: &str, value: Option<u64>) {
    if let Some(value) = value {
        payload.insert(key.to_owned(), json!(value));
    }
}

fn bail_review_error(payload: &Value) -> Result<()> {
    if let Some(error) = payload["error"]
        .as_str()
        .or_else(|| payload["detail"].as_str())
    {
        bail!("Error: {error}");
    }
    Ok(())
}

fn run_request_codex_review(client: &ApiClient, mut args: RequestCodexReviewArgs) -> Result<()> {
    match args.command.take() {
        Some(RequestCodexReviewCommand::List) => run_request_codex_review_list(client, args),
        Some(RequestCodexReviewCommand::Status { request_id }) => {
            run_request_codex_review_status(client, args, request_id)
        }
        Some(RequestCodexReviewCommand::Cancel { request_id }) => {
            run_request_codex_review_cancel(client, args, request_id)
        }
        None => run_request_codex_review_create(client, args),
    }
}

fn run_request_codex_review_create(client: &ApiClient, args: RequestCodexReviewArgs) -> Result<()> {
    let action_or_pr = args
        .action_or_pr
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("first argument must be a PR number, list, status, or cancel"))?;
    let pr_number = action_or_pr
        .parse::<i64>()
        .map_err(|_| anyhow!("first argument must be a PR number, list, status, or cancel"))?;
    let current_session_id = optional_current_session_id();
    let effective_notify = args
        .notify
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| current_session_id.clone())
        .ok_or_else(|| {
            anyhow!("No notify target. Use --notify or run from within a managed session.")
        })?;
    let resolved_repo = args
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(resolve_codex_review_repo_from_cwd);
    if resolved_repo.is_none() && current_session_id.is_none() {
        bail!("Could not determine GitHub repo; pass --repo explicitly.");
    }
    let payload = codex_review_create_payload(
        pr_number,
        resolved_repo,
        args.steer.as_deref(),
        &effective_notify,
        current_session_id.as_deref(),
        args.poll_interval_seconds,
        args.retry_interval_seconds,
    );
    let response = client.post_json("/codex-review-requests", payload)?;
    println!("Review requested for PR #{pr_number}, will sm send you when review arrives.");
    println!(
        "  Request: {} -> {}",
        response["id"].as_str().unwrap_or("unknown"),
        response["notify_name"]
            .as_str()
            .or_else(|| response["notify_session_id"].as_str())
            .unwrap_or(&effective_notify)
    );
    Ok(())
}

fn run_request_codex_review_list(client: &ApiClient, args: RequestCodexReviewArgs) -> Result<()> {
    let path = codex_review_requests_list_path(&args, args.inactive || args.all)?;
    let payload = client.get_json(&path)?;
    let requests = payload["requests"].as_array().cloned().unwrap_or_default();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&requests)?);
        return Ok(());
    }
    print_codex_review_requests(&requests);
    Ok(())
}

fn run_request_codex_review_status(
    client: &ApiClient,
    args: RequestCodexReviewArgs,
    request_id: Option<String>,
) -> Result<()> {
    let payload = if let Some(request_id) = request_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        client.get_json(&format!(
            "/codex-review-requests/{}",
            encode_path_segment(request_id)
        ))?
    } else {
        let path = codex_review_requests_list_path(&args, true)?;
        let requests = client.get_json(&path)?["requests"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        requests
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("No Codex review request found"))?
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    print_codex_review_request(&payload);
    Ok(())
}

fn run_request_codex_review_cancel(
    client: &ApiClient,
    args: RequestCodexReviewArgs,
    request_id: Option<String>,
) -> Result<()> {
    let request_id = request_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("request ID required for cancel"))?;
    let payload = client.delete_json(
        &format!("/codex-review-requests/{}", encode_path_segment(request_id)),
        json!({}),
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    println!(
        "Cancelled Codex review request: {}",
        payload["id"].as_str().unwrap_or(request_id)
    );
    Ok(())
}

fn codex_review_requests_list_path(
    args: &RequestCodexReviewArgs,
    include_inactive: bool,
) -> Result<String> {
    let mut query = Vec::new();
    let effective_notify = args
        .notify
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            if args.all {
                None
            } else {
                optional_current_session_id()
            }
        });
    if !args.all && effective_notify.is_none() {
        bail!("No session context. Use --notify or --all.");
    }
    if let Some(notify) = effective_notify {
        query.push(format!("notify_target={}", encode_query_component(&notify)));
    }
    if let Some(repo) = args
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        query.push(format!("repo={}", encode_query_component(repo)));
    }
    if let Some(pr_number) = args.pr_number {
        query.push(format!("pr_number={pr_number}"));
    }
    if include_inactive {
        query.push("include_inactive=true".to_owned());
    }
    Ok(if query.is_empty() {
        "/codex-review-requests".to_owned()
    } else {
        format!("/codex-review-requests?{}", query.join("&"))
    })
}

fn codex_review_create_payload(
    pr_number: i64,
    repo: Option<String>,
    steer: Option<&str>,
    notify_target: &str,
    requester_session_id: Option<&str>,
    poll_interval_seconds: i64,
    retry_interval_seconds: i64,
) -> Value {
    json!({
        "pr_number": pr_number,
        "repo": repo,
        "steer": trimmed_string(steer),
        "notify_target": notify_target,
        "requester_session_id": trimmed_string(requester_session_id),
        "poll_interval_seconds": poll_interval_seconds,
        "retry_interval_seconds": retry_interval_seconds,
    })
}

fn trimmed_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn resolve_codex_review_repo_from_cwd() -> Option<String> {
    let output = process::Command::new("gh")
        .args([
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let repo = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!repo.is_empty()).then_some(repo)
}

fn print_codex_review_requests(requests: &[Value]) {
    if requests.is_empty() {
        println!("No Codex review requests.");
        return;
    }
    let headers = [
        "ID",
        "PR",
        "Notify",
        "State",
        "Attempts",
        "Pickup",
        "Next Retry",
    ];
    let rows = requests
        .iter()
        .map(|request| {
            vec![
                json_string(request, "id"),
                format!(
                    "{}#{}",
                    request["repo"].as_str().unwrap_or("?"),
                    request["pr_number"]
                        .as_i64()
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "?".to_owned())
                ),
                request["notify_name"]
                    .as_str()
                    .or_else(|| request["notify_session_id"].as_str())
                    .unwrap_or("")
                    .to_owned(),
                request["state"]
                    .as_str()
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| {
                        if request["is_active"].as_bool().unwrap_or(true) {
                            "active".to_owned()
                        } else {
                            "inactive".to_owned()
                        }
                    }),
                request["attempt_count"]
                    .as_i64()
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "0".to_owned()),
                if request["pickup_detected_at"].is_null() {
                    "-".to_owned()
                } else {
                    "yes".to_owned()
                },
                request["next_retry_at"].as_str().unwrap_or("-").to_owned(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&headers, &rows);
}

fn print_codex_review_request(payload: &Value) {
    println!("Request: {}", payload["id"].as_str().unwrap_or("unknown"));
    println!(
        "PR: {}#{}",
        payload["repo"].as_str().unwrap_or("?"),
        payload["pr_number"]
            .as_i64()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_owned())
    );
    println!(
        "Notify: {}",
        payload["notify_name"]
            .as_str()
            .or_else(|| payload["notify_session_id"].as_str())
            .unwrap_or("-")
    );
    println!("State: {}", payload["state"].as_str().unwrap_or("-"));
    println!(
        "Attempts: {}",
        payload["attempt_count"].as_i64().unwrap_or(0)
    );
    println!(
        "Pickup: {}",
        payload["pickup_detected_at"].as_str().unwrap_or("-")
    );
    println!(
        "Review landed: {}",
        payload["review_landed_at"].as_str().unwrap_or("-")
    );
    println!(
        "Review source: {}",
        payload["review_source"].as_str().unwrap_or("-")
    );
    println!(
        "Next retry: {}",
        payload["next_retry_at"].as_str().unwrap_or("-")
    );
    println!(
        "Last error: {}",
        payload["last_error"].as_str().unwrap_or("-")
    );
}

fn print_queue_jobs(jobs: &[Value]) {
    if jobs.is_empty() {
        println!("No queue jobs.");
        return;
    }
    let headers = ["ID", "Type", "State", "Notify", "Label", "Holding", "Log"];
    let rows = jobs
        .iter()
        .map(|job| {
            vec![
                json_string(job, "id"),
                json_string(job, "type"),
                json_string(job, "state"),
                job["notify_name"]
                    .as_str()
                    .or_else(|| job["notify_session_id"].as_str())
                    .unwrap_or("")
                    .to_owned(),
                json_string(job, "label"),
                job["holding_reason"].as_str().unwrap_or("-").to_owned(),
                job["log_path"].as_str().unwrap_or("-").to_owned(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&headers, &rows);
}

fn run_remove_device(client: &ApiClient, args: RemoveDeviceArgs) -> Result<()> {
    let device_id = args.device_id.trim();
    if device_id.is_empty() {
        bail!("device id is required");
    }
    let mut path = format!(
        "/client/mobile-terminal/devices/{}",
        encode_path_segment(device_id)
    );
    if let Some(user_id) = args
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        path.push_str("?user_id=");
        path.push_str(&encode_query_component(user_id));
    }
    let payload = client.request("DELETE", &path, None)?.into_json()?;
    let response_device_id = payload["device_key_id"].as_str().unwrap_or(device_id);
    let user_id = payload["user_id"].as_str().unwrap_or("unknown-user");
    let pending = payload["pending_tickets_revoked"].as_u64().unwrap_or(0);
    let active = payload["active_attaches_terminated"].as_u64().unwrap_or(0);
    let runtime_note = if payload["runtime_only"].as_bool().unwrap_or(false) {
        " runtime-only"
    } else {
        ""
    };
    if payload["already_revoked"].as_bool().unwrap_or(false) {
        println!("Device already revoked{runtime_note}: {response_device_id} ({user_id})");
    } else {
        println!("Device revoked{runtime_note}: {response_device_id} ({user_id})");
    }
    if pending > 0 || active > 0 {
        println!("Cleared {pending} pending ticket(s); terminated {active} active attach(es)");
    }
    Ok(())
}

fn print_mobile_devices(payload: &Value) -> Result<()> {
    let devices = payload["devices"]
        .as_array()
        .ok_or_else(|| anyhow!("device inventory response missing devices"))?;
    if devices.is_empty() {
        println!("No mobile terminal devices");
        return Ok(());
    }
    for device in devices {
        println!("{}", format_mobile_device_line(device));
    }
    Ok(())
}

fn format_mobile_device_line(device: &Value) -> String {
    let device_id = device["device_key_id"].as_str().unwrap_or("unknown-device");
    let user_id = device["user_id"].as_str().unwrap_or("unknown-user");
    let state = if device["revoked"].as_bool().unwrap_or(false) {
        "revoked"
    } else if device["enabled"].as_bool().unwrap_or(false) {
        "enabled"
    } else {
        "disabled"
    };
    format!("{device_id} {user_id} {state}")
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

fn restore_session(client: &ApiClient, args: RestoreArgs) -> Result<()> {
    let node = args
        .node
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let restore_session_id =
        if let Some(node) = node.filter(|value| !is_primary_restore_node(value)) {
            resolve_node_restore_candidate_id(client, node, &args.session_id)?
                .ok_or_else(|| anyhow!("Session '{}' not found", args.session_id))?
        } else {
            args.session_id.clone()
        };
    let path = restore_session_path(&restore_session_id, node);
    let payload = client.post_json(&path, json!({}))?;
    let restored_id = payload["id"].as_str().unwrap_or(&restore_session_id);
    if let Some(node) = node.filter(|value| !is_primary_restore_node(value)) {
        println!("Session restored: {restored_id} on node {node}");
    } else {
        println!("Session restored: {restored_id}");
    }
    Ok(())
}

fn restore_session_path(session_id: &str, node: Option<&str>) -> String {
    if let Some(node) = node.filter(|value| !is_primary_restore_node(value)) {
        format!(
            "/nodes/{}/restore-candidates/{}/restore",
            encode_path_segment(node),
            encode_path_segment(session_id)
        )
    } else {
        format!("/sessions/{}/restore", encode_path_segment(session_id))
    }
}

fn is_primary_restore_node(node: &str) -> bool {
    matches!(node.trim(), "" | "primary")
}

fn resolve_node_restore_candidate_id(
    client: &ApiClient,
    node: &str,
    identifier: &str,
) -> Result<Option<String>> {
    let payload = client.get_json(&node_restore_candidates_path(node))?;
    let sessions = payload["sessions"].as_array().cloned().unwrap_or_default();
    resolve_node_restore_candidate_id_from_sessions(identifier, &sessions)
}

fn node_restore_candidates_path(node: &str) -> String {
    format!(
        "/nodes/{}/restore-candidates?refresh=true",
        encode_path_segment(node)
    )
}

fn resolve_node_restore_candidate_id_from_sessions(
    identifier: &str,
    sessions: &[Value],
) -> Result<Option<String>> {
    let direct_matches = sessions
        .iter()
        .filter(|session| {
            ["id", "source_session_id"].iter().any(|field| {
                session[*field]
                    .as_str()
                    .is_some_and(|value| value == identifier)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    match direct_matches.len() {
        1 => return Ok(non_empty_json_string(&direct_matches[0], "id")),
        count if count > 1 => {
            let matched_ids = node_restore_candidate_ids(&direct_matches);
            bail!(
                "Multiple node restore candidates match '{}': {}. Use a session ID.",
                identifier,
                matched_ids
            );
        }
        _ => {}
    }

    let alias_matches = sessions
        .iter()
        .filter(|session| {
            session["aliases"].as_array().is_some_and(|aliases| {
                aliases
                    .iter()
                    .any(|alias| alias.as_str() == Some(identifier))
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let name_matches = sessions
        .iter()
        .filter(|session| session["friendly_name"].as_str() == Some(identifier))
        .cloned()
        .collect::<Vec<_>>();
    let candidates = if alias_matches.is_empty() {
        name_matches
    } else {
        alias_matches
    };
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(non_empty_json_string(&candidates[0], "id")),
        _ => {
            let candidate_ids = node_restore_candidate_ids(&candidates);
            bail!(
                "Multiple node restore candidates match '{}': {}. Use a session ID.",
                identifier,
                candidate_ids
            );
        }
    }
}

fn node_restore_candidate_ids(candidates: &[Value]) -> String {
    candidates
        .iter()
        .filter_map(|candidate| non_empty_json_string(candidate, "id"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn non_empty_json_string(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
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

fn encode_query_component(value: &str) -> String {
    encode_path_segment(value)
}

fn parse_duration_seconds(value: &str) -> Result<i64> {
    if value.is_empty() {
        bail!("invalid duration: {value}");
    }
    if value.chars().all(|ch| ch.is_ascii_digit()) {
        let seconds = value
            .parse::<i64>()
            .with_context(|| format!("invalid duration: {value}"))?;
        if seconds <= 0 {
            bail!("invalid duration: {value}");
        }
        return Ok(seconds);
    }
    let mut total = 0i64;
    let mut index = 0usize;
    let bytes = value.as_bytes();
    while index < bytes.len() {
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if start == index || index >= bytes.len() {
            bail!("invalid duration: {value}");
        }
        let number = value[start..index]
            .parse::<i64>()
            .with_context(|| format!("invalid duration: {value}"))?;
        let multiplier = match bytes[index].to_ascii_lowercase() {
            b's' => 1,
            b'm' => 60,
            b'h' => 3600,
            b'd' => 86400,
            _ => bail!("invalid duration: {value}"),
        };
        total = total
            .checked_add(
                number
                    .checked_mul(multiplier)
                    .ok_or_else(|| anyhow!("invalid duration: {value}"))?,
            )
            .ok_or_else(|| anyhow!("invalid duration: {value}"))?;
        index += 1;
    }
    if total <= 0 {
        bail!("invalid duration: {value}");
    }
    Ok(total)
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
    fn device_management_cli_parses_retained_commands() {
        let enroll_cli = Cli::try_parse_from([
            "sm",
            "enroll-device",
            "--config",
            "config.yaml",
            "--user-id",
            "rajesh",
            "--url-base",
            "http://studio.local:19192",
        ])
        .unwrap();
        let Command::EnrollDevice(enroll_args) = enroll_cli.command else {
            panic!("expected enroll-device command");
        };
        assert_eq!(enroll_args.config, PathBuf::from("config.yaml"));
        assert_eq!(enroll_args.user_id.as_deref(), Some("rajesh"));
        assert_eq!(enroll_args.expires_in_minutes, 15);
        assert_eq!(
            enroll_args.url_base.as_deref(),
            Some("http://studio.local:19192")
        );

        let list_cli = Cli::try_parse_from(["sm", "list-devices", "--json"]).unwrap();
        let Command::ListDevices(list_args) = list_cli.command else {
            panic!("expected list-devices command");
        };
        assert!(list_args.json);

        let remove_cli = Cli::try_parse_from([
            "sm",
            "remove-device",
            "android-1",
            "--user-id",
            "local_bypass",
        ])
        .unwrap();
        let Command::RemoveDevice(remove_args) = remove_cli.command else {
            panic!("expected remove-device command");
        };
        assert_eq!(remove_args.device_id, "android-1");
        assert_eq!(remove_args.user_id.as_deref(), Some("local_bypass"));
    }

    #[test]
    fn queue_run_cli_parses_retained_writer_command() {
        let cli = Cli::try_parse_from([
            "sm",
            "--api-url",
            "http://127.0.0.1:8422",
            "queue",
            "run",
            "--type",
            "tests",
            "--label",
            "unit queue",
            "--cwd",
            "/tmp",
            "--timeout",
            "10m",
            "--env",
            "EXTRA=1",
            "--notify",
            "run12345",
            "--",
            "echo",
            "hello",
        ])
        .unwrap();
        assert_eq!(cli.api_url.as_deref(), Some("http://127.0.0.1:8422"));
        let Command::Queue(queue_args) = cli.command else {
            panic!("expected queue command");
        };
        let QueueCommand::Run(run_args) = queue_args.command else {
            panic!("expected queue run command");
        };
        assert_eq!(run_args.job_type, "tests");
        assert_eq!(run_args.label.as_deref(), Some("unit queue"));
        assert_eq!(run_args.cwd.as_deref(), Some("/tmp"));
        assert_eq!(run_args.timeout.as_deref(), Some("10m"));
        assert_eq!(run_args.env_pairs, vec!["EXTRA=1"]);
        assert_eq!(run_args.notify.as_deref(), Some("run12345"));
        assert_eq!(run_args.command, vec!["echo", "hello"]);
        assert_eq!(parse_duration_seconds("45").unwrap(), 45);
        assert_eq!(parse_duration_seconds("10m").unwrap(), 600);
        assert_eq!(parse_duration_seconds("2h30m").unwrap(), 9000);
        assert_eq!(parse_duration_seconds("1d").unwrap(), 86400);
    }

    #[test]
    fn queue_cancel_cli_parses_retained_runtime_command() {
        let cli = Cli::try_parse_from(["sm", "queue", "cancel", "job_123abc"]).unwrap();
        let Command::Queue(queue_args) = cli.command else {
            panic!("expected queue command");
        };
        let QueueCommand::Cancel(cancel_args) = queue_args.command else {
            panic!("expected queue cancel command");
        };
        assert_eq!(cancel_args.job_id, "job_123abc");
    }

    #[test]
    fn review_cli_parses_retained_modes() {
        let existing_cli = Cli::try_parse_from([
            "sm",
            "review",
            "session-one",
            "--base",
            "main",
            "--wait",
            "12",
            "--steer",
            "focus on auth",
        ])
        .unwrap();
        let Command::Review(existing_args) = existing_cli.command else {
            panic!("expected review command");
        };
        assert_eq!(existing_args.session.as_deref(), Some("session-one"));
        assert_eq!(existing_args.base.as_deref(), Some("main"));
        assert_eq!(existing_args.wait, Some(12));
        assert_eq!(existing_args.steer.as_deref(), Some("focus on auth"));

        let new_cli = Cli::try_parse_from([
            "sm",
            "review",
            "--new",
            "--custom",
            "check the auth path",
            "--name",
            "reviewer",
            "--model",
            "gpt-5.4",
            "--working-dir",
            "/tmp/project",
        ])
        .unwrap();
        let Command::Review(new_args) = new_cli.command else {
            panic!("expected review command");
        };
        assert!(new_args.new);
        assert_eq!(new_args.custom.as_deref(), Some("check the auth path"));
        assert_eq!(new_args.name.as_deref(), Some("reviewer"));
        assert_eq!(new_args.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(new_args.working_dir.as_deref(), Some("/tmp/project"));

        let pr_cli = Cli::try_parse_from([
            "sm",
            "review",
            "--pr",
            "972",
            "--repo",
            "rajeshgoli/session-manager",
            "--wait",
            "600",
            "--steer",
            "focus on recovery",
        ])
        .unwrap();
        let Command::Review(pr_args) = pr_cli.command else {
            panic!("expected review command");
        };
        assert_eq!(pr_args.pr, Some(972));
        assert_eq!(pr_args.repo.as_deref(), Some("rajeshgoli/session-manager"));
        assert_eq!(pr_args.wait, Some(600));
        assert_eq!(pr_args.steer.as_deref(), Some("focus on recovery"));
    }

    #[test]
    fn review_mode_selection_preserves_python_validation() {
        let mut args = default_review_args();
        let error = review_mode_selection(&args).unwrap_err().to_string();
        assert!(error.contains(
            "Error: Must specify one of --base, --uncommitted, --commit, --custom, or --pr"
        ));

        args.base = Some("main".to_owned());
        args.uncommitted = true;
        let error = review_mode_selection(&args).unwrap_err().to_string();
        assert_eq!(
            error,
            "Error: Modes are mutually exclusive. Got: base, uncommitted"
        );

        args.uncommitted = false;
        let selection = review_mode_selection(&args).unwrap();
        assert_eq!(selection.mode, "branch");
        assert_eq!(selection.base_branch.as_deref(), Some("main"));
        assert!(selection.commit_sha.is_none());
        assert!(selection.custom_prompt.is_none());
    }

    #[test]
    fn review_payloads_preserve_python_fields() {
        let mut args = default_review_args();
        args.custom = Some("  inspect auth carefully  ".to_owned());
        let selection = review_mode_selection(&args).unwrap();
        let existing = review_existing_payload(
            &selection,
            Some("  focus on auth  "),
            Some(600),
            Some("parent001"),
        );
        assert_eq!(existing["mode"], "custom");
        assert_eq!(existing["custom_prompt"], "inspect auth carefully");
        assert_eq!(existing["steer"], "focus on auth");
        assert_eq!(existing["wait"], 600);
        assert_eq!(existing["watcher_session_id"], "parent001");
        assert!(existing["base_branch"].is_null());

        let mut base_args = default_review_args();
        base_args.base = Some(" main ".to_owned());
        let base_selection = review_mode_selection(&base_args).unwrap();
        let spawn = review_spawn_payload(
            "parent001",
            &base_selection,
            Some(" steer "),
            Some(" reviewer "),
            Some(60),
            Some(" gpt-5.4 "),
            Some(" /tmp/project "),
        );
        assert_eq!(spawn["parent_session_id"], "parent001");
        assert_eq!(spawn["mode"], "branch");
        assert_eq!(spawn["base_branch"], "main");
        assert_eq!(spawn["steer"], "steer");
        assert_eq!(spawn["name"], "reviewer");
        assert_eq!(spawn["wait"], 60);
        assert_eq!(spawn["model"], "gpt-5.4");
        assert_eq!(spawn["working_dir"], "/tmp/project");

        let pr = review_pr_payload(
            972,
            Some(" rajeshgoli/session-manager "),
            Some(" focus on recovery "),
            Some(600),
            Some("parent001"),
        );
        assert_eq!(pr["pr_number"], 972);
        assert_eq!(pr["repo"], "rajeshgoli/session-manager");
        assert_eq!(pr["steer"], "focus on recovery");
        assert_eq!(pr["wait"], 600);
        assert_eq!(pr["caller_session_id"], "parent001");
    }

    #[test]
    fn request_codex_review_cli_parses_retained_subcommands() {
        let create_cli = Cli::try_parse_from([
            "sm",
            "request-codex-review",
            "967",
            "--notify",
            "notify123",
            "--repo",
            "rajeshgoli/session-manager",
            "--steer",
            "focus on auth",
            "--poll-interval",
            "45",
            "--retry-interval",
            "900",
        ])
        .unwrap();
        let Command::RequestCodexReview(create_args) = create_cli.command else {
            panic!("expected request-codex-review command");
        };
        assert_eq!(create_args.action_or_pr.as_deref(), Some("967"));
        assert_eq!(create_args.notify.as_deref(), Some("notify123"));
        assert_eq!(
            create_args.repo.as_deref(),
            Some("rajeshgoli/session-manager")
        );
        assert_eq!(create_args.steer.as_deref(), Some("focus on auth"));
        assert_eq!(create_args.poll_interval_seconds, 45);
        assert_eq!(create_args.retry_interval_seconds, 900);
        assert!(create_args.command.is_none());

        let list_cli = Cli::try_parse_from([
            "sm",
            "request-codex-review",
            "list",
            "--notify",
            "notify123",
            "--repo",
            "rajeshgoli/session-manager",
            "--pr",
            "964",
            "--inactive",
            "--json",
        ])
        .unwrap();
        let Command::RequestCodexReview(list_args) = list_cli.command else {
            panic!("expected request-codex-review command");
        };
        assert_eq!(list_args.notify.as_deref(), Some("notify123"));
        assert_eq!(
            list_args.repo.as_deref(),
            Some("rajeshgoli/session-manager")
        );
        assert_eq!(list_args.pr_number, Some(964));
        assert!(list_args.inactive);
        assert!(list_args.json);
        assert!(matches!(
            list_args.command,
            Some(RequestCodexReviewCommand::List)
        ));

        let status_cli =
            Cli::try_parse_from(["sm", "request-codex-review", "--all", "status", "req123"])
                .unwrap();
        let Command::RequestCodexReview(status_args) = status_cli.command else {
            panic!("expected request-codex-review command");
        };
        assert!(status_args.all);
        let Some(RequestCodexReviewCommand::Status { request_id }) = status_args.command else {
            panic!("expected status subcommand");
        };
        assert_eq!(request_id.as_deref(), Some("req123"));

        let cancel_cli =
            Cli::try_parse_from(["sm", "request-codex-review", "cancel", "req456"]).unwrap();
        let Command::RequestCodexReview(cancel_args) = cancel_cli.command else {
            panic!("expected request-codex-review command");
        };
        let Some(RequestCodexReviewCommand::Cancel { request_id }) = cancel_args.command else {
            panic!("expected cancel subcommand");
        };
        assert_eq!(request_id.as_deref(), Some("req456"));
    }

    #[test]
    fn codex_review_request_list_path_preserves_python_filters() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SESSION_MANAGER_ID", "CLAUDE_SESSION_MANAGER_ID"]);
        env::set_var("SESSION_MANAGER_ID", "session one");

        let args = RequestCodexReviewArgs {
            action_or_pr: None,
            notify: None,
            repo: Some("rajeshgoli/session-manager".to_owned()),
            steer: None,
            all: false,
            inactive: false,
            json: false,
            pr_number: Some(964),
            poll_interval_seconds: 30,
            retry_interval_seconds: 600,
            command: Some(RequestCodexReviewCommand::List),
        };
        assert_eq!(
            codex_review_requests_list_path(&args, false).unwrap(),
            "/codex-review-requests?notify_target=session%20one&repo=rajeshgoli%2Fsession-manager&pr_number=964"
        );

        let all_args = RequestCodexReviewArgs {
            action_or_pr: None,
            notify: None,
            repo: None,
            steer: None,
            all: true,
            inactive: false,
            json: false,
            pr_number: None,
            poll_interval_seconds: 30,
            retry_interval_seconds: 600,
            command: Some(RequestCodexReviewCommand::List),
        };
        assert_eq!(
            codex_review_requests_list_path(&all_args, true).unwrap(),
            "/codex-review-requests?include_inactive=true"
        );
    }

    #[test]
    fn codex_review_create_payload_preserves_python_fields() {
        let payload = codex_review_create_payload(
            967,
            Some("rajeshgoli/session-manager".to_owned()),
            Some(" focus on auth "),
            "notify123",
            Some(" requester001 "),
            45,
            900,
        );
        assert_eq!(payload["pr_number"], 967);
        assert_eq!(payload["repo"], "rajeshgoli/session-manager");
        assert_eq!(payload["steer"], "focus on auth");
        assert_eq!(payload["notify_target"], "notify123");
        assert_eq!(payload["requester_session_id"], "requester001");
        assert_eq!(payload["poll_interval_seconds"], 45);
        assert_eq!(payload["retry_interval_seconds"], 900);

        let fallback_payload =
            codex_review_create_payload(967, None, Some("   "), "notify123", None, 30, 600);
        assert!(fallback_payload["repo"].is_null());
        assert!(fallback_payload["steer"].is_null());
        assert!(fallback_payload["requester_session_id"].is_null());
    }

    fn default_review_args() -> ReviewArgs {
        ReviewArgs {
            session: None,
            base: None,
            uncommitted: false,
            commit: None,
            custom: None,
            new: false,
            name: None,
            wait: None,
            model: None,
            working_dir: None,
            steer: None,
            pr: None,
            repo: None,
        }
    }

    #[test]
    fn mobile_device_lines_show_state_without_key_material() {
        let enabled = json!({
            "user_id": "local_bypass",
            "device_key_id": "android-1",
            "enabled": true,
            "revoked": false,
            "public_key": "should-not-be-printed",
        });
        let revoked = json!({
            "user_id": "local_bypass",
            "device_key_id": "android-1",
            "enabled": true,
            "revoked": true,
            "public_key": "should-not-be-printed",
        });

        assert_eq!(
            format_mobile_device_line(&enabled),
            "android-1 local_bypass enabled"
        );
        assert_eq!(
            format_mobile_device_line(&revoked),
            "android-1 local_bypass revoked"
        );
        assert!(!format_mobile_device_line(&enabled).contains("should-not-be-printed"));
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

    #[test]
    fn restore_session_path_uses_node_inventory_for_non_primary_nodes() {
        assert_eq!(
            restore_session_path("abc123", None),
            "/sessions/abc123/restore"
        );
        assert_eq!(
            restore_session_path("abc123", Some("primary")),
            "/sessions/abc123/restore"
        );
        assert_eq!(
            restore_session_path("abc123", Some("local")),
            "/nodes/local/restore-candidates/abc123/restore"
        );
        assert_eq!(
            restore_session_path("abc123", Some("localhost")),
            "/nodes/localhost/restore-candidates/abc123/restore"
        );
        assert_eq!(
            restore_session_path("abc123", Some("studio")),
            "/nodes/studio/restore-candidates/abc123/restore"
        );
        assert_eq!(
            restore_session_path("abc123", Some("macbook")),
            "/nodes/macbook/restore-candidates/abc123/restore"
        );
        assert_eq!(
            restore_session_path("id/with space", Some("node/with space")),
            "/nodes/node%2Fwith%20space/restore-candidates/id%2Fwith%20space/restore"
        );
    }

    #[test]
    fn node_restore_candidates_path_forces_inventory_refresh() {
        assert_eq!(
            node_restore_candidates_path("macbook"),
            "/nodes/macbook/restore-candidates?refresh=true"
        );
        assert_eq!(
            node_restore_candidates_path("node/with space"),
            "/nodes/node%2Fwith%20space/restore-candidates?refresh=true"
        );
    }

    #[test]
    fn node_restore_candidate_resolution_matches_python_order() {
        let sessions = vec![
            json!({
                "id": "candidate-a",
                "source_session_id": "source-a",
                "aliases": ["alias-a"],
                "friendly_name": "shared-name"
            }),
            json!({
                "id": "candidate-b",
                "source_session_id": "source-b",
                "aliases": ["alias-b"],
                "friendly_name": "friendly-b"
            }),
        ];

        assert_eq!(
            resolve_node_restore_candidate_id_from_sessions("candidate-a", &sessions).unwrap(),
            Some("candidate-a".to_owned())
        );
        assert_eq!(
            resolve_node_restore_candidate_id_from_sessions("source-b", &sessions).unwrap(),
            Some("candidate-b".to_owned())
        );
        assert_eq!(
            resolve_node_restore_candidate_id_from_sessions("alias-a", &sessions).unwrap(),
            Some("candidate-a".to_owned())
        );
        assert_eq!(
            resolve_node_restore_candidate_id_from_sessions("friendly-b", &sessions).unwrap(),
            Some("candidate-b".to_owned())
        );
        assert_eq!(
            resolve_node_restore_candidate_id_from_sessions("missing", &sessions).unwrap(),
            None
        );
    }

    #[test]
    fn node_restore_candidate_resolution_reports_ambiguity() {
        let sessions = vec![
            json!({
                "id": "candidate-a",
                "source_session_id": "source-a",
                "aliases": ["shared-alias"],
                "friendly_name": "friendly-a"
            }),
            json!({
                "id": "candidate-b",
                "source_session_id": "source-b",
                "aliases": ["shared-alias"],
                "friendly_name": "friendly-b"
            }),
        ];

        let error = resolve_node_restore_candidate_id_from_sessions("shared-alias", &sessions)
            .unwrap_err()
            .to_string();

        assert_eq!(
            error,
            "Multiple node restore candidates match 'shared-alias': candidate-a, candidate-b. Use a session ID."
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
