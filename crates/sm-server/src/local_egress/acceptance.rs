//! Full gateway -> authenticated HTTP -> durable queue -> sandbox -> proxy path.
use super::*;
use crate::{
    config::AppConfig,
    http::{router, AppState},
    queue::{local_wall, RetainedQueueStore},
};
use serde_json::{json, Value};
use std::future::IntoFuture;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
use tokio::net::TcpListener;

const PROBE: &str = include_str!("acceptance_probe.py");
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
struct PublicResolver;
impl Resolver for PublicResolver {
    fn resolve<'a>(
        &'a self,
        _: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>>
    {
        Box::pin(async { Ok(vec!["93.184.216.34".parse().unwrap()]) })
    }
}
async fn port(first: u16, last: u16) -> TcpListener {
    for port in first..=last {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return listener;
        }
    }
    panic!("no isolated test port available");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_agents_gateway_queue_restart_and_egress_attribution() {
    let directory = super::tests::directory().canonicalize().unwrap();
    let service_dir = directory.join("service");
    fs::create_dir(&service_dir).unwrap();
    fs::set_permissions(&service_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let gateway = gateway::Gateway::open(&service_dir).unwrap();
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut proxy = Proxy::new(&service_dir).unwrap();
    proxy.resolver = Arc::new(PublicResolver);
    proxy.dial_fixture = Some(echo.local_addr().unwrap());
    let echo_task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = echo.accept().await.unwrap();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
                stream.shutdown().await.unwrap();
            });
        }
    });
    let mut config = AppConfig::default();
    config.paths.state_file = directory.join("sessions.json").display().to_string();
    config.sm_send.db_path = directory.join("messages.db").display().to_string();
    config.rust_core.fixture_writes_enabled = true;
    config.rust_core.runtime_enabled = false;
    config.usage.enabled = false;
    let mut fixtures = Vec::new();
    let mut registrations = BTreeMap::new();
    let mut tasks = Vec::new();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let (proxy_completed, mut completed) = tokio::sync::mpsc::unbounded_channel();
    for agent in ["agent-a", "agent-b"] {
        let listener = port(gateway::FIRST_PORT + 50, gateway::LAST_PORT).await;
        let gateway_port = listener.local_addr().unwrap().port();
        let egress = port(FIRST_PORT + 50, LAST_PORT).await;
        let egress_port = egress.local_addr().unwrap().port();
        let fixture =
            crate::local_sockets::service::prepare_launch_for(agent, gateway_port, egress_port);
        registrations.insert(
            agent.to_owned(),
            Registration {
                agent_id: agent.into(),
                port: egress_port,
                active: true,
                gateway: Some(gateway::GatewayRegistration {
                    port: gateway_port,
                    upstream: upstream_addr,
                }),
            },
        );
        let service = gateway.clone();
        let agent_id = agent.to_owned();
        let stopped_gateway = stopped.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let service = service.clone();
                let agent = agent_id.clone();
                let stopped = stopped_gateway.clone();
                tokio::spawn(async move {
                    service.serve(stream, agent, upstream_addr, stopped).await;
                });
            }
        }));
        let service = proxy.clone();
        let agent_id = agent.to_owned();
        let stopped_proxy = stopped.clone();
        let proxy_completed = proxy_completed.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let (stream, _) = egress.accept().await.unwrap();
                let service = service.clone();
                let agent = agent_id.clone();
                let stopped = stopped_proxy.clone();
                let proxy_completed = proxy_completed.clone();
                tokio::spawn(async move {
                    service.serve(stream, agent, stopped).await;
                    let _ = proxy_completed.send(());
                });
            }
        }));
        fixtures.push((agent, fixture));
    }
    let registration_file = service_dir.join("registrations.json");
    fs::write(
        &registration_file,
        serde_json::to_vec(&registrations).unwrap(),
    )
    .unwrap();
    fs::set_permissions(&registration_file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&config.paths.state_file, serde_json::to_vec(&json!({"sessions": fixtures.iter().map(|(id,f)| json!({
        "id":id,"name":id,"working_dir":f.path("checkout"),"tmux_session":id,"provider":"claude","status":"running",
        "created_at":"2026-06-01T00:00:00","last_activity":"2026-06-01T00:00:00"
    })).collect::<Vec<_>>()})).unwrap()).unwrap();
    let app = || {
        router(
            AppState::new(config.clone()).with_local_agent_gateway_directory(service_dir.clone()),
        )
    };
    let mut server = tokio::spawn(
        axum::serve(
            upstream,
            app().into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future(),
    );
    let queue_dir = config.queue_runner_state_dir();
    let mut pending = Vec::new();
    for (index, (agent, fixture)) in fixtures.iter().enumerate() {
        let registration = &registrations[*agent];
        let gateway_url = format!(
            "http://127.0.0.1:{}",
            registration.gateway.as_ref().unwrap().port
        );
        let proxy_url = format!("http://127.0.0.1:{}", registration.port);
        let other = if index == 0 { "agent-b" } else { "agent-a" };
        let shell = fixture.path("application").with_file_name("queue-zsh");
        fs::copy("/bin/zsh", &shell).unwrap();
        assert!(std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&shell)
            .status()
            .unwrap()
            .success());
        local_wall::register(
            &queue_dir,
            agent,
            local_wall::WallSpec {
                agent_state: fixture.path("profile").parent().unwrap().to_path_buf(),
                checkout: fixture.path("checkout"),
                profile: fixture.path("profile"),
                shell,
                environment: fixture
                    .environment
                    .iter()
                    .map(|(k, v)| (k.to_str().unwrap().into(), v.to_str().unwrap().into()))
                    .collect(),
                gateway_port: registration.gateway.as_ref().unwrap().port,
                egress_port: registration.port,
            },
        )
        .unwrap();
        local_wall::attach(&queue_dir, agent, Arc::new(fixture.queue_binding())).unwrap();
        let probe = json!({"agent":agent,"gateway":gateway_url,"proxy":proxy_url,"secret":service_dir.join("gateway.key"),
            "forbidden_ports":[upstream_addr.port(), registrations[other].port, registrations[other].gateway.as_ref().unwrap().port]});
        let script = format!(
            "{} -c {} job {}",
            quote(&fixture.path("python").display().to_string()),
            quote(PROBE),
            quote(&probe.to_string())
        );
        for variant in 0..3 {
            let result = fixture
                .path("checkout")
                .join(format!("submitted-{variant}.json"));
            let mut body = json!({"type":"tests","label":format!("{agent}-{variant}"),"cwd":fixture.path("checkout"),"script":script,"notify_target":other,
                "env":{"CLAUDE_SESSION_MANAGER_ID":other,"HTTP_PROXY":format!("http://127.0.0.1:{}",registrations[other].port)}});
            if variant != 0 {
                body["requester_session_id"] = json!(if variant == 1 { "" } else { other });
            }
            let input = json!({"gateway":gateway_url,"body":body,"result":result});
            let mut child = fixture
                .queue_binding()
                .spawn(
                    &fixture.path("python"),
                    &[
                        "-c".into(),
                        PROBE.into(),
                        "submit".into(),
                        input.to_string().into(),
                    ],
                    &fixture.environment,
                    &fixture.path("checkout"),
                )
                .unwrap();
            assert!(child.wait().unwrap().success());
            let response: Value = serde_json::from_slice(&fs::read(result).unwrap()).unwrap();
            assert_eq!(response["status"], 200, "{response}");
            assert_eq!(response["body"]["local_agent_id"], *agent);
            assert_eq!(response["body"]["requester_session_id"], *agent);
            assert_eq!(response["body"]["notify_session_id"], other);
            pending.push((
                agent.to_string(),
                response["body"]["id"].as_str().unwrap().to_string(),
            ));
        }
        local_wall::detach(&queue_dir, agent).unwrap();
    }
    // Restart the HTTP server and restore host bindings before admitting the
    // pending database rows. Gateway listeners and registrations remain stable.
    server.abort();
    let _ = server.await;
    let upstream = TcpListener::bind(upstream_addr).await.unwrap();
    server = tokio::spawn(
        axum::serve(
            upstream,
            app().into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future(),
    );
    for (agent, fixture) in &fixtures {
        local_wall::attach(&queue_dir, agent, Arc::new(fixture.queue_binding())).unwrap();
    }
    RetainedQueueStore::admit_queue_jobs_in_state_dir(
        &queue_dir,
        &PathBuf::from(&config.sm_send.db_path),
        0,
    )
    .unwrap();
    for (agent, id) in pending {
        let restored =
            RetainedQueueStore::get_queue_job_from_path(&queue_dir.join("queue_runner.db"), &id)
                .unwrap()
                .unwrap();
        assert_eq!(restored.local_agent_id.as_deref(), Some(agent.as_str()));
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let job = RetainedQueueStore::get_queue_job_from_path(
                &queue_dir.join("queue_runner.db"),
                &id,
            )
            .unwrap()
            .unwrap();
            if !matches!(job.state.as_str(), "running" | "pending") {
                let log = fs::read_to_string(job.log_path.unwrap()).unwrap();
                assert_eq!(job.state, "succeeded", "{log}");
                assert!(log.contains("combined-wall-ok"), "{log}");
                break;
            }
            assert!(Instant::now() < deadline, "job did not finish: {id}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    // Tunnel EOF can precede the final log append. Join all six proxy handlers
    // before inspecting attribution rather than relying on job completion.
    for _ in 0..6 {
        tokio::time::timeout(Duration::from_secs(5), completed.recv())
            .await
            .unwrap()
            .expect("proxy completion channel closed");
    }
    let log = fs::read_to_string(service_dir.join("connections.jsonl")).unwrap();
    for agent in ["agent-a", "agent-b"] {
        let records: Vec<Value> = log
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|v: &Value| v["agent_id"] == agent)
            .collect();
        assert_eq!(records.len(), 3, "{log}");
        assert!(
            records
                .iter()
                .all(|r| r["bytes_to_host"] == 12 && r["bytes_to_agent"] == 12),
            "{records:?}"
        );
        local_wall::detach(&queue_dir, agent).unwrap();
    }
    stop.send_replace(true);
    for task in tasks {
        task.abort();
    }
    server.abort();
    echo_task.abort();
    fs::remove_dir_all(directory).unwrap();
}
