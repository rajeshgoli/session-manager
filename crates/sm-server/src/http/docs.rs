//! Owner docs HTTP surface (sm#1447 / #1449): publish, list, read.

use super::*;
use crate::owner_docs::{
    default_doc_title, git_blob_sha, is_full_commit_sha, is_owner_doc_id, render_doc_page,
    validate_repo_path, validate_repo_slug, DocCache, OwnerDoc, OwnerDocStore, OwnerDocSummary,
    PublishOwnerDoc, DOC_CACHE_MAX_IDLE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocFetchError {
    /// The file does not exist at that commit (contents API 404).
    NotFound(String),
    Other(String),
}

impl DocFetchError {
    fn from_gh(detail: String) -> Self {
        if detail.contains("HTTP 404") || detail.contains("Not Found") {
            Self::NotFound(detail)
        } else {
            Self::Other(detail)
        }
    }
}

/// Where doc bytes come from. The live server uses `gh`; tests substitute a
/// fixture the same way `GitHubReviewPoster` is substituted.
pub trait OwnerDocSource: Send + Sync {
    /// Raw file bytes at a commit.
    fn fetch_doc(&self, repo: &str, path: &str, commit_sha: &str)
        -> Result<Vec<u8>, DocFetchError>;

    /// File bytes plus the contents API's blob `sha`, used once at publish to
    /// check the locally computed blob SHA.
    fn fetch_doc_with_blob_sha(
        &self,
        repo: &str,
        path: &str,
        commit_sha: &str,
    ) -> Result<(Vec<u8>, String), DocFetchError>;
}

#[derive(Debug)]
pub(super) struct GhCliDocSource;

fn contents_endpoint(repo: &str, path: &str, commit_sha: &str) -> Result<String, DocFetchError> {
    let (owner, name) = split_github_repo(repo).map_err(DocFetchError::Other)?;
    let encoded_path = path
        .split('/')
        .map(percent_encode_path)
        .collect::<Vec<_>>()
        .join("/");
    Ok(format!(
        "repos/{owner}/{name}/contents/{encoded_path}?ref={commit_sha}"
    ))
}

fn gh_api_bytes(args: Vec<String>) -> Result<Vec<u8>, DocFetchError> {
    let output = gh_command_output(&args, Duration::from_secs(30))
        .map_err(|error| DocFetchError::Other(format!("gh api failed: {error}")))?;
    if !output.status.success() {
        return Err(DocFetchError::from_gh(format!(
            "gh api failed: {}",
            command_stderr(&output)
        )));
    }
    Ok(output.stdout)
}

impl OwnerDocSource for GhCliDocSource {
    fn fetch_doc(
        &self,
        repo: &str,
        path: &str,
        commit_sha: &str,
    ) -> Result<Vec<u8>, DocFetchError> {
        gh_api_bytes(vec![
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github.raw".to_owned(),
            contents_endpoint(repo, path, commit_sha)?,
        ])
    }

    fn fetch_doc_with_blob_sha(
        &self,
        repo: &str,
        path: &str,
        commit_sha: &str,
    ) -> Result<(Vec<u8>, String), DocFetchError> {
        let raw = gh_api_bytes(vec![
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github+json".to_owned(),
            contents_endpoint(repo, path, commit_sha)?,
        ])?;
        let payload: Value = serde_json::from_slice(&raw).map_err(|error| {
            DocFetchError::Other(format!("contents API returned invalid JSON: {error}"))
        })?;
        if payload.is_array() || payload["type"].as_str().is_some_and(|kind| kind != "file") {
            return Err(DocFetchError::NotFound(format!("{path} is not a file")));
        }
        let blob_sha = payload["sha"]
            .as_str()
            .ok_or_else(|| DocFetchError::Other("contents API returned no sha".to_owned()))?
            .to_owned();
        // Files over 1 MB come back with `encoding: none` and no content.
        let bytes = match payload["encoding"].as_str() {
            Some("base64") => STANDARD
                .decode(
                    payload["content"]
                        .as_str()
                        .unwrap_or("")
                        .replace(['\n', '\r'], ""),
                )
                .map_err(|error| {
                    DocFetchError::Other(format!("contents API returned bad base64: {error}"))
                })?,
            _ => self.fetch_doc(repo, path, commit_sha)?,
        };
        Ok((bytes, blob_sha))
    }
}

fn owner_doc_store(state: &AppState) -> OwnerDocStore {
    OwnerDocStore::new(expand_home(&state.config.sm_send.db_path))
}

/// `<state dir>/doc_cache`, beside the retained queue DB.
fn owner_doc_cache(config: &AppConfig) -> DocCache {
    let db_path = expand_home(&config.sm_send.db_path);
    let state_dir = db_path
        .parent()
        .map(StdPath::to_path_buf)
        .unwrap_or_default();
    DocCache::new(state_dir.join("doc_cache"))
}

/// Schema setup plus a once-per-process daily cache prune.
pub(super) fn init_owner_docs(config: &AppConfig) {
    let db_path = expand_home(&config.sm_send.db_path);
    if db_path.exists() {
        if let Err(error) = OwnerDocStore::new(db_path).ensure_schema() {
            eprintln!("Owner doc schema initialization failed: {error:#}");
        }
    }
    static PRUNER: std::sync::Once = std::sync::Once::new();
    let cache = owner_doc_cache(config);
    PRUNER.call_once(move || {
        let _ = thread::Builder::new()
            .name("sm-doc-cache-prune".to_owned())
            .spawn(move || loop {
                if let Err(error) = cache.prune(DOC_CACHE_MAX_IDLE) {
                    eprintln!("Owner doc cache prune failed: {error:#}");
                }
                thread::sleep(Duration::from_secs(24 * 60 * 60));
            });
    });
}

fn load_doc_bytes(
    source: &dyn OwnerDocSource,
    cache: &DocCache,
    doc: &OwnerDoc,
    commit_sha: &str,
) -> Result<Vec<u8>, DocFetchError> {
    if let Some(bytes) = cache.get(&doc.repo, commit_sha, &doc.path) {
        return Ok(bytes);
    }
    let bytes = source.fetch_doc(&doc.repo, &doc.path, commit_sha)?;
    if let Err(error) = cache.put(&doc.repo, commit_sha, &doc.path, &bytes) {
        eprintln!("Owner doc cache write failed: {error:#}");
    }
    Ok(bytes)
}

fn doc_fetch_api_error(error: DocFetchError, doc_path: &str, commit_sha: &str) -> ApiError {
    match error {
        DocFetchError::NotFound(_) => ApiError::Status {
            status: StatusCode::NOT_FOUND,
            detail: format!(
                "{doc_path} does not exist at {}",
                &commit_sha[..commit_sha.len().min(7)]
            ),
        },
        DocFetchError::Other(detail) => ApiError::Status {
            status: StatusCode::BAD_GATEWAY,
            detail,
        },
    }
}

pub(super) fn doc_reader_path(doc_id: &str) -> String {
    format!("/docs/{doc_id}")
}

/// Absolute reader URL as the caller reached this server. Clients that know
/// their own API base should prefer `reader_path`.
fn doc_reader_url(headers: &HeaderMap, doc_id: &str) -> String {
    let path = doc_reader_path(doc_id);
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let Some(host) = header(HOST.as_str()) else {
        return path;
    };
    let scheme = header("x-forwarded-proto").unwrap_or("http");
    format!("{scheme}://{host}{path}")
}

/// `https://<browser host>` when `cloudflare_access.browser` is enabled with a
/// hostname: the base the owner opens doc links under from any browser.
pub(super) fn doc_browser_base_url(config: &AppConfig) -> Option<String> {
    let browser = &config.cloudflare_access.browser;
    if !browser.enabled {
        return None;
    }
    let host = browser
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())?;
    Some(format!("https://{}", host.trim_end_matches('/')))
}

