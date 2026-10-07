use super::*;
use crate::opencode::tests::{ScratchDir, Stub};
use serde_json::{json, Value};

struct Fixture {
    _tmp: ScratchDir,
    root: PathBuf,
    config: OpencodeConfig,
}
impl Fixture {
    fn new() -> Self {
        let tmp = ScratchDir::new();
        let root = tmp.path().canonicalize().unwrap();
        let binary = root.join("opencode");
        host_write(
            &binary,
            br##"#!/bin/sh
if [ "$1" = --version ]; then echo 1.17.9; exit 0; fi
test "$1 $2" = 'debug config' || exit 40
test -z "${ANTHROPIC_API_KEY-}${OPENAI_API_KEY-}" || exit 41
printf '%s\n' "$HOME" > "$XDG_STATE_HOME/prepared"
folder="$XDG_CONFIG_HOME/opencode"
mkdir -p "$folder/node_modules/@opencode-ai/plugin"
printf '{}\n' > "$folder/node_modules/@opencode-ai/plugin/package.json"
printf 'export const fixture = 1;\n' > "$folder/node_modules/@opencode-ai/plugin/index.js"
printf '{}\n' > "$folder/package.json"
printf 'fixture-lock\n' > "$folder/bun.lock"
"##,
            0o700,
        )
        .unwrap();
        let config = OpencodeConfig {
            binary: binary.display().to_string(),
            state_root: root.join("home/state").display().to_string(),
            ..OpencodeConfig::default()
        };
        Self {
            _tmp: tmp,
            root,
            config,
        }
    }
    fn files(&self) -> LaunchFiles {
        LaunchFiles::prepare(&self.config, "local-test", 18503).unwrap()
    }
}

fn model(state: &str) -> ModelRecord {
    serde_json::from_value(json!({"key":"fixture", "server":"mtplx", "identifier":"actual-model", "seats":1,
        "context":200000, "reservation_bytes":0, "measured_peak_bytes":0, "state":state, "desired":true,
        "state_since":"now", "last_yield_reason":null, "last_yield_at":null, "last_error":null,
        "pid":null, "endpoint":"http://127.0.0.1:8000"})).unwrap()
}
fn seat() -> SessionRecord {
    serde_json::from_value(json!({"id":"old", "name":"old", "friendly_name":"local-owner", "working_dir":"/repo",
        "tmux_session":"sm-old", "provider":"opencode", "status":"running", "created_at":"now", "last_activity":"now"})).unwrap()
}

#[test]
fn loaded_model_state_and_identifier_authorize_admission() {
    let config = OpencodeConfig::default();
    assert!(loaded_config(&config, None, None).is_err());
    for state in ["loading", "draining", "yielded", "error"] {
        assert!(loaded_config(&config, Some(&model(state)), None).is_err());
    }
    assert!(loaded_config(&config, Some(&model("ready")), Some("wrong"))
        .unwrap_err()
        .to_string()
        .contains("loaded: actual-model"));
    let loaded = loaded_config(&config, Some(&model("ready")), Some("actual-model")).unwrap();
    assert_eq!(loaded.model_id, "actual-model");
    assert_eq!(loaded.model_base_url, "http://127.0.0.1:8000/v1");
    let mut loaded_model = model("ready");
    loaded_model.endpoint += "/v1/";
    assert_eq!(
        loaded_config(&config, Some(&loaded_model), None)
            .unwrap()
            .model_base_url,
        loaded.model_base_url
    );
}

#[test]
fn ordinary_second_launch_is_refused_and_authorized_handoff_keeps_the_seat() {
    let config = OpencodeConfig::default();
    check_seat(&config, &[], None).unwrap();
    let occupied = [seat()];
    assert!(check_seat(&config, &occupied, None)
        .unwrap_err()
        .to_string()
        .contains("1/1 used by local-owner"));
    check_seat(&config, &occupied, Some("old")).unwrap();
    assert!(check_seat(&config, &occupied, Some("forged")).is_err());
}

#[test]
fn port_selection_excludes_listeners_and_persisted_reservations() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = OpencodeConfig {
        port_range: [port, port],
        ..OpencodeConfig::default()
    };
    assert!(lowest_port(&config, &BTreeSet::new()).is_err());
    drop(listener);
    assert!(lowest_port(&config, &BTreeSet::from([port])).is_err());
    assert_eq!(lowest_port(&config, &BTreeSet::new()).unwrap(), port);
}

