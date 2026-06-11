use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub paths: PathsConfig,
    pub mobile_analytics: MobileAnalyticsConfig,
    pub google_auth: GoogleAuthConfig,
    pub external_access: ExternalAccessConfig,
    pub mobile_terminal: MobileTerminalConfig,
    pub tmux: TmuxConfig,
    pub sm_send: SmSendConfig,
    pub claude: ProviderLaunchConfig,
    pub codex: ProviderLaunchConfig,
    pub codex_fork: CodexForkLaunchConfig,
    pub nodes: NodesConfig,
    pub queue_runner: QueueRunnerConfig,
    pub rust_shadow: RustShadowConfig,
    pub rust_core: RustCoreConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            paths: PathsConfig::default(),
            mobile_analytics: MobileAnalyticsConfig::default(),
            google_auth: GoogleAuthConfig::default(),
            external_access: ExternalAccessConfig::default(),
            mobile_terminal: MobileTerminalConfig::default(),
            tmux: TmuxConfig::default(),
            sm_send: SmSendConfig::default(),
            claude: ProviderLaunchConfig::new(
                "claude".to_owned(),
                Vec::new(),
                Some("sonnet".to_owned()),
            ),
            codex: ProviderLaunchConfig::new("codex".to_owned(), Vec::new(), None),
            codex_fork: CodexForkLaunchConfig::default(),
            nodes: NodesConfig::default(),
            queue_runner: QueueRunnerConfig::default(),
            rust_shadow: RustShadowConfig::default(),
            rust_core: RustCoreConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::load_from_path_with_local_env(path, Option::<&Path>::None)
    }

    pub fn load_from_path_with_local_env(
        path: impl AsRef<Path>,
        local_env_path: Option<impl AsRef<Path>>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let mut config = if !path.exists() {
            Self::default()
        } else {
            let content = fs::read_to_string(path)
                .with_context(|| format!("failed to read config {}", path.display()))?;
            let raw: RawConfig = serde_yaml::from_str(&content)
                .with_context(|| format!("failed to parse config {}", path.display()))?;
            raw.into()
        };

        let env_path = local_env_path
            .as_ref()
            .map(|value| value.as_ref().to_path_buf())
            .unwrap_or_else(|| {
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(".local/android-parity/values.env")
            });
        if env_path.exists() {
            let env_values = load_env_file(&env_path)
                .with_context(|| format!("failed to read local env {}", env_path.display()))?;
            apply_local_auth_overrides(&mut config, &env_values);
        }

        Ok(config)
    }

    pub fn queue_runner_state_dir(&self) -> PathBuf {
        if !self.queue_runner.configured && self.paths.state_file != default_state_file() {
            return queue_runner_state_dir_for_state_file(&self.paths.state_file);
        }
        PathBuf::from(&self.queue_runner.state_dir)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PathsConfig {
    #[serde(default = "default_state_file")]
    pub state_file: String,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            state_file: default_state_file(),
        }
    }
}

fn default_state_file() -> String {
    "~/.local/share/claude-sessions/sessions.json".to_owned()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GoogleAuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub public_host: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub android_client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub allowlist_emails: Vec<String>,
    #[serde(default)]
    pub session_cookie_secret: Option<String>,
}

impl GoogleAuthConfig {
    pub fn requested(&self) -> bool {
        self.enabled
    }

