use super::*;
use crate::local_sockets::launch::LaunchBinding;
use std::{ffi::OsString, process::Command};

pub(crate) struct PreparedLaunch {
    _directory: TestDirectory,
    service: Arc<AgentService>,
    binding: LaunchBinding,
    paths: serde_json::Value,
    pub(crate) environment: Vec<(OsString, OsString)>,
    control_port: u16,
}

pub(crate) fn prepare_launch() -> PreparedLaunch {
    prepare_launch_for("launch", 18600, 18700)
}

pub(crate) fn prepare_launch_for(agent: &str, gateway: u16, egress: u16) -> PreparedLaunch {
    prepare_launch_for_inner(agent, gateway, egress, None)
}

fn prepare_launch_for_inner(
    agent: &str,
    gateway: u16,
    egress: u16,
    opencode: Option<&Path>,
) -> PreparedLaunch {
    let directory = TestDirectory::new();
    let broker = directory.path().join("h/s/a/tmp/b");
    fs::create_dir_all(&broker).unwrap();
    fs::set_permissions(&broker, fs::Permissions::from_mode(0o700)).unwrap();
    let control = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = control.local_addr().unwrap().port();
    let hub = BrokerHub::new(
        PortPolicy::new(PortConfiguration {
            agent_control: port..=port,
            gateway: gateway..=gateway,
            egress: egress..=egress,
            model: 24000,
            judge: 24001,
        })
        .unwrap(),
    );
    let service = Arc::new(
        hub.register_agent(agent, port, &broker, &broker.join("s"))
            .unwrap(),
    );
    service
        .state
        .control
        .lock()
        .unwrap()
        .push(control.try_clone().unwrap());
    let scripts = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/local-wall");
    let mut preparation = Command::new("python3");
    preparation.arg(scripts.join("test_launch.py"));
    if let Some(source) = opencode {
        preparation.arg("--opencode-binary").arg(source);
    }
    let preparation = preparation
        .arg("--root")
        .arg(directory.path())
        .arg("--control-port")
        .arg(port.to_string())
        .args([
            "--gateway-port",
            &gateway.to_string(),
            "--egress-port",
            &egress.to_string(),
        ])
        .arg("--peer-token")
        .args(service.peer_token().0.map(|word| word.to_string()))
        .output()
        .unwrap();
    assert!(
        preparation.status.success(),
        "{}",
        String::from_utf8_lossy(&preparation.stderr)
    );
    let prepared: serde_json::Value = serde_json::from_slice(&preparation.stdout).unwrap();
    let path = |key: &str| PathBuf::from(prepared[key].as_str().unwrap());
    let binding = LaunchBinding::new(
        service.clone(),
        Some(control),
        &path("profile"),
        &path("adapter"),
        &path("supervisor"),
    )
    .unwrap();
    let environment: Vec<(OsString, OsString)> = prepared["environment"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.into(), value.as_str().unwrap().into()))
        .collect();
    PreparedLaunch {
        _directory: directory,
        service,
        binding,
        paths: prepared,
        environment,
        control_port: port,
    }
}

impl PreparedLaunch {
    pub(crate) fn queue_binding(&self) -> LaunchBinding {
        LaunchBinding::new(
            self.service.clone(),
            None,
            &self.path("profile"),
            &self.path("adapter"),
            &self.path("supervisor"),
        )
        .unwrap()
    }
    pub(crate) fn path(&self, key: &str) -> PathBuf {
        PathBuf::from(self.paths[key].as_str().unwrap())
    }
    fn run(&self, executable: &Path, arguments: &[OsString], environment: &[(OsString, OsString)]) {
        let mut child = self
            .queue_binding()
            .spawn(executable, arguments, environment, &self.path("checkout"))
            .unwrap();
        let mut stdout = child.take_stdout().unwrap();
        let mut stderr = child.take_stderr().unwrap();
        let out = thread::spawn(move || {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut stdout, &mut text).unwrap();
            text
        });
        let err = thread::spawn(move || {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
            text
        });
        let status = child.wait().unwrap();
        assert!(
            status.success(),
            "{executable:?}: {status}\n{}\n{}",
            out.join().unwrap(),
            err.join().unwrap()
        );
        assert_eq!(child.try_wait().unwrap(), Some(status));
    }
}

