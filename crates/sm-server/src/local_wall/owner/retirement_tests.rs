use super::*;
use crate::opencode::tests::ScratchDir;

struct Fixture {
    _tmp: ScratchDir,
    config: Configuration,
    installed: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let tmp = ScratchDir::new();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let state = home.join("state");
        let checkout = home.join("checkout");
        for path in [
            &state.join("agent/xdg/config"),
            &checkout,
            &root.join("queue"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let installed = root.join("installed");
        atomic_write(&installed, b"owner fixture").unwrap();
        let app = crate::config::AppConfig::default();
        let config = Configuration {
            host: HostConfiguration {
                home,
                state_root: state,
                alias_root: root.join("aliases"),
                python: "/usr/bin/python3".into(),
                read_only_roots: vec![],
                executable_roots: vec![],
                control_ports: 18500..=18599,
                model_port: 8000,
                sm_upstream: "127.0.0.1:8420".parse().unwrap(),
            },
            agent: AgentRegistration {
                id: "agent".into(),
                name: "agent".into(),
                ticket: 2086,
                title: "restore".into(),
                branch: "fixture".into(),
                checkout,
                parent: "host".into(),
                control_port: 18503,
                tools: vec![],
            },
            queue: root.join("queue"),
            egress: ServiceClient::new(root.join("egress"), installed.clone()),
            judge: LocalJudgeRuntime::isolated(&app, root.join("judge")),
            provider: ProviderLaunch {
                tool: "provider".into(),
                arguments: vec![],
                settings: BTreeMap::from([("SM_SESSION_CREDENTIAL".into(), "old".into())]),
            },
        };
        let fixture = Self {
            _tmp: tmp,
            config,
            installed,
        };
        fixture.stage();
        fixture
    }
    fn stage(&self) {
        // Exercise the production stage API, including its immutable comparison.
        let cloned: Configuration =
            serde_json::from_value(serde_json::to_value(&self.config).unwrap()).unwrap();
        super::super::stage(
            &cloned.queue,
            cloned.host,
            cloned.agent,
            cloned.egress,
            cloned.judge,
            cloned.provider,
            &self.installed,
        )
        .unwrap();
    }
    fn reset(&self) -> Result<()> {
        retire_for_restaging(&self.config.queue, &self.config.agent.id)
    }
    fn owner_root(&self) -> PathBuf {
        directory(&self.config.queue, "agent").unwrap()
    }
    fn queued(&self, state: &str) {
        crate::queue::local_wall::ensure_no_running_jobs(&self.config.queue, "agent").unwrap();
        rusqlite::Connection::open(self.config.queue.join("queue_runner.db")).unwrap().execute(
            "INSERT INTO queue_jobs (id,type,label,notify_session_id,cwd,env_json,timeout_seconds,state,queued_at,local_agent_id) VALUES ('job','tests','old','agent','/repo','{}',60,?1,'2026-10-07T00:00:00Z','agent')", [state]).unwrap();
    }
}

#[test]
fn owner_restage_requires_clean_retirement_and_preserves_conversation_files() {
    let mut fixture = Fixture::new();
    let conversation = fixture.config.host.state_root.join("agent/conversation.db");
    fs::write(&conversation, b"conversation and diagnostics").unwrap();
    record_started(&fixture.config).unwrap();
    assert!(fixture.reset().is_err());
    record_retired(&fixture.config).unwrap();
    fixture.reset().unwrap();
    assert!(!fixture.owner_root().join("launch.json").exists());
    assert_eq!(
        fs::read(conversation).unwrap(),
        b"conversation and diagnostics"
    );
    fixture.config.agent.control_port = 18504;
    fixture
        .config
        .provider
        .settings
        .insert("SM_SESSION_CREDENTIAL".into(), "new".into());
    fixture.stage();
    let saved = read_configuration(&fixture.owner_root().join("launch.json")).unwrap();
    assert_eq!(saved.agent.control_port, 18504);
    assert_eq!(saved.provider.settings["SM_SESSION_CREDENTIAL"], "new");
}

#[test]
fn owner_restage_refuses_live_owner_and_generation_preparation_even_with_a_receipt() {
    let fixture = Fixture::new();
    record_started(&fixture.config).unwrap();
    record_retired(&fixture.config).unwrap();
    let owner = stage_lock(&fixture.owner_root(), libc::LOCK_SH | libc::LOCK_NB).unwrap();
    assert!(fixture.reset().is_err());
    drop(owner);
    let lock_path = fixture
        .config
        .host
        .state_root
        .join("agent/xdg/config/host.lock");
    let preparation = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(preparation.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(fixture.reset().is_err());
    drop(preparation);
    fixture.reset().unwrap();
}

#[test]
fn owner_restage_refuses_pending_and_running_jobs_without_erasing_proof() {
    for state in ["pending", "running"] {
        let fixture = Fixture::new();
        record_started(&fixture.config).unwrap();
        record_retired(&fixture.config).unwrap();
        fixture.queued(state);
        assert!(fixture.reset().is_err());
        assert!(fixture.owner_root().join("launch.json").exists());
        assert!(fixture.owner_root().join("retired.json").exists());
    }
}

#[test]
fn owner_restage_accepts_never_started_launch_and_refuses_legacy_or_wrong_receipts() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.owner_root().join("staged.json")).unwrap();
    assert!(fixture.reset().is_err());
    record_staged(&fixture.config).unwrap();
    let mut wrong = lifecycle(&fixture.config).unwrap();
    wrong.configuration_sha256 = "wrong".into();
    atomic_write(
        &fixture.owner_root().join("retired.json"),
        &serde_json::to_vec(&wrong).unwrap(),
    )
    .unwrap();
    assert!(fixture.reset().is_err());
    fs::remove_file(fixture.owner_root().join("retired.json")).unwrap();
    fixture.reset().unwrap();
}

#[test]
fn owner_restage_accepts_earlier_kernel_boot_but_not_same_boot_or_unknown_boot() {
    let fixture = Fixture::new();
    record_started(&fixture.config).unwrap();
    let Some(boot) = crate::host_restart::system_boot_time() else {
        return;
    };
    let mut old = lifecycle(&fixture.config).unwrap();
    old.boot_seconds = None;
    atomic_write(
        &fixture.owner_root().join("started.json"),
        &serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    assert!(fixture.reset().is_err());
    old.boot_seconds = Some(boot.unix_timestamp());
    atomic_write(
        &fixture.owner_root().join("started.json"),
        &serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    assert!(fixture.reset().is_err());
    old.boot_seconds = Some(boot.unix_timestamp() - 1);
    atomic_write(
        &fixture.owner_root().join("started.json"),
        &serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    fixture.reset().unwrap();
}
