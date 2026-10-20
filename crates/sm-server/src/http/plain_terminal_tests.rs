struct PlainTestTmux(String);
impl Drop for PlainTestTmux {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .output();
    }
}
fn plain_test_state() -> (AppState, PlainTestTmux) {
    let mut state = browser_terminal_state();
    let path = PathBuf::from(&state.config.paths.state_file);
    let mut fixture: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fixture["sessions"][0]["working_dir"] = json!(path.parent().unwrap());
    fs::write(&path, serde_json::to_vec(&fixture).unwrap()).unwrap();
    state.config.rust_core.runtime_enabled = true;
    let socket = format!(
        "sm-2333-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    state.config.tmux.socket_name = Some(socket.clone());
    state.config.rust_core.tmux_socket_name = Some(socket.clone());
    (state, PlainTestTmux(socket))
}
async fn plain_call(
    state: &AppState,
    method: Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = local_request(method, path, Body::from(body.to_string()));
    request
        .headers_mut()
        .insert(CONTENT_TYPE, "application/json".parse().unwrap());
    response_json(router(state.clone()).oneshot(request).await.unwrap()).await
}
fn plain_set_limits(state: &AppState, patch: Value) {
    state
        .session_store
        .update_owner_settings(&json!({"terminal_limits":patch}), |_| Ok(Ok(())))
        .unwrap()
        .unwrap();
}
async fn plain_new(state: &AppState) -> String {
    let (status, body) = plain_call(state, Method::POST, "/terminals", json!({"cwd":"/tmp"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().into()
}

#[tokio::test]
async fn plain_terminal_creation_persists_phone_attribution() {
    let (state, _tmux) = plain_test_state();
    let mut request = local_request(Method::POST, "/terminals", Body::from(r#"{"cwd":"/tmp"}"#));
    request.headers_mut().insert(CONTENT_TYPE, "application/json".parse().unwrap());
    request.headers_mut().insert("x-sm-device-key-id", "phone-1".parse().unwrap());
    let (status, body) = response_json(router(state.clone()).oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let terminal = plain_terminals::store(&state).get(body["id"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(terminal.created_by_device.as_deref(), Some("phone-1"));
    assert_eq!(terminal.created_via, "mobile");

    let cli_id = plain_new(&state).await;
    let cli = plain_terminals::store(&state).get(&cli_id).unwrap().unwrap();
    assert_eq!(cli.created_by_device, None);
    assert_eq!(cli.created_via, "cli");
}

#[tokio::test]
async fn plain_terminal_limits_cap_nonowner_and_kill_switch() {
    let (state, _tmux) = plain_test_state();
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(plain_new(&state).await);
    }
    let (status, body) =
        plain_call(&state, Method::POST, "/terminals", json!({"cwd":"/tmp"})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "too_many_terminals");
    assert_eq!(body["terminals"].as_array().unwrap().len(), 4);
    assert!(state.session_store.get_session(&ids[0]).unwrap().is_none());
    let (status, body) = plain_call(
        &state,
        Method::POST,
        &format!("/terminals/{}/route", ids[0]),
        json!({"where":"home"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ssh_host"], "studio-lan.rajeshgo.li");
    assert!(body["command"]
        .as_str()
        .unwrap()
        .ends_with(&format!("attach-session -t {}", ids[0])));
    let outsider =
        test_browser_access_assertion("sm-browser-aud", "outsider@example.com", 4_102_444_800);
    let response = router(state.clone())
        .oneshot(owner_web_request(
            Method::POST,
            "/terminals",
            "sm.example.com",
            Some(&outsider),
            &[("origin", "https://sm.example.com")],
            &json!({"cwd":"/tmp"}),
        ))
        .await
        .unwrap();
    assert!(is_auth_denial_status(response.status().as_u16()));
    let mut agent = local_request(Method::POST, "/terminals", Body::from("{}"));
    agent
        .headers_mut()
        .insert(handoff::SESSION_HEADER, "fork1001".parse().unwrap());
    assert_eq!(
        router(state.clone()).oneshot(agent).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    // Runtime owner settings close every shell in the same PUT.
    let owner =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let request = owner_web_request(
        Method::PUT,
        "/client/settings",
        "sm.example.com",
        Some(&owner),
        &[("origin", "https://sm.example.com")],
        &json!({"terminal_limits":{"plain_terminal_enabled":false}}),
    );
    let (status, body) = response_json(router(state.clone()).oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for id in &ids {
        let t = plain_terminals::store(&state).get(id).unwrap().unwrap();
        assert_eq!(t.retired_reason.as_deref(), Some("kill_switch"));
        assert!(!crate::plain_terminals::exists(&state.runtime(), &t).unwrap());
    }
    for (method, path, body) in [
        (Method::POST, "/terminals".into(), json!({"cwd":"/tmp"})),
        (
            Method::POST,
            format!("/terminals/{}/route", ids[0]),
            json!({"where":"home"}),
        ),
        (Method::DELETE, format!("/terminals/{}", ids[0]), json!({})),
    ] {
        let (status, body) = plain_call(&state, method, &path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "plain_disabled");
    }
}

#[tokio::test]
async fn plain_terminal_browser_tickets_reject_public_relay_and_disabled_shells() {
    let (state, _tmux) = plain_test_state();
    let id = plain_new(&state).await;
    let owner =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let request = owner_web_request(
        Method::POST,
        &format!("/client/sessions/{id}/browser-attach-ticket"),
        "sm.example.com",
        Some(&owner),
        &[("origin", "https://sm.example.com")],
        &json!({}),
    );
    let (status, ticket) =
        response_json(router(state.clone()).oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{ticket}");
    let frame = browser_terminal_frame(&ticket);
    assert!(consume_terminal_ticket(&state, &frame, Some("rajeshgoli@gmail.com"), false).is_err());
    let (_, attach, stop) =
        consume_terminal_ticket(&state, &frame, Some("rajeshgoli@gmail.com"), true).unwrap();
    assert!(!stop.load(Ordering::SeqCst));
    remove_mobile_terminal_active_attach(&state, &attach);
    plain_set_limits(&state, json!({"plain_terminal_enabled":false}));
    let session = plain_terminals::browser_session(&state, &id);
    assert!(session.is_err());
    state.reconcile_plain_terminals().unwrap();
}

#[tokio::test]
async fn plain_terminal_busy_jobs_restart_adoption_idle_reap_and_watch() {
    let (state, _tmux) = plain_test_state();
    let id = plain_new(&state).await;
    let store = plain_terminals::store(&state);
    let t = store.get(&id).unwrap().unwrap();
    let output = PathBuf::from(&state.config.paths.state_file).with_extension("shell-env");
    let line=format!("print -r -- ${{SESSION_MANAGER_ID-unset}}:${{CLAUDE_SESSION_MANAGER_ID-unset}}:${{SM_SESSION_CREDENTIAL-unset}} > '{}'; sleep 300 &",output.display());
    // Background work protects a prompt too, not only a foreground command.
    state
        .runtime()
        .tmux_command(["send-keys", "-t", &id, &line, "Enter"])
        .unwrap()
        .output()
        .unwrap();
    for _ in 0..100 {
        if output.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        fs::read_to_string(output).unwrap().trim(),
        "unset:unset:unset"
    );
    let conn = Connection::open(&state.config.usage.db_path).unwrap();
    conn.execute(
        "UPDATE plain_terminals SET created_at='2020-01-01T00:00:00Z' WHERE id=?1",
        [&id],
    )
    .unwrap();
    state.reconcile_plain_terminals().unwrap();
    assert!(store.get(&id).unwrap().unwrap().busy_since.is_some());
    let (_, watch) = plain_call(&state, Method::GET, "/watch/state", json!({})).await;
    let row = watch["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap();
    assert_eq!(row["kind"], "shell");
    assert_eq!(row["attention"]["section"], "moving");
    assert!(row["facts"]["you"].is_null());
    // The Android app decodes jobs.text as a non-null string; null breaks the whole Watch page.
    assert_eq!(row["facts"]["jobs"]["text"], "No jobs");
    // A fresh server state re-adopts the surviving tmux runtime.
    let restarted = AppState::new(state.config.clone());
    restarted.reconcile_plain_terminals().unwrap();
    assert_eq!(
        plain_terminals::store(&restarted)
            .get(&id)
            .unwrap()
            .unwrap()
            .state,
        "live"
    );
    let (status, _) = plain_call(
        &restarted,
        Method::DELETE,
        &format!("/terminals/{id}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!crate::plain_terminals::exists(&state.runtime(), &t).unwrap());
    // A missing runtime is retired after restart, and a fresh idle prompt is reaped.
    let gone = plain_new(&state).await;
    state.runtime().kill_session(&gone).unwrap();
    state.reconcile_plain_terminals().unwrap();
    assert_eq!(
        store.get(&gone).unwrap().unwrap().retired_reason.as_deref(),
        Some("tmux_gone")
    );
    let idle = plain_new(&state).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    conn.execute(
        "UPDATE plain_terminals SET created_at='2020-01-01T00:00:00Z' WHERE id=?1",
        [&idle],
    )
    .unwrap();
    plain_terminals::reconcile_at(
        &state,
        OffsetDateTime::now_utc() + time::Duration::minutes(181),
    )
    .unwrap();
    assert_eq!(
        store.get(&idle).unwrap().unwrap().retired_reason.as_deref(),
        Some("idle")
    );
}

#[test]
fn plain_terminal_settings_validate_ranges_and_reset() {
    for patch in [
        json!({"plain_terminal_enabled":"yes"}),
        json!({"max_plain_terminals":0}),
        json!({"max_plain_terminals":17}),
        json!({"idle_reap_minutes":14}),
        json!({"idle_reap_minutes":1441}),
        json!({"tunnel_linger_minutes":0}),
        json!({"tunnel_linger_minutes":121}),
    ] {
        assert!(
            crate::owner_settings::apply_patch(None, &json!({"terminal_limits":patch})).is_err()
        );
    }
    assert!(crate::owner_settings::apply_patch(None,&json!({"terminal_limits":{"plain_terminal_enabled":false,"max_plain_terminals":1,"idle_reap_minutes":15,"tunnel_linger_minutes":120}})).is_ok());
}

#[derive(Default)]
struct PlainPushRecorder(Mutex<Vec<BTreeMap<String, String>>>);
impl crate::owner_push::PushSender for PlainPushRecorder {
    fn send(
        &self,
        _token: &str,
        data: &BTreeMap<String, String>,
    ) -> anyhow::Result<(), crate::owner_push::PushError> {
        self.0.lock().unwrap().push(data.clone());
        Ok(())
    }
}
#[tokio::test]
async fn plain_terminal_from_session_push_and_concurrent_cap() {
    let (mut state, _tmux) = plain_test_state();
    let sender = Arc::new(PlainPushRecorder::default());
    state.push_sender = Some(sender.clone());
    follows::push_store(&state)
        .upsert_token(
            &crate::owner_push::PushTokenRegistration {
                user_id: "rajeshgoli@gmail.com".into(),
                token: "plain-token".into(),
                device_id: Some("phone".into()),
                device_name: "phone".into(),
                app_version: "test".into(),
            },
            OffsetDateTime::now_utc(),
        )
        .unwrap();
    plain_set_limits(&state, json!({"max_plain_terminals":1}));
    let (a, b) = tokio::join!(
        plain_call(
            &state,
            Method::POST,
            "/terminals",
            json!({"from_session":"fork1001"})
        ),
        plain_call(
            &state,
            Method::POST,
            "/terminals",
            json!({"from_session":"fork1001"})
        )
    );
    assert!(matches!(
        (a.0, b.0),
        (StatusCode::CREATED, StatusCode::CONFLICT) | (StatusCode::CONFLICT, StatusCode::CREATED)
    ));
    let created = if a.0 == StatusCode::CREATED { a.1 } else { b.1 };
    let id = created["id"].as_str().unwrap();
    let session = state
        .session_store
        .get_session("fork1001")
        .unwrap()
        .unwrap();
    assert_eq!(
        created["cwd"],
        PathBuf::from(session.working_dir)
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    for _ in 0..40 {
        if !sender.0.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    {
        let notices = sender.0.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["kind"], "shell_opened");
    }
    assert_eq!(
        plain_call(
            &state,
            Method::DELETE,
            &format!("/terminals/{id}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn plain_terminal_brief_native_attach_and_exec_root_survive_cleanup() {
    let (state, _tmux) = plain_test_state();
    let id = plain_new(&state).await;
    let store = plain_terminals::store(&state);
    let t = store.get(&id).unwrap().unwrap();
    Connection::open(&state.config.usage.db_path)
        .unwrap()
        .execute(
            "UPDATE plain_terminals SET created_at='2020-01-01T00:00:00Z' WHERE id=?1",
            [&id],
        )
        .unwrap();
    let mut ticket = test_mobile_terminal_ticket();
    ticket.tmux_session = id.clone();
    ticket.tmux_socket_name = state.runtime().socket_name().map(str::to_owned);
    let mut pty =
        start_mobile_terminal_attach_client(&ticket, 24, 80, Arc::new(AtomicBool::new(false)))
            .unwrap();
    for _ in 0..80 {
        if crate::plain_terminals::observe(&state.runtime(), &t).is_ok_and(|o| o.attached) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        crate::plain_terminals::observe(&state.runtime(), &t)
            .unwrap()
            .attached
    );
    write_mobile_terminal_pty(pty.master.clone(), b"true\r".to_vec())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    state
        .runtime()
        .tmux_command(["detach-client", "-s", &id])
        .unwrap()
        .output()
        .unwrap();
    for _ in 0..80 {
        if pty.child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        pty.child.try_wait().unwrap().is_some(),
        "native client must detach"
    );
    assert!(
        !crate::plain_terminals::observe(&state.runtime(), &t)
            .unwrap()
            .attached
    );
    // No lifecycle pass saw this client attached. Persistent tmux activity protects it.
    state.reconcile_plain_terminals().unwrap();
    let record = store.get(&id).unwrap().unwrap();
    assert_eq!(record.state, "live");
    assert!(record.last_attach_at.is_some());
    state
        .runtime()
        .tmux_command(["send-keys", "-t", &id, "exec sleep 300", "Enter"])
        .unwrap()
        .output()
        .unwrap();
    for _ in 0..80 {
        if crate::plain_terminals::observe(&state.runtime(), &t).is_ok_and(|o| o.command == "sleep")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let observed = crate::plain_terminals::observe(&state.runtime(), &t).unwrap();
    assert!(observed.busy);
    assert_eq!(observed.command, "sleep");
    plain_terminals::reconcile_at(
        &state,
        OffsetDateTime::now_utc() + time::Duration::minutes(181),
    )
    .unwrap();
    assert_eq!(store.get(&id).unwrap().unwrap().state, "live");
    assert_eq!(
        plain_call(
            &state,
            Method::DELETE,
            &format!("/terminals/{id}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn plain_terminal_watch_repo_prefix_and_node_filters_match_agents() {
    let (state, _tmux) = plain_test_state();
    let id = plain_new(&state).await;
    for (query, visible) in [
        ("repo=/private", true),
        ("repo=/private/tmp", true),
        ("repo=/priv", false),
        ("node=other-node", false),
    ] {
        let (status, watch) = plain_call(
            &state,
            Method::GET,
            &format!("/watch/state?{query}"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            watch["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"] == id),
            visible,
            "{query}: {watch}"
        );
    }
    assert_eq!(
        plain_call(
            &state,
            Method::DELETE,
            &format!("/terminals/{id}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn plain_terminal_rename_and_hide_persist_into_watch() {
    let (state, _tmux) = plain_test_state();
    let id = plain_new(&state).await;
    let path = format!("/terminals/{id}");
    let row = |watch: &Value| {
        watch["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .cloned()
            .unwrap()
    };
    let watch = || plain_call(&state, Method::GET, "/watch/state", json!({}));

    let shell = row(&watch().await.1);
    assert_eq!(shell["name"], "zsh · tmp");
    assert_eq!((shell["named"].clone(), shell["hidden"].clone()), (json!(false), json!(false)));

    let (status, body) = plain_call(&state, Method::PATCH, &path, json!({"name":"  bench runner "})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"id":id,"name":"bench runner","named":true,"hidden":false}));
    let (status, body) = plain_call(&state, Method::PATCH, &path, json!({"hidden":true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let shell = row(&watch().await.1);
    assert_eq!((shell["name"].clone(), shell["hidden"].clone()), (json!("bench runner"), json!(true)));
    let first_hide = plain_terminals::store(&state).get(&id).unwrap().unwrap().hidden_at;
    plain_call(&state, Method::PATCH, &path, json!({"hidden":true})).await;
    assert_eq!(plain_terminals::store(&state).get(&id).unwrap().unwrap().hidden_at, first_hide);

    // Blank restores the default; one call can change both.
    let (_, body) = plain_call(&state, Method::PATCH, &path, json!({"name":" ","hidden":false})).await;
    assert_eq!(body, json!({"id":id,"name":"zsh · tmp","named":false,"hidden":false}));
    let (_, body) = plain_call(&state, Method::PATCH, &path, json!({"name":"x"})).await;
    assert_eq!(body["named"], true);
    let (_, body) = plain_call(&state, Method::PATCH, &path, json!({"name":null})).await;
    assert_eq!(body["name"], "zsh · tmp");

    for bad in [
        json!({}),
        json!({"name":"a".repeat(41)}),
        json!({"name":"two\nlines"}),
        json!({"name":3}),
        json!({"hidden":"yes"}),
    ] {
        assert_eq!(plain_call(&state, Method::PATCH, &path, bad.clone()).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    assert_eq!(
        plain_call(&state, Method::PATCH, &path, json!({"name":"é".repeat(40)})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        plain_call(&state, Method::PATCH, "/terminals/term-missing", json!({"hidden":true})).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(plain_call(&state, Method::DELETE, &path, json!({})).await.0, StatusCode::OK);
    assert_eq!(
        plain_call(&state, Method::PATCH, &path, json!({"name":"late"})).await.0,
        StatusCode::NOT_FOUND
    );
}
