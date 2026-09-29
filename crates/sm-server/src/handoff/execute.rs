//! Handoff execution: the texts, the successor's name and brief, and which
//! stored session-id columns a handoff moves (spec Appendices D-H).
//!
//! Everything here is pure. `sessions.rs` owns the session-store half of the
//! transfer and `http/handoff.rs` drives the steps in order.

use serde_json::{json, Value};

use super::policy::round_percent;

/// Queue category of the successor's brief and the parent notice.
pub const NOTICE_CATEGORY: &str = "handoff";
/// Terminal provenance source of a predecessor retired by its handoff.
pub const RETIRE_SOURCE: &str = "handed_off";
/// D.4, printed by `sm handoff`.
pub const ACCEPTED_TEXT: &str = "Handoff accepted. End your turn now; sm will start your successor when it ends and retire this session.";
/// F.5 / G recovery: a `spawning` state found at startup.
pub const RESTARTED_ERROR: &str = "server restarted while starting the successor";
/// Stable queue ids of the brief and the parent notice, so a resumed
/// transfer queues each once.
pub fn brief_message_id(predecessor_id: &str) -> String {
    format!("handoff-brief-{predecessor_id}")
}

pub fn parent_notice_message_id(predecessor_id: &str) -> String {
    format!("handoff-notice-{predecessor_id}")
}
/// The chain walk of H.2 stops after this many hops.
pub const MAX_FORWARD_HOPS: usize = 32;

/// The note `sm handoff` recorded (Appendix E). sm never reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffNote {
    pub kind: NoteKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Link,
    Path,
}

impl HandoffNote {
    /// Validate a request body's `note`. The CLI has already checked the file
    /// exists and the link parses; the server only checks the shape.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let kind = match value.get("kind").and_then(Value::as_str) {
            Some("link") => NoteKind::Link,
            Some("path") => NoteKind::Path,
            _ => return Err("note.kind must be link or path".to_owned()),
        };
        let text = value
            .get("value")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| "note.value is required".to_owned())?;
        match kind {
            NoteKind::Link if !(text.starts_with("https://") || text.starts_with("http://")) => {
                return Err("note.value must be an http or https URL".to_owned());
            }
            NoteKind::Path if !text.starts_with('/') => {
                return Err("note.value must be an absolute path".to_owned());
            }
            _ => {}
        }
        Ok(Self {
            kind,
            value: text.to_owned(),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "kind": match self.kind {
                NoteKind::Link => "link",
                NoteKind::Path => "path",
            },
            "value": self.value,
        })
    }
}

