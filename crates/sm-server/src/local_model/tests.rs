use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct FakeServer {
    started: AtomicUsize,
    stopped: AtomicUsize,
    fail_stop: AtomicBool,
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
    fn footprint(&self, _: &ModelRecord) -> Result<Option<i64>> {
        Ok(Some(self.footprint))
    }
}
fn fixture() -> (ModelHost, Arc<FakeServer>, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "sm-model-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let server = Arc::new(FakeServer {
        started: AtomicUsize::new(0),
        stopped: AtomicUsize::new(0),
        fail_stop: AtomicBool::new(false),
        footprint: 150 * GB,
    });
    let host = ModelHost {
        config: LocalHostConfig::default(),
        db_path: root.join("message_queue.db"),
        state_file: root.join("sessions.json"),
        queue_dir: root.clone(),
        queue_policy: crate::queue::QueueAdmissionPolicy::default(),
        reserve: 8 * 1024 * 1024 * 1024,
        operation: Mutex::new(()),
        yield_worker: AtomicBool::new(false),
        force_unload: AtomicBool::new(false),
        backend: server.clone(),
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
    assert!(!guard_model(&root, Some((275 * GB, 40 * GB)), host.reserve).unwrap());
    assert!(hold_perf(&root, "measured run").unwrap());
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(host.record().unwrap().unwrap().state, "draining");
    let rss = std::collections::HashMap::from([(101, 30 * GB)]);
    assert_eq!(
        crate::queue::host_memory_guard_pass(&conn, Some((275 * GB, GB)), host.reserve, &rss)
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
        crate::queue::host_memory_guard_pass(&conn, Some((275 * GB, GB)), host.reserve, &rss)
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
    os.kill(int((root/'pid').read_text()),signal.SIGTERM)
    sys.exit(0)
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
    // The fixture's stop returns before the process necessarily exits. The
    // backend must wait for the owned pane to die, not trust command success.
    host.unload(true, None).unwrap();
    assert_eq!(host.record().unwrap().unwrap().state, "unloaded");
    assert!(backend.pane().unwrap().is_none());
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
fn stop_cli_watchdog_is_independent_and_bounded() {
    let start = Instant::now();
    assert!(bounded_stop(Path::new("/bin/sleep"), &["30"], 1).is_err());
    assert!(start.elapsed() < Duration::from_secs(5));
    bounded_stop(Path::new("/usr/bin/true"), &[], 1).unwrap();
}
