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

/// `HEAD`, then one line per changed tracked file: its status and a hash
/// of its staged and unstaged diff, so an edit to a file that was already
/// modified changes the snapshot too. This is what B4 compares before posting.
pub fn snapshot(checkout: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let run = |args: &[&str]| -> Result<Vec<u8>> {
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
        Ok(out.stdout)
    };
    let head = String::from_utf8(run(&["rev-parse", "HEAD"])?)?;
    let status = run(&["status", "--porcelain=v1", "-z", "--untracked-files=no"])?;
    let mut entries = status
        .split(|b| *b == 0)
        .map(|entry| String::from_utf8_lossy(entry).into_owned());
    let mut lines = vec![head.trim().to_owned()];
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let (code, path) = (&entry[..2], &entry[3..]);
        if code.contains('R') || code.contains('C') {
            // `-z` puts a rename's source path in its own entry.
            entries.next();
        }
        let mut hash = Sha256::new();
        hash.update(run(&["diff", "--cached", "--binary", "--", path])?);
        hash.update(run(&["diff", "--binary", "--", path])?);
        lines.push(format!("{code}\t{path}\t{:x}", hash.finalize()));
    }
    Ok(lines.join("\n"))
}

/// B4's check 4 text: the changed files, or `HEAD` when it moved.
pub fn snapshot_change(before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    let (head_before, files_before) = before.split_once('\n').unwrap_or((before, ""));
    let (head_after, files_after) = after.split_once('\n').unwrap_or((after, ""));
    if head_before != head_after {
        return Some("HEAD".into());
    }
    let files = |text: &str| -> std::collections::BTreeMap<String, String> {
        text.lines()
            .filter_map(|line| {
                let (code, rest) = line.split_once('\t')?;
                let (path, hash) = rest.rsplit_once('\t')?;
                Some((path.to_owned(), format!("{code} {hash}")))
            })
            .collect()
    };
    let (b, a) = (files(files_before), files(files_after));
    let changed: Vec<String> = b
        .keys()
        .chain(a.keys())
        .filter(|path| b.get(*path) != a.get(*path))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
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
        let before = "aaa\n M\tsrc/a.rs\th1";
        assert_eq!(snapshot_change(before, before), None);
        assert_eq!(
            snapshot_change(before, "aaa\n M\tsrc/a.rs\th1\n M\tsrc/b.rs\th2").as_deref(),
            Some("src/b.rs")
        );
        assert_eq!(
            snapshot_change(before, "bbb\n M\tsrc/a.rs\th1").as_deref(),
            Some("HEAD")
        );
        // Same status, different content: an already-modified file edited again.
        assert_eq!(
            snapshot_change(before, "aaa\n M\tsrc/a.rs\th9").as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(
            snapshot_change("aaa", "aaa\n M\tCargo.lock\th3").as_deref(),
            Some("Cargo.lock")
        );
    }

    #[test]
    fn editing_an_already_modified_file_changes_the_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "sm-paired-snap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .status()
                .unwrap()
                .success())
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "one"]);
        // The author left a.txt modified; the reviewer edits it further.
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let before = snapshot(&dir).unwrap();
        assert_eq!(snapshot(&dir).unwrap(), before);
        std::fs::write(dir.join("a.txt"), "three\n").unwrap();
        let after = snapshot(&dir).unwrap();
        assert_eq!(snapshot_change(&before, &after).as_deref(), Some("a.txt"));
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        assert_eq!(snapshot_change(&before, &snapshot(&dir).unwrap()), None);
        std::fs::remove_dir_all(dir).unwrap();
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
