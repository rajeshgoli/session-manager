//! Configuration for managed research artifacts (distinct from app downloads).
use crate::sessions::expand_home;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedArtifactsConfig {
    pub enabled: bool,
    pub deletion_mode: String,
    pub db_path: String,
    pub mirror_dir: String,
    pub scan_seconds: u64,
    pub steward_role: String,
    pub note_max_chars: usize,
    pub investigate_model: String,
    pub nightly_time: String,
    pub cold_after_days: i64,
    pub unrecorded_grace_days: i64,
    pub initial_grace_days: i64,
    pub orphan_building_days: i64,
    pub keep_live_days: i64,
    pub enough_window_days: i64,
    pub floor_percent: u64,
    pub disk_hold_max_wait_hours: u64,
    pub overrun_factor: f64,
    pub pressure_check_seconds: u64,
    pub freeze_level: i32,
    pub freeze_part_bytes: u64,
    pub freeze_parallel_parts: usize,
    pub thaw_parallel_parts: usize,
    pub max_concurrent_ops: usize,
    pub roots: Vec<ArtifactRoot>,
    pub backup: ArtifactBackupConfig,
}
impl Default for ManagedArtifactsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            deletion_mode: "report_only".into(),
            db_path: "~/.local/share/claude-sessions/artifacts.db".into(),
            mirror_dir: "~/.local/share/claude-sessions/artifact-mirrors".into(),
            scan_seconds: 60,
            steward_role: "artifact-steward".into(),
            note_max_chars: 140,
            investigate_model: "low".into(),
            nightly_time: "04:17".into(),
            cold_after_days: 14,
            unrecorded_grace_days: 7,
            initial_grace_days: 14,
            orphan_building_days: 7,
            keep_live_days: 30,
            enough_window_days: 30,
            floor_percent: 5,
            disk_hold_max_wait_hours: 24,
            overrun_factor: 1.5,
            pressure_check_seconds: 600,
            freeze_level: 9,
            freeze_part_bytes: 17_179_869_184,
            freeze_parallel_parts: 4,
            thaw_parallel_parts: 8,
            max_concurrent_ops: 2,
            roots: Vec::new(),
            backup: ArtifactBackupConfig::default(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactRoot {
    pub path: String,
    pub repo: String,
    pub repo_checkout: String,
    pub default_branch: String,
    pub named_exclude: Vec<String>,
    pub cold_store: String,
}
impl Default for ArtifactRoot {
    fn default() -> Self {
        Self {
            path: String::new(),
            repo: String::new(),
            repo_checkout: String::new(),
            default_branch: "main".into(),
            named_exclude: vec!["docs/working/".into(), "docs/execution/".into()],
            cold_store: String::new(),
        }
    }
}
impl ManagedArtifactsConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !matches!(self.deletion_mode.as_str(), "report_only" | "enforcing") {
            bail!("invalid managed_artifacts.deletion_mode");
        }
        let (hour, minute) = self
            .nightly_time
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("nightly_time must be HH:MM"))?;
        if hour.len() != 2
            || minute.len() != 2
            || hour.parse::<u8>().unwrap_or(255) > 23
            || minute.parse::<u8>().unwrap_or(255) > 59
        {
            bail!("nightly_time must be HH:MM");
        }
        if !(1..=50).contains(&self.floor_percent)
            || !(1..=19).contains(&self.freeze_level)
            || self.scan_seconds == 0
            || self.note_max_chars == 0
            || self.freeze_part_bytes == 0
            || self.freeze_parallel_parts == 0
            || self.thaw_parallel_parts == 0
            || self.max_concurrent_ops == 0
        {
            bail!("invalid managed_artifacts limits");
        }
        if self.backup.enabled {
            super::backup::parse_rate(&self.backup.backup_rate_default)?;
            if self.backup.outbox_part_bytes == 0
                || self.backup.ready_ttl_hours == 0
                || self.backup.ready_ttl_hours > 87600
                || self.backup.alert_after_missed_nights == 0
                || self.backup.alert_after_missed_nights > 3650
                || !self.backup.max_delete_fraction.is_finite()
                || !(0.0..=1.0).contains(&self.backup.max_delete_fraction)
                || self.backup.puller_devices.is_empty()
                || self.roots.is_empty()
            {
                bail!("invalid managed_artifacts backup limits or cold store");
            }
        }
        let mut paths = std::collections::BTreeSet::new();
        for root in &self.roots {
            let path = fs::canonicalize(expand_home(&root.path))?;
            if !path.is_dir() || !paths.insert(path.clone()) {
                bail!("artifact root must be a unique directory: {}", root.path);
            }
            crate::owner_docs::validate_repo_slug(&root.repo)?;
            let cold = expand_home(&root.cold_store);
            if root.cold_store.is_empty() || !cold.is_absolute() {
                bail!("cold_store must be an absolute path");
            }
            if cold.exists() {
                if !cold.is_dir() || fs::metadata(&cold)?.dev() == fs::metadata(&path)?.dev() {
                    bail!(
                        "cold_store must be on a different filesystem: {}",
                        cold.display()
                    );
                }
                #[cfg(target_os = "macos")]
                if nix::sys::statfs::statfs(&cold)?.filesystem_type_name() != "apfs" {
                    bail!("cold_store must be APFS");
                }
            }
        }
        Ok(())
    }
    /// Canonicalize once; filesystem operations use these stable parent paths.
    pub fn resolved(&self) -> Result<Self> {
        let mut config = self.clone();
        for root in &mut config.roots {
            root.path = fs::canonicalize(expand_home(&root.path))?
                .to_string_lossy()
                .into_owned();
            let cold = expand_home(&root.cold_store);
            root.cold_store = fs::canonicalize(&cold)
                .unwrap_or(cold)
                .to_string_lossy()
                .into_owned();
        }
        Ok(config)
    }
    pub fn resolve_root(
        &self,
        explicit: Option<&str>,
        repo: Option<&str>,
    ) -> Result<&ArtifactRoot> {
        if let Some(path) = explicit {
            let path = fs::canonicalize(expand_home(path))?;
            return self
                .roots
                .iter()
                .find(|r| Path::new(&r.path) == path)
                .ok_or_else(|| anyhow::anyhow!("unknown artifact root {}", path.display()));
        }
        let matches: Vec<_> = self
            .roots
            .iter()
            .filter(|r| Some(r.repo.as_str()) == repo)
            .collect();
        if matches.len() == 1 {
            return Ok(matches[0]);
        }
        if self.roots.len() == 1 {
            return Ok(&self.roots[0]);
        }
        bail!(
            "specify --root; configured roots: {}",
            self.roots
                .iter()
                .map(|r| r.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Backup configuration is persisted now; ticket #2162 implements transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactBackupConfig {
    pub enabled: bool,
    pub static_paths: Vec<String>,
    pub puller_devices: Vec<String>,
    pub outbox: String,
    pub outbox_part_bytes: u64,
    pub ready_ttl_hours: u64,
    pub alert_after_missed_nights: u64,
    pub max_delete_fraction: f64,
    pub plain_paths: Vec<String>,
    pub holding_days: std::collections::BTreeMap<String, u64>,
    pub backup_rate_default: String,
}
impl Default for ArtifactBackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            static_paths: vec!["~/data".into(), "~/projects".into(), "~/.config".into()],
            puller_devices: vec!["backup-mini".into()],
            outbox: "/Volumes/ColdSSD/.backup-outbox".into(),
            outbox_part_bytes: 4_294_967_296,
            ready_ttl_hours: 48,
            alert_after_missed_nights: 2,
            max_delete_fraction: 0.1,
            plain_paths: vec!["~/data".into()],
            holding_days: std::collections::BTreeMap::from([
                ("default".into(), 30),
                ("~/projects".into(), 60),
                ("~/.config".into(), 300),
            ]),
            backup_rate_default: "18.75MB".into(),
        }
    }
}
