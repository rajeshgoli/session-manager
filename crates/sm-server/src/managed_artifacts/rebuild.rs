//! Durable rebuild plans retain the original recipe independently of replay jobs.
use super::*;
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub original_job: String,
    pub commit: String,
    pub tag: String,
    pub cwd_rel: String,
    pub outputs: Vec<String>,
    pub inputs: Vec<String>,
    pub disk_bytes: i64,
    pub request: crate::queue::CreateQueueJob,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub root: ArtifactRoot,
    pub set: String,
    pub members: Vec<String>,
    pub requester: String,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub retry: bool,
}

/// Validate and snapshot the complete original recipe before changing any state.
pub fn plan(conn: &Connection, root: &ArtifactRoot, name: &str, requester: &str) -> Result<Plan> {
    let first = artifact(conn, &root.path, name)?;
    let failed_op = first["op"]
        .as_str()
        .filter(|op| op.starts_with("rebuild_failed:"));
    let set = first["set_id"].as_str().context("artifact set")?;
    let members = store::query(
        conn,
        "SELECT * FROM artifacts WHERE root=?1 AND set_id=?2 ORDER BY name",
        &[&root.path, &set],
    )?;
    let mut names = Vec::new();
    for a in &members {
        let name = a["name"].as_str().context("artifact name")?;
        if (a["state"] != "deleted"
            && !(failed_op.is_some() && a["state"] == "building" && a["op"] == first["op"]))
            || a["recipe_complete"] != 1
            || store::has_gpu(a)
        {
            bail!("{name} must be deleted and rebuildable");
        }
        if !a["op"].is_null() && failed_op != a["op"].as_str() {
            bail!("{name} is busy");
        }
        ensure_not_in_use(conn, &root.path, name)?;
        names.push(name.to_owned());
    }
    let rows = store::query(conn, "SELECT j.* FROM artifact_jobs j JOIN artifacts a ON a.root=j.root AND a.name=j.name WHERE a.root=?1 AND a.set_id=?2 AND j.state='succeeded' ORDER BY j.seq,j.job_id,j.name", &[&root.path, &set])?;
    let mut steps: Vec<Step> = Vec::new();
    let mut indices = BTreeMap::new();
    let mut covered = BTreeSet::new();
    for row in rows {
        let name = row["name"].as_str().context("recipe output")?.to_owned();
        covered.insert(name.to_ascii_lowercase());
        let id = row["job_id"].as_str().context("recipe job")?.to_owned();
        if let Some(index) = indices.get(&id).copied() {
            let step: &mut Step = &mut steps[index];
            step.outputs.push(name);
            continue;
        }
        let commit = row["commit_sha"]
            .as_str()
            .context("recipe commit is missing")?;
        let tag = row["tag"]
            .as_str()
            .context("preserved recipe tag is missing")?;
        let cwd_rel = row["cwd_rel"]
            .as_str()
            .context("recipe relative working directory is missing")?;
        if row["dirty"] != 0 || commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit())
        {
            bail!("recipe job {id} has no clean preserved commit");
        }
        if std::path::Path::new(cwd_rel).components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        }) {
            bail!("recipe working directory escapes its worktree");
        }
        let inputs = store::query(
            conn,
            "SELECT DISTINCT input FROM artifact_inputs WHERE root=?1 AND job_id=?2 ORDER BY input",
            &[&root.path, &id],
        )?;
        let mut input_names = Vec::new();
        for input in inputs {
            let input = input["input"].as_str().context("recipe input")?.to_owned();
            let a = artifact(conn, &root.path, &input)?;
            if a["state"] == "deleted" && !names.iter().any(|n| n.eq_ignore_ascii_case(&input)) {
                bail!("input {input} is deleted; rebuild it first");
            }
            input_names.push(input);
        }
        let argv = row["argv_json"]
            .as_str()
            .map(serde_json::from_str)
            .transpose()?;
        let script = row["script_text"].as_str().map(str::to_owned);
        if argv.is_none() == script.is_none() {
            bail!("recipe job {id} must have exactly one command");
        }
        indices.insert(id.clone(), steps.len());
        steps.push(Step {
            original_job: id,
            commit: commit.into(),
            tag: tag.into(),
            cwd_rel: cwd_rel.into(),
            outputs: vec![name],
            inputs: input_names,
            disk_bytes: row["bytes_written"].as_i64().unwrap_or(0).max(0),
            request: crate::queue::CreateQueueJob {
                local_submitter: None,
                job_type: row["type"].as_str().context("recipe job type")?.into(),
                label: row["label"].as_str().unwrap_or("artifact rebuild").into(),
                requester_session_id: Some(requester.into()),
                notify_session_id: requester.into(),
                cwd: String::new(),
                argv,
                script,
                env: serde_json::from_str(row["env_json"].as_str().unwrap_or("{}"))?,
                timeout_seconds: row["timeout_seconds"].as_i64().context("recipe timeout")?,
                cpu_percent: None,
                gpu_percent: None,
                memory_bytes: None,
                rank_tickets: None,
            },
        });
    }
    if names
        .iter()
        .any(|n| !covered.contains(&n.to_ascii_lowercase()))
    {
        bail!("every set member must have a succeeded recipe job");
    }
    // Internal dependencies must have been produced by an earlier recipe step.
    let mut available = BTreeSet::new();
    for step in &steps {
        for input in &step.inputs {
            if names.iter().any(|n| n.eq_ignore_ascii_case(input))
                && !available.contains(&input.to_ascii_lowercase())
            {
                bail!(
                    "input {input} is unavailable before recipe job {}",
                    step.original_job
                );
            }
        }
        available.extend(step.outputs.iter().map(|n| n.to_ascii_lowercase()));
    }
    Ok(Plan {
        root: root.clone(),
        set: set.into(),
        members: names,
        requester: requester.into(),
        steps,
        retry: failed_op.is_some(),
    })
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Execution {
    pub queue_dir: std::path::PathBuf,
    pub messages: std::path::PathBuf,
    pub worktrees: std::path::PathBuf,
    pub policy: crate::queue::QueueAdmissionPolicy,
    pub cancel_grace: u64,
}

