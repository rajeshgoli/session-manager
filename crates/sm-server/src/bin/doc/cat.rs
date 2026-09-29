use super::*;
use sm_server::{doc_markdown, owner_docs::git_blob_sha};

#[derive(Args)]
pub(super) struct DocCatArgs {
    doc: String,
    #[arg(long)]
    version: Option<String>,
    #[arg(long)]
    raw: bool,
    #[arg(long, num_args = 0..=1)]
    out: Option<Option<PathBuf>>,
}

fn local_path(doc: &str) -> Option<PathBuf> {
    if doc.starts_with("http://") || doc.starts_with("https://") {
        return None;
    }
    let path = PathBuf::from(doc);
    path.is_file().then_some(path)
}

fn cache_path(name: &str, sha: &str, raw: bool) -> Result<PathBuf> {
    // A readable doc name must never escape the temp cache, including when
    // metadata is supplied by a remote server.
    if Path::new(name)
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("invalid doc cache path");
    }
    if sha.len() < 12 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid doc revision");
    }
    Ok(std::env::temp_dir().join("sm-doc").join(format!(
        "{name}@{}.{}",
        &sha[..12],
        if raw { "raw" } else { "md" }
    )))
}

fn render(path: &str, bytes: Vec<u8>, raw: bool) -> Result<Vec<u8>> {
    if raw || !doc_markdown::is_html(path) {
        return Ok(bytes);
    }
    Ok(doc_markdown::convert(std::str::from_utf8(&bytes)?)?.into_bytes())
}

fn write_output(path: &Path, bytes: &[u8]) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&absolute, bytes)?;
    let lines = bytes.iter().filter(|b| **b == b'\n').count()
        + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'));
    println!("Wrote {} ({lines} lines)", absolute.display());
    Ok(())
}