fn summary_json(
    config: &AppConfig,
    summary: &OwnerDocSummary,
    headers: &HeaderMap,
) -> Result<Value, ApiError> {
    let mut value = serde_json::to_value(summary)?;
    value["reader_path"] = json!(doc_reader_path(&summary.doc.id));
    value["reader_url"] = json!(doc_reader_url(headers, &summary.doc.id));
    if let Some(base) = doc_browser_base_url(config) {
        value["browser_url"] = json!(format!("{base}{}", doc_reader_path(&summary.doc.id)));
    }
    Ok(value)
}

fn find_doc(state: &AppState, doc_id: &str) -> Result<OwnerDoc, ApiError> {
    if !is_owner_doc_id(doc_id) {
        return Err(ApiError::NotFound("Doc not found"));
    }
    owner_doc_store(state)
        .get(doc_id)?
        .ok_or(ApiError::NotFound("Doc not found"))
}

fn bad_request(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct PublishOwnerDocRequest {
    repo: String,
    path: String,
    #[serde(default)]
    pr_number: Option<i64>,
    commit_sha: String,
    session_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    review: bool,
}

pub(super) async fn publish_owner_doc(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<PublishOwnerDocRequest>,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer_addr), "/docs")?;
    ensure_core_writes_enabled(&state)?;
    if payload.review {
        return Err(bad_request(
            "Review requests are not supported yet (sm#1451); publish without --review",
        ));
    }
    let repo = payload.repo.trim().to_owned();
    let path = payload.path.trim().to_owned();
    let commit_sha = payload.commit_sha.trim().to_ascii_lowercase();
    validate_repo_slug(&repo).map_err(|error| bad_request(error.to_string()))?;
    validate_repo_path(&path).map_err(|error| bad_request(error.to_string()))?;
    if !is_full_commit_sha(&commit_sha) {
        return Err(bad_request("commit_sha must be a full 40-character SHA"));
    }
    if payload.pr_number.is_some_and(|pr| pr <= 0) {
        return Err(bad_request("pr_number must be positive"));
    }
    let session_id = payload.session_id.trim();
    let Some(author) = state.session_store.get_session(session_id)? else {
        return Err(bad_request(format!("Session {session_id} not found")));
    };

    let source = state.owner_doc_source.clone();
    let (fetch_repo, fetch_path, fetch_sha) = (repo.clone(), path.clone(), commit_sha.clone());
    let fetched = tokio::task::spawn_blocking(move || {
        source.fetch_doc_with_blob_sha(&fetch_repo, &fetch_path, &fetch_sha)
    })
    .await
    .map_err(|error| anyhow::anyhow!("doc fetch task failed: {error}"))?;
    let (bytes, api_blob_sha) =
        fetched.map_err(|error| doc_fetch_api_error(error, &path, &commit_sha))?;
    let blob_sha = git_blob_sha(&bytes);
    if blob_sha != api_blob_sha {
        return Err(ApiError::Status {
            status: StatusCode::BAD_GATEWAY,
            detail: format!(
                "Blob SHA mismatch for {path}: computed {blob_sha}, contents API {api_blob_sha}"
            ),
        });
    }
    if let Err(error) = owner_doc_cache(&state.config).put(&repo, &commit_sha, &path, &bytes) {
        eprintln!("Owner doc cache write failed: {error:#}");
    }
    let title = payload
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_doc_title(&path, &bytes));
    let store = owner_doc_store(&state);
    let published = store.publish(PublishOwnerDoc {
        repo,
        path,
        pr_number: payload.pr_number,
        session_id: author.id.clone(),
        session_name: author.friendly_name.clone().or(Some(author.name.clone())),
        title,
        note: payload
            .note
            .as_deref()
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(ToOwned::to_owned),
        commit_sha,
        blob_sha,
        review_requested: false,
    })?;
    let summary = store
        .summary(&published.doc.id)?
        .ok_or(ApiError::NotFound("Doc not found"))?;
    let mut response = summary_json(&state.config, &summary, &headers)?;
    response["created"] = json!(published.created);
    response["publish"] = serde_json::to_value(&published.publish)?;
    Ok(Json(response))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ListOwnerDocsQuery {
    #[serde(default)]
    session: Option<String>,
    /// Include the session's descendants.
    #[serde(default)]
    tree: bool,
    #[serde(default)]
    include_retracted: bool,
}

