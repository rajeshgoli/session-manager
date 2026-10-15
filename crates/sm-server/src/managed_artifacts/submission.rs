//! Queue submission preparation and persistence. Preparation performs git I/O;
//! persistence rechecks mutable artifact state inside the caller's transaction.
//! No directories are created here: admission owns output placement.
use super::*;
use crate::queue::CreateQueueJob;
use anyhow::Context;
use std::{collections::BTreeMap, path::Path, process::Command};

pub const MAX_SCRIPT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Declarations {
    pub outputs: Vec<String>,
    pub inputs: Vec<String>,
    pub disk_bytes: Option<i64>,
    pub record: Option<RecordFlags>,
}
impl Declarations {
    pub fn validate(&self, manager: &ManagedArtifacts, root: &ArtifactRoot) -> Result<()> {
        for name in self.outputs.iter().chain(&self.inputs) {
            validate_name(name)?;
        }
        if !self.outputs.is_empty() && self.disk_bytes.is_none() {
            bail!("--artifact requires --disk");
        }
        if self.disk_bytes.is_some_and(|n| n < 0) {
            bail!("disk budget must be nonnegative");
        }
        if let Some(record) = &self.record {
            if self.outputs.is_empty() {
                bail!("record flags require --artifact");
            }
            if record.gpu_hours.is_some() || record.gpu_cost.is_some() {
                bail!("queue record flags do not accept GPU hours or cost");
            }
            record.validate(&manager.config, root)?;
        }
        Ok(())
    }
}

/// Execute git in the build directory, never in the server's current directory.
/// An error is distinct from an empty successful result (notably dirty status).
pub fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("run git for artifact recipe")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?
        .trim_end_matches(['\n', '\r'])
        .to_owned())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitState {
    pub commit_sha: Option<String>,
    pub cwd_rel: Option<String>,
    pub dirty: bool,
}
impl GitState {
    pub fn capture(cwd: &Path) -> Self {
        // Non-git and unreadable repositories still permit builds, but cannot
        // attest a complete recipe. Never turn a failed status read into clean.
        let capture = || -> Result<Self> {
            let top = git_in(cwd, &["rev-parse", "--show-toplevel"])?;
            let top = std::fs::canonicalize(top)?;
            let cwd = std::fs::canonicalize(cwd)?;
            Ok(Self {
                commit_sha: Some(git_in(&cwd, &["rev-parse", "HEAD"])?),
                cwd_rel: Some(cwd.strip_prefix(top)?.to_string_lossy().into_owned()),
                dirty: !git_in(&cwd, &["status", "--porcelain", "--untracked-files=all"])?
                    .is_empty(),
            })
        };
        capture().unwrap_or(Self {
            commit_sha: None,
            cwd_rel: None,
            dirty: true,
        })
    }
}

/// The prepared command is private so it cannot change after git/input capture.
/// `persist` returns deleted-reference warnings for durable delivery by the
/// queue caller; it never sends a message before the transaction commits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedSubmission {
    pub(super) declarations: Declarations,
    pub(super) root: ArtifactRoot,
    pub(super) request: CreateQueueJob,
    git: GitState,
    detected: Vec<String>,
}
impl PreparedSubmission {
    pub fn prepare(
        manager: &ManagedArtifacts,
        root: &ArtifactRoot,
        declarations: Declarations,
        request: &CreateQueueJob,
    ) -> Result<Self> {
        declarations.validate(manager, root)?;
        if request
            .script
            .as_ref()
            .is_some_and(|s| s.len() > MAX_SCRIPT_BYTES)
        {
            bail!("artifact input detection requires a script of at most 1 MiB");
        }
        let mut texts = request.argv.clone().unwrap_or_default();
        if let Some(script) = &request.script {
            texts.push(script.clone());
        }
        let detected = detect_names(root, &texts);
        Ok(Self {
            declarations,
            root: root.clone(),
            request: request.clone(),
            git: GitState::capture(Path::new(&request.cwd)),
            detected,
        })
    }

