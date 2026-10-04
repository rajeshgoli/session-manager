use super::*;

pub(super) async fn status(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    follows::owner_web_or_guard(&state, &headers, peer, "GET", &uri)?;
    let host = crate::local_model::live(&expand_home(
        &state.config.queue_runner_state_dir().to_string_lossy(),
    ));
    let result = tokio::task::spawn_blocking(move || {
        host.map_or(
            Ok(json!({"model":null,"seats_used":0,"judge_seats":0})),
            |h| h.status(),
        )
    })
    .await
    .map_err(|e| ApiError::from(anyhow::anyhow!(e)))??;
    Ok(Json(result))
}
pub(super) async fn load(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(request): Json<crate::local_model::LoadRequest>,
) -> Result<Json<Value>, ApiError> {
    follows::owner_web_or_guard(&state, &headers, peer, "POST", &uri)?;
    ensure_core_writes_enabled(&state)?;
    let host = host(&state)?;
    let result = tokio::task::spawn_blocking(move || {
        host.load(request)?;
        host.status()
    })
    .await
    .map_err(|e| ApiError::from(anyhow::anyhow!(e)))?
    .map_err(model_error)?;
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UnloadRequest {
    #[serde(default)]
    force: bool,
}
pub(super) async fn unload(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(request): Json<UnloadRequest>,
) -> Result<Json<Value>, ApiError> {
    follows::owner_web_or_guard(&state, &headers, peer, "POST", &uri)?;
    ensure_core_writes_enabled(&state)?;
    let host = host(&state)?;
    let result = tokio::task::spawn_blocking(move || {
        host.unload(request.force, None)?;
        host.status()
    })
    .await
    .map_err(|e| ApiError::from(anyhow::anyhow!(e)))?
    .map_err(model_error)?;
    Ok(Json(result))
}
fn host(state: &AppState) -> Result<Arc<crate::local_model::ModelHost>, ApiError> {
    if !state.config.rust_core.runtime_enabled {
        return Err(ApiError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "local model control requires the live runtime".into(),
        });
    }
    crate::local_model::live(&expand_home(
        &state.config.queue_runner_state_dir().to_string_lossy(),
    ))
    .ok_or_else(|| ApiError::Status {
        status: StatusCode::SERVICE_UNAVAILABLE,
        detail: "local model controller is not registered".into(),
    })
}
fn model_error(error: anyhow::Error) -> ApiError {
    ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: format!("{error:#}"),
    }
}
