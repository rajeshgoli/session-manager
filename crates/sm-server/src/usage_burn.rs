use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{Map, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::usage_identity::{Provider, UsageIdentityStore};

const USAGE_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_WINDOW_MINUTES: i64 = 300;
const WEEKLY_WINDOW_MINUTES: i64 = 10_080;
const DB_TIMESTAMP_FORMAT: &[time::format_description::FormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z"
);

#[derive(Debug, Clone, PartialEq)]
pub struct BurnWindowSample {
    pub window_kind: String,
    pub window_scope: Option<String>,
    pub duration_minutes: i64,
    pub percent: f64,
    pub resets_at: OffsetDateTime,
    pub severity: Option<String>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct UsageBurnStore {
    db_path: PathBuf,
    identity_store: UsageIdentityStore,
}

impl UsageBurnStore {
    pub fn new(db_path: impl Into<PathBuf>) -> Result<Self> {
        let db_path = db_path.into();
        let store = Self {
            identity_store: UsageIdentityStore::new(&db_path)?,
            db_path,
        };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<()> {
        self.open()?
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS burn_samples (
                  id           INTEGER PRIMARY KEY AUTOINCREMENT,
                  account_key  TEXT NOT NULL REFERENCES accounts(account_key),
                  window_kind  TEXT NOT NULL,
                  window_scope TEXT,
                  window_start TEXT NOT NULL,
                  percent      REAL NOT NULL,
                  resets_at    TEXT NOT NULL,
                  severity     TEXT,
                  is_active    INTEGER,
                  source       TEXT NOT NULL,
                  observed_at  TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_burn_window
                  ON burn_samples(account_key, window_kind, observed_at);
                "#,
            )
            .context("failed to initialize usage burn schema")?;
        Ok(())
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self
            .db_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create usage DB directory {}", parent.display())
            })?;
        }
        let connection = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open usage DB {}", self.db_path.display()))?;
        connection.busy_timeout(USAGE_DB_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }

    pub fn record_for_provider(
        &self,
        provider: Provider,
        windows: &[BurnWindowSample],
        source: &str,
        observed_at: OffsetDateTime,
    ) -> Result<usize> {
        let Some(attribution) = self.identity_store.account_at(provider, observed_at)? else {
            return Ok(0);
        };
        self.record_for_account(&attribution.account_key, windows, source, observed_at)
    }

    pub fn record_for_account(
        &self,
        account_key: &str,
        windows: &[BurnWindowSample],
        source: &str,
        observed_at: OffsetDateTime,
    ) -> Result<usize> {
        if account_key.trim().is_empty() || source.trim().is_empty() {
            bail!("burn sample account and source must not be empty");
        }
        for window in windows {
            validate_window(window)?;
        }
        let observed_at = format_timestamp(observed_at)?;
        let mut connection = self.open()?;
        let transaction = connection.transaction()?;
        let inserted = insert_windows(&transaction, account_key, windows, source, &observed_at)?;
        transaction.commit()?;
        Ok(inserted)
    }

    pub fn record_claude_statusline(
        &self,
        observed_at: OffsetDateTime,
        five_hour_percent: Option<f64>,
        five_hour_resets_at: Option<&str>,
        seven_day_percent: Option<f64>,
        seven_day_resets_at: Option<&str>,
    ) -> Result<usize> {
        let mut windows = Vec::new();
        if let Some(window) = paired_claude_window(
            "session_5h",
            SESSION_WINDOW_MINUTES,
            five_hour_percent,
            five_hour_resets_at,
        )? {
            windows.push(window);
        }
        if let Some(window) = paired_claude_window(
            "weekly_all",
            WEEKLY_WINDOW_MINUTES,
            seven_day_percent,
            seven_day_resets_at,
        )? {
            windows.push(window);
        }
        self.record_for_provider(Provider::Claude, &windows, "statusline", observed_at)
    }

    pub fn record_claude_json_file(&self, path: &Path) -> Result<usize> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read Claude usage cache {}", path.display()))?;
        let value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse Claude usage cache {}", path.display()))?;
        let Some(snapshot) = parse_claude_cached_usage(&value)? else {
            return Ok(0);
        };
        self.record_for_account(
            &snapshot.account_key,
            &snapshot.windows,
            "claude_json",
            snapshot.observed_at,
        )
    }

    pub fn record_codex_event(
        &self,
        event: &Map<String, Value>,
        received_at: OffsetDateTime,
    ) -> Result<usize> {
        if !is_codex_rate_limit_event(event) {
            return Ok(0);
        }
        let observed_at = json_string(event, "ts")
            .as_deref()
            .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
            .unwrap_or(received_at);
        let Some(attribution) = self
            .identity_store
            .account_at(Provider::Codex, observed_at)?
        else {
            return Ok(0);
        };
        let rate_limits = event
            .get("payload")
            .and_then(Value::as_object)
            .and_then(|payload| {
                payload
                    .get("rateLimits")
                    .or_else(|| payload.get("rate_limits"))
            })
            .and_then(Value::as_object)
            .or_else(|| event.get("rate_limits").and_then(Value::as_object));
        let Some(rate_limits) = rate_limits else {
            return Ok(0);
        };
        let limit_id = json_string(rate_limits, "limitId")
            .or_else(|| json_string(rate_limits, "limit_id"))
            .unwrap_or_else(|| "codex".to_owned());
        let scope = (limit_id != "codex").then_some(limit_id);
        let severity = json_string(rate_limits, "rateLimitReachedType")
            .or_else(|| json_string(rate_limits, "rate_limit_reached_type"))
            .map(|_| "critical".to_owned());
        let observed_at_text = format_timestamp(observed_at)?;
        let mut connection = self.open()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut windows = Vec::new();
        for slot in ["primary", "secondary"] {
            let Some(update) = rate_limits.get(slot).and_then(Value::as_object) else {
                continue;
            };
            let Some(duration_minutes) = json_i64(
                update
                    .get("windowDurationMins")
                    .or_else(|| update.get("window_minutes")),
            ) else {
                continue;
            };
            let percent_update = json_f64(
                update
                    .get("usedPercent")
                    .or_else(|| update.get("used_percent")),
            );
            let reset_update = codex_reset_at(update, observed_at);
            if percent_update.is_none() && reset_update.is_none() {
                continue;
            }
            let window_kind = format!("codex_{duration_minutes}");
            let previous = latest_window(
                &transaction,
                &attribution.account_key,
                &window_kind,
                scope.as_deref(),
                &observed_at_text,
            )?;
            let percent = percent_update.or_else(|| previous.as_ref().map(|window| window.0));
            let resets_at = reset_update.or_else(|| previous.as_ref().map(|window| window.1));
            let (Some(percent), Some(resets_at)) = (percent, resets_at) else {
                continue;
            };
            windows.push(BurnWindowSample {
                window_kind,
                window_scope: scope.clone(),
                duration_minutes,
                percent,
                resets_at,
                severity: severity.clone(),
                is_active: None,
            });
        }
        for window in &windows {
            validate_window(window)?;
        }
        let inserted = insert_windows(
            &transaction,
            &attribution.account_key,
            &windows,
            "codex_event",
            &observed_at_text,
        )?;
        transaction.commit()?;
        Ok(inserted)
    }
}

