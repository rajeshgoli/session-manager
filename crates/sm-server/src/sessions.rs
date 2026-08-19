#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Arc, Condvar, Mutex, Weak},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Duration as TimeDuration,
    OffsetDateTime, PrimitiveDateTime,
};

use crate::queue::{
    followup_notification_text, ParentRoutingMessageRow, ParentRoutingSnapshot,
    ParentRoutingWakeRow, PendingMessage, QueueMessageMetadata, RetainedQueueStore,
    StopNotifyState,
};
use crate::{
    btw::BtwStore,
    config::{
        path_is_under_home, test_isolation_root_from_environment, CodexReviewConfig,
        ContextMonitorConfig,
    },
    runtime::{ConditionalClearOutcome, TmuxRuntime, TmuxSessionSpec},
    seat_sessions::{SeatSessionIdentity, SeatSessionStore},
    usage_burn::UsageBurnStore,
    usage_identity::{Provider as UsageProvider, UsageIdentityStore},
    usage_ledger::{ScanSummary, UsageLedgerStore, UsageSeatMetadata},
    usage_report::{UsageReport, UsageReportOptions, UsageReportStore, UsageReportTarget},
};

const DEFAULT_SESSION_STATE_FILE: &str = "~/.local/share/claude-sessions/sessions.json";
const LEGACY_TMP_SESSION_STATE_FILE: &str = "/tmp/claude-sessions/sessions.json";
const OUTPUT_TAIL_BYTES_PER_LINE: u64 = 4096;
const MIN_OUTPUT_TAIL_BYTES: u64 = 16 * 1024;
const MAX_OUTPUT_TAIL_BYTES: u64 = 1024 * 1024;
const CODEX_CLI_SESSION_BIND_TIMEOUT: Duration = Duration::from_secs(1);
const CODEX_CLI_DEFERRED_BIND_TIMEOUT: Duration = Duration::from_secs(30);
const CODEX_CLI_SESSION_BIND_POLL: Duration = Duration::from_millis(50);
const CODEX_FORK_THREAD_STARTED_TIMEOUT: Duration = Duration::from_secs(10);
const CODEX_FORK_EVENT_MONITOR_POLL: Duration = Duration::from_millis(250);
// Recovery must inspect a bounded tail before it starts at EOF: a native
// compaction while the service was down otherwise leaves an obsolete alert
// pending forever. Event streams are line-oriented; this covers the recent
// activity needed to establish the current root-thread occupancy without
// replaying unbounded history during startup.
const CODEX_FORK_CONTEXT_RECOVERY_TAIL_LINES: usize = 256;
const CODEX_FORK_CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const CODEX_FORK_CONTROL_RECOVERY_TIMEOUT: Duration = Duration::from_secs(1);
const CODEX_FORK_CONTROL_RECOVERY_POLL: Duration = Duration::from_millis(50);
const SEAT_SESSION_RETRY_ATTEMPTS: usize = 20;
const SEAT_SESSION_RETRY_DELAY: Duration = Duration::from_millis(100);
const REPARENT_REQUEST_TTL_HOURS: i64 = 24;
static STATE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct SessionStore {
    state_file: PathBuf,
    legacy_state_file: Option<PathBuf>,
    codex_sessions_root: PathBuf,
    claude_projects_roots: Vec<PathBuf>,
    write_lock: Arc<Mutex<()>>,
    /// Applies span the JSON registry and the retained queue, so their durable
    /// lease must have exactly one in-process driver. The persisted lease still
    /// provides restart recovery; this lock prevents a second HTTP request from
    /// racing a completed driver and mistaking its released lease for failure.
    reparent_apply_lock: Arc<Mutex<()>>,
    queue_store: Option<RetainedQueueStore>,
    /// Carried on the store rather than read per-request because the codex-fork
    /// event monitor threads evaluate the same thresholds and have no access to
    /// the HTTP layer's `AppConfig`.
    context_monitor: ContextMonitorConfig,
    /// Same reason: those threads enqueue context alerts, and nothing drains the
    /// message queue on a timer, so a message queued without a runtime waits for
    /// an unrelated request to happen to flush it.
    delivery_runtime: Option<TmuxRuntime>,
    codex_fork_handoff_monitors: Arc<Mutex<BTreeSet<String>>>,
    claude_handoff_workers: Arc<Mutex<BTreeSet<String>>>,
    credential_rotation_workers: Arc<Mutex<BTreeSet<String>>>,
    seat_session_appends: Arc<Mutex<BTreeSet<(String, String, String)>>>,
    clear_operation_locks: Arc<Mutex<BTreeMap<String, Weak<SessionClearLock>>>>,
    seat_session_store: SeatSessionStore,
    usage_identity_store: Option<UsageIdentityStore>,
    usage_burn_store: Option<UsageBurnStore>,
    usage_ledger_store: Option<UsageLedgerStore>,
    usage_report_store: Option<UsageReportStore>,
    usage_project_keys: Arc<Mutex<BTreeMap<String, (String, String)>>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SpawnBriefSource {
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SpawnBriefArtifact {
    pub sha256: String,
    pub path: String,
    pub byte_length: usize,
    pub source: SpawnBriefSource,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SpawnLaunchIntentRecord {
    pub id: String,
    pub artifact: SpawnBriefArtifact,
    pub requested_provider: String,
    #[serde(default)]
    pub requested_model: Option<String>,
    #[serde(default)]
    pub requested_reasoning_effort: Option<String>,
    #[serde(default)]
    pub requested_name: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub requested_node: Option<String>,
    #[serde(default)]
    pub requested_working_dir: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub accepted_at: String,
}

#[derive(Debug, Clone)]
pub struct AcceptSpawnBriefRequest {
    pub prompt: String,
    pub source: SpawnBriefSource,
    pub requested_provider: String,
    pub requested_model: Option<String>,
    pub requested_reasoning_effort: Option<String>,
    pub requested_name: Option<String>,
    pub parent_session_id: Option<String>,
    pub requested_node: Option<String>,
    pub requested_working_dir: Option<String>,
}

/// Set only by the HTTP handler after it has persisted and re-read the brief.
#[derive(Debug, Clone)]
pub struct SpawnBriefBinding {
    pub intent_id: String,
    pub sha256: String,
}

#[derive(Debug)]
pub struct SeatSessionReconciliationSnapshot {
    records: Vec<SessionRecord>,
    artifact_cutoff_ns: i128,
}

#[derive(Debug)]
struct SessionClearLock {
    held: Mutex<bool>,
    available: Condvar,
}

struct SessionClearGuard {
    lock: Arc<SessionClearLock>,
}

impl Drop for SessionClearGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = self.lock.held.lock() {
            *held = false;
            self.lock.available.notify_one();
        }
    }
}

impl SessionStore {
    pub fn new(state_file: PathBuf) -> Self {
        let test_isolation_root =
            test_isolation_root_from_environment().expect("invalid Rust test isolation root");
        let legacy_state_file = if state_file == expand_home(DEFAULT_SESSION_STATE_FILE) {
            Some(PathBuf::from(LEGACY_TMP_SESSION_STATE_FILE))
        } else {
            None
        };
        let seat_session_store = SeatSessionStore::for_state_file(&state_file);
        Self {
            state_file,
            legacy_state_file,
            codex_sessions_root: test_isolation_root
                .as_ref()
                .map(|root| root.join("codex-sessions"))
                .unwrap_or_else(|| expand_home("~/.codex/sessions")),
            claude_projects_roots: test_isolation_root
                .as_ref()
                .map(|root| vec![root.join("claude-projects")])
                .unwrap_or_else(|| claude_projects_roots(None)),
            write_lock: Arc::new(Mutex::new(())),
            reparent_apply_lock: Arc::new(Mutex::new(())),
            queue_store: None,
            context_monitor: ContextMonitorConfig::default(),
            delivery_runtime: None,
            codex_fork_handoff_monitors: Arc::new(Mutex::new(BTreeSet::new())),
            claude_handoff_workers: Arc::new(Mutex::new(BTreeSet::new())),
            credential_rotation_workers: Arc::new(Mutex::new(BTreeSet::new())),
            seat_session_appends: Arc::new(Mutex::new(BTreeSet::new())),
            clear_operation_locks: Arc::new(Mutex::new(BTreeMap::new())),
            seat_session_store,
            usage_identity_store: None,
            usage_burn_store: None,
            usage_ledger_store: None,
            usage_report_store: None,
            usage_project_keys: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Durably accept a launch brief before a session ID or runtime is allocated.
    /// The artifact is content-addressed and never rewritten after it is accepted.
    pub fn accept_spawn_brief(
        &self,
        request: AcceptSpawnBriefRequest,
    ) -> Result<SpawnLaunchIntentRecord> {
        validate_spawn_brief_source(&request.source)?;
        if request.prompt.trim().is_empty() {
            anyhow::bail!("spawn prompt must not be empty");
        }
        let _guard = self.write_guard()?;
        let artifact = persist_spawn_brief_artifact(
            &self.state_file,
            request.prompt.as_bytes(),
            request.source,
        )?;
        let mut state = self.load_raw_json_value()?;
        let intents = state
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("session state must be a JSON object"))?
            .entry("spawn_launch_intents")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("spawn_launch_intents must be an array"))?;
        let id = generate_unique_spawn_launch_intent_id(intents)?;
        let intent = SpawnLaunchIntentRecord {
            id,
            artifact,
            requested_provider: request.requested_provider,
            requested_model: optional_trimmed(request.requested_model.as_deref()),
            requested_reasoning_effort: optional_trimmed(
                request.requested_reasoning_effort.as_deref(),
            ),
            requested_name: optional_trimmed(request.requested_name.as_deref()),
            parent_session_id: optional_trimmed(request.parent_session_id.as_deref()),
            requested_node: optional_trimmed(request.requested_node.as_deref()),
            requested_working_dir: optional_trimmed(request.requested_working_dir.as_deref()),
            session_id: None,
            accepted_at: now_rfc3339(),
        };
        intents.push(serde_json::to_value(&intent)?);
        self.write_raw_json_value(&state)?;
        Ok(intent)
    }

    pub fn bind_spawn_launch_intent(&self, intent_id: &str, session_id: &str) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        bind_spawn_launch_intent_in_state(&mut state, intent_id, session_id)?;
        self.write_raw_json_value(&state)
    }

    pub fn read_spawn_brief(&self, artifact: &SpawnBriefArtifact) -> Result<String> {
        let bytes = fs::read(&artifact.path)
            .with_context(|| format!("failed to read accepted spawn brief {}", artifact.path))?;
        if bytes.len() != artifact.byte_length || sha256_bytes(&bytes) != artifact.sha256 {
            anyhow::bail!("accepted spawn brief artifact failed integrity verification");
        }
        String::from_utf8(bytes).context("accepted spawn brief is not valid UTF-8 text")
    }

    pub fn new_with_queue(state_file: PathBuf, queue_db_path: PathBuf) -> Self {
        let mut store = Self::new(state_file);
        store.queue_store = Some(RetainedQueueStore::new(queue_db_path));
        let _ = store.cancel_unsupported_context_alerts();
        store
    }

    pub fn with_usage_db_path(mut self, db_path: PathBuf) -> Self {
        self.seat_session_store = SeatSessionStore::new(db_path);
        self
    }

    pub fn with_usage_burn_store(mut self, store: UsageBurnStore) -> Self {
        self.usage_burn_store = Some(store);
        self
    }

    pub fn with_usage_identity_store(mut self, store: UsageIdentityStore) -> Self {
        self.usage_identity_store = Some(store);
        self
    }

    pub fn with_usage_ledger_store(mut self, store: UsageLedgerStore) -> Self {
        self.usage_ledger_store = Some(store);
        self
    }

    pub fn with_usage_report_store(mut self, store: UsageReportStore) -> Self {
        self.usage_report_store = Some(store);
        self
    }

    pub fn usage_report_for_session(
        &self,
        session_id: &str,
        include_children: bool,
        options: UsageReportOptions,
    ) -> Result<Option<UsageReport>> {
        let Some(store) = self.usage_report_store.as_ref() else {
            return Ok(None);
        };
        let Some(session) = self.get_session(session_id)? else {
            return Ok(None);
        };
        let all_sessions = self.load_snapshot()?.into_sessions();
        let mut descendants = Vec::new();
        let mut visited = BTreeSet::new();
        collect_descendants_preorder(&all_sessions, &session.id, &mut visited, &mut descendants);
        let available_descendant_count = descendants.len();
        let child_seats = if include_children {
            descendants
                .into_iter()
                .map(|descendant| descendant.id)
                .collect()
        } else {
            BTreeSet::new()
        };
        let target = UsageReportTarget {
            seat_id: session.id.clone(),
            friendly_name: session.cached_display_name(),
            account_key: session.account_key.clone(),
            usage_cap_fraction: session.usage_cap_fraction,
            self_seats: BTreeSet::from([session.id]),
            child_seats,
            available_descendant_count,
        };
        Ok(Some(store.report(Some(&target), options)?))
    }

    pub fn usage_report_for_accounts(
        &self,
        options: UsageReportOptions,
    ) -> Result<Option<UsageReport>> {
        let Some(store) = self.usage_report_store.as_ref() else {
            return Ok(None);
        };
        Ok(Some(store.report(None, options)?))
    }

    pub fn scan_usage_ledger(&self) -> Result<ScanSummary> {
        let Some(store) = self.usage_ledger_store.as_ref() else {
            return Ok(ScanSummary::default());
        };
        let records = self.load_snapshot()?.into_sessions();
        let observed_at = OffsetDateTime::now_utc();
        for record in &records {
            let provider = match record.provider.as_str() {
                "claude" => UsageProvider::Claude,
                "codex" | "codex-fork" => UsageProvider::Codex,
                _ => continue,
            };
            self.record_session_account_key(&record.id, provider, observed_at)?;
        }
        self.repair_current_codex_usage_artifacts(&records)?;
        let by_id = records
            .iter()
            .map(|record| (record.id.as_str(), record))
            .collect::<BTreeMap<_, _>>();
        let mut project_keys = self
            .usage_project_keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let seats = records
            .iter()
            .map(|record| {
                let project_key = project_keys
                    .get(&record.id)
                    .filter(|(working_dir, _)| working_dir == &record.working_dir)
                    .map(|(_, project_key)| project_key.clone())
                    .unwrap_or_else(|| {
                        let project_key =
                            UsageSeatMetadata::resolve_project_key(&record.working_dir);
                        project_keys.insert(
                            record.id.clone(),
                            (record.working_dir.clone(), project_key.clone()),
                        );
                        project_key
                    });
                UsageSeatMetadata {
                    seat_id: record.id.clone(),
                    friendly_name: record.cached_display_name(),
                    provider: record.provider.clone(),
                    model: record.model.clone(),
                    effort: record.reasoning_effort.clone(),
                    working_dir: record.working_dir.clone(),
                    parent_seat_id: record.parent_session_id.clone(),
                    root_seat_id: Some(root_seat_id(record, &by_id)),
                    project_key,
                }
            })
            .collect::<Vec<_>>();
        project_keys.retain(|seat_id, _| by_id.contains_key(seat_id.as_str()));
        drop(project_keys);
        store.scan(&seats)
    }

    fn repair_current_codex_usage_artifacts(&self, records: &[SessionRecord]) -> Result<()> {
        let missing = self
            .seat_session_store
            .provider_sessions_missing_artifacts("codex")?;
        if missing.is_empty() {
            return Ok(());
        }
        let current = records
            .iter()
            .filter(|record| record.provider == "codex")
            .filter_map(|record| {
                provider_resume_id_for_restore(record).and_then(|provider_session_id| {
                    missing
                        .contains(&provider_session_id)
                        .then_some((record, provider_session_id))
                })
            })
            .collect::<Vec<_>>();
        if current.is_empty() {
            return Ok(());
        }
        let ids = current
            .iter()
            .map(|(_, provider_session_id)| provider_session_id.clone())
            .collect::<BTreeSet<_>>();
        let paths = codex_cli_artifact_paths(&self.codex_sessions_root, &ids);
        let repairs = current
            .into_iter()
            .filter_map(|(record, provider_session_id)| {
                paths
                    .get(&provider_session_id)
                    .map(|path| SeatSessionIdentity {
                        seat_id: record.id.clone(),
                        provider: "codex".to_owned(),
                        provider_session_id,
                        artifact_path: Some(resolve_path_lossy(path.clone())),
                    })
            })
            .collect::<Vec<_>>();
        self.seat_session_store.append_batch(&repairs)
    }

    pub fn record_claude_statusline_burn(&self, event: &ContextUsageEvent) -> Result<usize> {
        let Some(store) = self.usage_burn_store.as_ref() else {
            return Ok(0);
        };
        let observed_at = event
            .emitted_at
            .as_deref()
            .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
            .unwrap_or_else(OffsetDateTime::now_utc);
        let recorded = store.record_claude_statusline(
            observed_at,
            event.five_hour_percent,
            event.five_hour_resets_at.as_deref(),
            event.seven_day_percent,
            event.seven_day_resets_at.as_deref(),
        )?;
        self.record_session_account_key(&event.session_id, UsageProvider::Claude, observed_at)?;
        Ok(recorded)
    }

    fn record_session_account_key(
        &self,
        session_id: &str,
        provider: UsageProvider,
        observed_at: OffsetDateTime,
    ) -> Result<()> {
        let Some(identity_store) = self.usage_identity_store.as_ref() else {
            return Ok(());
        };
        let Some(attribution) = identity_store.account_at(provider, observed_at)? else {
            return Ok(());
        };
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(());
        };
        if json_text(session.get("account_key")).as_deref()
            == Some(attribution.account_key.as_str())
        {
            return Ok(());
        }
        session.insert(
            "account_key".to_owned(),
            Value::String(attribution.account_key),
        );
        self.write_raw_json_value(&state)
    }

    pub fn with_context_monitor_config(mut self, config: ContextMonitorConfig) -> Self {
        self.context_monitor = config;
        self
    }

    pub fn with_codex_session_index_path(mut self, session_index_path: Option<&str>) -> Self {
        if let Some(session_index_path) = session_index_path
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let session_index_path = expand_home(session_index_path);
            let test_root =
                test_isolation_root_from_environment().expect("invalid Rust test isolation root");
            // Test launchers may retain explicit fixture indexes outside HOME,
            // but must never resolve the developer's live Codex index.
            let is_live_home_path =
                path_is_under_home(session_index_path.to_string_lossy().as_ref()).unwrap_or(true);
            if !test_root.is_some() || !is_live_home_path {
                if let Some(parent) = session_index_path.parent() {
                    self.codex_sessions_root = parent.join("sessions");
                }
            }
        }
        self
    }

    pub fn with_claude_transcript_root(mut self, transcript_root: Option<&str>) -> Self {
        if let Some(root) =
            test_isolation_root_from_environment().expect("invalid Rust test isolation root")
        {
            // `claude_projects_roots(None)` normally includes CLAUDE_CONFIG_DIR,
            // XDG_CONFIG_HOME, and HOME. A test process must not scan any of
            // those external roots, even after AppState applies its config. A
            // deliberately supplied fixture root outside HOME remains usable.
            let configured_root = transcript_root
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(expand_home);
            let configured_under_home = configured_root.as_ref().is_some_and(|path| {
                path_is_under_home(path.to_string_lossy().as_ref()).unwrap_or(true)
            });
            self.claude_projects_roots = match configured_root {
                Some(path) if !configured_under_home => vec![path],
                _ => vec![root.join("claude-projects")],
            };
        } else {
            self.claude_projects_roots = claude_projects_roots(transcript_root);
        }
        self
    }

    fn append_seat_session(
        &self,
        seat_id: &str,
        provider: &str,
        provider_session_id: &str,
        artifact_path: Option<&str>,
    ) {
        let append_key = (
            seat_id.to_owned(),
            provider.to_owned(),
            provider_session_id.to_owned(),
        );
        {
            let Ok(mut appends) = self.seat_session_appends.lock() else {
                eprintln!("usage ledger seat session append lock poisoned");
                return;
            };
            if !appends.insert(append_key.clone()) {
                return;
            }
        }
        let Err(error) =
            self.seat_session_store
                .append(seat_id, provider, provider_session_id, artifact_path)
        else {
            return;
        };
        eprintln!(
            "usage ledger failed to append provider session {provider_session_id} for seat {seat_id}: {error:#}"
        );
        if !usage_ledger_error_is_transient(&error) {
            if let Ok(mut appends) = self.seat_session_appends.lock() {
                appends.remove(&append_key);
            }
            return;
        }

        let store = self.seat_session_store.clone();
        let appends = self.seat_session_appends.clone();
        let retry_key = append_key.clone();
        let seat_id = seat_id.to_owned();
        let provider = provider.to_owned();
        let provider_session_id = provider_session_id.to_owned();
        let artifact_path = artifact_path.map(ToOwned::to_owned);
        let thread_name = format!(
            "sm-seat-session-retry-{}",
            sanitize_path_component(&seat_id)
        );
        if let Err(error) = thread::Builder::new().name(thread_name).spawn(move || {
            for attempt in 1..=SEAT_SESSION_RETRY_ATTEMPTS {
                thread::sleep(SEAT_SESSION_RETRY_DELAY);
                match store.append(
                    &seat_id,
                    &provider,
                    &provider_session_id,
                    artifact_path.as_deref(),
                ) {
                    Ok(()) => return,
                    Err(error) if usage_ledger_error_is_transient(&error) => {
                        if attempt == SEAT_SESSION_RETRY_ATTEMPTS {
                            eprintln!(
                                "usage ledger exhausted retries for provider session {provider_session_id} on seat {seat_id}: {error:#}"
                            );
                            if let Ok(mut appends) = appends.lock() {
                                appends.remove(&retry_key);
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "usage ledger retry failed permanently for provider session {provider_session_id} on seat {seat_id}: {error:#}"
                        );
                        if let Ok(mut appends) = appends.lock() {
                            appends.remove(&retry_key);
                        }
                        return;
                    }
                }
            }
        }) {
            eprintln!("failed to start usage ledger retry thread: {error}");
            if let Ok(mut appends) = self.seat_session_appends.lock() {
                appends.remove(&append_key);
            }
        }
    }

    pub fn reconcile_current_seat_sessions(&self) -> Result<()> {
        let snapshot = self.prepare_seat_session_reconciliation(i128::MAX)?;
        self.reconcile_seat_sessions(snapshot)
    }

    #[cfg(test)]
    fn reconcile_current_seat_sessions_through(&self, artifact_cutoff_ns: i128) -> Result<()> {
        let snapshot = self.prepare_seat_session_reconciliation(artifact_cutoff_ns)?;
        self.reconcile_seat_sessions(snapshot)
    }

    pub fn prepare_seat_session_reconciliation(
        &self,
        artifact_cutoff_ns: i128,
    ) -> Result<SeatSessionReconciliationSnapshot> {
        Ok(SeatSessionReconciliationSnapshot {
            records: self.load_snapshot()?.into_sessions(),
            artifact_cutoff_ns,
        })
    }

    pub fn reconcile_seat_sessions(
        &self,
        snapshot: SeatSessionReconciliationSnapshot,
    ) -> Result<()> {
        let SeatSessionReconciliationSnapshot {
            records,
            artifact_cutoff_ns,
        } = snapshot;
        let mut claim_attempt = 0;
        let mut claimed = loop {
            match self.seat_session_store.claimed_provider_sessions() {
                Ok(claimed) => break claimed,
                Err(error)
                    if usage_ledger_error_is_transient(&error)
                        && claim_attempt < SEAT_SESSION_RETRY_ATTEMPTS =>
                {
                    claim_attempt += 1;
                    thread::sleep(SEAT_SESSION_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        };
        let fork_artifact_paths = self
            .delivery_runtime
            .as_ref()
            .map(|runtime| {
                records
                    .iter()
                    .filter_map(|session| {
                        codex_fork_event_stream_path(session, runtime)
                            .map(|path| (session.id.clone(), path))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let current_codex_ids = records
            .iter()
            .filter(|session| session.provider == "codex")
            .filter_map(provider_resume_id_for_restore)
            .collect::<BTreeSet<_>>();
        let codex_artifact_paths =
            codex_cli_artifact_paths(&self.codex_sessions_root, &current_codex_ids);
        let current_identities = records
            .iter()
            .filter_map(|session| {
                provider_resume_id_for_restore(session).map(|provider_session_id| {
                    let artifact_path = fork_artifact_paths
                        .get(&session.id)
                        .map(|path| path.display().to_string())
                        .or_else(|| {
                            (session.provider == "codex")
                                .then(|| codex_artifact_paths.get(&provider_session_id))
                                .flatten()
                                .map(|path| resolve_path_lossy(path.clone()))
                        })
                        .or_else(|| session.transcript_path.clone());
                    SeatSessionIdentity {
                        seat_id: session.id.clone(),
                        provider: session.provider.clone(),
                        provider_session_id,
                        artifact_path,
                    }
                })
            })
            .collect::<Vec<_>>();
        let mut current_by_provider_session = BTreeMap::<_, Vec<_>>::new();
        for identity in current_identities {
            current_by_provider_session
                .entry((
                    identity.provider.clone(),
                    identity.provider_session_id.clone(),
                ))
                .or_default()
                .push(identity);
        }
        let mut sessions = Vec::with_capacity(current_by_provider_session.len());
        for ((provider, provider_session_id), mut identities) in current_by_provider_session {
            claimed.insert((provider.clone(), provider_session_id.clone()));
            if identities.len() == 1 {
                sessions.push(identities.pop().expect("one current identity"));
                continue;
            }
            let artifact_path = identities
                .first()
                .and_then(|identity| identity.artifact_path.clone())
                .filter(|path| {
                    identities
                        .iter()
                        .all(|identity| identity.artifact_path.as_deref() == Some(path))
                });
            sessions.push(SeatSessionIdentity {
                seat_id: "unassigned".to_owned(),
                provider,
                provider_session_id,
                artifact_path,
            });
        }
        sessions.extend(historical_claude_seat_sessions(
            &records,
            &self.claude_projects_roots,
            artifact_cutoff_ns,
            &mut claimed,
        ));
        sessions.extend(historical_codex_cli_seat_sessions(
            &records,
            &self.codex_sessions_root,
            artifact_cutoff_ns,
            &mut claimed,
        ));
        if let Some(runtime) = self.delivery_runtime.as_ref() {
            sessions.extend(historical_codex_fork_seat_sessions(
                &records,
                runtime,
                &mut claimed,
            ));
        }
        for attempt in 0..=SEAT_SESSION_RETRY_ATTEMPTS {
            match self.seat_session_store.append_batch(&sessions) {
                Ok(()) => return Ok(()),
                Err(error)
                    if usage_ledger_error_is_transient(&error)
                        && attempt < SEAT_SESSION_RETRY_ATTEMPTS =>
                {
                    thread::sleep(SEAT_SESSION_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded reconciliation loop always returns")
    }

    /// Drop context alerts this session raised about context it no longer has.
    /// An undelivered warning describes the discarded cycle, so delivering it
    /// after a clear tells the monitor about a problem that no longer exists.
    fn cancel_context_monitor_alerts(&self, session_id: &str) -> Result<()> {
        if let Some(queue) = &self.queue_store {
            queue.cancel_pending_messages_from_sender_category(session_id, "context_monitor")?;
        }
        Ok(())
    }

    fn cancel_unsupported_context_alerts(&self) -> Result<()> {
        for session in self
            .load_snapshot()?
            .into_sessions()
            .into_iter()
            .filter(|session| !provider_has_measured_context_gauge(&session.provider))
        {
            self.cancel_context_monitor_alerts(&session.id)?;
        }
        Ok(())
    }

    /// Runtime used to deliver messages queued from background threads, which
    /// have no request to piggyback a drain on.
    pub fn with_delivery_runtime(mut self, runtime: Option<TmuxRuntime>) -> Self {
        self.delivery_runtime = runtime;
        self
    }

    pub fn recover_session_runtime_launches(&self) -> Result<()> {
        loop {
            let launch_id = {
                let _guard = self.write_guard()?;
                let state = self.load_raw_json_value()?;
                session_runtime_launch_records(&state)?
                    .into_iter()
                    .find(|record| matches!(record.status.as_str(), "prepared" | "launching"))
                    .map(|record| record.id)
            };
            let Some(launch_id) = launch_id else {
                return Ok(());
            };
            self.recover_session_runtime_launch(&launch_id)?;
        }
    }

    fn recover_session_runtime_launch(&self, launch_id: &str) -> Result<()> {
        let pending_launch = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            session_runtime_launch_records(&state)?
                .into_iter()
                .find(|record| record.id == launch_id)
        };
        let codex_cli_binding_guard = pending_launch
            .as_ref()
            .filter(|launch| launch.operation_kind == "create" && launch.provider == "codex")
            .map(|launch| self.lock_codex_cli_binding_working_dir(&launch.working_dir))
            .transpose()?;
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let Some(mut launch) = session_runtime_launch_records(&state)?
            .into_iter()
            .find(|record| record.id == launch_id)
        else {
            return Ok(());
        };
        if !matches!(launch.status.as_str(), "prepared" | "launching") {
            return Ok(());
        }
        let terminal_session = snapshot_from_raw_value(&state)?
            .sessions
            .iter()
            .find(|session| session.id == launch.session_id)
            .is_some_and(|session| {
                completion_status_is_retired(session.completion_status.as_deref())
            });
        if terminal_session && !launch.is_authorized_restore_intent() {
            // Recovery is allowed to reconstruct interrupted launches, never
            // to revive a seat whose durable lifecycle has already become
            // terminal.  Finalize both records before any runtime/provider
            // operation so a later recovery pass cannot resurrect it.
            finalize_active_credential_rotations_for_terminal_session(
                &mut state,
                &launch.session_id,
            )?;
            mark_runtime_launch_failed(
                &mut state,
                launch_id,
                &launch.session_id,
                false,
                "target_terminal",
            )?;
            self.write_raw_json_value(&state)?;
            return Ok(());
        }
        let Some(runtime) = self.delivery_runtime.as_ref() else {
            mark_runtime_launch_failed(
                &mut state,
                launch_id,
                &launch.session_id,
                launch.operation_kind == "create",
                "runtime launch recovery is disabled",
            )?;
            self.write_raw_json_value(&state)?;
            return Ok(());
        };
        let session_runtime = runtime.for_socket_name(launch.tmux_socket_name.as_deref());
        let credential = generate_session_credential();
        let credential_sha256 = sha256_text(&credential);
        launch.credential_sha256 = credential_sha256.clone();
        launch.status = "launching".to_owned();
        launch.updated_at = now_rfc3339();
        launch.failure_reason = None;
        let mut records = session_runtime_launch_records(&state)?;
        let Some(stored_launch) = records.iter_mut().find(|record| record.id == launch_id) else {
            return Ok(());
        };
        *stored_launch = launch.clone();
        store_session_runtime_launch_records(&mut state, &records)?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, &launch.session_id) else {
            mark_runtime_launch_failed(
                &mut state,
                launch_id,
                &launch.session_id,
                false,
                "runtime launch session is missing",
            )?;
            self.write_raw_json_value(&state)?;
            return Ok(());
        };
        session.insert(
            "session_credential_sha256".to_owned(),
            Value::String(credential_sha256),
        );
        session.insert("status".to_owned(), Value::String("stopped".to_owned()));
        session.insert("stopped_at".to_owned(), Value::String(now_rfc3339()));
        self.write_raw_json_value(&state)?;

        let spec = TmuxSessionSpec {
            session_id: launch.session_id.clone(),
            session_credential: Some(credential),
            tmux_session: launch.tmux_session.clone(),
            working_dir: launch.working_dir.clone(),
            log_file: PathBuf::from(&launch.log_file),
            provider: launch.provider.clone(),
            initial_message: launch.initial_message.clone(),
            force_initial_prompt_stdin: launch.force_initial_prompt_stdin,
            model: launch.model.clone(),
            reasoning_effort: launch.reasoning_effort.clone(),
        };
        let codex_fork_artifacts = session_runtime.codex_fork_runtime_artifacts(&spec)?;
        let codex_cli_creation_binding =
            if launch.operation_kind == "create" && launch.provider == "codex" {
                let snapshot = snapshot_from_raw_value(&state)?;
                let record = snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id == launch.session_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("runtime launch session disappeared before recovery")
                    })?;
                let mut excluded_ids = snapshot
                    .sessions
                    .iter()
                    .filter_map(|session| session.provider_resume_id.clone())
                    .collect::<BTreeSet<_>>();
                excluded_ids.extend(codex_cli_existing_session_ids(
                    record,
                    &self.codex_sessions_root,
                ));
                Some((
                    record.clone(),
                    excluded_ids,
                    OffsetDateTime::now_utc().unix_timestamp_nanos(),
                ))
            } else {
                None
            };
        let result = (|| -> Result<()> {
            if session_runtime.session_exists(&launch.tmux_session)? {
                session_runtime.kill_session(&launch.tmux_session)?;
            }
            if launch.operation_kind == "create" {
                session_runtime.create_session(&spec)
            } else {
                session_runtime.restore_session(
                    &spec,
                    &launch.provider,
                    launch.provider_resume_id.as_deref(),
                )
            }
        })();
        if let Err(error) = result {
            mark_runtime_launch_failed(
                &mut state,
                launch_id,
                &launch.session_id,
                launch.operation_kind == "create",
                &error.to_string(),
            )?;
            self.write_raw_json_value(&state)?;
            return Ok(());
        }

        let mut recovered_provider_resume_id = launch.provider_resume_id.clone();
        if launch.operation_kind == "create" {
            if let Some((record, excluded_ids, launched_at_ns)) =
                codex_cli_creation_binding.as_ref()
            {
                recovered_provider_resume_id = wait_for_codex_cli_provider_resume_id(
                    record,
                    &self.codex_sessions_root,
                    excluded_ids,
                    *launched_at_ns,
                    CODEX_CLI_SESSION_BIND_TIMEOUT,
                );
            }
            if let Some(artifacts) = codex_fork_artifacts.as_ref() {
                match wait_for_codex_fork_provider_resume_id_for_launch(
                    &artifacts.event_stream_path,
                    CODEX_FORK_THREAD_STARTED_TIMEOUT,
                    &session_runtime,
                    &launch.tmux_session,
                ) {
                    Ok(provider_resume_id) => {
                        recovered_provider_resume_id = Some(provider_resume_id);
                    }
                    Err(error) => {
                        let _ = session_runtime.kill_session(&launch.tmux_session);
                        mark_runtime_launch_failed(
                            &mut state,
                            launch_id,
                            &launch.session_id,
                            true,
                            &error.to_string(),
                        )?;
                        self.write_raw_json_value(&state)?;
                        return Ok(());
                    }
                }
            }
        }

        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, &launch.session_id) else {
            let _ = session_runtime.kill_session(&launch.tmux_session);
            mark_runtime_launch_failed(
                &mut state,
                launch_id,
                &launch.session_id,
                false,
                "runtime launch session disappeared after recovery",
            )?;
            self.write_raw_json_value(&state)?;
            return Ok(());
        };
        session.insert("status".to_owned(), Value::String("running".to_owned()));
        session.insert("stopped_at".to_owned(), Value::Null);
        if launch.operation_kind == "restore" {
            session.insert("completion_status".to_owned(), Value::Null);
            session.insert("completion_message".to_owned(), Value::Null);
            session.insert("completed_at".to_owned(), Value::Null);
            session.insert("agent_task_completed_at".to_owned(), Value::Null);
        }
        if let Some(provider_resume_id) = recovered_provider_resume_id.as_deref() {
            session.insert(
                "provider_resume_id".to_owned(),
                Value::String(provider_resume_id.to_owned()),
            );
        }
        session.insert("last_activity".to_owned(), Value::String(now_rfc3339()));
        let recovered = serde_json::from_value::<SessionRecord>(Value::Object(session.clone()))?;
        mark_runtime_launch_applied(
            &mut state,
            launch_id,
            recovered_provider_resume_id.as_deref(),
        )?;
        self.write_raw_json_value(&state)?;
        drop(_guard);
        if let Some(provider_resume_id) = recovered_provider_resume_id.as_deref() {
            self.append_seat_session(
                &launch.session_id,
                &launch.provider,
                provider_resume_id,
                codex_fork_artifacts
                    .as_ref()
                    .and_then(|artifacts| artifacts.event_stream_path.to_str()),
            );
        }
        if recovered_provider_resume_id.is_none() {
            if let Some((record, excluded_ids, launched_at_ns)) = codex_cli_creation_binding {
                let store = self.clone();
                let session_id = launch.session_id.clone();
                let spawn_result = thread::Builder::new()
                    .name(format!(
                        "sm-codex-recovery-bind-{}",
                        sanitize_path_component(&session_id)
                    ))
                    .spawn(move || {
                        let _binding_guard = codex_cli_binding_guard;
                        if let Err(error) = store.complete_deferred_codex_cli_rebind(
                            &session_id,
                            &record,
                            &excluded_ids,
                            launched_at_ns,
                            None,
                            CODEX_CLI_DEFERRED_BIND_TIMEOUT,
                        ) {
                            eprintln!(
                                "deferred recovered Codex thread discovery failed for seat {session_id}: {error:#}"
                            );
                        }
                    });
                if let Err(error) = spawn_result {
                    eprintln!("failed to start recovered Codex thread discovery: {error}");
                }
            }
        }
        if let Some(artifacts) = codex_fork_artifacts {
            self.start_codex_fork_event_monitor(recovered.id, artifacts.event_stream_path)?;
        }
        Ok(())
    }

    fn start_credential_rotation_worker(&self, session_id: String) -> Result<()> {
        {
            let mut workers = self
                .credential_rotation_workers
                .lock()
                .map_err(|_| anyhow::anyhow!("credential rotation worker registry poisoned"))?;
            if !workers.insert(session_id.clone()) {
                return Ok(());
            }
        }
        let store = self.clone();
        let worker_session_id = session_id.clone();
        let spawn_result = thread::Builder::new()
            .name(format!(
                "sm-recredential-{}",
                sanitize_path_component(&session_id)
            ))
            .spawn(move || {
                loop {
                    match store.try_apply_waiting_credential_rotation(&worker_session_id) {
                        Ok(true) => break,
                        Ok(false) => thread::sleep(Duration::from_secs(1)),
                        Err(error) => {
                            eprintln!(
                                "credential rotation worker retrying for {worker_session_id}: {error:#}"
                            );
                            thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
                if let Ok(mut workers) = store.credential_rotation_workers.lock() {
                    workers.remove(&worker_session_id);
                }
            });
        if let Err(error) = spawn_result {
            if let Ok(mut workers) = self.credential_rotation_workers.lock() {
                workers.remove(&session_id);
            }
            return Err(error).context("failed to start credential rotation worker");
        }
        Ok(())
    }

    fn try_apply_waiting_credential_rotation(&self, session_id: &str) -> Result<bool> {
        let (rotation, session, recovering_launch_id) = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let rotations = session_credential_rotation_records(&state)?;
            let recovering_launch_id = rotations
                .iter()
                .find(|record| record.session_id == session_id && record.status == "relaunching")
                .map(|rotation| {
                    rotation.runtime_launch_id.clone().with_context(|| {
                        format!(
                            "relaunching credential rotation {} has no runtime launch",
                            rotation.id
                        )
                    })
                })
                .transpose()?;
            if recovering_launch_id.is_some() {
                (None, None, recovering_launch_id)
            } else {
                let Some(rotation) = rotations.into_iter().find(|record| {
                    record.session_id == session_id && record.status == "waiting_idle"
                }) else {
                    return Ok(true);
                };
                let Some(session) = snapshot_from_raw_value(&state)?
                    .sessions
                    .into_iter()
                    .find(|session| session.id == session_id)
                else {
                    return Ok(true);
                };
                (Some(rotation), Some(session), None)
            }
        };
        if let Some(launch_id) = recovering_launch_id {
            self.recover_session_runtime_launch(&launch_id)?;
            return Ok(true);
        }
        let rotation = rotation.expect("waiting rotation checked above");
        let session = session.expect("waiting rotation session checked above");
        if !session.is_live_for_registry() {
            self.finalize_terminal_credential_rotation(&rotation.id, session_id)?;
            return Ok(true);
        }
        let Some(runtime) = self.delivery_runtime.as_ref() else {
            return Ok(true);
        };
        let session_runtime = runtime.for_socket_name(rotation.tmux_socket_name.as_deref());
        if !session_runtime.session_exists(&rotation.tmux_session)? {
            self.finalize_missing_runtime_credential_rotation(&rotation.id, session_id)?;
            return Ok(true);
        }
        if !credential_rotation_has_fresh_idle_proof(&rotation, &session)
            || session_runtime.session_has_attached_clients(&rotation.tmux_session)?
            || !session_runtime.session_input_ready(&rotation.tmux_session, &rotation.provider)
            || !self.credential_rotation_queue_is_drained(session_id)?
            || self.credential_rotation_has_active_btw(session_id)?
        {
            return Ok(false);
        }

        let (_clear_guard, _guard, _input_guard) = self.lock_credential_rotation_fences(
            session_id,
            &session_runtime,
            &rotation.tmux_session,
        )?;
        let mut state = self.load_raw_json_value()?;
        let mut rotations = session_credential_rotation_records(&state)?;
        let Some(rotation_index) = rotations
            .iter()
            .position(|record| record.id == rotation.id && record.status == "waiting_idle")
        else {
            return Ok(true);
        };
        if !session_runtime.session_exists(&rotation.tmux_session)? {
            mark_session_runtime_missing_terminal(&mut state, session_id)?;
            self.write_raw_json_value(&state)?;
            return Ok(true);
        }
        let snapshot = snapshot_from_raw_value(&state)?;
        let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
        else {
            rotations[rotation_index].status = "failed".to_owned();
            rotations[rotation_index].updated_at = now_rfc3339();
            rotations[rotation_index].failure_reason = Some("session disappeared".to_owned());
            store_session_credential_rotation_records(&mut state, &rotations)?;
            self.write_raw_json_value(&state)?;
            return Ok(true);
        };
        if !session.is_live_for_registry() {
            finalize_active_credential_rotations_for_terminal_session(&mut state, session_id)?;
            self.write_raw_json_value(&state)?;
            return Ok(true);
        }
        if session.provider != rotation.provider
            || session.tmux_session != rotation.tmux_session
            || session.tmux_socket_name != rotation.tmux_socket_name
            || provider_resume_id_for_restore(&session).as_deref()
                != Some(rotation.provider_resume_id.as_str())
        {
            rotations[rotation_index].status = "failed".to_owned();
            rotations[rotation_index].updated_at = now_rfc3339();
            rotations[rotation_index].failure_reason =
                Some("session runtime identity changed while waiting for idle".to_owned());
            store_session_credential_rotation_records(&mut state, &rotations)?;
            self.write_raw_json_value(&state)?;
            return Ok(true);
        }
        let raw_session = raw_session_object(&state, session_id)
            .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared"))?;
        if !credential_rotation_has_fresh_idle_proof(&rotations[rotation_index], &session)
            || active_reparent_request_for_session(&state, session_id)?.is_some()
            || json_text(raw_session.get("pending_handoff_path")).is_some()
            || json_text(raw_session.get("claude_handoff_in_progress_at")).is_some()
            || review_dispatch_in_progress(raw_session)
            || !self.credential_rotation_queue_is_drained(session_id)?
            || self.credential_rotation_has_active_btw(session_id)?
            || session_runtime.session_has_attached_clients(&rotation.tmux_session)?
            || !session_runtime.session_input_ready(&rotation.tmux_session, &rotation.provider)
        {
            return Ok(false);
        }

        let Some(log_file) = session
            .log_file
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(expand_home)
        else {
            rotations[rotation_index].status = "failed".to_owned();
            rotations[rotation_index].updated_at = now_rfc3339();
            rotations[rotation_index].failure_reason =
                Some("session log file is missing".to_owned());
            store_session_credential_rotation_records(&mut state, &rotations)?;
            self.write_raw_json_value(&state)?;
            return Ok(true);
        };
        let credential = generate_session_credential();
        let credential_sha256 = sha256_text(&credential);
        let spec = TmuxSessionSpec {
            session_id: session.id.clone(),
            session_credential: Some(credential),
            tmux_session: session.tmux_session.clone(),
            working_dir: expand_home(&session.working_dir).display().to_string(),
            log_file,
            provider: session.provider.clone(),
            initial_message: None,
            force_initial_prompt_stdin: false,
            model: session.model.clone(),
            reasoning_effort: session.reasoning_effort.clone(),
        };
        let codex_fork_artifacts = session_runtime.codex_fork_runtime_artifacts(&spec)?;
        let mut launches = session_runtime_launch_records(&state)?;
        let launch_id = generate_unique_runtime_launch_id(&launches)?;
        let now = now_rfc3339();
        launches.push(SessionRuntimeLaunchRecord {
            id: launch_id.clone(),
            operation_kind: "recredential".to_owned(),
            session_id: session.id.clone(),
            tmux_session: session.tmux_session.clone(),
            tmux_socket_name: session_runtime.socket_name().map(ToOwned::to_owned),
            working_dir: spec.working_dir.clone(),
            log_file: spec.log_file.display().to_string(),
            provider: session.provider.clone(),
            provider_resume_id: Some(rotation.provider_resume_id.clone()),
            credential_rotation_id: Some(rotation.id.clone()),
            restore_authorized: false,
            initial_message: None,
            model: session.model.clone(),
            reasoning_effort: session.reasoning_effort.clone(),
            spawn_launch_intent_id: None,
            spawn_brief_sha256: None,
            force_initial_prompt_stdin: false,
            credential_sha256: credential_sha256.clone(),
            status: "launching".to_owned(),
            created_at: now.clone(),
            updated_at: now.clone(),
            failure_reason: None,
        });
        rotations[rotation_index].status = "relaunching".to_owned();
        rotations[rotation_index].idle_proof_at = Some(now.clone());
        rotations[rotation_index].runtime_launch_id = Some(launch_id.clone());
        rotations[rotation_index].updated_at = now;
        {
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let raw_session = session_object_mut(sessions, session_id)
                .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared"))?;
            raw_session.insert(
                "session_credential_sha256".to_owned(),
                Value::String(credential_sha256),
            );
            raw_session.insert("status".to_owned(), Value::String("stopped".to_owned()));
            raw_session.insert("stopped_at".to_owned(), Value::String(now_rfc3339()));
        }
        store_session_runtime_launch_records(&mut state, &launches)?;
        store_session_credential_rotation_records(&mut state, &rotations)?;
        self.write_raw_json_value(&state)?;

        let relaunch_result = (|| -> Result<()> {
            if session_runtime.session_exists(&session.tmux_session)? {
                session_runtime.kill_session(&session.tmux_session)?;
            }
            session_runtime.restore_session(
                &spec,
                &session.provider,
                Some(&rotation.provider_resume_id),
            )
        })();
        if let Err(error) = relaunch_result {
            mark_runtime_launch_failed(
                &mut state,
                &launch_id,
                session_id,
                false,
                &error.to_string(),
            )?;
            self.write_raw_json_value(&state)?;
            return Ok(true);
        }

        // The runtime could die after restore returns but before the durable
        // applied transition.  Check it under the retained session mutation
        // lock, then ensure a terminal writer did not finalize the rotation.
        if !session_runtime.session_exists(&session.tmux_session)? {
            mark_runtime_launch_failed(
                &mut state,
                &launch_id,
                session_id,
                false,
                "target_terminal",
            )?;
            self.write_raw_json_value(&state)?;
            return Ok(true);
        }
        let rotation_still_relaunching =
            session_credential_rotation_records(&state)?
                .iter()
                .any(|record| {
                    record.id == rotation.id
                        && record.status == "relaunching"
                        && record.runtime_launch_id.as_deref() == Some(launch_id.as_str())
                });
        if !rotation_still_relaunching {
            return Ok(true);
        }

        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(raw_session) = session_object_mut(sessions, session_id) else {
            let _ = session_runtime.kill_session(&session.tmux_session);
            mark_runtime_launch_failed(
                &mut state,
                &launch_id,
                session_id,
                false,
                "session disappeared after credential relaunch",
            )?;
            self.write_raw_json_value(&state)?;
            return Ok(true);
        };
        raw_session.insert("status".to_owned(), Value::String("running".to_owned()));
        raw_session.insert("stopped_at".to_owned(), Value::Null);
        raw_session.insert("last_activity".to_owned(), Value::String(now_rfc3339()));
        mark_runtime_launch_applied(&mut state, &launch_id, Some(&rotation.provider_resume_id))?;
        self.write_raw_json_value(&state)?;
        drop(_guard);
        if let Some(artifacts) = codex_fork_artifacts {
            self.start_codex_fork_event_monitor(session.id, artifacts.event_stream_path)?;
        }
        Ok(true)
    }

    fn finalize_terminal_credential_rotation(
        &self,
        _rotation_id: &str,
        session_id: &str,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        finalize_active_credential_rotations_for_terminal_session(&mut state, session_id)?;
        self.write_raw_json_value(&state)
    }

    fn finalize_missing_runtime_credential_rotation(
        &self,
        _rotation_id: &str,
        session_id: &str,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        mark_session_runtime_missing_terminal(&mut state, session_id)?;
        self.write_raw_json_value(&state)
    }

    fn credential_rotation_queue_is_drained(&self, session_id: &str) -> Result<bool> {
        self.queue_store
            .as_ref()
            .map(|queue| {
                queue
                    .pending_messages_for_target(session_id, 1)
                    .map(|messages| messages.is_empty())
            })
            .unwrap_or(Ok(true))
    }

    fn credential_rotation_has_active_btw(&self, session_id: &str) -> Result<bool> {
        let Some(queue) = self.queue_store.as_ref() else {
            return Ok(false);
        };
        if !queue.db_path().exists() {
            return Ok(false);
        }
        Ok(BtwStore::new(queue.db_path().to_path_buf())?
            .active_for_target(session_id)?
            .is_some())
    }

    #[cfg(test)]
    fn new_with_legacy_fallback(state_file: PathBuf, legacy_state_file: PathBuf) -> Self {
        let test_isolation_root =
            test_isolation_root_from_environment().expect("invalid Rust test isolation root");
        let seat_session_store = SeatSessionStore::for_state_file(&state_file);
        Self {
            state_file,
            legacy_state_file: Some(legacy_state_file),
            codex_sessions_root: test_isolation_root
                .as_ref()
                .map(|root| root.join("codex-sessions"))
                .unwrap_or_else(|| expand_home("~/.codex/sessions")),
            claude_projects_roots: test_isolation_root
                .as_ref()
                .map(|root| vec![root.join("claude-projects")])
                .unwrap_or_else(|| claude_projects_roots(None)),
            write_lock: Arc::new(Mutex::new(())),
            reparent_apply_lock: Arc::new(Mutex::new(())),
            queue_store: None,
            context_monitor: ContextMonitorConfig::default(),
            delivery_runtime: None,
            codex_fork_handoff_monitors: Arc::new(Mutex::new(BTreeSet::new())),
            claude_handoff_workers: Arc::new(Mutex::new(BTreeSet::new())),
            credential_rotation_workers: Arc::new(Mutex::new(BTreeSet::new())),
            seat_session_appends: Arc::new(Mutex::new(BTreeSet::new())),
            clear_operation_locks: Arc::new(Mutex::new(BTreeMap::new())),
            seat_session_store,
            usage_identity_store: None,
            usage_burn_store: None,
            usage_ledger_store: None,
            usage_report_store: None,
            usage_project_keys: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    #[cfg(test)]
    fn with_claude_projects_root(mut self, projects_root: PathBuf) -> Self {
        self.claude_projects_roots = vec![projects_root];
        self
    }

    pub fn list_sessions(&self, include_stopped: bool) -> Result<Vec<SessionRecord>> {
        let snapshot = self.load_snapshot()?;
        Ok(snapshot
            .into_sessions()
            .into_iter()
            .filter(|session| include_stopped || !session.is_stopped())
            .collect())
    }

    pub fn list_children(
        &self,
        parent_session_id: &str,
        recursive: bool,
        status_filter: Option<&str>,
        include_terminated: bool,
    ) -> Result<Vec<ChildSessionResponse>> {
        Ok(self
            .list_child_records(
                parent_session_id,
                recursive,
                status_filter,
                include_terminated,
            )?
            .into_iter()
            .map(ChildSessionResponse::from)
            .collect())
    }

    pub fn list_child_records(
        &self,
        parent_session_id: &str,
        recursive: bool,
        status_filter: Option<&str>,
        include_terminated: bool,
    ) -> Result<Vec<SessionRecord>> {
        let all_sessions = self.load_snapshot()?.into_sessions();
        let mut children = if recursive {
            let mut descendants = Vec::new();
            let mut visited = BTreeSet::new();
            collect_descendants_preorder(
                &all_sessions,
                parent_session_id,
                &mut visited,
                &mut descendants,
            );
            descendants
        } else {
            direct_children(&all_sessions, parent_session_id)
        };

        let status_filter = status_filter
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "all");
        if let Some(status_filter) = status_filter {
            children.retain(|session| match status_filter {
                "running" => {
                    !session.is_stopped() && normalized_status(&session.status) == "running"
                }
                "completed" => session.completion_status.as_deref() == Some("completed"),
                "error" => session.completion_status.as_deref() == Some("error"),
                _ => true,
            });
        }
        if !include_terminated {
            children.retain(|session| {
                !completion_status_is_retired(session.completion_status.as_deref())
            });
        }

        Ok(children)
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Ok(None);
        }
        let sessions = self.load_snapshot()?.into_sessions();
        if let Some(session) = sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
        {
            return Ok(Some(session));
        }
        if let Some(session) = sessions
            .iter()
            .find(|session| session.aliases.iter().any(|alias| alias == session_id))
            .cloned()
        {
            return Ok(Some(session));
        }
        Ok(sessions.into_iter().find(|session| {
            session.cached_display_name().as_deref() == Some(session_id)
                || session.friendly_name.as_deref() == Some(session_id)
                || session.native_title.as_deref() == Some(session_id)
                || session.name == session_id
        }))
    }

    pub fn get_context_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<ContextSnapshotResponse>> {
        let Some(session) = self.get_session(session_id)? else {
            return Ok(None);
        };
        Ok(Some(ContextSnapshotResponse::from_session(
            session,
            &self.context_monitor,
        )))
    }

    /// `hooks/notify_server.sh` dispatches every lifecycle hook through its own
    /// detached curl, so neither HTTP arrival order nor handler completion order
    /// matches the order the events actually happened in. Two independent guards
    /// keep an older `Stop` from overwriting a newer turn-start:
    ///
    /// - `emitted_at` — stamped in the hook script before it detaches, and
    ///   compared against the turn-start's own emission stamp. This is the
    ///   authoritative ordering, and it catches a `Stop` whose curl was delayed
    ///   past the next turn's `UserPromptSubmit` entirely.
    /// - `received_at` — stamped on arrival, before the handler's transcript
    ///   retry sleep. This catches a `Stop` that arrived first but applied late,
    ///   and covers hooks emitted by a script too old to send `emitted_at`.
    pub fn apply_claude_stop_hook(
        &self,
        session_id: &str,
        last_message: Option<&str>,
        native_title: Option<&str>,
        native_title_mtime_ns: Option<i64>,
        transcript_path: Option<&str>,
        received_at: Option<&str>,
        emitted_at: Option<&str>,
    ) -> Result<bool> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Ok(false);
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(false);
        };
        let provider = json_text(session.get("provider")).unwrap_or_else(|| "claude".to_owned());
        let mut seat_session_appends = Vec::new();
        if provider == "claude" {
            let previous_transcript_path = json_text(session.get("transcript_path"));
            let previous_provider_resume_id = previous_transcript_path
                .as_deref()
                .and_then(provider_resume_id_from_transcript_path)
                .or_else(|| json_text(session.get("provider_resume_id")));
            if let Some(previous_provider_resume_id) = previous_provider_resume_id {
                seat_session_appends.push((previous_provider_resume_id, previous_transcript_path));
            }
        }
        let provider_resume_id = if provider == "claude" {
            transcript_path.and_then(provider_resume_id_from_transcript_path)
        } else {
            None
        };
        if let Some(provider_resume_id) = provider_resume_id.as_deref() {
            seat_session_appends.push((
                provider_resume_id.to_owned(),
                transcript_path.map(ToOwned::to_owned),
            ));
        }
        if raw_session_is_stopped(session) {
            drop(_guard);
            for (provider_resume_id, artifact_path) in seat_session_appends {
                self.append_seat_session(
                    session_id,
                    &provider,
                    &provider_resume_id,
                    artifact_path.as_deref(),
                );
            }
            return Ok(false);
        }

        // A lifecycle hook newer than this one already decided the state. The
        // turn transition is superseded, but the transcript metadata this Stop
        // carries is still the freshest we have, so it is applied either way.
        //
        // Emission stamps come from the node that owns the session, so a turn
        // start and its Stop are always compared on a single clock.
        let superseded_by_emission = match (
            emitted_at,
            json_text(session.get("activity_hook_emitted_at")),
        ) {
            (Some(emitted_at), Some(stored)) => timestamp_is_after(&stored, emitted_at),
            _ => false,
        };
        let superseded_by_arrival = received_at.is_some_and(|received_at| {
            json_text(session.get("activity_hook_at"))
                .is_some_and(|stored| timestamp_is_after(&stored, received_at))
        });
        let superseded = superseded_by_emission || superseded_by_arrival;

        let now = now_rfc3339();
        if !superseded {
            let reserves_handoff = provider == "claude"
                && json_text(session.get("pending_handoff_path")).is_some()
                && json_text(session.get("pending_handoff_recorded_at")).is_some();
            if reserves_handoff {
                session.insert(
                    "claude_handoff_in_progress_at".to_owned(),
                    Value::String(now.clone()),
                );
                // Keep queue delivery from treating the Stop transition as an
                // ordinary idle prompt before the handoff owns the pane.
                session.insert("status".to_owned(), Value::String("running".to_owned()));
            } else {
                session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
                session.insert("status".to_owned(), Value::String("idle".to_owned()));
            }
            session.insert("last_activity".to_owned(), Value::String(now.clone()));
            session.insert("activity_hook_at".to_owned(), Value::String(now.clone()));
            session.insert(
                "activity_hook_emitted_at".to_owned(),
                emitted_at.map_or(Value::Null, |emitted_at| {
                    Value::String(emitted_at.to_owned())
                }),
            );
            session.insert("agent_status_text".to_owned(), Value::Null);
            session.insert("agent_status_at".to_owned(), Value::Null);
        }
        if let Some(last_message) = last_message {
            session.insert(
                "last_action_summary".to_owned(),
                Value::String(last_message.to_owned()),
            );
        }
        if let Some(transcript_path) = transcript_path {
            session.insert(
                "transcript_path".to_owned(),
                Value::String(transcript_path.to_owned()),
            );
            if let Some(provider_resume_id) = provider_resume_id {
                session.insert(
                    "provider_resume_id".to_owned(),
                    Value::String(provider_resume_id),
                );
            }
        }
        if let Some(native_title) = native_title {
            let title_changed =
                json_text(session.get("native_title")).as_deref() != Some(native_title);
            session.insert(
                "native_title".to_owned(),
                Value::String(native_title.to_owned()),
            );
            if let Some(native_title_mtime_ns) = native_title_mtime_ns {
                session.insert(
                    "native_title_source_mtime_ns".to_owned(),
                    json!(native_title_mtime_ns),
                );
                if title_changed {
                    session.insert(
                        "native_title_updated_at_ns".to_owned(),
                        json!(native_title_mtime_ns),
                    );
                }
            } else if title_changed {
                session.insert(
                    "native_title_updated_at_ns".to_owned(),
                    json!(now_unix_timestamp_nanos()),
                );
            }
        }
        self.write_raw_json_value(&state)?;
        drop(_guard);
        for (provider_resume_id, artifact_path) in seat_session_appends {
            self.append_seat_session(
                session_id,
                &provider,
                &provider_resume_id,
                artifact_path.as_deref(),
            );
        }
        Ok(!superseded)
    }

    pub fn apply_claude_pre_tool_use_hook(
        &self,
        session_id: &str,
        tool_name: Option<&str>,
    ) -> Result<bool> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Ok(false);
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(false);
        };
        if raw_session_is_stopped(session) {
            return Ok(false);
        }

        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("running".to_owned()));
        session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
        session.insert("last_activity".to_owned(), Value::String(now.clone()));
        session.insert("activity_hook_at".to_owned(), Value::String(now.clone()));
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
        if let Some(tool_name) = tool_name {
            session.insert(
                "last_tool_name".to_owned(),
                Value::String(tool_name.to_owned()),
            );
            session.insert("last_tool_call".to_owned(), Value::String(now));
        }
        self.write_raw_json_value(&state)?;
        Ok(true)
    }

    /// `UserPromptSubmit` brackets the start of a turn. Together with the `Stop`
    /// hook it makes active/idle a pure function of hook signals, so the pane
    /// scraper never has to guess from spinners or completion verbs.
    ///
    /// `emitted_at` orders this against the newest lifecycle signal already
    /// stored, mirroring the `Stop` path. Each hook rides its own detached curl,
    /// so a turn-start delayed past its own turn's `Stop` would otherwise
    /// resurrect a finished turn as active. Only emission order is checked here:
    /// this handler applies on arrival with no intervening sleep, so an
    /// arrival-time comparison could never fire.
    pub fn apply_claude_user_prompt_submit_hook(
        &self,
        session_id: &str,
        emitted_at: Option<&str>,
    ) -> Result<bool> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Ok(false);
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(false);
        };
        if raw_session_is_stopped(session) {
            return Ok(false);
        }

        // A newer lifecycle signal already decided the state; this turn-start
        // belongs to a turn that has since finished.
        let superseded = match (
            emitted_at,
            json_text(session.get("activity_hook_emitted_at")),
        ) {
            (Some(emitted_at), Some(stored)) => timestamp_is_after(&stored, emitted_at),
            _ => false,
        };
        if superseded {
            return Ok(false);
        }

        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("running".to_owned()));
        session.insert("last_activity".to_owned(), Value::String(now.clone()));
        session.insert("activity_hook_at".to_owned(), Value::String(now.clone()));
        // Records that turn-start hooks are actually wired for this session. Only
        // then is a stored idle strong enough to suppress the pane fallback.
        session.insert("activity_turn_start_hook_at".to_owned(), Value::String(now));
        // The stamp the hook script took before detaching. A Stop carrying an
        // older emission stamp is recognised as belonging to the previous turn.
        session.insert(
            "activity_hook_emitted_at".to_owned(),
            emitted_at.map_or(Value::Null, |emitted_at| {
                Value::String(emitted_at.to_owned())
            }),
        );
        session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
        self.write_raw_json_value(&state)?;
        Ok(true)
    }

    pub fn capture_output(&self, session_id: &str, lines: usize) -> Result<Option<String>> {
        let Some(session) = self.get_session(session_id)? else {
            return Ok(None);
        };
        let Some(log_file) = session
            .log_file
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let log_file = expand_home(log_file);
        let output = match read_tail_lines(&log_file, lines) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read session log {}", log_file.display()))
            }
        };
        Ok(Some(output))
    }

    pub fn list_context_monitors(&self) -> Result<Vec<ContextMonitorStatus>> {
        Ok(self
            .load_snapshot()?
            .into_sessions()
            .into_iter()
            .filter(|session| {
                session.context_monitor_enabled
                    && provider_has_measured_context_gauge(&session.provider)
            })
            .map(|session| {
                let friendly_name = session.cached_display_name();
                let thresholds = resolve_context_monitor_thresholds(
                    session.context_monitor_threshold_percentages.clone(),
                    session.context_monitor_warning_percentage,
                    session.context_monitor_critical_percentage,
                    &self.context_monitor,
                );
                ContextMonitorStatus {
                    session_id: session.id,
                    friendly_name,
                    notify_session_id: session.context_monitor_notify,
                    warning_percentage: thresholds
                        .as_ref()
                        .ok()
                        .map(|value| value.warning_percentage),
                    critical_percentage: thresholds
                        .as_ref()
                        .ok()
                        .map(|value| value.critical_percentage),
                    threshold_percentages: thresholds
                        .as_ref()
                        .ok()
                        .map(|value| value.percentages.clone()),
                    threshold_source: thresholds
                        .as_ref()
                        .map(|value| value.source.as_str().to_owned())
                        .unwrap_or_else(|_| "invalid".to_owned()),
                    enforced: thresholds.is_ok(),
                }
            })
            .collect())
    }

    pub fn create_reparent_request(
        &self,
        subject_session_id: &str,
        request: CreateReparentRequest,
        session_credential: &str,
    ) -> Result<ReparentMutationOutcome> {
        let subject_session_id = subject_session_id.trim();
        let target_parent_session_id = request.target_parent_session_id.trim();
        let requester_session_id = request.requester_session_id.trim();
        if subject_session_id.is_empty()
            || target_parent_session_id.is_empty()
            || requester_session_id.is_empty()
        {
            return Ok(ReparentMutationOutcome::BadRequest(
                "subject, target parent, and requester session IDs are required".to_owned(),
            ));
        }
        if subject_session_id == target_parent_session_id {
            return Ok(ReparentMutationOutcome::BadRequest(
                "a session cannot be its own parent".to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if !session_credential_matches(&sessions, requester_session_id, session_credential) {
            return Ok(ReparentMutationOutcome::Forbidden(
                "session credential is missing, stale, or does not match the claimed actor"
                    .to_owned(),
            ));
        }
        let Some(subject) = sessions
            .iter()
            .find(|session| session.id == subject_session_id)
        else {
            return Ok(ReparentMutationOutcome::SessionNotFound(
                subject_session_id.to_owned(),
            ));
        };
        if !subject.is_live_for_registry() {
            return Ok(ReparentMutationOutcome::BadRequest(format!(
                "subject session {subject_session_id} is stopped"
            )));
        }
        let Some(target_parent) = sessions
            .iter()
            .find(|session| session.id == target_parent_session_id)
        else {
            return Ok(ReparentMutationOutcome::SessionNotFound(
                target_parent_session_id.to_owned(),
            ));
        };
        if !target_parent.is_live_for_registry() {
            return Ok(ReparentMutationOutcome::BadRequest(format!(
                "target parent session {target_parent_session_id} is stopped"
            )));
        }
        if !session_supports_reparent_consent(target_parent) {
            return Ok(ReparentMutationOutcome::BadRequest(format!(
                "target parent session {target_parent_session_id} cannot participate in credential-bound consent"
            )));
        }
        if subject.parent_session_id.as_deref() == Some(target_parent_session_id) {
            return Ok(ReparentMutationOutcome::BadRequest(format!(
                "session {subject_session_id} is already a child of {target_parent_session_id}"
            )));
        }

        let expected_parent_session_id = subject.parent_session_id.clone();
        let expected_parent_is_live = expected_parent_session_id.as_deref().is_some_and(|id| {
            sessions
                .iter()
                .find(|session| session.id == id)
                .is_some_and(SessionRecord::is_live_for_registry)
        });
        let requester_is_current_parent = expected_parent_is_live
            && expected_parent_session_id.as_deref() == Some(requester_session_id);
        let requester_is_target_parent = requester_session_id == target_parent_session_id;
        if !requester_is_current_parent && !requester_is_target_parent {
            return Ok(ReparentMutationOutcome::Forbidden(
                "only the live current parent or proposed new parent may request reparenting"
                    .to_owned(),
            ));
        }
        if expected_parent_is_live {
            let expected_parent = sessions
                .iter()
                .find(|session| Some(session.id.as_str()) == expected_parent_session_id.as_deref())
                .expect("live expected parent checked above");
            if !session_supports_reparent_consent(expected_parent) {
                return Ok(ReparentMutationOutcome::BadRequest(format!(
                    "current parent session {} cannot participate in credential-bound consent",
                    expected_parent.id
                )));
            }
        }
        if reparent_would_create_cycle(&sessions, subject_session_id, target_parent_session_id) {
            return Ok(ReparentMutationOutcome::Conflict(
                "reparenting would create a session hierarchy cycle".to_owned(),
            ));
        }

        let now = OffsetDateTime::now_utc();
        let mut records = reparent_request_records(&state)?;
        let _ = refresh_reparent_requests(&mut records, &sessions, &state, now);
        let affected_ids = BTreeSet::from([subject_session_id.to_owned()]);
        if let Some(conflict) = records.iter().find(|record| {
            record.is_active() && !record.affected_session_ids().is_disjoint(&affected_ids)
        }) {
            store_reparent_request_records(&mut state, &records)?;
            self.write_raw_json_value(&state)?;
            return Ok(ReparentMutationOutcome::Conflict(format!(
                "request {} already controls session {}",
                conflict.id, subject_session_id
            )));
        }

        let mut required_agent_approvals = BTreeSet::from([target_parent_session_id.to_owned()]);
        if expected_parent_is_live {
            if let Some(parent_id) = expected_parent_session_id.as_ref() {
                required_agent_approvals.insert(parent_id.clone());
            }
        }
        let required_agent_approvals = required_agent_approvals.into_iter().collect::<Vec<_>>();
        let required_human_approval = !expected_parent_is_live;
        let created_at = now.format(&Rfc3339)?;
        let expires_at =
            (now + TimeDuration::hours(REPARENT_REQUEST_TTL_HOURS)).format(&Rfc3339)?;
        let id = generate_unique_reparent_request_id(&records)?;
        let topology_fingerprint = reparent_topology_fingerprint(
            "single",
            subject_session_id,
            target_parent_session_id,
            expected_parent_session_id.as_deref(),
            None,
            &[],
            false,
            false,
        );
        let approvals = vec![ReparentApprovalRecord {
            actor_kind: "agent".to_owned(),
            actor_id: requester_session_id.to_owned(),
            decision: "approved".to_owned(),
            decided_at: created_at.clone(),
        }];
        let ready_to_apply = approvals_satisfied(
            &required_agent_approvals,
            required_human_approval,
            &approvals,
        );
        let record = ReparentRequestRecord {
            id,
            kind: "single".to_owned(),
            subject_session_id: subject_session_id.to_owned(),
            target_parent_session_id: target_parent_session_id.to_owned(),
            expected_parent_session_id,
            expected_parent_is_live,
            expected_target_parent_session_id: None,
            expected_target_parent_is_live: false,
            stopped_root_authorized_maintainer_session_id: None,
            detach_non_live_parent: false,
            peer_root_succession: false,
            stopped_root_recovery: false,
            frozen_live_child_ids: Vec::new(),
            initiator_session_id: requester_session_id.to_owned(),
            required_agent_approvals,
            required_human_approval,
            approvals,
            status: "pending".to_owned(),
            ready_to_apply,
            created_at,
            expires_at,
            decided_at: None,
            applied_at: None,
            failure_reason: None,
            superseded_by_request_id: None,
            topology_fingerprint,
            apply_stage: None,
            apply_plan: None,
            notification_intents: Vec::new(),
            deferred_routing_intents: Vec::new(),
            repair_history: Vec::new(),
        };
        records.push(record.clone());
        store_reparent_request_records(&mut state, &records)?;
        self.write_raw_json_value(&state)?;
        Ok(ReparentMutationOutcome::Created(record))
    }

    pub fn create_reparent_tree_request(
        &self,
        source_session_id: &str,
        request: CreateReparentTreeRequest,
        session_credential: &str,
    ) -> Result<ReparentMutationOutcome> {
        let source_session_id = source_session_id.trim();
        let target_session_id = request.target_session_id.trim();
        let requester_session_id = request.requester_session_id.trim();
        if source_session_id.is_empty()
            || target_session_id.is_empty()
            || requester_session_id.is_empty()
        {
            return Ok(ReparentMutationOutcome::BadRequest(
                "source, target, and requester session IDs are required".to_owned(),
            ));
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if !session_credential_matches(&sessions, requester_session_id, session_credential) {
            return Ok(ReparentMutationOutcome::Forbidden(
                "session credential is missing, stale, or does not match the claimed actor"
                    .to_owned(),
            ));
        }
        let Some(source) = sessions
            .iter()
            .find(|session| session.id == source_session_id)
        else {
            return Ok(ReparentMutationOutcome::SessionNotFound(
                source_session_id.to_owned(),
            ));
        };
        let Some(target) = sessions
            .iter()
            .find(|session| session.id == target_session_id)
        else {
            return Ok(ReparentMutationOutcome::SessionNotFound(
                target_session_id.to_owned(),
            ));
        };
        let mut blockers = Vec::new();
        let stopped_root_recovery = stopped_root_recovery_source_eligible(source);
        if !source.is_live_for_registry() && !stopped_root_recovery {
            blockers.push(format!("source session {source_session_id} is stopped"));
        }
        if !target.is_live_for_registry() {
            blockers.push(format!("target session {target_session_id} is stopped"));
        }
        if !stopped_root_recovery && !session_supports_reparent_consent(source) {
            blockers.push(format!(
                "source session {source_session_id} cannot participate in credential-bound consent"
            ));
        }
        if !session_supports_reparent_consent(target) {
            blockers.push(format!(
                "target session {target_session_id} cannot participate in credential-bound consent"
            ));
        }
        let target_is_direct_child = target.parent_session_id.as_deref() == Some(source_session_id);
        let peer_root_succession = !stopped_root_recovery
            && source.parent_session_id.is_none()
            && target.parent_session_id.is_none();
        if !stopped_root_recovery && !target_is_direct_child && !peer_root_succession {
            blockers.push(format!(
                "target session {target_session_id} must be a live direct child of {source_session_id}, or both source and target must be roots for peer-root succession"
            ));
        }
        let expected_parent_session_id = source.parent_session_id.clone();
        let expected_target_parent_session_id = target.parent_session_id.clone();
        let expected_parent_is_live = expected_parent_session_id.as_deref().is_some_and(|id| {
            sessions
                .iter()
                .find(|session| session.id == id)
                .is_some_and(SessionRecord::is_live_for_registry)
        });
        if expected_parent_is_live {
            let expected_parent = sessions
                .iter()
                .find(|session| Some(session.id.as_str()) == expected_parent_session_id.as_deref())
                .expect("live expected parent checked above");
            if !session_supports_reparent_consent(expected_parent) {
                blockers.push(format!(
                    "current parent session {} cannot participate in credential-bound consent",
                    expected_parent.id
                ));
            }
        }
        let expected_target_parent = expected_target_parent_session_id
            .as_deref()
            .and_then(|id| sessions.iter().find(|session| session.id == id));
        let expected_target_parent_is_live =
            expected_target_parent.is_some_and(SessionRecord::is_live_for_registry);
        let stopped_root_authorized_maintainer_session_id = if stopped_root_recovery {
            match expected_target_parent {
                Some(target_parent) => {
                    if !target_parent.is_live_for_registry()
                        || !session_supports_reparent_consent(target_parent)
                    {
                        blockers.push(format!(
                            "current target parent session {} cannot participate in credential-bound consent",
                            target_parent.id
                        ));
                    }
                    let maintainer = find_raw_registration(&state, "maintainer")?;
                    match maintainer {
                        Some(maintainer) if maintainer.session_id == target_parent.id => {
                            Some(maintainer.session_id)
                        }
                        Some(_) => {
                            blockers.push(
                                "current target parent is not the durable maintainer".to_owned(),
                            );
                            None
                        }
                        None => {
                            blockers.push(
                                "stopped-root recovery requires a durable maintainer registration"
                                    .to_owned(),
                            );
                            None
                        }
                    }
                }
                None => {
                    blockers.push(
                        "stopped-root recovery requires a live current target parent".to_owned(),
                    );
                    None
                }
            }
        } else {
            None
        };
        if stopped_root_recovery
            && sessions
                .iter()
                .any(|session| session.parent_session_id.as_deref() == Some(target_session_id))
        {
            blockers.push(format!(
                "stopped-root recovery requires target session {target_session_id} to have no existing children"
            ));
        }
        let initiator_allowed = if stopped_root_recovery {
            requester_session_id == target_session_id
        } else {
            requester_session_id == source_session_id
                || requester_session_id == target_session_id
                || (expected_parent_is_live
                    && expected_parent_session_id.as_deref() == Some(requester_session_id))
        };
        if !initiator_allowed {
            return Ok(ReparentMutationOutcome::Forbidden(
                "only the source, target, or live source parent may request tree promotion"
                    .to_owned(),
            ));
        }
        let frozen_live_child_ids = sessions
            .iter()
            .filter(|session| {
                session.is_live_for_registry()
                    && session.parent_session_id.as_deref() == Some(source_session_id)
            })
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        let target_parent_session_id = expected_parent_session_id
            .as_deref()
            .filter(|_| expected_parent_is_live);
        let edge_changes = tree_reparent_edge_changes(
            source_session_id,
            target_session_id,
            expected_parent_session_id.as_deref(),
            target_parent_session_id,
            expected_target_parent_session_id.as_deref(),
            &frozen_live_child_ids,
            peer_root_succession,
            stopped_root_recovery,
        );
        if reparent_plan_would_create_cycle(&sessions, &edge_changes) {
            blockers.push("tree promotion would create a session hierarchy cycle".to_owned());
        }
        let (json_routing_changes, queue_routing_changes) =
            self.build_reparent_routing_changes(&state, &edge_changes)?;
        let mut required_agent_approvals = BTreeSet::from([target_session_id.to_owned()]);
        if !stopped_root_recovery {
            required_agent_approvals.insert(source_session_id.to_owned());
        }
        if expected_parent_is_live {
            if let Some(parent) = expected_parent_session_id.as_ref() {
                required_agent_approvals.insert(parent.clone());
            }
        }
        if stopped_root_recovery {
            if let Some(parent) = expected_target_parent_session_id.as_ref() {
                required_agent_approvals.insert(parent.clone());
            }
        }
        let required_agent_approvals = required_agent_approvals.into_iter().collect::<Vec<_>>();
        let required_human_approval = false;
        let preview = ReparentTreePreview {
            kind: "tree".to_owned(),
            source_session_id: source_session_id.to_owned(),
            target_session_id: target_session_id.to_owned(),
            peer_root_succession,
            stopped_root_recovery,
            frozen_live_child_ids: frozen_live_child_ids.clone(),
            edge_changes,
            json_routing_changes,
            queue_routing_changes,
            required_agent_approvals: required_agent_approvals.clone(),
            required_human_approval,
            blockers: blockers.clone(),
        };
        let now = OffsetDateTime::now_utc();
        let mut records = reparent_request_records(&state)?;
        let refreshed = refresh_reparent_requests(&mut records, &sessions, &state, now);
        let affected_ids = preview
            .edge_changes
            .iter()
            .map(|change| change.session_id.clone())
            .chain(expected_parent_session_id.iter().cloned())
            .chain(expected_target_parent_session_id.iter().cloned())
            .collect::<BTreeSet<_>>();
        let active_conflict = records.iter().find(|record| {
            record.is_active() && !record.affected_session_ids().is_disjoint(&affected_ids)
        });
        if let Some(blocker) = blockers.first() {
            if refreshed {
                store_reparent_request_records(&mut state, &records)?;
                self.write_raw_json_value(&state)?;
            }
            return Ok(ReparentMutationOutcome::BadRequest(blocker.clone()));
        }
        if let Some(conflict) = active_conflict {
            let overlapping_session = if affected_ids.contains(&conflict.subject_session_id) {
                conflict.subject_session_id.clone()
            } else {
                conflict
                    .affected_session_ids()
                    .intersection(&affected_ids)
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_owned())
            };
            store_reparent_request_records(&mut state, &records)?;
            self.write_raw_json_value(&state)?;
            return Ok(ReparentMutationOutcome::Conflict(format!(
                "request {} already controls session {} in the tree promotion",
                conflict.id, overlapping_session
            )));
        }
        if request.dry_run {
            if refreshed {
                store_reparent_request_records(&mut state, &records)?;
                self.write_raw_json_value(&state)?;
            }
            return Ok(ReparentMutationOutcome::Preview(preview));
        }
        let created_at = now.format(&Rfc3339)?;
        let expires_at =
            (now + TimeDuration::hours(REPARENT_REQUEST_TTL_HOURS)).format(&Rfc3339)?;
        let approvals = vec![ReparentApprovalRecord {
            actor_kind: "agent".to_owned(),
            actor_id: requester_session_id.to_owned(),
            decision: "approved".to_owned(),
            decided_at: created_at.clone(),
        }];
        let record = ReparentRequestRecord {
            id: generate_unique_reparent_request_id(&records)?,
            kind: "tree".to_owned(),
            subject_session_id: source_session_id.to_owned(),
            target_parent_session_id: target_session_id.to_owned(),
            expected_parent_session_id,
            expected_parent_is_live,
            expected_target_parent_session_id,
            expected_target_parent_is_live,
            stopped_root_authorized_maintainer_session_id,
            detach_non_live_parent: true,
            stopped_root_recovery,
            frozen_live_child_ids,
            initiator_session_id: requester_session_id.to_owned(),
            required_agent_approvals: required_agent_approvals.clone(),
            required_human_approval,
            ready_to_apply: approvals_satisfied(
                &required_agent_approvals,
                required_human_approval,
                &approvals,
            ),
            approvals,
            status: "pending".to_owned(),
            created_at,
            expires_at,
            decided_at: None,
            applied_at: None,
            failure_reason: None,
            superseded_by_request_id: None,
            topology_fingerprint: reparent_topology_fingerprint(
                "tree",
                source_session_id,
                target_session_id,
                source.parent_session_id.as_deref(),
                target.parent_session_id.as_deref(),
                &preview.frozen_live_child_ids,
                peer_root_succession,
                stopped_root_recovery,
            ),
            peer_root_succession,
            apply_stage: None,
            apply_plan: None,
            notification_intents: Vec::new(),
            deferred_routing_intents: Vec::new(),
            repair_history: Vec::new(),
        };
        records.push(record.clone());
        store_reparent_request_records(&mut state, &records)?;
        self.write_raw_json_value(&state)?;
        Ok(ReparentMutationOutcome::Created(record))
    }

    pub fn decide_reparent_request(
        &self,
        request_id: &str,
        request: DecideReparentRequest,
        decision: ReparentDecision,
        session_credential: &str,
    ) -> Result<ReparentMutationOutcome> {
        let request_id = request_id.trim();
        let requester_session_id = request.requester_session_id.trim();
        if request_id.is_empty() || requester_session_id.is_empty() {
            return Ok(ReparentMutationOutcome::BadRequest(
                "request ID and requester session ID are required".to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if !session_credential_matches(&sessions, requester_session_id, session_credential) {
            return Ok(ReparentMutationOutcome::Forbidden(
                "session credential is missing, stale, or does not match the claimed actor"
                    .to_owned(),
            ));
        }
        let mut records = reparent_request_records(&state)?;
        let now = OffsetDateTime::now_utc();
        let refreshed = refresh_reparent_requests(&mut records, &sessions, &state, now);
        let Some(index) = records.iter().position(|record| record.id == request_id) else {
            if refreshed {
                store_reparent_request_records(&mut state, &records)?;
                self.write_raw_json_value(&state)?;
            }
            return Ok(ReparentMutationOutcome::RequestNotFound);
        };
        let record = &mut records[index];
        if !record
            .required_agent_approvals
            .iter()
            .any(|actor| actor == requester_session_id)
        {
            return Ok(ReparentMutationOutcome::Forbidden(format!(
                "session {requester_session_id} is not a required approver for request {request_id}"
            )));
        }
        if let Some(existing) = record.approvals.iter().find(|approval| {
            approval.actor_kind == "agent" && approval.actor_id == requester_session_id
        }) {
            if existing.decision == decision.as_str() {
                let updated = record.clone();
                if refreshed {
                    store_reparent_request_records(&mut state, &records)?;
                    self.write_raw_json_value(&state)?;
                }
                return Ok(ReparentMutationOutcome::Updated(updated));
            }
            return Ok(ReparentMutationOutcome::Conflict(format!(
                "session {requester_session_id} already {} request {request_id}",
                existing.decision
            )));
        }
        if record.status == "expired" {
            store_reparent_request_records(&mut state, &records)?;
            self.write_raw_json_value(&state)?;
            return Ok(ReparentMutationOutcome::Expired);
        }
        if record.status != "pending" {
            let detail = format!(
                "request {} is already {}{}",
                record.id,
                record.status,
                record
                    .failure_reason
                    .as_deref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            );
            store_reparent_request_records(&mut state, &records)?;
            self.write_raw_json_value(&state)?;
            return Ok(ReparentMutationOutcome::Conflict(detail));
        }
        let decided_at = now.format(&Rfc3339)?;
        record.approvals.push(ReparentApprovalRecord {
            actor_kind: "agent".to_owned(),
            actor_id: requester_session_id.to_owned(),
            decision: decision.as_str().to_owned(),
            decided_at: decided_at.clone(),
        });
        if decision == ReparentDecision::Rejected {
            record.status = "rejected".to_owned();
            record.decided_at = Some(decided_at);
            record.ready_to_apply = false;
        } else {
            record.ready_to_apply = approvals_satisfied(
                &record.required_agent_approvals,
                record.required_human_approval,
                &record.approvals,
            );
        }
        let updated = record.clone();
        store_reparent_request_records(&mut state, &records)?;
        self.write_raw_json_value(&state)?;
        drop(_guard);
        if decision == ReparentDecision::Approved && updated.ready_to_apply {
            self.reconcile_reparent_requests()?;
            if let Some(applied) = self.get_reparent_request(&updated.id)? {
                return Ok(ReparentMutationOutcome::Updated(applied));
            }
        }
        Ok(ReparentMutationOutcome::Updated(updated))
    }

    pub fn decide_reparent_request_as_human(
        &self,
        request_id: &str,
        actor_id: &str,
        decision: ReparentDecision,
    ) -> Result<ReparentMutationOutcome> {
        let request_id = request_id.trim();
        let actor_id = actor_id.trim();
        if request_id.is_empty() || actor_id.is_empty() {
            return Ok(ReparentMutationOutcome::BadRequest(
                "request ID and authenticated human actor are required".to_owned(),
            ));
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let mut records = reparent_request_records(&state)?;
        let now = OffsetDateTime::now_utc();
        let refreshed = refresh_reparent_requests(&mut records, &sessions, &state, now);
        let Some(index) = records.iter().position(|record| record.id == request_id) else {
            if refreshed {
                store_reparent_request_records(&mut state, &records)?;
                self.write_raw_json_value(&state)?;
            }
            return Ok(ReparentMutationOutcome::RequestNotFound);
        };
        let record = &mut records[index];
        if !record.required_human_approval {
            return Ok(ReparentMutationOutcome::Forbidden(format!(
                "request {request_id} does not require human approval"
            )));
        }
        if let Some(existing) = record
            .approvals
            .iter()
            .find(|approval| approval.actor_kind == "human")
        {
            if existing.actor_id == actor_id && existing.decision == decision.as_str() {
                let updated = record.clone();
                if refreshed {
                    store_reparent_request_records(&mut state, &records)?;
                    self.write_raw_json_value(&state)?;
                }
                return Ok(ReparentMutationOutcome::Updated(updated));
            }
            return Ok(ReparentMutationOutcome::Conflict(format!(
                "human actor {} already {} request {request_id}",
                existing.actor_id, existing.decision
            )));
        }
        if record.status == "expired" {
            store_reparent_request_records(&mut state, &records)?;
            self.write_raw_json_value(&state)?;
            return Ok(ReparentMutationOutcome::Expired);
        }
        if record.status != "pending" {
            return Ok(ReparentMutationOutcome::Conflict(format!(
                "request {} is already {}",
                record.id, record.status
            )));
        }
        let decided_at = now.format(&Rfc3339)?;
        record.approvals.push(ReparentApprovalRecord {
            actor_kind: "human".to_owned(),
            actor_id: actor_id.to_owned(),
            decision: decision.as_str().to_owned(),
            decided_at: decided_at.clone(),
        });
        if decision == ReparentDecision::Rejected {
            record.status = "rejected".to_owned();
            record.decided_at = Some(decided_at);
            record.ready_to_apply = false;
        } else {
            record.ready_to_apply = approvals_satisfied(
                &record.required_agent_approvals,
                record.required_human_approval,
                &record.approvals,
            );
        }
        let updated = record.clone();
        store_reparent_request_records(&mut state, &records)?;
        self.write_raw_json_value(&state)?;
        drop(_guard);
        if decision == ReparentDecision::Approved && updated.ready_to_apply {
            self.reconcile_reparent_requests()?;
            if let Some(applied) = self.get_reparent_request(&updated.id)? {
                return Ok(ReparentMutationOutcome::Updated(applied));
            }
        }
        Ok(ReparentMutationOutcome::Updated(updated))
    }

    pub fn get_reparent_request(&self, request_id: &str) -> Result<Option<ReparentRequestRecord>> {
        // Poll routes call this method.  The registry is atomically replaced,
        // so a direct snapshot read sees either the old or new complete state
        // without waiting for a writer.  Lifecycle transitions belong to the
        // reparent worker, not to a GET request.
        let state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let mut records = reparent_request_records(&state)?;
        // This projection is intentionally not persisted.  It keeps the
        // response truthful at an expiry boundary while the lifecycle worker
        // performs the durable transition and outbox derivation separately.
        refresh_reparent_requests(&mut records, &sessions, &state, OffsetDateTime::now_utc());
        Ok(records
            .into_iter()
            .find(|record| record.id == request_id.trim()))
    }

    pub fn list_reparent_requests(&self) -> Result<Vec<ReparentRequestRecord>> {
        // Keep collection reads in the same lock-free snapshot class as
        // `list_sessions`.  In particular, do not refresh expiry/staleness
        // here: doing so turns a watch poll into a durable mutation.
        let state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let mut records = reparent_request_records(&state)?;
        refresh_reparent_requests(&mut records, &sessions, &state, OffsetDateTime::now_utc());
        records.sort_by(|left, right| {
            (&left.created_at, &left.id).cmp(&(&right.created_at, &right.id))
        });
        Ok(records)
    }

    pub fn list_reparent_requests_for_session(
        &self,
        session_id: &str,
        session_credential: &str,
    ) -> Result<Option<Vec<ReparentRequestRecord>>> {
        let session_id = session_id.trim();
        let state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if !session_credential_matches(&sessions, session_id, session_credential) {
            return Ok(None);
        }
        let mut records = reparent_request_records(&state)?;
        refresh_reparent_requests(&mut records, &sessions, &state, OffsetDateTime::now_utc());
        records.retain(|record| record.involves_session(session_id));
        records.sort_by(|left, right| {
            (&left.created_at, &left.id).cmp(&(&right.created_at, &right.id))
        });
        Ok(Some(records))
    }

    pub fn send_parent_notification(
        &self,
        child_session_id: &str,
        text: &str,
        delivery_mode: &str,
        message_category: &str,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if let Some(request_id) =
            active_reparent_route_request_for_session(&state, child_session_id)?
        {
            persist_deferred_parent_message_intent(
                &mut state,
                &request_id,
                &format!("{message_category}:{child_session_id}:{}", now_rfc3339()),
                child_session_id,
                text,
                delivery_mode,
                message_category,
            )?;
            return self.write_raw_json_value(&state);
        }
        let Some(parent_session_id) = raw_session_object(&state, child_session_id)
            .and_then(|session| json_text(session.get("parent_session_id")))
        else {
            return Ok(());
        };
        self.queue_parent_message(
            &mut state,
            child_session_id,
            &parent_session_id,
            text,
            delivery_mode,
            message_category,
            runtime,
        )?;
        self.write_raw_json_value(&state)
    }

    pub fn reconcile_reparent_requests(&self) -> Result<Option<ReparentRequestRecord>> {
        // `reparent_apply_lock` only protects HTTP calls served by this
        // SessionStore.  Startup recovery and an overlapping server process
        // construct distinct stores, so retain an advisory lock for the full
        // JSON + queue transaction as well.  flock is released on crash,
        // which leaves the persisted lease available to the next recovery
        // driver rather than stranding it.
        let _cross_process_guard = self.reparent_apply_file_lock()?;
        let _apply_guard = self
            .reparent_apply_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("reparent apply lock poisoned"))?;
        self.reconcile_reparent_requests_locked()
    }

    fn reconcile_reparent_requests_locked(&self) -> Result<Option<ReparentRequestRecord>> {
        let mut first = None;
        loop {
            let request_id = match self.acquire_reparent_apply_lease()? {
                Some(request_id) => request_id,
                None => return Ok(first),
            };
            if self
                .get_reparent_request(&request_id)?
                .is_some_and(|record| {
                    record.status == "failed"
                        && record.apply_stage.as_deref() != Some("prequiesce_aborting")
                })
            {
                return Ok(first.or(self.get_reparent_request(&request_id)?));
            }
            if let Err(error) = self.apply_reparent_request(&request_id) {
                self.fail_reparent_apply(&request_id, &error.to_string())?;
            }
            let current = self.get_reparent_request(&request_id)?;
            if first.is_none() {
                first = current.clone();
            }
            if current
                .as_ref()
                .is_some_and(|record| record.status == "failed")
            {
                return Ok(first);
            }
        }
    }

    pub fn reconcile_reparent_notifications(&self) -> Result<()> {
        // A notification is an outbox projection of the committed request,
        // not an observation made by a losing apply driver.  Serialize the
        // projection with apply/recovery, then enqueue only intents that are
        // still desired by the durable record while this lock is held.
        let _cross_process_guard = self.reparent_apply_file_lock()?;
        let pending = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let sessions = snapshot_from_raw_value(&state)?.into_sessions();
            let mut records = reparent_request_records(&state)?;
            let refreshed = refresh_reparent_requests(
                &mut records,
                &sessions,
                &state,
                OffsetDateTime::now_utc(),
            );
            let mut changed = refreshed;
            changed |= reconcile_reparent_notification_intents(&mut records);
            if changed {
                store_reparent_request_records(&mut state, &records)?;
                self.write_raw_json_value(&state)?;
            }
            let pending = records
                .iter()
                .flat_map(|record| {
                    desired_reparent_notifications(record)
                        .into_iter()
                        .filter(|desired| {
                            record.notification_intents.iter().any(|intent| {
                                intent.key == desired.intent.key && intent.enqueued_at.is_none()
                            })
                        })
                })
                .collect::<Vec<_>>();
            pending
        };
        for desired in pending {
            let desired = {
                let _guard = self.write_guard()?;
                let state = self.load_raw_json_value()?;
                let records = reparent_request_records(&state)?;
                let Some(record) = records
                    .iter()
                    .find(|record| record.id == desired.request_id)
                else {
                    continue;
                };
                let Some(current) = desired_reparent_notifications(record)
                    .into_iter()
                    .find(|current| current.intent.key == desired.intent.key)
                else {
                    // The request reached a different durable outcome after
                    // this reconciliation pass selected its work.  Never
                    // enqueue a stale terminal projection.
                    continue;
                };
                current
            };
            let queue = self
                .queue_store
                .as_ref()
                .context("reparent notifications require the retained queue store")?;
            queue.enqueue_message_once_with_metadata(
                &desired.intent.key,
                &desired.intent.recipient_session_id,
                &desired.text,
                "important",
                QueueMessageMetadata {
                    message_category: Some("reparent".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )?;
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let mut records = reparent_request_records(&state)?;
            let record = records
                .iter_mut()
                .find(|record| record.id == desired.request_id)
                .with_context(|| format!("reparent request {} disappeared", desired.request_id))?;
            let intent = record
                .notification_intents
                .iter_mut()
                .find(|intent| intent.key == desired.intent.key)
                .with_context(|| {
                    format!("reparent notification {} disappeared", desired.intent.key)
                })?;
            if intent.enqueued_at.is_none() {
                intent.enqueued_at = Some(now_rfc3339());
                store_reparent_request_records(&mut state, &records)?;
                self.write_raw_json_value(&state)?;
            }
        }
        // Queue projection is complete. Runtime delivery is deliberately
        // outside the cross-process apply lock so a hung tmux cannot block
        // state transitions or make another process wait on this lock.
        drop(_cross_process_guard);
        if let Some(runtime) = self.delivery_runtime.as_ref() {
            // Retry every recipient with an outstanding reparent queue row,
            // not merely recipients selected for a fresh enqueue above.  A
            // failed runtime delivery leaves its durable queue row pending and
            // must be retried by the next background reconciliation.
            let recipients = self
                .queue_store
                .as_ref()
                .context("reparent notifications require the retained queue store")?
                .pending_target_session_ids_by_category("reparent")?;
            let mut first_error = None;
            for recipient in recipients {
                if let Err(error) = self.drain_runtime_pending_messages_for_session_category(
                    &recipient,
                    runtime,
                    Some("reparent"),
                ) {
                    if first_error.is_none() {
                        first_error = Some(error.context(format!(
                            "failed to deliver reparent notifications to {recipient}"
                        )));
                    }
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn repair_reparent_request(
        &self,
        request_id: &str,
        actor_id: &str,
        action: ReparentRepairAction,
    ) -> Result<ReparentMutationOutcome> {
        let request_id = request_id.trim();
        let actor_id = actor_id.trim();
        if request_id.is_empty() || actor_id.is_empty() {
            return Ok(ReparentMutationOutcome::BadRequest(
                "request ID and authenticated human actor are required".to_owned(),
            ));
        }
        let attempted_at = now_rfc3339();
        let repaired = {
            let _cross_process_guard = self.reparent_apply_file_lock()?;
            {
                let _guard = self.write_guard()?;
                let mut state = self.load_raw_json_value()?;
                let mut records = reparent_request_records(&state)?;
                let Some(record) = records.iter_mut().find(|record| record.id == request_id) else {
                    return Ok(ReparentMutationOutcome::RequestNotFound);
                };
                let lease = reparent_apply_lease(&state)?;
                if record.status != "failed"
                    || lease.as_ref().map(|lease| lease.request_id.as_str()) != Some(request_id)
                {
                    return Ok(ReparentMutationOutcome::Conflict(format!(
                        "request {request_id} is not a quarantined apply transaction"
                    )));
                }
                let stage = record.apply_stage.as_deref().unwrap_or("applying");
                let allowed = match action {
                    ReparentRepairAction::Resume => matches!(
                        stage,
                        "prequiesce_aborting"
                            | "json_routing_quiesced"
                            | "routing_quiesced"
                            | "authority_committed"
                    ),
                    ReparentRepairAction::RollbackPrecommit => {
                        matches!(stage, "json_routing_quiesced" | "routing_quiesced")
                    }
                };
                if !allowed {
                    return Ok(ReparentMutationOutcome::Conflict(format!(
                        "repair action {} is not allowed from stage {stage}",
                        action.as_str()
                    )));
                }
                record.repair_history.push(ReparentRepairRecord {
                    actor_kind: "human".to_owned(),
                    actor_id: actor_id.to_owned(),
                    action: action.as_str().to_owned(),
                    prior_failure: record.failure_reason.clone(),
                    attempted_at: attempted_at.clone(),
                    verified_state_fingerprint: None,
                });
                if action == ReparentRepairAction::Resume {
                    record.status = "applying".to_owned();
                    record.failure_reason = None;
                }
                store_reparent_request_records(&mut state, &records)?;
                self.write_raw_json_value(&state)?;
            }
            let _apply_guard = self
                .reparent_apply_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("reparent apply lock poisoned"))?;
            match action {
                ReparentRepairAction::Resume => {
                    if let Err(error) = self.apply_reparent_request(request_id) {
                        self.fail_reparent_apply(request_id, &error.to_string())?;
                    }
                }
                ReparentRepairAction::RollbackPrecommit => {
                    if let Err(error) = self.rollback_reparent_precommit(request_id, None) {
                        self.fail_reparent_apply(request_id, &error.to_string())?;
                    }
                }
            }
            self.get_reparent_request(request_id)?
                .filter(|record| matches!(record.status.as_str(), "applied" | "repaired"))
        };
        if repaired.is_some() {
            self.verify_reparent_repair(request_id, &attempted_at)?;
            self.reconcile_reparent_requests()?;
        }
        Ok(self
            .get_reparent_request(request_id)?
            .map(ReparentMutationOutcome::Updated)
            .unwrap_or(ReparentMutationOutcome::RequestNotFound))
    }

    fn verify_reparent_repair(&self, request_id: &str, attempted_at: &str) -> Result<()> {
        let (record, state_projection) = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let record = reparent_request_records(&state)?
                .into_iter()
                .find(|record| record.id == request_id)
                .with_context(|| format!("reparent request {request_id} disappeared"))?;
            let sessions = record
                .affected_session_ids()
                .into_iter()
                .map(|session_id| {
                    let session = raw_session_object(&state, &session_id);
                    json!({
                        "id": session_id,
                        "parent_session_id": session.and_then(|value| value.get("parent_session_id")).cloned(),
                        "context_monitor_enabled": session.and_then(|value| value.get("context_monitor_enabled")).cloned(),
                        "context_monitor_notify": session.and_then(|value| value.get("context_monitor_notify")).cloned(),
                        "context_monitor_notify_source": session.and_then(|value| value.get("context_monitor_notify_source")).cloned(),
                    })
                })
                .collect::<Vec<_>>();
            let projection = json!({
                "sessions": sessions,
                "retained_parent_wake_registrations": state.get("retained_parent_wake_registrations"),
                "retained_pending_messages": state.get("retained_pending_messages"),
                "request_status": record.status,
                "apply_stage": record.apply_stage,
            });
            (record, projection)
        };
        let queue_projection = if let (Some(queue), Some(plan)) =
            (self.queue_store.as_ref(), record.apply_plan.as_ref())
        {
            let mut projections = Vec::new();
            for change in &plan.edge_changes {
                let parent = if record.status == "repaired" {
                    change.expected_parent_session_id.as_deref()
                } else {
                    change.new_parent_session_id.as_deref()
                };
                projections.push(json!({
                    "session_id": change.session_id,
                    "parent_session_id": parent,
                    "routing": match parent {
                        Some(parent) => queue.snapshot_parent_routing(&change.session_id, parent)?,
                        None => ParentRoutingSnapshot::default(),
                    },
                }));
            }
            Value::Array(projections)
        } else {
            Value::Null
        };
        let fingerprint = {
            let canonical = json!({
                "state": state_projection,
                "queue": queue_projection,
            });
            let digest = Sha256::digest(serde_json::to_vec(&canonical)?);
            digest.iter().map(|byte| format!("{byte:02x}")).collect()
        };
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut records = reparent_request_records(&state)?;
        let record = records
            .iter_mut()
            .find(|record| record.id == request_id)
            .with_context(|| format!("reparent request {request_id} disappeared"))?;
        let repair = record
            .repair_history
            .iter_mut()
            .find(|repair| repair.attempted_at == attempted_at)
            .with_context(|| format!("repair attempt {attempted_at} disappeared"))?;
        repair.verified_state_fingerprint = Some(fingerprint);
        store_reparent_request_records(&mut state, &records)?;
        self.write_raw_json_value(&state)
    }

    fn acquire_reparent_apply_lease(&self) -> Result<Option<String>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if let Some(lease) = reparent_apply_lease(&state)? {
            return Ok(Some(lease.request_id));
        }
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let mut records = reparent_request_records(&state)?;
        let refreshed =
            refresh_reparent_requests(&mut records, &sessions, &state, OffsetDateTime::now_utc());
        let Some(index) = records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                matches!(record.kind.as_str(), "single" | "tree")
                    && record.status == "pending"
                    && record.ready_to_apply
            })
            .min_by(|(_, left), (_, right)| {
                (&left.created_at, &left.id).cmp(&(&right.created_at, &right.id))
            })
            .map(|(index, _)| index)
        else {
            // The lifecycle worker may inspect an idle registry frequently.
            // An idle pass must not rewrite the complete sessions file.
            if refreshed || state.get("reparent_requests").is_none() {
                store_reparent_request_records(&mut state, &records)?;
                self.write_raw_json_value(&state)?;
            }
            return Ok(None);
        };
        if let Some(reason) = reparent_stale_reason(&records[index], &sessions, &state) {
            records[index].status = "stale".to_owned();
            records[index].ready_to_apply = false;
            records[index].failure_reason = Some(reason);
            records[index].decided_at = Some(now_rfc3339());
            store_reparent_request_records(&mut state, &records)?;
            self.write_raw_json_value(&state)?;
            return Ok(None);
        }
        let plan = self.build_reparent_apply_plan(&state, &records[index])?;
        let request_id = records[index].id.clone();
        let acquired_at = now_rfc3339();
        records[index].status = "applying".to_owned();
        records[index].apply_stage = Some("applying".to_owned());
        records[index].apply_plan = Some(plan);
        records[index].failure_reason = None;
        store_reparent_request_records(&mut state, &records)?;
        store_reparent_apply_lease(
            &mut state,
            Some(&ReparentApplyLease {
                request_id: request_id.clone(),
                acquired_at,
            }),
        )?;
        self.write_raw_json_value(&state)?;
        Ok(Some(request_id))
    }

    fn build_reparent_apply_plan(
        &self,
        state: &Value,
        record: &ReparentRequestRecord,
    ) -> Result<ReparentApplyPlan> {
        let edge_changes = if record.kind == "tree" {
            tree_reparent_edge_changes(
                &record.subject_session_id,
                &record.target_parent_session_id,
                record.expected_parent_session_id.as_deref(),
                tree_target_parent_session_id(record),
                record.expected_target_parent_session_id.as_deref(),
                &record.frozen_live_child_ids,
                record.peer_root_succession,
                record.stopped_root_recovery,
            )
        } else {
            vec![ReparentEdgeChange {
                session_id: record.subject_session_id.clone(),
                expected_parent_session_id: record.expected_parent_session_id.clone(),
                new_parent_session_id: Some(record.target_parent_session_id.clone()),
            }]
        };
        let (json_routing_changes, queue_routing_changes) =
            self.build_reparent_routing_changes(state, &edge_changes)?;
        Ok(ReparentApplyPlan {
            version: 1,
            edge_changes,
            json_routing_changes,
            queue_routing_changes,
        })
    }

    fn build_reparent_routing_changes(
        &self,
        state: &Value,
        edge_changes: &[ReparentEdgeChange],
    ) -> Result<(Vec<ReparentRoutingChange>, Vec<ReparentRoutingChange>)> {
        let mut json_routing_changes = Vec::new();
        let mut queue_routing_changes = Vec::new();
        for edge in edge_changes {
            let Some(old_parent) = edge.expected_parent_session_id.as_deref() else {
                continue;
            };
            for entry in state
                .get("retained_parent_wake_registrations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|entry| {
                    entry.get("child_session_id").and_then(Value::as_str)
                        == Some(edge.session_id.as_str())
                        && json_text(entry.get("parent_session_id")).as_deref() == Some(old_parent)
                        && entry
                            .get("is_active")
                            .and_then(Value::as_bool)
                            .unwrap_or(true)
                })
            {
                let record_id = json_text(entry.get("id")).ok_or_else(|| {
                    anyhow::anyhow!(
                        "parent wake for {} is missing its durable ID",
                        edge.session_id
                    )
                })?;
                let period_seconds = entry
                    .get("period_seconds")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| {
                        anyhow::anyhow!("parent wake {record_id} is missing its period")
                    })?;
                json_routing_changes.push(ReparentRoutingChange {
                    store: "json".to_owned(),
                    record_kind: "parent_wake".to_owned(),
                    record_id,
                    child_session_id: edge.session_id.clone(),
                    expected_target_session_id: Some(old_parent.to_owned()),
                    new_target_session_id: edge.new_parent_session_id.clone(),
                    prior_active: Some(true),
                    period_seconds: Some(period_seconds),
                    creates_parent_wake: false,
                });
            }
            if let Some(session) = raw_session_object(state, &edge.session_id).filter(|session| {
                json_text(session.get("context_monitor_notify")).as_deref() == Some(old_parent)
                    && json_text(session.get("context_monitor_notify_source")).as_deref()
                        == Some("parent_derived")
            }) {
                json_routing_changes.push(ReparentRoutingChange {
                    store: "json".to_owned(),
                    record_kind: "context_monitor".to_owned(),
                    record_id: edge.session_id.clone(),
                    child_session_id: edge.session_id.clone(),
                    expected_target_session_id: Some(old_parent.to_owned()),
                    new_target_session_id: edge.new_parent_session_id.clone(),
                    prior_active: Some(
                        session
                            .get("context_monitor_enabled")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    ),
                    period_seconds: None,
                    creates_parent_wake: false,
                });
            }
            let queue_snapshot = match self.queue_store.as_ref() {
                Some(queue) => queue.snapshot_parent_routing(&edge.session_id, old_parent)?,
                None => ParentRoutingSnapshot::default(),
            };
            queue_routing_changes.extend(queue_snapshot.wake_rows.into_iter().map(|row| {
                ReparentRoutingChange {
                    store: "queue".to_owned(),
                    record_kind: "parent_wake".to_owned(),
                    record_id: row.id,
                    child_session_id: row.child_session_id,
                    expected_target_session_id: Some(row.parent_session_id),
                    new_target_session_id: edge.new_parent_session_id.clone(),
                    prior_active: Some(row.is_active),
                    period_seconds: Some(row.period_seconds),
                    creates_parent_wake: false,
                }
            }));
            queue_routing_changes.extend(queue_snapshot.message_rows.into_iter().map(|row| {
                ReparentRoutingChange {
                    store: "queue".to_owned(),
                    record_kind: "message".to_owned(),
                    record_id: row.id,
                    child_session_id: row.child_session_id,
                    expected_target_session_id: Some(row.parent_session_id),
                    new_target_session_id: edge.new_parent_session_id.clone(),
                    prior_active: None,
                    period_seconds: None,
                    creates_parent_wake: row.creates_parent_wake,
                }
            }));
        }
        Ok((json_routing_changes, queue_routing_changes))
    }

    fn apply_reparent_request(&self, request_id: &str) -> Result<()> {
        for _ in 0..8 {
            let stage = {
                let _guard = self.write_guard()?;
                let state = self.load_raw_json_value()?;
                let records = reparent_request_records(&state)?;
                if records
                    .iter()
                    .find(|record| record.id == request_id)
                    .is_some_and(|record| record.status == "applied")
                {
                    // A competing completion may have won between an I/O
                    // stage and this retry.  Its durable terminal state is
                    // authoritative; never reinterpret the released lease
                    // as a failure.
                    return Ok(());
                }
                let lease =
                    reparent_apply_lease(&state)?.context("reparent apply lease disappeared")?;
                if lease.request_id != request_id {
                    anyhow::bail!(
                        "reparent apply lease belongs to {}, not {request_id}",
                        lease.request_id
                    );
                }
                records
                    .into_iter()
                    .find(|record| record.id == request_id)
                    .with_context(|| format!("reparent request {request_id} disappeared"))?
                    .apply_stage
                    .unwrap_or_else(|| "applying".to_owned())
            };
            match stage.as_str() {
                "applying" => self.quiesce_reparent_json_routing(request_id)?,
                "prequiesce_aborting" => {
                    self.complete_reparent_prequiesce_abort(request_id)?;
                    return Ok(());
                }
                "json_routing_quiesced" => self.quiesce_reparent_queue_routing(request_id)?,
                "routing_quiesced" => {
                    if let Some(reason) = self.commit_reparent_authority(request_id)? {
                        self.rollback_reparent_precommit(request_id, Some(&reason))?;
                        return Ok(());
                    }
                }
                "authority_committed" => {
                    self.finish_reparent_routing(request_id)?;
                    return Ok(());
                }
                "applied" => return Ok(()),
                other => {
                    anyhow::bail!("reparent request {request_id} cannot resume from stage {other}")
                }
            }
        }
        anyhow::bail!("reparent request {request_id} exceeded the apply stage limit")
    }

    fn quiesce_reparent_json_routing(&self, request_id: &str) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut records = reparent_request_records(&state)?;
        let record = leased_reparent_request_mut(&state, &mut records, request_id)?;
        let plan = record
            .apply_plan
            .clone()
            .context("reparent apply plan is missing")?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if let Some(reason) = reparent_stale_reason(record, &sessions, &state) {
            anyhow::bail!("reparent request became stale before quiesce: {reason}");
        }
        quiesce_json_reparent_routes(&mut state, &plan)?;
        record.apply_stage = Some("json_routing_quiesced".to_owned());
        store_reparent_request_records(&mut state, &records)?;
        self.write_raw_json_value(&state)
    }

    fn quiesce_reparent_queue_routing(&self, request_id: &str) -> Result<()> {
        let snapshot = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let records = reparent_request_records(&state)?;
            let record = leased_reparent_request(&state, &records, request_id)?;
            queue_snapshot_from_plan(
                record
                    .apply_plan
                    .as_ref()
                    .context("reparent apply plan is missing")?,
            )?
        };
        if let Some(queue) = self.queue_store.as_ref() {
            queue.quiesce_parent_routing(&snapshot)?;
        } else if !snapshot.wake_rows.is_empty() || !snapshot.message_rows.is_empty() {
            anyhow::bail!("reparent plan requires the retained queue store")
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut records = reparent_request_records(&state)?;
        let record = leased_reparent_request_mut(&state, &mut records, request_id)?;
        if record.apply_stage.as_deref() != Some("json_routing_quiesced") {
            anyhow::bail!("reparent request stage changed during queue quiesce")
        }
        record.apply_stage = Some("routing_quiesced".to_owned());
        store_reparent_request_records(&mut state, &records)?;
        self.write_raw_json_value(&state)
    }

    fn commit_reparent_authority(&self, request_id: &str) -> Result<Option<String>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut records = reparent_request_records(&state)?;
        let record = leased_reparent_request_mut(&state, &mut records, request_id)?;
        let plan = record
            .apply_plan
            .clone()
            .context("reparent apply plan is missing")?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if let Some(reason) = reparent_stale_reason(record, &sessions, &state) {
            return Ok(Some(reason));
        }
        commit_json_reparent_plan(&mut state, &plan)?;
        record.apply_stage = Some("authority_committed".to_owned());
        store_reparent_request_records(&mut state, &records)?;
        self.write_raw_json_value(&state)?;
        Ok(None)
    }

    fn finish_reparent_routing(&self, request_id: &str) -> Result<()> {
        let route_groups = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let records = reparent_request_records(&state)?;
            let record = leased_reparent_request(&state, &records, request_id)?;
            let plan = record
                .apply_plan
                .as_ref()
                .context("reparent apply plan is missing")?;
            queue_route_groups_from_plan(plan, false)?
        };
        let mut delivered_message_rows = Vec::new();
        for (snapshot, parent) in route_groups {
            if let Some(queue) = self.queue_store.as_ref() {
                delivered_message_rows.extend(
                    queue
                        .retarget_parent_routing(&snapshot, parent.as_deref())?
                        .delivered_message_rows,
                );
            } else if !snapshot.wake_rows.is_empty() || !snapshot.message_rows.is_empty() {
                anyhow::bail!("reparent plan requires the retained queue store")
            }
        }
        self.persist_delivered_reparent_wake_intents(request_id, &delivered_message_rows)?;
        let mut replay_targets = BTreeSet::new();
        loop {
            replay_targets.extend(self.replay_deferred_reparent_routes(request_id, false)?);
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let mut records = reparent_request_records(&state)?;
            let record = leased_reparent_request_mut(&state, &mut records, request_id)?;
            if record.apply_stage.as_deref() != Some("authority_committed") {
                anyhow::bail!("reparent request stage changed during queue retarget")
            }
            if record
                .deferred_routing_intents
                .iter()
                .any(|intent| intent.replayed_at.is_none())
            {
                continue;
            }
            let now = now_rfc3339();
            record.status = "applied".to_owned();
            record.apply_stage = Some("applied".to_owned());
            record.applied_at = Some(now.clone());
            record.decided_at = Some(now);
            record.ready_to_apply = false;
            record.failure_reason = None;
            store_reparent_request_records(&mut state, &records)?;
            store_reparent_apply_lease(&mut state, None)?;
            self.write_raw_json_value(&state)?;
            break;
        }
        self.drain_reparent_replay_targets(&replay_targets);
        Ok(())
    }

    fn replay_deferred_reparent_routes(
        &self,
        request_id: &str,
        reapply_completed: bool,
    ) -> Result<BTreeSet<String>> {
        let mut reapplied = BTreeSet::new();
        let mut replay_targets = BTreeSet::new();
        loop {
            let intent = {
                let _guard = self.write_guard()?;
                let state = self.load_raw_json_value()?;
                let records = reparent_request_records(&state)?;
                leased_reparent_request(&state, &records, request_id)?
                    .deferred_routing_intents
                    .iter()
                    .find(|intent| {
                        intent.replayed_at.is_none()
                            || (reapply_completed && !reapplied.contains(&intent.key))
                    })
                    .cloned()
            };
            let Some(intent) = intent else {
                return Ok(replay_targets);
            };
            let resolved_parent_session_id = {
                let _guard = self.write_guard()?;
                let state = self.load_raw_json_value()?;
                raw_session_object(&state, &intent.child_session_id)
                    .and_then(|session| json_text(session.get("parent_session_id")))
            };
            match intent.operation.as_str() {
                "parent_wake" => {
                    let period_seconds = intent
                        .payload
                        .get("period_seconds")
                        .and_then(Value::as_i64)
                        .context("deferred parent wake has no period")?;
                    if let (Some(queue), Some(parent_session_id)) = (
                        self.queue_store.as_ref(),
                        resolved_parent_session_id.as_deref(),
                    ) {
                        queue.register_parent_wake(
                            &intent.child_session_id,
                            parent_session_id,
                            period_seconds,
                        )?;
                    } else if resolved_parent_session_id.is_some() {
                        anyhow::bail!("deferred parent wake requires the retained queue store")
                    }
                }
                "task_complete" => {
                    let text = intent
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .context("deferred task-complete has no text")?;
                    let message_id = deferred_task_complete_message_id(request_id, &intent.key);
                    if let (Some(queue), Some(parent_session_id)) = (
                        self.queue_store.as_ref(),
                        resolved_parent_session_id.as_deref(),
                    ) {
                        queue.enqueue_message_once_with_metadata(
                            &message_id,
                            parent_session_id,
                            text,
                            "important",
                            QueueMessageMetadata {
                                message_category: Some("task_complete".to_owned()),
                                ..QueueMessageMetadata::default()
                            },
                        )?;
                        queue.cancel_parent_wake(&intent.child_session_id)?;
                        replay_targets.insert(parent_session_id.to_owned());
                    } else if resolved_parent_session_id.is_some() {
                        anyhow::bail!("deferred task-complete requires the retained queue store")
                    }
                }
                "parent_message" => {
                    let text = intent
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .context("deferred parent message has no text")?;
                    let delivery_mode = intent
                        .payload
                        .get("delivery_mode")
                        .and_then(Value::as_str)
                        .context("deferred parent message has no delivery mode")?;
                    let category = intent
                        .payload
                        .get("message_category")
                        .and_then(Value::as_str)
                        .context("deferred parent message has no category")?;
                    let message_id = deferred_parent_message_id(request_id, &intent.key);
                    if let (Some(queue), Some(parent_session_id)) = (
                        self.queue_store.as_ref(),
                        resolved_parent_session_id.as_deref(),
                    ) {
                        queue.enqueue_message_once_with_metadata(
                            &message_id,
                            parent_session_id,
                            text,
                            delivery_mode,
                            QueueMessageMetadata {
                                sender_session_id: Some(intent.child_session_id.clone()),
                                message_category: Some(category.to_owned()),
                                ..QueueMessageMetadata::default()
                            },
                        )?;
                        replay_targets.insert(parent_session_id.to_owned());
                    } else if resolved_parent_session_id.is_some() {
                        anyhow::bail!("deferred parent message requires the retained queue store")
                    }
                }
                "parent_input" => {
                    let text = intent
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .context("deferred parent input has no text")?;
                    let delivery_mode = intent
                        .payload
                        .get("delivery_mode")
                        .and_then(Value::as_str)
                        .context("deferred parent input has no delivery mode")?;
                    let message_id = deferred_parent_message_id(request_id, &intent.key);
                    let metadata = deferred_parent_input_metadata(
                        &intent,
                        resolved_parent_session_id.as_deref(),
                    )?;
                    if let Some(queue) = self.queue_store.as_ref() {
                        queue.enqueue_message_once_with_metadata(
                            &message_id,
                            &intent.child_session_id,
                            text,
                            delivery_mode,
                            metadata,
                        )?;
                        replay_targets.insert(intent.child_session_id.clone());
                    } else {
                        anyhow::bail!("deferred parent input requires the retained queue store")
                    }
                }
                other => anyhow::bail!("unsupported deferred routing operation {other}"),
            }
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let mut records = reparent_request_records(&state)?;
            let record = leased_reparent_request_mut(&state, &mut records, request_id)?;
            let current = record
                .deferred_routing_intents
                .iter_mut()
                .find(|current| current.key == intent.key)
                .with_context(|| format!("deferred routing intent {} disappeared", intent.key))?;
            match intent.operation.as_str() {
                "parent_wake" => {
                    let period_seconds = intent
                        .payload
                        .get("period_seconds")
                        .and_then(Value::as_i64)
                        .context("deferred parent wake has no period")?;
                    if let Some(parent_session_id) = resolved_parent_session_id.as_deref() {
                        upsert_parent_wake_raw(
                            &mut state,
                            &intent.child_session_id,
                            parent_session_id,
                            period_seconds,
                        )?;
                    }
                }
                "task_complete" => {
                    let text = intent
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .context("deferred task-complete has no text")?;
                    let message_id = deferred_task_complete_message_id(request_id, &intent.key);
                    if let Some(parent_session_id) = resolved_parent_session_id.as_deref() {
                        push_retained_message_once_raw(
                            &mut state,
                            &message_id,
                            parent_session_id,
                            text,
                            "important",
                            Some("task_complete"),
                        )?;
                        deactivate_parent_wake_raw(&mut state, &intent.child_session_id)?;
                    }
                }
                "parent_message" => {
                    let text = intent
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .context("deferred parent message has no text")?;
                    let delivery_mode = intent
                        .payload
                        .get("delivery_mode")
                        .and_then(Value::as_str)
                        .context("deferred parent message has no delivery mode")?;
                    let category = intent
                        .payload
                        .get("message_category")
                        .and_then(Value::as_str)
                        .context("deferred parent message has no category")?;
                    let message_id = deferred_parent_message_id(request_id, &intent.key);
                    if let Some(parent_session_id) = resolved_parent_session_id.as_deref() {
                        push_retained_message_once_raw(
                            &mut state,
                            &message_id,
                            parent_session_id,
                            text,
                            delivery_mode,
                            Some(category),
                        )?;
                    }
                }
                "parent_input" => {}
                other => anyhow::bail!("unsupported deferred routing operation {other}"),
            }
            if current.replayed_at.is_none() {
                current.replayed_at = Some(now_rfc3339());
                current.resolved_parent_session_id = resolved_parent_session_id;
            }
            store_reparent_request_records(&mut state, &records)?;
            self.write_raw_json_value(&state)?;
            reapplied.insert(intent.key);
        }
    }

    fn drain_reparent_replay_targets(&self, targets: &BTreeSet<String>) {
        let (Some(runtime), Some(queue)) =
            (self.delivery_runtime.as_ref(), self.queue_store.as_ref())
        else {
            return;
        };
        let result = (|| -> Result<()> {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            for target in targets {
                let node = raw_session_object(&state, target)
                    .and_then(|session| json_text(session.get("node")))
                    .unwrap_or_else(default_node);
                if is_primary_node(&node) {
                    drain_pending_runtime_messages_raw(
                        self, &mut state, target, runtime, queue, None, None, None, false,
                    )?;
                }
            }
            self.write_raw_json_value(&state)
        })();
        if let Err(error) = result {
            eprintln!("deferred reparent message drain will retry on later traffic: {error:#}");
        }
    }

    fn persist_delivered_reparent_wake_intents(
        &self,
        request_id: &str,
        delivered_message_rows: &[ParentRoutingMessageRow],
    ) -> Result<()> {
        if !delivered_message_rows
            .iter()
            .any(|message| message.creates_parent_wake)
        {
            return Ok(());
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut records = reparent_request_records(&state)?;
        let record = leased_reparent_request_mut(&state, &mut records, request_id)?;
        for message in delivered_message_rows
            .iter()
            .filter(|message| message.creates_parent_wake)
        {
            let key = format!("delivered-planned-parent-wake:{}", message.id);
            if !record
                .deferred_routing_intents
                .iter()
                .any(|intent| intent.key == key)
            {
                record
                    .deferred_routing_intents
                    .push(ReparentDeferredRoutingIntent {
                        key,
                        operation: "parent_wake".to_owned(),
                        child_session_id: message.child_session_id.clone(),
                        payload: json!({ "period_seconds": 600 }),
                        created_at: now_rfc3339(),
                        replayed_at: None,
                        resolved_parent_session_id: None,
                    });
            }
        }
        store_reparent_request_records(&mut state, &records)?;
        self.write_raw_json_value(&state)
    }

    fn fail_reparent_apply(&self, request_id: &str, failure_reason: &str) -> Result<()> {
        let before_quiesce = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let mut records = reparent_request_records(&state)?;
            if records
                .iter()
                .find(|record| record.id == request_id)
                .is_some_and(|record| record.status == "applied")
            {
                // The apply error belongs to a losing/retried driver.  Do
                // not overwrite, clear, or notify against an already
                // committed terminal outcome.
                return Ok(());
            }
            let record = leased_reparent_request_mut(&state, &mut records, request_id)?;
            let before_quiesce = matches!(record.apply_stage.as_deref(), None | Some("applying"));
            record.status = "failed".to_owned();
            if before_quiesce {
                record.apply_stage = Some("prequiesce_aborting".to_owned());
            }
            record.failure_reason = Some(failure_reason.to_owned());
            record.ready_to_apply = false;
            record.decided_at = Some(now_rfc3339());
            store_reparent_request_records(&mut state, &records)?;
            self.write_raw_json_value(&state)?;
            before_quiesce
        };
        if !before_quiesce {
            return Ok(());
        }
        self.complete_reparent_prequiesce_abort(request_id)
    }

    fn complete_reparent_prequiesce_abort(&self, request_id: &str) -> Result<()> {
        let mut replay_targets = BTreeSet::new();
        let mut reapply_completed = true;
        loop {
            replay_targets
                .extend(self.replay_deferred_reparent_routes(request_id, reapply_completed)?);
            reapply_completed = false;
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let mut records = reparent_request_records(&state)?;
            let record = leased_reparent_request_mut(&state, &mut records, request_id)?;
            if record.apply_stage.as_deref() != Some("prequiesce_aborting") {
                anyhow::bail!("reparent request stage changed during pre-quiesce abort")
            }
            if record
                .deferred_routing_intents
                .iter()
                .any(|intent| intent.replayed_at.is_none())
            {
                continue;
            }
            record.status = "stale".to_owned();
            record.apply_stage = Some("prequiesce_aborted".to_owned());
            store_reparent_request_records(&mut state, &records)?;
            store_reparent_apply_lease(&mut state, None)?;
            self.write_raw_json_value(&state)?;
            break;
        }
        self.drain_reparent_replay_targets(&replay_targets);
        Ok(())
    }

    fn rollback_reparent_precommit(
        &self,
        request_id: &str,
        stale_reason: Option<&str>,
    ) -> Result<()> {
        let (plan, route_groups) = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let records = reparent_request_records(&state)?;
            let record = leased_reparent_request(&state, &records, request_id)?;
            if !matches!(
                record.apply_stage.as_deref(),
                Some("json_routing_quiesced" | "routing_quiesced")
            ) {
                anyhow::bail!("request {request_id} is no longer pre-commit")
            }
            let plan = record
                .apply_plan
                .clone()
                .context("reparent apply plan is missing")?;
            (plan.clone(), queue_route_groups_from_plan(&plan, true)?)
        };
        let mut delivered_message_rows = Vec::new();
        for (snapshot, parent) in route_groups {
            if let Some(queue) = self.queue_store.as_ref() {
                delivered_message_rows.extend(
                    queue
                        .retarget_parent_routing(&snapshot, parent.as_deref())?
                        .delivered_message_rows,
                );
            } else if !snapshot.wake_rows.is_empty() || !snapshot.message_rows.is_empty() {
                anyhow::bail!("reparent plan requires the retained queue store")
            }
        }
        self.persist_delivered_reparent_wake_intents(request_id, &delivered_message_rows)?;
        {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let records = reparent_request_records(&state)?;
            let _ = leased_reparent_request(&state, &records, request_id)?;
            restore_json_reparent_routes(&mut state, &plan)?;
            verify_old_reparent_edges(&state, &plan)?;
            self.write_raw_json_value(&state)?;
        }
        let mut replay_targets = BTreeSet::new();
        let mut reapply_completed = true;
        loop {
            replay_targets
                .extend(self.replay_deferred_reparent_routes(request_id, reapply_completed)?);
            reapply_completed = false;
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let mut records = reparent_request_records(&state)?;
            let record = leased_reparent_request_mut(&state, &mut records, request_id)?;
            if record
                .deferred_routing_intents
                .iter()
                .any(|intent| intent.replayed_at.is_none())
            {
                continue;
            }
            record.status = if stale_reason.is_some() {
                "stale".to_owned()
            } else {
                "repaired".to_owned()
            };
            record.apply_stage = Some(if stale_reason.is_some() {
                "prequiesce_aborted".to_owned()
            } else {
                "repair_rolled_back".to_owned()
            });
            record.failure_reason = stale_reason.map(ToOwned::to_owned);
            record.ready_to_apply = false;
            record.decided_at = Some(now_rfc3339());
            store_reparent_request_records(&mut state, &records)?;
            store_reparent_apply_lease(&mut state, None)?;
            self.write_raw_json_value(&state)?;
            break;
        }
        self.drain_reparent_replay_targets(&replay_targets);
        Ok(())
    }

    pub fn create_session_credential_rotation(
        &self,
        session_id: &str,
        request_actor: &str,
    ) -> Result<CredentialRotationOutcome> {
        let session_id = session_id.trim();
        let request_actor = request_actor.trim();
        if session_id.is_empty() || request_actor.is_empty() {
            return Ok(CredentialRotationOutcome::BadRequest(
                "session ID and authenticated request actor are required".to_owned(),
            ));
        }
        if self.delivery_runtime.is_none() {
            return Ok(CredentialRotationOutcome::BadRequest(
                "runtime-backed sessions are disabled".to_owned(),
            ));
        }

        let record = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            if let Some(request_id) = active_reparent_request_for_session(&state, session_id)? {
                return Ok(CredentialRotationOutcome::Conflict(format!(
                    "reparent request {request_id} controls session {session_id}"
                )));
            }
            let snapshot = snapshot_from_raw_value(&state)?;
            let Some(session) = snapshot
                .sessions
                .iter()
                .find(|session| session.id == session_id)
            else {
                return Ok(CredentialRotationOutcome::SessionNotFound);
            };
            if !session.is_live_for_registry() {
                finalize_active_credential_rotations_for_terminal_session(&mut state, session_id)?;
                self.write_raw_json_value(&state)?;
                return Ok(CredentialRotationOutcome::BadRequest(format!(
                    "session {session_id} is stopped"
                )));
            }
            if !is_primary_node(&session.node) {
                return Ok(CredentialRotationOutcome::BadRequest(format!(
                    "session {session_id} belongs to remote node {}",
                    session.node
                )));
            }
            if !matches!(session.provider.as_str(), "claude" | "codex" | "codex-fork") {
                return Ok(CredentialRotationOutcome::BadRequest(format!(
                    "provider {} does not support credential rotation",
                    session.provider
                )));
            }
            let Some(provider_resume_id) = provider_resume_id_for_restore(session) else {
                return Ok(CredentialRotationOutcome::BadRequest(format!(
                    "session {session_id} has no resumable provider identity"
                )));
            };
            // The durable record can lag a provider/tmux exit.  Admission
            // linearizes its liveness proof under the same mutation lock that
            // writes `waiting_idle`; terminal scrollback is never consulted.
            let runtime = self
                .delivery_runtime
                .as_ref()
                .expect("runtime-backed rotation checked above");
            let session_runtime = runtime.for_socket_name(session.tmux_socket_name.as_deref());
            if !session_runtime.session_exists(&session.tmux_session)? {
                mark_session_runtime_missing_terminal(&mut state, session_id)?;
                self.write_raw_json_value(&state)?;
                return Ok(CredentialRotationOutcome::BadRequest(format!(
                    "session {session_id} is stopped"
                )));
            }
            let mut rotations = session_credential_rotation_records(&state)?;
            rotations.sort_by(|left, right| {
                (&left.requested_at, &left.id).cmp(&(&right.requested_at, &right.id))
            });
            if let Some(existing) = rotations.iter().rev().find(|rotation| {
                rotation.session_id == session_id
                    && matches!(
                        rotation.status.as_str(),
                        "waiting_idle" | "relaunching" | "applied"
                    )
            }) {
                return Ok(CredentialRotationOutcome::Existing(existing.clone()));
            }
            let now = now_rfc3339();
            let rotation = SessionCredentialRotationRecord {
                id: generate_unique_credential_rotation_id(&rotations)?,
                session_id: session.id.clone(),
                provider: session.provider.clone(),
                provider_resume_id,
                tmux_session: session.tmux_session.clone(),
                tmux_socket_name: session.tmux_socket_name.clone(),
                request_actor: request_actor.to_owned(),
                status: "waiting_idle".to_owned(),
                requested_at: now.clone(),
                idle_proof_at: None,
                runtime_launch_id: None,
                updated_at: now,
                applied_at: None,
                failure_reason: None,
            };
            rotations.push(rotation.clone());
            store_session_credential_rotation_records(&mut state, &rotations)?;
            self.write_raw_json_value(&state)?;
            rotation
        };
        self.start_credential_rotation_worker(record.session_id.clone())?;
        Ok(CredentialRotationOutcome::Created(record))
    }

    pub fn list_session_credential_rotations(
        &self,
    ) -> Result<Vec<SessionCredentialRotationRecord>> {
        let _guard = self.write_guard()?;
        let state = self.load_raw_json_value()?;
        let mut records = session_credential_rotation_records(&state)?;
        records.sort_by(|left, right| {
            (&left.requested_at, &left.id).cmp(&(&right.requested_at, &right.id))
        });
        Ok(records)
    }

    pub fn recover_session_credential_rotation_workers(&self) -> Result<usize> {
        let session_ids = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            session_credential_rotation_records(&state)?
                .into_iter()
                .filter(|record| record.status == "waiting_idle")
                .map(|record| record.session_id)
                .collect::<BTreeSet<_>>()
        };
        for session_id in &session_ids {
            self.start_credential_rotation_worker(session_id.clone())?;
        }
        Ok(session_ids.len())
    }

    pub fn create_core_session(
        &self,
        request: CreateCoreSessionRequest,
        log_dir: Option<PathBuf>,
    ) -> Result<SessionRecord> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if let Some(parent_id) = request.parent_session_id.as_deref() {
            ensure_session_not_reparent_fenced(&state, parent_id)?;
        }
        let record = {
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let record = self.build_core_session_record(
                sessions,
                &request,
                log_dir.as_deref(),
                false,
                None,
            )?;
            sessions.push(serde_json::to_value(&record)?);
            record
        };
        let log_file = record
            .log_file
            .as_deref()
            .map(expand_home)
            .ok_or_else(|| anyhow::anyhow!("fixture session missing log file"))?;
        append_log_line(&log_file, "[sm-rust] fixture session created")?;
        if let Some(initial_message) = request
            .initial_message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            append_log_line(&log_file, initial_message)?;
        }
        if let Some(wait) = request.wait {
            append_log_line(
                &log_file,
                &format!("[sm-rust] fixture watch requested: {wait}s"),
            )?;
        }
        if let Some(brief) = request.spawn_brief.as_ref() {
            bind_spawn_launch_intent_in_state(&mut state, &brief.intent_id, &record.id)?;
        }
        self.write_raw_json_value(&state)?;
        Ok(record)
    }

    pub fn create_core_session_with_runtime(
        &self,
        request: CreateCoreSessionRequest,
        log_dir: Option<PathBuf>,
        runtime: &TmuxRuntime,
    ) -> Result<SessionRecord> {
        let (provider, working_dir) = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let sessions = state
                .get("sessions")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            core_session_provider_and_working_dir(sessions, &request)
        };
        if provider == "codex-fork" {
            if let Some(model) = request
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                runtime.validate_codex_fork_model(model, &expand_home(&working_dir))?;
            }
        }
        let codex_cli_working_dir = (provider == "codex").then_some(working_dir);
        let codex_cli_binding_guard = codex_cli_working_dir
            .as_deref()
            .map(|working_dir| self.lock_codex_cli_binding_working_dir(working_dir))
            .transpose()?;
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if let Some(parent_id) = request.parent_session_id.as_deref() {
            ensure_session_not_reparent_fenced(&state, parent_id)?;
        }
        let mut record = {
            let sessions = ensure_sessions_array_mut(&mut state)?;
            self.build_core_session_record(
                sessions,
                &request,
                log_dir.as_deref(),
                true,
                runtime.socket_name(),
            )?
        };
        let session_credential = generate_session_credential();
        let credential_sha256 = sha256_text(&session_credential);
        record.session_credential_sha256 = Some(credential_sha256.clone());
        if record.provider == "codex"
            && codex_cli_working_dir.as_deref() != Some(record.working_dir.as_str())
        {
            anyhow::bail!("session creation context changed while waiting for Codex binding");
        }
        ensure_runtime_local_node(&record.node)?;
        let log_file = record
            .log_file
            .as_deref()
            .map(expand_home)
            .ok_or_else(|| anyhow::anyhow!("runtime session missing log file"))?;
        let runtime_initial_message = request.initial_message.clone();
        let force_initial_prompt_stdin = request.spawn_brief.is_some();
        let spec = TmuxSessionSpec {
            session_id: record.id.clone(),
            session_credential: Some(session_credential),
            tmux_session: record.tmux_session.clone(),
            working_dir: expand_home(&record.working_dir).display().to_string(),
            log_file,
            provider: record.provider.clone(),
            initial_message: runtime_initial_message.clone(),
            force_initial_prompt_stdin,
            model: request.model.clone(),
            reasoning_effort: request.reasoning_effort.clone(),
        };
        let codex_fork_artifacts = runtime.codex_fork_runtime_artifacts(&spec)?;
        let codex_cli_creation_binding = (record.provider == "codex").then(|| {
            let mut excluded_ids = state
                .get("sessions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|session| json_text(session.get("provider_resume_id")))
                .collect::<BTreeSet<_>>();
            excluded_ids.extend(codex_cli_existing_session_ids(
                &record,
                &self.codex_sessions_root,
            ));
            (
                excluded_ids,
                OffsetDateTime::now_utc().unix_timestamp_nanos(),
            )
        });
        let mut launch_records = session_runtime_launch_records(&state)?;
        let launch_id = generate_unique_runtime_launch_id(&launch_records)?;
        let launch_time = now_rfc3339();
        launch_records.push(SessionRuntimeLaunchRecord {
            id: launch_id.clone(),
            operation_kind: "create".to_owned(),
            session_id: record.id.clone(),
            tmux_session: record.tmux_session.clone(),
            tmux_socket_name: runtime.socket_name().map(ToOwned::to_owned),
            working_dir: spec.working_dir.clone(),
            log_file: spec.log_file.display().to_string(),
            provider: record.provider.clone(),
            provider_resume_id: None,
            credential_rotation_id: None,
            restore_authorized: false,
            initial_message: runtime_initial_message,
            model: request.model.clone(),
            reasoning_effort: request.reasoning_effort.clone(),
            spawn_launch_intent_id: request
                .spawn_brief
                .as_ref()
                .map(|brief| brief.intent_id.clone()),
            spawn_brief_sha256: request
                .spawn_brief
                .as_ref()
                .map(|brief| brief.sha256.clone()),
            force_initial_prompt_stdin,
            credential_sha256,
            status: "launching".to_owned(),
            created_at: launch_time.clone(),
            updated_at: launch_time,
            failure_reason: None,
        });
        record.status = "stopped".to_owned();
        record.stopped_at = Some(now_rfc3339());
        ensure_sessions_array_mut(&mut state)?.push(serde_json::to_value(&record)?);
        if let Some(brief) = request.spawn_brief.as_ref() {
            bind_spawn_launch_intent_in_state(&mut state, &brief.intent_id, &record.id)?;
        }
        store_session_runtime_launch_records(&mut state, &launch_records)?;
        self.write_raw_json_value(&state)?;
        // Runtime startup and provider acknowledgement can wait for tens of
        // seconds. The launch record is durable now, so never retain the
        // global state mutex while waiting for external provider progress.
        drop(_guard);

        if let Err(error) = runtime.create_session(&spec) {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let error_message = error.to_string();
            let recovery_detail = request.spawn_brief.as_ref().and_then(|brief| {
                state
                    .get("spawn_launch_intents")
                    .and_then(Value::as_array)
                    .and_then(|intents| {
                        intents.iter().find(|intent| {
                            intent.get("id").and_then(Value::as_str) == Some(brief.intent_id.as_str())
                        })
                    })
                    .and_then(|intent| intent.get("artifact"))
                    .and_then(|artifact| {
                        let path = artifact.get("path").and_then(Value::as_str)?;
                        let sha256 = artifact.get("sha256").and_then(Value::as_str)?;
                        Some(format!(
                            "accepted brief retained at {path} (sha256 {sha256}); inspect provider state before manually recovering it"
                        ))
                    })
            });
            let failure_reason = recovery_detail
                .as_deref()
                .map(|detail| format!("{error_message}; {detail}"))
                .unwrap_or_else(|| error_message.clone());
            let remove_provisional_session =
                remove_failed_provisional_runtime_session(&state, &record.id);
            mark_runtime_launch_failed(
                &mut state,
                &launch_id,
                &record.id,
                remove_provisional_session,
                &failure_reason,
            )?;
            self.write_raw_json_value(&state)?;
            return Err(match recovery_detail {
                Some(detail) => error.context(format!("{error_message}; {detail}")),
                None => error,
            });
        }
        if let Some((excluded_ids, launched_at_ns)) = codex_cli_creation_binding.as_ref() {
            record.provider_resume_id = wait_for_codex_cli_provider_resume_id(
                &record,
                &self.codex_sessions_root,
                excluded_ids,
                *launched_at_ns,
                CODEX_CLI_SESSION_BIND_TIMEOUT,
            );
        }
        if let Some(artifacts) = &codex_fork_artifacts {
            match wait_for_codex_fork_provider_resume_id_for_launch(
                &artifacts.event_stream_path,
                CODEX_FORK_THREAD_STARTED_TIMEOUT,
                runtime,
                &record.tmux_session,
            ) {
                Ok(provider_resume_id) => {
                    record.provider_resume_id = Some(provider_resume_id);
                }
                Err(error) => {
                    let _ = runtime.kill_session(&record.tmux_session);
                    let _guard = self.write_guard()?;
                    let mut state = self.load_raw_json_value()?;
                    let remove_provisional_session =
                        remove_failed_provisional_runtime_session(&state, &record.id);
                    mark_runtime_launch_failed(
                        &mut state,
                        &launch_id,
                        &record.id,
                        remove_provisional_session,
                        &error.to_string(),
                    )?;
                    self.write_raw_json_value(&state)?;
                    return Err(error).with_context(|| {
                        format!(
                            "codex-fork session {} did not publish a provider resume id",
                            record.id
                        )
                    });
                }
            }
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = sessions
            .iter_mut()
            .find(|session| session.get("id").and_then(Value::as_str) == Some(record.id.as_str()))
        else {
            let _ = runtime.kill_session(&record.tmux_session);
            mark_runtime_launch_failed(
                &mut state,
                &launch_id,
                &record.id,
                false,
                "provisional runtime session disappeared while waiting for provider startup",
            )?;
            self.write_raw_json_value(&state)?;
            anyhow::bail!("provisional runtime session {} disappeared", record.id);
        };
        if completion_status_is_retired(json_text(session.get("completion_status")).as_deref()) {
            let _ = runtime.kill_session(&record.tmux_session);
            mark_runtime_launch_failed(
                &mut state,
                &launch_id,
                &record.id,
                false,
                "session was retired while waiting for provider startup",
            )?;
            self.write_raw_json_value(&state)?;
            anyhow::bail!(
                "session {} was retired while waiting for provider startup",
                record.id
            );
        }
        let mut current_record = serde_json::from_value::<SessionRecord>(session.clone())?;
        current_record.provider_resume_id = record.provider_resume_id.clone();
        current_record.status = "running".to_owned();
        current_record.stopped_at = None;
        current_record.last_activity = now_rfc3339();
        *session = serde_json::to_value(&current_record)?;
        record = current_record;
        mark_runtime_launch_applied(&mut state, &launch_id, record.provider_resume_id.as_deref())?;
        if let Err(error) = self.write_raw_json_value(&state) {
            let _ = runtime.kill_session(&record.tmux_session);
            return Err(error);
        }
        if matches!(record.provider.as_str(), "codex" | "codex-fork") {
            if let Some(provider_resume_id) = record.provider_resume_id.as_deref() {
                self.append_seat_session(
                    &record.id,
                    &record.provider,
                    provider_resume_id,
                    codex_fork_artifacts
                        .as_ref()
                        .and_then(|artifacts| artifacts.event_stream_path.to_str()),
                );
            }
        }
        if record.provider_resume_id.is_none() {
            if let Some((excluded_ids, launched_at_ns)) = codex_cli_creation_binding {
                let store = self.clone();
                let deferred_record = record.clone();
                let deferred_session_id = record.id.clone();
                let spawn_result = thread::Builder::new()
                    .name(format!(
                        "sm-codex-create-bind-{}",
                        sanitize_path_component(&record.id)
                    ))
                    .spawn(move || {
                        let _binding_guard = codex_cli_binding_guard;
                        if let Err(error) = store.complete_deferred_codex_cli_rebind(
                            &deferred_session_id,
                            &deferred_record,
                            &excluded_ids,
                            launched_at_ns,
                            None,
                            CODEX_CLI_DEFERRED_BIND_TIMEOUT,
                        ) {
                            eprintln!(
                                "deferred initial Codex thread discovery failed for seat {deferred_session_id}: {error:#}"
                            );
                        }
                    });
                if let Err(error) = spawn_result {
                    eprintln!("failed to start deferred initial Codex thread discovery: {error}");
                }
            }
        }
        if let Some(artifacts) = codex_fork_artifacts {
            self.start_codex_fork_event_monitor(record.id.clone(), artifacts.event_stream_path)?;
        }
        Ok(record)
    }

    pub fn send_core_input(
        &self,
        session_id: &str,
        request: SendCoreInputRequest,
    ) -> Result<Option<CoreInputResult>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        let status = effective_raw_session_status(session);
        let delivered = !raw_session_is_stopped(session);
        if delivered {
            let now = now_rfc3339();
            mark_session_followup_activity(session, &now);
            if let Some(log_file) = json_text(session.get("log_file")) {
                append_log_line(&expand_home(&log_file), &request.text)?;
                if let Some(seconds) = request.notify_after_seconds {
                    append_log_line(
                        &expand_home(&log_file),
                        &format!("[sm-rust] fixture notify requested: {seconds}s"),
                    )?;
                }
            }
        }
        self.write_raw_json_value(&state)?;
        Ok(Some(CoreInputResult {
            ok: true,
            session_id: session_id.to_owned(),
            delivered,
            delivery_mode: request.delivery_mode,
            notify_after_seconds: request.notify_after_seconds,
            status,
        }))
    }

    pub fn send_core_input_with_runtime(
        &self,
        session_id: &str,
        request: SendCoreInputRequest,
        runtime: &TmuxRuntime,
    ) -> Result<Option<CoreInputResult>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;

        let Some(initial_status) = runtime_session_status_raw(&mut state, session_id)? else {
            return Ok(None);
        };
        if normalized_status(&initial_status) == "stopped" {
            return Ok(Some(CoreInputResult {
                ok: true,
                session_id: session_id.to_owned(),
                delivered: false,
                delivery_mode: request.delivery_mode,
                notify_after_seconds: request.notify_after_seconds,
                status: initial_status,
            }));
        }

        let delivery_mode = normalized_delivery_mode(&request.delivery_mode);
        let (queued_text, sender_name) = format_send_input_text_raw(&state, &request);
        if request.parent_session_id.is_some() {
            if let Some(request_id) = active_reparent_route_request_for_session(&state, session_id)?
            {
                let key = format!("parent-input:{}", generate_session_id());
                let metadata =
                    queue_metadata_for_send_request(&state, session_id, &request, sender_name);
                persist_deferred_parent_input_intent(
                    &mut state,
                    &request_id,
                    &key,
                    session_id,
                    &queued_text,
                    &delivery_mode,
                    &metadata,
                )?;
                self.write_raw_json_value(&state)?;
                return Ok(Some(CoreInputResult {
                    ok: true,
                    session_id: session_id.to_owned(),
                    delivered: false,
                    delivery_mode: request.delivery_mode,
                    notify_after_seconds: request.notify_after_seconds,
                    status: initial_status,
                }));
            }
        }
        if should_persist_runtime_send(&delivery_mode) {
            if let Some(queue) = &self.queue_store {
                let metadata =
                    queue_metadata_for_send_request(&state, session_id, &request, sender_name);
                let pending_message = pending_message_from_metadata(
                    session_id,
                    &queued_text,
                    &delivery_mode,
                    &metadata,
                );
                let message_id = queue.enqueue_message_with_metadata(
                    session_id,
                    &queued_text,
                    &delivery_mode,
                    metadata,
                )?;
                let pending_message = PendingMessage {
                    id: message_id.clone(),
                    ..pending_message
                };
                let drain = if delivery_mode == "urgent" {
                    deliver_urgent_runtime_message_raw(
                        self,
                        &mut state,
                        session_id,
                        runtime,
                        queue,
                        &pending_message,
                    )?
                } else {
                    drain_pending_runtime_messages_raw(
                        self,
                        &mut state,
                        session_id,
                        runtime,
                        queue,
                        if delivery_mode == "important" {
                            Some("important")
                        } else {
                            None
                        },
                        None,
                        Some(&message_id),
                        false,
                    )?
                };
                self.write_raw_json_value(&state)?;
                let delivered = drain
                    .delivered_message_ids
                    .iter()
                    .any(|id| id == &message_id)
                    || queue.message_delivered(&message_id)?;
                return Ok(Some(CoreInputResult {
                    ok: true,
                    session_id: session_id.to_owned(),
                    delivered,
                    delivery_mode: request.delivery_mode,
                    notify_after_seconds: request.notify_after_seconds,
                    status: drain.status,
                }));
            }
        }

        let (status, delivered) =
            deliver_runtime_text_to_session_raw(&mut state, session_id, &queued_text, runtime)?;
        self.write_raw_json_value(&state)?;
        Ok(Some(CoreInputResult {
            ok: true,
            session_id: session_id.to_owned(),
            delivered,
            delivery_mode: request.delivery_mode,
            notify_after_seconds: request.notify_after_seconds,
            status,
        }))
    }

    pub fn start_review_with_runtime(
        &self,
        session_id: &str,
        request: StartReviewRequest,
        runtime: &TmuxRuntime,
        timing: &CodexReviewConfig,
    ) -> Result<CoreReviewOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(CoreReviewOutcome::NotFound);
        };
        if raw_session_is_stopped(session) {
            return Ok(CoreReviewOutcome::Error(
                "Session is stopped. Restore it before starting a review.".to_owned(),
            ));
        }

        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        if !matches!(provider.as_str(), "codex" | "codex-fork" | "codex-app") {
            return Ok(CoreReviewOutcome::Error(
                "Review requires a Codex session (provider=codex, codex-fork, or codex-app)"
                    .to_owned(),
            ));
        }
        if provider == "codex-app" {
            return Ok(CoreReviewOutcome::Error(
                "Rust core review does not support codex-app review/start yet".to_owned(),
            ));
        }

        let mode = normalized_review_mode(&request.mode);
        if !matches!(
            mode.as_str(),
            "branch" | "uncommitted" | "commit" | "custom"
        ) {
            return Ok(CoreReviewOutcome::Error(format!(
                "Unsupported review mode: {mode}"
            )));
        }
        let status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
        if review_session_is_busy(session, &status) {
            return Ok(CoreReviewOutcome::Error(
                "Session is busy. Wait for current work to complete or use sm clear first."
                    .to_owned(),
            ));
        }

        let node = json_text(session.get("node")).unwrap_or_else(default_node);
        ensure_runtime_local_node(&node)?;
        let tmux_session = json_text(session.get("tmux_session"))
            .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
        let session_socket_name = json_text(session.get("tmux_socket_name"));
        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        if !session_runtime.session_exists(&tmux_session)? {
            return Ok(CoreReviewOutcome::Error(
                "Failed to send review sequence to tmux".to_owned(),
            ));
        }
        let working_dir = json_text(session.get("working_dir")).unwrap_or_else(|| ".".to_owned());
        let working_path = expand_home(&working_dir);

        if !git_command_success(&working_path, ["rev-parse", "--git-dir"])? {
            return Ok(CoreReviewOutcome::Error(format!(
                "Working directory is not a git repo: {working_dir}"
            )));
        }

        let branch_position = if mode == "branch" {
            match request
                .base_branch
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(base_branch) => match git_branch_position(&working_path, base_branch)? {
                    Some(position) => Some(position),
                    None => {
                        let branches = git_branch_list(&working_path)?;
                        return Ok(CoreReviewOutcome::Error(format!(
                            "Branch '{base_branch}' not found. Available: {}",
                            branches.join(", ")
                        )));
                    }
                },
                None => None,
            }
        } else {
            None
        };
        if mode == "commit" {
            if let Some(commit_sha) = request
                .commit_sha
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                if !git_commit_exists(&working_path, commit_sha)? {
                    return Ok(CoreReviewOutcome::Error(format!(
                        "Commit '{commit_sha}' not found"
                    )));
                }
            }
        }

        let now = now_rfc3339();
        session.insert(
            "review_config".to_owned(),
            review_config_value(&mode, &request),
        );
        session.insert("last_tool_call".to_owned(), Value::String(now.clone()));
        self.write_raw_json_value(&state)?;

        let delivered = match session_runtime.send_review_sequence(
            &tmux_session,
            &mode,
            request.base_branch.as_deref(),
            request.commit_sha.as_deref(),
            request.custom_prompt.as_deref(),
            branch_position,
            timing,
        ) {
            Ok(delivered) => delivered,
            Err(error) => {
                let mut state = self.load_raw_json_value()?;
                let sessions = ensure_sessions_array_mut(&mut state)?;
                if let Some(session) = session_object_mut(sessions, session_id) {
                    let now = now_rfc3339();
                    mark_review_dispatch_completed(session, &now);
                    self.write_raw_json_value(&state)?;
                }
                return Err(error);
            }
        };
        if !delivered {
            let mut state = self.load_raw_json_value()?;
            let sessions = ensure_sessions_array_mut(&mut state)?;
            if let Some(session) = session_object_mut(sessions, session_id) {
                let now = now_rfc3339();
                mark_review_dispatch_completed(session, &now);
                self.write_raw_json_value(&state)?;
            }
            return Ok(CoreReviewOutcome::Error(
                "Failed to send review sequence to tmux".to_owned(),
            ));
        }

        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(CoreReviewOutcome::NotFound);
        };
        let now = now_rfc3339();
        mark_review_dispatch_completed(session, &now);
        session.insert("status".to_owned(), Value::String("running".to_owned()));
        session.insert("last_activity".to_owned(), Value::String(now));
        self.write_raw_json_value(&state)?;

        Ok(CoreReviewOutcome::Started(CoreReviewResult {
            session_id: session_id.to_owned(),
            review_mode: mode,
            base_branch: request.base_branch,
            commit_sha: request.commit_sha,
            status: "started".to_owned(),
            steer_queued: request
                .steer_text
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty()),
            tmux_session,
            tmux_socket_name: session_socket_name,
            steer_text: request.steer_text,
        }))
    }

    pub fn mark_review_steer_delivered(&self, session_id: &str) -> Result<bool> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(false);
        };
        let Some(review_config) = session
            .get_mut("review_config")
            .and_then(Value::as_object_mut)
        else {
            return Ok(false);
        };
        review_config.insert("steer_delivered".to_owned(), Value::Bool(true));
        self.write_raw_json_value(&state)?;
        Ok(true)
    }

    pub fn drain_runtime_pending_messages_for_session(
        &self,
        session_id: &str,
        runtime: &TmuxRuntime,
    ) -> Result<()> {
        self.drain_runtime_pending_messages_for_session_category(session_id, runtime, None)
    }

    pub fn drain_runtime_pending_messages_for_session_category(
        &self,
        session_id: &str,
        runtime: &TmuxRuntime,
        message_category: Option<&str>,
    ) -> Result<()> {
        if message_category == Some("reparent") {
            return self.drain_reparent_runtime_messages(session_id, runtime);
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if let Some(queue) = &self.queue_store {
            drain_pending_runtime_messages_raw(
                self,
                &mut state,
                session_id,
                runtime,
                queue,
                None,
                message_category,
                None,
                false,
            )?;
            self.write_raw_json_value(&state)?;
        }
        Ok(())
    }

    /// Deliver reparent outbox messages without holding the sessions write
    /// guard.  tmux can block indefinitely; holding the guard here used to
    /// make unrelated readers queue behind a notification retry.
    fn drain_reparent_runtime_messages(
        &self,
        session_id: &str,
        runtime: &TmuxRuntime,
    ) -> Result<()> {
        let Some(queue) = self.queue_store.as_ref() else {
            return Ok(());
        };
        loop {
            let Some(message) = queue
                .pending_messages_for_target_by_category(session_id, "reparent", 1)?
                .into_iter()
                .next()
            else {
                return Ok(());
            };
            let target = {
                let _guard = self.write_guard()?;
                let state = self.load_raw_json_value()?;
                reparent_runtime_delivery_target(&state, session_id, runtime)?
            };
            let Some(target) = target else {
                return Ok(());
            };

            // This is the only potentially blocking operation in this path.
            // No state or apply lock is held while it runs.  codex-fork
            // recipients retain their managed control-channel policy, with
            // tmux used only when the configured fallback permits it.
            let mut control_result = None;
            let delivered = match &target.delivery_route {
                ReparentRuntimeDeliveryRoute::Tmux => runtime
                    .for_socket_name(target.tmux_socket_name.as_deref())
                    .send_input(&target.tmux_session, &message.text)?,
                ReparentRuntimeDeliveryRoute::CodexForkControl { control_socket } => {
                    match control_socket
                        .as_ref()
                        .map_err(|error| anyhow::anyhow!(error.clone()))
                        .and_then(|control_socket| {
                            codex_fork_submit_message(control_socket, &message.text)
                        }) {
                        Ok(()) => {
                            control_result = Some(Ok(()));
                            true
                        }
                        Err(error) => {
                            control_result = Some(Err(error.to_string()));
                            if runtime.codex_fork_control_tmux_fallback_enabled() {
                                runtime
                                    .for_socket_name(target.tmux_socket_name.as_deref())
                                    .send_input(&target.tmux_session, &message.text)?
                            } else {
                                false
                            }
                        }
                    }
                }
            };

            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            if !reparent_runtime_delivery_target(&state, session_id, runtime)?
                .is_some_and(|current| current == target)
            {
                // The session changed while tmux was handling the message.
                // Leave the queue row pending for a fresh, validated attempt.
                return Ok(());
            }
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let session = session_object_mut(sessions, session_id).ok_or_else(|| {
                anyhow::anyhow!("session {session_id} disappeared during delivery")
            })?;
            let control_state_changed = control_result.is_some();
            if let Some(control_result) = control_result {
                match control_result {
                    Ok(()) => {
                        clear_codex_fork_control_degraded_raw(session);
                    }
                    Err(error) => {
                        mark_codex_fork_control_degraded_raw(session, &error);
                    }
                }
            }
            if !delivered {
                if control_state_changed {
                    self.write_raw_json_value(&state)?;
                }
                return Ok(());
            }
            queue.mark_delivered_and_apply_side_effects(&message)?;
            mark_session_followup_activity(session, &now_rfc3339());
            self.write_raw_json_value(&state)?;
        }
    }

    fn drain_runtime_pending_messages_for_writable_session(
        &self,
        session_id: &str,
        runtime: &TmuxRuntime,
    ) -> Result<bool> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let Some(queue) = &self.queue_store else {
            return Ok(false);
        };
        if !runtime_session_accepts_background_delivery_raw(&mut state, session_id, runtime)? {
            return Ok(false);
        }
        drain_pending_runtime_messages_raw(
            self, &mut state, session_id, runtime, queue, None, None, None, true,
        )?;
        self.write_raw_json_value(&state)?;
        Ok(true)
    }

    pub fn drain_runtime_pending_message_targets_by_category(
        &self,
        message_category: &str,
    ) -> Result<usize> {
        let Some(runtime) = self.delivery_runtime.as_ref() else {
            return Ok(0);
        };
        let Some(queue) = self.queue_store.as_ref() else {
            return Ok(0);
        };
        let targets = queue.pending_target_session_ids_by_category(message_category)?;
        let mut failures = Vec::new();
        for target_session_id in &targets {
            if let Err(error) =
                self.drain_runtime_pending_messages_for_writable_session(target_session_id, runtime)
            {
                failures.push(format!("{target_session_id}: {error:#}"));
            }
        }
        if failures.is_empty() {
            Ok(targets.len())
        } else {
            Err(anyhow::anyhow!(
                "failed to drain {message_category} messages for {}",
                failures.join("; ")
            ))
        }
    }

    pub fn enqueue_stop_notification_for_session(
        &self,
        session_id: &str,
        text: &str,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if let Some(queue) = &self.queue_store {
            enqueue_stop_notification_raw(self, &mut state, runtime, queue, session_id, text)?;
        } else if raw_session_object(&state, session_id).is_some() {
            push_retained_message_raw(
                &mut state,
                session_id,
                text,
                "important",
                Some("stop_notify"),
            )?;
        }
        self.write_raw_json_value(&state)?;
        Ok(())
    }

    pub fn clear_core_session(
        &self,
        session_id: &str,
        request: ClearSessionRequest,
    ) -> Result<CoreClearOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(CoreClearOutcome::NotFound);
        };
        if let Some(message) =
            clear_authorization_error(session, request.requester_session_id.as_deref())
        {
            return Ok(CoreClearOutcome::Unauthorized(message));
        }
        if raw_session_is_stopped(session) {
            return Ok(CoreClearOutcome::NotRunning);
        }
        let now = now_rfc3339();
        reset_session_after_clear(session, &now);
        self.cancel_context_monitor_alerts(session_id)?;
        if let Some(log_file) = json_text(session.get("log_file")) {
            append_log_line(&expand_home(&log_file), "[sm-rust] fixture context cleared")?;
            if let Some(prompt) = request
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                append_log_line(&expand_home(&log_file), prompt)?;
            }
        }
        self.write_raw_json_value(&state)?;
        Ok(CoreClearOutcome::Cleared(CoreClearResult {
            status: "cleared".to_owned(),
            session_id: session_id.to_owned(),
        }))
    }

    pub fn clear_core_session_with_runtime(
        &self,
        session_id: &str,
        request: ClearSessionRequest,
        runtime: &TmuxRuntime,
    ) -> Result<CoreClearOutcome> {
        let clear_guard = self.lock_clear_operation(session_id)?;
        let prompt = request
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let (
            tmux_session,
            session_socket_name,
            provider,
            wake_completed,
            claimed_provider_resume_ids,
            codex_cli_record,
            codex_fork_spec,
            previous_provider_resume_id,
        ) = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let claimed_provider_resume_ids = sessions
                .iter()
                .filter_map(|session| json_text(session.get("provider_resume_id")))
                .collect::<BTreeSet<_>>();
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(CoreClearOutcome::NotFound);
            };
            if let Some(message) =
                clear_authorization_error(session, request.requester_session_id.as_deref())
            {
                return Ok(CoreClearOutcome::Unauthorized(message));
            }
            if raw_session_is_stopped(session) {
                return Ok(CoreClearOutcome::NotRunning);
            }
            let node = json_text(session.get("node")).unwrap_or_else(default_node);
            ensure_runtime_local_node(&node)?;
            let tmux_session = json_text(session.get("tmux_session"))
                .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
            let session_socket_name = json_text(session.get("tmux_socket_name"));
            let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
            let wake_completed = json_text(session.get("completion_status"))
                .is_some_and(|value| value == "completed");
            let previous_provider_resume_id = json_text(session.get("provider_resume_id"));
            let codex_cli_record = (provider == "codex")
                .then(|| serde_json::from_value::<SessionRecord>(Value::Object(session.clone())))
                .transpose()?;
            let codex_fork_spec = (provider == "codex-fork")
                .then(|| codex_fork_spec_for_session_raw(session_id, session))
                .transpose()?;
            (
                tmux_session,
                session_socket_name,
                provider,
                wake_completed,
                claimed_provider_resume_ids,
                codex_cli_record,
                codex_fork_spec,
                previous_provider_resume_id,
            )
        };

        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        // Classic Codex rollouts are discovered by root and cwd, so that whole
        // discovery domain must remain exclusive from the snapshot through bind.
        let codex_cli_binding_guard = codex_cli_record
            .as_ref()
            .map(|record| self.lock_codex_cli_binding_operation(record))
            .transpose()?;
        let codex_cli_clear_binding = codex_cli_record
            .map(|mut record| -> Result<_> {
                let launched_at = OffsetDateTime::now_utc();
                // `/new` writes under today's rollout directory, which may be far
                // from the seat's original creation date.
                record.created_at = launched_at
                    .format(&Rfc3339)
                    .context("failed to format Codex clear timestamp")?;
                let mut excluded_ids = claimed_provider_resume_ids;
                excluded_ids.extend(codex_cli_existing_session_ids(
                    &record,
                    &self.codex_sessions_root,
                ));
                Ok((record, excluded_ids, launched_at.unix_timestamp_nanos()))
            })
            .transpose()?;
        let codex_fork_clear_binding = codex_fork_spec
            .map(|spec| -> Result<_> {
                let artifacts = session_runtime
                    .codex_fork_runtime_artifacts(&spec)?
                    .ok_or_else(|| anyhow::anyhow!("session {session_id} has no fork artifacts"))?;
                let initial_offset = fs::metadata(&artifacts.event_stream_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                Ok((artifacts.event_stream_path, initial_offset))
            })
            .transpose()?;
        if matches!(provider.as_str(), "codex" | "codex-fork") {
            if let Some(previous_provider_resume_id) = previous_provider_resume_id.as_deref() {
                self.append_seat_session(
                    session_id,
                    &provider,
                    previous_provider_resume_id,
                    codex_fork_clear_binding
                        .as_ref()
                        .and_then(|(event_stream_path, _)| event_stream_path.to_str()),
                );
            }
        }
        let clear_command = if matches!(provider.as_str(), "codex" | "codex-fork") {
            "/new"
        } else {
            "/clear"
        };
        let delivered = session_runtime.clear_session(
            &tmux_session,
            clear_command,
            prompt.as_deref(),
            wake_completed,
        )?;
        if !delivered {
            return Err(anyhow::anyhow!("tmux session is not running"));
        }
        let replacement_provider_resume_id = if let Some((record, excluded_ids, launched_at_ns)) =
            codex_cli_clear_binding.as_ref()
        {
            let provider_resume_id = wait_for_codex_cli_provider_resume_id(
                record,
                &self.codex_sessions_root,
                excluded_ids,
                *launched_at_ns,
                CODEX_CLI_SESSION_BIND_TIMEOUT,
            );
            if provider_resume_id.is_none() {
                eprintln!("failed to discover replacement Codex thread for seat {session_id}");
            }
            provider_resume_id.map(|provider_resume_id| (provider_resume_id, None))
        } else if let Some((event_stream_path, initial_offset)) = codex_fork_clear_binding.as_ref()
        {
            match wait_for_codex_fork_provider_resume_id_after_offset(
                event_stream_path,
                *initial_offset,
                CODEX_FORK_THREAD_STARTED_TIMEOUT,
            ) {
                Ok(provider_resume_id) => Some((
                    provider_resume_id,
                    event_stream_path.to_str().map(ToOwned::to_owned),
                )),
                Err(error) => {
                    eprintln!(
                            "failed to discover replacement codex-fork thread for seat {session_id}: {error:#}"
                        );
                    None
                }
            }
        } else {
            None
        };
        self.cancel_context_monitor_alerts(session_id)?;

        {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(CoreClearOutcome::NotFound);
            };
            let current_node = json_text(session.get("node")).unwrap_or_else(default_node);
            ensure_runtime_local_node(&current_node)?;
            let current_tmux_session = json_text(session.get("tmux_session"));
            let current_socket_name = json_text(session.get("tmux_socket_name"));
            let current_provider =
                json_text(session.get("provider")).unwrap_or_else(default_provider);
            if current_tmux_session.as_deref() != Some(tmux_session.as_str())
                || current_socket_name != session_socket_name
                || current_provider != provider
            {
                anyhow::bail!("session {session_id} changed while clear was in progress");
            }
            if raw_session_is_stopped(session) {
                anyhow::bail!("session {session_id} stopped while clear was in progress");
            }
            if let Some((provider_resume_id, _)) = replacement_provider_resume_id.as_ref() {
                session.insert(
                    "provider_resume_id".to_owned(),
                    Value::String(provider_resume_id.clone()),
                );
            }
            let now = now_rfc3339();
            reset_session_after_clear(session, &now);
            self.write_raw_json_value(&state)?;
        }
        if let Some((provider_resume_id, artifact_path)) = replacement_provider_resume_id.as_ref() {
            self.append_seat_session(
                session_id,
                &provider,
                provider_resume_id,
                artifact_path.as_deref(),
            );
        }
        if replacement_provider_resume_id.is_none() {
            if let Some((record, excluded_ids, launched_at_ns)) = codex_cli_clear_binding {
                let store = self.clone();
                let deferred_session_id = session_id.to_owned();
                let expected_provider_resume_id = previous_provider_resume_id;
                let spawn_result = thread::Builder::new()
                    .name(format!(
                        "sm-codex-clear-rebind-{}",
                        sanitize_path_component(session_id)
                    ))
                    .spawn(move || {
                        let _clear_guard = clear_guard;
                        let _binding_guard = codex_cli_binding_guard;
                        if let Err(error) = store.complete_deferred_codex_cli_rebind(
                            &deferred_session_id,
                            &record,
                            &excluded_ids,
                            launched_at_ns,
                            expected_provider_resume_id.as_deref(),
                            CODEX_CLI_DEFERRED_BIND_TIMEOUT,
                        ) {
                            eprintln!(
                                "deferred Codex thread discovery failed for seat {deferred_session_id}: {error:#}"
                            );
                        }
                    });
                if let Err(error) = spawn_result {
                    eprintln!("failed to start deferred Codex thread discovery: {error}");
                }
            }
        }
        Ok(CoreClearOutcome::Cleared(CoreClearResult {
            status: "cleared".to_owned(),
            session_id: session_id.to_owned(),
        }))
    }

    fn complete_deferred_codex_cli_rebind(
        &self,
        session_id: &str,
        record: &SessionRecord,
        excluded_ids: &BTreeSet<String>,
        launched_at_ns: i128,
        expected_provider_resume_id: Option<&str>,
        timeout: Duration,
    ) -> Result<bool> {
        let Some(provider_resume_id) = wait_for_codex_cli_provider_resume_id(
            record,
            &self.codex_sessions_root,
            excluded_ids,
            launched_at_ns,
            timeout,
        ) else {
            eprintln!("deferred Codex thread discovery timed out for seat {session_id}");
            return Ok(false);
        };
        {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(false);
            };
            let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
            if provider != "codex"
                || raw_session_is_stopped(session)
                || json_text(session.get("provider_resume_id")).as_deref()
                    != expected_provider_resume_id
            {
                return Ok(false);
            }
            session.insert(
                "provider_resume_id".to_owned(),
                Value::String(provider_resume_id.clone()),
            );
            self.write_raw_json_value(&state)?;
        }
        self.append_seat_session(session_id, "codex", &provider_resume_id, None);
        Ok(true)
    }

    fn lock_clear_operation(&self, session_id: &str) -> Result<SessionClearGuard> {
        self.lock_named_clear_operation(session_id)
    }

    fn lock_credential_rotation_fences<'a>(
        &'a self,
        session_id: &str,
        runtime: &TmuxRuntime,
        tmux_session: &str,
    ) -> Result<(
        SessionClearGuard,
        std::sync::MutexGuard<'a, ()>,
        crate::runtime::SessionInputGuard,
    )> {
        let clear_guard = self.lock_clear_operation(session_id)?;
        let state_guard = self.write_guard()?;
        let input_guard = runtime.lock_session_input(tmux_session)?;
        Ok((clear_guard, state_guard, input_guard))
    }

    fn lock_codex_cli_binding_operation(
        &self,
        record: &SessionRecord,
    ) -> Result<SessionClearGuard> {
        self.lock_codex_cli_binding_working_dir(&record.working_dir)
    }

    fn lock_codex_cli_binding_working_dir(&self, working_dir: &str) -> Result<SessionClearGuard> {
        let sessions_root = resolve_path_lossy(self.codex_sessions_root.clone());
        let working_dir = resolve_path_lossy(expand_home(working_dir));
        self.lock_named_clear_operation(&format!(
            "codex-cli-binding:{sessions_root}\0{working_dir}"
        ))
    }

    fn lock_named_clear_operation(&self, operation_key: &str) -> Result<SessionClearGuard> {
        let lock = {
            let mut locks = self
                .clear_operation_locks
                .lock()
                .map_err(|_| anyhow::anyhow!("clear operation lock registry poisoned"))?;
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(operation_key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(SessionClearLock {
                    held: Mutex::new(false),
                    available: Condvar::new(),
                });
                locks.insert(operation_key.to_owned(), Arc::downgrade(&lock));
                lock
            }
        };
        let mut held = lock
            .held
            .lock()
            .map_err(|_| anyhow::anyhow!("clear operation lock poisoned"))?;
        while *held {
            held = lock
                .available
                .wait(held)
                .map_err(|_| anyhow::anyhow!("clear operation lock poisoned"))?;
        }
        *held = true;
        drop(held);
        Ok(SessionClearGuard { lock })
    }

    pub fn restore_core_session(&self, session_id: &str) -> Result<Option<CoreRestoreOutcome>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        ensure_session_not_reparent_fenced(&state, session_id)?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        if !raw_session_is_stopped(session) {
            return Ok(Some(CoreRestoreOutcome::NotStopped));
        }
        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("running".to_owned()));
        session.insert("stopped_at".to_owned(), Value::Null);
        session.insert("completion_status".to_owned(), Value::Null);
        session.insert("completion_message".to_owned(), Value::Null);
        session.insert("completed_at".to_owned(), Value::Null);
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
        session.insert("last_activity".to_owned(), Value::String(now));
        if let Some(log_file) = json_text(session.get("log_file")) {
            append_log_line(
                &expand_home(&log_file),
                "[sm-rust] fixture session restored",
            )?;
        }
        let restored = serde_json::from_value::<SessionRecord>(Value::Object(session.clone()))?;
        self.write_raw_json_value(&state)?;
        Ok(Some(CoreRestoreOutcome::Restored(restored)))
    }

    pub fn restore_core_session_with_runtime(
        &self,
        session_id: &str,
        runtime: &TmuxRuntime,
    ) -> Result<Option<CoreRestoreOutcome>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        ensure_session_not_reparent_fenced(&state, session_id)?;
        let snapshot = snapshot_from_raw_value(&state)?;
        let Some(mut record) = snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
        else {
            return Ok(None);
        };
        if !record.is_stopped() {
            return Ok(Some(CoreRestoreOutcome::NotStopped));
        }
        if !is_primary_node(&record.node) {
            return Ok(Some(CoreRestoreOutcome::UnsupportedNode(record.node)));
        }
        if !matches!(record.provider.as_str(), "claude" | "codex" | "codex-fork") {
            return Ok(Some(CoreRestoreOutcome::UnsupportedProvider(
                record.provider,
            )));
        }
        let stored_provider_resume_id = record.provider_resume_id.clone();
        if record.provider == "codex" && provider_resume_id_for_restore(&record).is_none() {
            let claimed_ids = snapshot
                .sessions
                .iter()
                .filter(|session| session.id != record.id)
                .filter_map(|session| session.provider_resume_id.clone())
                .collect::<BTreeSet<_>>();
            record.provider_resume_id = discover_codex_cli_resume_id(
                &record,
                &self.codex_sessions_root,
                &claimed_ids,
                CodexCliSessionDiscoveryMode::Restore,
            );
        }
        if record.provider == "claude"
            && record
                .transcript_path
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
        {
            record.transcript_path = discover_claude_transcript_path(
                &record,
                &snapshot.sessions,
                &self.claude_projects_roots,
            );
        }
        let provider_resume_id = provider_resume_id_for_restore(&record);
        let session_runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        if provider_resume_id.is_none()
            && !session_runtime.allows_restore_without_resume_id(&record.provider)
        {
            return Ok(Some(CoreRestoreOutcome::MissingProviderResumeId(
                record.provider,
            )));
        }
        let Some(log_file) = record
            .log_file
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(expand_home)
        else {
            return Err(anyhow::anyhow!("session {session_id} missing log_file"));
        };
        let session_credential = generate_session_credential();
        let credential_sha256 = sha256_text(&session_credential);
        let spec = TmuxSessionSpec {
            session_id: record.id.clone(),
            session_credential: Some(session_credential),
            tmux_session: record.tmux_session.clone(),
            working_dir: expand_home(&record.working_dir).display().to_string(),
            log_file,
            provider: record.provider.clone(),
            initial_message: None,
            force_initial_prompt_stdin: false,
            model: record.model.clone(),
            reasoning_effort: record.reasoning_effort.clone(),
        };
        let codex_fork_artifacts = session_runtime.codex_fork_runtime_artifacts(&spec)?;
        let mut launch_records = session_runtime_launch_records(&state)?;
        let launch_id = generate_unique_runtime_launch_id(&launch_records)?;
        let launch_time = now_rfc3339();
        launch_records.push(SessionRuntimeLaunchRecord {
            id: launch_id.clone(),
            operation_kind: "restore".to_owned(),
            session_id: record.id.clone(),
            tmux_session: record.tmux_session.clone(),
            tmux_socket_name: session_runtime.socket_name().map(ToOwned::to_owned),
            working_dir: spec.working_dir.clone(),
            log_file: spec.log_file.display().to_string(),
            provider: record.provider.clone(),
            provider_resume_id: provider_resume_id.clone(),
            credential_rotation_id: None,
            // This durable intent is written before the explicit restore
            // clears a retired/killed completion marker. Startup recovery may
            // continue only this authorized transition.
            restore_authorized: true,
            initial_message: None,
            model: record.model.clone(),
            reasoning_effort: record.reasoning_effort.clone(),
            spawn_launch_intent_id: None,
            spawn_brief_sha256: None,
            force_initial_prompt_stdin: false,
            credential_sha256: credential_sha256.clone(),
            status: "launching".to_owned(),
            created_at: launch_time.clone(),
            updated_at: launch_time,
            failure_reason: None,
        });
        {
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(None);
            };
            session.insert(
                "session_credential_sha256".to_owned(),
                Value::String(credential_sha256),
            );
            if stored_provider_resume_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                != provider_resume_id.as_deref()
            {
                if let Some(provider_resume_id) = provider_resume_id.as_deref() {
                    session.insert(
                        "provider_resume_id".to_owned(),
                        Value::String(provider_resume_id.to_owned()),
                    );
                }
            }
            if let Some(transcript_path) = record
                .transcript_path
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                session.insert(
                    "transcript_path".to_owned(),
                    Value::String(transcript_path.to_owned()),
                );
            }
        }
        store_session_runtime_launch_records(&mut state, &launch_records)?;
        self.write_raw_json_value(&state)?;

        if session_runtime.session_exists(&record.tmux_session)? {
            if let Err(error) = session_runtime.kill_session(&record.tmux_session) {
                mark_runtime_launch_failed(
                    &mut state,
                    &launch_id,
                    &record.id,
                    false,
                    &format!("failed to tear down prior runtime before restore: {error}"),
                )?;
                self.write_raw_json_value(&state)?;
                return Err(error);
            }
        }
        if let Err(error) =
            session_runtime.restore_session(&spec, &record.provider, provider_resume_id.as_deref())
        {
            mark_runtime_launch_failed(
                &mut state,
                &launch_id,
                &record.id,
                false,
                &error.to_string(),
            )?;
            self.write_raw_json_value(&state)?;
            return Err(error);
        }

        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("running".to_owned()));
        session.insert("stopped_at".to_owned(), Value::Null);
        session.insert("completion_status".to_owned(), Value::Null);
        session.insert("completion_message".to_owned(), Value::Null);
        session.insert("completed_at".to_owned(), Value::Null);
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
        session.insert("last_activity".to_owned(), Value::String(now));
        if let Some(socket_name) = session_runtime.socket_name() {
            session.insert(
                "tmux_socket_name".to_owned(),
                Value::String(socket_name.to_owned()),
            );
        }
        if stored_provider_resume_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            != provider_resume_id.as_deref()
        {
            if let Some(provider_resume_id) = provider_resume_id.as_deref() {
                session.insert(
                    "provider_resume_id".to_owned(),
                    Value::String(provider_resume_id.to_owned()),
                );
            }
        }
        if let Some(transcript_path) = record
            .transcript_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            session.insert(
                "transcript_path".to_owned(),
                Value::String(transcript_path.to_owned()),
            );
        }
        let restored = serde_json::from_value::<SessionRecord>(Value::Object(session.clone()))?;
        mark_runtime_launch_applied(&mut state, &launch_id, provider_resume_id.as_deref())?;
        self.write_raw_json_value(&state)?;
        if let Some(provider_resume_id) = provider_resume_id_for_restore(&restored) {
            self.append_seat_session(
                &restored.id,
                &restored.provider,
                &provider_resume_id,
                restored.transcript_path.as_deref(),
            );
        }
        if let Some(artifacts) = codex_fork_artifacts {
            self.start_codex_fork_event_monitor(restored.id.clone(), artifacts.event_stream_path)?;
        }
        Ok(Some(CoreRestoreOutcome::Restored(restored)))
    }

    pub fn revive_stopped_tmux_client_session(
        &self,
        tmux_session: &str,
        runtime: &TmuxRuntime,
    ) -> Result<Option<String>> {
        let tmux_session = tmux_session.trim();
        if tmux_session.is_empty() {
            return Ok(None);
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session_index) = sessions.iter().position(|value| {
            let Some(session) = value.as_object() else {
                return false;
            };
            json_text(session.get("tmux_session")).as_deref() == Some(tmux_session)
                && json_text(session.get("provider")).as_deref() == Some("codex-fork")
                && json_text(session.get("node"))
                    .as_deref()
                    .is_none_or(is_primary_node)
                && json_text(session.get("status"))
                    .as_deref()
                    .is_some_and(|status| normalized_status(status) == "stopped")
                && !completion_status_is_retired(
                    json_text(session.get("completion_status")).as_deref(),
                )
        }) else {
            return Ok(None);
        };

        let Some(session) = sessions[session_index].as_object() else {
            return Ok(None);
        };
        let Some(session_id) = json_text(session.get("id")) else {
            return Ok(None);
        };
        let session_runtime =
            runtime.for_socket_name(json_text(session.get("tmux_socket_name")).as_deref());
        if !session_runtime.session_exists(tmux_session)? {
            return Ok(None);
        }

        let now = now_rfc3339();
        let Some(session) = sessions[session_index].as_object_mut() else {
            return Ok(None);
        };
        session.insert("status".to_owned(), Value::String("idle".to_owned()));
        session.insert("stopped_at".to_owned(), Value::Null);
        session.insert("completion_status".to_owned(), Value::Null);
        session.insert("completion_message".to_owned(), Value::Null);
        session.insert("completed_at".to_owned(), Value::Null);
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
        session.insert("error_message".to_owned(), Value::Null);
        session.insert("last_activity".to_owned(), Value::String(now));
        if let Some(socket_name) = session_runtime.socket_name() {
            session.insert(
                "tmux_socket_name".to_owned(),
                Value::String(socket_name.to_owned()),
            );
        }

        let spec = codex_fork_spec_for_session_raw(&session_id, session)?;
        let codex_fork_artifacts = session_runtime.codex_fork_runtime_artifacts(&spec)?;
        self.write_raw_json_value(&state)?;
        if let Some(artifacts) = codex_fork_artifacts {
            self.start_codex_fork_event_monitor_from_current_end(
                session_id.clone(),
                artifacts.event_stream_path,
            )?;
        }
        Ok(Some(session_id))
    }

    pub fn set_context_monitor(
        &self,
        session_id: &str,
        request: ContextMonitorRequest,
    ) -> Result<ContextMonitorOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        ensure_session_not_reparent_fenced(&state, session_id)?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let requester_session_id = request.requester_session_id.trim();
        if requester_session_id.is_empty() {
            return Ok(ContextMonitorOutcome::Unauthorized);
        }
        let Some(session) = session_object(sessions, session_id) else {
            return Ok(ContextMonitorOutcome::NotFound);
        };
        let is_self = requester_session_id == session_id;
        let is_parent =
            json_text(session.get("parent_session_id")).as_deref() == Some(requester_session_id);
        if !is_self && !is_parent {
            return Ok(ContextMonitorOutcome::Unauthorized);
        }
        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        if request.enabled && raw_session_is_stopped(session) {
            return Ok(ContextMonitorOutcome::NotRunning);
        }
        if request.enabled && !provider_has_measured_context_gauge(&provider) {
            return Ok(ContextMonitorOutcome::UnsupportedProvider(provider));
        }
        let notify_session_id = request
            .notify_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        if request.enabled && notify_session_id.is_none() {
            return Ok(ContextMonitorOutcome::MissingNotifyTarget);
        }
        if request.enabled {
            let notify_session_id = notify_session_id.as_deref().unwrap_or_default();
            if session_object(sessions, notify_session_id).is_none() {
                return Ok(ContextMonitorOutcome::NotifyTargetNotFound(
                    notify_session_id.to_owned(),
                ));
            }
        }
        if !request.enabled
            && (request.threshold_percentages.is_some()
                || request.warning_percentage.is_some()
                || request.critical_percentage.is_some()
                || request.use_default_thresholds)
        {
            return Ok(ContextMonitorOutcome::InvalidThresholdConfig(
                "threshold options require enabled=true".to_owned(),
            ));
        }
        if request.use_default_thresholds
            && (request.threshold_percentages.is_some()
                || request.warning_percentage.is_some()
                || request.critical_percentage.is_some())
        {
            return Ok(ContextMonitorOutcome::InvalidThresholdConfig(
                "use_default_thresholds conflicts with custom thresholds".to_owned(),
            ));
        }
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(ContextMonitorOutcome::NotFound);
        };
        let effective_enabled = request.enabled;
        let current_warning = session
            .get("context_monitor_warning_percentage")
            .and_then(Value::as_f64);
        let current_critical = session
            .get("context_monitor_critical_percentage")
            .and_then(Value::as_f64);
        let current_percentages =
            json_percentages(session.get("context_monitor_threshold_percentages"));
        let (next_percentages, next_warning, next_critical) = if request.use_default_thresholds {
            (None, None, None)
        } else if let Some(percentages) = request.threshold_percentages.clone() {
            // A new arbitrary-size override supersedes the legacy two-value
            // override instead of combining incompatible policies.
            (Some(percentages), None, None)
        } else {
            (
                current_percentages.clone(),
                request.warning_percentage.or(current_warning),
                request.critical_percentage.or(current_critical),
            )
        };
        let thresholds = if request.enabled {
            match resolve_context_monitor_thresholds(
                next_percentages.clone(),
                next_warning,
                next_critical,
                &self.context_monitor,
            ) {
                Ok(thresholds) => Some(thresholds),
                Err(detail) => return Ok(ContextMonitorOutcome::InvalidThresholdConfig(detail)),
            }
        } else {
            None
        };
        let thresholds_changed = current_percentages != next_percentages
            || current_warning != next_warning
            || current_critical != next_critical;
        session.insert(
            "context_monitor_enabled".to_owned(),
            Value::Bool(effective_enabled),
        );
        if effective_enabled {
            session.insert(
                "context_monitor_threshold_percentages".to_owned(),
                next_percentages.map_or(Value::Null, |values| {
                    Value::Array(values.into_iter().map(|value| json!(value)).collect())
                }),
            );
            session.insert(
                "context_monitor_warning_percentage".to_owned(),
                next_warning.map_or(Value::Null, |value| json!(value)),
            );
            session.insert(
                "context_monitor_critical_percentage".to_owned(),
                next_critical.map_or(Value::Null, |value| json!(value)),
            );
            let notify_session_id = notify_session_id.unwrap();
            session.insert(
                "context_monitor_notify".to_owned(),
                Value::String(notify_session_id.clone()),
            );
            session.insert(
                "context_monitor_notify_source".to_owned(),
                Value::String(
                    if is_parent && notify_session_id == requester_session_id {
                        "parent_derived"
                    } else {
                        "explicit"
                    }
                    .to_owned(),
                ),
            );
            // Enabling starts a fresh notification cycle. Without this, latches
            // left set by an earlier cycle suppress the first warning until a
            // compaction or reset happens to clear them — and a newly
            // registered monitor would never hear about context that was
            // already high when it registered.
            reset_context_oneshot_flags(session);
        } else {
            // Disabling is an explicit end to this alert cycle. A retained
            // context_monitor message describes state the operator has
            // deliberately stopped monitoring, so it must not surface on a
            // later queue drain.
            self.cancel_context_monitor_alerts(session_id)?;
            session.insert("context_monitor_notify".to_owned(), Value::Null);
            session.insert(
                "context_monitor_notify_source".to_owned(),
                Value::String(default_context_monitor_notify_source()),
            );
            if request.enabled {
                reset_context_oneshot_flags(session);
            }
        }
        if thresholds_changed {
            self.cancel_context_monitor_alerts(session_id)?;
        }
        self.write_raw_json_value(&state)?;
        Ok(ContextMonitorOutcome::Updated(ContextMonitorResult {
            status: "ok".to_owned(),
            enabled: effective_enabled,
            warning_percentage: thresholds.as_ref().map(|value| value.warning_percentage),
            critical_percentage: thresholds.as_ref().map(|value| value.critical_percentage),
            threshold_percentages: thresholds.as_ref().map(|value| value.percentages.clone()),
            threshold_source: thresholds
                .as_ref()
                .map(|value| value.source.as_str().to_owned())
                .unwrap_or_else(|| "disabled".to_owned()),
            enforced: effective_enabled && thresholds.is_some(),
        }))
    }

    /// Apply one context-usage event from the Claude status line or the
    /// `PreCompact`/`SessionStart` hooks (sm#203).
    ///
    /// Compaction and context-reset bypass the `context_monitor_enabled` gate:
    /// they describe context *loss*, which matters to a parent whether or not
    /// anyone opted into usage reporting (#210). Usage, warning, and critical
    /// stay opt-in (#206).
    pub fn apply_context_usage_event(
        &self,
        event: &ContextUsageEvent,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<ContextUsageOutcome> {
        let session_id = event.session_id.trim();
        if session_id.is_empty() {
            return Ok(ContextUsageOutcome::UnknownSession);
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if raw_session_object(&state, session_id).is_none() {
            return Ok(ContextUsageOutcome::UnknownSession);
        }
        let emitted_at = event.emitted_at.as_deref();

        match event.event.as_deref().map(str::trim).unwrap_or("") {
            "compaction" => {
                self.apply_compaction_event(&mut state, session_id, emitted_at, runtime)
            }
            // No `_is_compacting` equivalent exists on the Rust side yet, so the
            // acknowledgement itself is a no-op. It carries the handoff path
            // back, though: the recovery hook needs it to reinject the doc, and
            // answering here means it never has to make a second, unauthenticated
            // read. `/sessions/{id}` is an ordinary API route behind Google auth,
            // which a hook on a remote node cannot satisfy — it has a node hook
            // secret and nothing else, and this route already accepts exactly
            // that.
            "compaction_complete" => Ok(ContextUsageOutcome::CompactionCompleteLogged {
                last_handoff_path: {
                    let sessions = ensure_sessions_array_mut(&mut state)?;
                    let Some(session) = session_object_mut(sessions, session_id) else {
                        return Ok(ContextUsageOutcome::UnknownSession);
                    };
                    session.insert("context_compaction_active".to_owned(), Value::Bool(false));
                    clear_context_snapshot(session);
                    let last_handoff_path = json_text(session.get("last_handoff_path"));
                    self.write_raw_json_value(&state)?;
                    last_handoff_path
                },
            }),
            "context_reset" => self.apply_context_reset_event(&mut state, session_id, emitted_at),
            _ => self.apply_context_usage_update(&mut state, session_id, event, runtime),
        }
    }

    fn apply_compaction_event(
        &self,
        state: &mut Value,
        session_id: &str,
        emitted_at: Option<&str>,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<ContextUsageOutcome> {
        let (notify_target, notify_source, label, unsupported_provider) = {
            let sessions = ensure_sessions_array_mut(state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(ContextUsageOutcome::UnknownSession);
            };
            // `PreCompact` fires before the context is refreshed, so this is the
            // reliable reset point for the next cycle. Waiting for usage to drop
            // back under the warning threshold would not work: post-compaction
            // context can legitimately land above it.
            reset_context_oneshot_flags(session);
            set_context_cycle_boundary(session, emitted_at);
            session.insert("context_compaction_active".to_owned(), Value::Bool(true));
            let unsupported_provider = !provider_has_measured_context_gauge(
                json_text(session.get("provider"))
                    .as_deref()
                    .unwrap_or("claude"),
            );
            let (notify_target, notify_source) = if unsupported_provider {
                session.insert("context_monitor_enabled".to_owned(), Value::Bool(false));
                session.insert("context_monitor_notify".to_owned(), Value::Null);
                session.insert(
                    "context_monitor_notify_source".to_owned(),
                    Value::String(default_context_monitor_notify_source()),
                );
                (None, default_context_monitor_notify_source())
            } else {
                let configured_target = json_text(session.get("context_monitor_notify"));
                let notify_source = if configured_target.is_some() {
                    json_text(session.get("context_monitor_notify_source"))
                        .unwrap_or_else(default_context_monitor_notify_source)
                } else {
                    "parent_derived".to_owned()
                };
                (
                    configured_target
                        // Fall back to the parent so an unregistered child still reports
                        // its own context loss upward (#210).
                        .or_else(|| json_text(session.get("parent_session_id"))),
                    notify_source,
                )
            };
            (
                notify_target,
                notify_source,
                raw_session_label(session, session_id),
                unsupported_provider,
            )
        };

        if let Some(notify_target) = notify_target {
            let text = format!(
                "[sm context] Compaction fired for {label} ({session_id}). \
                 Context was compacted — agent is still running."
            );
            let deferred_request_id = if notify_source == "parent_derived" {
                active_reparent_route_request_for_session(state, session_id)?
            } else {
                None
            };
            if let Some(request_id) = deferred_request_id {
                persist_deferred_parent_message_intent(
                    state,
                    &request_id,
                    &format!("context-compaction:{session_id}:{}", now_rfc3339()),
                    session_id,
                    &text,
                    "sequential",
                    "context_monitor",
                )?;
            } else {
                self.queue_context_monitor_message(
                    state,
                    session_id,
                    &notify_target,
                    &text,
                    "sequential",
                    runtime,
                )?;
            }
        }

        if unsupported_provider {
            self.cancel_context_monitor_alerts(session_id)?;
        }
        self.write_raw_json_value(state)?;
        Ok(ContextUsageOutcome::CompactionLogged)
    }

    fn apply_context_reset_event(
        &self,
        state: &mut Value,
        session_id: &str,
        emitted_at: Option<&str>,
    ) -> Result<ContextUsageOutcome> {
        {
            let sessions = ensure_sessions_array_mut(state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(ContextUsageOutcome::UnknownSession);
            };
            reset_context_oneshot_flags(session);
            set_context_cycle_boundary(session, emitted_at);
            session.insert("context_compaction_active".to_owned(), Value::Bool(false));
            clear_context_snapshot(session);
            // The discarded context is what the previous status was describing.
            session.insert("agent_status_text".to_owned(), Value::Null);
            session.insert("agent_status_at".to_owned(), Value::Null);
            session.insert("agent_task_completed_at".to_owned(), Value::Null);
        }
        self.cancel_context_monitor_alerts(session_id)?;
        self.write_raw_json_value(state)?;
        Ok(ContextUsageOutcome::FlagsReset)
    }

    fn apply_context_usage_update(
        &self,
        state: &mut Value,
        session_id: &str,
        event: &ContextUsageEvent,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<ContextUsageOutcome> {
        let session_snapshot = raw_session_object(state, session_id);
        let unsupported_provider = session_snapshot.is_some_and(|session| {
            !provider_has_measured_context_gauge(
                json_text(session.get("provider"))
                    .as_deref()
                    .unwrap_or("claude"),
            )
        });
        let enabled = session_snapshot
            .and_then(|session| session.get("context_monitor_enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && !unsupported_provider;
        // A sample emitted before the current cycle began describes context that
        // no longer exists. Both stamps come from the session's own host, so this
        // never compares clocks across machines. Samples from a hook too old to
        // stamp themselves are still accepted — the same tolerance the lifecycle
        // hooks apply.
        let cycle_boundary = session_snapshot
            .and_then(|session| json_text(session.get("context_cycle_reset_emitted_at")));
        if let (Some(emitted_at), Some(cycle_boundary)) =
            (event.emitted_at.as_deref(), cycle_boundary.as_deref())
        {
            if !timestamp_is_after(emitted_at, cycle_boundary) {
                return Ok(ContextUsageOutcome::StaleSample);
            }
        }
        let stored_sample_at =
            session_snapshot.and_then(|session| json_text(session.get("context_sampled_at")));
        if let (Some(emitted_at), Some(stored_sample_at)) =
            (event.emitted_at.as_deref(), stored_sample_at.as_deref())
        {
            if !timestamp_is_after(emitted_at, stored_sample_at) {
                return Ok(ContextUsageOutcome::StaleSample);
            }
        }
        // Null until the first API call of a session — nothing to record yet.
        let Some(used_percentage) = event.used_percentage else {
            return Ok(ContextUsageOutcome::NoUsage);
        };

        let (alert, changed) = {
            let sessions = ensure_sessions_array_mut(state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(ContextUsageOutcome::UnknownSession);
            };
            let tokens_used = event.total_input_tokens.unwrap_or(0);
            let sampled_at = event
                .emitted_at
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(now_rfc3339);
            let previous_used = session.get("tokens_used").and_then(Value::as_i64);
            let previous_pct = session
                .get("context_used_percentage")
                .and_then(Value::as_f64);
            let previous_context_tokens = session
                .get("context_total_input_tokens")
                .and_then(Value::as_i64);
            let previous_compaction_active = session
                .get("context_compaction_active")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut changed = previous_used != Some(tokens_used)
                || previous_pct != Some(used_percentage)
                || previous_context_tokens != Some(tokens_used)
                || json_text(session.get("context_sampled_at")).is_none()
                || event.emitted_at.is_some()
                    && json_text(session.get("context_sampled_at")).as_deref()
                        != Some(sampled_at.as_str())
                || previous_compaction_active;
            if changed {
                session.insert("tokens_used".to_owned(), json!(tokens_used));
                session.insert("context_used_percentage".to_owned(), json!(used_percentage));
                session.insert("context_total_input_tokens".to_owned(), json!(tokens_used));
                session.insert("context_sampled_at".to_owned(), Value::String(sampled_at));
                session.insert("context_compaction_active".to_owned(), Value::Bool(false));
            }
            if unsupported_provider {
                let had_alert_state = flag_is_set(session, "context_monitor_enabled")
                    || json_text(session.get("context_monitor_notify")).is_some()
                    || flag_is_set(session, "context_warning_sent")
                    || flag_is_set(session, "context_critical_sent");
                if had_alert_state {
                    session.insert("context_monitor_enabled".to_owned(), Value::Bool(false));
                    session.insert("context_monitor_notify".to_owned(), Value::Null);
                    session.insert(
                        "context_monitor_notify_source".to_owned(),
                        Value::String(default_context_monitor_notify_source()),
                    );
                    reset_context_oneshot_flags(session);
                    self.cancel_context_monitor_alerts(session_id)?;
                    changed = true;
                }
            }
            if !enabled {
                if changed {
                    self.write_raw_json_value(state)?;
                }
                return Ok(ContextUsageOutcome::NotRegistered);
            }
            (
                self.latch_context_alert(session, session_id, used_percentage, tokens_used),
                changed,
            )
        };

        let mut changed = changed;
        if let Some(alert) = alert {
            self.queue_context_monitor_message(
                state,
                session_id,
                &alert.notify_target,
                &alert.text,
                alert.delivery_mode,
                runtime,
            )?;
            changed = true;
        }

        // The status line re-renders far more often than the context actually
        // moves, and the state file is around a megabyte. Rewriting it for a
        // sample identical to the stored one would put the heaviest write in the
        // server on the most frequent event it handles.
        if changed {
            self.write_raw_json_value(state)?;
        }
        Ok(ContextUsageOutcome::Recorded { used_percentage })
    }

    /// Decide whether this usage sample reaches a previously unreported
    /// notification milestone. The monitor reports measured context only; it
    /// deliberately does not prescribe any provider or workflow policy.
    fn latch_context_alert(
        &self,
        session: &mut Map<String, Value>,
        session_id: &str,
        used_percentage: f64,
        _tokens_used: i64,
    ) -> Option<ContextAlert> {
        let notify_target = json_text(session.get("context_monitor_notify"))?;
        let thresholds = resolve_context_monitor_thresholds(
            json_percentages(session.get("context_monitor_threshold_percentages")),
            session
                .get("context_monitor_warning_percentage")
                .and_then(Value::as_f64),
            session
                .get("context_monitor_critical_percentage")
                .and_then(Value::as_f64),
            &self.context_monitor,
        )
        .ok()?;
        let label = raw_session_label(session, session_id);
        let rounded = format_percentage(used_percentage);
        let mut reported = context_reported_thresholds(session, &thresholds.percentages);
        let reached = thresholds
            .percentages
            .iter()
            .any(|threshold| used_percentage >= *threshold && !reported.contains(threshold));
        if !reached {
            return None;
        }

        // A sampled value can jump over several milestones. Report the actual
        // current value once, then mark every crossed milestone so later
        // renders cannot manufacture a burst of stale notifications.
        for threshold in &thresholds.percentages {
            if used_percentage >= *threshold && !reported.contains(threshold) {
                reported.push(*threshold);
            }
        }
        session.insert(
            "context_reported_thresholds".to_owned(),
            Value::Array(reported.into_iter().map(|value| json!(value)).collect()),
        );
        // Preserve the legacy latches as a migration projection. They are no
        // longer used to decide notification text or delivery, but existing
        // persisted-state consumers still expose the first and final level.
        if used_percentage >= thresholds.warning_percentage {
            session.insert("context_warning_sent".to_owned(), Value::Bool(true));
        }
        if used_percentage >= thresholds.critical_percentage {
            session.insert("context_critical_sent".to_owned(), Value::Bool(true));
        }
        let text = if notify_target == session_id {
            format!("[sm context] Your context is now at {rounded}%.")
        } else {
            format!("[sm context] Context for {label} ({session_id}) is now at {rounded}%.")
        };
        Some(ContextAlert {
            notify_target,
            text,
            delivery_mode: "sequential",
        })
    }

    fn queue_context_monitor_message(
        &self,
        state: &mut Value,
        sender_session_id: &str,
        notify_target: &str,
        text: &str,
        delivery_mode: &str,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<()> {
        self.queue_parent_message(
            state,
            sender_session_id,
            notify_target,
            text,
            delivery_mode,
            "context_monitor",
            runtime,
        )
    }

    fn queue_parent_message(
        &self,
        state: &mut Value,
        sender_session_id: &str,
        notify_target: &str,
        text: &str,
        delivery_mode: &str,
        message_category: &str,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<()> {
        if raw_session_object(state, notify_target).is_none() {
            return Ok(());
        }
        if let Some(queue) = &self.queue_store {
            let message_id = queue.enqueue_message_with_metadata(
                notify_target,
                text,
                delivery_mode,
                QueueMessageMetadata {
                    sender_session_id: Some(sender_session_id.to_owned()),
                    message_category: Some(message_category.to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )?;
            if let Some(runtime) = runtime {
                let target_node = raw_session_object(state, notify_target)
                    .and_then(|session| json_text(session.get("node")))
                    .unwrap_or_else(default_node);
                if is_primary_node(&target_node) {
                    drain_pending_runtime_messages_raw(
                        self,
                        state,
                        notify_target,
                        runtime,
                        queue,
                        None,
                        None,
                        Some(&message_id),
                        false,
                    )?;
                }
            }
        }
        push_retained_message_raw(
            state,
            notify_target,
            text,
            delivery_mode,
            Some(message_category),
        )?;
        Ok(())
    }

    pub fn schedule_handoff(
        &self,
        session_id: &str,
        request: HandoffRequest,
    ) -> Result<HandoffOutcome> {
        let monitor = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let sessions = ensure_sessions_array_mut(&mut state)?;
            if request.requester_session_id.trim() != session_id {
                return Ok(HandoffOutcome::Error(
                    "sm handoff is self-directed only - requester must equal target session"
                        .to_owned(),
                ));
            }
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(HandoffOutcome::Error(format!(
                    "Session {session_id} not found"
                )));
            };
            let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
            if provider == "codex-app" {
                return Ok(HandoffOutcome::Error(
                    "sm handoff is not supported for codex-app sessions".to_owned(),
                ));
            }

            let monitor = if provider == "codex-fork" {
                let runtime = self.delivery_runtime.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("codex-fork handoff requires the tmux runtime")
                })?;
                let spec = codex_fork_spec_for_session_raw(session_id, session)?;
                let artifacts = runtime
                    .codex_fork_runtime_artifacts(&spec)?
                    .ok_or_else(|| anyhow::anyhow!("codex-fork runtime artifacts unavailable"))?;
                let offset = fs::metadata(&artifacts.event_stream_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                session.insert("pending_handoff_event_offset".to_owned(), json!(offset));
                Some((artifacts.event_stream_path, offset))
            } else {
                None
            };
            session.insert(
                "pending_handoff_path".to_owned(),
                Value::String(request.file_path.clone()),
            );
            if provider == "claude" {
                // Also versions the Rust execution contract: older pending paths
                // must not rotate dormant seats when the fixed server is deployed.
                session.insert(
                    "pending_handoff_recorded_at".to_owned(),
                    Value::String(now_rfc3339()),
                );
                session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
            }
            self.write_raw_json_value(&state)?;
            monitor
        };

        if let Some((event_stream_path, offset)) = monitor {
            self.start_codex_fork_handoff_monitor(
                session_id.to_owned(),
                event_stream_path,
                offset,
            )?;
        }
        Ok(HandoffOutcome::Recorded(HandoffResult {
            status: "recorded".to_owned(),
        }))
    }

    pub fn recover_pending_codex_fork_handoffs(&self) -> Result<usize> {
        let Some(runtime) = self.delivery_runtime.as_ref() else {
            return Ok(0);
        };
        let monitors = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let Some(sessions) = state.get_mut("sessions").and_then(Value::as_array_mut) else {
                return Ok(0);
            };
            let mut monitors = Vec::new();
            let mut changed = false;
            for session in sessions.iter_mut().filter_map(Value::as_object_mut) {
                let Some(session_id) = json_text(session.get("id")) else {
                    continue;
                };
                if json_text(session.get("provider")).as_deref() != Some("codex-fork")
                    || json_text(session.get("pending_handoff_path")).is_none()
                {
                    continue;
                }
                let spec = codex_fork_spec_for_session_raw(&session_id, session)?;
                let Some(artifacts) = runtime.codex_fork_runtime_artifacts(&spec)? else {
                    continue;
                };
                let offset = match session
                    .get("pending_handoff_event_offset")
                    .and_then(Value::as_u64)
                {
                    Some(offset) => offset,
                    None => {
                        let offset = fs::metadata(&artifacts.event_stream_path)
                            .map(|metadata| metadata.len())
                            .unwrap_or(0);
                        session.insert("pending_handoff_event_offset".to_owned(), json!(offset));
                        changed = true;
                        offset
                    }
                };
                monitors.push((session_id, artifacts.event_stream_path, offset));
            }
            if changed {
                self.write_raw_json_value(&state)?;
            }
            monitors
        };

        for (session_id, event_stream_path, offset) in &monitors {
            self.start_codex_fork_handoff_monitor(
                session_id.clone(),
                event_stream_path.clone(),
                *offset,
            )?;
        }
        Ok(monitors.len())
    }

    pub fn recover_pending_claude_handoffs(&self) -> Result<usize> {
        if self.delivery_runtime.is_none() {
            return Ok(0);
        }
        let state = self.load_raw_json_value()?;
        let session_ids = state
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .filter(|session| {
                json_text(session.get("provider")).as_deref() == Some("claude")
                    && is_primary_node(&json_text(session.get("node")).unwrap_or_else(default_node))
                    && !raw_session_is_stopped(session)
                    && json_text(session.get("pending_handoff_path")).is_some()
                    && json_text(session.get("pending_handoff_recorded_at")).is_some()
                    && (json_text(session.get("claude_handoff_in_progress_at")).is_some()
                        || normalized_status(&json_text(session.get("status")).unwrap_or_default())
                            == "idle")
            })
            .filter_map(|session| json_text(session.get("id")))
            .collect::<Vec<_>>();

        let mut started = 0;
        for session_id in session_ids {
            started += usize::from(self.start_pending_claude_handoff(&session_id)?);
        }
        Ok(started)
    }

    pub fn start_pending_claude_handoff(&self, session_id: &str) -> Result<bool> {
        let reservation = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let (reservation, state_changed) = {
                let sessions = ensure_sessions_array_mut(&mut state)?;
                let Some(session) = session_object_mut(sessions, session_id) else {
                    return Ok(false);
                };
                let status = json_text(session.get("status")).unwrap_or_default();
                let file_path = json_text(session.get("pending_handoff_path"));
                let recorded_at = json_text(session.get("pending_handoff_recorded_at"));
                let eligible = json_text(session.get("provider")).as_deref() == Some("claude")
                    && file_path.is_some()
                    && recorded_at.is_some()
                    && !raw_session_is_stopped(session)
                    && is_primary_node(
                        &json_text(session.get("node")).unwrap_or_else(default_node),
                    );
                if !eligible {
                    (None, false)
                } else {
                    let (reservation_at, state_changed) = if let Some(existing) =
                        json_text(session.get("claude_handoff_in_progress_at"))
                    {
                        (existing, false)
                    } else if normalized_status(&status) == "idle" {
                        let now = now_rfc3339();
                        session.insert(
                            "claude_handoff_in_progress_at".to_owned(),
                            Value::String(now.clone()),
                        );
                        session.insert("status".to_owned(), Value::String("running".to_owned()));
                        session.insert("last_activity".to_owned(), Value::String(now.clone()));
                        (now, true)
                    } else {
                        return Ok(false);
                    };
                    (
                        Some((file_path.unwrap(), recorded_at.unwrap(), reservation_at)),
                        state_changed,
                    )
                }
            };
            if state_changed {
                self.write_raw_json_value(&state)?;
            }
            reservation
        };
        let Some((file_path, recorded_at, reservation_at)) = reservation else {
            return Ok(false);
        };

        {
            let mut workers = self
                .claude_handoff_workers
                .lock()
                .map_err(|_| anyhow::anyhow!("Claude handoff worker lock poisoned"))?;
            if !workers.insert(session_id.to_owned()) {
                return Ok(false);
            }
        }

        let store = self.clone();
        let worker_session_id = session_id.to_owned();
        let worker_file_path = file_path.clone();
        let worker_recorded_at = recorded_at.clone();
        let worker_reservation_at = reservation_at.clone();
        let spawn_result = thread::Builder::new()
            .name(format!(
                "sm-claude-handoff-{}",
                sanitize_path_component(session_id)
            ))
            .spawn(move || {
                if let Err(error) = store.execute_pending_claude_handoff(&worker_session_id) {
                    eprintln!("Claude handoff execution failed for {worker_session_id}: {error:#}");
                }
                if let Ok(mut workers) = store.claude_handoff_workers.lock() {
                    workers.remove(&worker_session_id);
                }
                match store.claude_handoff_reservation_was_replaced(
                    &worker_session_id,
                    &worker_file_path,
                    &worker_recorded_at,
                    &worker_reservation_at,
                ) {
                    Ok(true) => {
                        if let Err(error) =
                            store.start_pending_claude_handoff(&worker_session_id)
                        {
                            eprintln!(
                                "Replacement Claude handoff failed to start for {worker_session_id}: {error:#}"
                            );
                        }
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!(
                        "Replacement Claude handoff check failed for {worker_session_id}: {error:#}"
                    ),
                }
            });
        if let Err(error) = spawn_result {
            let mut workers = self
                .claude_handoff_workers
                .lock()
                .map_err(|_| anyhow::anyhow!("Claude handoff worker lock poisoned"))?;
            workers.remove(session_id);
            let rollback = self.release_failed_claude_handoff_worker_reservation(
                session_id,
                &file_path,
                &recorded_at,
                &reservation_at,
            );
            drop(workers);
            if let Err(rollback_error) = rollback {
                return Err(anyhow::anyhow!(error)).context(format!(
                    "failed to start Claude handoff worker; reservation rollback also failed: {rollback_error:#}"
                ));
            }
            return Err(error).with_context(|| "failed to start Claude handoff worker");
        }
        Ok(true)
    }

    fn claude_handoff_reservation_was_replaced(
        &self,
        session_id: &str,
        file_path: &str,
        recorded_at: &str,
        reservation_at: &str,
    ) -> Result<bool> {
        let _guard = self.write_guard()?;
        let state = self.load_raw_json_value()?;
        let Some(session) = raw_session_object(&state, session_id) else {
            return Ok(false);
        };
        Ok(claude_handoff_reservation_replaced_raw(
            session,
            file_path,
            recorded_at,
            reservation_at,
        ))
    }

    fn release_failed_claude_handoff_worker_reservation(
        &self,
        session_id: &str,
        file_path: &str,
        recorded_at: &str,
        reservation_at: &str,
    ) -> Result<bool> {
        let released = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(false);
            };
            let matches = json_text(session.get("pending_handoff_path")).as_deref()
                == Some(file_path)
                && json_text(session.get("pending_handoff_recorded_at")).as_deref()
                    == Some(recorded_at)
                && json_text(session.get("claude_handoff_in_progress_at")).as_deref()
                    == Some(reservation_at);
            if matches {
                session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
                session.insert("status".to_owned(), Value::String("idle".to_owned()));
                session.insert(
                    "error_message".to_owned(),
                    Value::String(
                        "claude_handoff_failed: failed to start handoff worker".to_owned(),
                    ),
                );
                self.write_raw_json_value(&state)?;
            }
            matches
        };
        if released {
            if let Some(runtime) = self.delivery_runtime.as_ref() {
                let _ = self.drain_runtime_pending_messages_for_session(session_id, runtime);
            }
        }
        Ok(released)
    }

    fn with_current_claude_handoff_reservation<F>(
        &self,
        session_id: &str,
        file_path: &str,
        recorded_at: &str,
        reservation_at: &str,
        tmux_session: &str,
        socket_name: &Option<String>,
        action: F,
    ) -> Result<bool>
    where
        F: FnOnce() -> Result<()>,
    {
        let _guard = self.write_guard()?;
        let state = self.load_raw_json_value()?;
        let Some(session) = raw_session_object(&state, session_id) else {
            return Ok(false);
        };
        let matches = json_text(session.get("provider")).as_deref() == Some("claude")
            && !raw_session_is_stopped(session)
            && is_primary_node(&json_text(session.get("node")).unwrap_or_else(default_node))
            && json_text(session.get("tmux_session")).as_deref() == Some(tmux_session)
            && json_text(session.get("tmux_socket_name")).as_deref() == socket_name.as_deref()
            && json_text(session.get("pending_handoff_path")).as_deref() == Some(file_path)
            && json_text(session.get("pending_handoff_recorded_at")).as_deref()
                == Some(recorded_at)
            && json_text(session.get("claude_handoff_in_progress_at")).as_deref()
                == Some(reservation_at);
        if !matches {
            return Ok(false);
        }
        action()?;
        Ok(true)
    }

    fn execute_pending_claude_handoff(&self, session_id: &str) -> Result<bool> {
        let _clear_guard = self.lock_clear_operation(session_id)?;
        let (file_path, recorded_at, reservation_at, tmux_session, socket_name) = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let Some(session) = raw_session_object(&state, session_id) else {
                return Ok(false);
            };
            if json_text(session.get("provider")).as_deref() != Some("claude")
                || raw_session_is_stopped(session)
            {
                return Ok(false);
            }
            let Some(file_path) = json_text(session.get("pending_handoff_path")) else {
                return Ok(false);
            };
            let Some(recorded_at) = json_text(session.get("pending_handoff_recorded_at")) else {
                return Ok(false);
            };
            let Some(reservation_at) = json_text(session.get("claude_handoff_in_progress_at"))
            else {
                return Ok(false);
            };
            (
                file_path,
                recorded_at,
                reservation_at,
                json_text(session.get("tmux_session"))
                    .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?,
                json_text(session.get("tmux_socket_name")),
            )
        };

        let clear_started = Arc::new(AtomicBool::new(false));
        let result = (|| {
            if !Path::new(&file_path).is_file() {
                anyhow::bail!("handoff file not found: {file_path}");
            }
            let runtime = self
                .delivery_runtime
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Claude handoff requires the tmux runtime"))?
                .for_socket_name(socket_name.as_deref());
            let prompt = format!("Read {file_path} and continue from where you left off.");
            let precondition_store = self.clone();
            let precondition_session_id = session_id.to_owned();
            let precondition_file_path = file_path.clone();
            let precondition_recorded_at = recorded_at.clone();
            let precondition_reservation_at = reservation_at.clone();
            let precondition_tmux_session = tmux_session.clone();
            let precondition_socket_name = socket_name.clone();
            let commit_store = self.clone();
            let commit_session_id = session_id.to_owned();
            let commit_file_path = file_path.clone();
            let commit_recorded_at = recorded_at.clone();
            let commit_reservation_at = reservation_at.clone();
            let commit_tmux_session = tmux_session.clone();
            let commit_socket_name = socket_name.clone();
            let commit_clear_started = clear_started.clone();
            let prompt_store = self.clone();
            let prompt_session_id = session_id.to_owned();
            let prompt_file_path = file_path.clone();
            let prompt_recorded_at = recorded_at.clone();
            let prompt_reservation_at = reservation_at.clone();
            let prompt_tmux_session = tmux_session.clone();
            let prompt_socket_name = socket_name.clone();
            runtime.clear_claude_session_if(
                &tmux_session,
                &prompt,
                move || {
                    precondition_store.with_current_claude_handoff_reservation(
                        &precondition_session_id,
                        &precondition_file_path,
                        &precondition_recorded_at,
                        &precondition_reservation_at,
                        &precondition_tmux_session,
                        &precondition_socket_name,
                        || Ok(()),
                    )
                },
                move |send_clear| {
                    commit_store.with_current_claude_handoff_reservation(
                        &commit_session_id,
                        &commit_file_path,
                        &commit_recorded_at,
                        &commit_reservation_at,
                        &commit_tmux_session,
                        &commit_socket_name,
                        || {
                            // Conservatively treat an attempted send as
                            // destructive if tmux reports an error mid-command.
                            commit_clear_started.store(true, Ordering::Release);
                            send_clear()
                        },
                    )
                },
                move |send_prompt| {
                    prompt_store.with_current_claude_handoff_reservation(
                        &prompt_session_id,
                        &prompt_file_path,
                        &prompt_recorded_at,
                        &prompt_reservation_at,
                        &prompt_tmux_session,
                        &prompt_socket_name,
                        send_prompt,
                    )
                },
            )
        })();

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(false);
        };
        let still_same_session = json_text(session.get("provider")).as_deref() == Some("claude")
            && !raw_session_is_stopped(session)
            && is_primary_node(&json_text(session.get("node")).unwrap_or_else(default_node))
            && json_text(session.get("tmux_session")).as_deref() == Some(tmux_session.as_str())
            && json_text(session.get("tmux_socket_name")) == socket_name;
        if !still_same_session {
            return Ok(false);
        }
        match result {
            Ok(ConditionalClearOutcome::Cleared) => {
                session.insert(
                    "last_handoff_path".to_owned(),
                    Value::String(file_path.clone()),
                );
                let (cleared_pending, stopped_after_prompt) = consume_completed_claude_handoff_raw(
                    session,
                    &file_path,
                    &recorded_at,
                    &reservation_at,
                );
                let now = now_rfc3339();
                reset_session_after_clear(session, &now);
                session.insert(
                    "status".to_owned(),
                    Value::String(if stopped_after_prompt {
                        "idle".to_owned()
                    } else {
                        "running".to_owned()
                    }),
                );
                clear_claude_handoff_error_raw(session);
                self.write_raw_json_value(&state)?;
                drop(_guard);
                let _ = self.cancel_context_monitor_alerts(session_id);
                if let Some(runtime) = self.delivery_runtime.as_ref() {
                    let _ = self.drain_runtime_pending_messages_for_session(session_id, runtime);
                }
                Ok(cleared_pending)
            }
            Ok(ConditionalClearOutcome::PostClearPreconditionFailed) => Ok(false),
            Ok(ConditionalClearOutcome::PreconditionFailed) => {
                let released = json_text(session.get("claude_handoff_in_progress_at")).as_deref()
                    == Some(reservation_at.as_str());
                if released {
                    session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
                    self.write_raw_json_value(&state)?;
                }
                drop(_guard);
                if let Some(runtime) = self.delivery_runtime.as_ref() {
                    let _ = self.drain_runtime_pending_messages_for_session(session_id, runtime);
                }
                Ok(false)
            }
            Ok(
                outcome @ (ConditionalClearOutcome::IdlePromptNotReady
                | ConditionalClearOutcome::SessionMissing),
            ) => {
                let still_current = json_text(session.get("pending_handoff_path")).as_deref()
                    == Some(file_path.as_str())
                    && json_text(session.get("pending_handoff_recorded_at")).as_deref()
                        == Some(recorded_at.as_str())
                    && json_text(session.get("claude_handoff_in_progress_at")).as_deref()
                        == Some(reservation_at.as_str());
                if still_current {
                    session.insert("status".to_owned(), Value::String("idle".to_owned()));
                    session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
                    let reason = match outcome {
                        ConditionalClearOutcome::IdlePromptNotReady => {
                            "Claude handoff aborted because the idle prompt was not ready"
                        }
                        ConditionalClearOutcome::SessionMissing => "tmux session is not running",
                        _ => unreachable!(),
                    };
                    session.insert(
                        "error_message".to_owned(),
                        Value::String(format!("claude_handoff_failed: {reason}")),
                    );
                    self.write_raw_json_value(&state)?;
                }
                drop(_guard);
                if still_current {
                    if let Some(runtime) = self.delivery_runtime.as_ref() {
                        let _ =
                            self.drain_runtime_pending_messages_for_session(session_id, runtime);
                    }
                }
                Ok(false)
            }
            Err(error) => {
                let still_current = json_text(session.get("pending_handoff_path")).as_deref()
                    == Some(file_path.as_str())
                    && json_text(session.get("pending_handoff_recorded_at")).as_deref()
                        == Some(recorded_at.as_str())
                    && json_text(session.get("claude_handoff_in_progress_at")).as_deref()
                        == Some(reservation_at.as_str());
                let failed_before_clear = !clear_started.load(Ordering::Acquire);
                if still_current {
                    session.insert("status".to_owned(), Value::String("idle".to_owned()));
                    if failed_before_clear {
                        session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
                    }
                    session.insert(
                        "error_message".to_owned(),
                        Value::String(format!("claude_handoff_failed: {error}")),
                    );
                    self.write_raw_json_value(&state)?;
                }
                drop(_guard);
                if still_current && failed_before_clear {
                    if let Some(runtime) = self.delivery_runtime.as_ref() {
                        let _ =
                            self.drain_runtime_pending_messages_for_session(session_id, runtime);
                    }
                }
                Ok(false)
            }
        }
    }

    pub fn recover_codex_fork_event_monitors(&self) -> Result<usize> {
        let Some(runtime) = self.delivery_runtime.as_ref() else {
            return Ok(0);
        };
        let monitors = self
            .load_snapshot()?
            .into_sessions()
            .into_iter()
            .filter(|session| {
                session.provider == "codex-fork"
                    && is_primary_node(&session.node)
                    && !session.is_stopped()
            })
            .filter_map(|session| {
                codex_fork_event_stream_path(&session, runtime)
                    .map(|event_stream_path| (session.id, event_stream_path))
            })
            .collect::<Vec<_>>();

        for (session_id, event_stream_path) in &monitors {
            // Use one boundary for both the bounded recovery read and the
            // monitor. Events appended after it are consumed by the monitor;
            // none can fall into a restart-time gap.
            let recovery_offset = codex_fork_complete_jsonl_boundary(event_stream_path);
            self.reconcile_codex_fork_context_at_restart(
                session_id,
                event_stream_path,
                recovery_offset,
            )?;
            self.start_codex_fork_event_monitor_at_offset(
                session_id.clone(),
                event_stream_path.clone(),
                recovery_offset,
            )?;
        }
        Ok(monitors.len())
    }

    /// Bring a persisted Codex context snapshot in line with the latest
    /// root-thread gauge before monitoring resumes at EOF. If the bounded tail
    /// cannot establish a current gauge, discard any pending alert and expose
    /// unknown occupancy rather than delivering an alert for context Codex may
    /// have compacted while the service was down.
    fn reconcile_codex_fork_context_at_restart(
        &self,
        session_id: &str,
        event_stream_path: &Path,
        recovery_offset: u64,
    ) -> Result<()> {
        let root_thread_id = self
            .get_session(session_id)?
            .and_then(|session| session.provider_resume_id);
        let usage = latest_codex_fork_context_usage_from_tail(
            event_stream_path,
            root_thread_id.as_deref(),
            recovery_offset,
        );

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(());
        };
        if json_text(session.get("provider")).as_deref() != Some("codex-fork") {
            return Ok(());
        }

        let previous_tokens = session
            .get("context_total_input_tokens")
            .and_then(Value::as_i64);
        let compaction_or_unknown = usage.as_ref().is_none_or(|usage| {
            previous_tokens.is_some_and(|previous_tokens| usage.tokens_used < previous_tokens)
        });
        if compaction_or_unknown {
            self.cancel_context_monitor_alerts(session_id)?;
            reset_context_oneshot_flags(session);
        }

        let mut changed = match usage.as_ref() {
            Some(usage) => {
                let snapshot_changed = previous_tokens != Some(usage.tokens_used)
                    || session
                        .get("context_used_percentage")
                        .and_then(Value::as_f64)
                        != Some(usage.used_percentage)
                    || json_text(session.get("context_sampled_at")).is_none();
                if snapshot_changed {
                    session.insert("tokens_used".to_owned(), json!(usage.tokens_used));
                    session.insert(
                        "context_used_percentage".to_owned(),
                        json!(usage.used_percentage),
                    );
                    session.insert(
                        "context_total_input_tokens".to_owned(),
                        json!(usage.tokens_used),
                    );
                    session.insert(
                        "context_sampled_at".to_owned(),
                        Value::String(now_rfc3339()),
                    );
                }
                snapshot_changed
            }
            None => {
                let had_snapshot = session
                    .get("context_used_percentage")
                    .is_some_and(|value| !value.is_null())
                    || session
                        .get("context_total_input_tokens")
                        .is_some_and(|value| !value.is_null())
                    || session
                        .get("context_sampled_at")
                        .is_some_and(|value| !value.is_null());
                if had_snapshot {
                    session.insert("context_used_percentage".to_owned(), Value::Null);
                    session.insert("context_total_input_tokens".to_owned(), Value::Null);
                    session.insert("context_sampled_at".to_owned(), Value::Null);
                }
                had_snapshot
            }
        };
        let context_alert = if flag_is_set(session, "context_monitor_enabled") {
            usage.as_ref().and_then(|usage| {
                self.latch_context_alert(
                    session,
                    session_id,
                    usage.used_percentage,
                    usage.tokens_used,
                )
            })
        } else {
            None
        };
        if let Some(alert) = context_alert {
            let runtime = self.delivery_runtime.clone();
            self.queue_context_monitor_message(
                &mut state,
                session_id,
                &alert.notify_target,
                &alert.text,
                alert.delivery_mode,
                runtime.as_ref(),
            )?;
            changed = true;
        }
        if changed || compaction_or_unknown {
            self.write_raw_json_value(&state)?;
        }
        Ok(())
    }

    fn start_codex_fork_handoff_monitor(
        &self,
        session_id: String,
        event_stream_path: PathBuf,
        initial_offset: u64,
    ) -> Result<()> {
        {
            let mut monitors = self
                .codex_fork_handoff_monitors
                .lock()
                .map_err(|_| anyhow::anyhow!("codex-fork handoff monitor lock poisoned"))?;
            if !monitors.insert(session_id.clone()) {
                return Ok(());
            }
        }

        let store = self.clone();
        let monitor_session_id = session_id.clone();
        let spawn_result = thread::Builder::new()
            .name(format!(
                "sm-codex-fork-handoff-{}",
                sanitize_path_component(&session_id)
            ))
            .spawn(move || {
                store.monitor_codex_fork_handoff(
                    &monitor_session_id,
                    &event_stream_path,
                    initial_offset,
                );
                if let Ok(mut monitors) = store.codex_fork_handoff_monitors.lock() {
                    monitors.remove(&monitor_session_id);
                }
            });
        if let Err(error) = spawn_result {
            if let Ok(mut monitors) = self.codex_fork_handoff_monitors.lock() {
                monitors.remove(&session_id);
            }
            return Err(error).with_context(|| "failed to start codex-fork handoff monitor");
        }
        Ok(())
    }

    fn monitor_codex_fork_handoff(
        &self,
        session_id: &str,
        event_stream_path: &Path,
        initial_offset: u64,
    ) {
        let mut offset = initial_offset;
        let mut buffer = String::new();
        loop {
            match self.codex_fork_handoff_is_pending(session_id) {
                Ok(true) => {}
                Ok(false) | Err(_) => return,
            }
            if let Ok(chunk) = read_file_from_offset(event_stream_path, &mut offset) {
                for line in split_complete_event_lines(&mut buffer, &chunk) {
                    if codex_fork_event_is_turn_complete(&line) {
                        if self
                            .execute_pending_codex_fork_handoff(session_id)
                            .is_ok_and(|completed| completed)
                        {
                            return;
                        }
                    }
                }
            }
            thread::sleep(CODEX_FORK_EVENT_MONITOR_POLL);
        }
    }

    fn codex_fork_handoff_is_pending(&self, session_id: &str) -> Result<bool> {
        let state = self.load_raw_json_value()?;
        Ok(raw_session_object(&state, session_id)
            .and_then(|session| json_text(session.get("pending_handoff_path")))
            .is_some())
    }

    fn execute_pending_codex_fork_handoff(&self, session_id: &str) -> Result<bool> {
        let (
            file_path,
            tmux_session,
            socket_name,
            event_stream_path,
            event_offset,
            previous_provider_resume_id,
        ) = {
            let _guard = self.write_guard()?;
            let state = self.load_raw_json_value()?;
            let Some(session) = raw_session_object(&state, session_id) else {
                return Ok(false);
            };
            let Some(file_path) = json_text(session.get("pending_handoff_path")) else {
                return Ok(false);
            };
            let runtime = self
                .delivery_runtime
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("codex-fork handoff requires the tmux runtime"))?;
            let spec = codex_fork_spec_for_session_raw(session_id, session)?;
            let artifacts = runtime
                .codex_fork_runtime_artifacts(&spec)?
                .ok_or_else(|| anyhow::anyhow!("codex-fork runtime artifacts unavailable"))?;
            let event_offset = fs::metadata(&artifacts.event_stream_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            (
                file_path,
                json_text(session.get("tmux_session"))
                    .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?,
                json_text(session.get("tmux_socket_name")),
                artifacts.event_stream_path,
                event_offset,
                json_text(session.get("provider_resume_id")),
            )
        };

        let result = (|| {
            if !Path::new(&file_path).is_file() {
                anyhow::bail!("handoff file not found: {file_path}");
            }
            let runtime = self
                .delivery_runtime
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("codex-fork handoff requires the tmux runtime"))?
                .for_socket_name(socket_name.as_deref());
            let prompt = format!("Read {file_path} and continue from where you left off.");
            if !runtime.clear_codex_session_confirming_prompt(
                &tmux_session,
                &prompt,
                &event_stream_path,
                event_offset,
            )? {
                anyhow::bail!("tmux session is not running");
            }
            wait_for_codex_fork_provider_resume_id_after_offset(
                &event_stream_path,
                event_offset,
                CODEX_FORK_THREAD_STARTED_TIMEOUT,
            )
        })();

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(false);
        };
        if raw_session_is_stopped(session) {
            return Ok(false);
        }
        match result {
            Ok(provider_resume_id) => {
                session.insert(
                    "provider_resume_id".to_owned(),
                    Value::String(provider_resume_id.clone()),
                );
                session.insert(
                    "last_handoff_path".to_owned(),
                    Value::String(file_path.clone()),
                );
                let cleared_pending = json_text(session.get("pending_handoff_path")).as_deref()
                    == Some(file_path.as_str());
                if cleared_pending {
                    session.insert("pending_handoff_path".to_owned(), Value::Null);
                    session.insert("pending_handoff_event_offset".to_owned(), Value::Null);
                }
                let now = now_rfc3339();
                reset_session_after_clear(session, &now);
                session.insert("status".to_owned(), Value::String("running".to_owned()));
                clear_codex_fork_control_degraded_raw(session);
                clear_codex_fork_handoff_error_raw(session);
                self.write_raw_json_value(&state)?;
                drop(_guard);
                if let Some(previous_provider_resume_id) = previous_provider_resume_id.as_deref() {
                    self.append_seat_session(
                        session_id,
                        "codex-fork",
                        previous_provider_resume_id,
                        event_stream_path.to_str(),
                    );
                }
                self.append_seat_session(
                    session_id,
                    "codex-fork",
                    &provider_resume_id,
                    event_stream_path.to_str(),
                );
                let _ = self.cancel_context_monitor_alerts(session_id);
                Ok(cleared_pending)
            }
            Err(error) => {
                session.insert(
                    "error_message".to_owned(),
                    Value::String(format!("codex_fork_handoff_failed: {error}")),
                );
                self.write_raw_json_value(&state)?;
                Ok(false)
            }
        }
    }

    pub fn list_agent_registrations(&self) -> Result<Vec<AgentRegistrationResponse>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut changed = recover_missing_maintainer_registration_raw(&mut state)?;
        changed |= prune_agent_registrations_raw(&mut state)?;
        let registrations = agent_registration_responses_from_state(&state)?;
        if changed {
            self.write_raw_json_value(&state)?;
        }
        Ok(registrations)
    }

    pub fn lookup_agent_registration(
        &self,
        role: &str,
    ) -> Result<Option<AgentRegistrationResponse>> {
        let normalized_role = normalize_role(role);
        if normalized_role.is_empty() {
            return Ok(None);
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut changed = recover_missing_maintainer_registration_raw(&mut state)?;
        changed |= prune_agent_registrations_raw(&mut state)?;
        let registration = agent_registration_responses_from_state(&state)?
            .into_iter()
            .find(|registration| registration.role == normalized_role);
        if changed {
            self.write_raw_json_value(&state)?;
        }
        Ok(registration)
    }

    pub fn update_session_metadata(
        &self,
        session_id: &str,
        request: UpdateSessionMetadataRequest,
        reserved_human_names: &BTreeSet<String>,
    ) -> Result<SessionMetadataOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        recover_missing_maintainer_registration_raw(&mut state)?;
        prune_agent_registrations_raw(&mut state)?;

        let snapshot = snapshot_from_raw_value(&state)?;
        if !snapshot
            .sessions
            .iter()
            .any(|session| session.id == session_id)
        {
            return Ok(SessionMetadataOutcome::NotFound);
        }
        let primary_alias = snapshot
            .alias_map()
            .get(session_id)
            .and_then(|aliases| aliases.iter().next().cloned());
        if let Some(friendly_name) = request.friendly_name.as_deref() {
            if let Some(error) = validate_friendly_name_update_raw(
                &state,
                session_id,
                friendly_name,
                &primary_alias,
                reserved_human_names,
            )? {
                return Ok(SessionMetadataOutcome::BadRequest(error));
            }
        }

        let sessions = ensure_sessions_array_mut(&mut state)?;
        for candidate in sessions.iter_mut() {
            let Some(candidate) = candidate.as_object_mut() else {
                continue;
            };
            let is_target = candidate.get("id").and_then(Value::as_str) == Some(session_id);
            if is_target {
                if let Some(friendly_name) = request.friendly_name.as_deref() {
                    candidate.insert(
                        "friendly_name".to_owned(),
                        Value::String(friendly_name.to_owned()),
                    );
                    candidate.insert("friendly_name_is_explicit".to_owned(), Value::Bool(true));
                    candidate.insert(
                        "friendly_name_updated_at_ns".to_owned(),
                        Value::Number(serde_json::Number::from(now_unix_timestamp_nanos())),
                    );
                }
                if let Some(is_em) = request.is_em {
                    candidate.insert("is_em".to_owned(), Value::Bool(is_em));
                    if is_em {
                        candidate.insert("role".to_owned(), Value::String("em".to_owned()));
                    } else if json_text(candidate.get("role")).as_deref() == Some("em") {
                        candidate.insert("role".to_owned(), Value::Null);
                    }
                }
            } else if request.is_em == Some(true)
                && candidate.get("is_em").and_then(Value::as_bool) == Some(true)
            {
                candidate.insert("is_em".to_owned(), Value::Bool(false));
                if json_text(candidate.get("role")).as_deref() == Some("em") {
                    candidate.insert("role".to_owned(), Value::Null);
                }
            }
        }

        self.write_raw_json_value(&state)?;
        let session = snapshot_from_raw_value(&state)?
            .into_sessions()
            .into_iter()
            .find(|session| session.id == session_id)
            .ok_or_else(|| anyhow::anyhow!("updated session {session_id} was not readable"))?;
        Ok(SessionMetadataOutcome::Updated(session))
    }

    pub fn queue_provider_native_rename(
        &self,
        session: &SessionRecord,
        friendly_name: &str,
    ) -> Result<bool> {
        if !is_safe_provider_native_rename_name(friendly_name)
            || session.is_stopped()
            || !matches!(session.provider.as_str(), "claude" | "codex-fork")
            || session.tmux_session.trim().is_empty()
        {
            return Ok(false);
        }
        let Some(queue) = &self.queue_store else {
            if session.native_title.as_deref().map(str::trim) == Some(friendly_name) {
                return Ok(true);
            }
            return Ok(false);
        };
        queue.cancel_pending_messages_for_target_category(&session.id, "native_rename")?;
        if session.native_title.as_deref().map(str::trim) == Some(friendly_name) {
            return Ok(true);
        }
        queue.enqueue_message(
            &session.id,
            &format!("/rename {friendly_name}"),
            "sequential",
            Some("native_rename"),
        )?;
        Ok(true)
    }

    pub fn register_agent_role(
        &self,
        session_id: &str,
        request: RoleRegistrationRequest,
    ) -> Result<RegistryMutationOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(RegistryMutationOutcome::BadRequest(
                "sm register is self-directed only".to_owned(),
            ));
        }
        self.register_agent_role_raw(session_id, &request.role)
    }

    pub fn unregister_agent_role(
        &self,
        session_id: &str,
        request: RoleRegistrationRequest,
    ) -> Result<RegistryMutationOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(RegistryMutationOutcome::BadRequest(
                "sm unregister is self-directed only".to_owned(),
            ));
        }
        self.unregister_agent_role_raw(session_id, &request.role)
    }

    pub fn set_maintainer_session(
        &self,
        session_id: &str,
        request: SetMaintainerRequest,
    ) -> Result<MaintainerMutationOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(MaintainerMutationOutcome::BadRequest(
                "sm maintainer is self-directed only".to_owned(),
            ));
        }
        match self.register_agent_role_raw(session_id, "maintainer")? {
            RegistryMutationOutcome::Registered(_) => {
                let session = self.get_session(session_id)?.ok_or_else(|| {
                    anyhow::anyhow!("session disappeared after maintainer update")
                })?;
                Ok(MaintainerMutationOutcome::Updated(session))
            }
            RegistryMutationOutcome::NotFound => Ok(MaintainerMutationOutcome::NotFound),
            RegistryMutationOutcome::BadRequest(_) | RegistryMutationOutcome::Conflict(_) => Ok(
                MaintainerMutationOutcome::BadRequest("Failed to register maintainer".to_owned()),
            ),
            RegistryMutationOutcome::RoleNotRegistered | RegistryMutationOutcome::RoleNotOwned => {
                Ok(MaintainerMutationOutcome::BadRequest(
                    "Failed to register maintainer".to_owned(),
                ))
            }
        }
    }

    pub fn clear_maintainer_session(
        &self,
        session_id: &str,
        request: SetMaintainerRequest,
    ) -> Result<MaintainerMutationOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(MaintainerMutationOutcome::BadRequest(
                "sm maintainer --clear is self-directed only".to_owned(),
            ));
        }
        match self.unregister_agent_role_raw(session_id, "maintainer")? {
            RegistryMutationOutcome::Registered(_) => {
                let session = self
                    .get_session(session_id)?
                    .ok_or_else(|| anyhow::anyhow!("session disappeared after maintainer clear"))?;
                Ok(MaintainerMutationOutcome::Updated(session))
            }
            RegistryMutationOutcome::NotFound => Ok(MaintainerMutationOutcome::NotFound),
            RegistryMutationOutcome::RoleNotRegistered
            | RegistryMutationOutcome::RoleNotOwned
            | RegistryMutationOutcome::BadRequest(_)
            | RegistryMutationOutcome::Conflict(_) => Ok(MaintainerMutationOutcome::BadRequest(
                "Session is not the active maintainer".to_owned(),
            )),
        }
    }

    fn register_agent_role_raw(
        &self,
        session_id: &str,
        role: &str,
    ) -> Result<RegistryMutationOutcome> {
        let normalized_role = normalize_role(role);
        if normalized_role.is_empty() {
            return Ok(RegistryMutationOutcome::BadRequest(
                "Role cannot be empty".to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        recover_missing_maintainer_registration_raw(&mut state)?;
        prune_agent_registrations_raw(&mut state)?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let Some(session) = sessions.iter().find(|session| session.id == session_id) else {
            return Ok(RegistryMutationOutcome::NotFound);
        };
        if session.is_stopped() {
            return Ok(RegistryMutationOutcome::Conflict(
                "Stopped sessions cannot register roles".to_owned(),
            ));
        }

        let existing = find_raw_registration(&state, &normalized_role)?;
        if let Some(existing) = existing
            .as_ref()
            .filter(|entry| entry.session_id != session_id)
        {
            return Ok(RegistryMutationOutcome::Conflict(format!(
                "Role \"{}\" is already registered to {}",
                normalized_role, existing.session_id
            )));
        }

        let created_at = existing
            .and_then(|entry| entry.created_at)
            .unwrap_or_else(now_rfc3339);
        upsert_raw_registration(&mut state, &normalized_role, session_id, &created_at)?;
        sync_maintainer_alias_raw(&mut state)?;
        let response = agent_registration_responses_from_state(&state)?
            .into_iter()
            .find(|registration| registration.role == normalized_role)
            .ok_or_else(|| anyhow::anyhow!("registered role {normalized_role} was not readable"))?;
        self.write_raw_json_value(&state)?;
        Ok(RegistryMutationOutcome::Registered(response))
    }

    fn unregister_agent_role_raw(
        &self,
        session_id: &str,
        role: &str,
    ) -> Result<RegistryMutationOutcome> {
        let normalized_role = normalize_role(role);
        if normalized_role.is_empty() {
            return Ok(RegistryMutationOutcome::BadRequest(
                "Role cannot be empty".to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        recover_missing_maintainer_registration_raw(&mut state)?;
        prune_agent_registrations_raw(&mut state)?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if !sessions.iter().any(|session| session.id == session_id) {
            return Ok(RegistryMutationOutcome::NotFound);
        }

        let registrations = agent_registration_responses_from_state(&state)?;
        let Some(response) = registrations
            .into_iter()
            .find(|registration| registration.role == normalized_role)
        else {
            return Ok(RegistryMutationOutcome::RoleNotRegistered);
        };
        if response.session_id != session_id {
            return Ok(RegistryMutationOutcome::RoleNotOwned);
        }

        remove_raw_registration(&mut state, &normalized_role)?;
        forget_role_last_session_raw(&mut state, &normalized_role)?;
        sync_maintainer_alias_raw(&mut state)?;
        self.write_raw_json_value(&state)?;
        Ok(RegistryMutationOutcome::Registered(response))
    }

    pub fn retire_core_session(
        &self,
        session_id: &str,
        requester_session_id: Option<&str>,
    ) -> Result<CoreRetireOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        ensure_session_not_reparent_fenced(&state, session_id)?;
        let recipient_name = {
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(CoreRetireOutcome::NotFound);
            };
            if let Some(requester_session_id) = requester_session_id {
                if !requester_session_id.is_empty()
                    && json_text(session.get("parent_session_id")).as_deref()
                        != Some(requester_session_id)
                {
                    return Ok(CoreRetireOutcome::NotChild);
                }
            }
            raw_session_display_name(session, session_id)
        };
        // A terminal write and its rotation finalization share this one atomic
        // state replacement, so recovery cannot later relaunch the seat.
        finalize_active_credential_rotations_for_terminal_session(&mut state, session_id)?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let session = session_object_mut(sessions, session_id)
            .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during retire"))?;
        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("stopped".to_owned()));
        mark_session_retired(session, &now);
        session.insert("stopped_at".to_owned(), Value::String(now.clone()));
        session.insert("last_activity".to_owned(), Value::String(now));
        if let Some(log_file) = json_text(session.get("log_file")) {
            append_log_line(&expand_home(&log_file), "[sm-rust] fixture session retired")?;
        }
        complete_stop_notify_after_stop_raw(self, &mut state, None, session_id, &recipient_name)?;
        self.write_raw_json_value(&state)?;
        Ok(CoreRetireOutcome::Retired(CoreRetireResult {
            ok: true,
            session_id: session_id.to_owned(),
            status: "retired".to_owned(),
        }))
    }

    pub fn retire_core_session_with_runtime(
        &self,
        session_id: &str,
        requester_session_id: Option<&str>,
        runtime: &TmuxRuntime,
    ) -> Result<CoreRetireOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        ensure_session_not_reparent_fenced(&state, session_id)?;
        let (node, tmux_session, session_socket_name, recipient_name) = {
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let Some(session) = session_object_mut(sessions, session_id) else {
                return Ok(CoreRetireOutcome::NotFound);
            };
            if let Some(requester_session_id) = requester_session_id {
                if !requester_session_id.is_empty()
                    && json_text(session.get("parent_session_id")).as_deref()
                        != Some(requester_session_id)
                {
                    return Ok(CoreRetireOutcome::NotChild);
                }
            }
            let node = json_text(session.get("node")).unwrap_or_else(default_node);
            let tmux_session = json_text(session.get("tmux_session"))
                .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
            let session_socket_name = json_text(session.get("tmux_socket_name"));
            let recipient_name = raw_session_display_name(session, session_id);
            (node, tmux_session, session_socket_name, recipient_name)
        };
        if !is_primary_node(&node) {
            return Ok(CoreRetireOutcome::UnsupportedNode(node));
        }
        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        let _ = session_runtime.kill_session(&tmux_session)?;
        // A terminal write and its rotation finalization share this one atomic
        // state replacement, so recovery cannot later relaunch the seat.
        finalize_active_credential_rotations_for_terminal_session(&mut state, session_id)?;
        let now = now_rfc3339();
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let session = session_object_mut(sessions, session_id)
            .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during retire"))?;
        session.insert("status".to_owned(), Value::String("stopped".to_owned()));
        mark_session_retired(session, &now);
        session.insert("stopped_at".to_owned(), Value::String(now.clone()));
        session.insert("last_activity".to_owned(), Value::String(now));
        complete_stop_notify_after_stop_raw(
            self,
            &mut state,
            Some(runtime),
            session_id,
            &recipient_name,
        )?;
        self.write_raw_json_value(&state)?;
        Ok(CoreRetireOutcome::Retired(CoreRetireResult {
            ok: true,
            session_id: session_id.to_owned(),
            status: "retired".to_owned(),
        }))
    }

    pub fn set_agent_status(
        &self,
        session_id: &str,
        request: AgentStatusRequest,
    ) -> Result<Option<AgentStatusResult>> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(None);
        };
        let now = now_rfc3339();
        match request.text {
            Some(text) => {
                session.insert("agent_status_text".to_owned(), Value::String(text.clone()));
                session.insert("agent_status_at".to_owned(), Value::String(now.clone()));
                session.insert("last_activity".to_owned(), Value::String(now));
                self.write_raw_json_value(&state)?;
                Ok(Some(AgentStatusResult {
                    status: "updated".to_owned(),
                    session_id: session_id.to_owned(),
                    agent_status_text: Some(text),
                }))
            }
            None => {
                session.insert("agent_status_text".to_owned(), Value::Null);
                session.insert("agent_status_at".to_owned(), Value::Null);
                session.insert("last_activity".to_owned(), Value::String(now));
                self.write_raw_json_value(&state)?;
                Ok(Some(AgentStatusResult {
                    status: "updated".to_owned(),
                    session_id: session_id.to_owned(),
                    agent_status_text: None,
                }))
            }
        }
    }

    pub fn task_complete(
        &self,
        session_id: &str,
        request: TaskCompleteRequest,
        runtime: Option<&TmuxRuntime>,
    ) -> Result<TaskCompleteOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(TaskCompleteOutcome::Error(
                "sm task-complete is self-directed only — requester must equal target session"
                    .to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let Some(session) = sessions.iter().find(|session| session.id == session_id) else {
            return Ok(TaskCompleteOutcome::Error(format!(
                "Session {session_id} not found"
            )));
        };

        let completed_at = now_rfc3339();
        let friendly = session
            .cached_display_name()
            .unwrap_or_else(|| non_empty_or(session.name.clone(), &session.id));
        let completion_text =
            format!("[sm task-complete] agent {session_id}({friendly}) completed its task.");
        if let Some(request_id) = active_reparent_route_request_for_session(&state, session_id)? {
            if let Some(queue) = &self.queue_store {
                queue.cancel_remind(session_id)?;
            }
            deactivate_remind_raw(&mut state, session_id)?;
            let sessions = ensure_sessions_array_mut(&mut state)?;
            let session_object = session_object_mut(sessions, session_id)
                .ok_or_else(|| anyhow::anyhow!("session disappeared during task-complete"))?;
            session_object.insert(
                "agent_task_completed_at".to_owned(),
                Value::String(completed_at.clone()),
            );
            session_object.insert("agent_status_text".to_owned(), Value::Null);
            session_object.insert("agent_status_at".to_owned(), Value::Null);

            let mut records = reparent_request_records(&state)?;
            let record = records
                .iter_mut()
                .find(|record| record.id == request_id)
                .with_context(|| format!("reparent request {request_id} disappeared"))?;
            let key = format!("task-complete:{session_id}:{completed_at}");
            if !record
                .deferred_routing_intents
                .iter()
                .any(|intent| intent.key == key)
            {
                record
                    .deferred_routing_intents
                    .push(ReparentDeferredRoutingIntent {
                        key,
                        operation: "task_complete".to_owned(),
                        child_session_id: session_id.to_owned(),
                        payload: json!({
                            "text": completion_text,
                            "completed_at": completed_at,
                        }),
                        created_at: now_rfc3339(),
                        replayed_at: None,
                        resolved_parent_session_id: None,
                    });
            }
            store_reparent_request_records(&mut state, &records)?;
            self.write_raw_json_value(&state)?;
            return Ok(TaskCompleteOutcome::Completed(TaskCompleteResult {
                status: "completed".to_owned(),
                session_id: session_id.to_owned(),
                em_notified: false,
                agent_task_completed_at: completed_at,
            }));
        }

        let queue_parent = match &self.queue_store {
            Some(queue) => queue.active_parent_wake_parent(session_id)?,
            None => None,
        };
        let em_session_id = queue_parent
            .or(active_parent_wake_parent_raw(&state, session_id)?)
            .or_else(|| session.parent_session_id.clone());
        if let Some(queue) = &self.queue_store {
            queue.cancel_remind(session_id)?;
            queue.cancel_parent_wake(session_id)?;
        }
        deactivate_remind_raw(&mut state, session_id)?;
        deactivate_parent_wake_raw(&mut state, session_id)?;

        let sessions = ensure_sessions_array_mut(&mut state)?;
        let session_object = session_object_mut(sessions, session_id)
            .ok_or_else(|| anyhow::anyhow!("session disappeared during task-complete"))?;
        session_object.insert(
            "agent_task_completed_at".to_owned(),
            Value::String(completed_at.clone()),
        );
        session_object.insert("agent_status_text".to_owned(), Value::Null);
        session_object.insert("agent_status_at".to_owned(), Value::Null);

        let mut em_notified = false;
        if let Some(em_session_id) = em_session_id {
            let text = completion_text;
            if let Some(queue) = &self.queue_store {
                let message_id = queue.enqueue_message(
                    &em_session_id,
                    &text,
                    "important",
                    Some("task_complete"),
                )?;
                if let Some(runtime) = runtime {
                    if let Some(parent_session) = raw_session_object(&state, &em_session_id) {
                        let parent_node =
                            json_text(parent_session.get("node")).unwrap_or_else(default_node);
                        if is_primary_node(&parent_node) {
                            let drain = drain_pending_runtime_messages_raw(
                                self,
                                &mut state,
                                &em_session_id,
                                runtime,
                                queue,
                                Some("important"),
                                None,
                                Some(&message_id),
                                false,
                            )?;
                            if drain
                                .delivered_message_ids
                                .iter()
                                .any(|delivered_id| delivered_id == &message_id)
                            {
                                clear_agent_task_completed_raw(&mut state, &em_session_id)?;
                            }
                        }
                    }
                }
            }
            push_retained_message_raw(
                &mut state,
                &em_session_id,
                &text,
                "important",
                Some("task_complete"),
            )?;
            em_notified = true;
        }

        self.write_raw_json_value(&state)?;
        Ok(TaskCompleteOutcome::Completed(TaskCompleteResult {
            status: "completed".to_owned(),
            session_id: session_id.to_owned(),
            em_notified,
            agent_task_completed_at: completed_at,
        }))
    }

    pub fn turn_complete(
        &self,
        session_id: &str,
        request: TaskCompleteRequest,
    ) -> Result<TurnCompleteOutcome> {
        if request.requester_session_id.trim() != session_id {
            return Ok(TurnCompleteOutcome::Error(
                "sm turn-complete is self-directed only — requester must equal target session"
                    .to_owned(),
            ));
        }

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        if !sessions.iter().any(|session| session.id == session_id) {
            return Ok(TurnCompleteOutcome::Error(format!(
                "Session {session_id} not found"
            )));
        }

        if let Some(queue) = &self.queue_store {
            queue.cancel_remind(session_id)?;
        }
        deactivate_remind_raw(&mut state, session_id)?;
        self.write_raw_json_value(&state)?;
        Ok(TurnCompleteOutcome::Completed(TurnCompleteResult {
            status: "turn_completed".to_owned(),
            session_id: session_id.to_owned(),
        }))
    }

    pub fn arm_stop_notify(
        &self,
        session_id: &str,
        request: ArmStopNotifyRequest,
    ) -> Result<ArmStopNotifyOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = snapshot_from_raw_value(&state)?.into_sessions();
        let Some(target) = sessions.iter().find(|session| session.id == session_id) else {
            return Ok(ArmStopNotifyOutcome::NotFound);
        };

        let requester = sessions
            .iter()
            .find(|session| session.id == request.requester_session_id);
        if !requester.is_some_and(|session| session.is_em) {
            return Ok(ArmStopNotifyOutcome::Forbidden(
                "Only EM sessions (is_em=True) may arm stop notifications".to_owned(),
            ));
        }

        if target.parent_session_id.as_deref() != Some(request.requester_session_id.as_str()) {
            return Ok(ArmStopNotifyOutcome::Forbidden(
                "Cannot arm stop notify — not the parent of target session".to_owned(),
            ));
        }

        let Some(sender) = sessions
            .iter()
            .find(|session| session.id == request.sender_session_id)
        else {
            return Ok(ArmStopNotifyOutcome::UnknownSender(
                request.sender_session_id,
            ));
        };

        if target.provider == "codex-fork" {
            return Ok(ArmStopNotifyOutcome::Suppressed(ArmStopNotifyResult {
                status: "suppressed".to_owned(),
                session_id: session_id.to_owned(),
                sender_session_id: request.sender_session_id,
                reason: Some("notify_on_stop disabled for codex-fork sessions".to_owned()),
            }));
        }

        let sender_name = sender
            .cached_display_name()
            .unwrap_or_else(|| non_empty_or(sender.name.clone(), &sender.id));
        if let Some(queue) = &self.queue_store {
            queue.upsert_stop_notify(
                session_id,
                &request.sender_session_id,
                &sender_name,
                request.delay_seconds.max(0),
            )?;
        }
        upsert_stop_notify_raw(
            &mut state,
            session_id,
            &request.sender_session_id,
            &sender_name,
            request.delay_seconds.max(0),
        )?;
        self.write_raw_json_value(&state)?;
        Ok(ArmStopNotifyOutcome::Armed(ArmStopNotifyResult {
            status: "ok".to_owned(),
            session_id: session_id.to_owned(),
            sender_session_id: request.sender_session_id,
            reason: None,
        }))
    }

    pub fn register_subagent_start(
        &self,
        session_id: &str,
        request: SubagentStartRequest,
    ) -> Result<SubagentStartOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let now = now_python_naive_iso();
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(SubagentStartOutcome::NotFound);
        };
        let subagent = json!({
            "agent_id": request.agent_id,
            "agent_type": request.agent_type,
            "parent_session_id": session_id,
            "transcript_path": request.transcript_path,
            "started_at": now,
            "stopped_at": null,
            "status": "running",
            "summary": null
        });
        let response = subagent_response_from_value(&subagent)?;
        ensure_subagents_array_mut(session).push(subagent);
        self.write_raw_json_value(&state)?;
        Ok(SubagentStartOutcome::Registered(response))
    }

    pub fn register_subagent_stop(
        &self,
        session_id: &str,
        agent_id: &str,
        request: SubagentStopRequest,
    ) -> Result<SubagentStopOutcome> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let now = now_python_naive_iso();
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(SubagentStopOutcome::SessionNotFound);
        };
        let subagents = ensure_subagents_array_mut(session);
        let Some(subagent) = subagents
            .iter_mut()
            .find(|subagent| subagent.get("agent_id").and_then(Value::as_str) == Some(agent_id))
        else {
            return Ok(SubagentStopOutcome::SubagentNotFound(agent_id.to_owned()));
        };
        if let Some(subagent) = subagent.as_object_mut() {
            subagent.insert("stopped_at".to_owned(), Value::String(now));
            subagent.insert("status".to_owned(), Value::String("completed".to_owned()));
            if let Some(transcript_path) = request.transcript_path {
                subagent.insert("transcript_path".to_owned(), Value::String(transcript_path));
            }
            if let Some(summary) = request.summary {
                subagent.insert("summary".to_owned(), Value::String(summary));
            }
        }
        let summary = subagent
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.write_raw_json_value(&state)?;
        Ok(SubagentStopOutcome::Stopped(SubagentStopResult {
            session_id: session_id.to_owned(),
            agent_id: agent_id.to_owned(),
            status: "stopped".to_owned(),
            summary,
        }))
    }

    pub fn list_subagents(&self, session_id: &str) -> Result<Option<SubagentListResponse>> {
        let state = self.load_raw_json_value()?;
        let Some(session) = raw_session_object(&state, session_id) else {
            return Ok(None);
        };
        let subagents = session
            .get("subagents")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(subagent_response_from_value)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Some(SubagentListResponse {
            session_id: session_id.to_owned(),
            subagents,
        }))
    }

    fn load_snapshot(&self) -> Result<StateSnapshot> {
        let state_file = self.readable_state_file();
        if !state_file.exists() {
            return Ok(StateSnapshot::default());
        }
        match read_snapshot(&state_file) {
            Ok(snapshot) => Ok(snapshot),
            Err(primary_error) => {
                if state_file == self.state_file {
                    if let Some(legacy_state_file) = &self.legacy_state_file {
                        if legacy_state_file.exists() {
                            return read_snapshot(legacy_state_file).with_context(|| {
                                format!(
                                    "failed to read fallback session state {} after primary failed: {primary_error:#}",
                                    legacy_state_file.display()
                                )
                            });
                        }
                    }
                }
                Err(primary_error)
            }
        }
    }

    fn readable_state_file(&self) -> PathBuf {
        if !self.state_file.exists() {
            if let Some(legacy_state_file) = &self.legacy_state_file {
                if legacy_state_file.exists() {
                    return legacy_state_file.clone();
                }
            }
        }
        self.state_file.clone()
    }

    fn load_raw_json_value(&self) -> Result<Value> {
        let state_file = self.readable_state_file();
        if !state_file.exists() {
            return Ok(json!({ "sessions": [] }));
        }
        let content = fs::read_to_string(&state_file)
            .with_context(|| format!("failed to read session state {}", state_file.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse session state {}", state_file.display()))
    }

    fn write_raw_json_value(&self, value: &Value) -> Result<()> {
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create state directory {}", parent.display())
            })?;
        }
        let tmp = self.state_file.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            STATE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&tmp, serde_json::to_vec_pretty(value)?)
            .with_context(|| format!("failed to write temp state {}", tmp.display()))?;
        fs::rename(&tmp, &self.state_file).with_context(|| {
            format!(
                "failed to atomically replace session state {}",
                self.state_file.display()
            )
        })?;
        Ok(())
    }

    fn write_guard(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.write_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("session state write lock poisoned"))
    }

    /// Lock the complete reparent transaction across SessionStore instances.
    /// The lock is intentionally separate from the atomically-replaced JSON
    /// state file: replacing that file would otherwise drop an advisory lock
    /// held on the old inode while an apply is still in progress.
    fn reparent_apply_file_lock(&self) -> Result<Flock<fs::File>> {
        let lock_path = self.state_file.with_extension("reparent-apply.lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create reparent lock directory {}",
                    parent.display()
                )
            })?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open reparent lock {}", lock_path.display()))?;
        Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, error)| {
            anyhow::anyhow!("failed to lock {}: {error}", lock_path.display())
        })
    }

    fn start_codex_fork_event_monitor(
        &self,
        session_id: String,
        event_stream_path: PathBuf,
    ) -> Result<()> {
        self.start_codex_fork_event_monitor_at_offset(session_id, event_stream_path, 0)
    }

    fn start_codex_fork_event_monitor_from_current_end(
        &self,
        session_id: String,
        event_stream_path: PathBuf,
    ) -> Result<()> {
        let initial_offset = fs::metadata(&event_stream_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        self.start_codex_fork_event_monitor_at_offset(session_id, event_stream_path, initial_offset)
    }

    fn start_codex_fork_event_monitor_at_offset(
        &self,
        session_id: String,
        event_stream_path: PathBuf,
        initial_offset: u64,
    ) -> Result<()> {
        let store = self.clone();
        let thread_session_id = format!(
            "{}-{}",
            sanitize_path_component(&session_id),
            stable_session_id_hash(&session_id)
        );
        thread::Builder::new()
            .name(format!("sm-codex-fork-events-{thread_session_id}"))
            .spawn(move || {
                store.monitor_codex_fork_event_stream(session_id, event_stream_path, initial_offset)
            })
            .with_context(|| "failed to start codex-fork event monitor")?;
        Ok(())
    }

    fn monitor_codex_fork_event_stream(
        &self,
        session_id: String,
        event_stream_path: PathBuf,
        initial_offset: u64,
    ) {
        let mut offset = initial_offset;
        let mut buffer = String::new();
        loop {
            match self.codex_fork_monitor_should_continue(&session_id) {
                Ok(true) => {}
                Ok(false) | Err(_) => return,
            }

            if let Ok(chunk) = read_file_from_offset(&event_stream_path, &mut offset) {
                for line in split_complete_event_lines(&mut buffer, &chunk) {
                    let _ = self.apply_codex_fork_event_line_with_artifact(
                        &session_id,
                        &line,
                        Some(&event_stream_path),
                    );
                }
            }
            thread::sleep(CODEX_FORK_EVENT_MONITOR_POLL);
        }
    }

    fn codex_fork_monitor_should_continue(&self, session_id: &str) -> Result<bool> {
        let state = self.load_raw_json_value()?;
        let Some(sessions) = state.get("sessions").and_then(Value::as_array) else {
            return Ok(false);
        };
        let Some(session) = sessions.iter().find(|session| {
            session.get("id").and_then(Value::as_str) == Some(session_id)
                || session
                    .get("aliases")
                    .and_then(Value::as_array)
                    .is_some_and(|aliases| {
                        aliases
                            .iter()
                            .any(|alias| alias.as_str() == Some(session_id))
                    })
        }) else {
            return Ok(false);
        };
        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        if provider != "codex-fork" {
            return Ok(false);
        }
        Ok(!session.as_object().is_some_and(raw_session_is_stopped))
    }

    #[cfg(test)]
    fn apply_codex_fork_event_line(&self, session_id: &str, line: &str) -> Result<()> {
        self.apply_codex_fork_event_line_with_artifact(session_id, line, None)
    }

    fn apply_codex_fork_event_line_with_artifact(
        &self,
        session_id: &str,
        line: &str,
        artifact_path: Option<&Path>,
    ) -> Result<()> {
        let raw = line.trim();
        if raw.is_empty() {
            return Ok(());
        }
        let Ok(event) = serde_json::from_str::<Value>(raw) else {
            return Ok(());
        };
        let Some(event) = event.as_object() else {
            return Ok(());
        };

        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let sessions = ensure_sessions_array_mut(&mut state)?;
        let Some(session) = session_object_mut(sessions, session_id) else {
            return Ok(());
        };
        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        if provider != "codex-fork" {
            return Ok(());
        }
        let status = json_text(session.get("status")).unwrap_or_else(|| "running".to_owned());
        if raw_session_is_stopped(session) {
            return Ok(());
        }

        let mut changed = false;
        let mut terminal_provider_event = false;
        if let Some(event_type) = codex_fork_event_type(event)
            .map(|value| normalize_codex_fork_event_type(&value.replace('/', "_")))
        {
            match event_type.as_str() {
                "control_socket_degraded" => {
                    let reason = event
                        .get("payload")
                        .and_then(Value::as_object)
                        .and_then(|payload| json_text(payload.get("reason")))
                        .unwrap_or_else(|| "control socket supervisor is recovering".to_owned());
                    mark_codex_fork_control_degraded_raw(session, &reason);
                    changed = true;
                }
                "control_socket_started" | "control_socket_restarted" => {
                    changed |= clear_codex_fork_control_degraded_raw(session);
                }
                _ => {}
            }
        }
        // The stream multiplexes root and descendant threads. Only root lifecycle events may
        // change what `sm restore` resumes, while every observed thread still belongs in the
        // attribution ledger.
        let provider_resume_id = codex_fork_provider_resume_id(event);
        let observed_provider_session_id = codex_fork_observed_provider_session_id(event);
        let provider_resume_id_changed = provider_resume_id.as_deref().is_some_and(|value| {
            json_text(session.get("provider_resume_id")).as_deref() != Some(value)
        });
        if let Some(provider_resume_id) = provider_resume_id.as_deref() {
            if provider_resume_id_changed {
                session.insert(
                    "provider_resume_id".to_owned(),
                    Value::String(provider_resume_id.to_owned()),
                );
                changed = true;
            }
        }

        let root_provider_resume_id = json_text(session.get("provider_resume_id"));
        if let Some(next_status) = codex_fork_status_for_event(event).filter(|next_status| {
            codex_fork_event_matches_root_thread(event, root_provider_resume_id.as_deref())
                && (status != "idle"
                    || *next_status != "running"
                    || codex_fork_event_starts_turn(event))
        }) {
            if status != next_status {
                session.insert("status".to_owned(), Value::String(next_status.to_owned()));
            }
            let now = now_rfc3339();
            session.insert("last_activity".to_owned(), Value::String(now.clone()));
            if next_status == "stopped" {
                session.insert("stopped_at".to_owned(), Value::String(now));
                terminal_provider_event = true;
            } else {
                session.insert("stopped_at".to_owned(), Value::Null);
            }
            changed = true;
        }

        // Codex reports token usage on its own event rather than through a
        // status line, so this is the codex-fork equivalent of the Claude
        // `/hooks/context-usage` usage update. The snapshot is always cached;
        // provider capability and registration only control warning/critical alerts.
        let mut context_alert = None;
        if codex_fork_event_matches_root_thread(event, root_provider_resume_id.as_deref()) {
            if let Some(usage) = codex_fork_context_usage(event) {
                let previous_used = session.get("tokens_used").and_then(Value::as_i64);
                let previous_pct = session
                    .get("context_used_percentage")
                    .and_then(Value::as_f64);
                let previous_context_tokens = session
                    .get("context_total_input_tokens")
                    .and_then(Value::as_i64);
                // Codex has no Claude-style PreCompact hook. Its measured resident
                // input drops when native compaction replaces the active window,
                // which begins the next durable alert cycle.
                if previous_context_tokens
                    .is_some_and(|previous_tokens| usage.tokens_used < previous_tokens)
                {
                    reset_context_oneshot_flags(session);
                    self.cancel_context_monitor_alerts(session_id)?;
                }
                let snapshot_changed = previous_used != Some(usage.tokens_used)
                    || previous_pct != Some(usage.used_percentage)
                    || previous_context_tokens != Some(usage.tokens_used)
                    || json_text(session.get("context_sampled_at")).is_none();
                if snapshot_changed {
                    session.insert("tokens_used".to_owned(), json!(usage.tokens_used));
                    session.insert(
                        "context_used_percentage".to_owned(),
                        json!(usage.used_percentage),
                    );
                    session.insert(
                        "context_total_input_tokens".to_owned(),
                        json!(usage.tokens_used),
                    );
                    session.insert(
                        "context_sampled_at".to_owned(),
                        Value::String(now_rfc3339()),
                    );
                    changed = true;
                }
                let unsupported_provider = !provider_has_measured_context_gauge(
                    json_text(session.get("provider"))
                        .as_deref()
                        .unwrap_or("claude"),
                );
                if unsupported_provider {
                    let had_alert_state = flag_is_set(session, "context_monitor_enabled")
                        || json_text(session.get("context_monitor_notify")).is_some()
                        || flag_is_set(session, "context_warning_sent")
                        || flag_is_set(session, "context_critical_sent");
                    if had_alert_state {
                        session.insert("context_monitor_enabled".to_owned(), Value::Bool(false));
                        session.insert("context_monitor_notify".to_owned(), Value::Null);
                        session.insert(
                            "context_monitor_notify_source".to_owned(),
                            Value::String(default_context_monitor_notify_source()),
                        );
                        reset_context_oneshot_flags(session);
                        self.cancel_context_monitor_alerts(session_id)?;
                        changed = true;
                    }
                } else if flag_is_set(session, "context_monitor_enabled") {
                    context_alert = self.latch_context_alert(
                        session,
                        session_id,
                        usage.used_percentage,
                        usage.tokens_used,
                    );
                }
            }
        }

        if let Some(alert) = context_alert {
            // Nothing drains the queue on a timer, and this thread has no
            // request behind it, so an alert queued without a runtime would sit
            // until some unrelated operation flushed the recipient — quite
            // possibly after the compaction it was warning about.
            let runtime = self.delivery_runtime.clone();
            self.queue_context_monitor_message(
                &mut state,
                session_id,
                &alert.notify_target,
                &alert.text,
                alert.delivery_mode,
                runtime.as_ref(),
            )?;
            changed = true;
        }

        if terminal_provider_event {
            // The provider is an authoritative terminal source.  Keep the
            // rotation/launch finalization in the same durable write as this
            // status transition so startup recovery cannot relaunch it.
            finalize_active_credential_rotations_for_terminal_session(&mut state, session_id)?;
        }

        if changed {
            self.write_raw_json_value(&state)?;
        }
        drop(_guard);
        if let Some(provider_resume_id) = observed_provider_session_id {
            self.append_seat_session(
                session_id,
                &provider,
                &provider_resume_id,
                artifact_path.and_then(Path::to_str),
            );
        }
        let observed_at = OffsetDateTime::now_utc();
        if let Some(store) = self.usage_burn_store.as_ref() {
            if let Err(error) = store.record_codex_event(event, observed_at) {
                eprintln!("Codex rate-limit ingest failed for {session_id}: {error:#}");
            }
        }
        if let Err(error) =
            self.record_session_account_key(session_id, UsageProvider::Codex, observed_at)
        {
            eprintln!("Codex account attribution update failed for {session_id}: {error:#}");
        }
        Ok(())
    }

    fn build_core_session_record(
        &self,
        sessions: &[Value],
        request: &CreateCoreSessionRequest,
        log_dir: Option<&Path>,
        runtime_backed: bool,
        tmux_socket_name: Option<&str>,
    ) -> Result<SessionRecord> {
        let requested_session_id = request
            .id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let session_id = match requested_session_id {
            Some(session_id) => session_id.to_owned(),
            None => generate_unique_session_id(sessions)?,
        };
        if session_id_exists(sessions, &session_id) {
            anyhow::bail!("session already exists: {session_id}");
        }
        let parent_session = request.parent_session_id.as_deref().and_then(|parent_id| {
            sessions
                .iter()
                .find(|value| value.get("id").and_then(Value::as_str) == Some(parent_id))
        });
        let parent_node = parent_session.and_then(|value| json_text(value.get("node")));
        let (provider, working_dir) = core_session_provider_and_working_dir(sessions, request);
        let name = request
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{provider}-{session_id}"));
        let node = request
            .node
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(parent_node.as_deref())
            .unwrap_or("primary")
            .to_owned();
        let now = now_rfc3339();
        let log_file = core_log_file_path(&self.state_file, log_dir, &session_id);
        Ok(SessionRecord {
            id: session_id.clone(),
            name,
            working_dir,
            tmux_session: if runtime_backed {
                core_tmux_session_name(&provider, &session_id)
            } else {
                format!("sm-rust-{session_id}")
            },
            tmux_socket_name: tmux_socket_name.map(ToOwned::to_owned),
            node,
            provider,
            model: optional_trimmed(request.model.as_deref()),
            reasoning_effort: optional_trimmed(request.reasoning_effort.as_deref()),
            account_key: None,
            usage_cap_fraction: None,
            log_file: Some(log_file.display().to_string()),
            provider_resume_id: None,
            transcript_path: None,
            codex_thread_id: None,
            forked_from_session_id: None,
            forked_from_provider_resume_id: None,
            forked_provider_resume_id: None,
            forked_at: None,
            forked_by_session_id: None,
            friendly_name: request.name.clone(),
            friendly_name_is_explicit: true,
            friendly_name_updated_at_ns: None,
            native_title: None,
            native_title_updated_at_ns: None,
            native_title_source_mtime_ns: None,
            telegram_chat_id: None,
            telegram_thread_id: None,
            telegram_topic_id: None,
            telegram_root_msg_id: None,
            current_task: None,
            git_remote_url: None,
            review_config: None,
            parent_session_id: request.parent_session_id.clone(),
            session_credential_sha256: None,
            last_handoff_path: None,
            agent_status_text: None,
            agent_status_at: None,
            agent_task_completed_at: None,
            completion_status: None,
            completion_message: None,
            completed_at: None,
            stopped_at: None,
            is_em: false,
            role: None,
            status: "running".to_owned(),
            spawned_at: Some(now.clone()),
            created_at: now.clone(),
            last_activity: now,
            activity_hook_at: None,
            activity_turn_start_hook_at: None,
            last_tool_call: None,
            last_tool_name: None,
            tokens_used: 0,
            context_used_percentage: None,
            context_total_input_tokens: None,
            context_sampled_at: None,
            context_compaction_active: false,
            context_monitor_enabled: false,
            context_monitor_notify: None,
            context_monitor_notify_source: default_context_monitor_notify_source(),
            context_monitor_threshold_percentages: None,
            context_monitor_warning_percentage: None,
            context_monitor_critical_percentage: None,
            context_warning_sent: false,
            context_critical_sent: false,
            aliases: Vec::new(),
            pending_adoption_proposals: Vec::new(),
        })
    }
}

fn wait_for_codex_fork_provider_resume_id_for_launch(
    event_stream_path: &Path,
    timeout: Duration,
    runtime: &TmuxRuntime,
    tmux_session: &str,
) -> Result<String> {
    wait_for_codex_fork_provider_resume_id_after_offset_with_startup(
        event_stream_path,
        0,
        timeout,
        || runtime.accept_codex_directory_trust_prompt(tmux_session),
    )
}

fn usage_ledger_error_is_transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|error| {
                matches!(
                    error,
                    rusqlite::Error::SqliteFailure(code, _)
                        if matches!(
                            code.code,
                            rusqlite::ErrorCode::DatabaseBusy
                                | rusqlite::ErrorCode::DatabaseLocked
                        )
                )
            })
    })
}

fn wait_for_codex_fork_provider_resume_id_after_offset(
    event_stream_path: &Path,
    initial_offset: u64,
    timeout: Duration,
) -> Result<String> {
    wait_for_codex_fork_provider_resume_id_after_offset_with_startup(
        event_stream_path,
        initial_offset,
        timeout,
        || Ok(false),
    )
}

fn wait_for_codex_fork_provider_resume_id_after_offset_with_startup<F>(
    event_stream_path: &Path,
    initial_offset: u64,
    timeout: Duration,
    mut handle_startup_prompt: F,
) -> Result<String>
where
    F: FnMut() -> Result<bool>,
{
    let started = Instant::now();
    let mut offset = initial_offset;
    let mut buffer = String::new();
    let mut directory_trust_accepted = false;
    loop {
        if let Ok(chunk) = read_file_from_offset(event_stream_path, &mut offset) {
            for line in split_complete_event_lines(&mut buffer, &chunk) {
                let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                let Some(event) = event.as_object() else {
                    continue;
                };
                if let Some(provider_resume_id) = extract_codex_fork_thread_started(event) {
                    return Ok(provider_resume_id);
                }
            }
        }
        if !directory_trust_accepted {
            directory_trust_accepted = handle_startup_prompt()?;
        }
        if started.elapsed() >= timeout {
            if directory_trust_accepted {
                anyhow::bail!(
                    "accepted the codex-fork directory trust prompt but timed out waiting for a new thread_started event in {}",
                    event_stream_path.display()
                );
            }
            anyhow::bail!(
                "timed out waiting for a new codex-fork thread_started event in {}",
                event_stream_path.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn read_file_from_offset(path: &Path, offset: &mut u64) -> Result<String> {
    let mut file = match fs::OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to open {}", path.display()))
        }
    };
    let len = file.metadata()?.len();
    if *offset > len {
        *offset = 0;
    }
    file.seek(SeekFrom::Start(*offset))?;
    let mut chunk = String::new();
    file.read_to_string(&mut chunk)?;
    *offset = file.stream_position()?;
    Ok(chunk)
}

fn split_complete_event_lines(buffer: &mut String, chunk: &str) -> Vec<String> {
    if chunk.is_empty() {
        return Vec::new();
    }
    buffer.push_str(chunk);
    let mut lines = Vec::new();
    while let Some(index) = buffer.find('\n') {
        let line = buffer[..index].to_owned();
        buffer.drain(..=index);
        lines.push(line);
    }
    lines
}

fn codex_fork_provider_resume_id(event: &Map<String, Value>) -> Option<String> {
    let event_type =
        normalize_codex_fork_event_type(&codex_fork_event_type(event)?.replace('/', "_"));
    match event_type.as_str() {
        "thread_started" => extract_codex_fork_thread_started(event),
        "session_configured" => codex_fork_payload(event)
            .and_then(|payload| payload.get("session_id"))
            .and_then(non_unknown_json_text)
            .or_else(|| event.get("session_id").and_then(non_unknown_json_text)),
        _ => None,
    }
}

fn codex_fork_observed_provider_session_id(event: &Map<String, Value>) -> Option<String> {
    event
        .get("session_id")
        .and_then(non_unknown_json_text)
        .or_else(|| extract_any_codex_fork_thread_started(event))
        .or_else(|| {
            codex_fork_payload(event)
                .and_then(|payload| payload.get("session_id"))
                .and_then(non_unknown_json_text)
        })
}

fn extract_codex_fork_thread_started(event: &Map<String, Value>) -> Option<String> {
    let thread_payload = codex_fork_thread_started_payload(event)?;
    let parent_thread_id = thread_payload
        .get("parentThreadId")
        .or_else(|| thread_payload.get("parent_thread_id"))
        .and_then(non_unknown_json_text);
    let thread_source = thread_payload
        .get("threadSource")
        .or_else(|| thread_payload.get("thread_source"))
        .and_then(Value::as_str)
        .map(str::trim);
    if parent_thread_id.is_some() || thread_source == Some("subagent") {
        return None;
    }
    codex_fork_thread_id(thread_payload)
}

fn extract_any_codex_fork_thread_started(event: &Map<String, Value>) -> Option<String> {
    codex_fork_thread_id(codex_fork_thread_started_payload(event)?)
}

fn codex_fork_thread_started_payload(event: &Map<String, Value>) -> Option<&Map<String, Value>> {
    let raw_event_type = codex_fork_event_type(event)?;
    let normalized_event_type = normalize_codex_fork_event_type(&raw_event_type.replace('/', "_"));
    if raw_event_type != "thread/started"
        && raw_event_type != "thread_started"
        && normalized_event_type != "thread_started"
    {
        return None;
    }
    let payload = codex_fork_payload(event)?;
    Some(
        payload
            .get("thread")
            .and_then(Value::as_object)
            .unwrap_or(payload),
    )
}

fn codex_fork_thread_id(thread_payload: &Map<String, Value>) -> Option<String> {
    thread_payload
        .get("id")
        .and_then(non_unknown_json_text)
        .or_else(|| {
            thread_payload
                .get("thread_id")
                .and_then(non_unknown_json_text)
        })
        .or_else(|| {
            thread_payload
                .get("session_id")
                .and_then(non_unknown_json_text)
        })
}

pub(crate) fn codex_fork_status_for_event_line(line: &str) -> Option<&'static str> {
    let raw = line.trim();
    if raw.is_empty() {
        return None;
    }
    let event = serde_json::from_str::<Value>(raw).ok()?;
    let event = event.as_object()?;
    codex_fork_status_for_event(event)
}

pub(crate) fn codex_fork_event_line_starts_turn(line: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
        return false;
    };
    event.as_object().is_some_and(codex_fork_event_starts_turn)
}

pub(crate) fn codex_fork_event_line_matches_root_thread(
    line: &str,
    root_thread_id: Option<&str>,
) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
        return false;
    };
    event
        .as_object()
        .is_some_and(|event| codex_fork_event_matches_root_thread(event, root_thread_id))
}

fn codex_fork_event_is_turn_complete(line: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
        return false;
    };
    let Some(event) = event.as_object() else {
        return false;
    };
    codex_fork_event_type(event)
        .map(|event_type| {
            normalize_codex_fork_event_type(&event_type.replace('/', "_")) == "turn_complete"
        })
        .unwrap_or(false)
}

fn codex_fork_status_for_event(event: &Map<String, Value>) -> Option<&'static str> {
    let event_type =
        normalize_codex_fork_event_type(&codex_fork_event_type(event)?.replace('/', "_"));
    match event_type.as_str() {
        "thread_status_changed" => codex_fork_thread_status(event),
        "turn_started" => Some("running"),
        "turn_complete" => Some("idle"),
        "turn_aborted" => {
            let reason = codex_fork_payload(event)
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            if reason.as_deref() == Some("interrupted") {
                Some("running")
            } else {
                Some("idle")
            }
        }
        "approval_request"
        | "user_input_request"
        | "approval_resolved"
        | "user_input_resolved"
        | "turn_delta"
        | "turn_diff"
        | "item_started"
        | "agent_message"
        | "exec_command_end" => Some("running"),
        "item_completed" => codex_fork_item_completed_status(event),
        "error" if codex_fork_error_will_retry(event) => Some("running"),
        // A non-retry error terminates the current turn, not the Codex process.
        // The same thread can accept another turn and publish later status events.
        "error" => Some("idle"),
        "shutdown" => Some("stopped"),
        "shutdown_complete" | "stream_error" | "thread_started" | "thread_name_updated" => None,
        other if other.ends_with("_begin") => Some("running"),
        _ => None,
    }
}

fn codex_fork_event_starts_turn(event: &Map<String, Value>) -> bool {
    let Some(event_type) = codex_fork_event_type(event)
        .map(|value| normalize_codex_fork_event_type(&value.replace('/', "_")))
    else {
        return false;
    };
    match event_type.as_str() {
        "turn_started" => true,
        "thread_status_changed" => codex_fork_thread_status(event) == Some("running"),
        _ => false,
    }
}

fn codex_fork_event_matches_root_thread(
    event: &Map<String, Value>,
    root_thread_id: Option<&str>,
) -> bool {
    let Some(root_thread_id) = root_thread_id else {
        return true;
    };
    codex_fork_event_thread_id(event)
        .as_deref()
        .is_none_or(|event_thread_id| event_thread_id == root_thread_id)
}

fn codex_fork_event_thread_id(event: &Map<String, Value>) -> Option<String> {
    let payload = codex_fork_payload(event);
    payload
        .and_then(|payload| {
            payload
                .get("threadId")
                .or_else(|| payload.get("thread_id"))
                .and_then(non_unknown_json_text)
        })
        .or_else(|| {
            payload
                .and_then(|payload| payload.get("thread"))
                .and_then(Value::as_object)
                .and_then(codex_fork_thread_id)
        })
        .or_else(|| {
            event
                .get("threadId")
                .or_else(|| event.get("thread_id"))
                .and_then(non_unknown_json_text)
        })
        .or_else(|| event.get("session_id").and_then(non_unknown_json_text))
}

struct CodexContextUsage {
    tokens_used: i64,
    used_percentage: f64,
}

/// Read context occupancy out of a `thread/tokenUsage/updated` event.
///
/// The event carries both a `total` (every token the thread has ever spent,
/// which runs far past the context window) and a `last` (the most recent
/// request). Only `last` describes what is currently resident in the window, so
/// that is what maps onto Claude's `context_window.total_input_tokens`.
fn codex_fork_context_usage(event: &Map<String, Value>) -> Option<CodexContextUsage> {
    let event_type =
        normalize_codex_fork_event_type(&codex_fork_event_type(event)?.replace('/', "_"));
    // `thread/tokenUsage/updated` normalizes to `thread_token_usage_updated`;
    // the unprefixed spelling is accepted too since other codex event names
    // appear both with and without the `thread` scope.
    if !matches!(
        event_type.as_str(),
        "thread_token_usage_updated" | "token_usage_updated"
    ) {
        return None;
    }
    let usage = codex_fork_payload(event)?
        .get("tokenUsage")
        .and_then(Value::as_object)?;
    let tokens_used = usage
        .get("last")
        .and_then(Value::as_object)
        .and_then(|last| last.get("totalTokens"))
        .and_then(Value::as_i64)?;
    let context_window = usage
        .get("modelContextWindow")
        .and_then(Value::as_i64)
        .filter(|window| *window > 0)?;
    Some(CodexContextUsage {
        tokens_used,
        used_percentage: (tokens_used as f64 / context_window as f64) * 100.0,
    })
}

/// Return the newest complete root-thread context gauge from a bounded event
/// stream tail. The first tail line may be partial, so malformed lines are
/// ignored rather than making restart recovery fail.
fn latest_codex_fork_context_usage_from_tail(
    event_stream_path: &Path,
    root_thread_id: Option<&str>,
    recovery_offset: u64,
) -> Option<CodexContextUsage> {
    read_tail_lines_at_offset(
        event_stream_path,
        recovery_offset,
        CODEX_FORK_CONTEXT_RECOVERY_TAIL_LINES,
    )
    .ok()?
    .lines()
    .rev()
    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    .filter_map(|event| event.as_object().cloned())
    .find_map(|event| {
        codex_fork_event_matches_root_thread(&event, root_thread_id)
            .then(|| codex_fork_context_usage(&event))
            .flatten()
    })
}

fn codex_fork_item_completed_status(event: &Map<String, Value>) -> Option<&'static str> {
    let payload = codex_fork_payload(event)?;
    let item = payload
        .get("item")
        .and_then(Value::as_object)
        .unwrap_or(payload);
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| item.get("kind").and_then(Value::as_str))
        .map(str::trim)
        .map(str::to_ascii_lowercase);

    match item_type.as_deref() {
        Some("agentmessage" | "agent_message" | "message") => Some("idle"),
        Some(_) => Some("running"),
        None => Some("running"),
    }
}

fn codex_fork_thread_status(event: &Map<String, Value>) -> Option<&'static str> {
    let payload = codex_fork_payload(event)?;
    let status = payload
        .get("status")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.as_object()?.get("type")?.as_str())
                .or_else(|| value.as_object()?.get("status")?.as_str())
        })?
        .trim()
        .to_ascii_lowercase();
    match status.as_str() {
        "active" | "running" | "working" => Some("running"),
        "idle" => Some("idle"),
        _ => None,
    }
}

fn codex_fork_error_will_retry(event: &Map<String, Value>) -> bool {
    event
        .get("willRetry")
        .or_else(|| event.get("will_retry"))
        .and_then(Value::as_bool)
        .or_else(|| {
            codex_fork_payload(event)
                .and_then(|payload| {
                    payload
                        .get("willRetry")
                        .or_else(|| payload.get("will_retry"))
                })
                .and_then(Value::as_bool)
        })
        .unwrap_or(false)
}

fn codex_fork_event_type(event: &Map<String, Value>) -> Option<String> {
    event
        .get("event_type")
        .or_else(|| event.get("type"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn codex_fork_payload(event: &Map<String, Value>) -> Option<&Map<String, Value>> {
    event.get("payload").and_then(Value::as_object)
}

fn non_unknown_json_text(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    if text.is_empty()
        || matches!(
            text.to_ascii_lowercase().as_str(),
            "unknown" | "none" | "null"
        )
    {
        return None;
    }
    Some(text.to_owned())
}

fn normalize_codex_fork_event_type(event_type: &str) -> String {
    let mut snake = String::new();
    let mut previous_is_separator = true;
    for ch in event_type.trim().chars() {
        if ch == '/' || ch == '-' || ch == ' ' {
            if !previous_is_separator {
                snake.push('_');
                previous_is_separator = true;
            }
            continue;
        }
        if ch.is_ascii_uppercase() {
            if !previous_is_separator && !snake.ends_with('_') {
                snake.push('_');
            }
            snake.push(ch.to_ascii_lowercase());
            previous_is_separator = false;
        } else {
            snake.push(ch);
            previous_is_separator = ch == '_';
        }
    }
    let normalized = snake.trim_matches('_');
    match normalized {
        "task_started" => "turn_started".to_owned(),
        "task_complete" | "turn_completed" => "turn_complete".to_owned(),
        "exec_approval_request" | "patch_approval_request" | "request_approval" => {
            "approval_request".to_owned()
        }
        "request_user_input" => "user_input_request".to_owned(),
        "approval_decision" | "approval_submitted" => "approval_resolved".to_owned(),
        "user_input_submitted" | "user_input_response" => "user_input_resolved".to_owned(),
        "runtime_error" | "fatal_error" => "error".to_owned(),
        _ => normalized.to_owned(),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateCoreSessionRequest {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default, alias = "prompt")]
    pub initial_message: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub wait: Option<u64>,
    #[serde(default)]
    pub spawn_prompt_source: Option<SpawnBriefSource>,
    #[serde(skip)]
    pub spawn_brief: Option<SpawnBriefBinding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartReviewRequest {
    #[serde(default = "default_review_mode")]
    pub mode: String,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub custom_prompt: Option<String>,
    #[serde(default, alias = "steer")]
    pub steer_text: Option<String>,
    #[serde(default)]
    pub wait: Option<u64>,
    #[serde(default)]
    pub watcher_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpawnReviewRequest {
    pub parent_session_id: String,
    #[serde(default = "default_review_mode")]
    pub mode: String,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub custom_prompt: Option<String>,
    #[serde(default, alias = "steer")]
    pub steer_text: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub wait: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

fn default_review_mode() -> String {
    "branch".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendCoreInputRequest {
    pub text: String,
    #[serde(default = "default_delivery_mode")]
    pub delivery_mode: String,
    #[serde(default)]
    pub sender_session_id: Option<String>,
    #[serde(default)]
    pub from_sm_send: bool,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub notify_on_delivery: bool,
    #[serde(default)]
    pub notify_after_seconds: Option<u64>,
    #[serde(default)]
    pub notify_on_stop: bool,
    #[serde(default)]
    pub remind_soft_threshold: Option<u64>,
    #[serde(default)]
    pub remind_hard_threshold: Option<u64>,
    #[serde(default)]
    pub remind_cancel_on_reply_session_id: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendCoreInputBatchRequest {
    #[serde(flatten)]
    pub input: SendCoreInputRequest,
    #[serde(default)]
    pub recipients: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentStatusRequest {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClearSessionRequest {
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub requester_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextMonitorRequest {
    pub enabled: bool,
    pub requester_session_id: String,
    #[serde(default)]
    pub notify_session_id: Option<String>,
    /// Ordered per-seat notification milestones. This is the policy-neutral
    /// interface; every reached percentage emits a factual context update.
    #[serde(default)]
    pub threshold_percentages: Option<Vec<f64>>,
    /// Per-seat warning threshold percentage. Absent values preserve an
    /// existing override or resolve to the configured global default.
    #[serde(default)]
    pub warning_percentage: Option<f64>,
    /// Per-seat critical threshold percentage. Absent values preserve an
    /// existing override or resolve to the configured global default.
    #[serde(default)]
    pub critical_percentage: Option<f64>,
    /// Clear both seat overrides and return to the configured global defaults.
    #[serde(default)]
    pub use_default_thresholds: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateReparentRequest {
    pub requester_session_id: String,
    pub target_parent_session_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateReparentTreeRequest {
    pub requester_session_id: String,
    pub target_session_id: String,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DecideReparentRequest {
    pub requester_session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReparentDecision {
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReparentRepairAction {
    Resume,
    RollbackPrecommit,
}

impl ReparentRepairAction {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "resume" => Some(Self::Resume),
            "rollback_precommit" => Some(Self::RollbackPrecommit),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::RollbackPrecommit => "rollback_precommit",
        }
    }
}

impl ReparentDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

/// One post from a context-monitor producer hook. `event` distinguishes the
/// lifecycle hooks (`compaction`, `compaction_complete`, `context_reset`) from a
/// plain status-line usage sample, which carries no `event` at all.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContextUsageEvent {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub used_percentage: Option<f64>,
    #[serde(default)]
    pub total_input_tokens: Option<i64>,
    #[serde(default)]
    pub five_hour_percent: Option<f64>,
    #[serde(default)]
    pub five_hour_resets_at: Option<String>,
    #[serde(default)]
    pub seven_day_percent: Option<f64>,
    #[serde(default)]
    pub seven_day_resets_at: Option<String>,
    /// Stamped by the producer hook before it detached its curl, so it describes
    /// when the sample was taken rather than when it arrived. Absent for hooks
    /// predating that change.
    #[serde(default)]
    pub emitted_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContextUsageOutcome {
    UnknownSession,
    CompactionLogged,
    CompactionCompleteLogged { last_handoff_path: Option<String> },
    FlagsReset,
    NotRegistered,
    NoUsage,
    StaleSample,
    Recorded { used_percentage: f64 },
}

impl ContextUsageOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            Self::UnknownSession => "unknown_session",
            Self::CompactionLogged => "compaction_logged",
            Self::CompactionCompleteLogged { .. } => "compaction_complete_logged",
            Self::FlagsReset => "flags_reset",
            Self::NotRegistered => "not_registered",
            Self::StaleSample => "stale_sample",
            Self::NoUsage | Self::Recorded { .. } => "ok",
        }
    }
}

struct ContextAlert {
    notify_target: String,
    text: String,
    delivery_mode: &'static str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HandoffRequest {
    pub requester_session_id: String,
    pub file_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetMaintainerRequest {
    pub requester_session_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoleRegistrationRequest {
    pub requester_session_id: String,
    pub role: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateSessionMetadataRequest {
    #[serde(default)]
    pub friendly_name: Option<String>,
    #[serde(default)]
    pub is_em: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskCompleteRequest {
    pub requester_session_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArmStopNotifyRequest {
    pub sender_session_id: String,
    pub requester_session_id: String,
    #[serde(default)]
    pub delay_seconds: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentStartRequest {
    pub agent_id: String,
    pub agent_type: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentStopRequest {
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRegistrationResponse {
    pub role: String,
    pub session_id: String,
    pub friendly_name: Option<String>,
    pub provider: Option<String>,
    pub status: String,
    pub activity_state: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreInputResult {
    pub ok: bool,
    pub session_id: String,
    pub delivered: bool,
    pub delivery_mode: String,
    pub notify_after_seconds: Option<u64>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreInputBatchResult {
    pub identifier: String,
    pub status: String,
    pub delivery_kind: String,
    pub session_id: Option<String>,
    pub target_name: Option<String>,
    pub provider: Option<String>,
    pub bootstrapped: bool,
    pub queue_position: Option<u64>,
    pub estimated_delivery: Option<String>,
    pub email_username: Option<String>,
    pub email_address: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreInputBatchResponse {
    pub ok: bool,
    pub requested_count: usize,
    pub success_count: usize,
    pub failure_count: usize,
    pub delivery_mode: String,
    pub results: Vec<CoreInputBatchResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreReviewResult {
    pub session_id: String,
    pub review_mode: String,
    pub base_branch: Option<String>,
    pub commit_sha: Option<String>,
    pub status: String,
    pub steer_queued: bool,
    #[serde(skip)]
    pub tmux_session: String,
    #[serde(skip)]
    pub tmux_socket_name: Option<String>,
    #[serde(skip)]
    pub steer_text: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CoreReviewOutcome {
    Started(CoreReviewResult),
    NotFound,
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentResponse {
    pub agent_id: String,
    pub agent_type: String,
    pub parent_session_id: String,
    pub started_at: String,
    pub stopped_at: Option<String>,
    pub status: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentStopResult {
    pub session_id: String,
    pub agent_id: String,
    pub status: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentListResponse {
    pub session_id: String,
    pub subagents: Vec<SubagentResponse>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreRetireResult {
    pub ok: bool,
    pub session_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub enum CoreRetireOutcome {
    Retired(CoreRetireResult),
    NotFound,
    NotChild,
    UnsupportedNode(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreClearResult {
    pub status: String,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub enum CoreClearOutcome {
    Cleared(CoreClearResult),
    NotFound,
    NotRunning,
    Unauthorized(String),
}

#[derive(Debug, Clone)]
pub enum CoreRestoreOutcome {
    Restored(SessionRecord),
    NotStopped,
    UnsupportedNode(String),
    UnsupportedProvider(String),
    MissingProviderResumeId(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentStatusResult {
    pub status: String,
    pub session_id: String,
    pub agent_status_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextMonitorStatus {
    pub session_id: String,
    pub friendly_name: Option<String>,
    pub notify_session_id: Option<String>,
    pub warning_percentage: Option<f64>,
    pub critical_percentage: Option<f64>,
    pub threshold_percentages: Option<Vec<f64>>,
    pub threshold_source: String,
    pub enforced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextSnapshotResponse {
    pub session_id: String,
    pub friendly_name: Option<String>,
    pub provider: Option<String>,
    pub used_percentage: Option<f64>,
    pub total_input_tokens: Option<i64>,
    pub sampled_at: Option<String>,
    pub lifecycle_status: String,
    pub state: String,
    pub warning_percentage: Option<f64>,
    pub critical_percentage: Option<f64>,
    pub threshold_percentages: Option<Vec<f64>>,
    pub threshold_source: String,
    pub context_monitor_enabled: bool,
    pub context_monitor_enforced: bool,
    pub notify_session_id: Option<String>,
    pub compaction_active: bool,
    pub last_handoff_path: Option<String>,
}

impl ContextSnapshotResponse {
    fn from_session(session: SessionRecord, config: &ContextMonitorConfig) -> Self {
        let used_percentage = session.context_used_percentage;
        let compaction_active = session.context_compaction_active;
        let lifecycle_status = session.lifecycle_status().to_owned();
        let friendly_name = session.cached_display_name();
        let provider = non_empty_or(session.provider, "claude");
        let unsupported_provider = !provider_has_measured_context_gauge(&provider);
        let thresholds = resolve_context_monitor_thresholds(
            session.context_monitor_threshold_percentages.clone(),
            session.context_monitor_warning_percentage,
            session.context_monitor_critical_percentage,
            config,
        );
        let state = if compaction_active {
            "compacting"
        } else if thresholds.is_err() {
            "thresholds_unavailable"
        } else if let Some(used_percentage) = used_percentage {
            let thresholds = thresholds.as_ref().expect("checked above");
            if used_percentage >= thresholds.critical_percentage {
                "critical"
            } else if used_percentage >= thresholds.warning_percentage {
                "warning"
            } else {
                "normal"
            }
        } else {
            "unknown"
        }
        .to_owned();

        Self {
            session_id: session.id,
            friendly_name,
            provider: Some(provider),
            used_percentage,
            total_input_tokens: session.context_total_input_tokens,
            sampled_at: session.context_sampled_at,
            lifecycle_status,
            state,
            warning_percentage: thresholds
                .as_ref()
                .ok()
                .map(|value| value.warning_percentage),
            critical_percentage: thresholds
                .as_ref()
                .ok()
                .map(|value| value.critical_percentage),
            threshold_percentages: thresholds
                .as_ref()
                .ok()
                .map(|value| value.percentages.clone()),
            threshold_source: thresholds
                .as_ref()
                .map(|value| value.source.as_str().to_owned())
                .unwrap_or_else(|_| "invalid".to_owned()),
            context_monitor_enabled: session.context_monitor_enabled && !unsupported_provider,
            context_monitor_enforced: session.context_monitor_enabled
                && !unsupported_provider
                && thresholds.is_ok(),
            notify_session_id: if unsupported_provider {
                None
            } else {
                session.context_monitor_notify
            },
            compaction_active,
            last_handoff_path: session.last_handoff_path,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextMonitorResult {
    pub status: String,
    pub enabled: bool,
    pub warning_percentage: Option<f64>,
    pub critical_percentage: Option<f64>,
    pub threshold_percentages: Option<Vec<f64>>,
    pub threshold_source: String,
    pub enforced: bool,
}

#[derive(Debug, Clone)]
pub enum ContextMonitorOutcome {
    Updated(ContextMonitorResult),
    InvalidThresholdConfig(String),
    NotFound,
    NotRunning,
    UnsupportedProvider(String),
    MissingNotifyTarget,
    NotifyTargetNotFound(String),
    Unauthorized,
}

#[derive(Debug, Clone)]
pub enum ReparentMutationOutcome {
    Created(ReparentRequestRecord),
    Updated(ReparentRequestRecord),
    Preview(ReparentTreePreview),
    SessionNotFound(String),
    RequestNotFound,
    BadRequest(String),
    Forbidden(String),
    Conflict(String),
    Expired,
}

#[derive(Debug, Clone)]
pub enum CredentialRotationOutcome {
    Created(SessionCredentialRotationRecord),
    Existing(SessionCredentialRotationRecord),
    SessionNotFound,
    BadRequest(String),
    Conflict(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct HandoffResult {
    pub status: String,
}

#[derive(Debug, Clone)]
pub enum HandoffOutcome {
    Recorded(HandoffResult),
    Error(String),
}

#[derive(Debug, Clone)]
pub enum RegistryMutationOutcome {
    Registered(AgentRegistrationResponse),
    NotFound,
    RoleNotRegistered,
    RoleNotOwned,
    BadRequest(String),
    Conflict(String),
}

#[derive(Debug, Clone)]
pub enum MaintainerMutationOutcome {
    Updated(SessionRecord),
    NotFound,
    BadRequest(String),
}

#[derive(Debug, Clone)]
pub enum SessionMetadataOutcome {
    Updated(SessionRecord),
    NotFound,
    BadRequest(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCompleteResult {
    pub status: String,
    pub session_id: String,
    pub em_notified: bool,
    pub agent_task_completed_at: String,
}

#[derive(Debug, Clone)]
pub enum TaskCompleteOutcome {
    Completed(TaskCompleteResult),
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnCompleteResult {
    pub status: String,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub enum TurnCompleteOutcome {
    Completed(TurnCompleteResult),
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ArmStopNotifyResult {
    pub status: String,
    pub session_id: String,
    pub sender_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ArmStopNotifyOutcome {
    Armed(ArmStopNotifyResult),
    Suppressed(ArmStopNotifyResult),
    NotFound,
    Forbidden(String),
    UnknownSender(String),
}

pub enum SubagentStartOutcome {
    Registered(SubagentResponse),
    NotFound,
}

pub enum SubagentStopOutcome {
    Stopped(SubagentStopResult),
    SessionNotFound,
    SubagentNotFound(String),
}

fn read_snapshot(path: &Path) -> Result<StateSnapshot> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read session state {}", path.display()))?;
    let raw: RawStateSnapshot = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse session state {}", path.display()))?;
    StateSnapshot::try_from(raw)
        .with_context(|| format!("failed to parse session records {}", path.display()))
}

fn snapshot_from_raw_value(value: &Value) -> Result<StateSnapshot> {
    let raw = serde_json::from_value::<RawStateSnapshot>(value.clone())
        .context("failed to parse raw session state")?;
    StateSnapshot::try_from(raw).context("failed to parse raw session records")
}

fn ensure_object_mut(value: &mut Value) -> Result<&mut Map<String, Value>> {
    if !value.is_object() {
        *value = json!({});
    }
    Ok(value.as_object_mut().expect("object value set above"))
}

fn ensure_sessions_array_mut(value: &mut Value) -> Result<&mut Vec<Value>> {
    let object = ensure_object_mut(value)?;
    let sessions = object
        .entry("sessions".to_owned())
        .or_insert_with(|| json!([]));
    if !sessions.is_array() {
        anyhow::bail!("session state field 'sessions' is not an array");
    }
    Ok(sessions.as_array_mut().expect("array checked above"))
}

fn ensure_subagents_array_mut(session: &mut Map<String, Value>) -> &mut Vec<Value> {
    let subagents = session
        .entry("subagents".to_owned())
        .or_insert_with(|| json!([]));
    if !subagents.is_array() {
        *subagents = json!([]);
    }
    subagents.as_array_mut().expect("array value set above")
}

fn ensure_agent_registrations_array_mut(value: &mut Value) -> Result<&mut Vec<Value>> {
    let object = ensure_object_mut(value)?;
    let registrations = object
        .entry("agent_registrations".to_owned())
        .or_insert_with(|| json!([]));
    if !registrations.is_array() {
        anyhow::bail!("session state field 'agent_registrations' is not an array");
    }
    Ok(registrations.as_array_mut().expect("array checked above"))
}

fn ensure_array_field_mut<'a>(value: &'a mut Value, field: &str) -> Result<&'a mut Vec<Value>> {
    let object = ensure_object_mut(value)?;
    let entries = object.entry(field.to_owned()).or_insert_with(|| json!([]));
    if !entries.is_array() {
        anyhow::bail!("session state field '{field}' is not an array");
    }
    Ok(entries.as_array_mut().expect("array checked above"))
}

fn active_parent_wake_parent_raw(state: &Value, child_session_id: &str) -> Result<Option<String>> {
    Ok(state
        .get("retained_parent_wake_registrations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.get("child_session_id").and_then(Value::as_str) == Some(child_session_id)
        })
        .find(|entry| {
            entry
                .get("is_active")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
        .and_then(|entry| json_text(entry.get("parent_session_id"))))
}

fn clear_agent_task_completed_raw(state: &mut Value, session_id: &str) -> Result<()> {
    let sessions = ensure_sessions_array_mut(state)?;
    if let Some(session) = session_object_mut(sessions, session_id) {
        session.insert("agent_task_completed_at".to_owned(), Value::Null);
    }
    Ok(())
}

fn deactivate_parent_wake_raw(state: &mut Value, child_session_id: &str) -> Result<()> {
    let registrations = ensure_array_field_mut(state, "retained_parent_wake_registrations")?;
    for entry in registrations.iter_mut().filter(|entry| {
        entry.get("child_session_id").and_then(Value::as_str) == Some(child_session_id)
    }) {
        if let Some(object) = entry.as_object_mut() {
            object.insert("is_active".to_owned(), Value::Bool(false));
            object.insert("cancelled_at".to_owned(), Value::String(now_rfc3339()));
        }
    }
    Ok(())
}

fn deactivate_remind_raw(state: &mut Value, session_id: &str) -> Result<()> {
    let registrations = ensure_array_field_mut(state, "retained_remind_registrations")?;
    for entry in registrations.iter_mut().filter(|entry| {
        entry.get("session_id").and_then(Value::as_str) == Some(session_id)
            || entry.get("target_session_id").and_then(Value::as_str) == Some(session_id)
    }) {
        if let Some(object) = entry.as_object_mut() {
            object.insert("is_active".to_owned(), Value::Bool(false));
            object.insert("cancelled_at".to_owned(), Value::String(now_rfc3339()));
        }
    }
    Ok(())
}

fn stop_notify_state_raw(state: &Value, session_id: &str) -> Option<StopNotifyState> {
    state
        .get("retained_stop_notify_states")
        .and_then(Value::as_array)?
        .iter()
        .find(|entry| entry.get("session_id").and_then(Value::as_str) == Some(session_id))
        .map(|entry| StopNotifyState {
            session_id: session_id.to_owned(),
            sender_session_id: json_text(entry.get("sender_session_id")).unwrap_or_default(),
            sender_name: json_text(entry.get("sender_name")).unwrap_or_default(),
            delay_seconds: entry
                .get("delay_seconds")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        })
        .filter(|entry| !entry.sender_session_id.is_empty())
}

fn clear_stop_notify_raw(state: &mut Value, session_id: &str) -> Result<()> {
    let entries = ensure_array_field_mut(state, "retained_stop_notify_states")?;
    entries.retain(|entry| entry.get("session_id").and_then(Value::as_str) != Some(session_id));
    Ok(())
}

/// Re-arm the one-shot latches and open a new accumulation cycle.
///
/// Clears the cycle boundary stamp. Only a reset reported by a producer hook can
/// set one — see [`set_context_cycle_boundary`] — because the boundary is only
/// ever compared against another stamp from that same producer.
fn reset_context_oneshot_flags(session: &mut Map<String, Value>) {
    session.insert("context_warning_sent".to_owned(), Value::Bool(false));
    session.insert("context_critical_sent".to_owned(), Value::Bool(false));
    session.insert(
        "context_reported_thresholds".to_owned(),
        Value::Array(Vec::new()),
    );
    session.insert("context_cycle_reset_emitted_at".to_owned(), Value::Null);
}

fn clear_context_snapshot(session: &mut Map<String, Value>) {
    session.insert("context_used_percentage".to_owned(), Value::Null);
    session.insert("context_total_input_tokens".to_owned(), Value::Null);
    session.insert("context_sampled_at".to_owned(), Value::Null);
}

/// Record where the new cycle begins, in the producer's own clock.
///
/// Status-line samples ride a detached curl, so a render that races a reset can
/// arrive after it while still describing the context that was just discarded.
/// Applying that sample would restore the stale token count and re-latch the
/// flags the reset cleared, silencing the next real warning for a whole cycle.
///
/// The stamp deliberately comes from the hook rather than from the server clock.
/// Both sides of the comparison are then produced on the session's own host, so
/// a remote node whose clock trails the primary cannot have its fresh samples
/// misread as stale — which would freeze the monitor for the length of the skew
/// after every reset. This mirrors the lifecycle hooks, which likewise compare
/// one emission stamp against another rather than against the server's clock.
fn set_context_cycle_boundary(session: &mut Map<String, Value>, emitted_at: Option<&str>) {
    if let Some(emitted_at) = emitted_at {
        session.insert(
            "context_cycle_reset_emitted_at".to_owned(),
            Value::String(emitted_at.to_owned()),
        );
    }
}

fn flag_is_set(session: &Map<String, Value>, key: &str) -> bool {
    session.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Human label for a session held as raw JSON, where the richer
/// `cached_display_name` resolution is not available.
fn raw_session_label(session: &Map<String, Value>, session_id: &str) -> String {
    json_text(session.get("friendly_name"))
        .or_else(|| json_text(session.get("name")))
        .unwrap_or_else(|| session_id.to_owned())
}

/// Percentages arrive as floats but read as noise past the decimal point, so
/// whole numbers are printed bare and anything else keeps one place.
fn format_percentage(value: f64) -> String {
    if (value - value.round()).abs() < f64::EPSILON {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.1}")
    }
}

#[cfg(test)]
fn format_thousands(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if negative {
        grouped.push('-');
    }
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn push_retained_message_raw(
    state: &mut Value,
    target_session_id: &str,
    text: &str,
    delivery_mode: &str,
    message_category: Option<&str>,
) -> Result<()> {
    let messages = ensure_array_field_mut(state, "retained_pending_messages")?;
    messages.push(json!({
        "target_session_id": target_session_id,
        "text": text,
        "delivery_mode": delivery_mode,
        "message_category": message_category,
        "created_at": now_rfc3339(),
    }));
    Ok(())
}

fn push_retained_message_once_raw(
    state: &mut Value,
    id: &str,
    target_session_id: &str,
    text: &str,
    delivery_mode: &str,
    message_category: Option<&str>,
) -> Result<()> {
    let messages = ensure_array_field_mut(state, "retained_pending_messages")?;
    if let Some(existing) = messages
        .iter()
        .find(|message| json_text(message.get("id")).as_deref() == Some(id))
    {
        let matches = json_text(existing.get("target_session_id")).as_deref()
            == Some(target_session_id)
            && existing.get("text").and_then(Value::as_str) == Some(text)
            && existing.get("delivery_mode").and_then(Value::as_str) == Some(delivery_mode)
            && json_text(existing.get("message_category")).as_deref() == message_category;
        if !matches {
            anyhow::bail!("retained message id {id} already exists with different content");
        }
        return Ok(());
    }
    messages.push(json!({
        "id": id,
        "target_session_id": target_session_id,
        "text": text,
        "delivery_mode": delivery_mode,
        "message_category": message_category,
        "created_at": now_rfc3339(),
    }));
    Ok(())
}

fn deferred_task_complete_message_id(request_id: &str, intent_key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(request_id.as_bytes());
    digest.update(b"\0");
    digest.update(intent_key.as_bytes());
    format!("reparent-task-complete-{:x}", digest.finalize())
}

fn deferred_parent_message_id(request_id: &str, intent_key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(request_id.as_bytes());
    digest.update(b"\0parent-message\0");
    digest.update(intent_key.as_bytes());
    format!("reparent-parent-message-{:x}", digest.finalize())
}

fn persist_deferred_parent_message_intent(
    state: &mut Value,
    request_id: &str,
    key: &str,
    child_session_id: &str,
    text: &str,
    delivery_mode: &str,
    message_category: &str,
) -> Result<()> {
    let mut records = reparent_request_records(state)?;
    let record = records
        .iter_mut()
        .find(|record| record.id == request_id)
        .with_context(|| format!("reparent request {request_id} disappeared"))?;
    if !record
        .deferred_routing_intents
        .iter()
        .any(|intent| intent.key == key)
    {
        record
            .deferred_routing_intents
            .push(ReparentDeferredRoutingIntent {
                key: key.to_owned(),
                operation: "parent_message".to_owned(),
                child_session_id: child_session_id.to_owned(),
                payload: json!({
                    "text": text,
                    "delivery_mode": delivery_mode,
                    "message_category": message_category,
                }),
                created_at: now_rfc3339(),
                replayed_at: None,
                resolved_parent_session_id: None,
            });
    }
    store_reparent_request_records(state, &records)
}

fn persist_deferred_parent_input_intent(
    state: &mut Value,
    request_id: &str,
    key: &str,
    target_session_id: &str,
    text: &str,
    delivery_mode: &str,
    metadata: &QueueMessageMetadata,
) -> Result<()> {
    let mut records = reparent_request_records(state)?;
    let record = records
        .iter_mut()
        .find(|record| record.id == request_id)
        .with_context(|| format!("reparent request {request_id} disappeared"))?;
    if !record
        .deferred_routing_intents
        .iter()
        .any(|intent| intent.key == key)
    {
        record
            .deferred_routing_intents
            .push(ReparentDeferredRoutingIntent {
                key: key.to_owned(),
                operation: "parent_input".to_owned(),
                child_session_id: target_session_id.to_owned(),
                payload: json!({
                    "text": text,
                    "delivery_mode": delivery_mode,
                    "sender_session_id": metadata.sender_session_id,
                    "sender_name": metadata.sender_name,
                    "from_sm_send": metadata.from_sm_send,
                    "timeout_seconds": metadata.timeout_seconds,
                    "notify_on_delivery": metadata.notify_on_delivery,
                    "notify_after_seconds": metadata.notify_after_seconds,
                    "notify_on_stop": metadata.notify_on_stop,
                    "remind_soft_threshold": metadata.remind_soft_threshold,
                    "remind_hard_threshold": metadata.remind_hard_threshold,
                    "remind_cancel_on_reply_session_id": metadata.remind_cancel_on_reply_session_id,
                }),
                created_at: now_rfc3339(),
                replayed_at: None,
                resolved_parent_session_id: None,
            });
    }
    store_reparent_request_records(state, &records)
}

fn deferred_parent_input_metadata(
    intent: &ReparentDeferredRoutingIntent,
    parent_session_id: Option<&str>,
) -> Result<QueueMessageMetadata> {
    Ok(QueueMessageMetadata {
        sender_session_id: intent
            .payload
            .get("sender_session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        sender_name: intent
            .payload
            .get("sender_name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        from_sm_send: intent
            .payload
            .get("from_sm_send")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        timeout_seconds: intent
            .payload
            .get("timeout_seconds")
            .and_then(Value::as_u64),
        notify_on_delivery: intent
            .payload
            .get("notify_on_delivery")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        notify_after_seconds: intent
            .payload
            .get("notify_after_seconds")
            .and_then(Value::as_u64),
        notify_on_stop: intent
            .payload
            .get("notify_on_stop")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        remind_soft_threshold: intent
            .payload
            .get("remind_soft_threshold")
            .and_then(Value::as_u64),
        remind_hard_threshold: intent
            .payload
            .get("remind_hard_threshold")
            .and_then(Value::as_u64),
        remind_cancel_on_reply_session_id: intent
            .payload
            .get("remind_cancel_on_reply_session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        parent_session_id: parent_session_id.map(ToOwned::to_owned),
        ..QueueMessageMetadata::default()
    })
}

fn upsert_remind_raw(
    state: &mut Value,
    target_session_id: &str,
    soft_threshold_seconds: u64,
    hard_threshold_seconds: u64,
    cancel_on_reply_session_id: Option<&str>,
) -> Result<()> {
    let registrations = ensure_array_field_mut(state, "retained_remind_registrations")?;
    let record = json!({
        "id": format!("rust-remind-{target_session_id}"),
        "session_id": target_session_id,
        "target_session_id": target_session_id,
        "soft_threshold_seconds": soft_threshold_seconds,
        "hard_threshold_seconds": hard_threshold_seconds,
        "cancel_on_reply_session_id": cancel_on_reply_session_id,
        "registered_at": now_rfc3339(),
        "last_reset_at": now_rfc3339(),
        "tracked_status_nudge_fired": false,
        "soft_fired": false,
        "persistent_tracking": false,
        "is_active": true,
    });
    if let Some(existing) = registrations.iter_mut().find(|entry| {
        entry.get("session_id").and_then(Value::as_str) == Some(target_session_id)
            || entry.get("target_session_id").and_then(Value::as_str) == Some(target_session_id)
    }) {
        *existing = record;
    } else {
        registrations.push(record);
    }
    Ok(())
}

fn upsert_parent_wake_raw(
    state: &mut Value,
    child_session_id: &str,
    parent_session_id: &str,
    period_seconds: i64,
) -> Result<()> {
    let registrations = ensure_array_field_mut(state, "retained_parent_wake_registrations")?;
    let record = json!({
        "id": format!("rust-wake-{child_session_id}"),
        "child_session_id": child_session_id,
        "parent_session_id": parent_session_id,
        "period_seconds": period_seconds,
        "registered_at": now_rfc3339(),
        "last_wake_at": null,
        "last_status_at_prev_wake": null,
        "escalated": false,
        "is_active": true,
    });
    if let Some(existing) = registrations.iter_mut().find(|entry| {
        entry.get("child_session_id").and_then(Value::as_str) == Some(child_session_id)
    }) {
        *existing = record;
    } else {
        registrations.push(record);
    }
    Ok(())
}

#[derive(Debug)]
struct QueueDrainResult {
    status: String,
    delivered_message_ids: Vec<String>,
}

fn should_persist_runtime_send(delivery_mode: &str) -> bool {
    matches!(
        normalized_delivery_mode(delivery_mode).as_str(),
        "sequential" | "important" | "urgent"
    )
}

fn normalized_delivery_mode(delivery_mode: &str) -> String {
    delivery_mode.trim().to_ascii_lowercase()
}

fn format_send_input_text_raw(
    state: &Value,
    request: &SendCoreInputRequest,
) -> (String, Option<String>) {
    let Some(sender_session_id) = optional_trimmed(request.sender_session_id.as_deref()) else {
        return (request.text.clone(), None);
    };
    let Some(sessions) = state.get("sessions").and_then(Value::as_array) else {
        return (request.text.clone(), None);
    };
    let Some(sender) = session_object(sessions, &sender_session_id) else {
        return (request.text.clone(), None);
    };
    let sender_name = raw_session_display_name(sender, &sender_session_id);
    (
        format!(
            "[Input from: {sender_name} ({}) via sm send]\n{}",
            short_session_id(&sender_session_id),
            request.text
        ),
        Some(sender_name),
    )
}

fn normalized_review_mode(mode: &str) -> String {
    let mode = mode.trim();
    if mode.is_empty() {
        "branch".to_owned()
    } else {
        mode.to_owned()
    }
}

fn review_config_value(mode: &str, request: &StartReviewRequest) -> Value {
    json!({
        "mode": mode,
        "base_branch": trimmed_value(request.base_branch.as_deref()),
        "commit_sha": trimmed_value(request.commit_sha.as_deref()),
        "custom_prompt": trimmed_value(request.custom_prompt.as_deref()),
        "steer_text": trimmed_value(request.steer_text.as_deref()),
        "steer_delivered": false,
        "dispatch_in_progress": true,
        "dispatch_completed_at": null,
        "pr_number": null,
        "pr_repo": null,
        "pr_comment_id": null
    })
}

fn review_session_is_busy(session: &Map<String, Value>, status: &str) -> bool {
    if review_dispatch_in_progress(session) {
        return true;
    }
    if normalized_status(status) != "running" {
        return false;
    }
    let Some(last_tool_call) = json_text(session.get("last_tool_call"))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    !review_dispatch_completed_after(session, &last_tool_call)
}

fn review_dispatch_in_progress(session: &Map<String, Value>) -> bool {
    session
        .get("review_config")
        .and_then(Value::as_object)
        .and_then(|config| config.get("dispatch_in_progress"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn review_dispatch_completed_after(session: &Map<String, Value>, last_tool_call: &str) -> bool {
    session
        .get("review_config")
        .and_then(Value::as_object)
        .and_then(|config| config.get("dispatch_completed_at"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some_and(|completed_at| completed_at >= last_tool_call)
}

fn mark_review_dispatch_completed(session: &mut Map<String, Value>, completed_at: &str) {
    if let Some(config) = session
        .get_mut("review_config")
        .and_then(Value::as_object_mut)
    {
        config.insert("dispatch_in_progress".to_owned(), Value::Bool(false));
        config.insert(
            "dispatch_completed_at".to_owned(),
            Value::String(completed_at.to_owned()),
        );
    }
}

fn trimmed_value(value: Option<&str>) -> Value {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Value::String(value.to_owned()))
        .unwrap_or(Value::Null)
}

fn git_command_success<const N: usize>(working_path: &Path, args: [&str; N]) -> Result<bool> {
    let output = Command::new("git")
        .args(args)
        .current_dir(working_path)
        .output()
        .with_context(|| format!("failed to run git in {}", working_path.display()))?;
    Ok(output.status.success())
}

fn git_commit_exists(working_path: &Path, commit_sha: &str) -> Result<bool> {
    let commit_ref = format!("{commit_sha}^{{commit}}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(commit_ref)
        .current_dir(working_path)
        .output()
        .with_context(|| format!("failed to verify git commit in {}", working_path.display()))?;
    Ok(output.status.success())
}

fn git_branch_position(working_path: &Path, branch: &str) -> Result<Option<usize>> {
    Ok(git_branch_list(working_path)?
        .iter()
        .position(|candidate| candidate == branch))
}

fn git_branch_list(working_path: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["branch", "--list"])
        .current_dir(working_path)
        .output()
        .with_context(|| format!("failed to list git branches in {}", working_path.display()))?;
    if !output.status.success() {
        anyhow::bail!("Failed to list git branches");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let branch = line.trim().trim_start_matches("* ").trim();
            (!branch.is_empty()).then(|| branch.to_owned())
        })
        .collect())
}

fn pending_message_from_metadata(
    target_session_id: &str,
    text: &str,
    delivery_mode: &str,
    metadata: &QueueMessageMetadata,
) -> PendingMessage {
    PendingMessage {
        id: String::new(),
        target_session_id: target_session_id.to_owned(),
        text: text.to_owned(),
        delivery_mode: delivery_mode.to_owned(),
        has_delivery_side_effects: metadata.has_delivery_side_effects(),
        sender_session_id: metadata.sender_session_id.clone(),
        sender_name: metadata.sender_name.clone(),
        from_sm_send: metadata.from_sm_send,
        notify_on_delivery: metadata.notify_on_delivery,
        notify_after_seconds: metadata.notify_after_seconds,
        notify_on_stop: metadata.notify_on_stop,
        remind_soft_threshold: metadata.remind_soft_threshold,
        remind_hard_threshold: metadata.remind_hard_threshold,
        remind_cancel_on_reply_session_id: metadata.remind_cancel_on_reply_session_id.clone(),
        parent_session_id: metadata.parent_session_id.clone(),
        message_category: metadata.message_category.clone(),
        response_relay_source: metadata
            .response_relay_source
            .clone()
            .or_else(|| metadata.from_sm_send.then(|| "sm-send".to_owned())),
    }
}

fn queue_metadata_for_send_request(
    state: &Value,
    target_session_id: &str,
    request: &SendCoreInputRequest,
    sender_name: Option<String>,
) -> QueueMessageMetadata {
    let sender_session_id = optional_trimmed(request.sender_session_id.as_deref())
        .filter(|sender_id| raw_session_object(state, sender_id).is_some());
    let has_sender = sender_session_id.is_some();
    let notify_on_stop = request.notify_on_stop
        && sender_session_id.as_deref().is_some_and(|sender_id| {
            sender_id != target_session_id
                && raw_session_is_em(state, sender_id)
                && raw_session_provider(state, target_session_id).as_deref() != Some("codex-fork")
        });
    QueueMessageMetadata {
        sender_session_id,
        sender_name,
        from_sm_send: request.from_sm_send,
        timeout_seconds: request.timeout_seconds,
        notify_on_delivery: request.notify_on_delivery && has_sender,
        notify_after_seconds: has_sender.then_some(request.notify_after_seconds).flatten(),
        notify_on_stop,
        remind_soft_threshold: request.remind_soft_threshold,
        remind_hard_threshold: request.remind_hard_threshold,
        remind_cancel_on_reply_session_id: request.remind_cancel_on_reply_session_id.clone(),
        parent_session_id: request.parent_session_id.clone(),
        message_category: None,
        response_relay_source: None,
    }
}

fn raw_session_display_name(session: &Map<String, Value>, fallback_id: &str) -> String {
    json_text(session.get("friendly_name"))
        .or_else(|| json_text(session.get("name")))
        .unwrap_or_else(|| fallback_id.to_owned())
}

fn raw_session_is_em(state: &Value, session_id: &str) -> bool {
    raw_session_object(state, session_id)
        .and_then(|session| session.get("is_em"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn raw_session_provider(state: &Value, session_id: &str) -> Option<String> {
    raw_session_object(state, session_id).and_then(|session| json_text(session.get("provider")))
}

fn raw_session_object<'a>(state: &'a Value, session_id: &str) -> Option<&'a Map<String, Value>> {
    let sessions = state.get("sessions").and_then(Value::as_array)?;
    session_object(sessions, session_id)
}

fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn runtime_session_status_raw(state: &mut Value, session_id: &str) -> Result<Option<String>> {
    let sessions = ensure_sessions_array_mut(state)?;
    let Some(session) = session_object_mut(sessions, session_id) else {
        return Ok(None);
    };
    let node = json_text(session.get("node")).unwrap_or_else(default_node);
    ensure_runtime_local_node(&node)?;
    let _tmux_session = json_text(session.get("tmux_session"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
    Ok(Some(effective_raw_session_status(session)))
}

fn runtime_session_accepts_background_delivery_raw(
    state: &mut Value,
    session_id: &str,
    runtime: &TmuxRuntime,
) -> Result<bool> {
    let Some(_status) = runtime_session_status_raw(state, session_id)? else {
        return Ok(false);
    };
    let session = raw_session_object(state, session_id)
        .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared"))?;
    // A Claude Stop hook is the durable lifecycle signal, but it is dispatched
    // through a detached curl and can be stale or lost.  Delivery must not wait
    // for that projection once the provider itself proves that its live composer
    // is empty.  The provider-specific readiness check below is the
    // authoritative, immediate safety fence; stopped and reserved-handoff
    // sessions still cannot receive background input.
    if raw_session_is_stopped(session) {
        return Ok(false);
    }
    if claude_handoff_is_pending_raw(session) {
        return Ok(false);
    }
    let tmux_session = json_text(session.get("tmux_session"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
    let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
    let session_socket_name = json_text(session.get("tmux_socket_name"));
    let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
    Ok(session_runtime.session_exists(&tmux_session)?
        && session_runtime.session_input_ready(&tmux_session, &provider))
}

fn deliver_runtime_text_to_session_raw(
    state: &mut Value,
    session_id: &str,
    text: &str,
    runtime: &TmuxRuntime,
) -> Result<(String, bool)> {
    deliver_runtime_text_to_session_with_ready_fence_raw(state, session_id, text, runtime, false)
}

#[derive(Debug, PartialEq, Eq)]
struct ReparentRuntimeDeliveryTarget {
    tmux_session: String,
    tmux_socket_name: Option<String>,
    delivery_route: ReparentRuntimeDeliveryRoute,
}

#[derive(Debug, PartialEq, Eq)]
enum ReparentRuntimeDeliveryRoute {
    Tmux,
    CodexForkControl {
        control_socket: std::result::Result<PathBuf, String>,
    },
}

fn reparent_runtime_delivery_target(
    state: &Value,
    session_id: &str,
    runtime: &TmuxRuntime,
) -> Result<Option<ReparentRuntimeDeliveryTarget>> {
    let Some(session) = raw_session_object(state, session_id) else {
        return Ok(None);
    };
    let node = json_text(session.get("node")).unwrap_or_else(default_node);
    ensure_runtime_local_node(&node)?;
    if raw_session_is_stopped(session) || claude_handoff_is_reserved_raw(session) {
        return Ok(None);
    }
    let tmux_session = json_text(session.get("tmux_session"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
    let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
    let delivery_route = if provider.eq_ignore_ascii_case("codex-fork") {
        ReparentRuntimeDeliveryRoute::CodexForkControl {
            control_socket: codex_fork_control_socket_path_for_session_raw(
                session_id, session, runtime,
            )
            .map_err(|error| error.to_string()),
        }
    } else {
        ReparentRuntimeDeliveryRoute::Tmux
    };
    Ok(Some(ReparentRuntimeDeliveryTarget {
        tmux_session,
        tmux_socket_name: json_text(session.get("tmux_socket_name")),
        delivery_route,
    }))
}

fn deliver_runtime_background_text_to_session_raw(
    state: &mut Value,
    session_id: &str,
    text: &str,
    runtime: &TmuxRuntime,
) -> Result<(String, bool)> {
    deliver_runtime_text_to_session_with_ready_fence_raw(state, session_id, text, runtime, true)
}

fn deliver_runtime_text_to_session_with_ready_fence_raw(
    state: &mut Value,
    session_id: &str,
    text: &str,
    runtime: &TmuxRuntime,
    require_ready_fence: bool,
) -> Result<(String, bool)> {
    let (mut status, delivered, terminal_delivery_failure) = {
        let sessions = ensure_sessions_array_mut(state)?;
        let session = session_object_mut(sessions, session_id)
            .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during delivery"))?;
        let node = json_text(session.get("node")).unwrap_or_else(default_node);
        ensure_runtime_local_node(&node)?;
        let status = effective_raw_session_status(session);
        if raw_session_is_stopped(session)
            || (require_ready_fence && claude_handoff_is_pending_raw(session))
            || (!require_ready_fence && claude_handoff_is_reserved_raw(session))
        {
            return Ok((status, false));
        }
        let tmux_session = json_text(session.get("tmux_session"))
            .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
        let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
        let session_socket_name = json_text(session.get("tmux_socket_name"));
        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        let _input_guard = require_ready_fence
            .then(|| session_runtime.lock_session_input(&tmux_session))
            .transpose()?;
        if require_ready_fence && !session_runtime.session_input_ready(&tmux_session, &provider) {
            return Ok((status, false));
        }
        let (delivered, mark_stopped_on_failure) =
            match deliver_codex_fork_control_text_to_session_raw(
                session_id, session, text, runtime,
            )? {
                Some(true) => (true, true),
                Some(false) if runtime.codex_fork_control_tmux_fallback_enabled() => {
                    let delivered = if require_ready_fence {
                        session_runtime.send_input_while_locked(&tmux_session, text)?
                    } else {
                        session_runtime.send_input(&tmux_session, text)?
                    };
                    (delivered, true)
                }
                Some(false) => (false, false),
                None => {
                    let delivered = if require_ready_fence {
                        session_runtime.send_input_while_locked(&tmux_session, text)?
                    } else {
                        session_runtime.send_input(&tmux_session, text)?
                    };
                    (delivered, true)
                }
            };
        if delivered {
            mark_session_followup_activity(session, &now_rfc3339());
        }
        (status, delivered, !delivered && mark_stopped_on_failure)
    };
    if terminal_delivery_failure {
        finalize_active_credential_rotations_for_terminal_session(state, session_id)?;
        let sessions = ensure_sessions_array_mut(state)?;
        let session = session_object_mut(sessions, session_id)
            .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during delivery"))?;
        let now = now_rfc3339();
        status = "stopped".to_owned();
        session.insert("status".to_owned(), Value::String(status.clone()));
        session.insert("stopped_at".to_owned(), Value::String(now.clone()));
        session.insert("last_activity".to_owned(), Value::String(now));
    }
    Ok((status, delivered))
}

fn deliver_urgent_runtime_text_to_session_raw(
    state: &mut Value,
    session_id: &str,
    text: &str,
    runtime: &TmuxRuntime,
) -> Result<(String, bool)> {
    let (mut status, delivered, terminal_delivery_failure) = {
        let sessions = ensure_sessions_array_mut(state)?;
        let session = session_object_mut(sessions, session_id)
            .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during delivery"))?;
        let node = json_text(session.get("node")).unwrap_or_else(default_node);
        ensure_runtime_local_node(&node)?;
        let status = effective_raw_session_status(session);
        if raw_session_is_stopped(session) || claude_handoff_is_reserved_raw(session) {
            return Ok((status, false));
        }
        let tmux_session = json_text(session.get("tmux_session"))
            .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
        let provider = json_text(session.get("provider")).unwrap_or_else(|| "claude".to_owned());
        let session_socket_name = json_text(session.get("tmux_socket_name"));
        let session_runtime = runtime.for_socket_name(session_socket_name.as_deref());
        let (delivered, mark_stopped_on_failure) =
            match deliver_codex_fork_control_text_to_session_raw(
                session_id, session, text, runtime,
            )? {
                Some(true) => (true, true),
                Some(false) if runtime.codex_fork_control_tmux_fallback_enabled() => (
                    session_runtime.send_urgent_input(
                        &tmux_session,
                        text,
                        provider.eq_ignore_ascii_case("claude"),
                    )?,
                    true,
                ),
                Some(false) => (false, false),
                None => (
                    session_runtime.send_urgent_input(
                        &tmux_session,
                        text,
                        provider.eq_ignore_ascii_case("claude"),
                    )?,
                    true,
                ),
            };
        if delivered {
            mark_session_followup_activity(session, &now_rfc3339());
        }
        (status, delivered, !delivered && mark_stopped_on_failure)
    };
    if terminal_delivery_failure {
        finalize_active_credential_rotations_for_terminal_session(state, session_id)?;
        let sessions = ensure_sessions_array_mut(state)?;
        let session = session_object_mut(sessions, session_id)
            .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during delivery"))?;
        let now = now_rfc3339();
        status = "stopped".to_owned();
        session.insert("status".to_owned(), Value::String(status.clone()));
        session.insert("stopped_at".to_owned(), Value::String(now.clone()));
        session.insert("last_activity".to_owned(), Value::String(now));
    }
    Ok((status, delivered))
}

fn deliver_runtime_native_rename_to_session_raw(
    state: &mut Value,
    session_id: &str,
    text: &str,
    runtime: &TmuxRuntime,
) -> Result<(String, bool)> {
    let sessions = ensure_sessions_array_mut(state)?;
    let session = session_object_mut(sessions, session_id)
        .ok_or_else(|| anyhow::anyhow!("session {session_id} disappeared during delivery"))?;
    let node = json_text(session.get("node")).unwrap_or_else(default_node);
    ensure_runtime_local_node(&node)?;
    let status = effective_raw_session_status(session);
    if raw_session_is_stopped(session) {
        return Ok((status, false));
    }
    if claude_handoff_is_reserved_raw(session) {
        return Ok((status, false));
    }
    let Some(friendly_name) = extract_provider_native_rename_name(text) else {
        return Ok((status, false));
    };
    let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
    let delivered = if provider.eq_ignore_ascii_case("codex-fork") {
        match codex_fork_control_socket_path_for_session_raw(session_id, session, runtime).and_then(
            |control_socket_path| codex_fork_set_thread_name(&control_socket_path, &friendly_name),
        ) {
            Ok(()) => {
                clear_codex_fork_control_degraded_raw(session);
                true
            }
            Err(error) => {
                mark_codex_fork_control_degraded_raw(session, &error.to_string());
                if runtime.codex_fork_control_tmux_fallback_enabled() {
                    let tmux_session = json_text(session.get("tmux_session")).ok_or_else(|| {
                        anyhow::anyhow!("session {session_id} missing tmux_session")
                    })?;
                    let session_socket_name = json_text(session.get("tmux_socket_name"));
                    runtime
                        .for_socket_name(session_socket_name.as_deref())
                        .send_input(&tmux_session, &format!("/rename {friendly_name}"))?
                } else {
                    false
                }
            }
        }
    } else if provider.eq_ignore_ascii_case("claude") {
        let tmux_session = json_text(session.get("tmux_session"))
            .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
        let session_socket_name = json_text(session.get("tmux_socket_name"));
        runtime
            .for_socket_name(session_socket_name.as_deref())
            .send_input(&tmux_session, &format!("/rename {friendly_name}"))?
    } else {
        false
    };
    if delivered {
        session.insert("last_activity".to_owned(), Value::String(now_rfc3339()));
    }
    Ok((status, delivered))
}

fn deliver_codex_fork_control_text_to_session_raw(
    session_id: &str,
    session: &mut Map<String, Value>,
    text: &str,
    runtime: &TmuxRuntime,
) -> Result<Option<bool>> {
    let provider = json_text(session.get("provider")).unwrap_or_else(default_provider);
    if !provider.eq_ignore_ascii_case("codex-fork") {
        return Ok(None);
    }

    let result = codex_fork_control_socket_path_for_session_raw(session_id, session, runtime)
        .and_then(|control_socket_path| codex_fork_submit_message(&control_socket_path, text));
    match result {
        Ok(()) => {
            clear_codex_fork_control_degraded_raw(session);
            Ok(Some(true))
        }
        Err(error) => {
            mark_codex_fork_control_degraded_raw(session, &error.to_string());
            Ok(Some(false))
        }
    }
}

fn codex_fork_control_socket_path_for_session_raw(
    session_id: &str,
    session: &Map<String, Value>,
    runtime: &TmuxRuntime,
) -> Result<PathBuf> {
    let spec = codex_fork_spec_for_session_raw(session_id, session)?;
    let artifacts = runtime
        .codex_fork_runtime_artifacts(&spec)?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} is not a codex-fork session"))?;
    Ok(artifacts.control_socket_path)
}

fn codex_fork_spec_for_session_raw(
    session_id: &str,
    session: &Map<String, Value>,
) -> Result<TmuxSessionSpec> {
    let tmux_session = json_text(session.get("tmux_session"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing tmux_session"))?;
    let working_dir = json_text(session.get("working_dir"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing working_dir"))?;
    let log_file = json_text(session.get("log_file"))
        .ok_or_else(|| anyhow::anyhow!("session {session_id} missing log_file"))?;
    Ok(TmuxSessionSpec {
        session_id: session_id.to_owned(),
        session_credential: None,
        tmux_session,
        working_dir: expand_home(&working_dir).display().to_string(),
        log_file: expand_home(&log_file),
        provider: "codex-fork".to_owned(),
        initial_message: None,
        force_initial_prompt_stdin: false,
        model: json_text(session.get("model")),
        reasoning_effort: json_text(session.get("reasoning_effort")),
    })
}

fn codex_fork_submit_message(control_socket_path: &Path, text: &str) -> Result<()> {
    let mut epoch = codex_fork_refresh_control_epoch(control_socket_path)?;
    let mut response =
        codex_fork_send_control_command(control_socket_path, "submit_message", &epoch, text)?;
    if !codex_fork_response_ok(&response)
        && codex_fork_error_code(&response).as_deref() == Some("stale_epoch")
    {
        epoch = codex_fork_refresh_control_epoch(control_socket_path)?;
        response =
            codex_fork_send_control_command(control_socket_path, "submit_message", &epoch, text)?;
    }
    ensure_codex_fork_response_ok(&response, "control command failed")
}

pub fn submit_codex_fork_btw(
    session: &SessionRecord,
    request_id: &str,
    prompt: &str,
    runtime: &TmuxRuntime,
) -> Result<PathBuf> {
    if !session.provider.eq_ignore_ascii_case("codex-fork") {
        anyhow::bail!("session {} is not a codex-fork session", session.id);
    }
    let spec = TmuxSessionSpec {
        session_id: session.id.clone(),
        session_credential: None,
        tmux_session: session.tmux_session.clone(),
        working_dir: session.working_dir.clone(),
        log_file: session
            .log_file
            .as_deref()
            .map(expand_home)
            .ok_or_else(|| anyhow::anyhow!("session {} missing log_file", session.id))?,
        provider: "codex-fork".to_owned(),
        initial_message: None,
        force_initial_prompt_stdin: false,
        model: session.model.clone(),
        reasoning_effort: session.reasoning_effort.clone(),
    };
    let artifacts = runtime
        .codex_fork_runtime_artifacts(&spec)?
        .ok_or_else(|| anyhow::anyhow!("codex-fork runtime artifacts unavailable"))?;
    codex_fork_submit_btw(&artifacts.control_socket_path, request_id, prompt)?;
    Ok(artifacts.event_stream_path)
}

fn codex_fork_submit_btw(control_socket_path: &Path, request_id: &str, prompt: &str) -> Result<()> {
    let mut epoch = codex_fork_refresh_control_epoch(control_socket_path)?;
    let mut response = codex_fork_send_control_command_payload_with_request_id(
        control_socket_path,
        request_id,
        "submit_btw",
        &epoch,
        json!({ "prompt": prompt }),
    )?;
    if !codex_fork_response_ok(&response)
        && codex_fork_error_code(&response).as_deref() == Some("stale_epoch")
    {
        epoch = codex_fork_refresh_control_epoch(control_socket_path)?;
        response = codex_fork_send_control_command_payload_with_request_id(
            control_socket_path,
            request_id,
            "submit_btw",
            &epoch,
            json!({ "prompt": prompt }),
        )?;
    }
    ensure_codex_fork_response_ok(&response, "submit_btw control command failed")
}

fn codex_fork_set_thread_name(control_socket_path: &Path, friendly_name: &str) -> Result<()> {
    let mut epoch = codex_fork_refresh_control_epoch(control_socket_path)?;
    let mut response = codex_fork_send_control_command_payload(
        control_socket_path,
        "set_thread_name",
        &epoch,
        json!({ "name": friendly_name }),
    )?;
    if !codex_fork_response_ok(&response)
        && codex_fork_error_code(&response).as_deref() == Some("stale_epoch")
    {
        epoch = codex_fork_refresh_control_epoch(control_socket_path)?;
        response = codex_fork_send_control_command_payload(
            control_socket_path,
            "set_thread_name",
            &epoch,
            json!({ "name": friendly_name }),
        )?;
    }
    ensure_codex_fork_response_ok(&response, "control command failed")
}

fn codex_fork_refresh_control_epoch(control_socket_path: &Path) -> Result<String> {
    let request = json!({
        "request_id": codex_fork_control_request_id(),
        "command": "get_epoch",
    });
    let response = codex_fork_control_roundtrip(control_socket_path, &request)
        .with_context(|| "failed to read control epoch")?;
    ensure_codex_fork_response_ok(&response, "failed to fetch epoch")?;
    codex_fork_response_epoch(&response)
        .ok_or_else(|| anyhow::anyhow!("control epoch missing from response"))
}

fn codex_fork_send_control_command(
    control_socket_path: &Path,
    command: &str,
    expected_epoch: &str,
    message: &str,
) -> Result<Value> {
    codex_fork_send_control_command_payload(
        control_socket_path,
        command,
        expected_epoch,
        json!({ "message": message }),
    )
}

fn codex_fork_send_control_command_payload(
    control_socket_path: &Path,
    command: &str,
    expected_epoch: &str,
    payload: Value,
) -> Result<Value> {
    codex_fork_send_control_command_payload_with_request_id(
        control_socket_path,
        &codex_fork_control_request_id(),
        command,
        expected_epoch,
        payload,
    )
}

fn codex_fork_send_control_command_payload_with_request_id(
    control_socket_path: &Path,
    request_id: &str,
    command: &str,
    expected_epoch: &str,
    payload: Value,
) -> Result<Value> {
    let mut request = Map::new();
    request.insert(
        "request_id".to_owned(),
        Value::String(request_id.to_owned()),
    );
    request.insert(
        "expected_epoch".to_owned(),
        Value::String(expected_epoch.to_owned()),
    );
    request.insert("command".to_owned(), Value::String(command.to_owned()));
    if let Some(payload) = payload.as_object() {
        for (key, value) in payload {
            request.insert(key.clone(), value.clone());
        }
    }
    codex_fork_control_roundtrip(control_socket_path, &Value::Object(request))
        .with_context(|| "control command failed")
}

#[cfg(unix)]
fn codex_fork_control_roundtrip(control_socket_path: &Path, request: &Value) -> Result<Value> {
    let deadline = Instant::now() + CODEX_FORK_CONTROL_RECOVERY_TIMEOUT;
    loop {
        match codex_fork_control_roundtrip_once(control_socket_path, request) {
            Ok(response) => return Ok(response),
            Err(error)
                if codex_fork_control_error_is_transient(&error) && Instant::now() < deadline =>
            {
                thread::sleep(CODEX_FORK_CONTROL_RECOVERY_POLL);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn codex_fork_control_roundtrip_once(control_socket_path: &Path, request: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(control_socket_path).with_context(|| {
        format!(
            "failed to connect control socket {}",
            control_socket_path.display()
        )
    })?;
    stream
        .set_read_timeout(Some(CODEX_FORK_CONTROL_TIMEOUT))
        .with_context(|| "failed to set control socket read timeout")?;
    stream
        .set_write_timeout(Some(CODEX_FORK_CONTROL_TIMEOUT))
        .with_context(|| "failed to set control socket write timeout")?;
    let mut raw_request = serde_json::to_string(request)?;
    raw_request.push('\n');
    stream
        .write_all(raw_request.as_bytes())
        .with_context(|| "failed to write control socket request")?;
    stream
        .flush()
        .with_context(|| "failed to flush control socket request")?;

    let mut reader = BufReader::new(stream);
    let mut raw_response = String::new();
    reader
        .read_line(&mut raw_response)
        .with_context(|| "failed to read control socket response")?;
    if raw_response.is_empty() {
        return Err(anyhow::anyhow!("control socket closed without response"));
    }
    serde_json::from_str(&raw_response).with_context(|| "control socket returned invalid JSON")
}

#[cfg(unix)]
fn codex_fork_control_error_is_transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
            )
        })
    })
}

#[cfg(not(unix))]
fn codex_fork_control_roundtrip(_control_socket_path: &Path, _request: &Value) -> Result<Value> {
    Err(anyhow::anyhow!(
        "codex-fork control sockets are only supported on Unix"
    ))
}

fn ensure_codex_fork_response_ok(response: &Value, default_message: &str) -> Result<()> {
    if codex_fork_response_ok(response) {
        return Ok(());
    }
    let code = codex_fork_error_code(response).unwrap_or_else(|| "unknown_error".to_owned());
    let message = codex_fork_error_message(response).unwrap_or_else(|| default_message.to_owned());
    Err(anyhow::anyhow!("{code}: {message}"))
}

fn codex_fork_response_ok(response: &Value) -> bool {
    response.get("ok").and_then(Value::as_bool).unwrap_or(false)
}

fn codex_fork_response_epoch(response: &Value) -> Option<String> {
    response
        .get("result")
        .and_then(Value::as_object)
        .and_then(|result| json_text(result.get("epoch")))
        .or_else(|| json_text(response.get("epoch")))
}

fn codex_fork_error_code(response: &Value) -> Option<String> {
    response
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| json_text(error.get("code")))
}

fn codex_fork_error_message(response: &Value) -> Option<String> {
    response
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| json_text(error.get("message")))
}

fn codex_fork_control_request_id() -> String {
    let counter = STATE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("rust-{}-{counter}", std::process::id())
}

fn mark_codex_fork_control_degraded_raw(session: &mut Map<String, Value>, reason: &str) {
    let reason = reason.trim();
    let reason = if reason.is_empty() {
        "unknown_control_error"
    } else {
        reason
    };
    session.insert(
        "error_message".to_owned(),
        Value::String(format!("codex_fork_control_degraded: {reason}")),
    );
}

fn clear_codex_fork_control_degraded_raw(session: &mut Map<String, Value>) -> bool {
    let is_degraded_error = json_text(session.get("error_message"))
        .as_deref()
        .is_some_and(|message| message.starts_with("codex_fork_control_degraded:"));
    if is_degraded_error {
        session.insert("error_message".to_owned(), Value::Null);
    }
    is_degraded_error
}

fn clear_codex_fork_handoff_error_raw(session: &mut Map<String, Value>) {
    let is_handoff_error = json_text(session.get("error_message"))
        .as_deref()
        .is_some_and(|message| message.starts_with("codex_fork_handoff_failed:"));
    if is_handoff_error {
        session.insert("error_message".to_owned(), Value::Null);
    }
}

fn clear_claude_handoff_error_raw(session: &mut Map<String, Value>) {
    let is_handoff_error = json_text(session.get("error_message"))
        .as_deref()
        .is_some_and(|message| message.starts_with("claude_handoff_failed:"));
    if is_handoff_error {
        session.insert("error_message".to_owned(), Value::Null);
    }
}

fn claude_handoff_is_pending_raw(session: &Map<String, Value>) -> bool {
    json_text(session.get("provider")).as_deref() == Some("claude")
        && json_text(session.get("pending_handoff_path")).is_some()
        && json_text(session.get("pending_handoff_recorded_at")).is_some()
}

fn claude_handoff_is_reserved_raw(session: &Map<String, Value>) -> bool {
    json_text(session.get("provider")).as_deref() == Some("claude")
        && json_text(session.get("claude_handoff_in_progress_at")).is_some()
}

fn claude_handoff_reservation_replaced_raw(
    session: &Map<String, Value>,
    file_path: &str,
    recorded_at: &str,
    reservation_at: &str,
) -> bool {
    if json_text(session.get("provider")).as_deref() != Some("claude")
        || raw_session_is_stopped(session)
        || !is_primary_node(&json_text(session.get("node")).unwrap_or_else(default_node))
    {
        return false;
    }
    let Some(current_file_path) = json_text(session.get("pending_handoff_path")) else {
        return false;
    };
    let Some(current_recorded_at) = json_text(session.get("pending_handoff_recorded_at")) else {
        return false;
    };
    let Some(current_reservation_at) = json_text(session.get("claude_handoff_in_progress_at"))
    else {
        return false;
    };
    current_file_path != file_path
        || current_recorded_at != recorded_at
        || current_reservation_at != reservation_at
}

fn consume_completed_claude_handoff_raw(
    session: &mut Map<String, Value>,
    file_path: &str,
    recorded_at: &str,
    reservation_at: &str,
) -> (bool, bool) {
    let cleared_pending = json_text(session.get("pending_handoff_path")).as_deref()
        == Some(file_path)
        && json_text(session.get("pending_handoff_recorded_at")).as_deref() == Some(recorded_at);
    let stopped_after_prompt = cleared_pending
        && json_text(session.get("claude_handoff_in_progress_at"))
            .is_some_and(|current| current != reservation_at);
    if cleared_pending {
        session.insert("pending_handoff_path".to_owned(), Value::Null);
        session.insert("pending_handoff_recorded_at".to_owned(), Value::Null);
        // The handoff turn's own Stop may refresh this timestamp before the
        // worker persists success. It still reserves the intent consumed here.
        session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
    } else if json_text(session.get("claude_handoff_in_progress_at")).as_deref()
        == Some(reservation_at)
    {
        session.insert("claude_handoff_in_progress_at".to_owned(), Value::Null);
    }
    (cleared_pending, stopped_after_prompt)
}

fn drain_pending_runtime_messages_raw(
    store: &SessionStore,
    state: &mut Value,
    session_id: &str,
    runtime: &TmuxRuntime,
    queue: &RetainedQueueStore,
    delivery_mode_filter: Option<&str>,
    message_category_filter: Option<&str>,
    stop_after_message_id: Option<&str>,
    require_ready_fence: bool,
) -> Result<QueueDrainResult> {
    let mut status =
        runtime_session_status_raw(state, session_id)?.unwrap_or_else(|| "stopped".to_owned());
    let mut delivered_message_ids = Vec::new();
    loop {
        let messages = match (delivery_mode_filter, message_category_filter) {
            (_, Some(message_category)) => {
                queue.pending_messages_for_target_by_category(session_id, message_category, 10)?
            }
            (Some(delivery_mode), None) => {
                queue.pending_messages_for_target_by_mode(session_id, delivery_mode, 10)?
            }
            (None, None) => queue.pending_messages_for_target(session_id, 10)?,
        };
        if messages.is_empty() {
            break;
        }

        let mut should_continue = true;
        for message in messages {
            let control_predecessor = require_ready_fence
                && (message.message_category.as_deref() == Some("native_rename")
                    || normalized_delivery_mode(&message.delivery_mode) == "urgent");
            let (next_status, delivered) = if message.message_category.as_deref()
                == Some("native_rename")
            {
                deliver_runtime_native_rename_to_session_raw(
                    state,
                    session_id,
                    &message.text,
                    runtime,
                )?
            } else if normalized_delivery_mode(&message.delivery_mode) == "urgent" {
                deliver_urgent_runtime_text_to_session_raw(
                    state,
                    session_id,
                    &message.text,
                    runtime,
                )?
            } else {
                if require_ready_fence {
                    deliver_runtime_background_text_to_session_raw(
                        state,
                        session_id,
                        &message.text,
                        runtime,
                    )?
                } else {
                    deliver_runtime_text_to_session_raw(state, session_id, &message.text, runtime)?
                }
            };
            status = next_status;
            if !delivered {
                should_continue = false;
                break;
            }
            complete_runtime_message_delivery_raw(store, state, runtime, queue, &message)?;
            if control_predecessor {
                // An urgent or provider-control predecessor can have just
                // started a turn. Wait for the next reconciliation, which
                // will take a fresh readiness proof under the input lock,
                // before considering a following completion wake.
                should_continue = false;
                break;
            }
            let delivered_target =
                stop_after_message_id.is_some_and(|target_id| target_id == message.id);
            delivered_message_ids.push(message.id);
            if delivered_target {
                should_continue = false;
                break;
            }
        }

        if !should_continue {
            break;
        }
    }
    Ok(QueueDrainResult {
        status,
        delivered_message_ids,
    })
}

fn complete_runtime_message_delivery_raw(
    store: &SessionStore,
    state: &mut Value,
    runtime: &TmuxRuntime,
    queue: &RetainedQueueStore,
    message: &PendingMessage,
) -> Result<()> {
    let sanitized_message;
    let message = if message
        .sender_session_id
        .as_deref()
        .is_some_and(|sender_id| raw_session_object(state, sender_id).is_none())
    {
        sanitized_message = PendingMessage {
            sender_session_id: None,
            sender_name: None,
            notify_on_delivery: false,
            notify_after_seconds: None,
            notify_on_stop: false,
            ..message.clone()
        };
        &sanitized_message
    } else {
        message
    };

    let fenced_message;
    let message = if message.parent_session_id.is_some()
        && message.remind_soft_threshold.is_some()
        && active_reparent_request_for_session(state, &message.target_session_id)?.is_some()
    {
        persist_deferred_parent_wake_intent(state, message)?;
        // The intent must be durable before SQLite marks the source message
        // delivered, otherwise a crash can lose the parent wake entirely.
        store.write_raw_json_value(state)?;
        fenced_message = PendingMessage {
            parent_session_id: None,
            ..message.clone()
        };
        &fenced_message
    } else {
        message
    };

    queue.mark_delivered_and_apply_side_effects(message)?;

    if message.notify_on_delivery {
        if let Some(sender_session_id) = message.sender_session_id.as_deref() {
            push_retained_message_raw(
                state,
                sender_session_id,
                &runtime_delivery_notification_text(message),
                "sequential",
                None,
            )?;
        }
    }

    if message.notify_on_stop {
        if let Some(sender_session_id) = message.sender_session_id.as_deref() {
            upsert_stop_notify_raw(
                state,
                &message.target_session_id,
                sender_session_id,
                message.sender_name.as_deref().unwrap_or(""),
                0,
            )?;
        }
    }

    if let Some(soft_threshold) = message.remind_soft_threshold {
        let hard_threshold = message
            .remind_hard_threshold
            .unwrap_or_else(|| soft_threshold.saturating_add(120));
        upsert_remind_raw(
            state,
            &message.target_session_id,
            soft_threshold,
            hard_threshold,
            message.remind_cancel_on_reply_session_id.as_deref(),
        )?;
        if let Some(parent_session_id) = message.parent_session_id.as_deref() {
            upsert_parent_wake_raw(state, &message.target_session_id, parent_session_id, 600)?;
        }
    }

    if message.notify_on_delivery {
        if let Some(sender_session_id) = message.sender_session_id.as_deref() {
            drain_pending_runtime_messages_raw(
                store,
                state,
                sender_session_id,
                runtime,
                queue,
                Some("sequential"),
                None,
                None,
                false,
            )?;
        }
    }

    schedule_runtime_followup_notification(
        store.clone(),
        runtime.clone(),
        queue.clone(),
        message.clone(),
    );
    Ok(())
}

fn persist_deferred_parent_wake_intent(state: &mut Value, message: &PendingMessage) -> Result<()> {
    let request_id = active_reparent_request_for_session(state, &message.target_session_id)?
        .context("routing fence disappeared while deferring parent wake")?;
    let mut records = reparent_request_records(state)?;
    let record = records
        .iter_mut()
        .find(|record| record.id == request_id)
        .with_context(|| format!("reparent request {request_id} disappeared"))?;
    let key = format!("message-parent-wake:{}", message.id);
    if !record
        .deferred_routing_intents
        .iter()
        .any(|intent| intent.key == key)
    {
        record
            .deferred_routing_intents
            .push(ReparentDeferredRoutingIntent {
                key,
                operation: "parent_wake".to_owned(),
                child_session_id: message.target_session_id.clone(),
                payload: json!({ "period_seconds": 600 }),
                created_at: now_rfc3339(),
                replayed_at: None,
                resolved_parent_session_id: None,
            });
        store_reparent_request_records(state, &records)?;
    }
    Ok(())
}

fn runtime_delivery_notification_text(message: &PendingMessage) -> String {
    let truncated = truncate_chars(&message.text, 100);
    format!(
        "[sm] Message delivered to {}\nOriginal: \"{}\"",
        message.target_session_id, truncated
    )
}

fn schedule_runtime_followup_notification(
    store: SessionStore,
    runtime: TmuxRuntime,
    queue: RetainedQueueStore,
    message: PendingMessage,
) {
    let Some(sender_session_id) = message.sender_session_id.clone() else {
        return;
    };
    let Some(seconds) = message.notify_after_seconds else {
        return;
    };
    if seconds == 0 {
        return;
    }
    let Some(text) = followup_notification_text(&message) else {
        return;
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            if queue
                .enqueue_message(&sender_session_id, &text, "sequential", None)
                .is_ok()
            {
                let _ =
                    store.drain_runtime_pending_messages_for_session(&sender_session_id, &runtime);
            }
        });
    }
}

fn complete_stop_notify_after_stop_raw(
    store: &SessionStore,
    state: &mut Value,
    runtime: Option<&TmuxRuntime>,
    session_id: &str,
    recipient_name: &str,
) -> Result<()> {
    let queue = store.queue_store.as_ref();
    let stop_notify = match queue {
        Some(queue) => queue.stop_notify_state(session_id)?,
        None => stop_notify_state_raw(state, session_id),
    };
    let Some(stop_notify) = stop_notify else {
        return Ok(());
    };

    if let Some(queue) = queue {
        queue.clear_stop_notify(session_id)?;
    }
    clear_stop_notify_raw(state, session_id)?;

    if raw_session_object(state, &stop_notify.sender_session_id).is_none() {
        return Ok(());
    }

    let text = runtime_stop_notification_text(recipient_name, session_id);
    if stop_notify.delay_seconds > 0 {
        schedule_stop_notification(
            store.clone(),
            runtime.cloned(),
            stop_notify.sender_session_id,
            text,
            stop_notify.delay_seconds as u64,
        );
        return Ok(());
    }

    if let Some(queue) = queue {
        enqueue_stop_notification_raw(
            store,
            state,
            runtime,
            queue,
            &stop_notify.sender_session_id,
            &text,
        )?;
    } else {
        push_retained_message_raw(
            state,
            &stop_notify.sender_session_id,
            &text,
            "important",
            Some("stop_notify"),
        )?;
    }
    Ok(())
}

fn enqueue_stop_notification_raw(
    store: &SessionStore,
    state: &mut Value,
    runtime: Option<&TmuxRuntime>,
    queue: &RetainedQueueStore,
    sender_session_id: &str,
    text: &str,
) -> Result<()> {
    if raw_session_object(state, sender_session_id).is_none() {
        return Ok(());
    }
    queue.enqueue_message(sender_session_id, text, "important", Some("stop_notify"))?;
    push_retained_message_raw(
        state,
        sender_session_id,
        text,
        "important",
        Some("stop_notify"),
    )?;
    if let Some(runtime) = runtime {
        drain_pending_runtime_messages_raw(
            store,
            state,
            sender_session_id,
            runtime,
            queue,
            Some("important"),
            None,
            None,
            false,
        )?;
    }
    Ok(())
}

fn schedule_stop_notification(
    store: SessionStore,
    runtime: Option<TmuxRuntime>,
    sender_session_id: String,
    text: String,
    delay_seconds: u64,
) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_secs(delay_seconds)).await;
            let _ = store.enqueue_stop_notification_for_session(
                &sender_session_id,
                &text,
                runtime.as_ref(),
            );
        });
    }
}

fn runtime_stop_notification_text(recipient_name: &str, recipient_session_id: &str) -> String {
    format!(
        "[sm] {} ({}) completed (Stop hook fired)",
        recipient_name,
        short_session_id(recipient_session_id)
    )
}

fn deliver_urgent_runtime_message_raw(
    store: &SessionStore,
    state: &mut Value,
    session_id: &str,
    runtime: &TmuxRuntime,
    queue: &RetainedQueueStore,
    message: &PendingMessage,
) -> Result<QueueDrainResult> {
    let (status, delivered) =
        deliver_urgent_runtime_text_to_session_raw(state, session_id, &message.text, runtime)?;
    let mut delivered_message_ids = Vec::new();
    if delivered {
        complete_runtime_message_delivery_raw(store, state, runtime, queue, message)?;
        delivered_message_ids.push(message.id.clone());
    }
    Ok(QueueDrainResult {
        status,
        delivered_message_ids,
    })
}

fn upsert_stop_notify_raw(
    state: &mut Value,
    session_id: &str,
    sender_session_id: &str,
    sender_name: &str,
    delay_seconds: i64,
) -> Result<()> {
    let entries = ensure_array_field_mut(state, "retained_stop_notify_states")?;
    let record = json!({
        "session_id": session_id,
        "sender_session_id": sender_session_id,
        "sender_name": sender_name,
        "delay_seconds": delay_seconds,
        "armed_at": now_rfc3339(),
    });
    if let Some(existing) = entries
        .iter_mut()
        .find(|entry| entry.get("session_id").and_then(Value::as_str) == Some(session_id))
    {
        *existing = record;
    } else {
        entries.push(record);
    }
    Ok(())
}

fn raw_registration_record(value: &Value) -> Option<AgentRegistrationRecord> {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .map(normalize_role)
        .filter(|role| !role.is_empty())?;
    let session_id = value
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())?
        .to_owned();
    let created_at = value
        .get("created_at")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|created_at| !created_at.is_empty())
        .map(ToOwned::to_owned);
    Some(AgentRegistrationRecord {
        role,
        session_id,
        created_at,
    })
}

fn find_raw_registration(state: &Value, role: &str) -> Result<Option<AgentRegistrationRecord>> {
    let normalized_role = normalize_role(role);
    let registrations = state
        .get("agent_registrations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(registrations
        .iter()
        .filter_map(raw_registration_record)
        .find(|registration| registration.role == normalized_role))
}

fn upsert_raw_registration(
    state: &mut Value,
    role: &str,
    session_id: &str,
    created_at: &str,
) -> Result<()> {
    let normalized_role = normalize_role(role);
    let registrations = ensure_agent_registrations_array_mut(state)?;
    if let Some(existing) = registrations.iter_mut().find(|entry| {
        entry
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|value| normalize_role(value) == normalized_role)
    }) {
        *existing = json!({
            "role": normalized_role,
            "session_id": session_id,
            "created_at": created_at,
        });
        return Ok(());
    }
    registrations.push(json!({
        "role": normalized_role,
        "session_id": session_id,
        "created_at": created_at,
    }));
    Ok(())
}

fn remove_raw_registration(state: &mut Value, role: &str) -> Result<()> {
    let normalized_role = normalize_role(role);
    let registrations = ensure_agent_registrations_array_mut(state)?;
    registrations.retain(|entry| {
        !entry
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|value| normalize_role(value) == normalized_role)
    });
    Ok(())
}

fn remember_role_last_session_raw(state: &mut Value, role: &str, session_id: &str) -> Result<()> {
    if role.is_empty() || session_id.trim().is_empty() {
        return Ok(());
    }
    let object = ensure_object_mut(state)?;
    let last = object
        .entry("agent_role_last_session_ids".to_owned())
        .or_insert_with(|| json!({}));
    if !last.is_object() {
        *last = json!({});
    }
    last.as_object_mut()
        .expect("object value set above")
        .insert(role.to_owned(), Value::String(session_id.to_owned()));
    Ok(())
}

fn forget_role_last_session_raw(state: &mut Value, role: &str) -> Result<()> {
    let normalized_role = normalize_role(role);
    if normalized_role.is_empty() {
        return Ok(());
    }
    if let Some(last) = ensure_object_mut(state)?
        .get_mut("agent_role_last_session_ids")
        .and_then(Value::as_object_mut)
    {
        last.remove(&normalized_role);
    }
    Ok(())
}

fn sync_maintainer_alias_raw(state: &mut Value) -> Result<()> {
    let maintainer = find_raw_registration(state, "maintainer")?;
    let object = ensure_object_mut(state)?;
    object.insert(
        "maintainer_session_id".to_owned(),
        maintainer
            .map(|registration| Value::String(registration.session_id))
            .unwrap_or(Value::Null),
    );
    Ok(())
}

fn validate_friendly_name_update_raw(
    state: &Value,
    session_id: &str,
    friendly_name: &str,
    primary_alias: &Option<String>,
    reserved_human_names: &BTreeSet<String>,
) -> Result<Option<String>> {
    if let Some(primary_alias) = primary_alias {
        if friendly_name != primary_alias {
            return Ok(Some(format!(
                "Session identity is controlled by registry role \"{primary_alias}\""
            )));
        }
    }

    let normalized_name = normalize_role(friendly_name);
    let mut reserved_aliases = BTreeSet::from(["maintainer".to_owned()]);
    for registration in state
        .get("agent_registrations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(raw_registration_record)
    {
        reserved_aliases.insert(registration.role);
    }
    if reserved_aliases.contains(&normalized_name)
        && primary_alias.as_deref() != Some(normalized_name.as_str())
    {
        return Ok(Some(format!(
            "Name \"{friendly_name}\" is reserved for registry identity \"{normalized_name}\""
        )));
    }
    if reserved_human_names.contains(&normalized_name) {
        return Ok(Some(format!(
            "Name \"{friendly_name}\" is reserved for configured human recipient \"{normalized_name}\""
        )));
    }
    if session_id.trim().is_empty() {
        return Ok(Some("Session not found".to_owned()));
    }
    Ok(None)
}

fn recover_missing_maintainer_registration_raw(state: &mut Value) -> Result<bool> {
    if find_raw_registration(state, "maintainer")?.is_some() {
        return Ok(false);
    }
    let sessions = snapshot_from_raw_value(state)?.into_sessions();
    let mut candidates = Vec::<(String, bool)>::new();
    let legacy_maintainer_session_id = state
        .get("maintainer_session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    if let Some(session_id) = legacy_maintainer_session_id.as_deref() {
        candidates.push((session_id.to_owned(), true));
    }
    if let Some(session_id) = state
        .get("agent_role_last_session_ids")
        .and_then(Value::as_object)
        .and_then(|last| last.get("maintainer"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        candidates.push((session_id.to_owned(), false));
    }

    for (session_id, from_legacy_field) in candidates {
        let Some(session) = sessions.iter().find(|session| session.id == session_id) else {
            continue;
        };
        if !session.is_live_for_registry() {
            continue;
        }
        if !from_legacy_field && !session_has_maintainer_identity(session) {
            continue;
        }
        upsert_raw_registration(state, "maintainer", &session_id, &now_rfc3339())?;
        remember_role_last_session_raw(state, "maintainer", &session_id)?;
        sync_maintainer_alias_raw(state)?;
        return Ok(true);
    }
    if let Some(session_id) = legacy_maintainer_session_id {
        remember_role_last_session_raw(state, "maintainer", &session_id)?;
        sync_maintainer_alias_raw(state)?;
        return Ok(true);
    }
    Ok(false)
}

fn session_has_maintainer_identity(session: &SessionRecord) -> bool {
    [session.friendly_name.as_deref(), session.role.as_deref()]
        .into_iter()
        .flatten()
        .any(|value| value.trim().eq_ignore_ascii_case("maintainer"))
}

fn prune_agent_registrations_raw(state: &mut Value) -> Result<bool> {
    let live_session_ids = snapshot_from_raw_value(state)?
        .into_sessions()
        .into_iter()
        .filter(SessionRecord::is_live_for_registry)
        .map(|session| session.id)
        .collect::<BTreeSet<_>>();
    let mut removed = Vec::<AgentRegistrationRecord>::new();
    {
        let registrations = ensure_agent_registrations_array_mut(state)?;
        registrations.retain(|entry| {
            let Some(registration) = raw_registration_record(entry) else {
                return false;
            };
            if live_session_ids.contains(&registration.session_id) {
                return true;
            }
            removed.push(registration);
            false
        });
    }
    for registration in &removed {
        remember_role_last_session_raw(state, &registration.role, &registration.session_id)?;
    }
    if !removed.is_empty() {
        sync_maintainer_alias_raw(state)?;
    }
    Ok(!removed.is_empty())
}

fn agent_registration_responses_from_state(
    state: &Value,
) -> Result<Vec<AgentRegistrationResponse>> {
    let sessions = snapshot_from_raw_value(state)?.into_sessions();
    let sessions_by_id = sessions
        .into_iter()
        .map(|session| (session.id.clone(), session))
        .collect::<BTreeMap<_, _>>();
    let mut responses = state
        .get("agent_registrations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(raw_registration_record)
        .filter_map(|registration| {
            let session = sessions_by_id.get(&registration.session_id)?;
            Some(agent_registration_response(
                &registration.role,
                session,
                registration.created_at.as_deref(),
            ))
        })
        .collect::<Vec<_>>();
    responses.sort_by(|left, right| left.role.cmp(&right.role));
    Ok(responses)
}

fn agent_registration_response(
    role: &str,
    session: &SessionRecord,
    created_at: Option<&str>,
) -> AgentRegistrationResponse {
    let status = session.lifecycle_status();
    let activity_state = projected_activity_state(session, status);
    AgentRegistrationResponse {
        role: normalize_role(role),
        session_id: session.id.clone(),
        friendly_name: session.cached_display_name(),
        provider: Some(non_empty_or(session.provider.clone(), "claude")),
        status: status.to_owned(),
        activity_state,
        created_at: created_at
            .map(ToOwned::to_owned)
            .unwrap_or_else(now_rfc3339),
    }
}

fn session_object_mut<'a>(
    sessions: &'a mut [Value],
    session_id: &str,
) -> Option<&'a mut Map<String, Value>> {
    sessions.iter_mut().find_map(|value| {
        if value.get("id").and_then(Value::as_str) == Some(session_id) {
            value.as_object_mut()
        } else {
            None
        }
    })
}

fn session_object<'a>(sessions: &'a [Value], session_id: &str) -> Option<&'a Map<String, Value>> {
    sessions.iter().find_map(|value| {
        if value.get("id").and_then(Value::as_str) == Some(session_id) {
            value.as_object()
        } else {
            None
        }
    })
}

fn direct_children(sessions: &[SessionRecord], parent_session_id: &str) -> Vec<SessionRecord> {
    sessions
        .iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(parent_session_id))
        .cloned()
        .collect()
}

fn collect_descendants_preorder(
    sessions: &[SessionRecord],
    parent_session_id: &str,
    visited: &mut BTreeSet<String>,
    descendants: &mut Vec<SessionRecord>,
) {
    for child in direct_children(sessions, parent_session_id) {
        if !visited.insert(child.id.clone()) {
            continue;
        }
        let child_id = child.id.clone();
        descendants.push(child);
        collect_descendants_preorder(sessions, &child_id, visited, descendants);
    }
}

fn reset_session_after_clear(session: &mut Map<String, Value>, now: &str) {
    // A clear starts a new accumulation cycle. Claude also reports this through
    // its SessionStart(clear) hook, but codex has no equivalent hook, so without
    // this a cleared codex session's latches would stay set and suppress every
    // warning in the new cycle.
    reset_context_oneshot_flags(session);
    clear_context_snapshot(session);
    session.insert("context_compaction_active".to_owned(), Value::Bool(false));
    session.insert("agent_status_text".to_owned(), Value::Null);
    session.insert("agent_status_at".to_owned(), Value::Null);
    session.insert("agent_task_completed_at".to_owned(), Value::Null);
    session.insert("completion_status".to_owned(), Value::Null);
    session.insert("completion_message".to_owned(), Value::Null);
    session.insert("completed_at".to_owned(), Value::Null);
    session.insert("last_activity".to_owned(), Value::String(now.to_owned()));
    mark_review_dispatch_completed(session, now);
}

fn mark_session_followup_activity(session: &mut Map<String, Value>, now: &str) {
    session.insert("agent_task_completed_at".to_owned(), Value::Null);
    session.insert("last_activity".to_owned(), Value::String(now.to_owned()));
}

fn mark_session_retired(session: &mut Map<String, Value>, now: &str) {
    session.insert(
        "completion_status".to_owned(),
        Value::String("retired".to_owned()),
    );
    session.insert(
        "completion_message".to_owned(),
        Value::String("Retired via sm retire".to_owned()),
    );
    session.insert("completed_at".to_owned(), Value::String(now.to_owned()));
}

fn clear_authorization_error(
    session: &Map<String, Value>,
    requester_session_id: Option<&str>,
) -> Option<String> {
    let parent_id = json_text(session.get("parent_session_id"));
    let requester_session_id = requester_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(requester_session_id) = requester_session_id {
        if parent_id.as_deref() != Some(requester_session_id) {
            return Some(format!(
                "Not authorized. You can only clear your child sessions. Target session parent: {}",
                parent_id.as_deref().unwrap_or("none")
            ));
        }
    } else if parent_id.is_none() {
        return Some("Can only clear child sessions. Target session has no parent.".to_owned());
    }
    None
}

fn json_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn subagent_response_from_value(value: &Value) -> Result<SubagentResponse> {
    Ok(SubagentResponse {
        agent_id: json_text(value.get("agent_id")).unwrap_or_default(),
        agent_type: json_text(value.get("agent_type")).unwrap_or_else(|| "unknown".to_owned()),
        parent_session_id: json_text(value.get("parent_session_id")).unwrap_or_default(),
        started_at: json_text(value.get("started_at")).unwrap_or_default(),
        stopped_at: json_text(value.get("stopped_at")),
        status: json_text(value.get("status")).unwrap_or_else(|| "running".to_owned()),
        summary: json_text(value.get("summary")),
    })
}

fn provider_resume_id_from_transcript_path(transcript_path: &str) -> Option<String> {
    claude_session_id_from_transcript(transcript_path).or_else(|| {
        let path = Path::new(transcript_path);
        let nested_session_dir = if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name == "chat.jsonl")
        {
            path.parent()
        } else if path
            .parent()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            == Some("subagents")
        {
            path.parent().and_then(Path::parent)
        } else {
            None
        };
        nested_session_dir
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|value| value.to_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
            })
    })
}

fn claude_session_id_from_transcript(transcript_path: &str) -> Option<String> {
    let file = fs::File::open(expand_home(transcript_path)).ok()?;
    BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
        .take(128)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .find_map(|value| {
            value
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
}

fn provider_resume_id_for_restore(record: &SessionRecord) -> Option<String> {
    if record.provider == "claude" {
        if let Some(resume_id) = record
            .transcript_path
            .as_deref()
            .and_then(provider_resume_id_from_transcript_path)
        {
            return Some(resume_id);
        }
    }
    record
        .provider_resume_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn wait_for_codex_cli_provider_resume_id(
    record: &SessionRecord,
    sessions_root: &Path,
    excluded_ids: &BTreeSet<String>,
    launched_at_ns: i128,
    timeout: Duration,
) -> Option<String> {
    let started = Instant::now();
    loop {
        if let Some(resume_id) = discover_codex_cli_resume_id(
            record,
            sessions_root,
            excluded_ids,
            CodexCliSessionDiscoveryMode::Creation { launched_at_ns },
        ) {
            return Some(resume_id);
        }
        if started.elapsed() >= timeout {
            return None;
        }
        thread::sleep(CODEX_CLI_SESSION_BIND_POLL);
    }
}

#[derive(Clone, Copy)]
enum CodexCliSessionDiscoveryMode {
    Creation { launched_at_ns: i128 },
    Restore,
}

fn discover_codex_cli_resume_id(
    record: &SessionRecord,
    sessions_root: &Path,
    excluded_ids: &BTreeSet<String>,
    mode: CodexCliSessionDiscoveryMode,
) -> Option<String> {
    if record.provider != "codex" || record.working_dir.trim().is_empty() {
        return None;
    }
    if !sessions_root.is_dir() {
        return None;
    }

    let resolved_working_dir = resolve_path_lossy(expand_home(&record.working_dir));
    let target_time_ns = match mode {
        CodexCliSessionDiscoveryMode::Creation { launched_at_ns } => launched_at_ns,
        CodexCliSessionDiscoveryMode::Restore => {
            parse_timestamp_ns(&record.created_at).unwrap_or(0)
        }
    };
    let mut candidates = Vec::new();

    for path in codex_cli_session_files(record, sessions_root) {
        let Some((candidate_id, candidate_cwd, started_at_ns)) =
            read_codex_cli_session_metadata(&path)
        else {
            continue;
        };
        if excluded_ids.contains(&candidate_id)
            || resolve_path_lossy(expand_home(&candidate_cwd)) != resolved_working_dir
        {
            continue;
        }
        let started_at_ns = match (mode, started_at_ns) {
            (CodexCliSessionDiscoveryMode::Creation { launched_at_ns }, Some(started_at_ns))
                if started_at_ns >= launched_at_ns =>
            {
                started_at_ns
            }
            (CodexCliSessionDiscoveryMode::Creation { .. }, _) => continue,
            (CodexCliSessionDiscoveryMode::Restore, started_at_ns) => started_at_ns.unwrap_or(0),
        };
        let distance = if started_at_ns == 0 {
            i128::MAX
        } else {
            (target_time_ns - started_at_ns).abs()
        };
        candidates.push((distance, -started_at_ns, candidate_id));
    }

    candidates.sort();
    candidates
        .into_iter()
        .next()
        .map(|(_, _, candidate_id)| candidate_id)
}

fn codex_cli_existing_session_ids(
    record: &SessionRecord,
    sessions_root: &Path,
) -> BTreeSet<String> {
    codex_cli_session_files(record, sessions_root)
        .into_iter()
        .filter_map(|path| {
            read_codex_cli_session_metadata(&path).map(|(session_id, _, _)| session_id)
        })
        .collect()
}

fn codex_cli_session_files(record: &SessionRecord, sessions_root: &Path) -> Vec<PathBuf> {
    let base_date = parse_timestamp(&record.created_at)
        .unwrap_or_else(OffsetDateTime::now_utc)
        .date();
    let mut session_files = Vec::new();
    for day_offset in [-1, 0, 1] {
        let Some(date) = base_date.checked_add(TimeDuration::days(day_offset)) else {
            continue;
        };
        let day_dir = sessions_root
            .join(format!("{:04}", date.year()))
            .join(format!("{:02}", date.month() as u8))
            .join(format!("{:02}", date.day()));
        let Ok(entries) = fs::read_dir(day_dir) else {
            continue;
        };
        session_files.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|file_name| file_name.starts_with("rollout-"))
                && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
        }));
    }
    session_files
}

fn read_codex_cli_session_metadata(path: &Path) -> Option<(String, String, Option<i128>)> {
    let first_line = BufReader::new(fs::File::open(path).ok()?)
        .lines()
        .next()?
        .ok()?;
    let record = serde_json::from_str::<Value>(&first_line).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = record.get("payload")?.as_object()?;
    let session_id = payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_owned();
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_owned();
    let started_at_ns = payload
        .get("timestamp")
        .or_else(|| record.get("timestamp"))
        .and_then(Value::as_str)
        .and_then(parse_timestamp_ns);
    Some((session_id, cwd, started_at_ns))
}

#[derive(Debug, Default)]
struct ClaudeTranscriptMetadata {
    mtime_ns: i128,
    session_id: Option<String>,
    started_at_ns: Option<i128>,
    ended_at_ns: Option<i128>,
    cwd: Option<String>,
}

fn historical_claude_seat_sessions(
    sessions: &[SessionRecord],
    projects_roots: &[PathBuf],
    artifact_cutoff_ns: i128,
    claimed: &mut BTreeSet<(String, String)>,
) -> Vec<SeatSessionIdentity> {
    let mut project_dirs = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for projects_root in projects_roots {
        for session in sessions
            .iter()
            .filter(|session| session.provider == "claude")
            .filter(|session| has_text(Some(&session.working_dir)))
        {
            project_dirs
                .entry(projects_root.join(claude_project_dir_name(&session.working_dir)))
                .or_default()
                .insert(resolve_path_lossy(expand_home(&session.working_dir)));
        }
    }

    let mut identities = Vec::new();
    for (project_dir, project_working_dirs) in project_dirs {
        for path in jsonl_files_recursive(&project_dir) {
            let Ok(metadata) = read_claude_transcript_metadata(&path) else {
                continue;
            };
            let Some(provider_session_id) = metadata.session_id else {
                continue;
            };
            let claim = ("claude".to_owned(), provider_session_id.clone());
            if claimed.contains(&claim) {
                continue;
            }
            let artifact_start = metadata.started_at_ns.unwrap_or(metadata.mtime_ns);
            let artifact_end = metadata.ended_at_ns.unwrap_or(metadata.mtime_ns);
            if artifact_start > artifact_cutoff_ns {
                continue;
            }
            let matching_seats = sessions
                .iter()
                .filter(|session| session.provider == "claude")
                .filter(|session| {
                    let working_dir = resolve_path_lossy(expand_home(&session.working_dir));
                    metadata.cwd.as_deref().map_or_else(
                        || project_working_dirs.contains(&working_dir),
                        |cwd| cwd == working_dir,
                    )
                })
                .filter(|session| session_lifetime_overlaps(session, artifact_start, artifact_end))
                .map(|session| session.id.as_str())
                .collect::<BTreeSet<_>>();
            if matching_seats.is_empty() {
                continue;
            }
            let seat_id = if matching_seats.len() == 1 {
                matching_seats.into_iter().next().unwrap().to_owned()
            } else {
                "unassigned".to_owned()
            };
            claimed.insert(claim);
            identities.push(SeatSessionIdentity {
                seat_id,
                provider: "claude".to_owned(),
                provider_session_id,
                artifact_path: Some(resolve_path_lossy(path)),
            });
        }
    }
    identities
}

fn historical_codex_cli_seat_sessions(
    sessions: &[SessionRecord],
    sessions_root: &Path,
    artifact_cutoff_ns: i128,
    claimed: &mut BTreeSet<(String, String)>,
) -> Vec<SeatSessionIdentity> {
    let codex_seats = sessions
        .iter()
        .filter(|session| session.provider == "codex")
        .collect::<Vec<_>>();
    if codex_seats.is_empty() {
        return Vec::new();
    }

    let mut identities = Vec::new();
    for path in jsonl_files_recursive(sessions_root) {
        let Some((provider_session_id, cwd, started_at_ns)) =
            read_codex_cli_session_metadata(&path)
        else {
            continue;
        };
        let claim = ("codex".to_owned(), provider_session_id.clone());
        if claimed.contains(&claim) {
            continue;
        }

        let cwd = resolve_path_lossy(expand_home(&cwd));
        let matching_cwd_seats = codex_seats
            .iter()
            .copied()
            .filter(|session| resolve_path_lossy(expand_home(&session.working_dir)) == cwd)
            .collect::<Vec<_>>();
        if matching_cwd_seats.is_empty() {
            continue;
        }

        let started_at_ns = started_at_ns
            .or_else(|| file_mtime_ns(&path).ok())
            .unwrap_or(0);
        if started_at_ns > artifact_cutoff_ns {
            continue;
        }
        let ended_at_ns = codex_transcript_end_ns(&path, started_at_ns);
        let matching_seats = matching_cwd_seats
            .into_iter()
            .filter(|session| session_lifetime_overlaps(session, started_at_ns, ended_at_ns))
            .map(|session| session.id.as_str())
            .collect::<BTreeSet<_>>();
        if matching_seats.is_empty() {
            continue;
        }
        let seat_id = if matching_seats.len() == 1 {
            matching_seats.into_iter().next().unwrap().to_owned()
        } else {
            "unassigned".to_owned()
        };
        claimed.insert(claim);
        identities.push(SeatSessionIdentity {
            seat_id,
            provider: "codex".to_owned(),
            provider_session_id,
            artifact_path: Some(resolve_path_lossy(path)),
        });
    }
    identities
}

fn codex_cli_artifact_paths(
    sessions_root: &Path,
    provider_session_ids: &BTreeSet<String>,
) -> BTreeMap<String, PathBuf> {
    if provider_session_ids.is_empty() {
        return BTreeMap::new();
    }
    let mut paths = BTreeMap::new();
    for path in jsonl_files_recursive(sessions_root) {
        let Some((provider_session_id, _, _)) = read_codex_cli_session_metadata(&path) else {
            continue;
        };
        if provider_session_ids.contains(&provider_session_id) {
            paths.entry(provider_session_id).or_insert(path);
        }
    }
    paths
}

fn codex_transcript_end_ns(path: &Path, started_at_ns: i128) -> i128 {
    let mut ended_at_ns = started_at_ns;
    let Ok(file) = fs::File::open(path) else {
        return ended_at_ns;
    };
    for line in BufReader::new(file).lines().map_while(|line| line.ok()) {
        let Some(timestamp) = serde_json::from_str::<Value>(&line).ok().and_then(|value| {
            value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_timestamp_ns)
        }) else {
            continue;
        };
        ended_at_ns = ended_at_ns.max(timestamp);
    }
    ended_at_ns
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

fn historical_codex_fork_seat_sessions(
    sessions: &[SessionRecord],
    runtime: &TmuxRuntime,
    claimed: &mut BTreeSet<(String, String)>,
) -> Vec<SeatSessionIdentity> {
    let mut identities = Vec::new();
    for session in sessions
        .iter()
        .filter(|session| session.provider == "codex-fork")
    {
        let Some(event_stream_path) = codex_fork_event_stream_path(session, runtime) else {
            continue;
        };
        let Ok(file) = fs::File::open(&event_stream_path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(|line| line.ok()) {
            let Some(provider_session_id) =
                serde_json::from_str::<Value>(&line).ok().and_then(|value| {
                    value
                        .as_object()
                        .and_then(extract_any_codex_fork_thread_started)
                })
            else {
                continue;
            };
            let claim = ("codex-fork".to_owned(), provider_session_id.clone());
            if !claimed.insert(claim) {
                continue;
            }
            identities.push(SeatSessionIdentity {
                seat_id: session.id.clone(),
                provider: "codex-fork".to_owned(),
                provider_session_id,
                artifact_path: Some(event_stream_path.display().to_string()),
            });
        }
    }
    identities
}

fn codex_fork_event_stream_path(session: &SessionRecord, runtime: &TmuxRuntime) -> Option<PathBuf> {
    if session.provider != "codex-fork" {
        return None;
    }
    let log_file = session.log_file.as_deref().map(expand_home)?;
    let spec = TmuxSessionSpec {
        session_id: session.id.clone(),
        session_credential: None,
        tmux_session: session.tmux_session.clone(),
        working_dir: session.working_dir.clone(),
        log_file: log_file.clone(),
        provider: session.provider.clone(),
        initial_message: None,
        force_initial_prompt_stdin: false,
        model: session.model.clone(),
        reasoning_effort: session.reasoning_effort.clone(),
    };
    runtime
        .codex_fork_runtime_artifacts(&spec)
        .ok()
        .flatten()
        .map(|artifacts| {
            codex_fork_newest_event_stream_path(
                &session.id,
                &log_file,
                &artifacts.event_stream_path,
            )
        })
}

pub(crate) fn codex_fork_legacy_event_stream_path_from_log_file(
    log_file: &Path,
) -> Option<PathBuf> {
    let stem = log_file.file_stem()?.to_str()?.trim();
    if stem.is_empty() {
        return None;
    }
    Some(log_file.with_file_name(format!("{stem}.codex-fork.events.jsonl")))
}

pub(crate) fn codex_fork_newest_event_stream_path(
    _session_id: &str,
    log_file: &Path,
    derived_path: &Path,
) -> PathBuf {
    let mut candidates = vec![derived_path.to_path_buf()];
    if let Some(legacy_path) = codex_fork_legacy_event_stream_path_from_log_file(log_file) {
        candidates.push(legacy_path);
    }
    candidates
        .iter()
        .filter_map(|path| {
            path.metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(|modified| (modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path.to_path_buf())
        .unwrap_or_else(|| derived_path.to_path_buf())
}

fn session_lifetime_overlaps(
    session: &SessionRecord,
    artifact_start: i128,
    artifact_end: i128,
) -> bool {
    let Some(seat_start) = parse_timestamp_ns(&session.created_at) else {
        return false;
    };
    let seat_end = if session.is_stopped() {
        session
            .stopped_at
            .as_deref()
            .and_then(parse_timestamp_ns)
            .or_else(|| parse_timestamp_ns(&session.last_activity))
            .unwrap_or(seat_start)
    } else {
        i128::MAX
    };
    artifact_end >= seat_start && artifact_start <= seat_end
}

fn discover_claude_transcript_path(
    record: &SessionRecord,
    sessions: &[SessionRecord],
    projects_roots: &[PathBuf],
) -> Option<String> {
    if record.provider != "claude" || !has_text(Some(record.working_dir.as_str())) {
        return record.transcript_path.clone();
    }
    if has_text(record.transcript_path.as_deref()) {
        return record.transcript_path.clone();
    }

    let resolved_working_dir = resolve_path_lossy(expand_home(&record.working_dir));
    let claimed_paths = sessions
        .iter()
        .filter(|session| session.id != record.id)
        .filter_map(|session| session.transcript_path.as_deref())
        .filter(|path| has_text(Some(path)))
        .map(|path| resolve_path_lossy(expand_home(path)))
        .collect::<BTreeSet<_>>();
    let claimed_provider_session_ids = sessions
        .iter()
        .filter(|session| session.id != record.id && session.provider == "claude")
        .filter_map(provider_resume_id_for_restore)
        .collect::<BTreeSet<_>>();
    let target_time_ns = session_time_ns(record)
        .or_else(|| file_mtime_ns(expand_home(&record.log_file.clone().unwrap_or_default())).ok())
        .unwrap_or(0);

    let mut candidates: Vec<(bool, i128, i128, i128, String)> = Vec::new();
    for projects_root in projects_roots {
        let project_dir = projects_root.join(claude_project_dir_name(&record.working_dir));
        for path in jsonl_files_recursive(&project_dir) {
            let resolved_transcript = resolve_path_lossy(path.clone());
            if claimed_paths.contains(&resolved_transcript) {
                continue;
            }
            let metadata = read_claude_transcript_metadata(&path).unwrap_or_default();
            let candidate_provider_session_id = metadata
                .session_id
                .clone()
                .or_else(|| provider_resume_id_from_transcript_path(&resolved_transcript));
            if candidate_provider_session_id
                .as_ref()
                .is_some_and(|session_id| claimed_provider_session_ids.contains(session_id))
            {
                continue;
            }
            if let Some(cwd) = metadata.cwd.as_deref() {
                if cwd != resolved_working_dir {
                    continue;
                }
            }
            let comparison_time = if metadata.mtime_ns > 0 {
                metadata.mtime_ns
            } else {
                metadata.started_at_ns.unwrap_or(0)
            };
            let distance = (target_time_ns - comparison_time).abs();
            let start_distance = metadata
                .started_at_ns
                .map(|started_at_ns| (target_time_ns - started_at_ns).abs())
                .unwrap_or(distance);
            candidates.push((
                path.parent()
                    .and_then(Path::file_name)
                    .and_then(|value| value.to_str())
                    == Some("subagents"),
                distance,
                start_distance,
                -metadata.mtime_ns,
                resolved_transcript,
            ));
        }
    }

    candidates.sort();
    candidates.into_iter().next().map(|(_, _, _, _, path)| path)
}

fn read_claude_transcript_metadata(path: &Path) -> Result<ClaudeTranscriptMetadata> {
    let mtime_ns = file_mtime_ns(path)?;
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut metadata = ClaudeTranscriptMetadata {
        mtime_ns,
        ..ClaudeTranscriptMetadata::default()
    };
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        if metadata.session_id.is_none() {
            metadata.session_id = object
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
        }
        if let Some(timestamp) = object
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ns)
        {
            metadata.started_at_ns = Some(
                metadata
                    .started_at_ns
                    .map_or(timestamp, |started| started.min(timestamp)),
            );
            metadata.ended_at_ns = Some(
                metadata
                    .ended_at_ns
                    .map_or(timestamp, |ended| ended.max(timestamp)),
            );
        }
        if object.get("type").and_then(Value::as_str) == Some("user") && metadata.cwd.is_none() {
            if let Some(cwd) = object
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                metadata.cwd = Some(resolve_path_lossy(expand_home(cwd)));
            }
        }
    }
    Ok(metadata)
}

fn claude_project_dir_name(working_dir: &str) -> String {
    resolve_path_lossy(expand_home(working_dir)).replace(std::path::MAIN_SEPARATOR, "-")
}

fn resolve_path_lossy(path: PathBuf) -> String {
    path.canonicalize()
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn session_time_ns(record: &SessionRecord) -> Option<i128> {
    [record.last_activity.as_str(), record.created_at.as_str()]
        .into_iter()
        .filter_map(parse_timestamp_ns)
        .max()
}

fn parse_timestamp_ns(value: &str) -> Option<i128> {
    parse_timestamp(value).map(OffsetDateTime::unix_timestamp_nanos)
}

fn parse_timestamp(value: &str) -> Option<OffsetDateTime> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(parsed) = OffsetDateTime::parse(value, &Rfc3339) {
        return Some(parsed);
    }
    parse_python_naive_datetime(value).map(PrimitiveDateTime::assume_utc)
}

fn file_mtime_ns(path: impl AsRef<Path>) -> Result<i128> {
    let modified = fs::metadata(path)?.modified()?;
    let duration = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
    Ok(i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX))
}

fn append_log_line(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open session log {}", path.display()))?;
    writeln!(file, "{line}")
        .with_context(|| format!("failed to append session log {}", path.display()))?;
    Ok(())
}

fn core_session_provider_and_working_dir(
    sessions: &[Value],
    request: &CreateCoreSessionRequest,
) -> (String, String) {
    let parent_working_dir = request.parent_session_id.as_deref().and_then(|parent_id| {
        sessions
            .iter()
            .find(|value| value.get("id").and_then(Value::as_str) == Some(parent_id))
            .and_then(|value| json_text(value.get("working_dir")))
    });
    let provider = request
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("claude")
        .to_owned();
    let working_dir = request
        .working_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(parent_working_dir.as_deref())
        .unwrap_or(".")
        .to_owned();
    (provider, working_dir)
}

fn core_log_file_path(state_file: &Path, log_dir: Option<&Path>, session_id: &str) -> PathBuf {
    let safe_id = sanitize_path_component(session_id);
    let id_hash = stable_session_id_hash(session_id);
    let dir = log_dir
        .map(Path::to_path_buf)
        .or_else(|| state_file.parent().map(|parent| parent.join("logs")))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join(format!("{safe_id}-{id_hash}.log"))
}

fn core_tmux_session_name(provider: &str, session_id: &str) -> String {
    let safe_provider = sanitize_path_component(provider);
    let safe_id = sanitize_path_component(session_id);
    let id_hash = stable_session_id_hash(session_id);
    format!("sm-rust-{safe_provider}-{safe_id}-{id_hash}")
}

fn sanitize_path_component(value: &str) -> String {
    let mut safe = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .collect::<String>();
    if safe.is_empty() {
        safe = "session".to_owned();
    }
    safe
}

fn stable_session_id_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut hash = String::with_capacity(12);
    for byte in &digest[..6] {
        hash.push(hex_char(byte >> 4));
        hash.push(hex_char(byte & 0x0f));
    }
    hash
}

fn hex_char(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + (value - 10)),
        _ => unreachable!("hex nibble out of range"),
    }
}

fn generate_session_id() -> String {
    let mut bytes = [0u8; 4];
    OsRng.fill_bytes(&mut bytes);
    let mut id = String::with_capacity(8);
    for byte in bytes {
        id.push(hex_char(byte >> 4));
        id.push(hex_char(byte & 0x0f));
    }
    id
}

fn generate_unique_session_id(sessions: &[Value]) -> Result<String> {
    for _ in 0..64 {
        let session_id = generate_session_id();
        if !session_id_exists(sessions, &session_id) {
            return Ok(session_id);
        }
    }
    anyhow::bail!("failed to generate unique session id after 64 attempts")
}

fn generate_unique_reparent_request_id(records: &[ReparentRequestRecord]) -> Result<String> {
    for _ in 0..64 {
        let mut bytes = [0u8; 6];
        OsRng.fill_bytes(&mut bytes);
        let mut id = String::with_capacity(12);
        for byte in bytes {
            id.push(hex_char(byte >> 4));
            id.push(hex_char(byte & 0x0f));
        }
        if !records.iter().any(|record| record.id == id) {
            return Ok(id);
        }
    }
    anyhow::bail!("failed to generate unique reparent request id after 64 attempts")
}

fn generate_unique_runtime_launch_id(records: &[SessionRuntimeLaunchRecord]) -> Result<String> {
    for _ in 0..64 {
        let id = generate_session_id();
        if !records.iter().any(|record| record.id == id) {
            return Ok(id);
        }
    }
    anyhow::bail!("failed to generate unique runtime launch id after 64 attempts")
}

fn generate_unique_credential_rotation_id(
    records: &[SessionCredentialRotationRecord],
) -> Result<String> {
    for _ in 0..64 {
        let id = generate_session_id();
        if !records.iter().any(|record| record.id == id) {
            return Ok(id);
        }
    }
    anyhow::bail!("failed to generate unique credential rotation id after 64 attempts")
}

fn generate_session_credential() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut credential = String::with_capacity(64);
    for byte in bytes {
        credential.push(hex_char(byte >> 4));
        credential.push(hex_char(byte & 0x0f));
    }
    credential
}

fn sha256_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_bytes(value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
    format!("{:x}", hasher.finalize())
}

fn validate_spawn_brief_source(source: &SpawnBriefSource) -> Result<()> {
    match source.kind.as_str() {
        "positional" | "stdin" if source.path.is_none() => Ok(()),
        "file"
            if source
                .path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty()) =>
        {
            Ok(())
        }
        _ => anyhow::bail!("invalid spawn prompt source metadata"),
    }
}

fn persist_spawn_brief_artifact(
    state_file: &Path,
    bytes: &[u8],
    source: SpawnBriefSource,
) -> Result<SpawnBriefArtifact> {
    let state_dir = state_file.parent().unwrap_or_else(|| Path::new("."));
    let artifact_dir = state_dir.join("spawn-briefs");
    fs::create_dir_all(&artifact_dir).with_context(|| {
        format!(
            "failed to create spawn brief directory {}",
            artifact_dir.display()
        )
    })?;
    #[cfg(unix)]
    fs::set_permissions(&artifact_dir, fs::Permissions::from_mode(0o700))?;
    let sha256 = sha256_bytes(bytes);
    let path = artifact_dir.join(format!("{sha256}.md"));
    match write_and_publish_spawn_brief(&artifact_dir, &path, bytes) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            verify_spawn_brief_artifact(&path, bytes)?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to publish spawn brief {}", path.display()));
        }
    }
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
    sync_spawn_brief_directory(&artifact_dir)?;
    Ok(SpawnBriefArtifact {
        sha256,
        path: path.display().to_string(),
        byte_length: bytes.len(),
        source,
    })
}

fn write_and_publish_spawn_brief(artifact_dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (temporary_path, mut file) = (0..32)
        .find_map(|_| {
            let mut random = [0_u8; 8];
            OsRng.fill_bytes(&mut random);
            let temporary_path = artifact_dir.join(format!(
                ".spawn-brief-{:016x}.tmp",
                u64::from_be_bytes(random)
            ));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&temporary_path) {
                Ok(file) => Some(Ok((temporary_path, file))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not reserve spawn brief temporary path",
            )
        })?;

    let write_result = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        fs::set_permissions(&temporary_path, fs::Permissions::from_mode(0o400))?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    let publish_result = fs::hard_link(&temporary_path, path);
    let _ = fs::remove_file(&temporary_path);
    publish_result
}

fn verify_spawn_brief_artifact(path: &Path, bytes: &[u8]) -> Result<()> {
    let existing = fs::read(path)
        .with_context(|| format!("failed to verify spawn brief {}", path.display()))?;
    if existing != bytes {
        anyhow::bail!("existing spawn brief artifact digest does not match its contents");
    }
    Ok(())
}

/// Persist the directory entry before its path is recorded in session state.
/// Without this barrier, a machine crash could preserve the intent state while
/// losing a just-published hard link.
fn sync_spawn_brief_directory(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(directory)
            .with_context(|| {
                format!(
                    "failed to open spawn brief directory {}",
                    directory.display()
                )
            })?
            .sync_all()
            .with_context(|| {
                format!(
                    "failed to sync spawn brief directory {}",
                    directory.display()
                )
            })?;
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

fn bind_spawn_launch_intent_in_state(
    state: &mut Value,
    intent_id: &str,
    session_id: &str,
) -> Result<()> {
    let Some(intents) = state
        .get_mut("spawn_launch_intents")
        .and_then(Value::as_array_mut)
    else {
        anyhow::bail!("spawn launch intent {intent_id} is missing");
    };
    let Some(intent) = intents
        .iter_mut()
        .find(|intent| intent.get("id").and_then(Value::as_str) == Some(intent_id))
    else {
        anyhow::bail!("spawn launch intent {intent_id} is missing");
    };
    intent["session_id"] = Value::String(session_id.to_owned());
    Ok(())
}

fn generate_unique_spawn_launch_intent_id(intents: &[Value]) -> Result<String> {
    for _ in 0..32 {
        let mut bytes = [0_u8; 8];
        OsRng.fill_bytes(&mut bytes);
        let id = format!("spawn-brief-{:016x}", u64::from_be_bytes(bytes));
        if !intents
            .iter()
            .any(|intent| intent.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            return Ok(id);
        }
    }
    anyhow::bail!("failed to generate a unique spawn launch intent ID")
}

fn constant_time_text_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn session_credential_matches(
    sessions: &[SessionRecord],
    session_id: &str,
    credential: &str,
) -> bool {
    let session_id = session_id.trim();
    let credential = credential.trim();
    if session_id.is_empty() || credential.is_empty() {
        return false;
    }
    let Some(expected) = sessions
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.session_credential_sha256.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    constant_time_text_eq(expected, &sha256_text(credential))
}

fn credential_rotation_has_fresh_idle_proof(
    rotation: &SessionCredentialRotationRecord,
    session: &SessionRecord,
) -> bool {
    if session.is_stopped() {
        return false;
    }
    match session.provider.as_str() {
        "claude" => {
            normalized_status(&session.status) == "idle"
                && session
                    .activity_hook_at
                    .as_deref()
                    .is_some_and(|proof_at| timestamp_is_after(proof_at, &rotation.requested_at))
        }
        "codex-fork" => {
            normalized_status(&session.status) == "idle"
                && timestamp_is_after(&session.last_activity, &rotation.requested_at)
        }
        // Classic Codex has no durable lifecycle event stream. Its paired
        // session_input_ready check is a fresh composer observation performed
        // after the request and repeated under the input fence.
        "codex" => true,
        _ => false,
    }
}

fn session_id_exists(sessions: &[Value], session_id: &str) -> bool {
    sessions
        .iter()
        .any(|value| value.get("id").and_then(Value::as_str) == Some(session_id))
}

fn reparent_request_records(state: &Value) -> Result<Vec<ReparentRequestRecord>> {
    state
        .get("reparent_requests")
        .and_then(Value::as_array)
        .map(|records| {
            records
                .iter()
                .map(|record| {
                    serde_json::from_value(record.clone())
                        .context("failed to parse reparent request record")
                })
                .collect()
        })
        .unwrap_or_else(|| Ok(Vec::new()))
}

fn store_reparent_request_records(
    state: &mut Value,
    records: &[ReparentRequestRecord],
) -> Result<()> {
    // Notification intents are part of the durable reparent state machine,
    // not an observation made later by a polling GET.  Every request-state
    // write therefore carries its exact, idempotency-keyed outbox projection.
    // This also lets a later worker retry an enqueue after a process crash
    // without requiring another client request to repair the record.
    let mut records = records.to_vec();
    reconcile_reparent_notification_intents(&mut records);
    let object = state
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("session state root must be an object"))?;
    object.insert(
        "reparent_requests".to_owned(),
        serde_json::to_value(records)?,
    );
    Ok(())
}

fn reconcile_reparent_notification_intents(records: &mut [ReparentRequestRecord]) -> bool {
    let mut changed = false;
    for record in records {
        let desired = desired_reparent_notifications(record);
        let desired_keys = desired
            .iter()
            .map(|desired| desired.intent.key.as_str())
            .collect::<BTreeSet<_>>();
        // Preserve enqueued audit history, but discard an unqueued terminal
        // projection if a later durable outcome superseded it.
        let previous_len = record.notification_intents.len();
        record.notification_intents.retain(|intent| {
            intent.enqueued_at.is_some()
                || !intent.event.starts_with("terminal:")
                || desired_keys.contains(intent.key.as_str())
        });
        changed |= record.notification_intents.len() != previous_len;
        for desired in desired {
            if record
                .notification_intents
                .iter()
                .all(|intent| intent.key != desired.intent.key)
            {
                record.notification_intents.push(desired.intent);
                changed = true;
            }
        }
    }
    changed
}

fn reparent_apply_lease(state: &Value) -> Result<Option<ReparentApplyLease>> {
    match state.get("reparent_apply_lease") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .context("failed to parse reparent apply lease")
            .map(Some),
    }
}

fn store_reparent_apply_lease(state: &mut Value, lease: Option<&ReparentApplyLease>) -> Result<()> {
    let object = state
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("session state root must be an object"))?;
    object.insert(
        "reparent_apply_lease".to_owned(),
        lease
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Value::Null),
    );
    Ok(())
}

fn active_reparent_request_for_session(state: &Value, session_id: &str) -> Result<Option<String>> {
    Ok(reparent_request_records(state)?
        .into_iter()
        .find(|record| {
            record.is_apply_fenced() && record.affected_session_ids().contains(session_id.trim())
        })
        .map(|record| record.id))
}

fn active_reparent_route_request_for_session(
    state: &Value,
    session_id: &str,
) -> Result<Option<String>> {
    Ok(reparent_request_records(state)?
        .into_iter()
        .find(|record| {
            record.is_apply_fenced()
                && record.apply_plan.as_ref().is_some_and(|plan| {
                    plan.edge_changes
                        .iter()
                        .any(|change| change.session_id == session_id.trim())
                })
        })
        .map(|record| record.id))
}

fn ensure_session_not_reparent_fenced(state: &Value, session_id: &str) -> Result<()> {
    if let Some(request_id) = active_reparent_request_for_session(state, session_id)? {
        anyhow::bail!("reparent request {request_id} controls session {session_id}")
    }
    Ok(())
}

fn leased_reparent_request<'a>(
    state: &Value,
    records: &'a [ReparentRequestRecord],
    request_id: &str,
) -> Result<&'a ReparentRequestRecord> {
    let lease = reparent_apply_lease(state)?.context("reparent apply lease disappeared")?;
    if lease.request_id != request_id {
        anyhow::bail!(
            "reparent apply lease belongs to {}, not {request_id}",
            lease.request_id
        );
    }
    records
        .iter()
        .find(|record| record.id == request_id)
        .with_context(|| format!("reparent request {request_id} disappeared"))
}

fn leased_reparent_request_mut<'a>(
    state: &Value,
    records: &'a mut [ReparentRequestRecord],
    request_id: &str,
) -> Result<&'a mut ReparentRequestRecord> {
    let lease = reparent_apply_lease(state)?.context("reparent apply lease disappeared")?;
    if lease.request_id != request_id {
        anyhow::bail!(
            "reparent apply lease belongs to {}, not {request_id}",
            lease.request_id
        );
    }
    records
        .iter_mut()
        .find(|record| record.id == request_id)
        .with_context(|| format!("reparent request {request_id} disappeared"))
}

fn queue_snapshot_from_plan(plan: &ReparentApplyPlan) -> Result<ParentRoutingSnapshot> {
    let mut snapshot = ParentRoutingSnapshot::default();
    for change in &plan.queue_routing_changes {
        if change.store != "queue" {
            anyhow::bail!("queue routing plan contains store {}", change.store);
        }
        let old_parent = change
            .expected_target_session_id
            .clone()
            .with_context(|| format!("routing change {} has no old parent", change.record_id))?;
        match change.record_kind.as_str() {
            "parent_wake" => snapshot.wake_rows.push(ParentRoutingWakeRow {
                id: change.record_id.clone(),
                child_session_id: change.child_session_id.clone(),
                parent_session_id: old_parent,
                period_seconds: change
                    .period_seconds
                    .with_context(|| format!("parent wake {} has no period", change.record_id))?,
                is_active: change.prior_active.with_context(|| {
                    format!("parent wake {} has no active state", change.record_id)
                })?,
            }),
            "message" => snapshot.message_rows.push(ParentRoutingMessageRow {
                id: change.record_id.clone(),
                child_session_id: change.child_session_id.clone(),
                parent_session_id: old_parent,
                creates_parent_wake: change.creates_parent_wake,
            }),
            other => anyhow::bail!("unsupported queue routing record kind {other}"),
        }
    }
    Ok(snapshot)
}

fn queue_route_groups_from_plan(
    plan: &ReparentApplyPlan,
    restore_old_targets: bool,
) -> Result<Vec<(ParentRoutingSnapshot, Option<String>)>> {
    let mut groups = BTreeMap::<Option<String>, ParentRoutingSnapshot>::new();
    for change in &plan.queue_routing_changes {
        if change.store != "queue" {
            anyhow::bail!("queue routing plan contains store {}", change.store);
        }
        let old_parent = change
            .expected_target_session_id
            .clone()
            .with_context(|| format!("routing change {} has no old parent", change.record_id))?;
        let target = if restore_old_targets {
            Some(old_parent.clone())
        } else {
            change.new_target_session_id.clone()
        };
        let snapshot = groups.entry(target).or_default();
        match change.record_kind.as_str() {
            "parent_wake" => snapshot.wake_rows.push(ParentRoutingWakeRow {
                id: change.record_id.clone(),
                child_session_id: change.child_session_id.clone(),
                parent_session_id: old_parent,
                period_seconds: change
                    .period_seconds
                    .with_context(|| format!("parent wake {} has no period", change.record_id))?,
                is_active: change.prior_active.with_context(|| {
                    format!("parent wake {} has no active state", change.record_id)
                })?,
            }),
            "message" => snapshot.message_rows.push(ParentRoutingMessageRow {
                id: change.record_id.clone(),
                child_session_id: change.child_session_id.clone(),
                parent_session_id: old_parent,
                creates_parent_wake: change.creates_parent_wake,
            }),
            other => anyhow::bail!("unsupported queue routing record kind {other}"),
        }
    }
    Ok(groups
        .into_iter()
        .map(|(target, snapshot)| (snapshot, target))
        .collect())
}

fn quiesce_json_reparent_routes(state: &mut Value, plan: &ReparentApplyPlan) -> Result<()> {
    for change in &plan.json_routing_changes {
        match change.record_kind.as_str() {
            "parent_wake" => {
                let registrations =
                    ensure_array_field_mut(state, "retained_parent_wake_registrations")?;
                let entry = registrations
                    .iter_mut()
                    .find(|entry| json_text(entry.get("id")).as_deref() == Some(&change.record_id))
                    .with_context(|| format!("parent wake {} disappeared", change.record_id))?;
                let object = entry
                    .as_object_mut()
                    .context("parent wake record must be an object")?;
                validate_json_parent_route(object, change, true)?;
                object.insert("is_active".to_owned(), Value::Bool(false));
            }
            "context_monitor" => {
                let sessions = ensure_sessions_array_mut(state)?;
                let session = session_object_mut(sessions, &change.child_session_id)
                    .with_context(|| format!("session {} disappeared", change.child_session_id))?;
                validate_json_parent_route(session, change, true)?;
                session.insert("context_monitor_enabled".to_owned(), Value::Bool(false));
            }
            other => anyhow::bail!("unsupported JSON routing record kind {other}"),
        }
    }
    Ok(())
}

fn commit_json_reparent_plan(state: &mut Value, plan: &ReparentApplyPlan) -> Result<()> {
    for change in &plan.edge_changes {
        let sessions = ensure_sessions_array_mut(state)?;
        let session = session_object_mut(sessions, &change.session_id)
            .with_context(|| format!("session {} disappeared", change.session_id))?;
        let current_parent = json_text(session.get("parent_session_id"));
        if current_parent != change.expected_parent_session_id {
            anyhow::bail!(
                "session {} parent changed after planning",
                change.session_id
            );
        }
        session.insert(
            "parent_session_id".to_owned(),
            change
                .new_parent_session_id
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
    }
    for change in &plan.json_routing_changes {
        match change.record_kind.as_str() {
            "parent_wake" => {
                let registrations =
                    ensure_array_field_mut(state, "retained_parent_wake_registrations")?;
                let entry = registrations
                    .iter_mut()
                    .find(|entry| json_text(entry.get("id")).as_deref() == Some(&change.record_id))
                    .with_context(|| format!("parent wake {} disappeared", change.record_id))?;
                let object = entry
                    .as_object_mut()
                    .context("parent wake record must be an object")?;
                validate_json_parent_route(object, change, false)?;
                if let Some(new_parent) = change.new_target_session_id.as_ref() {
                    object.insert(
                        "parent_session_id".to_owned(),
                        Value::String(new_parent.clone()),
                    );
                }
                object.insert(
                    "is_active".to_owned(),
                    Value::Bool(
                        change.new_target_session_id.is_some()
                            && change.prior_active.unwrap_or(false),
                    ),
                );
            }
            "context_monitor" => {
                let sessions = ensure_sessions_array_mut(state)?;
                let session = session_object_mut(sessions, &change.child_session_id)
                    .with_context(|| format!("session {} disappeared", change.child_session_id))?;
                validate_json_parent_route(session, change, false)?;
                session.insert(
                    "context_monitor_notify".to_owned(),
                    change
                        .new_target_session_id
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
                session.insert(
                    "context_monitor_enabled".to_owned(),
                    Value::Bool(
                        change.new_target_session_id.is_some()
                            && change.prior_active.unwrap_or(false),
                    ),
                );
            }
            other => anyhow::bail!("unsupported JSON routing record kind {other}"),
        }
    }
    Ok(())
}

fn restore_json_reparent_routes(state: &mut Value, plan: &ReparentApplyPlan) -> Result<()> {
    for change in &plan.json_routing_changes {
        let old_parent = change
            .expected_target_session_id
            .clone()
            .with_context(|| format!("routing change {} has no old parent", change.record_id))?;
        match change.record_kind.as_str() {
            "parent_wake" => {
                let registrations =
                    ensure_array_field_mut(state, "retained_parent_wake_registrations")?;
                let entry = registrations
                    .iter_mut()
                    .find(|entry| json_text(entry.get("id")).as_deref() == Some(&change.record_id))
                    .with_context(|| format!("parent wake {} disappeared", change.record_id))?;
                let object = entry
                    .as_object_mut()
                    .context("parent wake record must be an object")?;
                let already_restored = json_text(object.get("parent_session_id")).as_deref()
                    == Some(old_parent.as_str())
                    && object
                        .get("is_active")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        == change.prior_active.unwrap_or(false);
                if !already_restored {
                    validate_json_parent_route(object, change, false)?;
                }
                object.insert("parent_session_id".to_owned(), Value::String(old_parent));
                object.insert(
                    "is_active".to_owned(),
                    Value::Bool(change.prior_active.unwrap_or(false)),
                );
            }
            "context_monitor" => {
                let sessions = ensure_sessions_array_mut(state)?;
                let session = session_object_mut(sessions, &change.child_session_id)
                    .with_context(|| format!("session {} disappeared", change.child_session_id))?;
                let already_restored = json_text(session.get("context_monitor_notify")).as_deref()
                    == Some(old_parent.as_str())
                    && session
                        .get("context_monitor_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        == change.prior_active.unwrap_or(false);
                if !already_restored {
                    validate_json_parent_route(session, change, false)?;
                }
                session.insert(
                    "context_monitor_notify".to_owned(),
                    Value::String(old_parent),
                );
                session.insert(
                    "context_monitor_enabled".to_owned(),
                    Value::Bool(change.prior_active.unwrap_or(false)),
                );
            }
            other => anyhow::bail!("unsupported JSON routing record kind {other}"),
        }
    }
    Ok(())
}

fn verify_old_reparent_edges(state: &Value, plan: &ReparentApplyPlan) -> Result<()> {
    for change in &plan.edge_changes {
        let session = raw_session_object(state, &change.session_id)
            .with_context(|| format!("session {} disappeared", change.session_id))?;
        if json_text(session.get("parent_session_id")) != change.expected_parent_session_id {
            anyhow::bail!(
                "session {} authority changed before rollback",
                change.session_id
            );
        }
    }
    Ok(())
}

fn validate_json_parent_route(
    object: &Map<String, Value>,
    change: &ReparentRoutingChange,
    allow_pre_state: bool,
) -> Result<()> {
    let target_field = if change.record_kind == "context_monitor" {
        "context_monitor_notify"
    } else {
        "parent_session_id"
    };
    let active_field = if change.record_kind == "context_monitor" {
        "context_monitor_enabled"
    } else {
        "is_active"
    };
    let current_target = json_text(object.get(target_field));
    let current_active = object
        .get(active_field)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let old_parent = change.expected_target_session_id.as_deref();
    let valid = current_target.as_deref() == old_parent
        && (!current_active || (allow_pre_state && change.prior_active == Some(current_active)));
    if !valid {
        anyhow::bail!("JSON route {} changed after planning", change.record_id);
    }
    if change.record_kind == "context_monitor"
        && json_text(object.get("context_monitor_notify_source")).as_deref()
            != Some("parent_derived")
    {
        anyhow::bail!(
            "context monitor {} lost parent provenance",
            change.record_id
        );
    }
    Ok(())
}

fn session_runtime_launch_records(state: &Value) -> Result<Vec<SessionRuntimeLaunchRecord>> {
    state
        .get("session_runtime_launches")
        .and_then(Value::as_array)
        .map(|records| {
            records
                .iter()
                .map(|record| {
                    serde_json::from_value(record.clone())
                        .context("failed to parse session runtime launch record")
                })
                .collect()
        })
        .unwrap_or_else(|| Ok(Vec::new()))
}

fn store_session_runtime_launch_records(
    state: &mut Value,
    records: &[SessionRuntimeLaunchRecord],
) -> Result<()> {
    let object = state
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("session state root must be an object"))?;
    object.insert(
        "session_runtime_launches".to_owned(),
        serde_json::to_value(records)?,
    );
    Ok(())
}

fn session_credential_rotation_records(
    state: &Value,
) -> Result<Vec<SessionCredentialRotationRecord>> {
    state
        .get("session_credential_rotations")
        .and_then(Value::as_array)
        .map(|records| {
            records
                .iter()
                .map(|record| {
                    serde_json::from_value(record.clone())
                        .context("failed to parse session credential rotation record")
                })
                .collect()
        })
        .unwrap_or_else(|| Ok(Vec::new()))
}

fn store_session_credential_rotation_records(
    state: &mut Value,
    records: &[SessionCredentialRotationRecord],
) -> Result<()> {
    let object = state
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("session state root must be an object"))?;
    object.insert(
        "session_credential_rotations".to_owned(),
        serde_json::to_value(records)?,
    );
    Ok(())
}

/// Finalize every active rotation before a terminal session state is made
/// visible.  A relaunching runtime launch must fail too: otherwise startup
/// recovery could resurrect a seat after its terminal transition.
fn finalize_active_credential_rotations_for_terminal_session(
    state: &mut Value,
    session_id: &str,
) -> Result<()> {
    let mut rotations = session_credential_rotation_records(state)?;
    let active_rotation_ids = rotations
        .iter()
        .filter(|rotation| {
            rotation.session_id == session_id
                && matches!(rotation.status.as_str(), "waiting_idle" | "relaunching")
        })
        .map(|rotation| rotation.id.clone())
        .collect::<BTreeSet<_>>();
    if active_rotation_ids.is_empty() {
        return Ok(());
    }
    let now = now_rfc3339();
    for rotation in &mut rotations {
        if active_rotation_ids.contains(&rotation.id) {
            rotation.status = "failed".to_owned();
            rotation.updated_at = now.clone();
            rotation.failure_reason = Some("target_terminal".to_owned());
        }
    }
    store_session_credential_rotation_records(state, &rotations)?;

    let mut launches = session_runtime_launch_records(state)?;
    let mut launches_changed = false;
    for launch in &mut launches {
        if launch
            .credential_rotation_id
            .as_deref()
            .is_some_and(|id| active_rotation_ids.contains(id))
            && matches!(launch.status.as_str(), "prepared" | "launching")
        {
            launch.status = "failed".to_owned();
            launch.updated_at = now.clone();
            launch.failure_reason = Some("target_terminal".to_owned());
            launches_changed = true;
        }
    }
    if launches_changed {
        store_session_runtime_launch_records(state, &launches)?;
    }
    Ok(())
}

/// Reconcile a missing local tmux runtime to durable terminal state without
/// interpreting pane output.  This is only called while the caller holds the
/// session mutation lock.
fn mark_session_runtime_missing_terminal(state: &mut Value, session_id: &str) -> Result<()> {
    finalize_active_credential_rotations_for_terminal_session(state, session_id)?;
    let sessions = ensure_sessions_array_mut(state)?;
    let Some(session) = session_object_mut(sessions, session_id) else {
        return Ok(());
    };
    if !raw_session_is_stopped(session) {
        let now = now_rfc3339();
        session.insert("status".to_owned(), Value::String("stopped".to_owned()));
        session.insert("stopped_at".to_owned(), Value::String(now.clone()));
        session.insert("last_activity".to_owned(), Value::String(now));
    }
    Ok(())
}

fn mark_runtime_launch_applied(
    state: &mut Value,
    launch_id: &str,
    provider_resume_id: Option<&str>,
) -> Result<()> {
    let mut records = session_runtime_launch_records(state)?;
    let record = records
        .iter_mut()
        .find(|record| record.id == launch_id)
        .ok_or_else(|| anyhow::anyhow!("runtime launch {launch_id} disappeared"))?;
    record.status = "applied".to_owned();
    record.updated_at = now_rfc3339();
    record.failure_reason = None;
    if let Some(provider_resume_id) = provider_resume_id {
        record.provider_resume_id = Some(provider_resume_id.to_owned());
    }
    let credential_rotation_id = record.credential_rotation_id.clone();
    store_session_runtime_launch_records(state, &records)?;
    if let Some(credential_rotation_id) = credential_rotation_id {
        let mut rotations = session_credential_rotation_records(state)?;
        let rotation = rotations
            .iter_mut()
            .find(|rotation| rotation.id == credential_rotation_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "credential rotation {credential_rotation_id} disappeared during launch"
                )
            })?;
        let now = now_rfc3339();
        rotation.status = "applied".to_owned();
        rotation.updated_at = now.clone();
        rotation.applied_at = Some(now);
        rotation.failure_reason = None;
        rotation.runtime_launch_id = Some(launch_id.to_owned());
        store_session_credential_rotation_records(state, &rotations)?;
    }
    Ok(())
}

fn remove_failed_provisional_runtime_session(state: &Value, session_id: &str) -> bool {
    !state
        .get("sessions")
        .and_then(Value::as_array)
        .and_then(|sessions| {
            sessions
                .iter()
                .find(|session| session.get("id").and_then(Value::as_str) == Some(session_id))
        })
        .is_some_and(|session| {
            completion_status_is_retired(json_text(session.get("completion_status")).as_deref())
        })
}

fn mark_runtime_launch_failed(
    state: &mut Value,
    launch_id: &str,
    session_id: &str,
    remove_provisional_session: bool,
    failure_reason: &str,
) -> Result<()> {
    let mut records = session_runtime_launch_records(state)?;
    let record = records
        .iter_mut()
        .find(|record| record.id == launch_id)
        .ok_or_else(|| anyhow::anyhow!("runtime launch {launch_id} disappeared"))?;
    record.status = "failed".to_owned();
    record.updated_at = now_rfc3339();
    record.failure_reason = Some(failure_reason.to_owned());
    let credential_rotation_id = record.credential_rotation_id.clone();
    store_session_runtime_launch_records(state, &records)?;
    if let Some(credential_rotation_id) = credential_rotation_id {
        let mut rotations = session_credential_rotation_records(state)?;
        if let Some(rotation) = rotations
            .iter_mut()
            .find(|rotation| rotation.id == credential_rotation_id)
        {
            rotation.status = "failed".to_owned();
            rotation.updated_at = now_rfc3339();
            rotation.failure_reason = Some(failure_reason.to_owned());
            rotation.runtime_launch_id = Some(launch_id.to_owned());
            store_session_credential_rotation_records(state, &rotations)?;
        }
    }
    if remove_provisional_session {
        let sessions = ensure_sessions_array_mut(state)?;
        sessions.retain(|session| session.get("id").and_then(Value::as_str) != Some(session_id));
    } else {
        // A failed runtime launch leaves the seat terminal; fail any still
        // active rotation before the terminal state reaches durable storage.
        finalize_active_credential_rotations_for_terminal_session(state, session_id)?;
        let sessions = ensure_sessions_array_mut(state)?;
        if let Some(session) = session_object_mut(sessions, session_id) {
            session.insert("status".to_owned(), Value::String("stopped".to_owned()));
            session.insert("stopped_at".to_owned(), Value::String(now_rfc3339()));
        }
    }
    Ok(())
}

fn reparent_topology_fingerprint(
    kind: &str,
    subject_session_id: &str,
    target_parent_session_id: &str,
    expected_parent_session_id: Option<&str>,
    expected_target_parent_session_id: Option<&str>,
    frozen_live_child_ids: &[String],
    peer_root_succession: bool,
    stopped_root_recovery: bool,
) -> String {
    let mut children = frozen_live_child_ids.to_vec();
    children.sort();
    let mut canonical = json!({
        "kind": kind,
        "subject_session_id": subject_session_id,
        "target_parent_session_id": target_parent_session_id,
        "expected_parent_session_id": expected_parent_session_id,
        "frozen_live_child_ids": children,
    });
    // Keep the old canonical shape for existing direct-child requests so their
    // persisted fingerprints remain valid across this schema extension.
    if peer_root_succession {
        canonical["tree_mode"] = Value::String("peer_root_succession".to_owned());
    }
    if stopped_root_recovery {
        canonical["tree_mode"] = Value::String("stopped_root_recovery".to_owned());
        canonical["expected_target_parent_session_id"] = Value::String(
            expected_target_parent_session_id
                .unwrap_or_default()
                .to_owned(),
        );
    }
    let digest = Sha256::digest(serde_json::to_vec(&canonical).unwrap_or_default());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn approvals_satisfied(
    required_agent_approvals: &[String],
    required_human_approval: bool,
    approvals: &[ReparentApprovalRecord],
) -> bool {
    let agents_satisfied = required_agent_approvals.iter().all(|required| {
        approvals.iter().any(|approval| {
            approval.actor_kind == "agent"
                && approval.actor_id == *required
                && approval.decision == "approved"
        })
    });
    let human_satisfied = !required_human_approval
        || approvals
            .iter()
            .any(|approval| approval.actor_kind == "human" && approval.decision == "approved");
    agents_satisfied && human_satisfied
}

fn refresh_reparent_requests(
    records: &mut [ReparentRequestRecord],
    sessions: &[SessionRecord],
    state: &Value,
    now: OffsetDateTime,
) -> bool {
    let mut changed = false;
    let decided_at = now
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
    for index in 0..records.len() {
        let superseded_by_request_id = records
            .iter()
            .filter(|other| reparent_request_supersedes(other, &records[index]))
            .min_by(|left, right| (&left.applied_at, &left.id).cmp(&(&right.applied_at, &right.id)))
            .map(|record| record.id.clone());
        let record = &mut records[index];
        if record.status != "pending" {
            continue;
        }
        match parse_timestamp(&record.expires_at) {
            Some(expires_at) if expires_at <= now => {
                record.status = "expired".to_owned();
                record.decided_at = Some(decided_at.clone());
                record.failure_reason = Some("request expired before all approvals".to_owned());
                record.ready_to_apply = false;
                changed = true;
                continue;
            }
            None => {
                record.status = "stale".to_owned();
                record.decided_at = Some(decided_at.clone());
                record.failure_reason = Some("request expiry timestamp is invalid".to_owned());
                record.ready_to_apply = false;
                changed = true;
                continue;
            }
            _ => {}
        }
        if let Some(reason) = reparent_stale_reason(record, sessions, state) {
            if let Some(winner) = superseded_by_request_id {
                record.status = "superseded".to_owned();
                record.superseded_by_request_id = Some(winner.clone());
                record.failure_reason = Some(format!("request superseded by {winner}"));
            } else {
                record.status = "stale".to_owned();
                record.failure_reason = Some(reason);
            }
            record.decided_at = Some(decided_at.clone());
            record.ready_to_apply = false;
            changed = true;
        }
    }
    changed
}

/// An applied request is the winner only when it committed precisely the same
/// immutable plan after this request was opened.  Matching the subject and
/// destination alone can confuse an earlier, unrelated move with a winner.
fn reparent_request_supersedes(
    applied: &ReparentRequestRecord,
    pending: &ReparentRequestRecord,
) -> bool {
    if applied.id == pending.id
        || applied.status != "applied"
        || applied.topology_fingerprint != pending.topology_fingerprint
    {
        return false;
    }
    let (Some(applied_at), Some(created_at)) = (
        applied.applied_at.as_deref().and_then(parse_timestamp),
        parse_timestamp(&pending.created_at),
    ) else {
        return false;
    };
    applied_at > created_at
}

fn reparent_stale_reason(
    record: &ReparentRequestRecord,
    sessions: &[SessionRecord],
    state: &Value,
) -> Option<String> {
    let expected_fingerprint = reparent_topology_fingerprint(
        &record.kind,
        &record.subject_session_id,
        &record.target_parent_session_id,
        record.expected_parent_session_id.as_deref(),
        record.expected_target_parent_session_id.as_deref(),
        &record.frozen_live_child_ids,
        record.peer_root_succession,
        record.stopped_root_recovery,
    );
    if record.topology_fingerprint != expected_fingerprint {
        return Some("stored topology fingerprint does not match the request plan".to_owned());
    }
    let Some(subject) = sessions
        .iter()
        .find(|session| session.id == record.subject_session_id)
    else {
        return Some("subject session no longer exists".to_owned());
    };
    if record.stopped_root_recovery {
        if !stopped_root_recovery_source_eligible(subject) {
            return Some(
                "stopped-root predecessor terminal state changed after request creation".to_owned(),
            );
        }
    } else if !subject.is_live_for_registry() {
        return Some("subject session is no longer live".to_owned());
    }
    if subject.parent_session_id != record.expected_parent_session_id {
        return Some("subject parent changed after request creation".to_owned());
    }
    let expected_parent_is_live_now =
        record
            .expected_parent_session_id
            .as_deref()
            .is_some_and(|parent_id| {
                sessions
                    .iter()
                    .find(|session| session.id == parent_id)
                    .is_some_and(SessionRecord::is_live_for_registry)
            });
    if expected_parent_is_live_now != record.expected_parent_is_live {
        return Some("current parent liveness changed after request creation".to_owned());
    }
    let Some(target) = sessions
        .iter()
        .find(|session| session.id == record.target_parent_session_id)
    else {
        return Some("target parent session no longer exists".to_owned());
    };
    if !target.is_live_for_registry() {
        return Some("target parent session is no longer live".to_owned());
    }
    if record.kind == "tree" {
        if record.stopped_root_recovery {
            if target.parent_session_id != record.expected_target_parent_session_id {
                return Some(
                    "stopped-root successor parent changed after request creation".to_owned(),
                );
            }
            if !record.expected_target_parent_is_live {
                return Some(
                    "stopped-root successor did not have a live parent at request creation"
                        .to_owned(),
                );
            }
            let Some(target_parent_id) = record.expected_target_parent_session_id.as_deref() else {
                return Some(
                    "stopped-root successor parent is missing from request plan".to_owned(),
                );
            };
            let Some(target_parent) = sessions
                .iter()
                .find(|session| session.id == target_parent_id)
            else {
                return Some("stopped-root successor parent no longer exists".to_owned());
            };
            if !target_parent.is_live_for_registry()
                || !session_supports_reparent_consent(target_parent)
            {
                return Some("stopped-root successor parent can no longer consent".to_owned());
            }
            if record
                .stopped_root_authorized_maintainer_session_id
                .as_deref()
                != Some(target_parent_id)
            {
                return Some(
                    "stopped-root request lacks durable maintainer authorization binding"
                        .to_owned(),
                );
            }
            match find_raw_registration(state, "maintainer") {
                Ok(Some(maintainer)) if maintainer.session_id == target_parent_id => {}
                Ok(Some(_)) => {
                    return Some(
                        "durable maintainer registration changed after request creation".to_owned(),
                    );
                }
                Ok(None) => {
                    return Some(
                        "durable maintainer registration disappeared after request creation"
                            .to_owned(),
                    );
                }
                Err(_) => {
                    return Some(
                        "durable maintainer registration could not be revalidated".to_owned(),
                    );
                }
            }
            if sessions.iter().any(|session| {
                session.parent_session_id.as_deref()
                    == Some(record.target_parent_session_id.as_str())
            }) {
                return Some(
                    "stopped-root successor gained children after request creation".to_owned(),
                );
            }
        } else if record.peer_root_succession {
            if target.parent_session_id.is_some() {
                return Some("peer-root successor is no longer a root".to_owned());
            }
        } else if target.parent_session_id.as_deref() != Some(&record.subject_session_id) {
            return Some("tree target is no longer a direct child of the source".to_owned());
        }
        let current_live_children = sessions
            .iter()
            .filter(|session| {
                session.is_live_for_registry()
                    && session.parent_session_id.as_deref()
                        == Some(record.subject_session_id.as_str())
            })
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        if current_live_children != record.frozen_live_child_ids {
            return Some("source live-child set changed after request creation".to_owned());
        }
        let changes = tree_reparent_edge_changes(
            &record.subject_session_id,
            &record.target_parent_session_id,
            record.expected_parent_session_id.as_deref(),
            tree_target_parent_session_id(record),
            record.expected_target_parent_session_id.as_deref(),
            &record.frozen_live_child_ids,
            record.peer_root_succession,
            record.stopped_root_recovery,
        );
        if reparent_plan_would_create_cycle(sessions, &changes) {
            return Some("current tree plan would create a hierarchy cycle".to_owned());
        }
    } else if reparent_would_create_cycle(
        sessions,
        &record.subject_session_id,
        &record.target_parent_session_id,
    ) {
        return Some("current topology would create a hierarchy cycle".to_owned());
    }
    None
}

fn tree_reparent_edge_changes(
    source_session_id: &str,
    target_session_id: &str,
    expected_source_parent_session_id: Option<&str>,
    target_parent_session_id: Option<&str>,
    expected_target_parent_session_id: Option<&str>,
    frozen_live_child_ids: &[String],
    peer_root_succession: bool,
    stopped_root_recovery: bool,
) -> Vec<ReparentEdgeChange> {
    let mut changes = Vec::new();
    if stopped_root_recovery {
        changes.push(ReparentEdgeChange {
            session_id: target_session_id.to_owned(),
            expected_parent_session_id: expected_target_parent_session_id.map(ToOwned::to_owned),
            new_parent_session_id: None,
        });
    } else if !peer_root_succession {
        changes.push(ReparentEdgeChange {
            session_id: target_session_id.to_owned(),
            expected_parent_session_id: Some(source_session_id.to_owned()),
            new_parent_session_id: target_parent_session_id.map(ToOwned::to_owned),
        });
    }
    changes.push(ReparentEdgeChange {
        session_id: source_session_id.to_owned(),
        expected_parent_session_id: expected_source_parent_session_id.map(ToOwned::to_owned),
        new_parent_session_id: Some(target_session_id.to_owned()),
    });
    changes.extend(
        frozen_live_child_ids
            .iter()
            .filter(|child| child.as_str() != target_session_id)
            .map(|child| ReparentEdgeChange {
                session_id: child.clone(),
                expected_parent_session_id: Some(source_session_id.to_owned()),
                new_parent_session_id: Some(target_session_id.to_owned()),
            }),
    );
    changes
}

/// The predecessor-side eligibility rule is deliberately shared by planning
/// and commit freshness checks: a recovery can never silently widen to a live
/// source or to a stopped non-root.
fn stopped_root_recovery_source_eligible(source: &SessionRecord) -> bool {
    source.is_stopped() && source.parent_session_id.is_none()
}

fn tree_target_parent_session_id(record: &ReparentRequestRecord) -> Option<&str> {
    // Older persisted requests omit this flag and retain their immutable plan.
    if record.detach_non_live_parent && !record.expected_parent_is_live {
        None
    } else {
        record.expected_parent_session_id.as_deref()
    }
}

fn reparent_plan_would_create_cycle(
    sessions: &[SessionRecord],
    changes: &[ReparentEdgeChange],
) -> bool {
    let mut parents = sessions
        .iter()
        .map(|session| (session.id.clone(), session.parent_session_id.clone()))
        .collect::<BTreeMap<_, _>>();
    for change in changes {
        parents.insert(
            change.session_id.clone(),
            change.new_parent_session_id.clone(),
        );
    }
    for start in parents.keys() {
        let mut current = Some(start.as_str());
        let mut visited = BTreeSet::new();
        while let Some(id) = current {
            if !visited.insert(id.to_owned()) {
                return true;
            }
            current = parents.get(id).and_then(|parent| parent.as_deref());
        }
    }
    false
}

fn reparent_would_create_cycle(
    sessions: &[SessionRecord],
    subject_session_id: &str,
    target_parent_session_id: &str,
) -> bool {
    if subject_session_id == target_parent_session_id {
        return true;
    }
    let by_id = sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect::<BTreeMap<_, _>>();
    let mut current_id = Some(target_parent_session_id);
    let mut visited = BTreeSet::new();
    while let Some(session_id) = current_id {
        if session_id == subject_session_id {
            return true;
        }
        if !visited.insert(session_id) {
            return true;
        }
        current_id = by_id
            .get(session_id)
            .and_then(|session| session.parent_session_id.as_deref());
    }
    false
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn now_unix_timestamp_nanos() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap_or(i64::MAX)
}

fn now_python_naive_iso() -> String {
    let now_utc = OffsetDateTime::now_utc();
    let local = OffsetDateTime::now_local().unwrap_or(now_utc);
    local
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]"
        ))
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000000".to_owned())
}

fn claude_projects_roots(configured_transcript_root: Option<&str>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push = |path: PathBuf| {
        if !roots.contains(&path) {
            roots.push(path);
        }
    };

    if let Some(root) = configured_transcript_root
        .map(str::trim)
        .filter(|root| !root.is_empty())
    {
        push(expand_home(root));
    }
    if let Some(config_dirs) = env::var_os("CLAUDE_CONFIG_DIR") {
        for config_dir in config_dirs.to_string_lossy().split(',') {
            let config_dir = config_dir.trim();
            if !config_dir.is_empty() {
                push(expand_home(config_dir).join("projects"));
            }
        }
    }
    if let Some(xdg_config_home) = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        push(xdg_config_home.join("claude").join("projects"));
    } else {
        push(expand_home("~/.config/claude/projects"));
    }
    push(expand_home("~/.claude/projects"));
    roots
}

pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path));
    }
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    match env::var_os("HOME") {
        Some(home) => Path::new(&home).join(rest),
        None => PathBuf::from(path),
    }
}

#[derive(Debug, Default, Deserialize)]
struct StateSnapshot {
    #[serde(default)]
    sessions: Vec<SessionRecord>,
    #[serde(default)]
    maintainer_session_id: Option<String>,
    #[serde(default)]
    agent_registrations: Vec<AgentRegistrationRecord>,
    #[serde(default)]
    adoption_proposals: Vec<AdoptionProposalRecord>,
}

#[derive(Debug, Default, Deserialize)]
struct RawStateSnapshot {
    #[serde(default)]
    sessions: Vec<Value>,
    #[serde(default)]
    maintainer_session_id: Option<String>,
    #[serde(default)]
    agent_registrations: Vec<AgentRegistrationRecord>,
    #[serde(default)]
    adoption_proposals: Vec<AdoptionProposalRecord>,
}

impl TryFrom<RawStateSnapshot> for StateSnapshot {
    type Error = serde_json::Error;

    fn try_from(raw: RawStateSnapshot) -> std::result::Result<Self, Self::Error> {
        let mut sessions = Vec::new();
        for raw_session in raw.sessions {
            if is_legacy_codex_app_record(&raw_session) {
                continue;
            }
            sessions.push(serde_json::from_value(raw_session)?);
        }
        Ok(Self {
            sessions,
            maintainer_session_id: raw.maintainer_session_id,
            agent_registrations: raw.agent_registrations,
            adoption_proposals: raw.adoption_proposals,
        })
    }
}

impl StateSnapshot {
    fn into_sessions(mut self) -> Vec<SessionRecord> {
        let alias_map = self.alias_map();
        for session in &mut self.sessions {
            session.aliases = alias_map
                .get(&session.id)
                .map(|aliases| aliases.iter().cloned().collect())
                .unwrap_or_default();
        }

        let proposer_names = self
            .sessions
            .iter()
            .map(|session| {
                (
                    session.id.clone(),
                    session
                        .cached_display_name()
                        .unwrap_or_else(|| non_empty_or(session.name.clone(), &session.id)),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut proposal_map = self.pending_proposal_map(&proposer_names);
        for session in &mut self.sessions {
            session.pending_adoption_proposals =
                proposal_map.remove(&session.id).unwrap_or_default();
        }

        self.sessions
    }

    fn alias_map(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut aliases = BTreeMap::<String, BTreeSet<String>>::new();
        if let Some(session_id) = self
            .maintainer_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|session_id| self.session_is_live_for_registry(session_id))
        {
            aliases
                .entry(session_id.to_owned())
                .or_default()
                .insert("maintainer".to_owned());
        }
        for registration in &self.agent_registrations {
            let role = normalize_role(&registration.role);
            let session_id = registration.session_id.trim();
            if role.is_empty() || session_id.is_empty() {
                continue;
            }
            if !self.session_is_live_for_registry(session_id) {
                continue;
            }
            aliases
                .entry(session_id.to_owned())
                .or_default()
                .insert(role);
        }
        aliases
    }

    fn session_is_live_for_registry(&self, session_id: &str) -> bool {
        self.sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(SessionRecord::is_live_for_registry)
    }

    fn pending_proposal_map(
        &self,
        proposer_names: &BTreeMap<String, String>,
    ) -> BTreeMap<String, Vec<AdoptionProposalResponse>> {
        let mut proposal_map = BTreeMap::<String, Vec<AdoptionProposalResponse>>::new();
        for proposal in &self.adoption_proposals {
            if proposal.status != "pending" {
                continue;
            }
            proposal_map
                .entry(proposal.target_session_id.clone())
                .or_default()
                .push(AdoptionProposalResponse {
                    id: proposal.id.clone(),
                    proposer_session_id: proposal.proposer_session_id.clone(),
                    proposer_name: proposer_names.get(&proposal.proposer_session_id).cloned(),
                    target_session_id: proposal.target_session_id.clone(),
                    created_at: proposal.created_at.clone(),
                    status: "stale".to_owned(),
                    decided_at: proposal.decided_at.clone(),
                    actionable: false,
                    failure_reason: Some(
                        "legacy adoption proposal requires a new consent request".to_owned(),
                    ),
                });
        }
        for proposals in proposal_map.values_mut() {
            proposals.sort_by(|left, right| {
                (&left.created_at, &left.id).cmp(&(&right.created_at, &right.id))
            });
        }
        proposal_map
    }
}

fn is_legacy_codex_app_record(value: &Value) -> bool {
    let Some(record) = value.as_object() else {
        return false;
    };
    let provider = record.get("provider").and_then(Value::as_str);
    if provider != Some("codex") {
        return false;
    }
    let has_codex_thread_id = record
        .get("codex_thread_id")
        .is_some_and(|value| !value.is_null());
    let has_tmux_session = record
        .get("tmux_session")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    let has_log_file = record
        .get("log_file")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    has_codex_thread_id || (!has_tmux_session && !has_log_file)
}

#[derive(Debug, Clone, Deserialize)]
struct AgentRegistrationRecord {
    role: String,
    session_id: String,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AdoptionProposalRecord {
    id: String,
    proposer_session_id: String,
    target_session_id: String,
    created_at: String,
    status: String,
    #[serde(default)]
    decided_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReparentApprovalRecord {
    pub actor_kind: String,
    pub actor_id: String,
    pub decision: String,
    pub decided_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReparentEdgeChange {
    pub session_id: String,
    #[serde(default)]
    pub expected_parent_session_id: Option<String>,
    #[serde(default)]
    pub new_parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReparentRoutingChange {
    pub store: String,
    pub record_kind: String,
    pub record_id: String,
    pub child_session_id: String,
    #[serde(default)]
    pub expected_target_session_id: Option<String>,
    #[serde(default)]
    pub new_target_session_id: Option<String>,
    #[serde(default)]
    pub prior_active: Option<bool>,
    #[serde(default)]
    pub period_seconds: Option<i64>,
    #[serde(default)]
    pub creates_parent_wake: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReparentApplyPlan {
    pub version: u32,
    #[serde(default)]
    pub edge_changes: Vec<ReparentEdgeChange>,
    #[serde(default)]
    pub json_routing_changes: Vec<ReparentRoutingChange>,
    #[serde(default)]
    pub queue_routing_changes: Vec<ReparentRoutingChange>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReparentTreePreview {
    pub kind: String,
    pub source_session_id: String,
    pub target_session_id: String,
    pub peer_root_succession: bool,
    pub stopped_root_recovery: bool,
    pub frozen_live_child_ids: Vec<String>,
    pub edge_changes: Vec<ReparentEdgeChange>,
    pub json_routing_changes: Vec<ReparentRoutingChange>,
    pub queue_routing_changes: Vec<ReparentRoutingChange>,
    pub required_agent_approvals: Vec<String>,
    pub required_human_approval: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReparentApplyLease {
    pub request_id: String,
    pub acquired_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReparentNotificationIntent {
    pub key: String,
    pub event: String,
    pub recipient_session_id: String,
    #[serde(default)]
    pub enqueued_at: Option<String>,
}

#[derive(Debug, Clone)]
struct DesiredReparentNotification {
    request_id: String,
    intent: ReparentNotificationIntent,
    text: String,
}

fn desired_reparent_notifications(
    record: &ReparentRequestRecord,
) -> Vec<DesiredReparentNotification> {
    let mut desired = Vec::new();
    if record.status == "pending" {
        let approved = record
            .approvals
            .iter()
            .filter(|approval| approval.actor_kind == "agent" && approval.decision == "approved")
            .map(|approval| approval.actor_id.as_str())
            .collect::<BTreeSet<_>>();
        let missing = record
            .required_agent_approvals
            .iter()
            .filter(|actor| !approved.contains(actor.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let event = format!("approval:{}", missing.join(","));
        for recipient in &missing {
            desired.push(reparent_notification(
                record,
                &event,
                recipient,
                reparent_approval_notification_text(record),
            ));
        }
        if record.approvals.len() > 1 {
            for recipient in record
                .required_agent_approvals
                .iter()
                .chain(std::iter::once(&record.initiator_session_id))
                .filter(|recipient| !missing.contains(recipient))
                .collect::<BTreeSet<_>>()
            {
                desired.push(reparent_notification(
                    record,
                    &event,
                    recipient,
                    reparent_progress_notification_text(record, &missing),
                ));
            }
        }
    } else if record.status == "failed" {
        // `failed` is an operator-repairable quarantine, not a terminal
        // result: resume may still commit the plan and pre-commit rollback may
        // produce `repaired`.  Calling it terminal creates contradictory
        // notifications for a single request.
        let event = format!(
            "quarantined:{}",
            record.apply_stage.as_deref().unwrap_or("applying")
        );
        for recipient in record
            .required_agent_approvals
            .iter()
            .chain(std::iter::once(&record.initiator_session_id))
            .collect::<BTreeSet<_>>()
        {
            desired.push(reparent_notification(
                record,
                &event,
                recipient,
                reparent_quarantined_notification_text(record),
            ));
        }
    } else if matches!(
        record.status.as_str(),
        "applied" | "rejected" | "expired" | "stale" | "superseded" | "repaired"
    ) {
        let event = format!("terminal:{}", record.status);
        for recipient in record
            .required_agent_approvals
            .iter()
            .chain(std::iter::once(&record.initiator_session_id))
            .collect::<BTreeSet<_>>()
        {
            desired.push(reparent_notification(
                record,
                &event,
                recipient,
                reparent_terminal_notification_text(record),
            ));
        }
    }
    desired
}

fn reparent_notification(
    record: &ReparentRequestRecord,
    event: &str,
    recipient: &str,
    text: String,
) -> DesiredReparentNotification {
    DesiredReparentNotification {
        request_id: record.id.clone(),
        intent: ReparentNotificationIntent {
            key: format!("reparent:{}:{event}:{recipient}", record.id),
            event: event.to_owned(),
            recipient_session_id: recipient.to_owned(),
            enqueued_at: None,
        },
        text,
    }
}

fn reparent_approval_notification_text(record: &ReparentRequestRecord) -> String {
    format!(
        "[sm reparent] {} request {} from {} needs your approval. {} Expires {}. Approve: `sm reparent approve {}`. Reject: `sm reparent reject {}`.",
        record.kind,
        record.id,
        record.initiator_session_id,
        reparent_edge_summary(record),
        record.expires_at,
        record.id,
        record.id,
    )
}

fn reparent_progress_notification_text(
    record: &ReparentRequestRecord,
    missing: &[String],
) -> String {
    format!(
        "[sm reparent] Request {} remains pending. Missing agent approvals: {}.{}",
        record.id,
        if missing.is_empty() {
            "none".to_owned()
        } else {
            missing.join(", ")
        },
        if record.required_human_approval {
            " Human approval is also required in `sm watch`."
        } else {
            ""
        }
    )
}

fn reparent_terminal_notification_text(record: &ReparentRequestRecord) -> String {
    if let Some(winner) = record.superseded_by_request_id.as_deref() {
        return format!(
            "[sm reparent] Request {} was superseded by request {}. {}",
            record.id,
            winner,
            reparent_edge_summary(record),
        );
    }
    format!(
        "[sm reparent] Request {} is {}. {}{}",
        record.id,
        record.status,
        reparent_edge_summary(record),
        record
            .failure_reason
            .as_deref()
            .map(|reason| format!(" Reason: {reason}."))
            .unwrap_or_default(),
    )
}

fn reparent_quarantined_notification_text(record: &ReparentRequestRecord) -> String {
    format!(
        "[sm reparent] Request {} is quarantined at {} and has no terminal outcome yet. {}{}",
        record.id,
        record.apply_stage.as_deref().unwrap_or("applying"),
        reparent_edge_summary(record),
        record
            .failure_reason
            .as_deref()
            .map(|reason| format!(" Reason: {reason}."))
            .unwrap_or_default(),
    )
}

fn reparent_edge_summary(record: &ReparentRequestRecord) -> String {
    let edges = record
        .apply_plan
        .as_ref()
        .map(|plan| plan.edge_changes.clone())
        .unwrap_or_else(|| {
            if record.kind == "tree" {
                tree_reparent_edge_changes(
                    &record.subject_session_id,
                    &record.target_parent_session_id,
                    record.expected_parent_session_id.as_deref(),
                    tree_target_parent_session_id(record),
                    record.expected_target_parent_session_id.as_deref(),
                    &record.frozen_live_child_ids,
                    record.peer_root_succession,
                    record.stopped_root_recovery,
                )
            } else {
                vec![ReparentEdgeChange {
                    session_id: record.subject_session_id.clone(),
                    expected_parent_session_id: record.expected_parent_session_id.clone(),
                    new_parent_session_id: Some(record.target_parent_session_id.clone()),
                }]
            }
        });
    edges
        .iter()
        .map(|edge| {
            format!(
                "{}: {} -> {}",
                edge.session_id,
                edge.expected_parent_session_id.as_deref().unwrap_or("root"),
                edge.new_parent_session_id.as_deref().unwrap_or("root")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReparentDeferredRoutingIntent {
    pub key: String,
    pub operation: String,
    pub child_session_id: String,
    pub payload: Value,
    pub created_at: String,
    #[serde(default)]
    pub replayed_at: Option<String>,
    #[serde(default)]
    pub resolved_parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReparentRepairRecord {
    pub actor_kind: String,
    pub actor_id: String,
    pub action: String,
    #[serde(default)]
    pub prior_failure: Option<String>,
    pub attempted_at: String,
    #[serde(default)]
    pub verified_state_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReparentRequestRecord {
    pub id: String,
    pub kind: String,
    pub subject_session_id: String,
    pub target_parent_session_id: String,
    #[serde(default)]
    pub expected_parent_session_id: Option<String>,
    #[serde(default)]
    pub expected_parent_is_live: bool,
    #[serde(default)]
    pub expected_target_parent_session_id: Option<String>,
    #[serde(default)]
    pub expected_target_parent_is_live: bool,
    #[serde(default)]
    pub stopped_root_authorized_maintainer_session_id: Option<String>,
    #[serde(default)]
    pub detach_non_live_parent: bool,
    /// A root-to-root succession does not move the successor upward; it moves
    /// the outgoing root and its frozen children underneath that peer root.
    #[serde(default)]
    pub peer_root_succession: bool,
    /// A stopped root may only be recovered beneath an authorized live
    /// successor whose durable maintainer parent approves its detachment.
    #[serde(default)]
    pub stopped_root_recovery: bool,
    #[serde(default)]
    pub frozen_live_child_ids: Vec<String>,
    pub initiator_session_id: String,
    #[serde(default)]
    pub required_agent_approvals: Vec<String>,
    #[serde(default)]
    pub required_human_approval: bool,
    #[serde(default)]
    pub approvals: Vec<ReparentApprovalRecord>,
    pub status: String,
    #[serde(default)]
    pub ready_to_apply: bool,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default)]
    pub decided_at: Option<String>,
    #[serde(default)]
    pub applied_at: Option<String>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    /// When a same-edge request completed elsewhere, retain this request as
    /// audit history and tell a caller exactly which request won.
    #[serde(default)]
    pub superseded_by_request_id: Option<String>,
    pub topology_fingerprint: String,
    #[serde(default)]
    pub apply_stage: Option<String>,
    #[serde(default)]
    pub apply_plan: Option<ReparentApplyPlan>,
    #[serde(default)]
    pub notification_intents: Vec<ReparentNotificationIntent>,
    #[serde(default)]
    pub deferred_routing_intents: Vec<ReparentDeferredRoutingIntent>,
    #[serde(default)]
    pub repair_history: Vec<ReparentRepairRecord>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionRuntimeLaunchRecord {
    pub id: String,
    pub operation_kind: String,
    pub session_id: String,
    pub tmux_session: String,
    #[serde(default)]
    pub tmux_socket_name: Option<String>,
    pub working_dir: String,
    pub log_file: String,
    pub provider: String,
    #[serde(default)]
    pub provider_resume_id: Option<String>,
    #[serde(default)]
    pub credential_rotation_id: Option<String>,
    /// Set only by explicit restore admission after it has authorized a
    /// resumable provider identity. Legacy records default false and remain
    /// terminal-fenced when their session has a retired/killed marker.
    #[serde(default)]
    pub restore_authorized: bool,
    #[serde(default)]
    pub initial_message: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub spawn_launch_intent_id: Option<String>,
    #[serde(default)]
    pub spawn_brief_sha256: Option<String>,
    #[serde(default)]
    pub force_initial_prompt_stdin: bool,
    pub credential_sha256: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub failure_reason: Option<String>,
}

impl SessionRuntimeLaunchRecord {
    fn is_authorized_restore_intent(&self) -> bool {
        self.operation_kind == "restore" && self.restore_authorized
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionCredentialRotationRecord {
    pub id: String,
    pub session_id: String,
    pub provider: String,
    pub provider_resume_id: String,
    pub tmux_session: String,
    #[serde(default)]
    pub tmux_socket_name: Option<String>,
    pub request_actor: String,
    pub status: String,
    pub requested_at: String,
    #[serde(default)]
    pub idle_proof_at: Option<String>,
    #[serde(default)]
    pub runtime_launch_id: Option<String>,
    pub updated_at: String,
    #[serde(default)]
    pub applied_at: Option<String>,
    #[serde(default)]
    pub failure_reason: Option<String>,
}

impl ReparentRequestRecord {
    fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "applying")
            || (self.status == "failed"
                && matches!(
                    self.apply_stage.as_deref(),
                    Some(
                        "prequiesce_aborting"
                            | "json_routing_quiesced"
                            | "routing_quiesced"
                            | "authority_committed"
                    )
                ))
    }

    fn affected_session_ids(&self) -> BTreeSet<String> {
        let mut affected = self
            .frozen_live_child_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        affected.insert(self.subject_session_id.clone());
        affected.insert(self.target_parent_session_id.clone());
        if let Some(parent_id) = self.expected_parent_session_id.as_ref() {
            affected.insert(parent_id.clone());
        }
        if let Some(parent_id) = self.expected_target_parent_session_id.as_ref() {
            affected.insert(parent_id.clone());
        }
        if let Some(plan) = self.apply_plan.as_ref() {
            affected.extend(
                plan.edge_changes
                    .iter()
                    .map(|change| change.session_id.clone()),
            );
            affected.extend(
                plan.json_routing_changes
                    .iter()
                    .map(|change| change.child_session_id.clone()),
            );
            affected.extend(
                plan.queue_routing_changes
                    .iter()
                    .map(|change| change.child_session_id.clone()),
            );
        }
        affected
    }

    fn involves_session(&self, session_id: &str) -> bool {
        self.initiator_session_id == session_id
            || self
                .required_agent_approvals
                .iter()
                .any(|id| id == session_id)
            || self.affected_session_ids().contains(session_id)
    }

    fn is_apply_fenced(&self) -> bool {
        self.status == "applying"
            || (self.status == "failed"
                && matches!(
                    self.apply_stage.as_deref(),
                    Some(
                        "prequiesce_aborting"
                            | "json_routing_quiesced"
                            | "routing_quiesced"
                            | "authority_committed"
                    )
                ))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionRecord {
    pub id: String,
    pub name: String,
    pub working_dir: String,
    pub tmux_session: String,
    #[serde(default)]
    pub tmux_socket_name: Option<String>,
    #[serde(default = "default_node")]
    pub node: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub account_key: Option<String>,
    #[serde(default)]
    pub usage_cap_fraction: Option<f64>,
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default)]
    pub provider_resume_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub codex_thread_id: Option<String>,
    #[serde(default)]
    pub forked_from_session_id: Option<String>,
    #[serde(default)]
    pub forked_from_provider_resume_id: Option<String>,
    #[serde(default)]
    pub forked_provider_resume_id: Option<String>,
    #[serde(default)]
    pub forked_at: Option<String>,
    #[serde(default)]
    pub forked_by_session_id: Option<String>,
    #[serde(default)]
    pub friendly_name: Option<String>,
    #[serde(default)]
    pub friendly_name_is_explicit: bool,
    #[serde(default)]
    pub friendly_name_updated_at_ns: Option<i64>,
    #[serde(default)]
    pub native_title: Option<String>,
    #[serde(default)]
    pub native_title_updated_at_ns: Option<i64>,
    #[serde(default)]
    pub native_title_source_mtime_ns: Option<i64>,
    #[serde(default)]
    pub telegram_chat_id: Option<i64>,
    #[serde(default)]
    pub telegram_thread_id: Option<i64>,
    #[serde(default)]
    pub telegram_topic_id: Option<i64>,
    #[serde(default)]
    pub telegram_root_msg_id: Option<i64>,
    #[serde(default)]
    pub current_task: Option<String>,
    #[serde(default)]
    pub git_remote_url: Option<String>,
    #[serde(default)]
    pub review_config: Option<Value>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub session_credential_sha256: Option<String>,
    #[serde(default)]
    pub last_handoff_path: Option<String>,
    #[serde(default)]
    pub agent_status_text: Option<String>,
    #[serde(default)]
    pub agent_status_at: Option<String>,
    #[serde(default)]
    pub agent_task_completed_at: Option<String>,
    #[serde(default)]
    pub completion_status: Option<String>,
    #[serde(default)]
    pub completion_message: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub stopped_at: Option<String>,
    #[serde(default)]
    pub is_em: bool,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub spawned_at: Option<String>,
    pub created_at: String,
    pub last_activity: String,
    /// Timestamp of the most recent authoritative Claude lifecycle hook
    /// (`UserPromptSubmit`, `PreToolUse`, `Stop`). Used to decide whether the
    /// stored activity state is fresh enough to trust without scraping the pane.
    #[serde(default)]
    pub activity_hook_at: Option<String>,
    /// Timestamp of the most recent `UserPromptSubmit`. Its presence is the only
    /// evidence that turn-start hooks are wired for this session, which decides
    /// whether a stored idle may suppress the pane fallback.
    #[serde(default)]
    pub activity_turn_start_hook_at: Option<String>,
    #[serde(default)]
    pub last_tool_call: Option<String>,
    #[serde(default)]
    pub last_tool_name: Option<String>,
    #[serde(default)]
    pub tokens_used: i64,
    #[serde(default)]
    pub context_used_percentage: Option<f64>,
    #[serde(default)]
    pub context_total_input_tokens: Option<i64>,
    #[serde(default)]
    pub context_sampled_at: Option<String>,
    #[serde(default)]
    pub context_compaction_active: bool,
    #[serde(default)]
    pub context_monitor_enabled: bool,
    #[serde(default)]
    pub context_monitor_notify: Option<String>,
    #[serde(default = "default_context_monitor_notify_source")]
    pub context_monitor_notify_source: String,
    /// Optional ordered per-seat notification milestones. When absent, the
    /// configured global milestone list (or the legacy pair) applies.
    #[serde(default)]
    pub context_monitor_threshold_percentages: Option<Vec<f64>>,
    /// Optional per-seat warning override. When absent, the configured global
    /// context-monitor warning threshold remains effective.
    #[serde(default)]
    pub context_monitor_warning_percentage: Option<f64>,
    /// Optional per-seat critical override. When absent, the configured global
    /// context-monitor critical threshold remains effective.
    #[serde(default)]
    pub context_monitor_critical_percentage: Option<f64>,
    /// One-shot latches for the current accumulation cycle. Persisted rather
    /// than held in memory (as the Python server did) so a server restart does
    /// not re-fire a warning the agent has already been told about; the cycle
    /// only ends at a compaction or an explicit context reset.
    #[serde(default)]
    pub context_warning_sent: bool,
    #[serde(default)]
    pub context_critical_sent: bool,
    #[serde(skip)]
    pub aliases: Vec<String>,
    #[serde(skip)]
    pub pending_adoption_proposals: Vec<AdoptionProposalResponse>,
}

fn root_seat_id(record: &SessionRecord, records: &BTreeMap<&str, &SessionRecord>) -> String {
    let mut current = record;
    let mut visited = BTreeSet::new();
    visited.insert(current.id.as_str());
    while let Some(parent_id) = current.parent_session_id.as_deref() {
        if !visited.insert(parent_id) {
            break;
        }
        let Some(parent) = records.get(parent_id) else {
            break;
        };
        current = parent;
    }
    current.id.clone()
}

impl SessionRecord {
    pub(crate) fn lifecycle_status(&self) -> &str {
        if self.is_stopped() {
            "stopped"
        } else {
            normalized_status(&self.status)
        }
    }

    pub(crate) fn is_stopped(&self) -> bool {
        // Retirement persists both fields. Keep the terminal marker authoritative
        // if a delayed status-only writer leaves the activity status behind.
        normalized_status(&self.status) == "stopped"
            || completion_status_is_retired(self.completion_status.as_deref())
    }

    fn is_live_for_registry(&self) -> bool {
        !self.is_stopped()
    }

    pub(crate) fn cached_display_name(&self) -> Option<String> {
        if let Some(alias) = self.aliases.first() {
            return Some(alias.clone());
        }
        let native_title = self.native_title.as_deref().filter(|value| {
            matches!(
                self.provider.as_str(),
                "claude" | "codex" | "codex-app" | "codex-fork"
            ) && !value.trim().is_empty()
        });
        let friendly_name = self
            .friendly_name
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let friendly_name_updated_at_ns = self.friendly_name_updated_at_ns.unwrap_or(0);
        let native_title_updated_at_ns = self
            .native_title_updated_at_ns
            .or(self.native_title_source_mtime_ns)
            .unwrap_or(0);

        if let (Some(friendly_name), Some(native_title)) = (friendly_name, native_title) {
            if friendly_name_updated_at_ns >= native_title_updated_at_ns {
                return Some(friendly_name.to_owned());
            }
            return Some(native_title.to_owned());
        }
        if self.friendly_name_is_explicit {
            if let Some(friendly_name) = friendly_name {
                return Some(friendly_name.to_owned());
            }
        }
        if let Some(native_title) = native_title {
            return Some(native_title.to_owned());
        }
        if let Some(friendly_name) = friendly_name {
            return Some(friendly_name.to_owned());
        }
        Some(non_empty_or(self.name.clone(), &self.id))
    }

    fn resolved_telegram_thread_id(&self) -> Option<i64> {
        self.telegram_thread_id
            .or(self.telegram_topic_id)
            .or(self.telegram_root_msg_id)
    }
}

#[derive(Debug, Serialize)]
pub struct SessionsEnvelope<T> {
    pub sessions: Vec<T>,
}

impl From<Vec<SessionResponse>> for SessionsEnvelope<SessionResponse> {
    fn from(sessions: Vec<SessionResponse>) -> Self {
        Self { sessions }
    }
}

impl From<Vec<ClientSessionResponse>> for SessionsEnvelope<ClientSessionResponse> {
    fn from(sessions: Vec<ClientSessionResponse>) -> Self {
        Self { sessions }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AdoptionProposalResponse {
    id: String,
    proposer_session_id: String,
    proposer_name: Option<String>,
    target_session_id: String,
    created_at: String,
    status: String,
    decided_at: Option<String>,
    actionable: bool,
    failure_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionResponse {
    id: String,
    name: String,
    working_dir: String,
    status: String,
    created_at: String,
    last_activity: String,
    completed_at: Option<String>,
    stopped_at: Option<String>,
    tmux_session: String,
    tmux_socket_name: Option<String>,
    node: String,
    provider: Option<String>,
    account_key: Option<String>,
    usage_cap_fraction: Option<f64>,
    provider_resume_id: Option<String>,
    forked_from_session_id: Option<String>,
    forked_from_provider_resume_id: Option<String>,
    forked_provider_resume_id: Option<String>,
    forked_at: Option<String>,
    forked_by_session_id: Option<String>,
    friendly_name: Option<String>,
    telegram_chat_id: Option<i64>,
    telegram_thread_id: Option<i64>,
    current_task: Option<String>,
    git_remote_url: Option<String>,
    parent_session_id: Option<String>,
    last_handoff_path: Option<String>,
    agent_status_text: Option<String>,
    agent_status_at: Option<String>,
    agent_task_completed_at: Option<String>,
    is_em: bool,
    role: Option<String>,
    activity_state: String,
    last_tool_call: Option<String>,
    last_tool_name: Option<String>,
    last_action_summary: Option<String>,
    last_action_at: Option<String>,
    tokens_used: i64,
    context_monitor_enabled: bool,
    pending_adoption_proposals: Vec<AdoptionProposalResponse>,
    aliases: Vec<String>,
    is_maintainer: bool,
}

impl From<SessionRecord> for SessionResponse {
    fn from(session: SessionRecord) -> Self {
        let status = session.lifecycle_status().to_owned();
        let friendly_name = session.cached_display_name();
        let is_maintainer = session.aliases.iter().any(|alias| alias == "maintainer");
        let telegram_thread_id = session.resolved_telegram_thread_id();
        let activity_state = projected_activity_state(&session, &status);
        Self {
            id: session.id,
            name: session.name,
            working_dir: session.working_dir,
            status,
            created_at: session.created_at,
            last_activity: session.last_activity,
            completed_at: session.completed_at,
            stopped_at: session.stopped_at,
            tmux_session: session.tmux_session,
            tmux_socket_name: session.tmux_socket_name,
            node: non_empty_or(session.node, "primary"),
            provider: Some(non_empty_or(session.provider, "claude")),
            account_key: session.account_key,
            usage_cap_fraction: session.usage_cap_fraction,
            provider_resume_id: session.provider_resume_id,
            forked_from_session_id: session.forked_from_session_id,
            forked_from_provider_resume_id: session.forked_from_provider_resume_id,
            forked_provider_resume_id: session.forked_provider_resume_id,
            forked_at: session.forked_at,
            forked_by_session_id: session.forked_by_session_id,
            friendly_name,
            telegram_chat_id: session.telegram_chat_id,
            telegram_thread_id,
            current_task: session.current_task,
            git_remote_url: session.git_remote_url,
            parent_session_id: session.parent_session_id,
            last_handoff_path: session.last_handoff_path,
            agent_status_text: session.agent_status_text,
            agent_status_at: session.agent_status_at,
            agent_task_completed_at: session.agent_task_completed_at,
            is_em: session.is_em,
            role: session.role,
            activity_state,
            last_tool_call: session.last_tool_call,
            last_tool_name: session.last_tool_name,
            last_action_summary: None,
            last_action_at: None,
            tokens_used: session.tokens_used,
            context_monitor_enabled: session.context_monitor_enabled,
            pending_adoption_proposals: session.pending_adoption_proposals,
            aliases: session.aliases,
            is_maintainer,
        }
    }
}

impl SessionResponse {
    pub fn set_activity_state(&mut self, activity_state: impl Into<String>) {
        self.activity_state = activity_state.into();
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildSessionResponse {
    id: String,
    name: String,
    friendly_name: Option<String>,
    status: String,
    activity_state: String,
    completion_status: Option<String>,
    completion_message: Option<String>,
    last_activity: String,
    spawned_at: Option<String>,
    completed_at: Option<String>,
    tmux_session: String,
    tmux_socket_name: Option<String>,
    agent_status_text: Option<String>,
    agent_status_at: Option<String>,
    provider: String,
    activity_projection: Option<Value>,
}

impl From<SessionRecord> for ChildSessionResponse {
    fn from(session: SessionRecord) -> Self {
        let status = session.lifecycle_status().to_owned();
        let friendly_name = session.cached_display_name();
        let spawned_at = session
            .spawned_at
            .clone()
            .or(Some(session.created_at.clone()));
        let activity_state = projected_activity_state(&session, &status);
        Self {
            id: session.id,
            name: session.name,
            friendly_name,
            status: status.clone(),
            activity_state,
            completion_status: session.completion_status,
            completion_message: session.completion_message,
            last_activity: session.last_activity,
            spawned_at,
            completed_at: session.completed_at,
            tmux_session: session.tmux_session,
            tmux_socket_name: session.tmux_socket_name,
            agent_status_text: session.agent_status_text,
            agent_status_at: session.agent_status_at,
            provider: non_empty_or(session.provider, "claude"),
            activity_projection: None,
        }
    }
}

impl ChildSessionResponse {
    pub fn set_activity_state(&mut self, activity_state: impl Into<String>) {
        self.activity_state = activity_state.into();
    }
}

#[derive(Debug, Serialize)]
pub struct ClientSessionResponse {
    #[serde(flatten)]
    session: SessionResponse,
    attach_descriptor: AttachDescriptor,
    termux_attach: Option<Value>,
    mobile_terminal: Value,
    primary_action: PrimaryAction,
}

impl From<SessionRecord> for ClientSessionResponse {
    fn from(session: SessionRecord) -> Self {
        let response = SessionResponse::from(session);
        let attach_descriptor = AttachDescriptor {
            attach_supported: false,
            message: Some(
                "attach tickets are not implemented in the Rust read-only scaffold".to_owned(),
            ),
            tmux_session: Some(response.tmux_session.clone()),
            runtime_mode: Some("read_only".to_owned()),
        };
        Self {
            session: response,
            attach_descriptor,
            termux_attach: None,
            mobile_terminal: json!({
                "supported": false,
                "reason": "mobile terminal is not implemented in the Rust read-only scaffold"
            }),
            primary_action: PrimaryAction {
                action_type: "details",
                label: "View details",
                reason: None,
            },
        }
    }
}

impl ClientSessionResponse {
    pub fn set_activity_state(&mut self, activity_state: impl Into<String>) {
        self.session.set_activity_state(activity_state);
    }
}

#[derive(Debug, Serialize)]
pub struct AttachDescriptor {
    attach_supported: bool,
    message: Option<String>,
    tmux_session: Option<String>,
    runtime_mode: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PrimaryAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    label: &'static str,
    reason: Option<String>,
}

fn normalized_status(status: &str) -> &str {
    match status {
        "starting" => "running",
        "waiting_input" | "waiting_permission" | "error" => "idle",
        "running" | "idle" | "stopped" => status,
        _ => status,
    }
}

fn completion_status_is_retired(status: Option<&str>) -> bool {
    matches!(status, Some("retired" | "killed"))
}

fn raw_session_is_stopped(session: &Map<String, Value>) -> bool {
    normalized_status(&json_text(session.get("status")).unwrap_or_default()) == "stopped"
        || completion_status_is_retired(json_text(session.get("completion_status")).as_deref())
}

fn effective_raw_session_status(session: &Map<String, Value>) -> String {
    if raw_session_is_stopped(session) {
        "stopped".to_owned()
    } else {
        json_text(session.get("status")).unwrap_or_else(|| "running".to_owned())
    }
}

fn fallback_activity_state(status: &str) -> String {
    match status {
        "stopped" => "stopped".to_owned(),
        "running" => "working".to_owned(),
        _ => "idle".to_owned(),
    }
}

fn projected_activity_state(session: &SessionRecord, status: &str) -> String {
    let status = normalized_status(status);
    if status == "stopped" {
        return "stopped".to_owned();
    }
    if completion_status_is_retired(session.completion_status.as_deref()) {
        return "stopped".to_owned();
    }
    if session.completion_status.is_some() {
        return "waiting_input".to_owned();
    }
    if session.agent_task_completed_at.is_some() {
        return "idle".to_owned();
    }
    if matches!(status, "running" | "working") {
        if session_activity_is_recent(&session.last_activity) {
            return "working".to_owned();
        }
        return "idle".to_owned();
    }
    if status == "idle" {
        return "idle".to_owned();
    }
    fallback_activity_state(status)
}

fn session_activity_is_recent(last_activity: &str) -> bool {
    timestamp_is_within(last_activity, 30)
}

fn timestamp_is_within(value: &str, seconds: i64) -> bool {
    let window = TimeDuration::seconds(seconds);
    let now_utc = OffsetDateTime::now_utc();
    if let Ok(parsed) = OffsetDateTime::parse(value.trim(), &Rfc3339) {
        return now_utc - parsed < window;
    }
    let now_local = local_now_naive(now_utc);
    parse_python_naive_datetime(value).is_some_and(|parsed| now_local - parsed < window)
}

/// True when `value` is strictly newer than `other`. Both timestamps are written
/// by `now_rfc3339()`, but older records may still carry the Python-era naive
/// format, so both encodings are accepted. Returns false when either side is
/// unparseable — an unknown ordering must never supersede anything.
fn timestamp_is_after(value: &str, other: &str) -> bool {
    match (parse_timestamp_ns(value), parse_timestamp_ns(other)) {
        (Some(value), Some(other)) => value > other,
        _ => false,
    }
}

/// How long a hook-derived state is trusted before the pane is allowed to
/// second-guess it. Long enough to cover a slow tool call or a long stretch of
/// pure thinking (no `PreToolUse` fires during either).
///
/// This bounds *both* directions. Every lifecycle hook is delivered by a
/// detached, un-retried curl, so `UserPromptSubmit` is exactly as losable as
/// `Stop` — the very lossiness this ticket exists to work around. Treating
/// either as conclusive forever just relocates the original bug.
const CLAUDE_HOOK_STATE_FRESH_SECONDS: i64 = 180;

/// Sessions owned by another node have no pane to reconcile against, so this
/// window is the *only* bound on a lost hook. It is sized to outlast any
/// plausible turn: for a remote session the realistic failure is reporting idle
/// while a long turn is still running, whereas reporting working requires an
/// actual lost `Stop`. Past this window the session degrades to the default
/// projection, which is where remote sessions sat before hooks were introduced.
const CLAUDE_HOOK_STATE_FRESH_SECONDS_WITHOUT_PANE: i64 = 900;

/// How much the hook-derived activity state can be trusted, and therefore
/// whether the tmux pane needs to be consulted at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeHookGate {
    /// No Claude lifecycle hook has ever been recorded for this session, so the
    /// pane is the only signal available.
    Untracked,
    /// The `Stop` hook fired and no turn has started since. Authoritative: the
    /// agent's turn is over. Only reachable once a `UserPromptSubmit` has been
    /// seen, so both ends of the turn are known to be observable.
    TurnStopped,
    /// A turn is in flight and the hook signal is recent enough to trust.
    TurnRunning,
    /// A turn was reported in flight but the hook signal has gone stale — the
    /// `Stop` hook was probably lost (restart, event-loop stall, watchdog kill).
    Stale,
}

pub(crate) fn claude_hook_gate(session: &SessionRecord) -> ClaudeHookGate {
    let Some(hook_at) = session
        .activity_hook_at
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return ClaudeHookGate::Untracked;
    };
    // A stored idle is only conclusive when the *start* of a turn is observable
    // too. With only the Stop hook wired — the common case for sessions outside a
    // repo that installs both — the first Stop would otherwise pin the session to
    // idle for every later turn, right through a tool-free response.
    let turn_start_hook_wired = session
        .activity_turn_start_hook_at
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    // Only the owning node can capture the pane, so a remote session has nothing
    // to reconcile against and has to lean on the hook signal for longer.
    let fresh_seconds = if is_primary_node(&session.node) {
        CLAUDE_HOOK_STATE_FRESH_SECONDS
    } else {
        CLAUDE_HOOK_STATE_FRESH_SECONDS_WITHOUT_PANE
    };
    let hook_state_is_fresh = timestamp_is_within(hook_at, fresh_seconds);
    match normalized_status(session.status.trim()) {
        // Conclusive only while fresh. A turn-start lost in transit would
        // otherwise pin the session to idle for the whole of the next turn, with
        // the pane unable to say anything but `waiting`.
        "idle" if turn_start_hook_wired && hook_state_is_fresh => ClaudeHookGate::TurnStopped,
        "running" if hook_state_is_fresh => ClaudeHookGate::TurnRunning,
        _ => ClaudeHookGate::Stale,
    }
}

fn parse_python_naive_datetime(value: &str) -> Option<PrimitiveDateTime> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    PrimitiveDateTime::parse(
        value,
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]"),
    )
    .or_else(|_| {
        PrimitiveDateTime::parse(
            value,
            format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]"),
        )
    })
    .ok()
}

fn local_now_naive(now_utc: OffsetDateTime) -> PrimitiveDateTime {
    let local = OffsetDateTime::now_local().unwrap_or(now_utc);
    PrimitiveDateTime::new(local.date(), local.time())
}

fn non_empty_or(value: String, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

fn optional_trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn has_text(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn tail_lines(content: &str, lines: usize) -> String {
    if lines == 0 {
        return String::new();
    }
    let all_lines = content.lines().collect::<Vec<_>>();
    if all_lines.is_empty() {
        return String::new();
    }
    let start = all_lines.len().saturating_sub(lines);
    let mut output = all_lines[start..].join("\n");
    if content.ends_with('\n') {
        output.push('\n');
    }
    output
}

pub(crate) fn read_tail_lines(path: &Path, lines: usize) -> io::Result<String> {
    let file_len = fs::metadata(path)?.len();
    read_tail_lines_at_offset(path, file_len, lines)
}

fn read_tail_lines_at_offset(path: &Path, end_offset: u64, lines: usize) -> io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }

    let mut file = fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let end_offset = end_offset.min(file_len);
    if end_offset == 0 {
        return Ok(String::new());
    }

    let read_len = end_offset.min(output_tail_byte_limit(lines));
    file.seek(SeekFrom::Start(end_offset - read_len))?;
    let mut bytes = Vec::with_capacity(read_len as usize);
    file.take(read_len).read_to_end(&mut bytes)?;
    Ok(tail_lines(&String::from_utf8_lossy(&bytes), lines))
}

/// Capture a restart boundary that ends on a complete JSONL record.
///
/// Codex can be appending while the server starts. Starting a live monitor at
/// raw EOF would skip the suffix of such an in-flight record because recovery
/// correctly ignores incomplete JSON. Moving the shared boundary back to the
/// last newline lets the monitor replay that record once it is complete.
fn codex_fork_complete_jsonl_boundary(path: &Path) -> u64 {
    let Ok(mut file) = fs::File::open(path) else {
        return 0;
    };
    let Ok(file_len) = file.metadata().map(|metadata| metadata.len()) else {
        return 0;
    };
    if file_len == 0 {
        return 0;
    }

    // A record is normally tiny, but if its partial prefix exceeds the bounded
    // read, returning zero is conservative: recovery/monitoring may replay
    // history, but no event can be skipped.
    let read_len = file_len.min(MAX_OUTPUT_TAIL_BYTES);
    if file.seek(SeekFrom::Start(file_len - read_len)).is_err() {
        return 0;
    }
    let mut bytes = Vec::with_capacity(read_len as usize);
    if file.take(read_len).read_to_end(&mut bytes).is_err() {
        return 0;
    }
    bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| file_len - read_len + index as u64 + 1)
        .unwrap_or(0)
}

fn output_tail_byte_limit(lines: usize) -> u64 {
    let requested = (lines as u64).saturating_mul(OUTPUT_TAIL_BYTES_PER_LINE);
    requested.clamp(MIN_OUTPUT_TAIL_BYTES, MAX_OUTPUT_TAIL_BYTES)
}

fn default_node() -> String {
    "primary".to_owned()
}

pub fn is_primary_node(node: &str) -> bool {
    let node = node.trim();
    node.is_empty() || node == "primary"
}

fn ensure_runtime_local_node(node: &str) -> Result<()> {
    if is_primary_node(node) {
        return Ok(());
    }
    anyhow::bail!("Rust runtime does not support remote node {node}");
}

fn default_provider() -> String {
    "claude".to_owned()
}

fn default_context_monitor_notify_source() -> String {
    "explicit".to_owned()
}

/// Return whether the provider emits a measured current-context sample.
///
/// Codex-fork's `thread/tokenUsage/updated` event carries both resident input
/// tokens and `modelContextWindow`; generic Codex transports do not. Native
/// Codex compaction is deliberately not a reason to enroll an unmeasurable
/// session, nor does enrollment authorize any rotation.
fn provider_has_measured_context_gauge(provider: &str) -> bool {
    matches!(provider.trim(), "claude" | "codex-fork")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMonitorThresholdSource {
    Default,
    Custom,
}

impl ContextMonitorThresholdSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone)]
struct EffectiveContextMonitorThresholds {
    percentages: Vec<f64>,
    warning_percentage: f64,
    critical_percentage: f64,
    source: ContextMonitorThresholdSource,
}

/// Resolve the values that will actually govern a seat. A missing seat override
/// is deliberately normal: it means the existing server-level default remains
/// authoritative. Invalid persisted or global values fail closed so status and
/// alerting never claim an unenforceable policy is active.
fn resolve_context_monitor_thresholds(
    percentages_override: Option<Vec<f64>>,
    warning_override: Option<f64>,
    critical_override: Option<f64>,
    config: &ContextMonitorConfig,
) -> Result<EffectiveContextMonitorThresholds, String> {
    let has_custom_override =
        percentages_override.is_some() || warning_override.is_some() || critical_override.is_some();
    let configured_percentages = if config.threshold_percentages.is_empty() {
        vec![config.warning_percentage, config.critical_percentage]
    } else {
        config.threshold_percentages.clone()
    };
    let percentages = if let Some(percentages) = percentages_override {
        percentages
    } else if warning_override.is_some() || critical_override.is_some() {
        vec![
            warning_override.unwrap_or(configured_percentages[0]),
            critical_override.unwrap_or(
                *configured_percentages
                    .last()
                    .expect("configured thresholds exist"),
            ),
        ]
    } else {
        configured_percentages
    };
    for value in &percentages {
        if !value.is_finite() || !(0.0..=100.0).contains(value) {
            return Err(format!(
                "context-monitor thresholds must be finite percentages in the range (0, 100]"
            ));
        }
        if *value == 0.0 {
            return Err(format!(
                "context-monitor thresholds must be finite percentages in the range (0, 100]"
            ));
        }
    }
    if percentages.is_empty() || percentages.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(
            "context-monitor thresholds must be non-empty and strictly increasing".to_owned(),
        );
    }
    Ok(EffectiveContextMonitorThresholds {
        warning_percentage: percentages[0],
        critical_percentage: *percentages.last().expect("non-empty thresholds checked"),
        percentages,
        source: if has_custom_override {
            ContextMonitorThresholdSource::Custom
        } else {
            ContextMonitorThresholdSource::Default
        },
    })
}

/// Read durable notification latches, translating the pre-list two-threshold
/// representation exactly once for existing sessions.
fn context_reported_thresholds(session: &Map<String, Value>, thresholds: &[f64]) -> Vec<f64> {
    if let Some(values) = session
        .get("context_reported_thresholds")
        .and_then(Value::as_array)
    {
        return values.iter().filter_map(Value::as_f64).collect();
    }
    let mut reported = Vec::new();
    if flag_is_set(session, "context_warning_sent") {
        reported.push(thresholds[0]);
    }
    if flag_is_set(session, "context_critical_sent") {
        let final_threshold = *thresholds.last().expect("non-empty thresholds checked");
        if !reported.contains(&final_threshold) {
            reported.push(final_threshold);
        }
    }
    reported
}

fn json_percentages(value: Option<&Value>) -> Option<Vec<f64>> {
    value
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
}

fn session_supports_reparent_consent(session: &SessionRecord) -> bool {
    is_primary_node(&session.node)
        && matches!(session.provider.as_str(), "claude" | "codex" | "codex-fork")
        && (session.session_credential_sha256.is_some()
            || provider_resume_id_for_restore(session).is_some())
}

fn default_delivery_mode() -> String {
    "sequential".to_owned()
}

fn normalize_role(role: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_dash = false;
    for ch in role.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !normalized.is_empty() {
            normalized.push('-');
            last_was_dash = true;
        }
    }
    while normalized.ends_with('-') {
        normalized.pop();
    }
    normalized
}

fn is_safe_provider_native_rename_name(friendly_name: &str) -> bool {
    !friendly_name.is_empty()
        && friendly_name.chars().count() <= 32
        && friendly_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn extract_provider_native_rename_name(text: &str) -> Option<String> {
    let text = text.trim();
    let rest = text.strip_prefix("/rename")?;
    if !rest
        .chars()
        .next()
        .is_some_and(|character| character.is_whitespace())
    {
        return None;
    }
    let friendly_name = rest.trim();
    if friendly_name.split_whitespace().count() == 1
        && is_safe_provider_native_rename_name(friendly_name)
    {
        Some(friendly_name.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::usage_identity::{AccountIdentity, Provider, UsageIdentityStore};
    use rusqlite::Connection;
    use std::{
        process::Command,
        sync::{Arc, Barrier},
        time::Duration,
    };

    #[test]
    fn accepted_spawn_brief_is_private_immutable_and_records_launch_intent() {
        let state_file = unique_temp_path("spawn-brief");
        fs::write(&state_file, r#"{"sessions":[]}"#).unwrap();
        let store = SessionStore::new(state_file.clone());
        let prompt = "# Large brief\n\n`$(not executed)` 'quotes'\n日本語\n";
        let mutable_source = unique_temp_path("mutable-spawn-brief");
        fs::write(&mutable_source, prompt).unwrap();

        let intent = store
            .accept_spawn_brief(AcceptSpawnBriefRequest {
                prompt: prompt.to_owned(),
                source: SpawnBriefSource {
                    kind: "file".to_owned(),
                    path: Some(mutable_source.display().to_string()),
                },
                requested_provider: "codex-fork".to_owned(),
                requested_model: Some("gpt-5.6".to_owned()),
                requested_reasoning_effort: Some("high".to_owned()),
                requested_name: Some("implementer".to_owned()),
                parent_session_id: Some("parent001".to_owned()),
                requested_node: Some("primary".to_owned()),
                requested_working_dir: Some("/repo".to_owned()),
            })
            .unwrap();

        fs::write(&mutable_source, "mutated after acceptance").unwrap();
        assert_eq!(store.read_spawn_brief(&intent.artifact).unwrap(), prompt);
        assert!(intent.artifact.path.contains("spawn-briefs"));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&intent.artifact.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );

        store
            .bind_spawn_launch_intent(&intent.id, "child001")
            .unwrap();
        let state: Value = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
        let persisted = &state["spawn_launch_intents"][0];
        assert_eq!(persisted["session_id"], "child001");
        assert_eq!(persisted["artifact"]["sha256"], intent.artifact.sha256);
        assert_eq!(persisted["requested_provider"], "codex-fork");
        assert_eq!(fs::read_to_string(&intent.artifact.path).unwrap(), prompt);
    }

    #[test]
    fn inbound_session_requests_cannot_supply_an_accepted_brief_binding() {
        let request: CreateCoreSessionRequest = serde_json::from_value(json!({
            "initial_message": "untrusted prompt",
            "spawn_launch_intent_id": "forged-intent",
            "spawn_brief_sha256": "forged-digest",
            "spawn_brief_path": "/tmp/forged-brief",
            "spawn_brief": {
                "intent_id": "forged-intent",
                "sha256": "forged-digest"
            }
        }))
        .unwrap();

        assert!(request.spawn_brief.is_none());
    }

    #[test]
    fn failed_create_launch_preserves_a_concurrently_retired_session() {
        let mut retired = reparent_test_session("retired01", None, "secret");
        let retired = retired.as_object_mut().unwrap();
        retired.insert("status".to_owned(), Value::String("stopped".to_owned()));
        retired.insert(
            "completion_status".to_owned(),
            Value::String("retired".to_owned()),
        );
        retired.insert(
            "stopped_at".to_owned(),
            Value::String("2026-06-01T00:02:00Z".to_owned()),
        );
        let mut state = json!({
            "sessions": [retired],
            "session_runtime_launches": [{
                "id": "launch01",
                "operation_kind": "create",
                "session_id": "retired01",
                "tmux_session": "claude-retired01",
                "working_dir": "/repo",
                "log_file": "/tmp/retired01.log",
                "provider": "codex-fork",
                "credential_sha256": sha256_text("secret"),
                "status": "launching",
                "created_at": "2026-06-01T00:00:00Z",
                "updated_at": "2026-06-01T00:00:00Z"
            }]
        });

        assert!(!remove_failed_provisional_runtime_session(
            &state,
            "retired01"
        ));
        mark_runtime_launch_failed(
            &mut state,
            "launch01",
            "retired01",
            false,
            "runtime exited during startup",
        )
        .unwrap();

        assert_eq!(state["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(state["sessions"][0]["completion_status"], "retired");
        assert_eq!(state["session_runtime_launches"][0]["status"], "failed");
    }

    static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarRestore {
        key: &'static str,
        value: Option<std::ffi::OsString>,
    }

    impl EnvVarRestore {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = env::var_os(key);
            env::set_var(key, value);
            Self {
                key,
                value: previous,
            }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            if let Some(value) = self.value.as_ref() {
                env::set_var(self.key, value);
            } else {
                env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn expand_home_handles_bare_home_and_home_relative_paths() {
        let Some(home) = env::var_os("HOME") else {
            return;
        };
        let home = PathBuf::from(home);
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/work"), home.join("work"));
        assert_eq!(expand_home("/tmp/work"), PathBuf::from("/tmp/work"));
    }

    #[test]
    fn unsupported_codex_fork_model_fails_before_session_state_is_written() {
        let state_file = unique_temp_path("unsupported-codex-fork-model");
        let store = SessionStore::new(state_file.clone());
        let mut config = AppConfig::default();
        config.codex_fork.command = "/bin/sh".to_owned();
        config.codex_fork.args = vec![
            "-c".to_owned(),
            "printf '%s' '{\"models\":[{\"slug\":\"gpt-5.6-luna\",\"visibility\":\"list\"}]}'"
                .to_owned(),
        ];
        let runtime = TmuxRuntime::from_app_config(&config);
        let request = CreateCoreSessionRequest {
            id: Some("badmodel".to_owned()),
            name: Some("bad-model".to_owned()),
            working_dir: Some("/tmp".to_owned()),
            provider: Some("codex-fork".to_owned()),
            parent_session_id: None,
            node: Some("primary".to_owned()),
            initial_message: Some("stand by".to_owned()),
            model: Some("luna".to_owned()),
            reasoning_effort: None,
            wait: None,
            spawn_prompt_source: None,
            spawn_brief: None,
        };

        let error = store
            .create_core_session_with_runtime(request, None, &runtime)
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<crate::runtime::CodexModelValidationError>(),
            Some(crate::runtime::CodexModelValidationError::Unsupported { requested, .. })
                if requested == "luna"
        ));
        assert!(store.list_sessions(true).unwrap().is_empty());
        let state = store.load_raw_json_value().unwrap();
        assert!(session_runtime_launch_records(&state).unwrap().is_empty());
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn claude_transcript_identity_prefers_metadata_for_nested_layouts() {
        let root = unique_temp_path("nested-claude-transcript");
        let transcript = root
            .join("path-session")
            .join("subagents")
            .join("agent-child.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(
            &transcript,
            json!({
                "type": "user",
                "sessionId": "metadata-session",
                "timestamp": "2026-01-01T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            provider_resume_id_from_transcript_path(transcript.to_str().unwrap()).as_deref(),
            Some("metadata-session")
        );
        assert_eq!(
            provider_resume_id_from_transcript_path(
                root.join("fallback-session")
                    .join("chat.jsonl")
                    .to_str()
                    .unwrap()
            )
            .as_deref(),
            Some("fallback-session")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn claude_project_roots_include_config_environment_and_defaults() {
        let _guard = TEST_ENV_LOCK.lock().unwrap();
        let root = unique_temp_path("claude-project-roots");
        let home = root.join("home");
        let config_dir_a = root.join("claude-config-a");
        let config_dir_b = root.join("claude-config-b");
        let xdg_dir = root.join("xdg");
        let configured = root.join("configured-projects");
        let _home = EnvVarRestore::set("HOME", &home);
        let _config = EnvVarRestore::set(
            "CLAUDE_CONFIG_DIR",
            format!("{}, {}", config_dir_a.display(), config_dir_b.display()),
        );
        let _xdg = EnvVarRestore::set("XDG_CONFIG_HOME", &xdg_dir);

        assert_eq!(
            claude_projects_roots(Some(configured.to_str().unwrap())),
            vec![
                configured,
                config_dir_a.join("projects"),
                config_dir_b.join("projects"),
                xdg_dir.join("claude").join("projects"),
                home.join(".claude").join("projects"),
            ]
        );
    }

    #[test]
    fn test_isolation_prevents_default_home_scans_from_being_restored() {
        let root = test_isolation_root_from_environment().unwrap().unwrap();
        let state_file = root.join("external-scan-fixture.json");
        let store = SessionStore::new(state_file)
            .with_codex_session_index_path(Some("~/.codex/session_index.jsonl"))
            .with_claude_transcript_root(None);

        assert!(store.codex_sessions_root.starts_with(&root));
        assert!(store
            .claude_projects_roots
            .iter()
            .all(|path| path.starts_with(&root)));
    }

    #[test]
    fn generated_session_ids_match_python_short_hex_contract() {
        let session_id = generate_session_id();

        assert_eq!(session_id.len(), 8);
        assert!(session_id.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(session_id, session_id.to_ascii_lowercase());
    }

    #[test]
    fn credential_rotation_idle_proof_matches_provider_lifecycle_sources() {
        let rotation = SessionCredentialRotationRecord {
            id: "rotation01".to_owned(),
            session_id: "abc12345".to_owned(),
            provider: "claude".to_owned(),
            provider_resume_id: "provider-thread".to_owned(),
            tmux_session: "claude-abc12345".to_owned(),
            tmux_socket_name: None,
            request_actor: "operator".to_owned(),
            status: "waiting_idle".to_owned(),
            requested_at: "2026-06-01T00:00:30Z".to_owned(),
            idle_proof_at: None,
            runtime_launch_id: None,
            updated_at: "2026-06-01T00:00:30Z".to_owned(),
            applied_at: None,
            failure_reason: None,
        };

        let mut claude = session_record("idle");
        claude.activity_hook_at = Some("2026-06-01T00:00:31Z".to_owned());
        assert!(credential_rotation_has_fresh_idle_proof(&rotation, &claude));
        claude.status = "running".to_owned();
        assert!(!credential_rotation_has_fresh_idle_proof(
            &rotation, &claude
        ));

        let mut codex_fork = session_record("idle");
        codex_fork.provider = "codex-fork".to_owned();
        codex_fork.last_activity = "2026-06-01T00:00:31Z".to_owned();
        assert!(credential_rotation_has_fresh_idle_proof(
            &rotation,
            &codex_fork
        ));
        codex_fork.status = "running".to_owned();
        assert!(!credential_rotation_has_fresh_idle_proof(
            &rotation,
            &codex_fork
        ));

        let mut stock_codex = session_record("running");
        stock_codex.provider = "codex".to_owned();
        assert!(credential_rotation_has_fresh_idle_proof(
            &rotation,
            &stock_codex
        ));
        stock_codex.status = "stopped".to_owned();
        assert!(!credential_rotation_has_fresh_idle_proof(
            &rotation,
            &stock_codex
        ));
    }

    fn terminal_rotation_fixture(rotation_status: &str, launch_status: Option<&str>) -> Value {
        let mut session = reparent_test_session("rotate01", None, "old-secret");
        session["provider_resume_id"] = json!("provider-thread");
        let mut state = json!({
            "sessions": [session],
            "session_credential_rotations": [{
                "id": "rotation01",
                "session_id": "rotate01",
                "provider": "claude",
                "provider_resume_id": "provider-thread",
                "tmux_session": "claude-rotate01",
                "request_actor": "operator",
                "status": rotation_status,
                "requested_at": "2026-06-01T00:00:00Z",
                "updated_at": "2026-06-01T00:00:00Z"
            }]
        });
        if let Some(launch_status) = launch_status {
            state["session_runtime_launches"] = json!([{
                "id": "launch01",
                "operation_kind": "recredential",
                "session_id": "rotate01",
                "tmux_session": "claude-rotate01",
                "working_dir": "/repo",
                "log_file": "/tmp/rotate01.log",
                "provider": "claude",
                "provider_resume_id": "provider-thread",
                "credential_rotation_id": "rotation01",
                "credential_sha256": sha256_text("new-secret"),
                "status": launch_status,
                "created_at": "2026-06-01T00:00:00Z",
                "updated_at": "2026-06-01T00:00:00Z"
            }]);
            state["session_credential_rotations"][0]["runtime_launch_id"] = json!("launch01");
        }
        state
    }

    fn isolated_test_tmux_runtime() -> (String, TmuxRuntime) {
        // Rust durable-path isolation deliberately does not choose a tmux
        // socket.  Tests which model a missing runtime must therefore use an
        // unguessable private socket rather than accidentally inspecting or
        // targeting the operator's default tmux server.
        let socket_name = format!(
            "sm-test-1322-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default())
            .for_socket_name(Some(&socket_name));
        (socket_name, runtime)
    }

    #[test]
    fn credential_rotation_stopped_before_admission_is_first_call_truthful_and_idempotent() {
        let state_file = unique_temp_path("credential-rotation-stopped-admission");
        let mut state = terminal_rotation_fixture("waiting_idle", None);
        state["sessions"][0]["status"] = json!("stopped");
        fs::write(&state_file, state.to_string()).unwrap();
        let store = SessionStore::new(state_file.clone()).with_delivery_runtime(Some(
            TmuxRuntime::from_config(&crate::config::RustCoreConfig::default()),
        ));

        for _ in 0..2 {
            assert!(matches!(
                store.create_session_credential_rotation("rotate01", "operator").unwrap(),
                CredentialRotationOutcome::BadRequest(detail) if detail == "session rotate01 is stopped"
            ));
        }
        let state = store.load_raw_json_value().unwrap();
        assert_eq!(state["sessions"][0]["status"], "stopped");
        assert_eq!(state["session_credential_rotations"][0]["status"], "failed");
        assert_eq!(
            state["session_credential_rotations"][0]["failure_reason"],
            "target_terminal"
        );
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn credential_rotation_missing_runtime_admission_never_uses_tail_or_persists_waiting() {
        let state_file = unique_temp_path("credential-rotation-missing-runtime-admission");
        let mut state = terminal_rotation_fixture("failed", None);
        state["session_credential_rotations"] = json!([]);
        // Deliberately stale-looking log content is present but is not read by
        // admission; only the missing tmux runtime decides terminal liveness.
        state["sessions"][0]["log_file"] = json!("/tmp/stale-auth-scrollback.log");
        let (tmux_socket_name, runtime) = isolated_test_tmux_runtime();
        state["sessions"][0]["tmux_socket_name"] = json!(tmux_socket_name);
        fs::write(&state_file, state.to_string()).unwrap();
        let store = SessionStore::new(state_file.clone()).with_delivery_runtime(Some(runtime));

        assert!(matches!(
            store.create_session_credential_rotation("rotate01", "operator").unwrap(),
            CredentialRotationOutcome::BadRequest(detail) if detail == "session rotate01 is stopped"
        ));
        let state = store.load_raw_json_value().unwrap();
        assert_eq!(state["sessions"][0]["status"], "stopped");
        assert!(state["session_credential_rotations"]
            .as_array()
            .unwrap()
            .is_empty());
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn credential_rotation_stop_between_admission_and_worker_finalizes_waiting_record() {
        let mut state = terminal_rotation_fixture("waiting_idle", None);
        // Deterministic interleaving: admission already persisted waiting_idle,
        // then the terminal writer wins before the worker's next poll.
        state["sessions"][0]["status"] = json!("stopped");
        finalize_active_credential_rotations_for_terminal_session(&mut state, "rotate01").unwrap();

        assert_eq!(state["session_credential_rotations"][0]["status"], "failed");
        assert_eq!(
            state["session_credential_rotations"][0]["failure_reason"],
            "target_terminal"
        );
        assert_eq!(state["sessions"][0]["status"], "stopped");
    }

    #[test]
    fn credential_rotation_send_undeliverable_finalizes_waiting_record() {
        let mut state = terminal_rotation_fixture("waiting_idle", None);
        let (tmux_socket_name, runtime) = isolated_test_tmux_runtime();
        state["sessions"][0]["tmux_socket_name"] = json!(tmux_socket_name);

        let (status, delivered) =
            deliver_runtime_text_to_session_raw(&mut state, "rotate01", "probe", &runtime).unwrap();

        assert!(!delivered);
        assert_eq!(status, "stopped");
        assert_eq!(state["sessions"][0]["status"], "stopped");
        assert_eq!(state["session_credential_rotations"][0]["status"], "failed");
        assert_eq!(
            state["session_credential_rotations"][0]["failure_reason"],
            "target_terminal"
        );
    }

    #[test]
    fn credential_rotation_stop_immediately_before_apply_fails_launch_and_blocks_recovery() {
        let mut state = terminal_rotation_fixture("relaunching", Some("launching"));
        // Deterministic interleaving: the worker owns a prepared relaunch, then
        // the terminal writer lands before its durable applied transition.
        state["sessions"][0]["status"] = json!("stopped");
        state["sessions"][0]["completion_status"] = json!("retired");
        finalize_active_credential_rotations_for_terminal_session(&mut state, "rotate01").unwrap();

        assert_eq!(state["session_credential_rotations"][0]["status"], "failed");
        assert_eq!(state["session_runtime_launches"][0]["status"], "failed");
        assert_eq!(
            state["session_runtime_launches"][0]["failure_reason"],
            "target_terminal"
        );

        let state_file = unique_temp_path("credential-rotation-terminal-recovery");
        fs::write(&state_file, state.to_string()).unwrap();
        let restarted = SessionStore::new(state_file.clone());
        restarted.recover_session_runtime_launches().unwrap();
        let recovered = restarted.load_raw_json_value().unwrap();
        assert_eq!(recovered["sessions"][0]["status"], "stopped");
        assert_eq!(
            recovered["session_credential_rotations"][0]["status"],
            "failed"
        );
        assert_eq!(recovered["session_runtime_launches"][0]["status"], "failed");
        let _ = fs::remove_file(state_file);
    }

    #[cfg(unix)]
    #[test]
    fn credential_rotation_terminal_completion_marker_blocks_prepared_and_launching_recovery_without_runtime_invocation(
    ) {
        // The durable completion marker is authoritative even if a delayed
        // activity-state writer left `status` looking idle.  Put a sentinel
        // tmux binary first on PATH: recovery must finalize the records
        // without invoking it for every persisted terminal-marker/launch
        // interleaving.
        let _guard = TEST_ENV_LOCK.lock().unwrap();
        let root = unique_temp_path("terminal-marker-recovery");
        fs::create_dir_all(&root).unwrap();
        let tmux = root.join("tmux");
        let invoked = root.join("runtime-invoked");
        fs::write(
            &tmux,
            "#!/bin/sh\n: > \"$SM_1322_FAKE_TMUX_SENTINEL\"\nexit 99\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();
        let old_path = env::var_os("PATH").unwrap_or_default();
        let _path = EnvVarRestore::set(
            "PATH",
            format!("{}:{}", root.display(), old_path.to_string_lossy()),
        );
        let _sentinel = EnvVarRestore::set("SM_1322_FAKE_TMUX_SENTINEL", &invoked);

        for (completion_status, launch_status) in [
            ("retired", "prepared"),
            ("retired", "launching"),
            ("killed", "prepared"),
            ("killed", "launching"),
        ] {
            for operation_kind in ["create", "recredential", "restore"] {
                let state_file = root.join(format!(
                    "{completion_status}-{launch_status}-{operation_kind}.json"
                ));
                let mut state = terminal_rotation_fixture("relaunching", Some(launch_status));
                // This is deliberately stale: the completion marker, not the
                // projection, must fence recovery before provider/tmux work.
                state["sessions"][0]["status"] = json!("idle");
                state["sessions"][0]["completion_status"] = json!(completion_status);
                state["sessions"][0]["stopped_at"] = json!("2026-06-01T00:01:00Z");
                state["session_runtime_launches"][0]["operation_kind"] = json!(operation_kind);
                // Missing is deliberately equivalent to false for legacy
                // serialized launch records.
                state["session_runtime_launches"][0]["restore_authorized"] = json!(false);
                fs::write(&state_file, state.to_string()).unwrap();

                let store = SessionStore::new(state_file.clone()).with_delivery_runtime(Some(
                    TmuxRuntime::from_config(&crate::config::RustCoreConfig::default()),
                ));
                store.recover_session_runtime_launches().unwrap();

                assert!(
                    !invoked.exists(),
                    "terminal {completion_status}/{launch_status}/{operation_kind} recovery invoked tmux"
                );
                let recovered = store.load_raw_json_value().unwrap();
                assert_eq!(
                    recovered["sessions"][0]["completion_status"],
                    completion_status
                );
                assert_eq!(recovered["sessions"][0]["status"], "stopped");
                assert_eq!(
                    recovered["session_credential_rotations"][0]["status"],
                    "failed"
                );
                assert_eq!(
                    recovered["session_credential_rotations"][0]["failure_reason"],
                    "target_terminal"
                );
                assert_eq!(recovered["session_runtime_launches"][0]["status"], "failed");
                assert_eq!(
                    recovered["session_runtime_launches"][0]["failure_reason"],
                    "target_terminal"
                );
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn credential_rotation_authorized_restore_with_terminal_marker_continues_recovery() {
        for (completion_status, launch_status) in [
            ("retired", "prepared"),
            ("retired", "launching"),
            ("killed", "prepared"),
            ("killed", "launching"),
        ] {
            let state_file = unique_temp_path("credential-rotation-authorized-restore");
            let mut state = terminal_rotation_fixture("relaunching", Some(launch_status));
            state["sessions"][0]["status"] = json!("stopped");
            state["sessions"][0]["completion_status"] = json!(completion_status);
            state["session_credential_rotations"] = json!([]);
            state["session_runtime_launches"][0]["operation_kind"] = json!("restore");
            state["session_runtime_launches"][0]["credential_rotation_id"] = Value::Null;
            state["session_runtime_launches"][0]["restore_authorized"] = json!(true);
            fs::write(&state_file, state.to_string()).unwrap();

            // No runtime is configured, so an allowed recovery reaches its
            // ordinary disabled-runtime failure without invoking a provider.
            let store = SessionStore::new(state_file.clone());
            store.recover_session_runtime_launches().unwrap();

            let recovered = store.load_raw_json_value().unwrap();
            assert_eq!(
                recovered["sessions"][0]["completion_status"],
                completion_status
            );
            assert_eq!(
                recovered["session_runtime_launches"][0]["failure_reason"],
                "runtime launch recovery is disabled"
            );
            assert_ne!(
                recovered["session_runtime_launches"][0]["failure_reason"],
                "target_terminal"
            );
            let _ = fs::remove_file(state_file);
        }
    }

    #[test]
    fn credential_rotation_transactional_stopped_launch_is_not_terminalized_during_recovery() {
        for launch_status in ["prepared", "launching"] {
            let state_file = unique_temp_path("credential-rotation-transactional-recovery");
            let mut state = terminal_rotation_fixture("relaunching", Some(launch_status));
            // Runtime launch recovery itself writes this transitional status
            // before restoring a provider. It is not an authoritative terminal
            // marker and must not be collapsed into target_terminal.
            state["sessions"][0]["status"] = json!("stopped");
            fs::write(&state_file, state.to_string()).unwrap();

            // With no runtime the normal recovery path reaches its explicit
            // disabled-runtime failure. That distinguishes it from the
            // terminal-marker fence without launching a provider in the test.
            let store = SessionStore::new(state_file.clone());
            store.recover_session_runtime_launches().unwrap();

            let recovered = store.load_raw_json_value().unwrap();
            assert!(recovered["sessions"][0]["completion_status"].is_null());
            assert_eq!(
                recovered["session_credential_rotations"][0]["failure_reason"],
                "runtime launch recovery is disabled"
            );
            assert_eq!(
                recovered["session_runtime_launches"][0]["failure_reason"],
                "runtime launch recovery is disabled"
            );
            let _ = fs::remove_file(state_file);
        }
    }

    #[test]
    fn credential_rotation_worker_recovers_a_relaunching_runtime_transaction() {
        let state_file = unique_temp_path("credential-rotation-relaunch-recovery");
        fs::write(
            &state_file,
            json!({
                "sessions": [reparent_test_session("rotate01", None, "old-secret")],
                "session_runtime_launches": [{
                    "id": "launch01",
                    "operation_kind": "recredential",
                    "session_id": "rotate01",
                    "tmux_session": "claude-rotate01",
                    "working_dir": "/repo",
                    "log_file": "/tmp/rotate01.log",
                    "provider": "claude",
                    "provider_resume_id": "provider-thread",
                    "credential_rotation_id": "rotation01",
                    "credential_sha256": sha256_text("new-secret"),
                    "status": "launching",
                    "created_at": "2026-06-01T00:00:00Z",
                    "updated_at": "2026-06-01T00:00:01Z"
                }],
                "session_credential_rotations": [{
                    "id": "rotation01",
                    "session_id": "rotate01",
                    "provider": "claude",
                    "provider_resume_id": "provider-thread",
                    "tmux_session": "claude-rotate01",
                    "request_actor": "operator",
                    "status": "relaunching",
                    "requested_at": "2026-06-01T00:00:00Z",
                    "runtime_launch_id": "launch01",
                    "updated_at": "2026-06-01T00:00:01Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new(state_file.clone());

        assert!(store
            .try_apply_waiting_credential_rotation("rotate01")
            .unwrap());

        let state = store.load_raw_json_value().unwrap();
        assert_eq!(state["session_runtime_launches"][0]["status"], "failed");
        assert_eq!(state["session_credential_rotations"][0]["status"], "failed");
        assert_ne!(
            state["session_credential_rotations"][0]["status"],
            "relaunching"
        );
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn credential_rotation_and_clear_share_one_seat_operation_fence() {
        let state_file = unique_temp_path("credential-rotation-clear-fence");
        let store = SessionStore::new(state_file.clone());
        let clear_guard = store.lock_clear_operation("codex001").unwrap();
        let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default());
        let contender_store = store.clone();
        let contender_runtime = runtime.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let contender = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _guards = contender_store
                .lock_credential_rotation_fences("codex001", &contender_runtime, "tmux-codex001")
                .unwrap();
            acquired_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(acquired_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        drop(clear_guard);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        contender.join().unwrap();
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn credential_rotation_uses_the_send_state_then_input_lock_order() {
        let state_file = unique_temp_path("credential-rotation-send-lock-order");
        let store = SessionStore::new(state_file.clone());
        let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default());
        let tmux_session = format!("lock-order-{}", generate_session_id());
        let held_input = runtime.lock_session_input(&tmux_session).unwrap();
        let send_store = store.clone();
        let send_runtime = runtime.clone();
        let send_session = tmux_session.clone();
        let (send_state_tx, send_state_rx) = std::sync::mpsc::channel();
        let (send_done_tx, send_done_rx) = std::sync::mpsc::channel();
        let send = thread::spawn(move || {
            let _state_guard = send_store.write_guard().unwrap();
            send_state_tx.send(()).unwrap();
            let _input_guard = send_runtime.lock_session_input(&send_session).unwrap();
            send_done_tx.send(()).unwrap();
        });
        send_state_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let rotation_store = store.clone();
        let rotation_runtime = runtime.clone();
        let rotation_session = tmux_session.clone();
        let (rotation_done_tx, rotation_done_rx) = std::sync::mpsc::channel();
        let rotation = thread::spawn(move || {
            let _guards = rotation_store
                .lock_credential_rotation_fences("codex001", &rotation_runtime, &rotation_session)
                .unwrap();
            rotation_done_tx.send(()).unwrap();
        });
        assert!(rotation_done_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());

        drop(held_input);
        send_done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        rotation_done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        send.join().unwrap();
        rotation.join().unwrap();
        let _ = fs::remove_file(state_file);
    }

    #[cfg(unix)]
    #[test]
    fn codex_fork_btw_retries_stale_epoch_with_the_same_request_id() {
        use std::os::unix::net::UnixListener;

        let socket_path =
            env::temp_dir().join(format!("sm-btw-{}.control.sock", generate_session_id()));
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut raw_request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut raw_request)
                    .unwrap();
                let request: Value = serde_json::from_str(&raw_request).unwrap();
                requests.push(request);
                let response = match index {
                    0 => json!({
                        "ok": true,
                        "epoch": "epoch-stale",
                        "result": { "epoch": "epoch-stale" }
                    }),
                    1 => json!({
                        "ok": false,
                        "error": {
                            "code": "stale_epoch",
                            "message": "stale epoch"
                        }
                    }),
                    2 => json!({
                        "ok": true,
                        "epoch": "epoch-fresh",
                        "result": { "epoch": "epoch-fresh" }
                    }),
                    3 => json!({
                        "ok": true,
                        "epoch": "epoch-fresh",
                        "result": {}
                    }),
                    _ => unreachable!(),
                };
                let mut raw_response = serde_json::to_string(&response).unwrap();
                raw_response.push('\n');
                stream.write_all(raw_response.as_bytes()).unwrap();
            }
            requests
        });

        codex_fork_submit_btw(&socket_path, "btw-request-1", "summarize").unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests[0]["command"], "get_epoch");
        assert_eq!(requests[1]["command"], "submit_btw");
        assert_eq!(requests[1]["request_id"], "btw-request-1");
        assert_eq!(requests[1]["expected_epoch"], "epoch-stale");
        assert_eq!(requests[1]["prompt"], "summarize");
        assert_eq!(requests[2]["command"], "get_epoch");
        assert_eq!(requests[3]["command"], "submit_btw");
        assert_eq!(requests[3]["request_id"], "btw-request-1");
        assert_eq!(requests[3]["expected_epoch"], "epoch-fresh");
        assert_eq!(requests[3]["prompt"], "summarize");
        let _ = fs::remove_file(socket_path);
    }

    #[cfg(unix)]
    #[test]
    fn codex_fork_btw_waits_for_control_socket_recovery() {
        use std::os::unix::net::UnixListener;

        let socket_path =
            env::temp_dir().join(format!("sm-btw-recovery-{}.sock", generate_session_id()));
        let server_path = socket_path.clone();
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            let listener = UnixListener::bind(&server_path).unwrap();
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut raw_request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut raw_request)
                    .unwrap();
                requests.push(serde_json::from_str::<Value>(&raw_request).unwrap());
                let response = if index == 0 {
                    json!({
                        "ok": true,
                        "epoch": "epoch-recovered",
                        "result": { "epoch": "epoch-recovered" }
                    })
                } else {
                    json!({
                        "ok": true,
                        "epoch": "epoch-recovered",
                        "result": {}
                    })
                };
                writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
            }
            requests
        });

        codex_fork_submit_btw(&socket_path, "btw-recovery", "summarize").unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests[0]["command"], "get_epoch");
        assert_eq!(requests[1]["command"], "submit_btw");
        assert_eq!(requests[1]["request_id"], "btw-recovery");
        let _ = fs::remove_file(socket_path);
    }

    fn session_record(status: &str) -> SessionRecord {
        SessionRecord {
            id: "abc12345".to_owned(),
            name: "claude-abc12345".to_owned(),
            working_dir: "/repo".to_owned(),
            tmux_session: "claude-abc12345".to_owned(),
            tmux_socket_name: None,
            node: "primary".to_owned(),
            provider: "claude".to_owned(),
            model: None,
            reasoning_effort: None,
            account_key: None,
            usage_cap_fraction: None,
            log_file: Some("/tmp/abc12345.log".to_owned()),
            provider_resume_id: None,
            transcript_path: None,
            codex_thread_id: None,
            forked_from_session_id: None,
            forked_from_provider_resume_id: None,
            forked_provider_resume_id: None,
            forked_at: None,
            forked_by_session_id: None,
            friendly_name: Some("Example".to_owned()),
            friendly_name_is_explicit: false,
            friendly_name_updated_at_ns: None,
            native_title: None,
            native_title_updated_at_ns: None,
            native_title_source_mtime_ns: None,
            telegram_chat_id: None,
            telegram_thread_id: None,
            telegram_topic_id: None,
            telegram_root_msg_id: None,
            current_task: None,
            git_remote_url: None,
            review_config: None,
            parent_session_id: None,
            session_credential_sha256: None,
            last_handoff_path: None,
            agent_status_text: None,
            agent_status_at: None,
            agent_task_completed_at: None,
            completion_status: None,
            completion_message: None,
            completed_at: None,
            stopped_at: None,
            is_em: false,
            role: None,
            status: status.to_owned(),
            spawned_at: Some("2026-06-01T00:00:00".to_owned()),
            created_at: "2026-06-01T00:00:00".to_owned(),
            last_activity: "2026-06-01T00:01:00".to_owned(),
            activity_hook_at: None,
            activity_turn_start_hook_at: None,
            last_tool_call: None,
            last_tool_name: None,
            tokens_used: 0,
            context_used_percentage: None,
            context_total_input_tokens: None,
            context_sampled_at: None,
            context_compaction_active: false,
            context_monitor_enabled: false,
            context_monitor_notify: None,
            context_monitor_notify_source: default_context_monitor_notify_source(),
            context_monitor_threshold_percentages: None,
            context_monitor_warning_percentage: None,
            context_monitor_critical_percentage: None,
            context_warning_sent: false,
            context_critical_sent: false,
            aliases: Vec::new(),
            pending_adoption_proposals: Vec::new(),
        }
    }

    #[test]
    fn session_projection_maps_legacy_status_and_activity() {
        let response = SessionResponse::from(session_record("waiting_permission"));

        assert_eq!(response.status, "idle");
        assert_eq!(response.activity_state, "idle");
    }

    #[test]
    fn session_projection_keeps_stored_idle_until_live_projection() {
        let mut status_session = session_record("idle");
        status_session.agent_status_text = Some("Working on a review".to_owned());
        status_session.agent_status_at = Some(now_rfc3339());
        let response = SessionResponse::from(status_session.clone());
        assert_eq!(response.activity_state, "idle");
        let client_response = ClientSessionResponse::from(status_session);
        assert_eq!(client_response.session.activity_state, "idle");

        let mut stale_status_session = session_record("idle");
        stale_status_session.agent_status_text = Some("old work".to_owned());
        stale_status_session.agent_status_at = Some("2026-06-01T00:00:00Z".to_owned());
        let response = SessionResponse::from(stale_status_session);
        assert_eq!(response.activity_state, "idle");

        let mut completed_task_session = session_record("idle");
        completed_task_session.agent_status_text = Some("recent but complete".to_owned());
        completed_task_session.agent_status_at = Some(now_rfc3339());
        completed_task_session.agent_task_completed_at = Some(now_rfc3339());
        let response = SessionResponse::from(completed_task_session);
        assert_eq!(response.activity_state, "idle");

        let mut local_status_session = session_record("idle");
        local_status_session.agent_status_text = Some("local timestamp".to_owned());
        local_status_session.agent_status_at = Some(now_python_naive_iso());
        let response = SessionResponse::from(local_status_session);
        assert_eq!(response.activity_state, "idle");

        let mut stale_task_session = session_record("idle");
        stale_task_session.current_task = Some("reviewing".to_owned());
        stale_task_session.last_activity = now_rfc3339();
        let response = SessionResponse::from(stale_task_session);
        assert_eq!(response.activity_state, "idle");

        let mut explicit_idle_session = session_record("idle");
        explicit_idle_session.last_activity = now_rfc3339();
        let response = SessionResponse::from(explicit_idle_session);
        assert_eq!(response.activity_state, "idle");

        let mut recent_activity_session = session_record("paused");
        recent_activity_session.last_activity = now_rfc3339();
        let response = SessionResponse::from(recent_activity_session);
        assert_eq!(response.activity_state, "idle");

        let stale_running_session = session_record("running");
        let response = SessionResponse::from(stale_running_session);
        assert_eq!(response.activity_state, "idle");

        let mut recent_running_session = session_record("running");
        recent_running_session.last_activity = now_rfc3339();
        let response = SessionResponse::from(recent_running_session);
        assert_eq!(response.activity_state, "working");

        let mut recent_naive_running_session = session_record("running");
        recent_naive_running_session.last_activity = now_python_naive_iso();
        let response = SessionResponse::from(recent_naive_running_session);
        assert_eq!(response.activity_state, "working");
    }

    #[test]
    fn session_projection_uses_canonical_waiting_input_activity() {
        let mut session = session_record("idle");
        session.completion_status = Some("completed".to_owned());
        let response = SessionResponse::from(session);
        assert_eq!(response.activity_state, "waiting_input");
    }

    #[test]
    fn client_projection_disables_unported_attach_surfaces() {
        let response = ClientSessionResponse::from(session_record("running"));

        assert!(!response.attach_descriptor.attach_supported);
        assert!(response.termux_attach.is_none());
        assert_eq!(response.mobile_terminal["supported"], false);
        assert_eq!(response.primary_action.action_type, "details");
    }

    #[test]
    fn cached_display_name_prefers_newer_native_title() {
        let mut session = session_record("running");
        session.friendly_name = Some("stale-friendly-name".to_owned());
        session.friendly_name_updated_at_ns = Some(10);
        session.native_title = Some("cached-native-title".to_owned());
        session.native_title_updated_at_ns = Some(20);
        let response = SessionResponse::from(session);

        assert_eq!(
            response.friendly_name.as_deref(),
            Some("cached-native-title")
        );
    }

    #[test]
    fn cached_display_name_keeps_newer_explicit_friendly_name() {
        let mut session = session_record("running");
        session.friendly_name = Some("explicit-name".to_owned());
        session.friendly_name_is_explicit = true;
        session.friendly_name_updated_at_ns = Some(30);
        session.native_title = Some("older-native-title".to_owned());
        session.native_title_updated_at_ns = Some(20);
        let response = SessionResponse::from(session);

        assert_eq!(response.friendly_name.as_deref(), Some("explicit-name"));
    }

    #[test]
    fn cached_display_name_falls_back_to_session_name_or_id() {
        let mut session = session_record("running");
        session.friendly_name = None;
        let response = SessionResponse::from(session.clone());

        assert_eq!(response.friendly_name.as_deref(), Some("claude-abc12345"));

        session.name = String::new();
        let response = SessionResponse::from(session);

        assert_eq!(response.friendly_name.as_deref(), Some("abc12345"));
    }

    fn store_with_running_claude_session(session_id: &str) -> SessionStore {
        let state_file = unique_temp_path(session_id);
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    {
                        "id": session_id,
                        "name": format!("claude-{session_id}"),
                        "working_dir": "/repo",
                        "tmux_session": format!("claude-{session_id}"),
                        "log_file": format!("/tmp/{session_id}.log"),
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        SessionStore::new_with_legacy_fallback(state_file.clone(), state_file)
    }

    #[test]
    fn claude_stop_hook_idle_survives_a_server_restart() {
        // The Stop hook is the authoritative idle signal; if it only lived in
        // memory a restart would revert a finished session to stale active.
        let store = store_with_running_claude_session("stopdurable");
        assert!(store
            .apply_claude_stop_hook("stopdurable", None, None, None, None, None, None)
            .unwrap());

        // A fresh store over the same state file stands in for a restarted server.
        let restarted = SessionStore::new_with_legacy_fallback(
            store.state_file.clone(),
            store.state_file.clone(),
        );
        let session = restarted.get_session("stopdurable").unwrap().unwrap();

        assert_eq!(session.status, "idle");
        assert_eq!(projected_activity_state(&session, &session.status), "idle");
    }

    #[test]
    fn claude_stop_hook_releases_session_lock_before_blocked_ledger_write() {
        let store = store_with_running_claude_session("stopledgerlock");
        store
            .seat_session_store
            .append("seed", "claude", "seed-thread", None)
            .unwrap();
        let usage_db_path = store.state_file.with_extension("usage.db");
        let connection = rusqlite::Connection::open(usage_db_path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();

        let stop_store = store.clone();
        let stop = thread::spawn(move || {
            stop_store.apply_claude_stop_hook(
                "stopledgerlock",
                None,
                None,
                None,
                Some("/tmp/rebound-thread.jsonl"),
                None,
                None,
            )
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        let persisted_before_unlock = loop {
            if store
                .get_session("stopledgerlock")
                .unwrap()
                .and_then(|session| session.transcript_path)
                .as_deref()
                == Some("/tmp/rebound-thread.jsonl")
            {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let mutation_elapsed = if persisted_before_unlock {
            let started = Instant::now();
            assert!(store
                .apply_claude_pre_tool_use_hook("stopledgerlock", Some("Read"))
                .unwrap());
            Some(started.elapsed())
        } else {
            None
        };

        connection.execute_batch("ROLLBACK").unwrap();
        assert!(stop.join().unwrap().unwrap());
        assert!(
            persisted_before_unlock,
            "core Stop state remained behind the blocked usage ledger"
        );
        assert!(
            mutation_elapsed.is_some_and(|elapsed| elapsed < Duration::from_secs(1)),
            "the global session lock remained held during the usage ledger wait"
        );
    }

    #[test]
    fn claude_stop_hook_alone_does_not_suppress_the_pane_fallback() {
        // Without a turn-start hook the next turn is invisible until its first
        // PreToolUse, so a stored idle must not gate the pane off.
        let store = store_with_running_claude_session("stoponly");
        assert!(store
            .apply_claude_stop_hook("stoponly", None, None, None, None, None, None)
            .unwrap());

        let session = store.get_session("stoponly").unwrap().unwrap();

        assert!(session.activity_turn_start_hook_at.is_none());
        assert_eq!(claude_hook_gate(&session), ClaudeHookGate::Stale);
    }

    #[test]
    fn claude_stop_hook_is_conclusive_once_both_ends_of_the_turn_are_hooked() {
        let store = store_with_running_claude_session("bothends");
        assert!(store
            .apply_claude_user_prompt_submit_hook("bothends", None)
            .unwrap());
        assert!(store
            .apply_claude_stop_hook("bothends", None, None, None, None, None, None)
            .unwrap());

        let session = store.get_session("bothends").unwrap().unwrap();

        assert_eq!(session.status, "idle");
        assert_eq!(claude_hook_gate(&session), ClaudeHookGate::TurnStopped);
    }

    #[test]
    fn stale_stop_hook_does_not_overwrite_a_newer_turn_start() {
        // notify_server.sh dispatches detached curls and the Stop path can sleep
        // on its transcript retry, so a Stop received before the next prompt can
        // still land after that prompt's UserPromptSubmit.
        let store = store_with_running_claude_session("raced");
        let stop_received_at = now_rfc3339();
        assert!(store
            .apply_claude_user_prompt_submit_hook("raced", None)
            .unwrap());

        assert!(!store
            .apply_claude_stop_hook(
                "raced",
                Some("summary from the previous turn"),
                None,
                None,
                None,
                Some(&stop_received_at),
                None
            )
            .unwrap());

        let session = store.get_session("raced").unwrap().unwrap();

        assert_eq!(session.status, "running");
        assert_eq!(claude_hook_gate(&session), ClaudeHookGate::TurnRunning);

        let raw = store.load_raw_json_value().unwrap();
        let sessions = raw["sessions"].as_array().unwrap();
        let raw_session = sessions
            .iter()
            .find(|session| session["id"] == "raced")
            .unwrap();
        assert_eq!(
            raw_session["last_action_summary"], "summary from the previous turn",
            "a superseded Stop still carries the freshest transcript metadata"
        );
    }

    #[test]
    fn hook_emission_stamps_from_both_shell_clock_paths_are_comparable() {
        // notify_server.sh emits 9 fractional digits via GNU-style `date +%N` and
        // 6 via the perl Time::HiRes fallback. An unparseable stamp makes
        // `timestamp_is_after` return false, silently disabling the ordering
        // guard rather than failing loudly — so both shapes are pinned here.
        let nanos = "2026-07-27T18:30:55.266446000Z";
        let micros = "2026-07-27T18:30:55.266447Z";

        assert!(timestamp_is_after(micros, nanos));
        assert!(!timestamp_is_after(nanos, micros));
        assert!(!timestamp_is_after(nanos, nanos));
        // Mixed resolutions across the two paths must still order correctly.
        assert!(timestamp_is_after(
            "2026-07-27T18:30:56.000001Z",
            "2026-07-27T18:30:55.999999999Z"
        ));
        // Python-era state may have a naive ISO stamp while newer hooks carry
        // RFC3339. Both accepted encodings must compare on one timeline.
        assert!(timestamp_is_after(
            "2026-07-27T18:30:56.000001Z",
            "2026-07-27T18:30:55.999999"
        ));
        assert!(!timestamp_is_after(
            "2026-07-27T18:30:55.999999",
            "2026-07-27T18:30:56.000001Z"
        ));
        // An unparseable side must never supersede anything.
        assert!(!timestamp_is_after("not-a-timestamp", nanos));
        assert!(!timestamp_is_after(nanos, "not-a-timestamp"));
    }

    #[test]
    fn stop_hook_delivered_after_the_next_turn_is_ordered_by_emission_time() {
        // Each hook gets its own detached curl, so a Stop can be delayed until
        // after the next turn's UserPromptSubmit has already been delivered. By
        // arrival order the Stop looks newest; only the emission stamps, taken
        // before either curl detached, reveal the real order.
        let store = store_with_running_claude_session("latecurl");
        let stop_emitted_at = "2026-06-01T00:00:10.000000Z";
        let turn_start_emitted_at = "2026-06-01T00:00:11.000000Z";

        assert!(store
            .apply_claude_user_prompt_submit_hook("latecurl", Some(turn_start_emitted_at))
            .unwrap());

        // Arrival is now, i.e. after the turn start was stored, so the arrival
        // guard alone would let this stale Stop through.
        let stop_received_at = now_rfc3339();
        assert!(!store
            .apply_claude_stop_hook(
                "latecurl",
                None,
                None,
                None,
                None,
                Some(&stop_received_at),
                Some(stop_emitted_at)
            )
            .unwrap());

        let session = store.get_session("latecurl").unwrap().unwrap();

        assert_eq!(session.status, "running");
        assert_eq!(claude_hook_gate(&session), ClaudeHookGate::TurnRunning);
    }

    #[test]
    fn turn_start_delivered_after_its_own_stop_does_not_resurrect_the_turn() {
        // Mirror of the delayed-Stop case: each hook has its own detached curl,
        // so a turn-start can also lose the race against the Stop that ended it.
        let store = store_with_running_claude_session("lateprompt");
        assert!(store
            .apply_claude_stop_hook(
                "lateprompt",
                None,
                None,
                None,
                None,
                Some(&now_rfc3339()),
                Some("2026-06-01T00:00:11.000000Z")
            )
            .unwrap());

        assert!(!store
            .apply_claude_user_prompt_submit_hook("lateprompt", Some("2026-06-01T00:00:10.000000Z"))
            .unwrap());

        assert_eq!(
            store.get_session("lateprompt").unwrap().unwrap().status,
            "idle"
        );
    }

    #[test]
    fn turn_start_emitted_after_the_last_stop_still_applies() {
        let store = store_with_running_claude_session("nextturn");
        assert!(store
            .apply_claude_stop_hook(
                "nextturn",
                None,
                None,
                None,
                None,
                Some(&now_rfc3339()),
                Some("2026-06-01T00:00:10.000000Z")
            )
            .unwrap());

        assert!(store
            .apply_claude_user_prompt_submit_hook("nextturn", Some("2026-06-01T00:00:11.000000Z"))
            .unwrap());

        assert_eq!(
            store.get_session("nextturn").unwrap().unwrap().status,
            "running"
        );
    }

    #[test]
    fn remote_sessions_trust_a_running_hook_past_the_pane_reconciliation_window() {
        // A remote session has no pane to reconcile against, so the shorter
        // primary-node window would drop it onto the default projection — which
        // calls a running session idle after 30s — mid-turn.
        let stale_for_primary = OffsetDateTime::now_utc() - TimeDuration::seconds(300);
        let mut session = session_record("running");
        session.node = "macbook".to_owned();
        session.activity_hook_at = Some(stale_for_primary.format(&Rfc3339).unwrap());

        assert_eq!(claude_hook_gate(&session), ClaudeHookGate::TurnRunning);

        session.node = "primary".to_owned();
        assert_eq!(claude_hook_gate(&session), ClaudeHookGate::Stale);
    }

    #[test]
    fn stop_hook_emitted_after_the_last_turn_start_still_applies() {
        let store = store_with_running_claude_session("orderok");
        assert!(store
            .apply_claude_user_prompt_submit_hook("orderok", Some("2026-06-01T00:00:10.000000Z"))
            .unwrap());

        assert!(store
            .apply_claude_stop_hook(
                "orderok",
                None,
                None,
                None,
                None,
                Some(&now_rfc3339()),
                Some("2026-06-01T00:00:11.000000Z")
            )
            .unwrap());

        assert_eq!(
            store.get_session("orderok").unwrap().unwrap().status,
            "idle"
        );
    }

    #[test]
    fn stop_hook_applies_normally_when_no_newer_lifecycle_hook_raced_it() {
        let store = store_with_running_claude_session("unraced");
        assert!(store
            .apply_claude_user_prompt_submit_hook("unraced", None)
            .unwrap());
        let stop_received_at = now_rfc3339();

        assert!(store
            .apply_claude_stop_hook(
                "unraced",
                None,
                None,
                None,
                None,
                Some(&stop_received_at),
                None
            )
            .unwrap());

        assert_eq!(
            store.get_session("unraced").unwrap().unwrap().status,
            "idle"
        );
    }

    #[test]
    fn completed_claude_handoff_clears_refreshed_reservation_for_consumed_intent() {
        let mut session = json!({
            "pending_handoff_path": "/tmp/handoff.md",
            "pending_handoff_recorded_at": "2026-08-11T20:00:00Z",
            "claude_handoff_in_progress_at": "2026-08-11T20:00:02Z"
        })
        .as_object()
        .unwrap()
        .clone();

        let outcome = consume_completed_claude_handoff_raw(
            &mut session,
            "/tmp/handoff.md",
            "2026-08-11T20:00:00Z",
            "2026-08-11T20:00:01Z",
        );
        assert_eq!(outcome, (true, true));
        assert!(session["pending_handoff_path"].is_null());
        assert!(session["pending_handoff_recorded_at"].is_null());
        assert!(session["claude_handoff_in_progress_at"].is_null());
    }

    #[test]
    fn retiring_claude_handoff_worker_detects_a_replacement_reservation() {
        let session = json!({
            "provider": "claude",
            "status": "running",
            "node": "primary",
            "pending_handoff_path": "/tmp/replacement.md",
            "pending_handoff_recorded_at": "2026-08-11T20:00:02Z",
            "claude_handoff_in_progress_at": "2026-08-11T20:00:03Z"
        });

        assert!(claude_handoff_reservation_replaced_raw(
            session.as_object().unwrap(),
            "/tmp/original.md",
            "2026-08-11T20:00:00Z",
            "2026-08-11T20:00:01Z",
        ));
        assert!(!claude_handoff_reservation_replaced_raw(
            session.as_object().unwrap(),
            "/tmp/replacement.md",
            "2026-08-11T20:00:02Z",
            "2026-08-11T20:00:03Z",
        ));
    }

    #[test]
    fn failed_claude_handoff_worker_start_releases_matching_reservation() {
        let store = store_with_running_claude_session("workerfail");
        {
            let mut state = store.load_raw_json_value().unwrap();
            let sessions = ensure_sessions_array_mut(&mut state).unwrap();
            let session = session_object_mut(sessions, "workerfail").unwrap();
            session.insert(
                "pending_handoff_path".to_owned(),
                Value::String("/tmp/workerfail-handoff.md".to_owned()),
            );
            session.insert(
                "pending_handoff_recorded_at".to_owned(),
                Value::String("2026-08-11T20:00:00Z".to_owned()),
            );
            session.insert(
                "claude_handoff_in_progress_at".to_owned(),
                Value::String("2026-08-11T20:00:01Z".to_owned()),
            );
            store.write_raw_json_value(&state).unwrap();
        }

        assert!(store
            .release_failed_claude_handoff_worker_reservation(
                "workerfail",
                "/tmp/workerfail-handoff.md",
                "2026-08-11T20:00:00Z",
                "2026-08-11T20:00:01Z",
            )
            .unwrap());

        let state = store.load_raw_json_value().unwrap();
        let session = raw_session_object(&state, "workerfail").unwrap();
        assert_eq!(json_text(session.get("status")).as_deref(), Some("idle"));
        assert!(json_text(session.get("pending_handoff_path")).is_some());
        assert!(json_text(session.get("pending_handoff_recorded_at")).is_some());
        assert!(json_text(session.get("claude_handoff_in_progress_at")).is_none());
        assert_eq!(
            json_text(session.get("error_message")).as_deref(),
            Some("claude_handoff_failed: failed to start handoff worker")
        );
    }

    #[test]
    fn claude_user_prompt_submit_hook_marks_the_turn_running() {
        let store = store_with_running_claude_session("turnstart");
        {
            let mut state = store.load_raw_json_value().unwrap();
            let sessions = ensure_sessions_array_mut(&mut state).unwrap();
            let session = session_object_mut(sessions, "turnstart").unwrap();
            session.insert(
                "pending_handoff_path".to_owned(),
                Value::String("/tmp/turnstart-handoff.md".to_owned()),
            );
            session.insert(
                "pending_handoff_recorded_at".to_owned(),
                Value::String("2026-06-01T00:00:00Z".to_owned()),
            );
            store.write_raw_json_value(&state).unwrap();
        }
        assert!(store
            .apply_claude_stop_hook("turnstart", None, None, None, None, None, None)
            .unwrap());
        let reserved = store.load_raw_json_value().unwrap();
        assert!(json_text(
            raw_session_object(&reserved, "turnstart")
                .unwrap()
                .get("claude_handoff_in_progress_at")
        )
        .is_some());
        assert!(store
            .apply_claude_user_prompt_submit_hook("turnstart", None)
            .unwrap());

        let session = store.get_session("turnstart").unwrap().unwrap();

        assert_eq!(session.status, "running");
        assert!(session.agent_task_completed_at.is_none());
        assert_eq!(claude_hook_gate(&session), ClaudeHookGate::TurnRunning);
        let cancelled = store.load_raw_json_value().unwrap();
        assert!(json_text(
            raw_session_object(&cancelled, "turnstart")
                .unwrap()
                .get("claude_handoff_in_progress_at")
        )
        .is_none());
    }

    #[test]
    fn claude_user_prompt_submit_hook_leaves_stopped_sessions_alone() {
        let store = store_with_running_claude_session("stoppedsess");
        {
            let mut state = store.load_raw_json_value().unwrap();
            let sessions = ensure_sessions_array_mut(&mut state).unwrap();
            session_object_mut(sessions, "stoppedsess")
                .unwrap()
                .insert("status".to_owned(), Value::String("stopped".to_owned()));
            store.write_raw_json_value(&state).unwrap();
        }

        assert!(!store
            .apply_claude_user_prompt_submit_hook("stoppedsess", None)
            .unwrap());
        assert_eq!(
            store.get_session("stoppedsess").unwrap().unwrap().status,
            "stopped"
        );
    }

    #[test]
    fn claude_hook_gate_falls_back_to_the_pane_when_hooks_were_never_observed() {
        let session = session_record("running");

        assert!(session.activity_hook_at.is_none());
        assert_eq!(claude_hook_gate(&session), ClaudeHookGate::Untracked);
    }

    #[test]
    fn claude_hook_gate_goes_stale_when_a_running_turn_signal_ages_out() {
        let mut session = session_record("running");
        session.activity_hook_at = Some("2026-06-01T00:01:00Z".to_owned());

        assert_eq!(claude_hook_gate(&session), ClaudeHookGate::Stale);
    }

    #[test]
    fn killed_completion_marker_keeps_status_drift_out_of_the_live_roster() {
        let state_file = unique_temp_path("killed-status-drift");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "retired1",
                    "name": "claude-retired1",
                    "working_dir": "/repo",
                    "tmux_session": "claude-retired1",
                    "provider": "claude",
                    "parent_session_id": "parent1",
                    "status": "idle",
                    "completion_status": "killed",
                    "completed_at": "2026-06-01T00:02:00Z",
                    "stopped_at": "2026-06-01T00:02:00Z",
                    "context_used_percentage": 38.0,
                    "context_sampled_at": "2026-06-01T00:01:00Z",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:02:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new(state_file.clone());

        assert!(store.list_sessions(false).unwrap().is_empty());
        let retained = store.list_sessions(true).unwrap();
        assert_eq!(retained.len(), 1);
        assert!(retained[0].is_stopped());
        assert!(!store
            .apply_claude_pre_tool_use_hook("retired1", Some("Bash"))
            .unwrap());
        assert!(!store
            .apply_claude_user_prompt_submit_hook("retired1", None)
            .unwrap());
        let input = store
            .send_core_input(
                "retired1",
                SendCoreInputRequest {
                    text: "must not deliver".to_owned(),
                    delivery_mode: "sequential".to_owned(),
                    sender_session_id: None,
                    from_sm_send: false,
                    timeout_seconds: None,
                    notify_on_delivery: false,
                    notify_after_seconds: None,
                    notify_on_stop: false,
                    remind_soft_threshold: None,
                    remind_hard_threshold: None,
                    remind_cancel_on_reply_session_id: None,
                    parent_session_id: None,
                },
            )
            .unwrap()
            .unwrap();
        assert!(!input.delivered);
        assert_eq!(input.status, "stopped");
        let clear_request = ClearSessionRequest {
            prompt: None,
            requester_session_id: Some("parent1".to_owned()),
        };
        assert!(matches!(
            store
                .clear_core_session("retired1", clear_request.clone())
                .unwrap(),
            CoreClearOutcome::NotRunning
        ));
        assert!(matches!(
            store
                .clear_core_session_with_runtime(
                    "retired1",
                    clear_request,
                    &TmuxRuntime::from_config(&crate::config::RustCoreConfig::default()),
                )
                .unwrap(),
            CoreClearOutcome::NotRunning
        ));
        assert_eq!(
            store
                .get_context_snapshot("retired1")
                .unwrap()
                .unwrap()
                .lifecycle_status,
            "stopped"
        );
        assert_eq!(
            store.get_session("retired1").unwrap().unwrap().status,
            "idle"
        );
        let restored = store.restore_core_session("retired1").unwrap().unwrap();
        let CoreRestoreOutcome::Restored(restored) = restored else {
            panic!("killed-marker session should be restorable");
        };
        assert_eq!(restored.status, "running");
        assert_eq!(restored.completion_status, None);
        assert_eq!(store.list_sessions(false).unwrap().len(), 1);

        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn completion_reconciler_retains_messages_for_stopped_targets() {
        let state_file = unique_temp_path("queue-completion-stopped-target");
        let queue_db = state_file.with_extension("message-queue.db");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "stopped1",
                    "name": "codex-stopped1",
                    "working_dir": "/repo",
                    "tmux_session": "codex-stopped1",
                    "provider": "codex-fork",
                    "status": "stopped",
                    "completion_status": "killed",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue
            .enqueue_message_once_with_metadata(
                "queue-completion-job-test",
                "stopped1",
                "[sm queue] job-test completed: failed",
                "sequential",
                QueueMessageMetadata {
                    message_category: Some("queue-completion".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone())
            .with_delivery_runtime(Some(TmuxRuntime::from_config(
                &crate::config::RustCoreConfig::default(),
            )));

        assert_eq!(
            store
                .drain_runtime_pending_message_targets_by_category("queue-completion")
                .unwrap(),
            1
        );
        assert_eq!(
            queue
                .pending_messages_for_target_by_category("stopped1", "queue-completion", 10,)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn completion_reconciler_retains_messages_for_busy_targets() {
        let state_file = unique_temp_path("queue-completion-busy-target");
        let queue_db = state_file.with_extension("message-queue.db");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "busy1",
                    "name": "codex-busy1",
                    "working_dir": "/repo",
                    "tmux_session": "codex-busy1",
                    "provider": "codex-fork",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue
            .enqueue_message_once_with_metadata(
                "queue-completion-job-busy",
                "busy1",
                "[sm queue] job-busy completed: failed",
                "sequential",
                QueueMessageMetadata {
                    message_category: Some("queue-completion".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone())
            .with_delivery_runtime(Some(TmuxRuntime::from_config(
                &crate::config::RustCoreConfig::default(),
            )));

        assert_eq!(
            store
                .drain_runtime_pending_message_targets_by_category("queue-completion")
                .unwrap(),
            1
        );
        assert_eq!(
            queue
                .pending_messages_for_target_by_category("busy1", "queue-completion", 10,)
                .unwrap()
                .len(),
            1
        );
        let state: Value = serde_json::from_str(&fs::read_to_string(state_file).unwrap()).unwrap();
        let session = state["sessions"][0].as_object().unwrap();
        assert_eq!(
            session.get("status").and_then(Value::as_str),
            Some("running")
        );
        assert!(!session.contains_key("stopped_at"));
    }

    struct QueueCompletionTestPane {
        socket_name: String,
        input_path: PathBuf,
        runtime: TmuxRuntime,
        session_name: String,
    }

    impl Drop for QueueCompletionTestPane {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["-L", &self.socket_name, "kill-server"])
                .status();
            let _ = fs::remove_file(&self.input_path);
        }
    }

    impl QueueCompletionTestPane {
        fn input_lines(&self) -> Vec<String> {
            for _ in 0..100 {
                if let Ok(text) = fs::read_to_string(&self.input_path) {
                    return text.lines().map(ToOwned::to_owned).collect();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Vec::new()
        }
    }

    fn queue_completion_test_shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn queue_completion_test_pane(input_ready: bool) -> QueueCompletionTestPane {
        let (socket_name, runtime) = isolated_test_tmux_runtime();
        let session_name = "queue-completion-target".to_owned();
        let input_path = unique_temp_path("queue-completion-input");
        let pane = if input_ready { ">" } else { ">\n✽ Thinking" };
        let command = format!(
            "printf '%s\\n' {}; while IFS= read -r line; do printf '%s\\n' \"$line\" >> {}; printf '%s\\n' {}; done",
            queue_completion_test_shell_quote(pane),
            queue_completion_test_shell_quote(&input_path.display().to_string()),
            queue_completion_test_shell_quote(pane),
        );
        let output = Command::new("tmux")
            .args([
                "-L",
                &socket_name,
                "new-session",
                "-d",
                "-s",
                &session_name,
                "sh",
                "-c",
                &command,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to create isolated queue test pane: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        QueueCompletionTestPane {
            socket_name,
            input_path,
            runtime,
            session_name,
        }
    }

    fn queue_completion_test_store(
        pane: &QueueCompletionTestPane,
        status: &str,
    ) -> (SessionStore, RetainedQueueStore, PathBuf, PathBuf) {
        let state_file = unique_temp_path("queue-completion-state");
        let queue_db = state_file.with_extension("message-queue.db");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "queue-target",
                    "name": "queue-target",
                    "working_dir": "/repo",
                    "tmux_session": pane.session_name,
                    "tmux_socket_name": pane.socket_name,
                    "provider": "claude",
                    "status": status,
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:00:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone())
            .with_delivery_runtime(Some(pane.runtime.clone()));
        (store, queue, state_file, queue_db)
    }

    fn enqueue_queue_completion(queue: &RetainedQueueStore, id: &str) {
        queue
            .enqueue_message_once_with_metadata(
                id,
                "queue-target",
                "[sm queue] test job completed",
                "sequential",
                QueueMessageMetadata {
                    message_category: Some("queue-completion".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
    }

    #[test]
    fn completion_reconciler_delivers_to_a_provider_ready_stale_running_target() {
        let pane = queue_completion_test_pane(true);
        let (store, queue, state_file, queue_db) = queue_completion_test_store(&pane, "running");
        enqueue_queue_completion(&queue, "queue-completion-stale-running");

        store
            .drain_runtime_pending_message_targets_by_category("queue-completion")
            .unwrap();

        assert!(queue
            .pending_messages_for_target_by_category("queue-target", "queue-completion", 10)
            .unwrap()
            .is_empty());
        assert_eq!(pane.input_lines(), vec!["[sm queue] test job completed"]);
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn completion_reconciler_retains_idle_projection_when_provider_is_still_active() {
        let pane = queue_completion_test_pane(false);
        let (store, queue, state_file, queue_db) = queue_completion_test_store(&pane, "idle");
        enqueue_queue_completion(&queue, "queue-completion-active-pane");

        store
            .drain_runtime_pending_message_targets_by_category("queue-completion")
            .unwrap();

        assert_eq!(
            queue
                .pending_messages_for_target_by_category("queue-target", "queue-completion", 10)
                .unwrap()
                .len(),
            1
        );
        assert!(pane.input_lines().is_empty());
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn completion_reconciler_retains_ready_target_with_pending_claude_handoff() {
        let pane = queue_completion_test_pane(true);
        let (store, queue, state_file, queue_db) = queue_completion_test_store(&pane, "running");
        let mut state: Value =
            serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        let session = state["sessions"][0].as_object_mut().unwrap();
        session.insert(
            "pending_handoff_path".to_owned(),
            Value::String("/tmp/queued-handoff.md".to_owned()),
        );
        session.insert(
            "pending_handoff_recorded_at".to_owned(),
            Value::String("2026-06-01T00:02:00Z".to_owned()),
        );
        fs::write(&state_file, state.to_string()).unwrap();
        enqueue_queue_completion(&queue, "queue-completion-pending-handoff");

        store
            .drain_runtime_pending_message_targets_by_category("queue-completion")
            .unwrap();

        assert_eq!(
            queue
                .pending_messages_for_target_by_category("queue-target", "queue-completion", 10)
                .unwrap()
                .len(),
            1
        );
        assert!(pane.input_lines().is_empty());
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn direct_delivery_allows_retryable_unreserved_claude_handoff() {
        let pane = queue_completion_test_pane(true);
        let (_store, _queue, state_file, queue_db) = queue_completion_test_store(&pane, "idle");
        let mut state: Value =
            serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        let session = state["sessions"][0].as_object_mut().unwrap();
        session.insert(
            "pending_handoff_path".to_owned(),
            Value::String("/tmp/retryable-handoff.md".to_owned()),
        );
        session.insert(
            "pending_handoff_recorded_at".to_owned(),
            Value::String("2026-06-01T00:02:00Z".to_owned()),
        );

        let (status, delivered) = deliver_runtime_text_to_session_raw(
            &mut state,
            "queue-target",
            "[sm queue] handoff retry drain",
            &pane.runtime,
        )
        .unwrap();

        assert_eq!(status, "idle");
        assert!(delivered);
        assert_eq!(pane.input_lines(), vec!["[sm queue] handoff retry drain"]);
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn completion_reconciler_rechecks_after_urgent_predecessor_before_completion() {
        let pane = queue_completion_test_pane(true);
        let (store, queue, state_file, queue_db) = queue_completion_test_store(&pane, "running");
        queue
            .enqueue_message_once_with_metadata(
                "queue-completion-urgent-predecessor",
                "queue-target",
                "[sm queue] urgent predecessor",
                "urgent",
                QueueMessageMetadata::default(),
            )
            .unwrap();
        enqueue_queue_completion(&queue, "queue-completion-after-urgent-predecessor");

        store
            .drain_runtime_pending_message_targets_by_category("queue-completion")
            .unwrap();

        let inputs = pane.input_lines();
        assert_eq!(inputs.len(), 1);
        assert!(inputs[0].ends_with("[sm queue] urgent predecessor"));
        assert_eq!(
            queue
                .pending_messages_for_target_by_category("queue-target", "queue-completion", 10)
                .unwrap()
                .len(),
            1
        );

        store
            .drain_runtime_pending_message_targets_by_category("queue-completion")
            .unwrap();

        assert!(queue
            .pending_messages_for_target("queue-target", 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            pane.input_lines(),
            vec![
                inputs[0].clone(),
                "[sm queue] test job completed".to_owned()
            ]
        );
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn background_delivery_rechecks_provider_ready_state_under_input_lock() {
        let pane = queue_completion_test_pane(false);
        let (_store, _queue, state_file, queue_db) = queue_completion_test_store(&pane, "running");
        let mut state: Value =
            serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();

        let (status, delivered) = deliver_runtime_background_text_to_session_raw(
            &mut state,
            "queue-target",
            "[sm queue] stale readiness proof must not inject",
            &pane.runtime,
        )
        .unwrap();

        assert_eq!(status, "running");
        assert!(!delivered);
        assert!(pane.input_lines().is_empty());
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn completion_reconciler_delivers_after_restart_from_the_durable_queue() {
        let pane = queue_completion_test_pane(true);
        let (store, queue, state_file, queue_db) = queue_completion_test_store(&pane, "running");
        enqueue_queue_completion(&queue, "queue-completion-after-restart");
        drop(store);
        let restarted = SessionStore::new_with_queue(state_file.clone(), queue_db.clone())
            .with_delivery_runtime(Some(pane.runtime.clone()));

        restarted
            .drain_runtime_pending_message_targets_by_category("queue-completion")
            .unwrap();

        assert!(queue
            .pending_messages_for_target_by_category("queue-target", "queue-completion", 10)
            .unwrap()
            .is_empty());
        assert_eq!(pane.input_lines(), vec!["[sm queue] test job completed"]);
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn concurrent_completion_reconcilers_deliver_one_durable_message_once() {
        let pane = queue_completion_test_pane(true);
        let (store, queue, state_file, queue_db) = queue_completion_test_store(&pane, "running");
        enqueue_queue_completion(&queue, "queue-completion-concurrent");
        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .drain_runtime_pending_message_targets_by_category("queue-completion")
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        assert!(queue
            .pending_messages_for_target_by_category("queue-target", "queue-completion", 10)
            .unwrap()
            .is_empty());
        assert_eq!(pane.input_lines(), vec!["[sm queue] test job completed"]);
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn default_state_loader_reads_legacy_fallback_when_primary_missing() {
        let state_file = unique_temp_path("primary");
        let legacy_state_file = unique_temp_path("legacy");
        fs::write(
            &legacy_state_file,
            json!({
                "sessions": [
                    {
                        "id": "legacy1",
                        "name": "claude-legacy1",
                        "working_dir": "/repo",
                        "tmux_session": "claude-legacy1",
                        "log_file": "/tmp/legacy1.log",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file, legacy_state_file);

        let sessions = store.list_sessions(false).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "legacy1");
    }

    #[test]
    fn default_state_loader_reads_legacy_fallback_when_primary_is_invalid() {
        let state_file = unique_temp_path("primary");
        let legacy_state_file = unique_temp_path("legacy");
        fs::write(&state_file, "{not json").unwrap();
        fs::write(
            &legacy_state_file,
            json!({
                "sessions": [
                    {
                        "id": "legacy2",
                        "name": "claude-legacy2",
                        "working_dir": "/repo",
                        "tmux_session": "claude-legacy2",
                        "log_file": "/tmp/legacy2.log",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file, legacy_state_file);

        let sessions = store.list_sessions(false).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "legacy2");
    }

    #[test]
    fn snapshot_skips_legacy_codex_app_records_before_deserializing_sessions() {
        let raw = RawStateSnapshot {
            sessions: vec![
                json!({
                    "id": "legacyapp",
                    "name": "legacy app",
                    "provider": "codex",
                    "codex_thread_id": "thread-1",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }),
                json!({
                    "id": "tmux1",
                    "name": "claude-tmux1",
                    "working_dir": "/repo",
                    "tmux_session": "claude-tmux1",
                    "log_file": "/tmp/tmux1.log",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }),
            ],
            ..RawStateSnapshot::default()
        };

        let snapshot = StateSnapshot::try_from(raw).unwrap();
        let sessions = snapshot.into_sessions();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "tmux1");
    }

    #[test]
    fn codex_fork_turn_errors_are_not_terminal() {
        let retry = json!({
            "event_type": "error",
            "payload": {
                "willRetry": true,
                "error": {
                    "message": "Reconnecting... 1/5"
                }
            }
        });
        let retry_event = retry.as_object().unwrap();
        assert_eq!(codex_fork_status_for_event(retry_event), Some("running"));

        let non_retry = json!({
            "event_type": "error",
            "payload": {
                "willRetry": false,
                "error": {
                    "message": "This content was flagged for possible cybersecurity risk.",
                    "codexErrorInfo": "cyberPolicy"
                }
            }
        });
        let non_retry_event = non_retry.as_object().unwrap();
        assert_eq!(codex_fork_status_for_event(non_retry_event), Some("idle"));

        let shutdown = json!({ "event_type": "shutdown", "payload": {} });
        assert_eq!(
            codex_fork_status_for_event(shutdown.as_object().unwrap()),
            Some("stopped")
        );
    }

    #[test]
    fn codex_fork_monitor_survives_non_retry_error_and_observes_later_activity() {
        let state_file = unique_temp_path("codex-error-lifecycle");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"error","payload":{"willRetry":false,"error":{"message":"This content was flagged for possible cybersecurity risk.","codexErrorInfo":"cyberPolicy"}}}"#,
            )
            .unwrap();
        assert_eq!(
            store.get_session("codex001").unwrap().unwrap().status,
            "idle"
        );
        assert!(store
            .codex_fork_monitor_should_continue("codex001")
            .unwrap());

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"thread/status/changed","payload":{"status":{"type":"active"}}}"#,
            )
            .unwrap();
        assert_eq!(
            store.get_session("codex001").unwrap().unwrap().status,
            "running"
        );
    }

    #[test]
    fn codex_fork_control_lifecycle_events_track_and_clear_degradation() {
        let state_file = unique_temp_path("codex-control-lifecycle");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file.clone());

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"control_socket_degraded","payload":{"reason":"socket path disappeared"}}"#,
            )
            .unwrap();
        let degraded = store.load_raw_json_value().unwrap();
        assert_eq!(
            raw_session_object(&degraded, "codex001")
                .and_then(|session| json_text(session.get("error_message")))
                .as_deref(),
            Some("codex_fork_control_degraded: socket path disappeared")
        );

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"control_socket_restarted","payload":{"generation":2}}"#,
            )
            .unwrap();
        let recovered = store.load_raw_json_value().unwrap();
        assert!(raw_session_object(&recovered, "codex001")
            .and_then(|session| json_text(session.get("error_message")))
            .is_none());
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn codex_fork_thread_identity_events_append_the_provider_session_chain() {
        let state_file = unique_temp_path("codex-session-chain");
        let usage_db_path = state_file.with_extension("usage.db");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"thread/started","payload":{"thread":{"id":"thread-before-handoff"}}}"#,
            )
            .unwrap();
        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"thread/started","payload":{"thread":{"id":"thread-after-handoff"}}}"#,
            )
            .unwrap();

        let connection = rusqlite::Connection::open(usage_db_path).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT provider, provider_session_id FROM seat_sessions WHERE seat_id = 'codex001' ORDER BY provider_session_id",
            )
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("codex-fork".to_owned(), "thread-after-handoff".to_owned()),
                ("codex-fork".to_owned(), "thread-before-handoff".to_owned()),
            ]
        );
        assert_eq!(
            store
                .get_session("codex001")
                .unwrap()
                .unwrap()
                .provider_resume_id
                .as_deref(),
            Some("thread-after-handoff")
        );
    }

    #[test]
    fn codex_fork_subagent_events_do_not_replace_the_resumable_root_thread() {
        let state_file = unique_temp_path("codex-subagent-root-binding");
        let usage_db_path = state_file.with_extension("usage.db");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "provider_resume_id": "root-thread",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"thread/started","session_id":"child-thread","payload":{"thread":{"id":"child-thread","parentThreadId":"root-thread","threadSource":"subagent"}}}"#,
            )
            .unwrap();
        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"turn_started","session_id":"child-thread","payload":{"turn_id":"child-turn"}}"#,
            )
            .unwrap();

        assert_eq!(
            store
                .get_session("codex001")
                .unwrap()
                .unwrap()
                .provider_resume_id
                .as_deref(),
            Some("root-thread")
        );
        let connection = rusqlite::Connection::open(usage_db_path).unwrap();
        let child_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM seat_sessions WHERE seat_id = 'codex001' AND provider_session_id = 'child-thread'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(child_count, 1);
    }

    #[test]
    fn repeated_codex_session_identity_does_not_rewrite_the_provider_chain() {
        let state_file = unique_temp_path("codex-session-chain-repeat");
        let usage_db_path = state_file.with_extension("usage.db");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "provider_resume_id": "existing-thread",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);
        store
            .seat_session_store
            .append("codex001", "codex-fork", "existing-thread", None)
            .unwrap();
        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"account/rateLimits/updated","session_id":"existing-thread","payload":{"rateLimits":{}}}"#,
            )
            .unwrap();
        let connection = rusqlite::Connection::open(usage_db_path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = Instant::now();
        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"account/rateLimits/updated","session_id":"existing-thread","payload":{"rateLimits":{}}}"#,
            )
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "an unchanged provider identity attempted another usage DB write"
        );
        connection.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn transient_seat_session_append_is_retried_after_the_writer_unlocks() {
        let state_file = unique_temp_path("seat-session-retry");
        let usage_db_path = state_file.with_extension("usage.db");
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);
        store
            .seat_session_store
            .append("seed", "codex", "seed-thread", None)
            .unwrap();
        let connection = rusqlite::Connection::open(&usage_db_path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();

        store.append_seat_session("codex001", "codex", "retry-thread", None);
        connection.execute_batch("ROLLBACK").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let count: i64 = rusqlite::Connection::open(&usage_db_path)
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM seat_sessions WHERE seat_id = 'codex001' AND provider_session_id = 'retry-thread'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            if count == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "transient usage ledger append was not retried"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn startup_reconciliation_restores_a_missing_current_codex_identity() {
        let state_file = unique_temp_path("seat-session-reconcile");
        let usage_db_path = state_file.with_extension("usage.db");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex",
                    "provider_resume_id": "persisted-thread",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);

        store.reconcile_current_seat_sessions().unwrap();

        let count: i64 = rusqlite::Connection::open(usage_db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM seat_sessions WHERE seat_id = 'codex001' AND provider_session_id = 'persisted-thread'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn startup_reconciliation_marks_duplicate_current_identity_unassigned() {
        let state_file = unique_temp_path("seat-session-duplicate-current");
        let usage_db_path = state_file.with_extension("usage.db");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    {
                        "id": "claude001",
                        "name": "claude-claude001",
                        "provider": "claude",
                        "provider_resume_id": "shared-thread",
                        "transcript_path": "/repo/one/shared-thread.jsonl",
                        "working_dir": "/repo",
                        "tmux_session": "claude-claude001",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00Z",
                        "last_activity": "2026-06-01T00:01:00Z"
                    },
                    {
                        "id": "claude002",
                        "name": "claude-claude002",
                        "provider": "claude",
                        "provider_resume_id": "shared-thread",
                        "transcript_path": "/repo/two/shared-thread.jsonl",
                        "working_dir": "/repo",
                        "tmux_session": "claude-claude002",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00Z",
                        "last_activity": "2026-06-01T00:01:00Z"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);

        store.reconcile_current_seat_sessions().unwrap();

        let connection = rusqlite::Connection::open(usage_db_path).unwrap();
        let rows = connection
            .prepare(
                "SELECT seat_id, provider, provider_session_id, artifact_path FROM seat_sessions",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(
                "unassigned".to_owned(),
                "claude".to_owned(),
                "shared-thread".to_owned(),
                None,
            )]
        );
    }

    #[test]
    fn startup_reconciliation_backfills_historical_claude_identities_conservatively() {
        let state_file = unique_temp_path("seat-session-claude-backfill");
        let usage_db_path = state_file.with_extension("usage.db");
        let projects_root = state_file.with_extension("claude-projects");
        let working_dir = state_file.with_extension("repo");
        fs::create_dir_all(&working_dir).unwrap();
        let project_dir =
            projects_root.join(claude_project_dir_name(working_dir.to_str().unwrap()));
        fs::create_dir_all(&project_dir).unwrap();
        let current_path = project_dir.join("current-thread").join("chat.jsonl");
        let historical_path = project_dir.join("historical-thread").join("chat.jsonl");
        let historical_sidechain_path = project_dir
            .join("historical-thread")
            .join("subagents")
            .join("agent-child.jsonl");
        let ambiguous_path = project_dir.join("ambiguous.jsonl");
        fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        fs::create_dir_all(historical_sidechain_path.parent().unwrap()).unwrap();
        let transcript = |session_id: &str, timestamp: &str| {
            json!({
                "type": "user",
                "sessionId": session_id,
                "cwd": working_dir.display().to_string(),
                "timestamp": timestamp
            })
            .to_string()
        };
        fs::write(
            &current_path,
            transcript("current-thread", "2026-01-06T00:00:00Z"),
        )
        .unwrap();
        fs::write(
            &historical_path,
            transcript("historical-thread", "2026-01-02T00:00:00Z"),
        )
        .unwrap();
        fs::write(
            &historical_sidechain_path,
            transcript("historical-thread", "2026-01-02T00:01:00Z"),
        )
        .unwrap();
        fs::write(
            &ambiguous_path,
            transcript("ambiguous-thread", "2026-01-04T12:00:00Z"),
        )
        .unwrap();
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    {
                        "id": "seat-old",
                        "name": "claude-seat-old",
                        "provider": "claude",
                        "working_dir": working_dir.display().to_string(),
                        "tmux_session": "claude-seat-old",
                        "status": "stopped",
                        "created_at": "2026-01-01T00:00:00Z",
                        "last_activity": "2026-01-05T00:00:00Z",
                        "stopped_at": "2026-01-05T00:00:00Z"
                    },
                    {
                        "id": "seat-current",
                        "name": "claude-seat-current",
                        "provider": "claude",
                        "working_dir": working_dir.display().to_string(),
                        "tmux_session": "claude-seat-current",
                        "status": "running",
                        "transcript_path": current_path.display().to_string(),
                        "created_at": "2026-01-04T00:00:00Z",
                        "last_activity": "2026-01-06T00:00:00Z"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file)
            .with_claude_projects_root(projects_root);

        store.reconcile_current_seat_sessions().unwrap();

        let connection = rusqlite::Connection::open(usage_db_path).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT seat_id, provider_session_id FROM seat_sessions ORDER BY provider_session_id",
            )
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("unassigned".to_owned(), "ambiguous-thread".to_owned()),
                ("seat-current".to_owned(), "current-thread".to_owned()),
                ("seat-old".to_owned(), "historical-thread".to_owned()),
            ]
        );
    }

    #[test]
    fn startup_reconciliation_backfills_codex_fork_thread_history() {
        let state_file = unique_temp_path("seat-session-fork-backfill");
        let usage_db_path = state_file.with_extension("usage.db");
        let log_file = state_file.with_extension("log");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "provider_resume_id": "current-thread",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "log_file": log_file.display().to_string(),
                    "status": "running",
                    "created_at": "2026-01-01T00:00:00Z",
                    "last_activity": "2026-01-02T00:00:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default());
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file)
            .with_delivery_runtime(Some(runtime.clone()));
        let snapshot = store.load_snapshot().unwrap();
        let record = snapshot.sessions.first().unwrap();
        let spec = TmuxSessionSpec {
            session_id: record.id.clone(),
            session_credential: None,
            tmux_session: record.tmux_session.clone(),
            working_dir: record.working_dir.clone(),
            log_file,
            provider: record.provider.clone(),
            initial_message: None,
            force_initial_prompt_stdin: false,
            model: None,
            reasoning_effort: None,
        };
        let artifacts = runtime
            .codex_fork_runtime_artifacts(&spec)
            .unwrap()
            .unwrap();
        fs::write(
            &artifacts.event_stream_path,
            concat!(
                "{\"event_type\":\"thread/started\",\"payload\":{\"thread\":{\"id\":\"historical-thread\"}}}\n",
                "{\"event_type\":\"thread/started\",\"payload\":{\"thread\":{\"id\":\"historical-child\",\"parentThreadId\":\"historical-thread\",\"threadSource\":\"subagent\"}}}\n",
                "{\"event_type\":\"thread/started\",\"payload\":{\"thread\":{\"id\":\"current-thread\"}}}\n"
            ),
        )
        .unwrap();
        store.append_seat_session("codex001", "codex-fork", "current-thread", None);

        store.reconcile_current_seat_sessions().unwrap();

        let connection = rusqlite::Connection::open(usage_db_path).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM seat_sessions WHERE seat_id = 'codex001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let paths: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM seat_sessions WHERE seat_id = 'codex001' AND artifact_path IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(paths, 3);
    }

    #[test]
    fn startup_reconciliation_backfills_historical_codex_cli_rollouts() {
        let temp_dir = unique_temp_path("seat-session-codex-backfill");
        let state_file = temp_dir.join("sessions.json");
        let working_dir = temp_dir.join("repo");
        let sessions_root = temp_dir.join("codex-home").join("sessions");
        let day_dir = sessions_root.join("2026").join("01").join("02");
        fs::create_dir_all(&working_dir).unwrap();
        fs::create_dir_all(&day_dir).unwrap();
        let rollout = |id: &str, timestamp: &str| {
            fs::write(
                day_dir.join(format!("rollout-{id}.jsonl")),
                format!(
                    "{}\n",
                    json!({
                        "type": "session_meta",
                        "timestamp": timestamp,
                        "payload": {
                            "id": id,
                            "cwd": working_dir.display().to_string(),
                            "timestamp": timestamp
                        }
                    })
                ),
            )
            .unwrap();
        };
        rollout("historical-thread", "2026-01-02T00:00:00Z");
        rollout("current-thread", "2026-01-03T00:00:00Z");
        rollout("post-snapshot-thread", "2026-01-04T00:00:00Z");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex",
                    "provider_resume_id": "current-thread",
                    "working_dir": working_dir.display().to_string(),
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-01-01T00:00:00Z",
                    "last_activity": "2026-01-03T00:00:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let mut store =
            SessionStore::new_with_legacy_fallback(state_file.clone(), state_file.clone());
        store.codex_sessions_root = sessions_root;

        store
            .reconcile_current_seat_sessions_through(
                parse_timestamp_ns("2026-01-03T12:00:00Z").unwrap(),
            )
            .unwrap();

        let connection = rusqlite::Connection::open(state_file.with_extension("usage.db")).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT provider_session_id FROM seat_sessions WHERE seat_id = 'codex001' ORDER BY provider_session_id",
            )
            .unwrap();
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            ids,
            vec!["current-thread".to_owned(), "historical-thread".to_owned()]
        );
        let paths: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM seat_sessions WHERE seat_id = 'codex001' AND artifact_path IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(paths, 2);
    }

    #[test]
    fn usage_scan_repairs_a_late_classic_codex_artifact_without_restart() {
        let temp_dir = unique_temp_path("usage-late-codex-artifact");
        let state_file = temp_dir.join("sessions.json");
        let usage_db_path = state_file.with_extension("usage.db");
        let working_dir = temp_dir.join("repo");
        let sessions_root = temp_dir.join("codex-home").join("sessions");
        let day_dir = sessions_root.join("2026").join("08").join("10");
        fs::create_dir_all(&working_dir).unwrap();
        fs::create_dir_all(&day_dir).unwrap();
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex",
                    "provider_resume_id": "late-thread",
                    "working_dir": working_dir.display().to_string(),
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-08-10T16:00:00Z",
                    "last_activity": "2026-08-10T16:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let mut store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);
        store.codex_sessions_root = sessions_root;
        store.append_seat_session("codex001", "codex", "late-thread", None);
        assert_eq!(
            store
                .seat_session_store
                .provider_sessions_missing_artifacts("codex")
                .unwrap(),
            BTreeSet::from(["late-thread".to_owned()])
        );

        let rollout_path = day_dir.join("rollout-late-thread.jsonl");
        fs::write(
            &rollout_path,
            format!(
                "{}\n",
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-08-10T16:00:00Z",
                    "payload": {
                        "id": "late-thread",
                        "cwd": working_dir.display().to_string(),
                        "timestamp": "2026-08-10T16:00:00Z"
                    }
                })
            ),
        )
        .unwrap();
        let records = store.load_snapshot().unwrap().into_sessions();
        store
            .repair_current_codex_usage_artifacts(&records)
            .unwrap();

        assert!(store
            .seat_session_store
            .provider_sessions_missing_artifacts("codex")
            .unwrap()
            .is_empty());
        let repaired: String = Connection::open(usage_db_path)
            .unwrap()
            .query_row(
                "SELECT artifact_path FROM seat_sessions WHERE provider_session_id = 'late-thread'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(repaired, resolve_path_lossy(rollout_path));
    }

    #[test]
    fn codex_fork_identity_event_releases_session_lock_before_ledger_write() {
        let state_file = unique_temp_path("codex-event-ledger-lock");
        let usage_db_path = state_file.with_extension("usage.db");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);
        store
            .seat_session_store
            .append("seed", "codex-fork", "seed-thread", None)
            .unwrap();
        let connection = rusqlite::Connection::open(usage_db_path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();

        let event_store = store.clone();
        let event = thread::spawn(move || {
            event_store.apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"thread/started","payload":{"thread":{"id":"new-thread"}}}"#,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if store
                .get_session("codex001")
                .unwrap()
                .and_then(|session| session.provider_resume_id)
                .as_deref()
                == Some("new-thread")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fork identity state remained behind the blocked usage ledger"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        assert!(store
            .apply_claude_pre_tool_use_hook("codex001", Some("Read"))
            .unwrap());
        let mutation_elapsed = started.elapsed();

        connection.execute_batch("ROLLBACK").unwrap();
        event.join().unwrap().unwrap();
        assert!(
            mutation_elapsed < Duration::from_secs(1),
            "the global session lock remained held during the fork ledger wait"
        );
    }

    #[test]
    fn codex_fork_clear_rebind_reads_only_events_after_the_clear_offset() {
        let event_stream_path = unique_temp_path("codex-clear-rebind-events");
        fs::write(
            &event_stream_path,
            r#"{"event_type":"thread/started","payload":{"thread":{"id":"old-thread"}}}
"#,
        )
        .unwrap();
        let clear_offset = fs::metadata(&event_stream_path).unwrap().len();
        fs::OpenOptions::new()
            .append(true)
            .open(&event_stream_path)
            .unwrap()
            .write_all(
                br#"{"event_type":"turn_aborted","session_id":"old-thread","payload":{"session_id":"old-thread"}}
{"event_type":"thread/started","payload":{"thread":{"id":"new-thread"}}}
"#,
            )
            .unwrap();

        assert_eq!(
            wait_for_codex_fork_provider_resume_id_after_offset(
                &event_stream_path,
                clear_offset,
                Duration::ZERO,
            )
            .unwrap(),
            "new-thread"
        );
        let _ = fs::remove_file(event_stream_path);
    }

    #[test]
    fn codex_fork_binding_wait_skips_subagent_thread_started_events() {
        let event_stream_path = unique_temp_path("codex-root-bind-events");
        fs::write(
            &event_stream_path,
            concat!(
                "{\"event_type\":\"thread/started\",\"session_id\":\"child-thread\",\"payload\":{\"thread\":{\"id\":\"child-thread\",\"parentThreadId\":\"root-thread\",\"threadSource\":\"subagent\"}}}\n",
                "{\"event_type\":\"thread/started\",\"session_id\":\"root-thread\",\"payload\":{\"thread\":{\"id\":\"root-thread\",\"parentThreadId\":null,\"threadSource\":\"user\"}}}\n"
            ),
        )
        .unwrap();

        assert_eq!(
            wait_for_codex_fork_provider_resume_id_after_offset(
                &event_stream_path,
                0,
                Duration::ZERO,
            )
            .unwrap(),
            "root-thread"
        );
        let _ = fs::remove_file(event_stream_path);
    }

    #[test]
    fn codex_fork_launch_binding_accepts_trust_before_root_thread_starts() {
        let event_stream_path = unique_temp_path("codex-trust-bind-events");
        fs::write(&event_stream_path, "").unwrap();
        let writer_path = event_stream_path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(125));
            fs::write(
                writer_path,
                "{\"event_type\":\"thread/started\",\"payload\":{\"thread\":{\"id\":\"trusted-root\",\"parentThreadId\":null,\"threadSource\":\"user\"}}}\n",
            )
            .unwrap();
        });
        let mut prompt_checks = 0;

        let provider_resume_id = wait_for_codex_fork_provider_resume_id_after_offset_with_startup(
            &event_stream_path,
            0,
            Duration::from_secs(1),
            || {
                prompt_checks += 1;
                Ok(true)
            },
        )
        .unwrap();

        assert_eq!(provider_resume_id, "trusted-root");
        assert_eq!(prompt_checks, 1);
        writer.join().unwrap();
        let _ = fs::remove_file(event_stream_path);
    }

    #[test]
    fn codex_fork_clear_rebind_buffers_partial_events_between_polls() {
        let event_stream_path = unique_temp_path("codex-clear-rebind-partial-events");
        fs::write(
            &event_stream_path,
            r#"{"event_type":"thread/started","payload":{"thread":{"#,
        )
        .unwrap();
        let writer_path = event_stream_path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            fs::OpenOptions::new()
                .append(true)
                .open(writer_path)
                .unwrap()
                .write_all(
                    br#""id":"new-thread"}}}
"#,
                )
                .unwrap();
        });

        assert_eq!(
            wait_for_codex_fork_provider_resume_id_after_offset(
                &event_stream_path,
                0,
                Duration::from_secs(1),
            )
            .unwrap(),
            "new-thread"
        );
        writer.join().unwrap();
        let _ = fs::remove_file(event_stream_path);
    }

    #[test]
    fn concurrent_clears_for_the_same_seat_are_serialized() {
        let state_file = unique_temp_path("clear-operation-lock");
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);
        let first_guard = store.lock_clear_operation("codex001").unwrap();
        let second_store = store.clone();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let second = thread::spawn(move || {
            let _guard = second_store.lock_clear_operation("codex001").unwrap();
            acquired_tx.send(()).unwrap();
        });

        assert!(acquired_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        drop(first_guard);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        second.join().unwrap();
    }

    #[test]
    fn concurrent_codex_cli_bindings_for_the_same_discovery_domain_are_serialized() {
        let temp_dir = unique_temp_path("codex-binding-operation-lock");
        let state_file = temp_dir.join("sessions.json");
        let sessions_root = temp_dir.join("codex-home").join("sessions");
        let working_dir = temp_dir.join("repo");
        fs::create_dir_all(&sessions_root).unwrap();
        fs::create_dir_all(&working_dir).unwrap();
        let mut store =
            SessionStore::new_with_legacy_fallback(state_file.clone(), state_file.clone());
        store.codex_sessions_root = sessions_root;
        let mut first_record = session_record("running");
        first_record.id = "codex001".to_owned();
        first_record.provider = "codex".to_owned();
        first_record.working_dir = working_dir.display().to_string();
        let mut second_record = first_record.clone();
        second_record.id = "codex002".to_owned();
        let mut unrelated_record = first_record.clone();
        unrelated_record.id = "codex003".to_owned();
        unrelated_record.working_dir = temp_dir.join("other-repo").display().to_string();

        let first_guard = store
            .lock_codex_cli_binding_operation(&first_record)
            .unwrap();
        let second_store = store.clone();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let second = thread::spawn(move || {
            let _guard = second_store
                .lock_codex_cli_binding_operation(&second_record)
                .unwrap();
            acquired_tx.send(()).unwrap();
        });

        assert!(acquired_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        let _unrelated_guard = store
            .lock_codex_cli_binding_operation(&unrelated_record)
            .unwrap();
        drop(first_guard);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        second.join().unwrap();
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn codex_fork_clear_wait_does_not_hold_the_session_mutex() {
        let Some(fixture) = CodexForkHandoffFixture::new("clear-unlocked") else {
            return;
        };
        fixture.kill_tmux();
        fixture.start_delayed_clear_tmux();
        let mut state: Value =
            serde_json::from_slice(&fs::read(&fixture.state_file).unwrap()).unwrap();
        session_object_mut(ensure_sessions_array_mut(&mut state).unwrap(), "codex001")
            .unwrap()
            .insert(
                "parent_session_id".to_owned(),
                Value::String("parent01".to_owned()),
            );
        fs::write(&fixture.state_file, serde_json::to_vec(&state).unwrap()).unwrap();
        let store = fixture.store();
        let clear_store = store.clone();
        let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default());
        let clear = thread::spawn(move || {
            clear_store.clear_core_session_with_runtime(
                "codex001",
                ClearSessionRequest {
                    prompt: None,
                    requester_session_id: Some("parent01".to_owned()),
                },
                &runtime,
            )
        });

        thread::sleep(Duration::from_millis(250));
        if clear.is_finished() {
            let pane = fixture.capture_pane();
            let result = clear.join().unwrap();
            panic!("clear completed before the delayed identity event: {result:?}; pane={pane:?}");
        }
        let started = Instant::now();
        assert!(store
            .apply_claude_pre_tool_use_hook("codex001", Some("Read"))
            .unwrap());
        let mutation_elapsed = started.elapsed();

        assert!(matches!(
            clear.join().unwrap().unwrap(),
            CoreClearOutcome::Cleared(_)
        ));
        assert!(
            mutation_elapsed < Duration::from_secs(1),
            "the global session lock remained held during fork identity discovery"
        );
        assert_eq!(
            store
                .get_session("codex001")
                .unwrap()
                .unwrap()
                .provider_resume_id
                .as_deref(),
            Some("new-thread")
        );
    }

    #[test]
    fn codex_fork_handoff_executes_after_turn_complete() {
        let Some(fixture) = CodexForkHandoffFixture::new("execute") else {
            return;
        };
        let store = fixture.store();

        assert!(matches!(
            store
                .schedule_handoff(
                    "codex001",
                    HandoffRequest {
                        requester_session_id: "codex001".to_owned(),
                        file_path: fixture.handoff_path.display().to_string(),
                    },
                )
                .unwrap(),
            HandoffOutcome::Recorded(_)
        ));
        let state = store.load_raw_json_value().unwrap();
        assert!(raw_session_object(&state, "codex001")
            .unwrap()
            .get("pending_handoff_event_offset")
            .and_then(Value::as_u64)
            .is_some());

        fixture.append_turn_complete();
        fixture.wait_for_handoff(&store);

        let session = store.get_session("codex001").unwrap().unwrap();
        assert_eq!(session.id, "codex001");
        assert_eq!(session.friendly_name.as_deref(), Some("stable-agent"));
        assert_eq!(
            session.last_handoff_path.as_deref(),
            Some(fixture.handoff_path.to_str().unwrap())
        );
        assert_eq!(session.provider_resume_id.as_deref(), Some("new-thread"));
        let connection =
            rusqlite::Connection::open(fixture.state_file.with_extension("usage.db")).unwrap();
        let provider_session_ids = connection
            .prepare(
                "SELECT provider_session_id FROM seat_sessions WHERE seat_id = 'codex001' ORDER BY provider_session_id",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(provider_session_ids, vec!["new-thread", "old-thread"]);
        let state = store.load_raw_json_value().unwrap();
        let session = raw_session_object(&state, "codex001").unwrap();
        assert!(json_text(session.get("pending_handoff_path")).is_none());
        assert!(json_text(session.get("pending_handoff_event_offset")).is_none());
        let output = fixture.capture_pane();
        let compact_output = output
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let expected = format!(
            "received:Read{}andcontinuefromwhereyouleftoff.",
            fixture.handoff_path.display()
        );
        assert!(compact_output.contains(&expected), "{output}");
    }

    #[test]
    fn codex_fork_handoff_recovery_replays_from_persisted_offset() {
        let Some(fixture) = CodexForkHandoffFixture::new("recovery") else {
            return;
        };
        let state = fixture.store().load_raw_json_value().unwrap();
        let mut state = state;
        let session =
            session_object_mut(ensure_sessions_array_mut(&mut state).unwrap(), "codex001").unwrap();
        session.insert(
            "pending_handoff_path".to_owned(),
            Value::String(fixture.handoff_path.display().to_string()),
        );
        session.insert("pending_handoff_event_offset".to_owned(), json!(0));
        fs::write(
            &fixture.state_file,
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();
        fixture.append_turn_complete();

        let restarted = fixture.store();
        assert_eq!(restarted.recover_pending_codex_fork_handoffs().unwrap(), 1);
        fixture.wait_for_handoff(&restarted);

        assert_eq!(
            restarted
                .get_session("codex001")
                .unwrap()
                .unwrap()
                .last_handoff_path
                .as_deref(),
            Some(fixture.handoff_path.to_str().unwrap())
        );
        assert_eq!(
            restarted
                .get_session("codex001")
                .unwrap()
                .unwrap()
                .provider_resume_id
                .as_deref(),
            Some("new-thread")
        );
    }

    #[test]
    fn codex_fork_handoff_failure_keeps_pending_path() {
        let Some(fixture) = CodexForkHandoffFixture::new("failure") else {
            return;
        };
        let store = fixture.store();
        store
            .schedule_handoff(
                "codex001",
                HandoffRequest {
                    requester_session_id: "codex001".to_owned(),
                    file_path: fixture.handoff_path.display().to_string(),
                },
            )
            .unwrap();
        fixture.kill_tmux();
        fixture.append_turn_complete();
        fixture.wait_for_handoff_error(&store);

        let state = store.load_raw_json_value().unwrap();
        let session = raw_session_object(&state, "codex001").unwrap();
        assert_eq!(
            json_text(session.get("pending_handoff_path")).as_deref(),
            Some(fixture.handoff_path.to_str().unwrap())
        );
        assert!(json_text(session.get("last_handoff_path")).is_none());
        assert!(json_text(session.get("error_message"))
            .unwrap()
            .contains("codex_fork_handoff_failed: tmux session is not running"));

        fixture.start_tmux();
        fixture.append_turn_complete();
        fixture.wait_for_handoff(&store);
        let state = store.load_raw_json_value().unwrap();
        let session = raw_session_object(&state, "codex001").unwrap();
        assert!(json_text(session.get("pending_handoff_path")).is_none());
        assert!(json_text(session.get("error_message")).is_none());
    }

    #[test]
    fn codex_fork_handoff_recovery_initializes_legacy_offset_at_stream_end() {
        let Some(fixture) = CodexForkHandoffFixture::new("legacy-offset") else {
            return;
        };
        fixture.append_turn_complete();
        let historical_stream_len = fs::metadata(&fixture.event_stream_path).unwrap().len();
        let mut state = fixture.store().load_raw_json_value().unwrap();
        let session =
            session_object_mut(ensure_sessions_array_mut(&mut state).unwrap(), "codex001").unwrap();
        session.insert(
            "pending_handoff_path".to_owned(),
            Value::String(fixture.handoff_path.display().to_string()),
        );
        fs::write(
            &fixture.state_file,
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let restarted = fixture.store();
        assert_eq!(restarted.recover_pending_codex_fork_handoffs().unwrap(), 1);
        let state = restarted.load_raw_json_value().unwrap();
        let session = raw_session_object(&state, "codex001").unwrap();
        assert_eq!(
            session
                .get("pending_handoff_event_offset")
                .and_then(Value::as_u64),
            Some(historical_stream_len)
        );
        assert!(json_text(session.get("last_handoff_path")).is_none());

        fixture.append_turn_complete();
        fixture.wait_for_handoff(&restarted);
    }

    #[test]
    fn codex_fork_event_monitor_recovery_records_only_new_rate_limits() {
        let Some(fixture) = CodexForkHandoffFixture::new("event-monitor-recovery") else {
            return;
        };
        let usage_db_path = fixture.state_file.with_extension("usage.db");
        let observed_at = OffsetDateTime::now_utc();
        UsageIdentityStore::new(&usage_db_path)
            .unwrap()
            .record_observation(
                Provider::Codex,
                Some(&AccountIdentity {
                    provider: Provider::Codex,
                    external_id: "codex-restart-account".to_owned(),
                    label: None,
                    plan_tier: Some("pro".to_owned()),
                    extra_usage_enabled: None,
                }),
                observed_at - TimeDuration::minutes(1),
                None,
                None,
            )
            .unwrap();
        let legacy_event_stream =
            codex_fork_legacy_event_stream_path_from_log_file(&fixture.log_file).unwrap();
        fs::write(&legacy_event_stream, "").unwrap();
        fixture.append_rate_limit_to(&legacy_event_stream, 12.0);
        assert_eq!(
            codex_fork_newest_event_stream_path(
                "codex001",
                &fixture.log_file,
                &fixture.event_stream_path,
            ),
            legacy_event_stream
        );

        let restarted = fixture
            .store()
            .with_usage_burn_store(UsageBurnStore::new(&usage_db_path).unwrap());
        assert_eq!(restarted.recover_codex_fork_event_monitors().unwrap(), 1);
        fixture.append_rate_limit_to(&legacy_event_stream, 27.0);

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let samples = Connection::open(&usage_db_path)
                .unwrap()
                .prepare("SELECT percent FROM burn_samples ORDER BY id")
                .unwrap()
                .query_map([], |row| row.get::<_, f64>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            if samples == vec![27.0] {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "recovered event monitor did not ingest the new sample: {samples:?}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    struct CodexForkHandoffFixture {
        state_file: PathBuf,
        event_stream_path: PathBuf,
        handoff_path: PathBuf,
        log_file: PathBuf,
        tmux_socket: String,
        tmux_session: String,
    }

    impl CodexForkHandoffFixture {
        fn new(label: &str) -> Option<Self> {
            if !Command::new("tmux")
                .arg("-V")
                .output()
                .ok()?
                .status
                .success()
            {
                return None;
            }
            let state_file = unique_temp_path(&format!("handoff-{label}"));
            let fixture_dir = state_file.with_extension("dir");
            fs::create_dir_all(&fixture_dir).unwrap();
            let log_file = fixture_dir.join("codex001.log");
            let handoff_path = fixture_dir.join("handoff.md");
            fs::write(&log_file, "").unwrap();
            fs::write(&handoff_path, "durable handoff body").unwrap();
            let tmux_socket = format!("sm-handoff-{}-{}", std::process::id(), label);
            let tmux_session = "codex-fork-handoff".to_owned();

            fs::write(
                &state_file,
                json!({
                    "sessions": [{
                        "id": "codex001",
                        "name": "codex-codex001",
                        "provider": "codex-fork",
                        "working_dir": fixture_dir.display().to_string(),
                        "tmux_session": tmux_session,
                        "tmux_socket_name": tmux_socket,
                        "log_file": log_file.display().to_string(),
                        "status": "running",
                        "friendly_name": "stable-agent",
                        "provider_resume_id": "old-thread",
                        "created_at": "2026-06-01T00:00:00Z",
                        "last_activity": "2026-06-01T00:01:00Z"
                    }]
                })
                .to_string(),
            )
            .unwrap();
            let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default());
            let store =
                SessionStore::new_with_legacy_fallback(state_file.clone(), state_file.clone())
                    .with_delivery_runtime(Some(runtime.clone()));
            let state = store.load_raw_json_value().unwrap();
            let session = raw_session_object(&state, "codex001").unwrap();
            let spec = codex_fork_spec_for_session_raw("codex001", session).unwrap();
            let event_stream_path = runtime
                .codex_fork_runtime_artifacts(&spec)
                .unwrap()
                .unwrap()
                .event_stream_path;
            fs::write(&event_stream_path, "").unwrap();
            start_codex_fork_handoff_tmux(&tmux_socket, &tmux_session, &event_stream_path);
            Some(Self {
                state_file,
                event_stream_path,
                handoff_path,
                log_file,
                tmux_socket,
                tmux_session,
            })
        }

        fn store(&self) -> SessionStore {
            SessionStore::new_with_legacy_fallback(self.state_file.clone(), self.state_file.clone())
                .with_delivery_runtime(Some(TmuxRuntime::from_config(
                    &crate::config::RustCoreConfig::default(),
                )))
        }

        fn append_turn_complete(&self) {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&self.event_stream_path)
                .unwrap();
            writeln!(
                file,
                r#"{{"event_type":"turn_complete","payload":{{"turn_id":"turn-1"}}}}"#
            )
            .unwrap();
        }

        fn append_rate_limit_to(&self, event_stream_path: &Path, percent: f64) {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(event_stream_path)
                .unwrap();
            writeln!(
                file,
                "{}",
                json!({
                    "event_type": "account/rateLimits/updated",
                    "payload": {"rateLimits": {
                        "limitId": "codex",
                        "primary": {
                            "usedPercent": percent,
                            "windowDurationMins": 300,
                            "resetsAt": (OffsetDateTime::now_utc()
                                + TimeDuration::minutes(300))
                                .unix_timestamp()
                        },
                        "secondary": null
                    }}
                })
            )
            .unwrap();
        }

        fn wait_for_handoff(&self, store: &SessionStore) {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if store
                    .get_session("codex001")
                    .unwrap()
                    .and_then(|session| session.last_handoff_path)
                    .is_some()
                {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "handoff did not complete; state={} pane={:?}",
                    fs::read_to_string(&self.state_file).unwrap_or_default(),
                    self.capture_pane()
                );
                thread::sleep(Duration::from_millis(50));
            }
        }

        fn capture_pane(&self) -> String {
            let output = Command::new("tmux")
                .args([
                    "-L",
                    &self.tmux_socket,
                    "capture-pane",
                    "-p",
                    "-t",
                    &self.tmux_session,
                ])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).to_string()
        }

        fn kill_tmux(&self) {
            let status = Command::new("tmux")
                .args(["-L", &self.tmux_socket, "kill-server"])
                .status()
                .unwrap();
            assert!(status.success());
        }

        fn start_tmux(&self) {
            start_codex_fork_handoff_tmux(
                &self.tmux_socket,
                &self.tmux_session,
                &self.event_stream_path,
            );
        }

        fn start_delayed_clear_tmux(&self) {
            start_codex_fork_delayed_clear_tmux(
                &self.tmux_socket,
                &self.tmux_session,
                &self.event_stream_path,
            );
        }

        fn wait_for_handoff_error(&self, store: &SessionStore) {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let state = store.load_raw_json_value().unwrap();
                let session = raw_session_object(&state, "codex001").unwrap();
                if json_text(session.get("error_message"))
                    .is_some_and(|message| message.starts_with("codex_fork_handoff_failed:"))
                {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "handoff failure was not recorded; state={}",
                    fs::read_to_string(&self.state_file).unwrap_or_default()
                );
                thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Drop for CodexForkHandoffFixture {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["-L", &self.tmux_socket, "kill-server"])
                .status();
            let _ = fs::remove_file(&self.state_file);
            let _ = fs::remove_dir_all(self.state_file.with_extension("dir"));
        }
    }

    fn start_codex_fork_handoff_tmux(socket: &str, session: &str, event_stream_path: &Path) {
        let script = r#"event_file=$1; pending=; printf '› '; while IFS= read -r line; do if [ "${line%/new}" != "$line" ]; then printf '{"event_type":"thread/started","payload":{"thread":{"id":"new-thread"}}}\n' >> "$event_file"; printf 'received:%s\n› ' "$line"; elif [ -n "$pending" ]; then printf '{"event_type":"op_submitted","payload":{"UserTurn":{"items":[{"type":"text","text":"%s"}]}}}\n' "$pending" >> "$event_file"; pending=; printf '\n› '; elif [ "${line#Read }" != "$line" ]; then pending=$line; printf 'received:%s\n› %s' "$line" "$line"; else printf 'received:%s\n› ' "$line"; fi; done"#;
        let command = format!(
            "/bin/sh -lc {} handoff-shell {}",
            shell_quote_handoff_fixture(script),
            shell_quote_handoff_fixture(&event_stream_path.display().to_string())
        );
        let status = Command::new("tmux")
            .args(["-L", socket, "new-session", "-d", "-s", session, &command])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn start_codex_fork_delayed_clear_tmux(socket: &str, session: &str, event_stream_path: &Path) {
        let script = r#"event_file=$1; printf '› '; while IFS= read -r line; do if [ "${line%/new}" != "$line" ]; then sleep 1; printf '{"event_type":"thread/started","payload":{"thread":{"id":"new-thread"}}}\n' >> "$event_file"; printf 'received:%s\n› ' "$line"; else printf 'received:%s\n› ' "$line"; fi; done"#;
        let command = format!(
            "/bin/sh -lc {} clear-shell {}",
            shell_quote_handoff_fixture(script),
            shell_quote_handoff_fixture(&event_stream_path.display().to_string())
        );
        let status = Command::new("tmux")
            .args(["-L", socket, "new-session", "-d", "-s", session, &command])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn shell_quote_handoff_fixture(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[test]
    fn codex_fork_thread_status_events_drive_active_idle_status() {
        let active = r#"{"type":"thread/status/changed","payload":{"status":{"type":"active"}}}"#;
        let idle = r#"{"type":"thread/status/changed","payload":{"status":{"type":"idle"}}}"#;
        let unknown = r#"{"type":"thread/status/changed","payload":{"status":{"type":"mystery"}}}"#;

        assert_eq!(codex_fork_status_for_event_line(active), Some("running"));
        assert_eq!(codex_fork_status_for_event_line(idle), Some("idle"));
        assert_eq!(codex_fork_status_for_event_line(unknown), None);
        assert!(codex_fork_event_line_starts_turn(active));
        assert!(!codex_fork_event_line_starts_turn(idle));
    }

    #[test]
    fn codex_fork_late_command_completion_does_not_reopen_idle_session() {
        let state_file = unique_temp_path("codex-late-command-idle");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "idle",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"item/completed","payload":{"item":{"type":"commandExecution","status":"completed"}}}"#,
            )
            .unwrap();
        assert_eq!(
            store.get_session("codex001").unwrap().unwrap().status,
            "idle"
        );

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"turn_started","payload":{}}"#,
            )
            .unwrap();
        assert_eq!(
            store.get_session("codex001").unwrap().unwrap().status,
            "running"
        );
    }

    #[test]
    fn codex_fork_descendant_idle_does_not_stop_active_root_turn() {
        let state_file = unique_temp_path("codex-descendant-idle");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "provider_resume_id": "root-thread",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00Z",
                    "last_activity": "2026-06-01T00:01:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"thread/status/changed","session_id":"child-thread","payload":{"threadId":"child-thread","status":{"type":"idle"}}}"#,
            )
            .unwrap();
        assert_eq!(
            store.get_session("codex001").unwrap().unwrap().status,
            "running"
        );

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"thread/status/changed","session_id":"root-thread","payload":{"threadId":"root-thread","status":{"type":"idle"}}}"#,
            )
            .unwrap();
        assert_eq!(
            store.get_session("codex001").unwrap().unwrap().status,
            "idle"
        );
    }

    #[test]
    fn codex_fork_completed_agent_message_is_idle() {
        let completed_agent_message =
            r#"{"event_type":"item_completed","payload":{"item":{"type":"agentMessage"}}}"#;
        let completed_command = r#"{"event_type":"item_completed","payload":{"item":{"type":"commandExecution","status":"completed"}}}"#;
        let late_command_output_delta = r#"{"event_type":"item/commandExecution/outputDelta","payload":{"delta":"hmr update\n"}}"#;

        assert_eq!(
            codex_fork_status_for_event_line(completed_agent_message),
            Some("idle")
        );
        assert_eq!(
            codex_fork_status_for_event_line(completed_command),
            Some("running")
        );
        assert_eq!(
            codex_fork_status_for_event_line(late_command_output_delta),
            None
        );
    }

    #[test]
    fn session_projection_uses_legacy_telegram_thread_fields() {
        let mut session = session_record("running");
        session.telegram_topic_id = Some(123);
        let response = SessionResponse::from(session);

        assert_eq!(response.telegram_thread_id, Some(123));

        let mut session = session_record("running");
        session.telegram_root_msg_id = Some(456);
        let response = SessionResponse::from(session);

        assert_eq!(response.telegram_thread_id, Some(456));
    }

    #[test]
    fn snapshot_projects_aliases_and_pending_adoption_proposals() {
        let snapshot = StateSnapshot {
            sessions: vec![
                SessionRecord {
                    id: "em123456".to_owned(),
                    friendly_name: Some("em-ops".to_owned()),
                    is_em: true,
                    ..session_record("running")
                },
                SessionRecord {
                    id: "child001".to_owned(),
                    friendly_name: None,
                    ..session_record("running")
                },
            ],
            maintainer_session_id: Some("em123456".to_owned()),
            agent_registrations: vec![AgentRegistrationRecord {
                role: "Reviewer".to_owned(),
                session_id: "child001".to_owned(),
                created_at: None,
            }],
            adoption_proposals: vec![AdoptionProposalRecord {
                id: "proposal1".to_owned(),
                proposer_session_id: "em123456".to_owned(),
                target_session_id: "child001".to_owned(),
                created_at: "2026-06-01T00:03:00".to_owned(),
                status: "pending".to_owned(),
                decided_at: None,
            }],
        };

        let sessions = snapshot.into_sessions();
        let maintainer = sessions
            .iter()
            .find(|session| session.id == "em123456")
            .unwrap();
        let child = sessions
            .iter()
            .find(|session| session.id == "child001")
            .unwrap();

        assert_eq!(maintainer.aliases, vec!["maintainer"]);
        assert_eq!(
            maintainer.cached_display_name().as_deref(),
            Some("maintainer")
        );
        assert_eq!(child.aliases, vec!["reviewer"]);
        assert_eq!(child.pending_adoption_proposals.len(), 1);
        assert_eq!(
            child.pending_adoption_proposals[0].proposer_name.as_deref(),
            Some("maintainer")
        );
        assert_eq!(child.pending_adoption_proposals[0].status, "stale");
        assert!(!child.pending_adoption_proposals[0].actionable);
        assert_eq!(
            child.pending_adoption_proposals[0]
                .failure_reason
                .as_deref(),
            Some("legacy adoption proposal requires a new consent request")
        );
    }

    #[test]
    fn snapshot_prunes_stopped_aliases_even_when_restorable() {
        let snapshot = StateSnapshot {
            sessions: vec![
                SessionRecord {
                    id: "dead001".to_owned(),
                    provider_resume_id: None,
                    transcript_path: None,
                    ..session_record("stopped")
                },
                SessionRecord {
                    id: "restore1".to_owned(),
                    provider_resume_id: Some("resume-id".to_owned()),
                    ..session_record("stopped")
                },
            ],
            maintainer_session_id: Some("dead001".to_owned()),
            agent_registrations: vec![
                AgentRegistrationRecord {
                    role: "Stale Role".to_owned(),
                    session_id: "dead001".to_owned(),
                    created_at: None,
                },
                AgentRegistrationRecord {
                    role: "Restorable Role".to_owned(),
                    session_id: "restore1".to_owned(),
                    created_at: None,
                },
            ],
            adoption_proposals: Vec::new(),
        };

        let sessions = snapshot.into_sessions();
        let stale = sessions
            .iter()
            .find(|session| session.id == "dead001")
            .unwrap();
        let restorable = sessions
            .iter()
            .find(|session| session.id == "restore1")
            .unwrap();

        assert!(stale.aliases.is_empty());
        assert!(restorable.aliases.is_empty());
    }

    #[test]
    fn claude_restore_resume_id_falls_back_to_transcript_stem() {
        let mut session = session_record("stopped");
        session.provider = "claude".to_owned();
        session.provider_resume_id = None;
        session.transcript_path =
            Some("/Users/rajesh/.claude/projects/repo/resume-uuid.jsonl".to_owned());

        assert_eq!(
            provider_resume_id_for_restore(&session).as_deref(),
            Some("resume-uuid")
        );
    }

    #[test]
    fn claude_transcript_path_wins_over_stale_provider_resume_id() {
        let mut session = session_record("stopped");
        session.provider = "claude".to_owned();
        session.provider_resume_id = Some("stale-id".to_owned());
        session.transcript_path = Some("/tmp/transcript-id.jsonl".to_owned());

        assert_eq!(
            provider_resume_id_for_restore(&session).as_deref(),
            Some("transcript-id")
        );
    }

    #[test]
    fn claude_restore_resume_id_is_missing_without_resume_metadata() {
        let mut session = session_record("stopped");
        session.provider = "claude".to_owned();
        session.provider_resume_id = None;
        session.transcript_path = None;

        assert_eq!(provider_resume_id_for_restore(&session), None);
    }

    #[test]
    fn codex_resume_discovery_matches_cwd_time_and_unclaimed_id() {
        let temp_dir = unique_temp_path("codex-resume-discovery");
        let working_dir = temp_dir.join("repo");
        let other_working_dir = temp_dir.join("other-repo");
        let sessions_root = temp_dir.join("codex-home").join("sessions");
        let day_dir = sessions_root.join("2026").join("07").join("28");
        fs::create_dir_all(&working_dir).unwrap();
        fs::create_dir_all(&other_working_dir).unwrap();
        fs::create_dir_all(&day_dir).unwrap();

        for (id, cwd, timestamp) in [
            (
                "wrong-cwd",
                other_working_dir.as_path(),
                "2026-07-28T20:00:00Z",
            ),
            ("claimed-id", working_dir.as_path(), "2026-07-28T20:00:01Z"),
            ("closest-id", working_dir.as_path(), "2026-07-28T20:00:02Z"),
            ("farther-id", working_dir.as_path(), "2026-07-28T20:10:00Z"),
        ] {
            fs::write(
                day_dir.join(format!("rollout-{id}.jsonl")),
                format!(
                    "{}\n",
                    json!({
                        "type": "session_meta",
                        "payload": {
                            "id": id,
                            "cwd": cwd.display().to_string(),
                            "timestamp": timestamp
                        }
                    })
                ),
            )
            .unwrap();
        }

        let mut session = session_record("stopped");
        session.provider = "codex".to_owned();
        session.working_dir = working_dir.display().to_string();
        session.created_at = "2026-07-28T20:00:00Z".to_owned();
        session.last_activity = "2026-07-28T20:09:00Z".to_owned();
        let claimed_ids = BTreeSet::from(["claimed-id".to_owned()]);

        assert_eq!(
            discover_codex_cli_resume_id(
                &session,
                &sessions_root,
                &claimed_ids,
                CodexCliSessionDiscoveryMode::Restore,
            )
            .as_deref(),
            Some("closest-id")
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn codex_creation_binding_excludes_preexisting_rollouts() {
        let temp_dir = unique_temp_path("codex-creation-binding");
        let working_dir = temp_dir.join("repo");
        let sessions_root = temp_dir.join("codex-home").join("sessions");
        let day_dir = sessions_root.join("2026").join("07").join("28");
        fs::create_dir_all(&working_dir).unwrap();
        fs::create_dir_all(&day_dir).unwrap();

        let rollout = |id: &str, timestamp: &str| {
            fs::write(
                day_dir.join(format!("rollout-{id}.jsonl")),
                format!(
                    "{}\n",
                    json!({
                        "type": "session_meta",
                        "payload": {
                            "id": id,
                            "cwd": working_dir.display().to_string(),
                            "timestamp": timestamp
                        }
                    })
                ),
            )
            .unwrap();
        };
        rollout("stale-id", "2026-07-28T20:00:00Z");

        let mut session = session_record("stopped");
        session.provider = "codex".to_owned();
        session.working_dir = working_dir.display().to_string();
        session.created_at = "2026-07-28T20:00:00Z".to_owned();
        session.last_activity = "2026-07-28T20:00:00Z".to_owned();
        let excluded_ids = codex_cli_existing_session_ids(&session, &sessions_root);

        rollout("delayed-earlier-id", "2026-07-28T19:59:59Z");
        rollout("new-id", "2026-07-28T20:00:01Z");

        assert_eq!(
            discover_codex_cli_resume_id(
                &session,
                &sessions_root,
                &excluded_ids,
                CodexCliSessionDiscoveryMode::Creation {
                    launched_at_ns: parse_timestamp_ns("2026-07-28T20:00:00Z").unwrap(),
                },
            )
            .as_deref(),
            Some("new-id")
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn deferred_codex_clear_binding_captures_a_late_replacement_rollout() {
        let temp_dir = unique_temp_path("codex-deferred-clear-binding");
        let state_file = temp_dir.join("sessions.json");
        let working_dir = temp_dir.join("repo");
        let sessions_root = temp_dir.join("codex-home").join("sessions");
        let day_dir = sessions_root.join("2026").join("07").join("28");
        fs::create_dir_all(&working_dir).unwrap();
        fs::create_dir_all(&day_dir).unwrap();
        let mut record = session_record("running");
        record.id = "codex001".to_owned();
        record.provider = "codex".to_owned();
        record.working_dir = working_dir.display().to_string();
        record.provider_resume_id = Some("old-thread".to_owned());
        record.created_at = "2026-07-28T20:00:00Z".to_owned();
        record.last_activity = record.created_at.clone();
        fs::write(
            &state_file,
            json!({"sessions": [record.clone()]}).to_string(),
        )
        .unwrap();
        let mut store =
            SessionStore::new_with_legacy_fallback(state_file.clone(), state_file.clone());
        store.codex_sessions_root = sessions_root;
        store.append_seat_session("codex001", "codex", "old-thread", None);
        let rollout_path = day_dir.join("rollout-new-thread.jsonl");
        let rollout_working_dir = working_dir.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(1_200));
            fs::write(
                rollout_path,
                format!(
                    "{}\n",
                    json!({
                        "type": "session_meta",
                        "payload": {
                            "id": "new-thread",
                            "cwd": rollout_working_dir.display().to_string(),
                            "timestamp": "2026-07-28T20:00:01Z"
                        }
                    })
                ),
            )
            .unwrap();
        });

        assert!(store
            .complete_deferred_codex_cli_rebind(
                "codex001",
                &record,
                &BTreeSet::from(["old-thread".to_owned()]),
                parse_timestamp_ns("2026-07-28T20:00:00Z").unwrap(),
                Some("old-thread"),
                Duration::from_secs(2),
            )
            .unwrap());
        writer.join().unwrap();
        assert_eq!(
            store
                .get_session("codex001")
                .unwrap()
                .unwrap()
                .provider_resume_id
                .as_deref(),
            Some("new-thread")
        );
        let count: i64 = rusqlite::Connection::open(state_file.with_extension("usage.db"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM seat_sessions WHERE seat_id = 'codex001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn claude_restore_discovers_missing_transcript_path_from_project_history() {
        let _guard = TEST_ENV_LOCK.lock().unwrap();
        let temp_dir = unique_temp_path("claude-transcript-discovery");
        let home = temp_dir.join("home");
        let working_dir = temp_dir.join("repo");
        fs::create_dir_all(&working_dir).unwrap();
        let _home_restore = EnvVarRestore::set("HOME", &home);

        let transcript_id = "49d072b4-4080-4702-9215-b6e7f04aa2c8";
        let project_dir = home
            .join(".claude")
            .join("projects")
            .join(claude_project_dir_name(working_dir.to_str().unwrap()));
        fs::create_dir_all(&project_dir).unwrap();
        let transcript_path = project_dir.join(format!("{transcript_id}.jsonl"));
        fs::write(
            &transcript_path,
            format!(
                "{}\n{}\n",
                json!({
                    "type": "custom-title",
                    "customTitle": "UI-fixer",
                    "sessionId": transcript_id
                }),
                json!({
                    "type": "user",
                    "cwd": working_dir.display().to_string(),
                    "timestamp": "2026-06-23T01:20:00Z"
                })
            ),
        )
        .unwrap();

        let mut session = session_record("stopped");
        session.id = "c1b1b826".to_owned();
        session.provider = "claude".to_owned();
        session.working_dir = working_dir.display().to_string();
        session.provider_resume_id = None;
        session.transcript_path = None;
        session.last_activity = "2026-06-23T01:27:23Z".to_owned();

        let discovered = discover_claude_transcript_path(
            &session,
            &[session.clone()],
            &[home.join(".claude").join("projects")],
        );

        assert_eq!(
            discovered.as_deref(),
            Some(transcript_path.canonicalize().unwrap().to_str().unwrap())
        );
        session.transcript_path = discovered;
        assert_eq!(
            provider_resume_id_for_restore(&session).as_deref(),
            Some(transcript_id)
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn claude_restore_excludes_sibling_artifacts_of_a_claimed_provider_session() {
        let temp_dir = unique_temp_path("claude-claimed-transcript-siblings");
        let projects_root = temp_dir.join("projects");
        let working_dir = temp_dir.join("repo");
        let transcript_id = "49d072b4-4080-4702-9215-b6e7f04aa2c8";
        let transcript_dir = projects_root
            .join(claude_project_dir_name(working_dir.to_str().unwrap()))
            .join(transcript_id);
        let chat_path = transcript_dir.join("chat.jsonl");
        let subagent_path = transcript_dir.join("subagents").join("agent-worker.jsonl");
        fs::create_dir_all(subagent_path.parent().unwrap()).unwrap();
        let transcript_line = |timestamp: &str| {
            format!(
                "{}\n",
                json!({
                    "type": "user",
                    "sessionId": transcript_id,
                    "cwd": working_dir.display().to_string(),
                    "timestamp": timestamp
                })
            )
        };
        fs::write(&chat_path, transcript_line("2026-06-23T01:20:00Z")).unwrap();
        fs::write(&subagent_path, transcript_line("2026-06-23T01:21:00Z")).unwrap();

        let mut claimed = session_record("running");
        claimed.id = "claimed1".to_owned();
        claimed.provider = "claude".to_owned();
        claimed.working_dir = working_dir.display().to_string();
        claimed.transcript_path = Some(chat_path.display().to_string());
        let mut restoring = session_record("stopped");
        restoring.id = "restore1".to_owned();
        restoring.provider = "claude".to_owned();
        restoring.working_dir = working_dir.display().to_string();
        restoring.transcript_path = None;
        restoring.provider_resume_id = None;
        restoring.last_activity = "2026-06-23T01:22:00Z".to_owned();

        assert_eq!(
            discover_claude_transcript_path(
                &restoring,
                &[restoring.clone(), claimed],
                &[projects_root],
            ),
            None
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    /// A monitored child plus the parent it reports to. `enabled` drives the
    /// `context_monitor_enabled` gate; `notify` is the monitor target, left
    /// unset to exercise the parent fallback.
    fn store_with_monitored_child(
        label: &str,
        enabled: bool,
        notify: Option<&str>,
    ) -> SessionStore {
        let state_file = unique_temp_path(label);
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    {
                        "id": "parent01",
                        "name": "claude-parent01",
                        "working_dir": "/repo",
                        "tmux_session": "claude-parent01",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    },
                    {
                        "id": "child001",
                        "name": "claude-child001",
                        "friendly_name": "worker",
                        "working_dir": "/repo",
                        "tmux_session": "claude-child001",
                        "parent_session_id": "parent01",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00",
                        "context_monitor_enabled": enabled,
                        "context_monitor_notify": notify
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        SessionStore::new_with_legacy_fallback(state_file.clone(), state_file)
    }

    fn store_with_provider_child(label: &str, provider: &str, status: &str) -> SessionStore {
        let store = store_with_monitored_child(label, false, None);
        let mut state = store.load_raw_json_value().unwrap();
        let sessions = ensure_sessions_array_mut(&mut state).unwrap();
        let child = session_object_mut(sessions, "child001").unwrap();
        child.insert("provider".to_owned(), Value::String(provider.to_owned()));
        child.insert("status".to_owned(), Value::String(status.to_owned()));
        store.write_raw_json_value(&state).unwrap();
        store
    }

    fn usage_event(session_id: &str, used_percentage: f64, tokens: i64) -> ContextUsageEvent {
        ContextUsageEvent {
            session_id: session_id.to_owned(),
            used_percentage: Some(used_percentage),
            total_input_tokens: Some(tokens),
            ..ContextUsageEvent::default()
        }
    }

    fn usage_event_at(
        session_id: &str,
        used_percentage: f64,
        tokens: i64,
        emitted_at: &str,
    ) -> ContextUsageEvent {
        ContextUsageEvent {
            emitted_at: Some(emitted_at.to_owned()),
            ..usage_event(session_id, used_percentage, tokens)
        }
    }

    fn lifecycle_event(session_id: &str, event: &str) -> ContextUsageEvent {
        ContextUsageEvent {
            session_id: session_id.to_owned(),
            event: Some(event.to_owned()),
            ..ContextUsageEvent::default()
        }
    }

    fn context_monitor_messages(store: &SessionStore) -> Vec<Value> {
        store
            .load_raw_json_value()
            .unwrap()
            .get("retained_pending_messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|message| {
                message.get("message_category").and_then(Value::as_str) == Some("context_monitor")
            })
            .collect()
    }

    fn queued_context_monitor_messages(store: &SessionStore) -> Vec<PendingMessage> {
        store
            .queue_store
            .as_ref()
            .unwrap()
            .pending_messages_for_target_by_category("parent01", "context_monitor", 10)
            .unwrap()
    }

    fn store_with_queued_monitored_child(
        label: &str,
        enabled: bool,
        notify: Option<&str>,
    ) -> (SessionStore, PathBuf) {
        let legacy_store = store_with_monitored_child(label, enabled, notify);
        let queue_path = unique_temp_path(&format!("{label}-queue"));
        let store =
            SessionStore::new_with_queue(legacy_store.state_file.clone(), queue_path.clone());
        (store, queue_path)
    }

    fn context_monitor_request(
        warning_percentage: Option<f64>,
        critical_percentage: Option<f64>,
    ) -> ContextMonitorRequest {
        ContextMonitorRequest {
            enabled: true,
            requester_session_id: "parent01".to_owned(),
            notify_session_id: Some("parent01".to_owned()),
            threshold_percentages: None,
            warning_percentage,
            critical_percentage,
            use_default_thresholds: false,
        }
    }

    fn context_monitor_levels_request(
        levels: Vec<f64>,
        notify_session_id: &str,
    ) -> ContextMonitorRequest {
        ContextMonitorRequest {
            enabled: true,
            requester_session_id: "child001".to_owned(),
            notify_session_id: Some(notify_session_id.to_owned()),
            threshold_percentages: Some(levels),
            warning_percentage: None,
            critical_percentage: None,
            use_default_thresholds: false,
        }
    }

    #[test]
    fn context_usage_update_persists_tokens_and_warns_once_per_cycle() {
        let store = store_with_monitored_child("ctxwarn", true, Some("parent01"));

        assert_eq!(
            store
                .apply_context_usage_event(&usage_event("child001", 52.0, 104_000), None)
                .unwrap(),
            ContextUsageOutcome::Recorded {
                used_percentage: 52.0
            }
        );
        // Every subsequent render posts again; the agent must not be told twice.
        store
            .apply_context_usage_event(&usage_event("child001", 55.0, 110_000), None)
            .unwrap();
        store
            .apply_context_usage_event(&usage_event("child001", 60.0, 120_000), None)
            .unwrap();

        let session = store.get_session("child001").unwrap().unwrap();
        assert_eq!(session.tokens_used, 120_000);
        assert_eq!(session.context_used_percentage, Some(60.0));
        assert_eq!(session.context_total_input_tokens, Some(120_000));
        assert!(session.context_sampled_at.is_some());
        assert!(session.context_warning_sent);
        assert!(!session.context_critical_sent);
        assert_eq!(context_monitor_messages(&store).len(), 1);
    }

    #[test]
    fn context_monitor_reports_each_registered_level_without_prescribing_policy() {
        let store = store_with_monitored_child("ctxlevels", true, Some("child001"));
        store
            .set_context_monitor(
                "child001",
                context_monitor_levels_request(vec![10.0, 20.0, 30.0], "child001"),
            )
            .unwrap();

        store
            .apply_context_usage_event(&usage_event("child001", 9.0, 18_000), None)
            .unwrap();
        assert!(context_monitor_messages(&store).is_empty());

        store
            .apply_context_usage_event(&usage_event("child001", 10.0, 20_000), None)
            .unwrap();
        let first = context_monitor_messages(&store);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["text"], "[sm context] Your context is now at 10%.");
        assert!(!first[0]["text"].as_str().unwrap().contains("handoff"));
        assert!(!first[0]["text"].as_str().unwrap().contains("critical"));

        // A delayed status-line sample may cross multiple levels. It produces
        // one factual reading and latches every crossed level, so later
        // unchanged renders do not create stale alert bursts.
        store
            .apply_context_usage_event(&usage_event("child001", 25.0, 50_000), None)
            .unwrap();
        store
            .apply_context_usage_event(&usage_event("child001", 25.0, 50_000), None)
            .unwrap();
        let messages = context_monitor_messages(&store);
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[1]["text"],
            "[sm context] Your context is now at 25%."
        );

        store
            .apply_context_usage_event(&usage_event("child001", 30.0, 60_000), None)
            .unwrap();
        let messages = context_monitor_messages(&store);
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[2]["text"],
            "[sm context] Your context is now at 30%."
        );
    }

    #[test]
    fn context_snapshot_reports_latest_cached_usage() {
        let store = store_with_monitored_child("ctxsnapshot", true, Some("parent01"));
        store
            .apply_context_usage_event(&usage_event("child001", 43.0, 86_214), None)
            .unwrap();

        let snapshot = store.get_context_snapshot("child001").unwrap().unwrap();

        assert_eq!(snapshot.session_id, "child001");
        assert_eq!(snapshot.friendly_name.as_deref(), Some("worker"));
        assert_eq!(snapshot.used_percentage, Some(43.0));
        assert_eq!(snapshot.total_input_tokens, Some(86_214));
        assert_eq!(snapshot.state, "normal");
        assert_eq!(snapshot.warning_percentage, Some(50.0));
        assert_eq!(snapshot.critical_percentage, Some(65.0));
        assert_eq!(snapshot.threshold_source, "default");
        assert!(snapshot.context_monitor_enabled);
        assert!(snapshot.context_monitor_enforced);
        assert_eq!(snapshot.notify_session_id.as_deref(), Some("parent01"));
        assert!(!snapshot.compaction_active);
    }

    #[test]
    fn seat_threshold_override_persists_across_restart_and_controls_alerting() {
        let (store, queue_path) =
            store_with_queued_monitored_child("ctxseatthreshold", true, Some("parent01"));
        assert!(matches!(
            store
                .set_context_monitor("child001", context_monitor_request(Some(40.0), Some(60.0)),)
                .unwrap(),
            ContextMonitorOutcome::Updated(ContextMonitorResult {
                enabled: true,
                warning_percentage: Some(40.0),
                critical_percentage: Some(60.0),
                ..
            })
        ));

        store
            .apply_context_usage_event(&usage_event("child001", 39.9, 79_800), None)
            .unwrap();
        assert!(queued_context_monitor_messages(&store).is_empty());
        store
            .apply_context_usage_event(&usage_event("child001", 40.0, 80_000), None)
            .unwrap();
        assert_eq!(queued_context_monitor_messages(&store).len(), 1);

        let restarted = SessionStore::new_with_queue(store.state_file.clone(), queue_path);
        let status = restarted.list_context_monitors().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].warning_percentage, Some(40.0));
        assert_eq!(status[0].critical_percentage, Some(60.0));
        assert_eq!(status[0].threshold_source, "custom");
        assert!(status[0].enforced);
        let snapshot = restarted.get_context_snapshot("child001").unwrap().unwrap();
        assert_eq!(snapshot.warning_percentage, Some(40.0));
        assert_eq!(snapshot.critical_percentage, Some(60.0));
        assert_eq!(snapshot.threshold_source, "custom");
        assert!(snapshot.context_monitor_enforced);

        // The warning latch survives the restart while the critical threshold
        // remains independently available for the same cycle.
        restarted
            .apply_context_usage_event(&usage_event("child001", 55.0, 110_000), None)
            .unwrap();
        restarted
            .apply_context_usage_event(&usage_event("child001", 60.0, 120_000), None)
            .unwrap();
        assert_eq!(queued_context_monitor_messages(&restarted).len(), 2);
    }

    #[test]
    fn invalid_or_inverted_threshold_requests_leave_durable_state_unchanged() {
        let store = store_with_monitored_child("ctxinvalidthreshold", true, Some("parent01"));
        let before = store.load_raw_json_value().unwrap();
        for (warning, critical) in [
            (Some(0.0), None),
            (Some(70.0), Some(70.0)),
            (Some(80.0), Some(40.0)),
        ] {
            assert!(matches!(
                store
                    .set_context_monitor("child001", context_monitor_request(warning, critical),)
                    .unwrap(),
                ContextMonitorOutcome::InvalidThresholdConfig(_)
            ));
            assert_eq!(store.load_raw_json_value().unwrap(), before);
        }
    }

    #[test]
    fn threshold_reconfiguration_cancels_stale_alerts_and_waits_for_a_fresh_sample() {
        let (store, _) =
            store_with_queued_monitored_child("ctxthresholdstale", true, Some("parent01"));
        store
            .apply_context_usage_event(
                &usage_event_at("child001", 70.0, 140_000, "2026-08-17T10:00:00Z"),
                None,
            )
            .unwrap();
        assert_eq!(queued_context_monitor_messages(&store).len(), 1);

        store
            .set_context_monitor("child001", context_monitor_request(Some(40.0), Some(60.0)))
            .unwrap();
        // Reconfiguration cancels a pending alert that was generated under the
        // previous policy. Cached telemetry must not manufacture a replacement.
        assert!(queued_context_monitor_messages(&store).is_empty());
        assert_eq!(
            store
                .apply_context_usage_event(
                    &usage_event_at("child001", 70.0, 140_000, "2026-08-17T09:59:59Z"),
                    None,
                )
                .unwrap(),
            ContextUsageOutcome::StaleSample
        );
        assert!(queued_context_monitor_messages(&store).is_empty());

        store
            .apply_context_usage_event(
                &usage_event_at("child001", 40.0, 80_000, "2026-08-17T10:00:01Z"),
                None,
            )
            .unwrap();
        assert_eq!(queued_context_monitor_messages(&store).len(), 1);
    }

    #[test]
    fn invalid_persisted_threshold_is_visible_as_unenforced_and_never_alerts() {
        let store = store_with_monitored_child("ctxpersistedinvalid", true, Some("parent01"));
        let mut state = store.load_raw_json_value().unwrap();
        session_object_mut(ensure_sessions_array_mut(&mut state).unwrap(), "child001")
            .unwrap()
            .insert(
                "context_monitor_warning_percentage".to_owned(),
                json!(100.0),
            );
        store.write_raw_json_value(&state).unwrap();

        let status = store.list_context_monitors().unwrap();
        assert_eq!(status[0].threshold_source, "invalid");
        assert!(!status[0].enforced);
        assert_eq!(status[0].warning_percentage, None);
        assert_eq!(status[0].critical_percentage, None);
        let snapshot = store.get_context_snapshot("child001").unwrap().unwrap();
        assert_eq!(snapshot.state, "thresholds_unavailable");
        assert_eq!(snapshot.threshold_source, "invalid");
        assert!(!snapshot.context_monitor_enforced);

        store
            .apply_context_usage_event(&usage_event("child001", 99.0, 198_000), None)
            .unwrap();
        assert!(context_monitor_messages(&store).is_empty());
    }

    #[test]
    fn repeated_identical_usage_samples_do_not_rewrite_the_state_file() {
        // The status line re-renders far more often than the context moves, and
        // the live state file is around a megabyte.
        let store = store_with_monitored_child("ctxnowrite", true, Some("parent01"));
        store
            .apply_context_usage_event(&usage_event("child001", 20.0, 40_000), None)
            .unwrap();

        let modified_at = fs::metadata(&store.state_file).unwrap().modified().unwrap();
        thread::sleep(Duration::from_millis(20));
        store
            .apply_context_usage_event(&usage_event("child001", 20.0, 40_000), None)
            .unwrap();
        assert_eq!(
            fs::metadata(&store.state_file).unwrap().modified().unwrap(),
            modified_at
        );

        // A real change still lands.
        store
            .apply_context_usage_event(&usage_event("child001", 21.0, 42_000), None)
            .unwrap();
        assert_ne!(
            fs::metadata(&store.state_file).unwrap().modified().unwrap(),
            modified_at
        );
        assert_eq!(
            store.get_session("child001").unwrap().unwrap().tokens_used,
            42_000
        );
    }

    #[test]
    fn out_of_order_usage_samples_do_not_regress_the_cached_snapshot() {
        let store = store_with_monitored_child("ctxoldsample", false, Some("parent01"));
        store
            .apply_context_usage_event(
                &usage_event_at("child001", 60.0, 120_000, "2026-07-28T10:01:00.000000Z"),
                None,
            )
            .unwrap();

        assert_eq!(
            store
                .apply_context_usage_event(
                    &usage_event_at("child001", 45.0, 90_000, "2026-07-28T10:00:30.000000Z",),
                    None,
                )
                .unwrap(),
            ContextUsageOutcome::StaleSample
        );

        let session = store.get_session("child001").unwrap().unwrap();
        assert_eq!(session.context_used_percentage, Some(60.0));
        assert_eq!(session.context_total_input_tokens, Some(120_000));
        assert_eq!(session.tokens_used, 120_000);
    }

    #[test]
    fn identical_stamped_usage_samples_advance_the_ordering_watermark() {
        let store = store_with_monitored_child("ctxsamewatermark", false, Some("parent01"));
        store
            .apply_context_usage_event(
                &usage_event_at("child001", 60.0, 120_000, "2026-07-28T10:00:00.000000Z"),
                None,
            )
            .unwrap();

        store
            .apply_context_usage_event(
                &usage_event_at("child001", 60.0, 120_000, "2026-07-28T10:01:00.000000Z"),
                None,
            )
            .unwrap();

        assert_eq!(
            store
                .apply_context_usage_event(
                    &usage_event_at("child001", 45.0, 90_000, "2026-07-28T10:00:30.000000Z",),
                    None,
                )
                .unwrap(),
            ContextUsageOutcome::StaleSample
        );

        let session = store.get_session("child001").unwrap().unwrap();
        assert_eq!(session.context_used_percentage, Some(60.0));
        assert_eq!(session.context_total_input_tokens, Some(120_000));
        assert_eq!(
            session.context_sampled_at.as_deref(),
            Some("2026-07-28T10:01:00.000000Z")
        );
    }

    #[test]
    fn enabling_monitoring_rearms_the_latches() {
        // A monitor that registers after the context is already high would
        // otherwise hear nothing until a compaction happened to clear the
        // latches a previous cycle set.
        let store = store_with_monitored_child("ctxreenable", true, Some("parent01"));
        // A sample past the critical line takes the critical branch only.
        store
            .apply_context_usage_event(&usage_event("child001", 70.0, 140_000), None)
            .unwrap();
        assert_eq!(context_monitor_messages(&store).len(), 1);

        store
            .set_context_monitor(
                "child001",
                ContextMonitorRequest {
                    enabled: true,
                    requester_session_id: "parent01".to_owned(),
                    notify_session_id: Some("parent01".to_owned()),
                    threshold_percentages: None,
                    warning_percentage: None,
                    critical_percentage: None,
                    use_default_thresholds: false,
                },
            )
            .unwrap();

        let session = store.get_session("child001").unwrap().unwrap();
        assert!(!session.context_warning_sent);
        assert!(!session.context_critical_sent);

        store
            .apply_context_usage_event(&usage_event("child001", 70.0, 140_000), None)
            .unwrap();
        assert_eq!(context_monitor_messages(&store).len(), 2);
    }

    #[test]
    fn clearing_a_session_rearms_the_latches() {
        // Claude reports a clear through its SessionStart(clear) hook, but codex
        // has no equivalent, so a cleared codex session would carry its latches
        // into the new cycle and never warn again.
        let store = store_with_monitored_child("ctxclear", true, Some("parent01"));
        store
            .apply_context_usage_event(&usage_event("child001", 70.0, 140_000), None)
            .unwrap();
        assert!(
            store
                .get_session("child001")
                .unwrap()
                .unwrap()
                .context_critical_sent
        );

        store
            .clear_core_session(
                "child001",
                ClearSessionRequest {
                    prompt: None,
                    requester_session_id: Some("parent01".to_owned()),
                },
            )
            .unwrap();

        let session = store.get_session("child001").unwrap().unwrap();
        assert!(!session.context_warning_sent);
        assert!(!session.context_critical_sent);
    }

    #[test]
    fn a_sample_that_raced_a_reset_does_not_reopen_the_old_cycle() {
        // Status-line samples ride a detached curl, so a render that races a
        // /clear can arrive after it while still describing the discarded
        // context. Applying it would restore the stale token count and re-latch
        // the flags the reset just cleared, silencing the next real warning.
        let store = store_with_monitored_child("ctxrace", true, Some("parent01"));
        store
            .apply_context_usage_event(&usage_event("child001", 20.0, 40_000), None)
            .unwrap();

        // Both stamps come from the producer host: the reset hook's and the
        // sample's. The server clock is never in the comparison.
        let sampled_at = "2026-06-01T00:00:10.000000Z";
        let mut reset = lifecycle_event("child001", "context_reset");
        reset.emitted_at = Some("2026-06-01T00:00:11.000000Z".to_owned());
        store.apply_context_usage_event(&reset, None).unwrap();

        let mut late = usage_event("child001", 70.0, 140_000);
        late.emitted_at = Some(sampled_at.to_owned());
        assert_eq!(
            store.apply_context_usage_event(&late, None).unwrap(),
            ContextUsageOutcome::StaleSample
        );

        let session = store.get_session("child001").unwrap().unwrap();
        assert_eq!(session.tokens_used, 40_000);
        assert!(!session.context_critical_sent);
        assert!(context_monitor_messages(&store).is_empty());

        // The next sample of the new cycle still warns.
        let mut fresh = usage_event("child001", 70.0, 140_000);
        fresh.emitted_at = Some("2026-06-01T00:00:12.000000Z".to_owned());
        store.apply_context_usage_event(&fresh, None).unwrap();
        assert_eq!(context_monitor_messages(&store).len(), 1);
    }

    #[test]
    fn a_producer_clock_behind_the_server_still_reports() {
        // The boundary is the reset hook's own stamp, so a node whose clock
        // trails the primary is compared only against itself. Comparing against
        // the server clock would freeze the monitor for the length of the skew
        // after every reset.
        let store = store_with_monitored_child("ctxskew", true, Some("parent01"));

        let mut reset = lifecycle_event("child001", "context_reset");
        reset.emitted_at = Some("2000-01-01T00:00:00.000000Z".to_owned());
        store.apply_context_usage_event(&reset, None).unwrap();

        let mut sample = usage_event("child001", 70.0, 140_000);
        sample.emitted_at = Some("2000-01-01T00:00:01.000000Z".to_owned());

        assert_eq!(
            store.apply_context_usage_event(&sample, None).unwrap(),
            ContextUsageOutcome::Recorded {
                used_percentage: 70.0
            }
        );
        assert_eq!(context_monitor_messages(&store).len(), 1);
    }

    #[test]
    fn a_server_side_clear_does_not_gate_producer_samples() {
        // sm clear has no producer stamp to compare against, so no boundary is
        // recorded and samples are not second-guessed. The queued alerts from
        // the discarded cycle are dropped instead.
        let store = store_with_monitored_child("ctxserverclear", true, Some("parent01"));
        store
            .clear_core_session(
                "child001",
                ClearSessionRequest {
                    prompt: None,
                    requester_session_id: Some("parent01".to_owned()),
                },
            )
            .unwrap();

        let mut sample = usage_event("child001", 70.0, 140_000);
        sample.emitted_at = Some("2000-01-01T00:00:00.000000Z".to_owned());

        assert_eq!(
            store.apply_context_usage_event(&sample, None).unwrap(),
            ContextUsageOutcome::Recorded {
                used_percentage: 70.0
            }
        );
    }

    #[test]
    fn unstamped_samples_are_still_accepted() {
        // A hook script too old to stamp itself must keep working, matching the
        // tolerance the lifecycle hooks already apply.
        let store = store_with_monitored_child("ctxunstamped", true, Some("parent01"));
        store
            .apply_context_usage_event(&lifecycle_event("child001", "context_reset"), None)
            .unwrap();

        assert_eq!(
            store
                .apply_context_usage_event(&usage_event("child001", 70.0, 140_000), None)
                .unwrap(),
            ContextUsageOutcome::Recorded {
                used_percentage: 70.0
            }
        );
    }

    #[test]
    fn context_warning_does_not_refire_after_a_restart() {
        // The Python server kept these latches in memory, so every restart
        // re-alerted the parent about context it had already been told about.
        let store = store_with_monitored_child("ctxrestart", true, Some("parent01"));
        store
            .apply_context_usage_event(&usage_event("child001", 52.0, 104_000), None)
            .unwrap();

        let restarted = SessionStore::new_with_legacy_fallback(
            store.state_file.clone(),
            store.state_file.clone(),
        );
        restarted
            .apply_context_usage_event(&usage_event("child001", 53.0, 106_000), None)
            .unwrap();

        assert_eq!(context_monitor_messages(&restarted).len(), 1);
    }

    #[test]
    fn context_levels_notify_separately_without_priority_policy() {
        let store = store_with_monitored_child("ctxcrit", true, Some("parent01"));
        store
            .apply_context_usage_event(&usage_event("child001", 52.0, 104_000), None)
            .unwrap();
        store
            .apply_context_usage_event(&usage_event("child001", 70.0, 140_000), None)
            .unwrap();
        store
            .apply_context_usage_event(&usage_event("child001", 72.0, 144_000), None)
            .unwrap();

        let session = store.get_session("child001").unwrap().unwrap();
        assert!(session.context_critical_sent);
        let messages = context_monitor_messages(&store);
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[1].get("delivery_mode").and_then(Value::as_str),
            Some("sequential")
        );
    }

    #[test]
    fn context_usage_is_suppressed_for_unregistered_sessions() {
        // #206: alerting is opt-in, but cached context readout is passive.
        let store = store_with_monitored_child("ctxgate", false, None);

        assert_eq!(
            store
                .apply_context_usage_event(&usage_event("child001", 90.0, 180_000), None)
                .unwrap(),
            ContextUsageOutcome::NotRegistered
        );

        let session = store.get_session("child001").unwrap().unwrap();
        assert_eq!(session.tokens_used, 180_000);
        assert_eq!(session.context_used_percentage, Some(90.0));
        assert_eq!(session.context_total_input_tokens, Some(180_000));
        assert!(session.context_sampled_at.is_some());
        assert_eq!(
            store
                .get_context_snapshot("child001")
                .unwrap()
                .unwrap()
                .state,
            "critical"
        );
        assert!(context_monitor_messages(&store).is_empty());
    }

    #[test]
    fn compaction_notifies_the_parent_even_when_unregistered() {
        // #210: compaction is context loss, so it bypasses the opt-in gate and
        // falls back to the parent when no monitor is registered.
        let store = store_with_monitored_child("ctxcompact", false, None);

        assert_eq!(
            store
                .apply_context_usage_event(&lifecycle_event("child001", "compaction"), None)
                .unwrap(),
            ContextUsageOutcome::CompactionLogged
        );

        let messages = context_monitor_messages(&store);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].get("target_session_id").and_then(Value::as_str),
            Some("parent01")
        );
        assert!(messages[0]
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("worker")));
    }

    #[test]
    fn compaction_rearms_the_warning_for_the_next_cycle() {
        let store = store_with_monitored_child("ctxrearm", true, Some("parent01"));
        store
            .apply_context_usage_event(&usage_event("child001", 52.0, 104_000), None)
            .unwrap();
        store
            .apply_context_usage_event(&lifecycle_event("child001", "compaction"), None)
            .unwrap();

        let session = store.get_session("child001").unwrap().unwrap();
        assert!(!session.context_warning_sent);

        // Post-compaction context can legitimately sit above the warning line,
        // so the next cycle's first sample must be able to warn again.
        store
            .apply_context_usage_event(&usage_event("child001", 52.0, 104_000), None)
            .unwrap();
        // compaction notice + the two warnings
        assert_eq!(context_monitor_messages(&store).len(), 3);
    }

    #[test]
    fn fresh_usage_sample_clears_stale_compaction_state() {
        let store = store_with_monitored_child("ctxcompactfresh", true, Some("parent01"));
        let mut compaction = lifecycle_event("child001", "compaction");
        compaction.emitted_at = Some("2026-07-28T10:00:00.000000Z".to_owned());
        store.apply_context_usage_event(&compaction, None).unwrap();

        let compacting = store.get_context_snapshot("child001").unwrap().unwrap();
        assert_eq!(compacting.state, "compacting");
        assert!(compacting.compaction_active);

        store
            .apply_context_usage_event(
                &usage_event_at("child001", 43.0, 86_214, "2026-07-28T10:00:01.000000Z"),
                None,
            )
            .unwrap();

        let snapshot = store.get_context_snapshot("child001").unwrap().unwrap();
        assert_eq!(snapshot.state, "normal");
        assert!(!snapshot.compaction_active);
        assert_eq!(snapshot.used_percentage, Some(43.0));
    }

    #[test]
    fn context_reset_rearms_flags_and_clears_stale_status() {
        let store = store_with_monitored_child("ctxreset", true, Some("parent01"));
        store
            .apply_context_usage_event(&usage_event("child001", 70.0, 140_000), None)
            .unwrap();

        assert_eq!(
            store
                .apply_context_usage_event(&lifecycle_event("child001", "context_reset"), None)
                .unwrap(),
            ContextUsageOutcome::FlagsReset
        );

        let session = store.get_session("child001").unwrap().unwrap();
        assert!(!session.context_warning_sent);
        assert!(!session.context_critical_sent);
        assert!(session.agent_status_text.is_none());
    }

    #[test]
    fn context_usage_ignores_unknown_sessions_and_null_percentages() {
        let store = store_with_monitored_child("ctxnull", true, Some("parent01"));

        assert_eq!(
            store
                .apply_context_usage_event(&usage_event("nosuchid", 90.0, 1), None)
                .unwrap(),
            ContextUsageOutcome::UnknownSession
        );
        assert_eq!(
            store
                .apply_context_usage_event(
                    &ContextUsageEvent {
                        session_id: "child001".to_owned(),
                        total_input_tokens: Some(0),
                        ..ContextUsageEvent::default()
                    },
                    None
                )
                .unwrap(),
            ContextUsageOutcome::NoUsage
        );
        assert_eq!(
            store.get_session("child001").unwrap().unwrap().tokens_used,
            0
        );
    }

    #[test]
    fn codex_fork_monitor_enrolls_persists_across_restart_and_notifies_without_rotation() {
        let store = store_with_provider_child("ctxcodex", "codex-fork", "running");
        let request = ContextMonitorRequest {
            enabled: true,
            requester_session_id: "parent01".to_owned(),
            notify_session_id: Some("parent01".to_owned()),
            threshold_percentages: None,
            warning_percentage: None,
            critical_percentage: None,
            use_default_thresholds: false,
        };
        assert!(matches!(
            store
                .set_context_monitor("child001", request.clone())
                .unwrap(),
            ContextMonitorOutcome::Updated(ContextMonitorResult { enabled: true, .. })
        ));
        // Repeating enable is idempotent from the operator's perspective: one
        // durable status row, with a freshly armed alert cycle.
        assert!(matches!(
            store.set_context_monitor("child001", request).unwrap(),
            ContextMonitorOutcome::Updated(ContextMonitorResult { enabled: true, .. })
        ));
        assert_eq!(store.list_context_monitors().unwrap().len(), 1);

        let restarted = SessionStore::new_with_legacy_fallback(
            store.state_file.clone(),
            store.state_file.clone(),
        );
        assert_eq!(restarted.list_context_monitors().unwrap().len(), 1);

        let line = r#"{"event_type":"thread/tokenUsage/updated","payload":{"tokenUsage":{
            "total":{"totalTokens":900000},
            "last":{"totalTokens":181000},
            "modelContextWindow":258400}}}"#;
        restarted
            .apply_codex_fork_event_line("child001", line)
            .unwrap();

        let session = restarted.get_session("child001").unwrap().unwrap();
        assert_eq!(session.tokens_used, 181_000);
        assert_eq!(session.context_total_input_tokens, Some(181_000));
        assert!((session.context_used_percentage.unwrap() - 70.046_439_628_482_98).abs() < 1e-9);
        assert!(session.context_sampled_at.is_some());
        assert!(session.context_critical_sent);
        assert!(session.context_monitor_enabled);
        assert_eq!(session.context_monitor_notify.as_deref(), Some("parent01"));
        assert_eq!(session.status, "running");
        assert_eq!(context_monitor_messages(&restarted).len(), 1);
        let snapshot = restarted.get_context_snapshot("child001").unwrap().unwrap();
        assert!(snapshot.context_monitor_enabled);
        assert_eq!(snapshot.notify_session_id.as_deref(), Some("parent01"));

        restarted
            .apply_codex_fork_event_line("child001", line)
            .unwrap();
        assert_eq!(context_monitor_messages(&restarted).len(), 1);
    }

    #[test]
    fn codex_fork_monitor_uses_only_root_usage_and_rearms_after_native_compaction() {
        let initial = store_with_provider_child("ctxcodexcycles", "codex-fork", "running");
        let state_file = initial.state_file.clone();
        let queue_db = state_file.with_extension("db");
        let event_stream = state_file.with_extension("events.jsonl");
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        let store = SessionStore::new_with_queue(state_file, queue_db);
        let mut state = store.load_raw_json_value().unwrap();
        let sessions = ensure_sessions_array_mut(&mut state).unwrap();
        session_object_mut(sessions, "child001").unwrap().insert(
            "provider_resume_id".to_owned(),
            Value::String("root-thread".to_owned()),
        );
        store.write_raw_json_value(&state).unwrap();
        store
            .set_context_monitor(
                "child001",
                ContextMonitorRequest {
                    enabled: true,
                    requester_session_id: "child001".to_owned(),
                    notify_session_id: Some("child001".to_owned()),
                    threshold_percentages: None,
                    warning_percentage: None,
                    critical_percentage: None,
                    use_default_thresholds: false,
                },
            )
            .unwrap();

        let usage_event = |thread_id: &str, tokens: i64| {
            format!(
                r#"{{"event_type":"thread/tokenUsage/updated","payload":{{"threadId":"{thread_id}","tokenUsage":{{"last":{{"totalTokens":{tokens}}},"modelContextWindow":258400}}}}}}"#
            )
        };
        // Descendant activity belongs in the attribution ledger, not the
        // managed root's occupancy or alert cycle.
        store
            .apply_codex_fork_event_line("child001", &usage_event("descendant", 181_000))
            .unwrap();
        let session = store.get_session("child001").unwrap().unwrap();
        assert_eq!(session.tokens_used, 0);
        assert!(!session.context_critical_sent);
        assert!(context_monitor_messages(&store).is_empty());

        store
            .apply_codex_fork_event_line("child001", &usage_event("root-thread", 181_000))
            .unwrap();
        let first_alert = context_monitor_messages(&store).pop().unwrap();
        let first_text = first_alert["text"].as_str().unwrap();
        assert_eq!(first_text, "[sm context] Your context is now at 70.0%.");
        assert!(!first_text.contains("handoff"));
        assert_eq!(
            queue
                .pending_messages_for_target("child001", 10)
                .unwrap()
                .len(),
            1
        );

        // A lower resident-token count is Codex's observed native-compaction
        // boundary. Recovery begins at EOF, so it must reconcile that current
        // gauge before retaining the pre-restart high-context alert.
        fs::write(
            &event_stream,
            format!("{}\n", usage_event("root-thread", 20_000)),
        )
        .unwrap();
        let recovery_offset = fs::metadata(&event_stream).unwrap().len();
        store
            .reconcile_codex_fork_context_at_restart("child001", &event_stream, recovery_offset)
            .unwrap();
        let compacted = store.get_session("child001").unwrap().unwrap();
        assert!(!compacted.context_warning_sent);
        assert!(!compacted.context_critical_sent);
        assert_eq!(compacted.context_total_input_tokens, Some(20_000));
        assert!(queue
            .pending_messages_for_target("child001", 10)
            .unwrap()
            .is_empty());
        let recovered_high = usage_event("root-thread", 181_000);
        fs::write(&event_stream, format!("{recovered_high}\n")).unwrap();
        let recovery_offset = fs::metadata(&event_stream).unwrap().len();
        store
            .reconcile_codex_fork_context_at_restart("child001", &event_stream, recovery_offset)
            .unwrap();
        assert_eq!(context_monitor_messages(&store).len(), 2);
        assert_eq!(
            queue
                .pending_messages_for_target("child001", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn codex_fork_restart_boundary_replays_a_completed_partial_jsonl_record() {
        let initial = store_with_provider_child("ctxcodexpartial", "codex-fork", "running");
        let state_file = initial.state_file.clone();
        let queue_db = state_file.with_extension("db");
        let event_stream = state_file.with_extension("events.jsonl");
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        let store = SessionStore::new_with_queue(state_file, queue_db);
        let mut state = store.load_raw_json_value().unwrap();
        let sessions = ensure_sessions_array_mut(&mut state).unwrap();
        session_object_mut(sessions, "child001").unwrap().insert(
            "provider_resume_id".to_owned(),
            Value::String("root-thread".to_owned()),
        );
        store.write_raw_json_value(&state).unwrap();
        store
            .set_context_monitor(
                "child001",
                ContextMonitorRequest {
                    enabled: true,
                    requester_session_id: "child001".to_owned(),
                    notify_session_id: Some("child001".to_owned()),
                    threshold_percentages: None,
                    warning_percentage: None,
                    critical_percentage: None,
                    use_default_thresholds: false,
                },
            )
            .unwrap();

        let complete = r#"{"event_type":"thread/tokenUsage/updated","payload":{"threadId":"root-thread","tokenUsage":{"last":{"totalTokens":20000},"modelContextWindow":258400}}}"#;
        let later = r#"{"event_type":"thread/tokenUsage/updated","payload":{"threadId":"root-thread","tokenUsage":{"last":{"totalTokens":181000},"modelContextWindow":258400}}}"#;
        let partial_len = later.len() - 7;
        fs::write(
            &event_stream,
            format!("{complete}\n{}", &later[..partial_len]),
        )
        .unwrap();

        let recovery_offset = codex_fork_complete_jsonl_boundary(&event_stream);
        assert_eq!(recovery_offset, (complete.len() + 1) as u64);
        store
            .reconcile_codex_fork_context_at_restart("child001", &event_stream, recovery_offset)
            .unwrap();
        assert_eq!(
            store
                .get_session("child001")
                .unwrap()
                .unwrap()
                .context_total_input_tokens,
            Some(20_000)
        );

        let mut stream = fs::OpenOptions::new()
            .append(true)
            .open(&event_stream)
            .unwrap();
        stream.write_all(later[partial_len..].as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        let resumed = fs::read_to_string(&event_stream).unwrap();
        for line in resumed[recovery_offset as usize..].lines() {
            store.apply_codex_fork_event_line("child001", line).unwrap();
        }

        let session = store.get_session("child001").unwrap().unwrap();
        assert_eq!(session.context_total_input_tokens, Some(181_000));
        assert!(session.context_critical_sent);
        assert_eq!(
            queue
                .pending_messages_for_target("child001", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn disabling_codex_monitor_cancels_its_queued_context_alerts() {
        let initial = store_with_provider_child("ctxcodexdisable", "codex-fork", "running");
        let state_file = initial.state_file.clone();
        let queue_db = state_file.with_extension("db");
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        let store = SessionStore::new_with_queue(state_file, queue_db);
        store
            .set_context_monitor(
                "child001",
                ContextMonitorRequest {
                    enabled: true,
                    requester_session_id: "child001".to_owned(),
                    notify_session_id: Some("child001".to_owned()),
                    threshold_percentages: None,
                    warning_percentage: None,
                    critical_percentage: None,
                    use_default_thresholds: false,
                },
            )
            .unwrap();
        store
            .apply_codex_fork_event_line(
                "child001",
                r#"{"event_type":"thread/tokenUsage/updated","payload":{"tokenUsage":{"last":{"totalTokens":181000},"modelContextWindow":258400}}}"#,
            )
            .unwrap();
        assert_eq!(
            queue
                .pending_messages_for_target("child001", 10)
                .unwrap()
                .len(),
            1
        );

        assert!(matches!(
            store
                .set_context_monitor(
                    "child001",
                    ContextMonitorRequest {
                        enabled: false,
                        requester_session_id: "child001".to_owned(),
                        notify_session_id: None,
                        threshold_percentages: None,
                        warning_percentage: None,
                        critical_percentage: None,
                        use_default_thresholds: false,
                    },
                )
                .unwrap(),
            ContextMonitorOutcome::Updated(ContextMonitorResult { enabled: false, .. })
        ));
        assert!(
            !store
                .get_session("child001")
                .unwrap()
                .unwrap()
                .context_monitor_enabled
        );
        assert!(queue
            .pending_messages_for_target("child001", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn context_monitor_enable_fails_closed_without_a_gauge_or_live_session() {
        for provider in ["codex", "codex-app"] {
            let codex = store_with_provider_child("ctxcodexnogauge", provider, "running");
            let unsupported_before = codex.load_raw_json_value().unwrap();
            assert!(matches!(
                codex
                    .set_context_monitor(
                        "child001",
                        ContextMonitorRequest {
                            enabled: true,
                            requester_session_id: "parent01".to_owned(),
                            notify_session_id: Some("parent01".to_owned()),
                            threshold_percentages: None,
                            warning_percentage: None,
                            critical_percentage: None,
                            use_default_thresholds: false,
                        },
                    )
                    .unwrap(),
                ContextMonitorOutcome::UnsupportedProvider(reported) if reported == provider
            ));
            assert_eq!(codex.load_raw_json_value().unwrap(), unsupported_before);
            assert!(codex.list_context_monitors().unwrap().is_empty());
        }

        let stopped = store_with_provider_child("ctxstopped", "claude", "stopped");
        let stopped_before = stopped.load_raw_json_value().unwrap();
        assert!(matches!(
            stopped
                .set_context_monitor(
                    "child001",
                    ContextMonitorRequest {
                        enabled: true,
                        requester_session_id: "parent01".to_owned(),
                        notify_session_id: Some("parent01".to_owned()),
                        threshold_percentages: None,
                        warning_percentage: None,
                        critical_percentage: None,
                        use_default_thresholds: false,
                    },
                )
                .unwrap(),
            ContextMonitorOutcome::NotRunning
        ));
        assert_eq!(stopped.load_raw_json_value().unwrap(), stopped_before);

        assert!(matches!(
            stopped
                .set_context_monitor(
                    "missing01",
                    ContextMonitorRequest {
                        enabled: true,
                        requester_session_id: "parent01".to_owned(),
                        notify_session_id: Some("parent01".to_owned()),
                        threshold_percentages: None,
                        warning_percentage: None,
                        critical_percentage: None,
                        use_default_thresholds: false,
                    },
                )
                .unwrap(),
            ContextMonitorOutcome::NotFound
        ));
    }

    #[test]
    fn codex_rate_limit_event_records_burn_for_the_current_account() {
        let state_file = unique_temp_path("codexburn");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let usage_db_path = state_file.with_extension("usage.db");
        let observed_at = OffsetDateTime::now_utc();
        UsageIdentityStore::new(&usage_db_path)
            .unwrap()
            .record_observation(
                Provider::Codex,
                Some(&AccountIdentity {
                    provider: Provider::Codex,
                    external_id: "codex-test-account".to_owned(),
                    label: None,
                    plan_tier: Some("pro".to_owned()),
                    extra_usage_enabled: None,
                }),
                observed_at - TimeDuration::minutes(1),
                None,
                None,
            )
            .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file)
            .with_usage_burn_store(UsageBurnStore::new(&usage_db_path).unwrap());
        let event_timestamp = observed_at.format(&Rfc3339).unwrap();
        let reset_at = (observed_at + TimeDuration::minutes(300)).unix_timestamp();

        store
            .apply_codex_fork_event_line(
                "codex001",
                &json!({
                    "event_type": "account/rateLimits/updated",
                    "ts": event_timestamp,
                    "payload": {"rateLimits": {
                        "limitId": "codex",
                        "primary": {
                            "usedPercent": 27,
                            "windowDurationMins": 300,
                            "resetsAt": reset_at
                        },
                        "secondary": null
                    }}
                })
                .to_string(),
            )
            .unwrap();

        let row: (String, String, f64, String) = Connection::open(usage_db_path)
            .unwrap()
            .query_row(
                "SELECT account_key, window_kind, percent, source FROM burn_samples",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "codex:codex-test-account".to_owned(),
                "codex_300".to_owned(),
                27.0,
                "codex_event".to_owned(),
            )
        );
    }

    #[test]
    fn usage_scan_persists_current_accounts_for_idle_seats() {
        let state_file = unique_temp_path("codexscanaccount");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    {
                        "id": "codex001",
                        "name": "codex-codex001",
                        "provider": "codex",
                        "working_dir": "/repo",
                        "tmux_session": "codex-codex001",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    },
                    {
                        "id": "claude001",
                        "name": "claude-claude001",
                        "provider": "claude",
                        "working_dir": "/repo",
                        "tmux_session": "claude-claude001",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let usage_db_path = state_file.with_extension("usage.db");
        let observed_at = OffsetDateTime::now_utc();
        let identity_store = UsageIdentityStore::new(&usage_db_path).unwrap();
        identity_store
            .record_observation(
                Provider::Codex,
                Some(&AccountIdentity {
                    provider: Provider::Codex,
                    external_id: "idle-account".to_owned(),
                    label: None,
                    plan_tier: Some("pro".to_owned()),
                    extra_usage_enabled: None,
                }),
                observed_at - TimeDuration::minutes(1),
                None,
                None,
            )
            .unwrap();
        identity_store
            .record_observation(
                Provider::Claude,
                Some(&AccountIdentity {
                    provider: Provider::Claude,
                    external_id: "idle-account".to_owned(),
                    label: None,
                    plan_tier: Some("max".to_owned()),
                    extra_usage_enabled: None,
                }),
                observed_at - TimeDuration::minutes(1),
                None,
                None,
            )
            .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file)
            .with_usage_db_path(usage_db_path.clone())
            .with_usage_identity_store(identity_store)
            .with_usage_ledger_store(UsageLedgerStore::new(&usage_db_path).unwrap());

        store.scan_usage_ledger().unwrap();

        assert_eq!(
            store.get_session("codex001").unwrap().unwrap().account_key,
            Some("codex:idle-account".to_owned())
        );
        assert_eq!(
            store.get_session("claude001").unwrap().unwrap().account_key,
            Some("claude:idle-account".to_owned())
        );
    }

    #[test]
    fn store_startup_purges_pending_codex_context_alerts() {
        let state_file = unique_temp_path("ctxcodexstartup");
        let queue_db = state_file.with_extension("db");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    {
                        "id": "parent01",
                        "name": "claude-parent01",
                        "provider": "claude",
                        "working_dir": "/repo",
                        "tmux_session": "claude-parent01",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    },
                    {
                        "id": "codex001",
                        "name": "codex-codex001",
                        "provider": "codex-app",
                        "working_dir": "/repo",
                        "tmux_session": "",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00",
                        "last_activity": "2026-06-01T00:01:00"
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue
            .enqueue_message_with_metadata(
                "parent01",
                "[sm context] stale Codex handoff prompt",
                "urgent",
                QueueMessageMetadata {
                    sender_session_id: Some("codex001".to_owned()),
                    message_category: Some("context_monitor".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        assert_eq!(
            queue
                .pending_messages_for_target("parent01", 10)
                .unwrap()
                .len(),
            1
        );

        let _store = SessionStore::new_with_queue(state_file, queue_db);

        assert!(queue
            .pending_messages_for_target("parent01", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn codex_token_usage_caches_snapshot_without_registration() {
        let state_file = unique_temp_path("ctxcodexgate");
        fs::write(
            &state_file,
            json!({
                "sessions": [{
                    "id": "codex001",
                    "name": "codex-codex001",
                    "provider": "codex-fork",
                    "working_dir": "/repo",
                    "tmux_session": "codex-codex001",
                    "status": "running",
                    "created_at": "2026-06-01T00:00:00",
                    "last_activity": "2026-06-01T00:01:00"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new_with_legacy_fallback(state_file.clone(), state_file);

        store
            .apply_codex_fork_event_line(
                "codex001",
                r#"{"event_type":"thread/tokenUsage/updated","payload":{"tokenUsage":{
                    "last":{"totalTokens":181000},"modelContextWindow":258400}}}"#,
            )
            .unwrap();

        let session = store.get_session("codex001").unwrap().unwrap();
        assert_eq!(session.tokens_used, 181_000);
        assert_eq!(session.context_total_input_tokens, Some(181_000));
        assert!((session.context_used_percentage.unwrap() - 70.046_439_628_482_98).abs() < 1e-9);
        assert!(session.context_sampled_at.is_some());
        assert!(context_monitor_messages(&store).is_empty());
    }

    #[test]
    fn codex_token_usage_event_reports_the_resident_context() {
        // `total` accumulates across every turn and runs far past the window;
        // only `last` describes what is currently resident.
        let event: Value = serde_json::from_str(
            r#"{"event_type":"thread/tokenUsage/updated","payload":{"tokenUsage":{
                 "total":{"totalTokens":1362757,"inputTokens":1354397},
                 "last":{"totalTokens":129200,"inputTokens":114706},
                 "modelContextWindow":258400}}}"#,
        )
        .unwrap();

        let usage = codex_fork_context_usage(event.as_object().unwrap()).unwrap();

        assert_eq!(usage.tokens_used, 129_200);
        assert!((usage.used_percentage - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn codex_token_usage_ignores_unrelated_events() {
        let event: Value =
            serde_json::from_str(r#"{"event_type":"turn/started","payload":{}}"#).unwrap();

        assert!(codex_fork_context_usage(event.as_object().unwrap()).is_none());
    }

    #[test]
    fn thousands_formatting_matches_the_python_message_text() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(104_000), "104,000");
        assert_eq!(format_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn measured_context_gauge_capability_is_explicit_per_provider() {
        assert!(provider_has_measured_context_gauge("claude"));
        assert!(provider_has_measured_context_gauge("codex-fork"));
        assert!(!provider_has_measured_context_gauge("codex"));
        assert!(!provider_has_measured_context_gauge("codex-app"));
        assert!(!provider_has_measured_context_gauge("unknown"));
    }

    #[test]
    fn legacy_tree_requests_preserve_their_persisted_stopped_parent_plan() {
        let legacy: ReparentRequestRecord = serde_json::from_value(json!({
            "id": "legacy01",
            "kind": "tree",
            "subject_session_id": "source01",
            "target_parent_session_id": "target01",
            "expected_parent_session_id": "stopped01",
            "expected_parent_is_live": false,
            "frozen_live_child_ids": ["target01"],
            "initiator_session_id": "source01",
            "required_agent_approvals": ["source01", "target01"],
            "required_human_approval": true,
            "status": "pending",
            "ready_to_apply": false,
            "created_at": "2026-08-16T00:00:00Z",
            "expires_at": "2026-08-17T00:00:00Z",
            "topology_fingerprint": "legacy"
        }))
        .unwrap();

        assert!(!legacy.detach_non_live_parent);
        assert_eq!(tree_target_parent_session_id(&legacy), Some("stopped01"));

        let mut current = legacy;
        current.detach_non_live_parent = true;
        current.required_human_approval = false;
        assert_eq!(tree_target_parent_session_id(&current), None);
    }

    #[test]
    fn approved_single_reparent_retargets_parent_derived_routing_across_stores() {
        let state_file = unique_temp_path("reparent-apply");
        let queue_db = state_file.with_extension("db");
        let old_credential = "old-parent-secret";
        let new_credential = "new-parent-secret";
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("oldpar01", None, old_credential),
                    reparent_test_session("newpar01", None, new_credential),
                    {
                        "id": "child001",
                        "name": "claude-child001",
                        "working_dir": "/repo",
                        "tmux_session": "claude-child001",
                        "status": "running",
                        "created_at": "2026-06-01T00:00:00Z",
                        "last_activity": "2026-06-01T00:01:00Z",
                        "parent_session_id": "oldpar01",
                        "context_monitor_enabled": true,
                        "context_monitor_notify": "oldpar01",
                        "context_monitor_notify_source": "parent_derived"
                    }
                ],
                "retained_parent_wake_registrations": [{
                    "id": "json-wake-child001",
                    "child_session_id": "child001",
                    "parent_session_id": "oldpar01",
                    "period_seconds": 600,
                    "is_active": true
                }]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        queue
            .register_parent_wake("child001", "oldpar01", 600)
            .unwrap();
        let message_id = queue
            .enqueue_message_with_metadata(
                "child001",
                "wait for parent",
                "important",
                QueueMessageMetadata {
                    parent_session_id: Some("oldpar01".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone());

        let request = match store
            .create_reparent_request(
                "child001",
                CreateReparentRequest {
                    requester_session_id: "oldpar01".to_owned(),
                    target_parent_session_id: "newpar01".to_owned(),
                },
                old_credential,
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        let applied = match store
            .decide_reparent_request(
                &request.id,
                DecideReparentRequest {
                    requester_session_id: "newpar01".to_owned(),
                },
                ReparentDecision::Approved,
                new_credential,
            )
            .unwrap()
        {
            ReparentMutationOutcome::Updated(record) => record,
            other => panic!("unexpected approval outcome: {other:?}"),
        };

        assert_eq!(applied.status, "applied");
        assert_eq!(applied.apply_stage.as_deref(), Some("applied"));
        let child = store.get_session("child001").unwrap().unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some("newpar01"));
        assert_eq!(child.context_monitor_notify.as_deref(), Some("newpar01"));
        assert!(child.context_monitor_enabled);
        assert_eq!(child.context_monitor_notify_source, "parent_derived");
        assert_eq!(
            queue
                .active_parent_wake_parent("child001")
                .unwrap()
                .as_deref(),
            Some("newpar01")
        );
        let pending = queue.pending_messages_for_target("child001", 10).unwrap();
        assert_eq!(
            pending
                .iter()
                .find(|message| message.id == message_id)
                .and_then(|message| message.parent_session_id.as_deref()),
            Some("newpar01")
        );
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        assert!(state["reparent_apply_lease"].is_null());
        assert_eq!(
            state["retained_parent_wake_registrations"][0]["parent_session_id"],
            "newpar01"
        );
        assert_eq!(
            state["retained_parent_wake_registrations"][0]["is_active"],
            true
        );
        assert!(matches!(
            store.retire_core_session("child001", Some("oldpar01")),
            Ok(CoreRetireOutcome::NotChild)
        ));
        assert!(matches!(
            store.retire_core_session("child001", Some("newpar01")),
            Ok(CoreRetireOutcome::Retired(_))
        ));

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn reparent_request_rejects_a_remote_required_approver() {
        let state_file = unique_temp_path("reparent-remote-approver");
        let mut target = reparent_test_session("newpar01", None, "new-secret");
        target["node"] = json!("remote-builder");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("oldpar01", None, "old-secret"),
                    target,
                    reparent_test_session("child001", Some("oldpar01"), "child-secret")
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new(state_file.clone());

        let outcome = store
            .create_reparent_request(
                "child001",
                CreateReparentRequest {
                    requester_session_id: "oldpar01".to_owned(),
                    target_parent_session_id: "newpar01".to_owned(),
                },
                "old-secret",
            )
            .unwrap();

        assert!(matches!(
            outcome,
            ReparentMutationOutcome::BadRequest(message)
                if message.contains("cannot participate in credential-bound consent")
        ));
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn reparent_tree_preview_reports_unsupported_approver_provider() {
        let state_file = unique_temp_path("reparent-tree-unsupported-approver");
        let mut target = reparent_test_session("target01", Some("source01"), "target-secret");
        target["provider"] = json!("codex-app");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("source01", None, "source-secret"),
                    target
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new(state_file.clone());

        let outcome = store
            .create_reparent_tree_request(
                "source01",
                CreateReparentTreeRequest {
                    requester_session_id: "source01".to_owned(),
                    target_session_id: "target01".to_owned(),
                    dry_run: true,
                },
                "source-secret",
            )
            .unwrap();

        assert!(matches!(
            outcome,
            ReparentMutationOutcome::BadRequest(message)
                if message.contains("target session target01")
                    && message.contains("cannot participate in credential-bound consent")
        ));
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn prequiesce_abort_failure_keeps_reparent_exclusivity() {
        let (store, _queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-prequiesce-exclusivity");
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let mut records = reparent_request_records(&state).unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id == request_id)
                .unwrap();
            record.status = "failed".to_owned();
            record.apply_stage = Some("prequiesce_aborting".to_owned());
            record.failure_reason = Some("injected abort failure".to_owned());
            store_reparent_request_records(&mut state, &records).unwrap();
            store.write_raw_json_value(&state).unwrap();
        }

        let outcome = store
            .create_reparent_request(
                "child001",
                CreateReparentRequest {
                    requester_session_id: "oldpar01".to_owned(),
                    target_parent_session_id: "newpar01".to_owned(),
                },
                "old-parent-secret",
            )
            .unwrap();

        assert!(matches!(
            outcome,
            ReparentMutationOutcome::Conflict(message) if message.contains(&request_id)
        ));
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn reparent_notifications_are_exact_and_idempotent_across_reconciliation() {
        let state_file = unique_temp_path("reparent-notifications");
        let queue_db = state_file.with_extension("db");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("oldpar01", None, "old-secret"),
                    reparent_test_session("newpar01", None, "new-secret"),
                    reparent_test_session("child001", Some("oldpar01"), "child-secret")
                ]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone());
        let request = match store
            .create_reparent_request(
                "child001",
                CreateReparentRequest {
                    requester_session_id: "oldpar01".to_owned(),
                    target_parent_session_id: "newpar01".to_owned(),
                },
                "old-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };

        store.reconcile_reparent_notifications().unwrap();
        store.reconcile_reparent_notifications().unwrap();
        let pending = queue.pending_messages_for_target("newpar01", 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0]
            .text
            .contains(&format!("Approve: `sm reparent approve {}`", request.id)));
        assert!(pending[0]
            .text
            .contains(&format!("Reject: `sm reparent reject {}`", request.id)));
        assert!(pending[0].text.contains("child001: oldpar01 -> newpar01"));
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        let intents = state["reparent_requests"][0]["notification_intents"]
            .as_array()
            .unwrap();
        let target_intents = intents
            .iter()
            .filter(|intent| intent["recipient_session_id"] == "newpar01")
            .collect::<Vec<_>>();
        assert_eq!(target_intents.len(), 1);
        assert!(target_intents[0]["enqueued_at"].is_string());

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[cfg(unix)]
    #[test]
    fn reparent_notifications_retry_pending_codex_fork_control_delivery_without_tmux_fallback() {
        use std::os::unix::net::UnixListener;

        let root = env::temp_dir().join(format!("sm-rp-{}", generate_session_id()));
        fs::create_dir_all(&root).unwrap();
        let state_file = root.join("sessions.json");
        let queue_db = root.join("messages.db");
        let new_parent_id = generate_session_id();
        let log_file = env::temp_dir().join(format!("sm-rp-{new_parent_id}.log"));
        let mut new_parent = reparent_test_session(&new_parent_id, None, "new-secret");
        new_parent["provider"] = json!("codex-fork");
        new_parent["log_file"] = json!(log_file.display().to_string());
        new_parent["error_message"] = json!("codex_fork_control_degraded: prior outage");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("oldpar01", None, "old-secret"),
                    new_parent,
                    reparent_test_session("child001", Some("oldpar01"), "child-secret")
                ]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        let mut config = crate::config::AppConfig::default();
        config.codex_fork.control_tmux_fallback_enabled = false;
        let runtime = TmuxRuntime::from_app_config(&config);
        let control_socket = runtime
            .codex_fork_runtime_artifacts(&TmuxSessionSpec {
                session_id: new_parent_id.clone(),
                session_credential: None,
                tmux_session: format!("claude-{new_parent_id}"),
                working_dir: "/repo".to_owned(),
                log_file: log_file.clone(),
                provider: "codex-fork".to_owned(),
                initial_message: None,
                force_initial_prompt_stdin: false,
                model: None,
                reasoning_effort: None,
            })
            .unwrap()
            .unwrap()
            .control_socket_path;
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone())
            .with_delivery_runtime(Some(runtime));
        let request = match store
            .create_reparent_request(
                "child001",
                CreateReparentRequest {
                    requester_session_id: "oldpar01".to_owned(),
                    target_parent_session_id: new_parent_id.clone(),
                },
                "old-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };

        // The managed control socket is unavailable. Because terminal-session
        // fallback is disabled, the durable notice remains pending.
        store.reconcile_reparent_notifications().unwrap();
        assert_eq!(
            queue
                .pending_messages_for_target_by_category(&new_parent_id, "reparent", 10)
                .unwrap()
                .len(),
            1
        );

        let listener = UnixListener::bind(&control_socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            while requests.len() < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut raw_request = String::new();
                        BufReader::new(stream.try_clone().unwrap())
                            .read_line(&mut raw_request)
                            .unwrap();
                        let request: Value = serde_json::from_str(&raw_request).unwrap();
                        let response = if requests.is_empty() {
                            json!({
                                "ok": true,
                                "epoch": "epoch-1",
                                "result": { "epoch": "epoch-1" }
                            })
                        } else {
                            json!({ "ok": true, "epoch": "epoch-1", "result": {} })
                        };
                        writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
                        requests.push(request);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("control listener failed: {error}"),
                }
            }
            requests
        });

        // The request already has an enqueued intent, so this second pass is a
        // retry of the retained row rather than a fresh notification.
        store.reconcile_reparent_notifications().unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["command"], "get_epoch");
        assert_eq!(requests[1]["command"], "submit_message");
        assert!(requests[1]["message"]
            .as_str()
            .unwrap()
            .contains(&format!("sm reparent approve {}", request.id)));
        assert!(queue
            .pending_messages_for_target_by_category(&new_parent_id, "reparent", 10)
            .unwrap()
            .is_empty());
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        assert!(state["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == new_parent_id)
            .unwrap()["error_message"]
            .is_null());

        let _ = fs::remove_file(control_socket);
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn concurrent_approve_recovery_and_restart_emit_one_truthful_terminal_notification() {
        // Reproduce the production interleaving: two independently-created
        // stores (an approve handler and a recovering process) see one ready
        // request.  Before the cross-process transaction lock, one could
        // finish `applied` while the other persisted/enqueued `failed` after
        // observing the released lease.
        let (approve_store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-concurrent-terminal");
        let recovery_store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone());
        let start = Arc::new(Barrier::new(2));

        let approve_start = Arc::clone(&start);
        let approve = thread::spawn(move || {
            approve_start.wait();
            approve_store.reconcile_reparent_requests()
        });
        let recovery_start = Arc::clone(&start);
        let recovery = thread::spawn(move || {
            recovery_start.wait();
            recovery_store.reconcile_reparent_requests()
        });

        let approve = approve.join().unwrap().unwrap();
        let recovery = recovery.join().unwrap().unwrap();
        assert!([approve, recovery]
            .into_iter()
            .flatten()
            .all(|record| record.status == "applied"));

        // A fresh store represents restart recovery and must project only the
        // committed topology's terminal state, even when reconciliation is
        // retried.
        let restarted = SessionStore::new_with_queue(state_file.clone(), queue_db.clone());
        restarted.reconcile_reparent_notifications().unwrap();
        restarted.reconcile_reparent_notifications().unwrap();

        let record = restarted
            .get_reparent_request(&request_id)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, "applied");
        assert_eq!(record.apply_stage.as_deref(), Some("applied"));
        assert_eq!(
            restarted
                .get_session("child001")
                .unwrap()
                .unwrap()
                .parent_session_id
                .as_deref(),
            Some("newpar01")
        );
        for recipient in ["oldpar01", "newpar01"] {
            let messages = queue.pending_messages_for_target(recipient, 10).unwrap();
            assert_eq!(messages.len(), 1, "recipient {recipient}");
            assert!(messages[0]
                .text
                .contains(&format!("Request {request_id} is applied.")));
            assert!(!messages[0].text.contains(" is failed."));
        }
        assert!(record.notification_intents.iter().all(|intent| {
            !intent.event.starts_with("terminal:") || intent.event == "terminal:applied"
        }));

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn notification_retry_discards_an_unsent_losing_failed_terminal_projection() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-losing-failed-notification");
        store.reconcile_reparent_requests().unwrap();
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let mut records = reparent_request_records(&state).unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id == request_id)
                .unwrap();
            record
                .notification_intents
                .push(ReparentNotificationIntent {
                    key: format!("reparent:{request_id}:terminal:failed:oldpar01"),
                    event: "terminal:failed".to_owned(),
                    recipient_session_id: "oldpar01".to_owned(),
                    enqueued_at: None,
                });
            store_reparent_request_records(&mut state, &records).unwrap();
            store.write_raw_json_value(&state).unwrap();
        }

        store.reconcile_reparent_notifications().unwrap();
        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert!(record.notification_intents.iter().all(|intent| {
            intent.enqueued_at.is_some()
                || !intent.event.starts_with("terminal:")
                || intent.event == "terminal:applied"
        }));
        let messages = queue.pending_messages_for_target("oldpar01", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].text.contains(" is applied."));
        assert!(!messages[0].text.contains(" is failed."));

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn approved_tree_reparent_splits_queue_routes_across_each_new_parent() {
        let state_file = unique_temp_path("reparent-tree-routing");
        let queue_db = state_file.with_extension("db");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("grand001", None, "grand-secret"),
                    reparent_test_session("source01", Some("grand001"), "source-secret"),
                    reparent_test_session("target01", Some("source01"), "target-secret"),
                    reparent_test_session("sibling1", Some("source01"), "sibling-secret")
                ]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        let mut message_ids = BTreeMap::new();
        for (child, parent) in [
            ("source01", "grand001"),
            ("target01", "source01"),
            ("sibling1", "source01"),
        ] {
            queue.register_parent_wake(child, parent, 600).unwrap();
            let message_id = queue
                .enqueue_message_with_metadata(
                    child,
                    &format!("pending for {child}"),
                    "important",
                    QueueMessageMetadata {
                        parent_session_id: Some(parent.to_owned()),
                        ..QueueMessageMetadata::default()
                    },
                )
                .unwrap();
            message_ids.insert(child, message_id);
        }
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone());
        let request = match store
            .create_reparent_tree_request(
                "source01",
                CreateReparentTreeRequest {
                    requester_session_id: "source01".to_owned(),
                    target_session_id: "target01".to_owned(),
                    dry_run: false,
                },
                "source-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        let pending = match store
            .decide_reparent_request(
                &request.id,
                DecideReparentRequest {
                    requester_session_id: "target01".to_owned(),
                },
                ReparentDecision::Approved,
                "target-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Updated(record) => record,
            other => panic!("unexpected target approval outcome: {other:?}"),
        };
        assert_eq!(pending.status, "pending");
        let applied = match store
            .decide_reparent_request(
                &request.id,
                DecideReparentRequest {
                    requester_session_id: "grand001".to_owned(),
                },
                ReparentDecision::Approved,
                "grand-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Updated(record) => record,
            other => panic!("unexpected grandparent approval outcome: {other:?}"),
        };
        assert_eq!(applied.status, "applied");

        for (child, parent) in [
            ("source01", "target01"),
            ("target01", "grand001"),
            ("sibling1", "target01"),
        ] {
            assert_eq!(
                store
                    .get_session(child)
                    .unwrap()
                    .unwrap()
                    .parent_session_id
                    .as_deref(),
                Some(parent)
            );
            assert_eq!(
                queue.active_parent_wake_parent(child).unwrap().as_deref(),
                Some(parent)
            );
            let pending = queue.pending_messages_for_target(child, 10).unwrap();
            assert_eq!(
                pending
                    .iter()
                    .find(|message| message.id == message_ids[child])
                    .and_then(|message| message.parent_session_id.as_deref()),
                Some(parent)
            );
        }

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn failed_tree_reparent_restores_every_queue_route_before_releasing_lease() {
        let state_file = unique_temp_path("reparent-tree-rollback");
        let queue_db = state_file.with_extension("db");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("grand001", None, "grand-secret"),
                    reparent_test_session("source01", Some("grand001"), "source-secret"),
                    reparent_test_session("target01", Some("source01"), "target-secret"),
                    reparent_test_session("sibling1", Some("source01"), "sibling-secret")
                ]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        for (child, parent) in [
            ("source01", "grand001"),
            ("target01", "source01"),
            ("sibling1", "source01"),
        ] {
            queue.register_parent_wake(child, parent, 600).unwrap();
            queue
                .enqueue_message_with_metadata(
                    child,
                    &format!("pending for {child}"),
                    "important",
                    QueueMessageMetadata {
                        parent_session_id: Some(parent.to_owned()),
                        ..QueueMessageMetadata::default()
                    },
                )
                .unwrap();
        }
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone());
        let request = match store
            .create_reparent_tree_request(
                "source01",
                CreateReparentTreeRequest {
                    requester_session_id: "source01".to_owned(),
                    target_session_id: "target01".to_owned(),
                    dry_run: false,
                },
                "source-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let mut records = reparent_request_records(&state).unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id == request.id)
                .unwrap();
            for actor_id in ["target01", "grand001"] {
                record.approvals.push(ReparentApprovalRecord {
                    actor_kind: "agent".to_owned(),
                    actor_id: actor_id.to_owned(),
                    decision: "approved".to_owned(),
                    decided_at: now_rfc3339(),
                });
            }
            record.ready_to_apply = true;
            store_reparent_request_records(&mut state, &records).unwrap();
            store.write_raw_json_value(&state).unwrap();
        }
        assert_eq!(
            store.acquire_reparent_apply_lease().unwrap().as_deref(),
            Some(request.id.as_str())
        );
        store.quiesce_reparent_json_routing(&request.id).unwrap();
        store.quiesce_reparent_queue_routing(&request.id).unwrap();
        store
            .fail_reparent_apply(&request.id, "injected tree precommit failure")
            .unwrap();
        let repaired = match store
            .repair_reparent_request(
                &request.id,
                "owner@example.com",
                ReparentRepairAction::RollbackPrecommit,
            )
            .unwrap()
        {
            ReparentMutationOutcome::Updated(record) => record,
            other => panic!("unexpected repair outcome: {other:?}"),
        };
        assert_eq!(repaired.status, "repaired");
        assert!(repaired.repair_history[0]
            .verified_state_fingerprint
            .is_some());
        for (child, parent) in [
            ("source01", "grand001"),
            ("target01", "source01"),
            ("sibling1", "source01"),
        ] {
            assert_eq!(
                store
                    .get_session(child)
                    .unwrap()
                    .unwrap()
                    .parent_session_id
                    .as_deref(),
                Some(parent)
            );
            assert_eq!(
                queue.active_parent_wake_parent(child).unwrap().as_deref(),
                Some(parent)
            );
            assert_eq!(
                queue.pending_messages_for_target(child, 10).unwrap()[0]
                    .parent_session_id
                    .as_deref(),
                Some(parent)
            );
        }
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        assert!(state["reparent_apply_lease"].is_null());

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn legacy_context_monitor_provenance_defaults_to_explicit() {
        let value = reparent_test_session("legacy01", None, "legacy-secret");
        let record: SessionRecord = serde_json::from_value(value).unwrap();
        assert_eq!(record.context_monitor_notify_source, "explicit");
    }

    #[test]
    fn failed_precommit_reparent_rolls_back_exact_routes_and_releases_lease() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-rollback");
        assert_eq!(
            store.acquire_reparent_apply_lease().unwrap().as_deref(),
            Some(request_id.as_str())
        );
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();
        store
            .fail_reparent_apply(&request_id, "injected precommit failure")
            .unwrap();

        let failed = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.apply_stage.as_deref(), Some("routing_quiesced"));
        assert_eq!(queue.active_parent_wake_parent("child001").unwrap(), None);

        let repaired = match store
            .repair_reparent_request(
                &request_id,
                "owner@example.com",
                ReparentRepairAction::RollbackPrecommit,
            )
            .unwrap()
        {
            ReparentMutationOutcome::Updated(record) => record,
            other => panic!("unexpected repair outcome: {other:?}"),
        };
        assert_eq!(repaired.status, "repaired");
        assert_eq!(repaired.apply_stage.as_deref(), Some("repair_rolled_back"));
        assert_eq!(repaired.repair_history.len(), 1);
        assert!(repaired.repair_history[0]
            .verified_state_fingerprint
            .is_some());
        assert_ne!(
            repaired.repair_history[0]
                .verified_state_fingerprint
                .as_deref(),
            Some(repaired.topology_fingerprint.as_str())
        );
        assert_eq!(
            queue
                .active_parent_wake_parent("child001")
                .unwrap()
                .as_deref(),
            Some("oldpar01")
        );
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        assert!(state["reparent_apply_lease"].is_null());
        assert_eq!(
            state["retained_parent_wake_registrations"][0]["parent_session_id"],
            "oldpar01"
        );
        assert_eq!(
            state["retained_parent_wake_registrations"][0]["is_active"],
            true
        );
        assert_eq!(
            json_text(
                raw_session_object(&state, "child001")
                    .unwrap()
                    .get("parent_session_id")
            )
            .as_deref(),
            Some("oldpar01")
        );
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn parentless_precommit_reparent_rolls_back_without_stranding_the_lease() {
        let state_file = unique_temp_path("reparent-parentless-rollback");
        let queue_db = state_file.with_extension("db");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("newpar01", None, "new-parent-secret"),
                    reparent_test_session("child001", None, "child-secret")
                ]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone());
        let request = match store
            .create_reparent_request(
                "child001",
                CreateReparentRequest {
                    requester_session_id: "newpar01".to_owned(),
                    target_parent_session_id: "newpar01".to_owned(),
                },
                "new-parent-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let mut records = reparent_request_records(&state).unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id == request.id)
                .unwrap();
            record.approvals.push(ReparentApprovalRecord {
                actor_kind: "human".to_owned(),
                actor_id: "owner@example.com".to_owned(),
                decision: "approved".to_owned(),
                decided_at: now_rfc3339(),
            });
            record.ready_to_apply = true;
            store_reparent_request_records(&mut state, &records).unwrap();
            store.write_raw_json_value(&state).unwrap();
        }

        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request.id).unwrap();
        store.quiesce_reparent_queue_routing(&request.id).unwrap();
        store
            .send_parent_notification(
                "child001",
                "Child child001 completed: Session exited",
                "sequential",
                "child_wait",
                None,
            )
            .unwrap();
        store
            .fail_reparent_apply(&request.id, "injected parentless failure")
            .unwrap();
        let repaired = store
            .repair_reparent_request(
                &request.id,
                "owner@example.com",
                ReparentRepairAction::RollbackPrecommit,
            )
            .unwrap();
        assert!(matches!(repaired, ReparentMutationOutcome::Updated(_)));

        let state = store.load_raw_json_value().unwrap();
        assert!(state["reparent_apply_lease"].is_null());
        assert!(raw_session_object(&state, "child001")
            .unwrap()
            .get("parent_session_id")
            .is_none_or(Value::is_null));
        let record = store.get_reparent_request(&request.id).unwrap().unwrap();
        assert_eq!(record.status, "repaired");
        assert_eq!(record.apply_stage.as_deref(), Some("repair_rolled_back"));
        assert_eq!(record.deferred_routing_intents.len(), 1);
        assert!(record.deferred_routing_intents[0].replayed_at.is_some());
        assert!(record.deferred_routing_intents[0]
            .resolved_parent_session_id
            .is_none());
        assert!(queue
            .pending_messages_for_target("newpar01", 10)
            .unwrap()
            .is_empty());

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn authority_committed_reparent_resumes_forward_from_immutable_plan() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-forward-recovery");
        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();
        store.commit_reparent_authority(&request_id).unwrap();

        let recovered = store.reconcile_reparent_requests().unwrap().unwrap();
        assert_eq!(recovered.status, "applied");
        assert_eq!(
            store
                .get_session("child001")
                .unwrap()
                .unwrap()
                .parent_session_id
                .as_deref(),
            Some("newpar01")
        );
        assert_eq!(
            queue
                .active_parent_wake_parent("child001")
                .unwrap()
                .as_deref(),
            Some("newpar01")
        );
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        assert!(state["reparent_apply_lease"].is_null());
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn concurrent_reparent_reconcilers_return_committed_state_without_losing_the_lease() {
        let (store, _queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-concurrent-reconcile");
        let store = std::sync::Arc::new(store);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.reconcile_reparent_requests()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for worker in workers {
            assert!(worker.join().unwrap().is_ok());
        }

        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert_eq!(record.status, "applied");
        assert_eq!(record.apply_stage.as_deref(), Some("applied"));
        let state = store.load_raw_json_value().unwrap();
        assert!(state["reparent_apply_lease"].is_null());

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn stopped_root_recovery_has_exact_preview_apply_parity_and_post_state() {
        let (store, state_file) = stopped_root_recovery_store("exact-parity");
        let preview = match store
            .create_reparent_tree_request(
                "outgoing",
                CreateReparentTreeRequest {
                    requester_session_id: "successor".to_owned(),
                    target_session_id: "successor".to_owned(),
                    dry_run: true,
                },
                "successor-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Preview(preview) => preview,
            other => panic!("unexpected preview outcome: {other:?}"),
        };
        assert!(preview.stopped_root_recovery);
        assert!(preview.blockers.is_empty());
        assert_eq!(
            preview.edge_changes,
            vec![
                ReparentEdgeChange {
                    session_id: "successor".to_owned(),
                    expected_parent_session_id: Some("maintainer".to_owned()),
                    new_parent_session_id: None,
                },
                ReparentEdgeChange {
                    session_id: "outgoing".to_owned(),
                    expected_parent_session_id: None,
                    new_parent_session_id: Some("successor".to_owned()),
                },
                ReparentEdgeChange {
                    session_id: "worker-a".to_owned(),
                    expected_parent_session_id: Some("outgoing".to_owned()),
                    new_parent_session_id: Some("successor".to_owned()),
                },
                ReparentEdgeChange {
                    session_id: "worker-b".to_owned(),
                    expected_parent_session_id: Some("outgoing".to_owned()),
                    new_parent_session_id: Some("successor".to_owned()),
                },
            ]
        );

        let request = match store
            .create_reparent_tree_request(
                "outgoing",
                CreateReparentTreeRequest {
                    requester_session_id: "successor".to_owned(),
                    target_session_id: "successor".to_owned(),
                    dry_run: false,
                },
                "successor-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        assert!(request.stopped_root_recovery);
        assert_eq!(
            request.expected_target_parent_session_id.as_deref(),
            Some("maintainer")
        );
        assert_eq!(
            request.required_agent_approvals,
            vec!["maintainer", "successor"]
        );
        assert!(!request.ready_to_apply);
        let applied = store
            .decide_reparent_request(
                &request.id,
                DecideReparentRequest {
                    requester_session_id: "maintainer".to_owned(),
                },
                ReparentDecision::Approved,
                "maintainer-secret",
            )
            .unwrap();
        let ReparentMutationOutcome::Updated(applied) = applied else {
            panic!("unexpected approval outcome");
        };
        assert_eq!(applied.status, "applied");
        assert_eq!(
            applied.apply_plan.unwrap().edge_changes,
            preview.edge_changes
        );
        assert_eq!(
            store
                .get_session("successor")
                .unwrap()
                .unwrap()
                .parent_session_id,
            None
        );
        for session_id in ["outgoing", "worker-a", "worker-b"] {
            assert_eq!(
                store
                    .get_session(session_id)
                    .unwrap()
                    .unwrap()
                    .parent_session_id
                    .as_deref(),
                Some("successor")
            );
        }
        assert_eq!(
            store
                .get_session("stopped-worker")
                .unwrap()
                .unwrap()
                .parent_session_id
                .as_deref(),
            Some("outgoing")
        );
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn stopped_root_recovery_fails_closed_for_changed_children_and_pending_conflict() {
        let (store, state_file) = stopped_root_recovery_store("stale-and-conflict");
        let request = match store
            .create_reparent_tree_request(
                "outgoing",
                CreateReparentTreeRequest {
                    requester_session_id: "successor".to_owned(),
                    target_session_id: "successor".to_owned(),
                    dry_run: false,
                },
                "successor-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        assert!(matches!(
            store
                .create_reparent_tree_request(
                    "outgoing",
                    CreateReparentTreeRequest {
                        requester_session_id: "successor".to_owned(),
                        target_session_id: "successor".to_owned(),
                        dry_run: false,
                    },
                    "successor-secret",
                )
                .unwrap(),
            ReparentMutationOutcome::Conflict(_)
        ));
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            ensure_sessions_array_mut(&mut state)
                .unwrap()
                .push(reparent_test_session(
                    "late-worker",
                    Some("outgoing"),
                    "late-secret",
                ));
            store.write_raw_json_value(&state).unwrap();
        }
        let stale = store.get_reparent_request(&request.id).unwrap().unwrap();
        assert_eq!(stale.status, "stale");
        assert_eq!(
            store
                .get_session("outgoing")
                .unwrap()
                .unwrap()
                .parent_session_id,
            None
        );
        for session_id in ["worker-a", "worker-b", "late-worker"] {
            assert_eq!(
                store
                    .get_session(session_id)
                    .unwrap()
                    .unwrap()
                    .parent_session_id
                    .as_deref(),
                Some("outgoing")
            );
        }
        assert_eq!(
            store
                .get_session("successor")
                .unwrap()
                .unwrap()
                .parent_session_id
                .as_deref(),
            Some("maintainer")
        );
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn stopped_root_recovery_stales_on_maintainer_reassignment_without_edges() {
        let (store, state_file) = stopped_root_recovery_store("maintainer-reassignment");
        let request = match store
            .create_reparent_tree_request(
                "outgoing",
                CreateReparentTreeRequest {
                    requester_session_id: "successor".to_owned(),
                    target_session_id: "successor".to_owned(),
                    dry_run: false,
                },
                "successor-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            state["agent_registrations"] = json!([{
                "role": "maintainer",
                "session_id": "replacement-maintainer",
                "created_at": now_rfc3339(),
            }]);
            store.write_raw_json_value(&state).unwrap();
        }
        let stale = store.get_reparent_request(&request.id).unwrap().unwrap();
        assert_eq!(stale.status, "stale");
        assert_eq!(
            stale.failure_reason.as_deref(),
            Some("durable maintainer registration changed after request creation")
        );
        assert_eq!(
            store
                .get_session("successor")
                .unwrap()
                .unwrap()
                .parent_session_id
                .as_deref(),
            Some("maintainer")
        );
        assert_eq!(
            store
                .get_session("outgoing")
                .unwrap()
                .unwrap()
                .parent_session_id,
            None
        );
        for session_id in ["worker-a", "worker-b"] {
            assert_eq!(
                store
                    .get_session(session_id)
                    .unwrap()
                    .unwrap()
                    .parent_session_id
                    .as_deref(),
                Some("outgoing")
            );
        }
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn stopped_root_recovery_rejects_live_source_terminal_target_or_target_children() {
        let (store, state_file) = stopped_root_recovery_store("hostile-eligibility");
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            session_object_mut(ensure_sessions_array_mut(&mut state).unwrap(), "outgoing")
                .unwrap()
                .insert("status".to_owned(), Value::String("idle".to_owned()));
            store.write_raw_json_value(&state).unwrap();
        }
        assert!(matches!(
            store
                .create_reparent_tree_request(
                    "outgoing",
                    CreateReparentTreeRequest {
                        requester_session_id: "successor".to_owned(),
                        target_session_id: "successor".to_owned(),
                        dry_run: true,
                    },
                    "successor-secret",
                )
                .unwrap(),
            ReparentMutationOutcome::BadRequest(_)
        ));
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            session_object_mut(ensure_sessions_array_mut(&mut state).unwrap(), "outgoing")
                .unwrap()
                .insert("status".to_owned(), Value::String("stopped".to_owned()));
            session_object_mut(ensure_sessions_array_mut(&mut state).unwrap(), "successor")
                .unwrap()
                .insert("status".to_owned(), Value::String("stopped".to_owned()));
            store.write_raw_json_value(&state).unwrap();
        }
        assert!(matches!(
            store
                .create_reparent_tree_request(
                    "outgoing",
                    CreateReparentTreeRequest {
                        requester_session_id: "successor".to_owned(),
                        target_session_id: "successor".to_owned(),
                        dry_run: false,
                    },
                    "successor-secret",
                )
                .unwrap(),
            ReparentMutationOutcome::BadRequest(_)
        ));
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let sessions = ensure_sessions_array_mut(&mut state).unwrap();
            session_object_mut(sessions, "successor")
                .unwrap()
                .insert("status".to_owned(), Value::String("idle".to_owned()));
            sessions.push(reparent_test_session(
                "successor-child",
                Some("successor"),
                "successor-child-secret",
            ));
            store.write_raw_json_value(&state).unwrap();
        }
        assert!(matches!(
            store
                .create_reparent_tree_request(
                    "outgoing",
                    CreateReparentTreeRequest {
                        requester_session_id: "successor".to_owned(),
                        target_session_id: "successor".to_owned(),
                        dry_run: false,
                    },
                    "successor-secret",
                )
                .unwrap(),
            ReparentMutationOutcome::BadRequest(_)
        ));
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn stopped_root_recovery_restarts_idempotently_from_a_persisted_stage() {
        let (store, state_file) = stopped_root_recovery_store("restart");
        let request = match store
            .create_reparent_tree_request(
                "outgoing",
                CreateReparentTreeRequest {
                    requester_session_id: "successor".to_owned(),
                    target_session_id: "successor".to_owned(),
                    dry_run: false,
                },
                "successor-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let mut records = reparent_request_records(&state).unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id == request.id)
                .unwrap();
            record.approvals.push(ReparentApprovalRecord {
                actor_kind: "agent".to_owned(),
                actor_id: "maintainer".to_owned(),
                decision: "approved".to_owned(),
                decided_at: now_rfc3339(),
            });
            record.ready_to_apply = true;
            store_reparent_request_records(&mut state, &records).unwrap();
            store.write_raw_json_value(&state).unwrap();
        }
        assert_eq!(
            store.acquire_reparent_apply_lease().unwrap().as_deref(),
            Some(request.id.as_str())
        );
        store.quiesce_reparent_json_routing(&request.id).unwrap();
        drop(store);

        let recovered = SessionStore::new(state_file.clone())
            .reconcile_reparent_requests()
            .unwrap()
            .unwrap();
        assert_eq!(recovered.status, "applied");
        let final_store = SessionStore::new(state_file.clone());
        assert_eq!(
            final_store
                .get_session("outgoing")
                .unwrap()
                .unwrap()
                .parent_session_id
                .as_deref(),
            Some("successor")
        );
        assert_eq!(
            final_store
                .get_session("successor")
                .unwrap()
                .unwrap()
                .parent_session_id,
            None
        );
        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn peer_root_succession_recovers_from_a_persisted_apply_stage_after_restart() {
        let state_file = unique_temp_path("peer-root-restart");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("outgoing", None, "outgoing-secret"),
                    reparent_test_session("successor", None, "successor-secret"),
                    reparent_test_session("worker", Some("outgoing"), "worker-secret")
                ]
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore::new(state_file.clone());
        let request = match store
            .create_reparent_tree_request(
                "outgoing",
                CreateReparentTreeRequest {
                    requester_session_id: "outgoing".to_owned(),
                    target_session_id: "successor".to_owned(),
                    dry_run: false,
                },
                "outgoing-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        assert!(request.peer_root_succession);
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let mut records = reparent_request_records(&state).unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id == request.id)
                .unwrap();
            record.approvals.push(ReparentApprovalRecord {
                actor_kind: "agent".to_owned(),
                actor_id: "successor".to_owned(),
                decision: "approved".to_owned(),
                decided_at: now_rfc3339(),
            });
            record.ready_to_apply = true;
            store_reparent_request_records(&mut state, &records).unwrap();
            store.write_raw_json_value(&state).unwrap();
        }
        assert_eq!(
            store.acquire_reparent_apply_lease().unwrap().as_deref(),
            Some(request.id.as_str())
        );
        store.quiesce_reparent_json_routing(&request.id).unwrap();
        drop(store);

        let recovered = SessionStore::new(state_file.clone())
            .reconcile_reparent_requests()
            .unwrap()
            .unwrap();
        assert_eq!(recovered.status, "applied");
        let final_store = SessionStore::new(state_file.clone());
        assert_eq!(
            final_store
                .get_session("successor")
                .unwrap()
                .unwrap()
                .parent_session_id,
            None
        );
        for session_id in ["outgoing", "worker"] {
            assert_eq!(
                final_store
                    .get_session(session_id)
                    .unwrap()
                    .unwrap()
                    .parent_session_id
                    .as_deref(),
                Some("successor")
            );
        }
        let state = final_store.load_raw_json_value().unwrap();
        assert!(state["reparent_apply_lease"].is_null());

        let _ = fs::remove_file(state_file);
    }

    #[test]
    fn task_complete_during_reparent_notifies_only_the_committed_parent() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-task-complete");
        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();

        let completed = store
            .task_complete(
                "child001",
                TaskCompleteRequest {
                    requester_session_id: "child001".to_owned(),
                },
                None,
            )
            .unwrap();
        assert!(matches!(
            completed,
            TaskCompleteOutcome::Completed(TaskCompleteResult {
                em_notified: false,
                ..
            })
        ));
        assert!(queue
            .pending_messages_for_target("oldpar01", 10)
            .unwrap()
            .is_empty());

        store.commit_reparent_authority(&request_id).unwrap();
        store.finish_reparent_routing(&request_id).unwrap();

        let messages = queue.pending_messages_for_target("newpar01", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].message_category.as_deref(),
            Some("task_complete")
        );
        assert!(queue
            .active_parent_wake_parent("child001")
            .unwrap()
            .is_none());
        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert_eq!(record.status, "applied");
        assert_eq!(record.deferred_routing_intents.len(), 1);
        assert!(record.deferred_routing_intents[0].replayed_at.is_some());
        assert_eq!(
            record.deferred_routing_intents[0]
                .resolved_parent_session_id
                .as_deref(),
            Some("newpar01")
        );

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn task_complete_for_an_affected_parent_is_not_deferred_as_a_changed_edge() {
        let (store, _queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-parent-task-complete");
        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();

        store
            .task_complete(
                "newpar01",
                TaskCompleteRequest {
                    requester_session_id: "newpar01".to_owned(),
                },
                None,
            )
            .unwrap();

        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert!(record.deferred_routing_intents.is_empty());
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn parent_derived_input_created_during_reparent_replays_with_new_parent() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-parent-input");
        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();

        let outcome = store
            .send_core_input_with_runtime(
                "child001",
                SendCoreInputRequest {
                    text: "queued during reparent".to_owned(),
                    delivery_mode: "important".to_owned(),
                    sender_session_id: None,
                    from_sm_send: false,
                    timeout_seconds: None,
                    notify_on_delivery: true,
                    notify_after_seconds: Some(15),
                    notify_on_stop: true,
                    remind_soft_threshold: Some(600),
                    remind_hard_threshold: None,
                    remind_cancel_on_reply_session_id: None,
                    parent_session_id: Some("oldpar01".to_owned()),
                },
                &TmuxRuntime::from_config(&crate::config::RustCoreConfig::default()),
            )
            .unwrap()
            .unwrap();
        assert!(!outcome.delivered);
        assert!(queue
            .pending_messages_for_target("child001", 10)
            .unwrap()
            .is_empty());

        store.commit_reparent_authority(&request_id).unwrap();
        store.finish_reparent_routing(&request_id).unwrap();

        let messages = queue.pending_messages_for_target("child001", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].parent_session_id.as_deref(), Some("newpar01"));
        assert!(!messages[0].notify_on_delivery);
        assert_eq!(messages[0].notify_after_seconds, None);
        assert!(!messages[0].notify_on_stop);
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn compaction_during_reparent_notifies_only_the_committed_parent() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-compaction");
        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();

        assert_eq!(
            store
                .apply_context_usage_event(&lifecycle_event("child001", "compaction"), None)
                .unwrap(),
            ContextUsageOutcome::CompactionLogged
        );
        assert!(queue
            .pending_messages_for_target("oldpar01", 10)
            .unwrap()
            .is_empty());
        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert_eq!(record.deferred_routing_intents.len(), 1);
        assert_eq!(
            record.deferred_routing_intents[0].operation,
            "parent_message"
        );

        store.commit_reparent_authority(&request_id).unwrap();
        store.finish_reparent_routing(&request_id).unwrap();

        let messages = queue.pending_messages_for_target("newpar01", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].message_category.as_deref(),
            Some("context_monitor")
        );
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn explicit_compaction_target_is_not_retargeted_during_reparent() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-explicit-compaction");
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let child =
                session_object_mut(ensure_sessions_array_mut(&mut state).unwrap(), "child001")
                    .unwrap();
            child.insert(
                "context_monitor_notify".to_owned(),
                Value::String("oldpar01".to_owned()),
            );
            child.insert(
                "context_monitor_notify_source".to_owned(),
                Value::String("explicit".to_owned()),
            );
            store.write_raw_json_value(&state).unwrap();
        }
        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();

        store
            .apply_context_usage_event(&lifecycle_event("child001", "compaction"), None)
            .unwrap();

        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert!(record.deferred_routing_intents.is_empty());
        let messages = queue.pending_messages_for_target("oldpar01", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].message_category.as_deref(),
            Some("context_monitor")
        );
        assert!(queue
            .pending_messages_for_target("newpar01", 10)
            .unwrap()
            .is_empty());

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn child_wait_notification_resolves_parent_after_reparent_commit() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-child-wait");
        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();

        store
            .send_parent_notification(
                "child001",
                "Child child001 completed: Session exited",
                "sequential",
                "child_wait",
                None,
            )
            .unwrap();
        assert!(queue
            .pending_messages_for_target("oldpar01", 10)
            .unwrap()
            .is_empty());

        store.commit_reparent_authority(&request_id).unwrap();
        store.finish_reparent_routing(&request_id).unwrap();

        let messages = queue.pending_messages_for_target("newpar01", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_category.as_deref(), Some("child_wait"));
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn stale_after_routing_quiesce_rolls_back_without_human_repair() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-stale-rollback");
        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let sessions = ensure_sessions_array_mut(&mut state).unwrap();
            session_object_mut(sessions, "newpar01")
                .unwrap()
                .insert("status".to_owned(), Value::String("stopped".to_owned()));
            store.write_raw_json_value(&state).unwrap();
        }

        store.apply_reparent_request(&request_id).unwrap();

        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert_eq!(record.status, "stale");
        assert_eq!(record.apply_stage.as_deref(), Some("prequiesce_aborted"));
        assert!(record.failure_reason.is_some());
        assert_eq!(
            store
                .get_session("child001")
                .unwrap()
                .unwrap()
                .parent_session_id
                .as_deref(),
            Some("oldpar01")
        );
        assert_eq!(
            queue
                .active_parent_wake_parent("child001")
                .unwrap()
                .as_deref(),
            Some("oldpar01")
        );
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        assert!(state["reparent_apply_lease"].is_null());
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn prequiesce_abort_replays_deferred_completion_before_releasing_lease() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-prequiesce-abort");
        store.acquire_reparent_apply_lease().unwrap();
        store
            .task_complete(
                "child001",
                TaskCompleteRequest {
                    requester_session_id: "child001".to_owned(),
                },
                None,
            )
            .unwrap();
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let sessions = ensure_sessions_array_mut(&mut state).unwrap();
            session_object_mut(sessions, "newpar01")
                .unwrap()
                .insert("status".to_owned(), Value::String("stopped".to_owned()));
            store.write_raw_json_value(&state).unwrap();
        }

        let error = store.apply_reparent_request(&request_id).unwrap_err();
        store
            .fail_reparent_apply(&request_id, &error.to_string())
            .unwrap();

        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert_eq!(record.status, "stale");
        assert!(record.deferred_routing_intents[0].replayed_at.is_some());
        assert_eq!(
            record.deferred_routing_intents[0]
                .resolved_parent_session_id
                .as_deref(),
            Some("oldpar01")
        );
        let messages = queue.pending_messages_for_target("oldpar01", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].message_category.as_deref(),
            Some("task_complete")
        );
        let state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        assert!(state["reparent_apply_lease"].is_null());
        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn delivered_planned_message_recreates_its_parent_wake_after_commit() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-delivered-message");
        let message_id = queue
            .enqueue_message_with_metadata(
                "child001",
                "delivery races reparent",
                "important",
                QueueMessageMetadata {
                    remind_soft_threshold: Some(600),
                    parent_session_id: Some("oldpar01".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        store.acquire_reparent_apply_lease().unwrap();
        store.quiesce_reparent_json_routing(&request_id).unwrap();
        store.quiesce_reparent_queue_routing(&request_id).unwrap();
        let message = queue
            .pending_messages_for_target("child001", 10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == message_id)
            .unwrap();
        assert!(message.parent_session_id.is_none());
        queue
            .mark_delivered_and_apply_side_effects(&message)
            .unwrap();
        store.commit_reparent_authority(&request_id).unwrap();
        store.finish_reparent_routing(&request_id).unwrap();

        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        let intent = record
            .deferred_routing_intents
            .iter()
            .find(|intent| intent.key == format!("delivered-planned-parent-wake:{message_id}"))
            .unwrap();
        assert!(intent.replayed_at.is_some());
        assert_eq!(
            intent.resolved_parent_session_id.as_deref(),
            Some("newpar01")
        );
        assert_eq!(
            queue
                .active_parent_wake_parent("child001")
                .unwrap()
                .as_deref(),
            Some("newpar01")
        );

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    #[test]
    fn delivered_parent_metadata_without_reminder_creates_no_deferred_wake() {
        let (store, queue, state_file, queue_db, request_id) =
            prepared_reparent_transaction("reparent-no-reminder-wake");
        let message_id = queue
            .enqueue_message_with_metadata(
                "child001",
                "delivery has parent metadata only",
                "important",
                QueueMessageMetadata {
                    parent_session_id: Some("oldpar01".to_owned()),
                    ..QueueMessageMetadata::default()
                },
            )
            .unwrap();
        queue.cancel_parent_wake("child001").unwrap();
        store.acquire_reparent_apply_lease().unwrap();
        let message = queue
            .pending_messages_for_target("child001", 10)
            .unwrap()
            .into_iter()
            .find(|message| message.id == message_id)
            .unwrap();
        let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default());
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            complete_runtime_message_delivery_raw(&store, &mut state, &runtime, &queue, &message)
                .unwrap();
            store.write_raw_json_value(&state).unwrap();
        }

        let record = store.get_reparent_request(&request_id).unwrap().unwrap();
        assert!(record.deferred_routing_intents.is_empty());
        assert_eq!(queue.active_parent_wake_parent("child001").unwrap(), None);

        let _ = fs::remove_file(state_file);
        let _ = fs::remove_file(queue_db);
    }

    fn prepared_reparent_transaction(
        label: &str,
    ) -> (SessionStore, RetainedQueueStore, PathBuf, PathBuf, String) {
        let state_file = unique_temp_path(label);
        let queue_db = state_file.with_extension("db");
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("oldpar01", None, "old-parent-secret"),
                    reparent_test_session("newpar01", None, "new-parent-secret"),
                    reparent_test_session("child001", Some("oldpar01"), "child-secret")
                ],
                "retained_parent_wake_registrations": [{
                    "id": "json-wake-child001",
                    "child_session_id": "child001",
                    "parent_session_id": "oldpar01",
                    "period_seconds": 600,
                    "is_active": true
                }]
            })
            .to_string(),
        )
        .unwrap();
        let queue = RetainedQueueStore::new(queue_db.clone());
        queue.ensure_schema().unwrap();
        queue
            .register_parent_wake("child001", "oldpar01", 600)
            .unwrap();
        let store = SessionStore::new_with_queue(state_file.clone(), queue_db.clone());
        let request = match store
            .create_reparent_request(
                "child001",
                CreateReparentRequest {
                    requester_session_id: "oldpar01".to_owned(),
                    target_parent_session_id: "newpar01".to_owned(),
                },
                "old-parent-secret",
            )
            .unwrap()
        {
            ReparentMutationOutcome::Created(record) => record,
            other => panic!("unexpected create outcome: {other:?}"),
        };
        {
            let _guard = store.write_guard().unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            let mut records = reparent_request_records(&state).unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id == request.id)
                .unwrap();
            record.approvals.push(ReparentApprovalRecord {
                actor_kind: "agent".to_owned(),
                actor_id: "newpar01".to_owned(),
                decision: "approved".to_owned(),
                decided_at: now_rfc3339(),
            });
            record.ready_to_apply = true;
            store_reparent_request_records(&mut state, &records).unwrap();
            store.write_raw_json_value(&state).unwrap();
        }
        (store, queue, state_file, queue_db, request.id)
    }

    fn reparent_test_session(id: &str, parent: Option<&str>, credential: &str) -> Value {
        json!({
            "id": id,
            "name": format!("claude-{id}"),
            "working_dir": "/repo",
            "tmux_session": format!("claude-{id}"),
            "status": "running",
            "created_at": "2026-06-01T00:00:00Z",
            "last_activity": "2026-06-01T00:01:00Z",
            "parent_session_id": parent,
            "session_credential_sha256": sha256_text(credential)
        })
    }

    fn stopped_root_recovery_store(label: &str) -> (SessionStore, PathBuf) {
        let state_file = unique_temp_path(label);
        let mut outgoing = reparent_test_session("outgoing", None, "outgoing-secret");
        outgoing["status"] = Value::String("stopped".to_owned());
        let mut stopped_worker =
            reparent_test_session("stopped-worker", Some("outgoing"), "stopped-secret");
        stopped_worker["status"] = Value::String("stopped".to_owned());
        fs::write(
            &state_file,
            json!({
                "sessions": [
                    reparent_test_session("maintainer", None, "maintainer-secret"),
                    outgoing,
                    reparent_test_session("successor", Some("maintainer"), "successor-secret"),
                    reparent_test_session("worker-a", Some("outgoing"), "worker-a-secret"),
                    reparent_test_session("worker-b", Some("outgoing"), "worker-b-secret"),
                    stopped_worker
                ],
                "agent_registrations": [{
                    "role": "maintainer",
                    "session_id": "maintainer",
                    "created_at": "2026-08-18T00:00:00Z"
                }]
            })
            .to_string(),
        )
        .unwrap();
        (SessionStore::new(state_file.clone()), state_file)
    }

    fn unique_temp_path(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "sm-rust-session-store-{label}-{}-{nanos}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
}