pub(super) async fn list_owner_docs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListOwnerDocsQuery>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_owner_doc_read_allowed(&state, &request)?;
    let authors = match query
        .session
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(identifier) => {
            let mut authors = BTreeSet::new();
            match resolve_session_or_registry_role(&state, identifier)? {
                Some(session) => {
                    if query.tree {
                        for child in
                            state
                                .session_store
                                .list_child_records(&session.id, true, None, true)?
                        {
                            authors.insert(child.id);
                        }
                    }
                    authors.insert(session.id);
                }
                // A retired and purged author can still own docs.
                None => {
                    authors.insert(identifier.to_owned());
                }
            }
            Some(authors)
        }
        None => None,
    };
    let docs = owner_doc_store(&state)
        .summaries(authors.as_ref(), query.include_retracted)?
        .iter()
        .map(|summary| summary_json(&state.config, summary, request.headers()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({ "docs": docs })))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct GetOwnerDocQuery {
    #[serde(default)]
    format: Option<String>,
}

pub(super) async fn get_owner_doc(
    State(state): State<Arc<AppState>>,
    Path(doc_id): Path<String>,
    Query(query): Query<GetOwnerDocQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_doc_read_allowed(&state, &request)?;
    let doc = find_doc(&state, &doc_id)?;
    let store = owner_doc_store(&state);
    let summary = store
        .summary(&doc.id)?
        .ok_or(ApiError::NotFound("Doc not found"))?;
    if query.format.as_deref() == Some("json") {
        let mut value = summary_json(&state.config, &summary, request.headers())?;
        value["publishes"] = serde_json::to_value(store.publishes(&doc.id)?)?;
        return Ok(Json(value).into_response());
    }
    // Default to the latest *published* revision, the one the agent
    // announced and the one the state chip describes.
    Ok((
        StatusCode::FOUND,
        [
            (
                LOCATION,
                format!(
                    "{}/view?sha={}",
                    doc_reader_path(&doc.id),
                    summary.latest_commit_sha
                ),
            ),
            (CACHE_CONTROL, "no-cache".to_owned()),
        ],
    )
        .into_response())
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct DocShaQuery {
    #[serde(default)]
    sha: Option<String>,
}

/// The file bytes for `?sha=`, defaulting to the latest publish.
async fn doc_bytes_for_request(
    state: &Arc<AppState>,
    doc_id: &str,
    sha: Option<&str>,
) -> Result<(OwnerDoc, String, Vec<u8>), ApiError> {
    let doc = find_doc(state, doc_id)?;
    let commit_sha = match sha.map(str::trim).filter(|sha| !sha.is_empty()) {
        Some(sha) => {
            let sha = sha.to_ascii_lowercase();
            if !is_full_commit_sha(&sha) {
                return Err(bad_request("sha must be a full 40-character commit SHA"));
            }
            sha
        }
        None => owner_doc_store(state)
            .publishes(&doc.id)?
            .last()
            .map(|publish| publish.commit_sha.clone())
            .ok_or(ApiError::NotFound("Doc has no published revision"))?,
    };
    let source = state.owner_doc_source.clone();
    let cache = owner_doc_cache(&state.config);
    let (fetch_doc, fetch_sha) = (doc.clone(), commit_sha.clone());
    let bytes = tokio::task::spawn_blocking(move || {
        load_doc_bytes(source.as_ref(), &cache, &fetch_doc, &fetch_sha)
    })
    .await
    .map_err(|error| anyhow::anyhow!("doc fetch task failed: {error}"))?
    .map_err(|error| doc_fetch_api_error(error, &doc.path, &commit_sha))?;
    Ok((doc, commit_sha, bytes))
}

pub(super) async fn view_owner_doc(
    State(state): State<Arc<AppState>>,
    Path(doc_id): Path<String>,
    Query(query): Query<DocShaQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_doc_read_allowed(&state, &request)?;
    let (doc, _, bytes) = doc_bytes_for_request(&state, &doc_id, query.sha.as_deref()).await?;
    if let Err(error) = owner_doc_store(&state).record_view(&doc.id, &git_blob_sha(&bytes)) {
        eprintln!("Owner doc view record failed: {error:#}");
    }
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
            (CACHE_CONTROL, "private, no-cache".to_owned()),
        ],
        Body::from(render_doc_page(&doc.path, &doc.title, &bytes)),
    )
        .into_response())
}

