//! Owner notes, revision history, and search (ticket #1833).
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use rand_core::RngCore;
use regex::RegexBuilder;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use time::{format_description::well_known::Rfc3339, macros::format_description, OffsetDateTime};

pub const MAX_BODY: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct NotesStore {
    path: PathBuf,
}

#[derive(Clone, Serialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: String,
    pub version: i64,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub updated_at: String,
    pub chars: usize,
}

#[derive(Serialize)]
pub struct Revision {
    pub version: i64,
    pub at: String,
}

#[derive(Serialize)]
pub struct RevisionBody {
    pub version: i64,
    pub at: String,
    pub body: String,
}

#[derive(Serialize)]
pub struct Match {
    pub start: usize,
    pub end: usize,
}

#[derive(Serialize)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub updated_at: String,
    pub chars: usize,
    pub snippet: String,
    pub matches: Vec<Match>,
}

pub enum Save {
    Saved(Note),
    Stale(Note),
    Missing,
}

impl NotesStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;
            CREATE TABLE IF NOT EXISTS notes(id TEXT PRIMARY KEY, title TEXT NOT NULL, body TEXT NOT NULL,
              version INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT);
            CREATE TABLE IF NOT EXISTS note_revisions(note_id TEXT NOT NULL, version INTEGER NOT NULL,
              body TEXT NOT NULL, at TEXT NOT NULL, PRIMARY KEY(note_id,version));
            CREATE INDEX IF NOT EXISTS notes_updated ON notes(deleted_at,updated_at DESC);")?;
        prune(&conn)?;
        Ok(conn)
    }

    pub fn list(&self) -> Result<Vec<NoteSummary>> {
        let conn = self.open()?;
        let mut stmt = conn.prepare("SELECT id,title,updated_at,body FROM notes WHERE deleted_at IS NULL ORDER BY updated_at DESC,id")?;
        let rows = stmt.query_map([], |r| {
            let body: String = r.get(3)?;
            Ok(NoteSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                updated_at: r.get(2)?,
                chars: body.chars().count(),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get(&self, id: &str) -> Result<Option<Note>> {
        let conn = self.open()?;
        read_note(&conn, id)
    }

    pub fn create(&self, body: &str, title: Option<&str>) -> Result<Note> {
        check_body(body)?;
        let conn = self.open()?;
        let mut random = [0u8; 16];
        rand_core::OsRng.fill_bytes(&mut random);
        let id = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let now = now();
        let title = note_title(body, title);
        conn.execute(
            "INSERT INTO notes VALUES (?1,?2,?3,1,?4,?4,NULL)",
            params![id, title, body, now],
        )?;
        conn.execute(
            "INSERT INTO note_revisions VALUES (?1,1,?2,?3)",
            params![id, body, now],
        )?;
        Ok(Note {
            id,
            title,
            body: body.to_owned(),
            version: 1,
            updated_at: now,
        })
    }

    pub fn save(&self, id: &str, body: &str, title: Option<&str>, expected: i64) -> Result<Save> {
        check_body(body)?;
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(old) = read_note(&tx, id)? else {
            return Ok(Save::Missing);
        };
        if old.version != expected {
            return Ok(Save::Stale(old));
        }
        let now = now();
        let version = old.version + 1;
        let title = match title {
            Some(title) => note_title(body, Some(title)),
            None if old.title == note_title(&old.body, None) => note_title(body, None),
            None => old.title.clone(),
        };
        tx.execute(
            "UPDATE notes SET title=?2,body=?3,version=?4,updated_at=?5 WHERE id=?1",
            params![id, title, body, version, now],
        )?;
        keep_revision(&tx, id, version, body, &now)?;
        prune(&tx)?;
        tx.commit()?;
        Ok(Save::Saved(Note {
            id: id.to_owned(),
            title,
            body: body.to_owned(),
            version,
            updated_at: now,
        }))
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        let conn = self.open()?;
        let changed = conn.execute(
            "UPDATE notes SET deleted_at=?2 WHERE id=?1 AND deleted_at IS NULL",
            params![id, now()],
        )?;
        prune(&conn)?;
        Ok(changed > 0)
    }

    pub fn revisions(&self, id: &str) -> Result<Option<Vec<Revision>>> {
        let conn = self.open()?;
        if read_note(&conn, id)?.is_none() {
            return Ok(None);
        }
        let mut stmt = conn.prepare(
            "SELECT version,at FROM note_revisions WHERE note_id=? ORDER BY version DESC",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(Revision {
                version: r.get(0)?,
                at: r.get(1)?,
            })
        })?;
        Ok(Some(rows.collect::<rusqlite::Result<Vec<_>>>()?))
    }

    pub fn revision(&self, id: &str, version: i64) -> Result<Option<RevisionBody>> {
        let conn = self.open()?;
        if read_note(&conn, id)?.is_none() {
            return Ok(None);
        }
        conn.query_row(
            "SELECT version,at,body FROM note_revisions WHERE note_id=?1 AND version=?2",
            params![id, version],
            |r| {
                Ok(RevisionBody {
                    version: r.get(0)?,
                    at: r.get(1)?,
                    body: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn search(&self, q: &str, regex: bool) -> Result<Vec<SearchHit>> {
        self.search_until(q, regex, Instant::now() + Duration::from_millis(100))
    }

    fn search_until(&self, q: &str, regex: bool, deadline: Instant) -> Result<Vec<SearchHit>> {
        if q.len() > 4096 {
            return Err(anyhow!("search query is too long"));
        }
        let pattern = if !q.is_empty() {
            Some(
                RegexBuilder::new(&if regex {
                    q.to_owned()
                } else {
                    regex::escape(q)
                })
                .case_insensitive(true)
                .size_limit(1_000_000)
                .build()?,
            )
        } else {
            None
        };
        let conn = self.open()?;
        let mut stmt=conn.prepare("SELECT id,title,body,updated_at FROM notes WHERE deleted_at IS NULL ORDER BY updated_at DESC,id")?;
        let mut rows = stmt.query([])?;
        let mut hits = Vec::new();
        while let Some(row) = rows.next()? {
            if Instant::now() > deadline {
                return Err(anyhow!("search timed out"));
            }
            let id: String = row.get(0)?;
            let title: String = row.get(1)?;
            let body: String = row.get(2)?;
            let updated_at: String = row.get(3)?;
            let first = if q.is_empty() {
                Some((0, 0))
            } else if let Some(pattern) = &pattern {
                pattern.find(&body).map(|m| (m.start(), m.end()))
            } else {
                None
            };
            let Some((start, _)) = first else {
                continue;
            };
            let snippet = if body.len() < 1200 {
                body.clone()
            } else {
                paragraph(&body, start).to_owned()
            };
            let matches = if q.is_empty() {
                Vec::new()
            } else if let Some(pattern) = &pattern {
                pattern
                    .find_iter(&snippet)
                    .take(50)
                    .map(|m| Match {
                        start: m.start(),
                        end: m.end(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            hits.push(SearchHit {
                id,
                title,
                updated_at,
                chars: body.chars().count(),
                snippet,
                matches,
            });
            if hits.len() == 100 {
                break;
            }
        }
        Ok(hits)
    }
}

fn read_note(conn: &Connection, id: &str) -> Result<Option<Note>> {
    conn.query_row(
        "SELECT id,title,body,version,updated_at FROM notes WHERE id=? AND deleted_at IS NULL",
        [id],
        |r| {
            Ok(Note {
                id: r.get(0)?,
                title: r.get(1)?,
                body: r.get(2)?,
                version: r.get(3)?,
                updated_at: r.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn check_body(body: &str) -> Result<()> {
    if body.len() > MAX_BODY {
        Err(anyhow!("note body exceeds 2 MB"))
    } else {
        Ok(())
    }
}
fn note_title(body: &str, title: Option<&str>) -> String {
    title
        .unwrap_or_else(|| {
            body.lines()
                .next()
                .unwrap_or_default()
                .trim_start_matches('#')
                .trim()
        })
        .chars()
        .take(80)
        .collect()
}
fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z"
        ))
        .expect("UTC timestamp")
}

fn keep_revision(conn: &Connection, id: &str, version: i64, body: &str, at: &str) -> Result<()> {
    let previous:Option<(i64,String,String)>=conn.query_row(
        "SELECT version,body,at FROM note_revisions WHERE note_id=? ORDER BY version DESC LIMIT 1",
        [id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((prev_version, prev_body, prev_at)) = previous {
        let age = OffsetDateTime::parse(at, &Rfc3339)? - OffsetDateTime::parse(&prev_at, &Rfc3339)?;
        if age < time::Duration::minutes(10) && changed_chars(&prev_body, body) <= 2000 {
            conn.execute(
                "DELETE FROM note_revisions WHERE note_id=?1 AND version=?2",
                params![id, prev_version],
            )?;
        }
    }
    conn.execute(
        "INSERT INTO note_revisions VALUES (?1,?2,?3,?4)",
        params![id, version, body, at],
    )?;
    Ok(())
}

fn changed_chars(before: &str, after: &str) -> usize {
    let old: Vec<char> = before.chars().collect();
    let new: Vec<char> = after.chars().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (old.len() - prefix - suffix).max(new.len() - prefix - suffix)
}

fn prune(conn: &Connection) -> Result<()> {
    let cutoff = (OffsetDateTime::now_utc() - time::Duration::days(90)).format(&Rfc3339)?;
    conn.execute("DELETE FROM note_revisions WHERE at<?1 AND (note_id,version) NOT IN (
        SELECT note_id,MAX(version) FROM note_revisions WHERE at<?1 GROUP BY note_id,substr(at,1,10))",
        params![cutoff])?;
    let purge = (OffsetDateTime::now_utc() - time::Duration::days(30)).format(&Rfc3339)?;
    conn.execute(
        "DELETE FROM note_revisions WHERE note_id IN (SELECT id FROM notes WHERE deleted_at<?1)",
        [&purge],
    )?;
    conn.execute("DELETE FROM notes WHERE deleted_at<?1", [&purge])?;
    Ok(())
}

fn paragraph(body: &str, pos: usize) -> &str {
    let mut fence: Option<usize> = None;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            if let Some(start) = fence.take() {
                if pos >= start && pos < offset + line.len() {
                    return body[start..offset + line.len()].trim_end_matches('\n');
                }
            } else {
                fence = Some(offset);
            }
        }
        offset += line.len();
    }
    if let Some(start) = fence {
        if pos >= start {
            return &body[start..];
        }
    }
    let start = body[..pos].rfind("\n\n").map_or(0, |n| n + 2);
    let end = body[pos..].find("\n\n").map_or(body.len(), |n| pos + n);
    &body[start..end]
}

pub fn import_parts(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with('#') || line.trim_end_matches(['\r', '\n']) == "---" {
            if !current.trim().is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            if line.trim_end_matches(['\r', '\n']) == "---" {
                continue;
            }
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> NotesStore {
        let mut random = [0u8; 8];
        rand_core::OsRng.fill_bytes(&mut random);
        let path =
            std::env::temp_dir().join(format!("sm-notes-test-{:x}.db", u64::from_ne_bytes(random)));
        NotesStore::new(path)
    }

    #[test]
    fn versions_revisions_restore_and_delete() {
        let store = fixture();
        let note = store.create("# First\ntext", None).unwrap();
        assert_eq!(note.title, "First");
        assert!(
            matches!(store.save(&note.id,"other",None,0).unwrap(), Save::Stale(current) if current.version == 1)
        );
        let changed = store.save(&note.id, "# Second\ntext", None, 1).unwrap();
        assert!(matches!(changed,Save::Saved(current) if current.version == 2));
        assert_eq!(store.revisions(&note.id).unwrap().unwrap().len(), 1);
        assert_eq!(
            store.revision(&note.id, 2).unwrap().unwrap().body,
            "# Second\ntext"
        );
        assert!(store.delete(&note.id).unwrap());
        assert!(store.get(&note.id).unwrap().is_none());
        assert!(store.search("", false).unwrap().is_empty());

        let custom = store.create("# Body title", Some("My title")).unwrap();
        let saved = store.save(&custom.id, "# Changed body", None, 1).unwrap();
        assert!(matches!(saved, Save::Saved(note) if note.title == "My title"));
    }

    #[test]
    fn simultaneous_saves_return_one_version_conflict() {
        use std::sync::{Arc, Barrier};
        let store = fixture();
        let note = store.create("original", None).unwrap();
        let gate = Arc::new(Barrier::new(3));
        let handles: Vec<_> = ["device one", "device two"]
            .into_iter()
            .map(|body| {
                let store = store.clone();
                let id = note.id.clone();
                let gate = gate.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    store.save(&id, body, None, 1).unwrap()
                })
            })
            .collect();
        gate.wait();
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Save::Saved(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Save::Stale(note) if note.version == 2))
                .count(),
            1
        );
    }

    #[test]
    fn revision_retention_keeps_old_or_large_changes_and_thins_old_days() {
        let store = fixture();
        let note = store.create("a", None).unwrap();
        let conn = store.open().unwrap();
        conn.execute(
            "UPDATE note_revisions SET at='2025-01-01T00:00:00Z' WHERE note_id=?",
            [&note.id],
        )
        .unwrap();
        drop(conn);
        let _ = store.save(&note.id, "b", None, 1).unwrap();
        assert_eq!(store.revisions(&note.id).unwrap().unwrap().len(), 2);
        let big = "x".repeat(2001);
        let _ = store.save(&note.id, &big, None, 2).unwrap();
        assert_eq!(store.revisions(&note.id).unwrap().unwrap().len(), 3);
        let conn = store.open().unwrap();
        conn.execute(
            "INSERT INTO note_revisions VALUES (?1,10,'old','2025-01-01T01:00:00Z')",
            [&note.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO note_revisions VALUES (?1,11,'later','2025-01-01T02:00:00Z')",
            [&note.id],
        )
        .unwrap();
        drop(conn);
        let revisions = store.revisions(&note.id).unwrap().unwrap();
        assert!(revisions.iter().any(|r| r.version == 11));
        assert!(!revisions.iter().any(|r| r.version == 10));
    }

    #[test]
    fn snippets_import_and_regex() {
        let store = fixture();
        let short = store.create("Hello World", None).unwrap();
        let long = format!(
            "{}\n\n```rust\nlet target = 1;\n```\n\nend",
            "padding".repeat(180)
        );
        let fenced = store.create(&long, None).unwrap();
        let hits = store.search("world", false).unwrap();
        assert_eq!(hits[0].id, short.id);
        assert_eq!(hits[0].snippet, "Hello World");
        assert_eq!((hits[0].matches[0].start, hits[0].matches[0].end), (6, 11));
        let hits = store.search("target\\s*=", true).unwrap();
        assert_eq!(hits[0].id, fenced.id);
        assert_eq!(hits[0].snippet, "```rust\nlet target = 1;\n```");
        assert!(store
            .search_until("target.*", true, Instant::now() - Duration::from_millis(1))
            .err()
            .unwrap()
            .to_string()
            .contains("timed out"));
        assert_eq!(
            import_parts("intro\n# One\nbody\n---\n# Two\nend"),
            vec!["intro\n", "# One\nbody\n", "# Two\nend"]
        );
    }
}
