//! Shell-driven SSH lifecycle and VERBOSE sshd log accounting.
use crate::{
    config::StudioSshConfig,
    device_ssh::{atomic_private_file, Key},
    sessions::expand_home,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
    process::Command,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Process {
    pub pid: i32,
    pub parent: i32,
    pub birth: String,
    pub command: String,
}
pub fn processes() -> Result<BTreeMap<i32, Process>> {
    let mut cmd = Command::new("/bin/ps");
    cmd.args(["-axo", "pid=,ppid=,lstart=,comm="]);
    let out = crate::child_output::output_with_timeout(cmd, Duration::from_secs(2))
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(out.status.success(), "Cannot read SSH process identities");
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            if words.len() < 8 {
                return None;
            }
            let pid = words[0].parse().ok()?;
            Some((
                pid,
                Process {
                    pid,
                    parent: words[1].parse().ok()?,
                    birth: words[2..7].join(" "),
                    command: words[7..].join(" "),
                },
            ))
        })
        .collect())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub pid: i32,
    pub birth: String,
    pub fingerprint: String,
    pub away: bool,
}
impl Connection {
    pub fn alive(&self, ps: &BTreeMap<i32, Process>) -> bool {
        ps.get(&self.pid)
            .is_some_and(|p| p.birth == self.birth && p.command.contains("sshd"))
    }
}

/// Kill only a still-matching sshd login and its descendants. tmux's existing server survives.
pub fn terminate(connection: &Connection) -> Result<()> {
    let ps = processes()?;
    if !connection.alive(&ps) {
        return Ok(());
    }
    let mut victims = BTreeSet::from([connection.pid]);
    loop {
        let before = victims.len();
        for process in ps.values() {
            if victims.contains(&process.parent) {
                victims.insert(process.pid);
            }
        }
        if victims.len() == before {
            break;
        }
    }
    // Stop authentication/session creation first, then close the captured descendants.
    for pid in
        std::iter::once(connection.pid).chain(victims.into_iter().filter(|p| *p != connection.pid))
    {
        let result = unsafe { libc::kill(pid, libc::SIGKILL) };
        if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
pub struct Runtime {
    #[serde(default)]
    pub connections: BTreeMap<i32, Connection>,
    #[serde(default)]
    pub requested_at: Option<i64>,
    #[serde(default)]
    pub last_away_end: Option<i64>,
    #[serde(default)]
    inode: u64,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    partial: String,
    #[serde(skip)]
    pub last_policy: Option<(bool, bool)>,
    #[serde(skip)]
    pub last_repair: i64,
}
impl Runtime {
    pub fn load(cfg: &StudioSshConfig) -> Result<Self> {
        let path = expand_home(&cfg.assets_dir).join("transport.json");
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, cfg: &StudioSshConfig) -> Result<()> {
        atomic_private_file(
            &expand_home(&cfg.assets_dir).join("transport.json"),
            &serde_json::to_vec(self)?,
        )
    }
    /// Returns tunnel logins for owner pushes; disconnected/dead PIDs end their leases.
    pub fn read_log(&mut self, cfg: &StudioSshConfig, now: i64) -> Result<Vec<Connection>> {
        self.read_log_with_snapshot(cfg, now, processes)
    }

    fn read_log_with_snapshot(
        &mut self,
        cfg: &StudioSshConfig,
        now: i64,
        snapshot: impl FnOnce() -> Result<BTreeMap<i32, Process>>,
    ) -> Result<Vec<Connection>> {
        // Every accepted line must predate the snapshot used to identify its live process.
        let cursor = (self.inode, self.offset, self.partial.clone());
        let complete = self.log_lines(cfg)?;
        let ps = match snapshot() {
            Ok(ps) => ps,
            Err(error) => {
                // A transient ps failure must not acknowledge unprocessed authentication events.
                (self.inode, self.offset, self.partial) = cursor;
                return Err(error);
            }
        };
        let dead: Vec<_> = self
            .connections
            .values()
            .filter(|c| !c.alive(&ps))
            .map(|c| c.pid)
            .collect();
        for pid in dead {
            self.disconnected(pid, now);
        }
        let mut notices = Vec::new();
        for line in complete.lines() {
            if let Some(login) = self.ingest(line, &ps, now) {
                notices.push(login);
            }
        }
        Ok(notices)
    }
    fn log_lines(&mut self, cfg: &StudioSshConfig) -> Result<String> {
        let path = expand_home(&cfg.assets_dir).join("sshd.err.log");
        let mut log = match fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
            Err(e) => return Err(e.into()),
        };
        let meta = log.metadata()?;
        if self.inode != meta.ino() || meta.len() < self.offset {
            self.inode = meta.ino();
            self.offset = 0;
            self.partial.clear();
        }
        log.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        // Read a bounded chunk each second; never split and discard a partial log line.
        log.take(4 * 1024 * 1024).read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&bytes));
        let end = self.partial.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let complete = self.partial[..end].to_owned();
        self.partial.drain(..end);
        Ok(complete)
    }
    pub fn ingest(
        &mut self,
        line: &str,
        ps: &BTreeMap<i32, Process>,
        now: i64,
    ) -> Option<Connection> {
        let (pid, message) = if let Some((prefix, message)) = line.split_once("): ") {
            (prefix.rsplit_once("pid=")?.1.parse::<i32>().ok()?, message)
        } else {
            let (prefix, message) = line.split_once("]: ")?;
            (prefix.rsplit_once('[')?.1.parse::<i32>().ok()?, message)
        };
        if message.contains("Disconnected from ")
            || message.contains("Received disconnect from ")
            || message.contains("Connection closed by ")
        {
            self.disconnected(pid, now);
            return None;
        }
        let message = message.strip_prefix("Accepted publickey for ")?;
        let (_, rest) = message.split_once(" from ")?;
        let source = rest.split_whitespace().next()?;
        let fp = rest.split_whitespace().find(|s| s.starts_with("SHA256:"))?;
        let process = ps.get(&pid).filter(|p| p.command.contains("sshd"));
        let connection = Connection {
            pid,
            birth: process.map(|p| p.birth.clone()).unwrap_or_default(),
            fingerprint: fp.into(),
            away: matches!(source, "127.0.0.1" | "::1" | "::ffff:127.0.0.1"),
        };
        // Even a login that exits between ticks must generate its owner notice.
        let new = if process.is_some() {
            self.connections.insert(pid, connection.clone()).is_none()
        } else {
            if connection.away {
                self.last_away_end = Some(now);
            }
            true
        };
        if new && connection.away {
            Some(connection)
        } else {
            None
        }
    }
    fn disconnected(&mut self, pid: i32, now: i64) {
        if self.connections.remove(&pid).is_some_and(|c| c.away) {
            self.last_away_end = Some(now);
        }
    }
    pub fn tunnel_needed(
        &mut self,
        now: i64,
        manual: bool,
        shells: bool,
        linger_minutes: i64,
    ) -> bool {
        if manual {
            return true;
        }
        if !shells {
            self.requested_at = None;
            return false;
        }
        let Some(request) = self.requested_at else {
            return false;
        };
        if self.connections.values().any(|c| c.away) {
            return true;
        }
        if now - request.max(self.last_away_end.unwrap_or(request)) < linger_minutes * 60 {
            return true;
        }
        self.requested_at = None;
        false
    }
    pub fn revoke(&mut self, keys: &[Key]) -> Result<()> {
        let allowed: BTreeSet<_> = keys
            .iter()
            .filter(|k| k.revoked_at.is_none())
            .map(|k| k.fingerprint.as_str())
            .collect();
        // Leave killed logins tracked until the next process snapshot reaps them.
        for login in self
            .connections
            .values()
            .filter(|c| !allowed.contains(c.fingerprint.as_str()))
        {
            terminate(login)?;
        }
        Ok(())
    }
}

