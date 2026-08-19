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
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const DEFAULT_API_URL: &str = "http://127.0.0.1:8420";
const CONTEXT_COMPACT_STALE_SECONDS: i64 = 10 * 60;
const CLIENT_CONFIG_ENV: &str = "SM_CLIENT_CONFIG";
const CLIENT_CONFIG_SUBPATH: &str = "session-manager/client.yaml";
const WATCH_PYTHON_ENV: &str = "SM_WATCH_PYTHON";
const WATCH_REPO_ROOT_ENV: &str = "SM_WATCH_REPO_ROOT";

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
    #[command(alias = "btw")]
    What(WhatArgs),
    Usage(UsageArgs),
    Remind(RemindArgs),
    Wait(WaitArgs),
    Spawn(SpawnArgs),
    Fork(ForkArgs),
    New(NewArgs),
    Name(NameArgs),
    Children(ChildrenArgs),
    Tail(TailArgs),
    Retire(SessionIdArgs),
    Reparent(ReparentArgs),
    #[command(name = "reparent-tree")]
    ReparentTree(ReparentTreeArgs),
    Adopt(AdoptArgs),
    Recredential(RecredentialArgs),
    Restore(RestoreArgs),
    Attach(SessionIdArgs),
    Output(OutputArgs),
    Clear(ClearArgs),
    Handoff(HandoffArgs),
    #[command(name = "task-complete")]
    TaskComplete(EmptyArgs),
    #[command(name = "turn-complete")]
    TurnComplete(EmptyArgs),
    #[command(alias = "ctx")]
    Context(ContextArgs),
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
    #[command(name = "codex-original", alias = "codex-stock")]
    CodexOriginal(ProviderLaunchArgs),
    #[command(name = "codex-app")]
    CodexApp(ProviderLaunchArgs),
    #[command(name = "codex-fork", alias = "codex_fork")]
    CodexFork(ProviderLaunchArgs),
    #[command(name = "codex-2")]
    Codex2(ProviderLaunchArgs),
    Watch(WatchArgs),
}

#[derive(Args)]
struct EmptyArgs {}

#[derive(Args)]
struct StatusArgs {
    text: Vec<String>,
}

#[derive(Args)]
struct ReparentArgs {
    #[command(subcommand)]
    command: ReparentCommand,
}

#[derive(Subcommand)]
enum ReparentCommand {
    Request {
        child: String,
        #[arg(long)]
        to: String,
    },
    Approve {
        request_id: String,
    },
    Reject {
        request_id: String,
    },
    Status {
        request_id: Option<String>,
    },
    Repair {
        request_id: String,
        #[arg(long, conflicts_with = "rollback_precommit")]
        resume: bool,
        #[arg(long = "rollback-precommit", conflicts_with = "resume")]
        rollback_precommit: bool,
    },
}

#[derive(Args)]
struct ReparentTreeArgs {
    source: String,
    #[arg(long)]
    to: String,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct AdoptArgs {
    child: String,
}

#[derive(Args)]
struct RecredentialArgs {
    session: Option<String>,
    #[arg(long, conflicts_with = "session")]
    all_live: bool,
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
struct WhatArgs {
    session_id: String,
    prompt: Vec<String>,
}

#[derive(Args)]
struct UsageArgs {
    agent: Option<String>,
    #[arg(long)]
    include_children: bool,
    #[arg(long, conflicts_with = "history")]
    since_reset: bool,
    #[arg(long, conflicts_with = "since_reset")]
    history: bool,
    #[arg(long)]
    account: bool,
    #[arg(long)]
    by_model: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct RemindArgs {
    #[arg(long)]
    recurring: bool,
    #[arg(value_name = "DELAY_OR_ACTION")]
    delay_or_action: String,
    #[arg(value_name = "MESSAGE_OR_ID")]
    message_or_id: Vec<String>,
}

#[derive(Args)]
struct WaitArgs {
    session_id: String,
    seconds: u64,
}

#[derive(Args)]
struct SpawnArgs {
    provider: String,
    /// Legacy shell-argument prompt. Prefer --prompt-file or --prompt-stdin for briefs.
    #[arg(
        value_name = "PROMPT",
        help = "Initial prompt (legacy shell-argument form)"
    )]
    prompt: Vec<String>,
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "prompt_stdin",
        help = "Read the exact initial brief from a UTF-8 file"
    )]
    prompt_file: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "prompt_file",
        help = "Read the exact initial brief from standard input"
    )]
    prompt_stdin: bool,
    #[arg(long)]
    name: Option<String>,
    #[arg(long, value_name = "SECONDS")]
    wait: Option<u64>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_name = "LEVEL")]
    effort: Option<String>,
    #[arg(long)]
    working_dir: Option<String>,
    #[arg(long)]
    node: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long, hide = true)]
    id: Option<String>,
}

fn read_spawn_prompt(args: &SpawnArgs) -> Result<(String, Value)> {
    let mut stdin = io::stdin();
    read_spawn_prompt_from(args, &mut stdin)
}

fn read_spawn_prompt_from<R: Read>(args: &SpawnArgs, stdin: &mut R) -> Result<(String, Value)> {
    let source_count = usize::from(!args.prompt.is_empty())
        + usize::from(args.prompt_file.is_some())
        + usize::from(args.prompt_stdin);
    if source_count != 1 {
        bail!("provide exactly one prompt source: positional PROMPT, --prompt-file PATH, or --prompt-stdin");
    }

    let (bytes, source) = if let Some(path) = args.prompt_file.as_ref() {
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read spawn prompt file {}", path.display()))?;
        (
            bytes,
            json!({"kind": "file", "path": path.display().to_string()}),
        )
    } else if args.prompt_stdin {
        let mut bytes = Vec::new();
        stdin
            .read_to_end(&mut bytes)
            .context("failed to read spawn prompt from stdin")?;
        (bytes, json!({"kind": "stdin"}))
    } else {
        (
            args.prompt.join(" ").into_bytes(),
            json!({"kind": "positional"}),
        )
    };
    if bytes.is_empty() || String::from_utf8_lossy(&bytes).trim().is_empty() {
        bail!("spawn prompt must not be empty");
    }
    let prompt = String::from_utf8(bytes).context("spawn prompt must be valid UTF-8 text")?;
    Ok((prompt, source))
}

#[derive(Args)]
struct ForkArgs {
    #[arg(long = "self")]
    self_: bool,
    #[arg(long)]
    attach: bool,
}

#[derive(Args)]
struct NameArgs {
    name_or_session: String,
    new_name: Option<String>,
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
    #[arg(long)]
    usage: bool,
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
    #[arg(long, help = "Show rendered terminal output instead of activity")]
    raw: bool,
    #[arg(short = 'n', long, default_value_t = 10)]
    lines: usize,
}

#[derive(Args)]
#[command(
    after_help = "Destructive operation: after the current turn, this sends /clear to the calling Claude session and injects the handoff prompt."
)]
struct HandoffArgs {
    file_path: Option<String>,
}

