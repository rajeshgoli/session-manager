//! Owner messages (sm#1580): markdown an agent sends the owner with
//! `sm send <person>`, the owner's passage comments on it, and the replies
//! that go back to the agent. Rows live in the retained queue DB
//! (`sm_send.db_path`) beside owner docs, so a reply is recorded and queued
//! for the agent in one transaction. Spec:
//! `specs/1580_app_messages_replace_email.html`, appendices A, C1 and D.

use std::{collections::BTreeSet, fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Longest message body, in characters (Unicode scalar values).
pub const MAX_MESSAGE_CHARS: usize = 25_000;
/// Longest title, given or derived, in characters.
pub const MAX_TITLE_CHARS: usize = 120;
/// Unread messages one sender may have waiting for one human.
pub const UNREAD_CAP: i64 = 5;
/// Passage comments one message may hold before a reply sends them.
pub const MAX_DRAFTS_PER_MESSAGE: usize = 100;
/// How much of a passage the reply quotes back to the agent.
const MAX_REPLY_QUOTE_CHARS: usize = 300;

pub const TOO_LONG_DETAIL: &str =
    "too long for a message (25,000 characters); publish it as a doc with sm doc publish";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerMessage {
    pub id: String,
    pub human: String,
    pub sender_session_id: String,
    pub sender_session_name: String,
    pub title: String,
    pub body_markdown: String,
    pub blocking: bool,
    pub created_at: String,
    pub first_viewed_at: Option<String>,
    pub handled_at: Option<String>,
    pub handled_via: Option<String>,
}

/// A passage comment the owner has not sent yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerMessageDraft {
    pub id: String,
    pub message_id: String,
    /// Source line of the markdown; `None` is not placeable.
    pub line: Option<i64>,
    pub quote: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One passage comment as a reply delivered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyComment {
    pub line: Option<i64>,
    pub quote: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerMessageReply {
    /// The client's `submission_id`.
    pub id: String,
    pub message_id: String,
    pub body: String,
    pub comments: Vec<ReplyComment>,
    pub delivered_text: String,
    pub delivered_to_session_id: String,
    pub created_at: String,
}

/// Derived message state (appendix A); there is no state column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerMessageState {
    New,
    Read,
    NeedsYou,
    Replied,
    Handled,
}

impl OwnerMessageState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Read => "read",
            Self::NeedsYou => "needs_you",
            Self::Replied => "replied",
            Self::Handled => "handled",
        }
    }
}

/// First match wins: replied, handled, needs you (blocking and the sender
/// has not ended), read, new.
pub fn derive_message_state(
    message: &OwnerMessage,
    replied: bool,
    sender_ended: bool,
) -> OwnerMessageState {
    if replied {
        OwnerMessageState::Replied
    } else if message.handled_at.is_some() {
        OwnerMessageState::Handled
    } else if message.blocking && !sender_ended {
        OwnerMessageState::NeedsYou
    } else if message.first_viewed_at.is_some() {
        OwnerMessageState::Read
    } else {
        OwnerMessageState::New
    }
}

pub fn is_owner_message_id(value: &str) -> bool {
    value.strip_prefix("msg_").is_some_and(|hex| {
        hex.len() == 8
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// A submitted prompt that did not come from sm's own delivery queue.
pub fn is_owner_typed_prompt(prompt: &str, recently_delivered: bool) -> bool {
    let prompt = prompt.trim();
    !prompt.is_empty()
        && !prompt.starts_with("[Input from:")
        && !prompt.starts_with("[sm")
        && !recently_delivered
}

pub fn message_reader_path(id: &str) -> String {
    format!("/messages/{id}")
}

/// A `--title`: trimmed, 1-120 characters.
pub fn validate_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        bail!("title must be 1-{MAX_TITLE_CHARS} characters");
    }
    Ok(title.to_owned())
}

/// The title when none is given (appendix D2): the first ATX or setext
/// heading within the first 10 non-blank lines, else the first non-blank
/// line without leading list/quote/heading markers and inline markup.
/// Whitespace is collapsed and the result cut at 120 characters.
pub fn derive_title(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut heading = None;
    let mut non_blank = 0;
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        non_blank += 1;
        if non_blank > 10 {
            break;
        }
        if let Some(text) = atx_heading(line) {
            heading = Some(text);
            break;
        }
        if lines
            .get(index + 1)
            .is_some_and(|next| is_setext_underline(next))
            && !is_setext_underline(line)
        {
            heading = Some(line.trim().to_owned());
            break;
        }
    }
    let raw = heading.unwrap_or_else(|| {
        lines
            .iter()
            .find(|line| !line.trim().is_empty())
            .map(|line| strip_inline_markup(strip_leading_markers(line)))
            .unwrap_or_default()
    });
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = match collapsed.char_indices().nth(MAX_TITLE_CHARS) {
        Some((cut, _)) => format!("{}…", &collapsed[..cut]),
        None => collapsed,
    };
    if title.is_empty() {
        "Message".to_owned()
    } else {
        title
    }
}