fn latest_window(
    transaction: &Transaction<'_>,
    account_key: &str,
    window_kind: &str,
    window_scope: Option<&str>,
    observed_at: &str,
) -> Result<Option<(f64, OffsetDateTime)>> {
    let row = transaction
        .query_row(
            r#"
            SELECT percent, resets_at
            FROM burn_samples
            WHERE account_key = ?1
              AND window_kind = ?2
              AND window_scope IS ?3
              AND observed_at <= ?4
            ORDER BY observed_at DESC, id DESC
            LIMIT 1
            "#,
            params![account_key, window_kind, window_scope, observed_at],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(percent, resets_at)| Ok((percent, OffsetDateTime::parse(&resets_at, &Rfc3339)?)))
        .transpose()
}

fn insert_windows(
    transaction: &Transaction<'_>,
    account_key: &str,
    windows: &[BurnWindowSample],
    source: &str,
    observed_at: &str,
) -> Result<usize> {
    let mut inserted = 0;
    for window in windows {
        let resets_at = format_timestamp(window.resets_at)?;
        let window_start =
            format_timestamp(window.resets_at - time::Duration::minutes(window.duration_minutes))?;
        inserted += transaction.execute(
            r#"
            INSERT INTO burn_samples (
              account_key, window_kind, window_scope, window_start, percent,
              resets_at, severity, is_active, source, observed_at
            )
            SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
            WHERE NOT EXISTS (
              SELECT 1 FROM burn_samples
              WHERE account_key = ?1
                AND window_kind = ?2
                AND window_scope IS ?3
                AND source = ?9
                AND observed_at = ?10
            )
            "#,
            params![
                account_key,
                window.window_kind,
                window.window_scope,
                window_start,
                window.percent,
                resets_at,
                window.severity,
                window.is_active,
                source,
                observed_at,
            ],
        )?;
    }
    Ok(inserted)
}

