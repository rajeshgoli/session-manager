//! File-based identity for unattended backup clients. Renewal retains the key,
//! so publishing a new certificate is one atomic rename, not a key/cert swap.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) fn openssl(args: &[&str], input: &str) -> Result<String> {
    let mut child = Command::new("openssl")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run openssl")?;
    child
        .stdin
        .take()
        .context("openssl stdin")?
        .write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "openssl {} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}
#[derive(Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
    pub expires_at: String,
}
pub struct Identity {
    root: PathBuf,
    pub name: String,
    _lock: File,
}
fn regular(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "identity path is not a regular file: {}",
        path.display()
    );
    ensure!(
        meta.permissions().mode() & 0o077 == 0,
        "identity file must have mode 0600: {}",
        path.display()
    );
    Ok(())
}
fn atomic(root: &Path, name: &str, value: &str) -> Result<()> {
    let temp = root.join(format!(".{name}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(value.as_bytes())?;
    file.sync_all()?;
    fs::rename(temp, root.join(name))?;
    File::open(root)?.sync_all()?;
    Ok(())
}
impl Identity {
    pub fn open(root: &Path, name: &str, create: bool) -> Result<Self> {
        ensure!(
            crate::mobile_devices::valid_computer_name(name),
            "invalid device name"
        );
        if create && !root.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root)?;
        }
        let meta = fs::symlink_metadata(root)?;
        ensure!(
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && meta.permissions().mode() & 0o077 == 0,
            "identity directory must be a private real directory (0700)"
        );
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join(".lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "device identity is already in use"
        );
        let identity = Self {
            root: root.into(),
            name: name.into(),
            _lock: lock,
        };
        let key = root.join("private-key.pem");
        if !key.exists() {
            ensure!(create, "device private key is missing");
            let value = openssl(
                &[
                    "genpkey",
                    "-algorithm",
                    "RSA",
                    "-pkeyopt",
                    "rsa_keygen_bits:2048",
                ],
                "",
            )?;
            atomic(root, "private-key.pem", &value)?;
        }
        regular(&key)?;
        if root.join("identity.json").exists() {
            let metadata: Metadata =
                serde_json::from_slice(&fs::read(root.join("identity.json"))?)?;
            ensure!(
                metadata.name == name,
                "identity directory belongs to another device"
            );
        }
        Ok(identity)
    }
    pub fn load(root: &Path) -> Result<Self> {
        regular(&root.join("identity.json"))?;
        let metadata: Metadata = serde_json::from_slice(&fs::read(root.join("identity.json"))?)?;
        Self::open(root, &metadata.name, false)
    }
    pub fn csr(&self) -> Result<String> {
        openssl(
            &[
                "req",
                "-new",
                "-sha256",
                "-key",
                self.root
                    .join("private-key.pem")
                    .to_str()
                    .context("identity path is not UTF-8")?,
                "-subj",
                &format!("/CN={}", self.name),
            ],
            "",
        )
    }
    pub fn certificate(&self) -> Result<String> {
        regular(&self.root.join("certificate.pem"))?;
        Ok(fs::read_to_string(self.root.join("certificate.pem"))?)
    }
    pub fn renewal_due(&self) -> Result<bool> {
        Ok(
            certificate_expiry(&self.certificate()?)? - time::OffsetDateTime::now_utc()
                <= time::Duration::days(30),
        )
    }
    pub fn install(&self, chain: &str) -> Result<()> {
        let public = openssl(&["x509", "-pubkey", "-noout"], chain)?;
        let expected = openssl(
            &[
                "pkey",
                "-pubout",
                "-in",
                self.root
                    .join("private-key.pem")
                    .to_str()
                    .context("identity path is not UTF-8")?,
            ],
            "",
        )?;
        ensure!(
            public.trim() == expected.trim(),
            "certificate does not match device private key"
        );
        ensure!(
            certificate_name(chain)? == self.name,
            "certificate names another device"
        );
        let expiry = certificate_expiry(chain)?;
        ensure!(
            expiry > time::OffsetDateTime::now_utc(),
            "certificate is expired"
        );
        atomic(&self.root, "certificate.pem", chain)?;
        atomic(
            &self.root,
            "identity.json",
            &serde_json::to_string(&Metadata {
                name: self.name.clone(),
                expires_at: expiry.format(&time::format_description::well_known::Rfc3339)?,
            })?,
        )
    }
}
pub(crate) fn certificate_name(pem: &str) -> Result<String> {
    let subject = openssl(&["x509", "-subject", "-noout", "-nameopt", "RFC2253"], pem)?;
    Ok(subject
        .trim()
        .strip_prefix("subject=CN=")
        .context("certificate must have one common name")?
        .to_owned())
}
pub(crate) fn certificate_expiry(pem: &str) -> Result<time::OffsetDateTime> {
    let value = openssl(&["x509", "-enddate", "-noout"], pem)?;
    let value = value
        .trim()
        .strip_prefix("notAfter=")
        .context("certificate expiry missing")?;
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let format = time::format_description::parse(
        "[month repr:short] [day padding:none] [hour]:[minute]:[second] [year] GMT",
    )?;
    Ok(time::PrimitiveDateTime::parse(&normalized, &format)?.assume_utc())
}