/// `# Title #` → `Title`: one to six `#`, then a space or the line's end.
fn atx_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim();
    Some(text.to_owned())
}

fn is_setext_underline(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && (trimmed.bytes().all(|b| b == b'=') || trimmed.bytes().all(|b| b == b'-'))
}

/// Leading `#`, `>`, `-`, `*` and `1.`-style list markers, repeatedly.
fn strip_leading_markers(line: &str) -> &str {
    let mut rest = line.trim_start();
    loop {
        if let Some(stripped) = rest.strip_prefix(['#', '>', '-', '*']) {
            rest = stripped.trim_start();
            continue;
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            let after = &rest[digits..];
            if let Some(stripped) = after.strip_prefix('.') {
                if stripped.is_empty() || stripped.starts_with([' ', '\t']) {
                    rest = stripped.trim_start();
                    continue;
                }
            }
        }
        return rest;
    }
}

fn strip_inline_markup(text: &str) -> String {
    text.chars()
        .filter(|ch| !matches!(ch, '*' | '_' | '`'))
        .collect()
}

/// What the agent receives for a reply (appendix D4). `comments` are in
/// delivered order.
pub fn render_delivered_text(
    owner_name: &str,
    message: &OwnerMessage,
    body: &str,
    comments: &[ReplyComment],
) -> String {
    let mut sections = Vec::new();
    let body = body.trim();
    if !body.is_empty() {
        sections.push(body.to_owned());
    }
    for comment in comments {
        let quote = comment.quote.trim();
        if quote.is_empty() {
            sections.push(comment.body.clone());
            continue;
        }
        sections.push(format!("{}\n{}", quote_block(quote), comment.body));
    }
    format!(
        "[Input from: {owner_name} via sm app] Re: \"{}\" ({})\n{}",
        message.title,
        message.id,
        sections.join("\n\n")
    )
}

/// What the agent receives for a send from its Inbox thread (sm#1647): the
/// quoted passages, then the text. `message` is the message the send answers;
/// `None` is a note the owner wrote first, which has no `Re:` part.
pub fn render_thread_text(
    owner_name: &str,
    message: Option<&OwnerMessage>,
    body: &str,
    quotes: &[String],
) -> String {
    let mut sections: Vec<String> = quotes
        .iter()
        .map(|quote| quote.trim())
        .filter(|quote| !quote.is_empty())
        .map(quote_block)
        .collect();
    let body = body.trim();
    if !body.is_empty() {
        sections.push(body.to_owned());
    }
    let header = match message {
        Some(message) => format!(
            "[Input from: {owner_name} via sm app] Re: \"{}\" ({})",
            message.title, message.id
        ),
        None => format!("[Input from: {owner_name} via sm app]"),
    };
    format!("{header}\n{}", sections.join("\n\n"))
}

