//! HTTP and persisted-delivery primitives for the pinned local harness.
//! Production admission and outbox completion remain owned by the session store.
use std::{
    sync::Mutex,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpencodeConfig {
    pub binary: String,
    pub version: String,
    pub state_root: String,
    pub port_range: [u16; 2],
    pub model_base_url: String,
    pub model_id: String,
    pub context_window: u64,
    pub output_limit: u64,
    pub max_agents: usize,
    pub judge_url: String,
    pub health_timeout_secs: u64,
    pub confirm_timeout_secs: u64,
}

impl Default for OpencodeConfig {
    fn default() -> Self {
        Self {
            binary: "/opt/homebrew/bin/opencode".into(),
            version: "1.17.9".into(),
            state_root: "~/.local/share/claude-sessions/opencode".into(),
            port_range: [18500, 18599],
            model_base_url: "http://127.0.0.1:8000/v1".into(),
            model_id: "qwen3.8-flash-next".into(),
            context_window: 200000,
            output_limit: 32000,
            max_agents: 1,
            judge_url: "http://127.0.0.1:8441".into(),
            health_timeout_secs: 60,
            confirm_timeout_secs: 10,
        }
    }
}

impl OpencodeConfig {
    pub fn validate(&self) -> Result<()> {
        if self.port_range[0] == 0 || self.port_range[0] > self.port_range[1] {
            bail!("opencode.port_range must be an ascending nonzero port pair");
        }
        if self.max_agents == 0 || self.health_timeout_secs == 0 || self.confirm_timeout_secs == 0 {
            bail!("opencode agent limit and timeouts must be positive");
        }
        if self.output_limit == 0 || self.context_window <= self.output_limit {
            bail!("opencode context_window must exceed its positive output_limit");
        }
        for (name, value) in [
            ("binary", &self.binary),
            ("version", &self.version),
            ("state_root", &self.state_root),
            ("model_id", &self.model_id),
        ] {
            if value.trim().is_empty() {
                bail!("opencode.{name} must not be empty");
            }
        }
        loopback_url(&self.model_base_url)?;
        loopback_url(&self.judge_url)?;
        Ok(())
    }

    /// Every permission is allow or deny. The judge still sees web tools
    /// under the owner's #1978 ruling; task/question cannot block or delegate.
    pub fn render_agent_config(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(&json!({
            "$schema": "https://opencode.ai/config.json",
            "autoupdate": false, "share": "disabled", "snapshot": false,
            "lsp": false, "formatter": false,
            "provider": {"local": {
                "npm": "@ai-sdk/openai-compatible", "name": "Local model",
                "options": {"baseURL": self.model_base_url, "apiKey": "local", "timeout": 3000000},
                "models": {&self.model_id: {"name": self.model_id,
                    "limit": {"context": self.context_window, "output": self.output_limit}}}
            }},
            "model": format!("local/{}", self.model_id),
            "small_model": format!("local/{}", self.model_id),
            "enabled_providers": ["local"], "compaction": {"auto": true},
            "permission": {"*": "allow", "question": "deny", "task": "deny",
                "webfetch": "allow", "websearch": "allow", "doom_loop": "allow",
                "external_directory": "allow"}
        }))? + "\n")
    }
}

fn loopback_url(url: &str) -> Result<ureq::http::Uri> {
    let uri: ureq::http::Uri = url.parse().context("invalid opencode loopback URL")?;
    if uri.scheme_str() != Some("http")
        || uri.host() != Some("127.0.0.1")
        || uri.port_u16().is_none_or(|port| port == 0)
    {
        bail!("opencode URLs must use http://127.0.0.1:<nonzero port>");
    }
    Ok(uri)
}

/// All three fields are persisted atomically before the first HTTP attempt.
/// Keep this binding across retries, even when the current conversation changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageBinding {
    pub message_id: String,
    pub part_id: String,
    pub conversation_id: String,
}

impl MessageBinding {
    pub fn new(conversation_id: &str) -> Result<Self> {
        validate_id(conversation_id, "ses")?;
        let (message_id, part_id) = next_ids()?;
        Ok(Self {
            message_id,
            part_id,
            conversation_id: conversation_id.into(),
        })
    }

    pub fn validate(&self) -> Result<()> {
        validate_id(&self.conversation_id, "ses")?;
        validate_id(&self.message_id, "msg")?;
        validate_id(&self.part_id, "prt")
    }
}

fn validate_id(id: &str, prefix: &str) -> Result<()> {
    let tail = id.strip_prefix(prefix).and_then(|s| s.strip_prefix('_'));
    if tail.is_none_or(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric())) {
        bail!("invalid opencode {prefix} identifier");
    }
    Ok(())
}

// Hold a single logical millisecond/counter for both prefixes. On clock
// regression or counter exhaustion advance logical time to retain ordering.
static ID_CLOCK: Mutex<u64> = Mutex::new(0);
const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

fn next_ids() -> Result<(String, String)> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let mut clock = ID_CLOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("opencode id clock poisoned"))?;
    let mut make = |prefix| {
        *clock = (*clock + 1).max(millis * 4096 + 1);
        let mut suffix = String::with_capacity(14);
        while suffix.len() < 14 {
            let random = (OsRng.next_u32() & 255) as u8;
            // Rejection sampling avoids bias among the 62 permitted bytes.
            if random < 248 {
                suffix.push(ALPHABET[(random % 62) as usize] as char);
            }
        }
        format!("{prefix}_{:012x}{suffix}", *clock & ((1 << 48) - 1))
    };
    Ok((make("msg"), make("prt")))
}

