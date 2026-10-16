//! Artifact commands exercise the real router and the one-minute worker.
use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use sm_server::{
    config::{AppConfig, PathsConfig, SmSendConfig},
    http::{router, AppState},
    managed_artifacts::{
        config::{ArtifactRoot, ManagedArtifactsConfig},
        store::ArtifactStore,
    },
    sessions::{RoleRegistrationRequest, SessionStore},
    work_claims::WorkClaimStore,
};
use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use tower::ServiceExt;
struct Fixture {
    dir: PathBuf,
    state: AppState,
    app: axum::Router,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sm-artifact-http-{}-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("root")).unwrap();
        fs::create_dir(dir.join("cold")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let sessions = dir.join("sessions.json");
        fs::write(&sessions,json!({"sessions":(["maker","steward","holder"].map(|id|json!({"id":id,"name":id,"working_dir":"/repo","tmux_session":id,"status":"running","created_at":"2026-10-01T00:00:00Z","last_activity":"2026-10-01T00:00:00Z"})))}).to_string()).unwrap();
        let mut config = AppConfig {
            paths: PathsConfig {
                state_file: sessions.display().to_string(),
                ..Default::default()
            },
            sm_send: SmSendConfig {
                db_path: dir.join("queue.db").display().to_string(),
            },
            managed_artifacts: ManagedArtifactsConfig {
                enabled: true,
                db_path: dir.join("artifacts.db").display().to_string(),
                roots: vec![ArtifactRoot {
                    path: dir.join("root").display().to_string(),
                    cold_store: dir.join("cold").display().to_string(),
                    repo: "owner/research".into(),
                    repo_checkout: dir.join("repo").display().to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        config.queue_runner.state_dir = dir.join("queue-runner").display().to_string();
        config.rust_core.fixture_writes_enabled = true;
        config.rust_core.log_dir = Some(dir.join("logs").display().to_string());
        let state = AppState::new(config);
        let app = router(state.clone());
        WorkClaimStore::new(dir.join("queue.db"))
            .ensure_schema()
            .unwrap();
        Self { dir, state, app }
    }
    fn store(&self) -> ArtifactStore {
        ArtifactStore::new(self.dir.join("artifacts.db"))
    }
    fn root(&self) -> String {
        self.dir.join("root").display().to_string()
    }
    fn register_steward(&self) {
        SessionStore::new_with_queue(self.dir.join("sessions.json"), self.dir.join("queue.db"))
            .register_agent_role(
                "steward",
                RoleRegistrationRequest {
                    requester_session_id: "steward".into(),
                    role: "artifact-steward".into(),
                },
            )
            .unwrap();
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        actor: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(actor) = actor {
            builder = builder.header("x-sm-session-id", actor);
        }
        let mut request = builder
            .body(if method == "GET" {
                Body::empty()
            } else {
                Body::from(body.to_string())
            })
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
}
#[tokio::test]
async fn managed_artifacts_http_commands_and_refusals() {
    let f = Fixture::new();
    let body = json!({"name":"123_data","note":"dataset","consumers":["#123"]});
    assert_eq!(
        f.request("POST", "/artifacts", None, body.clone()).await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, a) = f
        .request("POST", "/artifacts", Some("maker"), body.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(a["state"], "building");
    assert_eq!(
        f.request("POST", "/artifacts", Some("maker"), body).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/123_data/done",
            Some("holder"),
            json!({})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request("POST", "/artifacts/123_data/done", Some("maker"), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let (status, listing) = f
        .request("GET", "/artifacts?state=live", None, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listing["artifacts"].as_array().unwrap().len(), 1);
    assert_eq!(
        f.request("GET", "/artifacts/missing", None, json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/123_data/marks",
            Some("maker"),
            json!({"project":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/123_data/delete",
            Some("maker"),
            json!({"reason":"done"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/123_data/marks",
            Some("maker"),
            json!({"project":false})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/123_data/delete",
            Some("maker"),
            json!({"reason":"done"})
        )
        .await
        .0,
        StatusCode::OK
    );
}
#[tokio::test]
async fn managed_artifacts_steward_requests_and_unclaimable() {
    let f = Fixture::new();
    f.register_steward();
    fs::create_dir(f.dir.join("root/123_data")).unwrap();
    f.state.managed_artifacts_pass().unwrap();
    f.state.managed_artifacts_pass().unwrap();
    let a = f.store().detail(&f.root(), "123_data").unwrap();
    assert_eq!(a["owner_session_id"], "steward");
    assert_eq!(
        a["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "request_sent")
            .count(),
        1
    );
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/123_data/unclaimable",
            Some("maker"),
            json!({"note":"unknown"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/123_data/unclaimable",
            Some("steward"),
            json!({"note":"No provenance found"})
        )
        .await
        .0,
        StatusCode::OK
    );
    f.store()
        .open()
        .unwrap()
        .execute("DELETE FROM artifact_events WHERE kind='request_sent'", [])
        .unwrap();
    f.state.managed_artifacts_pass().unwrap();
    let a = f.store().detail(&f.root(), "123_data").unwrap();
    assert!(a["events"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["kind"] != "request_sent"));
}
#[tokio::test]
async fn managed_artifacts_claim_holder_excludes_reservations_and_ended_claims() {
    let f = Fixture::new();
    let conn = rusqlite::Connection::open(f.dir.join("queue.db")).unwrap();
    conn.execute("INSERT INTO work_claims(id,repo,number,kind,session_id,source,claimed_at,reserved_at) VALUES('one','owner/research',123,'ticket','holder','ticket','2026-10-01T00:00:00Z','2026-10-01T00:00:00Z')",[]).unwrap();
    let claims = WorkClaimStore::new(f.dir.join("queue.db"));
    assert_eq!(claims.active_holder("owner/research", 123).unwrap(), None);
    conn.execute("UPDATE work_claims SET reserved_at=NULL", [])
        .unwrap();
    assert_eq!(
        claims.active_holder("owner/research", 123).unwrap(),
        Some("holder".into())
    );
    conn.execute("UPDATE work_claims SET ended_at='2026-10-02T00:00:00Z'", [])
        .unwrap();
    assert_eq!(claims.active_holder("owner/research", 123).unwrap(), None);
}

#[tokio::test]
async fn managed_artifacts_real_cli_round_trip_and_exit_codes() {
    let f = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = f.app.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let dir = f.dir.clone();
    tokio::task::spawn_blocking(move || {
        let run = |args: &[&str]| {
            std::process::Command::new(env!("CARGO_BIN_EXE_sm"))
                .args(["--api-url", &format!("http://{address}"), "artifact"])
                .args(args)
                .env("SESSION_MANAGER_ID", "maker")
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        let created = run(&[
            "new",
            "123_cli",
            "--note",
            "CLI dataset",
            "--consumer",
            "#123",
        ]);
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&created.stdout).trim(),
            dir.join("root/123_cli").display().to_string()
        );
        let refused = run(&[
            "new",
            "123_cli",
            "--note",
            "CLI dataset",
            "--consumer",
            "#123",
        ]);
        assert_eq!(refused.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&refused.stderr).starts_with("error:"));
        assert!(run(&["done", "123_cli"]).status.success());
        let listing = run(&["list", "--json"]);
        assert!(listing.status.success());
        let value: Value = serde_json::from_slice(&listing.stdout).unwrap();
        assert_eq!(value["artifacts"][0]["state"], "live");
        assert!(run(&["consumer", "123_cli", "add", "#456"])
            .status
            .success());
        assert!(run(&["show", "123_cli"]).status.success());
        assert!(run(&["disk"]).status.success());
        assert!(run(&["delete", "123_cli", "--reason", "test finished"])
            .status
            .success());
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn queue_artifact_api_validates_before_persisting() {
    let f = Fixture::new();
    for declarations in [
        json!({"outputs":["out"]}),
        json!({"record":{"note":"note","project":true}}),
        json!({"outputs":["out"],"disk_bytes":1024,"record":{"note":"bad\nnote","project":true}}),
    ] {
        let (status,body)=f.request("POST","/queue-jobs",Some("maker"),json!({"type":"background","notify_target":"maker","requester_session_id":"maker","cwd":f.dir,"argv":["/usr/bin/true"],"artifacts":declarations})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(f.store().detail(&f.root(), "out").is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_cli_build_records_artifact_recipe_and_preserves_commit() {
    use sm_server::{managed_artifacts::submission::git_in, queue::RetainedQueueStore};
    let f = Fixture::new();
    let repo = PathBuf::from(&f.state.config().managed_artifacts.roots[0].repo_checkout);
    fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "-q"]).unwrap();
    fs::write(repo.join("main.c"), "int main(void) { return 0; }\n").unwrap();
    let output = f.dir.join("root/compiled/tool");
    let script = repo.join("build.sh");
    fs::write(
        &script,
        format!(
            "/usr/bin/cc main.c -o '{}'\n'{}'\n",
            output.display(),
            output.display()
        ),
    )
    .unwrap();
    git_in(&repo, &["add", "main.c", "build.sh"]).unwrap();
    git_in(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "build fixture",
        ],
    )
    .unwrap();
    let remote = f.dir.join("remote.git");
    git_in(&repo, &["init", "--bare", "-q", remote.to_str().unwrap()]).unwrap();
    git_in(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = f.app.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let result = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_sm"))
            .args([
                "--api-url",
                &format!("http://{addr}"),
                "queue",
                "run",
                "--type",
                "background",
                "--label",
                "artifact-real-build",
                "--cwd",
                repo.to_str().unwrap(),
                "--artifact",
                "compiled",
                "--disk",
                "1M",
                "--note",
                "Compiled test tool",
                "--consumer",
                "#123",
                "--recipe-complete",
                "--notify",
                "maker",
                "--script-file",
                script.to_str().unwrap(),
            ])
            .env("SESSION_MANAGER_ID", "maker")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let state_dir = f.dir.join("queue-runner");
    let job = RetainedQueueStore::resolve_queue_job_from_path(
        &state_dir.join("queue_runner.db"),
        "artifact-real-build",
    )
    .unwrap()
    .unwrap();
    RetainedQueueStore::start_queue_job_in_state_dir(
        &state_dir,
        &f.dir.join("queue.db"),
        &job.id,
        1,
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let a = f.store().detail(&f.root(), "compiled").unwrap();
        if a["state"] == "live" {
            let notices = RetainedQueueStore::new(f.dir.join("queue.db"))
                .pending_messages_for_target_by_category("maker", "queue-completion", 20)
                .unwrap();
            assert_eq!(
                a["recipe_complete"],
                1,
                "artifact: {a}; notices: {:?}",
                notices.iter().map(|n| &n.text).collect::<Vec<_>>()
            );
            assert_eq!(a["note"], "Compiled test tool");
            assert!(a["jobs"][0]["tag"]
                .as_str()
                .unwrap()
                .starts_with("artifact/compiled/"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "build did not record: {a}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(output.is_file());
    let before = f.store().detail(&f.root(), "compiled").unwrap();
    // Exercise the actual CLI mutation routes for deletion and rebuild.
    for action in ["delete", "rebuild"] {
        let result = tokio::task::spawn_blocking(move || {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_sm"));
            command.args([
                "--api-url",
                &format!("http://{addr}"),
                "artifact",
                action,
                "compiled",
            ]);
            if action == "delete" {
                command.args(["--reason", "rebuild acceptance test"]);
            }
            command.env("SESSION_MANAGER_ID", "maker").output().unwrap()
        })
        .await
        .unwrap();
        assert!(
            result.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        if action == "delete" {
            assert!(!output.exists());
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let after = f.store().detail(&f.root(), "compiled").unwrap();
        if after["state"] == "live" {
            assert_eq!(after["consumers"], before["consumers"]);
            assert_eq!(after["jobs"].as_array().unwrap().len(), 1);
            assert_eq!(after["note"], before["note"]);
            assert!(output.is_file());
            assert!(after["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "rebuilt"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "rebuild did not finish: {after}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    server.abort();
}

#[tokio::test]
async fn queue_submission_survives_a_failed_post_commit_thaw_notice() {
    use sm_server::managed_artifacts::{
        queue::{install_operations, Operations},
        ManagedArtifacts,
    };
    struct Unavailable;
    impl Operations for Unavailable {
        fn thaw(&self, _: &ManagedArtifacts, _: &ArtifactRoot, _: &str) -> anyhow::Result<()> {
            anyhow::bail!("injected thaw worker failure")
        }
        fn pressure(&self, _: &ManagedArtifacts, _: &ArtifactRoot, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let f = Fixture::new();
    let conn = f.store().open().unwrap();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'frozen-input','frozen','cold','frozen-input','queue','owner/research',?2)",rusqlite::params![f.root(),sm_server::managed_artifacts::now()]).unwrap();
    install_operations(&f.root(), std::sync::Arc::new(Unavailable));
    let (status,body)=f.request("POST","/queue-jobs",Some("maker"),json!({"type":"background","label":"notice-failure","notify_target":"maker","requester_session_id":"maker","cwd":f.dir,"argv":["/usr/bin/true"],"artifacts":{"inputs":["frozen-input"]}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "pending");
    assert!(body["id"].is_string());
    let queue = rusqlite::Connection::open(f.dir.join("queue-runner/queue_runner.db")).unwrap();
    sm_server::managed_artifacts::queue::repair(&queue, Some(&f.dir.join("queue.db"))).unwrap();
    let count: i64 = queue
        .query_row(
            "SELECT COUNT(*) FROM queue_jobs WHERE label='notice-failure'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn managed_artifacts_needs_you_badge_counts_current_items() {
    let f = Fixture::new();
    for name in ["orphan", "second"] {
        fs::create_dir(f.dir.join("root").join(name)).unwrap();
    }
    f.state.managed_artifacts_pass().unwrap();
    let (status, list) = f.request("GET", "/artifacts", None, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["needs_you_count"], 2);
    assert_eq!(list["needs_you"].as_array().unwrap().len(), 2);
    f.request(
        "POST",
        "/artifacts/orphan/record",
        Some("maker"),
        json!({"note":"recorded","consumers":["#1"]}),
    )
    .await;
    let (_, list) = f.request("GET", "/artifacts", None, json!({})).await;
    assert_eq!(list["needs_you_count"], 1);
    assert_eq!(list["needs_you"][0]["name"], "second");
    assert_eq!(list["mode"], "report_only");
}

#[tokio::test]
async fn managed_artifacts_system_messages_refuse_replies() {
    use sm_server::owner_messages::{CreateOwnerMessage, NewOwnerMessage, OwnerMessageStore};
    let f = Fixture::new();
    let result = OwnerMessageStore::new(f.dir.join("queue.db"))
        .create(NewOwnerMessage {
            human: f.state.config().owner_name.clone(),
            sender_session_id: "sm-artifacts".into(),
            sender_session_name: "sm artifacts".into(),
            title: "Disk space".into(),
            body_markdown: "Use the artifact list".into(),
            blocking: true,
        })
        .unwrap();
    let CreateOwnerMessage::Created(message) = result else {
        panic!("message not created")
    };
    let (status, body) = f
        .request(
            "POST",
            &format!("/messages/{}/reply", message.id),
            None,
            json!({"submission_id":"reply-artifacts","body":"test"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("use the artifact list"));
    let (_, body) = f
        .request(
            "GET",
            &format!("/messages/{}?format=json", message.id),
            None,
            json!({}),
        )
        .await;
    assert!(body["reply_to_session_id"].is_null());
}
