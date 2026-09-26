//! Resolves the Claude Remote Control link (`https://claude.ai/code/<id>`) for
//! live Claude sessions, so the mobile app can open the Claude app instead of
//! the in-app terminal.
//!
//! Each interactive Claude process writes `<claude config dir>/sessions/<pid>.json`
//! with its `bridgeSessionId` (the Remote Control session id) and its `tmux`
//! pane (`<tmux session>:@window.%pane`). The file format is undocumented, so
//! anything unexpected yields no link and the app falls back to the terminal.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use serde::Deserialize;

use crate::sessions::expand_home;

const REMOTE_CONTROL_URL_PREFIX: &str = "https://claude.ai/code/";
const CACHE_TTL: Duration = Duration::from_secs(3);

type LinkMap = HashMap<String, String>;

static CACHE: Mutex<Option<(Instant, Arc<LinkMap>)>> = Mutex::new(None);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSessionFile {
    pid: Option<i64>,
    bridge_session_id: Option<String>,
    tmux: Option<String>,
}

/// Remote Control link for the Claude process running in `tmux_session`, if one
/// is live. Scans are cached briefly because session lists are polled.
pub fn remote_control_url(tmux_session: &str) -> Option<String> {
    let tmux_session = tmux_session.trim();
    if tmux_session.is_empty() {
        return None;
    }
    links().get(tmux_session).cloned()
}

fn links() -> Arc<LinkMap> {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((scanned_at, links)) = cache.as_ref() {
        if scanned_at.elapsed() < CACHE_TTL {
            return Arc::clone(links);
        }
    }
    let links = Arc::new(scan_session_dirs(&claude_session_dirs(), process_alive));
    *cache = Some((Instant::now(), Arc::clone(&links)));
    links
}

fn claude_session_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config_dirs) = env::var_os("CLAUDE_CONFIG_DIR") {
        for config_dir in config_dirs.to_string_lossy().split(',') {
            let config_dir = config_dir.trim();
            if !config_dir.is_empty() {
                dirs.push(expand_home(config_dir).join("sessions"));
            }
        }
    }
    let default_dir = expand_home("~/.claude/sessions");
    if !dirs.contains(&default_dir) {
        dirs.push(default_dir);
    }
    dirs
}

/// Maps tmux session name to Remote Control URL for every live Claude process
/// found in `dirs`.
fn scan_session_dirs(dirs: &[PathBuf], is_alive: impl Fn(i64) -> bool) -> LinkMap {
    let mut links = LinkMap::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some((tmux_session, url)) = link_from_file(&path, &is_alive) {
                links.entry(tmux_session).or_insert(url);
            }
        }
    }
    links
}

fn link_from_file(path: &Path, is_alive: impl Fn(i64) -> bool) -> Option<(String, String)> {
    let contents = fs::read(path).ok()?;
    let file: ClaudeSessionFile = serde_json::from_slice(&contents).ok()?;
    let bridge_session_id = file.bridge_session_id?;
    let bridge_session_id = bridge_session_id.trim();
    if bridge_session_id.is_empty()
        || !bridge_session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    let tmux_session = file.tmux?.split(':').next()?.trim().to_owned();
    if tmux_session.is_empty() || !is_alive(file.pid?) {
        return None;
    }
    Some((
        tmux_session,
        format!("{REMOTE_CONTROL_URL_PREFIX}{bridge_session_id}"),
    ))
}

fn process_alive(pid: i64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = env::temp_dir().join(format!(
                "sm-claude-rc-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).expect("write session file");
    }

    #[test]
    fn maps_live_sessions_by_tmux_session_name() {
        let dir = TempDir::new();
        write(
            dir.path(),
            "100.json",
            r#"{"pid":100,"bridgeSessionId":"session_01Abc","tmux":"sm-rust-claude-aaa:@4.%4"}"#,
        );
        write(
            dir.path(),
            "200.json",
            r#"{"pid":200,"bridgeSessionId":"session_02Dead","tmux":"sm-rust-claude-bbb:@5.%5"}"#,
        );
        write(
            dir.path(),
            "300.json",
            r#"{"pid":300,"tmux":"sm-rust-claude-ccc:@6.%6"}"#,
        );
        write(
            dir.path(),
            "400.json",
            r#"{"pid":400,"bridgeSessionId":"session_04?x=1","tmux":"sm-rust-claude-ddd:@7.%7"}"#,
        );
        write(dir.path(), "500.json", "not json");
        write(
            dir.path(),
            "600.key",
            r#"{"pid":600,"bridgeSessionId":"session_06","tmux":"sm-rust-claude-fff"}"#,
        );

        let links = scan_session_dirs(&[dir.path().to_path_buf()], |pid| pid != 200);

        assert_eq!(
            links,
            LinkMap::from([(
                "sm-rust-claude-aaa".to_owned(),
                "https://claude.ai/code/session_01Abc".to_owned()
            )])
        );
    }

    #[test]
    fn missing_directory_yields_no_links() {
        let dir = TempDir::new();
        let links = scan_session_dirs(&[dir.path().join("absent")], |_| true);
        assert!(links.is_empty());
    }

    #[test]
    fn current_process_is_alive() {
        assert!(process_alive(i64::from(std::process::id())));
        assert!(!process_alive(0));
        assert!(!process_alive(-1));
    }
}
