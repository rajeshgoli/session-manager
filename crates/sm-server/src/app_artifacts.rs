use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub const APP_ARTIFACT_MAX_SIZE_BYTES: usize = 100 * 1024 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppArtifactMetadata {
    pub artifact_hash: String,
    pub size_bytes: u64,
    pub uploaded_at: String,
    pub uploaded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredArtifact {
    pub artifact_hash: String,
    pub size_bytes: u64,
}

pub fn valid_app_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    value.len() <= 80
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn valid_artifact_hash(value: &str) -> bool {
    value.len() == 8
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub fn app_dir(root: &Path, app_name: &str) -> PathBuf {
    root.join(app_name)
}

pub fn latest_path(root: &Path, app_name: &str) -> PathBuf {
    app_dir(root, app_name).join("latest.apk")
}

pub fn hashed_path(root: &Path, app_name: &str, artifact_hash: &str) -> PathBuf {
    app_dir(root, app_name).join(format!("{artifact_hash}.apk"))
}

pub fn meta_path(root: &Path, app_name: &str) -> PathBuf {
    app_dir(root, app_name).join("meta.json")
}

pub fn store_artifact(
    root: &Path,
    app_name: &str,
    bytes: &[u8],
    uploaded_by: Option<String>,
    version_code: Option<i64>,
    version_name: Option<String>,
) -> Result<StoredArtifact> {
    if bytes.is_empty() {
        anyhow::bail!("Uploaded artifact is empty");
    }
    if bytes.len() > APP_ARTIFACT_MAX_SIZE_BYTES {
        anyhow::bail!("Artifact exceeds 100 MB limit");
    }
    let app_dir = app_dir(root, app_name);
    fs::create_dir_all(&app_dir)
        .with_context(|| format!("failed to create app artifact dir {}", app_dir.display()))?;

    let digest = Sha256::digest(bytes);
    let artifact_hash = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let latest = latest_path(root, app_name);
    write_atomically(&latest, bytes, ".tmp-artifact-", ".apk")?;
    let hashed = hashed_path(root, app_name, &artifact_hash);
    if !hashed.exists() {
        write_atomically(&hashed, bytes, ".tmp-artifact-copy-", ".apk")?;
    }
    let metadata = AppArtifactMetadata {
        artifact_hash: artifact_hash.clone(),
        size_bytes: bytes.len() as u64,
        uploaded_at: now_rfc3339(),
        uploaded_by,
        version_code,
        version_name,
    };
    write_json_atomically(&meta_path(root, app_name), &metadata)?;
    Ok(StoredArtifact {
        artifact_hash,
        size_bytes: bytes.len() as u64,
    })
}

pub fn read_metadata(root: &Path, app_name: &str) -> Result<AppArtifactMetadata> {
    let path = meta_path(root, app_name);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read artifact metadata {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("failed to parse artifact metadata {}", path.display()))
}

fn write_json_atomically(path: &Path, metadata: &AppArtifactMetadata) -> Result<()> {
    let bytes = serde_json::to_vec(metadata)?;
    write_atomically(path, &bytes, ".tmp-meta-", ".json")
}

fn write_atomically(path: &Path, bytes: &[u8], prefix: &str, suffix: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("artifact path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        "{prefix}{}-{}{suffix}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut handle = fs::File::create(&temp)
            .with_context(|| format!("failed to create {}", temp.display()))?;
        handle.write_all(bytes)?;
        handle.sync_all()?;
    }
    fs::rename(&temp, path)
        .with_context(|| format!("failed to publish artifact {}", path.display()))?;
    Ok(())
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{valid_app_name, valid_artifact_hash};

    #[test]
    fn app_names_match_python_managed_artifact_contract() {
        assert!(valid_app_name("session-manager-android"));
        assert!(valid_app_name("app1"));
        for name in [
            "", ".", "..", "foo.bar", "_bad", "-bad", "Bad", "bad_name", "app ", " app", "app\t",
        ] {
            assert!(!valid_app_name(name), "{name}");
        }
    }

    #[test]
    fn artifact_hashes_match_python_lower_hex_contract() {
        assert!(valid_artifact_hash("deadbeef"));
        for hash in ["DEADBEEF", "deadbee", "deadbeef0", "zzzzzzzz"] {
            assert!(!valid_artifact_hash(hash), "{hash}");
        }
    }
}
