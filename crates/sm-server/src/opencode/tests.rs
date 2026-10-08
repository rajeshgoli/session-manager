use super::*;
pub(crate) struct ScratchDir(std::path::PathBuf);
impl ScratchDir {
    pub(crate) fn new() -> Self {
        Self::in_dir(&std::env::temp_dir())
    }
    fn in_dir(parent: &std::path::Path) -> Self {
        let path = parent.join(format!("oc-test-{:016x}", OsRng.next_u64()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[derive(Default)]
struct StubState {
    messages: BTreeMap<String, String>,
    requests: Vec<(String, String, Value)>,
    lookup_error: Option<u16>,
    hidden: usize,
    lose_post_reply: bool,
    rename_error: bool,
}

pub(crate) struct Stub {
    pub(crate) port: u16,
    state: Arc<Mutex<StubState>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Stub {
    pub(crate) fn new() -> Self {
        Self::new_on_port(0)
    }

    pub(crate) fn new_on_port(port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(StubState::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let (worker_state, worker_stopped) = (state.clone(), stopped.clone());
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                if worker_stopped.load(Ordering::Acquire) {
                    break;
                }
                handle(stream.unwrap(), &worker_state);
            }
        });
        Self {
            port,
            state,
            stopped,
            worker: Some(worker),
        }
    }

    fn client(&self) -> Client {
        Client::new(self.port, "secret", Duration::from_secs(1)).unwrap()
    }

    pub(crate) fn posts(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(verb, path, _)| verb == "POST" && path.ends_with("/prompt_async"))
            .count()
    }

    pub(crate) fn lose_post_reply(&self) {
        self.state.lock().unwrap().lose_post_reply = true;
    }

    pub(crate) fn conversation_creations(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(verb, path, _)| verb == "POST" && path == "/session")
            .count()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        self.worker.take().unwrap().join().unwrap();
    }
}

fn handle(mut stream: TcpStream, state: &Mutex<StubState>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    let (headers, offset, size) = loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let size = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (headers, end + 4, size);
        }
    };
    while bytes.len() < offset + size {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    let mut words = headers.lines().next().unwrap().split_whitespace();
    let method = words.next().unwrap();
    let path = words.next().unwrap();
    let body: Value = if size == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[offset..offset + size]).unwrap()
    };
    let authorized = headers.lines().any(|line| {
        line.split_once(':').is_some_and(|(key, value)| {
            key.eq_ignore_ascii_case("authorization")
                && value.trim() == "Basic b3BlbmNvZGU6c2VjcmV0"
        })
    });
    let mut state = state.lock().unwrap();
    state
        .requests
        .push((method.into(), path.into(), body.clone()));
    let (status, reply) = if !authorized {
        (401, json!({"error": "unauthorized"}))
    } else if method == "GET" && path.contains("/message/") {
        if let Some(error) = state.lookup_error {
            (error, json!({"error": "unavailable"}))
        } else if state.hidden > 0 {
            state.hidden -= 1;
            (404, Value::Null)
        } else if let Some(text) = state.messages.get(path.rsplit('/').next().unwrap()) {
            (200, json!({"parts": [{"type": "text", "text": text}]}))
        } else {
            (404, Value::Null)
        }
    } else if method == "GET" && path.starts_with("/session/") && path.ends_with("/message") {
        (200, json!([]))
    } else if method == "POST" && path.ends_with("/prompt_async") {
        // Real opencode appends on duplicate message IDs. A naive retry fails.
        state
            .messages
            .entry(body["messageID"].as_str().unwrap().into())
            .or_default()
            .push_str(body["parts"][0]["text"].as_str().unwrap());
        if state.lose_post_reply {
            state.lose_post_reply = false;
            return;
        }
        (204, Value::Null)
    } else if method == "GET" && path == "/global/health" {
        (200, json!({"healthy": true}))
    } else if method == "GET" && path == "/session/status" {
        (200, json!({"ses_test": {"type": "busy"}}))
    } else if method == "POST" && path == "/session" {
        (200, json!({"id": "ses_test"}))
    } else if method == "PATCH" && state.rename_error {
        (503, Value::Null)
    } else if method == "PATCH" || path.ends_with("/abort") {
        (200, json!({}))
    } else {
        (404, Value::Null)
    };
    let reply = if status == 204 {
        String::new()
    } else {
        reply.to_string()
    };
    let output = format!("HTTP/1.1 {status} Stub\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
    let _ = stream.write_all(output.as_bytes());
}

