/// Every path `router()` registers, read from its source so a route added
/// later cannot skip `no_route_answers_a_public_host_without_identity`.
fn registered_route_paths() -> Vec<String> {
    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/http.rs"));
    let start = source
        .find("pub fn router(state: AppState) -> Router {")
        .unwrap();
    let end = start + source[start..].find(".fallback(not_found);").unwrap();
    let mut paths = Vec::new();
    for chunk in source[start..end].split(".route(").skip(1) {
        let chunk = chunk.trim_start();
        if let Some(rest) = chunk.strip_prefix('"') {
            paths.push(rest[..rest.find('"').unwrap()].to_owned());
        } else if chunk.starts_with("DEFAULT_EMAIL_WEBHOOK_PATH") {
            paths.push(DEFAULT_EMAIL_WEBHOOK_PATH.to_owned());
        } else {
            panic!("route path is not a literal: {}", &chunk[..chunk.len().min(60)]);
        }
    }
    paths
}

/// `/users/{id}/x` -> `/users/missing/x`; `{*rest}` takes the same stand-in.
fn concrete_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with('{') {
                "missing"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[tokio::test]
async fn no_route_answers_a_public_host_without_identity() {
    let mut config = google_auth_config();
    config.cloudflare_access = cloudflare_access_config().cloudflare_access;
    config.paths.state_file = write_session_state("fork1001", "running");
    let state = AppState::new(config);
    seed_cloudflare_access_jwks(&state);
    let app = router(state);

    let paths = registered_route_paths();
    assert!(paths.len() > 200, "parsed only {} routes", paths.len());
    // Signed for the browser application, so it fails on every other host.
    let wrong_audience =
        test_browser_access_assertion("sm-other-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let expired =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 1_000_000_000);
    let mut answered = Vec::new();
    for path in &paths {
        // Android verifies App Links without signing in (#2594). The email
        // webhook needs its worker secret instead of a login; with a real body
        // and no secret it refuses (`inbound_email_on_browser_access_hostname_
        // still_uses_worker_secret`), but this empty body fails parsing first.
        if path == "/.well-known/assetlinks.json" || path == DEFAULT_EMAIL_WEBHOOK_PATH {
            continue;
        }
        let uri = concrete_path(path);
        for method in [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ] {
            for (host, assertion) in [
                ("sm.example.com", None),
                ("sm.example.com", Some("not-a-jwt")),
                ("sm.example.com", Some(wrong_audience.as_str())),
                ("sm.example.com", Some(expired.as_str())),
                ("sm-app.example.com", None),
                ("sm-node.example.com", None),
                ("sm-email.example.com", None),
            ] {
                let request =
                    owner_web_request(method.clone(), &uri, host, assertion, &[], &json!({}));
                let response = tokio::time::timeout(
                    Duration::from_secs(10),
                    app.clone().oneshot(request),
                )
                .await;
                let status = match response {
                    Ok(response) => response.unwrap().status().as_u16(),
                    Err(_) => 0,
                };
                // 405: the route does not take this method, so nothing answered.
                if !matches!(status, 401 | 403 | 405) {
                    answered.push(format!(
                        "{method} {uri} host={host} assertion={} -> {status}",
                        assertion.is_some()
                    ));
                }
            }
        }
    }
    assert!(
        answered.is_empty(),
        "{} requests answered without identity:\n{}",
        answered.len(),
        answered.join("\n")
    );
}

/// The four routes that answered before #2603 also need the owner's login, not
/// just any verified Access assertion, and the Google-auth host's session.
#[tokio::test]
async fn formerly_open_routes_refuse_a_stranger_and_answer_the_owner() {
    let mut config = google_auth_config();
    config.cloudflare_access = cloudflare_access_config().cloudflare_access;
    config.paths.state_file = write_session_state("fork1001", "running");
    let state = AppState::new(config);
    seed_cloudflare_access_jwks(&state);
    let app = router(state);
    let owner =
        test_browser_access_assertion("sm-browser-aud", "rajeshgoli@gmail.com", 4_102_444_800);
    let stranger =
        test_browser_access_assertion("sm-browser-aud", "stranger@example.com", 4_102_444_800);
    for uri in [
        "/health",
        "/health/detailed",
        "/auth/session",
        "/client/terminal/probe",
    ] {
        let (status, body) = browser_host_get(&app, uri, Some(&owner)).await;
        assert_eq!(status, StatusCode::OK, "{uri} owner: {body}");
        let (status, body) = browser_host_get(&app, uri, Some(&stranger)).await;
        assert!(is_auth_denial_status(status.as_u16()), "{uri} stranger: {status} {body}");
        let response = app
            .clone()
            .oneshot(public_request(Method::GET, uri, Body::empty()))
            .await
            .unwrap();
        assert!(
            is_auth_denial_status(response.status().as_u16()),
            "{uri} google host: {}",
            response.status()
        );
        let response = app
            .clone()
            .oneshot(local_request(Method::GET, uri, Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri} localhost");
    }
}
