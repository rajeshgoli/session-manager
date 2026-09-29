//! Durable merge holds and GitHub draft transitions.
use super::*;

// Serialize GitHub transitions without holding the shared message database lock.
static HOLD_CHANGES: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS merge_holds (
        id TEXT PRIMARY KEY, repo TEXT NOT NULL, pr INTEGER NOT NULL,
        placed_by_session_id TEXT, placed_by_name TEXT NOT NULL, reason TEXT,
        made_draft INTEGER NOT NULL, placed_at TEXT NOT NULL, ended_at TEXT,
        ended_by_session_id TEXT, ended_by_name TEXT, end_reason TEXT);
        CREATE UNIQUE INDEX IF NOT EXISTS merge_holds_active ON merge_holds(repo,pr) WHERE ended_at IS NULL;")?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeHold {
    pub id: String,
    pub repo: String,
    pub pr: i64,
    pub placed_by_session_id: Option<String>,
    #[serde(rename = "placed_by")]
    pub placed_by_name: String,
    pub reason: Option<String>,
    pub made_draft: bool,
    pub placed_at: String,
}
#[derive(Debug, Clone)]
pub struct HoldActor {
    pub session_id: Option<String>,
    pub name: String,
}
#[derive(Debug, Clone)]
pub struct HoldPr {
    pub node_id: String,
    pub state: String,
    pub is_draft: bool,
}
pub trait MergeHoldSource: Send + Sync {
    fn pull_request(&self, repo: &str, pr: i64) -> Result<HoldPr, String>;
    fn set_draft(&self, node_id: &str, draft: bool) -> Result<(), String>;
}
#[derive(Debug)]
pub enum HoldError {
    NotFound(String),
    Conflict(String),
    Forbidden(String),
    Github(String),
    Store(anyhow::Error),
}
impl From<anyhow::Error> for HoldError {
    fn from(e: anyhow::Error) -> Self {
        Self::Store(e)
    }
}
impl From<rusqlite::Error> for HoldError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}
#[derive(Serialize)]
pub struct HoldResult {
    pub hold: MergeHold,
    pub already_held: bool,
    pub warning: Option<String>,
    pub notified: Vec<String>,
}

