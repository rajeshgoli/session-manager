//! Conversation-scoped event decoding, reconnect replay and usage journaling.
//! The session-store adapter applies effects and saves the checkpoint together.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{validate_id, Client};

const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Activity {
    #[default]
    Idle,
    Busy,
    Retry,
}

impl Activity {
    fn from_value(value: &Value) -> Result<Self> {
        match value["type"].as_str() {
            Some("idle") => Ok(Self::Idle),
            Some("busy") => Ok(Self::Busy),
            Some("retry") => Ok(Self::Retry),
            _ => bail!("unknown opencode activity"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub part_id: String,
    pub conversation_id: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn total_input(&self) -> Result<u64> {
        self.input
            .checked_add(self.cache_read)
            .and_then(|total| total.checked_add(self.cache_write))
            .context("opencode input token count overflow")
    }

    fn from_part(part: &Value) -> Result<Self> {
        let tokens = &part["tokens"];
        let count = |value: &Value| value.as_u64().context("invalid opencode token count");
        let usage = Self {
            part_id: part["id"].as_str().context("usage part missing id")?.into(),
            conversation_id: part["sessionID"]
                .as_str()
                .context("usage part missing session")?
                .into(),
            input: count(&tokens["input"])?,
            output: count(&tokens["output"])?
                .checked_add(count(&tokens["reasoning"])?)
                .context("opencode output token count overflow")?,
            cache_read: count(&tokens["cache"]["read"])?,
            cache_write: count(&tokens["cache"]["write"])?,
        };
        validate_id(&usage.part_id, "prt")?;
        validate_id(&usage.conversation_id, "ses")?;
        usage.total_input()?;
        Ok(usage)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    TurnStart {
        message_id: Option<String>,
        prompt: String,
    },
    TurnStop {
        message_id: Option<String>,
        text: String,
    },
    OwnerPrompt {
        message_id: String,
        text: String,
    },
    Tool {
        part_id: String,
        call_id: String,
        name: String,
        input: Value,
    },
    Usage(Usage),
    Title(String),
    Error(String),
    Compacted,
    Touch,
}

/// Host checkpoint, saved by the adapter only after the effects are applied.
/// A clear replaces this with a checkpoint for the new conversation; the
/// usage journal remains shared across all conversations of the sm session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Projection {
    pub conversation_id: String,
    pub cursor: Option<String>,
    pub activity: Activity,
    #[serde(default)]
    users: BTreeSet<String>,
    #[serde(default)]
    pending_owner_prompts: BTreeSet<String>,
    #[serde(default)]
    tools: BTreeSet<String>,
    #[serde(default)]
    usage: BTreeSet<String>,
    #[serde(default)]
    stops: BTreeSet<String>,
    #[serde(default)]
    text: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    last_user: String,
    #[serde(default)]
    last_user_id: Option<String>,
    #[serde(default)]
    last_assistant: Option<String>,
}

impl Projection {
    pub fn new(conversation_id: &str, activity: Activity) -> Result<Self> {
        validate_id(conversation_id, "ses")?;
        Ok(Self {
            conversation_id: conversation_id.into(),
            cursor: None,
            activity,
            users: BTreeSet::new(),
            pending_owner_prompts: BTreeSet::new(),
            tools: BTreeSet::new(),
            usage: BTreeSet::new(),
            stops: BTreeSet::new(),
            text: BTreeMap::new(),
            last_user: String::new(),
            last_user_id: None,
            last_assistant: None,
        })
    }

    /// Work on a clone. If applying effects fails, retain the old checkpoint
    /// and replay, using the store's idempotent turn/tool mutations and journal.
    pub fn live(
        &mut self,
        event: &Value,
        generated_user_ids: &BTreeSet<String>,
    ) -> Result<Vec<Effect>> {
        let props = &event["properties"];
        let session = props["sessionID"]
            .as_str()
            .or_else(|| props["part"]["sessionID"].as_str())
            .or_else(|| props["info"]["sessionID"].as_str())
            .or_else(|| props["info"]["id"].as_str());
        if session != Some(self.conversation_id.as_str()) {
            return Ok(Vec::new());
        }
        let mut effects = Vec::new();
        match event["type"].as_str() {
            Some("session.status") => {
                self.change_activity(Activity::from_value(&props["status"])?, None, &mut effects)
            }
            Some("message.updated") => {
                self.message(&props["info"], &[], generated_user_ids, &mut effects)?
            }
            Some("message.part.updated") => self.part(&props["part"], false, &mut effects)?,
            Some("session.updated") => {
                if let Some(title) = props["info"]["title"].as_str() {
                    effects.push(Effect::Title(title.into()));
                }
            }
            Some("session.error") => effects.push(Effect::Error(props["error"].to_string())),
            Some("session.compacted") => effects.push(Effect::Compacted),
            _ => effects.push(Effect::Touch),
        }
        if effects.is_empty() {
            effects.push(Effect::Touch);
        }
        Ok(effects)
    }

    /// Replay the cursor itself so an unfinished assistant can gain new parts.
    /// User messages are atomic submissions; unlike assistants, they do not
    /// carry time.completed in opencode. The first unfinished assistant holds
    /// the cursor while later owner messages can still be observed once.
    pub fn backfill(
        &mut self,
        messages: &[Value],
        activity: Activity,
        generated_user_ids: &BTreeSet<String>,
    ) -> Result<Vec<Effect>> {
        let mut ordered: Vec<_> = messages
            .iter()
            .filter(|message| {
                message["info"]["sessionID"].as_str() == Some(self.conversation_id.as_str())
            })
            .collect();
        ordered.sort_by(|left, right| {
            left["info"]["id"]
                .as_str()
                .cmp(&right["info"]["id"].as_str())
        });
        let cursor = self.cursor.clone();
        let mut effects = Vec::new();
        let mut held = false;
        for message in ordered {
            let info = &message["info"];
            let id = info["id"].as_str().context("backfill message missing id")?;
            validate_id(id, "msg")?;
            if cursor.as_deref().is_some_and(|cursor| id < cursor) {
                continue;
            }
            let parts = message["parts"]
                .as_array()
                .context("backfill message missing parts")?;
            self.message(info, parts, generated_user_ids, &mut effects)?;
            // Metadata can precede the user's text, even in a snapshot taken
            // during submission. Keep revisiting it until the reply is known.
            if self.pending_owner_prompts.contains(id) && !held {
                held = true;
                self.cursor = Some(id.into());
            }
            if info["role"] == "assistant" {
                // A submitted user message can precede processing. Only an
                // assistant or the final busy/retry status proves a turn
                // began. A previously applied stop must remain untouched.
                if self.activity == Activity::Idle && !self.stops.contains(id) {
                    self.change_activity(Activity::Busy, self.last_user_id.clone(), &mut effects);
                }
                self.last_assistant = Some(id.into());
                for part in parts {
                    self.part(part, true, &mut effects)?;
                }
                if info["time"]["completed"].as_u64().is_some() {
                    // An assistant message is one model request, not one
                    // agent turn. tool-calls completes that request while the
                    // tools and next request still belong to the same turn.
                    if info["finish"] != "tool-calls"
                        && (info["finish"].as_str().is_some() || activity == Activity::Idle)
                        && self.stops.insert(id.into())
                    {
                        self.activity = Activity::Idle;
                        effects.push(Effect::TurnStop {
                            message_id: Some(id.into()),
                            text: self.assistant_text(),
                        });
                    }
                } else if !held {
                    held = true;
                    self.cursor = Some(id.into());
                }
            }
            if !held {
                self.cursor = Some(id.into());
            }
        }
        self.change_activity(activity, None, &mut effects);
        Ok(effects)
    }

    fn message(
        &mut self,
        info: &Value,
        parts: &[Value],
        generated: &BTreeSet<String>,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let id = info["id"].as_str().context("opencode message missing id")?;
        validate_id(id, "msg")?;
        if info["role"] == "user" {
            let mut text = parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                text = self.message_text(id);
            }
            if !text.is_empty() || !self.users.contains(id) {
                self.last_user = text.clone();
            }
            let new_user = self.users.insert(id.into());
            if new_user {
                self.last_user_id = Some(id.into());
                if !generated.contains(id) {
                    self.pending_owner_prompts.insert(id.into());
                }
            }
            self.owner_prompt(id, &text, effects);
        } else if info["role"] == "assistant" {
            self.last_assistant = Some(id.into());
        }
        Ok(())
    }

    fn part(&mut self, part: &Value, backfill: bool, effects: &mut Vec<Effect>) -> Result<()> {
        if part["sessionID"].as_str() != Some(self.conversation_id.as_str()) {
            return Ok(());
        }
        let id = part["id"].as_str().context("opencode part missing id")?;
        validate_id(id, "prt")?;
        match part["type"].as_str() {
            Some("text") => {
                let message_id = part["messageID"]
                    .as_str()
                    .context("text part missing message id")?;
                self.text
                    .entry(message_id.into())
                    .or_default()
                    .insert(id.into(), part["text"].as_str().unwrap_or_default().into());
                let text = self.message_text(message_id);
                if self.last_user_id.as_deref() == Some(message_id) {
                    self.last_user = text.clone();
                }
                self.owner_prompt(message_id, &text, effects);
            }
            Some("tool")
                if part["state"]["status"] == "running"
                    || (backfill
                        && matches!(
                            part["state"]["status"].as_str(),
                            Some("completed" | "error")
                        )) =>
            {
                if !self.tools.contains(id) {
                    let call_id = part["callID"]
                        .as_str()
                        .context("tool part missing call id")?;
                    let name = part["tool"].as_str().context("tool part missing name")?;
                    self.tools.insert(id.into());
                    effects.push(Effect::Tool {
                        part_id: id.into(),
                        call_id: call_id.into(),
                        name: name.into(),
                        input: part["state"]["input"].clone(),
                    });
                }
            }
            Some("step-finish") if !self.usage.contains(id) => {
                let usage = Usage::from_part(part)?;
                self.usage.insert(id.into());
                effects.push(Effect::Usage(usage));
            }
            _ => {}
        }
        Ok(())
    }

    fn message_text(&self, message_id: &str) -> String {
        self.text
            .get(message_id)
            .map(|parts| parts.values().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }

    fn assistant_text(&self) -> String {
        self.last_assistant
            .as_ref()
            .map(|id| self.message_text(id))
            .unwrap_or_default()
    }

    fn owner_prompt(&mut self, message_id: &str, text: &str, effects: &mut Vec<Effect>) {
        if !text.is_empty() && self.pending_owner_prompts.remove(message_id) {
            effects.push(Effect::OwnerPrompt {
                message_id: message_id.into(),
                text: text.into(),
            });
        }
    }

    fn change_activity(
        &mut self,
        activity: Activity,
        message_id: Option<String>,
        effects: &mut Vec<Effect>,
    ) {
        if self.activity == Activity::Idle && activity != Activity::Idle {
            effects.push(Effect::TurnStart {
                message_id,
                prompt: self.last_user.clone(),
            });
        } else if self.activity != Activity::Idle && activity == Activity::Idle {
            let message_id = message_id.or_else(|| self.last_assistant.clone());
            if message_id
                .as_ref()
                .is_none_or(|id| self.stops.insert(id.clone()))
            {
                effects.push(Effect::TurnStop {
                    message_id,
                    text: self.assistant_text(),
                });
            }
        }
        self.activity = activity;
    }
}

impl Client {
    pub fn conversation_messages(&self, conversation_id: &str) -> Result<Vec<Value>> {
        validate_id(conversation_id, "ses")?;
        let value = super::read_json(self.get(
            &format!("/session/{conversation_id}/message"),
            self.request_timeout,
        )?)?;
        value
            .as_array()
            .cloned()
            .context("opencode returned invalid message history")
    }

    /// Bound each stream connection to twenty seconds, then reconnect and
    /// backfill. This also bounds a stalled reader's wait before it can check
    /// retirement. ureq's body timeout is total duration, not idle duration.
    pub fn event_stream(&self) -> Result<BufReader<impl std::io::Read + '_>> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .timeout_connect(Some(self.request_timeout))
            .timeout_recv_response(Some(self.request_timeout))
            .timeout_recv_body(Some(std::time::Duration::from_secs(20)))
            .build()
            .into();
        let response = agent
            .get(&format!("{}/event", self.base_url))
            .header("Authorization", &self.authorization)
            .call()?;
        super::require_success(&response)?;
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("text/event-stream") {
            bail!("opencode event response is not an event stream");
        }
        Ok(BufReader::new(response.into_body().into_reader()))
    }
}

/// Read one complete SSE frame; heartbeats and other fields carry no event.
/// Size limits apply to lines as well as frames before allocation can grow.
pub fn read_event(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut data = Vec::new();
    let mut size = 0;
    loop {
        let mut line = Vec::new();
        let read = (&mut *reader)
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(None);
        }
        size += read;
        if size > MAX_FRAME_BYTES || read > MAX_FRAME_BYTES {
            bail!("opencode event frame too large");
        }
        if !line.ends_with(b"\n") {
            return Ok(None);
        }
        let line = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if data.is_empty() {
                size = 0;
                continue;
            }
            return Ok(Some(serde_json::from_str(&data.join("\n"))?));
        }
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
        }
    }
}

