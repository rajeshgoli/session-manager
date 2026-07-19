//! On-demand "Studio SSH" toggle.
//!
//! Drives two per-user `launchd` agents through `launchctl`:
//!   * a dedicated loopback `sshd` bound to `127.0.0.1:<local_sshd_port>`, and
//!   * a `cloudflared` tunnel that publishes it at `<hostname>`.
//!
//! The functions here are pure/synchronous (they shell out to `launchctl` and do
//! a TCP probe) and take a [`StudioSshConfig`] by reference. `launchctl` exits
//! nonzero for several benign conditions (bootstrapping an already-loaded agent,
//! booting out an agent that is not loaded); those are treated as non-fatal.

use std::{
    io::Read,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::config::StudioSshConfig;

/// Observed state of the Studio SSH toggle. Field names/values are a frozen JSON
/// contract shared with the Android app and the web UI — do not rename them.
#[derive(Debug, Clone, Serialize)]
pub struct StudioSshStatus {
    /// Desired state: both LaunchAgents are loaded/enabled in launchd.
    pub enabled: bool,
    /// Observed lifecycle: `"off" | "starting" | "on" | "error"`.
    pub status: String,
    /// Public hostname the tunnel publishes.
    pub host: String,
    /// The loopback sshd is accepting TCP connections.
    pub sshd_listening: bool,
    /// The cloudflared tunnel agent is loaded and running.
    pub tunnel_running: bool,
    /// Human-readable error, populated only when `status == "error"`.
    pub error: Option<String>,
}

const STATUS_OFF: &str = "off";
const STATUS_STARTING: &str = "starting";
const STATUS_ON: &str = "on";
const STATUS_ERROR: &str = "error";

const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(10);
const TCP_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Observe both agents and derive the toggle status. Never fails; on unexpected
/// `launchctl` trouble it simply reports the agents as not loaded.
pub fn status(cfg: &StudioSshConfig) -> StudioSshStatus {
    let uid = current_uid();
    let sshd_loaded = agent_loaded(uid, &cfg.sshd_launch_agent_label);
    let tunnel_loaded = agent_loaded(uid, &cfg.tunnel_launch_agent_label);
    let enabled = sshd_loaded && tunnel_loaded;

    let sshd_listening = sshd_loaded && tcp_listening(cfg.local_sshd_port);
    let tunnel_running = tunnel_loaded && agent_running(uid, &cfg.tunnel_launch_agent_label);

    let status = if !enabled {
        STATUS_OFF
    } else if sshd_listening && tunnel_running {
        STATUS_ON
    } else {
        STATUS_STARTING
    };

    StudioSshStatus {
        enabled,
        status: status.to_owned(),
        host: cfg.hostname.clone(),
        sshd_listening,
        tunnel_running,
        error: None,
    }
}

/// Enable the toggle: enable + bootstrap + kickstart the sshd agent, then the
/// tunnel agent. Returns the freshly observed status (likely `"starting"` since
/// the tunnel takes a moment to connect). Errors only on genuinely fatal
/// `launchctl` failures.
pub fn enable(cfg: &StudioSshConfig) -> Result<StudioSshStatus, String> {
    let uid = current_uid();
    enable_agent(uid, &cfg.sshd_launch_agent_label)?;
    enable_agent(uid, &cfg.tunnel_launch_agent_label)?;
    Ok(status(cfg))
}

/// Disable the toggle: boot out + disable the tunnel agent, then the sshd agent.
pub fn disable(cfg: &StudioSshConfig) -> Result<StudioSshStatus, String> {
    let uid = current_uid();
    disable_agent(uid, &cfg.tunnel_launch_agent_label)?;
    disable_agent(uid, &cfg.sshd_launch_agent_label)?;
    Ok(status(cfg))
}

/// Repair toward the `desired` state. Called on every reconcile tick, so it drives
/// BOTH directions: when `desired` is true it re-enables a toggle that drifted off
/// (e.g. a crashed agent); when `desired` is false it re-disables one that drifted
/// on (e.g. a stray enable that raced a disable). This makes the in-memory desired
/// flag authoritative — "off" stays off. Never flips the desired flag itself.
pub fn reconcile(cfg: &StudioSshConfig, desired: bool) -> StudioSshStatus {
    let current = status(cfg);
    if desired {
        if current.status == STATUS_ON {
            return current;
        }
        match enable(cfg) {
            Ok(status) => status,
            Err(error) => error_status(cfg, error),
        }
    } else {
        if current.status == STATUS_OFF {
            return current;
        }
        match disable(cfg) {
            Ok(status) => status,
            Err(error) => error_status(cfg, error),
        }
    }
}

fn error_status(cfg: &StudioSshConfig, error: String) -> StudioSshStatus {
    let mut status = status(cfg);
    status.status = STATUS_ERROR.to_owned();
    status.error = Some(error);
    status
}

fn enable_agent(uid: u32, label: &str) -> Result<(), String> {
    let target = service_target(uid, label);
    let domain = domain_target(uid);
    let plist = plist_path(label);
    let plist_str = plist.to_string_lossy().into_owned();

    // `enable` clears a prior `disable`; it is idempotent, so ignore its exit.
    let _ = run_launchctl(&["enable", &target]);

    // `bootstrap` loads + starts the agent (RunAtLoad). Its exit code is NOT a
    // reliable success signal: it returns nonzero both for the benign
    // "already loaded" case and for genuine failures (a malformed or unreadable
    // plist) — both of which can surface as "5: Input/output error". So we do not
    // trust the exit here; the authoritative check is `agent_loaded` below.
    let bootstrap = run_launchctl(&["bootstrap", &domain, &plist_str]);

    // `kickstart -k` force-restarts an already-loaded agent. It is best-effort:
    // `bootstrap` already started the service, and kickstart can legitimately time
    // out while a slow child (cloudflared) shuts down before respawning. Never
    // fail `enable` on kickstart alone — a bootstrapped agent is up regardless.
    let _ = run_launchctl(&["kickstart", "-k", &target]);

    // Authoritative success signal: the agent is actually loaded. This surfaces a
    // genuine bootstrap failure (plist problem => not loaded) while tolerating the
    // benign already-loaded and kickstart-timeout cases.
    if agent_loaded(uid, label) {
        Ok(())
    } else {
        Err(format!(
            "launchctl bootstrap {label} did not load the agent: {}",
            describe(&bootstrap)
        ))
    }
}

fn disable_agent(uid: u32, label: &str) -> Result<(), String> {
    let target = service_target(uid, label);

    // `bootout` unloads the agent. Booting out one that is not loaded exits
    // nonzero ("3: No such process" / "Could not find specified service") — benign.
    let bootout = run_launchctl(&["bootout", &target]);
    if !bootout.success && !is_benign_bootout(&bootout.stderr) {
        return Err(format!(
            "launchctl bootout {label} failed: {}",
            describe(&bootout)
        ));
    }

    // `disable` persists the off state so the agent will NOT RunAtLoad on next
    // login. This is the step that makes "off" durable across reboots, so a
    // failure here matters: report it rather than claiming the toggle is off while
    // the RunAtLoad agent could return after a reboot.
    let disable = run_launchctl(&["disable", &target]);
    if !disable.success {
        return Err(format!(
            "launchctl disable {label} failed: {}",
            describe(&disable)
        ));
    }
    Ok(())
}

/// True when `launchctl print gui/<uid>/<label>` succeeds (agent is loaded).
fn agent_loaded(uid: u32, label: &str) -> bool {
    run_launchctl(&["print", &service_target(uid, label)]).success
}

/// True when the agent is loaded AND launchd reports it running (has a live pid /
/// `state = running`).
fn agent_running(uid: u32, label: &str) -> bool {
    let output = run_launchctl(&["print", &service_target(uid, label)]);
    if !output.success {
        return false;
    }
    let text = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    text.contains("state = running")
        || text
            .lines()
            .filter_map(|line| line.split_once('='))
            .any(|(key, value)| {
                key.trim() == "pid" && value.trim().chars().any(|c| c.is_ascii_digit())
            })
}

fn tcp_listening(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, TCP_PROBE_TIMEOUT).is_ok()
}