/// This client owns no queue mutation. Callers serialize an agent's attempts,
/// persist MessageBinding first, and complete side effects only on Accepted.
/// Debug deliberately omits credentials.
#[derive(Clone)]
pub struct Client {
    base_url: String,
    authorization: String,
    request_timeout: Duration,
}

impl Client {
    pub fn new(port: u16, password: &str, request_timeout: Duration) -> Result<Self> {
        if port == 0 || password.is_empty() || request_timeout.is_zero() {
            bail!("opencode client requires a port, password and positive timeout");
        }
        Ok(Self {
            base_url: format!("http://127.0.0.1:{port}"),
            authorization: format!("Basic {}", STANDARD.encode(format!("opencode:{password}"))),
            request_timeout,
        })
    }

    fn agent(&self, timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_global(Some(timeout))
            .build()
            .into()
    }

    fn get(&self, path: &str, timeout: Duration) -> Result<ureq::http::Response<ureq::Body>> {
        Ok(self
            .agent(timeout)
            .get(&format!("{}{path}", self.base_url))
            .header("Authorization", &self.authorization)
            .call()?)
    }

    fn post(&self, path: &str, body: Value) -> Result<ureq::http::Response<ureq::Body>> {
        Ok(self
            .agent(self.request_timeout)
            .post(&format!("{}{path}", self.base_url))
            .header("Authorization", &self.authorization)
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())?)
    }

    pub fn ready(&self) -> Result<bool> {
        let response = self.get("/global/health", self.request_timeout)?;
        if response.status().as_u16() != 200 {
            return Ok(false);
        }
        self.status()?;
        Ok(true)
    }

    pub fn status(&self) -> Result<Value> {
        let value = read_json(self.get("/session/status", self.request_timeout)?)?;
        if !value.is_object() {
            bail!("opencode returned invalid conversation status");
        }
        Ok(value)
    }

    pub fn create_conversation(&self, title: &str) -> Result<String> {
        let value = read_json(self.post("/session", json!({"title": title}))?)?;
        let id = value["id"]
            .as_str()
            .context("opencode did not return a conversation id")?;
        validate_id(id, "ses")?;
        Ok(id.into())
    }

    pub fn message_exists(&self, binding: &MessageBinding) -> Result<bool> {
        self.message_exists_with_timeout(binding, self.request_timeout)
    }

    fn message_exists_with_timeout(
        &self,
        binding: &MessageBinding,
        timeout: Duration,
    ) -> Result<bool> {
        binding.validate()?;
        let path = format!(
            "/session/{}/message/{}",
            binding.conversation_id, binding.message_id
        );
        let response = self.get(&path, timeout)?;
        match response.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            status => bail!("opencode message lookup returned HTTP {status}; delivery unresolved"),
        }
    }

    /// A failed POST, timeout or bad GET leaves the persisted binding intact.
    /// A later attempt checks acceptance before sending the same text again.
    pub fn attempt_delivery(
        &self,
        binding: &MessageBinding,
        text: &str,
        confirm_timeout: Duration,
    ) -> Result<DeliveryOutcome> {
        if self.message_exists(binding)? {
            return Ok(DeliveryOutcome::Accepted);
        }
        require_success(&self.post(
            &format!("/session/{}/prompt_async", binding.conversation_id),
            json!({"messageID": binding.message_id, "parts": [{"id": binding.part_id,
                "type": "text", "text": text}]}),
        )?)?;
        let started = Instant::now();
        loop {
            let remaining = confirm_timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(DeliveryOutcome::Unconfirmed);
            }
            if self.message_exists_with_timeout(binding, remaining.min(self.request_timeout))? {
                return Ok(DeliveryOutcome::Accepted);
            }
            thread::sleep(
                Duration::from_millis(250).min(confirm_timeout.saturating_sub(started.elapsed())),
            );
        }
    }

    pub fn rename(&self, conversation_id: &str, title: &str) -> Result<()> {
        validate_id(conversation_id, "ses")?;
        let response = self
            .agent(self.request_timeout)
            .patch(&format!("{}/session/{conversation_id}", self.base_url))
            .header("Authorization", &self.authorization)
            .header("Content-Type", "application/json")
            .send(json!({"title": title}).to_string().as_bytes())?;
        require_success(&response)
    }

    /// Reserved for clear. Sending urgent input must never call this.
    pub fn abort(&self, conversation_id: &str) -> Result<()> {
        validate_id(conversation_id, "ses")?;
        require_success(&self.post(&format!("/session/{conversation_id}/abort"), json!({}))?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Accepted,
    Unconfirmed,
}

fn require_success(response: &ureq::http::Response<ureq::Body>) -> Result<()> {
    if !response.status().is_success() {
        bail!("opencode returned HTTP {}", response.status().as_u16());
    }
    Ok(())
}

fn read_json(mut response: ureq::http::Response<ureq::Body>) -> Result<Value> {
    require_success(&response)?;
    Ok(serde_json::from_str(
        &response.body_mut().read_to_string()?,
    )?)
}

#[cfg(test)]
pub(crate) mod tests;
