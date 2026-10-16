//! Managed artifact records. Filesystem operations share the database's immediate
//! transaction so a competing mutation cannot invalidate a deletion's checks.
pub mod archive;
pub mod attribution;
pub mod backup;
pub mod config;
pub mod delete;
pub mod hotcold;
pub mod moves;
pub mod needs_you;
pub mod ops;
pub mod pressure;
pub mod queue;
pub mod rebuild;
pub mod recipe;
pub mod scan;
pub mod score;
pub mod store;
pub mod submission;
use anyhow::{bail, Result};
use config::{ArtifactRoot, ManagedArtifactsConfig};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use store::{artifact, event, ArtifactStore};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC time formats")
}
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    {
        bail!("invalid artifact name: {name}");
    }
    Ok(())
}
pub fn validate_note(note: &str, max: usize) -> Result<()> {
    if note.trim().is_empty()
        || note.chars().count() > max
        || note
            .chars()
            .any(|c| c == '\n' || c == '\r' || c == '\u{2028}' || c == '\u{2029}')
    {
        bail!("note must be one line of 1–{max} characters");
    }
    Ok(())
}
pub fn validate_date(date: &str) -> Result<()> {
    validate_date_at(date, OffsetDateTime::now_utc())
}
fn validate_date_at(date: &str, at: OffsetDateTime) -> Result<()> {
    let format = time::format_description::parse("[year]-[month]-[day]")?;
    let (today, _) = attribution::local_calendar(at)?;
    if time::Date::parse(date, &format)? <= time::Date::parse(&today, &format)? {
        bail!("keep-until must be after today");
    }
    Ok(())
}
pub fn consumer(reference: &str, repo: &str) -> Result<(String, i64)> {
    let (r, n) = reference
        .rsplit_once('#')
        .ok_or_else(|| anyhow::anyhow!("consumer must be #N or owner/repo#N"))?;
    let r = if r.is_empty() { repo } else { r };
    crate::owner_docs::validate_repo_slug(r)?;
    let n = n.parse::<i64>()?;
    if n <= 0 {
        bail!("consumer number must be positive");
    }
    Ok((r.into(), n))
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordFlags {
    pub note: String,
    pub describe: Option<String>,
    pub consumers: Vec<String>,
    pub project: bool,
    pub keep_until: Option<String>,
    pub recipe_complete: bool,
    pub gpu_hours: Option<f64>,
    pub gpu_cost: Option<f64>,
    pub ticket: Option<i64>,
    pub epic: Option<String>,
}
impl RecordFlags {
    pub fn validate(&self, config: &ManagedArtifactsConfig, root: &ArtifactRoot) -> Result<()> {
        validate_note(&self.note, config.note_max_chars)?;
        if self
            .describe
            .as_ref()
            .is_some_and(|s| s.chars().count() > 8000)
        {
            bail!("description exceeds 8000 characters");
        }
        if !self.project && self.keep_until.is_none() && self.consumers.is_empty() {
            bail!("record requires a consumer, --project or --keep-until");
        }
        if let Some(date) = &self.keep_until {
            validate_date(date)?;
        }
        for reference in &self.consumers {
            consumer(reference, &root.repo)?;
        }
        for amount in [self.gpu_hours, self.gpu_cost].into_iter().flatten() {
            if !amount.is_finite() || amount < 0.0 {
                bail!("GPU hours and cost must be finite and nonnegative");
            }
        }
        if self.ticket.is_some_and(|n| n <= 0) {
            bail!("ticket must be positive");
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct ManagedArtifacts {
    pub config: ManagedArtifactsConfig,
    pub store: ArtifactStore,
}
impl ManagedArtifacts {
    pub fn new(config: &ManagedArtifactsConfig) -> Result<Self> {
        let manager = Self {
            config: config.resolved()?,
            store: ArtifactStore::new(crate::sessions::expand_home(&config.db_path)),
        };
        let mut conn = manager.store.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for root in &manager.config.roots {
            store::set_meta(
                &tx,
                &format!("cold_days:{}", root.path),
                &manager.config.cold_after_days.to_string(),
            )?;
        }
        tx.commit()?;
        Ok(manager)
    }
    pub fn create(
        &self,
        root: &ArtifactRoot,
        name: &str,
        flags: &RecordFlags,
        actor: &str,
        actor_name: &str,
    ) -> Result<Value> {
        validate_name(name)?;
        flags.validate(&self.config, root)?;
        if flags.recipe_complete {
            bail!("hand-made artifacts cannot declare a recipe");
        }
        let mut conn = self.store.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if artifact(&tx, &root.path, name).is_ok_and(|a| a["state"] != "deleted") {
            bail!("artifact already exists: {name}");
        }
        let path = std::path::Path::new(&root.path).join(name);
        if path.symlink_metadata().is_ok() {
            bail!("filesystem entry already exists: {name}");
        }
        let mut location = "root";
        let stats = nix::sys::statvfs::statvfs(std::path::Path::new(&root.path))?;
        if u64::from(stats.blocks_available()) * 100
            < u64::from(stats.blocks()) * self.config.floor_percent
        {
            let cold = std::path::Path::new(&root.cold_store);
            let cs = nix::sys::statvfs::statvfs(cold)?;
            if u64::from(cs.blocks_available()) * 100
                < u64::from(cs.blocks()) * self.config.floor_percent
            {
                bail!("neither filesystem has room above its floor");
            }
            location = "cold";
        }
        let physical = if location == "cold" {
            std::path::Path::new(&root.cold_store).join(name)
        } else {
            path.clone()
        };
        std::fs::create_dir(&physical)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&physical, std::fs::Permissions::from_mode(0o755))?;
        if location == "cold" {
            if let Err(e) = std::os::unix::fs::symlink(&physical, &path) {
                let _ = std::fs::remove_dir(&physical);
                return Err(e.into());
            }
        }
        let result = (|| {
            // A deleted record can be recreated; old recipes must not attach to a new hand-made folder.
            for table in [
                "artifact_consumers",
                "artifact_signals",
                "artifact_jobs",
                "artifact_inputs",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE root=?1 AND name=?2"),
                    params![root.path, name],
                )?;
            }
            tx.execute(
                "DELETE FROM artifacts WHERE root=?1 AND name=?2",
                params![root.path, name],
            )?;
            tx.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,owner_session_id,owner_session_name,repo,created_at) VALUES(?1,?2,'building',?3,?2,'hand',?4,?5,?6,?7)",params![root.path,name,location,actor,actor_name,root.repo,now()])?;
            apply_record(&tx, root, name, flags, actor)?;
            event(&tx, &root.path, name, "created", actor, json!({}))?;
            let value = store::detail(&tx, &root.path, name)?;
            tx.commit()?;
            Ok(value)
        })();
        if result.is_err() {
            if location == "cold" {
                let _ = std::fs::remove_file(&path);
            }
            let _ = std::fs::remove_dir(&physical);
        }
        result
    }
    pub fn record(
        &self,
        root: &ArtifactRoot,
        names: &[String],
        flags: &RecordFlags,
        actor: &str,
    ) -> Result<Value> {
        flags.validate(&self.config, root)?;
        if names.is_empty() {
            bail!("name at least one artifact");
        }
        let mut conn = self.store.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for name in names {
            validate_name(name)?;
            let a = artifact(&tx, &root.path, name)?;
            if !matches!(a["state"].as_str(), Some("building" | "unrecorded")) {
                bail!("{name} must be building or unrecorded");
            }
            ensure_not_in_use(&tx, &root.path, name)?;
            std::fs::metadata(std::path::Path::new(&root.path).join(a["name"].as_str().unwrap()))
                .map_err(|error| anyhow::anyhow!("{name} cannot be recorded: {error}"))?;
        }
        tx.commit()?;
        if flags.recipe_complete {
            recipe::preserve(&conn, &self.config, root, names)?;
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for name in names {
            validate_name(name)?;
            let a = artifact(&tx, &root.path, name)?;
            if !matches!(a["state"].as_str(), Some("building" | "unrecorded")) {
                bail!("{name} must be building or unrecorded");
            }
            ensure_not_in_use(&tx, &root.path, name)?;
            std::fs::metadata(std::path::Path::new(&root.path).join(a["name"].as_str().unwrap()))
                .map_err(|error| anyhow::anyhow!("{name} cannot be recorded: {error}"))?;
        }
        for name in names {
            apply_record(&tx, root, name, flags, actor)?;
            tx.execute("UPDATE artifacts SET state='live',recipe_complete=?3,origin=CASE WHEN origin='unrecorded' THEN 'adopted' ELSE origin END WHERE root=?1 AND name=?2",params![root.path,name,flags.recipe_complete])?;
            event(&tx, &root.path, name, "recorded", actor, json!({}))?;
        }
        let result = names
            .iter()
            .map(|n| store::detail(&tx, &root.path, n))
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(json!(result))
    }
    pub fn done(&self, root: &ArtifactRoot, name: &str, actor: &str) -> Result<Value> {
        let mut conn = self.store.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let a = artifact(&tx, &root.path, name)?;
        if a["state"] != "building" || a["origin"] != "hand" || a["owner_session_id"] != actor {
            bail!("done requires your hand-made building artifact");
        }
        ensure_not_in_use(&tx, &root.path, name)?;
        tx.execute(
            "UPDATE artifacts SET state='live' WHERE root=?1 AND name=?2",
            params![root.path, name],
        )?;
        event(
            &tx,
            &root.path,
            name,
            "recorded",
            actor,
            json!({"done":true}),
        )?;
        let result = store::detail(&tx, &root.path, name)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn edit(
        &self,
        root: &ArtifactRoot,
        name: &str,
        action: &str,
        body: &Value,
        actor: &str,
    ) -> Result<Value> {
        let mut conn = self.store.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let a = artifact(&tx, &root.path, name)?;
        if !matches!(a["state"].as_str(), Some("live" | "frozen")) {
            bail!("only live or frozen records can be edited");
        }
        match action {
            "consumers" => {
                let (repo, number) = consumer(body["consumer"].as_str().unwrap_or(""), &root.repo)?;
                match body["operation"].as_str() {
                    Some("add") => {
                        tx.execute("INSERT OR IGNORE INTO artifact_consumers(root,name,repo,number,added_by,added_at,last_state) VALUES(?1,?2,?3,?4,?5,?6,'UNKNOWN')",params![root.path,name,repo,number,actor,now()])?;
                    }
                    Some("remove") => {
                        tx.execute("DELETE FROM artifact_consumers WHERE root=?1 AND name=?2 AND repo=?3 AND number=?4",params![root.path,name,repo,number])?;
                    }
                    _ => bail!("consumer operation must be add or remove"),
                }
            }
            "marks" => {
                if let Some(project) = body["project"].as_bool() {
                    tx.execute(
                        "UPDATE artifacts SET project=?3 WHERE root=?1 AND name=?2",
                        params![root.path, name, project],
                    )?;
                    if project && a["project"] != 1 {
                        event(&tx, &root.path, name, "project_added", actor, json!({}))?;
                    }
                }
                if let Some(date) = body.get("keep_until") {
                    if let Some(d) = date.as_str() {
                        validate_date(d)?;
                    } else if !date.is_null() {
                        bail!("keep_until must be a date or null");
                    }
                    tx.execute(
                        "UPDATE artifacts SET keep_until=?3 WHERE root=?1 AND name=?2",
                        params![root.path, name, date.as_str()],
                    )?;
                }
            }
            _ => bail!("unknown artifact action"),
        }
        event(&tx, &root.path, name, action, actor, body.clone())?;
        let result = store::detail(&tx, &root.path, name)?;
        tx.commit()?;
        Ok(result)
    }
}
fn apply_record(
    conn: &Connection,
    root: &ArtifactRoot,
    name: &str,
    flags: &RecordFlags,
    actor: &str,
) -> Result<()> {
    conn.execute("UPDATE artifacts SET note=?3,description=?4,project=?5,keep_until=?6,gpu_hours=?7,gpu_cost_usd=?8,ticket=?9,epic=?10,recorded_at=?11 WHERE root=?1 AND name=?2",params![root.path,name,flags.note,flags.describe,flags.project,flags.keep_until,flags.gpu_hours,flags.gpu_cost,flags.ticket,flags.epic,now()])?;
    conn.execute(
        "DELETE FROM artifact_consumers WHERE root=?1 AND name=?2",
        params![root.path, name],
    )?;
    for c in &flags.consumers {
        let (repo, number) = consumer(c, &root.repo)?;
        conn.execute("INSERT OR IGNORE INTO artifact_consumers(root,name,repo,number,added_by,added_at,last_state) VALUES(?1,?2,?3,?4,?5,?6,'UNKNOWN')",params![root.path,name,repo,number,actor,now()])?;
    }
    if flags.project {
        event(conn, &root.path, name, "project_added", actor, json!({}))?;
    }
    Ok(())
}
pub fn ensure_not_in_use(conn: &Connection, root: &str, name: &str) -> Result<()> {
    ensure_not_in_use_except(conn, root, name, "")
}

fn ensure_not_in_use_except(conn: &Connection, root: &str, name: &str, except: &str) -> Result<()> {
    let a = artifact(conn, root, name)?;
    if a["op"]
        .as_str()
        .is_some_and(|op| op.starts_with("rebuilding:"))
    {
        bail!("{name} is in use by an artifact rebuild");
    }
    let jobs=store::query(conn,"SELECT job_id FROM artifact_jobs WHERE root=?1 AND name=?2 AND job_id<>?3 AND state IN ('queued','waiting','running','holding')",&[&root,&name,&except])?;
    if let Some(job) = jobs.first() {
        bail!("{name} is in use by job {}", job["job_id"]);
    }
    let signals = store::query(
        conn,
        "SELECT evidence FROM artifact_signals WHERE root=?1 AND name=?2 AND kind='in_use'",
        &[&root, &name],
    )?;
    if let Some(signal) = signals.first() {
        bail!("{name} is in use: {}", signal["evidence"]);
    }
    // Reservations protect inputs as well as outputs before the signal evaluator runs.
    let reservations = store::query(
        conn,
        "SELECT job_id,inputs_json,outputs_json FROM artifact_job_reservations WHERE root=?1",
        &[&root],
    )?;
    for r in reservations {
        if r["job_id"] == except {
            continue;
        }
        for field in ["inputs_json", "outputs_json"] {
            let names: Vec<String> = serde_json::from_str(r[field].as_str().unwrap_or("[]"))?;
            if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                bail!("{name} is in use by job {}", r["job_id"]);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
