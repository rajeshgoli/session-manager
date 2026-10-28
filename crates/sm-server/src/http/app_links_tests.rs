use super::*;

async fn get(config: AppConfig, path: &str) -> (StatusCode, String, String) {
    let mut request = local_request(Method::GET, path, Body::empty());
    // From the internet through the tunnel, with no Cloudflare Access assertion.
    request.headers_mut().insert(HOST, "sm.example.com".parse().unwrap());
    let response = router(AppState::new(config)).oneshot(request).await.unwrap();
    let status = response.status();
    let kind = response.headers().get(CONTENT_TYPE).map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, kind, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn assetlinks_is_public_exact_and_absent_until_configured() {
    let mut config = context_monitor_test_config("claude", "stopped");
    config.cloudflare_access.browser.enabled = true;
    config.cloudflare_access.browser.hostname = Some("sm.example.com".to_owned());
    config.cloudflare_access.browser.jwt_audience = Some("browser-aud".to_owned());
    let (status, _, _) = get(config.clone(), "/.well-known/assetlinks.json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    config.android_app_links.package_name = "li.rajeshgo.sm".to_owned();
    config.android_app_links.sha256_cert_fingerprints = vec!["fdbf0bbd96cb4f42912a78edb94bca7c4fc10b30302f611731a2bfa00300f6bf".to_owned()];
    let (status, kind, body) = get(config.clone(), "/.well-known/assetlinks.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kind, "application/json");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body[0]["target"]["package_name"], "li.rajeshgo.sm");
    assert_eq!(body[0]["relation"], json!(["delegate_permission/common.handle_all_urls"]));
    // Only the exact path: nothing below or beside it, and no dot-segment walk to another route.
    for path in [
        "/.well-known/assetlinks.json/",
        "/.well-known/assetlinks.json/x",
        "/.well-known/assetlinks.jsonx",
        "/.well-known/",
        "/.well-known/assetlinks.json/../../health",
        "/.well-known/assetlinks.json/%2e%2e/%2e%2e/health",
    ] {
        let (status, _, body) = get(config.clone(), path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
}
