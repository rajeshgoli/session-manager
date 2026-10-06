//! Bounded metadata-only connection log with host-controlled rotation.
use super::ConnectionLog;
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const LIMIT: u64 = 8 * 1024 * 1024;
const ARCHIVES: usize = 4;
pub(super) struct ConnectionLogger {
    directory: PathBuf,
    file: File,
    size: u64,
    limit: u64,
}
impl ConnectionLogger {
    pub fn open(directory: &Path) -> io::Result<Self> {
        let file = open(&directory.join("connections.jsonl"))?;
        let size = file.metadata()?.len();
        Ok(Self {
            directory: directory.into(),
            file,
            size,
            limit: LIMIT,
        })
    }
    fn path(&self, archive: usize) -> PathBuf {
        self.directory.join(if archive == 0 {
            "connections.jsonl".into()
        } else {
            format!("connections.jsonl.{archive}")
        })
    }
    fn rotate(&mut self) -> io::Result<()> {
        // Rename from oldest to newest while holding the writer mutex. No
        // writer can append to an archive after the new current file opens.
        for archive in (1..=ARCHIVES).rev() {
            match std::fs::rename(self.path(archive - 1), self.path(archive)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        self.file = open(&self.path(0))?;
        self.size = self.file.metadata()?.len();
        Ok(())
    }
    pub fn write(&mut self, record: &ConnectionLog) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(record)?;
        bytes.push(b'\n');
        if self.size > 0 && self.size.saturating_add(bytes.len() as u64) > self.limit {
            self.rotate()?;
        }
        self.file.write_all(&bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }
}
fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_retains_bounded_complete_lines_and_resumes_after_restart() {
        let dir = super::super::tests::directory();
        let mut logger = ConnectionLogger::open(&dir).unwrap();
        logger.limit = 512;
        let mut record = ConnectionLog {
            time: "2026-10-06T00:00:00Z".into(),
            agent_id: "agent".into(),
            host: Some("docs.rs".into()),
            resolved_address: None,
            port: Some(443),
            bytes_to_host: 0,
            bytes_to_agent: 0,
            duration_ms: 0,
            outcome: "allowed".into(),
            established: true,
        };
        for n in 0..20 {
            record.bytes_to_host = n;
            logger.write(&record).unwrap();
        }
        drop(logger);
        let mut logger = ConnectionLogger::open(&dir).unwrap();
        logger.limit = 512;
        assert!(logger.size > 0);
        record.bytes_to_host = 20;
        logger.write(&record).unwrap();
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), ARCHIVES + 1);
        for path in files {
            let bytes = std::fs::read(&path).unwrap();
            assert!(bytes.len() <= 512);
            assert!(bytes.ends_with(b"\n"));
            for line in String::from_utf8(bytes).unwrap().lines() {
                serde_json::from_str::<serde_json::Value>(line).unwrap();
            }
        }
        let latest: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(logger.path(0))
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(latest["bytes_to_host"], 20);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
