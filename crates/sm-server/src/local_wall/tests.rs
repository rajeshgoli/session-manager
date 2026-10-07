use super::*;
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    judge: LocalJudgeRuntime,
    egress_task: tokio::task::JoinHandle<std::io::Result<()>>,
    http_tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut stream) =
            std::os::unix::net::UnixStream::connect(self.judge.directory().join("control.sock"))
        {
            let _ = stream.write_all(b"{\"op\":\"shutdown\"}\n");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
        }
        self.egress_task.abort();
        for task in &self.http_tasks {
            task.abort();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_preparation_two_agents_restore_and_failed_launch_are_confined() {
    let root = PathBuf::from(format!(
        "/private/tmp/wh-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    private_directory(&root).unwrap();
    let home = root.join("h");
    private_directory(&home.join(".config/gh")).unwrap();
    fs::write(home.join(".config/gh/hosts.yml"), "fixture-token\n").unwrap();
    fs::write(
        home.join(".config/gh/config.yml"),
        "fixture-other-host-settings\n",
    )
    .unwrap();
    let egress_directory = root.join("egress");
    let service_directory = egress_directory.clone();
    let egress_task =
        tokio::spawn(async move { crate::local_egress::run_service(&service_directory).await });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !egress_directory.join("control.sock").exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let upstream = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let gateway_app = axum::Router::new().fallback(axum::routing::get(
        |headers: axum::http::HeaderMap| async move {
            headers[crate::local_egress::gateway::AGENT_HEADER]
                .to_str()
                .unwrap()
                .to_owned()
        },
    ));
    let gateway_task = tokio::spawn(async move {
        axum::serve(upstream, gateway_app).await.unwrap();
    });
    let model = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let model_port = model.local_addr().unwrap().port();
    let model_task = tokio::spawn(async move {
        axum::serve(
            model,
            axum::Router::new().fallback(axum::routing::get(|| async { "model" })),
        )
        .await
        .unwrap();
    });
    let mut config = crate::config::AppConfig::default();
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    config.local_judge.port = reservation.local_addr().unwrap().port();
    drop(reservation);
    config.local_host.base_url = format!("http://127.0.0.1:{model_port}");
    let judge = LocalJudgeRuntime::isolated(&config, root.join("judge"));
    let fixture = Fixture {
        root: root.clone(),
        judge: judge.clone(),
        egress_task,
        http_tasks: vec![gateway_task, model_task],
    };
    let egress = ServiceClient::new(egress_directory, std::env::current_exe().unwrap());
    let python_output = Command::new("python3")
        .args([
            "-c",
            "import sys; from pathlib import Path; print(Path(sys._base_executable).resolve())",
        ])
        .output()
        .unwrap();
    let python = PathBuf::from(String::from_utf8(python_output.stdout).unwrap().trim());
    let tool_source = root.join("probe");
    let compilation = Command::new("clang")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/local-wall/native/test_preparation.c"),
        )
        .arg("-o")
        .arg(&tool_source)
        .output()
        .unwrap();
    assert!(
        compilation.status.success(),
        "{}",
        String::from_utf8_lossy(&compilation.stderr)
    );
    // The physical state path cannot fit in sockaddr_un. Only the host-owned
    // exact endpoint alias grants socket access, not its parent or siblings.
    let state_root = home.join("state-root-long-enough-that-an-agent-broker-endpoint-exceeds-the-native-unix-socket-address-limit");
    let host = LocalWallRuntime::new(
        HostConfiguration {
            home: home.clone(),
            state_root,
            alias_root: root.join("a"),
            python,
            read_only_roots: vec![],
            executable_roots: vec![],
            control_ports: 18500..=18599,
            model_port,
            sm_upstream: upstream_address,
        },
        egress.clone(),
        judge.clone(),
    )
    .unwrap();
    let registration = |id: &str, port: u16| {
        let checkout = home.join(id);
        private_directory(&checkout).unwrap();
        AgentRegistration {
            id: id.into(),
            name: format!("sm-{id}"),
            ticket: 2025,
            title: "wall fixture".into(),
            branch: id.into(),
            checkout,
            parent: "host".into(),
            control_port: port,
            tools: vec![StageTool {
                name: "probe".into(),
                source: tool_source.clone(),
            }],
        }
    };
    let mut a_request = registration("wall-a", 18500);
    a_request.tools.push(StageTool {
        name: "wall-withdrawn".into(),
        source: tool_source.clone(),
    });
    let b_request = registration("wall-b", 18501);
    let a = host.prepare(&a_request).unwrap();
    let b = host.prepare(&b_request).unwrap();
    eprintln!("host fixture: both agents prepared");
    assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, a_request.control_port)).is_err());
    assert!(
        host.prepare(&a_request).is_err(),
        "live state must not be replaced"
    );
    let a_registration = egress.registration("wall-a").unwrap().unwrap();
    let b_registration = egress.registration("wall-b").unwrap().unwrap();
    assert_ne!(a_registration.port, b_registration.port);
    assert_ne!(a.environment["TMPDIR"], b.environment["TMPDIR"]);
    let run = |wall: &Arc<PreparedWall>, other: &EgressRegistration| {
        let arguments: Vec<OsString> = [
            other.gateway.as_ref().unwrap().port.to_string(),
            other.port.to_string(),
            model_port.to_string(),
            wall.artifacts.profile.display().to_string(),
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        let mut child = wall.spawn_queue("probe", &arguments).unwrap();
        assert!(wall.suspend().is_err(), "live child prevents suspension");
        let mut stdout = child.take_stdout().unwrap();
        let mut stderr = child.take_stderr().unwrap();
        let status = child.wait().unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).unwrap();
        let mut errors = String::new();
        stderr.read_to_string(&mut errors).unwrap();
        assert!(status.success(), "{errors}");
        let port: u16 = output.trim().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "background child retained listener"
            );
            thread::sleep(Duration::from_millis(10));
        }
    };
    let mut provider = a.spawn_provider("probe", &["provider".into()]).unwrap();
    eprintln!("host fixture: provider spawned");
    let mut ready = String::new();
    use std::io::BufRead;
    let stdout = provider.take_stdout().unwrap();
    let mut descriptor = libc::pollfd {
        fd: stdout.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut descriptor, 1, 5000) } <= 0 {
        let mut stderr = provider.take_stderr().unwrap();
        drop(provider);
        let mut errors = String::new();
        stderr.read_to_string(&mut errors).unwrap();
        panic!("provider did not announce readiness: {errors}");
    }
    std::io::BufReader::new(stdout)
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready.trim(), "provider-ready");
    eprintln!("host fixture: provider ready");
    let mut client =
        std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, a_request.control_port)).unwrap();
    let mut response = [0u8; 8];
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"provider");
    assert!(provider.wait().unwrap().success());
    drop(provider);
    drop(client);
    eprintln!("host fixture: provider completed");
    run(&a, &b_registration);
    run(&b, &a_registration);
    eprintln!("host fixture: both queue launches completed");
    let old_adapter = fs::read(&a.artifacts.adapter).unwrap();
    a.suspend().unwrap();
    let executables = a.artifacts.executables.clone();
    assert!(executables.join("wall-withdrawn").is_file());
    drop(a);
    // Simulate host termination during tool, supervisor and profile writes.
    fs::write(executables.join(".probe.new"), b"interrupted").unwrap();
    fs::write(executables.join(".supervisor.new"), b"interrupted").unwrap();
    fs::write(
        executables.parent().unwrap().join(".wall.sb.new"),
        b"interrupted",
    )
    .unwrap();
    a_request.tools.retain(|tool| tool.name != "wall-withdrawn");
    let restored = host.prepare(&a_request).unwrap();
    assert!(!executables.join("wall-withdrawn").exists());
    assert!(!executables.join(".probe.new").exists());
    assert!(!executables.join(".supervisor.new").exists());
    assert!(restored.spawn_queue("wall-withdrawn", &[]).is_err());
    assert_eq!(
        egress.registration("wall-a").unwrap().unwrap(),
        a_registration
    );
    // A new live service may have the same host peer token within this process,
    // but source regeneration is still checked through a restored launch.
    assert!(!old_adapter.is_empty());
    run(&restored, &b_registration);
    let mut invalid = registration("wall-invalid", 18502);
    invalid.tools[0].source = home.join("missing-tool");
    assert!(host.prepare(&invalid).is_err());
    assert!(!egress.registration("wall-invalid").unwrap().unwrap().active);
    let records: Vec<serde_json::Value> =
        fs::read_to_string(egress.directory().join("connections.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    for id in ["wall-a", "wall-b"] {
        assert!(records.iter().any(|record| record["agent_id"] == id));
    }
    restored.suspend().unwrap();
    b.suspend().unwrap();
    drop(restored);
    drop(b);
    drop(host);
    drop(fixture);
}