#[derive(Args)]
struct ContextArgs {
    session_id: Option<String>,
    #[arg(long, conflicts_with = "detailed")]
    details: bool,
    #[arg(long)]
    detailed: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ContextMonitorArgs {
    #[command(subcommand)]
    command: Option<ContextMonitorCommand>,
}

#[derive(Subcommand)]
enum ContextMonitorCommand {
    Enable {
        target: Option<String>,
        /// Notification percentage for this seat. Repeat to register multiple
        /// levels, for example: --threshold 10 --threshold 20 --threshold 30.
        #[arg(long, value_name = "PERCENT")]
        threshold: Vec<f64>,
        /// Legacy second threshold. Prefer repeating --threshold instead.
        #[arg(long, value_name = "PERCENT", conflicts_with = "threshold")]
        critical_threshold: Option<f64>,
        /// Clear seat-specific thresholds and use the server defaults.
        #[arg(long, conflicts_with_all = ["threshold", "critical_threshold"])]
        use_default_thresholds: bool,
    },
    Disable {
        target: Option<String>,
    },
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
    Log(QueueLogArgs),
    Cancel(QueueCancelArgs),
}

#[derive(Args)]
struct QueueRunArgs {
    #[arg(long = "type", value_parser = ["tests", "perf", "background", "service"], default_value = "tests")]
    job_type: String,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    cwd: Option<String>,
    #[arg(long, value_name = "DURATION|none")]
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
    #[arg(
        long,
        help = "Include terminal history; without --notify, show every notify target"
    )]
    all: bool,
    #[arg(long = "type", value_parser = ["tests", "perf", "background", "service"])]
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
struct QueueLogArgs {
    job_id: String,
    #[arg(long, default_value_t = 200, value_parser = parse_queue_log_lines)]
    lines: usize,
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

#[derive(Args)]
struct WatchArgs {
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    role: Option<String>,
    #[arg(long, default_value_t = 2.0)]
    interval: f64,
    #[arg(long)]
    restore: bool,
    #[arg(long)]
    top_level: bool,
    #[arg(long, default_value = "retired", value_parser = ["retired", "last-active", "name"])]
    sort: String,
    #[arg(long)]
    node: Option<String>,
    #[arg(long)]
    all_nodes: bool,
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
            let (prompt, prompt_source) = read_spawn_prompt(&args)?;
            let parent_session_id = optional_current_session_id();
            let provider = launch_provider_for_alias(&args.provider)?;
            let payload = if let Some(parent_session_id) = parent_session_id {
                client.post_json(
                    "/sessions/spawn",
                    json!({
                        "id": args.id,
                        "parent_session_id": parent_session_id,
                        "prompt": prompt,
                        "prompt_source": prompt_source,
                        "name": args.name,
                        "wait": args.wait,
                        "model": args.model,
                        "reasoning_effort": args.effort,
                        "working_dir": args.working_dir,
                        "provider": provider,
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
                        "provider": provider,
                        "node": args.node,
                        "initial_message": prompt,
                        "spawn_prompt_source": prompt_source,
                        "model": args.model,
                        "reasoning_effort": args.effort,
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
        Command::Name(args) => rename_session(&client, args)?,
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
        Command::CodexOriginal(args) => launch_provider_session(
            &client,
            launch_provider_for_alias("codex-original")?,
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
            let mut payload = send_input_payload(text.clone(), delivery_mode, args.wait);
            if targets.len() > 1 {
                let targets = targets
                    .iter()
                    .map(|target| resolve_send_target(&client, target))
                    .collect::<Result<Vec<_>>>()?;
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
                if let Some(session_id) = lookup_identifier_exact(&client, target)? {
                    let payload =
                        client.post_json(&format!("/sessions/{session_id}/input"), payload)?;
                    println!(
                        "{}",
                        if payload["delivered"].as_bool().unwrap_or(false) {
                            "delivered"
                        } else {
                            "not delivered"
                        }
                    );
                } else {
                    if args.urgent || args.wait.is_some() {
                        bail!(
                            "email fallback only supports plain sequential sends without --wait/--urgent"
                        );
                    }
                    send_registered_email_fallback(&client, target, &text)?;
                }
            }
        }
        Command::What(args) => run_what(&client, args)?,
        Command::Usage(args) => run_usage(&client, args)?,
        Command::Remind(args) => run_remind(&client, args)?,
        Command::Output(args) => print_output(&client, &args.session_id, args.lines)?,
        Command::Tail(args) => print_tail(&client, &args.session_id, args.lines, args.raw)?,
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
            if args.usage {
                query.push("usage=true".to_owned());
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
            let requester_session_id = optional_current_session_id();
            let payload = client.post_json(
                &format!("/sessions/{}/retire", args.session_id),
                retire_request_payload(requester_session_id),
            )?;
            println!("{}", retire_response_status(&payload, &args.session_id)?);
        }
        Command::Reparent(args) => run_reparent(&client, args)?,
        Command::ReparentTree(args) => run_reparent_tree(&client, args)?,
        Command::Adopt(args) => run_adopt(&client, args)?,
        Command::Recredential(args) => run_recredential(&client, args)?,
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
                _ => println!(
                    "Handoff scheduled - after this turn it will /clear this session and inject the handoff prompt"
                ),
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
        Command::Context(args) => {
            run_context(&client, args)?;
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
            if let Some(human) = lookup_human(&client, &identifier)? {
                print_human_lookup(&human);
            } else if let Some(session_id) = lookup_identifier(&client, &identifier)? {
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
        Command::Watch(args) => run_watch(&api_url, args)?,
        _ => bail!("this retained command is not implemented in the Rust core slice yet"),
    }
    Ok(())
}

fn run_watch(api_url: &str, args: WatchArgs) -> Result<()> {
    if optional_current_session_id().is_some() {
        bail!(
            "Error: sm watch is operator-only. Run it from a non-managed shell \
             (without SESSION_MANAGER_ID)."
        );
    }
    if !args.interval.is_finite() || args.interval <= 0.0 {
        bail!("Error: --interval must be > 0");
    }

    let repo_root = watch_repo_root()?;
    let python = env::var_os(WATCH_PYTHON_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let venv_python = repo_root.join("venv/bin/python");
            if venv_python.is_file() {
                venv_python
            } else {
                PathBuf::from("python3")
            }
        });
    let python_path = watch_python_path(&repo_root)?;
    let status = process::Command::new(&python)
        .args(watch_python_args(&args))
        .env("PYTHONPATH", python_path)
        .env("SM_API_URL", api_url)
        .status()
        .with_context(|| {
            format!(
                "failed to launch retained sm watch implementation with {}",
                python.display()
            )
        })?;
    if !status.success() {
        bail!(
            "sm watch exited with status {}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_owned())
        );
    }
    Ok(())
}

fn watch_python_args(args: &WatchArgs) -> Vec<String> {
    let mut command = vec![
        "-m".to_owned(),
        "src.cli.main".to_owned(),
        "watch".to_owned(),
    ];
    if let Some(repo) = &args.repo {
        command.extend(["--repo".to_owned(), repo.clone()]);
    }
    if let Some(role) = &args.role {
        command.extend(["--role".to_owned(), role.clone()]);
    }
    command.extend(["--interval".to_owned(), args.interval.to_string()]);
    if args.restore {
        command.push("--restore".to_owned());
    }
    if args.top_level {
        command.push("--top-level".to_owned());
    }
    command.extend(["--sort".to_owned(), args.sort.clone()]);
    if let Some(node) = &args.node {
        command.extend(["--node".to_owned(), node.clone()]);
    }
    if args.all_nodes {
        command.push("--all-nodes".to_owned());
    }
    command
}

fn watch_repo_root() -> Result<PathBuf> {
    if let Some(explicit) = env::var_os(WATCH_REPO_ROOT_ENV) {
        let root = PathBuf::from(explicit);
        if is_watch_repo_root(&root) {
            return Ok(root);
        }
        bail!(
            "{WATCH_REPO_ROOT_ENV} does not point to a Session Manager checkout: {}",
            root.display()
        );
    }

    if let Ok(executable) = env::current_exe() {
        if let Some(root) = find_watch_repo_root(&executable) {
            return Ok(root);
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = find_watch_repo_root(&manifest_dir) {
        return Ok(root);
    }
    bail!("cannot locate the retained sm watch implementation; set {WATCH_REPO_ROOT_ENV}")
}

fn find_watch_repo_root(start: &Path) -> Option<PathBuf> {
    let start = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    start
        .ancestors()
        .find(|candidate| is_watch_repo_root(candidate))
        .map(Path::to_path_buf)
}

fn is_watch_repo_root(path: &Path) -> bool {
    path.join("src/cli/main.py").is_file() && path.join("src/cli/watch_tui.py").is_file()
}

fn watch_python_path(repo_root: &Path) -> Result<std::ffi::OsString> {
    let mut paths = vec![repo_root.to_path_buf()];
    if let Some(existing) = env::var_os("PYTHONPATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).context("failed to construct PYTHONPATH for sm watch")
}

fn required_positional(value: Option<String>, label: &str) -> Result<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("{label} is required"))
}

fn retire_request_payload(requester_session_id: Option<String>) -> Value {
    json!({ "requester_session_id": requester_session_id })
}

fn retire_response_status(payload: &Value, target_session_id: &str) -> Result<&'static str> {
    if let Some(error) = payload["error"].as_str() {
        bail!("{error}");
    }
    match payload["status"].as_str() {
        Some("retired" | "killed") => Ok("retired"),
        _ => bail!("Invalid retire response for session {target_session_id}"),
    }
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
        QueueCommand::Log(args) => run_queue_log(client, args),
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
    // Queue jobs run from a deliberately cleared process environment. Capture
    // the small, documented execution context at submission time so the job
    // resolves the same managed-session tools without inheriting credentials
    // or unrelated server state.
    let env_values =
        apply_queue_environment_overrides(captured_queue_environment(), args.env_pairs)?;
    let timeout_seconds = args
        .timeout
        .as_deref()
        .map(parse_queue_timeout_seconds)
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

fn apply_queue_environment_overrides(
    mut env_values: BTreeMap<String, String>,
    pairs: impl IntoIterator<Item = String>,
) -> Result<BTreeMap<String, String>> {
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --env value {pair:?}; expected KEY=VALUE"))?;
        if key.trim().is_empty() {
            bail!("invalid --env value {pair:?}; key is empty");
        }
        env_values.insert(key.to_owned(), value.to_owned());
    }
    Ok(env_values)
}

fn captured_queue_environment() -> BTreeMap<String, String> {
    queue_environment_from(|key| env::var(key).ok())
}

fn queue_environment_from<F>(mut lookup: F) -> BTreeMap<String, String>
where
    F: FnMut(&str) -> Option<String>,
{
    ["PATH"]
        .into_iter()
        .filter_map(|key| {
            lookup(key)
                .filter(|value| !value.is_empty())
                .map(|value| (key, value))
        })
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn run_queue_list(client: &ApiClient, args: QueueListArgs) -> Result<()> {
    let mut query = Vec::new();
    let explicit_notify = args
        .notify
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let effective_notify = explicit_notify.clone().or_else(|| {
        if args.all {
            None
        } else {
            optional_current_session_id()
        }
    });
    if !args.all && effective_notify.is_none() {
        bail!("No session context. Use --notify or --all.");
    }
    if let Some(ref notify) = effective_notify {
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
    println!(
        "{}",
        queue_list_scope_text(
            effective_notify.as_deref(),
            explicit_notify.is_some(),
            args.all,
            args.state.as_deref(),
        )
    );
    print_queue_jobs(&jobs);
    Ok(())
}

fn queue_list_scope_text(
    notify_target: Option<&str>,
    explicit_notify: bool,
    include_terminal: bool,
    state: Option<&str>,
) -> String {
    match (notify_target, explicit_notify, include_terminal, state) {
        (Some(notify), true, false, None) => format!(
            "Queue scope: active pending and running jobs for explicit notify target {notify}. --all retains this target and includes terminal history; use --all without --notify for history across notify targets."
        ),
        (Some(notify), true, _, Some(state)) => format!(
            "Queue scope: explicit notify target {notify}, filtered to state {state}. --all retains this target; use --all without --notify for history across notify targets."
        ),
        (Some(notify), true, true, None) => format!(
            "Queue scope: all jobs, including terminal history, for explicit notify target {notify}. Use --all without --notify for history across notify targets."
        ),
        (Some(notify), false, false, None) => format!(
            "Queue scope: active pending and running jobs for notify target {notify}. Use --all for history across notify targets."
        ),
        (Some(notify), false, _, Some(state)) => format!(
            "Queue scope: notify target {notify}, filtered to state {state}. Use --all with no --notify for history across notify targets."
        ),
        (Some(notify), false, true, None) => format!(
            "Queue scope: all jobs, including terminal history, for notify target {notify}. Use --all with no --notify for history across notify targets."
        ),
        (None, _, true, Some(state)) => format!(
            "Queue scope: all notify targets, filtered to state {state}."
        ),
        (None, _, true, _) => "Queue scope: all jobs, including terminal history, across notify targets.".to_owned(),
        (None, _, false, _) => "Queue scope: active pending and running jobs across notify targets.".to_owned(),
    }
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
    println!("Exit: {}", queue_exit_text(&payload));
    println!(
        "Termination: {}",
        payload["termination_reason"].as_str().unwrap_or("-")
    );
    println!("Log: {}", payload["log_path"].as_str().unwrap_or("-"));
    Ok(())
}

fn run_queue_log(client: &ApiClient, args: QueueLogArgs) -> Result<()> {
    let job_id = args.job_id.trim();
    if job_id.is_empty() {
        bail!("job id is required");
    }
    let payload = client.get_json(&format!(
        "/queue-jobs/{}/log?lines={}",
        encode_path_segment(job_id),
        args.lines
    ))?;
    let text = payload["text"]
        .as_str()
        .ok_or_else(|| anyhow!("queue log response is missing text"))?;
    print!("{text}");
    io::stdout().flush()?;
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
    let status_without_id = matches!(
        &args.command,
        Some(RequestCodexReviewCommand::Status { request_id: None })
    );
    let effective_notify = args
        .notify
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            if args.all || status_without_id {
                None
            } else {
                optional_current_session_id()
            }
        });
    if !args.all && !status_without_id && effective_notify.is_none() {
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
    let headers = [
        "ID", "Type", "State", "Exit", "Notify", "Label", "Holding", "Log",
    ];
    let rows = jobs
        .iter()
        .map(|job| {
            vec![
                json_string(job, "id"),
                json_string(job, "type"),
                json_string(job, "state"),
                queue_exit_text(job),
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

fn queue_exit_text(job: &Value) -> String {
    if let Some(exit_code) = job["exit_code"].as_i64() {
        return exit_code.to_string();
    }
    match job["exit_evidence"].as_str() {
        Some("missing_partial_output") => "unknown (partial/non-evidence)".to_owned(),
        _ => "-".to_owned(),
    }
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

fn print_tail(client: &ApiClient, identifier: &str, lines: usize, raw: bool) -> Result<()> {
    if lines == 0 || lines > 100 {
        bail!("tail lines must be between 1 and 100");
    }
    let session = client.get_json(&format!("/sessions/{}", encode_path_segment(identifier)))?;
    let session_id = session["id"]
        .as_str()
        .ok_or_else(|| anyhow!("session response missing id"))?;
    if raw {
        let payload = client.get_json(&format!(
            "/sessions/{}/output?lines={lines}&rendered=true",
            encode_path_segment(session_id)
        ))?;
        let Some(output) = payload["output"].as_str() else {
            bail!("No rendered output available for {identifier}");
        };
        print!("{}", strip_terminal_controls(output));
        return Ok(());
    }

    let name = session["friendly_name"]
        .as_str()
        .or_else(|| session["name"].as_str())
        .unwrap_or(session_id);
    let provider = session["provider"].as_str().unwrap_or("claude");
    if provider == "codex-app" {
        let payload = client.get_json(&format!(
            "/sessions/{}/activity-actions?limit={lines}",
            encode_path_segment(session_id)
        ))?;
        let actions = payload["actions"].as_array().cloned().unwrap_or_default();
        if actions.is_empty() {
            println!("No activity data for {name} ({session_id})");
            return Ok(());
        }
        println!(
            "Last {} actions ({name} {}):",
            actions.len(),
            short_session_id(session_id)
        );
        for action in actions {
            let timestamp = action["ended_at"]
                .as_str()
                .or_else(|| action["started_at"].as_str());
            let summary = action["summary_text"]
                .as_str()
                .or_else(|| action["action_kind"].as_str())
                .unwrap_or("activity");
            let status = action["status"].as_str().unwrap_or("");
            let status_suffix = if status.is_empty() {
                String::new()
            } else {
                format!(" [{status}]")
            };
            println!(
                "  [{} ago] {summary}{status_suffix}",
                format_tail_age(timestamp)
            );
        }
        return Ok(());
    }

    let payload = client.get_json(&format!(
        "/sessions/{}/tool-calls?limit={lines}",
        encode_path_segment(session_id)
    ))?;
    let mut rows = payload["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("No activity data for {name} ({session_id})");
        println!("Use `sm tail {identifier} --raw` for rendered terminal output.");
        return Ok(());
    }
    if rows.len() > 1
        && rows.first().and_then(|row| row["timestamp"].as_str())
            > rows.last().and_then(|row| row["timestamp"].as_str())
    {
        rows.reverse();
    }
    println!(
        "Last {} actions ({name} {}):",
        rows.len(),
        short_session_id(session_id)
    );
    for row in rows {
        let tool_name = row["tool_name"].as_str().unwrap_or("tool");
        println!(
            "  [{} ago] {tool_name}",
            format_tail_age(row["timestamp"].as_str())
        );
    }
    Ok(())
}

fn short_session_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}

fn format_tail_age(timestamp: Option<&str>) -> String {
    let Some(timestamp) = timestamp else {
        return "?".to_owned();
    };
    let parsed = OffsetDateTime::parse(timestamp, &Rfc3339).or_else(|_| {
        let format =
            time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
        time::PrimitiveDateTime::parse(timestamp, format).map(|value| value.assume_utc())
    });
    let Ok(timestamp) = parsed else {
        return "?".to_owned();
    };
    let seconds = (OffsetDateTime::now_utc() - timestamp)
        .whole_seconds()
        .max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3600 {
        return format!("{}m{:02}s", seconds / 60, seconds % 60);
    }
    format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
}

fn strip_terminal_controls(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut previous_escape = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_escape && next == '\\') {
                            break;
                        }
                        previous_escape = next == '\u{1b}';
                    }
                }
                Some('P' | 'X' | '^' | '_') => {
                    let mut previous_escape = false;
                    for next in chars.by_ref() {
                        if previous_escape && next == '\\' {
                            break;
                        }
                        previous_escape = next == '\u{1b}';
                    }
                }
                Some(_) | None => {}
            }
            continue;
        }
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            output.push(ch);
        }
    }
    output
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

fn resolve_send_target(client: &ApiClient, identifier: &str) -> Result<String> {
    match lookup_identifier(client, identifier)? {
        Some(session_id) => Ok(session_id),
        None => Ok(identifier.to_owned()),
    }
}

fn run_what(client: &ApiClient, args: WhatArgs) -> Result<()> {
    let target_id = lookup_identifier_exact(client, &args.session_id)?
        .ok_or_else(|| anyhow!("No agent named '{}' is reachable.", args.session_id))?;
    let requester_session_id = optional_current_session_id();
    let managed_caller = requester_session_id.is_some();
    let prompt = args.prompt.join(" ");
    let response = client.post_json(
        &format!("/sessions/{}/what", encode_path_segment(&target_id)),
        json!({
            "prompt": (!prompt.trim().is_empty()).then_some(prompt),
            "requester_session_id": requester_session_id,
            "delivery_mode": if managed_caller { "session" } else { "poll" },
        }),
    )?;
    let request_id = response["request_id"]
        .as_str()
        .ok_or_else(|| anyhow!("sm what response missing request_id"))?;
    if managed_caller {
        return Ok(());
    }

    let deadline = Instant::now() + Duration::from_secs(125);
    loop {
        let status = client.get_json(&format!(
            "/btw-requests/{}",
            encode_path_segment(request_id)
        ))?;
        match status["status"].as_str().unwrap_or("unknown") {
            "completed" => {
                println!("{}", status["result"].as_str().unwrap_or_default());
                return Ok(());
            }
            "failed" | "timed_out" => {
                bail!(
                    "{}",
                    status["error"].as_str().unwrap_or("sm what request failed")
                );
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            bail!("sm what timed out waiting for request {request_id}");
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn run_reparent(client: &ApiClient, args: ReparentArgs) -> Result<()> {
    match args.command {
        ReparentCommand::Request { child, to } => {
            let requester = current_session_id()?;
            let credential = current_session_credential(&requester)?;
            let child = resolve_required_session(client, &child)?;
            let target = resolve_required_session(client, &to)?;
            let record = client.post_json_with_session_credential(
                &format!("/sessions/{child}/reparent-requests"),
                json!({
                    "requester_session_id": requester,
                    "target_parent_session_id": target,
                }),
                &credential,
            )?;
            print_reparent_request_created(&record);
        }
        ReparentCommand::Approve { request_id } => {
            decide_reparent_request(client, &request_id, "approve")?;
        }
        ReparentCommand::Reject { request_id } => {
            decide_reparent_request(client, &request_id, "reject")?;
        }
        ReparentCommand::Status { request_id } => match request_id {
            Some(request_id) => {
                let record = get_reparent_json(
                    client,
                    &format!("/reparent-requests/{}", encode_path_segment(&request_id)),
                )?;
                print_reparent_request_detail(&record);
            }
            None => {
                let payload = get_reparent_json(client, "/reparent-requests")?;
                let records = payload["requests"].as_array().cloned().unwrap_or_default();
                if records.is_empty() {
                    println!("No reparent requests");
                } else {
                    for record in records {
                        println!("{}", format_reparent_request_row(&record));
                    }
                }
            }
        },
        ReparentCommand::Repair {
            request_id,
            resume,
            rollback_precommit,
        } => {
            let action = match (resume, rollback_precommit) {
                (true, false) => "resume",
                (false, true) => "rollback_precommit",
                _ => bail!("choose exactly one of --resume or --rollback-precommit"),
            };
            let record = client.post_json(
                &format!(
                    "/reparent-requests/{}/repair",
                    encode_path_segment(&request_id)
                ),
                json!({ "action": action }),
            )?;
            print_reparent_request_detail(&record);
        }
    }
    Ok(())
}

fn run_reparent_tree(client: &ApiClient, args: ReparentTreeArgs) -> Result<()> {
    let requester = current_session_id()?;
    let credential = current_session_credential(&requester)?;
    let source = resolve_required_session(client, &args.source)?;
    let target = resolve_required_session(client, &args.to)?;
    let response = client.post_json_with_session_credential(
        &format!("/sessions/{source}/reparent-tree-requests"),
        json!({
            "requester_session_id": requester,
            "target_session_id": target,
            "dry_run": args.dry_run,
        }),
        &credential,
    )?;
    if args.dry_run {
        print_reparent_tree_preview(&response);
    } else {
        print_reparent_request_created(&response);
    }
    Ok(())
}

fn run_adopt(client: &ApiClient, args: AdoptArgs) -> Result<()> {
    let requester = current_session_id()?;
    let credential = current_session_credential(&requester)?;
    let child = resolve_required_session(client, &args.child)?;
    let record = client.post_json_with_session_credential(
        &format!("/sessions/{child}/reparent-requests"),
        json!({
            "requester_session_id": requester,
            "target_parent_session_id": requester,
        }),
        &credential,
    )?;
    print_reparent_request_created(&record);
    Ok(())
}

fn run_recredential(client: &ApiClient, args: RecredentialArgs) -> Result<()> {
    let targets = if args.all_live {
        let payload = client.get_json("/sessions")?;
        payload["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|session| {
                matches!(
                    session["status"].as_str().unwrap_or_default(),
                    "running" | "idle" | "waiting_permission"
                )
            })
            .filter_map(|session| session["id"].as_str().map(ToOwned::to_owned))
            .collect::<Vec<_>>()
    } else {
        let target = args
            .session
            .as_deref()
            .context("session is required unless --all-live is used")?;
        vec![resolve_required_session(client, target)?]
    };
    if targets.is_empty() {
        println!("No live sessions to recredential");
        return Ok(());
    }
    let mut failed = 0usize;
    for target in targets {
        match client.post_json(
            &format!("/sessions/{target}/credential-rotation"),
            json!({}),
        ) {
            Ok(record) => println!("{}", format_recredential_outcome(&target, &record)),
            Err(error) => {
                failed += 1;
                eprintln!("{target} failed: {error:#}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} recredential request(s) failed");
    }
    Ok(())
}

fn format_recredential_outcome(target: &str, record: &Value) -> String {
    match record["status"].as_str().unwrap_or("unknown") {
        "waiting_idle" => format!(
            "{target} waiting_idle (pending; target is not recredentialed until idle proof completes)"
        ),
        "relaunching" => format!("{target} relaunching (pending; target is not yet recovered)"),
        status => format!("{target} {status}"),
    }
}

fn decide_reparent_request(client: &ApiClient, request_id: &str, action: &str) -> Result<()> {
    let requester = current_session_id()?;
    let credential = current_session_credential(&requester)?;
    let record = client.post_json_with_session_credential(
        &format!(
            "/reparent-requests/{}/{}",
            encode_path_segment(request_id),
            action
        ),
        json!({ "requester_session_id": requester }),
        &credential,
    )?;
    print_reparent_request_detail(&record);
    Ok(())
}

fn current_session_credential(session_id: &str) -> Result<String> {
    env::var("SM_SESSION_CREDENTIAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| {
            format!(
                "this session has no runtime credential; ask the operator to run `sm recredential {session_id}`"
            )
        })
}

fn get_reparent_json(client: &ApiClient, path: &str) -> Result<Value> {
    let Some(session_id) = optional_current_session_id() else {
        return client.get_json(path);
    };
    let credential = current_session_credential(&session_id)?;
    client.get_json_with_session_credential(path, &session_id, &credential)
}

fn resolve_required_session(client: &ApiClient, identifier: &str) -> Result<String> {
    lookup_identifier_exact(client, identifier)?
        .with_context(|| format!("No agent named '{identifier}' is reachable"))
}

fn print_reparent_request_created(record: &Value) {
    println!(
        "Reparent request {} created: {}",
        record["id"].as_str().unwrap_or("unknown"),
        format_reparent_request_row(record)
    );
}

fn format_reparent_request_row(record: &Value) -> String {
    let id = record["id"].as_str().unwrap_or("unknown");
    let kind = record["kind"].as_str().unwrap_or("unknown");
    let source = record["subject_session_id"].as_str().unwrap_or("unknown");
    let target = record["target_parent_session_id"]
        .as_str()
        .unwrap_or("unknown");
    let status = record["status"].as_str().unwrap_or("unknown");
    let mut row = format!("{id} {kind} {source} -> {target} {status}");
    if let Some(reason) = record["failure_reason"].as_str() {
        row.push_str(&format!(" ({reason})"));
    }
    row
}

fn print_reparent_request_detail(record: &Value) {
    println!("{}", format_reparent_request_row(record));
    println!(
        "initiator: {} · expires: {} · stage: {}",
        record["initiator_session_id"].as_str().unwrap_or("unknown"),
        record["expires_at"].as_str().unwrap_or("unknown"),
        record["apply_stage"].as_str().unwrap_or("-")
    );
    let edges = reparent_edges_for_display(record);
    print_reparent_edges(Some(&edges));
    let approvals = record["approvals"].as_array().cloned().unwrap_or_default();
    let required = record["required_agent_approvals"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for actor in required {
        let actor = actor.as_str().unwrap_or("unknown");
        let decision = approvals
            .iter()
            .find(|approval| approval["actor_kind"] == "agent" && approval["actor_id"] == actor)
            .and_then(|approval| approval["decision"].as_str())
            .unwrap_or("pending");
        println!("agent approval: {actor} {decision}");
    }
    if record["required_human_approval"].as_bool().unwrap_or(false) {
        let decision = approvals
            .iter()
            .find(|approval| approval["actor_kind"] == "human")
            .and_then(|approval| approval["decision"].as_str())
            .unwrap_or("pending");
        println!("human approval: {decision}");
    }
}

fn reparent_edges_for_display(record: &Value) -> Vec<Value> {
    if let Some(edges) = record["apply_plan"]["edge_changes"].as_array() {
        return edges.clone();
    }
    let source = record["subject_session_id"].as_str().unwrap_or("unknown");
    let target = record["target_parent_session_id"]
        .as_str()
        .unwrap_or("unknown");
    let old_parent = record["expected_parent_session_id"].as_str();
    if record["kind"] == "tree" {
        let peer_root_succession = record["peer_root_succession"].as_bool().unwrap_or(false);
        let mut edges = Vec::new();
        if !peer_root_succession {
            edges.push(json!({
                "session_id": target,
                "expected_parent_session_id": source,
                "new_parent_session_id": old_parent,
            }));
        }
        edges.push(json!({
            "session_id": source,
            "expected_parent_session_id": old_parent,
            "new_parent_session_id": target,
        }));
        edges.extend(
            record["frozen_live_child_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|child| *child != target)
                .map(|child| {
                    json!({
                        "session_id": child,
                        "expected_parent_session_id": source,
                        "new_parent_session_id": target,
                    })
                }),
        );
        edges
    } else {
        vec![json!({
            "session_id": source,
            "expected_parent_session_id": old_parent,
            "new_parent_session_id": target,
        })]
    }
}

fn print_reparent_tree_preview(preview: &Value) {
    println!(
        "Dry run: {} -> {}",
        preview["source_session_id"].as_str().unwrap_or("unknown"),
        preview["target_session_id"].as_str().unwrap_or("unknown")
    );
    if preview["peer_root_succession"].as_bool().unwrap_or(false) {
        println!("mode: peer-root succession");
    }
    print_reparent_edges(preview["edge_changes"].as_array());
    for blocker in preview["blockers"].as_array().into_iter().flatten() {
        println!("blocker: {}", blocker.as_str().unwrap_or("unknown"));
    }
    for actor in preview["required_agent_approvals"]
        .as_array()
        .into_iter()
        .flatten()
    {
        println!("agent approval: {}", actor.as_str().unwrap_or("unknown"));
    }
    if preview["required_human_approval"]
        .as_bool()
        .unwrap_or(false)
    {
        println!("human approval: required");
    }
    println!(
        "routing changes: JSON {} · queue {}",
        preview["json_routing_changes"]
            .as_array()
            .map_or(0, Vec::len),
        preview["queue_routing_changes"]
            .as_array()
            .map_or(0, Vec::len)
    );
}

fn print_reparent_edges(edges: Option<&Vec<Value>>) {
    if let Some(edges) = edges {
        for edge in edges {
            println!(
                "edge: {} {} -> {}",
                edge["session_id"].as_str().unwrap_or("unknown"),
                edge["expected_parent_session_id"]
                    .as_str()
                    .unwrap_or("root"),
                edge["new_parent_session_id"].as_str().unwrap_or("root")
            );
        }
    }
}

fn run_usage(client: &ApiClient, args: UsageArgs) -> Result<()> {
    if args.account && (args.agent.is_some() || args.include_children) {
        bail!("--account is mutually exclusive with AGENT and --include-children");
    }
    let current_session_id = optional_current_session_id();
    let account_view = usage_uses_account_view(&args, current_session_id.as_deref());
    if account_view && args.include_children {
        bail!("--include-children requires AGENT or a managed session context");
    }
    let mut query = Vec::new();
    if args.include_children {
        query.push("include_children=true");
    }
    if args.since_reset || !args.history {
        query.push("since_reset=true");
    }
    if args.by_model {
        query.push("by_model=true");
    }
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    let payload = if account_view {
        client.get_json(&format!("/usage/accounts{query}"))?
    } else {
        let target = match args.agent {
            Some(identifier) => lookup_identifier_exact(client, &identifier)?
                .ok_or_else(|| anyhow!("No agent named '{identifier}' is reachable."))?,
            None => current_session_id
                .ok_or_else(|| anyhow!("SESSION_MANAGER_ID is required to report status"))?,
        };
        client.get_json(&format!(
            "/sessions/{}/usage{query}",
            encode_path_segment(&target)
        ))?
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print_usage_report(&payload);
    }
    Ok(())
}

fn usage_uses_account_view(args: &UsageArgs, current_session_id: Option<&str>) -> bool {
    args.account || (args.agent.is_none() && current_session_id.is_none())
}

fn print_usage_decision(payload: &Value) {
    let decision = &payload["decision"];
    if let Some(banner) = usage_decision_banner(decision) {
        println!("{banner}");
    }
    for reason in decision["reasons"].as_array().into_iter().flatten() {
        if let Some(reason) = reason.as_str() {
            println!("  {reason}");
        }
    }
    for guidance in decision["refresh_guidance"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(guidance) = guidance.as_str() {
            println!("  refresh: {guidance}");
        }
    }
}

fn usage_decision_banner(decision: &Value) -> Option<&'static str> {
    match decision["status"].as_str().unwrap_or("non_actionable") {
        "actionable" => None,
        "partial" => Some("PARTIAL · some current quota meters are not decision-grade"),
        _ => Some("NON-ACTIONABLE · required quota picture is incomplete"),
    }
}

fn print_usage_report(payload: &Value) {
    if let Some(target) = payload.get("target").filter(|target| !target.is_null()) {
        let id = target["seat_id"].as_str().unwrap_or("unknown");
        let name = target["friendly_name"].as_str().unwrap_or(id);
        let descendants = target["descendant_count"].as_u64().unwrap_or(0);
        let available_descendants = target["available_descendant_count"]
            .as_u64()
            .unwrap_or(descendants);
        if descendants > 0 {
            println!("{name} ({id}) · {descendants} descendants · prior weights");
        } else if available_descendants > 0 {
            println!(
                "{name} ({id}) · {available_descendants} descendants excluded · prior weights"
            );
        } else {
            println!("{name} ({id}) · prior weights");
        }
    } else {
        println!("account usage · prior weights");
    }
    print_usage_decision(payload);
    let accounts = payload["accounts"].as_array().cloned().unwrap_or_default();
    if accounts.is_empty() {
        println!("No usage data");
        return;
    }
    for account in accounts {
        let key = account["account_key"].as_str().unwrap_or("unknown");
        let plan = account["plan_tier"].as_str().unwrap_or("unknown tier");
        println!("\n{key} · {plan}");
        for window in account["windows"].as_array().into_iter().flatten() {
            let kind = usage_window_label(window);
            let account_usage = window["account_percent"].as_f64().map_or_else(
                || {
                    let last = window["last_known_percent"].as_f64().unwrap_or(0.0);
                    let age_minutes = window["sample_age_seconds"].as_i64().unwrap_or(0) / 60;
                    if window["current"].as_bool().unwrap_or(false) {
                        format!(">={last:.1}% ({age_minutes}m old sample)")
                    } else {
                        format!("unknown (last {last:.1}%, {age_minutes}m ago)")
                    }
                },
                |percent| format!("{percent:.1}%"),
            );
            let self_percent = window["self_percent"]
                .as_f64()
                .map_or_else(|| "unknown".to_owned(), |value| format!("{value:.1}%"));
            let child_percent = window["children_percent"]
                .as_f64()
                .map_or_else(|| "unknown".to_owned(), |value| format!("{value:.1}%"));
            let headroom = window["free_headroom_points"].as_f64().map_or_else(
                || {
                    window["free_headroom_upper_bound_points"]
                        .as_f64()
                        .map_or_else(|| "unknown".to_owned(), |value| format!("<={value:.1} pts"))
                },
                |value| format!("{value:.1} pts"),
            );
            let freshness = if window["freshness"].as_str() == Some("stale") {
                " · bounds only"
            } else if window["freshness"].as_str() == Some("expired") {
                " · closed"
            } else {
                ""
            };
            let binding = if window["binding_for_scoped_seats"]
                .as_bool()
                .unwrap_or(false)
            {
                " · binding for scoped seats"
            } else {
                ""
            };
            if payload
                .get("target")
                .is_some_and(|target| !target.is_null())
            {
                println!(
                    "  {kind:<22} self {self_percent:>7} · children {child_percent:>7} · account {account_usage} · free {headroom}{binding}{freshness}"
                );
            } else {
                println!(
                    "  {kind:<22} account {account_usage} · free {headroom}{binding}{freshness}"
                );
            }
            let seats = window["seats"].as_array().cloned().unwrap_or_default();
            for seat in seats {
                let id = seat["seat_id"].as_str().unwrap_or("unknown");
                let name = seat["friendly_name"].as_str().unwrap_or(id);
                let identity = if name == id {
                    id.to_owned()
                } else {
                    format!("{name} ({id})")
                };
                let burn = seat["burn_percent"].as_f64().map_or_else(
                    || "burn unknown".to_owned(),
                    |value| format!("burn {value:.1}%"),
                );
                let tokens = format_usage_tokens(seat["total_tokens"].as_i64().unwrap_or(0));
                println!("    seat {identity} · {burn} · {tokens} tokens");
            }
            let credit_tokens = window["credit_tokens"].as_i64().unwrap_or(0);
            if credit_tokens > 0 {
                println!("    paid usage: {credit_tokens} tokens");
            }
            if let (Some(cap_fraction), Some(consumed)) = (
                payload["target"]["usage_cap_fraction"].as_f64(),
                window["cap_consumed_percent"].as_f64(),
            ) {
                println!(
                    "    cap {:.1}% of week · {:.1}% consumed",
                    cap_fraction * 100.0,
                    consumed
                );
            }
            for model in window["models"].as_array().into_iter().flatten() {
                let burn = model["burn_percent"]
                    .as_f64()
                    .map_or_else(|| "unknown".to_owned(), |value| format!("{value:.1}%"));
                println!(
                    "    {} · {} · {burn} · {}",
                    model["seat_id"].as_str().unwrap_or("unknown"),
                    model["model"].as_str().unwrap_or("unknown"),
                    model["weight_source"].as_str().unwrap_or("prior"),
                );
            }
            if let Some(projection) = window.get("projection").filter(|value| !value.is_null()) {
                let days = projection["horizon_seconds"].as_i64().unwrap_or(0) as f64 / 86_400.0;
                let rate = projection["burn_rate_points_per_day"]
                    .as_f64()
                    .unwrap_or(0.0);
                let projected = projection["projected_account_percent_at_reset"]
                    .as_f64()
                    .unwrap_or(0.0);
                let projected_free = projection["projected_free_headroom_points"]
                    .as_f64()
                    .unwrap_or(0.0);
                println!(
                    "    projection (low confidence): {rate:.1} pts/day · {projected:.1}% at reset in {days:.1}d · projected free {projected_free:.1} pts"
                );
                let additional_seats = projection["additional_seats"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if additional_seats.is_empty() {
                    println!("      additional seats unavailable · no observed model baseline");
                }
                for seat in additional_seats {
                    let model = seat["model"].as_str().unwrap_or("unknown");
                    let baseline = seat["baseline_seats"].as_u64().unwrap_or(0);
                    let equivalents = seat["additional_seat_equivalents"].as_f64().unwrap_or(0.0);
                    println!(
                        "      {model}: {equivalents:.1} additional seat equivalents · conservative baseline {baseline}"
                    );
                }
            }
            let residual_lower = window["residual_lower_bound_percent"]
                .as_f64()
                .unwrap_or(0.0);
            let residual_upper = window["residual_upper_bound_percent"]
                .as_f64()
                .unwrap_or(window["last_known_percent"].as_f64().unwrap_or(0.0));
            println!(
                "    residual: unknown ({residual_lower:.1}..{residual_upper:.1} pts) · seat burn may be inflated"
            );
        }
    }
    for warning in payload["warnings"].as_array().into_iter().flatten() {
        if let Some(warning) = warning.as_str() {
            println!("warning: {warning}");
        }
    }
}

fn format_usage_tokens(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn usage_window_label(window: &Value) -> String {
    let kind = window["window_kind"].as_str().unwrap_or("unknown");
    match window["window_scope"].as_str() {
        Some(scope) => format!("{kind} ({scope})"),
        None => kind.to_owned(),
    }
}

fn run_remind(client: &ApiClient, args: RemindArgs) -> Result<()> {
    if args.delay_or_action == "cancel" {
        if args.recurring || args.message_or_id.len() != 1 {
            bail!("usage: sm remind cancel <reminder-id>");
        }
        let reminder_id = &args.message_or_id[0];
        let payload = client.delete_json(
            &format!("/scheduler/remind/{}", encode_path_segment(reminder_id)),
            json!({}),
        )?;
        println!(
            "Reminder cancelled ({}) for {}",
            payload["reminder_id"].as_str().unwrap_or(reminder_id),
            payload["session_id"].as_str().unwrap_or("unknown")
        );
        return Ok(());
    }

    let delay_seconds = args.delay_or_action.parse::<u64>().with_context(|| {
        format!(
            "Expected integer delay (seconds), got: {:?}",
            args.delay_or_action
        )
    })?;
    if delay_seconds == 0 {
        bail!("Reminder delay must be greater than zero");
    }
    let session_id = current_session_id()?;
    let message = if args.message_or_id.is_empty() {
        "Reminder".to_owned()
    } else {
        args.message_or_id.join(" ")
    };
    let mut path = format!(
        "/scheduler/remind?session_id={}&delay_seconds={delay_seconds}&message={}",
        encode_query_component(&session_id),
        encode_query_component(&message)
    );
    if args.recurring {
        path.push_str(&format!("&recurring_interval_seconds={delay_seconds}"));
    }
    let payload = client.post_json(&path, json!({}))?;
    let reminder_id = payload["reminder_id"].as_str().unwrap_or("unknown");
    if args.recurring {
        println!(
            "Recurring reminder scheduled ({reminder_id}): every {}",
            format_reminder_delay(delay_seconds)
        );
    } else {
        println!(
            "Reminder scheduled ({reminder_id}): fires in {}",
            format_reminder_delay(delay_seconds)
        );
    }
    Ok(())
}

fn format_reminder_delay(seconds: u64) -> String {
    if seconds % 3600 == 0 {
        format!("{}h", seconds / 3600)
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
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

fn send_registered_email_fallback(client: &ApiClient, recipient: &str, text: &str) -> Result<()> {
    let requester_session_id = optional_current_session_id()
        .ok_or_else(|| anyhow!("Managed sender session is required for email fallback"))?;
    let human_response = client.request(
        "GET",
        &format!("/humans/{}", encode_path_segment(recipient)),
        None,
    )?;
    if (200..300).contains(&human_response.status) {
        let human = human_response.into_json()?;
        let canonical = human["recipient"].as_str().unwrap_or(recipient);
        let payload = client.post_json(
            &format!("/humans/{}/email", encode_path_segment(canonical)),
            json!({
                "requester_session_id": requester_session_id,
                "text": text,
                "auto_subject": true
            }),
        )?;
        println!(
            "Email sent to {}",
            payload["recipient"].as_str().unwrap_or(canonical)
        );
        return Ok(());
    }
    if human_response.status != 404 {
        return Err(human_response.into_status_error());
    }
    let payload = client.post_json(
        "/email/send",
        json!({
            "requester_session_id": requester_session_id,
            "recipients": [recipient],
            "cc": [],
            "body_text": text,
            "auto_subject": true
        }),
    )?;
    let recipient_summary = payload["to"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["username"].as_str().or_else(|| item["email"].as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| recipient.to_owned());
    println!("Email sent to {recipient_summary}");
    Ok(())
}

fn launch_provider_for_alias(alias: &str) -> Result<&'static str> {
    match alias {
        "new" | "claude" => Ok("claude"),
        "codex-original" | "codex-stock" => Ok("codex"),
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

fn rename_session(client: &ApiClient, args: NameArgs) -> Result<()> {
    let requester_session_id = current_session_id()?;
    let (target_session_id, friendly_name) = match args.new_name {
        None => (requester_session_id.clone(), args.name_or_session),
        Some(new_name) => {
            let target_identifier = args.name_or_session;
            let Some((target_session_id, target_session)) =
                resolve_name_target(client, &target_identifier)?
            else {
                bail!("Session '{target_identifier}' not found");
            };
            let parent_id = target_session["parent_session_id"].as_str();
            if !can_rename_target(&requester_session_id, &target_session_id, parent_id) {
                bail!(
                    "Not authorized. You can only rename your child sessions.\nTarget session parent: {}",
                    parent_id.unwrap_or("none")
                );
            }
            (target_session_id, new_name)
        }
    };
    validate_friendly_name(&friendly_name)?;
    let payload = client.patch_json(
        &format!("/sessions/{}", encode_path_segment(&target_session_id)),
        json!({ "friendly_name": friendly_name }),
    )?;
    if let Some(error) = payload["error"]
        .as_str()
        .or_else(|| payload["detail"].as_str())
    {
        bail!("{error}");
    }
    let session_id = payload["id"].as_str().unwrap_or(&target_session_id);
    let name = payload["friendly_name"].as_str().unwrap_or(&friendly_name);
    println!("Name set: {name} ({session_id})");
    Ok(())
}

fn can_rename_target(
    requester_session_id: &str,
    target_session_id: &str,
    target_parent_session_id: Option<&str>,
) -> bool {
    target_session_id == requester_session_id
        || target_parent_session_id == Some(requester_session_id)
}

fn resolve_name_target(client: &ApiClient, identifier: &str) -> Result<Option<(String, Value)>> {
    let session_path = format!("/sessions/{}", encode_path_segment(identifier));
    let response = client.request("GET", &session_path, None)?;
    if (200..300).contains(&response.status) {
        let payload = response.into_json()?;
        let session_id = payload["id"].as_str().unwrap_or(identifier).to_owned();
        return Ok(Some((session_id, payload)));
    }
    if response.status != 404 {
        return Err(response.into_status_error());
    }

    let payload = client.get_json("/sessions")?;
    let sessions = payload["sessions"].as_array().cloned().unwrap_or_default();
    Ok(resolve_exact_session_identifier_from_sessions(
        identifier, &sessions,
    ))
}

fn resolve_exact_session_identifier_from_sessions(
    identifier: &str,
    sessions: &[Value],
) -> Option<(String, Value)> {
    sessions
        .iter()
        .find(|session| {
            session["aliases"].as_array().is_some_and(|aliases| {
                aliases
                    .iter()
                    .any(|alias| alias.as_str() == Some(identifier))
            })
        })
        .and_then(|session| {
            session["id"]
                .as_str()
                .map(|session_id| (session_id.to_owned(), session.clone()))
        })
        .or_else(|| {
            sessions
                .iter()
                .find(|session| session["friendly_name"].as_str() == Some(identifier))
                .and_then(|session| {
                    session["id"]
                        .as_str()
                        .map(|session_id| (session_id.to_owned(), session.clone()))
                })
        })
}

fn validate_friendly_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Name cannot be empty");
    }
    if name.chars().count() > 32 {
        bail!("Name too long (max 32 chars)");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        bail!("Name must be alphanumeric with - or _ only (no spaces)");
    }
    Ok(())
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

fn run_context(client: &ApiClient, args: ContextArgs) -> Result<()> {
    let target = match args.session_id {
        Some(session_id) => session_id,
        None => optional_current_session_id().ok_or_else(|| {
            anyhow!("sm context requires a managed session or explicit session target")
        })?,
    };
    let payload = client.get_json(&session_context_path(&target))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let percentage = format_context_percentage(payload.get("used_percentage"));
    if !(args.details || args.detailed) {
        println!(
            "{}",
            format_compact_context(
                &percentage,
                payload["sampled_at"].as_str(),
                payload["lifecycle_status"].as_str(),
                OffsetDateTime::now_utc(),
            )
        );
        return Ok(());
    }

    let token_text = payload
        .get("total_input_tokens")
        .and_then(Value::as_i64)
        .map(|tokens| format!(" ({} tokens)", format_int(tokens)))
        .unwrap_or_default();
    let session_id = payload["session_id"].as_str().unwrap_or(&target);
    let label = payload["friendly_name"].as_str().unwrap_or(session_id);
    let provider = payload["provider"].as_str().unwrap_or("-");
    let state = payload["state"].as_str().unwrap_or("unknown");
    let levels = payload["threshold_percentages"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .map(|value| format_context_percentage(Some(value)))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| {
            let warning = format_context_percentage(payload.get("warning_percentage"));
            let critical = format_context_percentage(payload.get("critical_percentage"));
            format!("{warning}, {critical}")
        });
    let notify = payload["notify_session_id"].as_str();
    let notify_text = match notify {
        Some(value) if value == session_id => "self".to_owned(),
        Some(value) => value.to_owned(),
        None => "none".to_owned(),
    };
    let monitor = if payload["context_monitor_enforced"]
        .as_bool()
        .unwrap_or(false)
    {
        "enabled"
    } else if payload["context_monitor_enabled"]
        .as_bool()
        .unwrap_or(false)
    {
        "enabled but NOT enforced (invalid thresholds)"
    } else {
        "disabled"
    };
    let compaction = if payload["compaction_active"].as_bool().unwrap_or(false) {
        "active"
    } else {
        "not active"
    };

    println!("Context: {percentage}{token_text}");
    println!(
        "Sample: {} (cached telemetry, not liveness)",
        format_context_sample_age(payload["sampled_at"].as_str())
    );
    println!(
        "Lifecycle: {}",
        payload["lifecycle_status"].as_str().unwrap_or("unknown")
    );
    println!(
        "State: {state} (levels {levels}; {})",
        payload["threshold_source"].as_str().unwrap_or("unknown")
    );
    println!("Monitor: {monitor}, alerts -> {notify_text}");
    println!("Compaction: {compaction}");
    println!(
        "Last handoff: {}",
        payload["last_handoff_path"].as_str().unwrap_or("-")
    );
    println!("Session: {label} [{session_id}] {provider}");
    Ok(())
}

fn session_context_path(target: &str) -> String {
    format!("/sessions/{}/context", encode_path_segment(target))
}

fn format_context_percentage(value: Option<&Value>) -> String {
    let Some(numeric) = value.and_then(Value::as_f64) else {
        return "unknown".to_owned();
    };
    if (numeric.fract()).abs() < f64::EPSILON {
        format!("{}%", numeric as i64)
    } else {
        let mut text = format!("{numeric:.1}");
        while text.contains('.') && text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
        format!("{text}%")
    }
}

fn format_context_sample_age(sampled_at: Option<&str>) -> String {
    format_context_sample_age_at(sampled_at, OffsetDateTime::now_utc())
}

fn format_compact_context(
    percentage: &str,
    sampled_at: Option<&str>,
    lifecycle_status: Option<&str>,
    now: OffsetDateTime,
) -> String {
    if percentage == "unknown" {
        return percentage.to_owned();
    }
    let stopped = lifecycle_status == Some("stopped");
    let sampled = sampled_at
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .map(|timestamp| ((now - timestamp).whole_seconds()).max(0));
    let stale = sampled.is_none_or(|seconds| seconds >= CONTEXT_COMPACT_STALE_SECONDS);
    // Preserve the terse percentage for fresh live telemetry, but make cached
    // or terminal snapshots impossible to mistake for a heartbeat.
    if !stopped && !stale {
        return percentage.to_owned();
    }

    let mut notes = Vec::new();
    if stale {
        let age = format_context_sample_age_at(sampled_at, now);
        notes.push(if age == "unknown" {
            "cached sample age unknown".to_owned()
        } else {
            format!("cached {age}")
        });
    } else {
        notes.push("cached".to_owned());
    }
    if stopped {
        notes.push("session stopped".to_owned());
    }
    format!("{percentage} ({})", notes.join("; "))
}

fn format_context_sample_age_at(sampled_at: Option<&str>, now: OffsetDateTime) -> String {
    let Some(sampled_at) = sampled_at else {
        return "unknown".to_owned();
    };
    let Ok(timestamp) = OffsetDateTime::parse(sampled_at, &Rfc3339) else {
        return "unknown".to_owned();
    };
    format_elapsed_context_age((now - timestamp).whole_seconds())
}

fn format_elapsed_context_age(mut seconds: i64) -> String {
    if seconds < 0 {
        seconds = 0;
    }
    if seconds < 60 {
        return format!("{seconds}s ago");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}min ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}hr ago");
    }
    let days = hours / 24;
    if days == 1 {
        "1day ago".to_owned()
    } else {
        format!("{days}days ago")
    }
}

fn format_int(value: i64) -> String {
    let raw = value.to_string();
    let mut output = String::new();
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            output.push(',');
        }
        output.push(ch);
    }
    output.chars().rev().collect()
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
            println!(
                "{:<12} {:<24} {:<14} Thresholds",
                "Session", "Name", "Notify Target"
            );
            println!("{}", "-".repeat(78));
            for entry in monitored {
                let session_id = entry["session_id"].as_str().unwrap_or("unknown");
                let name = entry["friendly_name"].as_str().unwrap_or("");
                let notify = entry["notify_session_id"].as_str().unwrap_or("(none)");
                println!(
                    "{session_id:<12} {name:<24} {notify:<14} {}",
                    format_context_monitor_thresholds(&entry)
                );
            }
        }
        ContextMonitorCommand::Enable {
            target,
            threshold,
            critical_threshold,
            use_default_thresholds,
        } => {
            let requester = current_session_id()?;
            let target = target.unwrap_or_else(|| requester.clone());
            let response = client.post_json(
                &format!("/sessions/{target}/context-monitor"),
                json!({
                    "enabled": true,
                    "requester_session_id": requester,
                    "notify_session_id": requester,
                    "threshold_percentages": (!threshold.is_empty()).then_some(threshold),
                    "warning_percentage": null,
                    "critical_percentage": critical_threshold,
                    "use_default_thresholds": use_default_thresholds
                }),
            )?;
            ensure_context_monitor_enabled(&response)?;
            if target == requester {
                println!(
                    "Context monitoring enabled - notifications -> self ({requester}); thresholds {}",
                    format_context_monitor_thresholds(&response)
                );
            } else {
                println!(
                    "Context monitoring enabled for {target} - notifications -> {requester}; thresholds {}",
                    format_context_monitor_thresholds(&response)
                );
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

fn ensure_context_monitor_enabled(response: &Value) -> Result<()> {
    if response["enabled"].as_bool() == Some(true) {
        return Ok(());
    }
    bail!(
        "Context monitoring was not enrolled: server returned enabled={:?}. Run `sm context-monitor status` to verify coverage.",
        response["enabled"]
    );
}

fn format_context_monitor_thresholds(payload: &Value) -> String {
    if payload["enforced"].as_bool() != Some(true) {
        return "INVALID / NOT ENFORCED".to_owned();
    }
    let source = payload["threshold_source"].as_str().unwrap_or("unknown");
    let levels = payload["threshold_percentages"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .map(|value| format_context_percentage(Some(value)))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| {
            let warning = format_context_percentage(payload.get("warning_percentage"));
            let critical = format_context_percentage(payload.get("critical_percentage"));
            format!("{warning}, {critical}")
        });
    format!("{levels} ({source})")
}

fn lookup_identifier(client: &ApiClient, identifier: &str) -> Result<Option<String>> {
    if let Some(session_id) = lookup_identifier_exact(client, identifier)? {
        return Ok(Some(session_id));
    }

    let payload = client.get_json("/sessions")?;
    let sessions = payload["sessions"].as_array().cloned().unwrap_or_default();
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

fn lookup_identifier_exact(client: &ApiClient, identifier: &str) -> Result<Option<String>> {
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

    let prefix_matches = sessions
        .iter()
        .filter(|session| {
            session["id"]
                .as_str()
                .is_some_and(|session_id| session_id.starts_with(identifier))
        })
        .cloned()
        .collect::<Vec<_>>();
    match prefix_matches.len() {
        1 => Ok(prefix_matches[0]["id"].as_str().map(ToOwned::to_owned)),
        count if count > 1 => bail_ambiguous_lookup(identifier, &prefix_matches),
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
    let humans_payload = client.get_json("/humans")?;
    let humans = humans_payload["humans"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if registrations.is_empty() && humans.is_empty() {
        println!("No registered roles or humans.");
        return Ok(());
    }

    if !registrations.is_empty() {
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
    }

    if !humans.is_empty() {
        if !registrations.is_empty() {
            println!();
        }
        println!("Humans");
        let rows = humans.iter().map(human_roster_row).collect::<Vec<_>>();
        print_table(
            &["Name", "Display", "Aliases", "Default", "Channels"],
            &rows,
        );
    }
    Ok(())
}

fn json_string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or("").to_owned()
}

fn lookup_human(client: &ApiClient, identifier: &str) -> Result<Option<Value>> {
    let response = client.request(
        "GET",
        &format!("/humans/{}", encode_path_segment(identifier)),
        None,
    )?;
    if (200..300).contains(&response.status) {
        return Ok(Some(response.into_json()?));
    }
    if response.status == 404 {
        return Ok(None);
    }
    Err(response.into_status_error())
}

fn print_human_lookup(payload: &Value) {
    let recipient = payload["recipient"].as_str().unwrap_or("<unknown>");
    let aliases = payload["aliases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|alias| *alias != recipient)
        .collect::<Vec<_>>();
    println!("Human recipient: {recipient}");
    if !aliases.is_empty() {
        println!("Aliases: {}", aliases.join(", "));
    }
    println!(
        "Default delivery: {}",
        payload["default_channel"].as_str().unwrap_or("unknown")
    );
    let channels = payload["available_channels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if !channels.is_empty() {
        println!("Available delivery: {}", channels.join(", "));
    }
    if channels.contains(&"telegram") {
        println!("Telegram delivery posts into the sending agent's SM-managed Telegram thread.");
    }
    if channels.contains(&"email") {
        println!("Email is available as fallback/explicit only; use email sparingly.");
    }
}

fn human_roster_row(entry: &Value) -> Vec<String> {
    let recipient = json_string(entry, "recipient");
    let aliases = entry["aliases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|alias| *alias != recipient)
        .collect::<Vec<_>>()
        .join(",");
    let channels = entry["available_channels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(",");
    vec![
        recipient,
        json_string(entry, "display_name"),
        aliases,
        json_string(entry, "default_channel"),
        channels,
    ]
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
    let mut line = format!("{name} ({id}) | {provider} | {status}");
    if let Some(percent) = child["weekly_usage_percent"].as_f64() {
        line.push_str(&format!(" | {percent:.1}% wk"));
    } else if child.get("weekly_usage_percent").is_some() {
        line.push_str(" | unknown wk");
    }
    line
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

    fn get_json_with_session_credential(
        &self,
        path: &str,
        session_id: &str,
        credential: &str,
    ) -> Result<Value> {
        self.request_with_headers(
            "GET",
            path,
            None,
            &[
                ("X-SM-Session-ID", session_id),
                ("X-SM-Session-Credential", credential),
            ],
        )?
        .into_json()
    }

    fn post_json(&self, path: &str, body: Value) -> Result<Value> {
        let response = self.request("POST", path, Some(body))?;
        response.into_json()
    }

    fn post_json_with_session_credential(
        &self,
        path: &str,
        body: Value,
        credential: &str,
    ) -> Result<Value> {
        let response = self.request_with_headers(
            "POST",
            path,
            Some(body),
            &[("X-SM-Session-Credential", credential)],
        )?;
        response.into_json()
    }

    fn put_json(&self, path: &str, body: Value) -> Result<Value> {
        let response = self.request("PUT", path, Some(body))?;
        response.into_json()
    }

    fn patch_json(&self, path: &str, body: Value) -> Result<Value> {
        let response = self.request("PATCH", path, Some(body))?;
        response.into_json()
    }

    fn delete_json(&self, path: &str, body: Value) -> Result<Value> {
        let response = self.request("DELETE", path, Some(body))?;
        response.into_json()
    }

    fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<ApiResponse> {
        self.request_with_headers(method, path, body, &[])
    }

    fn request_with_headers(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Result<ApiResponse> {
        if self.scheme == "https" {
            return self.request_https(method, path, body, headers);
        }
        let body_bytes = match body {
            Some(value) => serde_json::to_vec(&value)?,
            None => Vec::new(),
        };
        let full_path = format!("{}{}", self.path_prefix, path);
        let mut stream = TcpStream::connect((self.host.as_str(), self.port))
            .with_context(|| format!("failed to connect to {}", self.authority))?;
        let mut request = format!(
            "{method} {full_path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.authority, body_bytes.len()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes())?;
        stream.write_all(&body_bytes)?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        parse_response(&raw)
    }

    fn request_https(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Result<ApiResponse> {
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
            "GET" => {
                let mut request = agent
                    .get(&url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                for (name, value) in headers {
                    request = request.header(*name, *value);
                }
                request.call()
            }
            "POST" => {
                let mut request = agent
                    .post(&url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                for (name, value) in headers {
                    request = request.header(*name, *value);
                }
                request.send(body_bytes.as_slice())
            }
            "PUT" => {
                let mut request = agent
                    .put(&url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                for (name, value) in headers {
                    request = request.header(*name, *value);
                }
                request.send(body_bytes.as_slice())
            }
            "PATCH" => {
                let mut request = agent
                    .patch(&url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                for (name, value) in headers {
                    request = request.header(*name, *value);
                }
                request.send(body_bytes.as_slice())
            }
            "DELETE" => {
                let mut request = agent
                    .delete(&url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json")
                    .force_send_body();
                for (name, value) in headers {
                    request = request.header(*name, *value);
                }
                request.send(body_bytes.as_slice())
            }
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

fn parse_queue_timeout_seconds(value: &str) -> Result<i64> {
    if value.eq_ignore_ascii_case("none") {
        return Ok(0);
    }
    parse_duration_seconds(value)
}

fn parse_queue_log_lines(value: &str) -> std::result::Result<usize, String> {
    let lines = value
        .parse::<usize>()
        .map_err(|_| "lines must be an integer between 1 and 10000".to_owned())?;
    if !(1..=10_000).contains(&lines) {
        return Err("lines must be between 1 and 10000".to_owned());
    }
    Ok(lines)
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
    if let Some(message) = retired_command_message(&command) {
        eprintln!("{message}");
        process::exit(2);
    }
}

fn retired_command_message(command: &[&str]) -> Option<&'static str> {
    match command {
        ["kill", ..] => Some("removed: use sm retire"),
        ["dispatch", ..] => Some("removed: dispatch is not part of the Rust cutover scope"),
        ["watch-job", ..] => Some(
            "removed: watch-job is not available; start supervised commands with `sm queue run \
             --notify <session> -- <command>` instead. Queue jobs automatically send an \
             `[sm queue]` completion wake. Existing external processes cannot be registered.",
        ),
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
        io::{BufRead, BufReader},
        net::TcpListener,
        sync::Mutex,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn start_lookup_server<const N: usize>(
        responses: [(&'static str, u16, &'static str); N],
    ) -> (ApiClient, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut paths = Vec::new();
            for (expected_path, status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut buffer = [0_u8; 4096];
                let bytes_read = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..bytes_read]);
                let request_line = request.lines().next().unwrap_or_default();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();
                assert_eq!(method, "GET");
                assert_eq!(path, expected_path);
                paths.push(path.to_owned());
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
            paths
        });
        let client = ApiClient::parse(&format!("http://{address}")).unwrap();
        (client, server)
    }

    fn single_request_server(
        status: u16,
        body: &'static str,
    ) -> (ApiClient, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0_u8; 8192];
            let bytes_read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            vec![request]
        });
        let client = ApiClient::parse(&format!("http://{address}")).unwrap();
        (client, server)
    }

    fn read_test_request(stream: &mut std::net::TcpStream) -> (String, String, String) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let mut content_length = 0_usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap();
                }
            }
        }
        let mut body = vec![0_u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).unwrap();
        }
        (method, path, String::from_utf8(body).unwrap())
    }

    #[test]
    fn tail_parser_restores_legacy_defaults_and_short_line_flag() {
        let default_cli = Cli::try_parse_from(["sm", "tail", "worker"]).unwrap();
        let Command::Tail(default_args) = default_cli.command else {
            panic!("expected tail command");
        };
        assert_eq!(default_args.session_id, "worker");
        assert_eq!(default_args.lines, 10);
        assert!(!default_args.raw);

        let raw_cli = Cli::try_parse_from(["sm", "tail", "worker", "--raw", "-n", "7"]).unwrap();
        let Command::Tail(raw_args) = raw_cli.command else {
            panic!("expected tail command");
        };
        assert_eq!(raw_args.lines, 7);
        assert!(raw_args.raw);
    }

    #[test]
    fn remind_parser_restores_recurring_and_cancel_forms() {
        let recurring =
            Cli::try_parse_from(["sm", "remind", "--recurring", "1020", "Inspect", "queues"])
                .unwrap();
        let Command::Remind(recurring) = recurring.command else {
            panic!("expected remind command");
        };
        assert!(recurring.recurring);
        assert_eq!(recurring.delay_or_action, "1020");
        assert_eq!(recurring.message_or_id, ["Inspect", "queues"]);

        let cancel = Cli::try_parse_from(["sm", "remind", "cancel", "abc123"]).unwrap();
        let Command::Remind(cancel) = cancel.command else {
            panic!("expected remind command");
        };
        assert!(!cancel.recurring);
        assert_eq!(cancel.delay_or_action, "cancel");
        assert_eq!(cancel.message_or_id, ["abc123"]);
        assert_eq!(retired_command_message(&["remind"]), None);
    }

    #[test]
    fn watch_job_retirement_names_the_automatic_queue_replacement() {
        let message = retired_command_message(&["watch-job", "add", "--help"]).unwrap();

        assert!(message.contains("sm queue run"));
        assert!(message.contains("automatically send"));
        assert!(message.contains("[sm queue]"));
        assert!(message.contains("external processes cannot be registered"));
    }

    #[test]
    fn usage_and_children_usage_flags_parse_the_surface_contract() {
        let cli = Cli::try_parse_from([
            "sm",
            "usage",
            "worker",
            "--include-children",
            "--since-reset",
            "--by-model",
            "--json",
        ])
        .unwrap();
        let Command::Usage(args) = cli.command else {
            panic!("expected usage command");
        };
        assert_eq!(args.agent.as_deref(), Some("worker"));
        assert!(args.include_children);
        assert!(args.since_reset);
        assert!(!args.history);
        assert!(!args.account);
        assert!(args.by_model);
        assert!(args.json);

        let account = Cli::try_parse_from(["sm", "usage", "--account"]).unwrap();
        let Command::Usage(account) = account.command else {
            panic!("expected account usage command");
        };
        assert!(account.account);
        assert!(account.agent.is_none());

        let history = Cli::try_parse_from(["sm", "usage", "worker", "--history"]).unwrap();
        let Command::Usage(history) = history.command else {
            panic!("expected usage command");
        };
        assert!(history.history);
        assert!(!history.since_reset);
        assert!(Cli::try_parse_from(["sm", "usage", "--history", "--since-reset"]).is_err());

        let children = Cli::try_parse_from(["sm", "children", "root", "--usage"]).unwrap();
        let Command::Children(children) = children.command else {
            panic!("expected children command");
        };
        assert!(children.usage);
        assert_eq!(children.session_id.as_deref(), Some("root"));
    }

    #[test]
    fn usage_defaults_to_accounts_only_without_a_managed_or_explicit_seat() {
        let args = UsageArgs {
            agent: None,
            include_children: false,
            since_reset: false,
            history: false,
            account: false,
            by_model: false,
            json: false,
        };
        assert!(usage_uses_account_view(&args, None));
        assert!(!usage_uses_account_view(&args, Some("managed-seat")));

        let explicit = UsageArgs {
            agent: Some("worker".to_owned()),
            ..args
        };
        assert!(!usage_uses_account_view(&explicit, None));
    }

    #[test]
    fn unmanaged_bare_usage_requests_the_account_endpoint() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SESSION_MANAGER_ID", "CLAUDE_SESSION_MANAGER_ID"]);
        env::remove_var("SESSION_MANAGER_ID");
        env::remove_var("CLAUDE_SESSION_MANAGER_ID");
        let (client, server) = start_lookup_server([(
            "/usage/accounts?since_reset=true",
            200,
            r#"{"mode":"prior","accounts":[],"warnings":[],"residual":null}"#,
        )]);
        let args = UsageArgs {
            agent: None,
            include_children: false,
            since_reset: false,
            history: false,
            account: false,
            by_model: false,
            json: false,
        };

        run_usage(&client, args).unwrap();

        assert_eq!(server.join().unwrap(), ["/usage/accounts?since_reset=true"]);
    }

    #[test]
    fn usage_history_explicitly_requests_closed_windows() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SESSION_MANAGER_ID", "CLAUDE_SESSION_MANAGER_ID"]);
        env::remove_var("SESSION_MANAGER_ID");
        env::remove_var("CLAUDE_SESSION_MANAGER_ID");
        let (client, server) = start_lookup_server([(
            "/usage/accounts",
            200,
            r#"{"mode":"prior","accounts":[],"warnings":[],"residual":null}"#,
        )]);
        let args = UsageArgs {
            agent: None,
            include_children: false,
            since_reset: false,
            history: true,
            account: false,
            by_model: false,
            json: false,
        };

        run_usage(&client, args).unwrap();

        assert_eq!(server.join().unwrap(), ["/usage/accounts"]);
    }

    #[test]
    fn usage_token_totals_use_compact_readable_units() {
        assert_eq!(format_usage_tokens(999), "999");
        assert_eq!(format_usage_tokens(12_345), "12.3k");
        assert_eq!(format_usage_tokens(2_345_678), "2.3m");
    }

    #[test]
    fn non_actionable_banner_does_not_claim_fresh_partial_data_is_absent() {
        let decision = json!({
            "status": "non_actionable",
            "fresh_current_windows": 1,
            "missing_current_windows": ["codex:a:codex_10080"]
        });

        assert_eq!(
            usage_decision_banner(&decision),
            Some("NON-ACTIONABLE · required quota picture is incomplete")
        );
    }

    #[test]
    fn child_usage_column_formats_known_and_unknown_weekly_burn() {
        let base = json!({
            "id": "abc12345",
            "friendly_name": "worker",
            "provider": "codex-fork",
            "status": "running"
        });
        let mut known = base.clone();
        known["weekly_usage_percent"] = json!(4.25);
        assert_eq!(
            format_child_line(&known),
            "worker (abc12345) | codex-fork | running | 4.2% wk"
        );

        let mut unknown = base;
        unknown["weekly_usage_percent"] = Value::Null;
        assert_eq!(
            format_child_line(&unknown),
            "worker (abc12345) | codex-fork | running | unknown wk"
        );
    }

    #[test]
    fn retire_payload_carries_managed_identity_and_surfaces_errors() {
        assert_eq!(
            retire_request_payload(Some("parent001".to_owned())),
            json!({ "requester_session_id": "parent001" })
        );
        assert_eq!(
            retire_request_payload(None),
            json!({ "requester_session_id": null })
        );
        assert_eq!(
            retire_response_status(&json!({ "status": "killed" }), "child001").unwrap(),
            "retired"
        );
        assert_eq!(
            retire_response_status(&json!({ "status": "retired" }), "child001").unwrap(),
            "retired"
        );
        assert_eq!(
            retire_response_status(
                &json!({ "error": "Cannot retire session child001 - not your child session" }),
                "child001"
            )
            .unwrap_err()
            .to_string(),
            "Cannot retire session child001 - not your child session"
        );
        assert_eq!(
            retire_response_status(&json!({}), "child001")
                .unwrap_err()
                .to_string(),
            "Invalid retire response for session child001"
        );
    }

    #[test]
    fn compact_context_marks_stale_or_stopped_samples_as_cached() {
        let now = OffsetDateTime::parse("2026-08-16T23:00:00Z", &Rfc3339).unwrap();

        assert_eq!(
            format_compact_context("43%", Some("2026-08-16T22:59:30Z"), Some("running"), now,),
            "43%"
        );
        assert_eq!(
            format_compact_context("38%", Some("2026-08-16T22:45:00Z"), Some("running"), now,),
            "38% (cached 15min ago)"
        );
        assert_eq!(
            format_compact_context("38%", Some("2026-08-16T22:59:30Z"), Some("stopped"), now,),
            "38% (cached; session stopped)"
        );
        assert_eq!(
            format_compact_context("unknown", None, Some("stopped"), now),
            "unknown"
        );
    }

    #[test]
    fn watch_cli_preserves_python_dashboard_flags() {
        let cli = Cli::try_parse_from([
            "sm",
            "watch",
            "--repo",
            "/tmp/project",
            "--role",
            "engineer",
            "--interval",
            "3.5",
            "--restore",
            "--top-level",
            "--sort",
            "last-active",
            "--node",
            "studio",
            "--all-nodes",
        ])
        .unwrap();
        let Command::Watch(args) = cli.command else {
            panic!("expected watch command");
        };

        assert_eq!(args.repo.as_deref(), Some("/tmp/project"));
        assert_eq!(args.role.as_deref(), Some("engineer"));
        assert_eq!(args.interval, 3.5);
        assert!(args.restore);
        assert!(args.top_level);
        assert_eq!(args.sort, "last-active");
        assert_eq!(args.node.as_deref(), Some("studio"));
        assert!(args.all_nodes);
        assert_eq!(
            watch_python_args(&args),
            vec![
                "-m",
                "src.cli.main",
                "watch",
                "--repo",
                "/tmp/project",
                "--role",
                "engineer",
                "--interval",
                "3.5",
                "--restore",
                "--top-level",
                "--sort",
                "last-active",
                "--node",
                "studio",
                "--all-nodes",
            ]
        );
    }

    #[test]
    fn watch_rejects_managed_session_before_launch() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SESSION_MANAGER_ID", "CLAUDE_SESSION_MANAGER_ID"]);
        env::set_var("SESSION_MANAGER_ID", "managed001");
        let args = WatchArgs {
            repo: None,
            role: None,
            interval: 2.0,
            restore: false,
            top_level: false,
            sort: "retired".to_owned(),
            node: None,
            all_nodes: false,
        };

        let error = run_watch(DEFAULT_API_URL, args).unwrap_err().to_string();

        assert!(error.contains("sm watch is operator-only"));
    }

    #[test]
    fn watch_delegates_through_internal_python_component() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&[
            "SESSION_MANAGER_ID",
            "CLAUDE_SESSION_MANAGER_ID",
            WATCH_PYTHON_ENV,
            WATCH_REPO_ROOT_ENV,
        ]);
        env::remove_var("SESSION_MANAGER_ID");
        env::remove_var("CLAUDE_SESSION_MANAGER_ID");
        env::set_var(WATCH_PYTHON_ENV, "/usr/bin/true");
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        env::set_var(WATCH_REPO_ROOT_ENV, repo_root);
        let args = WatchArgs {
            repo: None,
            role: None,
            interval: 2.0,
            restore: false,
            top_level: false,
            sort: "retired".to_owned(),
            node: None,
            all_nodes: false,
        };

        run_watch("http://127.0.0.1:8420", args).unwrap();
    }

    #[test]
    fn what_and_btw_parse_to_the_same_command() {
        for command in ["what", "btw"] {
            let cli =
                Cli::try_parse_from(["sm", command, "worker", "Summarize", "current", "work"])
                    .unwrap();
            let Command::What(args) = cli.command else {
                panic!("expected what command");
            };
            assert_eq!(args.session_id, "worker");
            assert_eq!(args.prompt.join(" "), "Summarize current work");
        }
        assert_eq!(retired_command_message(&["what", "worker"]), None);
    }

    #[test]
    fn reparent_cli_parses_request_tree_decision_repair_and_rollout_forms() {
        let request =
            Cli::try_parse_from(["sm", "reparent", "request", "child", "--to", "parent"]).unwrap();
        let Command::Reparent(request) = request.command else {
            panic!("expected reparent command");
        };
        assert!(matches!(
            request.command,
            ReparentCommand::Request { child, to } if child == "child" && to == "parent"
        ));

        let tree = Cli::try_parse_from([
            "sm",
            "reparent-tree",
            "source",
            "--to",
            "target",
            "--dry-run",
        ])
        .unwrap();
        let Command::ReparentTree(tree) = tree.command else {
            panic!("expected reparent-tree command");
        };
        assert_eq!(tree.source, "source");
        assert_eq!(tree.to, "target");
        assert!(tree.dry_run);

        assert!(matches!(
            Cli::try_parse_from(["sm", "reparent", "approve", "request1"])
                .unwrap()
                .command,
            Command::Reparent(ReparentArgs {
                command: ReparentCommand::Approve { .. }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "sm",
                "reparent",
                "repair",
                "request1",
                "--rollback-precommit",
            ])
            .unwrap()
            .command,
            Command::Reparent(ReparentArgs {
                command: ReparentCommand::Repair {
                    rollback_precommit: true,
                    ..
                }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["sm", "recredential", "--all-live"])
                .unwrap()
                .command,
            Command::Recredential(RecredentialArgs { all_live: true, .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["sm", "adopt", "child"])
                .unwrap()
                .command,
            Command::Adopt(AdoptArgs { child }) if child == "child"
        ));
    }

    #[test]
    fn session_credential_header_is_sent_without_entering_the_json_body() {
        let (client, handle) = single_request_server(200, r#"{"id":"request1"}"#);

        let response = client
            .post_json_with_session_credential(
                "/sessions/child/reparent-requests",
                json!({"requester_session_id": "parent"}),
                "opaque-secret",
            )
            .unwrap();

        assert_eq!(response["id"], "request1");
        let requests = handle.join().unwrap();
        assert!(requests[0].contains("X-SM-Session-Credential: opaque-secret\r\n"));
        assert!(!requests[0].contains("\"opaque-secret\""));
    }

    #[test]
    fn tail_default_uses_structured_tool_activity_endpoint() {
        let (client, server) = start_lookup_server([
            (
                "/sessions/worker",
                200,
                r#"{"id":"tail001","name":"codex-tail001","friendly_name":"worker","provider":"codex-fork"}"#,
            ),
            (
                "/sessions/tail001/tool-calls?limit=3",
                200,
                r#"{"session_id":"tail001","tool_calls":[{"timestamp":"2026-07-28T20:00:00Z","tool_name":"exec_command","hook_type":"CodexForkToolCall"}]}"#,
            ),
        ]);

        print_tail(&client, "worker", 3, false).unwrap();

        assert_eq!(
            server.join().unwrap(),
            vec!["/sessions/worker", "/sessions/tail001/tool-calls?limit=3"]
        );
    }

    #[test]
    fn tail_raw_requests_rendered_output_instead_of_pipe_pane_log() {
        let (client, server) = start_lookup_server([
            (
                "/sessions/worker",
                200,
                r#"{"id":"tail001","name":"codex-tail001","friendly_name":"worker","provider":"codex-fork"}"#,
            ),
            (
                "/sessions/tail001/output?lines=4&rendered=true",
                200,
                r#"{"session_id":"tail001","output":"readable output\n"}"#,
            ),
        ]);

        print_tail(&client, "worker", 4, true).unwrap();

        assert_eq!(
            server.join().unwrap(),
            vec![
                "/sessions/worker",
                "/sessions/tail001/output?lines=4&rendered=true"
            ]
        );
    }

    #[test]
    fn tail_terminal_cleanup_removes_csi_osc_and_control_bytes() {
        let raw = "\u{1b}[31mred\u{1b}[0m\n\u{1b}]0;title\u{7}plain\u{0}\ttext";
        assert_eq!(strip_terminal_controls(raw), "red\nplain\ttext");
    }

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
    fn api_client_https_supports_patch_requests() {
        let client = ApiClient::parse("https://127.0.0.1:1").unwrap();
        let error = match client.request(
            "PATCH",
            "/sessions/abc123",
            Some(json!({"friendly_name": "deskbar"})),
        ) {
            Ok(_) => panic!("expected connection failure"),
            Err(error) => error.to_string(),
        };

        assert!(!error.contains("unsupported HTTP method PATCH"));
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
    fn send_registered_email_fallback_posts_auto_subject_payload() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SESSION_MANAGER_ID", "CLAUDE_SESSION_MANAGER_ID"]);
        env::set_var("SESSION_MANAGER_ID", "sender001");

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let (method, path, _) = read_test_request(&mut stream);
            assert_eq!(method, "GET");
            assert_eq!(path, "/humans/teammate");
            let response_body = r#"{"detail":"Human recipient not configured"}"#;
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            drop(stream);

            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let (method, path, body) = read_test_request(&mut stream);
            assert_eq!(method, "POST");
            assert_eq!(path, "/email/send");
            let payload: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(payload["requester_session_id"], "sender001");
            assert_eq!(payload["recipients"], json!(["teammate"]));
            assert_eq!(payload["cc"], json!([]));
            assert_eq!(payload["body_text"], "hello via fallback");
            assert_eq!(payload["auto_subject"], true);

            let response_body =
                r#"{"to":[{"username":"teammate","email":"teammate@example.test"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let client = ApiClient::parse(&format!("http://{address}")).unwrap();

        send_registered_email_fallback(&client, "teammate", "hello via fallback").unwrap();

        server.join().unwrap();
    }

    #[test]
    fn send_registered_email_fallback_uses_human_email_endpoint() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::new(&["SESSION_MANAGER_ID", "CLAUDE_SESSION_MANAGER_ID"]);
        env::set_var("SESSION_MANAGER_ID", "sender001");

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (method, path, _) = read_test_request(&mut stream);
            assert_eq!(method, "GET");
            assert_eq!(path, "/humans/rajeshgoli");
            let response_body = r#"{"recipient":"rajesh"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            drop(stream);

            let (mut stream, _) = listener.accept().unwrap();
            let (method, path, body) = read_test_request(&mut stream);
            assert_eq!(method, "POST");
            assert_eq!(path, "/humans/rajesh/email");
            let payload: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(payload["requester_session_id"], "sender001");
            assert_eq!(payload["text"], "hello human fallback");
            assert_eq!(payload["auto_subject"], true);

            let response_body = r#"{"recipient":"rajesh","status":"sent"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            drop(stream);
        });
        let client = ApiClient::parse(&format!("http://{address}")).unwrap();

        send_registered_email_fallback(&client, "rajeshgoli", "hello human fallback").unwrap();

        server.join().unwrap();
    }

    #[test]
    fn lookup_human_resolves_configured_alias() {
        let (client, server) = start_lookup_server([(
            "/humans/rajeshgoli",
            200,
            r#"{"recipient":"rajesh","display_name":"Human operator","aliases":["rajesh","rajeshgoli"],"default_channel":"telegram","available_channels":["email","telegram"]}"#,
        )]);

        let human = lookup_human(&client, "rajeshgoli").unwrap().unwrap();

        assert_eq!(human["recipient"], "rajesh");
        assert_eq!(human["default_channel"], "telegram");
        assert_eq!(server.join().unwrap(), vec!["/humans/rajeshgoli"]);
    }

    #[test]
    fn lookup_identifier_exact_ignores_fuzzy_session_name_match() {
        let (client, server) = start_lookup_server([
            ("/registry/rajesh", 404, r#"{"detail":"Role not found"}"#),
            ("/sessions/rajesh", 404, r#"{"detail":"Session not found"}"#),
            (
                "/sessions",
                200,
                r#"{"sessions":[{"id":"helper001","friendly_name":"rajesh-helper","name":"codex-fork-helper001","aliases":[]}]}"#,
            ),
        ]);

        assert_eq!(lookup_identifier_exact(&client, "rajesh").unwrap(), None);
        assert_eq!(
            server.join().unwrap(),
            vec!["/registry/rajesh", "/sessions/rajesh", "/sessions"]
        );
    }

    #[test]
    fn lookup_identifier_exact_accepts_only_unique_session_id_prefixes() {
        let (client, server) = start_lookup_server([
            ("/registry/abc1", 404, r#"{"detail":"Role not found"}"#),
            ("/sessions/abc1", 404, r#"{"detail":"Session not found"}"#),
            (
                "/sessions",
                200,
                r#"{"sessions":[{"id":"abc12345","friendly_name":"one","aliases":[]},{"id":"def12345","friendly_name":"two","aliases":[]}]}"#,
            ),
        ]);
        assert_eq!(
            lookup_identifier_exact(&client, "abc1").unwrap().as_deref(),
            Some("abc12345")
        );
        server.join().unwrap();

        let (client, server) = start_lookup_server([
            ("/registry/abc", 404, r#"{"detail":"Role not found"}"#),
            ("/sessions/abc", 404, r#"{"detail":"Session not found"}"#),
            (
                "/sessions",
                200,
                r#"{"sessions":[{"id":"abc12345","friendly_name":"one","aliases":[]},{"id":"abc99999","friendly_name":"two","aliases":[]}]}"#,
            ),
        ]);
        assert!(lookup_identifier_exact(&client, "abc").is_err());
        server.join().unwrap();
    }

    #[test]
    fn human_roster_row_formats_aliases_and_channels() {
        let row = human_roster_row(&json!({
            "recipient": "rajesh",
            "display_name": "Human operator",
            "aliases": ["rajesh", "rajeshgoli", "user"],
            "default_channel": "telegram",
            "available_channels": ["email", "telegram"]
        }));

        assert_eq!(
            row,
            vec![
                "rajesh".to_owned(),
                "Human operator".to_owned(),
                "rajeshgoli,user".to_owned(),
                "telegram".to_owned(),
                "email,telegram".to_owned(),
            ]
        );
    }

    #[test]
    fn resolve_send_target_uses_friendly_name_lookup() {
        let (client, server) = start_lookup_server([
            (
                "/registry/playback-spec",
                404,
                r#"{"detail":"Role not found"}"#,
            ),
            (
                "/sessions/playback-spec",
                404,
                r#"{"detail":"Session not found"}"#,
            ),
            (
                "/sessions",
                200,
                r#"{"sessions":[{"id":"rs18ba032e54229928","friendly_name":"playback-spec","name":"codex-fork-rs18ba032e54229928","aliases":[]}]}"#,
            ),
        ]);

        assert_eq!(
            resolve_send_target(&client, "playback-spec").unwrap(),
            "rs18ba032e54229928"
        );

        let paths = server.join().unwrap();
        assert_eq!(
            paths,
            vec![
                "/registry/playback-spec",
                "/sessions/playback-spec",
                "/sessions"
            ]
        );
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
    fn name_cli_parses_self_and_child_rename_forms() {
        let self_cli = Cli::try_parse_from(["sm", "name", "maintainer"]).unwrap();
        let Command::Name(self_args) = self_cli.command else {
            panic!("expected name command");
        };
        assert_eq!(self_args.name_or_session, "maintainer");
        assert!(self_args.new_name.is_none());

        let child_cli = Cli::try_parse_from(["sm", "name", "child-session", "worker_1"]).unwrap();
        let Command::Name(child_args) = child_cli.command else {
            panic!("expected name command");
        };
        assert_eq!(child_args.name_or_session, "child-session");
        assert_eq!(child_args.new_name.as_deref(), Some("worker_1"));
    }

    #[test]
    fn name_authorization_allows_self_and_child_targets() {
        assert!(can_rename_target("session-a", "session-a", None));
        assert!(can_rename_target("session-a", "child-a", Some("session-a")));
        assert!(!can_rename_target("session-a", "session-b", None));
        assert!(!can_rename_target(
            "session-a",
            "child-b",
            Some("session-b")
        ));
    }

    #[test]
    fn name_target_resolution_uses_exact_alias_or_friendly_name_only() {
        let sessions = vec![
            json!({
                "id": "child-a",
                "aliases": ["worker-a"],
                "friendly_name": "api-worker",
                "name": "codex-api-worker"
            }),
            json!({
                "id": "child-b",
                "aliases": [],
                "friendly_name": "worker-b",
                "name": "codex-worker-b"
            }),
        ];

        assert_eq!(
            resolve_exact_session_identifier_from_sessions("worker-a", &sessions)
                .map(|(session_id, _)| session_id),
            Some("child-a".to_owned())
        );
        assert_eq!(
            resolve_exact_session_identifier_from_sessions("worker-b", &sessions)
                .map(|(session_id, _)| session_id),
            Some("child-b".to_owned())
        );
        assert!(resolve_exact_session_identifier_from_sessions("api", &sessions).is_none());
        assert!(
            resolve_exact_session_identifier_from_sessions("codex-api-worker", &sessions).is_none()
        );
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
        assert_eq!(parse_queue_timeout_seconds("none").unwrap(), 0);
        assert_eq!(parse_queue_timeout_seconds("2h").unwrap(), 7200);
    }

    #[test]
    fn queue_list_scope_labels_the_actual_projection() {
        assert_eq!(
            queue_list_scope_text(Some("session123"), false, false, None),
            "Queue scope: active pending and running jobs for notify target session123. Use --all for history across notify targets."
        );
        assert_eq!(
            queue_list_scope_text(Some("session123"), false, false, Some("running")),
            "Queue scope: notify target session123, filtered to state running. Use --all with no --notify for history across notify targets."
        );
        assert_eq!(
            queue_list_scope_text(Some("session123"), true, false, None),
            "Queue scope: active pending and running jobs for explicit notify target session123. --all retains this target and includes terminal history; use --all without --notify for history across notify targets."
        );
        assert_eq!(
            queue_list_scope_text(Some("session123"), true, true, None),
            "Queue scope: all jobs, including terminal history, for explicit notify target session123. Use --all without --notify for history across notify targets."
        );
        assert_eq!(
            queue_list_scope_text(None, false, true, None),
            "Queue scope: all jobs, including terminal history, across notify targets."
        );
        assert_eq!(
            queue_list_scope_text(None, false, true, Some("done")),
            "Queue scope: all notify targets, filtered to state done."
        );
    }

    #[test]
    fn queue_run_captures_only_path_and_allows_an_explicit_path_override() {
        let captured = queue_environment_from(|key| match key {
            "PATH" => Some("/opt/homebrew/bin:/usr/bin".to_owned()),
            "PYTHONPATH" => Some("/workspace".to_owned()),
            "VIRTUAL_ENV" => Some("/workspace/.venv".to_owned()),
            _ => None,
        });

        assert_eq!(
            captured,
            BTreeMap::from([("PATH".to_owned(), "/opt/homebrew/bin:/usr/bin".to_owned())])
        );
        assert_eq!(
            apply_queue_environment_overrides(captured, vec!["PATH=/custom/bin".to_owned()],)
                .unwrap(),
            BTreeMap::from([("PATH".to_owned(), "/custom/bin".to_owned())])
        );
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
    fn queue_log_cli_parses_bounded_tail_request() {
        let cli =
            Cli::try_parse_from(["sm", "queue", "log", "job_123abc", "--lines", "75"]).unwrap();
        let Command::Queue(queue_args) = cli.command else {
            panic!("expected queue command");
        };
        let QueueCommand::Log(log_args) = queue_args.command else {
            panic!("expected queue log command");
        };
        assert_eq!(log_args.job_id, "job_123abc");
        assert_eq!(log_args.lines, 75);
        assert!(
            Cli::try_parse_from(["sm", "queue", "log", "job_123abc", "--lines", "10001",]).is_err()
        );
    }

    #[test]
    fn queue_exit_text_marks_missing_terminal_receipt_as_non_evidence() {
        assert_eq!(
            queue_exit_text(&json!({
                "exit_code": null,
                "exit_evidence": "missing_partial_output"
            })),
            "unknown (partial/non-evidence)"
        );
        assert_eq!(
            queue_exit_text(&json!({
                "exit_code": 143,
                "exit_evidence": "recorded"
            })),
            "143"
        );
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

        let status_args = RequestCodexReviewArgs {
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
            command: Some(RequestCodexReviewCommand::Status { request_id: None }),
        };
        assert_eq!(
            codex_review_requests_list_path(&status_args, true).unwrap(),
            "/codex-review-requests?repo=rajeshgoli%2Fsession-manager&pr_number=964&include_inactive=true"
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
            launch_provider_for_alias("codex-original").unwrap(),
            "codex"
        );
        assert_eq!(launch_provider_for_alias("codex-stock").unwrap(), "codex");
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
    fn spawn_cli_provider_aliases_match_launch_aliases() {
        let cli = Cli::try_parse_from([
            "sm",
            "spawn",
            "codex",
            "--model",
            "gpt-5.6-terra",
            "--effort",
            "xhigh",
            "review this",
        ])
        .unwrap();
        let Command::Spawn(args) = cli.command else {
            panic!("expected spawn command");
        };
        assert_eq!(
            launch_provider_for_alias(&args.provider).unwrap(),
            "codex-fork"
        );
        assert_eq!(args.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(args.effort.as_deref(), Some("xhigh"));

        let cli = Cli::try_parse_from(["sm", "spawn", "codex-original", "review this"]).unwrap();
        let Command::Spawn(args) = cli.command else {
            panic!("expected spawn command");
        };
        assert_eq!(launch_provider_for_alias(&args.provider).unwrap(), "codex");

        let cli = Cli::try_parse_from(["sm", "spawn", "codex-2", "review this"]).unwrap();
        let Command::Spawn(args) = cli.command else {
            panic!("expected spawn command");
        };
        assert_eq!(
            launch_provider_for_alias(&args.provider).unwrap(),
            "codex-fork"
        );
    }

    #[test]
    fn spawn_prompt_file_preserves_verbatim_unicode_markdown() {
        let expected = "# Brief\n\nUse `$(never-run)` and 'apostrophes'.\n\nこんにちは 👋\n";
        let path = write_temp_file("sm-rust-spawn-brief", ".md", expected);
        let cli = Cli::try_parse_from([
            "sm",
            "spawn",
            "claude",
            "--prompt-file",
            path.to_str().unwrap(),
        ])
        .unwrap();
        let Command::Spawn(args) = cli.command else {
            panic!("expected spawn command");
        };

        let (prompt, source) = read_spawn_prompt(&args).unwrap();
        assert_eq!(prompt, expected);
        assert_eq!(source["kind"], "file");
        assert_eq!(source["path"], path.display().to_string());
    }

    #[test]
    fn spawn_prompt_sources_are_exclusive_and_nonempty() {
        let args = SpawnArgs {
            provider: "claude".to_owned(),
            prompt: vec!["stand by".to_owned()],
            prompt_file: Some(PathBuf::from("brief.md")),
            prompt_stdin: false,
            name: None,
            wait: None,
            model: None,
            effort: None,
            working_dir: None,
            node: None,
            json: false,
            id: None,
        };
        assert!(read_spawn_prompt(&args)
            .unwrap_err()
            .to_string()
            .contains("exactly one"));

        let empty = SpawnArgs {
            prompt: Vec::new(),
            prompt_file: None,
            ..args
        };
        assert!(read_spawn_prompt(&empty)
            .unwrap_err()
            .to_string()
            .contains("exactly one"));
    }

    #[test]
    fn spawn_prompt_stdin_preserves_multiline_bytes() {
        let args = SpawnArgs {
            provider: "claude".to_owned(),
            prompt: Vec::new(),
            prompt_file: None,
            prompt_stdin: true,
            name: None,
            wait: None,
            model: None,
            effort: None,
            working_dir: None,
            node: None,
            json: false,
            id: None,
        };
        let expected = "# Brief\n\nBackticks: `x`\n";
        let mut input = io::Cursor::new(expected.as_bytes());
        let (prompt, source) = read_spawn_prompt_from(&args, &mut input).unwrap();

        assert_eq!(prompt, expected);
        assert_eq!(source["kind"], "stdin");
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

    #[test]
    fn context_percentage_formatter_matches_terse_contract() {
        assert_eq!(format_context_percentage(Some(&json!(43))), "43%");
        assert_eq!(format_context_percentage(Some(&json!(43.5))), "43.5%");
        assert_eq!(format_context_percentage(Some(&Value::Null)), "unknown");
        assert_eq!(format_context_percentage(None), "unknown");
    }

    #[test]
    fn context_monitor_threshold_formatter_is_truthful_for_default_custom_and_invalid_rows() {
        assert_eq!(
            format_context_monitor_thresholds(&json!({
                "enforced": true,
                "warning_percentage": 65.0,
                "critical_percentage": 75.0,
                "threshold_source": "default"
            })),
            "65%, 75% (default)"
        );
        assert_eq!(
            format_context_monitor_thresholds(&json!({
                "enforced": true,
                "warning_percentage": 40.0,
                "critical_percentage": 60.0,
                "threshold_source": "custom"
            })),
            "40%, 60% (custom)"
        );
        assert_eq!(
            format_context_monitor_thresholds(&json!({
                "enforced": false,
                "threshold_source": "invalid"
            })),
            "INVALID / NOT ENFORCED"
        );
    }

    #[test]
    fn peer_root_pending_request_display_has_no_successor_move() {
        let record = json!({
            "kind": "tree",
            "subject_session_id": "outgoing",
            "target_parent_session_id": "successor",
            "expected_parent_session_id": null,
            "peer_root_succession": true,
            "frozen_live_child_ids": ["worker"]
        });

        assert_eq!(
            reparent_edges_for_display(&record),
            vec![
                json!({
                    "session_id": "outgoing",
                    "expected_parent_session_id": null,
                    "new_parent_session_id": "successor",
                }),
                json!({
                    "session_id": "worker",
                    "expected_parent_session_id": "outgoing",
                    "new_parent_session_id": "successor",
                }),
            ]
        );
    }

    #[test]
    fn context_monitor_enable_success_contract_requires_enabled_response() {
        let response = json!({"status": "ok", "enabled": true});
        assert!(ensure_context_monitor_enabled(&response).is_ok());
        let missing = ensure_context_monitor_enabled(&json!({"status": "ok"}))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("was not enrolled"));
        let false_response =
            ensure_context_monitor_enabled(&json!({"status": "ok", "enabled": false}))
                .unwrap_err()
                .to_string();
        assert!(false_response.contains("enabled=Bool(false)"));
    }

    #[test]
    fn context_monitor_enable_rejects_an_actionable_server_failure_before_printing_success() {
        let error = ApiResponse {
            status: 422,
            body: r#"{"detail":"Context monitoring cannot enroll provider \\\"codex\\\": no measured context gauge is available"}"#.to_owned(),
        }
        .into_json()
        .unwrap_err()
        .to_string();

        assert!(error.contains("HTTP 422"));
        assert!(error.contains("cannot enroll provider"));
    }

    #[test]
    fn context_target_path_segments_are_encoded() {
        assert_eq!(
            session_context_path("Runner Native/1"),
            "/sessions/Runner%20Native%2F1/context"
        );
    }

    #[test]
    fn recredential_waiting_output_is_explicitly_pending() {
        assert_eq!(
            format_recredential_outcome("rotate01", &json!({"status": "waiting_idle"})),
            "rotate01 waiting_idle (pending; target is not recredentialed until idle proof completes)"
        );
        assert_eq!(
            format_recredential_outcome("rotate01", &json!({"status": "applied"})),
            "rotate01 applied"
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
