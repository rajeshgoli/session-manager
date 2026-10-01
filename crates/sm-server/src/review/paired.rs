//! The `paired_reviewers` registry (#1779): one row per reviewer agent that
//! works in an author's checkout for one ticket.
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// Created with the review request tables, so a handoff can re-point rows
/// in the same transaction as the request's `reviewer_session_id`.
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS paired_reviewers (
    session_id TEXT PRIMARY KEY,
    repo TEXT NOT NULL,
    ticket INTEGER NOT NULL,
    pr_number INTEGER NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    effort TEXT NOT NULL,
    checkout TEXT NOT NULL,
    created_at TEXT NOT NULL,
    retired_at TEXT,
    rounds INTEGER NOT NULL DEFAULT 0,
    last_review_url TEXT
)";

#[derive(Clone, Debug, PartialEq)]
pub struct PairedReviewer {
    pub session_id: String,
    pub repo: String,
    pub ticket: i64,
    pub pr_number: i64,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub checkout: String,
    pub created_at: String,
    pub retired_at: Option<String>,
    /// Rounds sent to this reviewer: the first gets the full brief (E5).
    pub rounds: i64,
    pub last_review_url: Option<String>,
}

const COLUMNS: &str = "session_id,repo,ticket,pr_number,provider,model,effort,checkout,\
    created_at,retired_at,rounds,last_review_url";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PairedReviewer> {
    Ok(PairedReviewer {
        session_id: r.get(0)?,
        repo: r.get(1)?,
        ticket: r.get(2)?,
        pr_number: r.get(3)?,
        provider: r.get(4)?,
        model: r.get(5)?,
        effort: r.get(6)?,
        checkout: r.get(7)?,
        created_at: r.get(8)?,
        retired_at: r.get(9)?,
        rounds: r.get(10)?,
        last_review_url: r.get(11)?,
    })
}

fn open(db: &Path) -> Result<Connection> {
    let conn = Connection::open(db)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// The reviewer still registered for a ticket, if any.
pub fn live(db: &Path, repo: &str, ticket: i64) -> Result<Option<PairedReviewer>> {
    Ok(open(db)?
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM paired_reviewers WHERE repo=?1 AND ticket=?2 \
                 AND retired_at IS NULL ORDER BY created_at DESC LIMIT 1"
            ),
            params![repo, ticket],
            row,
        )
        .optional()?)
}

pub fn get(db: &Path, session_id: &str) -> Result<Option<PairedReviewer>> {
    Ok(open(db)?
        .query_row(
            &format!("SELECT {COLUMNS} FROM paired_reviewers WHERE session_id=?1"),
            [session_id],
            row,
        )
        .optional()?)
}

pub fn list_live(db: &Path) -> Result<Vec<PairedReviewer>> {
    if !db.exists() {
        return Ok(Vec::new());
    }
    let conn = open(db)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM paired_reviewers WHERE retired_at IS NULL ORDER BY created_at"
    ))?;
    let rows = stmt.query_map([], row)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn insert(db: &Path, r: &PairedReviewer) -> Result<()> {
    open(db)?.execute(
        &format!("INSERT INTO paired_reviewers ({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"),
        params![
            r.session_id,
            r.repo,
            r.ticket,
            r.pr_number,
            r.provider,
            r.model,
            r.effort,
            r.checkout,
            r.created_at,
            r.retired_at,
            r.rounds,
            r.last_review_url
        ],
    )?;
    Ok(())
}

pub fn retire(db: &Path, session_id: &str, now: &str) -> Result<()> {
    open(db)?.execute(
        "UPDATE paired_reviewers SET retired_at=?2 WHERE session_id=?1 AND retired_at IS NULL",
        params![session_id, now],
    )?;
    Ok(())
}

/// A round was sent; a new PR number follows the request.
pub fn record_round(db: &Path, session_id: &str, pr_number: i64) -> Result<()> {
    open(db)?.execute(
        "UPDATE paired_reviewers SET rounds=rounds+1, pr_number=?2 WHERE session_id=?1",
        params![session_id, pr_number],
    )?;
    Ok(())
}

pub fn record_review(db: &Path, session_id: &str, url: Option<&str>) -> Result<()> {
    open(db)?.execute(
        "UPDATE paired_reviewers SET last_review_url=COALESCE(?2,last_review_url) WHERE session_id=?1",
        params![session_id, url],
    )?;
    Ok(())
}

