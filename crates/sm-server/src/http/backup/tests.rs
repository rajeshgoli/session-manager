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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pretend_mini_mutual_tls_seed_resume_ack_and_restore() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    use crate::{
        device_identity::{openssl, Identity},
        managed_artifacts::backup::{
            client::Destination,
            pull::{self, HttpRemote, Remote},
            seed, Part,
        },
    };
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
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
    let dir = f.dir.clone();
    let manager = f.state.managed_artifacts.as_ref().unwrap().clone();
    tokio::task::spawn_blocking(move || {
        moves::TEST_MOUNTED.set(true);
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
            ureq::tls::PrivateKey::from_pem(
                &fs::read(identity_dir.join("private-key.pem")).unwrap(),
            )
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
        struct Mini {
            remote: HttpRemote,
            manager: ManagedArtifacts,
            interrupt: bool,
            lose_ack: bool,
            offsets: Vec<u64>,
        }
        impl Remote for Mini {
            fn json(
                &mut self,
                method: &str,
                path: &str,
                body: Option<Value>,
            ) -> anyhow::Result<(u16, Value)> {
                let response = self.remote.json(method, path, body)?;
                if method == "POST" && path == "/backup/requests" && response.0 == 202 {
                    let id = response.1["id"].as_str().unwrap();
                    life::prepare_request(&self.manager, id)?;
                    return self
                        .remote
                        .json("GET", &format!("/backup/requests/{id}"), None);
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
        fs::write(dir.join("data/prices"), b"original").unwrap();
        fs::write(dir.join("data/deleted"), b"delete me").unwrap();
        fs::write(dir.join("projects/code"), b"code").unwrap();
        let mut dest = Destination::open(&dir.join("remote")).unwrap();
        let seeded = seed::seed(&manager, "backup-mini", &mut dest, None).unwrap();
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
