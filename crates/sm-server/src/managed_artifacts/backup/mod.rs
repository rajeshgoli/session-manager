//! Backup foundation. The request coordinator owns scheduling and authentication;
//! these primitives never contact a remote device or delete source data.
pub mod cli_build;
pub mod client;
pub mod disk;
pub mod lifecycle;
mod outbox;
pub mod pull;
mod scope;
pub mod seed;
mod store;
pub mod update;
pub use outbox::{prepare, Prepared, TransferManifest};
pub use scope::{collect_scope, exclude_set, Exclude, Scope};
pub use store::{confirm, confirmed, load_manifest, rates, record_rate, save_manifest, Rates};

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Component, Path},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRecord {
    /// Destination-relative path, including plain/ or packed/ and the scope name.
    pub path: String,
    pub size: u64,
    pub mtime_ns: i128,
    pub mode: u32,
    pub sha256: Option<String>,
    /// Symlinks are manifest-only and never followed or put in transit archives.
    pub target: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub manifest_id: String,
    pub base: Option<String>,
    pub files: BTreeMap<String, FileRecord>,
    pub holding_days: BTreeMap<String, u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Part {
    pub n: usize,
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
}

/// The inner name of a one-file scope: a loose file `<name>` at an artifact
/// root is stored as `packed/<name>/.sm-file.zst` and restored as `<name>`.
/// No scope may have a top-level entry of this name.
pub const FILE_ENTRY: &str = ".sm-file";

pub fn safe_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains('\0')
            && path
                .split('/')
                .all(|s| !s.is_empty() && s != "." && s != "..")
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "unsafe backup path: {path}"
    );
    Ok(())
}

/// Decimal bytes/second, matching the documented 18.75MB (150 Mbit/s).
pub fn parse_rate(value: &str) -> Result<f64> {
    let value = value.trim();
    let (number, multiplier) = [("GB", 1e9), ("MB", 1e6), ("KB", 1e3), ("B", 1.0)]
        .into_iter()
        .find_map(|(unit, m)| value.strip_suffix(unit).map(|v| (v, m)))
        .unwrap_or((value, 1.0));
    let rate = number.trim().parse::<f64>()? * multiplier;
    ensure!(
        rate.is_finite() && rate > 0.0,
        "backup rate must be finite and positive"
    );
    Ok(rate)
}

#[cfg(test)]
mod tests;
