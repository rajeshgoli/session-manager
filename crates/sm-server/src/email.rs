use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{trimmed, AppConfig};

pub const DEFAULT_EMAIL_WEBHOOK_PATH: &str = "/api/email-inbound";
pub const DEFAULT_EMAIL_WORKER_SECRET_HEADER: &str = "x-email-worker-secret";
pub const DEFAULT_EMAIL_SESSION_ID_HEADER: &str = "x-email-session-id";
const MAX_EMAIL_SUBJECT_LENGTH: usize = 140;

#[derive(Debug)]
pub struct EmailBridge {
    path: PathBuf,
    config: BridgeFileConfig,
}

impl EmailBridge {
    pub fn load(config: &AppConfig) -> Result<Self> {
        let path = expand_email_config_path(&config.email.bridge_config);
        let config = match fs::read_to_string(&path) {
            Ok(content) => serde_yaml::from_str(&content).with_context(|| {
                format!("failed to parse email bridge config {}", path.display())
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                BridgeFileConfig::default()
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read email bridge config {}", path.display())
                })
            }
        };
        Ok(Self { path, config })
    }

    pub fn bridge_is_available(&self) -> bool {
        self.api_key().is_some() && self.domain().is_some()
    }

    pub fn webhook_path(&self) -> String {
        normalize_webhook_path(
            self.config
                .email_bridge
                .webhook_path
                .as_deref()
                .unwrap_or(DEFAULT_EMAIL_WEBHOOK_PATH),
        )
    }

    pub fn worker_secret_header(&self) -> String {
        self.config
            .email_bridge
            .worker_secret_header
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_EMAIL_WORKER_SECRET_HEADER)
            .to_ascii_lowercase()
    }

    pub fn worker_secret(&self) -> Option<String> {
        trimmed(&self.config.email_bridge.worker_secret)
    }

    pub fn session_id_header(&self) -> String {
        self.config
            .email_bridge
            .session_id_header
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_EMAIL_SESSION_ID_HEADER)
            .to_ascii_lowercase()
    }

    pub fn is_authorized_sender(&self, sender: &str) -> bool {
        let sender = sender.trim().to_ascii_lowercase();
        !sender.is_empty() && self.authorized_senders().contains(&sender)
    }

    pub fn list_humans(&self) -> Vec<HumanRecipient> {
        let mut recipients = self
            .config
            .humans
            .iter()
            .filter_map(|(name, spec)| HumanRecipient::from_spec(name, spec))
            .collect::<Vec<_>>();
        recipients.sort_by(|left, right| left.name.cmp(&right.name));
        recipients
    }

    pub fn lookup_human(&self, identifier: &str) -> Result<Option<HumanRecipient>> {
        let needle = identifier.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return Ok(None);
        }
        let mut matches = self
            .list_humans()
            .into_iter()
            .filter(|human| human.aliases.iter().any(|alias| alias == &needle))
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.name.cmp(&right.name));
        if matches.len() > 1 {
            let names = matches
                .iter()
                .map(|human| human.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "Human recipient alias \"{identifier}\" is configured for multiple humans: {names}"
            );
        }
        Ok(matches.pop())
    }

    pub fn lookup_human_email_user(&self, identifier: &str) -> Result<Option<RegisteredEmailUser>> {
        let Some(human) = self.lookup_human(identifier)? else {
            return Ok(None);
        };
        let Some(channel) = human.channel("email") else {
            return Ok(None);
        };
        let email = channel
            .resolved_address()
            .or_else(|| self.lookup_user(&human.name).map(|user| user.email))
            .filter(|value| !value.trim().is_empty());
        let Some(email) = email else {
            return Ok(None);
        };
        Ok(Some(RegisteredEmailUser {
            username: human.name.clone(),
            email,
            display_name: human.display_name.clone(),
            aliases: human.aliases.iter().cloned().collect(),
        }))
    }

    pub fn lookup_user(&self, identifier: &str) -> Option<RegisteredEmailUser> {
        let needle = identifier.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return None;
        }
        self.config
            .users
            .iter()
            .filter_map(|(username, spec)| RegisteredEmailUser::from_spec(username, spec))
            .find(|user| user.aliases.contains(&needle))
    }

    pub fn resolve_users(&self, identifiers: &[String]) -> Result<Vec<RegisteredEmailUser>> {
        let mut users = Vec::new();
        let mut seen = BTreeSet::new();
        for identifier in identifiers {
            let Some(user) = self.lookup_user(identifier) else {
                bail!("No registered email user found for '{identifier}'");
            };
            let key = user.email.to_ascii_lowercase();
            if seen.insert(key) {
                users.push(user);
            }
        }
        if users.is_empty() {
            bail!("No registered email users were provided");
        }
        Ok(users)
    }

    pub fn send_agent_email(&self, request: SendAgentEmailRequest) -> Result<SentEmail> {
        if !self.bridge_is_available() {
            bail!(
                "Email bridge config is unavailable at {}",
                self.path.display()
            );
        }
        let sender_session_id = request.sender_session_id.trim();
        if sender_session_id.is_empty() {
            bail!("Managed sender session is required for agent email delivery");
        }
        let sender_name = if request.sender_name.trim().is_empty() {
            sender_session_id
        } else {
            request.sender_name.trim()
        };
        let sender_provider = if request.sender_provider.trim().is_empty() {
            "unknown"
        } else {
            request.sender_provider.trim()
        };

        let mut text_payload = request.body_text.trim().to_owned();
        let mut html_payload = request.body_html.trim().to_owned();
        if text_payload.is_empty() && html_payload.is_empty() {
            bail!("Email body is required");
        }
        if request.body_markdown && !text_payload.is_empty() && html_payload.is_empty() {
            html_payload = render_markdown_to_html(&text_payload);
        } else if !text_payload.is_empty() && html_payload.is_empty() {
            html_payload = plain_text_to_html(&text_payload);
        } else if !html_payload.is_empty() && text_payload.is_empty() {
            text_payload = strip_html(&html_payload);
        }

        let subject = match request
            .subject
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(subject) => subject.to_owned(),
            None if request.auto_subject => default_subject(sender_name, &text_payload),
            None => bail!("Email subject is required"),
        };
        let footer = build_routing_footer(sender_name, sender_session_id, sender_provider);
        let (text_payload, html_payload) =
            append_routing_footer(&text_payload, &html_payload, &footer);
        let from_address = self.reply_address(sender_session_id)?;
        let from_header = format!("{sender_name} <{from_address}>");
        let api_key = self
            .api_key()
            .ok_or_else(|| anyhow!("Email bridge is missing resend.api_key"))?;
        let endpoint = format!("{}/emails", self.api_base_url());

        let mut payload = json!({
            "from": from_header,
            "to": request.to_users.iter().map(|user| user.email.clone()).collect::<Vec<_>>(),
            "subject": subject,
            "text": text_payload,
            "reply_to": from_address,
            "headers": {
                "X-SM-Session-ID": sender_session_id,
            },
        });
        if !request.cc_users.is_empty() {
            payload["cc"] = json!(request
                .cc_users
                .iter()
                .map(|user| user.email.clone())
                .collect::<Vec<_>>());
        }
        if !html_payload.is_empty() {
            payload["html"] = Value::String(html_payload);
        }

        let body_bytes = serde_json::to_vec(&payload)?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .post(&endpoint)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .send(body_bytes.as_slice())
            .with_context(|| format!("failed to request {endpoint}"))?;
        let status = response.status().as_u16();
        let response_body = response.body_mut().read_to_string()?;
        if status >= 400 {
            bail!("Resend email send failed ({status}): {response_body}");
        }
        let response_payload = serde_json::from_str::<Value>(&response_body).unwrap_or(Value::Null);
        Ok(SentEmail {
            subject: payload["subject"].as_str().unwrap_or("").to_owned(),
            to: request.to_users,
            cc: request.cc_users,
            message_id: response_payload
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            from: payload["from"].as_str().unwrap_or("").to_owned(),
            reply_to: payload["reply_to"].as_str().unwrap_or("").to_owned(),
        })
    }

    fn authorized_senders(&self) -> BTreeSet<String> {
        string_list(&self.config.email_bridge.authorized_senders)
            .into_iter()
            .map(|value| value.to_ascii_lowercase())
            .collect()
    }

    fn api_key(&self) -> Option<String> {
        trimmed(&self.config.resend.api_key)
    }

    fn domain(&self) -> Option<String> {
        trimmed(&self.config.resend.domain)
    }

    fn reply_address(&self, sender_session_id: &str) -> Result<String> {
        if let Some(value) = trimmed(&self.config.resend.reply_address)
            .or_else(|| trimmed(&self.config.resend.from_address))
        {
            return Ok(value);
        }
        let domain = trimmed(&self.config.resend.reply_domain)
            .or_else(|| self.domain())
            .ok_or_else(|| anyhow!("Email bridge is missing resend.domain"))?;
        Ok(format!("{sender_session_id}@{domain}"))
    }

    fn api_base_url(&self) -> String {
        trimmed(&self.config.resend.api_base_url)
            .unwrap_or_else(|| "https://api.resend.com".to_owned())
            .trim_end_matches('/')
            .to_owned()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HumanRecipientResponse {
    pub recipient: String,
    pub display_name: String,
    pub aliases: Vec<String>,
    pub default_channel: String,
    pub available_channels: Vec<String>,
    pub telegram_delivery: Option<String>,
    pub email_use: Option<String>,
}

impl From<HumanRecipient> for HumanRecipientResponse {
    fn from(human: HumanRecipient) -> Self {
        let telegram_delivery = human
            .channel("telegram")
            .and_then(|channel| channel.delivery);
        let email_use = human.channel("email").and_then(|channel| channel.use_);
        let available_channels = human.available_channels();
        Self {
            recipient: human.name,
            display_name: human.display_name,
            aliases: human.aliases,
            default_channel: human.default_channel,
            available_channels,
            telegram_delivery,
            email_use,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HumanRecipient {
    pub name: String,
    pub display_name: String,
    pub aliases: Vec<String>,
    pub default_channel: String,
    pub channels: BTreeMap<String, HumanChannel>,
}

impl HumanRecipient {
    fn from_spec(raw_name: &str, raw_spec: &HumanSpec) -> Option<Self> {
        let name = raw_name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return None;
        }
        let display_name = trimmed(&raw_spec.display_name)
            .or_else(|| trimmed(&raw_spec.name))
            .unwrap_or_else(|| name.clone());
        let default_channel = trimmed(&raw_spec.default_channel)
            .unwrap_or_else(|| "telegram".to_owned())
            .to_ascii_lowercase();
        let mut aliases = vec![name.clone()];
        aliases.extend(
            string_list(&raw_spec.aliases)
                .into_iter()
                .map(|value| value.to_ascii_lowercase()),
        );
        aliases = dedupe_nonempty(aliases);
        let channels = raw_spec
            .channels
            .iter()
            .filter_map(|(channel_name, spec)| {
                HumanChannel::from_spec(channel_name, spec)
                    .map(|channel| (channel.name.clone(), channel))
            })
            .collect();
        Some(Self {
            name,
            display_name,
            aliases,
            default_channel,
            channels,
        })
    }

    pub fn channel(&self, name: &str) -> Option<HumanChannel> {
        self.channels
            .get(name)
            .filter(|channel| channel.enabled)
            .cloned()
    }

    fn available_channels(&self) -> Vec<String> {
        self.channels
            .values()
            .filter(|channel| channel.enabled)
            .map(|channel| channel.name.clone())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct HumanChannel {
    pub name: String,
    pub enabled: bool,
    pub delivery: Option<String>,
    pub address: Option<String>,
    pub address_env: Option<String>,
    pub use_: Option<String>,
}

impl HumanChannel {
    fn from_spec(raw_name: &str, raw_spec: &ChannelSpec) -> Option<Self> {
        let name = raw_name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return None;
        }
        Some(Self {
            name,
            enabled: raw_spec.enabled(),
            delivery: trimmed(&raw_spec.delivery()).map(|value| value.to_ascii_lowercase()),
            address: trimmed(&raw_spec.address()),
            address_env: trimmed(&raw_spec.address_env()),
            use_: trimmed(&raw_spec.use_()).map(|value| value.to_ascii_lowercase()),
        })
    }

    fn resolved_address(&self) -> Option<String> {
        trimmed(&self.address).or_else(|| {
            self.address_env
                .as_deref()
                .and_then(|key| env::var(key).ok())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredEmailUser {
    pub username: String,
    pub email: String,
    pub display_name: String,
    pub aliases: BTreeSet<String>,
}

impl RegisteredEmailUser {
    fn from_spec(username: &str, spec: &UserSpec) -> Option<Self> {
        let username = username.trim().to_owned();
        if username.is_empty() {
            return None;
        }
        let email = match spec {
            UserSpec::Address(value) => value.trim().to_owned(),
            UserSpec::Details(details) => trimmed(&details.email).unwrap_or_default(),
        };
        if email.is_empty() {
            return None;
        }
        let display_name = match spec {
            UserSpec::Address(_) => username.clone(),
            UserSpec::Details(details) => {
                trimmed(&details.name).unwrap_or_else(|| username.clone())
            }
        };
        let mut aliases = BTreeSet::from([username.to_ascii_lowercase()]);
        if let UserSpec::Details(details) = spec {
            aliases.extend(
                string_list(&details.aliases)
                    .into_iter()
                    .map(|value| value.to_ascii_lowercase()),
            );
        }
        Some(Self {
            username,
            email,
            display_name,
            aliases,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SendAgentEmailRequest {
    pub sender_session_id: String,
    pub sender_name: String,
    pub sender_provider: String,
    pub to_users: Vec<RegisteredEmailUser>,
    pub cc_users: Vec<RegisteredEmailUser>,
    pub subject: Option<String>,
    pub body_text: String,
    pub body_html: String,
    pub body_markdown: bool,
    pub auto_subject: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SentEmail {
    pub subject: String,
    pub to: Vec<RegisteredEmailUser>,
    pub cc: Vec<RegisteredEmailUser>,
    pub message_id: Option<String>,
    pub from: String,
    pub reply_to: String,
}

#[derive(Debug, Default, Deserialize)]
struct BridgeFileConfig {
    #[serde(default)]
    resend: ResendConfig,
    #[serde(default)]
    humans: BTreeMap<String, HumanSpec>,
    #[serde(default)]
    users: BTreeMap<String, UserSpec>,
    #[serde(default)]
    email_bridge: EmailBridgeConfig,
}

#[derive(Debug, Default, Deserialize)]
struct ResendConfig {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    reply_domain: Option<String>,
    #[serde(default)]
    reply_address: Option<String>,
    #[serde(default)]
    from_address: Option<String>,
    #[serde(default)]
    api_base_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct EmailBridgeConfig {
    #[serde(default)]
    authorized_senders: StringList,
    #[serde(default)]
    worker_secret: Option<String>,
    #[serde(default)]
    worker_secret_header: Option<String>,
    #[serde(default)]
    session_id_header: Option<String>,
    #[serde(default)]
    webhook_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HumanSpec {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    aliases: StringList,
    #[serde(default)]
    default_channel: Option<String>,
    #[serde(default)]
    channels: BTreeMap<String, ChannelSpec>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum ChannelSpec {
    Bool(bool),
    Details(ChannelDetails),
    #[default]
    Invalid,
}

impl ChannelSpec {
    fn enabled(&self) -> bool {
        match self {
            Self::Bool(value) => *value,
            Self::Details(details) => details.enabled,
            Self::Invalid => false,
        }
    }

    fn delivery(&self) -> Option<String> {
        match self {
            Self::Details(details) => details.delivery.clone(),
            _ => None,
        }
    }

    fn address(&self) -> Option<String> {
        match self {
            Self::Details(details) => details.address.clone(),
            _ => None,
        }
    }

    fn address_env(&self) -> Option<String> {
        match self {
            Self::Details(details) => details.address_env.clone(),
            _ => None,
        }
    }

    fn use_(&self) -> Option<String> {
        match self {
            Self::Details(details) => details.use_.clone(),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ChannelDetails {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    delivery: Option<String>,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    address_env: Option<String>,
    #[serde(default, rename = "use")]
    use_: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UserSpec {
    Address(String),
    Details(UserDetails),
}

#[derive(Debug, Default, Deserialize)]
struct UserDetails {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    aliases: StringList,
}

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum StringList {
    One(String),
    Many(Vec<String>),
    #[default]
    Empty,
}

fn string_list(value: &StringList) -> Vec<String> {
    match value {
        StringList::One(value) => vec![value.trim().to_owned()],
        StringList::Many(values) => values.iter().map(|value| value.trim().to_owned()).collect(),
        StringList::Empty => Vec::new(),
    }
    .into_iter()
    .filter(|value| !value.is_empty())
    .collect()
}

fn dedupe_nonempty(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for value in values {
        if !value.trim().is_empty() && seen.insert(value.clone()) {
            result.push(value);
        }
    }
    result
}

fn expand_email_config_path(path: &str) -> PathBuf {
    if path == "~" {
        return env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    Path::new(path).to_path_buf()
}

pub fn normalize_webhook_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        return DEFAULT_EMAIL_WEBHOOK_PATH.to_owned();
    }
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

pub fn normalize_explicit_session_id(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() < 6
        || !value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
    {
        return None;
    }
    Some(value)
}

pub fn extract_routed_session_id(body_text: &str) -> Option<String> {
    body_text
        .replace("\r\n", "\n")
        .lines()
        .rev()
        .find_map(routed_session_from_line)
}

fn routed_session_from_line(line: &str) -> Option<String> {
    let line = line.trim().trim_start_matches('>').trim();
    let rest = line.strip_prefix("SM:")?.trim();
    let parts = rest.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 3 {
        return None;
    }
    normalize_explicit_session_id(parts[parts.len() - 2])
}

pub fn extract_reply_message_body(body_text: &str) -> String {
    let normalized = body_text.replace("\r\n", "\n");
    let mut body_lines = Vec::new();
    for line in normalized.trim().lines() {
        let trimmed = line.trim();
        if line.starts_with('>')
            || (trimmed.starts_with("On ") && trimmed.ends_with("wrote:"))
            || line.starts_with("From:")
            || line.starts_with("Sent:")
            || line.starts_with("Subject:")
            || line.starts_with("To:")
        {
            break;
        }
        body_lines.push(line.to_owned());
    }
    while body_lines.last().is_some_and(|line| line.trim().is_empty()) {
        body_lines.pop();
    }
    if body_lines
        .last()
        .and_then(|line| routed_session_from_line(line))
        .is_some()
    {
        body_lines.pop();
        while body_lines.last().is_some_and(|line| line.trim().is_empty()) {
            body_lines.pop();
        }
        if body_lines.last().is_some_and(|line| line.trim() == "--") {
            body_lines.pop();
        }
    }
    body_lines.join("\n").trim().to_owned()
}

pub fn extract_text_from_raw_email(raw_email: &str) -> String {
    let normalized = raw_email.trim();
    if normalized.is_empty() {
        return String::new();
    }
    let (_, fallback_body) = split_headers_body(normalized);
    extract_mime_text(normalized)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback_body.trim().to_owned())
}

pub fn extract_subject_from_raw_email(raw_email: &str) -> Option<String> {
    for line in raw_email.lines() {
        if line.trim().is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("subject") {
            let subject = value.split_whitespace().collect::<Vec<_>>().join(" ");
            return (!subject.is_empty()).then_some(subject);
        }
    }
    None
}

fn extract_mime_text(raw_email: &str) -> Option<String> {
    let (headers, body) = split_headers_body(raw_email);
    let headers = parse_headers(headers);
    if header_contains(&headers, "content-disposition", "attachment") {
        return None;
    }

    let content_type = headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("text/plain");
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or("text/plain")
        .trim()
        .to_ascii_lowercase();
    if media_type.starts_with("multipart/") {
        return extract_multipart_text(body, content_type);
    }

    let decoded = decode_transfer_encoded_body(
        body,
        headers
            .get("content-transfer-encoding")
            .map(String::as_str)
            .unwrap_or(""),
    );
    if media_type == "text/html" {
        return Some(strip_html(&decoded));
    }
    Some(decoded.trim().to_owned())
}

fn extract_multipart_text(body: &str, content_type: &str) -> Option<String> {
    let boundary = content_type_param(content_type, "boundary")?;
    let mut html_fallback = None;
    for part in multipart_parts(body, &boundary) {
        let (part_headers_raw, part_body) = split_headers_body(&part);
        let part_headers = parse_headers(part_headers_raw);
        if header_contains(&part_headers, "content-disposition", "attachment") {
            continue;
        }
        let part_content_type = part_headers
            .get("content-type")
            .map(String::as_str)
            .unwrap_or("text/plain");
        let media_type = part_content_type
            .split(';')
            .next()
            .unwrap_or("text/plain")
            .trim()
            .to_ascii_lowercase();
        if media_type.starts_with("multipart/") {
            let nested = extract_mime_text(&part);
            if nested
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                return nested;
            }
            continue;
        }
        let decoded = decode_transfer_encoded_body(
            part_body,
            part_headers
                .get("content-transfer-encoding")
                .map(String::as_str)
                .unwrap_or(""),
        );
        if media_type == "text/plain" {
            let decoded = decoded.trim().to_owned();
            if !decoded.is_empty() {
                return Some(decoded);
            }
        } else if media_type == "text/html" && html_fallback.is_none() {
            let html = strip_html(&decoded);
            if !html.trim().is_empty() {
                html_fallback = Some(html);
            }
        }
    }
    html_fallback
}

fn split_headers_body(raw: &str) -> (&str, &str) {
    raw.split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .unwrap_or(("", raw))
}

fn parse_headers(raw: &str) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    let mut current_name: Option<String> = None;
    for line in raw.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(name) = current_name.as_ref() {
                let value = headers.entry(name.clone()).or_insert_with(String::new);
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        headers.insert(name.clone(), value.trim().to_owned());
        current_name = Some(name);
    }
    headers
}

fn header_contains(headers: &BTreeMap<String, String>, name: &str, needle: &str) -> bool {
    headers
        .get(name)
        .is_some_and(|value| value.to_ascii_lowercase().contains(needle))
}

fn content_type_param(content_type: &str, name: &str) -> Option<String> {
    for part in content_type.split(';').skip(1) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

fn multipart_parts(body: &str, boundary: &str) -> Vec<String> {
    let delimiter = format!("--{boundary}");
    let closing = format!("--{boundary}--");
    let normalized = body.replace("\r\n", "\n");
    let mut parts = Vec::new();
    let mut current = Vec::new();
    let mut in_part = false;
    for line in normalized.lines() {
        let marker = line.trim_end();
        if marker == closing {
            if in_part && !current.is_empty() {
                parts.push(current.join("\n"));
            }
            break;
        }
        if marker == delimiter {
            if in_part && !current.is_empty() {
                parts.push(current.join("\n"));
                current.clear();
            }
            in_part = true;
            continue;
        }
        if in_part {
            current.push(line.to_owned());
        }
    }
    parts
}

fn decode_transfer_encoded_body(body: &str, encoding: &str) -> String {
    match encoding.trim().to_ascii_lowercase().as_str() {
        "base64" => {
            let compact = body.split_whitespace().collect::<String>();
            STANDARD
                .decode(compact.as_bytes())
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_else(|| body.trim().to_owned())
        }
        "quoted-printable" => decode_quoted_printable(body),
        _ => body.trim().to_owned(),
    }
}

fn decode_quoted_printable(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'=' {
            if index + 1 < bytes.len() && bytes[index + 1] == b'\n' {
                index += 2;
                continue;
            }
            if index + 2 < bytes.len() && bytes[index + 1] == b'\r' && bytes[index + 2] == b'\n' {
                index += 3;
                continue;
            }
            if index + 2 < bytes.len() {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
                {
                    output.push((high << 4) | low);
                    index += 3;
                    continue;
                }
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).trim().to_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn build_routing_footer(
    sender_name: &str,
    sender_session_id: &str,
    sender_provider: &str,
) -> String {
    format!(
        "SM: {} {} {}",
        collapse_whitespace(sender_name).unwrap_or_else(|| "session".to_owned()),
        sender_session_id,
        collapse_whitespace(sender_provider).unwrap_or_else(|| "unknown".to_owned())
    )
}

fn append_routing_footer(body_text: &str, body_html: &str, footer: &str) -> (String, String) {
    let text = if body_text.trim().is_empty() {
        format!("--\n{footer}")
    } else {
        format!("{}\n\n--\n{footer}", body_text.trim_end())
    };
    let html_footer = format!("<hr/>\n<p>{}</p>", escape_html(footer));
    let html = if body_html.trim().is_empty() {
        html_footer
    } else {
        format!("{}\n{html_footer}", body_html.trim_end())
    };
    (text, html)
}

fn default_subject(sender_name: &str, body_text: &str) -> String {
    let mut subject = String::new();
    for line in body_text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let line = line.trim().trim_start_matches(['#', '*', '-', ' ']).trim();
        if !line.is_empty() {
            subject = line.to_owned();
            break;
        }
    }
    if subject.is_empty() {
        subject = format!(
            "Message from {}",
            if sender_name.is_empty() {
                "Session Manager"
            } else {
                sender_name
            }
        );
    }
    subject.chars().take(MAX_EMAIL_SUBJECT_LENGTH).collect()
}

fn collapse_whitespace(value: &str) -> Option<String> {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

fn plain_text_to_html(body_text: &str) -> String {
    let paragraphs = body_text
        .split("\n\n")
        .map(str::trim)
        .filter(|chunk| !chunk.is_empty())
        .map(|paragraph| format!("<p>{}</p>", escape_html(paragraph).replace('\n', "<br/>")))
        .collect::<Vec<_>>();
    if paragraphs.is_empty() {
        "<p></p>".to_owned()
    } else {
        paragraphs.join("\n")
    }
}

fn render_markdown_to_html(body_text: &str) -> String {
    plain_text_to_html(body_text)
}

fn strip_html(body_html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    for ch in body_html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' => {
                in_tag = false;
                if html_tag_breaks_text(&tag) && !result.ends_with('\n') {
                    result.push('\n');
                }
            }
            _ if in_tag => tag.push(ch),
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}

fn html_tag_breaks_text(tag: &str) -> bool {
    let tag = tag
        .trim()
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        tag.as_str(),
        "br" | "div" | "p" | "li" | "tr" | "table" | "hr"
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_last_routing_footer_session_id() {
        let body = "hello\n\n--\nSM: Agent Name abc123 codex-fork\n\n> SM: old old123 claude";
        assert_eq!(extract_routed_session_id(body).as_deref(), Some("old123"));
    }

    #[test]
    fn strips_footer_and_quoted_reply() {
        let body = "Thanks\n\n--\nSM: Agent abc123 claude\n\n> older";
        assert_eq!(extract_reply_message_body(body), "Thanks");
    }

    #[test]
    fn extracts_text_plain_from_multipart_raw_email() {
        let raw = concat!(
            "Subject: Re: status\r\n",
            "Content-Type: multipart/alternative; boundary=\"sm-boundary\"\r\n",
            "\r\n",
            "--sm-boundary\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<p>html should not win</p>\r\n",
            "--sm-boundary\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            "\r\n",
            "Here=20is=20the=20reply=0A=0A--=0ASM:=20Runner=20run12345=20claude\r\n",
            "--sm-boundary--\r\n",
        );

        let text = extract_text_from_raw_email(raw);

        assert_eq!(
            extract_routed_session_id(&text).as_deref(),
            Some("run12345")
        );
        assert_eq!(extract_reply_message_body(&text), "Here is the reply");
        assert!(!text.contains("html should not win"));
    }

    #[test]
    fn decodes_base64_html_raw_email_when_no_text_part_exists() {
        let html = "<p>Fallback reply</p><p>SM: Runner run12345 claude</p>";
        let raw = format!(
            "Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
            STANDARD.encode(html.as_bytes())
        );

        let text = extract_text_from_raw_email(&raw);

        assert!(text.contains("Fallback reply"));
        assert_eq!(
            extract_routed_session_id(&text).as_deref(),
            Some("run12345")
        );
    }
}