#[test]
fn production_wall_launch_registers_before_socket_use_and_restores() {
    let prepared = prepare_launch();
    let path = |key| prepared.path(key);
    let run = |executable: &Path, arguments: &[OsString], environment: &[(OsString, OsString)]| {
        prepared.run(executable, arguments, environment)
    };
    let environment = &prepared.environment;
    run(&path("application"), &[], environment);
    run(
        &path("application"),
        &["queue-capabilities".into()],
        environment,
    );
    // New host registrations model queue jobs and restored launches. Neither
    // a missing requester ID nor forged caller metadata supplies authority.
    for forged in [false, true] {
        let mut environment = environment.to_vec();
        if forged {
            environment.extend([
                ("CLAUDE_SESSION_MANAGER_ID".into(), "another-agent".into()),
                ("SM_BROKER_ENDPOINT".into(), "/tmp/forged".into()),
                ("DYLD_INSERT_LIBRARIES".into(), "/tmp/forged.dylib".into()),
            ]);
        }
        run(&path("python"), &["-c".into(),
            "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); c=socket.create_connection(s.getsockname()); c.sendall(b'ok'); a,_=s.accept(); assert a.recv(2)==b'ok'; a.close(); c.close(); s.close()".into()], &environment);
    }
    // Knowing the endpoint is insufficient for an unregistered host process.
    let stream = UnixStream::connect(prepared.service.endpoint()).unwrap();
    assert!(prepared
        .service
        .state
        .authority
        .authorize("launch", &stream)
        .is_err());
    use std::io::{BufRead, BufReader};
    let mut child = prepared.binding.spawn(&path("python"), &["-c".into(),
        "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); print(s.getsockname()[1],flush=True); time.sleep(60)".into()],
        &prepared.environment, &path("checkout")).unwrap();
    let mut line = String::new();
    BufReader::new(child.take_stdout().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port: u16 = line.trim().parse().unwrap();
    let root = ProcessIdentity::capture(child.id()).unwrap();
    drop(child);
    assert!(!root.is_live());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(_) => break,
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
                assert!(
                    Instant::now() < deadline,
                    "cancelled listener remained allocated"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    let alias = path("checkout").join("adapter-alias");
    fs::hard_link(path("adapter"), &alias).unwrap();
    let control = prepared.service.state.control.lock().unwrap()[0]
        .try_clone()
        .unwrap();
    assert!(LaunchBinding::new(
        prepared.service.clone(),
        Some(control),
        &path("profile"),
        &path("adapter"),
        &path("supervisor")
    )
    .is_err());
    fs::remove_file(alias).unwrap();
    // A child whose parent completes stays in the kernel-confined launch
    // group even after attempting to daemonize; completion must remove it.
    let script = "import os,socket,time\ns=socket.socket(); s.bind(('127.0.0.1',0)); s.listen()\nr,w=os.pipe()\npid=os.fork()\nif pid == 0:\n os.close(r)\n try: os.setsid(); raise AssertionError('detached')\n except PermissionError: pass\n print(os.getpid(),s.getsockname()[1],flush=True)\n os.write(w,b'R'); os.close(w); time.sleep(60)\nelse:\n os.close(w); assert os.read(r,1)==b'R'; os._exit(0)";
    let mut child = prepared
        .queue_binding()
        .spawn(
            &path("python"),
            &["-c".into(), script.into()],
            &prepared.environment,
            &path("checkout"),
        )
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.take_stdout().unwrap())
        .read_line(&mut line)
        .unwrap();
    let parts: Vec<_> = line.split_whitespace().collect();
    let descendant = ProcessIdentity::capture(parts[0].parse().unwrap()).unwrap();
    let port: u16 = parts[1].parse().unwrap();
    assert!(child.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "orphan retained its listener");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !descendant.is_live(),
        "orphan still runs after launch completion"
    );
}

