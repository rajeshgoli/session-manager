use super::*;

pub(crate) fn terminal() -> Terminal {
    Terminal {
        id: "term-abcd".into(),
        cwd: "/tmp".into(),
        name: None,
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
    let mut named = terminal();
    named.name = Some("Benchmarks".into());
    store.insert(&named).unwrap();
    store.ensure_schema().unwrap();
    store.observe("term-abcd", true, true).unwrap();
    store.input("term-abcd").unwrap();
    let reopened = Store::new(path);
    let t = reopened.get("term-abcd").unwrap().unwrap();
    assert_eq!(t.name.as_deref(), Some("Benchmarks"));
    assert!(t.busy_since.is_some() && t.last_attach_at.is_some() && t.last_input_at.is_some());
    reopened.retire("term-abcd", "owner").unwrap();
    assert!(reopened.list(false).unwrap().is_empty());
    assert_eq!(
        reopened.list(true).unwrap()[0].retired_reason.as_deref(),
        Some("owner")
    );
    std::fs::remove_dir_all(dir).unwrap();
}
