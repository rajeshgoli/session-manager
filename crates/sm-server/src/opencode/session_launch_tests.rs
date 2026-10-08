use super::*;
use crate::opencode::tests::{ScratchDir, Stub};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

type Hook = Box<dyn FnOnce(&SessionRecord) + Send>;
struct Driver {
    config: OpencodeConfig,
    path: PathBuf,
    server: Mutex<Option<Stub>>,
    starts: AtomicUsize,
    attachments: AtomicUsize,
    old_posts: AtomicUsize,
    old_conversations: AtomicUsize,
    ready: AtomicBool,
    fail_start: AtomicBool,
    fail_stop: AtomicBool,
    uncertain_present: AtomicBool,
    lost_reply: AtomicBool,
    start_hook: Mutex<Option<Hook>>,
    attach_hook: Mutex<Option<Hook>>,
    stop_hook: Mutex<Option<Hook>>,
}
impl Driver {
    fn posts(&self) -> usize {
        self.old_posts.load(Ordering::Acquire)
            + self.server.lock().unwrap().as_ref().map_or(0, Stub::posts)
    }
    fn conversations(&self) -> usize {
        self.old_conversations.load(Ordering::Acquire)
            + self
                .server
                .lock()
                .unwrap()
                .as_ref()
                .map_or(0, Stub::conversation_creations)
    }
}
impl OpencodeLaunchDriver for Driver {
    fn base_config(&self) -> OpencodeConfig {
        self.config.clone()
    }
    fn config(&self, requested: Option<&str>) -> Result<OpencodeConfig> {
        if !self.ready.load(Ordering::Acquire) {
            anyhow::bail!("no local model loaded")
        }
        if requested.is_some_and(|model| model != self.config.model_id) {
            anyhow::bail!("requested model is not loaded")
        }
        Ok(self.config.clone())
    }
    fn binding(&self, config: &OpencodeConfig, id: &str, port: u16) -> Result<RuntimeBinding> {
        Ok(RuntimeBinding {
            port,
            state_dir: format!("{}/{id}", config.state_root),
            version: config.version.clone(),
            model_base_url: config.model_base_url.clone(),
        })
    }
    fn start(
        &self,
        _: &OpencodeConfig,
        record: &SessionRecord,
        credential: &str,
        _: &TmuxRuntime,
    ) -> Result<()> {
        let raw: Value = serde_json::from_slice(&fs::read(&self.path)?)?;
        let saved = raw_session_object(&raw, &record.id).unwrap();
        assert_eq!(saved["status"], "starting");
        assert_eq!(saved["host"], "local");
        assert_eq!(saved["session_credential_sha256"], sha256_text(credential));
        assert!(session_runtime_launch_records(&raw)?
            .iter()
            .any(|launch| launch.session_id == record.id && launch.status == "launching"));
        self.starts.fetch_add(1, Ordering::AcqRel);
        let server = Stub::new_on_port(record.opencode.as_ref().unwrap().port);
        if self.lost_reply.swap(false, Ordering::AcqRel) {
            server.lose_post_reply();
        }
        *self.server.lock().unwrap() = Some(server);
        if let Some(hook) = self.start_hook.lock().unwrap().take() {
            hook(record);
        }
        if self.fail_start.load(Ordering::Acquire) {
            anyhow::bail!("injected native launch failure")
        }
        Ok(())
    }
    fn client(&self, binding: &RuntimeBinding) -> Result<Client> {
        Client::new(binding.port, "secret", Duration::from_secs(1))
    }
    fn attach(&self, record: &SessionRecord, _: &TmuxRuntime) -> Result<()> {
        self.attachments.fetch_add(1, Ordering::AcqRel);
        let raw: Value = serde_json::from_slice(&fs::read(&self.path)?)?;
        let launches = session_runtime_launch_records(&raw)?;
        let saved = launches
            .iter()
            .rfind(|launch| launch.session_id == record.id)
            .unwrap();
        assert_eq!(saved.provider_resume_id, record.provider_resume_id);
        assert!(record.provider_resume_id.is_some());
        if saved
            .initial_message
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        {
            assert!(saved.brief_message_id.is_some() && saved.brief_part_id.is_some());
        }
        if let Some(hook) = self.attach_hook.lock().unwrap().take() {
            hook(record);
        }
        Ok(())
    }
    fn present(&self, _: &SessionRecord, _: &TmuxRuntime) -> Result<bool> {
        if self.uncertain_present.load(Ordering::Acquire) {
            anyhow::bail!("injected tmux transport uncertainty")
        }
        Ok(self.server.lock().unwrap().is_some())
    }
    fn stop(&self, record: &SessionRecord, _: &TmuxRuntime) -> Result<()> {
        if self.fail_stop.load(Ordering::Acquire) {
            anyhow::bail!("teardown not proved")
        }
        if let Some(server) = self.server.lock().unwrap().take() {
            self.old_posts.fetch_add(server.posts(), Ordering::AcqRel);
            self.old_conversations
                .fetch_add(server.conversation_creations(), Ordering::AcqRel);
        }
        if let Some(hook) = self.stop_hook.lock().unwrap().take() {
            hook(record);
        }
        Ok(())
    }
}
struct Fixture {
    _scratch: ScratchDir,
    store: SessionStore,
    driver: Arc<Driver>,
    runtime: TmuxRuntime,
}
impl Fixture {
    fn new() -> Self {
        let scratch = ScratchDir::new();
        let path = scratch.path().join("sessions.json");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let driver = Arc::new(Driver {
            config: OpencodeConfig {
                port_range: [port, port],
                state_root: scratch.path().join("opencode").display().to_string(),
                ..OpencodeConfig::default()
            },
            path: path.clone(),
            server: Mutex::new(None),
            starts: AtomicUsize::new(0),
            attachments: AtomicUsize::new(0),
            old_posts: AtomicUsize::new(0),
            old_conversations: AtomicUsize::new(0),
            ready: AtomicBool::new(true),
            fail_start: AtomicBool::new(false),
            fail_stop: AtomicBool::new(false),
            uncertain_present: AtomicBool::new(false),
            lost_reply: AtomicBool::new(false),
            start_hook: Mutex::new(None),
            attach_hook: Mutex::new(None),
            stop_hook: Mutex::new(None),
        });
        let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default());
        let store = SessionStore::new(path)
            .with_delivery_runtime(Some(runtime.clone()))
            .with_opencode_launch_driver(driver.clone());
        Self {
            _scratch: scratch,
            store,
            driver,
            runtime,
        }
    }
    fn request(id: &str, brief: Option<&str>) -> CreateCoreSessionRequest {
        CreateCoreSessionRequest {
            id: Some(id.into()),
            provider: Some("opencode".into()),
            initial_message: brief.map(str::to_owned),
            ..CreateCoreSessionRequest::default()
        }
    }
    fn create(&self, id: &str, brief: Option<&str>) -> Result<SessionRecord> {
        self.store
            .create_core_session_with_runtime(Self::request(id, brief), None, &self.runtime)
    }
    fn change(&self, id: &str, field: &str, value: Value) {
        let _guard = self.store.write_guard().unwrap();
        let mut raw = self.store.load_raw_json_value().unwrap();
        session_object_mut(ensure_sessions_array_mut(&mut raw).unwrap(), id)
            .unwrap()
            .insert(field.into(), value);
        self.store.write_raw_json_value(&raw).unwrap();
    }
    fn launches(&self) -> Vec<SessionRuntimeLaunchRecord> {
        session_runtime_launch_records(&self.store.load_parsed_state().unwrap().raw).unwrap()
    }
}

