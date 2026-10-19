//! Public device keys only. The database is authoritative; sshd reads an atomic projection.
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use base64::{
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
    Engine,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{plain_terminals::timestamp, usage_db::UsageDbPool};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Key {
    pub fingerprint: String,
    pub device_id: String,
    pub public_key: String,
    pub added_at: String,
    pub revoked_at: Option<String>,
}

/// Reject options, multiple lines and malformed SSH wire encodings, even if base64 is valid.
pub fn parse(public_key: &str) -> Result<(String, String)> {
    ensure!(
        public_key.len() <= 4096 && !public_key.contains(['\n', '\r', '\0']),
        "Expected one Ed25519 public key without options"
    );
    let mut words = public_key.split_whitespace();
    ensure!(
        words.next() == Some("ssh-ed25519"),
        "Only Ed25519 public keys without options are accepted"
    );
    let encoded = words.next().context("Missing public key")?;
    let wire = STANDARD
        .decode(encoded)
        .or_else(|_| STANDARD_NO_PAD.decode(encoded))?;
    ensure!(
        wire.len() == 51
            && wire[..4] == 11_u32.to_be_bytes()
            && &wire[4..15] == b"ssh-ed25519"
            && wire[15..19] == 32_u32.to_be_bytes(),
        "Invalid Ed25519 public key"
    );
    let comment = words.collect::<Vec<_>>().join(" ");
    ensure!(
        !comment.chars().any(char::is_control),
        "Invalid key comment"
    );
    let canonical = format!(
        "ssh-ed25519 {}{}",
        STANDARD.encode(&wire),
        if comment.is_empty() {
            String::new()
        } else {
            format!(" {comment}")
        }
    );
    Ok((
        format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&wire))),
        canonical,
    ))
}

#[derive(Clone)]
pub struct Store(UsageDbPool);
impl Store {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self(UsageDbPool::new(path, Duration::from_secs(5)))
    }
    pub fn ensure_schema(&self) -> Result<()> {
        self.0.get()?.execute_batch(
            "CREATE TABLE IF NOT EXISTS device_ssh_keys (
            fingerprint TEXT PRIMARY KEY, device_id TEXT NOT NULL, public_key TEXT NOT NULL,
            added_at TEXT NOT NULL, revoked_at TEXT);
            CREATE INDEX IF NOT EXISTS device_ssh_keys_device ON device_ssh_keys(device_id);",
        )?;
        Ok(())
    }
    pub fn list(&self) -> Result<Vec<Key>> {
        let conn = self.0.get()?;
        let mut stmt = conn.prepare("SELECT fingerprint,device_id,public_key,added_at,revoked_at FROM device_ssh_keys ORDER BY added_at,fingerprint")?;
        let keys = stmt
            .query_map([], |r| {
                Ok(Key {
                    fingerprint: r.get(0)?,
                    device_id: r.get(1)?,
                    public_key: r.get(2)?,
                    added_at: r.get(3)?,
                    revoked_at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(keys)
    }
    /// Callers serialize this with projection and connection teardown.
    pub fn add(&self, device: &str, public_key: &str) -> Result<Key> {
        ensure!(
            !device.trim().is_empty()
                && device.len() <= 200
                && !device.chars().any(char::is_control),
            "Invalid device name"
        );
        let (fingerprint, public_key) = parse(public_key)?;
        let conn = self.0.get()?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT device_id FROM device_ssh_keys WHERE fingerprint=?1",
                [&fingerprint],
                |r| r.get(0),
            )
            .optional()?;
        if existing.as_deref().is_some_and(|owner| owner != device) {
            bail!("Key already belongs to another device");
        }
        let added_at = timestamp();
        conn.execute("INSERT INTO device_ssh_keys VALUES (?1,?2,?3,?4,NULL) ON CONFLICT(fingerprint) DO UPDATE SET public_key=excluded.public_key,revoked_at=NULL", params![fingerprint,device,public_key,added_at])?;
        self.list()?
            .into_iter()
            .find(|k| k.fingerprint == fingerprint)
            .context("Registered key disappeared")
    }
    pub fn revoke(&self, device: &str, fingerprint: Option<&str>) -> Result<()> {
        self.0.get()?.execute("UPDATE device_ssh_keys SET revoked_at=COALESCE(revoked_at,?3) WHERE device_id=?1 AND (?2 IS NULL OR fingerprint=?2)", params![device,fingerprint,timestamp()])?;
        Ok(())
    }
    pub fn project(&self, path: &Path) -> Result<()> {
        let mut text = String::new();
        for key in self.list()?.into_iter().filter(|k| k.revoked_at.is_none()) {
            text.push_str(&format!("restrict,pty {}\n", key.public_key));
        }
        atomic_private_file(path, text.as_bytes())
    }
}

pub(crate) fn atomic_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o600)
        && fs::read(path).is_ok_and(|current| current == bytes)
    {
        return Ok(());
    }
    let parent = path.parent().context("File has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".sm-{}-{}.tmp",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ed25519_wire_validation_and_canonical_fingerprint() {
        let mut wire = Vec::from(11_u32.to_be_bytes());
        wire.extend(b"ssh-ed25519");
        wire.extend(32_u32.to_be_bytes());
        wire.extend([7; 32]);
        let text = format!("ssh-ed25519 {} laptop", STANDARD.encode(wire));
        let (fp, canonical) = parse(&text).unwrap();
        assert!(fp.starts_with("SHA256:"));
        assert_eq!(text, canonical);
        for invalid in [
            format!("restrict {text}"),
            format!("{text}\n{text}"),
            "ssh-ed25519 YWJj".into(),
            "ssh-rsa AAAA".into(),
        ] {
            assert!(parse(&invalid).is_err(), "{invalid}");
        }
    }
}
