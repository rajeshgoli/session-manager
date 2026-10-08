use super::*;
use crate::opencode::{tests::ScratchDir, RuntimeBinding};

fn record(provider: &str) -> Value {
    json!({"id":"local-test", "name":"local-test", "working_dir":"/repo",
        "tmux_session":"sm-rust-local-test", "provider":provider, "status":"running",
        "created_at":"2026-10-07T00:00:00Z", "last_activity":"2026-10-07T00:00:01Z"})
}

fn binding() -> RuntimeBinding {
    RuntimeBinding {
        port: 18503,
        state_dir: "/Users/rajesh/.local/share/claude-sessions/opencode/local-test".into(),
        version: "1.17.9".into(),
        model_base_url: "http://127.0.0.1:8000/v1".into(),
    }
}

fn launch(provider: &str) -> SessionRuntimeLaunchRecord {
    serde_json::from_value(json!({"id":"launch-test", "operation_kind":"create", "session_id":"local-test",
        "tmux_session":"sm-rust-local-test", "working_dir":"/repo", "log_file":"/tmp/local-test.log",
        "provider":provider, "provider_resume_id":"ses_conversation", "initial_message":"brief",
        "credential_sha256":"not-a-credential", "status":"launching", "created_at":"2026-10-07T00:00:00Z",
        "updated_at":"2026-10-07T00:00:00Z"})).unwrap()
}

#[test]
fn existing_provider_records_omit_new_optional_fields_and_keep_raw_state() {
    for provider in ["claude", "codex", "codex-fork"] {
        let mut value = record(provider);
        value["reasoning_effort"] = json!("high");
        let raw = json!({"sessions":[value.clone()]});
        let parsed = ParsedState::new(raw.clone());
        assert_eq!(parsed.raw, raw);
        let session: SessionRecord = serde_json::from_value(value).unwrap();
        let serialized = serde_json::to_value(session).unwrap();
        assert!(!serialized.as_object().unwrap().contains_key("opencode"));
        assert!(!serialized
            .as_object()
            .unwrap()
            .contains_key("provider_event_cursor"));
        let serialized = serde_json::to_value(launch(provider)).unwrap();
        assert!(!serialized
            .as_object()
            .unwrap()
            .contains_key("brief_message_id"));
        assert!(!serialized
            .as_object()
            .unwrap()
            .contains_key("brief_part_id"));
        assert!(!serialized
            .as_object()
            .unwrap()
            .contains_key("opencode_restore_terminal_metadata"));
    }
}

#[test]
fn opencode_binding_cursor_and_effort_round_trip_through_the_store() {
    let tmp = ScratchDir::new();
    let path = tmp.path().join("sessions.json");
    let mut value = record("opencode");
    value["opencode"] = serde_json::to_value(binding()).unwrap();
    value["provider_resume_id"] = json!("ses_conversation");
    value["provider_event_cursor"] = json!("msg_cursor");
    value["reasoning_effort"] = json!("high");
    fs::write(
        &path,
        serde_json::to_vec(&json!({"sessions":[value]})).unwrap(),
    )
    .unwrap();
    let store = SessionStore::new(path);
    let session = store.get_session("local-test").unwrap().unwrap();
    assert_eq!(session.opencode, Some(binding()));
    assert_eq!(session.provider_event_cursor.as_deref(), Some("msg_cursor"));
    assert!(session.reasoning_effort.is_none());
    assert!(!session.is_stopped());
    let raw = store.load_raw_json_value().unwrap();
    assert!(raw["sessions"][0]["reasoning_effort"].is_null());
    assert_eq!(
        raw["sessions"][0]["opencode"],
        serde_json::to_value(binding()).unwrap()
    );
}