fn service_target(uid: u32, label: &str) -> String {
    format!("gui/{uid}/{label}")
}

fn domain_target(uid: u32) -> String {
    format!("gui/{uid}")
}

fn plist_path(label: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/rajesh".to_owned());
    PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{label}.plist"))
}

/// Resolve the current uid once. Falls back to `id -u`, then to 501 (the frozen
/// `gui/501` domain) so behaviour is deterministic even if the lookup fails.
fn current_uid() -> u32 {
    static UID: OnceLock<u32> = OnceLock::new();
    *UID.get_or_init(|| {
        Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(501)
    })
}

struct LaunchctlResult {
    success: bool,
    stdout: String,
    stderr: String,
}

fn describe(result: &LaunchctlResult) -> String {
    let stderr = result.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_owned();
    }
    let stdout = result.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_owned();
    }
    "no output".to_owned()
}

fn run_launchctl(args: &[&str]) -> LaunchctlResult {
    let mut command = Command::new("launchctl");
    command.args(args);
    match run_command_with_timeout(command, LAUNCHCTL_TIMEOUT) {
        Ok((success, stdout, stderr)) => LaunchctlResult {
            success,
            stdout,
            stderr,
        },
        Err(error) => LaunchctlResult {
            success: false,
            stdout: String::new(),
            stderr: error,
        },
    }
}