/// Rewrites only the addresses and key source. SIGHUP reopens listeners while logins survive.
pub fn configure(cfg: &StudioSshConfig, lan: Option<std::net::Ipv4Addr>) -> Result<bool> {
    let path = expand_home(&cfg.assets_dir).join("sshd_config");
    let original =
        fs::read_to_string(&path).context("Run scripts/setup_studio_ssh.sh --local-only first")?;
    let mut config = String::new();
    for line in original.lines() {
        let name = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "listenaddress" | "authorizedkeysfile" | "loglevel" | "logverbose"
        ) {
            config.push_str(line);
            config.push('\n');
        }
    }
    config.push_str(&format!("ListenAddress 127.0.0.1\nAuthorizedKeysFile {}\nLogLevel VERBOSE\nLogVerbose auth.c:auth_log():*\n", expand_home(&cfg.device_keys_file).display()));
    if let Some(address) = lan {
        config.push_str(&format!("ListenAddress {address}\n"));
    }
    if original == config {
        return Ok(false);
    }
    atomic_private_file(&path, config.as_bytes())?;
    Ok(true)
}

pub fn manual_enabled(cfg: &StudioSshConfig) -> bool {
    fs::read_to_string(expand_home(&cfg.assets_dir).join("manual_enabled"))
        .map(|s| s.trim() == "true")
        .unwrap_or_else(|_| {
            // One-time upgrade from the original toggle. Automatic leases have their own state.
            if crate::config::test_isolation_active() {
                return false;
            }
            super::agent_loaded(super::current_uid(), &cfg.sshd_launch_agent_label)
                && super::agent_loaded(super::current_uid(), &cfg.tunnel_launch_agent_label)
        })
}
pub fn save_manual(cfg: &StudioSshConfig, enabled: bool) -> Result<()> {
    atomic_private_file(
        &expand_home(&cfg.assets_dir).join("manual_enabled"),
        if enabled { b"true" } else { b"false" },
    )
}