#[test]
fn duplicate_append_provider_is_not_reposted_after_lost_acknowledgement() {
    let stub = Stub::new();
    let binding = MessageBinding::new("ses_test").unwrap();
    stub.state.lock().unwrap().lose_post_reply = true;
    assert!(stub
        .client()
        .attempt_delivery(&binding, "once", Duration::from_secs(1))
        .is_err());
    assert_eq!(
        stub.client()
            .attempt_delivery(&binding, "once", Duration::from_secs(1))
            .unwrap(),
        DeliveryOutcome::Accepted
    );
    assert_eq!(stub.posts(), 1);
    assert_eq!(
        stub.state.lock().unwrap().messages[&binding.message_id],
        "once"
    );
}

#[test]
fn unconfirmed_post_is_retained_and_later_lookup_accepts_without_resend() {
    let stub = Stub::new();
    let binding = MessageBinding::new("ses_test").unwrap();
    assert_eq!(
        stub.client()
            .attempt_delivery(&binding, "once", Duration::ZERO)
            .unwrap(),
        DeliveryOutcome::Unconfirmed
    );
    assert_eq!(
        stub.client()
            .attempt_delivery(&binding, "once", Duration::from_secs(1))
            .unwrap(),
        DeliveryOutcome::Accepted
    );
    assert_eq!(stub.posts(), 1);
}

#[test]
fn only_a_404_can_authorize_submission_and_busy_input_never_aborts() {
    let stub = Stub::new();
    let binding = MessageBinding::new("ses_test").unwrap();
    for status in [401, 403, 500, 503, 302] {
        stub.state.lock().unwrap().lookup_error = Some(status);
        assert!(stub
            .client()
            .attempt_delivery(&binding, "hello", Duration::from_secs(1))
            .is_err());
        assert_eq!(stub.posts(), 0);
    }
    stub.state.lock().unwrap().lookup_error = None;
    assert_eq!(stub.client().status().unwrap()["ses_test"]["type"], "busy");
    assert_eq!(
        stub.client()
            .attempt_delivery(&binding, "urgent", Duration::from_secs(1))
            .unwrap(),
        DeliveryOutcome::Accepted
    );
    assert!(!stub
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|(_, path, _)| path.ends_with("/abort")));
}

#[test]
fn readiness_create_rename_abort_and_every_request_require_authentication() {
    let stub = Stub::new();
    assert!(stub.client().ready().unwrap());
    assert_eq!(
        stub.client().create_conversation("local agent").unwrap(),
        "ses_test"
    );
    stub.state.lock().unwrap().rename_error = true;
    assert!(stub.client().rename("ses_test", "new title").is_err());
    stub.state.lock().unwrap().rename_error = false;
    stub.client().rename("ses_test", "new title").unwrap();
    stub.client().abort("ses_test").unwrap();
    let bad = Client::new(stub.port, "wrong", Duration::from_secs(1)).unwrap();
    assert!(!bad.ready().unwrap());
    assert!(bad.create_conversation("bad").is_err());
    assert!(bad.status().is_err());
    assert!(bad.rename("ses_test", "bad").is_err());
    assert!(bad.abort("ses_test").is_err());
    let binding = MessageBinding::new("ses_test").unwrap();
    assert!(bad.message_exists(&binding).is_err());
    assert!(bad
        .attempt_delivery(&binding, "bad", Duration::from_secs(1))
        .is_err());
    assert_eq!(stub.posts(), 0);
}

#[test]
fn unavailable_provider_and_malformed_identifiers_cannot_be_treated_as_absent() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let client = Client::new(port, "secret", Duration::from_millis(100)).unwrap();
    let binding = MessageBinding::new("ses_test").unwrap();
    assert!(client.message_exists(&binding).is_err());
    assert!(client
        .attempt_delivery(&binding, "hello", Duration::from_millis(100))
        .is_err());
    assert!(client.rename("ses_test/../../abort", "bad").is_err());
    assert!(MessageBinding::new("ses_test?x=bad").is_err());
}

