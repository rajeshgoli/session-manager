//! Durable recovery work. Issue creation uses the same stable key after a crash;
//! recording and launching are replayed until the board accepts authorization.
use super::experiments::{self, Experiment};
use super::*;
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub key: String,
    pub experiment: String,
    pub mode: String,
    pub epoch: i64,
    pub parent: i64,
    pub title: String,
    pub body: String,
}

impl Job {
    pub fn kind(&self) -> &str {
        if self.mode == "revert" {
            "revert"
        } else {
            "cleanup"
        }
    }
}

pub fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS autopm_recovery (
        key TEXT PRIMARY KEY, payload TEXT NOT NULL, number INTEGER,
        done INTEGER NOT NULL DEFAULT 0);",
    )?;
    Ok(())
}

fn due(conn: &Connection, x: &Experiment, now: i64) -> Result<Option<(&'static str, i64)>> {
    let epoch = x.live_at_ms.unwrap_or(x.created_at_ms);
    if x.state == "rolling_back" {
        return Ok(Some(("revert", epoch)));
    }
    let Some(key) = &x.switch_key else {
        return Ok(None);
    };
    let Some(switch) = switches::get(conn, key)? else {
        return Ok(None);
    };
    if x.state == "reverted" && !switch.on {
        return Ok(Some(("remove-new", epoch)));
    }
    if x.state == "kept"
        && switch.on
        && !switch.held
        && x.kept_at_ms
            .is_some_and(|at| now >= at.saturating_add(30 * DAY_MS))
    {
        return Ok(Some(("remove-old", x.kept_at_ms.unwrap())));
    }
    Ok(None)
}

fn job(x: &Experiment, mode: &str, epoch: i64) -> Job {
    let instruction = match mode {
        "revert" => "Revert the experiment's merged implementation commits in a PR. Run sm request-review, merge after a clean review, and deploy with scripts/restart-rust-server.sh. If conflicts prevent an immediate revert, deploy the previous build with --allow-behind-main first, then land the revert. For a rows change, execute the restore procedure tested in shadow mode and approved in the owner ask; record restore_ok=true and the restore evidence in decision_note with PATCH /autopm/experiments/{id}. Never invent a restore procedure. Keep the experiment rolling_back until the server confirms the deployed revert and required restore evidence. Budget, skip, pause, kill and review-budget limits do not stop this revert.",
        "remove-old" => "The change has been kept for 30 days. Remove the OLD path and the switch, leaving the NEW behavior permanent. Recheck the experiment and switch immediately before merging; stop if it is no longer kept, on and unheld. Follow the normal reviewed PR and deployment workflow. The server removes the switch row after the cleanup merges.",
        _ => "The experiment was reverted. Remove the NEW code path and switch, preserving the OLD behavior. Recheck the experiment immediately before merging; stop if the owner re-enabled it or this ticket was cancelled. Follow the normal reviewed PR and deployment workflow.",
    };
    Job {
        key: format!("recovery:{}:{mode}:{epoch}", x.id),
        experiment: x.id.clone(), mode: mode.into(), epoch, parent: x.cycle_ticket,
        title: format!("Auto PM {mode}: {}", x.title),
        body: format!("Tier: Mid\n\nExperiment: {}\nSwitch: {}\nPlan: {} at {}\nImplementation tickets: {}\n\n{}\n\nRead the merged plan and approved shadow evidence before changing production. Link this ticket with sm pr and put Closes #<this ticket> in the PR body.", x.id, x.switch_key.as_deref().unwrap_or("none"), x.plan_path.as_deref().unwrap_or("see experiment record"), x.plan_commit.as_deref().unwrap_or("see experiment record"), x.tickets_json, instruction),
    }
}

pub trait Runtime {
    fn file(&self, job: &Job) -> Result<i64>;
    fn attach(&self, number: i64, parent: i64) -> Result<()>;
    fn start(&self, number: i64, job: &Job) -> Result<()>;
    fn merged(&self, number: i64) -> Result<bool>;
    fn effects(&self, effects: Vec<Effect>);
}

/// Whether a previously filed recovery ticket still describes the required work.
pub fn current(conn: &Connection, job: &Job, now: i64) -> Result<bool> {
    let Some(x) = experiments::get(conn, &job.experiment)? else {
        return Ok(false);
    };
    Ok(due(conn, &x, now)?.is_some_and(|(mode, epoch)| mode == job.mode && epoch == job.epoch))
}

