use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct FakeServer {
    started: AtomicUsize,
    stopped: AtomicUsize,
    fail_stop: AtomicBool,
    running: AtomicBool,
    fail_probe: AtomicBool,
    footprint: i64,
}
impl ModelServer for FakeServer {
    fn start(&self, _: &ModelRecord) -> Result<Option<i32>> {
        self.started.fetch_add(1, Ordering::SeqCst);
        Ok(Some(123))
    }
    fn ready(&self, _: &ModelRecord) -> Result<bool> {
        Ok(true)
    }
    fn stop(&self, _: &ModelRecord) -> Result<()> {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        if self.fail_stop.load(Ordering::SeqCst) {
            bail!("shutdown timeout");
        }
        Ok(())
    }
    fn running(&self, _: &ModelRecord) -> Result<bool> {
        if self.fail_probe.load(Ordering::SeqCst) {
            bail!("probe unavailable");
        }
        Ok(self.running.load(Ordering::SeqCst))
    }
    fn footprint(&self, _: &ModelRecord) -> Result<Option<i64>> {
        Ok(Some(self.footprint))
    }
}
fn fixture() -> (ModelHost, Arc<FakeServer>, PathBuf) {
    static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "sm-model-{}-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
        FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let server = Arc::new(FakeServer {
        started: AtomicUsize::new(0),
        stopped: AtomicUsize::new(0),
        fail_stop: AtomicBool::new(false),
        running: AtomicBool::new(true),
        fail_probe: AtomicBool::new(false),
        footprint: 150 * GB,
    });
    let host = ModelHost {
        config: LocalHostConfig::default(),
        db_path: root.join("message_queue.db"),
        state_file: root.join("sessions.json"),
        sessions: SessionStore::new_with_queue(
            root.join("sessions.json"),
            root.join("message_queue.db"),
        ),
        queue_dir: root.clone(),
        queue_policy: crate::queue::QueueAdmissionPolicy::default(),
        operation: Mutex::new(()),
        yield_worker: AtomicBool::new(false),
        force_unload: AtomicBool::new(false),
        backend: server.clone(),
        reload_since: Mutex::new(None),
    };
    (host, server, root)
}
fn request(
    key: &str,
    seats: Option<u32>,
    context: Option<u64>,
    reservation: Option<f64>,
) -> LoadRequest {
    LoadRequest {
        key: key.into(),
        seats,
        context,
        reservation,
    }
}
fn ready(host: &ModelHost) -> ModelRecord {
    let mut m = host.prepare(request(FLASH_KEY, None, None, None)).unwrap();
    m.state = "ready".into();
    m.pid = Some(123);
    host.save(&m).unwrap();
    m
}
#[test]
fn measured_flash_reservation_and_identifier_match_spec() {
    let (host, _, root) = fixture();
    let m = host.prepare(request(FLASH_KEY, None, None, None)).unwrap();
    assert_eq!(
        (
            m.seats,
            m.context,
            m.measured_peak_bytes,
            m.reservation_bytes
        ),
        (1, 200_000, 137 * GB, 150_700_000_000)
    );
    assert_eq!(m.identifier, "qwen3.8-flash-next");
    let two = host
        .prepare(request(FLASH_KEY, Some(2), Some(160_000), None))
        .unwrap();
    assert_eq!(two.reservation_bytes, 180_400_000_000);
    assert!(host
        .prepare(request("unknown/model", None, None, None))
        .unwrap_err()
        .to_string()
        .contains("--reservation"));
    let unknown = host
        .prepare(request("unknown/model", None, None, Some(50.)))
        .unwrap();
    assert_eq!(unknown.reservation_bytes, 50 * GB);
    assert!(host
        .prepare(request(FLASH_KEY, Some(0), None, None))
        .is_err());
    assert!(gb_bytes(f64::NAN).is_err());
    assert!(gb_bytes(-1.).is_err());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn insufficient_memory_never_starts_model() {
    let (host, server, root) = fixture();
    let mut m = host.prepare(request(FLASH_KEY, None, None, None)).unwrap();
    assert!(host.load_prepared(&mut m, 180 * GB).is_err());
    assert_eq!(server.started.load(Ordering::SeqCst), 0);
    assert!(host.record().unwrap().is_none());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn failed_unload_keeps_resident_state_and_desired_then_retry_confirms_stop() {
    let (host, server, root) = fixture();
    ready(&host);
    server.fail_stop.store(true, Ordering::SeqCst);
    assert!(host.unload(true, Some("perf benchmark")).is_err());
    let m = host.record().unwrap().unwrap();
    assert_eq!(m.state, "draining");
    assert!(m.desired);
    assert!(m.resident());
    assert_eq!(m.pid, Some(123));
    assert_eq!(m.last_yield_reason.as_deref(), Some("perf benchmark"));
    assert!(m.last_error.unwrap().contains("shutdown timeout"));
    server.fail_stop.store(false, Ordering::SeqCst);
    host.unload(true, Some("perf benchmark")).unwrap();
    let m = host.record().unwrap().unwrap();
    assert_eq!(m.state, "yielded");
    assert!(m.desired);
    assert_eq!(m.pid, None);
    host.unload(false, None).unwrap();
    let m = host.record().unwrap().unwrap();
    assert_eq!(m.state, "unloaded");
    assert!(!m.desired);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn peaks_survive_unload_and_switching_keys() {
    let (host, _, root) = fixture();
    ready(&host);
    assert_eq!(host.sample().unwrap(), Some(150 * GB));
    host.unload(true, None).unwrap();
    let other = host
        .prepare(request("other/model", None, None, Some(50.)))
        .unwrap();
    host.save(&other).unwrap();
    let flash = host.prepare(request(FLASH_KEY, None, None, None)).unwrap();
    assert_eq!(flash.measured_peak_bytes, 150 * GB);
    assert_eq!(flash.reservation_bytes, 165 * GB);
    let two = host
        .prepare(request(FLASH_KEY, Some(2), Some(160_000), None))
        .unwrap();
    assert_eq!(two.measured_peak_bytes, 164 * GB);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn formula_accounts_full_attention_layers_and_rejects_unknown_layout() {
    let (host, _, root) = fixture();
    host.connect().unwrap();
    fs::write(root.join("config.json"),r#"{"num_hidden_layers":48,"full_attention_interval":4,"num_key_value_heads":2,"head_dim":256}"#).unwrap();
    fs::write(root.join("model.safetensors"), [0; 10]).unwrap();
    assert_eq!(
        lmstudio_reservation(&root, 4, 200_000).unwrap(),
        10 + 4 * 200_000 * 24_576
    );
    fs::write(
        root.join("config.json"),
        r#"{"num_hidden_layers":62,"num_key_value_heads":8,"head_dim":128}"#,
    )
    .unwrap();
    assert_eq!(
        lmstudio_reservation(&root, 2, 100_000).unwrap(),
        10 + 2 * 100_000 * 253_952
    );
    fs::write(root.join("config.json"), "{}").unwrap();
    assert!(lmstudio_reservation(&root, 1, 200_000).is_err());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn only_owned_descendants_are_sampled() {
    let listing = "10 1 model\n11 10 worker\n12 11 child\n20 1 other\n21 20 unrelated";
    assert_eq!(descendants(listing, vec![10]), vec![10, 11, 12]);
    assert_eq!(quote("a'$(touch nope)"), "'a'\\''$(touch nope)'");
}

struct BlockingServer {
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    released: (Mutex<bool>, std::sync::Condvar),
}
impl ModelServer for BlockingServer {
    fn start(&self, _: &ModelRecord) -> Result<Option<i32>> {
        Ok(Some(123))
    }
    fn ready(&self, _: &ModelRecord) -> Result<bool> {
        Ok(true)
    }
    fn stop(&self, _: &ModelRecord) -> Result<()> {
        let _ = self.entered.lock().unwrap().send(());
        let mut released = self.released.0.lock().unwrap();
        while !*released {
            released = self.released.1.wait(released).unwrap();
        }
        Ok(())
    }
    fn footprint(&self, _: &ModelRecord) -> Result<Option<i64>> {
        Ok(Some(1))
    }
}
#[test]
fn perf_and_memory_guard_wait_for_confirmed_model_stop_before_a_job_is_selected() {
    let (mut host, _, root) = fixture();
    let (tx, rx) = std::sync::mpsc::channel();
    let server = Arc::new(BlockingServer {
        entered: Mutex::new(tx),
        released: (Mutex::new(false), std::sync::Condvar::new()),
    });
    host.backend = server.clone();
    let host = Arc::new(host);
    ready(&host);
    hosts()
        .lock()
        .unwrap()
        .insert(fs::canonicalize(&root).unwrap(), host.clone());
    let job = crate::queue::RetainedQueueStore::create_queue_job_in_state_dir(
        &root,
        crate::queue::CreateQueueJob {
            local_submitter: None,
            job_type: "tests".into(),
            label: "must survive model unload".into(),
            requester_session_id: Some("owner".into()),
            notify_session_id: "owner".into(),
            cwd: root.to_string_lossy().into(),
            argv: Some(vec!["true".into()]),
            script: None,
            env: BTreeMap::new(),
            timeout_seconds: 600,
            cpu_percent: None,
            gpu_percent: None,
            memory_bytes: None,
            rank_tickets: None,
        },
    )
    .unwrap();
    let conn = Connection::open(root.join("queue_runner.db")).unwrap();
    conn.execute(
        "UPDATE queue_jobs SET state='running',pid=101,process_group_id=101 WHERE id=?1",
        [&job.id],
    )
    .unwrap();
    assert!(!guard_model(
        &root,
        Some((275 * GB, 40 * GB)),
        host.queue_policy.memory_min_free_bytes
    )
    .unwrap());
    assert!(hold_perf(&root, "measured run").unwrap());
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(host.record().unwrap().unwrap().state, "draining");
    let rss = std::collections::HashMap::from([(101, 30 * GB)]);
    assert_eq!(
        crate::queue::host_memory_guard_pass(
            &conn,
            Some((275 * GB, GB)),
            host.queue_policy.memory_min_free_bytes,
            &rss
        )
        .unwrap(),
        None
    );
    let state: String = conn
        .query_row("SELECT state FROM queue_jobs WHERE id=?1", [&job.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "running");
    assert!(hold_perf(&root, "measured run").unwrap());
    *server.released.0.lock().unwrap() = true;
    server.released.1.notify_all();
    // Taking the operation lock waits for the worker to finish its transition.
    host.unload(true, Some("perf measured run")).unwrap();
    assert!(!hold_perf(&root, "measured run").unwrap());
    assert_eq!(host.record().unwrap().unwrap().state, "yielded");
    assert_eq!(
        crate::queue::host_memory_guard_pass(
            &conn,
            Some((275 * GB, GB)),
            host.queue_policy.memory_min_free_bytes,
            &rss
        )
        .unwrap(),
        Some((job.id, 101))
    );
    hosts()
        .lock()
        .unwrap()
        .remove(&fs::canonicalize(&root).unwrap());
    drop(conn);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn lifecycle_save_cannot_erase_a_newer_peak() {
    let (host, _, root) = fixture();
    let mut stale = ready(&host);
    host.sample().unwrap();
    stale.state = "draining".into();
    host.save(&stale).unwrap();
    let m = host.record().unwrap().unwrap();
    assert_eq!(m.measured_peak_bytes, 150 * GB);
    assert_eq!(m.reservation_bytes, 165 * GB);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cli_backend_launches_and_cleanly_stops_an_owned_fixture_server() {
    use std::os::unix::fs::PermissionsExt;
    let (mut host, _, root) = fixture();
    host.connect().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let program = root.join("fake-mtplx");
    fs::write(&program,r#"#!/usr/bin/env python3
import sys, os, json, signal, time
from pathlib import Path
from http.server import BaseHTTPRequestHandler, HTTPServer
root=Path(__file__).parent
if sys.argv[1]=='--version':
    print('mtplx 2.12.0'); sys.exit(0)
if sys.argv[1]=='stop':
    (root/'stop-called').write_text('unauthenticated public CLI')
    print('Port is in use, but not by an MTPLX server. Not touching it.')
    sys.exit(1)
(root/'pid').write_text(str(os.getpid()))
(root/'launch.json').write_text(json.dumps({'argv':sys.argv[1:],'bank':os.environ['MTPLX_SESSION_BANK_MAX_BYTES']}))
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path!='/v1/models' or self.headers.get('Authorization')!='Bearer local':
            self.send_response(401); self.end_headers();return
        self.send_response(200); self.end_headers();self.wfile.write(b'{"data":[{"id":"fixture"}]}')
    def log_message(self,*args): pass
port=int(sys.argv[sys.argv.index('--port')+1])
HTTPServer(('127.0.0.1',port),Handler).serve_forever()
"#).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    host.config.mtplx_path = program.to_string_lossy().into();
    host.config.base_url = format!("http://127.0.0.1:{port}");
    let backend = Arc::new(CliServer {
        config: host.config.clone(),
        socket: format!("sm-fixture-{port}"),
        port,
    });
    host.backend = backend.clone();
    let mut m = host.prepare(request(FLASH_KEY, None, None, None)).unwrap();
    host.load_prepared(&mut m, 275 * GB).unwrap();
    assert_eq!(m.state, "ready");
    assert!(m.pid.is_some());
    assert!(host
        .load(request(FLASH_KEY, None, None, None))
        .unwrap_err()
        .to_string()
        .contains("already loaded"));
    let launch: Value =
        serde_json::from_slice(&fs::read(root.join("launch.json")).unwrap()).unwrap();
    assert_eq!(launch["bank"], "16000000000");
    let args = launch["argv"].as_array().unwrap();
    for (flag, value) in [
        ("--context-window", "200000"),
        ("--max-active-requests", "2"),
        ("--batching-preset", "agent"),
    ] {
        let i = args.iter().position(|v| v == flag).unwrap();
        assert_eq!(args[i + 1], value);
    }
    // Simulate a restart after launch but before the launch worker saved PID.
    let pid = m.pid;
    m.state = "loading".into();
    m.pid = None;
    host.save(&m).unwrap();
    host.recover().unwrap();
    let recovered = host.record().unwrap().unwrap();
    assert_eq!(recovered.state, "draining");
    assert_eq!(recovered.pid, pid);
    assert_eq!(
        recovered.last_error.as_deref(),
        Some("sm restarted during loading; unload before reloading")
    );
    // Persisted recovery remains unloadable after another controller restart.
    host.recover().unwrap();
    assert_eq!(host.record().unwrap().unwrap().pid, pid);
    // A changed private pane binding must not signal the running model.
    let mut wrong = recovered.clone();
    wrong.pid = Some(pid.unwrap() + 1);
    assert!(backend
        .stop(&wrong)
        .unwrap_err()
        .to_string()
        .contains("ownership changed"));
    assert!(backend.ready(&recovered).unwrap());
    // The authenticated server's public CLI refuses an unauthenticated probe.
    // Stop must use the verified pane, not that CLI or a health-reported PID.
    host.unload(true, None).unwrap();
    assert_eq!(host.record().unwrap().unwrap().state, "unloaded");
    assert!(backend.pane().unwrap().is_none());
    assert!(!root.join("stop-called").exists());
    let _ = backend.tmux(&["kill-server"]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn model_load_cannot_race_running_perf_or_cooldown() {
    let (host, _, root) = fixture();
    host.connect().unwrap();
    let job = crate::queue::RetainedQueueStore::create_queue_job_in_state_dir(
        &root,
        crate::queue::CreateQueueJob {
            local_submitter: None,
            job_type: "perf".into(),
            label: "quiet run".into(),
            requester_session_id: None,
            notify_session_id: "owner".into(),
            cwd: root.to_string_lossy().into(),
            argv: Some(vec!["true".into()]),
            script: None,
            env: BTreeMap::new(),
            timeout_seconds: 60,
            cpu_percent: Some(100),
            gpu_percent: Some(0),
            memory_bytes: Some(GB),
            rank_tickets: None,
        },
    )
    .unwrap();
    let conn = Connection::open(root.join("queue_runner.db")).unwrap();
    conn.execute(
        "UPDATE queue_jobs SET state='running' WHERE id=?1",
        [&job.id],
    )
    .unwrap();
    let error =
        crate::queue::with_model_load_admission(&root, host.queue_policy, || Ok(())).unwrap_err();
    assert!(error.to_string().contains("performance run"));
    conn.execute(
        "UPDATE queue_jobs SET state='succeeded',finished_at=?2 WHERE id=?1",
        params![job.id, now()],
    )
    .unwrap();
    assert!(crate::queue::with_model_load_admission(&root, host.queue_policy, || Ok(())).is_err());
    conn.execute(
        "UPDATE queue_jobs SET finished_at='2020-01-01T00:00:00Z' WHERE id=?1",
        [&job.id],
    )
    .unwrap();
    crate::queue::with_model_load_admission(&root, host.queue_policy, || Ok(())).unwrap();
    drop(conn);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn graceful_model_stop_is_bounded_without_forced_kill() {
    use std::io::{BufRead, BufReader};
    let mut child = Command::new("python3")
        .args(["-c", "import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print('ready',flush=True); time.sleep(30)"])
        .stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "ready");
    let start = Instant::now();
    let result = terminate_owned_model(child.id() as i32, Duration::from_millis(150));
    let still_running = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("admission stays blocked"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(still_running, "graceful timeout force-killed the model");
}

#[test]
fn model_command_failure_preserves_stdout_reason() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf 'fixture shutdown refused'; exit 1"]);
    let error = command_output(command, Duration::from_secs(2)).unwrap_err();
    assert!(error.to_string().contains("fixture shutdown refused"));
    assert!(error.to_string().contains("exit status: 1"));
}

#[test]
fn runtime_reconciles_late_exit_after_failed_unload_without_sampling_or_retry() {
    for reason in [None, Some("perf benchmark")] {
        let (host, server, root) = fixture();
        ready(&host);
        server.fail_stop.store(true, Ordering::SeqCst);
        assert!(host.unload(true, reason).is_err());
        host.reconcile_draining().unwrap();
        assert_eq!(host.record().unwrap().unwrap().state, "draining");
        server.fail_probe.store(true, Ordering::SeqCst);
        assert!(host.reconcile_draining().is_err());
        assert_eq!(host.record().unwrap().unwrap().state, "draining");
        server.fail_probe.store(false, Ordering::SeqCst);
        server.running.store(false, Ordering::SeqCst);
        // Reconciliation must not change state during an active operation.
        let lock = host.operation.lock().unwrap();
        host.reconcile_draining().unwrap();
        assert_eq!(host.record().unwrap().unwrap().state, "draining");
        drop(lock);
        host.reconcile_draining().unwrap();
        assert_eq!(host.sample().unwrap(), Some(0));
        let m = host.record().unwrap().unwrap();
        assert_eq!(
            m.state,
            if reason.is_some() {
                "yielded"
            } else {
                "unloaded"
            }
        );
        assert_eq!(m.desired, reason.is_some());
        assert_eq!(m.last_yield_reason.as_deref(), reason);
        assert!(m.last_error.is_none());
        assert!(m.pid.is_none());
        assert!(!m.resident());
        assert!(!host.force_unload.load(Ordering::SeqCst));
        assert_eq!(server.stopped.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn stale_live_pane_does_not_keep_an_exited_process_resident() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    assert_eq!(live_pane_pid(&format!("0 {pid}")).unwrap(), None);
    let pid = std::process::id() as i32;
    assert_eq!(live_pane_pid(&format!("0 {pid}")).unwrap(), Some(pid));
    assert_eq!(live_pane_pid(&format!("1 {pid}")).unwrap(), None);
    for invalid in ["", "unexpected 123", "0", "0 -1", "0 0"] {
        assert!(live_pane_pid(invalid).is_err());
    }
}

fn local_session(host: &ModelHost, status: &str) -> SessionStore {
    let store = host.session_store().clone();
    store
        .create_core_session(
            crate::sessions::CreateCoreSessionRequest {
                id: Some("local-seat".into()),
                name: Some("sm-local-seat".into()),
                working_dir: Some(host.queue_dir.display().to_string()),
                ..Default::default()
            },
            Some(host.queue_dir.join("logs")),
        )
        .unwrap();
    let mut state: Value = serde_json::from_slice(&fs::read(&host.state_file).unwrap()).unwrap();
    let session = &mut state["sessions"][0];
    session["host"] = serde_json::json!("local");
    session["provider"] = serde_json::json!("opencode");
    session["status"] = serde_json::json!(status);
    session["opencode"] = serde_json::json!({"port":18500,"state_dir":root_path(host),"version":"1.17.9","model_base_url":"http://127.0.0.1:8000/v1"});
    fs::write(&host.state_file, serde_json::to_vec(&state).unwrap()).unwrap();
    store
}
#[test]
fn concurrent_force_unload_shortens_inflight_local_session_drain() {
    let (mut host, server, root) = fixture();
    host.config.drain_timeout = 60;
    ready(&host);
    let store = local_session(&host, "running");
    let host = Arc::new(host);
    let draining = host.clone();
    let worker = thread::spawn(move || draining.unload(false, Some("perf test")));
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.record().unwrap().unwrap().state != "draining" {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let start = Instant::now();
    host.unload(true, None).unwrap();
    worker.join().unwrap().unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(server.stopped.load(Ordering::SeqCst), 1);
    assert_eq!(host.record().unwrap().unwrap().state, "unloaded");
    let raw: Value = serde_json::from_slice(&fs::read(&host.state_file).unwrap()).unwrap();
    assert_eq!(raw["sessions"][0]["resume_needed"], true);
    assert!(store
        .get_session("local-seat")
        .unwrap()
        .unwrap()
        .local_parked_reason
        .is_some());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn reload_hold_resets_on_pressure_or_perf_and_resumes_without_duplicate_prompt() {
    let (mut host, server, root) = fixture();
    host.config.reload_hold = 600;
    ready(&host);
    let store = local_session(&host, "running");
    host.unload(true, Some("host memory pressure")).unwrap();
    host.reload_pass(Some(275 * GB), false).unwrap();
    assert!(host.reload_since.lock().unwrap().is_some());
    host.reload_pass(Some(1), false).unwrap();
    assert!(host.reload_since.lock().unwrap().is_none());
    host.reload_pass(Some(275 * GB), false).unwrap();
    host.reload_pass(Some(275 * GB), true).unwrap();
    assert!(host.reload_since.lock().unwrap().is_none());
    *host.reload_since.lock().unwrap() = Some(Instant::now() - Duration::from_secs(601));
    host.reload_pass(Some(275 * GB), false).unwrap();
    assert_eq!(host.record().unwrap().unwrap().state, "ready");
    assert_eq!(server.started.load(Ordering::SeqCst), 1);
    assert!(store
        .get_session("local-seat")
        .unwrap()
        .unwrap()
        .local_parked_reason
        .is_none());
    // A retrying opencode turn gets no continue prompt.
    let queue = crate::queue::RetainedQueueStore::new(host.db_path.clone());
    assert!(queue
        .pending_messages_for_target("local-seat", 10)
        .unwrap()
        .is_empty());
    let mut raw: Value = serde_json::from_slice(&fs::read(&host.state_file).unwrap()).unwrap();
    raw["sessions"][0]["status"] = serde_json::json!("idle");
    fs::write(&host.state_file, serde_json::to_vec(&raw).unwrap()).unwrap();
    host.reload_pass(Some(275 * GB), false).unwrap();
    host.reload_pass(Some(275 * GB), false).unwrap();
    let messages = queue.pending_messages_for_target("local-seat", 10).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "[sm] The local model was unloaded while you were working (host memory pressure). Continue where you left off.");
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn queue_card_reports_free_full_draining_and_yielded_reload_hold() {
    let (host, _, root) = fixture();
    ready(&host);
    let card = host.card_snapshot(Some(70 * GB), false).unwrap();
    assert_eq!(card["model_text"], "loaded (qwen3.8-flash-next)");
    assert_eq!(card["seats_text"], "0 of 1");
    assert!(card["memory_text"]
        .as_str()
        .unwrap()
        .starts_with("70.0 GB free, yields below "));
    local_session(&host, "idle");
    assert_eq!(
        host.card_snapshot(Some(70 * GB), false).unwrap()["seats_text"],
        "1 of 1: sm-local-seat"
    );
    let mut model = host.record().unwrap().unwrap();
    host.transition(&mut model, "draining").unwrap();
    assert_eq!(
        host.card_snapshot(Some(70 * GB), false).unwrap()["model_text"],
        "draining"
    );
    host.unload(true, Some("perf run")).unwrap();
    *host.reload_since.lock().unwrap() = Some(Instant::now() - Duration::from_secs(30));
    let card = host.card_snapshot(Some(275 * GB), false).unwrap();
    assert_eq!(card["model_text"], "yielded to perf run");
    assert!(card["reload_held_seconds"].as_u64().unwrap() >= 30);
    assert!(card["reload_text"].as_str().unwrap().contains("held 30s"));
    assert!(host.delivery_held().unwrap());
    fs::remove_dir_all(root).unwrap();
}

pub(super) fn register_outbox_fixture(
    db: PathBuf,
    state: PathBuf,
    store: SessionStore,
) -> Arc<ModelHost> {
    let (mut host, _, unused_root) = fixture();
    host.db_path = db.clone();
    host.state_file = state;
    host.sessions = store;
    host.queue_dir = db.parent().unwrap().to_path_buf();
    ready(&host);
    let host = Arc::new(host);
    hosts()
        .lock()
        .unwrap()
        .insert(fs::canonicalize(db).unwrap(), host.clone());
    let _ = fs::remove_dir_all(unused_root);
    host
}

fn root_path(host: &ModelHost) -> String {
    host.queue_dir.display().to_string()
}
