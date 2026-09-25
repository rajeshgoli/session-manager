//! Owner docs HTTP surface (sm#1447 / #1449): publish, list, read.

use super::*;
use crate::owner_docs::{
    default_doc_title, doc_name, doc_readable_path, git_blob_sha, is_doc_version,
    is_full_commit_sha, is_owner_doc_id, render_doc_page, validate_repo_path, validate_repo_slug,
    DocCache, OwnerDoc, OwnerDocPublish, OwnerDocStore, OwnerDocSummary, PublishOwnerDoc,
    ReadableDocError, DOC_CACHE_MAX_IDLE,
};

mod review;
pub(super) use review::recover_owner_doc_reviews;

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

    /// The PR's node id, state and head.
    fn pull_request(&self, _repo: &str, _pr_number: i64) -> Result<DocPullRequest, String> {
        Err("this doc source cannot read pull requests".to_owned())
    }

    /// GraphQL `addPullRequestReview` with no event: a pending review pinned
    /// to `commit_sha`. Returns the review's node id.
    fn add_pending_review(
        &self,
        _pr_node_id: &str,
        _commit_sha: &str,
        _body: &str,
    ) -> Result<String, String> {
        Err("this doc source cannot post reviews".to_owned())
    }

    /// GraphQL `addPullRequestReviewThread`: a `LINE` thread on the RIGHT
    /// side when `line` is set, otherwise a `FILE` thread. `Ok(false)` is
    /// GitHub returning a `null` thread without an error, which is a failure.
    fn add_review_thread(
        &self,
        _review_node_id: &str,
        _path: &str,
        _line: Option<i64>,
        _body: &str,
    ) -> Result<bool, String> {
        Err("this doc source cannot post reviews".to_owned())
    }

    /// GraphQL `submitPullRequestReview` with event `COMMENT`.
    fn submit_pending_review(
        &self,
        _review_node_id: &str,
        _body: &str,
    ) -> Result<SubmittedDocReview, String> {
        Err("this doc source cannot post reviews".to_owned())
    }

    /// GraphQL `deletePullRequestReview` (pending reviews only).
    fn delete_pending_review(&self, _review_node_id: &str) -> Result<(), String> {
        Err("this doc source cannot post reviews".to_owned())
    }

    /// The viewer's reviews on the PR, `PENDING` included.
    fn viewer_reviews(
        &self,
        _repo: &str,
        _pr_number: i64,
    ) -> Result<Vec<DocReviewOnGitHub>, String> {
        Err("this doc source cannot read reviews".to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocPullRequest {
    pub node_id: String,
    /// `open`, `closed` or `merged`.
    pub state: String,
    pub head_sha: String,
    pub url: String,
}

impl DocPullRequest {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedDocReview {
    pub database_id: Option<i64>,
    pub url: String,
}

/// One of the viewer's reviews, as reconciliation needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocReviewOnGitHub {
    pub node_id: String,
    pub database_id: Option<i64>,
    pub url: String,
    /// GraphQL review state: `PENDING`, `COMMENTED`, ...
    pub state: String,
    pub body: String,
    /// Each comment's body and whether it is anchored to a line.
    pub comments: Vec<(String, bool)>,
}

#[derive(Debug)]
pub(super) struct GhCliDocSource;

/// `gh api graphql` with the request body in a temp file, so variables keep
/// their JSON types. Mutations are not retried: a retry after a lost
/// response could post a second thread.
fn gh_graphql(query: &str, variables: Value, retry: bool) -> Result<Value, String> {
    let mut nonce = [0u8; 8];
    OsRng.fill_bytes(&mut nonce);
    let input = std::env::temp_dir().join(format!(
        "sm-doc-graphql-{}-{}.json",
        std::process::id(),
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    fs::write(
        &input,
        serde_json::to_vec(&json!({"query": query, "variables": variables}))
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("failed to write GraphQL request: {error}"))?;
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "--input".to_owned(),
        input.display().to_string(),
    ];
    let output = if retry {
        gh_command_output(&args, Duration::from_secs(30))
    } else {
        let mut command = Command::new("gh");
        command.args(&args);
        command_output_with_timeout(command, Duration::from_secs(30))
    };
    let _ = fs::remove_file(&input);
    let output = output.map_err(|error| format!("gh api graphql failed: {error}"))?;
    let payload: Option<Value> = serde_json::from_slice(&output.stdout).ok();
    if let Some(errors) = payload
        .as_ref()
        .and_then(|payload| payload["errors"].as_array())
        .filter(|errors| !errors.is_empty())
    {
        return Err(errors
            .iter()
            .map(|error| {
                error["message"]
                    .as_str()
                    .unwrap_or("GraphQL error")
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("; "));
    }
    match payload {
        Some(payload) if output.status.success() => Ok(payload["data"].clone()),
        _ => Err(format!(
            "gh api graphql failed: {}",
            command_stderr(&output)
        )),
    }
}

const ADD_PENDING_REVIEW: &str = "mutation($pr: ID!, $commit: GitObjectID!, $body: String!) {
  addPullRequestReview(input: {pullRequestId: $pr, commitOID: $commit, body: $body}) {
    pullRequestReview { id }
  }
}";
const ADD_LINE_THREAD: &str =
    "mutation($review: ID!, $path: String!, $line: Int!, $body: String!) {
  addPullRequestReviewThread(input: {pullRequestReviewId: $review, path: $path, line: $line,
      side: RIGHT, subjectType: LINE, body: $body}) { thread { id } }
}";
const ADD_FILE_THREAD: &str = "mutation($review: ID!, $path: String!, $body: String!) {
  addPullRequestReviewThread(input: {pullRequestReviewId: $review, path: $path,
      subjectType: FILE, body: $body}) { thread { id } }
}";
const SUBMIT_REVIEW: &str = "mutation($review: ID!, $body: String!) {
  submitPullRequestReview(input: {pullRequestReviewId: $review, event: COMMENT, body: $body}) {
    pullRequestReview { databaseId url }
  }
}";
const DELETE_REVIEW: &str = "mutation($review: ID!) {
  deletePullRequestReview(input: {pullRequestReviewId: $review}) { clientMutationId }
}";
const VIEWER_REVIEWS: &str = "query($owner: String!, $name: String!, $number: Int!) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviews(last: 100) {
        nodes { id databaseId url state body author { login }
          comments(first: 100) { nodes { body line originalLine } } }
      }
    }
  }
}";

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

    fn pull_request(&self, repo: &str, pr_number: i64) -> Result<DocPullRequest, String> {
        let args = vec![
            "pr".to_owned(),
            "view".to_owned(),
            pr_number.to_string(),
            "--repo".to_owned(),
            repo.to_owned(),
            "--json".to_owned(),
            "id,state,headRefOid,url".to_owned(),
        ];
        let output = gh_command_output(&args, Duration::from_secs(15))
            .map_err(|error| format!("gh pr view failed: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "PR #{pr_number} not found in {repo}: {}",
                command_stderr(&output)
            ));
        }
        let payload: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("gh pr view returned invalid JSON: {error}"))?;
        let text = |key: &str| payload[key].as_str().unwrap_or_default().to_owned();
        Ok(DocPullRequest {
            node_id: text("id"),
            state: text("state").to_ascii_lowercase(),
            head_sha: text("headRefOid").to_ascii_lowercase(),
            url: text("url"),
        })
    }

    fn add_pending_review(
        &self,
        pr_node_id: &str,
        commit_sha: &str,
        body: &str,
    ) -> Result<String, String> {
        let data = gh_graphql(
            ADD_PENDING_REVIEW,
            json!({"pr": pr_node_id, "commit": commit_sha, "body": body}),
            false,
        )?;
        data["addPullRequestReview"]["pullRequestReview"]["id"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| "GitHub created no pending review".to_owned())
    }

    fn add_review_thread(
        &self,
        review_node_id: &str,
        path: &str,
        line: Option<i64>,
        body: &str,
    ) -> Result<bool, String> {
        let data = match line {
            Some(line) => gh_graphql(
                ADD_LINE_THREAD,
                json!({"review": review_node_id, "path": path, "line": line, "body": body}),
                false,
            )?,
            None => gh_graphql(
                ADD_FILE_THREAD,
                json!({"review": review_node_id, "path": path, "body": body}),
                false,
            )?,
        };
        Ok(data["addPullRequestReviewThread"]["thread"]["id"].is_string())
    }

    fn submit_pending_review(
        &self,
        review_node_id: &str,
        body: &str,
    ) -> Result<SubmittedDocReview, String> {
        let data = gh_graphql(
            SUBMIT_REVIEW,
            json!({"review": review_node_id, "body": body}),
            false,
        )?;
        let review = &data["submitPullRequestReview"]["pullRequestReview"];
        Ok(SubmittedDocReview {
            database_id: review["databaseId"].as_i64(),
            url: review["url"]
                .as_str()
                .ok_or_else(|| "GitHub returned no submitted review".to_owned())?
                .to_owned(),
        })
    }

    fn delete_pending_review(&self, review_node_id: &str) -> Result<(), String> {
        gh_graphql(DELETE_REVIEW, json!({"review": review_node_id}), false).map(|_| ())
    }

    fn viewer_reviews(&self, repo: &str, pr_number: i64) -> Result<Vec<DocReviewOnGitHub>, String> {
        let (owner, name) = split_github_repo(repo)?;
        let data = gh_graphql(
            VIEWER_REVIEWS,
            json!({"owner": owner, "name": name, "number": pr_number}),
            true,
        )?;
        let viewer = data["viewer"]["login"].as_str().unwrap_or_default();
        let nodes = data["repository"]["pullRequest"]["reviews"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(nodes
            .iter()
            .filter(|node| node["author"]["login"].as_str() == Some(viewer))
            .map(|node| DocReviewOnGitHub {
                node_id: node["id"].as_str().unwrap_or_default().to_owned(),
                database_id: node["databaseId"].as_i64(),
                url: node["url"].as_str().unwrap_or_default().to_owned(),
                state: node["state"].as_str().unwrap_or_default().to_owned(),
                body: node["body"].as_str().unwrap_or_default().to_owned(),
                comments: node["comments"]["nodes"]
                    .as_array()
                    .map(|comments| {
                        comments
                            .iter()
                            .map(|comment| {
                                (
                                    comment["body"].as_str().unwrap_or_default().to_owned(),
                                    !comment["line"].is_null()
                                        || !comment["originalLine"].is_null(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect())
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

/// The readable reader path for a doc's latest publish. Every doc URL a
/// person or agent sees uses this form; `/docs/{id}` is internal API only.
pub(super) fn doc_reader_path(summary: &OwnerDocSummary) -> String {
    doc_readable_path(
        &summary.doc.repo,
        &summary.doc.path,
        &summary.latest_commit_sha,
    )
}

/// Absolute reader URL as the caller reached this server. Clients that know
/// their own API base should prefer `reader_path`.
fn doc_reader_url(headers: &HeaderMap, path: String) -> String {
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
    let reader_path = doc_reader_path(summary);
    value["name"] = json!(doc_name(&summary.doc.repo, &summary.doc.path));
    value["reader_url"] = json!(doc_reader_url(headers, reader_path.clone()));
    if let Some(base) = doc_browser_base_url(config) {
        value["browser_url"] = json!(format!("{base}{reader_path}"));
    }
    value["reader_path"] = json!(reader_path);
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
    if payload.review {
        // A review lands as a GitHub PR review, so it needs an open PR.
        let Some(pr_number) = payload.pr_number else {
            return Err(bad_request("--review needs a PR: publish with --pr <N>"));
        };
        let (lookup_state, lookup_repo) = (state.clone(), repo.clone());
        let pr = tokio::task::spawn_blocking(move || {
            doc_pull_request(&lookup_state, &lookup_repo, pr_number, true)
        })
        .await
        .map_err(|error| anyhow::anyhow!("PR lookup task failed: {error}"))?
        .map_err(|error| ApiError::Status {
            status: StatusCode::BAD_GATEWAY,
            detail: error,
        })?;
        if !pr.is_open() {
            return Err(ApiError::Status {
                status: StatusCode::CONFLICT,
                detail: format!("--review needs an open PR; PR #{pr_number} is {}", pr.state),
            });
        }
    }

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
        review_requested: payload.review,
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
    if query.format.as_deref() == Some("json") {
        return doc_metadata_response(&state, &doc, request.headers());
    }
    // Default to the latest *published* revision, the one the agent
    // announced and the one the state chip describes. The address bar only
    // ever shows the readable form.
    let summary = owner_doc_store(&state)
        .summary(&doc.id)?
        .ok_or(ApiError::NotFound("Doc not found"))?;
    Ok((
        StatusCode::FOUND,
        [
            (LOCATION, doc_reader_path(&summary)),
            (CACHE_CONTROL, "no-cache".to_owned()),
        ],
    )
        .into_response())
}

/// `?format=json`: the summary plus every publish.
fn doc_metadata_response(
    state: &AppState,
    doc: &OwnerDoc,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let store = owner_doc_store(state);
    let summary = store
        .summary(&doc.id)?
        .ok_or(ApiError::NotFound("Doc not found"))?;
    let mut value = summary_json(&state.config, &summary, headers)?;
    value["publishes"] = serde_json::to_value(store.publishes(&doc.id)?)?;
    value["reviews"] = serde_json::to_value(store.reviews(&doc.id)?)?;
    Ok(Json(value).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct DocSubpathQuery {
    /// Id routes: a full commit SHA.
    #[serde(default)]
    sha: Option<String>,
    /// Readable routes: a commit SHA prefix matching one of the doc's publishes.
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

/// The commit an id route renders: `?sha=`, defaulting to the latest publish.
fn id_route_commit(
    state: &AppState,
    doc: &OwnerDoc,
    sha: Option<&str>,
) -> Result<String, ApiError> {
    match sha.map(str::trim).filter(|sha| !sha.is_empty()) {
        Some(sha) => {
            let sha = sha.to_ascii_lowercase();
            if !is_full_commit_sha(&sha) {
                return Err(bad_request("sha must be a full 40-character commit SHA"));
            }
            Ok(sha)
        }
        None => owner_doc_store(state)
            .publishes(&doc.id)?
            .last()
            .map(|publish| publish.commit_sha.clone())
            .ok_or(ApiError::NotFound("Doc has no published revision")),
    }
}

async fn load_doc_bytes_async(
    state: &Arc<AppState>,
    doc: &OwnerDoc,
    commit_sha: &str,
) -> Result<Vec<u8>, ApiError> {
    let source = state.owner_doc_source.clone();
    let cache = owner_doc_cache(&state.config);
    let (fetch_doc, fetch_sha) = (doc.clone(), commit_sha.to_owned());
    tokio::task::spawn_blocking(move || {
        load_doc_bytes(source.as_ref(), &cache, &fetch_doc, &fetch_sha)
    })
    .await
    .map_err(|error| anyhow::anyhow!("doc fetch task failed: {error}"))?
    .map_err(|error| doc_fetch_api_error(error, &doc.path, commit_sha))
}

/// PR state and head are cached for 30s; `fresh` skips the cache (submit).
const DOC_PR_CACHE_TTL: Duration = Duration::from_secs(30);

pub(super) type DocPullRequestCache = BTreeMap<(String, i64), (Instant, DocPullRequest)>;

pub(super) fn doc_pull_request(
    state: &AppState,
    repo: &str,
    pr_number: i64,
    fresh: bool,
) -> Result<DocPullRequest, String> {
    let key = (repo.to_owned(), pr_number);
    if !fresh {
        if let Some((at, pr)) = state
            .owner_doc_pr_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
        {
            if at.elapsed() < DOC_PR_CACHE_TTL {
                return Ok(pr.clone());
            }
        }
    }
    let pr = state.owner_doc_source.pull_request(repo, pr_number)?;
    state
        .owner_doc_pr_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, (Instant::now(), pr.clone()));
    Ok(pr)
}

async fn doc_pull_request_async(
    state: &Arc<AppState>,
    doc: &OwnerDoc,
    fresh: bool,
) -> Option<Result<DocPullRequest, String>> {
    let pr_number = doc.pr_number?;
    let (state, repo) = (state.clone(), doc.repo.clone());
    Some(
        tokio::task::spawn_blocking(move || doc_pull_request(&state, &repo, pr_number, fresh))
            .await
            .unwrap_or_else(|error| Err(format!("PR lookup task failed: {error}"))),
    )
}

/// The doc token lets the reader page's own `fetch` calls reach the doc's
/// JSON endpoints where device auth headers don't accompany script
/// requests (the Android WebView). It covers one doc at every revision for
/// 24 hours, and is signed with the session-cookie secret.
const DOC_TOKEN_TTL_SECONDS: i64 = 24 * 60 * 60;
pub(super) const DOC_TOKEN_HEADER: &str = "x-sm-doc-token";

fn doc_token_signature(secret: &str, doc_id: &str, expires_at: i64) -> Option<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(format!("{doc_id}|{expires_at}").as_bytes());
    Some(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn issue_doc_token_at(config: &AppConfig, doc_id: &str, now: i64) -> Option<String> {
    let secret = trimmed(&config.google_auth.session_cookie_secret)?;
    let expires_at = now + DOC_TOKEN_TTL_SECONDS;
    let signature = doc_token_signature(&secret, doc_id, expires_at)?;
    Some(format!("smdt_{doc_id}.{expires_at}.{signature}"))
}

pub(super) fn issue_doc_token(config: &AppConfig, doc_id: &str) -> Option<String> {
    issue_doc_token_at(config, doc_id, OffsetDateTime::now_utc().unix_timestamp())
}

pub(super) fn doc_token_valid(config: &AppConfig, doc_id: &str, token: &str) -> bool {
    let Some(secret) = trimmed(&config.google_auth.session_cookie_secret) else {
        return false;
    };
    let Some((token_doc, rest)) = token
        .trim()
        .strip_prefix("smdt_")
        .and_then(|t| t.split_once('.'))
    else {
        return false;
    };
    let Some((expires_at, signature)) = rest.split_once('.') else {
        return false;
    };
    let Ok(expires_at) = expires_at.parse::<i64>() else {
        return false;
    };
    let Ok(signature) = URL_SAFE_NO_PAD.decode(signature) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(format!("{token_doc}|{expires_at}").as_bytes());
    token_doc == doc_id
        && mac.verify_slice(&signature).is_ok()
        && expires_at > OffsetDateTime::now_utc().unix_timestamp()
}

/// The doc's JSON endpoints (`/head`, `/drafts`, `/review`) accept its doc
/// token in place of normal auth; everything else about auth is unchanged.
fn doc_token_presented(state: &AppState, headers: &HeaderMap, doc_id: &str) -> bool {
    header_text(headers, DOC_TOKEN_HEADER)
        .is_some_and(|token| doc_token_valid(&state.config, doc_id, &token))
}

fn revision_entries(doc: &OwnerDoc, publishes: &[OwnerDocPublish]) -> Vec<Value> {
    publishes
        .iter()
        .rev()
        .map(|publish| {
            json!({
                "sha": publish.commit_sha,
                "blobSha": publish.blob_sha,
                "publishedAt": publish.published_at,
                "reviewRequested": publish.review_requested,
                "path": doc_readable_path(&doc.repo, &doc.path, &publish.commit_sha),
            })
        })
        .collect()
}

/// The review client (one `<style>`, one `<script>`) with its config inlined.
fn review_client_injection(config: &Value) -> String {
    format!(
        "<style id=\"sm-doc-style\">{}</style><script id=\"sm-doc-client\">{}({});</script>",
        include_str!("doc_client.css"),
        include_str!("doc_client.js"),
        crate::owner_doc_render::inline_json(config)
    )
}

/// The rendered page plus the review client, recording the owner's view of
/// that blob.
async fn view_doc_response(
    state: &Arc<AppState>,
    doc: &OwnerDoc,
    commit_sha: &str,
) -> Result<Response, ApiError> {
    let bytes = load_doc_bytes_async(state, doc, commit_sha).await?;
    let store = owner_doc_store(state);
    if let Err(error) = store.record_view(&doc.id, &git_blob_sha(&bytes)) {
        eprintln!("Owner doc view record failed: {error:#}");
    }
    let publishes = store.publishes(&doc.id)?;
    let latest_sha = publishes.last().map(|publish| publish.commit_sha.clone());
    let pull_request = doc_pull_request_async(state, doc, false).await;
    let (pr_state, pr_url, can_comment) = match &pull_request {
        None => (Value::Null, Value::Null, false),
        Some(Ok(pr)) => (json!(pr.state), json!(pr.url), pr.is_open()),
        Some(Err(error)) => {
            eprintln!("Owner doc PR lookup failed for {}: {error}", doc.id);
            (json!("unknown"), Value::Null, false)
        }
    };
    let pr_url = match (pr_url, doc.pr_number) {
        (Value::Null, Some(pr)) => json!(format!("https://github.com/{}/pull/{pr}", doc.repo)),
        (url, _) => url,
    };
    let config = json!({
        "docId": doc.id,
        "title": doc.title,
        "name": doc_name(&doc.repo, &doc.path),
        "sha": commit_sha,
        "latestSha": latest_sha,
        "prNumber": doc.pr_number,
        "prState": pr_state,
        "prUrl": pr_url,
        "canComment": can_comment,
        "token": issue_doc_token(&state.config, &doc.id),
        "revisions": revision_entries(doc, &publishes),
        "drafts": store.drafts(&doc.id)?,
    });
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8".to_owned()),
            (CACHE_CONTROL, "private, no-cache".to_owned()),
        ],
        Body::from(render_doc_page(
            &doc.path,
            &doc.title,
            &bytes,
            &review_client_injection(&config),
        )),
    )
        .into_response())
}

/// `GET /docs/{id}/head?sha=`: what the banners need. `pr_head_blob_differs`
/// compares the file at the PR head with the viewed revision (`?sha=`), or
/// with the latest publish without one.
async fn doc_head_response(
    state: &Arc<AppState>,
    doc: &OwnerDoc,
    viewed_sha: Option<&str>,
) -> Result<Response, ApiError> {
    let publishes = owner_doc_store(state).publishes(&doc.id)?;
    let latest = publishes
        .last()
        .ok_or(ApiError::NotFound("Doc has no published revision"))?;
    let viewed_sha = viewed_sha.map(|sha| sha.trim().to_ascii_lowercase());
    let viewed_sha = match viewed_sha.filter(|sha| !sha.is_empty()) {
        Some(sha) if is_full_commit_sha(&sha) => sha,
        Some(_) => return Err(bad_request("sha must be a full 40-character commit SHA")),
        None => latest.commit_sha.clone(),
    };
    let mut response = json!({
        "latest_published_sha": latest.commit_sha,
        "latest_reader_path": doc_readable_path(&doc.repo, &doc.path, &latest.commit_sha),
        "pr_head_sha": null,
        "pr_head_blob_sha": null,
        "pr_head_blob_differs": false,
        "pr_head_reader_path": null,
        "pr_state": null,
    });
    match doc_pull_request_async(state, doc, false).await {
        None => {}
        Some(Err(error)) => {
            response["pr_state"] = json!("unknown");
            response["pr_error"] = json!(error);
        }
        Some(Ok(pr)) => {
            response["pr_state"] = json!(pr.state);
            response["pr_head_sha"] = json!(pr.head_sha);
            if is_full_commit_sha(&pr.head_sha) {
                response["pr_head_reader_path"] =
                    json!(doc_readable_path(&doc.repo, &doc.path, &pr.head_sha));
                // A head without the file (deleted on the branch) has nothing to show.
                if let Ok(head_bytes) = load_doc_bytes_async(state, doc, &pr.head_sha).await {
                    let head_blob = git_blob_sha(&head_bytes);
                    let viewed_blob = match publishes.iter().find(|p| p.commit_sha == viewed_sha) {
                        Some(publish) => Some(publish.blob_sha.clone()),
                        None => load_doc_bytes_async(state, doc, &viewed_sha)
                            .await
                            .ok()
                            .map(|bytes| git_blob_sha(&bytes)),
                    };
                    response["pr_head_blob_differs"] =
                        json!(viewed_blob.as_deref() != Some(head_blob.as_str()));
                    response["pr_head_blob_sha"] = json!(head_blob);
                }
            }
        }
    }
    Ok(Json(response).into_response())
}

async fn raw_doc_response(
    state: &Arc<AppState>,
    doc: &OwnerDoc,
    commit_sha: &str,
) -> Result<Response, ApiError> {
    let bytes = load_doc_bytes_async(state, doc, commit_sha).await?;
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

/// The id sub-route a GET or POST names, when `first` is a stored doc id and
/// `rest` is one of that method's actions. Anything else is a readable name.
fn id_subroute(
    state: &AppState,
    first: &str,
    rest: &str,
    actions: &[&str],
) -> Result<Option<OwnerDoc>, ApiError> {
    if !is_owner_doc_id(first) || !actions.contains(&rest) {
        return Ok(None);
    }
    Ok(owner_doc_store(state).get(first)?)
}

/// `GET /docs/{a}/{*rest}`: the internal id routes (`/docs/{id}/view`,
/// `/docs/{id}/raw`) or the readable reader `/docs/<repo-name>/<path>`.
///
/// The readable form renders the pinned `?version=` (a commit SHA prefix
/// matching one of the doc's publishes), or the latest publish without one,
/// in place: it never redirects, so the address bar keeps the readable URL.
/// `?format=json` returns the doc's metadata instead, which is how `sm doc`
/// resolves a readable name.
pub(super) async fn get_owner_doc_subpath(
    State(state): State<Arc<AppState>>,
    Path((first, rest)): Path<(String, String)>,
    Query(query): Query<DocSubpathQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    // The JSON endpoints also take the page's doc token; pages never do.
    if let Some(doc) = id_subroute(&state, &first, &rest, &["head", "drafts"])? {
        if !doc_token_presented(&state, request.headers(), &doc.id) {
            ensure_owner_doc_read_allowed(&state, &request)?;
        }
        return if rest == "head" {
            doc_head_response(&state, &doc, query.sha.as_deref()).await
        } else {
            Ok(Json(json!({ "drafts": owner_doc_store(&state).drafts(&doc.id)? })).into_response())
        };
    }
    ensure_owner_doc_read_allowed(&state, &request)?;
    if let Some(doc) = id_subroute(&state, &first, &rest, &["view", "raw"])? {
        let commit_sha = id_route_commit(&state, &doc, query.sha.as_deref())?;
        return if rest == "raw" {
            raw_doc_response(&state, &doc, &commit_sha).await
        } else {
            view_doc_response(&state, &doc, &commit_sha).await
        };
    }
    let version = query
        .version
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty());
    let resolved = match owner_doc_store(&state).resolve_readable(&first, &rest, version)? {
        Ok(resolved) => resolved,
        // The unpublished-changes banner links to the PR head by version.
        Err(ReadableDocError::VersionNotFound) => {
            match readable_pr_head(&state, &first, &rest, version).await? {
                Some(resolved) => resolved,
                None => return Err(readable_not_found(ReadableDocError::VersionNotFound)),
            }
        }
        Err(error) => return Err(readable_not_found(error)),
    };
    let (doc, commit_sha) = resolved;
    if query.format.as_deref() == Some("json") {
        return doc_metadata_response(&state, &doc, request.headers());
    }
    view_doc_response(&state, &doc, &commit_sha).await
}

fn readable_not_found(error: ReadableDocError) -> ApiError {
    ApiError::Status {
        status: StatusCode::NOT_FOUND,
        detail: error.detail().to_owned(),
    }
}

/// A `?version=` that matches no publish may name a PR doc's current head,
/// which is how the reader shows changes the agent pushed but hasn't
/// republished. Once the head moves on, that link is a 404 like any other
/// unknown version.
async fn readable_pr_head(
    state: &Arc<AppState>,
    name: &str,
    path: &str,
    version: Option<&str>,
) -> Result<Option<(OwnerDoc, String)>, ApiError> {
    let Some(version) = version
        .map(str::to_ascii_lowercase)
        .filter(|version| is_doc_version(version))
    else {
        return Ok(None);
    };
    for doc in owner_doc_store(state).pr_docs_named(name, path)? {
        if let Some(Ok(pr)) = doc_pull_request_async(state, &doc, false).await {
            if is_full_commit_sha(&pr.head_sha) && pr.head_sha.starts_with(&version) {
                return Ok(Some((doc, pr.head_sha)));
            }
        }
    }
    Ok(None)
}

/// Normal CLI/app write auth, or the doc's own token.
fn ensure_doc_write_allowed(
    state: &AppState,
    headers: &HeaderMap,
    peer_addr: SocketAddr,
    doc_id: &str,
    rest: &str,
) -> Result<(), ApiError> {
    if !doc_token_presented(state, headers, doc_id) {
        ensure_session_allowed_from_parts(
            &state.config,
            headers,
            Some(peer_addr),
            &format!("/docs/{doc_id}/{rest}"),
        )?;
    }
    ensure_core_writes_enabled(state)
}

fn parse_json_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(if body.is_empty() { b"{}" } else { body })
        .map_err(|error| bad_request(format!("invalid JSON body: {error}")))
}

/// `POST /docs/{id}/retract|drafts|review`.
pub(super) async fn post_owner_doc_subpath(
    State(state): State<Arc<AppState>>,
    Path((doc_id, rest)): Path<(String, String)>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    if !matches!(rest.as_str(), "retract" | "drafts" | "review") {
        ensure_session_allowed_from_parts(
            &state.config,
            &headers,
            Some(peer_addr),
            &format!("/docs/{doc_id}/{rest}"),
        )?;
        return Err(ApiError::NotFound("Not found"));
    }
    if rest == "retract" {
        ensure_session_allowed_from_parts(
            &state.config,
            &headers,
            Some(peer_addr),
            &format!("/docs/{doc_id}/{rest}"),
        )?;
        ensure_core_writes_enabled(&state)?;
        find_doc(&state, &doc_id)?;
        let store = owner_doc_store(&state);
        store.retract(&doc_id)?;
        let summary = store
            .summary(&doc_id)?
            .ok_or(ApiError::NotFound("Doc not found"))?;
        return Ok(Json(summary_json(&state.config, &summary, &headers)?));
    }
    ensure_doc_write_allowed(&state, &headers, peer_addr, &doc_id, &rest)?;
    let doc = find_doc(&state, &doc_id)?;
    if rest == "drafts" {
        // Drafts don't change under a review being submitted.
        let _guard = state.owner_doc_review_lock.lock().await;
        return create_draft(&state, &doc, parse_json_body(&body)?).map(Json);
    }
    review::submit_owner_doc_review(&state, &doc, parse_json_body(&body)?)
        .await
        .map(Json)
}

/// `PATCH /docs/{id}/drafts/{draft_id}`: edit a draft's text.
pub(super) async fn patch_owner_doc_subpath(
    State(state): State<Arc<AppState>>,
    Path((doc_id, rest)): Path<(String, String)>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ensure_doc_write_allowed(&state, &headers, peer_addr, &doc_id, &rest)?;
    let draft_id = draft_subroute(&rest)?;
    let doc = find_doc(&state, &doc_id)?;
    let payload: UpdateDraftRequest = parse_json_body(&body)?;
    let text = validated_draft_body(&payload.body)?;
    let _guard = state.owner_doc_review_lock.lock().await;
    let draft = owner_doc_store(&state)
        .update_draft(&doc.id, draft_id, &text)?
        .ok_or(ApiError::NotFound("Draft not found"))?;
    Ok(Json(serde_json::to_value(draft)?))
}

/// `DELETE /docs/{id}/drafts/{draft_id}`.
pub(super) async fn delete_owner_doc_subpath(
    State(state): State<Arc<AppState>>,
    Path((doc_id, rest)): Path<(String, String)>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    ensure_doc_write_allowed(&state, &headers, peer_addr, &doc_id, &rest)?;
    let draft_id = draft_subroute(&rest)?;
    let doc = find_doc(&state, &doc_id)?;
    let _guard = state.owner_doc_review_lock.lock().await;
    if !owner_doc_store(&state).delete_draft(&doc.id, draft_id)? {
        return Err(ApiError::NotFound("Draft not found"));
    }
    Ok(Json(json!({ "deleted": true, "id": draft_id })))
}

fn draft_subroute(rest: &str) -> Result<&str, ApiError> {
    rest.strip_prefix("drafts/")
        .filter(|id| !id.is_empty() && !id.contains('/'))
        .ok_or(ApiError::NotFound("Not found"))
}

/// Draft and quote size caps: far above any real comment, well under
/// GitHub's 65536-character comment limit once quoted.
const MAX_DRAFT_BODY: usize = 20_000;
const MAX_DRAFT_QUOTE: usize = 20_000;

fn validated_draft_body(body: &str) -> Result<String, ApiError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(bad_request("A comment needs some text"));
    }
    if body.chars().count() > MAX_DRAFT_BODY {
        return Err(bad_request(format!(
            "Comments are limited to {MAX_DRAFT_BODY} characters"
        )));
    }
    Ok(body.to_owned())
}

#[derive(Debug, Deserialize)]
struct CreateDraftRequest {
    sha: String,
    #[serde(default)]
    line: Option<i64>,
    #[serde(default)]
    quote: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct UpdateDraftRequest {
    body: String,
}

fn create_draft(
    state: &AppState,
    doc: &OwnerDoc,
    payload: CreateDraftRequest,
) -> Result<Value, ApiError> {
    let sha = payload.sha.trim().to_ascii_lowercase();
    if !is_full_commit_sha(&sha) {
        return Err(bad_request("sha must be a full 40-character commit SHA"));
    }
    if payload.line.is_some_and(|line| line < 1) {
        return Err(bad_request("line must be positive"));
    }
    let quote = payload.quote.trim();
    if quote.chars().count() > MAX_DRAFT_QUOTE {
        return Err(bad_request(format!(
            "Quotes are limited to {MAX_DRAFT_QUOTE} characters"
        )));
    }
    let body = validated_draft_body(&payload.body)?;
    let draft = owner_doc_store(state).create_draft(&doc.id, &sha, payload.line, quote, &body)?;
    Ok(serde_json::to_value(draft)?)
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
        "name": doc_name(&summary.doc.repo, &summary.doc.path),
        "reader_path": doc_reader_path(summary),
        "review_undelivered": summary.review_undelivered,
    });
    if let Some(base) = browser_base {
        entry["browser_url"] = json!(format!("{base}{}", doc_reader_path(summary)));
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
    fn doc_tokens_cover_one_doc_and_expire() {
        let mut config = AppConfig::default();
        assert_eq!(issue_doc_token(&config, "d0c00001"), None);
        config.google_auth.session_cookie_secret = Some("cookie-secret".to_owned());
        let token = issue_doc_token(&config, "d0c00001").unwrap();
        assert!(token.starts_with("smdt_d0c00001."), "{token}");
        assert!(doc_token_valid(&config, "d0c00001", &token));
        // Another doc, a forged doc id, garbage, an expired token.
        assert!(!doc_token_valid(&config, "d0c00002", &token));
        let forged = token.replacen("d0c00001", "d0c00002", 1);
        assert!(!doc_token_valid(&config, "d0c00002", &forged));
        assert!(!doc_token_valid(
            &config,
            "d0c00001",
            "smdt_d0c00001.9999999999.AAAA"
        ));
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let expired =
            issue_doc_token_at(&config, "d0c00001", now - DOC_TOKEN_TTL_SECONDS - 1).unwrap();
        assert!(!doc_token_valid(&config, "d0c00001", &expired));
        // A rotated secret invalidates every token.
        config.google_auth.session_cookie_secret = Some("rotated".to_owned());
        assert!(!doc_token_valid(&config, "d0c00001", &token));
    }

    #[test]
    fn reader_url_uses_the_callers_host() {
        let path = || "/docs/widgets/memo.md?version=aaaaaaaaaaaa".to_owned();
        let mut headers = HeaderMap::new();
        assert_eq!(doc_reader_url(&headers, path()), path());
        headers.insert(HOST, "127.0.0.1:8420".parse().unwrap());
        assert_eq!(
            doc_reader_url(&headers, path()),
            format!("http://127.0.0.1:8420{}", path())
        );
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(
            doc_reader_url(&headers, path()),
            format!("https://127.0.0.1:8420{}", path())
        );
    }
}
