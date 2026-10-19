//! Exercise the real CLI's draft, submission and source-cleanup protocol.
use serde_json::{json, Value};
use sm_server::sessions::handoff_notes::digest;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

fn run(root: &Path, args: &[&str], responses: Vec<(&str, u16, Value)>) -> (Output, Vec<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let mut responses = responses;
    if responses.last().is_some_and(|(path, status, _)| {
        *path == "POST /sessions/agent001/handoff" && *status == 200
    }) && !responses
        .first()
        .is_some_and(|(_, _, body)| body["resubmit"] == true)
    {
        responses.push((
            "POST /sessions/agent001/handoff-note/source",
            200,
            json!({"ok":true}),
        ));
    }
    let responses = responses
        .into_iter()
        .map(|(path, status, body)| (path.to_owned(), status, body))
        .collect::<Vec<_>>();
    let server = thread::spawn(move || {
        let mut bodies = vec![];
        for (expected, status, response) in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("expected {expected}: {e}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = vec![];
            let mut buffer = [0; 4096];
            let (header_end, length) = loop {
                let n = socket.read(&mut buffer).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buffer[..n]);
                if let Some(at) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..at]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|s| s.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    assert!(
                        header.starts_with(&format!("{expected} HTTP/1.1")),
                        "{}",
                        header.lines().next().unwrap()
                    );
                    break (at + 4, length);
                }
            };
            while request.len() < header_end + length {
                let n = socket.read(&mut buffer).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buffer[..n]);
            }
            bodies.push(if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&request[header_end..header_end + length]).unwrap()
            });
            let body = response.to_string();
            write!(socket,"HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        bodies
    });
    let output = Command::new(env!("CARGO_BIN_EXE_sm"))
        .args(["--api-url", &format!("http://{addr}"), "handoff"])
        .args(args)
        .env("SESSION_MANAGER_ID", "agent001")
        .current_dir(root)
        .output()
        .unwrap();
    (output, server.join().unwrap())
}
fn root() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "handoff-cli-{}-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn handoff_cli_drafts_refuses_unchanged_retains_incomplete_and_removes_accepted() {
    let root = root();
    let path = root.join(".sm/handoff.md");
    let text = "# Handoff\n<!-- hint -->\n";
    let draft = json!({"path":path,"text":text,"sha256":digest(text)});
    let get = "GET /sessions/agent001/handoff-draft";
    let post = "POST /sessions/agent001/handoff";
    let (output, _) = run(&root, &[], vec![(get, 200, draft.clone())]);
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    let (output, _) = run(&root, &[], vec![(get, 200, draft.clone())]);
    assert_eq!(output.status.code(), Some(2));
    fs::write(&path, "<!-- remove hint -->\nNext: continue.").unwrap();
    let (output, _) = run(
        &root,
        &[],
        vec![
            (get, 200, draft.clone()),
            (
                post,
                409,
                json!({"error":"incomplete","message":"missing job"}),
            ),
        ],
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(path.exists());
    let (output, bodies) = run(
        &root,
        &[],
        vec![(get, 200, draft), (post, 200, json!({"status":"accepted"}))],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        bodies[1]["note"],
        json!({"kind":"text","value":"Next: continue.","source":"draft"})
    );
    assert!(!root.join(".sm").exists());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn handoff_cli_imports_file_and_reports_its_removal() {
    let root = root();
    let path = root.join("note.md");
    fs::write(&path, "<!--keep--> Next step").unwrap();
    let draft =
        json!({"path":root.join(".sm/handoff.md"),"text":"template","sha256":digest("template")});
    let (output, bodies) = run(
        &root,
        &["--path", path.to_str().unwrap()],
        vec![
            ("GET /sessions/agent001/handoff-draft", 200, draft),
            (
                "POST /sessions/agent001/handoff",
                200,
                json!({"status":"accepted"}),
            ),
            (
                "POST /sessions/agent001/handoff-note/source",
                200,
                json!({"ok":true}),
            ),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!path.exists());
    assert_eq!(bodies[1]["note"]["value"], "<!--keep--> Next step");
    assert_eq!(bodies[2]["removed"], true);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn handoff_cli_failed_retry_reuses_stored_text_and_show_prints_it() {
    let root = root();
    let draft = json!({"path":root.join(".sm/handoff.md"),"text":"template","sha256":digest("template"),"resubmit":true});
    let (output, bodies) = run(
        &root,
        &[],
        vec![
            ("GET /sessions/agent001/handoff-draft", 200, draft),
            (
                "POST /sessions/agent001/handoff",
                200,
                json!({"status":"accepted"}),
            ),
        ],
    );
    assert!(output.status.success());
    assert_eq!(bodies[1]["resubmit"], true);
    assert!(bodies[1].get("note").is_none());
    assert!(!root.join(".sm").exists());
    let view = json!({"notes":[{"session":{"id":"pred0001","name":"predecessor"},"text":"Saved next steps"}],"owner_messages":[{"at":"today","source":"terminal","session":{"name":"predecessor"},"text":"Exact owner words"}],"line_note_count":2,"owner_name":"Rajesh"});
    let (output, _) = run(
        &root,
        &["show"],
        vec![(
            "GET /sessions/agent001/handoff-note?line=0&own=1",
            200,
            view,
        )],
    );
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Saved next steps"));
    assert!(text.contains("Exact owner words"));
    assert!(text.contains("sm handoff show --line"));
    fs::remove_dir_all(root).unwrap();
}