/// The note's link or path as the texts show it.
pub fn note_value(note: Option<&Value>) -> String {
    note.and_then(|note| note.get("value"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// F.3: `-h<k>` with k ≥ 2 becomes `-h<k+1>`; anything else gains `-h2`.
pub fn successor_name(predecessor_name: &str) -> String {
    if let Some((stem, suffix)) = predecessor_name.rsplit_once("-h") {
        if !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            if let Ok(k) = suffix.parse::<u64>() {
                if k >= 2 {
                    return format!("{stem}-h{}", k + 1);
                }
            }
        }
    }
    format!("{predecessor_name}-h2")
}

/// D.6.
pub fn failed_text(error: &str) -> String {
    format!(
        "[sm context management] sm could not start your successor: {error}. You still hold all your work. Continue, and run `sm handoff` again to retry."
    )
}

/// D.7. `percent` is the predecessor's last context reading.
pub fn parent_notice_text(
    predecessor: (&str, &str),
    successor: (&str, &str),
    percent: Option<f64>,
    note: &str,
) -> String {
    let at = percent
        .map(|percent| format!(" at {}% context", round_percent(percent)))
        .unwrap_or_default();
    format!(
        "[sm handoff] {} ({}) handed off to {} ({}){at}. Note: {note}",
        predecessor.0, predecessor.1, successor.0, successor.1
    )
}

/// D.8, printed by `sm send` when the target resolved through forwarding.
pub fn forwarded_text(predecessor_name: &str, successor_name: &str, successor_id: &str) -> String {
    format!("{predecessor_name} handed off; delivered to {successor_name} ({successor_id}).")
}

/// What the successor's first message lists (Appendix F.4). Empty values
/// drop their line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Brief {
    pub predecessor_name: String,
    pub predecessor_id: String,
    pub percent: Option<f64>,
    pub note: String,
    pub working_dir: String,
    pub branch: Option<String>,
    pub claims: String,
    pub roles: Vec<String>,
    pub pending: Vec<String>,
    pub children: Vec<String>,
    pub original_brief: Option<String>,
}

impl Brief {
    pub fn text(&self) -> String {
        let stopped_at = self
            .percent
            .map(|percent| format!(", which stopped at {}% context", round_percent(percent)))
            .unwrap_or_default();
        let mut lines = vec![format!(
            "[sm handoff] You are taking over from {} ({}){stopped_at}.",
            self.predecessor_name, self.predecessor_id
        )];
        if !self.note.is_empty() {
            lines.push(format!("Handoff note: {}", self.note));
        }
        if !self.working_dir.is_empty() {
            let branch = self
                .branch
                .as_deref()
                .map(|branch| format!(" (branch {branch})"))
                .unwrap_or_default();
            lines.push(format!("Working directory: {}{branch}", self.working_dir));
        }
        if !self.claims.is_empty() && self.claims != "none" {
            lines.push(format!("You now hold: {}", self.claims));
        }
        if !self.roles.is_empty() {
            lines.push(format!("Registered roles: {}", self.roles.join(", ")));
        }
        if !self.pending.is_empty() {
            lines.push(format!("Pending for you: {}", self.pending.join("\n")));
        }
        if !self.children.is_empty() {
            lines.push(format!("Children: {}", self.children.join(", ")));
        }
        if let Some(path) = self
            .original_brief
            .as_deref()
            .filter(|path| !path.is_empty())
        {
            lines.push(format!("Original brief: {path}"));
        }
        lines.push("Read the handoff note first, then continue the work.".to_owned());
        lines.join("\n")
    }
}

/// Every stored `*session_id` column and what a handoff does with it
/// (Appendix G, completeness guard). A column in neither list fails the
/// `every_session_id_column_is_classified` test.
pub const MOVED_COLUMNS: &[(&str, &str)] = &[
    ("work_claims", "session_id"),
    ("worktree_keeps", "session_id"),
    ("message_queue", "target_session_id"),
    ("message_queue", "remind_cancel_on_reply_session_id"),
    ("message_queue", "parent_session_id"),
    ("codex_review_request_registrations", "notify_session_id"),
    ("codex_review_request_registrations", "requester_session_id"),
    ("queue_jobs", "notify_session_id"),
    ("queue_jobs", "requester_session_id"),
    ("scheduled_reminders", "target_session_id"),
    ("remind_registrations", "target_session_id"),
    ("remind_registrations", "cancel_on_reply_session_id"),
    ("rust_stop_notify_states", "session_id"),
    ("rust_stop_notify_states", "sender_session_id"),
    ("parent_wake_registrations", "parent_session_id"),
    ("parent_wake_registrations", "child_session_id"),
    ("owner_docs", "author_session_id"),
    ("owner_doc_reviews", "assigned_session_id"),
    ("owner_follows", "session_id"),
];

/// Columns a handoff leaves alone, with the reason.
pub const NOT_MOVED_COLUMNS: &[(&str, &str, &str)] = &[
    ("work_claims", "parent_session_id", "the parent is the same"),
    ("work_claims", "ended_by_session_id", "history"),
    ("events", "session_id", "history"),
    ("merge_holds", "placed_by_session_id", "history"),
    ("merge_holds", "ended_by_session_id", "history"),
    (
        "message_queue",
        "sender_session_id",
        "who sent it; a reply to an ended sender is forwarded",
    ),
    (
        "btw_requests",
        "requester_session_id",
        "short-lived; retire fails them",
    ),
    (
        "btw_requests",
        "target_session_id",
        "short-lived; retire fails them",
    ),
    ("bug_reports", "selected_session_id", "history"),
    ("guestbook_entries", "session_id", "history"),
    ("owner_doc_publishes", "session_id", "history"),
    (
        "owner_doc_reviews",
        "delivered_to_session_id",
        "delivery record; later reviews are forwarded",
    ),
    ("owner_message_notes", "session_id", "history"),
    ("owner_message_notes", "delivered_to_session_id", "history"),
    (
        "owner_messages",
        "sender_session_id",
        "the thread keeps its history; replies are forwarded",
    ),
    (
        "owner_message_replies",
        "delivered_to_session_id",
        "history",
    ),
    ("owner_notices", "session_id", "history"),
    (
        "seat_sessions",
        "provider_session_id",
        "a provider transcript id, not an sm session",
    ),
    ("tool_usage", "session_id", "telemetry"),
    ("tool_usage", "parent_session_id", "telemetry"),
    (
        "tool_usage",
        "claude_session_id",
        "a provider transcript id, not an sm session",
    ),
    ("telegram_telemetry", "session_id", "telemetry"),
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;

    #[test]
    fn successor_names_follow_appendix_f3() {
        assert_eq!(successor_name("sm-1651-engineer"), "sm-1651-engineer-h2");
        assert_eq!(successor_name("sm-1651-engineer-h2"), "sm-1651-engineer-h3");
        assert_eq!(
            successor_name("sm-1651-engineer-h9"),
            "sm-1651-engineer-h10"
        );
        assert_eq!(
            successor_name("sm-pr-1660-reviewer-2"),
            "sm-pr-1660-reviewer-2-h2"
        );
        assert_eq!(successor_name("x-h1"), "x-h1-h2");
        assert_eq!(successor_name("x-h"), "x-h-h2");
        assert_eq!(successor_name("x-hot"), "x-hot-h2");
    }

    #[test]
    fn notes_are_validated_by_shape_only() {
        let link = HandoffNote::parse(&json!({"kind": "link", "value": "https://x/y"})).unwrap();
        assert_eq!(link.kind, NoteKind::Link);
        assert!(HandoffNote::parse(&json!({"kind": "link", "value": "ftp://x"})).is_err());
        assert!(HandoffNote::parse(&json!({"kind": "path", "value": "notes.md"})).is_err());
        assert!(HandoffNote::parse(&json!({"kind": "file", "value": "/a"})).is_err());
        assert!(HandoffNote::parse(&json!({"kind": "path"})).is_err());
    }

    #[test]
    fn texts_match_appendix_d() {
        assert_eq!(
            ACCEPTED_TEXT,
            "Handoff accepted. End your turn now; sm will start your successor when it ends and retire this session."
        );
        assert_eq!(
            failed_text("boom"),
            "[sm context management] sm could not start your successor: boom. You still hold all your work. Continue, and run `sm handoff` again to retry."
        );
        assert_eq!(
            parent_notice_text(("a", "1"), ("a-h2", "2"), Some(24.4), "https://n"),
            "[sm handoff] a (1) handed off to a-h2 (2) at 24% context. Note: https://n"
        );
        assert_eq!(
            parent_notice_text(("a", "1"), ("a-h2", "2"), None, "/n.md"),
            "[sm handoff] a (1) handed off to a-h2 (2). Note: /n.md"
        );
        assert_eq!(
            forwarded_text("a", "a-h2", "2"),
            "a handed off; delivered to a-h2 (2)."
        );
    }

    #[test]
    fn brief_omits_empty_lines() {
        let full = Brief {
            predecessor_name: "sm-1651-engineer".into(),
            predecessor_id: "0f52d06d".into(),
            percent: Some(24.0),
            note: "https://n".into(),
            working_dir: "/w".into(),
            branch: Some("b".into()),
            claims: "ticket #1651, PR #1660".into(),
            roles: vec!["maintainer".into()],
            pending: vec![
                "Codex review of PR #1660, requested 14:02".into(),
                "Queue job t (j1), running".into(),
            ],
            children: vec!["c (9)".into()],
            original_brief: Some("/brief.md".into()),
        };
        assert_eq!(
            full.text(),
            "[sm handoff] You are taking over from sm-1651-engineer (0f52d06d), which stopped at 24% context.\n\
             Handoff note: https://n\n\
             Working directory: /w (branch b)\n\
             You now hold: ticket #1651, PR #1660\n\
             Registered roles: maintainer\n\
             Pending for you: Codex review of PR #1660, requested 14:02\n\
             Queue job t (j1), running\n\
             Children: c (9)\n\
             Original brief: /brief.md\n\
             Read the handoff note first, then continue the work."
        );
        let bare = Brief {
            predecessor_name: "a".into(),
            predecessor_id: "1".into(),
            claims: "none".into(),
            ..Brief::default()
        };
        assert_eq!(
            bare.text(),
            "[sm handoff] You are taking over from a (1).\nRead the handoff note first, then continue the work."
        );
    }

    fn identifier(text: &str) -> &str {
        let end = text
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(text.len());
        &text[..end]
    }

    /// `(table, column)` for each `*session_id` column in the `CREATE TABLE`
    /// statements, `ALTER TABLE … ADD COLUMN` statements and
    /// `("table", "column", …)` migration tuples of one source file.
    fn session_id_columns_in(source: &str, found: &mut BTreeSet<(String, String)>) {
        for (at, _) in source.match_indices("CREATE TABLE") {
            let rest = source[at + "CREATE TABLE".len()..].trim_start();
            let rest = rest
                .strip_prefix("IF NOT EXISTS")
                .unwrap_or(rest)
                .trim_start();
            let table = identifier(rest);
            let Some(open) = rest.find('(') else { continue };
            let mut depth = 0;
            let mut segment_start = open + 1;
            for (offset, c) in rest[open..].char_indices() {
                let index = open + offset;
                match c {
                    '(' => depth += 1,
                    ')' | ',' if depth == 1 => {
                        let column = identifier(rest[segment_start..index].trim_start());
                        if column.ends_with("session_id") {
                            found.insert((table.to_owned(), column.to_owned()));
                        }
                        segment_start = index + 1;
                        if c == ')' {
                            break;
                        }
                    }
                    ')' => depth -= 1,
                    _ => {}
                }
            }
        }
        for (at, _) in source.match_indices("ALTER TABLE") {
            let rest = source[at + "ALTER TABLE".len()..].trim_start();
            let table = identifier(rest);
            if let Some(added) = rest[table.len()..].trim_start().strip_prefix("ADD COLUMN") {
                let column = identifier(added.trim_start());
                if column.ends_with("session_id") {
                    found.insert((table.to_owned(), column.to_owned()));
                }
            }
        }
        // `("table", "column", "TYPE")`, not a call such as `f("a", "b")`.
        for (at, _) in source.match_indices("(\"") {
            let called = source[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            let rest = &source[at + 2..];
            let table = identifier(rest);
            let Some(after) = rest[table.len()..].strip_prefix('"') else {
                continue;
            };
            let Some(after) = after.trim_start().strip_prefix(',') else {
                continue;
            };
            let Some(after) = after.trim_start().strip_prefix('"') else {
                continue;
            };
            let column = identifier(after);
            let Some(tail) = after[column.len()..].strip_prefix('"') else {
                continue;
            };
            let typed = tail.trim_start().starts_with(',');
            if !called && typed && !table.is_empty() && column.ends_with("session_id") {
                found.insert((table.to_owned(), column.to_owned()));
            }
        }
    }

    /// Every `*session_id` column any SQLite store creates, from the source.
    fn schema_session_id_columns() -> BTreeSet<(String, String)> {
        let mut found = BTreeSet::new();
        let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if !name.ends_with(".rs") || name == "tests.rs" || name == "execute.rs" {
                    continue;
                }
                session_id_columns_in(&std::fs::read_to_string(&path).unwrap(), &mut found);
            }
        }
        found
    }

    #[test]
    fn the_schema_scan_reads_each_form() {
        let mut found = BTreeSet::new();
        session_id_columns_in(
            "CREATE TABLE IF NOT EXISTS t (\n id TEXT PRIMARY KEY,\n a_session_id TEXT NOT NULL,\n n INTEGER DEFAULT (1), session_id TEXT);\n\
             ALTER TABLE u ADD COLUMN b_session_id TEXT;\n\
             (\"v\", \"c_session_id\", \"TEXT\") field(\"w\", \"d_session_id\", 1)",
            &mut found,
        );
        let found = found
            .iter()
            .map(|(t, c)| format!("{t}.{c}"))
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            [
                "t.a_session_id",
                "t.session_id",
                "u.b_session_id",
                "v.c_session_id"
            ]
        );
    }

    #[test]
    fn every_session_id_column_is_classified() {
        let found = schema_session_id_columns();
        assert!(
            found.contains(&("work_claims".to_owned(), "session_id".to_owned())),
            "the schema scan found nothing; fix the scan: {found:?}"
        );
        let classified = MOVED_COLUMNS
            .iter()
            .map(|(table, column)| (*table, *column))
            .chain(
                NOT_MOVED_COLUMNS
                    .iter()
                    .map(|(table, column, _)| (*table, *column)),
            )
            .collect::<BTreeSet<_>>();
        let unclassified = found
            .iter()
            .filter(|(table, column)| !classified.contains(&(table.as_str(), column.as_str())))
            .collect::<Vec<_>>();
        assert!(
            unclassified.is_empty(),
            "classify each column in handoff::execute MOVED_COLUMNS or NOT_MOVED_COLUMNS: {unclassified:?}"
        );
    }
}