    /// Call after attaching `artifacts.db` and starting an IMMEDIATE transaction
    /// on the queue connection. The caller inserts the queue row in this same
    /// transaction and rolls it back on ANY error from this method.
    /// SQLite WAL does not guarantee power-loss atomicity across attached files;
    /// queue startup reconciliation must repair that boundary before admission.
    pub fn persist(&self, tx: &rusqlite::Transaction<'_>, job_id: &str) -> Result<Vec<String>> {
        let root = &self.root;
        let actor = self
            .request
            .requester_session_id
            .as_deref()
            .unwrap_or(&self.request.notify_session_id);
        let mut outputs = BTreeMap::new();
        for name in &self.declarations.outputs {
            let rows = store::query(
                tx,
                "SELECT * FROM artifacts WHERE root=?1 AND name=?2",
                &[&root.path, name],
            )?;
            let stored = if let Some(a) = rows.first() {
                if a["state"] != "building" || !a["op"].is_null() {
                    bail!("{name} is not an available building output; choose a new name");
                }
                a["name"].as_str().context("artifact name")?.to_owned()
            } else {
                let path = Path::new(&root.path).join(name);
                match path.symlink_metadata() {
                    Ok(_) => bail!("unrecorded filesystem entry exists: {name}"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                name.clone()
            };
            outputs.insert(stored.to_ascii_lowercase(), stored);
        }
        let mut inputs: BTreeMap<String, (String, &'static str)> = BTreeMap::new();
        let mut warnings = Vec::new();
        for (names, declared) in [(&self.declarations.inputs, true), (&self.detected, false)] {
            for name in names {
                let Some(a) = resolve_input(tx, &root.path, name)? else {
                    if declared {
                        bail!("artifact input not found: {name}");
                    }
                    continue;
                };
                let stored = a["name"].as_str().context("artifact name")?.to_owned();
                let key = stored.to_ascii_lowercase();
                if outputs.contains_key(&key) {
                    if declared {
                        bail!("{stored} cannot be both input and output");
                    }
                    continue;
                }
                if inputs.contains_key(&key) {
                    continue;
                }
                match a["state"].as_str() {
                    Some("deleted") if !declared => {
                        warnings.push(format!(
                            "[sm artifact] job {} references deleted artifact {stored}",
                            self.request.label
                        ));
                    }
                    Some("deleted") => {
                        if a["rebuildable"] == 1 {
                            bail!("run sm artifact rebuild {stored} first");
                        }
                        bail!("deleted input {stored} is not rebuildable");
                    }
                    Some("live" | "frozen") => {}
                    Some("building") => ensure_no_writer(tx, &root.path, &stored)?,
                    _ => bail!("input {stored} must be live, frozen or building without a writer"),
                }
                if !a["op"].is_null() && a["op"] != "thawing" {
                    bail!("input {stored} has an operation in progress");
                }
                inputs.insert(
                    key,
                    (stored, if declared { "declared" } else { "detected" }),
                );
            }
        }
        let outputs: Vec<_> = outputs.into_values().collect();
        let input_names: Vec<_> = inputs.values().map(|(name, _)| name).collect();
        let pending = self
            .declarations
            .record
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        tx.execute("INSERT INTO artifact_job_reservations(job_id,root,outputs_json,inputs_json,disk_bytes,pending_record_json) VALUES(?1,?2,?3,?4,?5,?6)", params![job_id,root.path,serde_json::to_string(&outputs)?,serde_json::to_string(&input_names)?,self.declarations.disk_bytes.unwrap_or(0),pending])?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM artifact_jobs",
            [],
            |r| r.get(0),
        )?;
        for name in &outputs {
            let inserted = tx.execute("INSERT OR IGNORE INTO artifacts(root,name,state,location,set_id,origin,owner_session_id,repo,created_at) VALUES(?1,?2,'building','root',?2,'queue',?3,?4,?5)",params![root.path,name,actor,root.repo,now()])?;
            if inserted != 0 {
                event(
                    tx,
                    &root.path,
                    name,
                    "created",
                    actor,
                    json!({"job_id":job_id}),
                )?;
            }
            tx.execute("INSERT INTO artifact_jobs(root,name,job_id,seq,type,label,argv_json,script_text,env_json,timeout_seconds,cwd,cwd_rel,commit_sha,dirty,state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'queued')", params![root.path,name,job_id,seq,self.request.job_type,self.request.label,self.request.argv.as_ref().map(serde_json::to_string).transpose()?,self.request.script,serde_json::to_string(&self.request.env)?,self.request.timeout_seconds,self.request.cwd,self.git.cwd_rel,self.git.commit_sha,self.git.dirty])?;
            for (input, source) in inputs.values() {
                tx.execute("INSERT INTO artifact_inputs(root,name,input,source,job_id) VALUES(?1,?2,?3,?4,?5)",params![root.path,name,input,source,job_id])?;
            }
        }
        // Union entire existing components, not only this job's outputs.
        let mut sets = std::collections::BTreeSet::new();
        for name in &outputs {
            let a = artifact(tx, &root.path, name)?;
            sets.insert(a["set_id"].as_str().context("set id")?.to_owned());
        }
        if let Some(set_id) = sets.first() {
            for old in &sets {
                tx.execute(
                    "UPDATE artifacts SET set_id=?3 WHERE root=?1 AND set_id=?2",
                    params![root.path, old, set_id],
                )?;
            }
        }
        Ok(warnings)
    }
}

fn resolve_input(conn: &Connection, root: &str, name: &str) -> Result<Option<Value>> {
    let direct = store::query(
        conn,
        "SELECT * FROM artifacts WHERE root=?1 AND name=?2",
        &[&root, &name],
    )?;
    if let Some(a) = direct.into_iter().next() {
        return Ok(Some(a));
    }
    Ok(store::query(conn,"SELECT a.* FROM artifacts a JOIN artifact_aliases x ON x.root=a.root AND x.target_name=a.name WHERE x.root=?1 AND x.alias=?2", &[&root,&name])?.into_iter().next())
}

pub(super) fn awaiting_placement(conn: &Connection, root: &str, name: &str) -> Result<bool> {
    let mut waiting = false;
    for r in store::query(
        conn,
        "SELECT outputs_json,start_bytes FROM artifact_job_reservations WHERE root=?1",
        &[&root],
    )? {
        let outputs: Vec<String> =
            serde_json::from_str(r["outputs_json"].as_str().context("reservation outputs")?)?;
        if outputs
            .iter()
            .any(|output| output.eq_ignore_ascii_case(name))
        {
            // A later queued writer cannot excuse a started writer's missing output.
            if !r["start_bytes"].is_null() {
                return Ok(false);
            }
            waiting = true;
        }
    }
    Ok(waiting)
}

fn ensure_no_writer(conn: &Connection, root: &str, name: &str) -> Result<()> {
    for r in store::query(
        conn,
        "SELECT job_id,outputs_json FROM artifact_job_reservations WHERE root=?1",
        &[&root],
    )? {
        let names: Vec<String> =
            serde_json::from_str(r["outputs_json"].as_str().context("reservation outputs")?)?;
        if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            bail!("input {name} is being written by job {}", r["job_id"]);
        }
    }
    Ok(())
}

/// Attach once before beginning the queue transaction. Unqualified artifact
/// table names resolve here because the queue schema has no artifact tables.
pub fn attach_store(conn: &Connection, store: &ArtifactStore) -> Result<()> {
    if !conn.is_autocommit() {
        bail!("attach artifacts before starting the queue transaction");
    }
    drop(store.open()?);
    let path = std::fs::canonicalize(&store.path)?;
    for db in store::query(conn, "PRAGMA database_list", &[])? {
        if db["name"] == "managed_artifacts" {
            if Path::new(db["file"].as_str().unwrap_or("")) != path {
                bail!("different artifacts database is already attached");
            }
            return Ok(());
        }
    }
    conn.execute(
        "ATTACH DATABASE ?1 AS managed_artifacts",
        [path.to_string_lossy().as_ref()],
    )?;
    Ok(())
}

/// Extract the complete segment, then resolve against records inside the
/// transaction. This avoids matching `bundle` in `bundle-old` and captures
/// nested file references (`bundle/data.bin`) as references to `bundle`.
fn detect_names(root: &ArtifactRoot, texts: &[String]) -> Vec<String> {
    let mut paths = vec![root.path.clone(), root.cold_store.clone()];
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rel) = Path::new(&root.path).strip_prefix(home) {
            paths.push(format!("~/{}", rel.display()));
            paths.push(format!("$HOME/{}", rel.display()));
        }
    }
    let mut names = std::collections::BTreeSet::new();
    for path in paths.into_iter().filter(|p| !p.is_empty()) {
        let pattern = format!(
            r"(?i){}/([A-Za-z0-9][A-Za-z0-9_.-]*)",
            regex::escape(path.trim_end_matches('/'))
        );
        let re = regex::Regex::new(&pattern).expect("escaped literal path");
        for text in texts {
            for capture in re.captures_iter(text) {
                let name = &capture[1];
                if validate_name(name).is_ok() {
                    names.insert(name.to_ascii_lowercase());
                }
            }
        }
    }
    names.into_iter().collect()
}

#[cfg(test)]
mod tests;