/// Run a command with a wall-clock timeout, capturing stdout/stderr. Mirrors the
/// `command_output_with_timeout` pattern used elsewhere in the crate.
fn run_command_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> Result<(bool, String, String), String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut handle) = child.stdout.take() {
                    let _ = handle.read_to_string(&mut stdout);
                }
                if let Some(mut handle) = child.stderr.take() {
                    let _ = handle.read_to_string(&mut stderr);
                }
                return Ok((status.success(), stdout, stderr));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn is_benign_bootout(stderr: &str) -> bool {
    let text = stderr.to_ascii_lowercase();
    text.contains("no such process")
        || text.contains("could not find")
        || text.contains("not find service")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StudioSshConfig {
        StudioSshConfig::default()
    }

    #[test]
    fn status_serializes_frozen_field_names() {
        let status = StudioSshStatus {
            enabled: true,
            status: STATUS_ON.to_owned(),
            host: "studio-ssh.rajeshgo.li".to_owned(),
            sshd_listening: true,
            tunnel_running: true,
            error: None,
        };
        // Serialized field order matches the frozen contract declaration order.
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(
            json,
            r#"{"enabled":true,"status":"on","host":"studio-ssh.rajeshgo.li","sshd_listening":true,"tunnel_running":true,"error":null}"#
        );
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["enabled"], serde_json::json!(true));
        assert_eq!(value["status"], serde_json::json!("on"));
        assert_eq!(value["host"], serde_json::json!("studio-ssh.rajeshgo.li"));
        assert_eq!(value["sshd_listening"], serde_json::json!(true));
        assert_eq!(value["tunnel_running"], serde_json::json!(true));
        assert_eq!(value["error"], serde_json::Value::Null);
        // Exactly the frozen snake_case keys, nothing extra.
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "enabled",
                "error",
                "host",
                "sshd_listening",
                "status",
                "tunnel_running",
            ]
        );
    }

    #[test]
    fn service_and_domain_targets_use_frozen_domain() {
        assert_eq!(
            service_target(501, "com.rajesh.sm-studio-ssh-sshd"),
            "gui/501/com.rajesh.sm-studio-ssh-sshd"
        );
        assert_eq!(domain_target(501), "gui/501");
    }

    #[test]
    fn plist_path_is_under_launch_agents() {
        let path = plist_path("com.rajesh.sm-studio-ssh-tunnel");
        assert!(path.ends_with("Library/LaunchAgents/com.rajesh.sm-studio-ssh-tunnel.plist"));
    }

    #[test]
    fn benign_bootout_errors_are_recognized() {
        // bootout of an agent that is not loaded is benign (nothing to unload).
        assert!(is_benign_bootout("Boot-out failed: 3: No such process"));
        assert!(is_benign_bootout("Could not find specified service"));
        // a real permission failure is NOT benign and must surface.
        assert!(!is_benign_bootout(
            "Boot-out failed: 1: Operation not permitted"
        ));
    }

    #[test]
    fn error_status_carries_message() {
        let status = error_status(&cfg(), "boom".to_owned());
        assert_eq!(status.status, STATUS_ERROR);
        assert_eq!(status.error.as_deref(), Some("boom"));
        assert_eq!(status.host, "studio-ssh.rajeshgo.li");
    }
}
