use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DEFAULT_SESSION_STATE_FILE: &str = "~/.local/share/claude-sessions/sessions.json";
const LEGACY_TMP_SESSION_STATE_FILE: &str = "/tmp/claude-sessions/sessions.json";

#[derive(Debug, Clone)]
pub struct SessionStore {
    state_file: PathBuf,
    legacy_state_file: Option<PathBuf>,
}

impl SessionStore {
    pub fn new(state_file: PathBuf) -> Self {
        let legacy_state_file = if state_file == expand_home(DEFAULT_SESSION_STATE_FILE) {
            Some(PathBuf::from(LEGACY_TMP_SESSION_STATE_FILE))
        } else {
            None
        };
        Self {
            state_file,
            legacy_state_file,
        }
    }

    #[cfg(test)]
    fn new_with_legacy_fallback(state_file: PathBuf, legacy_state_file: PathBuf) -> Self {
        Self {
            state_file,
            legacy_state_file: Some(legacy_state_file),
        }
    }

    pub fn list_sessions(&self, include_stopped: bool) -> Result<Vec<SessionRecord>> {
        let snapshot = self.load_snapshot()?;
        Ok(snapshot
            .sessions
            .into_iter()
            .filter(|session| include_stopped || !session.is_stopped())
            .collect())
    }

