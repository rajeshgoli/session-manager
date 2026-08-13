use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    usage_burn::UsageBurnStore,
    usage_identity::{Provider, UsageIdentityStore},
};

const USAGE_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const PROJECT_KEY_GIT_TIMEOUT: Duration = Duration::from_secs(1);
const LEDGER_WRITE_BATCH_SIZE: usize = 16;
const MATERIALIZATION_BATCH_PAUSE: Duration = Duration::from_millis(1);
const DB_TIMESTAMP_FORMAT: &[time::format_description::FormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z"
);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageModelDefaults {
    pub claude: Option<String>,
    pub codex: Option<String>,
    pub codex_fork: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSeatMetadata {
    pub seat_id: String,
    pub friendly_name: Option<String>,
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub working_dir: String,
    pub parent_seat_id: Option<String>,
    pub root_seat_id: Option<String>,
    pub project_key: String,
}

impl UsageSeatMetadata {
    pub fn resolve_project_key(working_dir: &str) -> String {
        let working_dir = expand_home(working_dir);
        let mut command = Command::new("git");
        command.args(["-C"]).arg(&working_dir).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]);
        resolve_project_key_with_command(&working_dir, command, PROJECT_KEY_GIT_TIMEOUT)
    }
}

fn resolve_project_key_with_command(
    working_dir: &Path,
    command: Command,
    timeout: Duration,
) -> String {
    if let Some(output) = command_output_with_timeout(command, timeout) {
        if output.status.success() {
            if let Some(path) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                return canonical_path(Path::new(path));
            }
        }
    }
    canonical_path(working_dir)
}