pub(super) async fn raw_owner_doc(
    State(state): State<Arc<AppState>>,
    Path(doc_id): Path<String>,
    Query(query): Query<DocShaQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_doc_read_allowed(&state, &request)?;
    let (doc, _, bytes) = doc_bytes_for_request(&state, &doc_id, query.sha.as_deref()).await?;
    let file_name = doc
        .path
        .rsplit('/')
        .next()
        .unwrap_or("doc")
        .replace(['"', '\\'], "_");
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "text/plain; charset=utf-8".to_owned()),
            (CACHE_CONTROL, "private, no-cache".to_owned()),
            (
                CONTENT_DISPOSITION,
                format!("inline; filename=\"{file_name}\""),
            ),
            (
                axum::http::header::X_CONTENT_TYPE_OPTIONS,
                "nosniff".to_owned(),
            ),
        ],
        Body::from(bytes),
    )
        .into_response())
}

pub(super) async fn retract_owner_doc(
    State(state): State<Arc<AppState>>,
    Path(doc_id): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    ensure_session_allowed_from_parts(
        &state.config,
        &headers,
        Some(peer_addr),
        &format!("/docs/{doc_id}/retract"),
    )?;
    ensure_core_writes_enabled(&state)?;
    find_doc(&state, &doc_id)?;
    let store = owner_doc_store(&state);
    store.retract(&doc_id)?;
    let summary = store
        .summary(&doc_id)?
        .ok_or(ApiError::NotFound("Doc not found"))?;
    Ok(Json(summary_json(&state.config, &summary, &headers)?))
}

