fn browser_terminal_state() -> AppState {
    let mut config = google_auth_config();
    config.cloudflare_access = cloudflare_access_config().cloudflare_access;
    config.paths.state_file = write_session_state("fork1001", "running");
    config.mobile_terminal.enabled = true;
    let state = AppState::new(config);
    seed_cloudflare_access_jwks(&state);
    state
}

fn browser_terminal_request(
    method: Method,
    path: &str,
    assertion: Option<&str>,
    origin: &str,
) -> Request {
    owner_web_request(
        method,
        path,
        "sm.example.com",
        assertion,
        &[("origin", origin)],
        &json!({}),
    )
}

async fn mint_browser_terminal_ticket(state: &AppState) -> Value {
    let assertion =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let response = router(state.clone())
        .oneshot(browser_terminal_request(
            Method::POST,
            "/client/sessions/fork1001/browser-attach-ticket",
            Some(&assertion),
            "https://sm.example.com",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response_json(response).await.1
}

fn browser_terminal_frame(ticket: &Value) -> MobileTerminalAuthFrame {
    serde_json::from_value(json!({"type":"auth", "ticket_id":ticket["ticket_id"], "ticket_secret":ticket["ticket_secret"], "output_ack":true})).unwrap()
}

#[tokio::test]
async fn browser_terminal_requires_owner_login_and_same_origin_for_mint_and_upgrade() {
    let state = browser_terminal_state();
    let owner =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let outsider =
        test_browser_access_assertion("sm-browser-aud", "outsider@example.com", 4_102_444_800);
    for (method, path) in [
        (
            Method::POST,
            "/client/sessions/fork1001/browser-attach-ticket",
        ),
        (Method::GET, "/client/terminal"),
    ] {
        for (assertion, origin, agent) in [
            (None, "https://sm.example.com", false),
            (Some(outsider.as_str()), "https://sm.example.com", false),
            (Some(owner.as_str()), "https://evil.example.com", false),
            (Some(owner.as_str()), "", false),
            (Some(owner.as_str()), "https://sm.example.com", true),
        ] {
            let mut request = browser_terminal_request(method.clone(), path, assertion, origin);
            if agent {
                request
                    .headers_mut()
                    .insert("x-sm-session-id", "agent".parse().unwrap());
            }
            let response = router(state.clone()).oneshot(request).await.unwrap();
            assert!(
                matches!(
                    response.status(),
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ),
                "{path}: {}",
                response.status()
            );
        }
    }
}

#[tokio::test]
async fn browser_terminal_ticket_is_short_lived_single_use_and_bound_to_login() {
    let state = browser_terminal_state();
    let ticket = mint_browser_terminal_ticket(&state).await;
    assert_eq!(ticket["ws_url"], "/client/terminal");
    let id = ticket["ticket_id"].as_str().unwrap();
    {
        let tickets = state.mobile_terminal_tickets.lock().unwrap();
        assert_eq!(tickets[id].kind, TerminalTicketKind::Browser);
        assert_eq!(
            tickets[id].expires_at_unix - tickets[id].created_at_unix,
            60
        );
        assert!(!format!("{:?}", tickets[id]).contains(ticket["ticket_secret"].as_str().unwrap()));
    }
    let mut frame = browser_terminal_frame(&ticket);
    // Even knowing the internal device marker cannot bypass upgrade authentication.
    frame.device_key_id = Some("browser".into());
    assert!(consume_terminal_ticket(&state, &frame, None).is_err());
    assert!(consume_terminal_ticket(&state, &frame, Some("outsider@example.com")).is_err());
    let (_, attach, _) =
        consume_terminal_ticket(&state, &frame, Some("rajeshgoli@gmail.com")).unwrap();
    remove_mobile_terminal_active_attach(&state, &attach);
    assert!(consume_terminal_ticket(&state, &frame, Some("rajeshgoli@gmail.com")).is_err());
    let ticket = mint_browser_terminal_ticket(&state).await;
    state
        .mobile_terminal_tickets
        .lock()
        .unwrap()
        .get_mut(ticket["ticket_id"].as_str().unwrap())
        .unwrap()
        .expires_at_unix = 0;
    assert!(consume_terminal_ticket(
        &state,
        &browser_terminal_frame(&ticket),
        Some("rajeshgoli@gmail.com")
    )
    .is_err());
}

#[tokio::test]
async fn browser_terminal_does_not_remove_phone_signature_requirement() {
    let key = SigningKey::random(&mut OsRng);
    let state = AppState::new(mobile_ticket_config(&key));
    let ticket = mint_mobile_attach_ticket(&state, &key, "phone-proof").await;
    let mut frame = signed_mobile_terminal_auth_frame(&key, &ticket, "ws-proof");
    frame.signature = None;
    assert!(consume_terminal_ticket(&state, &frame, None).is_err());
    assert!(consume_terminal_ticket(&state, &frame, Some("rajeshgoli@gmail.com")).is_err());
}

#[tokio::test]
async fn browser_terminal_authenticated_websocket_echoes_input_and_detaches() {
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{client::IntoClientRequest, Message as ClientMessage},
    };
    struct TestTmux(String);
    impl Drop for TestTmux {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["-L", &self.0, "kill-server"])
                .output();
        }
    }
    let tmux = TestTmux(format!(
        "sm-browser-test-{}",
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let created = Command::new("tmux")
        .args([
            "-L",
            &tmux.0,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "terminal",
            "cat",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let state = browser_terminal_state();
    let ticket = mint_browser_terminal_ticket(&state).await;
    {
        let mut tickets = state.mobile_terminal_tickets.lock().unwrap();
        let stored = tickets
            .get_mut(ticket["ticket_id"].as_str().unwrap())
            .unwrap();
        stored.tmux_socket_name = Some(tmux.0.clone());
        stored.tmux_session = "terminal".into();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let assertion =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let request = || {
        let mut req = format!("ws://{addr}/client/terminal")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("host", "sm.example.com".parse().unwrap());
        req.headers_mut()
            .insert("origin", "https://sm.example.com".parse().unwrap());
        req.headers_mut()
            .insert("cf-access-jwt-assertion", assertion.parse().unwrap());
        req
    };
    let mut missing = request();
    missing.headers_mut().remove("cf-access-jwt-assertion");
    assert!(connect_async(missing).await.is_err());
    let mut foreign = request();
    foreign
        .headers_mut()
        .insert("origin", "https://evil.example.com".parse().unwrap());
    assert!(connect_async(foreign).await.is_err());
    let (mut client, _) = connect_async(request()).await.unwrap();
    client.send(ClientMessage::Text(json!({"type":"auth", "ticket_id":ticket["ticket_id"], "ticket_secret":ticket["ticket_secret"], "output_ack":true}).to_string().into())).await.unwrap();
    client
        .send(ClientMessage::Text(
            json!({"type":"resize", "cols":80, "rows":24})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let mut attached = false;
    let mut output = Vec::new();
    timeout(Duration::from_secs(10), async {
        while let Some(message) = client.next().await {
            let message = message.unwrap();
            let ClientMessage::Text(text) = message else {
                if matches!(message, ClientMessage::Close(_)) {
                    panic!("Closed before echo: {message:?}");
                }
                client.flush().await.unwrap();
                continue;
            };
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["type"] == "status" {
                assert_eq!(frame["state"], "attached");
                attached = true;
                client
                    .send(ClientMessage::Text(
                        json!({"type":"input", "data":"browser-terminal-echo\r"})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
            if frame["type"] == "output" {
                output.extend(STANDARD.decode(frame["data"].as_str().unwrap()).unwrap());
                client
                    .send(ClientMessage::Text(
                        json!({"type":"output_ack", "sequence":frame["sequence"]})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                if String::from_utf8_lossy(&output).contains("browser-terminal-echo") {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(attached);
    // The actual tmux client must see later dimensions, not just the initial size.
    for (cols, rows) in [(40, 12), (120, 36)] {
        client
            .send(ClientMessage::Text(
                json!({"type":"resize", "cols":cols, "rows":rows})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let expected = format!("{cols}x{rows}");
        let mut observed = String::new();
        for _ in 0..40 {
            let size = Command::new("tmux")
                .args([
                    "-L",
                    &tmux.0,
                    "list-clients",
                    "-F",
                    "#{client_width}x#{client_height}",
                ])
                .output()
                .unwrap();
            observed = String::from_utf8_lossy(&size.stdout).trim().to_owned();
            if observed == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            observed, expected,
            "tmux client must receive resize signals"
        );
    }

    assert!(String::from_utf8_lossy(&output).contains("browser-terminal-echo"));
    assert!(consume_terminal_ticket(
        &state,
        &browser_terminal_frame(&ticket),
        Some("rajeshgoli@gmail.com")
    )
    .is_err());
    client
        .send(ClientMessage::Text(
            json!({"type":"detach"}).to_string().into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        while let Some(Ok(message)) = client.next().await {
            if matches!(message, ClientMessage::Close(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    server.abort();
}
