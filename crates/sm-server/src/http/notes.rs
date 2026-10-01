//! Owner-only Notes HTTP API (ticket #1833).
use super::*;
use crate::notes::{NotesStore, Save, MAX_BODY};

fn guard(
    state: &AppState,
    headers: &HeaderMap,
    peer: SocketAddr,
    method: &str,
    uri: &Uri,
) -> Result<(), ApiError> {
    if header_text(headers, "x-sm-session").is_some()
        || header_text(headers, "x-sm-session-id").is_some()
        || header_text(headers, handoff::SESSION_HEADER).is_some()
    {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Notes are owner-only".into(),
        });
    }
    board::owner_guard(state, headers, peer, method, uri, method != "GET")?;
    if owner_web_guard(state, headers, Some(peer), method)?.is_none()
        && authenticated_user(headers, &state.config).is_none()
    {
        return Err(ApiError::Status {
            status: StatusCode::FORBIDDEN,
            detail: "Owner login required for Notes".into(),
        });
    }
    Ok(())
}
fn store(state: &AppState) -> NotesStore {
    NotesStore::new(expand_home(&state.config.paths.notes_db))
}
fn bad(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}
fn missing() -> ApiError {
    ApiError::NotFound("note not found")
}

#[derive(Deserialize)]
pub(super) struct WriteNote {
    body: String,
    title: Option<String>,
    if_version: Option<i64>,
}
#[derive(Deserialize)]
pub(super) struct Restore {
    version: i64,
}
#[derive(Deserialize)]
pub(super) struct Search {
    #[serde(default)]
    q: String,
    #[serde(default)]
    regex: u8,
}
#[derive(Deserialize)]
pub(super) struct Issue {
    repo: String,
    title: String,
    body: String,
}

pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Result<Response, ApiError> {
    guard(&state, request.headers(), peer, "GET", request.uri())?;
    // Browser navigation asks for HTML. Existing API callers, including local
    // clients with Accept: */*, keep receiving the JSON list.
    if request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
    {
        if let Some(shell) = web::shell_page(&state, &request) {
            return Ok(shell);
        }
    }
    let rows = board::blocking(&state, |state| Ok(store(state).list()?)).await?;
    Ok(Json(json!(rows)).into_response())
}
pub(super) async fn get(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &headers, peer, "GET", &uri)?;
    let note = board::blocking(&state, move |state| Ok(store(state).get(&id)?))
        .await?
        .ok_or_else(missing)?;
    Ok(Json(json!(note)))
}
pub(super) async fn create(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(input): Json<WriteNote>,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &headers, peer, "POST", &uri)?;
    if input.body.len() > MAX_BODY {
        return Err(bad("note body exceeds 2 MB"));
    }
    let note = board::blocking(&state, move |state| {
        Ok(store(state).create(&input.body, input.title.as_deref())?)
    })
    .await?;
    Ok(Json(json!(note)))
}
pub(super) async fn save(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(input): Json<WriteNote>,
) -> Result<Response, ApiError> {
    guard(&state, &headers, peer, "PUT", &uri)?;
    if input.body.len() > MAX_BODY {
        return Err(bad("note body exceeds 2 MB"));
    }
    let expected = input
        .if_version
        .ok_or_else(|| bad("if_version is required"))?;
    let saved = board::blocking(&state, move |state| {
        Ok(store(state).save(&id, &input.body, input.title.as_deref(), expected)?)
    })
    .await?;
    match saved {
        Save::Saved(note) => Ok(Json(json!(note)).into_response()),
        Save::Stale(note) => Ok((StatusCode::CONFLICT, Json(json!(note))).into_response()),
        Save::Missing => Err(missing()),
    }
}
pub(super) async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    guard(&state, &headers, peer, "DELETE", &uri)?;
    if board::blocking(&state, move |state| Ok(store(state).delete(&id)?)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(missing())
    }
}
pub(super) async fn revisions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &headers, peer, "GET", &uri)?;
    let rows = board::blocking(&state, move |state| Ok(store(state).revisions(&id)?))
        .await?
        .ok_or_else(missing)?;
    Ok(Json(json!(rows)))
}
pub(super) async fn revision(
    State(state): State<Arc<AppState>>,
    Path((id, version)): Path<(String, i64)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &headers, peer, "GET", &uri)?;
    let row = board::blocking(
        &state,
        move |state| Ok(store(state).revision(&id, version)?),
    )
    .await?
    .ok_or_else(missing)?;
    Ok(Json(json!(row)))
}
pub(super) async fn restore(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(input): Json<Restore>,
) -> Result<Response, ApiError> {
    guard(&state, &headers, peer, "POST", &uri)?;
    let saved = board::blocking(&state, move |state| {
        let store = store(state);
        let revision = store.revision(&id, input.version)?.ok_or_else(missing)?;
        let current = store.get(&id)?.ok_or_else(missing)?;
        Ok(store.save(&id, &revision.body, Some(&current.title), current.version)?)
    })
    .await?;
    match saved {
        Save::Saved(note) => Ok(Json(json!(note)).into_response()),
        Save::Stale(note) => Ok((StatusCode::CONFLICT, Json(json!(note))).into_response()),
        Save::Missing => Err(missing()),
    }
}
pub(super) async fn search(
    State(state): State<Arc<AppState>>,
    Query(query): Query<Search>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &headers, peer, "GET", &uri)?;
    if query.regex > 1 {
        return Err(bad("regex must be 0 or 1"));
    }
    let rows = timeout(
        Duration::from_millis(100),
        board::blocking(&state, move |state| {
            store(state)
                .search(&query.q, query.regex == 1)
                .map_err(|e| bad(e.to_string()))
        }),
    )
    .await
    .map_err(|_| ApiError::Status {
        status: StatusCode::REQUEST_TIMEOUT,
        detail: "search timed out".into(),
    })??;
    Ok(Json(json!(rows)))
}
pub(super) async fn preview(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(input): Json<WriteNote>,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &headers, peer, "POST", &uri)?;
    if input.body.len() > MAX_BODY {
        return Err(bad("note body exceeds 2 MB"));
    }
    Ok(Json(
        json!({"html": crate::owner_doc_render::render_markdown_sanitized(&input.body)}),
    ))
}
pub(super) async fn import(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &headers, peer, "POST", &uri)?;
    let field = multipart
        .next_field()
        .await
        .map_err(|e| bad(e.to_string()))?
        .ok_or_else(|| bad("file is required"))?;
    let filename = field.file_name().unwrap_or_default().to_owned();
    if !(filename.ends_with(".md") || filename.ends_with(".txt")) {
        return Err(bad("import requires a .md or .txt file"));
    }
    let bytes = field.bytes().await.map_err(|e| bad(e.to_string()))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| bad("file must be UTF-8"))?;
    let parts = crate::notes::import_parts(text);
    if parts.iter().any(|part| part.len() > MAX_BODY) {
        return Err(bad("imported note exceeds 2 MB"));
    }
    let ids = board::blocking(&state, move |state| {
        let store = store(state);
        parts
            .into_iter()
            .map(|body| {
                store
                    .create(&body, None)
                    .map(|note| note.id)
                    .map_err(Into::into)
            })
            .collect::<Result<Vec<_>, ApiError>>()
    })
    .await?;
    Ok(Json(json!({"ids":ids})))
}
pub(super) async fn create_issue(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(issue): Json<Issue>,
) -> Result<Json<Value>, ApiError> {
    guard(&state, &headers, peer, "POST", &uri)?;
    let repo = crate::work_claims::canonical_repo(&issue.repo);
    crate::owner_docs::validate_repo_slug(&repo).map_err(|e| bad(e.to_string()))?;
    if issue.title.trim().is_empty() {
        return Err(bad("title is required"));
    }
    let (number, url) = board::blocking(&state, move |state| {
        let configured = state
            .config
            .board
            .repos
            .iter()
            .any(|known| crate::work_claims::canonical_repo(known) == repo);
        let claimed = claims::work_claim_store(state)
            .all_claims()?
            .iter()
            .any(|claim| claim.repo == repo);
        let lane = board::board_store(state)
            .active_lanes()?
            .iter()
            .any(|lane| lane.goal.0 == repo);
        if !(configured || claimed || lane) {
            return Err(bad("repository is not known to sm"));
        }
        state
            .board_source
            .create_issue(&repo, &issue.title, &issue.body)
            .map_err(|detail| ApiError::Status {
                status: StatusCode::BAD_GATEWAY,
                detail,
            })
    })
    .await?;
    Ok(Json(json!({"number":number,"url":url})))
}