#[test]
fn message_ids_sort_in_submission_order_and_have_native_shape() {
    let ids: Vec<_> = (0..5000)
        .map(|_| MessageBinding::new("ses_test").unwrap())
        .collect();
    for binding in &ids {
        for (id, prefix) in [(&binding.message_id, "msg_"), (&binding.part_id, "prt_")] {
            assert_eq!(id.len(), 30);
            assert!(id.starts_with(prefix));
            assert!(id[4..16]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            assert!(id[16..].bytes().all(|b| b.is_ascii_alphanumeric()));
        }
    }
    assert!(ids
        .windows(2)
        .all(|pair| pair[0].message_id < pair[1].message_id && pair[0].part_id < pair[1].part_id));
}

#[test]
fn approved_agent_config_matches_fixture_and_owner_web_ruling() {
    let config = OpencodeConfig::default();
    let rendered = config.render_agent_config().unwrap();
    let expected: Value = serde_json::from_str(include_str!("config.expected.json")).unwrap();
    assert_eq!(
        rendered,
        serde_json::to_string_pretty(&expected).unwrap() + "\n"
    );
    assert_eq!(expected["permission"]["websearch"], "allow");
    assert_eq!(expected["permission"]["webfetch"], "allow");
    assert!(expected["permission"]
        .as_object()
        .unwrap()
        .values()
        .all(|v| v == "allow" || v == "deny"));
    assert_eq!(config.version, "1.17.9");
    assert_eq!(config.port_range, [18500, 18599]);
    assert_eq!(config.max_agents, 1);
}

#[test]
fn app_config_loads_opencode_overrides_and_refuses_invalid_settings() {
    let tmp = ScratchDir::new();
    let path = tmp.path().join("config.yaml");
    std::fs::write(
        &path,
        "opencode:\n  model_id: test-model\n  confirm_timeout_secs: 3\n",
    )
    .unwrap();
    let config = crate::config::AppConfig::load_from_path(&path).unwrap();
    assert_eq!(config.opencode.model_id, "test-model");
    assert_eq!(config.opencode.confirm_timeout_secs, 3);
    assert_eq!(config.opencode.version, "1.17.9");
    for yaml in [
        "opencode:\n  port_range: [1, 0]\n",
        "opencode:\n  max_agents: 0\n",
        "opencode:\n  model_base_url: https://example.com/v1\n",
        "opencode:\n  confirm_timeout_secs: 0\n",
        "opencode:\n  typo_field: true\n",
    ] {
        std::fs::write(&path, yaml).unwrap();
        assert!(
            crate::config::AppConfig::load_from_path(&path).is_err(),
            "{yaml}"
        );
    }
}

#[test]
fn plugin_checks_relative_and_escaping_paths_against_production_judge() {
    use std::{
        io::{BufRead, BufReader},
        os::unix::{fs::symlink, net::UnixStream},
        process::{Command, Stdio},
    };
    let root = ScratchDir::in_dir(std::path::Path::new("/tmp"));
    let checkout = root.path().join("checkout");
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    std::fs::create_dir_all(root.path().join("tmp")).unwrap();
    std::fs::create_dir_all(root.path().join("outside")).unwrap();
    symlink(root.path().join("outside"), checkout.join("escape-link")).unwrap();
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let child = Command::new("python3")
        .arg(repo.join("scripts/local-judge/service.py"))
        .arg("--root")
        .arg(root.path())
        .args(["--port", "0", "--no-model", "--policy"])
        .arg(repo.join("scripts/local-judge/policy.md"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Cleanup(std::process::Child);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Cleanup(child);
    let control = |body: Value| -> Result<Value> {
        let mut socket = UnixStream::connect(root.path().join("control.sock"))?;
        socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        writeln!(socket, "{body}")?;
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line)?;
        let reply: Value = serde_json::from_str(&line)?;
        reply
            .get("result")
            .cloned()
            .with_context(|| format!("judge control rejected request: {reply}"))
    };
    let start = Instant::now();
    loop {
        if control(json!({"op": "health"})).is_ok() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "judge did not start"
        );
        assert!(child.0.try_wait().unwrap().is_none(), "judge exited");
        thread::sleep(Duration::from_millis(20));
    }
    let endpoint = control(
        json!({"op": "register", "session_id": "plugin-test", "agent": {
            "name": "plugin-test", "ticket": 2043, "title": "plugin", "branch": "2043-plugin",
            "checkout": checkout, "tmp": root.path().join("tmp"), "parent": "parent",
            "sm_url": "http://127.0.0.1:8420", "proxy_port": 18700
        }}),
    )
    .unwrap();
    let output = Command::new("node")
        .arg(repo.join("scripts/opencode/judge-rules.acceptance.mjs"))
        .env("OPENCODE_TEST_CHECKOUT", &checkout)
        .env("LOCAL_AGENT_ID", "plugin-test")
        .env("LOCAL_JUDGE_URL", endpoint["url"].as_str().unwrap())
        .env("LOCAL_JUDGE_TOKEN", endpoint["token"].as_str().unwrap())
        .env_remove("SM_JUDGE_PLUGIN_LOG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