#[derive(Debug, Clone)]
struct ClaudeCachedUsage {
    account_key: String,
    observed_at: OffsetDateTime,
    windows: Vec<BurnWindowSample>,
}

fn parse_claude_cached_usage(value: &Value) -> Result<Option<ClaudeCachedUsage>> {
    let Some(cached) = value
        .get("cachedUsageUtilization")
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    let account_id = json_string(cached, "accountUuid")
        .or_else(|| {
            value
                .get("oauthAccount")
                .and_then(Value::as_object)
                .and_then(|account| json_string(account, "accountUuid"))
        })
        .context("Claude usage cache is missing accountUuid")?;
    let fetched_at_ms =
        json_i64(cached.get("fetchedAtMs")).context("Claude usage cache is missing fetchedAtMs")?;
    let observed_at =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(fetched_at_ms) * 1_000_000)
            .context("Claude usage fetchedAtMs is out of range")?;
    let utilization = cached
        .get("utilization")
        .and_then(Value::as_object)
        .context("Claude usage cache is missing utilization")?;
    let mut windows = Vec::new();
    if let Some(limits) = utilization.get("limits").and_then(Value::as_array) {
        for limit in limits.iter().filter_map(Value::as_object) {
            let Some(kind) = json_string(limit, "kind") else {
                continue;
            };
            let (window_kind, duration_minutes) = match kind.as_str() {
                "session" => ("session_5h", SESSION_WINDOW_MINUTES),
                "weekly_all" => ("weekly_all", WEEKLY_WINDOW_MINUTES),
                "weekly_scoped" => ("weekly_scoped", WEEKLY_WINDOW_MINUTES),
                _ => continue,
            };
            let (Some(percent), Some(resets_at)) = (
                json_f64(limit.get("percent")),
                json_string(limit, "resets_at")
                    .as_deref()
                    .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok()),
            ) else {
                continue;
            };
            let window_scope = limit
                .get("scope")
                .and_then(Value::as_object)
                .and_then(|scope| scope.get("model"))
                .and_then(Value::as_object)
                .and_then(|model| json_string(model, "display_name"));
            windows.push(BurnWindowSample {
                window_kind: window_kind.to_owned(),
                window_scope,
                duration_minutes,
                percent,
                resets_at,
                severity: json_string(limit, "severity"),
                is_active: limit.get("is_active").and_then(Value::as_bool),
            });
        }
    }
    if windows.is_empty() {
        for (key, kind, duration) in [
            ("five_hour", "session_5h", SESSION_WINDOW_MINUTES),
            ("seven_day", "weekly_all", WEEKLY_WINDOW_MINUTES),
        ] {
            let Some(window) = utilization.get(key).and_then(Value::as_object) else {
                continue;
            };
            let (Some(percent), Some(resets_at)) = (
                json_f64(window.get("utilization")),
                json_string(window, "resets_at")
                    .as_deref()
                    .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok()),
            ) else {
                continue;
            };
            windows.push(BurnWindowSample {
                window_kind: kind.to_owned(),
                window_scope: None,
                duration_minutes: duration,
                percent,
                resets_at,
                severity: None,
                is_active: None,
            });
        }
    }
    Ok(Some(ClaudeCachedUsage {
        account_key: format!("claude:{account_id}"),
        observed_at,
        windows,
    }))
}