fn active(conn: &Connection, repo: Option<&str>, pr: Option<i64>) -> Result<Vec<MergeHold>> {
    if !table_exists(conn, "merge_holds")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare("SELECT id,repo,pr,placed_by_session_id,placed_by_name,reason,made_draft,placed_at FROM merge_holds WHERE ended_at IS NULL AND (?1 IS NULL OR repo=?1) AND (?2 IS NULL OR pr=?2) ORDER BY placed_at,id")?;
    let holds = stmt
        .query_map(params![repo, pr], |r| {
            Ok(MergeHold {
                id: r.get(0)?,
                repo: r.get(1)?,
                pr: r.get(2)?,
                placed_by_session_id: r.get(3)?,
                placed_by_name: r.get(4)?,
                reason: r.get(5)?,
                made_draft: r.get(6)?,
                placed_at: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(holds)
}
fn event(
    conn: &Connection,
    hold: &MergeHold,
    actor: &HoldActor,
    kind: &str,
    text: Option<&str>,
    recipients: &[String],
) -> Result<Vec<String>> {
    write_event(
        conn,
        kind,
        actor.session_id.as_deref(),
        Some(&hold.repo),
        None,
        Some(hold.pr),
        json!({"by":actor.name,"reason":hold.reason}),
        &now_rfc3339(),
    )?;
    let mut notified = Vec::new();
    if let Some(text) = text {
        for target in recipients {
            if actor.session_id.as_ref() != Some(target) {
                queue_notice(conn, target, text, &mut notified)?;
            }
        }
    }
    Ok(notified)
}
impl MergeHold {
    pub fn placed_message(&self) -> String {
        let mut s = format!(
            "[sm hold] Merge hold on PR #{} ({}) requested by {}: do not merge until released.",
            self.pr,
            repo_name(&self.repo),
            self.placed_by_name
        );
        if let Some(reason) = &self.reason {
            s.push_str(&format!("\nReason: {reason}"));
        }
        s
    }
    pub fn merged_message(&self) -> String {
        format!(
            "[sm hold] PR #{} ({}) merged while a merge hold by {} was in place.",
            self.pr,
            repo_name(&self.repo),
            self.placed_by_name
        )
    }
    pub fn projection(&self) -> Value {
        json!({"placed_by":self.placed_by_name,"placed_at":self.placed_at,"reason":self.reason})
    }
    pub fn can_release(&self, actor: &HoldActor) -> bool {
        actor.session_id.is_none() || actor.session_id == self.placed_by_session_id
    }
}
impl WorkClaimStore {
    pub fn merge_holds(&self, repo: Option<&str>) -> Result<Vec<MergeHold>> {
        match self.open_read()? {
            Some(conn) => active(&conn, repo, None),
            None => Ok(Vec::new()),
        }
    }
    pub fn merge_hold(&self, repo: &str, pr: i64) -> Result<Option<MergeHold>> {
        match self.open_read()? {
            Some(conn) => Ok(active(&conn, Some(&canonical_repo(repo)), Some(pr))?.pop()),
            None => Ok(None),
        }
    }
    // One server owns the GitHub transitions; SQLite transactions contain only
    // local writes, so a slow GitHub request cannot stall durable messaging.
    pub fn place_merge_hold(
        &self,
        repo: &str,
        pr: i64,
        actor: &HoldActor,
        reason: Option<&str>,
        source: &dyn MergeHoldSource,
        recipients: &[String],
    ) -> Result<HoldResult, HoldError> {
        let repo = canonical_repo(repo);
        let mut conn = self.open_write()?;
        let _guard = HOLD_CHANGES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hold) = active(&conn, Some(&repo), Some(pr))?.pop() {
            return Ok(HoldResult {
                hold,
                already_held: true,
                warning: None,
                notified: vec![],
            });
        }
        let github = source.pull_request(&repo, pr).map_err(|e| {
            if e.contains("not found") || e.contains("Could not resolve") || e.contains("HTTP 404")
            {
                HoldError::NotFound(format!("PR #{pr} not found in {repo}"))
            } else {
                HoldError::Github(e)
            }
        })?;
        if github.state != "open" {
            return Err(HoldError::Conflict(format!("PR #{pr} is {}", github.state)));
        }
        if !github.is_draft {
            source
                .set_draft(&github.node_id, true)
                .map_err(HoldError::Github)?;
        }
        let hold = MergeHold {
            id: random_claim_id(),
            repo,
            pr,
            placed_by_session_id: actor.session_id.clone(),
            placed_by_name: actor.name.clone(),
            reason: reason.map(str::to_owned),
            made_draft: !github.is_draft,
            placed_at: now_rfc3339(),
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO merge_holds(id,repo,pr,placed_by_session_id,placed_by_name,reason,made_draft,placed_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![hold.id,hold.repo,pr,actor.session_id,actor.name,hold.reason,hold.made_draft,hold.placed_at])?;
        let notified = event(
            &tx,
            &hold,
            actor,
            "hold.placed",
            Some(&hold.placed_message()),
            recipients,
        )?;
        tx.commit()?;
        Ok(HoldResult {
            hold,
            already_held: false,
            warning: None,
            notified,
        })
    }
    pub fn release_merge_hold(
        &self,
        repo: &str,
        pr: i64,
        actor: &HoldActor,
        owner: &str,
        source: &dyn MergeHoldSource,
        recipients: &[String],
    ) -> Result<HoldResult, HoldError> {
        let mut conn = self.open_write()?;
        let _guard = HOLD_CHANGES.lock().unwrap_or_else(|e| e.into_inner());
        let hold = active(&conn, Some(&canonical_repo(repo)), Some(pr))?
            .pop()
            .ok_or_else(|| HoldError::NotFound(format!("No merge hold on PR #{pr}")))?;
        if !hold.can_release(actor) {
            let who = if hold.placed_by_session_id.is_none() {
                owner.to_owned()
            } else {
                format!("{} or {owner}", hold.placed_by_name)
            };
            return Err(HoldError::Forbidden(format!(
                "Merge hold on PR #{pr} was placed by {}; only {who} can release it.",
                hold.placed_by_name
            )));
        }
        let warning = if hold.made_draft {
            let result = source.pull_request(&hold.repo, pr).and_then(|p| {
                if p.state == "open" && p.is_draft {
                    source.set_draft(&p.node_id, false)
                } else {
                    Ok(())
                }
            });
            result.err().map(|e| format!("PR is still a draft: {e}"))
        } else {
            None
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        end(&tx, &hold, actor, "released")?;
        let text = format!(
            "[sm hold] Merge hold on PR #{pr} ({}) released by {}.",
            repo_name(&hold.repo),
            actor.name
        );
        let notified = event(&tx, &hold, actor, "hold.released", Some(&text), recipients)?;
        tx.commit()?;
        Ok(HoldResult {
            hold,
            already_held: false,
            warning,
            notified,
        })
    }
    pub fn sync_merge_hold(
        &self,
        id: &str,
        source: &dyn MergeHoldSource,
        recipients: &[String],
    ) -> Result<(Vec<String>, Option<MergeHold>), HoldError> {
        let mut conn = self.open_write()?;
        let _guard = HOLD_CHANGES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(hold) = active(&conn, None, None)?.into_iter().find(|h| h.id == id) else {
            return Ok((vec![], None));
        };
        let pr = source
            .pull_request(&hold.repo, hold.pr)
            .map_err(HoldError::Github)?;
        let actor = HoldActor {
            session_id: None,
            name: "sm".into(),
        };
        if pr.state == "open" && !pr.is_draft {
            source
                .set_draft(&pr.node_id, true)
                .map_err(HoldError::Github)?;
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut notified = Vec::new();
        let mut owner_notice = None;
        if pr.state != "open" {
            end(
                &tx,
                &hold,
                &actor,
                if pr.state == "merged" {
                    "pr_merged"
                } else {
                    "pr_closed"
                },
            )?;
            let targets = if pr.state == "merged" {
                hold.placed_by_session_id
                    .clone()
                    .into_iter()
                    .collect::<Vec<_>>()
            } else {
                vec![]
            };
            notified = event(
                &tx,
                &hold,
                &actor,
                "hold.ended",
                Some(&hold.merged_message()),
                &targets,
            )?;
            if pr.state == "merged" && hold.placed_by_session_id.is_none() {
                owner_notice = Some(hold);
            }
        } else if !pr.is_draft {
            let text = format!("[sm hold] PR #{} ({}) was marked ready for review while a merge hold by {} is in place; sm returned it to draft.",hold.pr,repo_name(&hold.repo),hold.placed_by_name);
            notified = event(
                &tx,
                &hold,
                &actor,
                "hold.redrafted",
                Some(&text),
                recipients,
            )?;
        }
        tx.commit()?;
        Ok((notified, owner_notice))
    }
}
fn end(conn: &Connection, hold: &MergeHold, actor: &HoldActor, reason: &str) -> Result<()> {
    conn.execute("UPDATE merge_holds SET ended_at=?2,ended_by_session_id=?3,ended_by_name=?4,end_reason=?5 WHERE id=?1",params![hold.id,now_rfc3339(),actor.session_id,actor.name,reason])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Github {
        pr: Mutex<HoldPr>,
        calls: Mutex<Vec<bool>>,
        fail: bool,
    }
    impl Github {
        fn new(draft: bool) -> Self {
            Self {
                pr: Mutex::new(HoldPr {
                    node_id: "PR1".into(),
                    state: "open".into(),
                    is_draft: draft,
                }),
                calls: Mutex::new(vec![]),
                fail: false,
            }
        }
    }
    impl MergeHoldSource for Github {
        fn pull_request(&self, _: &str, _: i64) -> Result<HoldPr, String> {
            Ok(self.pr.lock().unwrap().clone())
        }
        fn set_draft(&self, _: &str, draft: bool) -> Result<(), String> {
            if self.fail {
                return Err("offline".into());
            }
            self.calls.lock().unwrap().push(draft);
            self.pr.lock().unwrap().is_draft = draft;
            Ok(())
        }
    }
    fn store() -> WorkClaimStore {
        WorkClaimStore::new(std::env::temp_dir().join(format!("sm-holds-{}.db", random_claim_id())))
    }
    fn actor(id: Option<&str>) -> HoldActor {
        HoldActor {
            session_id: id.map(str::to_owned),
            name: id.unwrap_or("Rajesh").into(),
        }
    }
    #[test]
    fn merge_hold_authority_and_draft_ownership() {
        for placer in [None, Some("a")] {
            for releaser in [None, Some("a"), Some("b")] {
                let store = store();
                let gh = Github::new(false);
                let result = store
                    .place_merge_hold("Acme/Repo", 12, &actor(placer), None, &gh, &[])
                    .unwrap();
                assert!(result.hold.made_draft);
                assert_eq!(store.tracked_items().unwrap()["acme/repo"], vec![12]);
                let result =
                    store.release_merge_hold("acme/repo", 12, &actor(releaser), "Rajesh", &gh, &[]);
                if releaser.is_none() || placer == releaser {
                    assert!(result.is_ok());
                    assert!(store.merge_holds(None).unwrap().is_empty());
                    assert_eq!(*gh.calls.lock().unwrap(), vec![true, false]);
                    let conn = store.open_read().unwrap().unwrap();
                    let fields:(String,Option<String>,String,String)=conn.query_row("SELECT ended_at,ended_by_session_id,ended_by_name,end_reason FROM merge_holds",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
                    assert!(!fields.0.is_empty());
                    assert_eq!(fields.1, releaser.map(str::to_owned));
                    assert_eq!(fields.3, "released");
                } else {
                    assert!(matches!(result, Err(HoldError::Forbidden(_))));
                }
                fs::remove_file(&store.db_path).unwrap();
            }
        }
        let store = store();
        let gh = Github::new(true);
        store
            .place_merge_hold("acme/repo", 12, &actor(None), None, &gh, &[])
            .unwrap();
        store
            .release_merge_hold("acme/repo", 12, &actor(None), "Rajesh", &gh, &[])
            .unwrap();
        assert!(gh.calls.lock().unwrap().is_empty());
        fs::remove_file(&store.db_path).unwrap();
    }
    #[test]
    fn merge_hold_idempotence_failures_and_actor_exclusion() {
        let store = store();
        let mut gh = Github::new(false);
        gh.fail = true;
        assert!(matches!(
            store.place_merge_hold("acme/repo", 12, &actor(Some("a")), None, &gh, &[]),
            Err(HoldError::Github(_))
        ));
        assert!(store.merge_holds(None).unwrap().is_empty());
        gh.fail = false;
        let placed = store
            .place_merge_hold(
                "acme/repo",
                12,
                &actor(Some("a")),
                Some("decision"),
                &gh,
                &["a".into(), "b".into()],
            )
            .unwrap();
        assert_eq!(placed.notified, vec!["b"]);
        assert!(placed.hold.placed_message().ends_with("\nReason: decision"));
        gh.fail = true;
        let again = store
            .place_merge_hold("acme/repo", 12, &actor(None), None, &gh, &["b".into()])
            .unwrap();
        assert!(again.already_held);
        assert!(again.notified.is_empty());
        assert_eq!(again.hold.id, placed.hold.id);
        let released = store
            .release_merge_hold("acme/repo", 12, &actor(None), "Rajesh", &gh, &[])
            .unwrap();
        assert!(released.warning.unwrap().contains("still a draft"));
        assert!(store.merge_holds(None).unwrap().is_empty());
        assert_eq!(
            store
                .events()
                .unwrap()
                .iter()
                .map(|e| e.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["hold.placed", "hold.released"]
        );
        fs::remove_file(&store.db_path).unwrap();
    }
    #[test]
    fn merge_hold_sync_redrafts_and_ends() {
        for state in ["merged", "closed"] {
            let store = store();
            let gh = Github::new(false);
            let h = store
                .place_merge_hold("acme/repo", 12, &actor(Some("a")), None, &gh, &[])
                .unwrap()
                .hold;
            gh.pr.lock().unwrap().is_draft = false;
            assert_eq!(
                store.sync_merge_hold(&h.id, &gh, &["b".into()]).unwrap().0,
                vec!["b"]
            );
            assert_eq!(*gh.calls.lock().unwrap(), vec![true, true]);
            gh.pr.lock().unwrap().state = state.into();
            let (targets, owner) = store.sync_merge_hold(&h.id, &gh, &["b".into()]).unwrap();
            assert_eq!(targets, if state == "merged" { vec!["a"] } else { vec![] });
            assert!(owner.is_none());
            assert!(store.merge_holds(None).unwrap().is_empty());
            assert_eq!(
                store
                    .events()
                    .unwrap()
                    .iter()
                    .map(|e| e.kind.as_str())
                    .collect::<Vec<_>>(),
                vec!["hold.placed", "hold.redrafted", "hold.ended"]
            );
            fs::remove_file(&store.db_path).unwrap();
        }
    }
}