/// Docs for the obligations projection: stored data only, never `gh`.
pub(super) fn obligation_doc_summaries(state: &AppState) -> Result<Vec<OwnerDocSummary>, ApiError> {
    Ok(owner_doc_store(state).summaries(None, false)?)
}

pub(super) fn obligation_doc_entry(summary: &OwnerDocSummary, browser_base: Option<&str>) -> Value {
    let mut entry = json!({
        "id": summary.doc.id,
        "title": summary.doc.title,
        "state": summary.state,
        "repo": summary.doc.repo,
        "path": summary.doc.path,
        "pr_number": summary.doc.pr_number,
        "latest_commit_sha": summary.latest_commit_sha,
        "published_at": summary.published_at,
        "reader_path": doc_reader_path(&summary.doc.id),
    });
    if let Some(base) = browser_base {
        entry["browser_url"] = json!(format!("{base}{}", doc_reader_path(&summary.doc.id)));
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contents_endpoint_encodes_path_segments() {
        assert_eq!(
            contents_endpoint("acme/widgets", "docs/my memo#1.md", &"a".repeat(40)).unwrap(),
            format!(
                "repos/acme/widgets/contents/docs/my%20memo%231.md?ref={}",
                "a".repeat(40)
            )
        );
    }

    #[test]
    fn gh_404_maps_to_not_found() {
        assert!(matches!(
            DocFetchError::from_gh("gh: Not Found (HTTP 404)".into()),
            DocFetchError::NotFound(_)
        ));
        assert!(matches!(
            DocFetchError::from_gh("HTTP 502".into()),
            DocFetchError::Other(_)
        ));
    }

    #[test]
    fn browser_url_needs_an_enabled_browser_hostname() {
        let mut config = AppConfig::default();
        config.cloudflare_access.browser.hostname = Some("sm.example.com".to_owned());
        assert_eq!(doc_browser_base_url(&config), None);
        config.cloudflare_access.browser.enabled = true;
        assert_eq!(
            doc_browser_base_url(&config).as_deref(),
            Some("https://sm.example.com")
        );
        config.cloudflare_access.browser.hostname = Some("  ".to_owned());
        assert_eq!(doc_browser_base_url(&config), None);
    }

    #[test]
    fn reader_url_uses_the_callers_host() {
        let mut headers = HeaderMap::new();
        assert_eq!(doc_reader_url(&headers, "abcd1234"), "/docs/abcd1234");
        headers.insert(HOST, "127.0.0.1:8420".parse().unwrap());
        assert_eq!(
            doc_reader_url(&headers, "abcd1234"),
            "http://127.0.0.1:8420/docs/abcd1234"
        );
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(
            doc_reader_url(&headers, "abcd1234"),
            "https://127.0.0.1:8420/docs/abcd1234"
        );
    }
}