pub fn start(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    name: &str,
    requester: &str,
    execution: Execution,
) -> Result<Value> {
    let mut conn = manager.store.open()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let plan = plan(&tx, root, name, requester)?;
    let id = format!(
        "rebuild-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    tx.execute("INSERT INTO artifact_rebuilds(id,root,set_id,plan_json,execution_json,state,started_at) VALUES(?1,?2,?3,?4,?5,'preparing',?6)", params![id,root.path,plan.set,serde_json::to_string(&plan)?,serde_json::to_string(&execution)?,now()])?;
    for index in 0..plan.steps.len() {
        tx.execute(
            "INSERT INTO artifact_rebuild_steps(run_id,position) VALUES(?1,?2)",
            params![id, index],
        )?;
    }
    for name in &plan.members {
        tx.execute(
            "UPDATE artifacts SET state='building',op=?3 WHERE root=?1 AND name=?2",
            params![root.path, name, format!("rebuilding:{id}")],
        )?;
        event(
            &tx,
            &root.path,
            name,
            "rebuild_started",
            requester,
            json!({"run_id":id}),
        )?;
    }
    tx.commit()?;
    Ok(json!({"rebuild_id":id,"state":"preparing","members":plan.members}))
}

/// One short-lived coordinator per store; queue completions and startup scans
/// wake it. It does not watch processes or poll running jobs.
pub fn kick(manager: ManagedArtifacts) {
    std::thread::spawn(move || {
        if let Err(error) = resume(&manager) {
            eprintln!("artifact rebuild recovery deferred: {error:#}");
        }
    });
}

struct Coordinator(ManagedArtifacts);
fn active() -> &'static std::sync::Mutex<BTreeMap<std::path::PathBuf, bool>> {
    static ACTIVE: std::sync::OnceLock<std::sync::Mutex<BTreeMap<std::path::PathBuf, bool>>> =
        std::sync::OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}
impl Drop for Coordinator {
    fn drop(&mut self) {
        let pending = active()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0.store.path)
            .unwrap_or(false);
        if pending {
            kick(self.0.clone());
        }
    }
}

fn worktree(execution: &Execution, id: &str, commit: &str) -> std::path::PathBuf {
    execution
        .worktrees
        .join(format!("sm-{id}-{}", &commit[..7]))
}
fn prepare_worktrees(plan: &Plan, execution: &Execution, id: &str) -> Result<()> {
    use submission::git_in;
    let checkout = crate::sessions::expand_home(&plan.root.repo_checkout);
    std::fs::create_dir_all(&execution.worktrees)?;
    for step in &plan.steps {
        let path = worktree(execution, id, &step.commit);
        if path.exists() {
            if git_in(&path, &["rev-parse", "HEAD"])? != step.commit {
                bail!("rebuild worktree {} is at another commit", path.display());
            }
            continue;
        }
        git_in(
            &checkout,
            &["fetch", "origin", &format!("refs/tags/{}", step.tag)],
        )?;
        if git_in(&checkout, &["rev-parse", "FETCH_HEAD"])? != step.commit {
            bail!(
                "preserved tag {} no longer points at {}",
                step.tag,
                step.commit
            );
        }
        git_in(
            &checkout,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().context("worktree path")?,
                &step.commit,
            ],
        )?;
    }
    Ok(())
}

