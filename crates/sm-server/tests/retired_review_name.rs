//! The pre-#1786 `sm request-codex-review` name must register a review through
//! the real executable, not only parse (#1855).

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

#[test]
fn retired_request_codex_review_name_posts_a_review_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut content_length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap();
                }
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        let reply = r#"{"requested_head_sha":"abcdef1234"}"#;
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
        (request_line, String::from_utf8(body).unwrap())
    });

    let output = Command::new(env!("CARGO_BIN_EXE_sm"))
        .args([
            "request-codex-review",
            "967",
            "--notify",
            "notify1",
            "--repo",
            "rajeshgoli/session-manager",
            "--api-url",
            &format!("http://{addr}"),
        ])
        .env_remove("SESSION_MANAGER_ID")
        .env_remove("CLAUDE_SESSION_MANAGER_ID")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "sm failed: {stderr}");

    let (request_line, body) = server.join().unwrap();
    assert!(
        request_line.starts_with("POST /review-requests "),
        "{request_line}"
    );
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["pr_number"], 967);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Review requested for PR #967"));
}
