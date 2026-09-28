//! Guestbook (sm#1603): an optional note an agent leaves as it finishes,
//! with `sm task-complete --sign-guestbook`, read at `/guestbook`. The agent
//! writes only the text; the server fills in who signed, on what model, and
//! the work the session claimed. Entries live in the queue DB beside the
//! work claims, and an agent may sign any number of times.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

/// The largest entry, in bytes of UTF-8.
pub const MAX_ENTRY_BYTES: usize = 16 * 1024;
/// Entries per page when the request names no limit.
pub const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// Why an entry is refused before anything is stored or completed; the CLI
/// checks the same rule before it calls the server.
pub fn validate_entry(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("Guestbook entry is empty.".to_owned());
    }
    if text.len() > MAX_ENTRY_BYTES {
        return Err(format!(
            "Guestbook entry is {} bytes; the limit is 16 KB ({MAX_ENTRY_BYTES} bytes).",
            text.len()
        ));
    }
    Ok(())
}

/// A ticket or PR the signing session claimed, as it stood at signing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedWork {
    pub repo: String,
    pub number: i64,
    pub kind: String,
    pub title: String,
}

/// What the server records when an agent signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEntry {
    pub session_id: String,
    pub session_name: String,
    pub provider: String,
    pub model: Option<String>,
    pub working_dir: String,
    /// `owner/name` slugs: every claimed repo plus the working
    /// directory's `origin`, lowercase and deduplicated.
    pub repos: Vec<String>,
    pub claims: Vec<ClaimedWork>,
    pub signed_at: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuestbookEntry {
    pub id: i64,
    pub session_id: String,
    pub session_name: String,
    pub provider: String,
    pub model: Option<String>,
    pub working_dir: String,
    pub repos: Vec<String>,
    pub claims: Vec<ClaimedWork>,
    pub signed_at: String,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct GuestbookQuery {
    /// `owner/name` or bare `name`, case-insensitive.
    pub repo: Option<String>,
    /// Only entries older than this id (the previous page's `next_before`).
    pub before: Option<i64>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuestbookPage {
    pub entries: Vec<GuestbookEntry>,
    pub next_before: Option<i64>,
}

pub fn init_guestbook_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS guestbook_entries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            session_name TEXT NOT NULL,
            provider TEXT NOT NULL,
            model TEXT,
            working_dir TEXT NOT NULL,
            repos_json TEXT NOT NULL,
            claims_json TEXT NOT NULL,
            signed_at TEXT NOT NULL,
            text TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS guestbook_entries_session
            ON guestbook_entries(session_id);
        "#,
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct GuestbookStore {
    db_path: PathBuf,
}

impl GuestbookStore {
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
        init_guestbook_schema(&conn)?;
        Ok(conn)
    }

    /// Reads never create the DB or the table; a missing one reads as empty.
    fn open_read(&self) -> Result<Option<Connection>> {
        if !self.db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let exists = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'guestbook_entries'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(exists.then_some(conn))
    }

    /// Stores the entry and returns its id.
    pub fn sign(&self, entry: &NewEntry) -> Result<i64> {
        validate_entry(&entry.text).map_err(anyhow::Error::msg)?;
        let conn = self.open_write()?;
        conn.execute(
            "INSERT INTO guestbook_entries
                (session_id, session_name, provider, model, working_dir,
                 repos_json, claims_json, signed_at, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                entry.session_id,
                entry.session_name,
                entry.provider,
                entry.model,
                entry.working_dir,
                serde_json::to_string(&entry.repos)?,
                serde_json::to_string(&entry.claims)?,
                entry.signed_at,
                entry.text,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Newest first.
    pub fn list(&self, query: &GuestbookQuery) -> Result<GuestbookPage> {
        let empty = GuestbookPage {
            entries: Vec::new(),
            next_before: None,
        };
        let Some(conn) = self.open_read()? else {
            return Ok(empty);
        };
        let limit = query.limit.clamp(1, MAX_LIMIT);
        let repo = query
            .repo
            .as_deref()
            .map(str::trim)
            .filter(|repo| !repo.is_empty())
            .map(str::to_ascii_lowercase);
        let mut statement = conn.prepare(
            "SELECT id, session_id, session_name, provider, model, working_dir,
                    repos_json, claims_json, signed_at, text
               FROM guestbook_entries
              WHERE ?1 IS NULL OR id < ?1
              ORDER BY id DESC",
        )?;
        let rows = statement.query_map(params![query.before], |row| {
            Ok((
                GuestbookEntry {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    session_name: row.get(2)?,
                    provider: row.get(3)?,
                    model: row.get(4)?,
                    working_dir: row.get(5)?,
                    repos: Vec::new(),
                    claims: Vec::new(),
                    signed_at: row.get(8)?,
                    text: row.get(9)?,
                },
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (mut entry, repos, claims) = row?;
            entry.repos = serde_json::from_str(&repos).unwrap_or_default();
            entry.claims = serde_json::from_str(&claims).unwrap_or_default();
            if repo
                .as_deref()
                .is_some_and(|wanted| !entry.repos.iter().any(|r| repo_matches(r, wanted)))
            {
                continue;
            }
            entries.push(entry);
            if entries.len() > limit {
                break;
            }
        }
        let next_before = (entries.len() > limit).then(|| {
            entries.truncate(limit);
            entries[limit - 1].id
        });
        Ok(GuestbookPage {
            entries,
            next_before,
        })
    }
}

/// The model a session last ran on, from the usage ledger's token rows
/// (read from the provider's transcript), for sessions whose record names
/// none, such as ones the owner started by hand. Best effort: a missing DB,
/// table or row gives `None`.
pub fn observed_model(usage_db: &Path, session_id: &str) -> Option<String> {
    if !usage_db.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(usage_db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.pragma_update(None, "busy_timeout", 5000).ok()?;
    conn.query_row(
        "SELECT model FROM seat_tokens WHERE seat_id = ?1
          ORDER BY bucket_ts DESC, updated_at DESC LIMIT 1",
        params![session_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .filter(|model| !model.trim().is_empty())
}

/// `wanted` (lowercase) names `slug` in full or by its name part.
fn repo_matches(slug: &str, wanted: &str) -> bool {
    let slug = slug.to_ascii_lowercase();
    slug == wanted || slug.rsplit('/').next() == Some(wanted)
}

/// An entry's markdown as HTML that cannot carry markup or script: raw HTML
/// renders as text, images render as their alt text (no remote loads), and
/// a link survives only with an `http(s)`, `mailto`, same-site or fragment
/// target.
pub fn render_entry_html(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut dropped_links = Vec::new();
    let events = Parser::new_ext(text, options).filter_map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Some(Event::Text(raw)),
        Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,
        Event::Start(Tag::Link { ref dest_url, .. }) => {
            let safe = safe_link(dest_url);
            dropped_links.push(!safe);
            safe.then_some(event)
        }
        Event::End(TagEnd::Link) => {
            if dropped_links.pop().unwrap_or(false) {
                None
            } else {
                Some(event)
            }
        }
        other => Some(other),
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    html
}

fn safe_link(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("mailto:")
        || lower.starts_with('#')
        || (lower.starts_with('/') && !lower.starts_with("//"))
}

#[cfg(test)]
mod tests;
