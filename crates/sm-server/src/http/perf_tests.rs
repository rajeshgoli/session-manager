use super::*;
use axum::http::HeaderValue;

const AT: &str = "2100-01-01T00:00:00Z";

fn polling_fixture() -> AppState {
    let mut config = context_monitor_test_config("claude", "idle");
    config.usage.enabled = false;
    config.codex_requests.db_path = config.sm_send.db_path.clone();
    let mut raw: Value =
        serde_json::from_slice(&fs::read(&config.paths.state_file).unwrap()).unwrap();
    raw["sessions"][0]["id"] = json!("poll-author");
    raw["sessions"][0]["name"] = json!("poll-author");
    raw["sessions"][0]["working_dir"] = json!("/fixture/repo");
    raw["sessions"][0]["last_activity"] = json!(AT);
    raw["sessions"][0]["activity_hook_at"] = json!(AT);
    fs::write(&config.paths.state_file, serde_json::to_vec(&raw).unwrap()).unwrap();
    let state = AppState::new(config);
    let docs = crate::owner_docs::OwnerDocStore::new(expand_home(&state.config.sm_send.db_path));
    docs.ensure_schema().unwrap();
    let db = rusqlite::Connection::open(expand_home(&state.config.sm_send.db_path)).unwrap();
    crate::owner_messages::init_owner_messages_schema(&db).unwrap();
    crate::work_claims::init_work_claims_schema(&db).unwrap();
    RetainedQueueStore::ensure_codex_review_requests_schema_from_path(&expand_home(
        &state.config.sm_send.db_path,
    ))
    .unwrap();
    board::board_store(&state).ensure_schema().unwrap();
    db.execute("INSERT INTO board_items(repo,number,title,url,state,first_seen_at,synced_at) VALUES('acme/widgets',1,'Polling','https://example.test/1','open',?1,?1)", [AT]).unwrap();
    db.execute("INSERT INTO work_claims(id,repo,number,kind,session_id,source,claimed_at) VALUES('claim-poll','acme/widgets',1,'ticket','poll-author','fixture',?1)", [AT]).unwrap();
    db.execute("INSERT INTO board_prs(repo,issue_number,pr_repo,pr_number,pr_state,url) VALUES('acme/widgets',1,'acme/widgets',7,'OPEN','https://example.test/pr/7')", []).unwrap();
    db.execute("INSERT INTO owner_messages(id,human,sender_session_id,sender_session_name,title,body_markdown,blocking,created_at) VALUES('message-poll','rajesh','poll-author','poll-author','Choose a direction','Please choose',1,?1)", [AT]).unwrap();
    db.execute("INSERT INTO board_lanes(id,goal_repo,goal_number,rank,added_at,added_by,added_by_name) VALUES(1,'acme/widgets',1,1,?1,'fixture','Fixture')", [AT]).unwrap();
    for i in 0..150 {
        let id = format!("doc-poll-{i:03}");
        db.execute("INSERT INTO owner_docs(id,repo,path,pr_number,author_session_id,title,created_at,updated_at) VALUES(?1,'acme/widgets',?2,7,'poll-author',?3,?4,?4)", rusqlite::params![id,format!("docs/memo-{i:03}.html"),format!("Memo {i:03}"),AT]).unwrap();
        for n in 0..2 {
            db.execute("INSERT INTO owner_doc_publishes(doc_id,commit_sha,blob_sha,session_id,review_requested,published_at,pr_number) VALUES(?1,?2,?3,'poll-author',1,?4,7)", rusqlite::params![id,if n==0 {"a".repeat(40)}else{"b".repeat(40)},format!("{i:040x}"),AT]).unwrap();
        }
        // Identical submitted_at values exercise the rowid tie breaker, with
        // IDs deliberately ordered opposite to insertion order.
        for suffix in ["z", "a"] {
            db.execute("INSERT INTO owner_doc_reviews(id,status,doc_id,commit_sha,blob_sha,verdict,line_comment_count,file_comment_count,submitted_at,posted_at) VALUES(?1,'posted',?2,?3,?4,'comment',0,0,?5,?5)", rusqlite::params![format!("review-{i}-{suffix}"),id,"a".repeat(40),format!("{i:040x}"),AT]).unwrap();
        }
    }
    for (id, active, status) in [
        ("old-review", false, "completed"),
        ("active-review", true, "waiting"),
    ] {
        db.execute("INSERT INTO codex_review_request_registrations(id,repo,pr_number,requester_session_id,notify_session_id,requested_at,attempt_count,poll_interval_seconds,retry_interval_seconds,state,is_active,review_landed_at,review_url) VALUES(?1,'acme/widgets',7,'poll-author','poll-author',?2,1,60,300,?3,?4,?5,?6)", rusqlite::params![id,AT,status,active,(!active).then_some(AT),(!active).then_some("https://example.test/review")]).unwrap();
    }
    let queue_dir = expand_home(&state.config.queue_runner_state_dir().to_string_lossy());
    fs::create_dir_all(&queue_dir).unwrap();
    RetainedQueueStore::owned_queue_job(&queue_dir, "fixture").unwrap();
    let queue = rusqlite::Connection::open(queue_dir.join("queue_runner.db")).unwrap();
    for (id, status) in [
        ("job-poll-waiting", "pending"),
        ("job-poll-running", "running"),
    ] {
        queue.execute("INSERT INTO queue_jobs(id,type,label,requester_session_id,notify_session_id,cwd,argv_json,env_json,timeout_seconds,state,queued_at,started_at) VALUES(?1,'tests',?1,'poll-author','poll-author','/fixture/repo','[\"true\"]','{}',600,?2,?3,?3)",rusqlite::params![id,status,AT]).unwrap();
    }
    state
}

