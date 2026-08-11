use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const DB_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const STALE_AFTER_SECONDS: i64 = 600;
const MIN_PROJECTION_ELAPSED_SECONDS: i64 = 3600;
const DEFAULT_PREMIUM_CAP_RATIO: f64 = 0.5;

#[derive(Debug, Clone, Copy, Default)]
pub struct UsageReportOptions {
    pub since_reset: bool,
    pub by_model: bool,
}

#[derive(Debug, Clone)]
pub struct UsageReportTarget {
    pub seat_id: String,
    pub friendly_name: Option<String>,
    pub account_key: Option<String>,
    pub usage_cap_fraction: Option<f64>,
    pub self_seats: BTreeSet<String>,
    pub child_seats: BTreeSet<String>,
    pub available_descendant_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageReport {
    pub mode: &'static str,
    pub decision: UsageDecisionSummary,
    pub residual: Option<f64>,
    pub possibly_inflated: bool,
    pub generated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<UsageReportTargetView>,
    pub accounts: Vec<UsageAccountReport>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageReportTargetView {
    pub seat_id: String,
    pub friendly_name: Option<String>,
    pub descendant_count: usize,
    pub available_descendant_count: usize,
    pub account_key: Option<String>,
    pub usage_cap_fraction: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageDecisionSummary {
    pub status: &'static str,
    pub fresh_current_windows: usize,
    pub stale_current_windows: usize,
    pub missing_current_windows: Vec<String>,
    pub reasons: Vec<String>,
    pub refresh_guidance: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageAccountReport {
    pub account_key: String,
    pub label: Option<String>,
    pub provider: String,
    pub plan_tier: Option<String>,
    pub active: bool,
    pub windows: Vec<UsageWindowReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageWindowReport {
    pub window_kind: String,
    pub window_scope: Option<String>,
    pub window_start: String,
    pub resets_at: String,
    pub observed_at: String,
    pub account_percent: Option<f64>,
    pub last_known_percent: f64,
    pub sample_age_seconds: Option<i64>,
    pub current: bool,
    pub freshness: &'static str,
    pub stale: bool,
    pub weight_source: &'static str,
    pub residual: Option<f64>,
    pub possibly_inflated: bool,
    pub self_percent: Option<f64>,
    pub children_percent: Option<f64>,
    pub total_percent: Option<f64>,
    pub cap_consumed_percent: Option<f64>,
    pub free_headroom_points: Option<f64>,
    pub account_lower_bound_percent: Option<f64>,
    pub free_headroom_upper_bound_points: Option<f64>,
    pub binding_for_scoped_seats: Option<bool>,
    pub binding_for_other_seats: Option<bool>,
    pub credit_tokens: i64,
    pub residual_lower_bound_percent: f64,
    pub residual_upper_bound_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection: Option<UsageProjection>,
    pub seats: Vec<UsageSeatShare>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<UsageModelShare>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSeatShare {
    pub seat_id: String,
    pub friendly_name: Option<String>,
    pub burn_percent: Option<f64>,
    pub share: Option<f64>,
    pub weighted_units: f64,
    pub total_tokens: i64,
    pub credit_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageProjection {
    pub method: &'static str,
    pub confidence: &'static str,
    pub assumptions: Vec<&'static str>,
    pub elapsed_seconds: i64,
    pub horizon_seconds: i64,
    pub burn_rate_points_per_day: f64,
    pub projected_account_percent_at_reset: f64,
    pub projected_free_headroom_points: f64,
    pub seat_projection_status: &'static str,
    pub additional_seats: Vec<UsageAdditionalSeatProjection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageAdditionalSeatProjection {
    pub model: String,
    pub baseline_seats: usize,
    pub burn_points_per_seat_per_day: f64,
    pub additional_seat_equivalents: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageModelShare {
    pub seat_id: String,
    pub model: String,
    pub burn_percent: Option<f64>,
    pub weighted_units: f64,
    pub weight_source: &'static str,
}

#[derive(Debug, Clone)]
pub struct UsageReportStore {
    db_path: PathBuf,
    account_labels: BTreeMap<String, String>,
    premium_cap_ratio: f64,
}

impl UsageReportStore {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            account_labels: BTreeMap::new(),
            premium_cap_ratio: DEFAULT_PREMIUM_CAP_RATIO,
        }
    }

    pub fn with_premium_cap_ratio(mut self, ratio: f64) -> Self {
        self.premium_cap_ratio = ratio;
        self
    }

    pub fn with_account_labels(
        mut self,
        labels: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.account_labels = labels.into_iter().collect();
        self
    }

    pub fn report(
        &self,
        target: Option<&UsageReportTarget>,
        options: UsageReportOptions,
    ) -> Result<UsageReport> {
        let connection = self.open()?;
        let now = OffsetDateTime::now_utc();
        let account_meta = load_account_metadata(&connection)?;
        let names = load_seat_names(&connection)?;
        let all_windows = load_latest_windows(&connection)?;
        let selected = select_windows(&all_windows, options.since_reset, now);
        let mut warnings = BTreeSet::new();
        let mut accounts = BTreeMap::<String, UsageAccountReport>::new();

        for window in selected {
            let metadata = account_meta
                .get(&window.account_key)
                .cloned()
                .unwrap_or_else(|| AccountMetadata {
                    provider: window
                        .account_key
                        .split_once(':')
                        .map(|(provider, _)| provider)
                        .unwrap_or("unknown")
                        .to_owned(),
                    plan_tier: None,
                    active: false,
                });
            let rows = load_window_rows(&connection, &window)?;
            let premium = premium_context(&window, &all_windows, self.premium_cap_ratio);
            let report = build_window_report(
                &window,
                &metadata.provider,
                &rows,
                premium.as_ref(),
                target,
                &names,
                options.by_model,
                self.premium_cap_ratio,
                now,
                &mut warnings,
            );
            accounts
                .entry(window.account_key.clone())
                .or_insert_with(|| UsageAccountReport {
                    account_key: window.account_key.clone(),
                    label: self.account_labels.get(&window.account_key).cloned(),
                    provider: metadata.provider.clone(),
                    plan_tier: metadata.plan_tier.clone(),
                    active: metadata.active,
                    windows: Vec::new(),
                })
                .windows
                .push(report);
        }

        for (account_key, metadata) in &account_meta {
            if !metadata.active {
                continue;
            }
            accounts
                .entry(account_key.clone())
                .or_insert_with(|| UsageAccountReport {
                    account_key: account_key.clone(),
                    label: self.account_labels.get(account_key).cloned(),
                    provider: metadata.provider.clone(),
                    plan_tier: metadata.plan_tier.clone(),
                    active: true,
                    windows: Vec::new(),
                });
        }

        let mut accounts = accounts.into_values().collect::<Vec<_>>();
        if let Some(target) = target {
            let target_ids = target
                .self_seats
                .union(&target.child_seats)
                .collect::<BTreeSet<_>>();
            accounts.retain(|account| {
                target.account_key.as_deref() == Some(account.account_key.as_str())
                    || account.windows.iter().any(|window| {
                        window
                            .seats
                            .iter()
                            .any(|seat| target_ids.contains(&seat.seat_id))
                    })
            });
        }
        for account in &mut accounts {
            account.windows.sort_by(|left, right| {
                left.window_kind
                    .cmp(&right.window_kind)
                    .then_with(|| right.window_start.cmp(&left.window_start))
            });
            mark_binding_limits(&mut account.windows);
        }
        let decision = usage_decision_summary(&accounts);

        Ok(UsageReport {
            mode: "prior",
            decision,
            residual: None,
            possibly_inflated: true,
            generated_at: now.format(&Rfc3339)?,
            target: target.map(|target| UsageReportTargetView {
                seat_id: target.seat_id.clone(),
                friendly_name: target.friendly_name.clone(),
                descendant_count: target.child_seats.len(),
                available_descendant_count: target.available_descendant_count,
                account_key: target.account_key.clone(),
                usage_cap_fraction: target.usage_cap_fraction,
            }),
            accounts,
            warnings: warnings.into_iter().collect(),
        })
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
        connection.busy_timeout(DB_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Ok(connection)
    }
}

#[derive(Debug, Clone)]
struct AccountMetadata {
    provider: String,
    plan_tier: Option<String>,
    active: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct BurnWindow {
    id: i64,
    account_key: String,
    window_kind: String,
    window_scope: Option<String>,
    window_start: String,
    percent: f64,
    resets_at: String,
    observed_at: String,
}

#[derive(Debug, Clone, Default)]
struct TokenCounts {
    input: i64,
    output: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    cache_read: i64,
}

impl TokenCounts {
    fn total(&self) -> i64 {
        self.input + self.output + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }
}

#[derive(Debug, Clone)]
struct WindowRow {
    seat_id: String,
    model: String,
    credit_metered: bool,
    first_bucket_ts: Option<String>,
    tokens: TokenCounts,
}

#[derive(Debug, Clone, Default)]
struct ModelBurn {
    burn_percent: f64,
    weighted_units: f64,
    first_bucket_ts: Option<String>,
}

#[derive(Debug, Clone)]
struct PremiumContext {
    scope: String,
    points: f64,
}

fn load_account_metadata(connection: &Connection) -> Result<BTreeMap<String, AccountMetadata>> {
    let mut statement = connection.prepare(
        r#"
        SELECT accounts.account_key, accounts.provider, accounts.plan_tier,
               EXISTS (
                 SELECT 1
                 FROM account_timeline
                 WHERE account_timeline.account_key = accounts.account_key
                   AND account_timeline.provider = accounts.provider
                   AND account_timeline.to_ts IS NULL
               )
        FROM accounts
        ORDER BY accounts.account_key
        "#,
    )?;
    let values = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                AccountMetadata {
                    provider: row.get(1)?,
                    plan_tier: row.get(2)?,
                    active: row.get(3)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    Ok(values)
}

fn load_seat_names(connection: &Connection) -> Result<BTreeMap<String, Option<String>>> {
    let mut statement = connection.prepare(
        r#"
        SELECT seat_id, friendly_name
        FROM seat_meta
        ORDER BY seat_id, observed_at DESC
        "#,
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut names = BTreeMap::new();
    for (seat_id, friendly_name) in rows {
        names.entry(seat_id).or_insert(friendly_name);
    }
    names.entry("unassigned".to_owned()).or_insert(None);
    Ok(names)
}

fn load_latest_windows(connection: &Connection) -> Result<Vec<BurnWindow>> {
    let mut statement = connection.prepare(
        r#"
        SELECT id, account_key, window_kind, window_scope, window_start,
               percent, resets_at, observed_at
        FROM burn_samples
        ORDER BY account_key, window_kind, window_scope, window_start,
                 observed_at DESC, id DESC
        "#,
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok(BurnWindow {
                id: row.get(0)?,
                account_key: row.get(1)?,
                window_kind: row.get(2)?,
                window_scope: row.get(3)?,
                window_start: row.get(4)?,
                percent: row.get(5)?,
                resets_at: row.get(6)?,
                observed_at: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut latest = BTreeMap::new();
    for row in rows {
        let key = (
            row.account_key.clone(),
            row.window_kind.clone(),
            row.window_scope.clone(),
            row.window_start.clone(),
        );
        latest.entry(key).or_insert(row);
    }
    Ok(latest.into_values().collect())
}

fn select_windows(
    windows: &[BurnWindow],
    since_reset: bool,
    now: OffsetDateTime,
) -> Vec<BurnWindow> {
    let mut grouped = BTreeMap::<(String, String, Option<String>), Vec<BurnWindow>>::new();
    for window in windows {
        grouped
            .entry((
                window.account_key.clone(),
                window.window_kind.clone(),
                window.window_scope.clone(),
            ))
            .or_default()
            .push(window.clone());
    }
    let mut selected = Vec::new();
    for group in grouped.into_values() {
        let mut current = group
            .iter()
            .filter(|window| parse_timestamp(&window.resets_at).is_some_and(|reset| reset > now))
            .cloned()
            .collect::<Vec<_>>();
        current.sort_by(|left, right| {
            right
                .observed_at
                .cmp(&left.observed_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        if let Some(window) = current.into_iter().next() {
            selected.push(window);
        } else if since_reset {
            let mut latest = group.clone();
            latest.sort_by(|left, right| {
                right
                    .observed_at
                    .cmp(&left.observed_at)
                    .then_with(|| right.id.cmp(&left.id))
            });
            if let Some(window) = latest.into_iter().next() {
                selected.push(window);
            }
        }
        if !since_reset {
            let mut closed = group
                .into_iter()
                .filter(|window| {
                    parse_timestamp(&window.resets_at).is_none_or(|reset| reset <= now)
                })
                .collect::<Vec<_>>();
            closed.sort_by(|left, right| {
                right
                    .window_start
                    .cmp(&left.window_start)
                    .then_with(|| right.observed_at.cmp(&left.observed_at))
                    .then_with(|| right.id.cmp(&left.id))
            });
            closed.truncate(3);
            selected.extend(closed);
        }
    }
    selected
}

fn load_window_rows(connection: &Connection, window: &BurnWindow) -> Result<Vec<WindowRow>> {
    let mut statement = connection.prepare(
        r#"
        SELECT seat_id, model, credit_metered,
               MIN(bucket_ts), SUM(input_tokens), SUM(output_tokens), SUM(cache_write_5m),
               SUM(cache_write_1h), SUM(cache_read_tokens)
        FROM seat_tokens
        WHERE account_key = ?1 AND window_kind = ?2 AND window_start = ?3
        GROUP BY seat_id, model, credit_metered
        ORDER BY seat_id, model, credit_metered
        "#,
    )?;
    let rows = statement
        .query_map(
            params![window.account_key, window.window_kind, window.window_start],
            |row| {
                Ok(WindowRow {
                    seat_id: row.get(0)?,
                    model: row.get(1)?,
                    credit_metered: row.get(2)?,
                    first_bucket_ts: row.get(3)?,
                    tokens: TokenCounts {
                        input: row.get(4)?,
                        output: row.get(5)?,
                        cache_write_5m: row.get(6)?,
                        cache_write_1h: row.get(7)?,
                        cache_read: row.get(8)?,
                    },
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn premium_context(
    window: &BurnWindow,
    windows: &[BurnWindow],
    premium_cap_ratio: f64,
) -> Option<PremiumContext> {
    if window.window_kind == "weekly_scoped" {
        return window.window_scope.clone().map(|scope| PremiumContext {
            scope,
            points: window.percent,
        });
    }
    if window.window_kind != "weekly_all" {
        return None;
    }
    windows
        .iter()
        .filter(|candidate| {
            candidate.account_key == window.account_key
                && candidate.window_kind == "weekly_scoped"
                && candidate.window_start == window.window_start
        })
        .max_by(|left, right| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| left.id.cmp(&right.id))
        })
        .and_then(|scoped| {
            scoped.window_scope.clone().map(|scope| PremiumContext {
                scope,
                points: scoped.percent * premium_cap_ratio,
            })
        })
}

#[allow(clippy::too_many_arguments)]
fn build_window_report(
    window: &BurnWindow,
    provider: &str,
    rows: &[WindowRow],
    premium: Option<&PremiumContext>,
    target: Option<&UsageReportTarget>,
    names: &BTreeMap<String, Option<String>>,
    by_model: bool,
    premium_cap_ratio: f64,
    now: OffsetDateTime,
    warnings: &mut BTreeSet<String>,
) -> UsageWindowReport {
    let sample_age_seconds = parse_timestamp(&window.observed_at)
        .map(|observed_at| (now - observed_at).whole_seconds().max(0));
    let reset_elapsed = parse_timestamp(&window.resets_at).is_some_and(|reset| reset <= now);
    let stale = reset_elapsed
        || sample_age_seconds
            .map(|age| age > STALE_AFTER_SECONDS)
            .unwrap_or(true);
    let mut weighted = Vec::new();
    let mut credit_by_seat = BTreeMap::<String, i64>::new();
    let mut tokens_by_seat = BTreeMap::<String, i64>::new();
    for row in rows {
        *tokens_by_seat.entry(row.seat_id.clone()).or_default() += row.tokens.total();
        if row.credit_metered {
            *credit_by_seat.entry(row.seat_id.clone()).or_default() += row.tokens.total();
            continue;
        }
        let (units, known) = weighted_units(provider, &row.model, &row.tokens);
        if !known {
            warnings.insert(format!(
                "No price prior for model {}; uniform token weights used",
                row.model
            ));
        }
        weighted.push((row, units));
    }

    let is_scoped = window.window_kind == "weekly_scoped";
    let premium_units = weighted
        .iter()
        .filter(|(row, _)| {
            premium.is_some_and(|premium| model_matches_scope(&row.model, &premium.scope))
        })
        .map(|(_, units)| *units)
        .sum::<f64>();
    let rest_units = if is_scoped {
        0.0
    } else {
        weighted
            .iter()
            .filter(|(row, _)| {
                !premium.is_some_and(|premium| model_matches_scope(&row.model, &premium.scope))
            })
            .map(|(_, units)| *units)
            .sum::<f64>()
    };
    let premium_points = premium.map(|premium| premium.points).unwrap_or(0.0);
    let rest_points = if is_scoped {
        0.0
    } else {
        (window.percent - premium_points).max(0.0)
    };

    let mut burn_by_seat = BTreeMap::<String, f64>::new();
    let mut units_by_seat = BTreeMap::<String, f64>::new();
    let mut burn_by_model = BTreeMap::<(String, String), ModelBurn>::new();
    for (row, units) in weighted {
        let premium_row =
            premium.is_some_and(|premium| model_matches_scope(&row.model, &premium.scope));
        let burn = if premium_row && premium_units > 0.0 {
            premium_points * units / premium_units
        } else if !premium_row && rest_units > 0.0 {
            rest_points * units / rest_units
        } else {
            0.0
        };
        *burn_by_seat.entry(row.seat_id.clone()).or_default() += burn;
        *units_by_seat.entry(row.seat_id.clone()).or_default() += units;
        let model = burn_by_model
            .entry((row.seat_id.clone(), row.model.clone()))
            .or_default();
        model.burn_percent += burn;
        model.weighted_units += units;
        if model.first_bucket_ts.as_ref().is_none_or(|current| {
            row.first_bucket_ts
                .as_ref()
                .is_some_and(|candidate| candidate < current)
        }) {
            model.first_bucket_ts = row.first_bucket_ts.clone();
        }
    }
    let unassigned_points = (premium_units == 0.0)
        .then_some(premium_points)
        .unwrap_or(0.0)
        + (rest_units == 0.0).then_some(rest_points).unwrap_or(0.0);
    if unassigned_points > 0.0 {
        *burn_by_seat.entry("unassigned".to_owned()).or_default() += unassigned_points;
        burn_by_model
            .entry(("unassigned".to_owned(), "unassigned".to_owned()))
            .or_default()
            .burn_percent += unassigned_points;
    }

    let mut seat_ids = burn_by_seat.keys().cloned().collect::<BTreeSet<_>>();
    seat_ids.extend(credit_by_seat.keys().cloned());
    seat_ids.extend(tokens_by_seat.keys().cloned());
    if let Some(target) = target {
        seat_ids.retain(|seat_id| {
            target.self_seats.contains(seat_id) || target.child_seats.contains(seat_id)
        });
    }
    let mut seats = seat_ids
        .into_iter()
        .map(|seat_id| {
            let burn_percent = burn_by_seat.get(&seat_id).copied().unwrap_or(0.0);
            UsageSeatShare {
                friendly_name: names.get(&seat_id).cloned().flatten(),
                weighted_units: units_by_seat.get(&seat_id).copied().unwrap_or(0.0),
                total_tokens: tokens_by_seat.get(&seat_id).copied().unwrap_or(0),
                credit_tokens: credit_by_seat.get(&seat_id).copied().unwrap_or(0),
                share: (!stale).then_some(if window.percent > 0.0 {
                    burn_percent / window.percent
                } else {
                    0.0
                }),
                burn_percent: (!stale).then_some(burn_percent),
                seat_id,
            }
        })
        .collect::<Vec<_>>();
    seats.sort_by(|left, right| {
        right
            .burn_percent
            .unwrap_or(0.0)
            .total_cmp(&left.burn_percent.unwrap_or(0.0))
            .then_with(|| left.seat_id.cmp(&right.seat_id))
    });

    let projection = build_projection(
        window,
        stale,
        is_scoped,
        premium_cap_ratio,
        if window.window_kind == "weekly_all" {
            premium.map(|premium| premium.scope.as_str())
        } else {
            None
        },
        &burn_by_model,
    );
    let models = if by_model {
        burn_by_model
            .into_iter()
            .filter(|((seat_id, _), _)| {
                target.is_none_or(|target| {
                    target.self_seats.contains(seat_id) || target.child_seats.contains(seat_id)
                })
            })
            .map(|((seat_id, model), burn)| {
                let weight_source =
                    if premium.is_some_and(|premium| model_matches_scope(&model, &premium.scope)) {
                        "assumed-ratio"
                    } else {
                        "prior"
                    };
                UsageModelShare {
                    seat_id,
                    model,
                    burn_percent: (!stale).then_some(burn.burn_percent),
                    weighted_units: burn.weighted_units,
                    weight_source,
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let (self_percent, children_percent) = target.map_or((0.0, 0.0), |target| {
        (
            burn_for(&burn_by_seat, &target.self_seats),
            burn_for(&burn_by_seat, &target.child_seats),
        )
    });
    let total_percent = if target.is_some() {
        self_percent + children_percent
    } else {
        burn_by_seat.values().sum()
    };
    let cap_consumed_percent = if stale {
        None
    } else {
        target
            .filter(|target| target.account_key.as_deref() == Some(window.account_key.as_str()))
            .and_then(|target| target.usage_cap_fraction)
            .filter(|fraction| *fraction > 0.0)
            .filter(|_| {
                window.window_kind == "weekly_all"
                    || window.window_kind == "codex_10080"
                    || window.window_kind.contains("10080")
            })
            .map(|fraction| total_percent / fraction)
    };

    UsageWindowReport {
        window_kind: window.window_kind.clone(),
        window_scope: window.window_scope.clone(),
        window_start: window.window_start.clone(),
        resets_at: window.resets_at.clone(),
        observed_at: window.observed_at.clone(),
        account_percent: (!stale).then_some(window.percent),
        last_known_percent: window.percent,
        sample_age_seconds,
        current: !reset_elapsed,
        freshness: if reset_elapsed {
            "expired"
        } else if stale {
            "stale"
        } else {
            "fresh"
        },
        stale,
        weight_source: "prior",
        residual: None,
        possibly_inflated: true,
        self_percent: (!stale).then_some(self_percent),
        children_percent: (!stale).then_some(children_percent),
        total_percent: (!stale).then_some(total_percent),
        cap_consumed_percent,
        free_headroom_points: (!stale).then_some(if is_scoped {
            premium_cap_ratio * (100.0 - window.percent).max(0.0)
        } else {
            (100.0 - window.percent).max(0.0)
        }),
        account_lower_bound_percent: (!reset_elapsed).then_some(window.percent),
        free_headroom_upper_bound_points: (!reset_elapsed).then_some(if is_scoped {
            premium_cap_ratio * (100.0 - window.percent).max(0.0)
        } else {
            (100.0 - window.percent).max(0.0)
        }),
        binding_for_scoped_seats: (!stale).then_some(false),
        binding_for_other_seats: (!stale).then_some(false),
        credit_tokens: credit_by_seat.values().sum(),
        residual_lower_bound_percent: 0.0,
        residual_upper_bound_percent: window.percent.max(0.0),
        projection,
        seats,
        models,
    }
}

fn build_projection(
    window: &BurnWindow,
    stale: bool,
    is_scoped: bool,
    premium_cap_ratio: f64,
    excluded_model_scope: Option<&str>,
    burn_by_model: &BTreeMap<(String, String), ModelBurn>,
) -> Option<UsageProjection> {
    let weekly = window.window_kind == "weekly_all"
        || window.window_kind == "weekly_scoped"
        || window.window_kind.contains("10080");
    if stale || !weekly {
        return None;
    }
    let started_at = parse_timestamp(&window.window_start)?;
    let observed_at = parse_timestamp(&window.observed_at)?;
    let resets_at = parse_timestamp(&window.resets_at)?;
    let elapsed_seconds = (observed_at - started_at).whole_seconds();
    let horizon_seconds = (resets_at - observed_at).whole_seconds();
    if elapsed_seconds < MIN_PROJECTION_ELAPSED_SECONDS || horizon_seconds <= 0 {
        return None;
    }

    let burn_rate_per_second = window.percent.max(0.0) / elapsed_seconds as f64;
    let projected_account_percent_at_reset =
        window.percent + burn_rate_per_second * horizon_seconds as f64;
    let projected_raw_headroom = (100.0 - projected_account_percent_at_reset).max(0.0);
    let projected_free_headroom_points = if is_scoped {
        premium_cap_ratio * projected_raw_headroom
    } else {
        projected_raw_headroom
    };

    let mut model_seats = BTreeMap::<String, BTreeMap<String, (f64, String)>>::new();
    for ((seat_id, model), burn) in burn_by_model {
        if seat_id == "unassigned"
            || burn.burn_percent <= 0.0
            || excluded_model_scope.is_some_and(|scope| model_matches_scope(model, scope))
        {
            continue;
        }
        let Some(first_bucket_ts) = burn.first_bucket_ts.clone() else {
            continue;
        };
        model_seats
            .entry(model.clone())
            .or_default()
            .insert(seat_id.clone(), (burn.burn_percent, first_bucket_ts));
    }
    let additional_seats = model_seats
        .into_iter()
        .filter_map(|(model, seats)| {
            let eligible_rates = seats
                .values()
                .filter_map(|(burn_percent, first_bucket_ts)| {
                    let first_bucket_at = parse_timestamp(first_bucket_ts)?;
                    let active_started_at = first_bucket_at.max(started_at);
                    let active_seconds = (observed_at - active_started_at).whole_seconds();
                    (active_seconds >= MIN_PROJECTION_ELAPSED_SECONDS)
                        .then_some(*burn_percent / active_seconds as f64)
                })
                .collect::<Vec<_>>();
            let baseline_seats = eligible_rates.len();
            let burn_per_second = eligible_rates.into_iter().max_by(f64::total_cmp)?;
            let candidate_remaining_burn = burn_per_second * horizon_seconds as f64;
            (candidate_remaining_burn > 0.0).then_some(UsageAdditionalSeatProjection {
                model,
                baseline_seats,
                burn_points_per_seat_per_day: burn_per_second * 86_400.0,
                additional_seat_equivalents: projected_raw_headroom / candidate_remaining_burn,
            })
        })
        .collect::<Vec<UsageAdditionalSeatProjection>>();

    Some(UsageProjection {
        method: "linear_current_window_conservative_model_max",
        confidence: "low_conservative",
        assumptions: vec![
            "current-window account burn continues linearly until reset",
            "candidate capacity uses the highest same-model seat rate over each seat's active interval after one hour",
            "prior attribution may include invisible usage and biases capacity downward",
        ],
        elapsed_seconds,
        horizon_seconds,
        burn_rate_points_per_day: burn_rate_per_second * 86_400.0,
        projected_account_percent_at_reset,
        projected_free_headroom_points,
        seat_projection_status: if additional_seats.is_empty() {
            "no_observed_model_baseline"
        } else {
            "available"
        },
        additional_seats,
    })
}

fn mark_binding_limits(windows: &mut [UsageWindowReport]) {
    let overall = windows
        .iter()
        .enumerate()
        .filter(|(_, window)| !window.stale && window.window_kind == "weekly_all")
        .map(|(index, window)| (index, window.window_start.clone()))
        .collect::<Vec<_>>();
    for (overall_index, window_start) in overall {
        windows[overall_index].binding_for_other_seats = Some(true);
        let scoped_index = windows.iter().position(|window| {
            !window.stale
                && window.window_kind == "weekly_scoped"
                && window.window_start == window_start
        });
        match scoped_index {
            Some(scoped_index)
                if windows[scoped_index].free_headroom_points
                    <= windows[overall_index].free_headroom_points =>
            {
                windows[scoped_index].binding_for_scoped_seats = Some(true);
            }
            _ => windows[overall_index].binding_for_scoped_seats = Some(true),
        }
    }
    for index in 0..windows.len() {
        if !windows[index].stale && windows[index].window_kind == "weekly_scoped" {
            let has_overall = windows.iter().any(|window| {
                !window.stale
                    && window.window_kind == "weekly_all"
                    && window.window_start == windows[index].window_start
            });
            if !has_overall {
                windows[index].binding_for_scoped_seats = Some(true);
            }
        }
    }
}

fn usage_decision_summary(accounts: &[UsageAccountReport]) -> UsageDecisionSummary {
    let fresh_current_windows = accounts
        .iter()
        .filter(|account| account.active)
        .flat_map(|account| &account.windows)
        .filter(|window| window.current && !window.stale)
        .count();
    let stale_current_windows = accounts
        .iter()
        .filter(|account| account.active)
        .flat_map(|account| &account.windows)
        .filter(|window| window.current && window.stale)
        .count();
    let mut missing_current_windows = Vec::new();
    let mut accounts_needing_refresh = BTreeSet::new();
    for account in accounts {
        if !account.active {
            continue;
        }
        let mut required = match account.provider.as_str() {
            "claude" => vec![("session_5h", false), ("weekly_all", false)],
            "codex" => vec![("codex_300", false), ("codex_10080", false)],
            _ => Vec::new(),
        };
        let scoped_expected = account.provider == "claude"
            && (account
                .plan_tier
                .as_deref()
                .is_some_and(|tier| tier.to_ascii_lowercase().contains("max"))
                || account
                    .windows
                    .iter()
                    .any(|window| window.window_kind == "weekly_scoped"));
        if scoped_expected {
            required.push(("weekly_scoped", true));
        }
        for (kind, scoped) in required {
            let present = account.windows.iter().any(|window| {
                window.current
                    && window.window_kind == kind
                    && (window.window_scope.is_some() == scoped)
            });
            if !present {
                missing_current_windows.push(format!("{}:{kind}", account.account_key));
                accounts_needing_refresh.insert(account.account_key.clone());
            }
        }
        if account
            .windows
            .iter()
            .any(|window| window.current && window.stale)
        {
            accounts_needing_refresh.insert(account.account_key.clone());
        }
    }
    let status = if !missing_current_windows.is_empty() || fresh_current_windows == 0 {
        "non_actionable"
    } else if stale_current_windows == 0 {
        "actionable"
    } else {
        "partial"
    };
    let mut reasons = Vec::new();
    if fresh_current_windows == 0 && stale_current_windows == 0 {
        reasons.push("No current quota window is available".to_owned());
    } else if stale_current_windows > 0 {
        reasons.push(format!(
            "{stale_current_windows} current quota meter(s) are older than {} minutes; only bounds are safe and projections are suppressed",
            STALE_AFTER_SECONDS / 60
        ));
    }
    if fresh_current_windows == 0 && stale_current_windows > 0 {
        reasons.push("No fresh quota meter is available for a spawn decision".to_owned());
    }
    if !missing_current_windows.is_empty() {
        reasons.push(format!(
            "Missing current limiting meter(s): {}",
            missing_current_windows.join(", ")
        ));
    }

    let mut refresh_guidance = BTreeSet::new();
    for account in accounts {
        let needs_refresh = accounts_needing_refresh.contains(&account.account_key);
        if !needs_refresh {
            continue;
        }
        refresh_guidance.insert(match account.provider.as_str() {
            "claude" => "Claude refreshes when its next status-line payload includes rate limits; sm usage cannot force an account refresh".to_owned(),
            "codex" => "Codex refreshes when an attached provider emits its next rate-limit event; sm usage cannot force an account refresh".to_owned(),
            provider => format!(
                "{provider} has no on-demand usage refresh path in Session Manager"
            ),
        });
    }

    UsageDecisionSummary {
        status,
        fresh_current_windows,
        stale_current_windows,
        missing_current_windows,
        reasons,
        refresh_guidance: refresh_guidance.into_iter().collect(),
    }
}

fn burn_for(values: &BTreeMap<String, f64>, seats: &BTreeSet<String>) -> f64 {
    seats
        .iter()
        .filter_map(|seat| values.get(seat))
        .copied()
        .sum()
}

fn weighted_units(provider: &str, model: &str, tokens: &TokenCounts) -> (f64, bool) {
    let model = model.to_ascii_lowercase();
    let rates = if provider == "claude" {
        if model.contains("fable") {
            Some((10.0, 50.0, 12.5, 20.0, 1.0))
        } else if model.contains("opus") {
            Some((5.0, 25.0, 6.25, 10.0, 0.5))
        } else if model.contains("sonnet") {
            Some((3.0, 15.0, 3.75, 6.0, 0.3))
        } else if model.contains("haiku") {
            Some((1.0, 5.0, 1.25, 2.0, 0.1))
        } else {
            None
        }
    } else if model == "gpt-5.6-sol" || model == "gpt-5.5" {
        Some((5.0, 30.0, 5.0, 5.0, 0.5))
    } else if model.contains("gpt-5.6-terra") {
        Some((2.0, 12.0, 2.0, 2.0, 0.25))
    } else if model.contains("gpt-5.6-luna") {
        Some((0.2, 1.2, 0.2, 0.2, 0.1))
    } else if model == "gpt-5.4" {
        Some((2.5, 15.0, 2.5, 2.5, 0.25))
    } else if model == "gpt-5.4-mini" {
        Some((0.75, 4.5, 0.75, 0.75, 0.075))
    } else {
        None
    };
    let known = rates.is_some();
    let rates = rates.unwrap_or((1.0, 1.0, 1.0, 1.0, 1.0));
    (
        tokens.input as f64 * rates.0
            + tokens.output as f64 * rates.1
            + tokens.cache_write_5m as f64 * rates.2
            + tokens.cache_write_1h as f64 * rates.3
            + tokens.cache_read as f64 * rates.4,
        known,
    )
}

fn model_matches_scope(model: &str, scope: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let scope = scope.to_ascii_lowercase();
    model == scope || model.contains(&scope) || scope.contains(&model)
}

fn parse_timestamp(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_weighting_uses_provider_bucket_prices_and_uniform_unknown_fallback() {
        let tokens = TokenCounts {
            input: 10,
            output: 2,
            cache_write_5m: 3,
            cache_write_1h: 4,
            cache_read: 5,
        };
        let (known, present) = weighted_units("claude", "claude-fable-5", &tokens);
        assert!(present);
        assert_eq!(known, 322.5);
        let (unknown, present) = weighted_units("claude", "future-model", &tokens);
        assert!(!present);
        assert_eq!(unknown, 24.0);
        let (sol_wm, present) = weighted_units("codex", "gpt-5.6-sol-wm", &tokens);
        assert!(!present);
        assert_eq!(sol_wm, 24.0);
    }

    #[test]
    fn prior_share_arithmetic_sums_to_the_measured_account_burn() {
        let window = BurnWindow {
            id: 1,
            account_key: "claude:a".to_owned(),
            window_kind: "weekly_all".to_owned(),
            window_scope: None,
            window_start: "2026-08-10T00:00:00Z".to_owned(),
            percent: 20.0,
            resets_at: "2026-08-17T00:00:00Z".to_owned(),
            observed_at: "2026-08-10T16:00:00Z".to_owned(),
        };
        let rows = vec![
            WindowRow {
                seat_id: "seat-a".to_owned(),
                model: "claude-fable-5".to_owned(),
                credit_metered: false,
                first_bucket_ts: None,
                tokens: TokenCounts {
                    input: 10,
                    ..TokenCounts::default()
                },
            },
            WindowRow {
                seat_id: "seat-b".to_owned(),
                model: "claude-sonnet-5".to_owned(),
                credit_metered: false,
                first_bucket_ts: None,
                tokens: TokenCounts {
                    input: 10,
                    ..TokenCounts::default()
                },
            },
        ];
        let premium = PremiumContext {
            scope: "Fable".to_owned(),
            points: 5.0,
        };
        let mut warnings = BTreeSet::new();
        let report = build_window_report(
            &window,
            "claude",
            &rows,
            Some(&premium),
            None,
            &BTreeMap::new(),
            true,
            DEFAULT_PREMIUM_CAP_RATIO,
            OffsetDateTime::parse("2026-08-10T16:00:00Z", &Rfc3339).unwrap(),
            &mut warnings,
        );
        assert_eq!(report.seats.len(), 2);
        assert!(
            (report
                .seats
                .iter()
                .filter_map(|seat| seat.burn_percent)
                .sum::<f64>()
                - 20.0)
                .abs()
                < 1e-9
        );
        assert!(
            (report
                .seats
                .iter()
                .filter_map(|seat| seat.share)
                .sum::<f64>()
                - 1.0)
                .abs()
                < 1e-9
        );
        assert!(report.residual.is_none());
        assert!(report.possibly_inflated);
        assert_eq!(report.models[0].weight_source, "assumed-ratio");
    }

    #[test]
    fn credit_metered_tokens_are_reported_without_rejoining_quota_shares() {
        let window = BurnWindow {
            id: 1,
            account_key: "claude:a".to_owned(),
            window_kind: "session_5h".to_owned(),
            window_scope: None,
            window_start: "2026-08-10T15:00:00Z".to_owned(),
            percent: 25.0,
            resets_at: "2026-08-10T20:00:00Z".to_owned(),
            observed_at: "2026-08-10T16:00:00Z".to_owned(),
        };
        let rows = vec![
            WindowRow {
                seat_id: "quota-seat".to_owned(),
                model: "claude-sonnet-5".to_owned(),
                credit_metered: false,
                first_bucket_ts: None,
                tokens: TokenCounts {
                    input: 100,
                    ..TokenCounts::default()
                },
            },
            WindowRow {
                seat_id: "paid-seat".to_owned(),
                model: "claude-fable-5".to_owned(),
                credit_metered: true,
                first_bucket_ts: None,
                tokens: TokenCounts {
                    input: 1_000,
                    ..TokenCounts::default()
                },
            },
        ];
        let mut warnings = BTreeSet::new();
        let report = build_window_report(
            &window,
            "claude",
            &rows,
            None,
            None,
            &BTreeMap::new(),
            false,
            DEFAULT_PREMIUM_CAP_RATIO,
            OffsetDateTime::parse("2026-08-10T16:00:00Z", &Rfc3339).unwrap(),
            &mut warnings,
        );

        assert_eq!(report.credit_tokens, 1_000);
        assert_eq!(report.seats.len(), 2);
        assert_eq!(report.seats[0].seat_id, "quota-seat");
        assert_eq!(report.seats[0].burn_percent, Some(25.0));
        let paid = report
            .seats
            .iter()
            .find(|seat| seat.seat_id == "paid-seat")
            .unwrap();
        assert_eq!(paid.burn_percent, Some(0.0));
        assert_eq!(paid.credit_tokens, 1_000);
    }

    #[test]
    fn empty_attribution_bucket_is_assigned_to_unassigned() {
        let window = BurnWindow {
            id: 1,
            account_key: "claude:a".to_owned(),
            window_kind: "weekly_all".to_owned(),
            window_scope: None,
            window_start: "2026-08-10T00:00:00Z".to_owned(),
            percent: 40.0,
            resets_at: "2026-08-17T00:00:00Z".to_owned(),
            observed_at: "2026-08-10T16:00:00Z".to_owned(),
        };
        let rows = vec![WindowRow {
            seat_id: "seat-a".to_owned(),
            model: "claude-sonnet-5".to_owned(),
            credit_metered: false,
            first_bucket_ts: None,
            tokens: TokenCounts {
                input: 10,
                ..TokenCounts::default()
            },
        }];
        let premium = PremiumContext {
            scope: "Fable".to_owned(),
            points: 10.0,
        };
        let mut warnings = BTreeSet::new();
        let report = build_window_report(
            &window,
            "claude",
            &rows,
            Some(&premium),
            None,
            &BTreeMap::new(),
            true,
            DEFAULT_PREMIUM_CAP_RATIO,
            OffsetDateTime::parse("2026-08-10T16:00:00Z", &Rfc3339).unwrap(),
            &mut warnings,
        );

        let unassigned = report
            .seats
            .iter()
            .find(|seat| seat.seat_id == "unassigned")
            .unwrap();
        assert_eq!(unassigned.burn_percent, Some(10.0));
        assert_eq!(unassigned.share, Some(0.25));
        assert_eq!(report.total_percent, Some(40.0));
        assert_eq!(
            report
                .seats
                .iter()
                .filter_map(|seat| seat.burn_percent)
                .sum::<f64>(),
            40.0
        );
        assert!(report
            .models
            .iter()
            .any(|model| { model.seat_id == "unassigned" && model.burn_percent == Some(10.0) }));
    }

    #[test]
    fn scoped_weekly_headroom_is_compared_in_pool_units_for_binding() {
        let window = |kind: &str, scope: Option<&str>, headroom: f64| UsageWindowReport {
            window_kind: kind.to_owned(),
            window_scope: scope.map(ToOwned::to_owned),
            window_start: "2026-08-10T00:00:00Z".to_owned(),
            resets_at: "2026-08-17T00:00:00Z".to_owned(),
            observed_at: "2026-08-10T16:00:00Z".to_owned(),
            account_percent: Some(0.0),
            last_known_percent: 0.0,
            sample_age_seconds: Some(0),
            current: true,
            freshness: "fresh",
            stale: false,
            weight_source: "prior",
            residual: None,
            possibly_inflated: true,
            self_percent: Some(0.0),
            children_percent: Some(0.0),
            total_percent: Some(0.0),
            cap_consumed_percent: None,
            free_headroom_points: Some(headroom),
            account_lower_bound_percent: Some(0.0),
            free_headroom_upper_bound_points: Some(headroom),
            binding_for_scoped_seats: Some(false),
            binding_for_other_seats: Some(false),
            credit_tokens: 0,
            residual_lower_bound_percent: 0.0,
            residual_upper_bound_percent: 0.0,
            projection: None,
            seats: Vec::new(),
            models: Vec::new(),
        };
        let mut windows = vec![
            window("weekly_all", None, 92.0),
            window("weekly_scoped", Some("Fable"), 46.5),
        ];

        mark_binding_limits(&mut windows);

        assert_eq!(windows[0].binding_for_other_seats, Some(true));
        assert_eq!(windows[0].binding_for_scoped_seats, Some(false));
        assert_eq!(windows[1].binding_for_scoped_seats, Some(true));
        assert_eq!(windows[1].binding_for_other_seats, Some(false));
    }

    #[test]
    fn stale_samples_expose_last_known_value_without_a_current_percentage() {
        let window = BurnWindow {
            id: 1,
            account_key: "claude:a".to_owned(),
            window_kind: "session_5h".to_owned(),
            window_scope: None,
            window_start: "2026-08-10T15:00:00Z".to_owned(),
            percent: 34.0,
            resets_at: "2026-08-10T20:00:00Z".to_owned(),
            observed_at: "2026-08-10T16:00:00Z".to_owned(),
        };
        let now = OffsetDateTime::parse("2026-08-10T16:11:00Z", &Rfc3339).unwrap();
        let mut warnings = BTreeSet::new();
        let rows = vec![WindowRow {
            seat_id: "seat-a".to_owned(),
            model: "claude-sonnet-5".to_owned(),
            credit_metered: false,
            first_bucket_ts: None,
            tokens: TokenCounts {
                input: 10,
                ..TokenCounts::default()
            },
        }];
        let target = UsageReportTarget {
            seat_id: "seat-a".to_owned(),
            friendly_name: None,
            account_key: Some("claude:a".to_owned()),
            usage_cap_fraction: Some(0.5),
            self_seats: BTreeSet::from(["seat-a".to_owned()]),
            child_seats: BTreeSet::new(),
            available_descendant_count: 0,
        };
        let report = build_window_report(
            &window,
            "claude",
            &rows,
            None,
            Some(&target),
            &BTreeMap::new(),
            true,
            DEFAULT_PREMIUM_CAP_RATIO,
            now,
            &mut warnings,
        );

        assert!(report.stale);
        assert_eq!(report.account_percent, None);
        assert_eq!(report.last_known_percent, 34.0);
        assert_eq!(report.sample_age_seconds, Some(660));
        assert!(report.current);
        assert_eq!(report.freshness, "stale");
        assert_eq!(report.account_lower_bound_percent, Some(34.0));
        assert_eq!(report.free_headroom_upper_bound_points, Some(66.0));
        assert_eq!(report.residual_lower_bound_percent, 0.0);
        assert_eq!(report.residual_upper_bound_percent, 34.0);
        assert!(report.projection.is_none());
        assert_eq!(report.self_percent, None);
        assert_eq!(report.children_percent, None);
        assert_eq!(report.total_percent, None);
        assert_eq!(report.cap_consumed_percent, None);
        assert_eq!(report.free_headroom_points, None);
        assert_eq!(report.binding_for_scoped_seats, None);
        assert_eq!(report.binding_for_other_seats, None);
        assert_eq!(report.seats[0].burn_percent, None);
        assert_eq!(report.seats[0].share, None);
        assert_eq!(report.seats[0].total_tokens, 10);
        assert_eq!(report.models[0].burn_percent, None);
    }

    #[test]
    fn expired_windows_are_stale_even_when_the_sample_is_recent() {
        let window = BurnWindow {
            id: 1,
            account_key: "codex:a".to_owned(),
            window_kind: "weekly_all".to_owned(),
            window_scope: None,
            window_start: "2026-08-04T16:00:00Z".to_owned(),
            percent: 21.0,
            resets_at: "2026-08-11T15:59:00Z".to_owned(),
            observed_at: "2026-08-11T15:59:30Z".to_owned(),
        };
        let now = OffsetDateTime::parse("2026-08-11T16:00:00Z", &Rfc3339).unwrap();
        let mut warnings = BTreeSet::new();

        let report = build_window_report(
            &window,
            "codex",
            &[],
            None,
            None,
            &BTreeMap::new(),
            false,
            DEFAULT_PREMIUM_CAP_RATIO,
            now,
            &mut warnings,
        );

        assert!(report.stale);
        assert!(!report.current);
        assert_eq!(report.freshness, "expired");
        assert_eq!(report.account_lower_bound_percent, None);
        assert_eq!(report.free_headroom_upper_bound_points, None);
        assert_eq!(report.account_percent, None);
        assert_eq!(report.last_known_percent, 21.0);
        assert_eq!(report.sample_age_seconds, Some(30));
    }

    #[test]
    fn target_model_breakdown_excludes_other_account_seats() {
        let window = BurnWindow {
            id: 1,
            account_key: "claude:a".to_owned(),
            window_kind: "session_5h".to_owned(),
            window_scope: None,
            window_start: "2026-08-10T15:00:00Z".to_owned(),
            percent: 20.0,
            resets_at: "2026-08-10T20:00:00Z".to_owned(),
            observed_at: "2026-08-10T16:00:00Z".to_owned(),
        };
        let rows = ["target", "other"]
            .into_iter()
            .map(|seat_id| WindowRow {
                seat_id: seat_id.to_owned(),
                model: "claude-sonnet-5".to_owned(),
                credit_metered: false,
                first_bucket_ts: None,
                tokens: TokenCounts {
                    input: 10,
                    ..TokenCounts::default()
                },
            })
            .collect::<Vec<_>>();
        let target = UsageReportTarget {
            seat_id: "target".to_owned(),
            friendly_name: None,
            account_key: None,
            usage_cap_fraction: None,
            self_seats: BTreeSet::from(["target".to_owned()]),
            child_seats: BTreeSet::new(),
            available_descendant_count: 0,
        };
        let mut warnings = BTreeSet::new();
        let report = build_window_report(
            &window,
            "claude",
            &rows,
            None,
            Some(&target),
            &BTreeMap::new(),
            true,
            DEFAULT_PREMIUM_CAP_RATIO,
            OffsetDateTime::parse("2026-08-10T16:00:00Z", &Rfc3339).unwrap(),
            &mut warnings,
        );

        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].seat_id, "target");
        assert_eq!(report.seats.len(), 1);
        assert_eq!(report.seats[0].seat_id, "target");
        assert_eq!(report.seats[0].total_tokens, 10);
    }

    #[test]
    fn fresh_weekly_window_projects_existing_pace_and_model_seat_equivalents() {
        let window = BurnWindow {
            id: 1,
            account_key: "codex:a".to_owned(),
            window_kind: "codex_10080".to_owned(),
            window_scope: None,
            window_start: "2026-08-10T00:00:00Z".to_owned(),
            percent: 10.0,
            resets_at: "2026-08-17T00:00:00Z".to_owned(),
            observed_at: "2026-08-11T00:00:00Z".to_owned(),
        };
        let rows = vec![WindowRow {
            seat_id: "luna-seat".to_owned(),
            model: "gpt-5.6-luna".to_owned(),
            credit_metered: false,
            first_bucket_ts: Some("2026-08-10T00:00:00Z".to_owned()),
            tokens: TokenCounts {
                input: 100,
                ..TokenCounts::default()
            },
        }];
        let mut warnings = BTreeSet::new();

        let report = build_window_report(
            &window,
            "codex",
            &rows,
            None,
            None,
            &BTreeMap::new(),
            false,
            DEFAULT_PREMIUM_CAP_RATIO,
            OffsetDateTime::parse("2026-08-11T00:00:00Z", &Rfc3339).unwrap(),
            &mut warnings,
        );

        let projection = report.projection.unwrap();
        assert_eq!(projection.confidence, "low_conservative");
        assert_eq!(projection.assumptions.len(), 3);
        assert_eq!(projection.seat_projection_status, "available");
        assert_eq!(projection.elapsed_seconds, 86_400);
        assert_eq!(projection.horizon_seconds, 6 * 86_400);
        assert_eq!(projection.burn_rate_points_per_day, 10.0);
        assert_eq!(projection.projected_account_percent_at_reset, 70.0);
        assert_eq!(projection.projected_free_headroom_points, 30.0);
        assert_eq!(projection.additional_seats.len(), 1);
        assert_eq!(projection.additional_seats[0].model, "gpt-5.6-luna");
        assert_eq!(projection.additional_seats[0].baseline_seats, 1);
        assert_eq!(
            projection.additional_seats[0].additional_seat_equivalents,
            0.5
        );
    }

    #[test]
    fn premium_models_are_excluded_from_overall_seat_capacity_projection() {
        let window = BurnWindow {
            id: 1,
            account_key: "claude:a".to_owned(),
            window_kind: "weekly_all".to_owned(),
            window_scope: None,
            window_start: "2026-08-10T00:00:00Z".to_owned(),
            percent: 10.0,
            resets_at: "2026-08-17T00:00:00Z".to_owned(),
            observed_at: "2026-08-11T00:00:00Z".to_owned(),
        };
        let burn_by_model = BTreeMap::from([
            (
                ("premium-seat".to_owned(), "claude-fable-5".to_owned()),
                ModelBurn {
                    burn_percent: 5.0,
                    weighted_units: 10.0,
                    first_bucket_ts: Some("2026-08-10T00:00:00Z".to_owned()),
                },
            ),
            (
                ("standard-seat".to_owned(), "claude-sonnet-5".to_owned()),
                ModelBurn {
                    burn_percent: 5.0,
                    weighted_units: 10.0,
                    first_bucket_ts: Some("2026-08-10T00:00:00Z".to_owned()),
                },
            ),
        ]);

        let projection = build_projection(
            &window,
            false,
            false,
            DEFAULT_PREMIUM_CAP_RATIO,
            Some("Fable"),
            &burn_by_model,
        )
        .unwrap();

        assert_eq!(projection.additional_seats.len(), 1);
        assert_eq!(projection.additional_seats[0].model, "claude-sonnet-5");
    }

    #[test]
    fn short_lived_seat_is_not_used_as_a_capacity_baseline() {
        let window = BurnWindow {
            id: 1,
            account_key: "codex:a".to_owned(),
            window_kind: "codex_10080".to_owned(),
            window_scope: None,
            window_start: "2026-08-10T00:00:00Z".to_owned(),
            percent: 10.0,
            resets_at: "2026-08-17T00:00:00Z".to_owned(),
            observed_at: "2026-08-11T00:00:00Z".to_owned(),
        };
        let burn_by_model = BTreeMap::from([(
            ("new-seat".to_owned(), "gpt-5.6-luna".to_owned()),
            ModelBurn {
                burn_percent: 10.0,
                weighted_units: 10.0,
                first_bucket_ts: Some("2026-08-10T23:30:00Z".to_owned()),
            },
        )]);

        let projection = build_projection(
            &window,
            false,
            false,
            DEFAULT_PREMIUM_CAP_RATIO,
            None,
            &burn_by_model,
        )
        .unwrap();

        assert_eq!(
            projection.seat_projection_status,
            "no_observed_model_baseline"
        );
        assert!(projection.additional_seats.is_empty());
    }

    #[test]
    fn stale_current_meter_marks_report_non_actionable_with_refresh_guidance() {
        let now = OffsetDateTime::parse("2026-08-11T01:00:00Z", &Rfc3339).unwrap();
        let window = BurnWindow {
            id: 1,
            account_key: "codex:a".to_owned(),
            window_kind: "codex_10080".to_owned(),
            window_scope: None,
            window_start: "2026-08-10T00:00:00Z".to_owned(),
            percent: 12.0,
            resets_at: "2026-08-17T00:00:00Z".to_owned(),
            observed_at: "2026-08-11T00:49:00Z".to_owned(),
        };
        let mut warnings = BTreeSet::new();
        let report = build_window_report(
            &window,
            "codex",
            &[],
            None,
            None,
            &BTreeMap::new(),
            false,
            DEFAULT_PREMIUM_CAP_RATIO,
            now,
            &mut warnings,
        );
        let accounts = vec![UsageAccountReport {
            account_key: "codex:a".to_owned(),
            label: None,
            provider: "codex".to_owned(),
            plan_tier: Some("pro".to_owned()),
            active: true,
            windows: vec![report],
        }];

        let decision = usage_decision_summary(&accounts);

        assert_eq!(decision.status, "non_actionable");
        assert_eq!(decision.fresh_current_windows, 0);
        assert_eq!(decision.stale_current_windows, 1);
        assert_eq!(decision.missing_current_windows, ["codex:a:codex_300"]);
        assert!(decision
            .reasons
            .iter()
            .any(|reason| reason.contains("older than 10 minutes")));
        assert!(decision
            .refresh_guidance
            .iter()
            .any(|guidance| guidance.contains("cannot force")));
    }

    #[test]
    fn fresh_partial_provider_payload_is_non_actionable_when_weekly_meter_is_missing() {
        let now = OffsetDateTime::parse("2026-08-11T01:00:00Z", &Rfc3339).unwrap();
        let window = BurnWindow {
            id: 1,
            account_key: "codex:a".to_owned(),
            window_kind: "codex_300".to_owned(),
            window_scope: None,
            window_start: "2026-08-11T00:00:00Z".to_owned(),
            percent: 2.0,
            resets_at: "2026-08-11T05:00:00Z".to_owned(),
            observed_at: "2026-08-11T01:00:00Z".to_owned(),
        };
        let mut warnings = BTreeSet::new();
        let report = build_window_report(
            &window,
            "codex",
            &[],
            None,
            None,
            &BTreeMap::new(),
            false,
            DEFAULT_PREMIUM_CAP_RATIO,
            now,
            &mut warnings,
        );
        let accounts = vec![UsageAccountReport {
            account_key: "codex:a".to_owned(),
            label: None,
            provider: "codex".to_owned(),
            plan_tier: Some("pro".to_owned()),
            active: true,
            windows: vec![report],
        }];

        let decision = usage_decision_summary(&accounts);

        assert_eq!(decision.status, "non_actionable");
        assert_eq!(decision.fresh_current_windows, 1);
        assert_eq!(decision.missing_current_windows, ["codex:a:codex_10080"]);
        assert!(decision
            .reasons
            .iter()
            .any(|reason| reason.contains("Missing current limiting meter")));
    }

    #[test]
    fn active_account_without_windows_blocks_an_otherwise_complete_report() {
        let now = OffsetDateTime::parse("2026-08-11T01:00:00Z", &Rfc3339).unwrap();
        let mut warnings = BTreeSet::new();
        let build = |kind: &str, start: &str, reset: &str, warnings: &mut BTreeSet<String>| {
            build_window_report(
                &BurnWindow {
                    id: 1,
                    account_key: "codex:a".to_owned(),
                    window_kind: kind.to_owned(),
                    window_scope: None,
                    window_start: start.to_owned(),
                    percent: 2.0,
                    resets_at: reset.to_owned(),
                    observed_at: "2026-08-11T01:00:00Z".to_owned(),
                },
                "codex",
                &[],
                None,
                None,
                &BTreeMap::new(),
                false,
                DEFAULT_PREMIUM_CAP_RATIO,
                now,
                warnings,
            )
        };
        let accounts = vec![
            UsageAccountReport {
                account_key: "codex:a".to_owned(),
                label: None,
                provider: "codex".to_owned(),
                plan_tier: Some("pro".to_owned()),
                active: true,
                windows: vec![
                    build(
                        "codex_300",
                        "2026-08-11T00:00:00Z",
                        "2026-08-11T05:00:00Z",
                        &mut warnings,
                    ),
                    build(
                        "codex_10080",
                        "2026-08-10T00:00:00Z",
                        "2026-08-17T00:00:00Z",
                        &mut warnings,
                    ),
                ],
            },
            UsageAccountReport {
                account_key: "claude:b".to_owned(),
                label: None,
                provider: "claude".to_owned(),
                plan_tier: Some("max".to_owned()),
                active: true,
                windows: Vec::new(),
            },
        ];

        let decision = usage_decision_summary(&accounts);

        assert_eq!(decision.status, "non_actionable");
        assert_eq!(decision.fresh_current_windows, 2);
        assert_eq!(
            decision.missing_current_windows,
            [
                "claude:b:session_5h",
                "claude:b:weekly_all",
                "claude:b:weekly_scoped"
            ]
        );
        assert!(decision
            .refresh_guidance
            .iter()
            .any(|guidance| guidance.starts_with("Claude refreshes")));
    }

    #[test]
    fn rolling_resets_select_one_current_snapshot_and_three_closed_windows() {
        let window = |id: i64, start: &str, reset: &str, observed: &str| BurnWindow {
            id,
            account_key: "codex:a".to_owned(),
            window_kind: "codex_10080".to_owned(),
            window_scope: Some("codex_bengalfox".to_owned()),
            window_start: start.to_owned(),
            percent: id as f64,
            resets_at: reset.to_owned(),
            observed_at: observed.to_owned(),
        };
        let windows = vec![
            window(
                1,
                "2026-08-10T12:00:00Z",
                "2026-08-17T12:00:00Z",
                "2026-08-10T12:00:01Z",
            ),
            window(
                2,
                "2026-08-10T12:00:05Z",
                "2026-08-17T12:00:05Z",
                "2026-08-10T12:00:06Z",
            ),
            window(
                3,
                "2026-07-20T00:00:00Z",
                "2026-07-27T00:00:00Z",
                "2026-07-20T01:00:00Z",
            ),
            window(
                4,
                "2026-07-13T00:00:00Z",
                "2026-07-20T00:00:00Z",
                "2026-07-13T01:00:00Z",
            ),
            window(
                5,
                "2026-07-06T00:00:00Z",
                "2026-07-13T00:00:00Z",
                "2026-07-06T01:00:00Z",
            ),
            window(
                6,
                "2026-06-29T00:00:00Z",
                "2026-07-06T00:00:00Z",
                "2026-06-29T01:00:00Z",
            ),
        ];
        let now = OffsetDateTime::parse("2026-08-11T00:00:00Z", &Rfc3339).unwrap();

        let current = select_windows(&windows, true, now);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, 2);

        let history = select_windows(&windows, false, now);
        assert_eq!(
            history.iter().map(|window| window.id).collect::<Vec<_>>(),
            vec![2, 3, 4, 5]
        );
    }
}
