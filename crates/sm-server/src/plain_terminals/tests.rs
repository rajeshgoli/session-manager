use super::*;

pub(crate) fn terminal() -> Terminal {
    Terminal {
        id: "term-abcd".into(),
        cwd: "/tmp".into(),
        created_by_user: "owner".into(),
        created_by_device: None,
        created_via: "cli".into(),
        tmux_session: "term-abcd".into(),
        state: "live".into(),
        last_attach_at: None,
        last_input_at: None,
        busy_since: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        retired_at: None,
        retired_reason: None,
    }
}
#[test]
fn cleanup_requires_no_jobs_no_recent_attach_and_no_recent_input() {
    let now = OffsetDateTime::parse("2026-01-01T04:00:00Z", &Rfc3339).unwrap();
    let mut t = terminal();
    assert!(t.idle_expired(now, 180));
    t.busy_since = Some(t.created_at.clone());
    assert!(!t.idle_expired(now, 180));
    t.busy_since = None;
    t.last_attach_at = Some("2026-01-01T02:00:00Z".into());
    assert!(!t.idle_expired(now, 180));
    t.last_attach_at = None;
    t.last_input_at = Some("2026-01-01T02:00:00Z".into());
    assert!(!t.idle_expired(now, 180));
    t.last_input_at = Some("bad timestamp".into());
    assert!(!t.idle_expired(now, 180));
}
#[test]
fn records_survive_reopen_and_retire_preserves_history() {
    let dir = std::env::temp_dir().join(format!(
        "sm-plain-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let path = dir.join("usage.db");
    let store = Store::new(path.clone());
    store.ensure_schema().unwrap();
    store.insert(&terminal()).unwrap();
    store.observe("term-abcd", true, true).unwrap();
    store.input("term-abcd").unwrap();
    let reopened = Store::new(path);
    let t = reopened.get("term-abcd").unwrap().unwrap();
    assert!(t.busy_since.is_some() && t.last_attach_at.is_some() && t.last_input_at.is_some());
    reopened.retire("term-abcd", "owner").unwrap();
    assert!(reopened.list(false).unwrap().is_empty());
    assert_eq!(
        reopened.list(true).unwrap()[0].retired_reason.as_deref(),
        Some("owner")
    );
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn observe_parses_panes_when_the_server_has_no_utf8_locale() {
    let dir = std::env::temp_dir().join(format!(
        "sm-plain-locale-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // launchd starts sm-server with no LANG or LC_*; the wrapper reproduces that for tmux.
    let wrapper = dir.join("tmux");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nexec env -u LANG -u LC_ALL -u LC_CTYPE tmux \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .unwrap();
    let socket = format!(
        "sm-2356-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig {
        tmux_socket_name: Some(socket.clone()),
        ..crate::config::RustCoreConfig::default()
    })
    .with_tmux_binary_for_test(wrapper.display().to_string());
    let started = std::process::Command::new(&wrapper)
        .args(["-L", &socket, "new-session", "-d", "-s", "term-abcd", "sleep 30"])
        .status()
        .unwrap();
    assert!(started.success());
    let observed = observe(&runtime, &terminal());
    let _ = std::process::Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
    std::fs::remove_dir_all(dir).unwrap();
    let observed = observed.unwrap();
    assert!(observed.busy);
    assert_eq!(observed.command, "sleep");
}
