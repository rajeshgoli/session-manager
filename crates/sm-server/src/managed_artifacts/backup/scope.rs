use super::super::{config::ArtifactBackupConfig, scan, score, store, ManagedArtifacts};
use super::*;
use crate::sessions::expand_home;
use anyhow::{ensure, Context};
use rusqlite::{params, Connection};
use std::{collections::BTreeSet, fs, path::PathBuf};

#[derive(Clone, Debug)]
pub struct Scope {
    pub source: PathBuf,
    pub segment: String,
    pub plain: bool,
    pub holding_days: u64,
    pub estimated_bytes: u64,
}
impl Scope {
    pub fn prefix(&self) -> String {
        format!(
            "{}/{}",
            if self.plain { "plain" } else { "packed" },
            self.segment
        )
    }
}
fn scope(
    config: &ArtifactBackupConfig,
    source: PathBuf,
    identity: &Path,
    estimate: u64,
) -> Result<Scope> {
    let segment = identity
        .file_name()
        .and_then(|s| s.to_str())
        .context("backup scope needs a UTF-8 final segment")?
        .to_owned();
    safe_relative(&segment)?;
    let plain = config
        .plain_paths
        .iter()
        .any(|p| expand_home(p) == identity);
    let holding_days = config
        .holding_days
        .iter()
        .find(|(p, _)| p.as_str() != "default" && expand_home(p) == identity)
        .map(|(_, d)| *d)
        .unwrap_or_else(|| *config.holding_days.get("default").unwrap_or(&30));
    Ok(Scope {
        source,
        segment,
        plain,
        holding_days,
        estimated_bytes: estimate,
    })
}

/// Refresh estimates every preparation; a rate change can change scope without a
/// content change. An unavailable source is an error, never an inferred deletion.
pub fn collect_scope(manager: &ManagedArtifacts, device: &str) -> Result<Vec<Scope>> {
    let config = &manager.config.backup;
    let conn = manager.store.open()?;
    let rate = rates(&conn, device, parse_rate(&config.backup_rate_default)?)?.effective;
    let mut scopes = Vec::new();
    for path in &config.static_paths {
        let identity = expand_home(path);
        let source = fs::canonicalize(&identity)
            .with_context(|| format!("backup source unavailable: {}", identity.display()))?;
        ensure!(source.is_dir(), "backup static source must be a directory");
        let measured = scan::measure(&source)?;
        conn.execute("INSERT INTO backup_static(path,content_stamp) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET content_stamp=excluded.content_stamp",params![identity.to_string_lossy(),measured.stamp()])?;
        scopes.push(scope(config, source, &identity, measured.data_bytes)?);
    }
    for root in &manager.config.roots {
        for a in manager.store.list(&root.path)? {
            if !matches!(a["state"].as_str(), Some("live" | "frozen" | "unrecorded")) {
                continue;
            }
            // A lifecycle operation can temporarily hide a source. Fail this
            // preparation so a transient move cannot become a remote deletion.
            ensure!(
                a["op"].is_null(),
                "artifact operation in progress; retry backup"
            );
            let name = a["name"].as_str().context("artifact name")?;
            safe_relative(name)?;
            ensure!(!name.contains('/'), "artifact name is not a segment");
            let identity = Path::new(&root.path).join(name);
            let source = if a["state"] == "frozen" {
                Path::new(&root.cold_store).join(".frozen").join(name)
            } else {
                identity.clone()
            };
            let source = fs::canonicalize(source)
                .with_context(|| format!("backup artifact unavailable: {name}"))?;
            ensure!(source.is_dir(), "backup artifact must be a directory");
            let bytes = if a["state"] == "frozen" {
                scan::measure(&source)?.data_bytes
            } else {
                let ratio = score::ratio(manager, root, &a)?;
                (scan::measure(&source)?.data_bytes as f64 / ratio).ceil() as u64
            };
            if included(&conn, &a, bytes, rate)? {
                scopes.push(scope(config, source, &identity, bytes)?);
            }
        }
    }
    validate_scopes(&scopes)?;
    scopes.sort_by_key(Scope::prefix);
    Ok(scopes)
}
fn included(conn: &Connection, a: &serde_json::Value, bytes: u64, rate: f64) -> Result<bool> {
    if a["state"] == "unrecorded" || a["recipe_complete"] != 1 || store::has_gpu(a) {
        return Ok(true);
    }
    let seconds: f64 = conn.query_row("SELECT COALESCE(SUM(wall_seconds),0) FROM artifact_jobs WHERE root=?1 AND name=?2 AND state='succeeded'", params![a["root"].as_str(),a["name"].as_str()],|r| r.get(0))?;
    Ok(seconds > bytes as f64 / rate)
}
pub(super) fn validate_scopes(scopes: &[Scope]) -> Result<()> {
    let mut segments = BTreeSet::new();
    for scope in scopes {
        safe_relative(&scope.segment)?;
        ensure!(
            !scope.segment.contains('/') && segments.insert(scope.segment.to_lowercase()),
            "ambiguous backup scope name: {}",
            scope.segment
        );
        ensure!(
            scope.source.is_absolute() && scope.source.is_dir(),
            "backup source unavailable"
        );
    }
    for (i, a) in scopes.iter().enumerate() {
        for b in &scopes[i + 1..] {
            ensure!(
                !a.source.starts_with(&b.source) && !b.source.starts_with(&a.source),
                "overlapping backup sources"
            );
        }
    }
    Ok(())
}