#[test]
#[ignore = "requires installed pinned opencode 1.17.9"]
fn production_wall_pinned_opencode_serves_authenticated_health() {
    use std::io::{Read, Write};
    let source = Path::new("/opt/homebrew/bin/opencode")
        .canonicalize()
        .unwrap();
    let version = Command::new(&source).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(String::from_utf8(version.stdout).unwrap().trim(), "1.17.9");
    let prepared = prepare_launch_for_inner("launch", 18600, 18700, Some(&source));
    let checkout = prepared.path("checkout");
    let git = Command::new("/usr/bin/git")
        .args(["init", "--quiet"])
        .current_dir(&checkout)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(git.status.success());
    let executable = prepared.path("opencode");
    let config = prepared.paths["environment"]["XDG_CONFIG_HOME"]
        .as_str()
        .unwrap();
    fs::create_dir_all(Path::new(config).join("opencode")).unwrap();
    fs::write(
        Path::new(config).join("opencode/.gitignore"),
        "node_modules\npackage.json\npackage-lock.json\nbun.lock\n.gitignore\n",
    )
    .unwrap();
    fs::write(
        Path::new(config).join("opencode/opencode.json"),
        serde_json::to_vec(&serde_json::json!({
            "autoupdate": false, "snapshot": false, "share": "disabled", "plugin": [],
            "shell": prepared.path("shell")
        }))
        .unwrap(),
    )
    .unwrap();
    let mut environment = prepared.environment.clone();
    environment.extend([
        ("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()),
        ("OPENCODE_DISABLE_MODELS_FETCH".into(), "1".into()),
        ("OPENCODE_DISABLE_LSP_DOWNLOAD".into(), "1".into()),
        ("OPENCODE_SERVER_PASSWORD".into(), "fixture-password".into()),
    ]);
    let mut child = prepared
        .binding
        .spawn(
            &executable,
            &[
                "serve".into(),
                "--hostname".into(),
                "127.0.0.1".into(),
                "--port".into(),
                prepared.control_port.to_string().into(),
                "--print-logs".into(),
                "--log-level".into(),
                "INFO".into(),
            ],
            &environment,
            &prepared.path("checkout"),
        )
        .unwrap();
    let mut stdout = child.take_stdout().unwrap();
    let out = thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).unwrap();
        text
    });
    let mut stderr = child.take_stderr().unwrap();
    let err = thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).unwrap();
        text
    });
    let request = |authenticated: bool, path: &str| -> io::Result<String> {
        let mut connection = TcpStream::connect((Ipv4Addr::LOCALHOST, prepared.control_port))?;
        connection.set_read_timeout(Some(Duration::from_secs(1)))?;
        let authorization = if authenticated {
            "Authorization: Basic b3BlbmNvZGU6Zml4dHVyZS1wYXNzd29yZA==\r\n"
        } else {
            ""
        };
        connection.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{authorization}Connection: close\r\n\r\n").as_bytes())?;
        let mut response = String::new();
        connection.read_to_string(&mut response)?;
        Ok(response)
    };
    let deadline = Instant::now() + Duration::from_secs(25);
    let response = loop {
        if let Ok(response) = request(true, "/global/health") {
            break response;
        }
        if child.try_wait().unwrap().is_some() || Instant::now() > deadline {
            drop(child);
            panic!("opencode did not start: {}", err.join().unwrap());
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        response.contains(r#""healthy":true"#) && response.contains("1.17.9"),
        "{response}"
    );
    let rejected = request(false, "/global/health").unwrap();
    assert!(rejected.starts_with("HTTP/1.1 401"), "{rejected}");
    // Health bypasses project initialization. Production readiness also reads
    // conversation status, which runs native Git subprocesses in the checkout.
    let client = crate::opencode::Client::new(
        prepared.control_port,
        "fixture-password",
        Duration::from_secs(10),
    )
    .unwrap();
    let readiness = client.ready();
    let project_status = (|| -> anyhow::Result<String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        let mut response = agent
            .get(&format!(
                "http://127.0.0.1:{}/session/status",
                prepared.control_port
            ))
            .header(
                "Authorization",
                "Basic b3BlbmNvZGU6Zml4dHVyZS1wYXNzd29yZA==",
            )
            .call()?;
        Ok(format!(
            "{}; {}",
            response.status(),
            response.body_mut().read_to_string()?
        ))
    })();
    let conversation = if matches!(readiness, Ok(true)) {
        Some(client.create_conversation("contained native project"))
    } else {
        None
    };
    drop(child);
    let stdout = out.join().unwrap();
    let stderr = err.join().unwrap();
    assert!(
        matches!(readiness, Ok(true)),
        "{readiness:?}; {project_status:?}; {stdout}; {stderr}"
    );
    assert!(
        matches!(conversation, Some(Ok(_))),
        "{conversation:?}; {stderr}"
    );
    let mut environment = prepared.environment.clone();
    environment.push(("BUN_BE_BUN".into(), "1".into()));
    prepared.run(
        &executable,
        &[
            "-e".into(),
            include_str!("../../../../../../scripts/local-wall/native/test_opencode_spawn.cjs")
                .into(),
        ],
        &environment,
    );
    assert_eq!(
        fs::read_to_string(checkout.join("native-tool.txt")).unwrap(),
        "native-tool"
    );
    let cancelled: u32 = fs::read_to_string(checkout.join("native-cancel-pid.txt"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        ProcessIdentity::capture(cancelled).is_err(),
        "cancelled native child survived"
    );
    // The same adapter still refuses new groups from other executable images.
    prepared.run(&prepared.path("application"), &[], &prepared.environment);
}

#[test]
#[ignore = "requires prebuilt read_only_http test binary in SM_WALL_HTTP_TEST_BINARY"]
fn production_wall_runs_existing_http_and_queue_socket_fixtures() {
    let prepared = prepare_launch();
    let http = prepared.path("application").with_file_name("http-tests");
    let unit = prepared.path("application").with_file_name("unit-tests");
    fs::copy(std::env::var_os("SM_WALL_HTTP_TEST_BINARY").unwrap(), &http).unwrap();
    fs::copy(std::env::current_exe().unwrap(), &unit).unwrap();
    prepared.run(
        &http,
        &[
            "registered_email_send_posts_resend_payload_with_routing_footer".into(),
            "--exact".into(),
            "--nocapture".into(),
        ],
        &prepared.environment,
    );
    for forged in [false, true] {
        let mut environment = prepared.environment.clone();
        if forged {
            environment.push(("CLAUDE_SESSION_MANAGER_ID".into(), "another-agent".into()));
            environment.push(("SM_QUEUE_REQUESTER".into(), "another-agent".into()));
        }
        prepared.run(
            &unit,
            &[
                "queue::tests::queue_job_does_not_inherit_handed_over_listeners".into(),
                "--exact".into(),
                "--nocapture".into(),
            ],
            &environment,
        );
    }
}
