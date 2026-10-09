//! Analytics endpoints (sm#1662): `GET /client/analytics/spend` and
//! `GET /client/analytics/time`. Owner reads like `/client/queue`; each
//! parameter set's result is cached for 60 s. Data assembly lives in
//! `crate::analytics_spend` and `crate::analytics_time`.

use super::*;
use crate::analytics_spend::{self, SpendRange, SpendSources};
use crate::analytics_time::{self, TimeRange, TimeSources};

const ANALYTICS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Default, Deserialize)]
pub(super) struct SpendParams {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    range: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct TimeParams {
    #[serde(default)]
    range: Option<String>,
}

pub(super) type AnalyticsCache = BTreeMap<String, (std::time::Instant, Value)>;

/// The databases Analytics › Time reads.
pub(super) struct TimePaths {
    activity_db: PathBuf,
    usage_db: PathBuf,
    queue_db: PathBuf,
    queue_runner_db: PathBuf,
}

impl TimePaths {
    pub(super) fn new(config: &AppConfig) -> Self {
        Self {
            activity_db: expand_home(&config.activity.db_path),
            usage_db: expand_home(&config.usage.db_path),
            queue_db: expand_home(&config.sm_send.db_path),
            queue_runner_db: expand_home(&config.queue_runner_state_dir().to_string_lossy())
                .join("queue_runner.db"),
        }
    }

    pub(super) fn activity_db(&self) -> &StdPath {
        &self.activity_db
    }

    pub(super) fn sources<'a>(
        &'a self,
        live_sessions: &'a BTreeMap<String, bool>,
    ) -> TimeSources<'a> {
        TimeSources {
            activity_db: &self.activity_db,
            usage_db: &self.usage_db,
            queue_db: &self.queue_db,
            queue_runner_db: &self.queue_runner_db,
            live_sessions,
            repo_of: &crate::work_attribution::folder_repo,
        }
    }
}

/// Live sessions, and whether each is in a turn now.
pub(super) fn live_sessions(records: &[SessionRecord]) -> BTreeMap<String, bool> {
    records
        .iter()
        .filter(|record| !record.is_stopped())
        .map(|record| (record.id.clone(), record.lifecycle_status() == "running"))
        .collect()
}

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
                    detail: "provider must be claude, codex or local".into(),
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
    store(&state, cache_key, &body);
    Ok(Json(body))
}

/// The Claude and Codex meters the web usage dash shows (sm#1881).
/// Uncached: samples land every few seconds and this read is a handful of
/// indexed lookups.
pub(super) async fn client_usage_meters(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_client_read(&state, &request)?;
    let usage_db = expand_home(&state.config.usage.db_path);
    let account_labels: BTreeMap<String, String> = state
        .config
        .usage
        .accounts
        .iter()
        .map(|account| (account.key.clone(), account.label.clone()))
        .collect();
    let body = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        Ok(serde_json::to_value(crate::usage_meters::meters(
            &usage_db,
            &account_labels,
            time::OffsetDateTime::now_utc(),
        )?)?)
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    Ok(Json(body))
}

pub(super) async fn client_analytics_time(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TimeParams>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    ensure_client_read(&state, &request)?;
    let range = match params.range.as_deref() {
        None => TimeRange::Week,
        Some(value) => TimeRange::parse(value).ok_or_else(|| ApiError::Status {
            status: StatusCode::BAD_REQUEST,
            detail: "range must be 24h, 7d or 30d".into(),
        })?,
    };
    let cache_key = format!("time:{}", range.id());
    if let Some(body) = cached(&state, &cache_key) {
        return Ok(Json(body));
    }

    let paths = TimePaths::new(&state.config);
    let live_sessions = live_sessions(&state.session_store.list_sessions(true)?);
    let body = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        Ok(serde_json::to_value(analytics_time::time_report(
            &paths.sources(&live_sessions),
            range,
            time::OffsetDateTime::now_utc(),
        )?)?)
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    store(&state, cache_key, &body);
    Ok(Json(body))
}

fn store(state: &AppState, key: String, body: &Value) {
    if let Ok(mut cache) = state.analytics_cache.lock() {
        cache.retain(|_, (at, _)| at.elapsed() < ANALYTICS_CACHE_TTL);
        cache.insert(key, (std::time::Instant::now(), body.clone()));
    }
}

fn cached(state: &AppState, key: &str) -> Option<Value> {
    let cache = state.analytics_cache.lock().ok()?;
    cache
        .get(key)
        .filter(|(at, _)| at.elapsed() < ANALYTICS_CACHE_TTL)
        .map(|(_, body)| body.clone())
}