#[test]
fn opencode_create_persists_authority_and_brief_before_http() {
    let f = Fixture::new();
    let subscribed = Arc::new(AtomicBool::new(false));
    let flag = subscribed.clone();
    let driver = f.driver.clone();
    f.store
        .register_opencode_reader_start(Arc::new(move |_| {
            assert_eq!(driver.posts(), 0);
            assert_eq!(driver.conversations(), 1);
            flag.store(true, Ordering::Release);
            Ok(())
        }))
        .unwrap();
    let record = f.create("local1", Some("begin ticket")).unwrap();
    assert!(subscribed.load(Ordering::Acquire));
    assert_eq!(record.provider_resume_id.as_deref(), Some("ses_test"));
    assert_eq!(record.host.as_deref(), Some("local"));
    assert_eq!(
        record.model.as_deref(),
        Some(f.driver.config.model_id.as_str())
    );
    assert_eq!(record.status, "running");
    assert_eq!(f.launches()[0].status, "applied");
    assert_eq!((f.driver.posts(), f.driver.conversations()), (1, 1));
    assert_eq!(f.driver.attachments.load(Ordering::Acquire), 1);
    f.store.recover_opencode_runtime_launches().unwrap();
    assert_eq!((f.driver.posts(), f.driver.conversations()), (1, 1));
}

