//! Where a note's text went. Snapshots survive later edits and revision pruning.
use super::*;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Ticket,
    AgentStarted,
    TerminalAdded,
}

impl ActivityKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Ticket => "ticket",
            Self::AgentStarted => "agent_started",
            Self::TerminalAdded => "terminal_added",
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct NewActivity {
    /// Generated once by the client; retrying a history write cannot duplicate it.
    pub id: String,
    pub kind: ActivityKind,
    pub target: String,
    pub label: String,
    pub body: String,
}

impl NewActivity {
    pub fn validate(&self) -> Result<()> {
        check_body(&self.body)?;
        if self.id.is_empty()
            || self.id.len() > 100
            || self.target.is_empty()
            || self.target.len() > 500
            || self.label.trim().is_empty()
            || self.label.len() > 500
        {
            return Err(anyhow!("invalid note activity"));
        }
        match self.kind {
            ActivityKind::Ticket => {
                let (repo, number) = self
                    .target
                    .rsplit_once('#')
                    .ok_or_else(|| anyhow!("ticket target must be owner/repo#number"))?;
                crate::owner_docs::validate_repo_slug(repo)?;
                if number.parse::<i64>().ok().filter(|n| *n > 0).is_none() {
                    return Err(anyhow!("invalid ticket number"));
                }
            }
            ActivityKind::AgentStarted | ActivityKind::TerminalAdded => {
                if !self
                    .target
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
                {
                    return Err(anyhow!("invalid agent destination"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
pub struct Activity {
    pub id: String,
    pub note_id: String,
    pub kind: String,
    pub target: String,
    pub label: String,
    pub at: String,
    pub chars: usize,
}

#[derive(Serialize)]
pub struct ActivityPreview {
    #[serde(flatten)]
    pub latest: Activity,
    pub count: i64,
}

pub(super) fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS note_activity (
            id TEXT PRIMARY KEY, note_id TEXT NOT NULL, kind TEXT NOT NULL,
            target TEXT NOT NULL, label TEXT NOT NULL, body TEXT NOT NULL,
            at TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS note_activity_note ON note_activity(note_id, at DESC, id DESC);",
    )?;
    Ok(())
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Activity> {
    Ok(Activity {
        id: r.get(0)?,
        note_id: r.get(1)?,
        kind: r.get(2)?,
        target: r.get(3)?,
        label: r.get(4)?,
        at: r.get(5)?,
        chars: r.get(6)?,
    })
}

impl NotesStore {
    pub fn record_activity(&self, note_id: &str, input: &NewActivity) -> Result<Option<Activity>> {
        input.validate()?;
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_note(&tx, note_id)?.is_none() {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO note_activity(id,note_id,kind,target,label,body,at)
             VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO NOTHING",
            params![
                input.id,
                note_id,
                input.kind.as_str(),
                input.target,
                input.label,
                input.body,
                now()
            ],
        )?;
        let activity = tx
            .query_row(
                "SELECT id,note_id,kind,target,label,at,length(body) FROM note_activity WHERE id=?1
             AND note_id=?2 AND kind=?3 AND target=?4 AND label=?5 AND body=?6",
                params![
                    input.id,
                    note_id,
                    input.kind.as_str(),
                    input.target,
                    input.label,
                    input.body
                ],
                row,
            )
            .optional()?
            .ok_or_else(|| anyhow!("activity id already records a different action"))?;
        tx.commit()?;
        Ok(Some(activity))
    }

    pub fn activities(&self, note_id: &str) -> Result<Option<Vec<Activity>>> {
        let conn = self.open()?;
        if read_note(&conn, note_id)?.is_none() {
            return Ok(None);
        }
        let mut stmt = conn.prepare(
            "SELECT id,note_id,kind,target,label,at,length(body) FROM note_activity
             WHERE note_id=? ORDER BY at DESC,id DESC",
        )?;
        let rows = stmt
            .query_map([note_id], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(rows))
    }

    pub fn activity_body(&self, note_id: &str, id: &str) -> Result<Option<String>> {
        let conn = self.open()?;
        Ok(conn
            .query_row(
                "SELECT a.body FROM note_activity a JOIN notes n ON n.id=a.note_id
             WHERE a.note_id=?1 AND a.id=?2 AND n.deleted_at IS NULL",
                params![note_id, id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn activity_previews(&self) -> Result<Vec<ActivityPreview>> {
        let conn = self.open()?;
        let mut stmt = conn.prepare(
            "SELECT a.id,a.note_id,a.kind,a.target,a.label,a.at,length(a.body),
                (SELECT count(*) FROM note_activity b WHERE b.note_id=a.note_id)
             FROM note_activity a JOIN notes n ON n.id=a.note_id
             WHERE n.deleted_at IS NULL AND a.id=(SELECT b.id FROM note_activity b
               WHERE b.note_id=a.note_id ORDER BY b.at DESC,b.id DESC LIMIT 1)
             ORDER BY a.at DESC,a.id DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ActivityPreview {
                    latest: row(r)?,
                    count: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn activity_keeps_exact_text_across_edits_and_retries() {
        let mut random = [0u8; 8];
        rand_core::OsRng.fill_bytes(&mut random);
        let store = NotesStore::new(std::env::temp_dir().join(format!(
            "sm-note-activity-{:x}.db",
            u64::from_ne_bytes(random)
        )));
        let note = store
            .create("Whole note with a selected paragraph", None)
            .unwrap();
        let event = NewActivity {
            id: "event-1".into(),
            kind: ActivityKind::Ticket,
            target: "acme/widgets#42".into(),
            label: "Selected paragraph".into(),
            body: "selected paragraph".into(),
        };
        store.record_activity(&note.id, &event).unwrap().unwrap();
        store.record_activity(&note.id, &event).unwrap().unwrap();
        store
            .save(&note.id, "Rewritten note", None, note.version)
            .unwrap();
        let restored = NotesStore::new(store.path.clone());
        assert_eq!(restored.activities(&note.id).unwrap().unwrap().len(), 1);
        assert_eq!(
            restored
                .activity_body(&note.id, &event.id)
                .unwrap()
                .unwrap(),
            event.body
        );
        assert_eq!(restored.activity_previews().unwrap()[0].count, 1);
        let mut changed = event.clone();
        changed.body = "different".into();
        assert!(store.record_activity(&note.id, &changed).is_err());
        store.delete(&note.id).unwrap();
        assert!(store.activity_previews().unwrap().is_empty());
        assert!(store.activity_body(&note.id, &event.id).unwrap().is_none());
        assert!(store.record_activity(&note.id, &event).unwrap().is_none());
    }
}