    pub fn ready(&self) -> bool {
        self.enabled
            && has_text(&self.client_id)
            && has_text(&self.client_secret)
            && has_text(&self.session_cookie_secret)
            && has_text(&self.public_host)
            && has_text(&self.redirect_uri)
            && self
                .allowlist_emails
                .iter()
                .any(|value| !value.trim().is_empty())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExternalAccessConfig {
    #[serde(default)]
    pub public_http_host: Option<String>,
    #[serde(default)]
    pub public_ssh_host: Option<String>,
    #[serde(default)]
    pub http_origin_url: Option<String>,
    #[serde(default)]
    pub ssh_username: Option<String>,
    #[serde(default)]
    pub ssh_proxy_command: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MobileTerminalConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub ws_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TmuxConfig {
    #[serde(default)]
    pub socket_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SmSendConfig {
    #[serde(default = "default_message_queue_db_path")]
    pub db_path: String,
}

impl Default for SmSendConfig {
    fn default() -> Self {
        Self {
            db_path: default_message_queue_db_path(),
        }
    }
}

const PRIMARY_NODE: &str = "primary";

#[derive(Debug, Clone)]
pub struct NodesConfig {
    pub default_node: String,
    pub registry: BTreeMap<String, NodeConfig>,
    pub restore_inventory_cache_seconds: f64,
}

impl Default for NodesConfig {
    fn default() -> Self {
        let mut registry = BTreeMap::new();
        registry.insert(
            PRIMARY_NODE.to_owned(),
            NodeConfig::new(PRIMARY_NODE.to_owned()),
        );
        Self {
            default_node: PRIMARY_NODE.to_owned(),
            registry,
            restore_inventory_cache_seconds: 10.0,
        }
    }
}

impl NodesConfig {
    pub fn redacted_nodes(&self) -> Vec<PublicNodeConfig> {
        self.registry
            .values()
            .map(|node| PublicNodeConfig {
                id: node.id.clone(),
                primary: node.id == PRIMARY_NODE,
                ssh: node.ssh.clone(),
                api_url: node.api_url.clone(),
                hook_base_url: node.hook_base_url.clone(),
                projects_root: node.projects_root.clone(),
                log_dir: node.log_dir.clone(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct NodeConfig {
    pub id: String,
    pub ssh: Option<String>,
    pub ssh_proxy_command: Option<String>,
    pub control_path: Option<String>,
    pub api_url: Option<String>,
    pub hook_base_url: Option<String>,
    pub hook_secret: Option<String>,
    pub node_token: Option<String>,
    pub projects_root: Option<String>,
    pub log_dir: Option<String>,
}

impl NodeConfig {
    fn new(id: String) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicNodeConfig {
    pub id: String,
    pub primary: bool,
    pub ssh: Option<String>,
    pub api_url: Option<String>,
    pub hook_base_url: Option<String>,
    pub projects_root: Option<String>,
    pub log_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLaunchConfig {
    pub command: String,
    pub args: Vec<String>,
    pub default_model: Option<String>,
}

impl ProviderLaunchConfig {
    fn new(command: String, args: Vec<String>, default_model: Option<String>) -> Self {
        Self {
            command,
            args,
            default_model,
        }
    }
}

impl Default for ProviderLaunchConfig {
    fn default() -> Self {
        Self::new("claude".to_owned(), Vec::new(), Some("sonnet".to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexForkLaunchConfig {
    pub command: String,
    pub args: Vec<String>,
    pub default_model: Option<String>,
    pub event_schema_version: u32,
}

impl Default for CodexForkLaunchConfig {
    fn default() -> Self {
        Self {
            command: "codex".to_owned(),
            args: codex_fork_managed_args(Vec::new()),
            default_model: None,
            event_schema_version: 2,
        }
    }
}

fn default_message_queue_db_path() -> String {
    "~/.local/share/claude-sessions/message_queue.db".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
pub struct MobileAnalyticsConfig {
    #[serde(default = "default_message_queue_db_path")]
    pub message_queue_db: String,
    #[serde(default = "default_server_log_file")]
    pub server_log_file: String,
}

impl Default for MobileAnalyticsConfig {
    fn default() -> Self {
        Self {
            message_queue_db: default_message_queue_db_path(),
            server_log_file: default_server_log_file(),
        }
    }
}

fn default_server_log_file() -> String {
    "/tmp/session-manager.log".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueueRunnerConfig {
    #[serde(default = "default_queue_runner_state_dir")]
    pub state_dir: String,
    #[serde(skip)]
    pub configured: bool,
}

impl Default for QueueRunnerConfig {
    fn default() -> Self {
        Self {
            state_dir: default_queue_runner_state_dir(),
            configured: false,
        }
    }
}

fn default_queue_runner_state_dir() -> String {
    "~/.local/share/claude-sessions/queue-runner".to_owned()
}

fn queue_runner_config_for_state_file(state_file: &str) -> QueueRunnerConfig {
    if state_file == default_state_file() {
        return QueueRunnerConfig::default();
    }
    QueueRunnerConfig {
        state_dir: queue_runner_state_dir_for_state_file(state_file)
            .to_string_lossy()
            .into_owned(),
        configured: false,
    }
}

fn queue_runner_state_dir_for_state_file(state_file: &str) -> PathBuf {
    let state_file = Path::new(state_file);
    let parent = state_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("queue-runner")
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RustShadowConfig {
    #[serde(default)]
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RustCoreConfig {
    #[serde(default)]
    pub fixture_writes_enabled: bool,
    #[serde(default)]
    pub runtime_enabled: bool,
    #[serde(default)]
    pub log_dir: Option<String>,
    #[serde(default)]
    pub tmux_socket_name: Option<String>,
    #[serde(default)]
    pub runtime_command: Option<String>,
    #[serde(default)]
    pub runtime_prompt_mode: Option<String>,
    #[serde(default)]
    pub runtime_start_settle_ms: Option<u64>,
    #[serde(default)]
    pub send_keys_settle_ms: Option<f64>,
    #[serde(default)]
    pub send_keys_settle_max_ms: Option<f64>,
    #[serde(default)]
    pub send_keys_settle_per_ki_ms: Option<f64>,
    #[serde(default)]
    pub send_keys_settle_per_extra_line_ms: Option<f64>,
    #[serde(default)]
    pub send_keys_max_chunk_chars: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    paths: RawPathsConfig,
    #[serde(default)]
    auth: RawAuthConfig,
    #[serde(default)]
    external_access: ExternalAccessConfig,
    #[serde(default)]
    mobile_terminal: MobileTerminalConfig,
    #[serde(default)]
    tmux: TmuxConfig,
    #[serde(default)]
    sm_send: SmSendConfig,
    #[serde(default)]
    claude: RawProviderLaunchConfig,
    #[serde(default)]
    codex: RawProviderLaunchConfig,
    #[serde(default)]
    codex_fork: RawCodexForkLaunchConfig,
    #[serde(default)]
    nodes: YamlValue,
    #[serde(default)]
    queue_runner: Option<QueueRunnerConfig>,
    #[serde(default)]
    timeouts: RawTimeoutsConfig,
    #[serde(default)]
    rust_shadow: RustShadowConfig,
    #[serde(default)]
    rust_core: RustCoreConfig,
}

impl From<RawConfig> for AppConfig {
    fn from(raw: RawConfig) -> Self {
        let mut rust_core = raw.rust_core;
        let paths = raw.paths;
        let queue_runner = raw
            .queue_runner
            .map(|mut config| {
                config.configured = true;
                config
            })
            .unwrap_or_else(|| queue_runner_config_for_state_file(&paths.state_file));
        let claude = provider_launch_config(raw.claude, "claude", Some("sonnet"), Vec::new());
        let codex = provider_launch_config(raw.codex, "codex", None, Vec::new());
        let codex_fork = codex_fork_launch_config(raw.codex_fork, &codex);
        if trimmed(&rust_core.tmux_socket_name).is_none() {
            rust_core.tmux_socket_name = trimmed(&raw.tmux.socket_name);
        }
        let tmux_timeouts = raw.timeouts.tmux;
        if rust_core.send_keys_settle_ms.is_none() {
            rust_core.send_keys_settle_ms =
                seconds_to_millis(tmux_timeouts.send_keys_settle_seconds);
        }
        if rust_core.send_keys_settle_max_ms.is_none() {
            rust_core.send_keys_settle_max_ms =
                seconds_to_millis(tmux_timeouts.send_keys_settle_max_seconds);
        }
        if rust_core.send_keys_settle_per_ki_ms.is_none() {
            rust_core.send_keys_settle_per_ki_ms =
                seconds_to_millis(tmux_timeouts.send_keys_settle_per_ki_chars);
        }
        if rust_core.send_keys_settle_per_extra_line_ms.is_none() {
            rust_core.send_keys_settle_per_extra_line_ms =
                seconds_to_millis(tmux_timeouts.send_keys_settle_per_extra_line);
        }
        if rust_core.send_keys_max_chunk_chars.is_none() {
            rust_core.send_keys_max_chunk_chars = tmux_timeouts.send_keys_max_chunk_chars;
        }
        Self {
            paths: PathsConfig {
                state_file: paths.state_file,
            },
            mobile_analytics: MobileAnalyticsConfig {
                message_queue_db: paths.message_queue_db,
                server_log_file: paths.server_log_file,
            },
            google_auth: raw.auth.google,
            external_access: raw.external_access,
            mobile_terminal: raw.mobile_terminal,
            tmux: raw.tmux,
            sm_send: raw.sm_send,
            claude,
            codex,
            codex_fork,
            nodes: nodes_config_from_yaml(raw.nodes),
            queue_runner,
            rust_shadow: raw.rust_shadow,
            rust_core,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawPathsConfig {
    #[serde(default = "default_state_file")]
    state_file: String,
    #[serde(default = "default_message_queue_db_path")]
    message_queue_db: String,
    #[serde(default = "default_server_log_file")]
    server_log_file: String,
}

impl Default for RawPathsConfig {
    fn default() -> Self {
        Self {
            state_file: default_state_file(),
            message_queue_db: default_message_queue_db_path(),
            server_log_file: default_server_log_file(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawAuthConfig {
    #[serde(default)]
    google: GoogleAuthConfig,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct RawProviderLaunchConfig {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    default_model: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct RawCodexForkLaunchConfig {
    #[serde(flatten)]
    provider: RawProviderLaunchConfig,
    #[serde(default)]
    event_schema_version: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTimeoutsConfig {
    #[serde(default)]
    tmux: RawTmuxTimeoutsConfig,
}

#[derive(Debug, Default, Deserialize)]
struct RawTmuxTimeoutsConfig {
    #[serde(default)]
    send_keys_settle_seconds: Option<f64>,
    #[serde(default)]
    send_keys_settle_max_seconds: Option<f64>,
    #[serde(default)]
    send_keys_settle_per_ki_chars: Option<f64>,
    #[serde(default)]
    send_keys_settle_per_extra_line: Option<f64>,
    #[serde(default)]
    send_keys_max_chunk_chars: Option<usize>,
}

fn provider_launch_config(
    raw: RawProviderLaunchConfig,
    default_command: &str,
    default_model: Option<&str>,
    default_args: Vec<String>,
) -> ProviderLaunchConfig {
    ProviderLaunchConfig::new(
        raw.command
            .as_ref()
            .and_then(|value| trimmed(&Some(value.clone())))
            .unwrap_or_else(|| default_command.to_owned()),
        raw.args
            .unwrap_or(default_args)
            .into_iter()
            .map(|value| value.to_string())
            .collect(),
        raw.default_model
            .as_ref()
            .and_then(|value| trimmed(&Some(value.clone())))
            .or_else(|| default_model.map(ToOwned::to_owned)),
    )
}

fn codex_fork_launch_config(
    raw: RawCodexForkLaunchConfig,
    codex: &ProviderLaunchConfig,
) -> CodexForkLaunchConfig {
    let command = raw
        .provider
        .command
        .as_ref()
        .and_then(|value| trimmed(&Some(value.clone())))
        .unwrap_or_else(|| codex.command.clone());
    let args = raw.provider.args.unwrap_or_else(|| codex.args.clone());
    let default_model = raw
        .provider
        .default_model
        .as_ref()
        .and_then(|value| trimmed(&Some(value.clone())))
        .or_else(|| codex.default_model.clone());
    CodexForkLaunchConfig {
        command,
        args: codex_fork_managed_args(args),
        default_model,
        event_schema_version: raw.event_schema_version.unwrap_or(2),
    }
}

fn codex_fork_managed_args(args: Vec<String>) -> Vec<String> {
    let mut managed_args = args;
    if !managed_args.iter().any(|arg| {
        arg.replace(' ', "")
            .contains("check_for_update_on_startup=false")
    }) {
        managed_args.extend([
            "-c".to_owned(),
            "check_for_update_on_startup=false".to_owned(),
        ]);
    }
    managed_args
}

fn nodes_config_from_yaml(value: YamlValue) -> NodesConfig {
    let mut config = NodesConfig::default();
    let Some(root) = value.as_mapping() else {
        return config;
    };

    if let Some(seconds) = yaml_mapping_get(root, "restore_inventory_cache_seconds")
        .and_then(yaml_f64)
        .filter(|value| value.is_finite())
    {
        config.restore_inventory_cache_seconds = seconds;
    }

    if let Some(registry) = yaml_mapping_get(root, "registry").and_then(YamlValue::as_mapping) {
        for (raw_id, raw_value) in registry {
            let Some(node_id) = yaml_clean_optional(raw_id) else {
                continue;
            };
            let mut node = NodeConfig::new(node_id.clone());
            if let Some(value) = raw_value.as_mapping() {
                node.ssh = yaml_mapping_get(value, "ssh").and_then(yaml_clean_optional);
                node.ssh_proxy_command =
                    yaml_mapping_get(value, "ssh_proxy_command").and_then(yaml_clean_optional);
                node.control_path = yaml_mapping_get(value, "control_path")
                    .and_then(yaml_clean_optional)
                    .map(expand_user_path);
                node.api_url = yaml_mapping_get(value, "api_url").and_then(yaml_clean_optional);
                node.hook_base_url =
                    yaml_mapping_get(value, "hook_base_url").and_then(yaml_clean_optional);
                node.hook_secret =
                    yaml_mapping_get(value, "hook_secret").and_then(yaml_clean_optional);
                node.node_token =
                    yaml_mapping_get(value, "node_token").and_then(yaml_clean_optional);
                node.projects_root =
                    yaml_mapping_get(value, "projects_root").and_then(yaml_clean_optional);
                node.log_dir = yaml_mapping_get(value, "log_dir").and_then(yaml_clean_optional);
            }
            config.registry.insert(node_id, node);
        }
    }

    let requested_default = yaml_mapping_get(root, "default")
        .and_then(yaml_clean_optional)
        .unwrap_or_else(|| PRIMARY_NODE.to_owned());
    config.default_node = if config.registry.contains_key(&requested_default) {
        requested_default
    } else {
        PRIMARY_NODE.to_owned()
    };

    config
}

fn yaml_mapping_get<'a>(mapping: &'a YamlMapping, key: &str) -> Option<&'a YamlValue> {
    mapping.get(YamlValue::String(key.to_owned()))
}

fn yaml_clean_optional(value: &YamlValue) -> Option<String> {
    let text = match value {
        YamlValue::Null => return None,
        YamlValue::Bool(value) => value.to_string(),
        YamlValue::Number(value) => value.to_string(),
        YamlValue::String(value) => value.clone(),
        _ => serde_yaml::to_string(value).ok()?,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn yaml_f64(value: &YamlValue) -> Option<f64> {
    match value {
        YamlValue::Number(value) => value.as_f64(),
        YamlValue::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn expand_user_path(value: String) -> String {
    if value == "~" {
        return env::var("HOME").unwrap_or(value);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    value
}

pub fn trimmed(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn has_text(value: &Option<String>) -> bool {
    trimmed(value).is_some()
}

fn seconds_to_millis(value: Option<f64>) -> Option<f64> {
    value
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| seconds * 1000.0)
}

fn load_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let content = fs::read_to_string(path)?;
    let mut values = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        values.insert(key.to_owned(), strip_env_quotes(value.trim()).to_owned());
    }
    Ok(values)
}

fn strip_env_quotes(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn apply_local_auth_overrides(config: &mut AppConfig, values: &BTreeMap<String, String>) {
    let public_http_host = env_text(values, "PUBLIC_HTTP_HOST");
    let public_ssh_host = env_text(values, "PUBLIC_SSH_HOST");
    let http_origin_url = env_text(values, "HTTP_ORIGIN_URL");
    let ssh_username = env_text(values, "SSH_USERNAME");
    let ssh_proxy_command = env_text(values, "SSH_PROXY_COMMAND");
    let web_client_id = env_text(values, "GOOGLE_WEB_CLIENT_ID");
    let web_client_secret = env_text(values, "GOOGLE_WEB_CLIENT_SECRET");
    let android_client_id = env_text(values, "GOOGLE_ANDROID_CLIENT_ID");
    let allowlist = parse_allowlist(values.get("ALLOWLIST_EMAIL").map(String::as_str));
    let session_secret = env_text(values, "SESSION_COOKIE_SECRET").or_else(|| {
        derive_session_cookie_secret(public_http_host.as_deref(), web_client_secret.as_deref())
    });

    if let Some(value) = public_http_host.clone() {
        config.google_auth.public_host = Some(value.clone());
        config.google_auth.redirect_uri = Some(format!("https://{value}/auth/google/callback"));
        config.external_access.public_http_host = Some(value);
    }
    if let Some(value) = web_client_id {
        config.google_auth.client_id = Some(value);
    }
    if let Some(value) = android_client_id {
        config.google_auth.android_client_id = Some(value);
    }
    if let Some(value) = web_client_secret {
        config.google_auth.client_secret = Some(value);
    }
    if !allowlist.is_empty() {
        config.google_auth.allowlist_emails = allowlist;
    }
    if let Some(value) = session_secret {
        config.google_auth.session_cookie_secret = Some(value);
    }

    if let Some(value) = public_ssh_host {
        config.external_access.public_ssh_host = Some(value);
    }
    if let Some(value) = http_origin_url {
        config.external_access.http_origin_url = Some(value);
    }
    if let Some(value) = ssh_username {
        config.external_access.ssh_username = Some(value);
    }
    if let Some(value) = ssh_proxy_command {
        config.external_access.ssh_proxy_command = Some(value);
    }

    if has_text(&config.google_auth.public_host)
        && has_text(&config.google_auth.client_id)
        && has_text(&config.google_auth.client_secret)
        && has_text(&config.google_auth.session_cookie_secret)
        && !config.google_auth.allowlist_emails.is_empty()
    {
        config.google_auth.enabled = true;
    }
}

fn env_text(values: &BTreeMap<String, String>, key: &str) -> Option<String> {
    values
        .get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_allowlist(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .replace(';', ",")
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn derive_session_cookie_secret(
    public_http_host: Option<&str>,
    web_client_secret: Option<&str>,
) -> Option<String> {
    let public_http_host = public_http_host?.trim();
    let web_client_secret = web_client_secret?.trim();
    if web_client_secret.is_empty() {
        return None;
    }
    let digest = Sha256::digest(format!(
        "sm-google-session:{public_http_host}:{web_client_secret}"
    ));
    Some(format!("{digest:x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_env_overlay_maps_mobile_auth_and_external_access() {
        let mut values = BTreeMap::new();
        values.insert("PUBLIC_HTTP_HOST".to_owned(), "sm.example.com".to_owned());
        values.insert(
            "PUBLIC_SSH_HOST".to_owned(),
            "ssh.sm.example.com".to_owned(),
        );
        values.insert(
            "HTTP_ORIGIN_URL".to_owned(),
            "http://127.0.0.1:8420".to_owned(),
        );
        values.insert("SSH_USERNAME".to_owned(), "rajesh".to_owned());
        values.insert(
            "SSH_PROXY_COMMAND".to_owned(),
            "cloudflared access ssh --hostname %h".to_owned(),
        );
        values.insert(
            "GOOGLE_WEB_CLIENT_ID".to_owned(),
            "web-client-id".to_owned(),
        );
        values.insert(
            "GOOGLE_WEB_CLIENT_SECRET".to_owned(),
            "web-client-secret".to_owned(),
        );
        values.insert(
            "GOOGLE_ANDROID_CLIENT_ID".to_owned(),
            "android-client-id".to_owned(),
        );
        values.insert(
            "ALLOWLIST_EMAIL".to_owned(),
            "rajesh@example.com;other@example.com".to_owned(),
        );

        let mut config = AppConfig::default();
        apply_local_auth_overrides(&mut config, &values);

        assert!(config.google_auth.enabled);
        assert!(config.google_auth.ready());
        assert_eq!(
            trimmed(&config.google_auth.public_host).as_deref(),
            Some("sm.example.com")
        );
        assert_eq!(
            trimmed(&config.google_auth.redirect_uri).as_deref(),
            Some("https://sm.example.com/auth/google/callback")
        );
        assert_eq!(
            config.google_auth.allowlist_emails,
            vec!["rajesh@example.com", "other@example.com"]
        );
        assert_eq!(
            config
                .google_auth
                .session_cookie_secret
                .as_ref()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(
            trimmed(&config.external_access.ssh_proxy_command).as_deref(),
            Some("cloudflared access ssh --hostname %h")
        );
    }

    #[test]
    fn raw_config_reads_python_sm_send_db_path() {
        let raw: RawConfig = serde_yaml::from_str(
            r#"
sm_send:
  db_path: /tmp/custom-message-queue.db
"#,
        )
        .unwrap();
        let config = AppConfig::from(raw);

        assert_eq!(config.sm_send.db_path, "/tmp/custom-message-queue.db");
    }

    #[test]
    fn raw_config_reads_queue_runner_state_dir() {
        let raw: RawConfig = serde_yaml::from_str(
            r#"
queue_runner:
  state_dir: /tmp/custom-queue-runner
"#,
        )
        .unwrap();
        let config = AppConfig::from(raw);

        assert_eq!(config.queue_runner.state_dir, "/tmp/custom-queue-runner");
    }

    #[test]
    fn raw_config_derives_queue_runner_state_dir_from_custom_state_file() {
        let raw: RawConfig = serde_yaml::from_str(
            r#"
paths:
  state_file: /tmp/sm-fixture/sessions.json
"#,
        )
        .unwrap();
        let config = AppConfig::from(raw);

        assert_eq!(
            config.queue_runner.state_dir,
            "/tmp/sm-fixture/queue-runner"
        );
    }

    #[test]
    fn raw_config_reads_codex_fork_launch_config_with_codex_fallbacks() {
        let raw: RawConfig = serde_yaml::from_str(
            r#"
codex:
  command: "/opt/bin/codex"
  args:
    - "--dangerously-bypass-approvals-and-sandbox"
  default_model: "gpt-5"
codex_fork:
  event_schema_version: 7
"#,
        )
        .unwrap();
        let config = AppConfig::from(raw);

        assert_eq!(config.codex_fork.command, "/opt/bin/codex");
        assert_eq!(
            config.codex_fork.args,
            vec![
                "--dangerously-bypass-approvals-and-sandbox",
                "-c",
                "check_for_update_on_startup=false"
            ]
        );
        assert_eq!(config.codex_fork.default_model.as_deref(), Some("gpt-5"));
        assert_eq!(config.codex_fork.event_schema_version, 7);
    }

    #[test]
    fn raw_config_does_not_duplicate_codex_fork_startup_update_disable_arg() {
        let raw: RawConfig = serde_yaml::from_str(
            r#"
codex:
  command: "codex"
  args:
    - "-c"
    - "check_for_update_on_startup=false"
codex_fork:
  command: "/opt/bin/codex-fork"
"#,
        )
        .unwrap();
        let config = AppConfig::from(raw);

        assert_eq!(config.codex_fork.command, "/opt/bin/codex-fork");
        assert_eq!(
            config.codex_fork.args,
            vec!["-c", "check_for_update_on_startup=false"]
        );
    }
}