#[test]
fn opencode_lost_brief_reply_recovers_saved_identity_without_reposting() {
    let f = Fixture::new();
    f.driver.lost_reply.store(true, Ordering::Release);
    assert!(f
        .create("local1", Some("one brief"))
        .unwrap_err()
        .to_string()
        .contains("unknown"));
    let pending = f.launches()[0].clone();
    assert_eq!(pending.status, "launching");
    assert_eq!(f.driver.posts(), 1);
    let restarted = SessionStore::new(f.driver.path.clone())
        .with_delivery_runtime(Some(f.runtime.clone()))
        .with_opencode_launch_driver(f.driver.clone());
    restarted.recover_opencode_runtime_launches().unwrap();
    let recovered =
        session_runtime_launch_records(&restarted.load_parsed_state().unwrap().raw).unwrap();
    assert_eq!(recovered[0].status, "applied");
    assert_eq!(recovered[0].brief_message_id, pending.brief_message_id);
    assert_eq!(recovered[0].brief_part_id, pending.brief_part_id);
    assert_eq!((f.driver.posts(), f.driver.conversations()), (1, 1));
    assert_eq!(f.driver.starts.load(Ordering::Acquire), 1);
}

#[test]
fn opencode_restore_reuses_conversation_rotates_credential_and_preserves_checkpoint() {
    let f = Fixture::new();
    let original = f.create("local1", Some("first brief")).unwrap();
    f.change("local1", "status", json!("stopped"));
    f.change(
        "local1",
        "opencode_checkpoint",
        json!({"version": 1, "marker": "retain"}),
    );
    let restored = f
        .store
        .restore_core_session_with_runtime("local1", &f.runtime)
        .unwrap()
        .unwrap();
    let CoreRestoreOutcome::Restored(restored) = restored else {
        panic!("restore refused")
    };
    assert_eq!(restored.provider_resume_id, original.provider_resume_id);
    assert_eq!(restored.status, "idle");
    assert_ne!(
        restored.session_credential_sha256,
        original.session_credential_sha256
    );
    assert_eq!(
        f.store.load_parsed_state().unwrap().raw["sessions"][0]["opencode_checkpoint"]["marker"],
        "retain"
    );
    assert_eq!((f.driver.posts(), f.driver.conversations()), (1, 1));
    assert_eq!(f.driver.starts.load(Ordering::Acquire), 2);
    assert_eq!(f.launches()[1].operation_kind, "restore");
    assert!(f.launches()[1].initial_message.is_none());
}

#[test]
fn opencode_retirement_during_restore_teardown_wins_without_new_launch_or_credential() {
    let f = Fixture::new();
    let original = f.create("local1", Some("first brief")).unwrap();
    f.change("local1", "status", json!("stopped"));
    let store = f.store.clone();
    *f.driver.stop_hook.lock().unwrap() = Some(Box::new(move |record| {
        assert!(matches!(
            store.retire_core_session(&record.id, None).unwrap(),
            CoreRetireOutcome::Retired(_)
        ));
    }));
    let error = f
        .store
        .restore_core_session_with_runtime("local1", &f.runtime)
        .unwrap_err();
    assert!(error.to_string().contains("lifecycle changed"));
    let retired = f.store.get_session("local1").unwrap().unwrap();
    assert_eq!(retired.completion_status.as_deref(), Some("retired"));
    assert_eq!(
        retired.session_credential_sha256,
        original.session_credential_sha256
    );
    assert!(retired.is_stopped());
    assert_eq!(f.launches().len(), 1);
    assert_eq!(f.driver.starts.load(Ordering::Acquire), 1);
    assert_eq!((f.driver.posts(), f.driver.conversations()), (1, 1));
}