/// One host writer per agent. Reopen reconstructs part-ID deduplication; an
/// incomplete final line is truncated before replay writes the complete part.
pub struct UsageJournal {
    file: File,
    seen: BTreeSet<String>,
}

impl UsageJournal {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let mut reader = BufReader::new(file.try_clone()?);
        let mut seen = BTreeSet::new();
        let mut offset = 0u64;
        loop {
            let mut line = Vec::new();
            let count = (&mut reader)
                .take((MAX_FRAME_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if count == 0 {
                break;
            }
            if count > MAX_FRAME_BYTES {
                bail!("opencode usage line too large");
            }
            if !line.ends_with(b"\n") {
                file.set_len(offset)?;
                file.sync_all()?;
                break;
            }
            let value: Value =
                serde_json::from_slice(&line).context("corrupt opencode usage journal")?;
            let id = value["message"]["id"]
                .as_str()
                .context("usage journal line missing part id")?;
            validate_id(id, "prt")?;
            seen.insert(id.into());
            offset += count as u64;
        }
        // The read handle shares the file offset. Explicit append semantics
        // avoid overwriting when an interrupted final line was truncated.
        use std::io::{Seek, SeekFrom};
        let mut file = file;
        file.seek(SeekFrom::End(0))?;
        Ok(Self { file, seen })
    }

    pub fn append(
        &mut self,
        usage: &Usage,
        cwd: &str,
        model_id: &str,
        timestamp: &str,
    ) -> Result<bool> {
        if self.seen.contains(&usage.part_id) {
            return Ok(false);
        }
        validate_id(&usage.part_id, "prt")?;
        validate_id(&usage.conversation_id, "ses")?;
        let row = json!({"type": "assistant", "timestamp": timestamp, "sessionId": usage.conversation_id,
            "cwd": cwd, "requestId": usage.part_id, "message": {"id": usage.part_id,
            "model": format!("local/{model_id}"), "role": "assistant", "content": [],
            "usage": {"input_tokens": usage.input, "output_tokens": usage.output,
                "cache_read_input_tokens": usage.cache_read, "cache_creation_input_tokens": usage.cache_write}}});
        let bytes = serde_json::to_vec(&row)?;
        use std::io::{Seek, SeekFrom};
        let offset = self.file.stream_position()?;
        let written = (|| -> std::io::Result<()> {
            self.file.write_all(&bytes)?;
            self.file.write_all(b"\n")?;
            self.file.sync_all()
        })();
        if let Err(error) = written {
            self.file.set_len(offset)?;
            self.file.seek(SeekFrom::End(0))?;
            self.file.sync_all()?;
            return Err(error.into());
        }
        self.seen.insert(usage.part_id.clone());
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
