//! Check D (#2605): a PR merged with P0/P1 review findings nobody answered.
//! Once per merged PR, sm tells the sessions whose claim ended with the
//! merge, in facts only; with none of them live, the caller tells the owner.
//! The findings themselves come from `crate::review::findings`.

use super::checks::nudge_event;
use super::*;
use crate::review::findings::{finding_line, Finding};

/// A PR is checked only while its merge is this recent, so deploying the
/// check does not report every PR the database ever saw merge.
pub const FINDINGS_CHECK_WINDOW: Duration = Duration::from_secs(6 * 60 * 60);

pub(super) fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS merge_findings_checks (
            repo TEXT NOT NULL,
            pr INTEGER NOT NULL,
            checked_at TEXT NOT NULL,
            open_count INTEGER NOT NULL,
            PRIMARY KEY (repo, pr)
        );",
    )?;
    Ok(())
}

impl WorkClaimStore {
    /// Tracked PRs GitHub reported merged within `FINDINGS_CHECK_WINDOW` of
    /// `now` that Check D has not recorded.
    pub fn merged_prs_due_findings_check(&self, now: OffsetDateTime) -> Result<Vec<(String, i64)>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        if !table_exists(&conn, "merge_findings_checks")? {
            return Ok(Vec::new());
        }
        let since = now - FINDINGS_CHECK_WINDOW;
        let mut statement = conn.prepare(
            "SELECT i.repo, i.number, i.merged_at FROM work_items i
             WHERE i.kind = 'pr' AND i.state = 'merged' AND i.merged_at IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM merge_findings_checks c
                               WHERE c.repo = i.repo AND c.pr = i.number)
             ORDER BY i.repo, i.number",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|(_, _, merged_at)| {
                checks::parse_time(merged_at).is_some_and(|merged| merged >= since)
            })
            .map(|(repo, number, _)| (repo, number))
            .collect())
    }

    /// The live or stopped sessions whose claim on `pr` ended with its merge.
    pub fn merged_pr_holders(
        &self,
        repo: &str,
        pr: i64,
        sessions: &SessionDirectory,
    ) -> Result<Vec<String>> {
        let Some(conn) = self.open_read()? else {
            return Ok(Vec::new());
        };
        let mut holders: Vec<String> = query_claims(
            &conn,
            "WHERE repo = ?1 AND number = ?2 AND end_reason = 'merged'",
            params![repo, pr],
        )?
        .into_iter()
        .map(|claim| claim.session_id)
        .filter(|id| {
            sessions
                .get(id)
                .is_some_and(|session| session.state != HolderState::Retired)
        })
        .collect();
        holders.sort();
        holders.dedup();
        Ok(holders)
    }

    /// Records Check D for `pr` and, when findings are open, sends `text` to
    /// each of `recipients` and writes a `claim.nudge` event (with a null
    /// message id when there is no recipient). Returns the sessions sent a
    /// message. A PR already recorded is left alone.
    pub fn record_findings_check(
        &self,
        repo: &str,
        pr: i64,
        open: &[Finding],
        text: &str,
        recipients: &[String],
        now: OffsetDateTime,
    ) -> Result<Vec<String>> {
        let mut conn = self.open_write()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stamp = checks::format_time(now);
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO merge_findings_checks (repo, pr, checked_at, open_count)
             VALUES (?1, ?2, ?3, ?4)",
            params![repo, pr, stamp, open.len() as i64],
        )?;
        let mut notified = Vec::new();
        if inserted == 1 && !open.is_empty() {
            if recipients.is_empty() {
                nudge_event(&tx, "D", None, None, repo, WorkKind::Pr, pr, text, &stamp)?;
            }
            for session_id in recipients {
                let message_id = crate::queue::enqueue_important_in_conn(
                    &tx,
                    session_id,
                    text,
                    MESSAGE_CATEGORY,
                )?;
                nudge_event(
                    &tx,
                    "D",
                    Some(session_id),
                    Some(&message_id),
                    repo,
                    WorkKind::Pr,
                    pr,
                    text,
                    &stamp,
                )?;
                notified.push(session_id.clone());
            }
        }
        tx.commit()?;
        Ok(notified)
    }
}

/// `[sm claim] PR #2468 merged with 2 P0/P1 review findings nobody
/// answered:` and one line per finding.
pub fn findings_check_text(pr: i64, open: &[Finding]) -> String {
    let mut text = format!(
        "[sm claim] PR #{pr} merged with {} P0/P1 review finding{} nobody answered:",
        open.len(),
        if open.len() == 1 { "" } else { "s" }
    );
    for finding in open {
        let commit = &finding.commit[..finding.commit.len().min(7)];
        text.push_str(&format!(
            "\n- {} (reviewed at {commit}) {}",
            finding_line(finding),
            finding.url
        ));
    }
    text.push_str(
        "\nA finding is answered by a reply on its thread, a resolve, or a PR comment from \
         the author naming its reviewed commit.",
    );
    text
}