/// `{repo_short}-{ticket}-reviewer`, cut to 32 characters, with the first
/// numeric suffix that no live session uses.
pub fn reviewer_name(short: &str, ticket: i64, taken: &dyn Fn(&str) -> bool) -> String {
    let base: String = format!("{short}-{ticket}-reviewer")
        .chars()
        .take(32)
        .collect();
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            let keep = 32 - suffix.len();
            format!("{}{suffix}", base.chars().take(keep).collect::<String>())
        })
        .find(|name| !taken(name))
        .expect("an unused name")
}

/// `HEAD` plus tracked-file status: what B4 compares before posting.
pub fn snapshot(checkout: &Path) -> Result<String> {
    let run = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(checkout)
            .args(args)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(String::from_utf8(out.stdout)?)
    };
    let head = run(&["rev-parse", "HEAD"])?;
    let status = run(&["status", "--porcelain", "--untracked-files=no"])?;
    Ok(format!("{}\n{status}", head.trim()))
}

/// B4's check 4 text: the changed files, or `HEAD` when it moved.
pub fn snapshot_change(before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    let (head_before, status_before) = before.split_once('\n').unwrap_or((before, ""));
    let (head_after, status_after) = after.split_once('\n').unwrap_or((after, ""));
    if head_before != head_after {
        return Some("HEAD".into());
    }
    let files = |status: &str| -> std::collections::BTreeSet<String> {
        status
            .lines()
            .filter(|l| l.len() > 3)
            .map(|l| l[3..].to_owned())
            .collect()
    };
    let (b, a) = (files(status_before), files(status_after));
    let changed: Vec<String> = b.symmetric_difference(&a).cloned().collect();
    // Same file set but different status letters: name them all.
    let changed = if changed.is_empty() {
        a.into_iter().collect()
    } else {
        changed
    };
    Some(changed.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cut_and_suffixed() {
        assert_eq!(reviewer_name("far", 1848, &|_| false), "far-1848-reviewer");
        let long = reviewer_name("fractal-algo-rust-long", 1848, &|_| false);
        assert_eq!(long.chars().count(), 32);
        let taken = |n: &str| n == "far-1848-reviewer";
        assert_eq!(reviewer_name("far", 1848, &taken), "far-1848-reviewer-2");
        let long_taken = |n: &str| n.chars().count() == 32 && !n.ends_with("-2");
        assert!(reviewer_name("fractal-algo-rust-long", 1848, &long_taken).ends_with("-2"));
    }

    #[test]
    fn snapshot_changes_name_files_or_head() {
        let before = "aaa\n M src/a.rs\n";
        assert_eq!(snapshot_change(before, before), None);
        assert_eq!(
            snapshot_change(before, "aaa\n M src/a.rs\n M src/b.rs\n").as_deref(),
            Some("src/b.rs")
        );
        assert_eq!(
            snapshot_change(before, "bbb\n M src/a.rs\n").as_deref(),
            Some("HEAD")
        );
        assert_eq!(
            snapshot_change("aaa\n", "aaa\n M Cargo.lock\n").as_deref(),
            Some("Cargo.lock")
        );
    }

    #[test]
    fn registry_round_trip_and_retire() {
        let dir = std::env::temp_dir().join(format!(
            "sm-paired-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("q.db");
        let reviewer = PairedReviewer {
            session_id: "r1".into(),
            repo: "far/repo".into(),
            ticket: 1848,
            pr_number: 1851,
            provider: "codex".into(),
            model: "gpt-6-astra".into(),
            effort: "high".into(),
            checkout: "/wt".into(),
            created_at: "2026-09-30T00:00:00Z".into(),
            retired_at: None,
            rounds: 0,
            last_review_url: None,
        };
        insert(&db, &reviewer).unwrap();
        record_round(&db, "r1", 1851).unwrap();
        record_review(&db, "r1", Some("https://x/1")).unwrap();
        let live_row = live(&db, "far/repo", 1848).unwrap().unwrap();
        assert_eq!(live_row.rounds, 1);
        assert_eq!(live_row.last_review_url.as_deref(), Some("https://x/1"));
        retire(&db, "r1", "later").unwrap();
        assert!(live(&db, "far/repo", 1848).unwrap().is_none());
        assert!(list_live(&db).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