fn paired_claude_window(
    kind: &str,
    duration_minutes: i64,
    percent: Option<f64>,
    resets_at: Option<&str>,
) -> Result<Option<BurnWindowSample>> {
    let (Some(percent), Some(resets_at)) = (percent, resets_at) else {
        return Ok(None);
    };
    Ok(Some(BurnWindowSample {
        window_kind: kind.to_owned(),
        window_scope: None,
        duration_minutes,
        percent,
        resets_at: OffsetDateTime::parse(resets_at, &Rfc3339)
            .with_context(|| format!("invalid {kind} reset timestamp"))?,
        severity: None,
        is_active: None,
    }))
}

fn validate_window(window: &BurnWindowSample) -> Result<()> {
    if window.window_kind.trim().is_empty() || window.duration_minutes <= 0 {
        bail!("burn sample window kind and duration must be valid");
    }
    if !window.percent.is_finite() || !(0.0..=100.0).contains(&window.percent) {
        bail!("burn sample percent must be finite and between 0 and 100");
    }
    Ok(())
}

fn is_codex_rate_limit_event(event: &Map<String, Value>) -> bool {
    let event_type = json_string(event, "event_type")
        .or_else(|| json_string(event, "type"))
        .unwrap_or_default();
    matches!(
        event_type.as_str(),
        "account/rateLimits/updated" | "account_rate_limits_updated"
    )
}

fn codex_reset_at(
    update: &Map<String, Value>,
    observed_at: OffsetDateTime,
) -> Option<OffsetDateTime> {
    if let Some(timestamp) = json_i64(update.get("resetsAt").or_else(|| update.get("resets_at"))) {
        return OffsetDateTime::from_unix_timestamp(timestamp).ok();
    }
    json_i64(update.get("resets_in_seconds"))
        .map(|seconds| observed_at + time::Duration::seconds(seconds))
}

fn json_string(value: &Map<String, Value>, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
    })
}

fn json_f64(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| value.as_f64().or_else(|| value.as_i64().map(|v| v as f64)))
}

