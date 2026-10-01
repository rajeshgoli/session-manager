//! Notes the owner pins to an agent (sm#1851), such as "waiting for the
//! midnight window". While one is pinned, an idle agent with a ticket is not
//! called stalled: the Agents page lists it as waiting and the Board shows
//! the note. One note per agent, kept until the owner removes it. Rows live
//! in the retained queue DB (`sm_send.db_path`).

use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

/// Longest note, in characters.
pub const MAX_NOTE_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentNote {
    pub text: String,
    pub at: String,
}

#[derive(Debug, Clone)]
pub struct AgentNoteStore {
    db_path: PathBuf,
}

impl AgentNoteStore {
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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_notes (session_id TEXT PRIMARY KEY, \
             text TEXT NOT NULL, at TEXT NOT NULL)",
        )?;
        Ok(conn)
    }

    /// Pins `text` to the agent, replacing any note; blank text removes it.
    pub fn set(&self, session_id: &str, text: &str, at: &str) -> Result<Option<AgentNote>> {
        let text = text.trim();
        let conn = self.open_write()?;
        if text.is_empty() {
            conn.execute(
                "DELETE FROM agent_notes WHERE session_id = ?1",
                params![session_id],
            )?;
            return Ok(None);
        }
        let text: String = text.chars().take(MAX_NOTE_CHARS).collect();
        conn.execute(
            "INSERT INTO agent_notes (session_id, text, at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(session_id) DO UPDATE SET text = excluded.text, at = excluded.at",
            params![session_id, text, at],
        )?;
        Ok(Some(AgentNote {
            text,
            at: at.to_owned(),
        }))
    }

    /// Every pinned note, by session. A missing database reads as none.
    pub fn all(&self) -> Result<BTreeMap<String, AgentNote>> {
        if !self.db_path.exists() {
            return Ok(BTreeMap::new());
        }
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'agent_notes'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Ok(BTreeMap::new());
        }
        let mut statement = conn.prepare("SELECT session_id, text, at FROM agent_notes")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    AgentNote {
                        text: row.get(1)?,
                        at: row.get(2)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::{OsRng, RngCore};

    #[test]
    fn a_note_is_set_replaced_cut_and_removed() {
        let dir = std::env::temp_dir().join(format!("sm-agent-notes-{}", OsRng.next_u64()));
        let store = AgentNoteStore::new(dir.join("message_queue.db"));
        assert!(store.all().unwrap().is_empty());
        store
            .set(
                "s1",
                "Waiting for the midnight window",
                "2026-10-01T03:30:00Z",
            )
            .unwrap();
        store
            .set("s1", &"x".repeat(300), "2026-10-01T03:31:00Z")
            .unwrap();
        let notes = store.all().unwrap();
        assert_eq!(notes["s1"].text.chars().count(), MAX_NOTE_CHARS);
        assert_eq!(notes["s1"].at, "2026-10-01T03:31:00Z");
        assert_eq!(store.set("s1", "  ", "2026-10-01T03:32:00Z").unwrap(), None);
        assert!(store.all().unwrap().is_empty());
    }
}
