use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    thread,
};

use jsonwebtoken::{DecodingKey, Validation};
use rand_core::{OsRng, RngCore};

use super::*;

/// A fresh PKCS#8 RSA key pair, the format Google issues. Generated per run
/// so no private key is committed.
fn rsa_key_pair(dir: &Path) -> (String, String) {
    let private_path = dir.join("key.pem");
    let status = Command::new("openssl")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
            "-out",
        ])
        .arg(&private_path)
        .status()
        .expect("openssl is required for FCM signing tests");
    assert!(status.success());
    let public = Command::new("openssl")
        .args(["pkey", "-pubout", "-in"])
        .arg(&private_path)
        .output()
        .unwrap();
    assert!(public.status.success());
    (
        fs::read_to_string(&private_path).unwrap(),
        String::from_utf8(public.stdout).unwrap(),
    )
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sm-push-fcm-{}-{}",
        std::process::id(),
        OsRng.next_u64()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_key(dir: &Path, private_key: &str, token_uri: &str) -> PathBuf {
    let path = dir.join("service-account.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "type": "service_account",
            "project_id": "proj-1",
            "client_email": "sm-push@proj-1.iam.gserviceaccount.com",
            "private_key": private_key,
            "token_uri": token_uri,
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

#[derive(Debug, Clone)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Serves `responses` in order, one per connection, recording requests.
fn stub_server(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let log = recorded.clone();
    thread::spawn(move || {
        for (status, body) in responses {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (key, value) = line.split_once(':').unwrap();
                headers.push((key.trim().to_owned(), value.trim().to_owned()));
            }
            let length = headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.parse::<usize>().unwrap())
                .unwrap_or(0);
            let mut request_body = vec![0u8; length];
            reader.read_exact(&mut request_body).unwrap();
            log.lock().unwrap().push(Recorded {
                request_line: request_line.trim_end().to_owned(),
                headers,
                body: String::from_utf8(request_body).unwrap(),
            });
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (base, recorded)
}

#[test]
fn jwt_claims_shape() {
    let dir = temp_dir();
    let (private_key, public_key) = rsa_key_pair(&dir);
    let sender = FcmSender::load(&write_key(&dir, &private_key, DEFAULT_TOKEN_URI)).unwrap();
    assert_eq!(sender.project_id(), "proj-1");
    let now = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
    let assertion = sender.signed_assertion(now).unwrap();
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[DEFAULT_TOKEN_URI]);
    validation.validate_exp = false;
    let decoded = jsonwebtoken::decode::<JwtClaims>(
        &assertion,
        &DecodingKey::from_rsa_pem(public_key.as_bytes()).unwrap(),
        &validation,
    )
    .unwrap();
    assert_eq!(
        decoded.claims,
        JwtClaims {
            iss: "sm-push@proj-1.iam.gserviceaccount.com".to_owned(),
            scope: "https://www.googleapis.com/auth/firebase.messaging".to_owned(),
            aud: "https://oauth2.googleapis.com/token".to_owned(),
            iat: 1_790_000_000,
            exp: 1_790_003_600,
        }
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn send_body_shape_and_access_token_reuse() {
    let dir = temp_dir();
    let (private_key, _) = rsa_key_pair(&dir);
    let (base, recorded) = stub_server(vec![
        (
            200,
            json!({"access_token": "tok-1", "expires_in": 3599}).to_string(),
        ),
        (200, json!({"name": "projects/proj-1/messages/1"}).to_string()),
        (200, json!({"name": "projects/proj-1/messages/2"}).to_string()),
        (
            404,
            json!({"error": {"code": 404, "status": "NOT_FOUND", "message": "Requested entity was not found.",
                "details": [{"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError", "errorCode": "UNREGISTERED"}]}})
            .to_string(),
        ),
    ]);
    let sender = FcmSender::load(&write_key(&dir, &private_key, &format!("{base}/token")))
        .unwrap()
        .with_fcm_base_url(&base);
    let data = BTreeMap::from([
        ("kind".to_owned(), "test".to_owned()),
        ("title".to_owned(), "sm notifications work".to_owned()),
    ]);
    sender.send("device-token-1", &data).unwrap();
    sender.send("device-token-2", &data).unwrap();
    assert!(matches!(
        sender.send("device-token-3", &data),
        Err(PushError::InvalidToken(_))
    ));

    let requests = recorded.lock().unwrap().clone();
    assert_eq!(requests.len(), 4, "one token exchange, three sends");
    assert_eq!(requests[0].request_line, "POST /token HTTP/1.1");
    assert!(requests[0].body.starts_with(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion="
    ));
    assert_eq!(
        requests[1].request_line,
        "POST /v1/projects/proj-1/messages:send HTTP/1.1"
    );
    assert_eq!(requests[1].header("authorization"), Some("Bearer tok-1"));
    assert_eq!(requests[2].header("authorization"), Some("Bearer tok-1"));
    let body: Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(
        body,
        json!({"message": {
            "token": "device-token-1",
            "android": {"priority": "HIGH", "ttl": "86400s"},
            "data": {"kind": "test", "title": "sm notifications work"},
        }})
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn send_errors_classify_by_token_validity() {
    let unregistered = json!({"error": {"status": "NOT_FOUND",
        "details": [{"errorCode": "UNREGISTERED"}]}})
    .to_string();
    assert!(matches!(
        classify_send_error(404, &unregistered),
        PushError::InvalidToken(_)
    ));
    let bad_token = json!({"error": {"status": "INVALID_ARGUMENT",
        "message": "The registration token is not a valid FCM registration token"}})
    .to_string();
    assert!(matches!(
        classify_send_error(400, &bad_token),
        PushError::InvalidToken(_)
    ));
    let bad_payload = json!({"error": {"status": "INVALID_ARGUMENT",
        "message": "Invalid value at 'message.data'"}})
    .to_string();
    assert!(matches!(
        classify_send_error(400, &bad_payload),
        PushError::Retryable(_)
    ));
    for status in [401, 403, 429, 500, 503] {
        assert!(matches!(
            classify_send_error(status, "{}"),
            PushError::Retryable(_)
        ));
    }
}

#[test]
fn load_rejects_a_key_without_a_usable_private_key() {
    let dir = temp_dir();
    let path = write_key(&dir, "not a pem", DEFAULT_TOKEN_URI);
    let error = FcmSender::load(&path).unwrap_err();
    assert!(format!("{error:#}").contains("unusable private_key"));
    fs::remove_dir_all(dir).unwrap();
}
