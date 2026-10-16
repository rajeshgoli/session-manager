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