fn normalize_generated_at(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "generated_at" {
                    *value = json!("<request time>");
                } else {
                    normalize_generated_at(value);
                }
            }
        }
        Value::Array(array) => array.iter_mut().for_each(normalize_generated_at),
        _ => {}
    }
}

#[tokio::test]
async fn polling_routes_match_prechange_goldens() {
    let state = polling_fixture();
    let app = router(state);
    for (name, path) in [
        ("board", "/client/board"),
        ("obligations", "/session-obligations"),
        ("watch", "/watch/state"),
    ] {
        let (status, mut value) = response_json(
            app.clone()
                .oneshot(local_request(Method::GET, path, Body::empty()))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        normalize_generated_at(&mut value);
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("src/http/fixtures/polling-{name}.json"));
        if std::env::var_os("SM_UPDATE_PERF_GOLDENS").is_some() {
            fs::create_dir_all(golden.parent().unwrap()).unwrap();
            fs::write(&golden, &bytes).unwrap();
        }
        assert_eq!(bytes, fs::read(&golden).unwrap(), "{path} changed");
    }
}

#[test]
fn bulk_doc_reads_preserve_per_doc_order_and_filtering() {
    let state = polling_fixture();
    let store = crate::owner_docs::OwnerDocStore::new(expand_home(&state.config.sm_send.db_path));
    let mut ids: Vec<_> = (0..150).map(|i| format!("doc-poll-{i:03}")).collect();
    ids.push("missing".into());
    ids.push(ids[0].clone());
    let publishes = store.publishes_for(&ids).unwrap();
    let reviews = store.reviews_for(&ids).unwrap();
    assert_eq!(publishes.len(), 150);
    assert_eq!(reviews.len(), 150);
    for id in &ids[..150] {
        assert_eq!(publishes[id], store.publishes(id).unwrap());
        assert_eq!(reviews[id], store.reviews(id).unwrap());
    }
    assert!(store.publishes_for(&[]).unwrap().is_empty());
    assert!(store.reviews_for(&[]).unwrap().is_empty());
    assert_eq!(store.publishes_for(&ids[..1]).unwrap().len(), 1);
    assert_eq!(store.reviews_for(&ids[..1]).unwrap().len(), 1);
}

// A single async worker makes accidental synchronous lock waits observable.
#[tokio::test(flavor = "current_thread")]
async fn waiting_hooks_leave_health_and_bootstrap_responsive() {
    let state = polling_fixture();
    let app = router(state.clone());
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let store = state.session_store.clone();
    let holder = std::thread::spawn(move || {
        store.hold_test_write_lock(|| {
            locked_tx.send(()).unwrap();
            // Rescue the test if the async worker blocks before it can release us.
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        })
    });
    locked_rx.await.unwrap();
    let attempts = state.session_store.test_write_lock_attempts();
    let mut hooks = Vec::new();
    for index in 0..32 {
        let app = app.clone();
        hooks.push(tokio::spawn(async move {
            let (path,body) = match index % 3 {
                0 => ("/hooks/claude",json!({"session_manager_id":"poll-author","hook_event_name":"UserPromptSubmit"})),
                1 => ("/hooks/tool-use",json!({"session_manager_id":"poll-author","hook_event_name":"PreToolUse","tool_name":"Read"})),
                _ => ("/hooks/context-usage",json!({"session_id":"poll-author","event":"usage","used_percentage":25})),
            };
            let mut request = local_request(Method::POST,path,Body::from(serde_json::to_vec(&body).unwrap()));
            request.headers_mut().insert(CONTENT_TYPE,HeaderValue::from_static("application/json"));
            app.oneshot(request).await.unwrap().status()
        }));
    }
    let started = std::time::Instant::now();
    while state.session_store.test_write_lock_attempts() < attempts + 32
        && started.elapsed() < Duration::from_secs(2)
    {
        tokio::task::yield_now().await;
    }
    let reached = state.session_store.test_write_lock_attempts() - attempts;
    let mut results = Vec::new();
    for path in ["/health", "/client/bootstrap"] {
        let start = std::time::Instant::now();
        let status = app
            .clone()
            .oneshot(local_request(Method::GET, path, Body::empty()))
            .await
            .unwrap()
            .status();
        results.push((path, status, start.elapsed()));
    }
    release_tx.send(()).unwrap_or(());
    holder.join().unwrap();
    for hook in hooks {
        assert_eq!(hook.await.unwrap(), StatusCode::OK);
    }
    assert_eq!(reached, 32, "hooks must actually wait on the write lock");
    for (path, status, elapsed) in results {
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            elapsed < Duration::from_millis(50),
            "{path} took {elapsed:?}"
        );
    }
}