fn command_output_with_timeout(mut command: Command, timeout: Duration) -> Option<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let started_at = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
        if started_at.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub artifacts_scanned: usize,
    pub messages_inserted: usize,
    pub messages_replaced: usize,
    pub messages_ignored: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MaterializationSummary {
    windows_examined: usize,
    messages_selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BurnWindow {
    account_key: String,
    kind: String,
    scope: String,
    start: String,
    resets_at: String,
}

#[derive(Debug, Clone)]
pub struct UsageLedgerStore {
    db_path: PathBuf,
    identity_store: UsageIdentityStore,
    burn_store: UsageBurnStore,
    model_defaults: UsageModelDefaults,
}

impl UsageLedgerStore {
    pub fn new(db_path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_model_defaults(db_path, UsageModelDefaults::default())
    }

    pub fn with_model_defaults(
        db_path: impl Into<PathBuf>,
        model_defaults: UsageModelDefaults,
    ) -> Result<Self> {
        let db_path = db_path.into();
        let store = Self {
            identity_store: UsageIdentityStore::new(&db_path)?,
            burn_store: UsageBurnStore::new(&db_path)?,
            db_path,
            model_defaults,
        };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<()> {
        self.open()?
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS seat_tokens (
                  seat_id            TEXT NOT NULL,
                  account_key        TEXT NOT NULL,
                  project_key        TEXT NOT NULL,
                  window_kind        TEXT NOT NULL,
                  window_start       TEXT NOT NULL,
                  bucket_ts          TEXT NOT NULL,
                  model              TEXT NOT NULL,
                  effort             TEXT,
                  credit_metered     INTEGER NOT NULL DEFAULT 0,
                  input_tokens       INTEGER NOT NULL DEFAULT 0,
                  output_tokens      INTEGER NOT NULL DEFAULT 0,
                  reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
                  cache_write_5m     INTEGER NOT NULL DEFAULT 0,
                  cache_write_1h     INTEGER NOT NULL DEFAULT 0,
                  cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
                  message_count      INTEGER NOT NULL DEFAULT 0,
                  updated_at         TEXT NOT NULL,
                  PRIMARY KEY (seat_id, account_key, project_key, window_kind, window_start,
                               bucket_ts, model, credit_metered)
                );
                CREATE INDEX IF NOT EXISTS idx_seat_tokens_fit
                  ON seat_tokens(account_key, window_kind, bucket_ts);

                CREATE TABLE IF NOT EXISTS seat_sessions (
                  seat_id             TEXT NOT NULL,
                  provider            TEXT NOT NULL,
                  provider_session_id TEXT NOT NULL,
                  artifact_path       TEXT,
                  first_seen          TEXT NOT NULL,
                  last_seen           TEXT NOT NULL,
                  PRIMARY KEY (seat_id, provider_session_id)
                );
                CREATE INDEX IF NOT EXISTS idx_seat_sessions_provider
                  ON seat_sessions(provider_session_id);

                CREATE TABLE IF NOT EXISTS scan_offsets (
                  artifact_path TEXT PRIMARY KEY,
                  byte_offset   INTEGER NOT NULL,
                  last_uuid     TEXT,
                  mtime_ns      INTEGER NOT NULL,
                  scanned_at    TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS burn_scan_offsets (
                  artifact_path TEXT PRIMARY KEY,
                  byte_offset   INTEGER NOT NULL,
                  mtime_ns      INTEGER NOT NULL,
                  scanned_at    TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS codex_thread_cursor (
                  thread_id            TEXT PRIMARY KEY,
                  artifact_path        TEXT NOT NULL,
                  last_seq             INTEGER NOT NULL,
                  last_input_tokens    INTEGER NOT NULL,
                  last_cached_input    INTEGER NOT NULL,
                  last_cache_write     INTEGER NOT NULL,
                  last_output_tokens   INTEGER NOT NULL,
                  last_reasoning       INTEGER NOT NULL,
                  last_total_tokens    INTEGER NOT NULL,
                  updated_at           TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS codex_thread_settings (
                  thread_id  TEXT NOT NULL,
                  source_seq INTEGER NOT NULL,
                  model      TEXT NOT NULL,
                  effort     TEXT,
                  PRIMARY KEY (thread_id, source_seq)
                );

                CREATE TABLE IF NOT EXISTS message_ledger (
                  msg_id       INTEGER PRIMARY KEY AUTOINCREMENT,
                  message_id   TEXT NOT NULL,
                  request_id   TEXT,
                  is_sidechain INTEGER NOT NULL,
                  has_speed    INTEGER NOT NULL,
                  total_tokens INTEGER NOT NULL,
                  seat_id      TEXT NOT NULL,
                  account_key  TEXT NOT NULL,
                  project_key  TEXT NOT NULL,
                  source_ref   TEXT NOT NULL,
                  source_seq   INTEGER,
                  bucket_ts    TEXT NOT NULL,
                  model        TEXT NOT NULL,
                  effort       TEXT,
                  input_tokens      INTEGER NOT NULL DEFAULT 0,
                  output_tokens     INTEGER NOT NULL DEFAULT 0,
                  reasoning_tokens  INTEGER NOT NULL DEFAULT 0,
                  cache_write_5m    INTEGER NOT NULL DEFAULT 0,
                  cache_write_1h    INTEGER NOT NULL DEFAULT 0,
                  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                  credit_metered    INTEGER NOT NULL DEFAULT 0,
                  recorded_at  TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_ledger_msgid
                  ON message_ledger(message_id);
                CREATE INDEX IF NOT EXISTS idx_ledger_rollup
                  ON message_ledger(account_key, bucket_ts);
                CREATE INDEX IF NOT EXISTS idx_ledger_window_materialization
                  ON message_ledger(account_key, recorded_at);

                CREATE TABLE IF NOT EXISTS message_window (
                  msg_id       INTEGER NOT NULL REFERENCES message_ledger(msg_id) ON DELETE CASCADE,
                  window_kind  TEXT NOT NULL,
                  window_start TEXT NOT NULL,
                  PRIMARY KEY (msg_id, window_kind, window_start)
                );
                CREATE INDEX IF NOT EXISTS idx_mw_rollup
                  ON message_window(window_kind, window_start);

                CREATE TABLE IF NOT EXISTS burn_window_materialization (
                  account_key  TEXT NOT NULL,
                  window_kind  TEXT NOT NULL,
                  window_scope TEXT NOT NULL,
                  window_start TEXT NOT NULL,
                  resets_at    TEXT NOT NULL,
                  materialized_at TEXT NOT NULL,
                  PRIMARY KEY (
                    account_key, window_kind, window_scope, window_start, resets_at
                  )
                );
                CREATE INDEX IF NOT EXISTS idx_burn_window_materialization_source
                  ON burn_samples(
                    account_key, window_kind, window_scope, window_start, resets_at
                  );

                CREATE TABLE IF NOT EXISTS message_alias (
                  lookup_key TEXT PRIMARY KEY,
                  kind       TEXT NOT NULL,
                  msg_id     INTEGER NOT NULL REFERENCES message_ledger(msg_id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_alias_msg ON message_alias(msg_id);

                CREATE TABLE IF NOT EXISTS seat_meta (
                  seat_id        TEXT NOT NULL,
                  observed_at    TEXT NOT NULL,
                  friendly_name  TEXT,
                  provider       TEXT NOT NULL,
                  model          TEXT,
                  project_key    TEXT NOT NULL,
                  working_dir    TEXT,
                  parent_seat_id TEXT,
                  root_seat_id   TEXT,
                  PRIMARY KEY (seat_id, observed_at)
                );
                "#,
            )
            .context("failed to initialize usage token ledger schema")?;
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

    pub fn scan(&self, seats: &[UsageSeatMetadata]) -> Result<ScanSummary> {
        let mut seats = self.resolve_seat_models(seats)?;
        self.snapshot_seat_meta(&seats)?;
        let bindings = self.artifact_bindings()?;
        self.extend_with_persisted_bound_seats(&mut seats, &bindings)?;
        let seat_by_source = bindings
            .iter()
            .map(|binding| {
                (
                    (
                        binding.provider.clone(),
                        binding.provider_session_id.clone(),
                    ),
                    binding.seat_id.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let seat_meta = seats
            .iter()
            .map(|seat| (seat.seat_id.clone(), seat.clone()))
            .collect::<BTreeMap<_, _>>();
        self.rebind_provisional_messages(&bindings, &seat_meta)?;
        let artifacts = expand_artifacts(&bindings);
        let mut summary = ScanSummary::default();
        let mut errors = Vec::new();
        for artifact in artifacts {
            let artifact_summary = match self.scan_artifact(&artifact, &seat_by_source, &seat_meta)
            {
                Ok(summary) => summary,
                Err(error) => {
                    errors.push(format!("{}: {error:#}", artifact.path.display()));
                    continue;
                }
            };
            summary.artifacts_scanned += artifact_summary.artifacts_scanned;
            summary.messages_inserted += artifact_summary.messages_inserted;
            summary.messages_replaced += artifact_summary.messages_replaced;
            summary.messages_ignored += artifact_summary.messages_ignored;
        }
        self.materialize_pending_windows()?;
        if !errors.is_empty() {
            bail!(
                "{} usage artifact scan(s) failed: {}",
                errors.len(),
                errors.join("; ")
            );
        }
        Ok(summary)
    }

    fn resolve_seat_models(&self, seats: &[UsageSeatMetadata]) -> Result<Vec<UsageSeatMetadata>> {
        let connection = self.open()?;
        seats
            .iter()
            .map(|seat| {
                let persisted = connection
                    .query_row(
                        r#"
                        SELECT model FROM seat_meta
                        WHERE seat_id = ?1 AND model IS NOT NULL AND TRIM(model) != ''
                        ORDER BY observed_at DESC LIMIT 1
                        "#,
                        [&seat.seat_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                let mut resolved = seat.clone();
                resolved.model = normalized_model(seat.model.as_deref())
                    .or(persisted)
                    .or_else(|| self.default_model(&seat.provider));
                Ok(resolved)
            })
            .collect()
    }

    fn extend_with_persisted_bound_seats(
        &self,
        seats: &mut Vec<UsageSeatMetadata>,
        bindings: &[ArtifactBinding],
    ) -> Result<()> {
        let mut known = seats
            .iter()
            .map(|seat| seat.seat_id.clone())
            .collect::<BTreeSet<_>>();
        let connection = self.open()?;
        for seat_id in bindings.iter().map(|binding| &binding.seat_id) {
            if !known.insert(seat_id.clone()) {
                continue;
            }
            let persisted = connection
                .query_row(
                    r#"
                    SELECT friendly_name, provider, model, project_key, working_dir,
                           parent_seat_id, root_seat_id
                    FROM seat_meta
                    WHERE seat_id = ?1
                    ORDER BY observed_at DESC LIMIT 1
                    "#,
                    [seat_id],
                    |row| {
                        Ok(UsageSeatMetadata {
                            seat_id: seat_id.clone(),
                            friendly_name: row.get(0)?,
                            provider: row.get(1)?,
                            model: row.get(2)?,
                            effort: None,
                            project_key: row.get(3)?,
                            working_dir: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                            parent_seat_id: row.get(5)?,
                            root_seat_id: row.get(6)?,
                        })
                    },
                )
                .optional()?;
            if let Some(persisted) = persisted {
                seats.push(persisted);
            }
        }
        Ok(())
    }

    fn rebind_provisional_messages(
        &self,
        bindings: &[ArtifactBinding],
        seat_meta: &BTreeMap<String, UsageSeatMetadata>,
    ) -> Result<()> {
        let mut connection = self.open()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for binding in bindings
            .iter()
            .filter(|binding| binding.seat_id != "unassigned")
        {
            let Some(metadata) = seat_meta.get(&binding.seat_id) else {
                continue;
            };
            let account_prefix = match binding.provider.as_str() {
                "claude" => "claude:%",
                "codex" | "codex-fork" => "codex:%",
                _ => continue,
            };
            let msg_ids = tx
                .prepare(
                    r#"
                    SELECT msg_id
                    FROM message_ledger
                    WHERE seat_id = 'unassigned'
                      AND source_ref = ?1
                      AND account_key LIKE ?2
                    ORDER BY msg_id
                    "#,
                )?
                .query_map(
                    params![binding.provider_session_id, account_prefix],
                    |row| row.get::<_, i64>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for msg_id in msg_ids {
                let incumbent = load_contribution(&tx, msg_id)?;
                reverse_contribution(&tx, msg_id, &incumbent)?;
                let mut rebound = incumbent;
                rebound.seat_id = binding.seat_id.clone();
                rebound.project_key = metadata.project_key.clone();
                overwrite_contribution(&tx, msg_id, &rebound)?;
                materialize_contribution(&tx, msg_id, &rebound)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn rebuild_rollups(&self) -> Result<()> {
        let mut connection = self.open()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM seat_tokens", [])?;
        tx.execute("DELETE FROM message_window", [])?;
        tx.execute("DELETE FROM burn_window_materialization", [])?;
        let ids = tx
            .prepare("SELECT msg_id FROM message_ledger ORDER BY msg_id")?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for msg_id in ids {
            let contribution = load_contribution(&tx, msg_id)?;
            materialize_contribution(&tx, msg_id, &contribution)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn rebuild(&self, seats: &[UsageSeatMetadata]) -> Result<ScanSummary> {
        let mut connection = self.open()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM seat_tokens", [])?;
        tx.execute("DELETE FROM message_alias", [])?;
        tx.execute("DELETE FROM message_window", [])?;
        tx.execute("DELETE FROM burn_window_materialization", [])?;
        tx.execute("DELETE FROM message_ledger", [])?;
        tx.execute("DELETE FROM scan_offsets", [])?;
        tx.execute("DELETE FROM codex_thread_cursor", [])?;
        tx.execute("DELETE FROM codex_thread_settings", [])?;
        tx.commit()?;
        self.scan(seats)
    }

    fn snapshot_seat_meta(&self, seats: &[UsageSeatMetadata]) -> Result<()> {
        if seats.is_empty() {
            return Ok(());
        }
        let observed_at = format_timestamp(OffsetDateTime::now_utc())?;
        let mut connection = self.open()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for seat in seats {
            let previous = tx
                .query_row(
                    r#"
                    SELECT friendly_name, provider, model, project_key, working_dir,
                           parent_seat_id, root_seat_id
                    FROM seat_meta
                    WHERE seat_id = ?1
                    ORDER BY observed_at DESC
                    LIMIT 1
                    "#,
                    [&seat.seat_id],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, Option<String>>(6)?,
                        ))
                    },
                )
                .optional()?;
            let current = (
                seat.friendly_name.clone(),
                seat.provider.clone(),
                seat.model.clone(),
                seat.project_key.clone(),
                Some(seat.working_dir.clone()),
                seat.parent_seat_id.clone(),
                seat.root_seat_id.clone(),
            );
            if previous.as_ref() == Some(&current) {
                continue;
            }
            tx.execute(
                r#"
                INSERT INTO seat_meta (
                  seat_id, observed_at, friendly_name, provider, model, project_key,
                  working_dir, parent_seat_id, root_seat_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                "#,
                params![
                    seat.seat_id,
                    observed_at,
                    seat.friendly_name,
                    seat.provider,
                    seat.model,
                    seat.project_key,
                    seat.working_dir,
                    seat.parent_seat_id,
                    seat.root_seat_id,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn artifact_bindings(&self) -> Result<Vec<ArtifactBinding>> {
        let connection = self.open()?;
        let mut statement = connection.prepare(
            r#"
            SELECT seat_id, provider, provider_session_id, artifact_path
            FROM seat_sessions
            WHERE artifact_path IS NOT NULL AND TRIM(artifact_path) != ''
            ORDER BY provider, artifact_path, provider_session_id
            "#,
        )?;
        let bindings = statement
            .query_map([], |row| {
                Ok(ArtifactBinding {
                    seat_id: row.get(0)?,
                    provider: row.get(1)?,
                    provider_session_id: row.get(2)?,
                    artifact_path: PathBuf::from(row.get::<_, String>(3)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(bindings)
    }

    fn scan_artifact(
        &self,
        artifact: &Artifact,
        seat_by_source: &BTreeMap<(String, String), String>,
        seat_meta: &BTreeMap<String, UsageSeatMetadata>,
    ) -> Result<ScanSummary> {
        let metadata = match fs::metadata(&artifact.path) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => return Ok(ScanSummary::default()),
        };
        let mtime_ns = file_mtime_ns(&metadata);
        let file_len = metadata.len();
        if matches!(artifact.provider.as_str(), "codex" | "codex-fork") {
            self.scan_codex_burn(artifact, file_len, mtime_ns)?;
        }
        let connection = self.open()?;
        let saved = connection
            .query_row(
                "SELECT byte_offset, mtime_ns FROM scan_offsets WHERE artifact_path = ?1",
                [artifact.path.to_string_lossy().as_ref()],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let offset = saved
            .map(|(offset, _)| offset)
            .filter(|offset| *offset <= file_len)
            .unwrap_or(0);
        if offset == file_len && saved.is_some_and(|(_, saved_mtime)| saved_mtime == mtime_ns) {
            return Ok(ScanSummary::default());
        }

        self.ensure_bootstrap_for_artifact(artifact, offset)?;
        let artifact_project_key = matches!(artifact.provider.as_str(), "codex" | "codex-fork")
            .then(|| codex_artifact_project_key(&artifact.path))
            .flatten();

        let mut connection = self.open()?;
        let mut tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_codex_cursor_baselines(&tx, artifact)?;
        let mut reader = BufReader::new(fs::File::open(&artifact.path).with_context(|| {
            format!("failed to open usage artifact {}", artifact.path.display())
        })?);
        reader.seek(SeekFrom::Start(offset))?;
        let mut summary = ScanSummary {
            artifacts_scanned: 1,
            ..ScanSummary::default()
        };
        let mut line_offset = offset;
        let mut batch_lines = 0;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            if line.last() != Some(&b'\n') {
                break;
            }
            let source_seq = i64::try_from(line_offset).unwrap_or(i64::MAX);
            line_offset = line_offset.saturating_add(read as u64);
            batch_lines += 1;
            let outcome = match artifact.provider.as_str() {
                "claude" => parse_claude_line(&line, &artifact.path)
                    .and_then(|parsed| {
                        parsed
                            .map(|parsed| {
                                self.resolve_claude_contribution(
                                    &tx,
                                    parsed,
                                    seat_by_source,
                                    seat_meta,
                                )
                            })
                            .transpose()
                    })?
                    .flatten(),
                "codex" | "codex-fork" => parse_codex_line(
                    &line,
                    source_seq,
                    artifact
                        .provider_session_ids
                        .iter()
                        .next()
                        .map(String::as_str),
                )?
                .map(|event| {
                    self.process_codex_event(
                        &tx,
                        artifact,
                        event,
                        seat_by_source,
                        seat_meta,
                        artifact_project_key.as_deref(),
                    )
                })
                .transpose()?
                .flatten(),
                _ => None,
            };
            if let Some(outcome) = outcome {
                match outcome {
                    IngestOutcome::Inserted => summary.messages_inserted += 1,
                    IngestOutcome::Replaced => summary.messages_replaced += 1,
                    IngestOutcome::Ignored => summary.messages_ignored += 1,
                }
            }
            if batch_lines >= LEDGER_WRITE_BATCH_SIZE {
                save_scan_offset(&tx, &artifact.path, line_offset, mtime_ns)?;
                tx.commit()?;
                tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                ensure_codex_cursor_baselines(&tx, artifact)?;
                batch_lines = 0;
            }
        }
        save_scan_offset(&tx, &artifact.path, line_offset, mtime_ns)?;
        tx.commit()?;
        Ok(summary)
    }

    fn scan_codex_burn(&self, artifact: &Artifact, file_len: u64, mtime_ns: i64) -> Result<()> {
        let artifact_path = artifact.path.to_string_lossy();
        let saved = self
            .open()?
            .query_row(
                "SELECT byte_offset, mtime_ns FROM burn_scan_offsets WHERE artifact_path = ?1",
                [artifact_path.as_ref()],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let offset = saved
            .map(|(offset, _)| offset)
            .filter(|offset| *offset <= file_len)
            .unwrap_or(0);
        if offset == file_len && saved.is_some_and(|(_, saved_mtime)| saved_mtime == mtime_ns) {
            return Ok(());
        }

        let mut reader = BufReader::new(fs::File::open(&artifact.path).with_context(|| {
            format!(
                "failed to open Codex burn artifact {}",
                artifact.path.display()
            )
        })?);
        reader.seek(SeekFrom::Start(offset))?;
        let mut line_offset = offset;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 || line.last() != Some(&b'\n') {
                break;
            }
            let value: Value = match serde_json::from_slice(&line) {
                Ok(value) => value,
                Err(_) => {
                    line_offset = line_offset.saturating_add(read as u64);
                    continue;
                }
            };
            if let Some(event) = value.as_object() {
                self.burn_store
                    .record_codex_event(event, OffsetDateTime::now_utc())?;
            }
            line_offset = line_offset.saturating_add(read as u64);
        }
        let scanned_at = format_timestamp(OffsetDateTime::now_utc())?;
        self.open()?.execute(
            r#"
            INSERT INTO burn_scan_offsets (artifact_path, byte_offset, mtime_ns, scanned_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(artifact_path) DO UPDATE SET
              byte_offset = excluded.byte_offset,
              mtime_ns = excluded.mtime_ns,
              scanned_at = excluded.scanned_at
            "#,
            params![artifact_path.as_ref(), line_offset, mtime_ns, scanned_at],
        )?;
        Ok(())
    }

    fn ensure_bootstrap_for_artifact(&self, artifact: &Artifact, offset: u64) -> Result<()> {
        let provider = match artifact.provider.as_str() {
            "claude" => Provider::Claude,
            "codex" | "codex-fork" => Provider::Codex,
            _ => return Ok(()),
        };
        let now = format_timestamp(OffsetDateTime::now_utc())?;
        let connection = self.open()?;
        let open_from = connection.query_row(
            r#"
                SELECT MIN(burn.window_start)
                FROM burn_samples AS burn
                JOIN accounts ON accounts.account_key = burn.account_key
                WHERE accounts.provider = ?1 AND burn.resets_at > ?2
                "#,
            params![provider.as_str(), now],
            |row| row.get::<_, Option<String>>(0),
        )?;
        let open_from = open_from
            .and_then(|value| parse_timestamp(&value).ok())
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let Some(earliest) = earliest_message_at_or_after(artifact, offset, open_from)? else {
            return Ok(());
        };
        self.identity_store
            .ensure_bootstrap_interval(provider, earliest)?;
        Ok(())
    }

    fn resolve_claude_contribution(
        &self,
        tx: &Transaction<'_>,
        parsed: ParsedMessage,
        seat_by_source: &BTreeMap<(String, String), String>,
        seat_meta: &BTreeMap<String, UsageSeatMetadata>,
    ) -> Result<Option<IngestOutcome>> {
        let Some(account_key) = account_at(tx, Provider::Claude, parsed.timestamp)? else {
            if predates_first_account_interval(tx, Provider::Claude, parsed.timestamp)? {
                return Ok(None);
            }
            bail!(
                "Claude message {} has no account timeline attribution",
                parsed.message_id
            );
        };
        let seat_id = seat_by_source
            .get(&("claude".to_owned(), parsed.source_ref.clone()))
            .cloned()
            .unwrap_or_else(|| "unassigned".to_owned());
        let project_key = seat_meta
            .get(&seat_id)
            .map(|seat| seat.project_key.clone())
            .unwrap_or_else(|| UsageSeatMetadata::resolve_project_key(&parsed.cwd));
        let credit_metered = credit_metered(tx, &account_key, &parsed.model, parsed.timestamp)?;
        let contribution = Contribution {
            message_id: parsed.message_id,
            request_id: parsed.request_id,
            is_sidechain: parsed.is_sidechain,
            has_speed: parsed.has_speed,
            total_tokens: parsed.tokens.total(),
            seat_id,
            account_key,
            project_key,
            source_ref: parsed.source_ref,
            source_seq: None,
            bucket_ts: minute_timestamp(parsed.timestamp)?,
            timestamp: parsed.timestamp,
            model: parsed.model,
            effort: None,
            tokens: parsed.tokens,
            credit_metered,
        };
        Ok(Some(ingest_contribution(tx, &contribution)?))
    }

    fn process_codex_event(
        &self,
        tx: &Transaction<'_>,
        artifact: &Artifact,
        event: CodexEvent,
        seat_by_source: &BTreeMap<(String, String), String>,
        seat_meta: &BTreeMap<String, UsageSeatMetadata>,
        artifact_project_key: Option<&str>,
    ) -> Result<Option<IngestOutcome>> {
        match event {
            CodexEvent::Settings {
                thread_id,
                source_seq,
                model,
                effort,
            } => {
                tx.execute(
                    r#"
                    INSERT INTO codex_thread_settings (thread_id, source_seq, model, effort)
                    VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT(thread_id, source_seq) DO UPDATE SET
                      model = excluded.model,
                      effort = excluded.effort
                    "#,
                    params![thread_id, source_seq, model, effort],
                )?;
                Ok(None)
            }
            CodexEvent::Cumulative {
                thread_id,
                turn_id,
                source_seq,
                timestamp,
                totals,
            } => {
                let previous = load_codex_cursor(tx, &thread_id)?;
                if previous
                    .as_ref()
                    .is_some_and(|cursor| source_seq <= cursor.last_seq)
                {
                    return Ok(Some(IngestOutcome::Ignored));
                }
                let tokens = totals.delta(previous.as_ref())?;
                let raw_provider = artifact.provider.as_str();
                let seat_id = seat_by_source
                    .get(&(raw_provider.to_owned(), thread_id.clone()))
                    .or_else(|| {
                        artifact.provider_session_ids.iter().find_map(|source| {
                            seat_by_source.get(&(raw_provider.to_owned(), source.clone()))
                        })
                    })
                    .cloned()
                    .unwrap_or_else(|| "unassigned".to_owned());
                let seat = seat_meta.get(&seat_id);
                let settings = codex_settings_at(tx, &thread_id, source_seq)?;
                let model = settings
                    .as_ref()
                    .map(|(model, _)| model.clone())
                    .or_else(|| seat.and_then(|seat| normalized_model(seat.model.as_deref())))
                    .or_else(|| self.default_model(raw_provider))
                    .context("Codex token event has no resolvable model")?;
                let effort = settings
                    .and_then(|(_, effort)| effort)
                    .or_else(|| seat.and_then(|seat| seat.effort.clone()));
                let Some(account_key) = account_at(tx, Provider::Codex, timestamp)? else {
                    if predates_first_account_interval(tx, Provider::Codex, timestamp)? {
                        save_codex_cursor(tx, &thread_id, &artifact.path, source_seq, &totals)?;
                        return Ok(Some(IngestOutcome::Ignored));
                    }
                    bail!(
                        "Codex token event {thread_id}:{source_seq} has no account timeline attribution"
                    );
                };
                let project_key = seat
                    .map(|seat| seat.project_key.clone())
                    .or_else(|| artifact_project_key.map(ToOwned::to_owned))
                    .unwrap_or_else(|| "unassigned".to_owned());
                let credit_metered = credit_metered(tx, &account_key, &model, timestamp)?;
                let contribution = Contribution {
                    message_id: format!("codex:{thread_id}:{source_seq}"),
                    request_id: turn_id,
                    is_sidechain: false,
                    has_speed: false,
                    total_tokens: tokens.total(),
                    seat_id,
                    account_key,
                    project_key,
                    source_ref: thread_id.clone(),
                    source_seq: Some(source_seq),
                    bucket_ts: minute_timestamp(timestamp)?,
                    timestamp,
                    model,
                    effort,
                    tokens,
                    credit_metered,
                };
                let outcome = if contribution.total_tokens == 0 {
                    IngestOutcome::Ignored
                } else {
                    ingest_contribution(tx, &contribution)?
                };
                save_codex_cursor(tx, &thread_id, &artifact.path, source_seq, &totals)?;
                Ok(Some(outcome))
            }
        }
    }

    fn default_model(&self, provider: &str) -> Option<String> {
        let value = match provider {
            "claude" => self.model_defaults.claude.as_deref(),
            "codex" => self.model_defaults.codex.as_deref(),
            "codex-fork" => self.model_defaults.codex_fork.as_deref(),
            _ => None,
        };
        normalized_model(value)
    }

    fn materialize_pending_windows(&self) -> Result<MaterializationSummary> {
        let windows = self.pending_burn_windows()?;
        let mut summary = MaterializationSummary {
            windows_examined: windows.len(),
            ..MaterializationSummary::default()
        };
        let window_count = windows.len();
        for (index, window) in windows.into_iter().enumerate() {
            let ids = self.pending_message_ids_for_window(&window)?;
            summary.messages_selected += ids.len();
            self.materialize_window_messages(&window, &ids)?;
            self.mark_burn_window_materialized(&window)?;
            if index + 1 < window_count {
                thread::sleep(MATERIALIZATION_BATCH_PAUSE);
            }
        }
        Ok(summary)
    }

    fn pending_burn_windows(&self) -> Result<Vec<BurnWindow>> {
        let connection = self.open()?;
        let mut statement = connection.prepare(
            r#"
                SELECT b.account_key, b.window_kind, COALESCE(b.window_scope, ''),
                       b.window_start, b.resets_at
                FROM burn_samples AS b
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM burn_window_materialization AS completed
                    WHERE completed.account_key = b.account_key
                      AND completed.window_kind = b.window_kind
                      AND completed.window_scope = COALESCE(b.window_scope, '')
                      AND completed.window_start = b.window_start
                      AND completed.resets_at = b.resets_at
                )
                GROUP BY b.account_key, b.window_kind, COALESCE(b.window_scope, ''),
                         b.window_start, b.resets_at
                ORDER BY b.account_key, b.window_start, b.window_kind, b.window_scope
                "#,
        )?;
        let windows = statement
            .query_map([], |row| {
                Ok(BurnWindow {
                    account_key: row.get(0)?,
                    kind: row.get(1)?,
                    scope: row.get(2)?,
                    start: row.get(3)?,
                    resets_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(anyhow::Error::from)?;
        Ok(windows)
    }

    fn pending_message_ids_for_window(&self, window: &BurnWindow) -> Result<Vec<i64>> {
        let connection = self.open()?;
        let mut statement = connection.prepare(
            r#"
                SELECT ledger.msg_id
                FROM message_ledger AS ledger INDEXED BY idx_ledger_window_materialization
                WHERE ledger.account_key = ?1
                  AND ledger.recorded_at >= ?2
                  AND ledger.recorded_at < ?3
                  AND (
                        ?4 != 'weekly_scoped'
                     OR (
                          ?5 != ''
                      AND (
                             LOWER(ledger.model) = LOWER(?5)
                          OR INSTR(LOWER(ledger.model), LOWER(?5)) > 0
                          OR INSTR(LOWER(?5), LOWER(ledger.model)) > 0
                      )
                     )
                  )
                  AND NOT EXISTS (
                      SELECT 1
                      FROM message_window AS mapped
                      WHERE mapped.msg_id = ledger.msg_id
                        AND mapped.window_kind = ?4
                        AND mapped.window_start = ?2
                  )
                ORDER BY ledger.recorded_at, ledger.msg_id
                "#,
        )?;
        let ids = statement
            .query_map(
                params![
                    window.account_key,
                    window.start,
                    window.resets_at,
                    window.kind,
                    window.scope,
                ],
                |row| row.get::<_, i64>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(anyhow::Error::from)?;
        Ok(ids)
    }

    fn materialize_window_messages(&self, window: &BurnWindow, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let message_window = MessageWindow {
            kind: window.kind.clone(),
            start: window.start.clone(),
        };
        let mut connection = self.open()?;
        for (index, batch) in ids.chunks(LEDGER_WRITE_BATCH_SIZE).enumerate() {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for msg_id in batch {
                let contribution = load_contribution(&tx, *msg_id)?;
                materialize_contribution_for_window(&tx, *msg_id, &contribution, &message_window)?;
            }
            tx.commit()?;
            if index + 1 < ids.len().div_ceil(LEDGER_WRITE_BATCH_SIZE) {
                thread::sleep(MATERIALIZATION_BATCH_PAUSE);
            }
        }
        Ok(())
    }

    fn mark_burn_window_materialized(&self, window: &BurnWindow) -> Result<()> {
        let mut connection = self.open()?;
        let tx = connection.transaction()?;
        tx.execute(
            r#"
            INSERT OR IGNORE INTO burn_window_materialization (
              account_key, window_kind, window_scope, window_start, resets_at, materialized_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                window.account_key,
                window.kind,
                window.scope,
                window.start,
                window.resets_at,
                format_timestamp(OffsetDateTime::now_utc())?,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn save_scan_offset(
    transaction: &Transaction<'_>,
    artifact_path: &Path,
    byte_offset: u64,
    mtime_ns: i64,
) -> Result<()> {
    let scanned_at = format_timestamp(OffsetDateTime::now_utc())?;
    transaction.execute(
        r#"
        INSERT INTO scan_offsets (artifact_path, byte_offset, last_uuid, mtime_ns, scanned_at)
        VALUES (?1, ?2, NULL, ?3, ?4)
        ON CONFLICT(artifact_path) DO UPDATE SET
          byte_offset = excluded.byte_offset,
          mtime_ns = excluded.mtime_ns,
          scanned_at = excluded.scanned_at
        "#,
        params![
            artifact_path.to_string_lossy(),
            byte_offset,
            mtime_ns,
            scanned_at
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
struct ArtifactBinding {
    seat_id: String,
    provider: String,
    provider_session_id: String,
    artifact_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Artifact {
    provider: String,
    path: PathBuf,
    provider_session_ids: BTreeSet<String>,
}

fn expand_artifacts(bindings: &[ArtifactBinding]) -> Vec<Artifact> {
    let mut artifacts = BTreeMap::<(String, PathBuf), BTreeSet<String>>::new();
    for binding in bindings {
        let mut paths = vec![binding.artifact_path.clone()];
        if binding.provider == "claude" {
            paths.extend(claude_sibling_artifacts(
                &binding.artifact_path,
                &binding.provider_session_id,
            ));
        }
        for path in paths {
            artifacts
                .entry((binding.provider.clone(), path))
                .or_default()
                .insert(binding.provider_session_id.clone());
        }
    }
    artifacts
        .into_iter()
        .map(|((provider, path), provider_session_ids)| Artifact {
            provider,
            path,
            provider_session_ids,
        })
        .collect()
}

fn codex_artifact_project_key(path: &Path) -> Option<String> {
    let reader = BufReader::new(fs::File::open(path).ok()?);
    for line in reader.lines().take(100).flatten() {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = value.get("payload").and_then(Value::as_object);
        let settings = payload
            .and_then(|payload| payload.get("threadSettings"))
            .and_then(Value::as_object);
        let cwd = value
            .get("cwd")
            .and_then(Value::as_str)
            .or_else(|| {
                payload
                    .and_then(|payload| payload.get("cwd"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                payload
                    .and_then(|payload| payload.get("workingDirectory"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                payload
                    .and_then(|payload| payload.get("working_dir"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                settings
                    .and_then(|settings| settings.get("cwd"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                settings
                    .and_then(|settings| settings.get("workingDirectory"))
                    .and_then(Value::as_str)
            })
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty());
        if let Some(cwd) = cwd {
            return Some(UsageSeatMetadata::resolve_project_key(cwd));
        }
    }
    None
}

fn claude_sibling_artifacts(path: &Path, session_id: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if path.file_name().and_then(|value| value.to_str()) == Some("chat.jsonl") {
        if let Some(parent) = path.parent() {
            roots.push(parent.to_path_buf());
        }
    } else if path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        == Some("subagents")
    {
        if let Some(root) = path.parent().and_then(Path::parent) {
            roots.push(root.to_path_buf());
        }
    } else if let Some(parent) = path.parent() {
        roots.push(parent.join(session_id));
    }
    roots
        .into_iter()
        .flat_map(|root| jsonl_files_recursive(&root))
        .filter(|candidate| candidate != path)
        .collect()
}

fn jsonl_files_recursive(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TokenBuckets {
    input: i64,
    output: i64,
    reasoning: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    cache_read: i64,
}

impl TokenBuckets {
    fn total(&self) -> i64 {
        self.input + self.output + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }

    fn validate(&self) -> Result<()> {
        if [
            self.input,
            self.output,
            self.reasoning,
            self.cache_write_5m,
            self.cache_write_1h,
            self.cache_read,
        ]
        .into_iter()
        .any(|value| value < 0)
        {
            bail!("token buckets must not be negative");
        }
        if self.reasoning > self.output {
            bail!("reasoning tokens must be a subset of output tokens");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ParsedMessage {
    message_id: String,
    request_id: Option<String>,
    is_sidechain: bool,
    has_speed: bool,
    timestamp: OffsetDateTime,
    source_ref: String,
    cwd: String,
    model: String,
    tokens: TokenBuckets,
}

fn parse_claude_line(line: &[u8], path: &Path) -> Result<Option<ParsedMessage>> {
    if !line
        .windows(b"\"usage\":{".len())
        .any(|bytes| bytes == b"\"usage\":{")
    {
        return Ok(None);
    }
    let value: Value = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if has_explicit_null(&value) {
        return Ok(None);
    }
    let Some(root) = value.as_object() else {
        return Ok(None);
    };
    if root
        .get("version")
        .and_then(Value::as_str)
        .is_some_and(|version| !valid_semver_prefix(version))
    {
        return Ok(None);
    }
    let Some(message) = root.get("message").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(usage) = message.get("usage").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(timestamp) =
        json_text(root, "timestamp").and_then(|value| OffsetDateTime::parse(&value, &Rfc3339).ok())
    else {
        return Ok(None);
    };
    let required = |object: &Map<String, Value>, key: &str| {
        json_text(object, key).filter(|value| !value.is_empty())
    };
    let Some(message_id) = required(message, "id") else {
        return Ok(None);
    };
    let Some(source_ref) = required(root, "sessionId") else {
        return Ok(None);
    };
    let Some(model) = required(message, "model") else {
        return Ok(None);
    };
    let request_id = if root.contains_key("requestId") {
        let Some(request_id) = json_text(root, "requestId") else {
            return Ok(None);
        };
        Some(request_id)
    } else {
        None
    };
    let cwd = if root.contains_key("cwd") {
        let Some(cwd) = json_text(root, "cwd") else {
            return Ok(None);
        };
        cwd
    } else {
        String::new()
    };
    let cache_creation = usage.get("cache_creation").and_then(Value::as_object);
    let (cache_write_5m, cache_write_1h) = if let Some(cache_creation) = cache_creation {
        (
            json_i64(cache_creation.get("ephemeral_5m_input_tokens")).unwrap_or(0),
            json_i64(cache_creation.get("ephemeral_1h_input_tokens")).unwrap_or(0),
        )
    } else {
        (
            json_i64(usage.get("cache_creation_input_tokens")).unwrap_or(0),
            0,
        )
    };
    let tokens = TokenBuckets {
        input: json_i64(usage.get("input_tokens")).unwrap_or(0),
        output: json_i64(usage.get("output_tokens")).unwrap_or(0),
        reasoning: 0,
        cache_write_5m,
        cache_write_1h,
        cache_read: json_i64(usage.get("cache_read_input_tokens")).unwrap_or(0),
    };
    tokens.validate()?;
    let is_sidechain = root
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            path.components()
                .any(|component| component.as_os_str() == "subagents")
        });
    let has_speed = usage.get("speed").is_some_and(|value| !value.is_null())
        || root.get("speed").is_some_and(|value| !value.is_null());
    Ok(Some(ParsedMessage {
        message_id,
        request_id,
        is_sidechain,
        has_speed,
        timestamp,
        source_ref,
        cwd,
        model,
        tokens,
    }))
}

const NULL_REJECT_FIELDS: [&str; 12] = [
    "id",
    "cwd",
    "model",
    "speed",
    "costUSD",
    "version",
    "sessionId",
    "requestId",
    "isApiErrorMessage",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
    "usage",
];

fn has_explicit_null(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (NULL_REJECT_FIELDS.contains(&key.as_str()) && value.is_null())
                || has_explicit_null(value)
        }),
        Value::Array(values) => values.iter().any(has_explicit_null),
        _ => false,
    }
}

fn valid_semver_prefix(value: &str) -> bool {
    let prefix = value
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .next()
        .unwrap_or_default();
    let mut parts = prefix.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(a), Some(b), Some(c))
            if !a.is_empty()
                && !b.is_empty()
                && !c.is_empty()
                && a.chars().all(|value| value.is_ascii_digit())
                && b.chars().all(|value| value.is_ascii_digit())
                && c.chars().all(|value| value.is_ascii_digit())
    )
}

#[derive(Debug, Clone)]
enum CodexEvent {
    Settings {
        thread_id: String,
        source_seq: i64,
        model: String,
        effort: Option<String>,
    },
    Cumulative {
        thread_id: String,
        turn_id: Option<String>,
        source_seq: i64,
        timestamp: OffsetDateTime,
        totals: CodexTotals,
    },
}

#[derive(Debug, Clone, Default)]
struct CodexTotals {
    input: i64,
    cached_input: i64,
    cache_write: i64,
    output: i64,
    reasoning: i64,
    total: i64,
}

impl CodexTotals {
    fn validate(&self) -> Result<()> {
        if [
            self.input,
            self.cached_input,
            self.cache_write,
            self.output,
            self.reasoning,
            self.total,
        ]
        .into_iter()
        .any(|value| value < 0)
        {
            bail!("Codex cumulative totals must not be negative");
        }
        if self.input + self.output != self.total {
            bail!("Codex input + output must equal total");
        }
        if self.cached_input + self.cache_write > self.input {
            bail!("Codex cached and cache-write tokens must be nested in input");
        }
        if self.reasoning > self.output {
            bail!("Codex reasoning tokens must be nested in output");
        }
        Ok(())
    }

    fn delta(&self, previous: Option<&CodexCursor>) -> Result<TokenBuckets> {
        self.validate()?;
        let previous = previous.cloned().unwrap_or_default();
        let delta = CodexTotals {
            input: self.input - previous.last_input_tokens,
            cached_input: self.cached_input - previous.last_cached_input,
            cache_write: self.cache_write - previous.last_cache_write,
            output: self.output - previous.last_output_tokens,
            reasoning: self.reasoning - previous.last_reasoning,
            total: self.total - previous.last_total_tokens,
        };
        delta.validate()?;
        let tokens = TokenBuckets {
            input: delta.input - delta.cached_input - delta.cache_write,
            output: delta.output,
            reasoning: delta.reasoning,
            cache_write_5m: delta.cache_write,
            cache_write_1h: 0,
            cache_read: delta.cached_input,
        };
        tokens.validate()?;
        if tokens.total() != delta.total {
            bail!("normalized Codex buckets do not equal cumulative total delta");
        }
        Ok(tokens)
    }
}

fn parse_codex_line(
    line: &[u8],
    fallback_seq: i64,
    fallback_thread_id: Option<&str>,
) -> Result<Option<CodexEvent>> {
    let value: Value = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let Some(root) = value.as_object() else {
        return Ok(None);
    };
    let source_seq = json_i64(root.get("seq")).unwrap_or(fallback_seq);
    let event_type = json_text(root, "event_type")
        .or_else(|| json_text(root, "type"))
        .unwrap_or_default();
    let payload = root
        .get("payload")
        .and_then(Value::as_object)
        .unwrap_or(root);

    if event_type == "thread/settings/updated" {
        let thread_id =
            json_text(payload, "threadId").or_else(|| fallback_thread_id.map(ToOwned::to_owned));
        let settings = payload
            .get("threadSettings")
            .and_then(Value::as_object)
            .unwrap_or(payload);
        let Some((thread_id, model)) = thread_id.zip(json_text(settings, "model")) else {
            return Ok(None);
        };
        return Ok(Some(CodexEvent::Settings {
            thread_id,
            source_seq,
            model,
            effort: json_text(settings, "effort"),
        }));
    }
    if event_type == "turn_context" {
        let Some((thread_id, model)) = fallback_thread_id
            .map(ToOwned::to_owned)
            .zip(json_text(payload, "model"))
        else {
            return Ok(None);
        };
        return Ok(Some(CodexEvent::Settings {
            thread_id,
            source_seq,
            model,
            effort: json_text(payload, "effort"),
        }));
    }

    let (usage, thread_id, turn_id) = if matches!(
        event_type.as_str(),
        "thread/tokenUsage/updated" | "tokenUsage/updated"
    ) {
        let Some(usage) = payload.get("tokenUsage").and_then(Value::as_object) else {
            return Ok(None);
        };
        let Some(total) = usage.get("total").and_then(Value::as_object) else {
            return Ok(None);
        };
        (
            total,
            json_text(payload, "threadId")
                .or_else(|| json_text(root, "threadId"))
                .or_else(|| fallback_thread_id.map(ToOwned::to_owned)),
            json_text(payload, "turnId").or_else(|| json_text(root, "turnId")),
        )
    } else if event_type == "event_msg"
        && json_text(payload, "type").as_deref() == Some("token_count")
    {
        let info = payload.get("info").and_then(Value::as_object);
        let Some(total) = info
            .and_then(|info| info.get("total_token_usage"))
            .and_then(Value::as_object)
        else {
            return Ok(None);
        };
        (
            total,
            fallback_thread_id.map(ToOwned::to_owned),
            json_text(payload, "turn_id"),
        )
    } else {
        return Ok(None);
    };
    let Some(thread_id) = thread_id else {
        return Ok(None);
    };
    let Some(timestamp) = json_text(root, "ts")
        .or_else(|| json_text(root, "timestamp"))
        .and_then(|value| OffsetDateTime::parse(&value, &Rfc3339).ok())
    else {
        return Ok(None);
    };
    let totals = CodexTotals {
        input: json_i64_any(usage, &["inputTokens", "input_tokens"]).unwrap_or(0),
        cached_input: json_i64_any(usage, &["cachedInputTokens", "cached_input_tokens"])
            .unwrap_or(0),
        cache_write: json_i64_any(
            usage,
            &["cacheWriteInputTokens", "cache_write_input_tokens"],
        )
        .unwrap_or(0),
        output: json_i64_any(usage, &["outputTokens", "output_tokens"]).unwrap_or(0),
        reasoning: json_i64_any(usage, &["reasoningOutputTokens", "reasoning_output_tokens"])
            .unwrap_or(0),
        total: json_i64_any(usage, &["totalTokens", "total_tokens"]).unwrap_or(0),
    };
    totals.validate()?;
    Ok(Some(CodexEvent::Cumulative {
        thread_id,
        turn_id,
        source_seq,
        timestamp,
        totals,
    }))
}

#[derive(Debug, Clone)]
struct Contribution {
    message_id: String,
    request_id: Option<String>,
    is_sidechain: bool,
    has_speed: bool,
    total_tokens: i64,
    seat_id: String,
    account_key: String,
    project_key: String,
    source_ref: String,
    source_seq: Option<i64>,
    bucket_ts: String,
    timestamp: OffsetDateTime,
    model: String,
    effort: Option<String>,
    tokens: TokenBuckets,
    credit_metered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngestOutcome {
    Inserted,
    Replaced,
    Ignored,
}

fn ingest_contribution(tx: &Transaction<'_>, candidate: &Contribution) -> Result<IngestOutcome> {
    let exact_key = alias_key(
        "exact",
        &candidate.message_id,
        candidate.request_id.as_deref(),
    );
    let loose_key = alias_key("loose", &candidate.message_id, None);
    let exact_id = alias_target(tx, &exact_key)?;
    let matched_id = if exact_id.is_some() {
        exact_id
    } else if let Some(loose_id) = alias_target(tx, &loose_key)? {
        let incumbent_sidechain = tx.query_row(
            "SELECT is_sidechain FROM message_ledger WHERE msg_id = ?1",
            [loose_id],
            |row| row.get::<_, bool>(0),
        )?;
        (incumbent_sidechain != candidate.is_sidechain).then_some(loose_id)
    } else {
        None
    };

    let (msg_id, outcome) = if let Some(msg_id) = matched_id {
        let incumbent = load_contribution(tx, msg_id)?;
        if should_replace(candidate, &incumbent) {
            reverse_contribution(tx, msg_id, &incumbent)?;
            overwrite_contribution(tx, msg_id, candidate)?;
            materialize_contribution(tx, msg_id, candidate)?;
            (msg_id, IngestOutcome::Replaced)
        } else {
            (msg_id, IngestOutcome::Ignored)
        }
    } else {
        let msg_id = insert_contribution(tx, candidate)?;
        materialize_contribution(tx, msg_id, candidate)?;
        (msg_id, IngestOutcome::Inserted)
    };
    upsert_alias(tx, &exact_key, "exact", msg_id)?;
    upsert_alias(tx, &loose_key, "loose", msg_id)?;
    Ok(outcome)
}

fn should_replace(candidate: &Contribution, incumbent: &Contribution) -> bool {
    if candidate.is_sidechain != incumbent.is_sidechain {
        return incumbent.is_sidechain;
    }
    if candidate.total_tokens != incumbent.total_tokens {
        return candidate.total_tokens > incumbent.total_tokens;
    }
    candidate.has_speed && !incumbent.has_speed
}

fn insert_contribution(tx: &Transaction<'_>, value: &Contribution) -> Result<i64> {
    let recorded_at = format_timestamp(value.timestamp)?;
    tx.execute(
        r#"
        INSERT INTO message_ledger (
          message_id, request_id, is_sidechain, has_speed, total_tokens, seat_id,
          account_key, project_key, source_ref, source_seq, bucket_ts, model, effort,
          input_tokens, output_tokens, reasoning_tokens, cache_write_5m, cache_write_1h,
          cache_read_tokens, credit_metered, recorded_at
        ) VALUES (
          ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
          ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21
        )
        "#,
        params![
            value.message_id,
            value.request_id,
            value.is_sidechain,
            value.has_speed,
            value.total_tokens,
            value.seat_id,
            value.account_key,
            value.project_key,
            value.source_ref,
            value.source_seq,
            value.bucket_ts,
            value.model,
            value.effort,
            value.tokens.input,
            value.tokens.output,
            value.tokens.reasoning,
            value.tokens.cache_write_5m,
            value.tokens.cache_write_1h,
            value.tokens.cache_read,
            value.credit_metered,
            recorded_at,
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

fn overwrite_contribution(tx: &Transaction<'_>, msg_id: i64, value: &Contribution) -> Result<()> {
    tx.execute(
        r#"
        UPDATE message_ledger SET
          message_id = ?1, request_id = ?2, is_sidechain = ?3, has_speed = ?4,
          total_tokens = ?5, seat_id = ?6, account_key = ?7, project_key = ?8,
          source_ref = ?9, source_seq = ?10, bucket_ts = ?11, model = ?12,
          effort = ?13, input_tokens = ?14, output_tokens = ?15,
          reasoning_tokens = ?16, cache_write_5m = ?17, cache_write_1h = ?18,
          cache_read_tokens = ?19, credit_metered = ?20, recorded_at = ?21
        WHERE msg_id = ?22
        "#,
        params![
            value.message_id,
            value.request_id,
            value.is_sidechain,
            value.has_speed,
            value.total_tokens,
            value.seat_id,
            value.account_key,
            value.project_key,
            value.source_ref,
            value.source_seq,
            value.bucket_ts,
            value.model,
            value.effort,
            value.tokens.input,
            value.tokens.output,
            value.tokens.reasoning,
            value.tokens.cache_write_5m,
            value.tokens.cache_write_1h,
            value.tokens.cache_read,
            value.credit_metered,
            format_timestamp(value.timestamp)?,
            msg_id,
        ],
    )?;
    Ok(())
}

fn load_contribution(tx: &Transaction<'_>, msg_id: i64) -> Result<Contribution> {
    tx.query_row(
        r#"
        SELECT message_id, request_id, is_sidechain, has_speed, total_tokens, seat_id,
               account_key, project_key, source_ref, source_seq, bucket_ts, model, effort,
               input_tokens, output_tokens, reasoning_tokens, cache_write_5m,
               cache_write_1h, cache_read_tokens, credit_metered, recorded_at
        FROM message_ledger WHERE msg_id = ?1
        "#,
        [msg_id],
        |row| {
            let recorded_at = row.get::<_, String>(20)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, i64>(15)?,
                row.get::<_, i64>(16)?,
                row.get::<_, i64>(17)?,
                row.get::<_, i64>(18)?,
                row.get::<_, bool>(19)?,
                recorded_at,
            ))
        },
    )
    .map_err(Into::into)
    .and_then(
        |(
            message_id,
            request_id,
            is_sidechain,
            has_speed,
            total_tokens,
            seat_id,
            account_key,
            project_key,
            source_ref,
            source_seq,
            bucket_ts,
            model,
            effort,
            input,
            output,
            reasoning,
            cache_write_5m,
            cache_write_1h,
            cache_read,
            credit_metered,
            recorded_at,
        )| {
            Ok(Contribution {
                message_id,
                request_id,
                is_sidechain,
                has_speed,
                total_tokens,
                seat_id,
                account_key,
                project_key,
                source_ref,
                source_seq,
                bucket_ts,
                timestamp: parse_timestamp(&recorded_at)?,
                model,
                effort,
                tokens: TokenBuckets {
                    input,
                    output,
                    reasoning,
                    cache_write_5m,
                    cache_write_1h,
                    cache_read,
                },
                credit_metered,
            })
        },
    )
}

fn materialize_contribution(tx: &Transaction<'_>, msg_id: i64, value: &Contribution) -> Result<()> {
    for window in windows_for(tx, value)? {
        materialize_contribution_for_window(tx, msg_id, value, &window)?;
    }
    Ok(())
}

fn materialize_contribution_for_window(
    tx: &Transaction<'_>,
    msg_id: i64,
    value: &Contribution,
    window: &MessageWindow,
) -> Result<()> {
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO message_window (msg_id, window_kind, window_start) VALUES (?1, ?2, ?3)",
        params![msg_id, window.kind, window.start],
    )?;
    if inserted > 0 && value.model != "<synthetic>" {
        apply_rollup(tx, value, window, 1)?;
    }
    Ok(())
}

fn reverse_contribution(tx: &Transaction<'_>, msg_id: i64, value: &Contribution) -> Result<()> {
    let windows = tx
        .prepare(
            "SELECT window_kind, window_start FROM message_window WHERE msg_id = ?1 ORDER BY window_kind, window_start",
        )?
        .query_map([msg_id], |row| {
            Ok(MessageWindow {
                kind: row.get(0)?,
                start: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if value.model != "<synthetic>" {
        for window in &windows {
            apply_rollup(tx, value, window, -1)?;
        }
    }
    tx.execute("DELETE FROM message_window WHERE msg_id = ?1", [msg_id])?;
    Ok(())
}

#[derive(Debug, Clone)]
struct MessageWindow {
    kind: String,
    start: String,
}

fn windows_for(tx: &Transaction<'_>, value: &Contribution) -> Result<Vec<MessageWindow>> {
    let timestamp = format_timestamp(value.timestamp)?;
    let rows = tx
        .prepare(
            r#"
            SELECT DISTINCT window_kind, window_start, window_scope
            FROM burn_samples
            WHERE account_key = ?1 AND window_start <= ?2 AND ?2 < resets_at
            ORDER BY window_kind, window_start
            "#,
        )?
        .query_map(params![value.account_key, timestamp], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(kind, _, scope)| {
            kind != "weekly_scoped"
                || scope
                    .as_deref()
                    .is_some_and(|scope| model_matches_scope(&value.model, scope))
        })
        .map(|(kind, start, _)| MessageWindow { kind, start })
        .collect())
}

fn apply_rollup(
    tx: &Transaction<'_>,
    value: &Contribution,
    window: &MessageWindow,
    direction: i64,
) -> Result<()> {
    let updated_at = format_timestamp(OffsetDateTime::now_utc())?;
    tx.execute(
        r#"
        INSERT INTO seat_tokens (
          seat_id, account_key, project_key, window_kind, window_start, bucket_ts,
          model, effort, credit_metered, input_tokens, output_tokens, reasoning_tokens,
          cache_write_5m, cache_write_1h, cache_read_tokens, message_count, updated_at
        ) VALUES (
          ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
        )
        ON CONFLICT(seat_id, account_key, project_key, window_kind, window_start,
                    bucket_ts, model, credit_metered) DO UPDATE SET
          effort = COALESCE(excluded.effort, seat_tokens.effort),
          input_tokens = seat_tokens.input_tokens + excluded.input_tokens,
          output_tokens = seat_tokens.output_tokens + excluded.output_tokens,
          reasoning_tokens = seat_tokens.reasoning_tokens + excluded.reasoning_tokens,
          cache_write_5m = seat_tokens.cache_write_5m + excluded.cache_write_5m,
          cache_write_1h = seat_tokens.cache_write_1h + excluded.cache_write_1h,
          cache_read_tokens = seat_tokens.cache_read_tokens + excluded.cache_read_tokens,
          message_count = seat_tokens.message_count + excluded.message_count,
          updated_at = excluded.updated_at
        "#,
        params![
            value.seat_id,
            value.account_key,
            value.project_key,
            window.kind,
            window.start,
            value.bucket_ts,
            value.model,
            value.effort,
            value.credit_metered,
            value.tokens.input * direction,
            value.tokens.output * direction,
            value.tokens.reasoning * direction,
            value.tokens.cache_write_5m * direction,
            value.tokens.cache_write_1h * direction,
            value.tokens.cache_read * direction,
            direction,
            updated_at,
        ],
    )?;
    tx.execute(
        r#"
        DELETE FROM seat_tokens
        WHERE seat_id = ?1 AND account_key = ?2 AND project_key = ?3
          AND window_kind = ?4 AND window_start = ?5 AND bucket_ts = ?6
          AND model = ?7 AND credit_metered = ?8 AND message_count = 0
          AND input_tokens = 0 AND output_tokens = 0 AND reasoning_tokens = 0
          AND cache_write_5m = 0 AND cache_write_1h = 0 AND cache_read_tokens = 0
        "#,
        params![
            value.seat_id,
            value.account_key,
            value.project_key,
            window.kind,
            window.start,
            value.bucket_ts,
            value.model,
            value.credit_metered,
        ],
    )?;
    Ok(())
}

fn alias_key(kind: &str, message_id: &str, request_id: Option<&str>) -> String {
    let mut digest = Sha256::new();
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(message_id.as_bytes());
    digest.update([0]);
    if let Some(request_id) = request_id {
        digest.update(request_id.as_bytes());
    }
    format!("{kind}:{:x}", digest.finalize())
}

fn alias_target(tx: &Transaction<'_>, lookup_key: &str) -> Result<Option<i64>> {
    tx.query_row(
        "SELECT msg_id FROM message_alias WHERE lookup_key = ?1",
        [lookup_key],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn upsert_alias(tx: &Transaction<'_>, key: &str, kind: &str, msg_id: i64) -> Result<()> {
    tx.execute(
        r#"
        INSERT INTO message_alias (lookup_key, kind, msg_id) VALUES (?1, ?2, ?3)
        ON CONFLICT(lookup_key) DO UPDATE SET kind = excluded.kind, msg_id = excluded.msg_id
        "#,
        params![key, kind, msg_id],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
struct CodexCursor {
    last_seq: i64,
    last_input_tokens: i64,
    last_cached_input: i64,
    last_cache_write: i64,
    last_output_tokens: i64,
    last_reasoning: i64,
    last_total_tokens: i64,
}

fn load_codex_cursor(tx: &Transaction<'_>, thread_id: &str) -> Result<Option<CodexCursor>> {
    tx.query_row(
        r#"
        SELECT last_seq, last_input_tokens, last_cached_input, last_cache_write,
               last_output_tokens, last_reasoning, last_total_tokens
        FROM codex_thread_cursor WHERE thread_id = ?1
        "#,
        [thread_id],
        |row| {
            Ok(CodexCursor {
                last_seq: row.get(0)?,
                last_input_tokens: row.get(1)?,
                last_cached_input: row.get(2)?,
                last_cache_write: row.get(3)?,
                last_output_tokens: row.get(4)?,
                last_reasoning: row.get(5)?,
                last_total_tokens: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn save_codex_cursor(
    tx: &Transaction<'_>,
    thread_id: &str,
    artifact_path: &Path,
    source_seq: i64,
    totals: &CodexTotals,
) -> Result<()> {
    tx.execute(
        r#"
        INSERT INTO codex_thread_cursor (
          thread_id, artifact_path, last_seq, last_input_tokens, last_cached_input,
          last_cache_write, last_output_tokens, last_reasoning, last_total_tokens, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(thread_id) DO UPDATE SET
          artifact_path = excluded.artifact_path,
          last_seq = excluded.last_seq,
          last_input_tokens = excluded.last_input_tokens,
          last_cached_input = excluded.last_cached_input,
          last_cache_write = excluded.last_cache_write,
          last_output_tokens = excluded.last_output_tokens,
          last_reasoning = excluded.last_reasoning,
          last_total_tokens = excluded.last_total_tokens,
          updated_at = excluded.updated_at
        "#,
        params![
            thread_id,
            artifact_path.to_string_lossy(),
            source_seq,
            totals.input,
            totals.cached_input,
            totals.cache_write,
            totals.output,
            totals.reasoning,
            totals.total,
            format_timestamp(OffsetDateTime::now_utc())?,
        ],
    )?;
    Ok(())
}

fn ensure_codex_cursor_baselines(tx: &Transaction<'_>, artifact: &Artifact) -> Result<()> {
    if !matches!(artifact.provider.as_str(), "codex" | "codex-fork") {
        return Ok(());
    }
    for thread_id in &artifact.provider_session_ids {
        if load_codex_cursor(tx, thread_id)?.is_some() {
            continue;
        }
        let baseline = tx
            .query_row(
                r#"
                SELECT MAX(source_seq), SUM(input_tokens + cache_read_tokens + cache_write_5m + cache_write_1h),
                       SUM(cache_read_tokens), SUM(cache_write_5m + cache_write_1h),
                       SUM(output_tokens), SUM(reasoning_tokens), SUM(total_tokens)
                FROM message_ledger
                WHERE source_ref = ?1 AND source_seq IS NOT NULL
                "#,
                [thread_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    ))
                },
            )?;
        let Some(last_seq) = baseline.0 else {
            continue;
        };
        save_codex_cursor(
            tx,
            thread_id,
            &artifact.path,
            last_seq,
            &CodexTotals {
                input: baseline.1,
                cached_input: baseline.2,
                cache_write: baseline.3,
                output: baseline.4,
                reasoning: baseline.5,
                total: baseline.6,
            },
        )?;
    }
    Ok(())
}

fn codex_settings_at(
    tx: &Transaction<'_>,
    thread_id: &str,
    source_seq: i64,
) -> Result<Option<(String, Option<String>)>> {
    tx.query_row(
        r#"
        SELECT model, effort FROM codex_thread_settings
        WHERE thread_id = ?1 AND source_seq <= ?2
        ORDER BY source_seq DESC LIMIT 1
        "#,
        params![thread_id, source_seq],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

fn account_at(
    tx: &Transaction<'_>,
    provider: Provider,
    timestamp: OffsetDateTime,
) -> Result<Option<String>> {
    let timestamp = format_timestamp(timestamp)?;
    tx.query_row(
        r#"
        SELECT account_key FROM account_timeline
        WHERE provider = ?1 AND from_ts <= ?2 AND (to_ts IS NULL OR ?2 < to_ts)
        ORDER BY from_ts DESC LIMIT 1
        "#,
        params![provider.as_str(), timestamp],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn predates_first_account_interval(
    tx: &Transaction<'_>,
    provider: Provider,
    timestamp: OffsetDateTime,
) -> Result<bool> {
    let first_from = tx.query_row(
        "SELECT MIN(from_ts) FROM account_timeline WHERE provider = ?1",
        [provider.as_str()],
        |row| row.get::<_, Option<String>>(0),
    )?;
    Ok(first_from
        .as_deref()
        .map(parse_timestamp)
        .transpose()?
        .is_some_and(|first_from| timestamp < first_from))
}

fn credit_metered(
    tx: &Transaction<'_>,
    account_key: &str,
    model: &str,
    timestamp: OffsetDateTime,
) -> Result<bool> {
    let provider = tx
        .query_row(
            "SELECT provider FROM accounts WHERE account_key = ?1",
            [account_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(provider) = provider else {
        return Ok(false);
    };
    let (plan_tier, extra_usage_enabled) = account_metadata_at(tx, account_key, timestamp)?;
    let excluded_premium = provider == "claude"
        && plan_tier.as_deref().is_some_and(plan_excludes_premium)
        && scoped_model_at(tx, account_key, timestamp)?
            .as_deref()
            .is_some_and(|scope| model_matches_scope(model, scope));
    if excluded_premium {
        return Ok(true);
    }
    let timestamp = format_timestamp(timestamp)?;
    let samples = tx
        .prepare(
            r#"
            SELECT window_kind, window_scope, percent
            FROM burn_samples
            WHERE account_key = ?1 AND observed_at <= ?2
              AND window_start <= ?2 AND ?2 < resets_at
            ORDER BY observed_at DESC, id DESC
            "#,
        )?
        .query_map(params![account_key, timestamp], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut seen = BTreeSet::new();
    let exhausted = samples.into_iter().any(|(kind, scope, percent)| {
        let key = (kind.clone(), scope.clone());
        seen.insert(key)
            && (kind != "weekly_scoped"
                || scope
                    .as_deref()
                    .is_some_and(|scope| model_matches_scope(model, scope)))
            && percent >= 100.0
    });
    Ok(exhausted && extra_usage_enabled.unwrap_or(false))
}

fn account_metadata_at(
    tx: &Transaction<'_>,
    account_key: &str,
    timestamp: OffsetDateTime,
) -> Result<(Option<String>, Option<bool>)> {
    let timestamp = format_timestamp(timestamp)?;
    let historical = tx
        .query_row(
            r#"
            SELECT plan_tier, extra_usage_enabled
            FROM account_metadata_history
            WHERE account_key = ?1 AND observed_at <= ?2
            ORDER BY observed_at DESC LIMIT 1
            "#,
            params![account_key, timestamp],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some(historical) = historical {
        return Ok(historical);
    }
    let bootstrap = tx
        .query_row(
            r#"
            SELECT plan_tier, extra_usage_enabled
            FROM account_metadata_history
            WHERE account_key = ?1
            ORDER BY observed_at ASC LIMIT 1
            "#,
            [account_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some(bootstrap) = bootstrap {
        return Ok(bootstrap);
    }
    tx.query_row(
        "SELECT plan_tier, extra_usage_enabled FROM accounts WHERE account_key = ?1",
        [account_key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map(|metadata| metadata.unwrap_or((None, None)))
    .map_err(Into::into)
}

fn scoped_model_at(
    tx: &Transaction<'_>,
    account_key: &str,
    timestamp: OffsetDateTime,
) -> Result<Option<String>> {
    let timestamp = format_timestamp(timestamp)?;
    tx.query_row(
        r#"
        SELECT window_scope FROM burn_samples
        WHERE account_key = ?1 AND window_kind = 'weekly_scoped'
          AND observed_at <= ?2 AND window_scope IS NOT NULL
        ORDER BY observed_at DESC, id DESC LIMIT 1
        "#,
        params![account_key, timestamp],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn plan_excludes_premium(plan_tier: &str) -> bool {
    let plan = plan_tier.to_ascii_lowercase();
    plan == "pro" || plan.contains("standard_team") || plan.contains("team_standard")
}

fn model_matches_scope(model: &str, scope: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let scope = scope.to_ascii_lowercase();
    model == scope || model.contains(&scope) || scope.contains(&model)
}

fn earliest_message_at_or_after(
    artifact: &Artifact,
    offset: u64,
    open_from: OffsetDateTime,
) -> Result<Option<OffsetDateTime>> {
    let mut reader = BufReader::new(fs::File::open(&artifact.path)?);
    reader.seek(SeekFrom::Start(offset))?;
    let mut line_offset = offset;
    let mut line = Vec::new();
    let mut earliest: Option<OffsetDateTime> = None;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        let source_seq = i64::try_from(line_offset).unwrap_or(i64::MAX);
        line_offset = line_offset.saturating_add(read as u64);
        let timestamp = match artifact.provider.as_str() {
            "claude" => parse_claude_line(&line, &artifact.path)?.map(|line| line.timestamp),
            "codex" | "codex-fork" => parse_codex_line(
                &line,
                source_seq,
                artifact
                    .provider_session_ids
                    .iter()
                    .next()
                    .map(String::as_str),
            )?
            .and_then(|event| match event {
                CodexEvent::Cumulative { timestamp, .. } => Some(timestamp),
                CodexEvent::Settings { .. } => None,
            }),
            _ => None,
        };
        if timestamp.is_some_and(|timestamp| timestamp >= open_from) {
            earliest = match (earliest, timestamp) {
                (Some(previous), Some(timestamp)) => Some(previous.min(timestamp)),
                (None, timestamp) => timestamp,
                (previous, None) => previous,
            };
        }
    }
    Ok(earliest)
}

fn json_text(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
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

fn json_i64_any(object: &Map<String, Value>, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| json_i64(object.get(*key)))
}

fn normalized_model(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("unset"))
        .map(ToOwned::to_owned)
}

fn minute_timestamp(timestamp: OffsetDateTime) -> Result<String> {
    let minute = timestamp.replace_second(0)?.replace_nanosecond(0)?;
    format_timestamp(minute)
}

pub fn derive_window_start(
    resets_at: OffsetDateTime,
    duration_minutes: i64,
) -> Result<OffsetDateTime> {
    if duration_minutes <= 0 {
        bail!("window duration must be positive");
    }
    Ok(resets_at - time::Duration::minutes(duration_minutes))
}

fn format_timestamp(timestamp: OffsetDateTime) -> Result<String> {
    timestamp
        .to_offset(time::UtcOffset::UTC)
        .format(DB_TIMESTAMP_FORMAT)
        .context("failed to format usage ledger timestamp")
}

fn parse_timestamp(timestamp: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(timestamp, &Rfc3339)
        .with_context(|| format!("invalid usage ledger timestamp {timestamp}"))
}

fn file_mtime_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
        .unwrap_or(0)
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn canonical_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use serde_json::json;

    use super::*;
    use crate::{
        seat_sessions::SeatSessionStore,
        usage_burn::{BurnWindowSample, UsageBurnStore},
        usage_identity::AccountIdentity,
    };

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "sm-usage-ledger-{label}-{}-{}",
                std::process::id(),
                NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
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

    fn identity(provider: Provider, external_id: &str, plan_tier: &str) -> AccountIdentity {
        AccountIdentity {
            provider,
            external_id: external_id.to_owned(),
            label: None,
            plan_tier: Some(plan_tier.to_owned()),
            extra_usage_enabled: Some(true),
        }
    }

    fn seed_provider(
        db_path: &Path,
        provider: Provider,
        external_id: &str,
        plan_tier: &str,
        window_kind: &str,
        duration_minutes: i64,
    ) {
        let account = identity(provider, external_id, plan_tier);
        UsageIdentityStore::new(db_path)
            .unwrap()
            .record_observation(
                provider,
                Some(&account),
                at("2026-08-10T15:00:00Z"),
                None,
                None,
            )
            .unwrap();
        UsageBurnStore::new(db_path)
            .unwrap()
            .record_for_account(
                &account.account_key(),
                &[BurnWindowSample {
                    window_kind: window_kind.to_owned(),
                    window_scope: None,
                    duration_minutes,
                    percent: 10.0,
                    resets_at: at("2026-08-10T21:00:00Z"),
                    severity: None,
                    is_active: Some(true),
                }],
                "test",
                at("2026-08-10T15:00:00Z"),
            )
            .unwrap();
    }

    fn contribution(
        message_id: &str,
        request_id: &str,
        sidechain: bool,
        speed: bool,
        cache_read: i64,
    ) -> Contribution {
        let tokens = TokenBuckets {
            input: 10,
            output: 5,
            cache_read,
            ..TokenBuckets::default()
        };
        Contribution {
            message_id: message_id.to_owned(),
            request_id: Some(request_id.to_owned()),
            is_sidechain: sidechain,
            has_speed: speed,
            total_tokens: tokens.total(),
            seat_id: "seat-one".to_owned(),
            account_key: "claude:account-one".to_owned(),
            project_key: "/repo/.git".to_owned(),
            source_ref: "session-one".to_owned(),
            source_seq: None,
            bucket_ts: "2026-08-10T16:00:00.000000000Z".to_owned(),
            timestamp: at("2026-08-10T16:00:00Z"),
            model: "claude-sonnet-5".to_owned(),
            effort: None,
            tokens,
            credit_metered: false,
        }
    }

    fn contribution_at(message_id: &str, request_id: &str, timestamp: &str) -> Contribution {
        let mut value = contribution(message_id, request_id, false, false, 10);
        value.timestamp = at(timestamp);
        value.bucket_ts = minute_timestamp(value.timestamp).unwrap();
        value
    }

    fn record_window(
        db_path: &Path,
        kind: &str,
        duration_minutes: i64,
        resets_at: &str,
        source: &str,
    ) {
        UsageBurnStore::new(db_path)
            .unwrap()
            .record_for_account(
                "claude:account-one",
                &[BurnWindowSample {
                    window_kind: kind.to_owned(),
                    window_scope: None,
                    duration_minutes,
                    percent: 10.0,
                    resets_at: at(resets_at),
                    severity: None,
                    is_active: Some(true),
                }],
                source,
                at("2026-08-10T15:30:00Z"),
            )
            .unwrap();
    }

    #[test]
    fn no_change_materialization_scan_does_not_rematerialize_ledger_rows() {
        let dir = TestDir::new("materialization-no-change");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "account-one",
            "max",
            "session_5h",
            300,
        );
        let store = UsageLedgerStore::new(&db_path).unwrap();
        let mut connection = Connection::open(&db_path).unwrap();
        let tx = connection.transaction().unwrap();
        for index in 0..64 {
            ingest_contribution(
                &tx,
                &contribution_at(
                    &format!("message-{index}"),
                    &format!("request-{index}"),
                    "2026-08-10T16:00:00Z",
                ),
            )
            .unwrap();
        }
        tx.commit().unwrap();

        // Existing message-window rows are backfilled into the durable window checkpoint
        // without rewriting a rollup. Every later scan skips the window entirely.
        assert_eq!(
            store.materialize_pending_windows().unwrap(),
            MaterializationSummary {
                windows_examined: 1,
                messages_selected: 0,
            }
        );
        let before: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT SUM(message_count) FROM seat_tokens", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(before, 64);
        assert_eq!(
            store.materialize_pending_windows().unwrap(),
            MaterializationSummary::default()
        );
        let after: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT SUM(message_count) FROM seat_tokens", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn new_burn_window_materializes_each_eligible_message_once() {
        let dir = TestDir::new("materialization-new-window");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "account-one",
            "max",
            "session_5h",
            300,
        );
        let store = UsageLedgerStore::new(&db_path).unwrap();
        let mut connection = Connection::open(&db_path).unwrap();
        let tx = connection.transaction().unwrap();
        ingest_contribution(
            &tx,
            &contribution_at("eligible", "eligible-request", "2026-08-10T16:00:00Z"),
        )
        .unwrap();
        ingest_contribution(
            &tx,
            &contribution_at("outside", "outside-request", "2026-08-10T19:00:00Z"),
        )
        .unwrap();
        tx.commit().unwrap();
        store.materialize_pending_windows().unwrap();

        record_window(
            &db_path,
            "weekly_all",
            300,
            "2026-08-10T18:00:00Z",
            "test-new-window",
        );
        assert_eq!(
            store.materialize_pending_windows().unwrap(),
            MaterializationSummary {
                windows_examined: 1,
                messages_selected: 1,
            }
        );
        let connection = Connection::open(&db_path).unwrap();
        let mapped = connection
            .prepare(
                "SELECT message_id FROM message_ledger JOIN message_window USING(msg_id) WHERE window_kind = 'weekly_all' ORDER BY message_id",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(mapped, vec!["eligible"]);
        let rollup_messages: i64 = connection
            .query_row(
                "SELECT SUM(message_count) FROM seat_tokens WHERE window_kind = 'weekly_all'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rollup_messages, 1);
        assert_eq!(
            store.materialize_pending_windows().unwrap(),
            MaterializationSummary::default()
        );
    }

    #[test]
    fn interrupted_window_materialization_restarts_without_duplicate_rollups() {
        let dir = TestDir::new("materialization-restart");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "account-one",
            "max",
            "session_5h",
            300,
        );
        let store = UsageLedgerStore::new(&db_path).unwrap();
        let mut connection = Connection::open(&db_path).unwrap();
        let tx = connection.transaction().unwrap();
        ingest_contribution(
            &tx,
            &contribution_at("eligible", "eligible-request", "2026-08-10T16:00:00Z"),
        )
        .unwrap();
        tx.commit().unwrap();
        store.materialize_pending_windows().unwrap();
        record_window(
            &db_path,
            "weekly_all",
            300,
            "2026-08-10T18:00:00Z",
            "test-restart-window",
        );

        let window = store.pending_burn_windows().unwrap().pop().unwrap();
        let ids = store.pending_message_ids_for_window(&window).unwrap();
        assert_eq!(ids.len(), 1);
        store.materialize_window_messages(&window, &ids).unwrap();
        let checkpoint_count: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM burn_window_materialization WHERE window_kind = 'weekly_all'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);

        let restarted = UsageLedgerStore::new(&db_path).unwrap();
        assert_eq!(
            restarted.materialize_pending_windows().unwrap(),
            MaterializationSummary {
                windows_examined: 1,
                messages_selected: 0,
            }
        );
        let rollup_messages: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT SUM(message_count) FROM seat_tokens WHERE window_kind = 'weekly_all'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rollup_messages, 1);
    }

    #[test]
    fn historical_window_materialization_yields_to_live_seat_session_writers() {
        let dir = TestDir::new("materialization-live-writer");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "account-one",
            "max",
            "session_5h",
            300,
        );
        let store = UsageLedgerStore::new(&db_path).unwrap();
        let mut connection = Connection::open(&db_path).unwrap();
        let tx = connection.transaction().unwrap();
        for index in 0..512 {
            ingest_contribution(
                &tx,
                &contribution_at(
                    &format!("message-{index}"),
                    &format!("request-{index}"),
                    "2026-08-10T16:00:00Z",
                ),
            )
            .unwrap();
        }
        tx.commit().unwrap();
        store.materialize_pending_windows().unwrap();
        record_window(
            &db_path,
            "weekly_all",
            300,
            "2026-08-10T18:00:00Z",
            "test-large-window",
        );

        let (started_tx, started_rx) = mpsc::channel();
        let scanner = store.clone();
        let handle = thread::spawn(move || {
            started_tx.send(()).unwrap();
            scanner.materialize_pending_windows()
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        thread::sleep(Duration::from_millis(5));

        let started_at = Instant::now();
        SeatSessionStore::new(&db_path)
            .append("live-seat", "claude", "live-session", None)
            .unwrap();
        assert!(started_at.elapsed() < Duration::from_millis(250));
        let summary = handle.join().unwrap().unwrap();
        assert_eq!(summary.messages_selected, 512);
    }

    #[test]
    fn pending_window_materialization_does_not_expand_into_overlapping_windows() {
        let dir = TestDir::new("materialization-exact-window");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "account-one",
            "max",
            "session_5h",
            300,
        );
        let store = UsageLedgerStore::new(&db_path).unwrap();
        let mut connection = Connection::open(&db_path).unwrap();
        let tx = connection.transaction().unwrap();
        ingest_contribution(
            &tx,
            &contribution_at("eligible", "eligible-request", "2026-08-10T16:00:00Z"),
        )
        .unwrap();
        tx.commit().unwrap();
        store.materialize_pending_windows().unwrap();

        record_window(
            &db_path,
            "weekly_all",
            300,
            "2026-08-10T18:00:00Z",
            "test-first-overlap",
        );
        record_window(
            &db_path,
            "codex_10080",
            300,
            "2026-08-10T18:00:00Z",
            "test-second-overlap",
        );
        let windows = store.pending_burn_windows().unwrap();
        assert_eq!(windows.len(), 2);
        let first = &windows[0];
        let ids = store.pending_message_ids_for_window(first).unwrap();
        assert_eq!(ids.len(), 1);
        store.materialize_window_messages(first, &ids).unwrap();

        let connection = Connection::open(&db_path).unwrap();
        let mapped = connection
            .prepare(
                "SELECT window_kind FROM message_window WHERE msg_id = ?1 ORDER BY window_kind",
            )
            .unwrap()
            .query_map([ids[0]], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let mut expected = vec!["session_5h".to_owned(), first.kind.clone()];
        expected.sort();
        assert_eq!(mapped, expected);
        let second_ids = store.pending_message_ids_for_window(&windows[1]).unwrap();
        assert_eq!(second_ids, ids);
    }

    #[test]
    fn claude_line_filter_rejects_each_invalid_shape_and_allows_content_null() {
        let base = json!({
            "type": "assistant",
            "timestamp": "2026-08-10T16:00:00Z",
            "sessionId": "session-one",
            "requestId": "request-one",
            "cwd": "/repo",
            "version": "1.2.3-build",
            "message": {
                "id": "message-one",
                "model": "claude-sonnet-5",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "cache_creation_input_tokens": 2,
                    "cache_read_input_tokens": 3
                },
                "content": null
            }
        });
        let line = serde_json::to_vec(&base).unwrap();
        assert!(parse_claude_line(&line, Path::new("chat.jsonl"))
            .unwrap()
            .is_some());
        assert!(
            parse_claude_line(b"not json with \"usage\":{", Path::new("chat.jsonl"))
                .unwrap()
                .is_none()
        );
        assert!(parse_claude_line(b"{}", Path::new("chat.jsonl"))
            .unwrap()
            .is_none());

        let mut explicit_null = base.clone();
        explicit_null["requestId"] = Value::Null;
        assert!(parse_claude_line(
            &serde_json::to_vec(&explicit_null).unwrap(),
            Path::new("chat.jsonl")
        )
        .unwrap()
        .is_none());

        let mut no_usage = base.clone();
        no_usage["message"].as_object_mut().unwrap().remove("usage");
        assert!(parse_claude_line(
            &serde_json::to_vec(&no_usage).unwrap(),
            Path::new("chat.jsonl")
        )
        .unwrap()
        .is_none());

        for (path, bad) in [("version", "not-semver"), ("timestamp", "not-a-time")] {
            let mut invalid = base.clone();
            invalid[path] = Value::String(bad.to_owned());
            assert!(parse_claude_line(
                &serde_json::to_vec(&invalid).unwrap(),
                Path::new("chat.jsonl")
            )
            .unwrap()
            .is_none());
        }
        for path in ["sessionId", "requestId", "cwd"] {
            let mut invalid = base.clone();
            invalid[path] = Value::String(String::new());
            assert!(parse_claude_line(
                &serde_json::to_vec(&invalid).unwrap(),
                Path::new("chat.jsonl")
            )
            .unwrap()
            .is_none());
        }
        for path in ["id", "model"] {
            let mut invalid = base.clone();
            invalid["message"][path] = Value::String(String::new());
            assert!(parse_claude_line(
                &serde_json::to_vec(&invalid).unwrap(),
                Path::new("chat.jsonl")
            )
            .unwrap()
            .is_none());
        }
    }

    #[test]
    fn claude_cache_creation_prefers_nested_split_and_falls_back_to_flat_5m() {
        let line = |usage: Value| {
            serde_json::to_vec(&json!({
                "timestamp": "2026-08-10T16:00:00Z",
                "sessionId": "session-one",
                "requestId": "request-one",
                "cwd": "/repo",
                "version": "1.0.0",
                "message": {"id": "message-one", "model": "claude-sonnet-5", "usage": usage}
            }))
            .unwrap()
        };
        let nested = parse_claude_line(
            &line(json!({
                "input_tokens": 1,
                "output_tokens": 2,
                "cache_creation_input_tokens": 999,
                "cache_read_input_tokens": 3,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 4,
                    "ephemeral_1h_input_tokens": 5
                }
            })),
            Path::new("chat.jsonl"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(nested.tokens.cache_write_5m, 4);
        assert_eq!(nested.tokens.cache_write_1h, 5);

        let flat = parse_claude_line(
            &line(json!({
                "input_tokens": 1,
                "output_tokens": 2,
                "cache_creation_input_tokens": 6,
                "cache_read_input_tokens": 3
            })),
            Path::new("chat.jsonl"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(flat.tokens.cache_write_5m, 6);
        assert_eq!(flat.tokens.cache_write_1h, 0);
    }

    #[test]
    fn dedupe_reverses_inflated_sidechain_and_honors_stream_and_speed_tiebreaks() {
        let dir = TestDir::new("dedupe");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "account-one",
            "max",
            "session_5h",
            300,
        );
        UsageLedgerStore::new(&db_path).unwrap();
        let mut connection = Connection::open(&db_path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        let tx = connection.transaction().unwrap();

        let replay = contribution("shared", "replay-request", true, false, 800);
        assert_eq!(
            ingest_contribution(&tx, &replay).unwrap(),
            IngestOutcome::Inserted
        );
        let parent = contribution("shared", "parent-request", false, false, 80);
        assert_eq!(
            ingest_contribution(&tx, &parent).unwrap(),
            IngestOutcome::Replaced
        );

        let mut streamed = contribution("streamed", "stream-request", false, false, 1);
        assert_eq!(
            ingest_contribution(&tx, &streamed).unwrap(),
            IngestOutcome::Inserted
        );
        streamed.tokens.output += 10;
        streamed.total_tokens = streamed.tokens.total();
        assert_eq!(
            ingest_contribution(&tx, &streamed).unwrap(),
            IngestOutcome::Replaced
        );

        let mut speed = contribution("speed", "speed-request", false, false, 1);
        assert_eq!(
            ingest_contribution(&tx, &speed).unwrap(),
            IngestOutcome::Inserted
        );
        speed.has_speed = true;
        assert_eq!(
            ingest_contribution(&tx, &speed).unwrap(),
            IngestOutcome::Replaced
        );

        let shared = load_contribution(&tx, 1).unwrap();
        assert!(!shared.is_sidechain);
        assert_eq!(shared.tokens.cache_read, 80);
        let rolled_cache: i64 = tx
            .query_row(
                "SELECT cache_read_tokens FROM seat_tokens WHERE model = 'claude-sonnet-5' AND message_count = 3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rolled_cache, 82);
        let alias_count: i64 = tx
            .query_row("SELECT COUNT(*) FROM message_alias", [], |row| row.get(0))
            .unwrap();
        assert_eq!(alias_count, 7);
        tx.commit().unwrap();

        UsageLedgerStore::new(&db_path)
            .unwrap()
            .rebuild_rollups()
            .unwrap();
        let rebuilt_cache: i64 = Connection::open(db_path)
            .unwrap()
            .query_row(
                "SELECT SUM(cache_read_tokens) FROM seat_tokens",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rebuilt_cache, 82);
    }

    #[test]
    fn codex_nested_buckets_normalize_without_double_counting_reasoning() {
        let line = br#"{"event_type":"thread/tokenUsage/updated","seq":2,"ts":"2026-08-10T16:20:00Z","payload":{"threadId":"thread-one","tokenUsage":{"total":{"inputTokens":120,"cachedInputTokens":90,"cacheWriteInputTokens":0,"outputTokens":30,"reasoningOutputTokens":10,"totalTokens":150}}}}"#;
        let event = parse_codex_line(line, 0, None).unwrap().unwrap();
        let CodexEvent::Cumulative { totals, .. } = event else {
            panic!("expected cumulative event");
        };
        let delta = totals.delta(None).unwrap();
        assert_eq!(delta.input, 30);
        assert_eq!(delta.cache_read, 90);
        assert_eq!(delta.output, 30);
        assert_eq!(delta.reasoning, 10);
        assert_eq!(delta.total(), 150);

        let invalid = CodexTotals {
            input: 120,
            cached_input: 90,
            output: 31,
            total: 150,
            ..CodexTotals::default()
        };
        assert!(invalid.delta(None).is_err());
    }

    #[test]
    fn classic_codex_turn_context_and_snake_case_totals_share_the_thread() {
        let settings = parse_codex_line(
            br#"{"type":"turn_context","timestamp":"2026-08-10T16:19:59Z","payload":{"model":"gpt-5.6-terra","effort":"high","turn_id":"turn-one"}}"#,
            10,
            Some("thread-one"),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            settings,
            CodexEvent::Settings {
                thread_id,
                source_seq: 10,
                model,
                effort: Some(effort),
            } if thread_id == "thread-one" && model == "gpt-5.6-terra" && effort == "high"
        ));

        let event = parse_codex_line(
            br#"{"type":"event_msg","timestamp":"2026-08-10T16:20:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":120,"cached_input_tokens":90,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":150}}}}"#,
            20,
            Some("thread-one"),
        )
        .unwrap()
        .unwrap();
        let CodexEvent::Cumulative {
            thread_id,
            source_seq,
            totals,
            ..
        } = event
        else {
            panic!("expected cumulative event");
        };
        assert_eq!(thread_id, "thread-one");
        assert_eq!(source_seq, 20);
        let normalized = totals.delta(None).unwrap();
        assert_eq!(normalized.input, 30);
        assert_eq!(normalized.cache_read, 90);
        assert_eq!(normalized.output, 30);
        assert_eq!(normalized.reasoning, 10);
    }

    #[test]
    fn classic_codex_scan_backfills_rate_limits_after_token_offset_was_checkpointed() {
        let dir = TestDir::new("classic-codex-burn");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Codex,
            "account-one",
            "pro",
            "codex_300",
            300,
        );
        let artifact = dir.0.join("rollout.jsonl");
        let reset = at("2026-08-10T21:00:00Z").unix_timestamp();
        fs::write(
            &artifact,
            format!(
                "{}\n",
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-08-10T16:20:00Z",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": 120,
                                "cached_input_tokens": 90,
                                "output_tokens": 30,
                                "reasoning_output_tokens": 10,
                                "total_tokens": 150
                            },
                        },
                        "rate_limits": {
                            "limit_id": "codex",
                            "primary": {
                                "used_percent": 25,
                                "window_minutes": 300,
                                "resets_at": reset
                            }
                        }
                    }
                })
            ),
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append("seat-one", "codex", "thread-one", artifact.to_str())
            .unwrap();
        let store = UsageLedgerStore::new(&db_path).unwrap();
        let metadata = fs::metadata(&artifact).unwrap();
        Connection::open(&db_path)
            .unwrap()
            .execute(
                "INSERT INTO scan_offsets (artifact_path, byte_offset, mtime_ns, scanned_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    artifact.to_string_lossy().as_ref(),
                    metadata.len(),
                    file_mtime_ns(&metadata),
                    "2026-08-10T16:21:00.000000000Z"
                ],
            )
            .unwrap();

        store.scan(&[]).unwrap();

        let (percent, source, observed_at): (f64, String, String) = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT percent, source, observed_at FROM burn_samples WHERE source = 'codex_event' ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(percent, 25.0);
        assert_eq!(source, "codex_event");
        assert_eq!(observed_at, "2026-08-10T16:20:00.000000000Z");
    }

    #[test]
    fn codex_fork_scan_backfills_persisted_rate_limits_after_restart() {
        let dir = TestDir::new("codex-fork-burn");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Codex,
            "account-one",
            "pro",
            "codex_300",
            300,
        );
        let artifact = dir.0.join("events.jsonl");
        let reset = at("2026-08-10T21:00:00Z").unix_timestamp();
        fs::write(
            &artifact,
            format!(
                "{}\n",
                json!({
                    "event_type": "account/rateLimits/updated",
                    "ts": "2026-08-10T16:20:00Z",
                    "payload": {"rateLimits": {
                        "limitId": "codex",
                        "primary": {
                            "usedPercent": 31,
                            "windowDurationMins": 300,
                            "resetsAt": reset
                        }
                    }}
                })
            ),
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append("seat-one", "codex-fork", "thread-one", artifact.to_str())
            .unwrap();
        let store = UsageLedgerStore::new(&db_path).unwrap();
        let metadata = fs::metadata(&artifact).unwrap();
        Connection::open(&db_path)
            .unwrap()
            .execute(
                "INSERT INTO scan_offsets (artifact_path, byte_offset, mtime_ns, scanned_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    artifact.to_string_lossy().as_ref(),
                    metadata.len(),
                    file_mtime_ns(&metadata),
                    "2026-08-10T16:21:00.000000000Z"
                ],
            )
            .unwrap();

        store.scan(&[]).unwrap();

        let (percent, source, observed_at): (f64, String, String) = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT percent, source, observed_at FROM burn_samples WHERE source = 'codex_event' ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(percent, 31.0);
        assert_eq!(source, "codex_event");
        assert_eq!(observed_at, "2026-08-10T16:20:00.000000000Z");
    }

    #[test]
    fn codex_history_is_attributed_when_no_burn_window_exists() {
        let dir = TestDir::new("codex-pre-identity-baseline");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Codex,
            "account-one",
            "pro",
            "codex_300",
            300,
        );
        let artifact = dir.0.join("codex-thread.jsonl");
        fs::write(
            &artifact,
            concat!(
                "{\"event_type\":\"thread/settings/updated\",\"seq\":1,\"ts\":\"2026-08-10T13:59:59Z\",\"payload\":{\"threadId\":\"thread-one\",\"threadSettings\":{\"model\":\"gpt-5.6-terra\",\"effort\":\"high\"}}}\n",
                "{\"event_type\":\"thread/tokenUsage/updated\",\"seq\":2,\"ts\":\"2026-08-10T14:00:00Z\",\"payload\":{\"threadId\":\"thread-one\",\"turnId\":\"old\",\"tokenUsage\":{\"total\":{\"inputTokens\":80,\"cachedInputTokens\":20,\"cacheWriteInputTokens\":0,\"outputTokens\":20,\"reasoningOutputTokens\":5,\"totalTokens\":100}}}}\n",
                "{\"event_type\":\"thread/tokenUsage/updated\",\"seq\":3,\"ts\":\"2026-08-10T16:30:00Z\",\"payload\":{\"threadId\":\"thread-one\",\"turnId\":\"current\",\"tokenUsage\":{\"total\":{\"inputTokens\":120,\"cachedInputTokens\":30,\"cacheWriteInputTokens\":0,\"outputTokens\":30,\"reasoningOutputTokens\":8,\"totalTokens\":150}}}}\n"
            ),
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append("seat-one", "codex-fork", "thread-one", artifact.to_str())
            .unwrap();
        let store = UsageLedgerStore::new(&db_path).unwrap();
        store
            .scan(&[UsageSeatMetadata {
                seat_id: "seat-one".to_owned(),
                friendly_name: None,
                provider: "codex-fork".to_owned(),
                model: None,
                effort: None,
                working_dir: "/repo".to_owned(),
                parent_seat_id: None,
                root_seat_id: Some("seat-one".to_owned()),
                project_key: "/repo".to_owned(),
            }])
            .unwrap();

        let connection = Connection::open(db_path).unwrap();
        let (message_count, total_tokens): (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), SUM(total_tokens) FROM message_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((message_count, total_tokens), (2, 150));
        let cursor_total: i64 = connection
            .query_row(
                "SELECT last_total_tokens FROM codex_thread_cursor WHERE thread_id = 'thread-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursor_total, 150);
    }

    #[test]
    fn window_start_is_an_exact_duration_across_dst_boundary() {
        let reset = at("2026-03-08T10:30:00Z");
        let start = derive_window_start(reset, 300).unwrap();
        assert_eq!((reset - start).whole_minutes(), 300);
        assert_eq!(start, at("2026-03-08T05:30:00Z"));
    }

    #[test]
    fn project_key_falls_back_after_a_hung_git_probe() {
        let dir = TestDir::new("project-key-timeout");
        let mut command = Command::new("/bin/sleep");
        command.arg("5");
        let started_at = Instant::now();

        let project_key =
            resolve_project_key_with_command(&dir.0, command, Duration::from_millis(50));

        assert_eq!(project_key, canonical_path(&dir.0));
        assert!(started_at.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn project_key_collapses_a_linked_worktree_to_the_common_git_directory() {
        let dir = TestDir::new("project-key");
        let repository = dir.0.join("repository");
        let worktree = dir.0.join("linked-worktree");
        fs::create_dir_all(&repository).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()
            .unwrap()
            .success());
        fs::write(repository.join("README"), "fixture\n").unwrap();
        assert!(Command::new("git")
            .args(["-C"])
            .arg(&repository)
            .args(["add", "README"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["-C"])
            .arg(&repository)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-q",
                "-m",
                "fixture",
            ])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["-C"])
            .arg(&repository)
            .args(["worktree", "add", "-q", "-b", "fixture-worktree"])
            .arg(&worktree)
            .status()
            .unwrap()
            .success());

        assert_eq!(
            UsageSeatMetadata::resolve_project_key(repository.to_str().unwrap()),
            UsageSeatMetadata::resolve_project_key(worktree.to_str().unwrap())
        );
    }

    #[test]
    fn scanner_bootstraps_the_earliest_message_in_an_open_window() {
        let dir = TestDir::new("bootstrap");
        let db_path = dir.0.join("usage.db");
        let account = identity(Provider::Claude, "account-one", "max");
        let observed_at = OffsetDateTime::now_utc();
        let old_message_at = observed_at - time::Duration::hours(10);
        let message_at = observed_at - time::Duration::hours(1);
        UsageIdentityStore::new(&db_path)
            .unwrap()
            .record_observation(Provider::Claude, Some(&account), observed_at, None, None)
            .unwrap();
        UsageBurnStore::new(&db_path)
            .unwrap()
            .record_for_account(
                &account.account_key(),
                &[BurnWindowSample {
                    window_kind: "session_5h".to_owned(),
                    window_scope: None,
                    duration_minutes: 300,
                    percent: 12.0,
                    resets_at: observed_at + time::Duration::hours(1),
                    severity: None,
                    is_active: Some(true),
                }],
                "test",
                observed_at,
            )
            .unwrap();
        let transcript = dir.0.join("session-one.jsonl");
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                json!({
                    "timestamp": old_message_at.format(&Rfc3339).unwrap(),
                    "sessionId": "session-one",
                    "requestId": "request-old",
                    "cwd": "/repo",
                    "version": "1.0.0",
                    "message": {
                        "id": "message-old",
                        "model": "claude-sonnet-5",
                        "usage": {
                            "input_tokens": 100,
                            "output_tokens": 50,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0
                        }
                    }
                }),
                json!({
                    "timestamp": message_at.format(&Rfc3339).unwrap(),
                    "sessionId": "session-one",
                    "requestId": "request-one",
                    "cwd": "/repo",
                    "version": "1.0.0",
                    "message": {
                        "id": "message-one",
                        "model": "claude-sonnet-5",
                        "usage": {
                            "input_tokens": 10,
                            "output_tokens": 5,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0
                        }
                    }
                })
            ),
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append("seat-one", "claude", "session-one", transcript.to_str())
            .unwrap();
        let store = UsageLedgerStore::new(&db_path).unwrap();
        store
            .scan(&[UsageSeatMetadata {
                seat_id: "seat-one".to_owned(),
                friendly_name: None,
                provider: "claude".to_owned(),
                model: None,
                effort: None,
                working_dir: "/repo".to_owned(),
                parent_seat_id: None,
                root_seat_id: Some("seat-one".to_owned()),
                project_key: "/repo".to_owned(),
            }])
            .unwrap();

        let connection = Connection::open(db_path).unwrap();
        let assumed: (String, String) = connection
            .query_row(
                "SELECT from_ts, to_ts FROM account_timeline WHERE provider = 'claude' AND is_assumed = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(assumed.0, format_timestamp(message_at).unwrap());
        assert_eq!(assumed.1, format_timestamp(observed_at).unwrap());
        let ledger_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM message_ledger", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ledger_count, 1);
    }

    #[test]
    fn scanner_bootstraps_identity_when_no_burn_window_exists() {
        let dir = TestDir::new("no-burn-bootstrap-boundary");
        let db_path = dir.0.join("usage.db");
        let account = identity(Provider::Claude, "account-one", "max");
        UsageIdentityStore::new(&db_path)
            .unwrap()
            .record_observation(
                Provider::Claude,
                Some(&account),
                at("2026-08-10T15:00:00Z"),
                None,
                None,
            )
            .unwrap();
        UsageBurnStore::new(&db_path).unwrap();
        let transcript = dir.0.join("session-one.jsonl");
        let message = |timestamp: &str, id: &str| {
            json!({
                "timestamp": timestamp,
                "sessionId": "session-one",
                "requestId": format!("request-{id}"),
                "cwd": "/repo",
                "version": "1.0.0",
                "message": {
                    "id": format!("message-{id}"),
                    "model": "claude-sonnet-5",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0
                    }
                }
            })
        };
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                message("2026-08-10T14:00:00Z", "old"),
                message("2026-08-10T16:00:00Z", "current")
            ),
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append("seat-one", "claude", "session-one", transcript.to_str())
            .unwrap();
        let store = UsageLedgerStore::new(&db_path).unwrap();
        store
            .scan(&[UsageSeatMetadata {
                seat_id: "seat-one".to_owned(),
                friendly_name: None,
                provider: "claude".to_owned(),
                model: None,
                effort: None,
                working_dir: "/repo".to_owned(),
                parent_seat_id: None,
                root_seat_id: Some("seat-one".to_owned()),
                project_key: "/repo".to_owned(),
            }])
            .unwrap();

        let connection = Connection::open(db_path).unwrap();
        let ledger_ids = connection
            .prepare("SELECT message_id FROM message_ledger ORDER BY message_id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ledger_ids, vec!["message-current", "message-old"]);
        let assumed: (String, String) = connection
            .query_row(
                "SELECT from_ts, to_ts FROM account_timeline WHERE provider = 'claude' AND is_assumed = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(assumed.0, "2026-08-10T14:00:00.000000000Z");
        assert_eq!(assumed.1, "2026-08-10T15:00:00.000000000Z");
        let window_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM message_window", [], |row| row.get(0))
            .unwrap();
        assert_eq!(window_count, 0);
    }

    #[test]
    fn fixture_corpus_scans_parent_subagent_resume_and_codex_once() {
        let dir = TestDir::new("corpus");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "claude-account",
            "max",
            "session_5h",
            300,
        );
        seed_provider(
            &db_path,
            Provider::Codex,
            "codex-account",
            "pro",
            "codex_300",
            300,
        );

        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/usage");
        let parent_root = dir.0.join("claude-session-parent");
        fs::create_dir_all(parent_root.join("subagents")).unwrap();
        let parent_path = parent_root.join("chat.jsonl");
        let subagent_path = parent_root.join("subagents/agent-redacted.jsonl");
        let resume_path = dir.0.join("claude-session-resume.jsonl");
        let codex_path = dir.0.join("codex-family.jsonl");
        for (source, target) in [
            ("claude-parent.jsonl", &parent_path),
            ("claude-subagent.jsonl", &subagent_path),
            ("claude-resume.jsonl", &resume_path),
            ("codex-family.jsonl", &codex_path),
        ] {
            fs::copy(fixture_root.join(source), target).unwrap();
        }

        let sessions = SeatSessionStore::new(&db_path);
        sessions
            .append(
                "seat-one",
                "claude",
                "claude-session-parent",
                parent_path.to_str(),
            )
            .unwrap();
        sessions
            .append(
                "seat-one",
                "claude",
                "claude-session-resume",
                resume_path.to_str(),
            )
            .unwrap();
        sessions
            .append(
                "seat-one",
                "codex-fork",
                "codex-thread-1",
                codex_path.to_str(),
            )
            .unwrap();
        let store = UsageLedgerStore::with_model_defaults(
            &db_path,
            UsageModelDefaults {
                claude: Some("claude-sonnet-5".to_owned()),
                codex: Some("gpt-5.1-codex".to_owned()),
                codex_fork: Some("gpt-5.1-codex".to_owned()),
            },
        )
        .unwrap();
        let seats = [UsageSeatMetadata {
            seat_id: "seat-one".to_owned(),
            friendly_name: Some("fixture-seat".to_owned()),
            provider: "claude".to_owned(),
            model: Some("fallback-model".to_owned()),
            effort: None,
            working_dir: "/redacted/project".to_owned(),
            parent_seat_id: None,
            root_seat_id: Some("seat-one".to_owned()),
            project_key: "/redacted/project".to_owned(),
        }];

        let summary = store.scan(&seats).unwrap();
        assert_eq!(summary.artifacts_scanned, 4);
        assert_eq!(summary.messages_inserted, 4);
        assert_eq!(summary.messages_replaced, 0);
        assert_eq!(summary.messages_ignored, 1);
        let second = store.scan(&seats).unwrap();
        assert_eq!(second, ScanSummary::default());
        let rebuilt = store.rebuild(&seats).unwrap();
        assert_eq!(rebuilt.messages_inserted, 4);
        assert_eq!(rebuilt.messages_ignored, 1);

        let connection = Connection::open(&db_path).unwrap();
        let ledger = connection
            .prepare(
                "SELECT message_id, is_sidechain, input_tokens, output_tokens, cache_write_5m, cache_read_tokens, reasoning_tokens, model, effort FROM message_ledger ORDER BY message_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ledger.len(), 4);
        assert!(ledger.iter().all(|row| !row.1));
        assert!(ledger
            .iter()
            .any(|row| { row.0 == "msg-shared" && row.2 == 200 && row.3 == 50 && row.5 == 80 }));
        assert!(ledger.iter().any(|row| {
            row.0 == "codex:codex-thread-1:2"
                && row.2 == 30
                && row.3 == 30
                && row.5 == 90
                && row.6 == 10
                && row.7 == "gpt-5.1-codex"
                && row.8.as_deref() == Some("high")
        }));
        let total: i64 = connection
            .query_row("SELECT SUM(total_tokens) FROM message_ledger", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(total, 730);
        let aliases: i64 = connection
            .query_row("SELECT COUNT(*) FROM message_alias", [], |row| row.get(0))
            .unwrap();
        assert_eq!(aliases, 9);
        let rollup_total: i64 = connection
            .query_row(
                "SELECT SUM(input_tokens + output_tokens + cache_write_5m + cache_write_1h + cache_read_tokens) FROM seat_tokens",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rollup_total, 730);
    }

    #[test]
    fn codex_model_fallback_prefers_seat_and_persists_the_launch_default() {
        let dir = TestDir::new("model-fallback");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Codex,
            "codex-account",
            "pro",
            "codex_300",
            300,
        );
        let artifact = dir.0.join("codex.jsonl");
        fs::write(
            &artifact,
            "{\"event_type\":\"thread/tokenUsage/updated\",\"seq\":1,\"ts\":\"2026-08-10T16:00:00Z\",\"payload\":{\"threadId\":\"thread-one\",\"tokenUsage\":{\"total\":{\"inputTokens\":80,\"cachedInputTokens\":60,\"cacheWriteInputTokens\":0,\"outputTokens\":20,\"reasoningOutputTokens\":5,\"totalTokens\":100}}}}\n",
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append("seat-one", "codex-fork", "thread-one", artifact.to_str())
            .unwrap();
        let seat = |model: Option<&str>| UsageSeatMetadata {
            seat_id: "seat-one".to_owned(),
            friendly_name: None,
            provider: "codex-fork".to_owned(),
            model: model.map(ToOwned::to_owned),
            effort: None,
            working_dir: "/repo".to_owned(),
            parent_seat_id: None,
            root_seat_id: Some("seat-one".to_owned()),
            project_key: "/repo".to_owned(),
        };

        let first = UsageLedgerStore::with_model_defaults(
            &db_path,
            UsageModelDefaults {
                codex_fork: Some("config-model-a".to_owned()),
                ..UsageModelDefaults::default()
            },
        )
        .unwrap();
        first.scan(&[seat(None)]).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&artifact)
            .unwrap()
            .write_all(
                b"{\"event_type\":\"thread/tokenUsage/updated\",\"seq\":2,\"ts\":\"2026-08-10T16:01:00Z\",\"payload\":{\"threadId\":\"thread-one\",\"tokenUsage\":{\"total\":{\"inputTokens\":160,\"cachedInputTokens\":120,\"cacheWriteInputTokens\":0,\"outputTokens\":40,\"reasoningOutputTokens\":10,\"totalTokens\":200}}}}\n",
            )
            .unwrap();
        let changed_config = UsageLedgerStore::with_model_defaults(
            &db_path,
            UsageModelDefaults {
                codex_fork: Some("config-model-b".to_owned()),
                ..UsageModelDefaults::default()
            },
        )
        .unwrap();
        changed_config.scan(&[seat(None)]).unwrap();

        Connection::open(&db_path)
            .unwrap()
            .execute(
                "DELETE FROM codex_thread_cursor WHERE thread_id = 'thread-one'",
                [],
            )
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&artifact)
            .unwrap()
            .write_all(
                b"{\"event_type\":\"thread/tokenUsage/updated\",\"seq\":3,\"ts\":\"2026-08-10T16:02:00Z\",\"payload\":{\"threadId\":\"thread-one\",\"tokenUsage\":{\"total\":{\"inputTokens\":240,\"cachedInputTokens\":180,\"cacheWriteInputTokens\":0,\"outputTokens\":60,\"reasoningOutputTokens\":15,\"totalTokens\":300}}}}\n",
            )
            .unwrap();
        changed_config.scan(&[seat(None)]).unwrap();

        let connection = Connection::open(db_path).unwrap();
        let models = connection
            .prepare("SELECT DISTINCT model FROM message_ledger ORDER BY model")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(models, vec!["config-model-a"]);
        let total: i64 = connection
            .query_row("SELECT SUM(total_tokens) FROM message_ledger", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(total, 300);

        let seat_preferred = seat(Some("seat-model"));
        let resolved = changed_config
            .resolve_seat_models(&[seat_preferred])
            .unwrap();
        assert_eq!(resolved[0].model.as_deref(), Some("seat-model"));
    }

    #[test]
    fn retired_seat_artifacts_use_persisted_model_and_project_metadata() {
        let dir = TestDir::new("retired-seat-metadata");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Codex,
            "codex-account",
            "pro",
            "codex_300",
            300,
        );
        let store = UsageLedgerStore::with_model_defaults(
            &db_path,
            UsageModelDefaults {
                codex_fork: Some("current-config-default".to_owned()),
                ..UsageModelDefaults::default()
            },
        )
        .unwrap();
        store
            .scan(&[UsageSeatMetadata {
                seat_id: "retired-seat".to_owned(),
                friendly_name: Some("retired".to_owned()),
                provider: "codex-fork".to_owned(),
                model: Some("persisted-launch-model".to_owned()),
                effort: Some("high".to_owned()),
                working_dir: "/retired/repo".to_owned(),
                parent_seat_id: Some("old-parent".to_owned()),
                root_seat_id: Some("old-root".to_owned()),
                project_key: "/retired/repo/.git".to_owned(),
            }])
            .unwrap();
        let artifact = dir.0.join("retired-codex.jsonl");
        fs::write(
            &artifact,
            "{\"event_type\":\"thread/tokenUsage/updated\",\"seq\":1,\"ts\":\"2026-08-10T16:00:00Z\",\"payload\":{\"threadId\":\"retired-thread\",\"tokenUsage\":{\"total\":{\"inputTokens\":80,\"cachedInputTokens\":60,\"cacheWriteInputTokens\":0,\"outputTokens\":20,\"reasoningOutputTokens\":5,\"totalTokens\":100}}}}\n",
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append(
                "retired-seat",
                "codex-fork",
                "retired-thread",
                artifact.to_str(),
            )
            .unwrap();

        store.scan(&[]).unwrap();
        let connection = Connection::open(db_path).unwrap();
        let metadata: (String, String, String) = connection
            .query_row(
                "SELECT seat_id, model, project_key FROM message_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            metadata,
            (
                "retired-seat".to_owned(),
                "persisted-launch-model".to_owned(),
                "/retired/repo/.git".to_owned()
            )
        );
    }

    #[test]
    fn definitive_binding_reattributes_checkpointed_provisional_messages() {
        let dir = TestDir::new("rebind-provisional");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Codex,
            "codex-account",
            "pro",
            "codex_300",
            300,
        );
        let artifact = dir.0.join("codex.jsonl");
        fs::write(
            &artifact,
            "{\"event_type\":\"thread/tokenUsage/updated\",\"seq\":1,\"ts\":\"2026-08-10T16:00:00Z\",\"payload\":{\"threadId\":\"thread-one\",\"tokenUsage\":{\"total\":{\"inputTokens\":80,\"cachedInputTokens\":60,\"cacheWriteInputTokens\":0,\"outputTokens\":20,\"reasoningOutputTokens\":5,\"totalTokens\":100}}}}\n",
        )
        .unwrap();
        let sessions = SeatSessionStore::new(&db_path);
        sessions
            .append("unassigned", "codex-fork", "thread-one", artifact.to_str())
            .unwrap();
        let store = UsageLedgerStore::with_model_defaults(
            &db_path,
            UsageModelDefaults {
                codex_fork: Some("gpt-5.6-terra".to_owned()),
                ..UsageModelDefaults::default()
            },
        )
        .unwrap();
        store.scan(&[]).unwrap();

        sessions
            .append("seat-one", "codex-fork", "thread-one", artifact.to_str())
            .unwrap();
        let seat = UsageSeatMetadata {
            seat_id: "seat-one".to_owned(),
            friendly_name: Some("owner".to_owned()),
            provider: "codex-fork".to_owned(),
            model: Some("gpt-5.6-terra".to_owned()),
            effort: None,
            working_dir: "/repo".to_owned(),
            parent_seat_id: None,
            root_seat_id: Some("seat-one".to_owned()),
            project_key: "/repo/.git".to_owned(),
        };
        assert_eq!(store.scan(&[seat]).unwrap(), ScanSummary::default());

        let connection = Connection::open(db_path).unwrap();
        let ledger: (String, String) = connection
            .query_row(
                "SELECT seat_id, project_key FROM message_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ledger, ("seat-one".to_owned(), "/repo/.git".to_owned()));
        let rollups = connection
            .prepare(
                r#"
                SELECT seat_id,
                       input_tokens + output_tokens + cache_write_5m
                         + cache_write_1h + cache_read_tokens
                FROM seat_tokens
                ORDER BY seat_id
                "#,
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rollups, vec![("seat-one".to_owned(), 100)]);
    }

    #[test]
    fn unassigned_codex_artifact_preserves_project_from_rollout_cwd() {
        let dir = TestDir::new("unassigned-codex-project");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Codex,
            "codex-account",
            "pro",
            "codex_300",
            300,
        );
        let project = dir.0.join("project");
        fs::create_dir_all(&project).unwrap();
        let artifact = dir.0.join("rollout.jsonl");
        fs::write(
            &artifact,
            format!(
                "not-json\n{}\n{}\n{}\n",
                json!({"type": "session_meta", "payload": {"cwd": project}}),
                json!({"type": "turn_context", "timestamp": "2026-08-10T15:59:59Z", "payload": {"model": "gpt-5.6-terra", "effort": "high"}}),
                json!({"type": "event_msg", "timestamp": "2026-08-10T16:00:00Z", "payload": {"type": "token_count", "info": {"total_token_usage": {"input_tokens": 80, "cached_input_tokens": 60, "output_tokens": 20, "reasoning_output_tokens": 5, "total_tokens": 100}}}})
            ),
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append("unassigned", "codex-fork", "thread-one", artifact.to_str())
            .unwrap();

        UsageLedgerStore::new(&db_path).unwrap().scan(&[]).unwrap();
        let connection = Connection::open(db_path).unwrap();
        let (seat_id, project_key): (String, String) = connection
            .query_row(
                "SELECT seat_id, project_key FROM message_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(seat_id, "unassigned");
        assert_eq!(
            project_key,
            UsageSeatMetadata::resolve_project_key(project.to_str().unwrap())
        );
    }

    #[test]
    fn post_exhaustion_credit_flag_remains_a_separate_rollup_dimension() {
        let dir = TestDir::new("credit-metered");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "account-one",
            "max",
            "session_5h",
            300,
        );
        UsageBurnStore::new(&db_path)
            .unwrap()
            .record_for_account(
                "claude:account-one",
                &[BurnWindowSample {
                    window_kind: "session_5h".to_owned(),
                    window_scope: None,
                    duration_minutes: 300,
                    percent: 100.0,
                    resets_at: at("2026-08-10T21:00:00Z"),
                    severity: Some("exhausted".to_owned()),
                    is_active: Some(true),
                }],
                "test-exhausted",
                at("2026-08-10T15:30:00Z"),
            )
            .unwrap();
        UsageBurnStore::new(&db_path)
            .unwrap()
            .record_for_account(
                "claude:account-one",
                &[BurnWindowSample {
                    window_kind: "weekly_all".to_owned(),
                    window_scope: None,
                    duration_minutes: 10_080,
                    percent: 10.0,
                    resets_at: at("2026-08-16T16:00:00Z"),
                    severity: None,
                    is_active: Some(true),
                }],
                "test-newer-nonexhausted-window",
                at("2026-08-10T15:31:00Z"),
            )
            .unwrap();
        UsageLedgerStore::new(&db_path).unwrap();
        let mut connection = Connection::open(&db_path).unwrap();
        let tx = connection.transaction().unwrap();
        assert!(credit_metered(
            &tx,
            "claude:account-one",
            "claude-sonnet-5",
            at("2026-08-10T16:00:00Z")
        )
        .unwrap());

        let quota = contribution("quota", "quota-request", false, false, 10);
        ingest_contribution(&tx, &quota).unwrap();
        let mut credits = contribution("credits", "credits-request", false, false, 10);
        credits.credit_metered = true;
        ingest_contribution(&tx, &credits).unwrap();
        let dimensions = tx
            .prepare(
                "SELECT credit_metered, SUM(message_count), COUNT(*) FROM seat_tokens GROUP BY credit_metered ORDER BY credit_metered",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, bool>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(dimensions, vec![(false, 2, 2), (true, 2, 2)]);
        tx.commit().unwrap();
    }

    #[test]
    fn historical_account_metadata_controls_credit_classification() {
        let dir = TestDir::new("historical-account-metadata");
        let db_path = dir.0.join("usage.db");
        let store = UsageIdentityStore::new(&db_path).unwrap();
        let mut max = identity(Provider::Claude, "account-one", "max");
        max.extra_usage_enabled = Some(false);
        store
            .record_observation(
                Provider::Claude,
                Some(&max),
                at("2026-08-10T15:00:00Z"),
                None,
                None,
            )
            .unwrap();
        let mut pro = identity(Provider::Claude, "account-one", "pro");
        pro.extra_usage_enabled = Some(false);
        store
            .record_observation(
                Provider::Claude,
                Some(&pro),
                at("2026-08-10T17:00:00Z"),
                Some(at("2026-08-10T15:00:00Z")),
                None,
            )
            .unwrap();
        pro.extra_usage_enabled = Some(true);
        store
            .record_observation(
                Provider::Claude,
                Some(&pro),
                at("2026-08-10T18:00:00Z"),
                Some(at("2026-08-10T17:00:00Z")),
                None,
            )
            .unwrap();
        let burn = UsageBurnStore::new(&db_path).unwrap();
        burn.record_for_account(
            "claude:account-one",
            &[
                BurnWindowSample {
                    window_kind: "session_5h".to_owned(),
                    window_scope: None,
                    duration_minutes: 300,
                    percent: 100.0,
                    resets_at: at("2026-08-10T21:00:00Z"),
                    severity: Some("exhausted".to_owned()),
                    is_active: Some(true),
                },
                BurnWindowSample {
                    window_kind: "weekly_scoped".to_owned(),
                    window_scope: Some("claude-fable-5".to_owned()),
                    duration_minutes: 10_080,
                    percent: 10.0,
                    resets_at: at("2026-08-16T16:00:00Z"),
                    severity: None,
                    is_active: Some(false),
                },
            ],
            "test",
            at("2026-08-10T15:30:00Z"),
        )
        .unwrap();
        UsageLedgerStore::new(&db_path).unwrap();
        let mut connection = Connection::open(db_path).unwrap();
        let tx = connection.transaction().unwrap();

        assert!(!credit_metered(
            &tx,
            "claude:account-one",
            "claude-fable-5",
            at("2026-08-10T16:30:00Z")
        )
        .unwrap());
        assert!(credit_metered(
            &tx,
            "claude:account-one",
            "claude-fable-5",
            at("2026-08-10T17:30:00Z")
        )
        .unwrap());
        assert!(!credit_metered(
            &tx,
            "claude:account-one",
            "claude-sonnet-5",
            at("2026-08-10T17:30:00Z")
        )
        .unwrap());
        assert!(credit_metered(
            &tx,
            "claude:account-one",
            "claude-sonnet-5",
            at("2026-08-10T18:30:00Z")
        )
        .unwrap());
    }

    #[test]
    fn exhaustion_from_a_closed_window_does_not_mark_new_window_tokens_as_credits() {
        let dir = TestDir::new("closed-exhaustion-window");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "account-one",
            "max",
            "session_5h",
            300,
        );
        UsageBurnStore::new(&db_path)
            .unwrap()
            .record_for_account(
                "claude:account-one",
                &[BurnWindowSample {
                    window_kind: "session_5h".to_owned(),
                    window_scope: None,
                    duration_minutes: 300,
                    percent: 100.0,
                    resets_at: at("2026-08-10T16:00:00Z"),
                    severity: Some("exhausted".to_owned()),
                    is_active: Some(true),
                }],
                "test-closed-exhausted-window",
                at("2026-08-10T15:30:00Z"),
            )
            .unwrap();
        UsageLedgerStore::new(&db_path).unwrap();
        let mut connection = Connection::open(db_path).unwrap();
        let tx = connection.transaction().unwrap();

        assert!(credit_metered(
            &tx,
            "claude:account-one",
            "claude-sonnet-5",
            at("2026-08-10T15:45:00Z")
        )
        .unwrap());
        assert!(!credit_metered(
            &tx,
            "claude:account-one",
            "claude-sonnet-5",
            at("2026-08-10T16:30:00Z")
        )
        .unwrap());
    }

    #[test]
    fn scan_does_not_checkpoint_valid_messages_before_identity_exists() {
        let dir = TestDir::new("no-identity");
        let db_path = dir.0.join("usage.db");
        UsageBurnStore::new(&db_path).unwrap();
        let transcript = dir.0.join("session-one.jsonl");
        fs::write(
            &transcript,
            fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/usage/claude-parent.jsonl"),
            )
            .unwrap(),
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append(
                "seat-one",
                "claude",
                "claude-session-parent",
                transcript.to_str(),
            )
            .unwrap();
        let store = UsageLedgerStore::new(&db_path).unwrap();
        assert!(store.scan(&[]).is_err());
        let offset_count: i64 = Connection::open(db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM scan_offsets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(offset_count, 0);
    }

    #[test]
    fn scan_commits_a_bounded_checkpoint_before_a_later_line_failure() {
        let dir = TestDir::new("bounded-checkpoint");
        let db_path = dir.0.join("usage.db");
        UsageBurnStore::new(&db_path).unwrap();
        let transcript = dir.0.join("session-one.jsonl");
        let committed_prefix = "{}\n".repeat(LEDGER_WRITE_BATCH_SIZE);
        let failing_message = json!({
            "timestamp": "2026-08-10T16:30:00Z",
            "sessionId": "session-one",
            "requestId": "request-one",
            "cwd": "/repo",
            "version": "1.0.0",
            "message": {
                "id": "message-one",
                "model": "claude-sonnet-5",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        });
        fs::write(
            &transcript,
            format!("{committed_prefix}{failing_message}\n"),
        )
        .unwrap();
        SeatSessionStore::new(&db_path)
            .append("seat-one", "claude", "session-one", transcript.to_str())
            .unwrap();

        let store = UsageLedgerStore::new(&db_path).unwrap();
        assert!(store.scan(&[]).is_err());

        let connection = Connection::open(db_path).unwrap();
        let (offset, messages): (i64, i64) = connection
            .query_row(
                r#"
                SELECT
                  (SELECT byte_offset FROM scan_offsets WHERE artifact_path = ?1),
                  (SELECT COUNT(*) FROM message_ledger)
                "#,
                [transcript.to_string_lossy().as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(offset as usize, committed_prefix.len());
        assert_eq!(messages, 0);
    }

    #[test]
    fn scan_retries_an_unterminated_jsonl_tail_after_the_writer_finishes_it() {
        let dir = TestDir::new("unterminated-tail");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Claude,
            "account-one",
            "max",
            "session_5h",
            300,
        );
        let transcript = dir.0.join("session-one.jsonl");
        let line = json!({
            "timestamp": "2026-08-10T16:30:00Z",
            "sessionId": "session-one",
            "requestId": "request-one",
            "cwd": "/repo",
            "version": "1.0.0",
            "message": {
                "id": "message-one",
                "model": "claude-sonnet-5",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        })
        .to_string();
        let split = line.len() / 2;
        fs::write(&transcript, &line.as_bytes()[..split]).unwrap();
        SeatSessionStore::new(&db_path)
            .append("seat-one", "claude", "session-one", transcript.to_str())
            .unwrap();
        let store = UsageLedgerStore::new(&db_path).unwrap();
        let seat = UsageSeatMetadata {
            seat_id: "seat-one".to_owned(),
            friendly_name: None,
            provider: "claude".to_owned(),
            model: Some("claude-sonnet-5".to_owned()),
            effort: None,
            working_dir: "/repo".to_owned(),
            parent_seat_id: None,
            root_seat_id: Some("seat-one".to_owned()),
            project_key: "/repo".to_owned(),
        };

        store.scan(std::slice::from_ref(&seat)).unwrap();
        let connection = Connection::open(&db_path).unwrap();
        let (messages, offset): (i64, i64) = connection
            .query_row(
                r#"
                SELECT
                  (SELECT COUNT(*) FROM message_ledger),
                  (SELECT byte_offset FROM scan_offsets WHERE artifact_path = ?1)
                "#,
                [transcript.to_string_lossy().as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((messages, offset), (0, 0));

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        file.write_all(&line.as_bytes()[split..]).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);

        store.scan(&[seat]).unwrap();
        let connection = Connection::open(&db_path).unwrap();
        let (messages, offset): (i64, i64) = connection
            .query_row(
                r#"
                SELECT
                  (SELECT COUNT(*) FROM message_ledger),
                  (SELECT byte_offset FROM scan_offsets WHERE artifact_path = ?1)
                "#,
                [transcript.to_string_lossy().as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(messages, 1);
        assert_eq!(offset as u64, fs::metadata(transcript).unwrap().len());
    }

    #[test]
    fn one_provider_failure_does_not_block_another_artifact_commit() {
        let dir = TestDir::new("provider-independent");
        let db_path = dir.0.join("usage.db");
        seed_provider(
            &db_path,
            Provider::Codex,
            "codex-account",
            "pro",
            "codex_300",
            300,
        );
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/usage");
        let claude_path = dir.0.join("a-claude.jsonl");
        let codex_path = dir.0.join("z-codex.jsonl");
        fs::copy(fixtures.join("claude-parent.jsonl"), &claude_path).unwrap();
        fs::copy(fixtures.join("codex-family.jsonl"), &codex_path).unwrap();
        let sessions = SeatSessionStore::new(&db_path);
        sessions
            .append(
                "claude-seat",
                "claude",
                "claude-session-parent",
                claude_path.to_str(),
            )
            .unwrap();
        sessions
            .append(
                "codex-seat",
                "codex-fork",
                "codex-thread-1",
                codex_path.to_str(),
            )
            .unwrap();
        let store = UsageLedgerStore::new(&db_path).unwrap();
        let seats = [UsageSeatMetadata {
            seat_id: "codex-seat".to_owned(),
            friendly_name: None,
            provider: "codex-fork".to_owned(),
            model: Some("fallback".to_owned()),
            effort: None,
            working_dir: "/repo".to_owned(),
            parent_seat_id: None,
            root_seat_id: Some("codex-seat".to_owned()),
            project_key: "/repo".to_owned(),
        }];

        assert!(store.scan(&seats).is_err());
        let connection = Connection::open(db_path).unwrap();
        let codex_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM message_ledger WHERE source_ref = 'codex-thread-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(codex_rows, 1);
        let offsets = connection
            .prepare("SELECT artifact_path FROM scan_offsets ORDER BY artifact_path")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(offsets, vec![codex_path.display().to_string()]);
    }
}