pub(super) fn run(client: &ApiClient, args: DocCatArgs) -> Result<()> {
    let default_out = matches!(args.out, Some(None));
    let (bytes, default_path) = if let Some(path) = local_path(&args.doc) {
        if args.version.is_some() {
            bail!("--version needs a published doc");
        }
        let source = fs::read(&path)?;
        let name = format!("local/{}", path.file_name().unwrap().to_string_lossy());
        let cached = cache_path(&name, &git_blob_sha(&source), args.raw)?;
        let bytes = if default_out && cached.is_file() {
            fs::read(&cached)?
        } else {
            render(&args.doc, source, args.raw)?
        };
        (bytes, cached)
    } else {
        let mut metadata_path = doc_metadata_path(&args.doc)?;
        if let Some(version) = &args.version {
            metadata_path = metadata_path.split("&version=").next().unwrap().to_owned();
            metadata_path.push_str(&format!("&version={}", url_segment(version)));
        }
        // For an explicit revision with at least 12 hex characters, the deterministic cache can be used
        // without even resolving metadata. Latest and short prefixes must be
        // resolved first to avoid serving an older publication.
        let requested_version = metadata_path
            .split("&version=")
            .nth(1)
            .map(str::to_ascii_lowercase);
        let requested_version = requested_version.as_deref();
        let readable = metadata_path.split('?').next().unwrap();
        let name = readable.strip_prefix("/docs/").unwrap();
        if default_out {
            if let Some(sha) = requested_version.filter(|s| {
                s.len() >= 12 && s.len() <= 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
            }) {
                let cached = cache_path(name, sha, args.raw)?;
                if cached.is_file() {
                    return write_output(&cached, &fs::read(&cached)?);
                }
            }
        }
        let metadata = client.get_json(&metadata_path)?;
        let sha = if let Some(version) = requested_version {
            metadata["publishes"]
                .as_array()
                .and_then(|rows| {
                    rows.iter().find_map(|row| {
                        row["commit_sha"]
                            .as_str()
                            .filter(|sha| sha.starts_with(version))
                    })
                })
                .ok_or_else(|| anyhow!("Version not found"))?
        } else {
            metadata["latest_commit_sha"]
                .as_str()
                .ok_or_else(|| anyhow!("Doc has no published revision"))?
        };
        let cached = cache_path(name, sha, args.raw)?;
        let bytes = if default_out && cached.is_file() {
            fs::read(&cached)?
        } else {
            let path = format!(
                "{readable}?format={}&version={sha}",
                if args.raw { "raw" } else { "markdown" }
            );
            // Unlike the JSON client, read bytes: --raw must preserve non-UTF8.
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(client.timeout)
                .build()
                .into();
            let mut response = agent.get(&client.url_for(&path)).call()?;
            let status = response.status().as_u16();
            let bytes = response
                .body_mut()
                .with_config()
                .limit(100 * 1024 * 1024)
                .read_to_vec()?;
            if !(200..300).contains(&status) {
                bail!("HTTP {status}: {}", String::from_utf8_lossy(&bytes));
            }
            bytes
        };
        (bytes, cached)
    };
    match args.out {
        Some(path) => write_output(path.as_deref().unwrap_or(&default_path), &bytes),
        None => {
            io::stdout().lock().write_all(&bytes)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[test]
    fn paths_and_rendering() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert!(cache_path("repo/docs/a.html", sha, false)
            .unwrap()
            .ends_with("sm-doc/repo/docs/a.html@0123456789ab.md"));
        assert!(cache_path("repo/a.html", sha, true)
            .unwrap()
            .ends_with("a.html@0123456789ab.raw"));
        assert!(cache_path("../escape", sha, false).is_err());
        assert_eq!(
            render("a.md", b"# Unchanged\n".to_vec(), false).unwrap(),
            b"# Unchanged\n"
        );
        assert_eq!(render("a.html", vec![255], true).unwrap(), vec![255]);
        assert_eq!(
            render("a.html", b"<h1>Title</h1>".to_vec(), false).unwrap(),
            b"# Title"
        );
        assert_ne!(
            cache_path("local/a.html", &git_blob_sha(b"one"), false).unwrap(),
            cache_path("local/a.html", &git_blob_sha(b"two"), false).unwrap()
        );
        assert!(local_path("https://example.com/docs/repo/a.html").is_none());
    }
    #[test]
    fn local_file_options_and_immutable_cache() {
        let root = std::env::temp_dir().join(format!(
            "sm-cat-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("memo.html");
        fs::write(&path, "<h1>Memo</h1>").unwrap();
        let client = ApiClient::parse("http://127.0.0.1:1").unwrap();
        let args = |version, raw, out| DocCatArgs {
            doc: path.display().to_string(),
            version,
            raw,
            out,
        };
        assert!(local_path(path.to_str().unwrap()).is_some());
        assert!(run(&client, args(Some("abc".into()), false, None))
            .unwrap_err()
            .to_string()
            .contains("--version needs a published doc"));
        let out = root.join("nested/memo.md");
        run(&client, args(None, false, Some(Some(out.clone())))).unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "# Memo");
        run(&client, args(None, true, Some(Some(out.clone())))).unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "<h1>Memo</h1>");
        run(&client, args(None, false, Some(None))).unwrap();
        let cached = cache_path("local/memo.html", &git_blob_sha(b"<h1>Memo</h1>"), false).unwrap();
        assert_eq!(fs::read_to_string(&cached).unwrap(), "# Memo");
        let name = format!(
            "sm-cat-{}/memo.html",
            root.file_name().unwrap().to_string_lossy()
        );
        let sha = "a".repeat(40);
        let cached_doc = cache_path(&name, &sha, false).unwrap();
        fs::create_dir_all(cached_doc.parent().unwrap()).unwrap();
        fs::write(&cached_doc, "# Cached revision").unwrap();
        // No server is listening: success proves the exact-revision cache
        // does not make a metadata or content request.
        run(
            &client,
            DocCatArgs {
                doc: name,
                version: Some(sha),
                raw: false,
                out: Some(None),
            },
        )
        .unwrap();
        fs::remove_file(cached_doc).unwrap();
        fs::remove_file(cached).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
