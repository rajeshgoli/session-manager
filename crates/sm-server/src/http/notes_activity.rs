//! Owner-only history of destinations used from Notes.
use super::*;
use crate::notes::{activity::NewActivity, NotesStore};

fn store(state: &AppState) -> NotesStore {
    NotesStore::new(expand_home(&state.config.paths.notes_db))
}
fn missing() -> ApiError {
    ApiError::NotFound("note activity not found")
}

pub(super) async fn previews(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    notes::guard(&state, &headers, peer, "GET", &uri)?;
    let rows = board::blocking(&state, |state| Ok(store(state).activity_previews()?)).await?;
    Ok(Json(json!(rows)))
}

pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    notes::guard(&state, &headers, peer, "GET", &uri)?;
    let rows = board::blocking(&state, move |state| Ok(store(state).activities(&id)?))
        .await?
        .ok_or_else(missing)?;
    Ok(Json(json!(rows)))
}

pub(super) async fn record(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(event): Json<NewActivity>,
) -> Result<Json<Value>, ApiError> {
    notes::guard(&state, &headers, peer, "POST", &uri)?;
    event.validate().map_err(|error| ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: error.to_string(),
    })?;
    let row = board::blocking(&state, move |state| {
        Ok(store(state).record_activity(&id, &event)?)
    })
    .await?
    .ok_or_else(missing)?;
    Ok(Json(json!(row)))
}

pub(super) async fn snapshot(
    State(state): State<Arc<AppState>>,
    Path((id, event)): Path<(String, String)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    notes::guard(&state, &headers, peer, "GET", &uri)?;
    let body = board::blocking(&state, move |state| {
        Ok(store(state).activity_body(&id, &event)?)
    })
    .await?
    .ok_or_else(missing)?;
    Ok(Json(json!({"body": body})))
}
