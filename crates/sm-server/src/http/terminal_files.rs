//! Owner-only, bounded local file reads for links printed in the web terminal.
use super::*;
use std::io::Read;

const LIMIT: u64 = 10 * 1024 * 1024;

#[derive(Deserialize)]
pub(super) struct FileQuery {
    session: String,
    path: String,
}

pub(super) async fn read_file(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FileQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    if owner_web_guard(
        &state,
        request.headers(),
        request_peer_addr(&request),
        "GET",
    )?
    .is_none()
    {
        return Err(browser_terminal::denied("Browser owner login required"));
    }
    board::blocking(&state, move |state| {
        let cwd = if let Some(session) = resolve_session_or_registry_role(state, &query.session)? {
            if !crate::sessions::is_primary_node(&session.node) {
                return Err(bad_file("Files on remote devices are not available here"));
            }
            session.working_dir
        } else {
            plain_terminals::store(state)
                .get(&query.session)?
                .ok_or(ApiError::NotFound("Terminal not found"))?
                .cwd
        };
        file_response(&cwd, &query.path)
    })
    .await
}

fn bad_file(detail: &str) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

fn file_response(cwd: &str, path: &str) -> Result<Response, ApiError> {
    if path.is_empty() || path.contains('\0') {
        return Err(bad_file("Invalid file path"));
    }
    let path = expand_home(cwd).join(expand_home(path));
    let metadata = std::fs::metadata(&path).map_err(|_| ApiError::NotFound("File not found"))?;
    if !metadata.is_file() {
        return Err(bad_file("This path is not a regular file"));
    }
    if metadata.len() > LIMIT {
        return Err(bad_file("File exceeds the 10 MiB preview limit"));
    }
    // O_NONBLOCK prevents a replacement FIFO from blocking a worker at open.
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .map_err(|_| ApiError::NotFound("File could not be opened"))?;
    if !file.metadata()?.is_file() {
        return Err(bad_file("This path is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(bad_file("File exceeds the 10 MiB preview limit"));
    }
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut mime = match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        _ if std::str::from_utf8(&bytes).is_ok() && !bytes.contains(&0) => {
            "text/plain; charset=utf-8"
        }
        _ => return Err(bad_file("This file type cannot be previewed")),
    };
    if mime.starts_with("text/plain") {
        let escape = crate::owner_docs::escape_html;
        let title = escape(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("File"),
        );
        let text = escape(std::str::from_utf8(&bytes).expect("text preview validated above"));
        // HTML parsing consumes one initial newline after <pre>; preserve the file's own.
        bytes = format!("<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>:root{{color-scheme:light dark}}body{{margin:1rem}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;font:14px/1.5 monospace}}</style><pre>\n{text}</pre>").into_bytes();
        mime = "text/html; charset=utf-8";
    }
    // Chrome refuses synthetic text/image documents with CSP sandbox. Only
    // authored active documents need an opaque origin; passive MIME types use
    // nosniff and a restrictive resource policy instead.
    let policy = if mime.starts_with("text/html") || extension == "svg" {
        "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    } else {
        "default-src 'none'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    };
    Ok((
        [
            ("content-type", mime),
            ("cache-control", "no-store"),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            // Authored HTML/SVG must never execute with the owner's SM origin.
            ("content-security-policy", policy),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preview_resolves_paths_and_sandboxes_authored_html() {
        let root = std::env::temp_dir().join(format!(
            "sm-file-preview-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let dir = root.as_path();
        std::fs::write(
            dir.join("memo.html"),
            "<script>alert(1)</script><h1>Memo</h1>",
        )
        .unwrap();
        let response = file_response(dir.to_str().unwrap(), "./memo.html").unwrap();
        assert_eq!(
            response.headers()["content-type"],
            "text/html; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .starts_with("sandbox;"));
        assert!(file_response("/", dir.join("memo.html").to_str().unwrap()).is_ok());
        assert!(file_response(dir.to_str().unwrap(), ".").is_err());
        assert!(file_response(dir.to_str().unwrap(), "missing").is_err());
        std::fs::write(dir.join("source.rs"), "fn main() {}\n").unwrap();
        let text = file_response(dir.to_str().unwrap(), "source.rs").unwrap();
        assert_eq!(text.headers()["content-type"], "text/html; charset=utf-8");
        assert!(text.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("sandbox"));
        std::fs::write(dir.join("escaped.txt"), "<script>alert(1)</script>&").unwrap();
        let escaped = file_response(dir.to_str().unwrap(), "escaped.txt").unwrap();
        let body = to_bytes(escaped.into_body(), 1024 * 1024).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;&amp;"));
        assert!(!body.contains("<script>"));
        std::fs::write(dir.join("binary"), [0, 255]).unwrap();
        assert!(file_response(dir.to_str().unwrap(), "binary").is_err());
        let large = std::fs::File::create(dir.join("large.txt")).unwrap();
        large.set_len(LIMIT + 1).unwrap();
        assert!(file_response(dir.to_str().unwrap(), "large.txt").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