    fn load_snapshot(&self) -> Result<StateSnapshot> {
        let state_file = self.readable_state_file();
        if !state_file.exists() {
            return Ok(StateSnapshot::default());
        }
        let content = fs::read_to_string(&state_file)
            .with_context(|| format!("failed to read session state {}", state_file.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse session state {}", state_file.display()))
    }

    fn readable_state_file(&self) -> PathBuf {
        if !self.state_file.exists() {
            if let Some(legacy_state_file) = &self.legacy_state_file {
                if legacy_state_file.exists() {
                    return legacy_state_file.clone();
                }
            }
        }
        self.state_file.clone()
    }
}

pub fn expand_home(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    match env::var_os("HOME") {
        Some(home) => Path::new(&home).join(rest),
        None => PathBuf::from(path),
    }
}

#[derive(Debug, Default, Deserialize)]
struct StateSnapshot {
    #[serde(default)]
    sessions: Vec<SessionRecord>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub name: String,
    pub working_dir: String,
    pub tmux_session: String,
    #[serde(default)]
    pub tmux_socket_name: Option<String>,
    #[serde(default = "default_node")]
    pub node: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub provider_resume_id: Option<String>,
    #[serde(default)]
    pub forked_from_session_id: Option<String>,
    #[serde(default)]
    pub forked_from_provider_resume_id: Option<String>,
    #[serde(default)]
    pub forked_provider_resume_id: Option<String>,
    #[serde(default)]
    pub forked_at: Option<String>,
    #[serde(default)]
    pub forked_by_session_id: Option<String>,
    #[serde(default)]
    pub friendly_name: Option<String>,
    #[serde(default)]
    pub friendly_name_is_explicit: bool,
    #[serde(default)]
    pub friendly_name_updated_at_ns: Option<i64>,
    #[serde(default)]
    pub native_title: Option<String>,
    #[serde(default)]
    pub native_title_updated_at_ns: Option<i64>,
    #[serde(default)]
    pub native_title_source_mtime_ns: Option<i64>,
    #[serde(default)]
    pub telegram_chat_id: Option<i64>,
    #[serde(default)]
    pub telegram_thread_id: Option<i64>,
    #[serde(default)]
    pub current_task: Option<String>,
    #[serde(default)]
    pub git_remote_url: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub last_handoff_path: Option<String>,
    #[serde(default)]
    pub agent_status_text: Option<String>,
    #[serde(default)]
    pub agent_status_at: Option<String>,
    #[serde(default)]
    pub agent_task_completed_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub stopped_at: Option<String>,
    #[serde(default)]
    pub is_em: bool,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: String,
    pub created_at: String,
    pub last_activity: String,
    #[serde(default)]
    pub last_tool_call: Option<String>,
    #[serde(default)]
    pub last_tool_name: Option<String>,
    #[serde(default)]
    pub tokens_used: i64,
    #[serde(default)]
    pub context_monitor_enabled: bool,
}

impl SessionRecord {
    fn is_stopped(&self) -> bool {
        normalized_status(&self.status) == "stopped"
    }

    fn cached_display_name(&self) -> Option<String> {
        let native_title = self.native_title.as_deref().filter(|value| {
            matches!(
                self.provider.as_str(),
                "claude" | "codex" | "codex-app" | "codex-fork"
            ) && !value.trim().is_empty()
        });
        let friendly_name = self
            .friendly_name
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let friendly_name_updated_at_ns = self.friendly_name_updated_at_ns.unwrap_or(0);
        let native_title_updated_at_ns = self
            .native_title_updated_at_ns
            .or(self.native_title_source_mtime_ns)
            .unwrap_or(0);

        if let (Some(friendly_name), Some(native_title)) = (friendly_name, native_title) {
            if friendly_name_updated_at_ns >= native_title_updated_at_ns {
                return Some(friendly_name.to_owned());
            }
            return Some(native_title.to_owned());
        }
        if self.friendly_name_is_explicit {
            if let Some(friendly_name) = friendly_name {
                return Some(friendly_name.to_owned());
            }
        }
        if let Some(native_title) = native_title {
            return Some(native_title.to_owned());
        }
        friendly_name.map(ToOwned::to_owned)
    }
}

#[derive(Debug, Serialize)]
pub struct SessionsEnvelope<T> {
    pub sessions: Vec<T>,
}

impl From<Vec<SessionResponse>> for SessionsEnvelope<SessionResponse> {
    fn from(sessions: Vec<SessionResponse>) -> Self {
        Self { sessions }
    }
}

impl From<Vec<ClientSessionResponse>> for SessionsEnvelope<ClientSessionResponse> {
    fn from(sessions: Vec<ClientSessionResponse>) -> Self {
        Self { sessions }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionResponse {
    id: String,
    name: String,
    working_dir: String,
    status: String,
    created_at: String,
    last_activity: String,
    completed_at: Option<String>,
    stopped_at: Option<String>,
    tmux_session: String,
    tmux_socket_name: Option<String>,
    node: String,
    provider: Option<String>,
    provider_resume_id: Option<String>,
    forked_from_session_id: Option<String>,
    forked_from_provider_resume_id: Option<String>,
    forked_provider_resume_id: Option<String>,
    forked_at: Option<String>,
    forked_by_session_id: Option<String>,
    friendly_name: Option<String>,
    telegram_chat_id: Option<i64>,
    telegram_thread_id: Option<i64>,
    current_task: Option<String>,
    git_remote_url: Option<String>,
    parent_session_id: Option<String>,
    last_handoff_path: Option<String>,
    agent_status_text: Option<String>,
    agent_status_at: Option<String>,
    agent_task_completed_at: Option<String>,
    is_em: bool,
    role: Option<String>,
    activity_state: String,
    last_tool_call: Option<String>,
    last_tool_name: Option<String>,
    last_action_summary: Option<String>,
    last_action_at: Option<String>,
    tokens_used: i64,
    context_monitor_enabled: bool,
    pending_adoption_proposals: Vec<Value>,
    aliases: Vec<String>,
    is_maintainer: bool,
}

impl From<SessionRecord> for SessionResponse {
    fn from(session: SessionRecord) -> Self {
        let status = normalized_status(&session.status);
        let friendly_name = session.cached_display_name();
        Self {
            id: session.id,
            name: session.name,
            working_dir: session.working_dir,
            status: status.to_owned(),
            created_at: session.created_at,
            last_activity: session.last_activity,
            completed_at: session.completed_at,
            stopped_at: session.stopped_at,
            tmux_session: session.tmux_session,
            tmux_socket_name: session.tmux_socket_name,
            node: non_empty_or(session.node, "primary"),
            provider: Some(non_empty_or(session.provider, "claude")),
            provider_resume_id: session.provider_resume_id,
            forked_from_session_id: session.forked_from_session_id,
            forked_from_provider_resume_id: session.forked_from_provider_resume_id,
            forked_provider_resume_id: session.forked_provider_resume_id,
            forked_at: session.forked_at,
            forked_by_session_id: session.forked_by_session_id,
            friendly_name,
            telegram_chat_id: session.telegram_chat_id,
            telegram_thread_id: session.telegram_thread_id,
            current_task: session.current_task,
            git_remote_url: session.git_remote_url,
            parent_session_id: session.parent_session_id,
            last_handoff_path: session.last_handoff_path,
            agent_status_text: session.agent_status_text,
            agent_status_at: session.agent_status_at,
            agent_task_completed_at: session.agent_task_completed_at,
            is_em: session.is_em,
            role: session.role,
            activity_state: fallback_activity_state(status),
            last_tool_call: session.last_tool_call,
            last_tool_name: session.last_tool_name,
            last_action_summary: None,
            last_action_at: None,
            tokens_used: session.tokens_used,
            context_monitor_enabled: session.context_monitor_enabled,
            pending_adoption_proposals: Vec::new(),
            aliases: Vec::new(),
            is_maintainer: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ClientSessionResponse {
    #[serde(flatten)]
    session: SessionResponse,
    attach_descriptor: AttachDescriptor,
    termux_attach: Option<Value>,
    mobile_terminal: Value,
    primary_action: PrimaryAction,
}

impl From<SessionRecord> for ClientSessionResponse {
    fn from(session: SessionRecord) -> Self {
        let response = SessionResponse::from(session);
        let attach_descriptor = AttachDescriptor {
            attach_supported: false,
            message: Some(
                "attach tickets are not implemented in the Rust read-only scaffold".to_owned(),
            ),
            tmux_session: Some(response.tmux_session.clone()),
            runtime_mode: Some("read_only".to_owned()),
        };
        Self {
            session: response,
            attach_descriptor,
            termux_attach: None,
            mobile_terminal: json!({
                "supported": false,
                "reason": "mobile terminal is not implemented in the Rust read-only scaffold"
            }),
            primary_action: PrimaryAction {
                action_type: "details",
                label: "View details",
                reason: None,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AttachDescriptor {
    attach_supported: bool,
    message: Option<String>,
    tmux_session: Option<String>,
    runtime_mode: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PrimaryAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    label: &'static str,
    reason: Option<String>,
}

fn normalized_status(status: &str) -> &str {
    match status {
        "starting" => "running",
        "waiting_input" | "waiting_permission" | "error" => "idle",
        "running" | "idle" | "stopped" => status,
        _ => status,
    }
}

fn fallback_activity_state(status: &str) -> String {
    match status {
        "stopped" => "stopped".to_owned(),
        "running" => "working".to_owned(),
        _ => "idle".to_owned(),
    }
}

fn non_empty_or(value: String, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

fn default_node() -> String {
    "primary".to_owned()
}

fn default_provider() -> String {
    "claude".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_record(status: &str) -> SessionRecord {
        SessionRecord {
            id: "abc12345".to_owned(),
            name: "claude-abc12345".to_owned(),
            working_dir: "/repo".to_owned(),
            tmux_session: "claude-abc12345".to_owned(),
            tmux_socket_name: None,
            node: "primary".to_owned(),
            provider: "claude".to_owned(),
            provider_resume_id: None,
            forked_from_session_id: None,
            forked_from_provider_resume_id: None,
            forked_provider_resume_id: None,
            forked_at: None,
            forked_by_session_id: None,
            friendly_name: Some("Example".to_owned()),
            friendly_name_is_explicit: false,
            friendly_name_updated_at_ns: None,
            native_title: None,
            native_title_updated_at_ns: None,
            native_title_source_mtime_ns: None,
            telegram_chat_id: None,
            telegram_thread_id: None,
            current_task: None,
            git_remote_url: None,
            parent_session_id: None,
            last_handoff_path: None,
            agent_status_text: None,
            agent_status_at: None,
            agent_task_completed_at: None,
            completed_at: None,
            stopped_at: None,
            is_em: false,
            role: None,
            status: status.to_owned(),
            created_at: "2026-06-01T00:00:00".to_owned(),
            last_activity: "2026-06-01T00:01:00".to_owned(),
            last_tool_call: None,
            last_tool_name: None,
            tokens_used: 0,
            context_monitor_enabled: false,
        }
    }

    #[test]
    fn session_projection_maps_legacy_status_and_activity() {
        let response = SessionResponse::from(session_record("waiting_permission"));

        assert_eq!(response.status, "idle");
        assert_eq!(response.activity_state, "idle");
    }

    #[test]
    fn client_projection_disables_unported_attach_surfaces() {
        let response = ClientSessionResponse::from(session_record("running"));

        assert!(!response.attach_descriptor.attach_supported);
        assert!(response.termux_attach.is_none());
        assert_eq!(response.mobile_terminal["supported"], false);
        assert_eq!(response.primary_action.action_type, "details");
    }

    #[test]
    fn cached_display_name_prefers_newer_native_title() {
        let mut session = session_record("running");
        session.friendly_name = Some("stale-friendly-name".to_owned());
        session.friendly_name_updated_at_ns = Some(10);
        session.native_title = Some("cached-native-title".to_owned());
        session.native_title_updated_at_ns = Some(20);
        let response = SessionResponse::from(session);

        assert_eq!(
            response.friendly_name.as_deref(),
            Some("cached-native-title")
        );
    }

    #[test]
    fn cached_display_name_keeps_newer_explicit_friendly_name() {
        let mut session = session_record("running");
        session.friendly_name = Some("explicit-name".to_owned());
        session.friendly_name_is_explicit = true;
        session.friendly_name_updated_at_ns = Some(30);
        session.native_title = Some("older-native-title".to_owned());
        session.native_title_updated_at_ns = Some(20);
        let response = SessionResponse::from(session);

        assert_eq!(response.friendly_name.as_deref(), Some("explicit-name"));
    }

    #[test]
    fn default_state_loader_reads_legacy_fallback_when_primary_missing() {
        let state_file = unique_temp_path("primary");
        let legacy_state_file = unique_temp_path("legacy");
        fs::write(
            &legacy_state_file,
            json!({
                "sessions": [
                    {
                        "id": "legacy1",
                        "name": "claude-legacy1",
                        "working_dir": "/repo",
                        "tmux_session": "claude-legacy1",
                        "log_file": "/tmp/legacy1.log",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file, legacy_state_file);

        let sessions = store.list_sessions(false).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "legacy1");
    }

    fn unique_temp_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "sm-rust-session-store-{label}-{}-{nanos}.json",
            std::process::id()
        ))
    }
}
