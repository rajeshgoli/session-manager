//! Owner settings over HTTP (sm#1718, spec 1710 appendix D4):
//! `GET /client/settings` and `PUT /client/settings`. The phone and the
//! browser share them; a `PUT` that changes queue limits applies them from
//! the queue's next admission pass (appendix D5), and one that changes
//! terminal limits applies them from the next attach (sm#1763).

use super::*;

pub(super) async fn get_settings(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    board::owner_guard(&state, &headers, peer_addr, "GET", &uri, false)?;
    let config_limits = terminal_config_limits(&state);
    let settings = tokio::task::spawn_blocking(move || state.session_store.owner_settings())
        .await
        .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    Ok(Json(with_config_limits(settings, config_limits)))
}

pub(super) async fn put_settings(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    board::owner_guard(&state, &headers, peer_addr, "PUT", &uri, true)?;
    ensure_core_writes_enabled(&state)?;
    let config_limits = terminal_config_limits(&state);
    let settings = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let queue_state_dir = expand_home(&state.config.queue_runner_state_dir().to_string_lossy());
        // Store and apply under the live policy's lock, so overlapping PUTs
        // cannot leave admission on older limits than the stored ones.
        let mut live = state
            .queue_admission
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = *live;
        let mut next = current;
        let settings = state
            .session_store
            .update_owner_settings(&body, |settings| {
                next = crate::owner_settings::queue_admission_policy(&state.config, settings);
                if next == current {
                    return Ok(Ok(()));
                }
                Ok(
                    match crate::queue::queue_admission_policy_refusal(&queue_state_dir, next)? {
                        Some(refusal) => Err(refusal),
                        None => Ok(()),
                    },
                )
            })?
            .map_err(|detail| ApiError::Status {
                status: StatusCode::BAD_REQUEST,
                detail,
            })?;
        *live = next;
        drop(live);
        *state
            .terminal_limits
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            crate::owner_settings::terminal_limits(&state.config, &settings);
        if next != current && state.config.rust_core.runtime_enabled {
            admit_now(&state, &queue_state_dir, next);
        }
        Ok(settings)
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    Ok(Json(with_config_limits(settings, config_limits)))
}

/// Config's terminal limits, which a `null` owner value falls back to.
fn terminal_config_limits(state: &AppState) -> Value {
    crate::owner_settings::TerminalLimits::from_config(&state.config).to_json()
}

/// The settings object with `terminal_config_limits` added, so the phone
/// and the web can show what Reset restores. It is read-only: a `PUT`
/// naming it is refused as an unknown field.
fn with_config_limits(mut settings: Value, config_limits: Value) -> Value {
    settings["terminal_config_limits"] = config_limits;
    settings
}

/// Run an admission pass so a raised limit starts waiting jobs now.
/// Lowering a limit never stops a running job; it only holds new starts.
fn admit_now(state: &AppState, queue_state_dir: &std::path::Path, policy: QueueAdmissionPolicy) {
    if let Err(error) =
        RetainedQueueStore::admit_queue_jobs_in_state_dir_continuing_after_failed_start_with_policy(
            queue_state_dir,
            &expand_home(&state.config.sm_send.db_path),
            state.config.queue_runner.cancel_grace_seconds,
            policy,
        )
    {
        eprintln!("queue admission after a settings change failed: {error:#}");
    }
}
