use super::*;
use axum::{routing::any, Router};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

fn directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "sm-gateway-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}
fn registrations(directory: &Path, active: bool) {
    let path = directory.join("registrations.json");
    let registration = Registration {
        agent_id: "agent-a".into(),
        port: super::super::FIRST_PORT,
        active,
        gateway: Some(GatewayRegistration {
            port: FIRST_PORT,
            upstream: "127.0.0.1:8420".parse().unwrap(),
        }),
    };
    fs::write(
        &path,
        serde_json::to_vec(&BTreeMap::from([("agent-a", registration)])).unwrap(),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn signed(gateway: &Gateway, method: &Method, uri: &Uri, body: &[u8], timestamp: u64) -> HeaderMap {
    let signature = request_mac(&gateway.key, "agent-a", timestamp, method, uri, body)
        .finalize()
        .into_bytes();
    HeaderMap::from_iter([
        (
            axum::http::HeaderName::from_static(AGENT_HEADER),
            HeaderValue::from_static("agent-a"),
        ),
        (
            axum::http::HeaderName::from_static(SIGNATURE_HEADER),
            HeaderValue::from_str(&format!(
                "{timestamp}:{}",
                URL_SAFE_NO_PAD.encode(signature)
            ))
            .unwrap(),
        ),
    ])
}
#[test]
fn verifier_binds_every_authority_component_and_revokes_on_suspend() {
    let dir = directory();
    let gateway = Gateway::open(&dir).unwrap();
    registrations(&dir, true);
    let verifier = StampVerifier::new(dir.clone());
    let peer = "127.0.0.1:45000".parse().unwrap();
    let method = Method::POST;
    let uri: Uri = "/queue-jobs?one=two".parse().unwrap();
    let body = b"{\"requester_session_id\":\"victim\"}";
    let headers = signed(&gateway, &method, &uri, body, seconds().unwrap());
    assert_eq!(
        verifier
            .verify(peer, &headers, &method, &uri, body)
            .unwrap()
            .unwrap()
            .agent_id(),
        "agent-a"
    );
    for (method, uri, body) in [
        (Method::DELETE, uri.clone(), body.as_slice()),
        (
            method.clone(),
            "/queue-jobs?one=three".parse().unwrap(),
            body.as_slice(),
        ),
        (method.clone(), uri.clone(), b"{}".as_slice()),
    ] {
        assert!(verifier
            .verify(peer, &headers, &method, &uri, body)
            .unwrap()
            .is_none());
    }
    assert!(verifier
        .verify("10.0.0.1:1".parse().unwrap(), &headers, &method, &uri, body)
        .unwrap()
        .is_none());
    let mut forged = headers.clone();
    forged.insert(AGENT_HEADER, HeaderValue::from_static("agent-b"));
    assert!(verifier
        .verify(peer, &forged, &method, &uri, body)
        .unwrap()
        .is_none());
    let mut duplicate = headers.clone();
    duplicate.append(AGENT_HEADER, HeaderValue::from_static("agent-a"));
    assert!(verifier
        .verify(peer, &duplicate, &method, &uri, body)
        .unwrap()
        .is_none());
    let mut bare = HeaderMap::new();
    bare.insert(AGENT_HEADER, HeaderValue::from_static("agent-a"));
    assert!(verifier
        .verify(peer, &bare, &method, &uri, body)
        .unwrap()
        .is_none());
    for timestamp in [seconds().unwrap() - 121, seconds().unwrap() + 60] {
        assert!(verifier
            .verify(
                peer,
                &signed(&gateway, &method, &uri, body, timestamp),
                &method,
                &uri,
                body
            )
            .unwrap()
            .is_none());
    }
    assert_eq!(*gateway.key, *Gateway::open(&dir).unwrap().key);
    registrations(&dir, false);
    assert!(verifier
        .verify(peer, &headers, &method, &uri, body)
        .unwrap()
        .is_none());
    fs::write(dir.join("gateway.key"), b"broken").unwrap();
    assert!(verifier
        .verify(peer, &headers, &method, &uri, body)
        .is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn forwards_only_fixed_upstream_strips_identity_and_streams_response() {
    let dir = directory();
    let gateway = Gateway::open(&dir).unwrap();
    registrations(&dir, true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let app = Router::new().fallback(any(move |request: Request<Body>| {
        let tx = tx.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = to_bytes(body, MAX_REQUEST).await.unwrap();
            let echoed_stamp = parts.headers[SIGNATURE_HEADER].clone();
            tx.send((parts.method, parts.uri, parts.headers, body))
                .await
                .unwrap();
            Response::builder()
                .header("content-type", "application/json")
                .header("set-cookie", "owner=secret")
                .header(SIGNATURE_HEADER, echoed_stamp)
                .header("x-sm-session-credential", "secret")
                .body(Body::from_stream(futures_util::stream::iter([
                    Ok::<_, io::Error>("first"),
                    Ok("second"),
                ])))
                .unwrap()
        }
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let body = "{\"requester_session_id\":\"other\",\"notify_target\":\"recipient\"}";
    let request = Request::builder()
        .method("POST")
        .uri("/queue-jobs?test=1")
        .header(AGENT_HEADER, "forged")
        .header(SIGNATURE_HEADER, "forged")
        .header("x-sm-session-id", "owner")
        .header("x-sm-session", "owner")
        .header("x-sm-session-credential", "stolen")
        .header("x-sm-local-agent-secret", "forged")
        .header("authorization", "Bearer stolen")
        .header("cookie", "sm_auth=stolen")
        .header("cf-access-jwt-assertion", "stolen")
        .header("x-forwarded-host", "owner.example")
        .header("x-forwarded-for", "1.2.3.4")
        .header("forwarded", "for=1.2.3.4")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let response = gateway.forward(request, "agent-a", upstream).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    for name in [SIGNATURE_HEADER, "set-cookie", "x-sm-session-credential"] {
        assert!(!response.headers().contains_key(name));
    }
    assert_eq!(
        &to_bytes(response.into_body(), 100).await.unwrap()[..],
        b"firstsecond"
    );
    let (method, uri, headers, payload) = rx.recv().await.unwrap();
    assert_eq!(payload, body);
    assert_eq!(headers["host"], upstream.to_string());
    for name in [
        "authorization",
        "cookie",
        "x-sm-session-id",
        "x-sm-session",
        "x-sm-session-credential",
        "x-sm-local-agent-secret",
        "cf-access-jwt-assertion",
        "x-forwarded-host",
        "x-forwarded-for",
        "forwarded",
    ] {
        assert!(!headers.contains_key(name), "{name}");
    }
    let identity = StampVerifier::new(dir.clone())
        .verify(upstream, &headers, &method, &uri, &payload)
        .unwrap()
        .unwrap();
    assert_eq!(identity.agent_id(), "agent-a");
    assert!(!headers[SIGNATURE_HEADER]
        .as_bytes()
        .windows(32)
        .any(|w| w == gateway.key.as_slice()));
    server.abort();
    fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn rejects_tunnels_absolute_targets_upgrades_and_oversized_bodies() {
    let dir = directory();
    let gateway = Gateway::open(&dir).unwrap();
    let upstream = "127.0.0.1:1".parse().unwrap();
    for (method, target) in [
        ("CONNECT", "example.com:443"),
        ("TRACE", "/"),
        ("GET", "http://example.com/"),
        ("GET", "//example.com/"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(target)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            gateway.forward(request, "agent-a", upstream).await.status(),
            StatusCode::BAD_REQUEST,
            "{method} {target}"
        );
    }
    let request = Request::builder()
        .uri("/")
        .header("upgrade", "websocket")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        gateway.forward(request, "agent-a", upstream).await.status(),
        StatusCode::BAD_REQUEST
    );
    let request = Request::builder()
        .uri("/")
        .body(Body::from(vec![0; MAX_REQUEST + 1]))
        .unwrap();
    assert_eq!(
        gateway.forward(request, "agent-a", upstream).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for upstream in [
        "0.0.0.0:8420",
        "10.0.0.1:8420",
        "127.0.0.1:18600",
        "127.0.0.1:18700",
        "127.0.0.1:0",
    ] {
        assert!(GatewayRegistration {
            port: FIRST_PORT,
            upstream: upstream.parse().unwrap()
        }
        .validate()
        .is_err());
    }
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn refuses_upstream_redirect_but_preserves_not_modified() {
    let dir = directory();
    let gateway = Gateway::open(&dir).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let app = Router::new().fallback(any(|uri: Uri| async move {
        Response::builder()
            .status(if uri.path() == "/cached" {
                StatusCode::NOT_MODIFIED
            } else {
                StatusCode::FOUND
            })
            .header("location", "https://example.com/")
            .header("set-cookie", "secret")
            .body(Body::empty())
            .unwrap()
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    for (path, status) in [
        ("/redirect", StatusCode::BAD_GATEWAY),
        ("/cached", StatusCode::NOT_MODIFIED),
    ] {
        let response = gateway
            .forward(
                Request::builder().uri(path).body(Body::empty()).unwrap(),
                "agent-a",
                upstream,
            )
            .await;
        assert_eq!(response.status(), status);
        assert!(!response.headers().contains_key("location"));
        assert!(!response.headers().contains_key("set-cookie"));
    }
    server.abort();
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn legacy_egress_registration_remains_readable() {
    let record: Registration =
        serde_json::from_str(r#"{"agent_id":"legacy","port":18700,"active":true}"#).unwrap();
    assert!(record.gateway.is_none());
    assert!(!record.environment().contains_key("SM_API_URL"));
}