#[test]
fn missing_opencode_binding_fences_typed_and_raw_runtime_reads() {
    for missing in [None, Some(Value::Null)] {
        let tmp = ScratchDir::new();
        let path = tmp.path().join("sessions.json");
        let mut value = record("opencode");
        if let Some(missing) = missing {
            value["opencode"] = missing;
        }
        // Even direct decoding cannot authorize a live registry record.
        let direct: SessionRecord = serde_json::from_value(value.clone()).unwrap();
        assert!(direct.is_stopped());
        assert!(raw_session_is_stopped(value.as_object().unwrap()));
        fs::write(
            &path,
            serde_json::to_vec(&json!({"sessions":[value.clone()]})).unwrap(),
        )
        .unwrap();
        let store = SessionStore::new(path);
        let session = store.get_session("local-test").unwrap().unwrap();
        assert_eq!(session.status, "stopped");
        assert_eq!(
            session.completion_message.as_deref(),
            Some(OPENCODE_MISSING_BINDING)
        );
        assert_eq!(session.completion_status.as_deref(), Some("error"));
        assert_eq!(session.stopped_at, Some(session.last_activity.clone()));
        let raw = store.load_raw_json_value().unwrap();
        assert_eq!(raw["sessions"][0]["status"], "stopped");
        assert_eq!(
            snapshot_from_raw_value(&json!({"sessions":[value]}))
                .unwrap()
                .sessions[0]
                .status,
            "stopped"
        );
    }
}

#[test]
fn missing_binding_does_not_remove_an_explicit_retirement() {
    for status in ["retired", "killed"] {
        let mut value = record("opencode");
        value["completion_status"] = json!(status);
        value["completion_message"] = json!("owner retired this task");
        let parsed = ParsedState::new(json!({"sessions":[value]}));
        let session = &parsed.snapshot().unwrap().sessions[0];
        assert!(session.is_retired());
        assert_eq!(session.status, "stopped");
        assert_eq!(session.completion_status.as_deref(), Some(status));
        assert_eq!(
            session.completion_message.as_deref(),
            Some("owner retired this task")
        );
        assert_eq!(
            session.agent_status_text.as_deref(),
            Some(OPENCODE_MISSING_BINDING)
        );
    }
}

#[test]
fn clear_resets_cursor_and_keeps_runtime_binding() {
    let mut value = record("opencode");
    value["opencode"] = serde_json::to_value(binding()).unwrap();
    value["provider_event_cursor"] = json!("msg_old");
    reset_session_after_clear(value.as_object_mut().unwrap(), "2026-10-07T00:01:00Z");
    let session: SessionRecord = serde_json::from_value(value).unwrap();
    assert!(session.provider_event_cursor.is_none());
    assert_eq!(session.opencode, Some(binding()));
}

#[test]
fn persisted_initial_brief_pair_is_reused_after_reopening() {
    let mut launch = launch("opencode");
    let binding = launch.ensure_opencode_brief_binding().unwrap();
    binding.validate().unwrap();
    assert_eq!(binding.conversation_id, "ses_conversation");
    let mut reopened: SessionRuntimeLaunchRecord =
        serde_json::from_value(serde_json::to_value(&launch).unwrap()).unwrap();
    assert_eq!(reopened.ensure_opencode_brief_binding().unwrap(), binding);
    assert_eq!(launch, reopened);
}

#[test]
fn broken_brief_bindings_and_missing_conversation_never_generate_replacements() {
    let mut cases = Vec::new();
    let mut partial = launch("opencode");
    partial.brief_message_id = Some("msg_existing".into());
    cases.push(partial);
    let mut partial = launch("opencode");
    partial.brief_part_id = Some("prt_existing".into());
    cases.push(partial);
    let mut invalid = launch("opencode");
    invalid.brief_message_id = Some("msg_invalid/path".into());
    invalid.brief_part_id = Some("prt_existing".into());
    cases.push(invalid);
    let mut missing = launch("opencode");
    missing.provider_resume_id = None;
    cases.push(missing);
    cases.push(launch("claude"));
    for mut launch in cases {
        let before = launch.clone();
        assert!(launch.ensure_opencode_brief_binding().is_err());
        assert_eq!(launch, before);
    }
}

#[test]
fn runtime_binding_validation_rejects_bad_ports_paths_versions_and_endpoints() {
    binding().validate().unwrap();
    let mut cases = Vec::new();
    let mut invalid = binding();
    invalid.port = 0;
    cases.push(invalid);
    let mut invalid = binding();
    invalid.state_dir = "/tmp/../secrets".into();
    cases.push(invalid);
    let mut invalid = binding();
    invalid.state_dir = "relative/state".into();
    cases.push(invalid);
    let mut invalid = binding();
    invalid.state_dir = "/".into();
    cases.push(invalid);
    let mut invalid = binding();
    invalid.version.clear();
    cases.push(invalid);
    let mut invalid = binding();
    invalid.model_base_url = "http://example.com:8000/v1".into();
    cases.push(invalid);
    for invalid in cases {
        assert!(invalid.validate().is_err());
    }
}