#[test]
fn opencode_restore_clears_old_terminal_metadata_and_preserves_conversation() {
    let f = Fixture::new();
    let original = f.create("local1", None).unwrap();
    f.change("local1", "status", json!("stopped"));
    assert!(matches!(
        f.store.retire_core_session("local1", None).unwrap(),
        CoreRetireOutcome::Retired(_)
    ));
    f.change("local1", "completion_message", json!("old result"));
    f.change("local1", "completed_at", json!("2026-10-07T00:00:00Z"));
    f.change(
        "local1",
        "agent_task_completed_at",
        json!("2026-10-07T00:00:00Z"),
    );
    f.change("local1", "retirement_intent", json!({"old": true}));
    let CoreRestoreOutcome::Restored(restored) = f
        .store
        .restore_core_session_with_runtime("local1", &f.runtime)
        .unwrap()
        .unwrap()
    else {
        panic!("restore refused")
    };
    assert_eq!(restored.provider_resume_id, original.provider_resume_id);
    assert_eq!(restored.status, "idle");
    for field in [
        "completion_status",
        "completion_message",
        "completed_at",
        "stopped_at",
        "terminal_provenance",
        "retirement_intent",
        "agent_task_completed_at",
    ] {
        assert!(
            f.store.load_parsed_state().unwrap().raw["sessions"][0][field].is_null(),
            "{field} retained"
        );
    }
}

#[test]
fn opencode_admission_refuses_missing_or_different_model_before_persisting() {
    let f = Fixture::new();
    f.driver.ready.store(false, Ordering::Release);
    assert!(f
        .create("local1", None)
        .unwrap_err()
        .to_string()
        .contains("no local model"));
    f.driver.ready.store(true, Ordering::Release);
    let mut request = Fixture::request("local1", None);
    request.model = Some("other".into());
    assert!(f
        .store
        .create_core_session_with_runtime(request, None, &f.runtime)
        .is_err());
    assert!(f.store.list_sessions(true).unwrap().is_empty());
    assert!(f.launches().is_empty());
    assert_eq!(f.driver.starts.load(Ordering::Acquire), 0);
}

#[test]
fn opencode_failed_creation_removes_only_confirmed_teardown() {
    for uncertain in [false, true] {
        let f = Fixture::new();
        f.driver.fail_start.store(true, Ordering::Release);
        f.driver.fail_stop.store(uncertain, Ordering::Release);
        assert!(f.create("local1", None).is_err());
        assert_eq!(f.store.get_session("local1").unwrap().is_some(), uncertain);
        assert_eq!(
            f.launches()[0].status,
            if uncertain {
                "teardown_pending"
            } else {
                "failed"
            }
        );
        assert!(f.launches()[0]
            .failure_reason
            .as_deref()
            .unwrap()
            .contains(&f.driver.config.state_root));
        assert_eq!(f.driver.conversations(), 0);
    }
}

#[test]
fn opencode_unconfirmed_teardown_reserves_capacity_until_recovery_proves_stop() {
    for retired in [false, true] {
        let f = Fixture::new();
        f.driver.fail_start.store(true, Ordering::Release);
        f.driver.fail_stop.store(true, Ordering::Release);
        assert!(f.create("local1", None).is_err());
        assert!(!f.store.get_session("local1").unwrap().unwrap().is_stopped());
        if retired {
            f.store.retire_core_session("local1", None).unwrap();
        }
        // Exercise durable recovery using a reopened store, including a
        // retired record and a failure before any conversation is committed.
        let store = SessionStore::new(f.driver.path.clone())
            .with_delivery_runtime(Some(f.runtime.clone()))
            .with_opencode_launch_driver(f.driver.clone());
        store.recover_opencode_teardowns().unwrap();
        assert_eq!(f.launches()[0].status, "teardown_pending");
        f.store.reconcile_opencode_runtime("local1").unwrap();
        let error = f.create("local2", None).unwrap_err();
        assert!(
            error.to_string().contains("no local seat free"),
            "{error:#}"
        );
        assert_eq!(f.driver.starts.load(Ordering::Acquire), 1);
        f.driver.fail_stop.store(false, Ordering::Release);
        store.recover_opencode_teardowns().unwrap();
        assert_eq!(f.launches()[0].status, "failed");
        assert_eq!(f.store.get_session("local1").unwrap().is_some(), retired);
        assert_eq!((f.driver.posts(), f.driver.conversations()), (0, 0));
        f.driver.fail_start.store(false, Ordering::Release);
        f.create("local2", None).unwrap();
        assert_eq!(f.driver.starts.load(Ordering::Acquire), 2);
    }
}