/// `> `-prefixed lines, cut at [`MAX_REPLY_QUOTE_CHARS`].
fn quote_block(quote: &str) -> String {
    let quote = match quote.char_indices().nth(MAX_REPLY_QUOTE_CHARS) {
        Some((cut, _)) => format!("{}…", &quote[..cut]),
        None => quote.to_owned(),
    };
    quote
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drafts in delivered order: by line, unplaceable ones last, then by age.
pub fn order_reply_comments(drafts: &[OwnerMessageDraft]) -> Vec<ReplyComment> {
    let mut ordered: Vec<&OwnerMessageDraft> = drafts.iter().collect();
    ordered.sort_by(|a, b| {
        let key = |draft: &OwnerMessageDraft| (draft.line.is_none(), draft.line.unwrap_or(0));
        key(a)
            .cmp(&key(b))
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    ordered
        .into_iter()
        .map(|draft| ReplyComment {
            line: draft.line,
            quote: draft.quote.clone(),
            body: draft.body.clone(),
        })
        .collect()
}

pub fn init_owner_messages_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS owner_messages (
            id TEXT PRIMARY KEY,
            human TEXT NOT NULL,
            sender_session_id TEXT NOT NULL,
            sender_session_name TEXT NOT NULL,
            title TEXT NOT NULL,
            body_markdown TEXT NOT NULL,
            blocking INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            first_viewed_at TEXT,
            handled_at TEXT,
            handled_via TEXT
        );
        CREATE INDEX IF NOT EXISTS owner_messages_sender
            ON owner_messages(sender_session_id, created_at);
        CREATE TABLE IF NOT EXISTS owner_message_drafts (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL REFERENCES owner_messages(id),
            line INTEGER,
            quote TEXT NOT NULL,
            body TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS owner_message_replies (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL REFERENCES owner_messages(id),
            body TEXT NOT NULL,
            comments_json TEXT NOT NULL,
            delivered_text TEXT NOT NULL,
            delivered_to_session_id TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS owner_message_replies_message
            ON owner_message_replies(message_id, created_at);
        "#,
    )?;
    let has_via = conn
        .prepare("PRAGMA table_info(owner_messages)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|column| column == "handled_via");
    if !has_via {
        conn.execute_batch("ALTER TABLE owner_messages ADD COLUMN handled_via TEXT")?;
    }
    Ok(())
}

/// What `POST /humans/{id}/messages` stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOwnerMessage {
    pub human: String,
    pub sender_session_id: String,
    pub sender_session_name: String,
    pub title: String,
    pub body_markdown: String,
    pub blocking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateOwnerMessage {
    Created(Box<OwnerMessage>),
    /// The sender already has [`UNREAD_CAP`] unread messages to this human.
    UnreadCapReached,
}

/// A reply ready to record: everything is computed before the transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordReply {
    pub submission_id: String,
    pub message_id: String,
    pub body: String,
    pub comments: Vec<ReplyComment>,
    pub delivered_text: String,
    pub recipient_session_id: String,
    /// The drafts the reply carries; deleted with the insert.
    pub draft_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct OwnerMessageStore {
    db_path: PathBuf,
}

const MESSAGE_COLUMNS: &str = "id, human, sender_session_id, sender_session_name, title, \
     body_markdown, blocking, created_at, first_viewed_at, handled_at, handled_via";
const LEGACY_MESSAGE_COLUMNS: &str = "id, human, sender_session_id, sender_session_name, title, \
     body_markdown, blocking, created_at, first_viewed_at, handled_at, NULL AS handled_via";

fn message_columns(conn: &Connection) -> Result<&'static str> {
    let columns = conn
        .prepare("PRAGMA table_info(owner_messages)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(if columns.iter().any(|column| column == "handled_via") {
        MESSAGE_COLUMNS
    } else {
        LEGACY_MESSAGE_COLUMNS
    })
}
const DRAFT_COLUMNS: &str = "id, message_id, line, quote, body, created_at, updated_at";
const REPLY_COLUMNS: &str =
    "id, message_id, body, comments_json, delivered_text, delivered_to_session_id, created_at";

impl OwnerMessageStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open_write(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(&self.db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_owner_messages_schema(&conn)?;
        Ok(conn)
    }

    /// Read connections never create the DB; a missing one reads as empty.
    fn open_read(&self) -> Result<Option<Connection>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let has_tables: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' \
                 AND name = 'owner_message_replies'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        Ok(has_tables.then_some(conn))
    }

    pub fn ensure_schema(&self) -> Result<()> {
        self.open_write().map(|_| ())
    }

    /// Inserts the message unless the sender already has [`UNREAD_CAP`]
    /// unread messages to the same human. The check and the insert share
    /// one write transaction, so two racing sends cannot both pass it.
    pub fn create(&self, message: NewOwnerMessage) -> Result<CreateOwnerMessage> {
        self.create_once(message, None)
    }

    /// A durable producer retries with the same key until its notification is accepted.
    pub fn create_once(
        &self,
        message: NewOwnerMessage,
        key: Option<&str>,
    ) -> Result<CreateOwnerMessage> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS owner_message_delivery_keys (key TEXT PRIMARY KEY, message_id TEXT NOT NULL)")?;
        if let Some(key) = key {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT message_id FROM owner_message_delivery_keys WHERE key=?1",
                    [key],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                return Ok(CreateOwnerMessage::Created(Box::new(
                    get_message_conn(&tx, &id)?.context("delivered message missing")?,
                )));
            }
        }
        let unread: i64 = tx.query_row(
            "SELECT COUNT(*) FROM owner_messages \
             WHERE sender_session_id = ?1 AND human = ?2 AND first_viewed_at IS NULL",
            params![message.sender_session_id, message.human],
            |row| row.get(0),
        )?;
        if unread >= UNREAD_CAP {
            return Ok(CreateOwnerMessage::UnreadCapReached);
        }
        let id = allocate_message_id(&tx)?;
        tx.execute(
            "INSERT INTO owner_messages (id, human, sender_session_id, sender_session_name, \
               title, body_markdown, blocking, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                message.human,
                message.sender_session_id,
                message.sender_session_name,
                message.title,
                message.body_markdown,
                message.blocking,
                now_rfc3339()
            ],
        )?;
        if let Some(key) = key {
            tx.execute(
                "INSERT INTO owner_message_delivery_keys(key,message_id) VALUES (?1,?2)",
                params![key, id],
            )?;
        }
        let created = get_message_conn(&tx, &id)?.context("inserted message vanished")?;
        tx.commit()?;
        Ok(CreateOwnerMessage::Created(Box::new(created)))
    }

    pub fn get(&self, id: &str) -> Result<Option<OwnerMessage>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        get_message_conn(&conn, id)
    }

    /// The sender's messages to `human` nobody has opened yet.
    pub fn unread_count(&self, sender_session_id: &str, human: &str) -> Result<i64> {
        let Some(conn) = self.open_read()? else {
            return Ok(0);
        };
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM owner_messages \
             WHERE sender_session_id = ?1 AND human = ?2 AND first_viewed_at IS NULL",
            params![sender_session_id, human],
            |row| row.get(0),
        )?)
    }

    /// Sets `first_viewed_at` when unset. Returns the message.
    pub fn mark_viewed(&self, id: &str) -> Result<Option<OwnerMessage>> {
        let conn = self.open_write()?;
        conn.execute(
            "UPDATE owner_messages SET first_viewed_at = ?2 \
             WHERE id = ?1 AND first_viewed_at IS NULL",
            params![id, now_rfc3339()],
        )?;
        get_message_conn(&conn, id)
    }

    /// Handled: sets `handled_at`, and `first_viewed_at` when unset.
    pub fn mark_handled(&self, id: &str) -> Result<Option<OwnerMessage>> {
        self.mark_handled_via(id, "manual")
    }

    pub fn mark_handled_via(&self, id: &str, via: &str) -> Result<Option<OwnerMessage>> {
        let conn = self.open_write()?;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE owner_messages SET handled_at = IFNULL(handled_at, ?2), \
               first_viewed_at = IFNULL(first_viewed_at, ?2), \
               handled_via = IFNULL(handled_via, ?3) WHERE id = ?1",
            params![id, now, via],
        )?;
        get_message_conn(&conn, id)
    }

    /// Answer all open blocking questions from one agent without sending it a message.
    pub fn answer_session(&self, session_id: &str, via: &str) -> Result<usize> {
        let conn = self.open_write()?;
        let now = now_rfc3339();
        Ok(conn.execute(
            "UPDATE owner_messages SET handled_at = ?2, first_viewed_at = \
             IFNULL(first_viewed_at, ?2), handled_via = ?3 \
             WHERE sender_session_id = ?1 AND blocking = 1 AND handled_at IS NULL \
             AND NOT EXISTS (SELECT 1 FROM owner_message_replies r WHERE r.message_id = owner_messages.id)",
            params![session_id, now, via],
        )?)
    }

    /// Clear a blocking review notice when its request receives a reviewer.
    pub fn mark_handled_by_delivery_key(&self, key: &str) -> Result<()> {
        let conn = self.open_write()?;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE owner_messages SET handled_at = IFNULL(handled_at, ?2), \
               first_viewed_at = IFNULL(first_viewed_at, ?2), \
               handled_via = IFNULL(handled_via, 'inbox') WHERE id = (\
               SELECT message_id FROM owner_message_delivery_keys WHERE key = ?1)",
            params![key, now],
        )?;
        Ok(())
    }

    /// Unhandled messages from `sender_session_id` created under a delivery
    /// key starting with `prefix`, as (key suffix, message id), oldest first.
    pub fn open_keyed(
        &self,
        sender_session_id: &str,
        prefix: &str,
    ) -> Result<Vec<(String, String)>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let has_keys = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' \
                 AND name = 'owner_message_delivery_keys'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !has_keys {
            return Ok(Vec::new());
        }
        let mut statement = conn.prepare(
            "SELECT k.key, m.id FROM owner_message_delivery_keys k \
             JOIN owner_messages m ON m.id = k.message_id \
             WHERE m.sender_session_id = ?1 AND m.handled_at IS NULL \
             AND substr(k.key, 1, length(?2)) = ?2 ORDER BY m.created_at, m.id",
        )?;
        let rows = statement
            .query_map(params![sender_session_id, prefix], |row| {
                let key: String = row.get(0)?;
                Ok((key[prefix.len()..].to_owned(), row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Whether the message sent under `key` still waits on the owner: it exists
    /// and nobody handled it. False when no message was sent under `key`.
    pub fn is_open_by_delivery_key(&self, key: &str) -> Result<bool> {
        let Some(conn) = self.open_read()? else {
            return Ok(false);
        };
        let open = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM owner_message_delivery_keys k \
               JOIN owner_messages m ON m.id = k.message_id \
               WHERE k.key = ?1 AND m.handled_at IS NULL)",
            params![key],
            |row| row.get::<_, bool>(0),
        );
        match open {
            Ok(open) => Ok(open),
            // No keyed message was ever sent, so the table does not exist yet.
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.contains("no such table") =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Messages the obligations projection needs: every message created at
    /// or after `since`, plus older blocking messages nobody has answered.
    /// Each comes with whether it has a reply. Newest first.
    pub fn for_obligations(&self, since: &str) -> Result<Vec<(OwnerMessage, bool)>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let replied = replied_ids(&conn)?;
        let mut statement = conn.prepare(&format!(
            "SELECT {} FROM owner_messages \
             WHERE created_at >= ?1 OR (blocking = 1 AND handled_at IS NULL) \
             ORDER BY created_at DESC, rowid DESC",
            message_columns(&conn)?
        ))?;
        let rows = statement
            .query_map(params![since], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|message| {
                let has_reply = replied.contains(&message.id);
                let recent = message.created_at.as_str() >= since;
                (recent || !has_reply).then_some((message, has_reply))
            })
            .collect())
    }

    /// Messages created at or after `since`, oldest first: the notice repair
    /// pass's candidates.
    pub fn created_since(&self, since: &str) -> Result<Vec<OwnerMessage>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {} FROM owner_messages WHERE created_at >= ?1 \
             ORDER BY created_at, rowid",
            message_columns(&conn)?
        ))?;
        let rows = statement
            .query_map(params![since], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every message, oldest first: the Inbox's agent threads.
    pub fn all(&self) -> Result<Vec<OwnerMessage>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {} FROM owner_messages ORDER BY created_at, rowid",
            message_columns(&conn)?
        ))?;
        let rows = statement
            .query_map([], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every reply, oldest first.
    pub fn all_replies(&self) -> Result<Vec<OwnerMessageReply>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {REPLY_COLUMNS} FROM owner_message_replies ORDER BY created_at, rowid"
        ))?;
        let rows = statement
            .query_map([], reply_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Sets `first_viewed_at` on every unread message from the sender.
    pub fn mark_sender_viewed(&self, sender_session_id: &str) -> Result<usize> {
        Ok(self.open_write()?.execute(
            "UPDATE owner_messages SET first_viewed_at = ?2 \
             WHERE sender_session_id = ?1 AND first_viewed_at IS NULL",
            params![sender_session_id, now_rfc3339()],
        )?)
    }

    pub fn has_reply(&self, message_id: &str) -> Result<bool> {
        let Some(conn) = self.open_read()? else {
            return Ok(false);
        };
        Ok(conn
            .query_row(
                "SELECT 1 FROM owner_message_replies WHERE message_id = ?1 LIMIT 1",
                params![message_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn drafts(&self, message_id: &str) -> Result<Vec<OwnerMessageDraft>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        drafts_conn(&conn, message_id)
    }

    pub fn create_draft(
        &self,
        message_id: &str,
        line: Option<i64>,
        quote: &str,
        body: &str,
    ) -> Result<OwnerMessageDraft> {
        let conn = self.open_write()?;
        let now = now_rfc3339();
        let id = random_hex(6);
        conn.execute(
            "INSERT INTO owner_message_drafts \
               (id, message_id, line, quote, body, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, message_id, line, quote, body, now],
        )?;
        get_draft_conn(&conn, message_id, &id)?.context("created draft vanished")
    }

    pub fn update_draft(
        &self,
        message_id: &str,
        draft_id: &str,
        body: &str,
    ) -> Result<Option<OwnerMessageDraft>> {
        let conn = self.open_write()?;
        conn.execute(
            "UPDATE owner_message_drafts SET body = ?3, updated_at = ?4 \
             WHERE message_id = ?1 AND id = ?2",
            params![message_id, draft_id, body, now_rfc3339()],
        )?;
        get_draft_conn(&conn, message_id, draft_id)
    }

    pub fn delete_draft(&self, message_id: &str, draft_id: &str) -> Result<bool> {
        let conn = self.open_write()?;
        Ok(conn.execute(
            "DELETE FROM owner_message_drafts WHERE message_id = ?1 AND id = ?2",
            params![message_id, draft_id],
        )? > 0)
    }

    pub fn reply(&self, submission_id: &str) -> Result<Option<OwnerMessageReply>> {
        let Some(conn) = self.open_read()? else {
            return Ok(None);
        };
        get_reply_conn(&conn, submission_id)
    }

    /// The message's replies, oldest first.
    pub fn replies(&self, message_id: &str) -> Result<Vec<OwnerMessageReply>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT {REPLY_COLUMNS} FROM owner_message_replies WHERE message_id = ?1 \
             ORDER BY created_at, rowid"
        ))?;
        let rows = statement
            .query_map(params![message_id], reply_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Records the reply, deletes its drafts, marks the message viewed and
    /// queues the reply for the agent, all in one transaction. A second
    /// call with the same submission id returns the stored reply and
    /// `false`, queueing nothing.
    pub fn record_reply(&self, reply: &RecordReply) -> Result<(OwnerMessageReply, bool)> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = get_reply_conn(&tx, &reply.submission_id)? {
            return Ok((existing, false));
        }
        let now = now_rfc3339();
        tx.execute(
            &format!(
                "INSERT INTO owner_message_replies ({REPLY_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ),
            params![
                reply.submission_id,
                reply.message_id,
                reply.body,
                serde_json::to_string(&reply.comments)?,
                reply.delivered_text,
                reply.recipient_session_id,
                now
            ],
        )?;
        for draft_id in &reply.draft_ids {
            tx.execute(
                "DELETE FROM owner_message_drafts WHERE message_id = ?1 AND id = ?2",
                params![reply.message_id, draft_id],
            )?;
        }
        tx.execute(
            "UPDATE owner_messages SET first_viewed_at = ?2 \
             WHERE id = ?1 AND first_viewed_at IS NULL",
            params![reply.message_id, now],
        )?;
        crate::queue::enqueue_message_once_in_conn(
            &tx,
            &format!("owner-reply-{}", reply.submission_id),
            &reply.recipient_session_id,
            &reply.delivered_text,
        )?;
        let stored = get_reply_conn(&tx, &reply.submission_id)?.context("reply row vanished")?;
        tx.commit()?;
        Ok((stored, true))
    }
}

fn replied_ids(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut statement = conn.prepare("SELECT DISTINCT message_id FROM owner_message_replies")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    Ok(ids)
}

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<OwnerMessage> {
    Ok(OwnerMessage {
        id: row.get(0)?,
        human: row.get(1)?,
        sender_session_id: row.get(2)?,
        sender_session_name: row.get(3)?,
        title: row.get(4)?,
        body_markdown: row.get(5)?,
        blocking: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
        first_viewed_at: row.get(8)?,
        handled_at: row.get(9)?,
        handled_via: row.get(10)?,
    })
}

fn draft_from_row(row: &Row<'_>) -> rusqlite::Result<OwnerMessageDraft> {
    Ok(OwnerMessageDraft {
        id: row.get(0)?,
        message_id: row.get(1)?,
        line: row.get(2)?,
        quote: row.get(3)?,
        body: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn reply_from_row(row: &Row<'_>) -> rusqlite::Result<OwnerMessageReply> {
    let comments_json: String = row.get(3)?;
    Ok(OwnerMessageReply {
        id: row.get(0)?,
        message_id: row.get(1)?,
        body: row.get(2)?,
        comments: serde_json::from_str(&comments_json).unwrap_or_default(),
        delivered_text: row.get(4)?,
        delivered_to_session_id: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn get_message_conn(conn: &Connection, id: &str) -> Result<Option<OwnerMessage>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {} FROM owner_messages WHERE id = ?1",
                message_columns(conn)?
            ),
            params![id],
            message_from_row,
        )
        .optional()?)
}

fn drafts_conn(conn: &Connection, message_id: &str) -> Result<Vec<OwnerMessageDraft>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {DRAFT_COLUMNS} FROM owner_message_drafts WHERE message_id = ?1 \
         ORDER BY created_at, rowid"
    ))?;
    let rows = statement
        .query_map(params![message_id], draft_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn get_draft_conn(
    conn: &Connection,
    message_id: &str,
    draft_id: &str,
) -> Result<Option<OwnerMessageDraft>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {DRAFT_COLUMNS} FROM owner_message_drafts \
                 WHERE message_id = ?1 AND id = ?2"
            ),
            params![message_id, draft_id],
            draft_from_row,
        )
        .optional()?)
}

fn get_reply_conn(conn: &Connection, id: &str) -> Result<Option<OwnerMessageReply>> {
    Ok(conn
        .query_row(
            &format!("SELECT {REPLY_COLUMNS} FROM owner_message_replies WHERE id = ?1"),
            params![id],
            reply_from_row,
        )
        .optional()?)
}

fn allocate_message_id(conn: &Connection) -> Result<String> {
    for _ in 0..16 {
        let id = format!("msg_{}", random_hex(4));
        if get_message_conn(conn, &id)?.is_none() {
            return Ok(id);
        }
    }
    bail!("could not allocate a unique message id")
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Seconds precision, so stored times sort as text; ties fall back to rowid.
fn now_rfc3339() -> String {
    crate::owner_push::format_ts(OffsetDateTime::now_utc())
}

#[cfg(test)]
mod tests;