pub fn resume(manager: &ManagedArtifacts) -> Result<()> {
    {
        let mut running = active().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = running.get_mut(&manager.store.path) {
            *pending = true;
            return Ok(());
        }
        running.insert(manager.store.path.clone(), false);
    }
    let _guard = Coordinator(manager.clone());
    let conn = manager.store.open()?;
    for row in store::query(
        &conn,
        "SELECT * FROM artifact_rebuilds WHERE state IN ('preparing','running') OR notified=0",
        &[],
    )? {
        let id = row["id"].as_str().context("rebuild id")?;
        let plan: Plan = serde_json::from_str(row["plan_json"].as_str().context("rebuild plan")?)?;
        let execution: Execution = serde_json::from_str(
            row["execution_json"]
                .as_str()
                .context("rebuild execution")?,
        )?;
        if matches!(row["state"].as_str(), Some("succeeded" | "failed")) {
            cleanup_and_notify(&conn, id, &plan, &execution, row["state"] == "succeeded")?;
            continue;
        }
        if let Err(error) = advance(manager, &conn, id, &plan, &execution) {
            fail(&conn, id, &plan, &execution, &error)?;
        }
    }
    Ok(())
}

fn advance(
    manager: &ManagedArtifacts,
    conn: &Connection,
    id: &str,
    plan: &Plan,
    execution: &Execution,
) -> Result<()> {
    use crate::queue::RetainedQueueStore;
    let state: String = conn.query_row(
        "SELECT state FROM artifact_rebuilds WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    if state == "preparing" {
        prepare_outputs(conn, plan, id)?;
        prepare_worktrees(plan, execution, id)?;
    }

    conn.execute(
        "UPDATE artifact_rebuilds SET state='running' WHERE id=?1",
        [id],
    )?;
    for (index, step) in plan.steps.iter().enumerate() {
        let row = store::query(
            conn,
            "SELECT job_id,error FROM artifact_rebuild_steps WHERE run_id=?1 AND position=?2",
            &[&id, &(index as i64)],
        )?
        .pop()
        .context("rebuild step")?;
        if let Some(error) = row["error"].as_str() {
            bail!(
                "recipe step {} output validation failed: {error}",
                index + 1
            );
        }
        if let Some(job_id) = row["job_id"].as_str() {
            let job = RetainedQueueStore::get_queue_job_from_path(
                &execution.queue_dir.join("queue_runner.db"),
                job_id,
            )?
            .context("rebuild queue job disappeared")?;
            if matches!(job.state.as_str(), "pending" | "running") {
                RetainedQueueStore::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(&execution.queue_dir,&execution.messages,execution.cancel_grace,execution.policy)?;
                return Ok(());
            }
            if job.state != "succeeded" {
                bail!(
                    "recipe step {} ended {} (job {job_id})",
                    index + 1,
                    job.state
                );
            }
            // Completion must release the artifact reservation before advancing.
            let reserved: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM artifact_job_reservations WHERE job_id=?1)",
                [job_id],
                |r| r.get(0),
            )?;
            if reserved {
                return Ok(());
            }
            continue;
        }
        let path = worktree(execution, id, &step.commit);
        let mut request = step.request.clone();
        let cwd = std::fs::canonicalize(path.join(&step.cwd_rel))?;
        if !cwd.starts_with(std::fs::canonicalize(&path)?) {
            bail!("recipe cwd escapes rebuild worktree");
        }
        request.cwd = cwd.display().to_string();
        request.label = format!("{id} step {}: {}", index + 1, step.request.label);
        if let Some(script) = &request.script {
            std::fs::create_dir_all(path.join(".sm-rebuild"))?;
            std::fs::write(path.join(format!(".sm-rebuild/{index}.sh")), script)?;
        }
        let declarations = submission::Declarations {
            outputs: step.outputs.clone(),
            inputs: step.inputs.clone(),
            disk_bytes: Some(step.disk_bytes),
            record: None,
        };
        let mut prepared =
            submission::PreparedSubmission::prepare(manager, &plan.root, declarations, &request)?;
        prepared.rebuild = Some((id.to_owned(), index));
        RetainedQueueStore::create_queue_job_with_artifacts(
            &execution.queue_dir,
            request,
            300,
            None,
            Some((manager, &prepared)),
        )?;
        RetainedQueueStore::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(&execution.queue_dir,&execution.messages,execution.cancel_grace,execution.policy)?;
        return Ok(());
    }
    // Validate every output before restoring the original records. Replay jobs
    // never overwrite consumers, marks, descriptions, or the original recipe.
    for name in &plan.members {
        super::queue::validate_output(conn, &plan.root, name)?;
    }
    let tx = conn.unchecked_transaction()?;
    for name in &plan.members {
        tx.execute("UPDATE artifacts SET state='live',op=NULL,deleted_at=NULL,deleted_by=NULL,delete_reason=NULL WHERE root=?1 AND name=?2", params![plan.root.path,name])?;
        event(
            &tx,
            &plan.root.path,
            name,
            "rebuilt",
            &plan.requester,
            json!({"run_id":id}),
        )?;
    }
    tx.execute(
        "UPDATE artifact_rebuilds SET state='succeeded',finished_at=?2 WHERE id=?1",
        params![id, now()],
    )?;
    tx.commit()?;
    // Cleanup and notification retry independently of successful execution.
    if let Err(error) = cleanup_and_notify(conn, id, plan, execution, true) {
        eprintln!("rebuild cleanup deferred for {id}: {error:#}");
    }
    Ok(())
}
fn prepare_outputs(conn: &Connection, plan: &Plan, id: &str) -> Result<()> {
    for name in &plan.members {
        let a = artifact(conn, &plan.root.path, name)?;
        let base = if a["location"] == "cold" {
            &plan.root.cold_store
        } else {
            &plan.root.path
        };
        let path = std::path::Path::new(base).join(name);
        let logical = std::path::Path::new(&plan.root.path).join(name);
        if !plan.retry {
            if path.symlink_metadata().is_ok() || logical.symlink_metadata().is_ok() {
                bail!("deleted output {name} still has a filesystem entry");
            }
            continue;
        }
        if let Ok(metadata) = path.symlink_metadata() {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                bail!("failed output {name} is not a managed directory");
            }
            let archive = std::path::Path::new(base).join(format!(".sm-{id}-{name}"));
            if archive.symlink_metadata().is_ok() {
                bail!("diagnostic archive already exists for {name}");
            }
            std::fs::rename(&path, archive)?;
        }
        if logical != path && logical.symlink_metadata().is_ok() {
            if !logical.symlink_metadata()?.file_type().is_symlink()
                || std::fs::read_link(&logical)? != path
            {
                bail!("failed output {name} has an unexpected logical path");
            }
            std::fs::remove_file(logical)?;
        }
    }
    Ok(())
}

