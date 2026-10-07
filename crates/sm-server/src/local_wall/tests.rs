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
        assert!(Command::new("/usr/bin/git")
            .args(["init", "-q"])
            .arg(&checkout)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&checkout)
            .args(["config", "--local", "fixture.preserved", "yes"])
            .status()
            .unwrap()
            .success());
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
    for request in [&a_request, &b_request] {
        for (key, expected) in [
            ("user.name", request.name.clone()),
            ("user.email", format!("{}@local-agent.invalid", request.id)),
            ("fixture.preserved", "yes".into()),
        ] {
            let result = Command::new("/usr/bin/git")
                .env_clear()
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .arg("-C")
                .arg(&request.checkout)
                .args(["config", "--local", "--no-includes", "--get", key])
                .output()
                .unwrap();
            assert!(result.status.success());
            assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), expected);
        }
    }
    let aliased = registration("wall-git-alias", 18506);
    let alias_target = root.join("host-git-config");
    fs::write(&alias_target, b"[user]\nname = host\n").unwrap();
    fs::remove_file(aliased.checkout.join(".git/config")).unwrap();
    std::os::unix::fs::symlink(&alias_target, aliased.checkout.join(".git/config")).unwrap();
    assert!(host.prepare(&aliased).is_err());
    assert_eq!(fs::read(alias_target).unwrap(), b"[user]\nname = host\n");
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
    for key in [
        "LOCAL_AGENT_ID",
        "local_agent_id",
        "HTTPS_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "DYLD_INSERT_LIBRARIES",
        "GIT_CONFIG_COUNT",
        "XDG_CONFIG_HOME",
        "CARGO_HOME",
        "GH_TOKEN",
        "SM_API_URL",
        "SESSION_MANAGER_ID",
        "OPENAI_API_KEY",
        "bad=name",
    ] {
        assert!(
            a.spawn_provider_with_environment(
                "probe",
                &["provider".into()],
                &BTreeMap::from([(key.into(), "override".into())])
            )
            .is_err(),
            "accepted reserved {key}"
        );
    }
    assert!(a
        .spawn_provider_with_environment(
            "probe",
            &["provider".into()],
            &BTreeMap::from([("HOST_SETTING".into(), "contains\0nul".into())])
        )
        .is_err());
    let mut provider = a
        .spawn_provider_with_environment(
            "probe",
            &["provider-extra".into()],
            &BTreeMap::from([(
                "HOST_PROVIDER_PASSWORD".into(),
                "host-selected-password".into(),
            )]),
        )
        .unwrap();
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
    let hidden = a_request.checkout.join("unreadable");
    fs::create_dir(&hidden).unwrap();
    fs::hard_link(&tool_source, hidden.join("host-alias")).unwrap();
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable_result = host.prepare(&a_request);
    // Restore permissions even if the assertion fails, so fixture cleanup works.
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        unreadable_result.is_err(),
        "uninspected directories must fail closed"
    );
    fs::remove_file(hidden.join("host-alias")).unwrap();
    fs::remove_dir(hidden).unwrap();
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
    let queue_state = root.join("queue-service");
    let mut c_registration = registration("wall-c", 18502);
    let d_registration = registration("wall-d", 18503);
    let c = host
        .prepare_for_queue(&c_registration, &queue_state)
        .unwrap();
    let d = host
        .prepare_for_queue(&d_registration, &queue_state)
        .unwrap();
    // Refuse manifest publication after authority has persisted, then retry.
    let publication = registration("wall-publication", 18505);
    let registry = queue_state.join("local-walls");
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o500)).unwrap();
    let interrupted = host.prepare_for_queue(&publication, &queue_state);
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(interrupted.is_err());
    assert!(host
        .config
        .state_root
        .join(&publication.id)
        .join("xdg/config/queue-authority.json")
        .is_file());
    assert!(
        crate::queue::local_wall::registered_spec(&queue_state, &publication.id)
            .unwrap()
            .is_none()
    );
    let publication = host.prepare_for_queue(&publication, &queue_state).unwrap();
    publication.suspend().unwrap();
    publication.retire_queue().unwrap();
    drop(publication);
    let request = |wall: &Arc<PreparedWall>, other: &AgentRegistration| {
        let other_network = egress.registration(&other.id).unwrap().unwrap();
        crate::queue::CreateQueueJob {
            local_submitter: Some(
                crate::local_egress::gateway::VerifiedLocalAgent::test_identity(&wall.id),
            ),
            job_type: "tests".into(),
            label: "host-composed-wall".into(),
            requester_session_id: Some("forged".into()),
            notify_session_id: "another-agent".into(),
            cwd: wall.checkout.display().to_string(),
            argv: None,
            script: Some(format!(
                "'{}' {} {} {} '{}' || exit 1\nprint durable-wall-ok\n",
                wall.artifacts.tools["probe"].display(),
                other_network.gateway.unwrap().port,
                other_network.port,
                model_port,
                wall.artifacts.profile.display()
            )),
            env: BTreeMap::from([
                ("LOCAL_AGENT_ID".into(), "forged".into()),
                ("DYLD_INSERT_LIBRARIES".into(), "/does-not-exist".into()),
                ("SM_API_URL".into(), "http://127.0.0.1:8420".into()),
            ]),
            timeout_seconds: 30,
            cpu_percent: None,
            gpu_percent: None,
            memory_bytes: None,
            rank_tickets: None,
        }
    };
    use crate::queue::RetainedQueueStore;
    let c_job = RetainedQueueStore::create_queue_job_in_state_dir(
        &queue_state,
        request(&c, &d_registration),
    )
    .unwrap();
    let d_job = RetainedQueueStore::create_queue_job_in_state_dir(
        &queue_state,
        request(&d, &c_registration),
    )
    .unwrap();
    let refused = RetainedQueueStore::create_queue_job_in_state_dir(
        &queue_state,
        request(&c, &d_registration),
    )
    .unwrap();
    let manifest = queue_state.join("local-walls/wall-c.json");
    assert!(crate::queue::local_wall::retire_registration(&queue_state, &c.id).is_err());
    let saved_manifest = fs::read(&manifest).unwrap();
    let saved_profile = fs::read(&c.artifacts.profile).unwrap();
    c.detach_queue().unwrap();
    d.detach_queue().unwrap();
    let held = RetainedQueueStore::start_queue_job_in_state_dir(
        &queue_state,
        &root.join("messages.db"),
        &refused.id,
        0,
    )
    .unwrap()
    .unwrap();
    assert_eq!(held.state, "pending");
    assert_eq!(held.holding_reason.as_deref(), Some("local_wall"));
    let mut hosted = request(&c, &d_registration);
    hosted.local_submitter = None;
    hosted.requester_session_id = None;
    hosted.env.clear();
    hosted.script = Some("print hosted-wall-recovery-ok".into());
    let hosted = RetainedQueueStore::create_queue_job_in_state_dir(&queue_state, hosted).unwrap();
    RetainedQueueStore::admit_queue_jobs_in_state_dir(&queue_state, &root.join("messages.db"), 0)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let result = RetainedQueueStore::get_queue_job_strict_from_path(
            &queue_state.join("queue_runner.db"),
            &hosted.id,
        )
        .unwrap()
        .unwrap();
        if result.state != "running" && result.state != "pending" {
            let output = fs::read_to_string(result.log_path.unwrap()).unwrap();
            assert_eq!(result.state, "succeeded", "{output}");
            assert!(output.contains("hosted-wall-recovery-ok"), "{output}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "hosted job blocked by restoration"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(c);
    drop(d);
    // An unrelated new host listener must not change the saved profile hash.
    let unrelated = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let original_title = c_registration.title.clone();
    c_registration.title.push_str(" changed");
    assert!(host
        .prepare_for_queue(&c_registration, &queue_state)
        .is_err());
    c_registration.title = original_title;
    let c = host
        .prepare_for_queue(&c_registration, &queue_state)
        .unwrap();
    let d = host
        .prepare_for_queue(&d_registration, &queue_state)
        .unwrap();
    assert_eq!(fs::read(manifest).unwrap(), saved_manifest);
    assert_eq!(fs::read(&c.artifacts.profile).unwrap(), saved_profile);
    let mut holding = request(&c, &d_registration);
    holding.script = None;
    holding.argv = Some(vec![
        c.artifacts.tools["probe"].display().to_string(),
        "hold".into(),
    ]);
    let holding = RetainedQueueStore::create_queue_job_in_state_dir(&queue_state, holding).unwrap();
    let holding_id = holding.id.clone();
    for job in [c_job, d_job, holding, refused] {
        RetainedQueueStore::start_queue_job_in_state_dir(
            &queue_state,
            &root.join("messages.db"),
            &job.id,
            0,
        )
        .unwrap();
        if job.id == holding_id {
            assert!(
                c.detach_queue().is_err(),
                "running queue child must retain launcher"
            );
            assert!(
                c.suspend().is_err(),
                "running queue child must retain active services"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let result = RetainedQueueStore::get_queue_job_strict_from_path(
                &queue_state.join("queue_runner.db"),
                &job.id,
            )
            .unwrap()
            .unwrap();
            if result.state != "running" && result.state != "pending" {
                let output = fs::read_to_string(result.log_path.unwrap()).unwrap();
                assert_eq!(result.state, "succeeded", "{output}");
                assert!(output.contains("durable-wall-ok"), "{output}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "durable queue job did not finish"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let corrupted_job = RetainedQueueStore::create_queue_job_in_state_dir(
        &queue_state,
        request(&c, &d_registration),
    )
    .unwrap();
    let authority_path = host
        .config
        .state_root
        .join(&c.id)
        .join("xdg/config/queue-authority.json");
    let saved_authority = fs::read(&authority_path).unwrap();
    fs::set_permissions(&authority_path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&authority_path, b"{}").unwrap();
    fs::set_permissions(&authority_path, fs::Permissions::from_mode(0o400)).unwrap();
    let corrupted_start = RetainedQueueStore::start_queue_job_in_state_dir(
        &queue_state,
        &root.join("messages.db"),
        &corrupted_job.id,
        0,
    );
    fs::set_permissions(&authority_path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&authority_path, saved_authority).unwrap();
    fs::set_permissions(&authority_path, fs::Permissions::from_mode(0o400)).unwrap();
    assert!(
        corrupted_start.is_err(),
        "changed host authority must fail closed"
    );
    assert_eq!(
        RetainedQueueStore::get_queue_job_strict_from_path(
            &queue_state.join("queue_runner.db"),
            &corrupted_job.id,
        )
        .unwrap()
        .unwrap()
        .state,
        "failed"
    );
    let saved_judge_token = c.environment["LOCAL_JUDGE_TOKEN"].clone();
    let restarted_job = RetainedQueueStore::create_queue_job_in_state_dir(
        &queue_state,
        request(&c, &d_registration),
    )
    .unwrap();
    let restart_input = root.join("restart-input.json");
    let manifest_path = queue_state.join("local-walls/wall-c.json");
    let manifest_before_restart = fs::read(&manifest_path).unwrap();
    fs::write(
        &restart_input,
        serde_json::to_vec(&json!({
            "queue": queue_state, "python": host.config.python,
            "upstream": upstream_address, "model_url": config.local_host.base_url,
            "egress": egress.directory(), "judge": judge.directory(),
            "judge_port": judge.port().unwrap(), "old_peer": c.broker_peer_token().0,
            "job": restarted_job.id,
        }))
        .unwrap(),
    )
    .unwrap();
    c.suspend().unwrap();
    d.suspend().unwrap();
    drop(c);
    drop(d);
    let restarted = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "local_wall::recovery::tests::restore_child",
            "--ignored",
            "--nocapture",
        ])
        .env("SM_WALL_RESTORE_FIXTURE", &restart_input)
        .output()
        .unwrap();
    assert!(
        restarted.status.success(),
        "restart stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&restarted.stdout),
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert_eq!(fs::read(manifest_path).unwrap(), manifest_before_restart);
    let c = host
        .prepare_for_queue(&c_registration, &queue_state)
        .unwrap();
    assert_eq!(c.environment["LOCAL_JUDGE_TOKEN"], saved_judge_token);
    c.suspend().unwrap();
    c.retire_queue().unwrap();
    assert!(
        crate::queue::local_wall::registered_spec(&queue_state, &c.id)
            .unwrap()
            .is_none()
    );
    drop(c);
    drop(unrelated);
    // A host tmux launcher outlives two sm generations, retaining the actual
    // provider root, control listener, test lease and immutable queue manifest.
    let durable = registration("wall-durable", 18504);
    let launch_path = queue_state.join("local-wall-owners/wall-durable/launch.json");
    let hash = format!("{:x}", Sha256::digest(durable.id.as_bytes()));
    let control_socket = host.config.alias_root.join(format!("{}.host", &hash[..16]));
    let installed_fixture = root.join("installed-owner");
    fs::copy(std::env::current_exe().unwrap(), &installed_fixture).unwrap();
    let generation = || {
        Arc::new(
            recovery::GenerationWalls::new(
                queue_state.clone(),
                host.config.python.clone(),
                upstream_address,
                &config.local_host.base_url,
                egress.clone(),
                judge.clone(),
            )
            .unwrap(),
        )
    };
    let first = generation();
    let launch = first
        .stage_provider(
            host.config.clone(),
            durable,
            owner::ProviderLaunch {
                tool: "probe".into(),
                arguments: vec![
                    "provider-persistent".into(),
                    launch_path.clone().into_os_string(),
                    control_socket.into_os_string(),
                ],
                settings: BTreeMap::new(),
            },
            &installed_fixture,
        )
        .unwrap();
    let owner_output = root.join("owner.log");
    struct OwnerGuard {
        client: owner::OwnerClient,
        child: std::process::Child,
    }
    impl Drop for OwnerGuard {
        fn drop(&mut self) {
            let _ = self.client.retire();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    let mut owner_process = OwnerGuard {
        client: launch.client.clone(),
        child: Command::new(&launch.executable)
            .args([
                "--exact",
                "local_wall::owner::tests::owner_child",
                "--ignored",
                "--nocapture",
            ])
            .env("SM_WALL_OWNER_FIXTURE", &launch_path)
            .stdout(File::create(&owner_output).unwrap())
            .stderr(File::create(root.join("owner.err")).unwrap())
            .spawn()
            .unwrap(),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if launch.client.ready(&queue_state).is_ok()
            && fs::read_to_string(&owner_output)
                .unwrap()
                .contains("owner-provider-ready")
        {
            break;
        }
        assert!(
            owner_process.child.try_wait().unwrap().is_none(),
            "owner exited: {}",
            fs::read_to_string(root.join("owner.err")).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "owner not ready: {}",
            fs::read_to_string(root.join("owner.err")).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let output = fs::read_to_string(&owner_output).unwrap();
    let fields = output
        .lines()
        .find(|line| line.starts_with("owner-provider-ready "))
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>();
    let test_port: u16 = fields[1].parse().unwrap();
    let descendant =
        crate::local_sockets::identity::ProcessIdentity::capture(fields[2].parse().unwrap())
            .unwrap();
    let verify_provider = || {
        for (port, expected) in [(18504, &b"provider"[..]), (test_port, &b"socket"[..])] {
            let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = vec![0; expected.len()];
            stream.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, expected);
        }
    };
    verify_provider();
    let info = launch.client.info_for_test();
    let spec = crate::queue::local_wall::registered_spec(&queue_state, "wall-durable")
        .unwrap()
        .unwrap();
    let manifest_path = queue_state.join("local-walls/wall-durable.json");
    let manifest = fs::read(&manifest_path).unwrap();
    let profile = fs::read(&spec.profile).unwrap();
    let adapter_path = spec.agent_state.join("xdg/config/adapter.dylib");
    let adapter = fs::read(&adapter_path).unwrap();
    first.reconcile().unwrap();
    assert!(first.get_durable("wall-durable").unwrap().is_some());
    first.stop().unwrap();
    drop(first);
    verify_provider();
    assert!(descendant.is_live());
    let mut queue_request = crate::queue::CreateQueueJob {
        local_submitter: Some(crate::local_egress::gateway::VerifiedLocalAgent::test_identity("wall-durable")),
        job_type: "tests".into(), label: "owner-recovery".into(), requester_session_id: None,
        notify_session_id: "wall-durable".into(), cwd: spec.checkout.display().to_string(), argv: None,
        script: Some(format!("'{}' -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\",0)); s.listen(); c=socket.create_connection(s.getsockname()); a,_=s.accept(); c.sendall(b\"ok\"); assert a.recv(2)==b\"ok\"' || exit 1\nprint owner-queue-ok", host.config.python.display())),
        env: BTreeMap::new(), timeout_seconds: 20, cpu_percent: None, gpu_percent: None, memory_bytes: None, rank_tickets: None,
    };
    let job =
        RetainedQueueStore::create_queue_job_in_state_dir(&queue_state, queue_request.clone())
            .unwrap();
    let recovered = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "local_wall::owner::tests::recovered_generation_child",
            "--ignored",
            "--nocapture",
        ])
        .env("SM_WALL_OWNER_FIXTURE", &launch_path)
        .env("SM_WALL_OWNER_JOB", &job.id)
        .output()
        .unwrap();
    assert!(
        recovered.status.success(),
        "recovery stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&recovered.stdout),
        String::from_utf8_lossy(&recovered.stderr)
    );
    verify_provider();
    assert!(descendant.is_live());
    assert_eq!(launch.client.info_for_test(), info);
    assert_eq!(fs::read(&manifest_path).unwrap(), manifest);
    assert_eq!(fs::read(&spec.profile).unwrap(), profile);
    assert_eq!(fs::read(&adapter_path).unwrap(), adapter);
    queue_request.script = Some(format!(
        "'{}' -c 'import time; time.sleep(60)'",
        host.config.python.display()
    ));
    let abandoned =
        RetainedQueueStore::create_queue_job_in_state_dir(&queue_state, queue_request).unwrap();
    let crashed = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "local_wall::owner::tests::recovered_generation_child",
            "--ignored",
            "--nocapture",
        ])
        .env("SM_WALL_OWNER_FIXTURE", &launch_path)
        .env("SM_WALL_OWNER_JOB", &abandoned.id)
        .env("SM_WALL_CRASH_AFTER_LAUNCH", "1")
        .output()
        .unwrap();
    assert!(
        crashed.status.success(),
        "{}",
        String::from_utf8_lossy(&crashed.stderr)
    );
    let abandoned = RetainedQueueStore::get_queue_job_strict_from_path(
        &queue_state.join("queue_runner.db"),
        &abandoned.id,
    )
    .unwrap()
    .unwrap();
    assert_eq!(abandoned.state, "running");
    let pgid = i32::try_from(abandoned.process_group_id.unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(-pgid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "abandoned queue process group survived the sm crash"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    RetainedQueueStore::cancel_queue_job_in_state_dir(
        &queue_state,
        &queue_state.join("messages.db"),
        &abandoned.id,
        0,
        crate::queue::QueueAdmissionPolicy::default(),
        false,
    )
    .unwrap();
    verify_provider();
    assert!(descendant.is_live());
    launch.client.retire().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while descendant.is_live() {
        assert!(
            Instant::now() < deadline,
            "retired provider descendant survived"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!egress.registration("wall-durable").unwrap().unwrap().active);
    assert!(launch.client.ready(&queue_state).is_err());
    assert!(generation()
        .reconcile()
        .unwrap()
        .iter()
        .any(|failure| failure.starts_with("wall-durable:")));
    assert!(owner_process.child.wait().unwrap().success());
    drop(owner_process);
    // Simulate durable activation followed by a lost registration reply.
    let uncertain = root.join("uncertain-egress");
    private_directory(&uncertain).unwrap();
    let socket = std::os::unix::net::UnixListener::bind(uncertain.join("control.sock")).unwrap();
    socket.set_nonblocking(true).unwrap();
    let record_path = uncertain.join("active");
    let thread_record = record_path.clone();
    let daemon = thread::spawn(move || {
        use std::io::BufRead;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "missing rollback request");
            let (mut stream, _) = match socket.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            // Readiness probes close immediately; Darwin can reject a timeout
            // option on their disconnected sockets. Bound readiness with poll.
            let mut descriptor = libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert!(unsafe { libc::poll(&mut descriptor, 1, 5000) } > 0);
            let mut request = String::new();
            std::io::BufReader::new(&mut stream)
                .read_line(&mut request)
                .unwrap();
            if request.is_empty() {
                continue;
            }
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            if request.get("RegisterGateway").is_some() {
                fs::write(&thread_record, "true").unwrap();
                // Close without the committed registration's reply.
            } else {
                assert!(request.get("Unregister").is_some(), "{request}");
                fs::write(&thread_record, "false").unwrap();
                stream
                    .write_all(b"{\"registration\":null,\"error\":null}\n")
                    .unwrap();
                break;
            }
        }
    });
    let uncertain_host = LocalWallRuntime::new(
        host.config.clone(),
        ServiceClient::new(uncertain, std::env::current_exe().unwrap()),
        judge.clone(),
    )
    .unwrap();
    assert!(uncertain_host
        .prepare(&registration("wall-uncertain", 18504))
        .is_err());
    daemon.join().unwrap();
    assert_eq!(fs::read_to_string(record_path).unwrap(), "false");
    drop(uncertain_host);
    drop(host);
    drop(fixture);
}
