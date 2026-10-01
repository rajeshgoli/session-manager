//! The meters the web usage dash shows (sm#1881): per provider and account,
//! the latest burn sample of each current window, with the spend report's
//! run-out projection. The accounts are Analytics › Spend's: every one with a
//! window that has not reset.

use std::{collections::BTreeMap, path::Path};

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use time::OffsetDateTime;

use crate::analytics_spend::{format_nanos, linear_pace, open_read_only, table_exists, Pace};
use crate::work_attribution::nanos;

/// Burn window kinds in dash order, per provider, with the dash window each
/// is. A scoped kind (a model's own week, such as Fable) keeps its scope.
const KINDS: [(&str, &str, Window); 5] = [
    ("claude", "session_5h", Window::FiveHour),
    ("claude", "weekly_all", Window::Week),
    ("claude", "weekly_scoped", Window::Week),
    ("codex", "codex_300", Window::FiveHour),
    ("codex", "codex_10080", Window::Week),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    FiveHour,
    Week,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageMeters {
    pub meters: Vec<UsageMeter>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageMeter {
    pub provider: &'static str,
    pub account_key: String,
    pub label: Option<String>,
    pub window: Window,
    /// The model a scoped window limits, such as "Fable"; none for the
    /// window that covers every model.
    pub scope: Option<String>,
    pub percent: f64,
    pub resets_at: String,
    pub observed_at: String,
    pub pace: Option<Pace>,
}

/// A window whose reset has passed is left out: its reading no longer holds.
/// `configured_labels` override the account labels the identity files gave.
pub fn meters(
    usage_db: &Path,
    configured_labels: &BTreeMap<String, String>,
    now: OffsetDateTime,
) -> Result<UsageMeters> {
    let mut meters = Vec::new();
    if !usage_db.exists() {
        return Ok(UsageMeters { meters });
    }
    let usage = open_read_only(usage_db)?;
    if !table_exists(&usage, "burn_samples")? || !table_exists(&usage, "accounts")? {
        return Ok(UsageMeters { meters });
    }
    let mut accounts =
        usage.prepare("SELECT account_key, label FROM accounts WHERE provider = ?1 ORDER BY 1")?;
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
    for provider in ["claude", "codex"] {
        let provider_accounts = accounts
            .query_map([provider], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (account_key, label) in provider_accounts {
            let label = configured_labels.get(&account_key).cloned().or(label);
            for (_, kind, window) in KINDS.iter().filter(|(p, _, _)| *p == provider) {
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
                        provider,
                        account_key: account_key.clone(),
                        label: label.clone(),
                        window: *window,
                        scope,
                        percent,
                        resets_at: format_nanos(resets_at),
                        observed_at: format_nanos(observed_at),
                        pace: linear_pace(percent, start, observed_at, resets_at),
                    });
                }
            }
        }
    }
    Ok(UsageMeters { meters })
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
        let labels = BTreeMap::from([("codex:old".to_owned(), "Old".to_owned())]);
        assert_eq!(
            meters(&db, &labels, OffsetDateTime::now_utc())
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
        let identities = UsageIdentityStore::new(&db).unwrap();
        identities.ensure_account(&identity, observed).unwrap();
        let key = identity.account_key();
        for external_id in ["old", "pro"] {
            let codex = AccountIdentity {
                provider: Provider::Codex,
                external_id: external_id.to_owned(),
                label: Some(format!("{external_id}@example.com")),
                ..identity.clone()
            };
            identities.ensure_account(&codex, observed).unwrap();
        }
        // The old Codex account's week reset before now; the pro one's holds.
        store
            .record_for_account(
                "codex:old",
                &[window(
                    "codex_10080",
                    None,
                    10_080,
                    87.0,
                    datetime!(2026-09-26 08:13 UTC),
                )],
                "test",
                datetime!(2026-09-25 23:32 UTC),
            )
            .unwrap();
        store
            .record_for_account(
                "codex:pro",
                &[window(
                    "codex_10080",
                    None,
                    10_080,
                    68.0,
                    datetime!(2026-10-03 18:11 UTC),
                )],
                "test",
                observed,
            )
            .unwrap();
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

        let report = meters(&db, &labels, datetime!(2026-10-01 05:01 UTC)).unwrap();
        let rows: Vec<_> = report
            .meters
            .iter()
            .map(|m| {
                (
                    m.provider,
                    m.account_key.as_str(),
                    m.window,
                    m.label.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("claude", key.as_str(), Window::FiveHour, None),
                ("claude", key.as_str(), Window::Week, None),
                ("codex", "codex:pro", Window::Week, Some("pro@example.com")),
            ]
        );
        assert_eq!(report.meters[2].percent, 68.0);
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