#[test]
fn opencode_unknown_transport_preserves_pending_launch_and_runtime() {
    let f = Fixture::new();
    f.driver.lost_reply.store(true, Ordering::Release);
    assert!(f.create("local1", Some("brief")).is_err());
    f.driver.uncertain_present.store(true, Ordering::Release);
    assert!(f
        .store
        .recover_opencode_launch_for_session("local1")
        .is_err());
    f.store.reconcile_opencode_runtime("local1").unwrap();
    assert_eq!(f.launches()[0].status, "launching");
    assert!(!f.store.get_session("local1").unwrap().unwrap().is_stopped());
    f.driver.uncertain_present.store(false, Ordering::Release);
    f.store
        .recover_opencode_launch_for_session("local1")
        .unwrap();
    assert_eq!(f.driver.posts(), 1);
}

#[test]
fn opencode_pending_launch_without_committed_conversation_fails_without_recreating() {
    let f = Fixture::new();
    f.driver.lost_reply.store(true, Ordering::Release);
    assert!(f.create("local1", Some("brief")).is_err());
    {
        let _guard = f.store.write_guard().unwrap();
        let mut raw = f.store.load_raw_json_value().unwrap();
        let mut launches = session_runtime_launch_records(&raw).unwrap();
        launches[0].provider_resume_id = None;
        store_session_runtime_launch_records(&mut raw, &launches).unwrap();
        session_object_mut(ensure_sessions_array_mut(&mut raw).unwrap(), "local1")
            .unwrap()
            .insert("provider_resume_id".into(), Value::Null);
        f.store.write_raw_json_value(&raw).unwrap();
    }
    assert!(f.store.recover_opencode_runtime_launches().is_err());
    assert_eq!(f.launches()[0].status, "failed");
    assert!(f.store.get_session("local1").unwrap().is_none());
    assert_eq!((f.driver.posts(), f.driver.conversations()), (1, 1));
}

#[test]
fn opencode_stopped_pending_launch_still_reserves_seat_and_port() {
    let f = Fixture::new();
    f.driver.lost_reply.store(true, Ordering::Release);
    assert!(f.create("local1", Some("brief")).is_err());
    f.change("local1", "status", json!("stopped"));
    let occupied =
        occupied_opencode_sessions(&f.store.load_parsed_state().unwrap().raw, None).unwrap();
    assert_eq!(occupied.len(), 1);
    assert!(check_opencode_capacity(&f.driver.config, &occupied, None).is_err());
    assert!(reserve_opencode_port(&f.driver.config, &occupied).is_err());
    assert!(f.create("local2", None).is_err());
    assert_eq!(f.driver.starts.load(Ordering::Acquire), 1);
}

#[test]
fn opencode_reader_observation_survives_launch_acknowledgement() {
    let f = Fixture::new();
    let store = f.store.clone();
    *f.driver.attach_hook.lock().unwrap() = Some(Box::new(move |record| {
        let _guard = store.write_guard().unwrap();
        let mut raw = store.load_raw_json_value().unwrap();
        session_object_mut(ensure_sessions_array_mut(&mut raw).unwrap(), &record.id)
            .unwrap()
            .insert("status".into(), json!("idle"));
        store.write_raw_json_value(&raw).unwrap();
    }));
    assert_eq!(f.create("local1", Some("brief")).unwrap().status, "idle");
}

#[test]
fn opencode_terminal_fence_prevents_initial_brief_submission() {
    let f = Fixture::new();
    let store = f.store.clone();
    *f.driver.attach_hook.lock().unwrap() = Some(Box::new(move |record| {
        assert!(matches!(
            store.retire_core_session(&record.id, None).unwrap(),
            CoreRetireOutcome::Retired(_)
        ));
    }));
    assert!(f.create("local1", Some("must not submit")).is_err());
    assert_eq!(f.driver.posts(), 0);
    assert_eq!(
        f.store
            .get_session("local1")
            .unwrap()
            .unwrap()
            .completion_status
            .as_deref(),
        Some("retired")
    );
}

