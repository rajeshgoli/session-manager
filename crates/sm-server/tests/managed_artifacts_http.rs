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
        config.google_auth.session_cookie_secret = Some("artifact-test-secret".into());
        for (email, owner) in [("owner@example.com", true), ("guest@example.com", false)] {
            config.mobile_terminal.allowed_users.insert(
                email.into(),
                sm_server::config::MobileTerminalUserConfig {
                    email: Some(email.into()),
                    owner,
                    ..Default::default()
                },
            );
        }
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
            if actor.contains('@') {
                use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
                use hmac::{Hmac, Mac};
                let payload = URL_SAFE_NO_PAD.encode(json!({"type":"device_access","email":actor,"exp":time::OffsetDateTime::now_utc().unix_timestamp()+3600}).to_string());
                let mut mac =
                    Hmac::<sha2::Sha256>::new_from_slice(b"artifact-test-secret").unwrap();
                mac.update(payload.as_bytes());
                let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
                builder = builder.header(
                    "authorization",
                    format!("Bearer smat_{payload}.{signature}"),
                );
            } else {
                builder = builder.header("x-sm-session-id", actor);
            }
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
async fn managed_artifacts_owner_action_refuses_local_bypass_and_agent_headers() {
    let f = Fixture::new();
    for actor in [None, Some("maker"), Some("guest@example.com")] {
        for action in ["delete", "thaw", "keep_live", "investigate"] {
            let (status, _) = f
                .request(
                    "POST",
                    "/artifacts/sample/owner-action",
                    actor,
                    json!({"action":action}),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{action} {actor:?}");
        }
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
async fn managed_artifacts_owner_keep_live_and_delete() {
    let f = Fixture::new();
    assert_eq!(
        f.request(
            "POST",
            "/artifacts",
            Some("maker"),
            json!({"name":"sample","note":"retained data","project":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.request("POST", "/artifacts/sample/done", Some("maker"), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let (status, kept) = f
        .request(
            "POST",
            "/artifacts/sample/owner-action",
            Some("owner@example.com"),
            json!({"action":"keep_live"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(
        kept["keep_live_until"],
        (time::OffsetDateTime::now_utc().date() + time::Duration::days(30)).to_string()
    );
    let (status, list) = f
        .request("GET", "/artifacts", Some("owner@example.com"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["can_act"], true);
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/sample/delete",
            Some("maker"),
            json!({"reason":"cleanup"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, deleted) = f
        .request(
            "POST",
            "/artifacts/sample/owner-action",
            Some("owner@example.com"),
            json!({"action":"delete"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["state"], "deleted");
}

#[tokio::test]
async fn managed_artifacts_brief_opens_the_launcher_and_any_agent_may_report() {
    let f = Fixture::new();
    assert_eq!(
        f.request(
            "POST",
            "/artifacts",
            Some("maker"),
            json!({"name":"sample","note":"inspect this dataset","project":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, brief) = f
        .request(
            "POST",
            "/artifacts/sample/owner-action",
            Some("owner@example.com"),
            json!({"action":"brief"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{brief}");
    assert!(brief["brief"]
        .as_str()
        .unwrap()
        .starts_with("The artifact folder sample in "));
    assert!(!brief["brief"].as_str().unwrap().contains("Tier"));
    assert_eq!(brief["name"], "artifact-sample");
    assert_eq!(brief["provider"], "codex");
    assert_eq!(brief["model"], "gpt-6-luna");
    assert_eq!(brief["effort"], "high");
    assert_eq!(
        brief["working_dir"],
        f.state.config().managed_artifacts.roots[0].repo_checkout
    );
    // The brief starts nothing: no investigation is recorded.
    let (_, list) = f
        .request("GET", "/artifacts", Some("owner@example.com"), json!({}))
        .await;
    assert!(list["artifacts"][0]["investigating"].is_null(), "{list}");
    // With none recorded, the agent the owner started may report.
    let (status, reported) = f
        .request(
            "POST",
            "/artifacts/sample/report",
            Some("maker"),
            json!({"note":"keep it","recommend":"keep"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reported}");
}

#[tokio::test]
async fn managed_artifacts_investigate_spawns_once_and_report_finishes() {
    let f = Fixture::new();
    f.register_steward();
    let repo = PathBuf::from(&f.state.config().managed_artifacts.roots[0].repo_checkout);
    fs::create_dir_all(&repo).unwrap();
    for args in [
        vec!["init"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
            .status
            .success());
    }
    assert_eq!(
        f.request(
            "POST",
            "/artifacts",
            Some("maker"),
            json!({"name":"sample","note":"inspect this dataset","project":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, started) = f
        .request(
            "POST",
            "/artifacts/sample/owner-action",
            Some("owner@example.com"),
            json!({"action":"investigate"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let child = started["investigating"].as_str().unwrap();
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/sample/owner-action",
            Some("owner@example.com"),
            json!({"action":"investigate"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request(
            "POST",
            "/artifacts/sample/report",
            Some("maker"),
            json!({"note":"keep it","recommend":"keep"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, reported) = f
        .request(
            "POST",
            "/artifacts/sample/report",
            Some(child),
            json!({"note":"keep it","recommend":"keep"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reported}");
    assert!(reported["investigating"].is_null());
    // Reporting clears artifact metadata, but the cleanup registration persists.
    let claims = WorkClaimStore::new(f.dir.join("queue.db"));
    let conn = rusqlite::Connection::open(f.dir.join("queue.db")).unwrap();
    let path: String = conn
        .query_row(
            "SELECT path FROM investigation_worktrees WHERE session_id = ?1",
            [child],
            |row| row.get(0),
        )
        .unwrap();
    use sm_server::work_claims::worktrees::{run_worktree_cleanup, CleanupRequest, CleanupSession};
    let mut sessions = [CleanupSession {
        id: child.into(),
        name: "investigator".into(),
        working_dir: path.clone(),
        retired: false,
        stopped: false,
        retired_at: None,
        local: true,
    }];
    assert!(run_worktree_cleanup(
        &claims,
        CleanupRequest {
            sessions: &sessions,
            ..Default::default()
        }
    )
    .unwrap()
    .is_empty());
    assert!(std::path::Path::new(&path).exists());
    sessions[0].retired = true;
    sessions[0].stopped = true;
    sessions[0].retired_at = Some(
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap(),
    );
    let outcomes = run_worktree_cleanup(
        &claims,
        CleanupRequest {
            sessions: &sessions,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome.path == path && outcome.removed),
        "{outcomes:?}"
    );
    assert!(!std::path::Path::new(&path).exists());

    assert!(reported["report"]["detail_json"]
        .as_str()
        .unwrap()
        .contains("keep it"));
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
async fn queue_job_without_artifact_flags_reserves_the_frozen_folder_it_names() {
    let f = Fixture::new();
    let conn = f.store().open().unwrap();
    conn.execute("INSERT INTO artifacts(root,name,state,location,set_id,origin,repo,created_at) VALUES(?1,'chains','frozen','cold','chains','queue','owner/research',?2)",rusqlite::params![f.root(),sm_server::managed_artifacts::now()]).unwrap();
    let path = format!("{}/chains/nk5", f.root());
    let (status,body)=f.request("POST","/queue-jobs",Some("maker"),json!({"type":"background","label":"plain-reader","notify_target":"maker","requester_session_id":"maker","cwd":f.dir,"argv":["/bin/ls",path]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let inputs: String = conn
        .query_row(
            "SELECT inputs_json FROM artifact_job_reservations WHERE job_id=?1",
            [body["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inputs, r#"["chains"]"#);
    let (status,body)=f.request("POST","/queue-jobs",Some("maker"),json!({"type":"background","label":"plain-other","notify_target":"maker","requester_session_id":"maker","cwd":f.dir,"argv":["/usr/bin/true"]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reserved: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_job_reservations WHERE job_id=?1",
            [body["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reserved, 0);
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
            blocked: false,
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
    assert_eq!(body["state"], "needs_you");
}

#[tokio::test]
async fn managed_artifacts_recovered_pressure_closes_blocking_message() {
    use sm_server::managed_artifacts::store;
    use sm_server::owner_messages::{NewOwnerMessage, OwnerMessageStore};
    let f = Fixture::new();
    let inbox = OwnerMessageStore::new(f.dir.join("queue.db"));
    let key = "pressure_pending:123:100";
    inbox
        .create_once(
            NewOwnerMessage {
                human: f.state.config().owner_name.clone(),
                sender_session_id: "sm-artifacts".into(),
                sender_session_name: "sm artifacts".into(),
                title: "Disk space".into(),
                body_markdown: "Space needed".into(),
                blocking: true,
                blocked: false,
            },
            Some(key),
        )
        .unwrap();
    assert!(inbox.is_open_by_delivery_key(key).unwrap());
    store::set_meta(&f.store().open().unwrap(), "pressure_pending:123", "null").unwrap();
    f.state.managed_artifacts_pass().unwrap();
    assert!(!inbox.is_open_by_delivery_key(key).unwrap());
    f.state.managed_artifacts_pass().unwrap();
    assert!(!inbox.is_open_by_delivery_key(key).unwrap());
}

#[tokio::test]
async fn managed_artifacts_failed_investigation_worktree_is_cleanup_candidate() {
    use sm_server::work_claims::worktrees::{run_worktree_cleanup, CleanupRequest};
    use std::os::unix::fs::PermissionsExt;
    for metadata_created in [false, true] {
        let f = Fixture::new();
        let repo = PathBuf::from(&f.state.config().managed_artifacts.roots[0].repo_checkout);
        fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        }
        if metadata_created {
            // Git creates the detached worktree, then its checkout hook fails.
            let hook = repo.join(".git/hooks/post-checkout");
            fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        } else {
            // A file blocks Git from creating its worktree metadata directory.
            fs::write(repo.join(".git/worktrees"), "blocked").unwrap();
        }
        assert_eq!(
            f.request(
                "POST",
                "/artifacts",
                Some("maker"),
                json!({"name":"sample","note":"inspect this dataset","project":true})
            )
            .await
            .0,
            StatusCode::OK
        );
        let failure = f
            .request(
                "POST",
                "/artifacts/sample/owner-action",
                Some("owner@example.com"),
                json!({"action":"investigate"}),
            )
            .await;
        assert_eq!(failure.0, StatusCode::CONFLICT, "{failure:?}");
        let conn = rusqlite::Connection::open(f.dir.join("queue.db")).unwrap();
        let (path, failed): (String, Option<String>) = conn
            .query_row(
                "SELECT path, spawn_failed_at FROM investigation_worktrees",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(failed.is_some());
        assert_eq!(std::path::Path::new(&path).exists(), metadata_created);
        let detail = f
            .request(
                "GET",
                "/artifacts/sample",
                Some("owner@example.com"),
                Value::Null,
            )
            .await;
        assert_eq!(detail.0, StatusCode::OK);
        assert!(detail.1["investigating"].is_null());
        let claims = WorkClaimStore::new(f.dir.join("queue.db"));
        let outcomes = run_worktree_cleanup(&claims, CleanupRequest::default()).unwrap();
        assert!(
            outcomes
                .iter()
                .any(|outcome| outcome.path == path && outcome.removed == metadata_created),
            "{outcomes:?}"
        );
        assert!(!std::path::Path::new(&path).exists());
    }
}

#[tokio::test]
async fn interrupted_investigations_recover_on_startup_without_releasing_live_requests() {
    use sm_server::managed_artifacts::{investigations, store};
    use sm_server::work_claims::worktrees::{run_worktree_cleanup, CleanupRequest};
    for stage in [
        "reservation",
        "intent",
        "mkdir",
        "git",
        "spawn",
        "persisted",
    ] {
        let f = Fixture::new();
        let manager = sm_server::managed_artifacts::ManagedArtifacts::new(
            &f.state.config().managed_artifacts,
        )
        .unwrap();
        let claims = WorkClaimStore::new(f.dir.join("queue.db"));
        let path = f.dir.join("investigation");
        let id = if stage == "persisted" {
            "holder"
        } else {
            "interrupted"
        };
        let key = format!("investigating:{}:sample", f.root());
        store::set_meta(&manager.store.open().unwrap(), &key, id).unwrap();
        if stage != "reservation" {
            claims
                .register_investigation_worktree(id, path.to_str().unwrap(), "owner/research", "")
                .unwrap();
        }
        if matches!(stage, "mkdir" | "git" | "spawn" | "persisted") {
            fs::create_dir(&path).unwrap();
        }
        if matches!(stage, "git" | "spawn" | "persisted") {
            let repo = f.dir.join("repo");
            fs::create_dir(&repo).unwrap();
            for args in [
                vec!["init"],
                vec![
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=f@example.com",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "initial",
                ],
            ] {
                assert!(std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success());
            }
            let base = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            claims
                .reserve_investigation_worktree(id, String::from_utf8_lossy(&base.stdout).trim())
                .unwrap();
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["worktree", "add", "--detach"])
                .arg(&path)
                .output()
                .unwrap()
                .status
                .success());
        }
        // A second server cannot recover the live request, even with no session.
        let lock = investigations::try_lock(&manager).unwrap().unwrap();
        let live = AppState::new(f.state.config().clone());
        assert_eq!(
            store::meta(&manager.store.open().unwrap(), &key)
                .unwrap()
                .as_deref(),
            Some(id)
        );
        if stage != "reservation" {
            let failed: Option<String> = rusqlite::Connection::open(f.dir.join("queue.db"))
                .unwrap()
                .query_row(
                    "SELECT spawn_failed_at FROM investigation_worktrees",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(failed.is_none());
        }
        drop(lock); // A crash releases the same kernel lock.
        live.managed_artifacts_pass().unwrap();
        // Repeated startup and worker recovery must be harmless.
        let recovered = AppState::new(f.state.config().clone());
        let reservation = store::meta(&manager.store.open().unwrap(), &key).unwrap();
        assert_eq!(
            reservation.as_deref(),
            (stage == "persisted").then_some(id),
            "{stage}"
        );
        if stage != "reservation" {
            let failed: Option<String> = rusqlite::Connection::open(f.dir.join("queue.db"))
                .unwrap()
                .query_row(
                    "SELECT spawn_failed_at FROM investigation_worktrees",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(failed.is_some(), stage != "persisted", "{stage}");
        }
        let outcomes = run_worktree_cleanup(&claims, CleanupRequest::default()).unwrap();
        assert_eq!(path.exists(), stage == "persisted", "{stage}: {outcomes:?}");
        drop(recovered);
    }
}

#[tokio::test]
async fn investigation_git_child_holds_recovery_lock_after_parent_drops_it() {
    use sm_server::managed_artifacts::{investigations, ManagedArtifacts};
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    let f = Fixture::new();
    let manager = ManagedArtifacts::new(&f.state.config().managed_artifacts).unwrap();
    let lock = investigations::try_lock(&manager).unwrap().unwrap();
    let mut command = tokio::process::Command::new("sh");
    command
        .args(["-c", "printf ready; read line"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    investigations::inherit_lock(&mut command, &lock);
    let mut child = command.spawn().unwrap();
    let mut ready = [0; 5];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        child.stdout.as_mut().unwrap().read_exact(&mut ready),
    )
    .await
    .unwrap()
    .unwrap();
    drop(lock);
    assert!(investigations::try_lock(&manager).unwrap().is_none());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    assert!(investigations::try_lock(&manager).unwrap().is_some());
}
#[tokio::test]
async fn server_starts_while_another_process_holds_the_artifact_writer() {
    use sm_server::managed_artifacts::store;
    let f = Fixture::new();
    let key = format!("investigating:{}:sample", f.root());
    store::set_meta(&f.store().open().unwrap(), &key, "ended").unwrap();
    let mut config = f.state.config().clone();
    config.managed_artifacts.cold_after_days += 1;
    let writer = f.store().open().unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = std::time::Instant::now();
    let state = AppState::try_new(config.clone()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    writer.execute_batch("ROLLBACK").unwrap();
    state.managed_artifacts_pass().unwrap();
    let conn = f.store().open().unwrap();
    assert_eq!(store::meta(&conn, &key).unwrap(), None);
    assert_eq!(
        store::meta(&conn, &format!("cold_days:{}", f.root())).unwrap(),
        Some(config.managed_artifacts.cold_after_days.to_string())
    );
}