fn cleanup_and_notify(
    conn: &Connection,
    id: &str,
    plan: &Plan,
    execution: &Execution,
    success: bool,
) -> Result<()> {
    if success {
        let checkout = crate::sessions::expand_home(&plan.root.repo_checkout);
        for commit in plan
            .steps
            .iter()
            .map(|s| &s.commit)
            .collect::<BTreeSet<_>>()
        {
            let path = worktree(execution, id, commit);
            if path.exists() {
                submission::git_in(
                    &checkout,
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        path.to_str().context("worktree path")?,
                    ],
                )?;
            }
        }
    }
    let started: String = conn.query_row(
        "SELECT started_at FROM artifact_rebuilds WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    let minutes =
        (OffsetDateTime::now_utc() - OffsetDateTime::parse(&started, &Rfc3339)?).whole_minutes();
    let text = if success {
        format!("[sm artifact] rebuilt {} in {minutes} min", plan.set)
    } else {
        format!("[sm artifact] rebuild {} failed; outputs remain building and worktrees are retained ({id})",plan.set)
    };
    crate::queue::RetainedQueueStore::new(execution.messages.clone())
        .enqueue_message_once_with_metadata(
            &format!("artifact-{id}-finished"),
            &plan.requester,
            &text,
            "sequential",
            crate::queue::QueueMessageMetadata {
                message_category: Some("queue-completion".into()),
                ..Default::default()
            },
        )?;
    conn.execute("UPDATE artifact_rebuilds SET notified=1 WHERE id=?1", [id])?;
    Ok(())
}
fn fail(
    conn: &Connection,
    id: &str,
    plan: &Plan,
    execution: &Execution,
    error: &anyhow::Error,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for name in &plan.members {
        tx.execute(
            "UPDATE artifacts SET op=?3 WHERE root=?1 AND name=?2",
            params![plan.root.path, name, format!("rebuild_failed:{id}")],
        )?;
        event(
            &tx,
            &plan.root.path,
            name,
            "rebuild_failed",
            &plan.requester,
            json!({"run_id":id,"error":format!("{error:#}")}),
        )?;
    }
    tx.execute(
        "UPDATE artifact_rebuilds SET state='failed',finished_at=?2,error=?3 WHERE id=?1",
        params![id, now(), format!("{error:#}")],
    )?;
    tx.commit()?;
    cleanup_and_notify(conn, id, plan, execution, false)
}
