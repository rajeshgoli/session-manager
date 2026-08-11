use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::usage_burn::UsageBurnStore;

const DB_TIMESTAMP_FORMAT: &[time::format_description::FormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountIdentity {
    pub provider: Provider,
    pub external_id: String,
    pub label: Option<String>,
    pub plan_tier: Option<String>,
    pub extra_usage_enabled: Option<bool>,
}

impl AccountIdentity {
    pub fn account_key(&self) -> String {
        format!("{}:{}", self.provider.as_str(), self.external_id)
    }

    fn validate(&self) -> Result<()> {
        if self.external_id.trim().is_empty() {
            bail!("{} account identity is empty", self.provider.as_str());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineAttribution {
    pub account_key: String,
    pub is_assumed: bool,
    pub is_uncertain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationOutcome {
    Opened {
        account_key: String,
    },
    Unchanged {
        account_key: String,
    },
    Switched {
        previous_account_key: String,
        account_key: String,
        uncertainty_ms: i64,
    },
    LoggedOut {
        previous_account_key: String,
    },
    RemainedLoggedOut,
}

#[derive(Debug, Clone)]
pub struct UsageIdentityStore {
    db_path: PathBuf,
}

impl UsageIdentityStore {
    pub fn new(db_path: impl Into<PathBuf>) -> Result<Self> {
        let store = Self {
            db_path: db_path.into(),
        };
        store.initialize()?;
        Ok(store)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn initialize(&self) -> Result<()> {
        let conn = self.open()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS accounts (
              account_key   TEXT PRIMARY KEY,
              provider      TEXT NOT NULL,
              external_id   TEXT NOT NULL,
              label         TEXT,
              plan_tier     TEXT,
              extra_usage_enabled INTEGER,
              first_seen    TEXT NOT NULL,
              last_seen     TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS account_timeline (
              provider          TEXT NOT NULL,
              account_key       TEXT NOT NULL REFERENCES accounts(account_key),
              from_ts           TEXT NOT NULL,
              to_ts             TEXT,
              from_uncertain_ms INTEGER NOT NULL DEFAULT 0,
              is_assumed        INTEGER NOT NULL DEFAULT 0,
              PRIMARY KEY (provider, from_ts)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_account_timeline_current
              ON account_timeline(provider) WHERE to_ts IS NULL;
            CREATE INDEX IF NOT EXISTS idx_account_timeline_lookup
              ON account_timeline(provider, from_ts, to_ts);

            CREATE TABLE IF NOT EXISTS account_metadata_history (
              account_key          TEXT NOT NULL REFERENCES accounts(account_key),
              observed_at          TEXT NOT NULL,
              plan_tier            TEXT,
              extra_usage_enabled  INTEGER,
              PRIMARY KEY (account_key, observed_at)
            );
            CREATE INDEX IF NOT EXISTS idx_account_metadata_lookup
              ON account_metadata_history(account_key, observed_at);
            "#,
        )
        .context("failed to initialize usage identity schema")?;
        let account_columns = conn
            .prepare("PRAGMA table_info(accounts)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !account_columns
            .iter()
            .any(|column| column == "extra_usage_enabled")
        {
            conn.execute(
                "ALTER TABLE accounts ADD COLUMN extra_usage_enabled INTEGER",
                [],
            )?;
        }
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
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open usage DB {}", self.db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000_i64)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(conn)
    }

    /// Persist one successful read of a provider's identity surface.
    ///
    /// `previous_poll_at` is the preceding successful read, including a logged-out
    /// read. On a direct account switch it bounds the uncertain interval. The
    /// switch itself is stored at `observed_at`, so the interval remains half-open
    /// and messages in `[previous_poll_at, observed_at)` stay with the earlier
    /// account.
    pub fn record_observation(
        &self,
        provider: Provider,
        identity: Option<&AccountIdentity>,
        observed_at: OffsetDateTime,
        previous_poll_at: Option<OffsetDateTime>,
        bootstrap_from: Option<OffsetDateTime>,
    ) -> Result<ObservationOutcome> {
        if let Some(identity) = identity {
            if identity.provider != provider {
                bail!(
                    "identity provider {} does not match observation provider {}",
                    identity.provider.as_str(),
                    provider.as_str()
                );
            }
            identity.validate()?;
        }

        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let observed_ts = format_timestamp(observed_at)?;
        let current = current_account(&tx, provider)?;

        let outcome = match (current, identity) {
            (Some((current_key, _)), None) => {
                close_current(&tx, provider, &observed_ts)?;
                ObservationOutcome::LoggedOut {
                    previous_account_key: current_key,
                }
            }
            (None, None) => ObservationOutcome::RemainedLoggedOut,
            (Some((current_key, _)), Some(identity)) if current_key == identity.account_key() => {
                upsert_account(&tx, identity, &observed_ts)?;
                ObservationOutcome::Unchanged {
                    account_key: current_key,
                }
            }
            (Some((previous_key, previous_last_seen)), Some(identity)) => {
                upsert_account(&tx, identity, &observed_ts)?;
                close_current(&tx, provider, &observed_ts)?;
                let persisted_previous = if previous_poll_at.is_none() {
                    Some(parse_timestamp(&previous_last_seen)?)
                } else {
                    None
                };
                let uncertainty_ms =
                    uncertainty_width(previous_poll_at.or(persisted_previous), observed_at);
                insert_timeline_row(
                    &tx,
                    provider,
                    &identity.account_key(),
                    &observed_ts,
                    None,
                    uncertainty_ms,
                    false,
                )?;
                ObservationOutcome::Switched {
                    previous_account_key: previous_key,
                    account_key: identity.account_key(),
                    uncertainty_ms,
                }
            }
            (None, Some(identity)) => {
                upsert_account(&tx, identity, &observed_ts)?;
                let uncertainty_ms = uncertainty_width(previous_poll_at, observed_at);
                insert_timeline_row(
                    &tx,
                    provider,
                    &identity.account_key(),
                    &observed_ts,
                    None,
                    uncertainty_ms,
                    false,
                )?;
                ObservationOutcome::Opened {
                    account_key: identity.account_key(),
                }
            }
        };

        if let Some(earliest) = bootstrap_from {
            ensure_bootstrap_interval_tx(&tx, provider, earliest)?;
        }
        tx.commit()?;
        Ok(outcome)
    }

    /// Register account metadata without changing the provider's active timeline.
    ///
    /// Cached usage can carry the final sample for an account that was replaced
    /// before the cache was read. That account must exist for burn-sample foreign
    /// keys, but it must not become the current account again.
    pub fn ensure_account(
        &self,
        identity: &AccountIdentity,
        observed_at: OffsetDateTime,
    ) -> Result<()> {
        identity.validate()?;
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let observed_ts = format_timestamp(observed_at)?;
        tx.execute(
            r#"
            INSERT INTO accounts (
              account_key, provider, external_id, label, plan_tier, extra_usage_enabled,
              first_seen, last_seen
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
            ON CONFLICT(account_key) DO UPDATE SET
              label = COALESCE(excluded.label, accounts.label),
              plan_tier = COALESCE(excluded.plan_tier, accounts.plan_tier),
              extra_usage_enabled = COALESCE(excluded.extra_usage_enabled, accounts.extra_usage_enabled),
              last_seen = MAX(accounts.last_seen, excluded.last_seen)
            "#,
            params![
                identity.account_key(),
                identity.provider.as_str(),
                &identity.external_id,
                identity.label.as_deref(),
                identity.plan_tier.as_deref(),
                identity.extra_usage_enabled,
                observed_ts,
            ],
        )?;
        record_account_metadata(&tx, &identity.account_key(), &observed_ts)?;
        tx.commit()?;
        Ok(())
    }

    /// Backfill the assumed interval required when open windows contain messages
    /// older than the first identity poll. This may be called after the scanner
    /// discovers the true earliest timestamp; it always uses the account from the
    /// provider's first genuinely observed timeline row.
    pub fn ensure_bootstrap_interval(
        &self,
        provider: Provider,
        earliest_open_message_at: OffsetDateTime,
    ) -> Result<bool> {
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = ensure_bootstrap_interval_tx(&tx, provider, earliest_open_message_at)?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn account_at(
        &self,
        provider: Provider,
        message_at: OffsetDateTime,
    ) -> Result<Option<TimelineAttribution>> {
        let conn = self.open()?;
        let message_ts = format_timestamp(message_at)?;
        let covering = conn
            .query_row(
                r#"
                SELECT account_key, is_assumed, from_ts, to_ts
                FROM account_timeline
                WHERE provider = ?1
                  AND from_ts <= ?2
                  AND (to_ts IS NULL OR ?2 < to_ts)
                ORDER BY from_ts DESC
                LIMIT 1
                "#,
                params![provider.as_str(), message_ts],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((account_key, is_assumed, from_ts, to_ts)) = covering else {
            return Ok(None);
        };

        let uncertainty = if let Some(boundary) = to_ts.as_deref() {
            conn.query_row(
                r#"
                SELECT from_uncertain_ms
                FROM account_timeline
                WHERE provider = ?1 AND from_ts = ?2
                "#,
                params![provider.as_str(), boundary],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
        } else {
            0
        };
        let from = parse_timestamp(&from_ts)?;
        let is_uncertain = uncertainty > 0
            && message_at >= from
            && to_ts
                .as_deref()
                .map(parse_timestamp)
                .transpose()?
                .is_some_and(|boundary| {
                    message_at >= boundary - time::Duration::milliseconds(uncertainty)
                        && message_at < boundary
                });

        Ok(Some(TimelineAttribution {
            account_key,
            is_assumed,
            is_uncertain,
        }))
    }
}

fn current_account(tx: &Transaction<'_>, provider: Provider) -> Result<Option<(String, String)>> {
    tx.query_row(
        r#"
        SELECT timeline.account_key, accounts.last_seen
        FROM account_timeline AS timeline
        JOIN accounts ON accounts.account_key = timeline.account_key
        WHERE timeline.provider = ?1 AND timeline.to_ts IS NULL
        ORDER BY timeline.from_ts DESC
        LIMIT 1
        "#,
        [provider.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

fn close_current(tx: &Transaction<'_>, provider: Provider, to_ts: &str) -> Result<()> {
    tx.execute(
        "UPDATE account_timeline SET to_ts = ?2 WHERE provider = ?1 AND to_ts IS NULL",
        params![provider.as_str(), to_ts],
    )?;
    Ok(())
}

fn upsert_account(
    tx: &Transaction<'_>,
    identity: &AccountIdentity,
    observed_ts: &str,
) -> Result<()> {
    tx.execute(
        r#"
        INSERT INTO accounts (
          account_key, provider, external_id, label, plan_tier, extra_usage_enabled,
          first_seen, last_seen
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
        ON CONFLICT(account_key) DO UPDATE SET
          label = COALESCE(excluded.label, accounts.label),
          plan_tier = COALESCE(excluded.plan_tier, accounts.plan_tier),
          extra_usage_enabled = COALESCE(excluded.extra_usage_enabled, accounts.extra_usage_enabled),
          last_seen = excluded.last_seen
        "#,
        params![
            identity.account_key(),
            identity.provider.as_str(),
            &identity.external_id,
            identity.label.as_deref(),
            identity.plan_tier.as_deref(),
            identity.extra_usage_enabled,
            observed_ts,
        ],
    )?;
    record_account_metadata(tx, &identity.account_key(), observed_ts)?;
    Ok(())
}

fn record_account_metadata(
    tx: &Transaction<'_>,
    account_key: &str,
    observed_ts: &str,
) -> Result<()> {
    tx.execute(
        r#"
        INSERT INTO account_metadata_history (
          account_key, observed_at, plan_tier, extra_usage_enabled
        )
        SELECT account_key, ?2, plan_tier, extra_usage_enabled
        FROM accounts WHERE account_key = ?1
        ON CONFLICT(account_key, observed_at) DO UPDATE SET
          plan_tier = excluded.plan_tier,
          extra_usage_enabled = excluded.extra_usage_enabled
        "#,
        params![account_key, observed_ts],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_timeline_row(
    tx: &Transaction<'_>,
    provider: Provider,
    account_key: &str,
    from_ts: &str,
    to_ts: Option<&str>,
    uncertainty_ms: i64,
    is_assumed: bool,
) -> Result<()> {
    tx.execute(
        r#"
        INSERT INTO account_timeline (
          provider, account_key, from_ts, to_ts, from_uncertain_ms, is_assumed
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        "#,
        params![
            provider.as_str(),
            account_key,
            from_ts,
            to_ts,
            uncertainty_ms,
            is_assumed,
        ],
    )?;
    Ok(())
}

fn ensure_bootstrap_interval_tx(
    tx: &Transaction<'_>,
    provider: Provider,
    earliest_open_message_at: OffsetDateTime,
) -> Result<bool> {
    let first_observed = tx
        .query_row(
            r#"
            SELECT account_key, from_ts
            FROM account_timeline
            WHERE provider = ?1 AND is_assumed = 0
            ORDER BY from_ts ASC
            LIMIT 1
            "#,
            [provider.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((account_key, first_observed_ts)) = first_observed else {
        return Ok(false);
    };
    let first_observed_at = parse_timestamp(&first_observed_ts)?;
    if earliest_open_message_at >= first_observed_at {
        return Ok(false);
    }
    let bootstrap_ts = format_timestamp(earliest_open_message_at)?;
    let existing = tx
        .query_row(
            r#"
            SELECT from_ts
            FROM account_timeline
            WHERE provider = ?1 AND is_assumed = 1 AND to_ts = ?2
            ORDER BY from_ts ASC
            LIMIT 1
            "#,
            params![provider.as_str(), first_observed_ts],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing_from) = existing {
        if bootstrap_ts < existing_from {
            tx.execute(
                r#"
                UPDATE account_timeline
                SET from_ts = ?3
                WHERE provider = ?1 AND from_ts = ?2
                "#,
                params![provider.as_str(), existing_from, bootstrap_ts],
            )?;
            return Ok(true);
        }
        return Ok(false);
    }
    insert_timeline_row(
        tx,
        provider,
        &account_key,
        &bootstrap_ts,
        Some(&first_observed_ts),
        0,
        true,
    )?;
    Ok(true)
}

fn uncertainty_width(previous_poll_at: Option<OffsetDateTime>, observed_at: OffsetDateTime) -> i64 {
    previous_poll_at
        .filter(|previous| *previous <= observed_at)
        .map(|previous| (observed_at - previous).whole_milliseconds())
        .and_then(|milliseconds| i64::try_from(milliseconds).ok())
        .unwrap_or(0)
}

fn format_timestamp(timestamp: OffsetDateTime) -> Result<String> {
    timestamp
        .to_offset(time::UtcOffset::UTC)
        .format(DB_TIMESTAMP_FORMAT)
        .context("failed to format usage timestamp")
}

fn parse_timestamp(timestamp: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(timestamp, &Rfc3339)
        .with_context(|| format!("invalid usage timestamp {timestamp:?}"))
}

pub fn read_claude_identity(path: &Path) -> Result<Option<AccountIdentity>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    let root: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let Some(account) = root.get("oauthAccount") else {
        return Ok(None);
    };
    let external_id = required_json_string(account, "accountUuid", path)?;
    Ok(Some(AccountIdentity {
        provider: Provider::Claude,
        external_id,
        label: json_string(account, "emailAddress"),
        plan_tier: json_string(account, "organizationRateLimitTier"),
        extra_usage_enabled: account.get("hasExtraUsageEnabled").and_then(Value::as_bool),
    }))
}

pub fn read_codex_identity(path: &Path) -> Result<Option<AccountIdentity>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    let root: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if root.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return Ok(None);
    }
    let tokens = root
        .get("tokens")
        .and_then(Value::as_object)
        .with_context(|| format!("{} has chatgpt auth without tokens", path.display()))?;
    let external_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .with_context(|| format!("{} has chatgpt auth without account_id", path.display()))?;
    let claims = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(decode_jwt_claims);
    Ok(Some(AccountIdentity {
        provider: Provider::Codex,
        external_id,
        label: claims
            .as_ref()
            .and_then(|claims| json_string(claims, "email")),
        plan_tier: claims.as_ref().and_then(|claims| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| json_string(auth, "chatgpt_plan_type"))
                .or_else(|| json_string(claims, "https://api.openai.com/auth.chatgpt_plan_type"))
        }),
        extra_usage_enabled: None,
    }))
}

fn decode_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn json_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn required_json_string(value: &Value, key: &str, path: &Path) -> Result<String> {
    json_string(value, key)
        .with_context(|| format!("{} is missing non-empty {key}", path.display()))
}

#[derive(Debug)]
pub struct IdentityPoller {
    store: UsageIdentityStore,
    burn_store: UsageBurnStore,
    claude_identity_path: PathBuf,
    codex_identity_path: PathBuf,
    last_successful_poll: Mutex<BTreeMap<Provider, OffsetDateTime>>,
}

impl IdentityPoller {
    pub fn new(
        db_path: impl Into<PathBuf>,
        claude_identity_path: impl Into<PathBuf>,
        codex_identity_path: impl Into<PathBuf>,
    ) -> Result<Self> {
        let db_path = db_path.into();
        Ok(Self {
            store: UsageIdentityStore::new(&db_path)?,
            burn_store: UsageBurnStore::new(&db_path)?,
            claude_identity_path: claude_identity_path.into(),
            codex_identity_path: codex_identity_path.into(),
            last_successful_poll: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn store(&self) -> &UsageIdentityStore {
        &self.store
    }

    /// Poll both identity files at one shared observation instant. Provider
    /// failures are returned independently so one malformed file cannot prevent
    /// the other provider's timeline from advancing.
    pub fn poll_once(&self, observed_at: OffsetDateTime) -> Vec<(Provider, anyhow::Error)> {
        let mut errors = Vec::new();
        let claude_polled = self.poll_provider(
            Provider::Claude,
            observed_at,
            read_claude_identity(&self.claude_identity_path),
            &mut errors,
        );
        self.poll_provider(
            Provider::Codex,
            observed_at,
            read_codex_identity(&self.codex_identity_path),
            &mut errors,
        );
        if claude_polled && self.claude_identity_path.exists() {
            if let Err(error) = self
                .burn_store
                .record_claude_json_file(&self.claude_identity_path)
            {
                errors.push((Provider::Claude, error));
            }
        }
        errors
    }

    fn poll_provider(
        &self,
        provider: Provider,
        observed_at: OffsetDateTime,
        identity: Result<Option<AccountIdentity>>,
        errors: &mut Vec<(Provider, anyhow::Error)>,
    ) -> bool {
        let identity = match identity {
            Ok(identity) => identity,
            Err(error) => {
                errors.push((provider, error));
                return false;
            }
        };
        let previous = self
            .last_successful_poll
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&provider)
            .copied();
        match self.store.record_observation(
            provider,
            identity.as_ref(),
            observed_at,
            previous,
            None,
        ) {
            Ok(_) => {
                self.last_successful_poll
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(provider, observed_at);
                true
            }
            Err(error) => {
                errors.push((provider, error));
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sm-usage-identity-{name}-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn at(value: &str) -> OffsetDateTime {
        OffsetDateTime::parse(value, &Rfc3339).unwrap()
    }

    fn identity(provider: Provider, external_id: &str) -> AccountIdentity {
        AccountIdentity {
            provider,
            external_id: external_id.to_owned(),
            label: None,
            plan_tier: None,
            extra_usage_enabled: None,
        }
    }

    #[test]
    fn usage_db_uses_wal_and_creates_identity_schema() {
        let dir = TestDir::new("schema");
        let db_path = dir.0.join("usage.db");
        UsageIdentityStore::new(&db_path).unwrap();
        let conn = Connection::open(db_path).unwrap();
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        let tables = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            tables,
            vec!["account_metadata_history", "account_timeline", "accounts"]
        );
    }

    #[test]
    fn existing_accounts_schema_adds_extra_usage_without_losing_rows() {
        let dir = TestDir::new("extra-usage-migration");
        let db_path = dir.0.join("usage.db");
        let connection = Connection::open(&db_path).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE accounts (
                  account_key TEXT PRIMARY KEY,
                  provider TEXT NOT NULL,
                  external_id TEXT NOT NULL,
                  label TEXT,
                  plan_tier TEXT,
                  first_seen TEXT NOT NULL,
                  last_seen TEXT NOT NULL
                );
                INSERT INTO accounts VALUES (
                  'claude:existing', 'claude', 'existing', NULL, 'max',
                  '2026-08-10T00:00:00Z', '2026-08-10T00:00:00Z'
                );
                "#,
            )
            .unwrap();
        drop(connection);

        UsageIdentityStore::new(&db_path).unwrap();

        let connection = Connection::open(db_path).unwrap();
        let columns = connection
            .prepare("PRAGMA table_info(accounts)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "extra_usage_enabled"));
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn parses_claude_and_codex_stable_identity_surfaces() {
        let dir = TestDir::new("parse");
        let claude_path = dir.0.join("claude.json");
        fs::write(
            &claude_path,
            r#"{"oauthAccount":{"accountUuid":"claude-uuid","emailAddress":"c@example.com","organizationUuid":"org-uuid","organizationRateLimitTier":"default_claude_max_20x","hasExtraUsageEnabled":true}}"#,
        )
        .unwrap();
        let claude = read_claude_identity(&claude_path).unwrap().unwrap();
        assert_eq!(claude.account_key(), "claude:claude-uuid");
        assert_eq!(claude.label.as_deref(), Some("c@example.com"));
        assert_eq!(claude.plan_tier.as_deref(), Some("default_claude_max_20x"));
        assert_eq!(claude.extra_usage_enabled, Some(true));

        let claims = URL_SAFE_NO_PAD.encode(
            br#"{"email":"x@example.com","https://api.openai.com/auth":{"chatgpt_plan_type":"pro"}}"#,
        );
        let codex_path = dir.0.join("auth.json");
        fs::write(
            &codex_path,
            format!(
                r#"{{"auth_mode":"chatgpt","tokens":{{"account_id":"codex-account","id_token":"e30.{claims}.sig","access_token":"not-read","refresh_token":"not-read"}}}}"#
            ),
        )
        .unwrap();
        let codex = read_codex_identity(&codex_path).unwrap().unwrap();
        assert_eq!(codex.account_key(), "codex:codex-account");
        assert_eq!(codex.label.as_deref(), Some("x@example.com"));
        assert_eq!(codex.plan_tier.as_deref(), Some("pro"));
    }

    #[test]
    fn timeline_join_is_half_open_and_marks_the_earlier_side_uncertain() {
        let dir = TestDir::new("switch");
        let store = UsageIdentityStore::new(dir.0.join("usage.db")).unwrap();
        let account_a = identity(Provider::Claude, "a");
        let account_b = identity(Provider::Claude, "b");
        let first = at("2026-08-10T09:59:30Z");
        let previous_poll = at("2026-08-10T10:00:00Z");
        let switch_observed = at("2026-08-10T10:00:30Z");
        store
            .record_observation(Provider::Claude, Some(&account_a), first, None, None)
            .unwrap();
        store
            .record_observation(
                Provider::Claude,
                Some(&account_a),
                previous_poll,
                Some(first),
                None,
            )
            .unwrap();
        let outcome = store
            .record_observation(
                Provider::Claude,
                Some(&account_b),
                switch_observed,
                Some(previous_poll),
                None,
            )
            .unwrap();
        assert_eq!(
            outcome,
            ObservationOutcome::Switched {
                previous_account_key: "claude:a".to_owned(),
                account_key: "claude:b".to_owned(),
                uncertainty_ms: 30_000,
            }
        );

        let before = store
            .account_at(Provider::Claude, at("2026-08-10T09:59:59Z"))
            .unwrap()
            .unwrap();
        assert_eq!(before.account_key, "claude:a");
        assert!(!before.is_uncertain);

        let inside = store
            .account_at(Provider::Claude, at("2026-08-10T10:00:15Z"))
            .unwrap()
            .unwrap();
        assert_eq!(inside.account_key, "claude:a");
        assert!(inside.is_uncertain);

        let boundary = store
            .account_at(Provider::Claude, switch_observed)
            .unwrap()
            .unwrap();
        assert_eq!(boundary.account_key, "claude:b");
        assert!(!boundary.is_uncertain);
    }

    #[test]
    fn switch_uncertainty_uses_persisted_last_seen_after_restart() {
        let dir = TestDir::new("restart-switch");
        let db_path = dir.0.join("usage.db");
        UsageIdentityStore::new(&db_path)
            .unwrap()
            .record_observation(
                Provider::Codex,
                Some(&identity(Provider::Codex, "a")),
                at("2026-08-10T10:00:00Z"),
                None,
                None,
            )
            .unwrap();

        let reopened = UsageIdentityStore::new(&db_path).unwrap();
        let outcome = reopened
            .record_observation(
                Provider::Codex,
                Some(&identity(Provider::Codex, "b")),
                at("2026-08-10T10:00:30Z"),
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            outcome,
            ObservationOutcome::Switched {
                previous_account_key: "codex:a".to_owned(),
                account_key: "codex:b".to_owned(),
                uncertainty_ms: 30_000,
            }
        );
    }

    #[test]
    fn bootstrap_is_assumed_until_the_first_observed_poll() {
        let dir = TestDir::new("bootstrap");
        let store = UsageIdentityStore::new(dir.0.join("usage.db")).unwrap();
        let observed = at("2026-08-10T12:00:00Z");
        let earliest = at("2026-08-04T12:00:00Z");
        store
            .record_observation(
                Provider::Codex,
                Some(&identity(Provider::Codex, "a")),
                observed,
                None,
                Some(earliest),
            )
            .unwrap();

        let assumed = store
            .account_at(Provider::Codex, at("2026-08-06T12:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(assumed.account_key, "codex:a");
        assert!(assumed.is_assumed);
        let real = store
            .account_at(Provider::Codex, observed)
            .unwrap()
            .unwrap();
        assert_eq!(real.account_key, "codex:a");
        assert!(!real.is_assumed);
    }

    #[test]
    fn bootstrap_can_be_added_and_extended_after_the_first_poll() {
        let dir = TestDir::new("late-bootstrap");
        let store = UsageIdentityStore::new(dir.0.join("usage.db")).unwrap();
        let observed = at("2026-08-10T12:00:00Z");
        store
            .record_observation(
                Provider::Claude,
                Some(&identity(Provider::Claude, "a")),
                observed,
                None,
                None,
            )
            .unwrap();
        assert!(store
            .ensure_bootstrap_interval(Provider::Claude, at("2026-08-09T12:00:00Z"))
            .unwrap());
        assert!(store
            .ensure_bootstrap_interval(Provider::Claude, at("2026-08-08T12:00:00Z"))
            .unwrap());
        assert!(!store
            .ensure_bootstrap_interval(Provider::Claude, at("2026-08-09T00:00:00Z"))
            .unwrap());
        assert!(
            store
                .account_at(Provider::Claude, at("2026-08-08T12:00:00Z"))
                .unwrap()
                .unwrap()
                .is_assumed
        );
    }

    #[test]
    fn codex_file_absence_closes_the_current_half_open_interval() {
        let dir = TestDir::new("logout");
        let store = UsageIdentityStore::new(dir.0.join("usage.db")).unwrap();
        let logged_in = at("2026-08-10T12:00:00Z");
        let logged_out = at("2026-08-10T12:00:30Z");
        store
            .record_observation(
                Provider::Codex,
                Some(&identity(Provider::Codex, "a")),
                logged_in,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .record_observation(Provider::Codex, None, logged_out, Some(logged_in), None,)
                .unwrap(),
            ObservationOutcome::LoggedOut {
                previous_account_key: "codex:a".to_owned()
            }
        );
        assert!(store
            .account_at(Provider::Codex, logged_out)
            .unwrap()
            .is_none());
    }

    #[test]
    fn poller_ingests_both_providers_and_keeps_failures_independent() {
        let dir = TestDir::new("poller");
        let claude_path = dir.0.join("claude.json");
        let codex_path = dir.0.join("auth.json");
        fs::write(
            &claude_path,
            r#"{"oauthAccount":{"accountUuid":"claude-a"}}"#,
        )
        .unwrap();
        fs::write(
            &codex_path,
            r#"{"auth_mode":"chatgpt","tokens":{"account_id":"codex-a"}}"#,
        )
        .unwrap();
        let poller = IdentityPoller::new(
            dir.0.join("usage.db"),
            claude_path.clone(),
            codex_path.clone(),
        )
        .unwrap();
        assert!(poller.poll_once(at("2026-08-10T12:00:00Z")).is_empty());
        assert_eq!(
            poller
                .store()
                .account_at(Provider::Claude, at("2026-08-10T12:00:00Z"))
                .unwrap()
                .unwrap()
                .account_key,
            "claude:claude-a"
        );
        assert_eq!(
            poller
                .store()
                .account_at(Provider::Codex, at("2026-08-10T12:00:00Z"))
                .unwrap()
                .unwrap()
                .account_key,
            "codex:codex-a"
        );

        fs::write(&claude_path, "not-json").unwrap();
        fs::write(
            &codex_path,
            r#"{"auth_mode":"chatgpt","tokens":{"account_id":"codex-b"}}"#,
        )
        .unwrap();
        let errors = poller.poll_once(at("2026-08-10T12:00:30Z"));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, Provider::Claude);
        assert_eq!(
            poller
                .store()
                .account_at(Provider::Claude, at("2026-08-10T12:00:30Z"))
                .unwrap()
                .unwrap()
                .account_key,
            "claude:claude-a"
        );
        assert_eq!(
            poller
                .store()
                .account_at(Provider::Codex, at("2026-08-10T12:00:30Z"))
                .unwrap()
                .unwrap()
                .account_key,
            "codex:codex-b"
        );
    }
}
