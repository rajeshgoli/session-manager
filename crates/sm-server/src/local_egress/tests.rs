use super::*;
use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf};

pub(super) fn directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "sm-egress-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}
#[test]
fn rejects_non_public_and_encoded_private_addresses() {
    for address in [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "0.0.0.0",
        "100.64.0.1",
        "224.0.0.1",
        "::",
        "::1",
        "fc00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "64:ff9b::a00:1",
        "2002:7f00:1::",
        "2001:db8::1",
    ] {
        assert!(!public_address(address.parse().unwrap()), "{address}");
    }
    for address in [
        "1.1.1.1",
        "8.8.8.8",
        "2606:4700:4700::1111",
        "::ffff:8.8.8.8",
    ] {
        assert!(public_address(address.parse().unwrap()), "{address}");
    }
}
#[test]
fn parses_only_bounded_https_connect_authorities() {
    assert_eq!(
        parse_request(b"CONNECT docs.rs:443 HTTP/1.1\r\nHost: docs.rs:443\r\n\r\n"),
        Ok(("docs.rs".into(), 443))
    );
    assert_eq!(
        parse_request(b"CONNECT [::1]:443 HTTP/1.1\r\n\r\n"),
        Ok(("::1".into(), 443))
    );
    for request in [
        "CONNECT host:22 HTTP/1.1\r\n\r\n",
        "CONNECT host:80 HTTP/1.1\r\n\r\n",
        "GET http://host/path HTTP/1.1\r\n\r\n",
        "CONNECT user@host:443 HTTP/1.1\r\n\r\n",
        "CONNECT host:443 HTTP/9\r\n\r\n",
        "CONNECT host:443 HTTP/1.1\r\nBad header\r\n\r\n",
        "CONNECT host:443 HTTP/1.1\r\nContent-Length: 3\r\n\r\n",
        "CONNECT host:443 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
    ] {
        assert!(parse_request(request.as_bytes()).is_err(), "{request}");
    }
}
struct FixtureResolver(Vec<IpAddr>);
impl Resolver for FixtureResolver {
    fn resolve<'a>(
        &'a self,
        _: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>>
    {
        Box::pin(async { Ok(self.0.clone()) })
    }
}
#[tokio::test]
async fn refusals_have_403_and_agent_attribution_without_content() {
    let dir = directory();
    let mut proxy = Proxy::new(&dir).unwrap();
    // One public answer must not hide the private answer in the same DNS set.
    proxy.resolver = Arc::new(FixtureResolver(vec![
        "8.8.8.8".parse().unwrap(),
        "127.0.0.1".parse().unwrap(),
    ]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = [
        (
            "CONNECT public.example:443 HTTP/1.1\r\nSecret: do-not-log\r\n\r\n",
            "non_public_address",
        ),
        ("CONNECT 127.0.0.1:8420 HTTP/1.1\r\n\r\n", "port_not_443"),
        (
            "CONNECT localhost:443 HTTP/1.1\r\n\r\n",
            "non_public_address",
        ),
        (
            "CONNECT 169.254.169.254:443 HTTP/1.1\r\n\r\n",
            "non_public_address",
        ),
        ("CONNECT example.org:22 HTTP/1.1\r\n\r\n", "port_not_443"),
        ("CONNECT example.org:80 HTTP/1.1\r\n\r\n", "port_not_443"),
        ("CONNECT bad HTTP/1.1\r\n\r\n", "malformed_request"),
        (
            "GET http://example.org/private-path HTTP/1.1\r\n\r\n",
            "connect_only",
        ),
    ];
    for (request, _) in requests {
        let mut client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let proxy = proxy.clone();
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            proxy.serve(server, "agent-a".into(), stopped).await;
        });
        client.write_all(request.as_bytes()).await.unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        assert!(reply.starts_with(b"HTTP/1.1 403"));
        task.await.unwrap();
    }
    let log = fs::read_to_string(dir.join("connections.jsonl")).unwrap();
    assert!(!log.contains("do-not-log"));
    assert!(!log.contains("private-path"));
    for (line, (_, reason)) in log.lines().zip(requests) {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(record["agent_id"], "agent-a");
        assert_eq!(record["outcome"], reason);
        assert_eq!(record["bytes_to_host"], 0);
    }
    assert_eq!(log.lines().count(), requests.len());
    fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn counts_transferred_bytes_and_preserves_half_close() {
    let (mut source, mut input) = tokio::io::duplex(128);
    let (mut output, mut target) = tokio::io::duplex(128);
    source.write_all(b"opaque TLS bytes").await.unwrap();
    source.shutdown().await.unwrap();
    let mut count = 0;
    let activity = tokio::sync::watch::channel(tokio::time::Instant::now()).0;
    let result = copy_counted(
        &mut input,
        &mut output,
        &mut count,
        &activity,
        Duration::from_secs(1),
    )
    .await;
    result.unwrap();
    assert_eq!(count, 16);
    let mut bytes = Vec::new();
    target.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(bytes, b"opaque TLS bytes");
}

struct FixtureNetworks(Option<Vec<networks::Network>>);
impl networks::Networks for FixtureNetworks {
    fn current(&self) -> io::Result<Vec<networks::Network>> {
        self.0
            .clone()
            .ok_or_else(|| io::Error::other("fixture interface failure"))
    }
}
#[tokio::test]
async fn globally_addressed_lan_and_interface_failures_are_refused() {
    let dir = directory();
    for (ip, mask, fail) in [
        ("8.8.8.8", "255.255.255.0", false),
        ("2606:4700:1234:5678::abcd", "ffff:ffff:ffff:ffff::", false),
        ("8.8.8.8", "255.255.255.0", true),
    ] {
        let mut proxy = Proxy::new(&dir).unwrap();
        proxy.resolver = Arc::new(FixtureResolver(vec![ip.parse().unwrap()]));
        proxy.networks = Arc::new(FixtureNetworks((!fail).then(|| {
            vec![networks::Network {
                address: ip.parse().unwrap(),
                mask: mask.parse().unwrap(),
            }]
        })));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            proxy.serve(server, "local-lan".into(), stopped).await;
        });
        client
            .write_all(b"CONNECT public.example:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 403"));
        task.await.unwrap();
        let log = fs::read_to_string(dir.join("connections.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(
            record["outcome"],
            if fail {
                "interface_lookup_failed"
            } else {
                "local_network_address"
            }
        );
    }
    fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn opposite_direction_activity_keeps_idle_read_alive_then_times_out() {
    let (mut source, mut input) = tokio::io::duplex(128);
    let (mut output, mut target) = tokio::io::duplex(128);
    let activity = tokio::sync::watch::channel(tokio::time::Instant::now()).0;
    let copy_activity = activity.clone();
    let task = tokio::spawn(async move {
        let mut count = 0;
        copy_counted(
            &mut input,
            &mut output,
            &mut count,
            &copy_activity,
            Duration::from_millis(100),
        )
        .await
    });
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        activity.send_replace(tokio::time::Instant::now());
    }
    assert!(!task.is_finished());
    source.write_all(b"still alive").await.unwrap();
    let mut bytes = [0; 11];
    target.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"still alive");
    assert_eq!(
        task.await.unwrap().unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
}