#[test]
fn preparation_writes_private_artifacts_and_reuses_the_password() {
    let fixture = Fixture::new();
    let files = fixture.files();
    files.binding.validate().unwrap();
    let state = Path::new(&files.binding.state_dir);
    for name in [
        "server.secret",
        "xdg/config/opencode/opencode.json",
        "xdg/config/opencode/plugins/sm_judge.js",
        "xdg/config/opencode/node_modules/@opencode-ai/plugin/index.js",
        "xdg/config/opencode/package.json",
        "xdg/config/opencode/bun.lock",
    ] {
        let metadata = fs::symlink_metadata(state.join(name)).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600, "{name}");
        assert_eq!(metadata.nlink(), 1);
    }
    assert_eq!(files.password.len(), 64);
    let rendered: Value =
        serde_json::from_slice(&fs::read(state.join("xdg/config/opencode/opencode.json")).unwrap())
            .unwrap();
    assert_eq!(rendered["model"], "local/qwen3.8-flash-next");
    assert_eq!(rendered["permission"]["webfetch"], "allow");
    let sdk = expand_home(&fixture.config.state_root).join("plugin-sdk/1.17.9");
    assert_eq!(
        fs::read_to_string(sdk.join("state/prepared"))
            .unwrap()
            .trim(),
        sdk.join("home").display().to_string()
    );
    fs::write(sdk.join("state/prepared"), "cached").unwrap();
    let reopened = fixture.files();
    assert_eq!(files.password, reopened.password);
    assert_eq!(
        fs::read_to_string(sdk.join("state/prepared")).unwrap(),
        "cached"
    );
}

#[test]
fn wrong_version_and_bad_id_fail_before_creating_agent_state() {
    let mut fixture = Fixture::new();
    fixture.config.version = "1.17.8".into();
    assert!(LaunchFiles::prepare(&fixture.config, "local-test", 18503)
        .unwrap_err_display()
        .contains("sm is pinned"));
    assert!(!expand_home(&fixture.config.state_root).exists());
    assert!(LaunchFiles::prepare(&fixture.config, "../escape", 18503).is_err());
}

#[test]
fn private_password_and_host_artifacts_reject_aliases() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let secret = Path::new(&files.binding.state_dir).join("server.secret");
    fs::hard_link(&secret, fixture.root.join("alias")).unwrap();
    assert!(password(&secret).is_err());
    assert!(fixture.files_result().is_err());
    fs::remove_file(&secret).unwrap();
    std::os::unix::fs::symlink(fixture.root.join("alias"), &secret).unwrap();
    assert!(password(&secret).is_err());
    assert_eq!(fs::metadata(fixture.root.join("alias")).unwrap().len(), 64);
}

#[test]
fn library_copy_materializes_internal_links_and_refuses_escape() {
    let fixture = Fixture::new();
    let source = fixture.root.join("sdk");
    let target = fixture.root.join("copy");
    private_directory(&source.join("node_modules/pkg")).unwrap();
    private_directory(&target).unwrap();
    host_write(&source.join("package.json"), b"{}", 0o600).unwrap();
    host_write(&source.join("node_modules/pkg/index.js"), b"fixture", 0o600).unwrap();
    std::os::unix::fs::symlink("pkg/index.js", source.join("node_modules/alias.js")).unwrap();
    copy_sdk(&source, &target).unwrap();
    assert!(!target.join("node_modules/alias.js").is_symlink());
    assert_eq!(
        fs::metadata(target.join("node_modules/alias.js"))
            .unwrap()
            .nlink(),
        1
    );
    std::os::unix::fs::symlink(&fixture.config.binary, source.join("node_modules/escape")).unwrap();
    assert!(copy_sdk(&source, &target).is_err());
}

#[test]
fn initial_brief_retries_after_lost_reply_without_appending_again() {
    let stub = Stub::new();
    stub.lose_post_reply();
    let fixture = Fixture::new();
    let mut files = fixture.files();
    files.binding.port = stub.port;
    files.password = "secret".into();
    let binding = MessageBinding::new("ses_native").unwrap();
    files
        .deliver_brief(&binding, "initial brief", Duration::from_secs(3))
        .unwrap();
    files
        .deliver_brief(&binding, "initial brief", Duration::from_secs(1))
        .unwrap();
    assert_eq!(stub.posts(), 1);
}

