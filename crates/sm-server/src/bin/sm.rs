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
    Clear(SessionIdArgs),
    Handoff(HandoffArgs),
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
    #[arg(long)]
    recursive: bool,
}

#[derive(Args)]
struct SessionIdArgs {
    session_id: String,
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
    Enable,
    Disable,
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
        Command::Retire(args) => {
            let payload =
                client.post_json(&format!("/sessions/{}/kill", args.session_id), json!({}))?;
            println!("{}", payload["status"].as_str().unwrap_or("stopped"));
        }
        Command::Wait(args) => wait_for_session(&client, &args.session_id, args.seconds)?,
        _ => bail!("this retained command is not implemented in the Rust core slice yet"),
    }
    Ok(())
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
            bail!("HTTP {}: {}", self.status, self.body);
        }
        serde_json::from_str(&self.body)
            .with_context(|| format!("response body was not JSON: {}", self.body))
    }
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
