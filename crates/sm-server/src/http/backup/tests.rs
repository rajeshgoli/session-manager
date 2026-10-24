use super::*;
use crate::managed_artifacts::{config::ArtifactRoot, moves};
use tower::ServiceExt;
struct Fixture {
    dir: PathBuf,
    state: AppState,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sm-backup-http-{}-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for name in ["root", "cold", "data"] {
            fs::create_dir_all(dir.join(name)).unwrap();
        }
        let dir = fs::canonicalize(dir).unwrap();
        let mut config = super::super::tests::cloudflare_access_config();
        config.paths.state_file = dir.join("sessions.json").display().to_string();
        config.sm_send.db_path = dir.join("messages.db").display().to_string();
        config.managed_artifacts.enabled = true;
        config.managed_artifacts.db_path = dir.join("artifacts.db").display().to_string();
        config.managed_artifacts.roots = vec![ArtifactRoot {
            path: dir.join("root").display().to_string(),
            cold_store: dir.join("cold").display().to_string(),
            repo: "owner/repo".into(),
            ..Default::default()
        }];
        config.managed_artifacts.backup.enabled = true;
        config.managed_artifacts.backup.outbox = dir.join("outbox").display().to_string();
        config.managed_artifacts.backup.static_paths = vec![dir.join("data").display().to_string()];
        config.managed_artifacts.backup.plain_paths =
            config.managed_artifacts.backup.static_paths.clone();
        config.managed_artifacts.backup.puller_devices =
            vec!["backup-mini".into(), "other-mini".into()];
        config.managed_artifacts.backup.cli_dir = dir.join("cli").display().to_string();
        let state = AppState::new(config);
        super::super::tests::seed_cloudflare_access_jwks(&state);
        Self { dir, state }
    }
    fn req(
        &self,
        method: &str,
        path: &str,
        name: Option<&str>,
        host: &str,
        body: Value,
    ) -> Request {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, host)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        if let Some(name) = name {
            req.headers_mut().insert(
                "cf-access-jwt-assertion",
                super::super::tests::test_mobile_access_assertion(name)
                    .parse()
                    .unwrap(),
            );
        }
        req
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        moves::TEST_MOUNTED.set(false);
        let _ = fs::remove_dir_all(&self.dir);
    }
}
#[tokio::test]
async fn authentication_is_mutual_and_has_no_local_bypass() {
    let f = Fixture::new();
    let app = router(f.state.clone());
    for (path, name, host, expected) in [
        ("/backup/status", None, "localhost", StatusCode::FORBIDDEN),
        (
            "/backup/status",
            Some("phone"),
            "sm-app.example.com",
            StatusCode::FORBIDDEN,
        ),
        (
            "/backup/status",
            Some("backup-mini"),
            "sm.example.com",
            StatusCode::FORBIDDEN,
        ),
        (
            "/backup/status",
            Some("backup-mini"),
            "sm-app.example.com",
            StatusCode::OK,
        ),
        (
            "/health",
            Some("backup-mini"),
            "sm-app.example.com",
            StatusCode::FORBIDDEN,
        ),
        (
            "/health",
            Some("backup-mini"),
            "localhost",
            StatusCode::FORBIDDEN,
        ),
        ("/health", None, "localhost", StatusCode::OK),
        (
            "/backup/allow-deletes",
            Some("backup-mini"),
            "sm-app.example.com",
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(f.req("GET", path, name, host, json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path} {host}");
    }
    let response = terminal_lan_router(Arc::new(f.state.clone()))
        .oneshot(f.req(
            "GET",
            "/client/terminal/probe",
            Some("backup-mini"),
            "sm-app.example.com",
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn http_lifecycle_owns_jobs_checks_device_and_serves_ranges_only_ready() {
    let f = Fixture::new();
    let app = router(f.state.clone());
    fs::write(f.dir.join("data/prices"), b"prices and more prices").unwrap();
    let send = |method: &str, path: &str, device: &str, body: Value| {
        f.req(method, path, Some(device), "sm-app.example.com", body)
    };
    let response = app
        .clone()
        .oneshot(send(
            "POST",
            "/backup/requests",
            "backup-mini",
            json!({"have":null}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let id = body["id"].as_str().unwrap();
    let manager = f.state.managed_artifacts.as_ref().unwrap();
    let queue = expand_home(&f.state.config.queue_runner_state_dir().to_string_lossy());
    let job = RetainedQueueStore::owned_queue_job(&queue, &format!("backup:{id}"))
        .unwrap()
        .unwrap();
    assert_eq!(job.job_type, "background");
    let url = format!("/backup/requests/{id}/parts/0");
    assert_eq!(
        app.clone()
            .oneshot(send("GET", &url, "backup-mini", json!({})))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    moves::TEST_MOUNTED.set(true);
    life::prepare_request(manager, id).unwrap();
    assert_eq!(
        app.clone()
            .oneshot(send("GET", &url, "other-mini", json!({})))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut req = send("GET", &url, "backup-mini", json!({}));
    req.headers_mut()
        .insert("range", "bytes=2-7".parse().unwrap());
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let part = fs::read(f.dir.join("outbox").join(id).join("part-0000.tar.zst")).unwrap();
    assert_eq!(&bytes[..], &part[2..8]);
    let ack = json!({"applied":true,"fetched_bytes":part.len(),"fetch_seconds":1.0,"apply_seconds":1.0,"held_deletions":0});
    assert_eq!(
        app.clone()
            .oneshot(send(
                "POST",
                &format!("/backup/requests/{id}/ack"),
                "backup-mini",
                ack
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(send("GET", &url, "backup-mini", json!({})))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.clone()
            .oneshot(send(
                "POST",
                "/backup/requests",
                "backup-mini",
                json!({"have":null})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
}
#[test]
fn range_parser_handles_resume_suffix_and_invalid_requests() {
    assert_eq!(byte_range(Some("bytes=5-"), 10).unwrap(), (5, 5, true));
    assert_eq!(byte_range(Some("bytes=-20"), 10).unwrap(), (0, 10, true));
    for value in ["bytes=10-", "bytes=-0", "bytes=2-1", "bytes=0-1,3-4", "bad"] {
        assert!(byte_range(Some(value), 10).is_err());
    }
}

#[tokio::test]
async fn probes_are_streamed_bounded_and_recorded_only_when_complete() {
    let f = Fixture::new();
    let app = router(f.state.clone());
    let short = f.req(
        "POST",
        "/backup/probe",
        Some("backup-mini"),
        "sm-app.example.com",
        json!({}),
    );
    assert_eq!(
        app.clone().oneshot(short).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let mut request = f.req(
        "POST",
        "/backup/probe",
        Some("backup-mini"),
        "sm-app.example.com",
        json!({}),
    );
    let chunk = Bytes::from(vec![0u8; 1024 * 1024]);
    *request.body_mut() = Body::from_stream(stream::iter(
        (0..256).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
    ));
    assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    let conn = f
        .state
        .managed_artifacts
        .as_ref()
        .unwrap()
        .store
        .open()
        .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM backup_rate_samples WHERE kind='probe'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn reconciliation_recovers_job_creation_and_terminal_failure_without_duplicates() {
    let f = Fixture::new();
    let manager = f.state.managed_artifacts.as_ref().unwrap();
    let life::RequestResult::Open(request) = life::request(manager, "backup-mini", None).unwrap()
    else {
        panic!()
    };
    let queue = expand_home(&f.state.config.queue_runner_state_dir().to_string_lossy());
    let executable = std::env::current_exe().unwrap();
    life::schedule(manager, &queue, &executable).unwrap();
    let owner = format!("backup:{}", request.id);
    let job = RetainedQueueStore::owned_queue_job(&queue, &owner)
        .unwrap()
        .unwrap();
    // Simulate the crash window after queue commit but before linking the job.
    manager
        .store
        .open()
        .unwrap()
        .execute("UPDATE backup_requests SET queue_job_id=NULL", [])
        .unwrap();
    life::schedule(manager, &queue, &executable).unwrap();
    assert_eq!(
        RetainedQueueStore::owned_queue_job(&queue, &owner)
            .unwrap()
            .unwrap()
            .id,
        job.id
    );
    let conn = rusqlite::Connection::open(queue.join("queue_runner.db")).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM queue_jobs", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    conn.execute("UPDATE queue_jobs SET state='failed' WHERE id=?1", [job.id])
        .unwrap();
    life::schedule(manager, &queue, &executable).unwrap();
    assert_eq!(
        life::status(manager, "backup-mini", &request.id)
            .unwrap()
            .state,
        "failed"
    );
    assert!(bk::confirmed(&manager.store.open().unwrap(), "backup-mini")
        .unwrap()
        .is_none());
}

#[test]
fn viewed_blocking_backup_alert_remains_actionable() {
    let f = Fixture::new();
    let manager = f.state.managed_artifacts.as_ref().unwrap();
    manager.store.open().unwrap().execute("INSERT INTO backup_alerts(key,title,body,blocking) VALUES('test-corruption','Backup integrity check failed','Bad file: prices',1)",[]).unwrap();
    deliver_alerts(&f.state, manager).unwrap();
    let store = messages::owner_message_store(&f.state);
    let message = store
        .all()
        .unwrap()
        .into_iter()
        .find(|m| m.title == "Backup integrity check failed")
        .unwrap();
    assert_eq!(message.sender_session_id, "sm-artifacts");
    let viewed = store.mark_viewed(&message.id).unwrap().unwrap();
    assert_eq!(
        crate::owner_messages::derive_message_state(&viewed, false, true),
        crate::owner_messages::OwnerMessageState::NeedsYou
    );
}

#[tokio::test]
async fn ready_parts_use_persisted_outbox_after_configuration_change() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("data/prices"), b"prices").unwrap();
    let manager = f.state.managed_artifacts.as_ref().unwrap();
    let life::RequestResult::Open(request) = life::request(manager, "backup-mini", None).unwrap()
    else {
        panic!()
    };
    moves::TEST_MOUNTED.set(true);
    life::prepare_request(manager, &request.id).unwrap();
    let original = fs::read(
        f.dir
            .join("outbox")
            .join(&request.id)
            .join("part-0000.tar.zst"),
    )
    .unwrap();
    // A restarted server uses the new config while the ready request keeps its old directory.
    f.state
        .managed_artifacts
        .as_mut()
        .unwrap()
        .config
        .backup
        .outbox = f.dir.join("new-outbox").display().to_string();
    let response = router(f.state.clone())
        .oneshot(f.req(
            "GET",
            &format!("/backup/requests/{}/parts/0", request.id),
            Some("backup-mini"),
            "sm-app.example.com",
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .as_ref(),
        original.as_slice()
    );
}

/// A mutual-TLS listener in front of the router, standing in for the Access edge,
/// and an agent holding the pretend mini's device certificate.
struct Tls {
    base: String,
    agent: ureq::Agent,
    handle: axum_server::Handle<SocketAddr>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}
fn serve_tls(f: &Fixture) -> Tls {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    use crate::device_identity::{openssl, Identity};
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let ca_key = f.dir.join("ca.key");
    let ca_path = f.dir.join("ca.pem");
    let ca = openssl(
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            ca_key.to_str().unwrap(),
            "-subj",
            "/CN=temporary-backup-test-ca",
            "-days",
            "1",
        ],
        "",
    )
    .unwrap();
    fs::write(&ca_path, &ca).unwrap();
    let server_key = f.dir.join("server.key");
    let csr = openssl(
        &[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            server_key.to_str().unwrap(),
            "-subj",
            "/CN=localhost",
        ],
        "",
    )
    .unwrap();
    let extensions = f.dir.join("server.ext");
    fs::write(
        &extensions,
        "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n",
    )
    .unwrap();
    let server_cert = openssl(
        &[
            "x509",
            "-req",
            "-CA",
            ca_path.to_str().unwrap(),
            "-CAkey",
            ca_key.to_str().unwrap(),
            "-set_serial",
            "10",
            "-days",
            "1",
            "-extfile",
            extensions.to_str().unwrap(),
        ],
        &csr,
    )
    .unwrap();
    let identity_dir = f.dir.join("identity");
    let identity = Identity::open(&identity_dir, "backup-mini", true).unwrap();
    let client_cert = openssl(
        &[
            "x509",
            "-req",
            "-CA",
            ca_path.to_str().unwrap(),
            "-CAkey",
            ca_key.to_str().unwrap(),
            "-set_serial",
            "11",
            "-days",
            "1",
        ],
        &identity.csr().unwrap(),
    )
    .unwrap();
    identity.install(&format!("{client_cert}\n{ca}")).unwrap();
    drop(identity);
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(ca.as_bytes()).unwrap())
        .unwrap();
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .unwrap();
    let tls = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(server_cert.as_bytes()).unwrap()],
            PrivateKeyDer::from_pem_slice(&fs::read(server_key).unwrap()).unwrap(),
        )
        .unwrap();
    // Emulate only the trusted Access edge: TLS requires the temporary client
    // certificate; the real application still verifies the signed assertion.
    let assertion = super::super::tests::test_mobile_access_assertion("backup-mini");
    let app = router(f.state.clone()).layer(axum::middleware::from_fn(
        move |mut req: Request, next: axum::middleware::Next| {
            let assertion = assertion.clone();
            async move {
                req.headers_mut()
                    .insert(HOST, "sm-app.example.com".parse().unwrap());
                req.headers_mut()
                    .insert("cf-access-jwt-assertion", assertion.parse().unwrap());
                next.run(req).await
            }
        },
    ));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = axum_server::Handle::new();
    let server = axum_server::from_tcp_rustls(
        listener,
        axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls)),
    )
    .unwrap()
    .handle(handle.clone())
    .serve(app.into_make_service_with_connect_info::<SocketAddr>());
    let server = tokio::spawn(server);
    let certs = |pem: &[u8]| {
        ureq::tls::parse_pem(pem)
            .filter_map(|item| match item.unwrap() {
                ureq::tls::PemItem::Certificate(c) => Some(c),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let trusted = ureq::tls::RootCerts::new_with_certs(&certs(ca.as_bytes()));
    let client = ureq::tls::ClientCert::new_with_certs(
        &certs(
            fs::read(identity_dir.join("certificate.pem"))
                .unwrap()
                .as_slice(),
        ),
        ureq::tls::PrivateKey::from_pem(&fs::read(identity_dir.join("private-key.pem")).unwrap())
            .unwrap(),
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(trusted.clone())
                .client_cert(Some(client))
                .build(),
        )
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let base = format!("https://127.0.0.1:{}", address.port());
    let no_identity: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(trusted).build())
        .build()
        .into();
    assert!(no_identity
        .get(format!("{base}/backup/status"))
        .call()
        .is_err());
    Tls {
        base,
        agent,
        handle,
        server,
    }
}
use crate::managed_artifacts::backup::{
    disk,
    pull::{HttpRemote, Remote},
    Part,
};
struct Mini {
    remote: HttpRemote,
    manager: ManagedArtifacts,
    interrupt: bool,
    lose_ack: bool,
    offsets: Vec<u64>,
    /// Backup requests created, to check one per night.
    requests: usize,
}
impl Remote for Mini {
    fn json(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<(u16, Value)> {
        let mut response = self.remote.json(method, path, body)?;
        if method == "POST" && path == "/backup/requests" && response.0 == 202 {
            self.requests += 1;
            let id = response.1["id"].as_str().unwrap();
            life::prepare_request(&self.manager, id)?;
            response = self
                .remote
                .json("GET", &format!("/backup/requests/{id}"), None)?;
        }
        // Pull independently of the host disk free space: the floor is 10% of
        // the real disk, which can exceed what is free.
        if path.starts_with("/backup/requests") && response.1.get("parts").is_some() {
            response.1[disk::FLOOR_KEY] = serde_json::json!(0);
        }
        if path.ends_with("/ack") && self.lose_ack {
            self.lose_ack = false;
            anyhow::bail!("injected lost ack response");
        }
        Ok(response)
    }
    fn download(
        &mut self,
        id: &str,
        part: &Part,
        offset: u64,
        output: &mut dyn std::io::Write,
    ) -> anyhow::Result<()> {
        self.offsets.push(offset);
        if self.interrupt {
            self.interrupt = false;
            let mut bytes = Vec::new();
            self.remote.download(id, part, offset, &mut bytes)?;
            output.write_all(&bytes[..7])?;
            anyhow::bail!("injected interrupted local write");
        }
        self.remote.download(id, part, offset, output)
    }
    fn probe(&mut self) -> anyhow::Result<Value> {
        self.remote.probe()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pretend_mini_mutual_tls_seed_resume_ack_and_restore() {
    use crate::managed_artifacts::backup::{client::Destination, pull, seed};
    let mut f = Fixture::new();
    fs::create_dir(f.dir.join("projects")).unwrap();
    f.state
        .config
        .managed_artifacts
        .backup
        .static_paths
        .push(f.dir.join("projects").display().to_string());
    f.state.managed_artifacts =
        Some(ManagedArtifacts::new(&f.state.config.managed_artifacts).unwrap());
    let Tls {
        base,
        agent,
        handle,
        server,
    } = serve_tls(&f);
    let dir = f.dir.clone();
    let manager = f.state.managed_artifacts.as_ref().unwrap().clone();
    tokio::task::spawn_blocking(move || {
        moves::TEST_MOUNTED.set(true);
        fs::write(dir.join("data/prices"), b"original").unwrap();
        fs::write(dir.join("data/deleted"), b"delete me").unwrap();
        fs::write(dir.join("projects/code"), b"code").unwrap();
        let mut dest = Destination::open(&dir.join("remote")).unwrap();
        let seeded = seed::seed(&manager, "backup-mini", &mut dest, None, false).unwrap();
        fs::write(dir.join("data/prices"), b"replacement").unwrap();
        fs::remove_file(dir.join("data/deleted")).unwrap();
        fs::write(dir.join("projects/code"), b"new code").unwrap();
        life::allow_deletes(&manager, "backup-mini").unwrap();
        let mut mini = Mini {
            remote: HttpRemote { agent, base },
            manager,
            interrupt: true,
            lose_ack: true,
            offsets: Vec::new(),
            requests: 0,
        };
        assert!(pull::pull(&mut dest, &mut mini).is_err());
        assert!(pull::pull(&mut dest, &mut mini).is_err());
        let ack = pull::pull(&mut dest, &mut mini).unwrap();
        assert!(ack.applied);
        assert!(mini.offsets.contains(&7));
        assert_ne!(
            dest.state().unwrap().confirmed.as_deref(),
            Some(seeded.as_str())
        );
        assert!(dest.check().unwrap().ok);
        assert!(!dest.root.join("plain/data/deleted").exists());
        dest.restore(&dir.join("restored"), None, &[]).unwrap();
        assert_eq!(
            fs::read(dir.join("restored/data/prices")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            fs::read(dir.join("restored/projects/code")).unwrap(),
            b"new code"
        );
        assert!(mini.remote.probe().is_ok());
        moves::TEST_MOUNTED.set(false);
    })
    .await
    .unwrap();
    handle.shutdown();
    server.await.unwrap().unwrap();
}

/// Publishes `bytes` as the mini's sm the way the deploy does, or a description
/// whose hash does not match when `corrupt`.
fn publish_cli(root: &std::path::Path, version: &str, bytes: &[u8], corrupt: bool) {
    use sha2::Digest;
    let dir = root.join("cli");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(bk::update::CLI_FILE), bytes).unwrap();
    let mut sha256 = format!("{:x}", sha2::Sha256::digest(bytes));
    if corrupt {
        sha256 = "0".repeat(64);
    }
    fs::write(
        dir.join(bk::update::CLI_JSON),
        json!({"version": version, "sha256": sha256, "bytes": bytes.len()}).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn published_sm_is_served_only_to_puller_devices_with_ranges() {
    let f = Fixture::new();
    let app = router(f.state.clone());
    let get =
        |path: &str, name: Option<&str>| f.req("GET", path, name, "sm-app.example.com", json!({}));
    let response = app
        .clone()
        .oneshot(get("/backup/cli", Some("backup-mini")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"building": false})
    );
    publish_cli(&f.dir, "sm 0.2.0 (new)", b"0123456789", false);
    for (name, host, status) in [
        (None, "sm-app.example.com", StatusCode::FORBIDDEN),
        (Some("phone"), "sm-app.example.com", StatusCode::FORBIDDEN),
        (Some("backup-mini"), "localhost", StatusCode::FORBIDDEN),
        (Some("backup-mini"), "sm-app.example.com", StatusCode::OK),
    ] {
        for path in ["/backup/cli", "/backup/cli/binary"] {
            let response = app
                .clone()
                .oneshot(f.req("GET", path, name, host, json!({})))
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{path} {name:?} {host}");
        }
    }
    let response = app
        .clone()
        .oneshot(get("/backup/cli", Some("backup-mini")))
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let info: bk::update::CliInfo = serde_json::from_slice(&body).unwrap();
    assert_eq!((info.version.as_str(), info.bytes), ("sm 0.2.0 (new)", 10));
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["building"],
        false
    );
    let mut req = get("/backup/cli/binary", Some("backup-mini"));
    req.headers_mut()
        .insert("range", "bytes=4-".parse().unwrap());
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "bytes 4-9/10");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"456789");
    // A binary that does not match its description is being replaced.
    fs::write(f.dir.join("cli").join(bk::update::CLI_FILE), b"short").unwrap();
    let response = app
        .oneshot(get("/backup/cli/binary", Some("backup-mini")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pretend_mini_updates_its_own_sm_then_pulls_and_reports_on_the_backup_card() {
    use crate::managed_artifacts::backup::{
        client::Destination,
        pull, seed,
        update::{self, CliSource, Gate, Outcome, UpdateReport},
    };
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    f.state.managed_artifacts =
        Some(ManagedArtifacts::new(&f.state.config.managed_artifacts).unwrap());
    let Tls {
        base,
        agent,
        handle,
        server,
    } = serve_tls(&f);
    let dir = f.dir.clone();
    let manager = f.state.managed_artifacts.as_ref().unwrap().clone();
    let old = b"#!/bin/sh\necho 'sm 0.1.0 (old)'\n".to_vec();
    let new =
        b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'sm 0.2.0 (new)'; fi\nexit 0\n".to_vec();
    tokio::task::spawn_blocking(move || {
        moves::TEST_MOUNTED.set(true);
        let binary = dir.join("bin/sm");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, &old).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("data/prices"), b"original").unwrap();
        let mut dest = Destination::open(&dir.join("remote")).unwrap();
        seed::seed(&manager, "backup-mini", &mut dest, None, false).unwrap();
        let mut mini = Mini {
            remote: HttpRemote {
                agent,
                base: base.clone(),
            },
            manager: manager.clone(),
            interrupt: false,
            lose_ack: false,
            offsets: Vec::new(),
            requests: 0,
        };
        let gate = Gate {
            api_url: base,
            identity: dir.join("identity"),
            timeout: std::time::Duration::from_secs(30),
            build_wait: std::time::Duration::from_secs(5),
            poll: std::time::Duration::from_millis(50),
        };
        let night = |mini: &mut Mini, dest: &mut Destination, own: &str| {
            let before = mini.requests;
            // As sm backup pull does: wait for tonight's backup, update, download.
            assert!(matches!(
                pull::wait_ready(dest, mini).unwrap(),
                pull::Ready::Ready(_)
            ));
            let outcome = update::update(&binary, own, dest, &mut mini.remote, &gate).unwrap();
            // After an install the new binary (exec with --no-update) resumes the
            // same request: one backup request per night.
            let ack = pull::pull(dest, mini).unwrap();
            assert_eq!(mini.requests, before + 1);
            (outcome, ack)
        };

        // Nothing published: the pull runs and reports no update.
        let (outcome, ack) = night(&mut mini, &mut dest, "sm 0.1.0 (old)");
        assert_eq!(outcome, Outcome::Unchanged);
        assert!(ack.applied && ack.update.is_none());

        publish_cli(&dir, "sm 0.2.0 (new)", &new, true);
        fs::write(dir.join("data/prices"), b"night two").unwrap();
        let (outcome, ack) = night(&mut mini, &mut dest, "sm 0.1.0 (old)");
        let Outcome::Failed(failed) = outcome else {
            panic!("corrupt sm installed")
        };
        assert!(failed.error.as_deref().unwrap().contains("SHA-256"));
        assert!(ack.applied);
        assert_eq!(ack.update, Some(failed.clone()));
        assert_eq!(fs::read(&binary).unwrap(), old);
        let card = life::owner_summary(&manager).unwrap();
        assert_eq!(card["update"]["ok"], false);
        assert_eq!(card["update"]["to"], "sm 0.2.0 (new)");

        publish_cli(&dir, "sm 0.2.0 (new)", &new, false);
        let advert = mini.remote.cli().unwrap();
        assert!(!advert.building);
        let info = advert.published.unwrap();
        let mut tail = Vec::new();
        mini.remote.cli_download(&info, 9, &mut tail).unwrap();
        assert_eq!(tail, new[9..]);
        fs::write(dir.join("data/prices"), b"night three").unwrap();
        let (outcome, ack) = night(&mut mini, &mut dest, "sm 0.1.0 (old)");
        let ok = UpdateReport {
            from: "sm 0.1.0 (old)".into(),
            to: "sm 0.2.0 (new)".into(),
            ok: true,
            error: None,
        };
        assert_eq!(outcome, Outcome::Installed(ok.clone()));
        assert!(ack.applied);
        assert_eq!(ack.update, Some(ok));
        assert_eq!(fs::read(&binary).unwrap(), new);
        assert_eq!(fs::read(update::sibling(&binary, ".prev")).unwrap(), old);
        assert_eq!(
            fs::read(dest.root.join("plain/data/prices")).unwrap(),
            b"night three"
        );
        let card = life::owner_summary(&manager).unwrap();
        assert_eq!(card["update"]["ok"], true);
        assert!(card["update"]["at"].is_string());
        let runs: Vec<Option<String>> = manager
            .store
            .open()
            .unwrap()
            .prepare("SELECT update_json FROM backup_runs ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(runs.len(), 3);
        assert!(runs[0].is_none() && runs[1].is_some() && runs[2].is_some());

        // The next night the new binary is current: no update, nothing reported.
        let (outcome, ack) = night(&mut mini, &mut dest, "sm 0.2.0 (new)");
        assert_eq!(outcome, Outcome::Unchanged);
        assert!(ack.update.is_none());
        moves::TEST_MOUNTED.set(false);
    })
    .await
    .unwrap();
    handle.shutdown();
    server.await.unwrap().unwrap();
}

/// Asserts `actual` still carries every field the fixture shows, with a type an
/// older client can parse. `null` accepts anything (an `Option` field); a `"*"`
/// key stands for a map whose keys vary; arrays compare their first element.
fn assert_shape(expected: &Value, actual: &Value, at: &str) {
    match expected {
        Value::Null => {}
        Value::Bool(_) => assert!(
            actual.is_boolean(),
            "{at}: expected a boolean, got {actual}"
        ),
        Value::String(_) => assert!(actual.is_string(), "{at}: expected a string, got {actual}"),
        Value::Number(n) if n.is_u64() => {
            assert!(
                actual.is_u64(),
                "{at}: expected an unsigned integer, got {actual}"
            )
        }
        Value::Number(_) => assert!(actual.is_number(), "{at}: expected a number, got {actual}"),
        Value::Array(items) => {
            let actual = actual
                .as_array()
                .unwrap_or_else(|| panic!("{at}: expected an array"));
            if let Some(first) = items.first() {
                let item = actual
                    .first()
                    .unwrap_or_else(|| panic!("{at}: expected items"));
                assert_shape(first, item, &format!("{at}[0]"));
            }
        }
        Value::Object(fields) => {
            let actual = actual
                .as_object()
                .unwrap_or_else(|| panic!("{at}: expected an object, got {actual}"));
            for (key, value) in fields {
                if key == "*" {
                    for (k, v) in actual {
                        assert_shape(value, v, &format!("{at}.{k}"));
                    }
                } else {
                    let field = actual
                        .get(key)
                        .unwrap_or_else(|| panic!("{at}: field {key} is missing"));
                    assert_shape(value, field, &format!("{at}.{key}"));
                }
            }
        }
    }
}

/// Section 5 of #2264: every sm that can be on the mini must keep working against
/// this server. The fixtures are what the client sent and parsed when they were
/// checked in; never edit one to make this test pass. A new client behaviour gets
/// a new fixture.
#[tokio::test]
async fn backup_routes_stay_compatible_with_checked_in_client_fixtures() {
    use crate::managed_artifacts::backup::{
        client::Destination, lifecycle::RequestStatus, seed, update::CliInfo,
    };
    let fixture = |name: &str| -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/backup_client")
            .join(name);
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap()
    };
    let mut f = Fixture::new();
    f.state.managed_artifacts =
        Some(ManagedArtifacts::new(&f.state.config.managed_artifacts).unwrap());
    let manager = f.state.managed_artifacts.clone().unwrap();
    moves::TEST_MOUNTED.set(true);
    fs::write(f.dir.join("data/prices"), b"original").unwrap();
    let mut dest = Destination::open(&f.dir.join("remote")).unwrap();
    let mut confirmed = seed::seed(&manager, "backup-mini", &mut dest, None, false).unwrap();
    publish_cli(&f.dir, "sm 0.2.0 (abcdef012345)", b"0123456789", false);
    let app = router(f.state.clone());
    let mut id = String::new();
    let mut part_bytes = 0u64;
    let replay = |name: &str, id: &str, part_bytes: u64, confirmed: &str| {
        let exchange = fixture(name);
        let path = exchange["path"].as_str().unwrap().replace("{id}", id);
        let mut body = exchange.get("request").cloned().unwrap_or(json!({}));
        if body["have"] == "$CONFIRMED" {
            body["have"] = json!(confirmed);
        }
        let mut req = f.req(
            exchange["method"].as_str().unwrap(),
            &path,
            Some("backup-mini"),
            "sm-app.example.com",
            body,
        );
        if let Some(bytes) = exchange["request_zero_bytes"].as_u64() {
            let chunks = stream::iter(
                (0..bytes / (1 << 20))
                    .map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![0u8; 1 << 20]))),
            );
            *req.body_mut() = Body::from_stream(chunks);
        }
        if let Some(headers) = exchange["request_headers"].as_object() {
            for (k, v) in headers {
                req.headers_mut().insert(
                    axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    v.as_str().unwrap().parse().unwrap(),
                );
            }
        }
        let app = app.clone();
        let name = name.to_owned();
        let size = part_bytes.to_string();
        let last = part_bytes.saturating_sub(1).to_string();
        async move {
            let response = app.oneshot(req).await.unwrap();
            assert_eq!(
                response.status().as_u16() as u64,
                exchange["status"].as_u64().unwrap(),
                "{name}"
            );
            if let Some(headers) = exchange["response_headers"].as_object() {
                for (k, v) in headers {
                    let want = v
                        .as_str()
                        .unwrap()
                        .replace("{size}", &size)
                        .replace("{last}", &last);
                    assert_eq!(response.headers()[k.as_str()], want.as_str(), "{name} {k}");
                }
            }
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let actual: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if let Some(expected) = exchange.get("response") {
                assert_shape(expected, &actual, &name);
            }
            actual
        }
    };
    replay("01_status.json", &id, 0, &confirmed).await;
    fs::rename(f.dir.join("cli"), f.dir.join("cli-hidden")).unwrap();
    replay("02a_cli_none.json", &id, 0, &confirmed).await;
    fs::rename(f.dir.join("cli-hidden"), f.dir.join("cli")).unwrap();
    let cli = replay("02_cli.json", &id, 0, &confirmed).await;
    serde_json::from_value::<CliInfo>(cli).unwrap();
    replay("03_cli_binary_resume.json", &id, 0, &confirmed).await;
    replay("04_request_mismatch.json", &id, 0, &confirmed).await;
    for (ack, content) in [
        ("08_ack_applied.json", "changed once"),
        ("10_ack_with_check_and_update.json", "changed twice"),
        ("13_ack_with_disk.json", "changed three times"),
    ] {
        fs::write(f.dir.join("data/prices"), content).unwrap();
        let created = replay("05_request_create.json", &id, 0, &confirmed).await;
        id = serde_json::from_value::<RequestStatus>(created).unwrap().id;
        life::prepare_request(&manager, &id).unwrap();
        let ready = replay("06_request_ready.json", &id, 0, &confirmed).await;
        let ready: RequestStatus = serde_json::from_value(ready).unwrap();
        part_bytes = ready.parts[0].bytes;
        replay("07_part_resume.json", &id, part_bytes, &confirmed).await;
        replay(ack, &id, part_bytes, &confirmed).await;
        let acked = replay("09_request_acked.json", &id, part_bytes, &confirmed).await;
        serde_json::from_value::<RequestStatus>(acked).unwrap();
        confirmed = id.clone();
    }
    replay("11_probe.json", &id, part_bytes, &confirmed).await;
    replay("12_renew_parsed.json", &id, part_bytes, &confirmed).await;
    // Every fixture file is replayed above.
    let count = fs::read_dir(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/backup_client"),
    )
    .unwrap()
    .count();
    assert_eq!(count, 14);
    let card = life::owner_summary(&manager).unwrap();
    assert_eq!(card["update"]["to"], "sm 0.2.0 (abcdef012345)");
    assert_eq!(card["card"]["disk"]["holding_oldest"], "2026-09-17");
    // Once the disk has reported its size, requests carry the floor.
    fs::write(f.dir.join("data/prices"), "changed four times").unwrap();
    let created = replay("05_request_create.json", &id, 0, &confirmed).await;
    let ready = serde_json::from_value::<RequestStatus>(created).unwrap();
    assert_eq!(ready.space_floor_bytes, Some(400_000_000_000));
    moves::TEST_MOUNTED.set(false);
    drop(dest);
}

/// Records issues sm files; GitHub is otherwise unreachable.
#[derive(Default)]
struct Tickets {
    created: std::sync::Mutex<Vec<(String, String, String)>>,
    resolves: std::sync::atomic::AtomicUsize,
}
impl crate::board::sync::BoardSource for Tickets {
    fn issues_page(
        &self,
        _: &str,
        _: Option<&str>,
    ) -> Result<crate::board::sync::IssuesPage, String> {
        Err("offline".into())
    }
    fn connection_page(
        &self,
        _: &str,
        _: i64,
        _: crate::board::sync::IssueConnection,
        _: &str,
    ) -> Result<(Vec<crate::board::sync::RefNode>, Option<String>), String> {
        Err("offline".into())
    }
    fn items(
        &self,
        _: &str,
        _: &[i64],
    ) -> Result<std::collections::BTreeMap<i64, Option<crate::board::sync::RefNode>>, String> {
        Err("offline".into())
    }
    fn resolve(
        &self,
        _: &[crate::board::model::Key],
    ) -> Result<Vec<Option<crate::board::sync::ResolvedIssue>>, String> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        Err("offline".into())
    }
    fn write_link(
        &self,
        _: &crate::board::sync::LinkMutation,
    ) -> Result<(), crate::board::sync::WriteError> {
        Err(crate::board::sync::WriteError::Transport("offline".into()))
    }
    fn create_issue(&self, repo: &str, title: &str, body: &str) -> Result<(i64, String), String> {
        let mut created = self.created.lock().unwrap();
        created.push((repo.into(), title.into(), body.into()));
        let n = 9000 + created.len() as i64;
        Ok((n, format!("https://github.com/{repo}/issues/{n}")))
    }
}

#[test]
fn a_failed_mini_build_files_one_ticket_under_the_configured_goal() {
    use crate::managed_artifacts::backup::cli_build;
    let mut f = Fixture::new();
    f.state.config.managed_artifacts.backup.cli_build.source = f.dir.display().to_string();
    f.state
        .config
        .managed_artifacts
        .backup
        .cli_build
        .ticket_goal = Some(2300);
    let tickets = Arc::new(Tickets::default());
    let state = f.state.clone().with_board_source(tickets.clone());
    let manager = ManagedArtifacts::new(&state.config.managed_artifacts).unwrap();
    let commit = "0123456789abcdef0123456789abcdef01234567";
    // The mini asks: one build is queued, and asking again queues no other.
    request_cli_build(&state, commit).unwrap();
    request_cli_build(&state, commit).unwrap();
    let queue = expand_home(&state.config.queue_runner_state_dir().to_string_lossy());
    let job = crate::queue::RetainedQueueStore::owned_queue_job(&queue, &cli_build::owner(commit))
        .unwrap()
        .unwrap();
    file_cli_build_failure(&state, &manager, commit).unwrap();
    assert!(
        tickets.created.lock().unwrap().is_empty(),
        "nothing failed yet"
    );
    fs::write(
        job.log_path.as_ref().unwrap(),
        "error[E0425]: cannot find value\n",
    )
    .unwrap();
    crate::queue::open_queue_jobs_connection(&queue.join("queue_runner.db"))
        .unwrap()
        .execute(
            "UPDATE queue_jobs SET state='failed', finished_at=?2 WHERE id=?1",
            rusqlite::params![
                job.id,
                OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap()
            ],
        )
        .unwrap();
    // GitHub is offline for the placement, so each pass fails to place it.
    let mut placements = Vec::new();
    for _ in 0..3 {
        assert!(file_cli_build_failure(&state, &manager, commit).is_err());
        placements.push(tickets.resolves.load(Ordering::SeqCst));
    }
    let created = tickets.created.lock().unwrap().clone();
    assert_eq!(created.len(), 1, "one ticket per failed commit");
    let (repo, title, body) = &created[0];
    assert_eq!(repo, "rajeshgoli/session-manager");
    assert_eq!(
        title,
        "Auto-filed: backup mini sm build failed at 0123456789ab"
    );
    assert!(body.contains(commit) && body.contains("error[E0425]") && body.contains(&job.id));
    assert!(body.contains("keeps its current"));
    // A failed placement is retried on every pass, without a second ticket.
    assert!(placements[0] > 0 && placements[0] < placements[1] && placements[1] < placements[2]);
}