#[test]
fn stage_binds_the_real_durable_owner_and_explicit_provider_environment() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let queue = fixture.root.join("queue");
    private_directory(&queue).unwrap();
    let installed = fixture.root.join("installed-owner");
    host_write(&installed, b"#!/bin/sh\nexit 0\n", 0o500).unwrap();
    let app_config = crate::config::AppConfig::default();
    let walls = GenerationWalls::new(
        queue.clone(),
        PathBuf::from("/usr/bin/python3"),
        "127.0.0.1:8420".parse().unwrap(),
        "http://127.0.0.1:8000",
        crate::local_egress::ServiceClient::new(fixture.root.join("egress"), installed.clone()),
        crate::local_judge::LocalJudgeRuntime::isolated(&app_config, fixture.root.join("judge")),
    )
    .unwrap();
    let checkout = fixture.root.join("home/checkout");
    private_directory(&checkout).unwrap();
    let host = HostConfiguration {
        home: fixture.root.join("home"),
        state_root: expand_home(&fixture.config.state_root),
        alias_root: PathBuf::from("/private/tmp/oc-test-alias"),
        python: PathBuf::from("/usr/bin/python3"),
        read_only_roots: vec![],
        executable_roots: vec![],
        control_ports: 18500..=18599,
        model_port: 8000,
        sm_upstream: "127.0.0.1:8420".parse().unwrap(),
    };
    let registration = || AgentRegistration {
        id: "local-test".into(),
        name: "local-test".into(),
        ticket: 2079,
        title: "launch driver".into(),
        branch: "fixture".into(),
        checkout: checkout.clone(),
        parent: "host".into(),
        control_port: 18503,
        tools: vec![],
    };
    files
        .stage(
            &walls,
            host.clone(),
            registration(),
            &installed,
            "credential",
        )
        .unwrap();
    // Repeated staging must preserve the owner's exact immutable launch.
    files
        .stage(&walls, host, registration(), &installed, "credential")
        .unwrap();
    let launch: Value = serde_json::from_slice(
        &fs::read(queue.join("local-wall-owners/local-test/launch.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(launch["provider"]["tool"], "opencode");
    let provider: ProviderLaunch = serde_json::from_value(launch["provider"].clone()).unwrap();
    assert_eq!(
        provider.arguments,
        [
            "serve",
            "--port",
            "18503",
            "--hostname",
            "127.0.0.1",
            "--print-logs",
            "--log-level",
            "INFO"
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>()
    );
    let settings = launch["provider"]["settings"].as_object().unwrap();
    assert_eq!(settings["OPENCODE_SERVER_PASSWORD"], files.password);
    assert_eq!(settings["SM_SESSION_CREDENTIAL"], "credential");
    assert_eq!(settings["OPENCODE_ENABLE_EXA"], "1");
    assert!(settings.keys().all(|key| !key.starts_with("LOCAL_")
        && !key.starts_with("ANTHROPIC_")
        && !key.starts_with("OPENAI_")));
    let script = fs::read_to_string(&files.serve_script).unwrap();
    assert!(script.contains("--local-wall-owner"));
    assert!(!script.contains(&files.password));
    assert!(!script.contains("credential"));
    assert_eq!(
        fs::metadata(&files.serve_script).unwrap().mode() & 0o777,
        0o700
    );
    let tool = Path::new(launch["agent"]["tools"][0]["source"].as_str().unwrap());
    assert!(tool.starts_with(expand_home(&fixture.config.state_root).join("provider-binaries")));
    assert_eq!(fs::metadata(tool).unwrap().nlink(), 1);
}

#[test]
fn attach_view_uses_only_password_and_terminal_environment() {
    let fixture = Fixture::new();
    let files = fixture.files();
    let script = attach_script(&fixture.config, &files.binding, "ses_native");
    assert!(script.contains("/usr/bin/env -i TERM="));
    assert!(script.contains("--session 'ses_native'"));
    assert!(script.contains("/bin/sleep 2"));
    for forbidden in [
        "SM_SESSION_CREDENTIAL",
        "LOCAL_JUDGE",
        "ANTHROPIC_",
        "OPENAI_",
        "--pure",
    ] {
        assert!(!script.contains(forbidden));
    }
}

#[test]
fn serve_script_launches_only_the_owner_and_stops_it_on_signal() {
    let fixture = Fixture::new();
    let state = fixture.root.join("state");
    private_directory(&state).unwrap();
    let owner = fixture.root.join("owner with ' quote");
    let stopped = fixture.root.join("stopped");
    let counter = fixture.root.join("starts");
    host_write(&owner, format!("#!/bin/bash\ntrap 'echo stopped > {}; exit 0' TERM\necho start >> {}\nwhile :; do /bin/sleep 0.05; done\n", path_quote(&stopped).unwrap(), path_quote(&counter).unwrap()).as_bytes(), 0o700).unwrap();
    let script = serve_script(
        &owner,
        &["--local-wall-owner".into(), "private launch.json".into()],
        &state,
    )
    .unwrap();
    assert!(script.contains("now - started < 600"));
    assert!(script.contains(">= 5"));
    assert!(script.contains("/bin/sleep 5"));
    assert!(!script.contains("opencode serve"));
    let path = state.join("launch-serve.sh");
    host_write(&path, script.as_bytes(), 0o700).unwrap();
    let mut child = Command::new("/bin/bash").arg(&path).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !counter.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let ready = counter.exists();
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let status = child.wait().unwrap();
    assert!(ready && status.success());
    assert!(stopped.exists());
    assert_eq!(fs::read_to_string(counter).unwrap().lines().count(), 1);
}

// Avoid requiring Debug on a type carrying a password just for error tests.
trait ErrorText {
    fn unwrap_err_display(self) -> String;
}
impl<T> ErrorText for Result<T> {
    fn unwrap_err_display(self) -> String {
        match self {
            Ok(_) => panic!("expected an error"),
            Err(e) => format!("{e:#}"),
        }
    }
}
impl Fixture {
    fn files_result(&self) -> Result<LaunchFiles> {
        LaunchFiles::prepare(&self.config, "local-test", 18503)
    }
}
