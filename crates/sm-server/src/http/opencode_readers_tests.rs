use super::*;
use crate::opencode::tests::ScratchDir;
use base64::Engine;
use std::{
    collections::VecDeque,
    io::Write,
    net::{TcpListener, TcpStream},
};

struct WireState {
    history: VecDeque<Vec<Value>>,
    statuses: VecDeque<Value>,
    events: Vec<Value>,
    requests: Vec<String>,
}
struct Wire {
    port: u16,
    state: Arc<Mutex<WireState>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Wire {
    fn new(history: Vec<Vec<Value>>, statuses: Vec<Value>, events: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(WireState {
            history: history.into(),
            statuses: statuses.into(),
            events,
            requests: Vec::new(),
        }));
        let stopped = Arc::new(AtomicBool::new(false));
        let (server_state, server_stop) = (state.clone(), stopped.clone());
        let worker = thread::spawn(move || {
            for socket in listener.incoming() {
                if server_stop.load(Ordering::Acquire) {
                    break;
                }
                let mut socket = socket.unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = String::new();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                    request.push_str(&line);
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let authorization = base64::engine::general_purpose::STANDARD
                    .encode(format!("opencode:{}", "a".repeat(64)));
                assert!(request.to_lowercase().contains(&format!(
                    "authorization: basic {}",
                    authorization.to_lowercase()
                )));
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                let mut state = server_state.lock().unwrap();
                state.requests.push(path.clone());
                let (kind, body) = match path.as_str() {
                    "/event" => (
                        "text/event-stream",
                        state
                            .events
                            .iter()
                            .map(|event| format!("data: {event}\n\n"))
                            .collect::<String>(),
                    ),
                    "/session/status" => {
                        let status = if state.statuses.len() > 1 {
                            state.statuses.pop_front().unwrap()
                        } else {
                            state.statuses.front().unwrap().clone()
                        };
                        ("application/json", status.to_string())
                    }
                    path if path.ends_with("/message") => {
                        let history = if state.history.len() > 1 {
                            state.history.pop_front().unwrap()
                        } else {
                            state.history.front().unwrap().clone()
                        };
                        ("application/json", serde_json::to_string(&history).unwrap())
                    }
                    _ => panic!("unexpected reader request {path}"),
                };
                drop(state);
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            port,
            state,
            stopped,
            worker: Some(worker),
        }
    }
}
impl Drop for Wire {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        self.worker.take().unwrap().join().unwrap();
    }
}
struct Fixture {
    state: Arc<AppState>,
    runtime: tokio::runtime::Runtime,
    root: PathBuf,
    _tmp: ScratchDir,
}
impl Fixture {
    fn new(port: u16) -> Self {
        let tmp = ScratchDir::new();
        let root = tmp.path().canonicalize().unwrap();
        let agent = root.join("opencode/agent");
        fs::create_dir_all(agent.join("xdg/config/opencode")).unwrap();
        private_write(&agent.join("server.secret"), "a".repeat(64).as_bytes());
        private_write(&agent.join("xdg/config/opencode/opencode.json"), br#"{"model":"local/fixture","provider":{"local":{"models":{"fixture":{"limit":{"context":4096,"output":1024}}}}}}"#);
        let mut config = AppConfig::default();
        config.paths.state_file = root.join("sessions.json").display().to_string();
        config.sm_send.db_path = root.join("queue.db").display().to_string();
        config.tool_logging.db_path = root.join("tools.db").display().to_string();
        config.opencode.state_root = root.join("opencode").display().to_string();
        config.rust_core.runtime_enabled = false;
        config.rust_core.fixture_writes_enabled = true;
        config.usage.enabled = false;
        let record = json!({"id":"agent","name":"agent","working_dir":"/repo","tmux_session":"sm-agent", "provider":"opencode","status":"running","provider_resume_id":"ses_test","node":"primary", "created_at":"2026-10-07T00:00:00Z","last_activity":"2026-10-07T00:00:01Z", "opencode":{"port":port,"state_dir":agent,"version":"1.17.9","model_base_url":"http://127.0.0.1:8000/v1"}});
        fs::write(
            &config.paths.state_file,
            serde_json::to_vec(&json!({"sessions":[record]})).unwrap(),
        )
        .unwrap();
        let state = Arc::new(AppState::try_new(config).unwrap());
        state
            .opencode_readers
            .workers
            .lock()
            .unwrap()
            .insert("agent".into(), ReaderState::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        Self {
            state,
            runtime,
            root,
            _tmp: tmp,
        }
    }
    fn session(&self) -> SessionRecord {
        self.state
            .session_store
            .get_session("agent")
            .unwrap()
            .unwrap()
    }
    fn read(&self) -> Result<()> {
        read_connection(
            &self.state,
            &self.state.opencode_readers,
            self.runtime.handle(),
            "agent",
        )
    }
}
fn private_write(path: &StdPath, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn history(complete: bool, text: &str) -> Vec<Value> {
    let mut reply = json!({"info":{"id":"msg_reply","sessionID":"ses_test","role":"assistant","parentID":"msg_user","time":{"created":20}},"parts":[{"id":"prt_text","type":"text","sessionID":"ses_test","messageID":"msg_reply","text":text}]});
    if complete {
        reply["info"]["time"]["completed"] = json!(30);
        reply["info"]["finish"] = json!("stop");
    }
    vec![
        json!({"info":{"id":"msg_user","sessionID":"ses_test","role":"user","time":{"created":10}},"parts":[{"id":"prt_prompt","type":"text","sessionID":"ses_test","messageID":"msg_user","text":"brief"}]}),
        reply,
    ]
}
fn status(kind: &str) -> Value {
    json!({"ses_test":{"type":kind}})
}
fn status_event(kind: &str) -> Value {
    json!({"type":"session.status","properties":{"sessionID":"ses_test","status":{"type":kind}}})
}

#[test]
fn opencode_reader_backfills_before_buffered_frames_and_reconnects_without_duplicate_turns() {
    let wire = Wire::new(
        vec![history(true, "complete reply")],
        vec![status("idle")],
        vec![
            status_event("busy"),
            json!({"type":"message.part.updated","properties":{"part":{"id":"prt_text","type":"text","sessionID":"ses_test","messageID":"msg_reply","text":"stale partial"}}}),
        ],
    );
    let fixture = Fixture::new(wire.port);
    fixture.read().unwrap();
    fixture.read().unwrap();
    assert_eq!(fixture.session().status, "idle");
    assert_eq!(fixture.session().turns_completed, 1);
    assert_eq!(
        fixture.state.opencode_readers.activity(&fixture.session()),
        Some("idle")
    );
    assert_eq!(
        fixture
            .state
            .session_store
            .turn_message_store()
            .unwrap()
            .last_turn("agent")
            .unwrap()
            .unwrap()
            .text,
        "complete reply"
    );
    assert!(fixture
        .state
        .session_store
        .opencode_pending_stop_signal("agent")
        .unwrap()
        .is_none());
    let requests = &wire.state.lock().unwrap().requests;
    assert_eq!(
        &requests[..4],
        [
            "/event",
            "/session/status",
            "/session/ses_test/message",
            "/session/status"
        ]
    );
}

#[test]
fn opencode_reader_idle_refreshes_complete_history_instead_of_stopping_with_partial_text() {
    let wire = Wire::new(
        vec![history(false, "partial"), history(true, "full reply")],
        vec![status("busy"), status("idle")],
        vec![status_event("idle")],
    );
    let fixture = Fixture::new(wire.port);
    fixture.read().unwrap();
    assert_eq!(fixture.session().turns_completed, 1);
    assert_eq!(
        fixture
            .state
            .session_store
            .turn_message_store()
            .unwrap()
            .last_turn("agent")
            .unwrap()
            .unwrap()
            .text,
        "full reply"
    );
}

#[test]
fn opencode_reader_unavailable_provider_keeps_session_running_and_shutdown_stops_before_apply() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let fixture = Fixture::new(port);
    assert!(fixture.read().is_err());
    assert_eq!(fixture.session().status, "running");
    assert_eq!(
        fixture.state.opencode_readers.activity(&fixture.session()),
        None
    );
    fixture.state.shutdown.stop();
    assert!(!continue_reading(&fixture.state, "agent").unwrap());
}

#[test]
fn opencode_reader_activity_expires_and_never_crosses_conversations() {
    let fixture = Fixture::new(18501);
    let readers = &fixture.state.opencode_readers;
    readers
        .update(&fixture.state.session_store, "agent")
        .unwrap();
    let mut session = fixture.session();
    assert_eq!(readers.activity(&session), Some("working"));
    session.provider_resume_id = Some("ses_new".into());
    assert_eq!(readers.activity(&session), None);
    readers
        .workers
        .lock()
        .unwrap()
        .get_mut("agent")
        .unwrap()
        .connected_at = Some(Instant::now() - Duration::from_secs(61));
    assert_eq!(readers.activity(&fixture.session()), None);
}

#[test]
fn opencode_reader_uses_persisted_limits_and_rejects_public_or_aliased_secret() {
    let fixture = Fixture::new(18501);
    let (_, config) = client_and_config(&fixture.state, &fixture.session()).unwrap();
    assert_eq!(config.context_window, 4096);
    assert_eq!(config.output_limit, 1024);
    assert_eq!(config.model_id, "fixture");
    let secret = fixture.root.join("opencode/agent/server.secret");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(client_and_config(&fixture.state, &fixture.session()).is_err());
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&secret, secret.with_extension("alias")).unwrap();
    assert!(client_and_config(&fixture.state, &fixture.session()).is_err());
}

#[test]
fn opencode_reader_shutdown_releases_single_worker_claim() {
    let fixture = Fixture::new(18501);
    fixture
        .state
        .opencode_readers
        .workers
        .lock()
        .unwrap()
        .clear();
    fixture.state.shutdown.stop();
    fixture
        .state
        .opencode_readers
        .start_session(&fixture.state, fixture.runtime.handle(), "agent".into())
        .unwrap();
    fixture
        .state
        .opencode_readers
        .start_session(&fixture.state, fixture.runtime.handle(), "agent".into())
        .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while !fixture
        .state
        .opencode_readers
        .workers
        .lock()
        .unwrap()
        .is_empty()
        && Instant::now() < until
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fixture
        .state
        .opencode_readers
        .workers
        .lock()
        .unwrap()
        .is_empty());
}