/// Called under the shell lifecycle lock. Tests use private SSH fixtures, never live LaunchAgents.
pub fn apply(cfg: &StudioSshConfig, manual: bool, lan_on: bool, tunnel_on: bool) -> Result<()> {
    if crate::config::test_isolation_active() {
        return Ok(());
    }
    save_manual(cfg, manual)?;
    let lan = lan_address(lan_on, crate::terminal_lan::default_route_ipv4);
    let changed = configure(cfg, lan)?;
    let uid = super::current_uid();
    if lan_on || tunnel_on {
        if !super::agent_loaded(uid, &cfg.sshd_launch_agent_label) {
            super::enable_agent(uid, &cfg.sshd_launch_agent_label).map_err(anyhow::Error::msg)?;
        } else if changed {
            let out = super::run_launchctl(&[
                "kill",
                "SIGHUP",
                &super::service_target(uid, &cfg.sshd_launch_agent_label),
            ]);
            anyhow::ensure!(
                out.success,
                "Cannot reload SSH listener: {}",
                super::describe(&out)
            );
        }
    }
    for (label, on) in [
        (&cfg.tunnel_launch_agent_label, tunnel_on),
        (&cfg.sshd_launch_agent_label, lan_on || tunnel_on),
    ] {
        if on {
            if !super::agent_running(uid, label) {
                super::enable_agent(uid, label).map_err(anyhow::Error::msg)?;
            }
        } else if super::agent_loaded(uid, label) {
            super::disable_agent(uid, label).map_err(anyhow::Error::msg)?;
        }
    }
    Ok(())
}

fn lan_address(
    requested: bool,
    discover: impl FnOnce() -> Result<std::net::Ipv4Addr>,
) -> Option<std::net::Ipv4Addr> {
    if !requested {
        return None;
    }
    match discover() {
        Ok(address) => Some(address),
        Err(error) => {
            eprintln!(
                "Studio SSH home listener unavailable; loopback/tunnel policy continues: {error:#}"
            );
            None
        }
    }
}

pub fn migration_candidates(path: &Path) -> Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .filter_map(|line| crate::device_ssh::parse(line).ok().map(|(_, key)| key))
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_lan_address_preserves_loopback_config_for_tunnel_start() {
        let dir = std::env::temp_dir().join(format!(
            "sm-2334-no-lan-{}",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let cfg = StudioSshConfig {
            assets_dir: dir.display().to_string(),
            ..StudioSshConfig::default()
        };
        fs::write(
            dir.join("sshd_config"),
            "Port 22222\nListenAddress 192.168.1.2\n",
        )
        .unwrap();
        let address = lan_address(true, || {
            anyhow::bail!("VPN interface has no private IPv4 address")
        });
        assert!(configure(&cfg, address).unwrap());
        let config = fs::read_to_string(dir.join("sshd_config")).unwrap();
        assert!(config.contains("ListenAddress 127.0.0.1\n"));
        assert!(!config.contains("ListenAddress 192.168.1.2"));
        assert!(lan_address(false, || panic!(
            "disabled LAN must not discover an address"
        ))
        .is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_process_snapshot_retries_login_lines_and_birth_identity_rejects_reuse() {
        let dir = std::env::temp_dir().join(format!(
            "sm-2334-log-{}",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let cfg = StudioSshConfig {
            assets_dir: dir.display().to_string(),
            ..StudioSshConfig::default()
        };
        fs::write(dir.join("sshd.err.log"),"auth.c:auth_log():306 (bin=sshd-session, pid=123): Accepted publickey for rajesh from 127.0.0.1 port 40000 ssh2: ED25519 SHA256:fixture\n").unwrap();
        let mut runtime = Runtime::default();
        assert!(runtime
            .read_log_with_snapshot(&cfg, 100, || anyhow::bail!("ps unavailable"))
            .is_err());
        assert_eq!(runtime.offset, 0);
        let ps = BTreeMap::from([(
            123,
            Process {
                pid: 123,
                parent: 1,
                birth: "first".into(),
                command: "sshd-session".into(),
            },
        )]);
        assert_eq!(
            runtime
                .read_log_with_snapshot(&cfg, 101, || Ok(ps.clone()))
                .unwrap()
                .len(),
            1
        );
        assert!(runtime.connections[&123].alive(&ps));
        let mut reused = ps;
        reused.get_mut(&123).unwrap().birth = "second".into();
        assert!(!runtime.connections[&123].alive(&reused));
        assert!(runtime
            .read_log_with_snapshot(&cfg, 102, || Ok(reused))
            .unwrap()
            .is_empty());
        assert!(runtime.connections.is_empty());
        assert_eq!(runtime.last_away_end, Some(102));
        fs::remove_dir_all(dir).unwrap();
    }
}
