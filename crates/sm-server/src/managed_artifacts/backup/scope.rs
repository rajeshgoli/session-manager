use super::super::{archive, config::ArtifactBackupConfig, scan, score, store, ManagedArtifacts};
use super::*;
use crate::sessions::expand_home;
use anyhow::{ensure, Context};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rusqlite::{params, Connection};
use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

/// Every configured exclusion, home-expanded. An invalid glob is a config error.
pub fn exclude_set(config: &ArtifactBackupConfig) -> Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for pattern in &config.exclude {
        let expanded = expand_home(pattern);
        let expanded = expanded.to_str().context("backup exclude is not UTF-8")?;
        set.add(
            GlobBuilder::new(expanded)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid backup exclude: {pattern}"))?,
        );
    }
    Ok(set.build()?)
}

/// Exclusions for one static scope, matched at its configured identity path
/// (`~/data`, not the canonical source a symlink resolves to). A matching file
/// or directory is absent from the backup, with everything beneath it.
#[derive(Clone, Debug, Default)]
pub struct Exclude(Option<(PathBuf, Arc<GlobSet>)>);
impl Exclude {
    pub fn for_static(config: &ArtifactBackupConfig, identity: &Path) -> Result<Self> {
        let set = exclude_set(config)?;
        Ok(Self(
            (!set.is_empty()).then(|| (identity.to_path_buf(), Arc::new(set))),
        ))
    }
    pub fn skips(&self, relative: &str) -> bool {
        self.0
            .as_ref()
            .is_some_and(|(identity, set)| set.is_match(identity.join(relative)))
    }
}

#[derive(Clone, Debug)]
pub struct Scope {
    pub source: PathBuf,
    pub segment: String,
    pub plain: bool,
    pub holding_days: u64,
    pub estimated_bytes: u64,
    pub exclude: Exclude,
}
impl Scope {
    /// A loose file at an artifact root's top level is a one-file scope whose
    /// only entry is named after itself (`packed/<name>/<name>.zst`), keeping
    /// the remote layout of `packed/<segment>/<path>`.
    pub fn inventory(&self) -> Result<Vec<archive::Entry>> {
        let m = fs::symlink_metadata(&self.source)?;
        if m.is_file() {
            use std::os::unix::fs::MetadataExt;
            return Ok(vec![archive::Entry {
                path: self.segment.clone(),
                kind: "file".into(),
                size: m.len(),
                mtime: m.mtime(),
                mtime_nsec: m.mtime_nsec(),
                mode: m.mode() & 0o7777,
                target: None,
            }]);
        }
        archive::inventory_excluding(&self.source, |p| self.exclude.skips(p))
    }
    /// The local file behind an inventory entry.
    pub fn path_of(&self, relative: &str) -> PathBuf {
        if self.source.is_file() {
            self.source.clone()
        } else {
            self.source.join(relative)
        }
    }
    pub fn measure(&self) -> Result<scan::Size> {
        scan::measure_excluding(&self.source, |p| self.exclude.skips(p))
    }
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
    exclude: Exclude,
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
        exclude,
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
        let mut static_scope = scope(
            config,
            source,
            &identity,
            0,
            Exclude::for_static(config, &identity)?,
        )?;
        let measured = static_scope.measure()?;
        conn.execute("INSERT INTO backup_static(path,content_stamp) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET content_stamp=excluded.content_stamp",params![identity.to_string_lossy(),measured.stamp()])?;
        static_scope.estimated_bytes = measured.data_bytes;
        scopes.push(static_scope);
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
            ensure!(
                source.is_dir() || source.is_file(),
                "backup artifact must be a directory or file"
            );
            let bytes = if a["state"] == "frozen" {
                scan::measure(&source)?.data_bytes
            } else {
                // A loose file is unrecorded, so always in scope; its estimate stays raw.
                let ratio = if source.is_file() {
                    1.0
                } else {
                    score::ratio(manager, root, &a)?
                };
                (scan::measure(&source)?.data_bytes as f64 / ratio).ceil() as u64
            };
            if included(&conn, &a, bytes, rate)? {
                scopes.push(scope(config, source, &identity, bytes, Exclude::default())?);
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
    let seconds: f64 = conn.query_row("SELECT COALESCE(SUM(seconds),0) FROM (SELECT j.job_id,MAX(j.wall_seconds) seconds FROM artifact_jobs j JOIN artifacts a ON a.root=j.root AND a.name=j.name WHERE a.root=?1 AND a.set_id=?2 AND j.state='succeeded' GROUP BY j.job_id)", params![a["root"].as_str(),a["set_id"].as_str()],|r| r.get(0))?;
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
            scope.source.is_absolute() && (scope.source.is_dir() || scope.source.is_file()),
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