fn format_timestamp(value: OffsetDateTime) -> Result<String> {
    value
        .to_offset(time::UtcOffset::UTC)
        .format(DB_TIMESTAMP_FORMAT)
        .context("failed to format timestamp")
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::Duration as StdDuration,
    };

    use rusqlite::Connection;
    use serde_json::json;

    use super::*;
    use crate::usage_identity::AccountIdentity;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sm-usage-burn-{label}-{}-{}.db",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn at(value: &str) -> OffsetDateTime {
        OffsetDateTime::parse(value, &Rfc3339).unwrap()
    }

    fn seed_account(db_path: &Path, provider: Provider, external_id: &str, observed_at: &str) {
        let identity = AccountIdentity {
            provider,
            external_id: external_id.to_owned(),
            label: None,
            plan_tier: None,
        };
        UsageIdentityStore::new(db_path)
            .unwrap()
            .record_observation(provider, Some(&identity), at(observed_at), None, None)
            .unwrap();
    }

    #[test]
    fn claude_cache_records_all_windows_at_fetch_time_once() {
        let db_path = temp_db("claude-cache");
        seed_account(
            &db_path,
            Provider::Claude,
            "claude-account",
            "2026-08-10T12:00:00Z",
        );
        let cache_path = db_path.with_extension("claude.json");
        fs::write(
            &cache_path,
            json!({
                "oauthAccount": {"accountUuid": "claude-account"},
                "cachedUsageUtilization": {
                    "fetchedAtMs": 1786365000000_i64,
                    "accountUuid": "claude-account",
                    "utilization": {
                        "limits": [
                            {"kind": "session", "percent": 9, "resets_at": "2026-08-10T20:00:00Z", "severity": "normal", "is_active": true},
                            {"kind": "weekly_all", "percent": 8, "resets_at": "2026-08-16T16:00:00Z", "severity": "warning", "is_active": false},
                            {"kind": "weekly_scoped", "percent": 7, "resets_at": "2026-08-16T16:00:00Z", "scope": {"model": {"display_name": "Fable"}}, "is_active": false}
                        ]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let store = UsageBurnStore::new(&db_path).unwrap();

        assert_eq!(store.record_claude_json_file(&cache_path).unwrap(), 3);
        assert_eq!(store.record_claude_json_file(&cache_path).unwrap(), 0);

        let connection = Connection::open(db_path).unwrap();
        let rows = connection
            .prepare(
                "SELECT window_kind, window_scope, percent, source, observed_at FROM burn_samples ORDER BY window_kind",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, "session_5h");
        assert_eq!(rows[1].0, "weekly_all");
        assert_eq!(rows[2].0, "weekly_scoped");
        assert_eq!(rows[2].1.as_deref(), Some("Fable"));
        assert!(rows.iter().all(|row| row.3 == "claude_json"));
        assert!(rows.iter().all(|row| row.4.starts_with("2026-08-10T")));
    }

    #[test]
    fn statusline_windows_resolve_the_half_open_claude_account() {
        let db_path = temp_db("statusline");
        seed_account(
            &db_path,
            Provider::Claude,
            "claude-account",
            "2026-08-10T12:00:00Z",
        );
        let store = UsageBurnStore::new(&db_path).unwrap();

        assert_eq!(
            store
                .record_claude_statusline(
                    at("2026-08-10T12:01:00Z"),
                    Some(12.0),
                    Some("2026-08-10T17:00:00Z"),
                    Some(34.0),
                    Some("2026-08-16T16:00:00Z"),
                )
                .unwrap(),
            2
        );
        let count: i64 = Connection::open(db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM burn_samples WHERE account_key = 'claude:claude-account' AND source = 'statusline'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn codex_sparse_merge_keys_windows_by_duration_across_slot_swap() {
        let db_path = temp_db("codex-sparse");
        seed_account(
            &db_path,
            Provider::Codex,
            "codex-account",
            "2026-08-10T12:00:00Z",
        );
        let store = UsageBurnStore::new(&db_path).unwrap();
        let first = json!({
            "event_type": "account/rateLimits/updated",
            "ts": "2026-08-10T12:01:00Z",
            "payload": {"rateLimits": {
                "limitId": "codex",
                "primary": {"usedPercent": 10, "windowDurationMins": 300, "resetsAt": 1786381200_i64},
                "secondary": {"usedPercent": 30, "windowDurationMins": 10080, "resetsAt": 1786982400_i64}
            }}
        });
        assert_eq!(
            store
                .record_codex_event(first.as_object().unwrap(), at("2026-08-10T12:01:01Z"))
                .unwrap(),
            2
        );
        let swapped_sparse = json!({
            "event_type": "account/rateLimits/updated",
            "ts": "2026-08-10T12:02:00Z",
            "payload": {"rateLimits": {
                "limitId": "codex",
                "primary": {"usedPercent": 40, "windowDurationMins": 10080},
                "secondary": null
            }}
        });
        assert_eq!(
            store
                .record_codex_event(
                    swapped_sparse.as_object().unwrap(),
                    at("2026-08-10T12:02:01Z"),
                )
                .unwrap(),
            1
        );
        let no_update = json!({
            "event_type": "account/rateLimits/updated",
            "ts": "2026-08-10T12:03:00Z",
            "payload": {"rateLimits": {
                "limitId": "codex",
                "primary": {
                    "usedPercent": null,
                    "windowDurationMins": 10080,
                    "resetsAt": null
                }
            }}
        });
        assert_eq!(
            store
                .record_codex_event(no_update.as_object().unwrap(), at("2026-08-10T12:03:01Z"),)
                .unwrap(),
            0
        );

        let connection = Connection::open(db_path).unwrap();
        let latest: (f64, String) = connection
            .query_row(
                "SELECT percent, resets_at FROM burn_samples WHERE window_kind = 'codex_10080' ORDER BY observed_at DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(latest.0, 40.0);
        assert_eq!(latest.1, "2026-08-17T16:00:00.000000000Z");
    }

    #[test]
    fn codex_sparse_merge_orders_same_second_samples_chronologically() {
        let db_path = temp_db("codex-same-second");
        seed_account(
            &db_path,
            Provider::Codex,
            "codex-account",
            "2026-08-10T12:00:00Z",
        );
        let store = UsageBurnStore::new(&db_path).unwrap();
        for (timestamp, percent) in [
            ("2026-08-10T12:01:00Z", 10),
            ("2026-08-10T12:01:00.900Z", 20),
        ] {
            let event = json!({
                "event_type": "account/rateLimits/updated",
                "ts": timestamp,
                "payload": {"rateLimits": {
                    "limitId": "codex",
                    "primary": {
                        "usedPercent": percent,
                        "windowDurationMins": 300,
                        "resetsAt": 1786381200_i64
                    }
                }}
            });
            assert_eq!(
                store
                    .record_codex_event(event.as_object().unwrap(), at(timestamp))
                    .unwrap(),
                1
            );
        }
        let reset_only = json!({
            "event_type": "account/rateLimits/updated",
            "ts": "2026-08-10T12:01:00.950Z",
            "payload": {"rateLimits": {
                "limitId": "codex",
                "primary": {
                    "windowDurationMins": 300,
                    "resetsAt": 1786384800_i64
                }
            }}
        });
        assert_eq!(
            store
                .record_codex_event(
                    reset_only.as_object().unwrap(),
                    at("2026-08-10T12:01:00.950Z"),
                )
                .unwrap(),
            1
        );

        let latest: (f64, String) = Connection::open(db_path)
            .unwrap()
            .query_row(
                "SELECT percent, observed_at FROM burn_samples ORDER BY observed_at DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(latest.0, 20.0);
        assert_eq!(latest.1, "2026-08-10T12:01:00.950000000Z");
    }

    #[test]
    fn codex_sparse_merge_does_not_inherit_from_future_samples() {
        let db_path = temp_db("codex-out-of-order");
        seed_account(
            &db_path,
            Provider::Codex,
            "codex-account",
            "2026-08-10T12:00:00Z",
        );
        let store = UsageBurnStore::new(&db_path).unwrap();
        for (timestamp, percent, reset) in [
            ("2026-08-10T12:01:00Z", 10, 1786381200_i64),
            ("2026-08-10T12:03:00Z", 30, 1786384800_i64),
        ] {
            let event = json!({
                "event_type": "account/rateLimits/updated",
                "ts": timestamp,
                "payload": {"rateLimits": {
                    "limitId": "codex",
                    "primary": {
                        "usedPercent": percent,
                        "windowDurationMins": 300,
                        "resetsAt": reset
                    }
                }}
            });
            assert_eq!(
                store
                    .record_codex_event(event.as_object().unwrap(), at(timestamp))
                    .unwrap(),
                1
            );
        }
        let delayed = json!({
            "event_type": "account/rateLimits/updated",
            "ts": "2026-08-10T12:02:00Z",
            "payload": {"rateLimits": {
                "limitId": "codex",
                "primary": {
                    "windowDurationMins": 300,
                    "resetsAt": 1786383000_i64
                }
            }}
        });
        assert_eq!(
            store
                .record_codex_event(delayed.as_object().unwrap(), at("2026-08-10T12:04:00Z"))
                .unwrap(),
            1
        );

        let connection = Connection::open(db_path).unwrap();
        let delayed_percent: f64 = connection
            .query_row(
                "SELECT percent FROM burn_samples WHERE observed_at = '2026-08-10T12:02:00.000000000Z'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let latest_percent: f64 = connection
            .query_row(
                "SELECT percent FROM burn_samples ORDER BY observed_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(delayed_percent, 10.0);
        assert_eq!(latest_percent, 30.0);
    }

    #[test]
    fn codex_sparse_merge_waits_for_a_concurrent_writer_before_reading_history() {
        let db_path = temp_db("codex-concurrent");
        seed_account(
            &db_path,
            Provider::Codex,
            "codex-account",
            "2026-08-10T12:00:00Z",
        );
        let store = UsageBurnStore::new(&db_path).unwrap();
        let baseline = json!({
            "event_type": "account/rateLimits/updated",
            "ts": "2026-08-10T12:01:00Z",
            "payload": {"rateLimits": {
                "limitId": "codex",
                "primary": {
                    "usedPercent": 10,
                    "windowDurationMins": 300,
                    "resetsAt": 1786381200_i64
                }
            }}
        });
        store
            .record_codex_event(baseline.as_object().unwrap(), at("2026-08-10T12:01:00Z"))
            .unwrap();

        let mut blocker_connection = store.open().unwrap();
        let blocker = blocker_connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        insert_windows(
            &blocker,
            "codex:codex-account",
            &[BurnWindowSample {
                window_kind: "codex_300".to_owned(),
                window_scope: None,
                duration_minutes: 300,
                percent: 20.0,
                resets_at: OffsetDateTime::from_unix_timestamp(1786381200).unwrap(),
                severity: None,
                is_active: None,
            }],
            "codex_event",
            "2026-08-10T12:02:00.000000000Z",
        )
        .unwrap();

        let reset_only = json!({
            "event_type": "account/rateLimits/updated",
            "ts": "2026-08-10T12:03:00Z",
            "payload": {"rateLimits": {
                "limitId": "codex",
                "primary": {
                    "windowDurationMins": 300,
                    "resetsAt": 1786384800_i64
                }
            }}
        });
        let (sent, received) = mpsc::channel();
        let concurrent_store = store.clone();
        let writer =
            thread::spawn(move || {
                sent.send(concurrent_store.record_codex_event(
                    reset_only.as_object().unwrap(),
                    at("2026-08-10T12:03:00Z"),
                ))
                .unwrap();
            });
        assert!(received.recv_timeout(StdDuration::from_millis(50)).is_err());
        blocker.commit().unwrap();
        assert_eq!(
            received
                .recv_timeout(StdDuration::from_secs(2))
                .unwrap()
                .unwrap(),
            1
        );
        writer.join().unwrap();

        let merged_percent: f64 = Connection::open(db_path)
            .unwrap()
            .query_row(
                "SELECT percent FROM burn_samples WHERE observed_at = '2026-08-10T12:03:00.000000000Z'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(merged_percent, 20.0);
    }

    #[test]
    fn codex_non_default_windows_are_keyed_by_limit_id_not_name() {
        let db_path = temp_db("codex-limit-id");
        seed_account(
            &db_path,
            Provider::Codex,
            "codex-account",
            "2026-08-10T12:00:00Z",
        );
        let store = UsageBurnStore::new(&db_path).unwrap();
        for (timestamp, name, percent, reset) in [
            (
                "2026-08-10T12:01:00Z",
                "Fable",
                Some(10),
                Some(1786381200_i64),
            ),
            ("2026-08-10T12:02:00Z", "Sol", Some(20), None),
        ] {
            let event = json!({
                "event_type": "account/rateLimits/updated",
                "ts": timestamp,
                "payload": {"rateLimits": {
                    "limitId": "premium",
                    "limitName": name,
                    "primary": {
                        "usedPercent": percent,
                        "windowDurationMins": 300,
                        "resetsAt": reset
                    }
                }}
            });
            assert_eq!(
                store
                    .record_codex_event(event.as_object().unwrap(), at(timestamp))
                    .unwrap(),
                1
            );
        }

        let connection = Connection::open(db_path).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM burn_samples WHERE window_kind = 'codex_300'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let latest: (String, f64, String) = connection
            .query_row(
                "SELECT window_scope, percent, resets_at FROM burn_samples WHERE window_kind = 'codex_300' ORDER BY observed_at DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(latest.0, "premium");
        assert_eq!(latest.1, 20.0);
        assert_eq!(latest.2, "2026-08-10T17:00:00.000000000Z");
    }
}