#[test]
fn opencode_retirement_waits_for_in_flight_brief_without_holding_registry_lock() {
    let f = Fixture::new();
    let (paused, pause) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let driver = f.driver.clone();
    *f.driver.start_hook.lock().unwrap() = Some(Box::new(move |_| {
        driver
            .server
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .before_lookup(Box::new(move || {
                paused.send(()).unwrap();
                released.recv().unwrap();
            }));
    }));
    let store = f.store.clone();
    let runtime = f.runtime.clone();
    let creator = std::thread::spawn(move || {
        store.create_core_session_with_runtime(
            Fixture::request("local1", Some("brief")),
            None,
            &runtime,
        )
    });
    pause.recv_timeout(Duration::from_secs(5)).unwrap();
    let store = f.store.clone();
    let (entered, entry) = std::sync::mpsc::channel();
    let (done, completion) = std::sync::mpsc::channel();
    let retirement = std::thread::spawn(move || {
        entered.send(()).unwrap();
        let result = store.retire_core_session("local1", None);
        done.send(result).unwrap();
    });
    entry.recv_timeout(Duration::from_secs(5)).unwrap();
    let prematurely_finished = completion.recv_timeout(Duration::from_millis(200)).is_ok();
    // This takes the registry lock while retirement is waiting on submission.
    f.change(
        "local1",
        "last_provider_error",
        json!("unrelated registry write"),
    );
    release.send(()).unwrap();
    creator.join().unwrap().unwrap();
    retirement.join().unwrap();
    assert!(
        !prematurely_finished,
        "retirement committed before provider submission completed"
    );
    assert_eq!(f.driver.posts(), 1);
    assert!(f.store.get_session("local1").unwrap().unwrap().is_retired());
    assert_eq!(f.launches()[0].status, "applied");
}

#[test]
fn opencode_restore_uses_free_name_when_live_session_reused_original() {
    let f = Fixture::new();
    let mut request = Fixture::request("local1", None);
    request.name = Some("vega".into());
    f.store
        .create_core_session_with_runtime(request, None, &f.runtime)
        .unwrap();
    f.change("local1", "status", json!("stopped"));
    f.change("local1", "stopped_at", json!("2026-01-01T00:00:00Z"));
    let mut other = Fixture::request("hosted2", None);
    other.provider = Some("claude".into());
    other.name = Some("vega".into());
    f.store.create_core_session(other, None).unwrap();
    let CoreRestoreOutcome::Restored(restored) = f
        .store
        .restore_core_session_with_runtime("local1", &f.runtime)
        .unwrap()
        .unwrap()
    else {
        panic!("restore refused")
    };
    assert_eq!(restored.name, "vega-2");
    assert_eq!(restored.friendly_name.as_deref(), Some("vega-2"));
    assert_eq!(f.store.get_session("vega").unwrap().unwrap().id, "hosted2");
    assert_eq!(f.store.get_session("vega-2").unwrap().unwrap().id, "local1");
    assert_eq!(f.driver.conversations(), 1);
}

#[test]
fn opencode_provisional_seat_blocks_concurrent_create_and_recovery_waits_for_creator() {
    let f = Fixture::new();
    let (published, waiting) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    *f.driver.start_hook.lock().unwrap() = Some(Box::new(move |_| {
        published.send(()).unwrap();
        released.recv().unwrap();
    }));
    let store = f.store.clone();
    let runtime = f.runtime.clone();
    let creator = std::thread::spawn(move || {
        store.create_core_session_with_runtime(
            Fixture::request("local1", Some("one")),
            None,
            &runtime,
        )
    });
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(f
        .create("local2", None)
        .unwrap_err()
        .to_string()
        .contains("no local seat free"));
    let store = f.store.clone();
    let recovery = std::thread::spawn(move || store.recover_opencode_launch_for_session("local1"));
    release.send(()).unwrap();
    creator.join().unwrap().unwrap();
    recovery.join().unwrap().unwrap();
    assert_eq!(f.launches()[0].status, "applied");
    assert_eq!((f.driver.posts(), f.driver.conversations()), (1, 1));
}

#[test]
fn opencode_absent_server_stops_session_with_saved_log_path() {
    let f = Fixture::new();
    let record = f.create("local1", None).unwrap();
    f.driver.stop(&record, &f.runtime).unwrap();
    f.store.reconcile_opencode_runtime("local1").unwrap();
    assert!(f.store.get_session("local1").unwrap().unwrap().is_stopped());
    assert!(
        f.store.load_parsed_state().unwrap().raw["sessions"][0]["error_message"]
            .as_str()
            .unwrap()
            .contains("serve.log")
    );
}

#[test]
fn opencode_pending_launch_is_not_stopped_before_its_server_is_started() {
    let f = Fixture::new();
    f.driver.lost_reply.store(true, Ordering::Release);
    assert!(f.create("local1", Some("brief")).is_err());
    let record = f.store.get_session("local1").unwrap().unwrap();
    f.driver.stop(&record, &f.runtime).unwrap();
    f.store.reconcile_opencode_runtime("local1").unwrap();
    assert!(!f.store.get_session("local1").unwrap().unwrap().is_stopped());
    assert_eq!(f.launches()[0].status, "launching");
}
