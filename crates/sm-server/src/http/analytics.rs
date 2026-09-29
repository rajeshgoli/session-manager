//! Analytics endpoints (sm#1662): `GET /client/analytics/spend`. Owner
//! reads like `/client/queue`; each parameter set's result is cached for
//! 60 s. Data assembly lives in `crate::analytics_spend`.

use super::*;
use crate::analytics_spend::{self, SpendRange, SpendSources};

const ANALYTICS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Default, Deserialize)]
pub(super) struct SpendParams {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    range: Option<String>,
}

pub(super) type AnalyticsCache = BTreeMap<String, (std::time::Instant, Value)>;

pub(super) async fn client_analytics_spend(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SpendParams>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_client_read(&state, &request)?;
    let provider = match params.provider.as_deref() {
        None => None,
        Some(value) => {
            Some(
                analytics_spend::parse_provider(value).ok_or_else(|| ApiError::Status {
                    status: StatusCode::BAD_REQUEST,
                    detail: "provider must be claude or codex".into(),
                })?,
            )
        }
    };
    let range = match params.range.as_deref() {
        None => SpendRange::Week,
        Some(value) => SpendRange::parse(value).ok_or_else(|| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "range must be week, last_week or 4w".into(),
        })?,
    };
    let cache_key = format!("spend:{}:{}", provider.unwrap_or("default"), range.id());
    if let Some(body) = cached(&state, &cache_key) {
        return Ok(Json(body));
    }

    let usage_db = expand_home(&state.config.usage.db_path);
    let queue_db = expand_home(&state.config.sm_send.db_path);
    let account_labels: BTreeMap<String, String> = state
        .config
        .usage
        .accounts
        .iter()
        .map(|account| (account.key.clone(), account.label.clone()))
        .collect();
    let live_sessions: BTreeSet<String> = state
        .session_store
        .list_sessions(true)?
        .into_iter()
        .filter(|record| !record.is_stopped() && !record.is_retired())
        .map(|record| record.id)
        .collect();
    let body = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let now = time::OffsetDateTime::now_utc();
        let provider = match provider {
            Some(provider) => provider,
            None => analytics_spend::default_provider(&usage_db, now)?,
        };
        let sources = SpendSources {
            usage_db: &usage_db,
            queue_db: &queue_db,
            account_labels: &account_labels,
            live_sessions: &live_sessions,
            repo_of: &crate::work_attribution::folder_repo,
        };
        Ok(serde_json::to_value(analytics_spend::spend_report(
            &sources, provider, range, now,
        )?)?)
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    if let Ok(mut cache) = state.analytics_cache.lock() {
        cache.retain(|_, (at, _)| at.elapsed() < ANALYTICS_CACHE_TTL);
        cache.insert(cache_key, (std::time::Instant::now(), body.clone()));
    }
    Ok(Json(body))
}

fn cached(state: &AppState, key: &str) -> Option<Value> {
    let cache = state.analytics_cache.lock().ok()?;
    cache
        .get(key)
        .filter(|(at, _)| at.elapsed() < ANALYTICS_CACHE_TTL)
        .map(|(_, body)| body.clone())
}
