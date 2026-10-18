use super::*;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;

const OLD: &[u8] = b"#!/bin/sh\necho 'sm 0.1.0 (old)'\n";

struct Mini {
    dir: PathBuf,
    binary: PathBuf,
    dest: Destination,
}
impl Mini {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sm-backup-update-{}-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("bin")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let binary = dir.join("bin/sm");
        fs::write(&binary, OLD).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let dest = Destination::open(&dir.join("disk")).unwrap();
        Self { dir, binary, dest }
    }
    fn gate(&self) -> Gate {
        Gate {
            api_url: "https://sm-app.example.com".into(),
            identity: self.dir.join("identity"),
            timeout: Duration::from_secs(10),
        }
    }
    fn update(&self, source: &mut Source) -> Outcome {
        update(
            &self.binary,
            "sm 0.1.0 (old)",
            &self.dest,
            source,
            &self.gate(),
        )
        .unwrap()
    }
    fn report(&self) -> Option<UpdateReport> {
        self.dest
            .meta(PENDING_UPDATE)
            .unwrap()
            .map(|s| serde_json::from_str(&s).unwrap())
    }
    /// The old binary is still at its path and still runs.
    fn assert_old_runs(&self) {
        assert_eq!(fs::read(&self.binary).unwrap(), OLD);
        let out = Command::new(&self.binary).output().unwrap();
        assert_eq!(out.stdout, b"sm 0.1.0 (old)\n");
        assert!(!sibling(&self.binary, ".new").exists());
    }
}
impl Drop for Mini {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A candidate that prints `printed` for --version, records its self-test
/// arguments next to itself, and exits `self_test` for the self-test.
fn candidate(printed: &str, self_test: i32, args_file: &Path) -> Vec<u8> {
    format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{printed}'; exit 0; fi\n\
         echo \"$@\" > '{}'\nexit {self_test}\n",
        args_file.display()
    )
    .into_bytes()
}

struct Source {
    info: Option<CliInfo>,
    bytes: Vec<u8>,
    interrupt: bool,
    offsets: Vec<u64>,
}
impl Source {
    fn new(version: &str, bytes: Vec<u8>) -> Self {
        Self {
            info: Some(CliInfo {
                version: version.into(),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                bytes: bytes.len() as u64,
            }),
            bytes,
            interrupt: false,
            offsets: vec![],
        }
    }
}
impl CliSource for Source {
    fn cli(&mut self) -> Result<Option<CliInfo>> {
        Ok(self.info.clone())
    }
    fn cli_download(&mut self, _: &CliInfo, offset: u64, output: &mut dyn Write) -> Result<()> {
        self.offsets.push(offset);
        let rest = &self.bytes[offset as usize..];
        if self.interrupt {
            self.interrupt = false;
            output.write_all(&rest[..rest.len() / 2])?;
            bail!("connection lost");
        }
        output.write_all(rest)?;
        Ok(())
    }
}

#[test]
fn installs_a_verified_candidate_resuming_the_download_and_keeps_the_previous_binary() {
    let b = Mini::new();
    let args = b.dir.join("args");
    let new = candidate("sm 0.2.0 (new)", 0, &args);
    let mut source = Source::new("sm 0.2.0 (new)", new.clone());
    source.interrupt = true;
    let report = UpdateReport {
        from: "sm 0.1.0 (old)".into(),
        to: "sm 0.2.0 (new)".into(),
        ok: true,
        error: None,
    };
    assert_eq!(b.update(&mut source), Outcome::Installed(report.clone()));
    assert_eq!(source.offsets, vec![0, new.len() as u64 / 2]);
    assert_eq!(fs::read(&b.binary).unwrap(), new);
    assert_eq!(
        fs::metadata(&b.binary).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(fs::read(sibling(&b.binary, ".prev")).unwrap(), OLD);
    assert!(!sibling(&b.binary, ".new").exists());
    assert_eq!(
        fs::read_to_string(&args).unwrap().trim(),
        format!(
            "backup self-test --api-url https://sm-app.example.com --identity {}",
            b.dir.join("identity").display()
        )
    );
    assert_eq!(b.report(), Some(report));
}

#[test]
fn nothing_published_or_already_current_leaves_everything_alone() {
    let b = Mini::new();
    let mut source = Source::new("sm 0.1.0 (old)", b"unused".to_vec());
    assert_eq!(b.update(&mut source), Outcome::Unchanged);
    source.info = None;
    assert_eq!(b.update(&mut source), Outcome::Unchanged);
    assert!(source.offsets.is_empty());
    assert_eq!(b.report(), None);
    b.assert_old_runs();
}

fn assert_rejected(b: &Mini, source: &mut Source, error: &str) {
    let Outcome::Failed(report) = b.update(source) else {
        panic!("candidate was not rejected");
    };
    assert!(!report.ok);
    assert!(
        report.error.as_deref().unwrap().contains(error),
        "{report:?}"
    );
    assert_eq!(b.report(), Some(report));
    b.assert_old_runs();
    assert!(!sibling(&b.binary, ".prev").exists());
}

#[test]
fn corrupt_download_wrong_version_failing_and_hanging_self_test_are_rejected() {
    let b = Mini::new();
    let args = b.dir.join("args");
    let mut corrupt = Source::new("sm 0.2.0 (new)", candidate("sm 0.2.0 (new)", 0, &args));
    corrupt.bytes[3] ^= 1;
    assert_rejected(&b, &mut corrupt, "SHA-256");

    let mut wrong = Source::new("sm 0.2.0 (new)", candidate("sm 0.3.0 (other)", 0, &args));
    assert_rejected(&b, &mut wrong, "reports version \"sm 0.3.0 (other)\"");

    let mut failing = Source::new("sm 0.2.0 (new)", candidate("sm 0.2.0 (new)", 3, &args));
    assert_rejected(&b, &mut failing, "self-test");

    let hang =
        b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'sm 0.2.0 (new)'; exit 0; fi\nsleep 30\n";
    let mut hanging = Source::new("sm 0.2.0 (new)", hang.to_vec());
    let gate = Gate {
        timeout: Duration::from_secs(1),
        ..b.gate()
    };
    let started = Instant::now();
    let outcome = update(&b.binary, "sm 0.1.0 (old)", &b.dest, &mut hanging, &gate).unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        matches!(outcome, Outcome::Failed(r) if r.error.as_deref().unwrap_or("").contains("did not finish"))
    );
    b.assert_old_runs();

    // A failed version is retried on the next night.
    let mut fixed = Source::new("sm 0.2.0 (new)", candidate("sm 0.2.0 (new)", 0, &args));
    assert!(matches!(b.update(&mut fixed), Outcome::Installed(_)));
}

