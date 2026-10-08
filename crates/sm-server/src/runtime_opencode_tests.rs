use super::*;
use crate::opencode::tests::{ScratchDir, Stub};
use serde_json::json;

#[test]
fn opencode_readiness_uses_authenticated_http_and_never_invokes_tmux() {
    let scratch = ScratchDir::new();
    let root = scratch.path().canonicalize().unwrap();
    let stub = Stub::new();
    let password = "a".repeat(64);
    stub.set_password(&password);
    let mut config = AppConfig::default();
    config.paths.state_file = root.join("sessions.json").display().to_string();
    config.opencode.state_root = root.join("native").display().to_string();
    config.opencode.port_range = [stub.port, stub.port];
    let state_dir = root.join("native/local");
    let native_config = state_dir.join("xdg/config/opencode/opencode.json");
    fs::create_dir_all(native_config.parent().unwrap()).unwrap();
    fs::write(
        &native_config,
        config.opencode.render_agent_config().unwrap(),
    )
    .unwrap();
    fs::set_permissions(&native_config, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(state_dir.join("server.secret"), &password).unwrap();
    fs::set_permissions(
        state_dir.join("server.secret"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut data = json!({"sessions":[{
        "id":"local", "name":"local", "provider":"opencode", "working_dir":root,
        "status":"idle", "tmux_session":"local-view", "provider_resume_id":"ses_test",
        "created_at":"now", "last_activity":"now",
        "opencode":{"port":stub.port,"state_dir":state_dir,"version":"1.17.9","model_base_url":"http://127.0.0.1:8000/v1"}
    }]});
    fs::write(&config.paths.state_file, data.to_string()).unwrap();
    let fake = root.join("tmux");
    let marker = root.join("pane-was-read");
    fs::write(
        &fake,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let runtime =
        TmuxRuntime::from_app_config(&config).with_tmux_binary_for_test(fake.display().to_string());
    assert!(runtime.session_input_ready("local-view", "opencode"));
    runtime
        .wait_for_initial_brief_readiness("local-view", "opencode")
        .unwrap();
    assert!(stub
        .request_log()
        .iter()
        .any(|(_, path, _)| path == "/global/health"));
    assert!(stub
        .request_log()
        .iter()
        .any(|(_, path, _)| path == "/session/status"));
    stub.set_password("wrong");
    assert!(!runtime.session_input_ready("local-view", "opencode"));
    stub.set_password(&password);
    assert!(!runtime
        .for_socket_name(Some("other"))
        .session_input_ready("local-view", "opencode"));
    assert!(!runtime.session_input_ready("unknown", "opencode"));
    assert!(!runtime.session_input_ready("local-view", "unknown"));
    data["sessions"][0]["status"] = json!("stopped");
    fs::write(&config.paths.state_file, data.to_string()).unwrap();
    assert!(!runtime.session_input_ready("local-view", "opencode"));
    assert!(!marker.exists());
}