pub fn tick(store: &AutopmStore, now: i64, runtime: &dyn Runtime) -> Result<()> {
    let conn = store.open()?;
    schema(&conn)?;
    for x in experiments::list(&conn)? {
        if let Some((mode, epoch)) = due(&conn, &x, now)? {
            let job = job(&x, mode, epoch);
            conn.execute(
                "INSERT OR IGNORE INTO autopm_recovery(key,payload) VALUES(?1,?2)",
                params![job.key, serde_json::to_string(&job)?],
            )?;
        }
    }
    let jobs: Vec<(String, Option<i64>)> = conn
        .prepare("SELECT payload,number FROM autopm_recovery WHERE done=0 ORDER BY key")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut errors = Vec::new();
    for (payload, number) in jobs {
        let job: Job = serde_json::from_str(&payload)?;
        if let Err(error) = advance(store, now, runtime, &job, number) {
            errors.push(format!("{}: {error:#}", job.key));
        }
    }
    anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

fn advance(
    store: &AutopmStore,
    now: i64,
    runtime: &dyn Runtime,
    job: &Job,
    number: Option<i64>,
) -> Result<()> {
    let mut conn = store.open()?;
    if !current(&conn, job, now)? {
        if job.kind() == "cleanup" {
            if let Some(number) = number {
                cancel(&conn, number, now, runtime)?;
            }
        }
        conn.execute("UPDATE autopm_recovery SET done=1 WHERE key=?1", [&job.key])?;
        return Ok(());
    }
    let number = match number {
        Some(n) => n,
        None => {
            let n = runtime.file(job)?;
            conn.execute(
                "UPDATE autopm_recovery SET number=?2 WHERE key=?1",
                params![job.key, n],
            )?;
            n
        }
    };
    // The owner can change state while GitHub is responding. Register only
    // against the current state, atomically with the cancellation predicate.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let valid = current(&tx, job, now)?;
    tx.execute("INSERT OR IGNORE INTO autopm_fixes(repo,number,kind,cycle_ticket,experiment_id,created_at_ms,cancelled_at_ms)
        VALUES(?1,?2,?3,?4,?5,?6,?7)", params![REPO,number,job.kind(),job.parent,job.experiment,now,(!valid).then_some(now)])?;
    tx.commit()?;
    if !valid || fix(&conn, number)?.is_some_and(|f| f.cancelled_at_ms.is_some()) {
        cancel(&conn, number, now, runtime)?;
        conn.execute("UPDATE autopm_recovery SET done=1 WHERE key=?1", [&job.key])?;
        return Ok(());
    }
    runtime.attach(number, job.parent)?;
    if job.kind() == "cleanup" && runtime.merged(number)? {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if current(&tx, job, now)? {
            tx.execute(
                "DELETE FROM experiment_switches WHERE experiment_id=?1",
                [&job.experiment],
            )?;
            tx.execute("UPDATE autopm_recovery SET done=1 WHERE key=?1", [&job.key])?;
        }
        tx.commit()?;
        return Ok(());
    }
    if ticket_open(&conn, number)? && live_holders(&conn, number)?.is_empty() {
        runtime.start(number, job)?;
    }
    Ok(())
}

fn cancel(conn: &Connection, number: i64, now: i64, runtime: &dyn Runtime) -> Result<()> {
    conn.execute("UPDATE autopm_fixes SET cancelled_at_ms=COALESCE(cancelled_at_ms,?3) WHERE repo=?1 AND number=?2", params![REPO,number,now])?;
    let reason = "experiment changed; recovery work is no longer applicable".to_owned();
    let mut effects = vec![
        Effect::CloseIssue {
            number,
            comment: reason.clone(),
        },
        Effect::CancelAutoStart {
            number,
            reason: reason.clone(),
        },
    ];
    for pr in linked_prs(conn, number)? {
        effects.push(Effect::MergeHold {
            pr,
            reason: reason.clone(),
        });
    }
    for session_id in live_holders(conn, number)? {
        effects.push(Effect::Message {
            session_id,
            text: format!(
                "[sm autopm] cleanup cancelled: {reason}. Close your PR unmerged and stop"
            ),
        });
    }
    runtime.effects(effects);
    Ok(())
}