#[test]
fn a_crash_between_the_renames_leaves_the_old_binary_and_the_next_night_recovers() {
    let b = Mini::new();
    let args = b.dir.join("args");
    let new = candidate("sm 0.2.0 (new)", 0, &args);
    FAIL_BETWEEN_RENAMES.set(true);
    let mut source = Source::new("sm 0.2.0 (new)", new.clone());
    let outcome = b.update(&mut source);
    FAIL_BETWEEN_RENAMES.set(false);
    assert!(
        matches!(outcome, Outcome::Failed(r) if r.error.as_deref().unwrap_or("").contains("between the two renames"))
    );
    b.assert_old_runs();
    assert_eq!(fs::read(sibling(&b.binary, ".prev")).unwrap(), OLD);

    // A real crash leaves the leftovers of every step; none blocks the next night.
    fs::write(sibling(&b.binary, ".new"), b"#!/bin/sh\nexit 9\ntruncated").unwrap();
    fs::write(sibling(&b.binary, ".prev.tmp"), b"partial copy").unwrap();
    let mut source = Source::new("sm 0.2.0 (new)", new.clone());
    assert!(
        matches!(b.update(&mut source), Outcome::Failed(r) if r.error.as_deref().unwrap_or("").contains("SHA-256"))
    );
    b.assert_old_runs();
    assert!(matches!(b.update(&mut source), Outcome::Installed(_)));
    assert_eq!(fs::read(&b.binary).unwrap(), new);
    assert_eq!(fs::read(sibling(&b.binary, ".prev")).unwrap(), OLD);
}

#[test]
fn rollback_restores_the_previous_binary_and_skips_its_replacement_until_another_is_advertised() {
    let b = Mini::new();
    let args = b.dir.join("args");
    b.dest.confirm("manifest-1").unwrap();
    assert!(rollback(&b.binary, "sm 0.1.0 (old)", &b.dest).is_err());
    let new = candidate("sm 0.2.0 (new)", 0, &args);
    let mut source = Source::new("sm 0.2.0 (new)", new);
    assert!(matches!(b.update(&mut source), Outcome::Installed(_)));

    // Run on site by the new binary.
    rollback(&b.binary, "sm 0.2.0 (new)", &b.dest).unwrap();
    assert_eq!(fs::read(&b.binary).unwrap(), OLD);
    assert!(!sibling(&b.binary, ".prev").exists());
    let state = b.dest.state().unwrap();
    assert_eq!(state.skip_versions, vec!["sm 0.2.0 (new)".to_string()]);
    // Confirming a backup keeps the skip list.
    b.dest.confirm("manifest-2").unwrap();
    assert_eq!(
        b.dest.state().unwrap().skip_versions,
        vec!["sm 0.2.0 (new)".to_string()]
    );
    assert_eq!(
        b.dest.state().unwrap().confirmed.as_deref(),
        Some("manifest-2")
    );

    source.offsets.clear();
    assert_eq!(b.update(&mut source), Outcome::Unchanged);
    assert!(source.offsets.is_empty());
    b.assert_old_runs();

    let mut next = Source::new("sm 0.3.0 (next)", candidate("sm 0.3.0 (next)", 0, &args));
    assert!(matches!(b.update(&mut next), Outcome::Installed(_)));
    assert!(b.dest.state().unwrap().skip_versions.is_empty());
}

#[test]
fn published_reads_the_installed_description() {
    let b = Mini::new();
    assert_eq!(published(&b.dir).unwrap(), None);
    fs::write(
        b.dir.join(CLI_JSON),
        r#"{"version":"sm 0.2.0 (new)","sha256":"ab","bytes":2}"#,
    )
    .unwrap();
    assert_eq!(
        published(&b.dir).unwrap(),
        Some(CliInfo {
            version: "sm 0.2.0 (new)".into(),
            sha256: "ab".into(),
            bytes: 2
        })
    );
    assert!(CLI_VERSION.starts_with("sm ") && CLI_VERSION.ends_with(')'));
}
