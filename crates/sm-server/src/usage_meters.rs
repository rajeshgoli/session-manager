//! The Claude meters the web usage dash shows (sm#1881): the latest burn
//! sample of each current window of the most recently observed Claude
//! account, with the spend report's run-out projection.

use std::path::Path;

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use time::OffsetDateTime;

use crate::analytics_spend::{format_nanos, linear_pace, open_read_only, table_exists, Pace};
use crate::work_attribution::nanos;

/// Window kinds in dash order: the 5-hour session, the all-models week, and
/// each model-scoped week (Fable).
const KINDS: [&str; 3] = ["session_5h", "weekly_all", "weekly_scoped"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageMeters {
    pub account_key: Option<String>,
    pub meters: Vec<UsageMeter>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageMeter {
    pub kind: &'static str,
    /// The model a `weekly_scoped` window limits, such as "Fable".
    pub scope: Option<String>,
    pub percent: f64,
    pub resets_at: String,
    pub observed_at: String,
    pub pace: Option<Pace>,
}

/// A window whose reset has passed is left out: its reading no longer holds.
pub fn claude_meters(usage_db: &Path, now: OffsetDateTime) -> Result<UsageMeters> {
    let empty = UsageMeters {
        account_key: None,
        meters: Vec::new(),
    };
    if !usage_db.exists() {
        return Ok(empty);
    }
    let usage = open_read_only(usage_db)?;
    if !table_exists(&usage, "burn_samples")? || !table_exists(&usage, "accounts")? {
        return Ok(empty);
    }
    let mut accounts =
        usage.prepare("SELECT account_key FROM accounts WHERE provider = 'claude'")?;
    let mut last_seen =
        usage.prepare("SELECT MAX(observed_at) FROM burn_samples WHERE account_key = ?1")?;
    let mut account: Option<(String, String)> = None;
    for key in accounts.query_map([], |row| row.get::<_, String>(0))? {
        let key = key?;
        let seen: Option<String> = last_seen.query_row([&key], |row| row.get(0))?;
        if let Some(seen) = seen {
            if account.as_ref().is_none_or(|(_, best)| seen > *best) {
                account = Some((key, seen));
            }
        }
    }
    let Some((account_key, _)) = account else {
        return Ok(empty);
    };

    let mut scopes = usage.prepare(
        "SELECT DISTINCT window_scope FROM burn_samples
          WHERE account_key = ?1 AND window_kind = ?2",
    )?;
    let mut latest = usage.prepare(
        "SELECT percent, window_start, resets_at, observed_at FROM burn_samples
          WHERE account_key = ?1 AND window_kind = ?2 AND window_scope IS ?3
          ORDER BY observed_at DESC, id DESC LIMIT 1",
    )?;
    let now = now.unix_timestamp_nanos();
    let mut meters = Vec::new();
    for kind in KINDS {
        let mut kind_scopes = scopes
            .query_map(params![account_key, kind], |row| {
                row.get::<_, Option<String>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        kind_scopes.sort();
        for scope in kind_scopes {
            let Some((percent, start, resets_text, observed_text)) = latest
                .query_row(params![account_key, kind, scope], |row| {
                    Ok((
                        row.get::<_, f64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .optional()?
            else {
                continue;
            };
            let (Some(start), Some(resets_at), Some(observed_at)) =
                (nanos(&start), nanos(&resets_text), nanos(&observed_text))
            else {
                continue;
            };
            if resets_at <= now {
                continue;
            }
            meters.push(UsageMeter {
                kind,
                scope,
                percent,
                resets_at: format_nanos(resets_at),
                observed_at: format_nanos(observed_at),
                pace: linear_pace(percent, start, observed_at, resets_at),
            });
        }
    }
    Ok(UsageMeters {
        account_key: Some(account_key),
        meters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_burn::{BurnWindowSample, UsageBurnStore};
    use crate::usage_identity::{AccountIdentity, Provider, UsageIdentityStore};
    use time::macros::datetime;

    fn window(
        kind: &str,
        scope: Option<&str>,
        minutes: i64,
        percent: f64,
        resets_at: OffsetDateTime,
    ) -> BurnWindowSample {
        BurnWindowSample {
            window_kind: kind.to_owned(),
            window_scope: scope.map(str::to_owned),
            duration_minutes: minutes,
            percent,
            resets_at,
            severity: None,
            is_active: None,
        }
    }

    #[test]
    fn reports_current_windows_with_pace_and_drops_reset_ones() {
        // The isolated test launcher points TMPDIR at a root it removes.
        let db = std::env::temp_dir().join(format!("sm-usage-meters-{}.db", std::process::id()));
        assert_eq!(
            claude_meters(&db, OffsetDateTime::now_utc())
                .unwrap()
                .meters,
            vec![]
        );

        let store = UsageBurnStore::new(&db).unwrap();
        let identity = AccountIdentity {
            provider: Provider::Claude,
            external_id: "acct".to_owned(),
            label: None,
            plan_tier: None,
            extra_usage_enabled: None,
        };
        let observed = datetime!(2026-10-01 05:00 UTC);
        UsageIdentityStore::new(&db)
            .unwrap()
            .ensure_account(&identity, observed)
            .unwrap();
        let key = identity.account_key();
        // An older Fable window that has since reset.
        store
            .record_for_account(
                &key,
                &[window(
                    "weekly_scoped",
                    Some("Fable"),
                    10_080,
                    40.0,
                    datetime!(2026-09-27 16:00 UTC),
                )],
                "test",
                datetime!(2026-09-25 00:00 UTC),
            )
            .unwrap();
        store
            .record_for_account(
                &key,
                &[
                    window(
                        "session_5h",
                        None,
                        300,
                        17.0,
                        datetime!(2026-10-01 07:50 UTC),
                    ),
                    window(
                        "weekly_all",
                        None,
                        10_080,
                        59.0,
                        datetime!(2026-10-04 16:00 UTC),
                    ),
                ],
                "test",
                observed,
            )
            .unwrap();

        let report = claude_meters(&db, datetime!(2026-10-01 05:01 UTC)).unwrap();
        assert_eq!(report.account_key.as_deref(), Some(key.as_str()));
        let kinds: Vec<_> = report.meters.iter().map(|m| m.kind).collect();
        assert_eq!(kinds, ["session_5h", "weekly_all"]);
        let weekly = &report.meters[1];
        assert_eq!(weekly.percent, 59.0);
        assert_eq!(weekly.resets_at, "2026-10-04T16:00:00Z");
        // 59% in 3.54 days runs out in another ~2.46 days, before the reset.
        assert!(matches!(&weekly.pace, Some(Pace::RunsOut { at }) if at.starts_with("2026-10-03")));
        // 17% in the 5-hour window's first 2h10m projects to ~39% at reset.
        assert!(
            matches!(report.meters[0].pace, Some(Pace::OnPace { percent }) if (percent - 39.2).abs() < 0.5)
        );
    }
}
