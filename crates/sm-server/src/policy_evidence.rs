//! Durable, append-only evidence for the policy vertical slice.
//!
//! This module intentionally does not decide policy or launch sessions.  D1 and
//! D3 supply frozen contract envelopes; this store accepts them once and offers
//! read-only projections for the small policy inspection surface.

use std::{fs, path::PathBuf, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;

use crate::policy_contracts::{
    EstimatedAmount, EstimatedCounter, PolicyEventEnvelope, PolicyUsageCounters,
};

const DB_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct PolicyEvidenceStore {
    db_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppendPolicyEventResult {
    pub event_id: String,
    pub ordinal: i64,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyStatus {
    pub lane: String,
    pub event_count: u64,
    pub requirement_count: u64,
    pub latest_event_at: Option<String>,
    pub actual_usage: PolicyUsageTotals,
    pub latest_forecast: Option<Value>,
    pub latest_breaker: Option<Value>,
    /// Published thresholds only. This evidence package never dispatches or
    /// stops work; callers decide how to act on the visible projection.
    pub slice_breaker_thresholds: SliceBreakerThresholds,
}

#[derive(Debug, Clone, Serialize)]
pub struct SliceBreakerThresholds {
    pub estimate_due_elapsed_hours: u64,
    pub hard_breaker_elapsed_hours: u64,
    pub warning_token_fraction: f64,
    pub warning_tokens: u64,
    pub hard_breaker_tokens: u64,
    pub hard_breaker_integration_failures: u64,
}

impl Default for SliceBreakerThresholds {
    fn default() -> Self {
        Self {
            estimate_due_elapsed_hours: 6,
            hard_breaker_elapsed_hours: 8,
            warning_token_fraction: 0.75,
            warning_tokens: 165_000_000,
            hard_breaker_tokens: 220_000_000,
            hard_breaker_integration_failures: 2,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PolicyUsageTotals {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_usd: f64,
    pub quota_points: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyExplanation {
    pub decision_id: String,
    pub events: Vec<StoredPolicyEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredPolicyEvent {
    pub ordinal: i64,
    #[serde(flatten)]
    pub envelope: PolicyEventEnvelope,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyTrialRow {
    pub requirement_id: String,
    pub event_id: String,
    pub operation_id: String,
    pub lane: String,
    pub incremental_cost: Value,
    pub benefit: Value,
    pub observed_at: String,
}

impl PolicyEvidenceStore {
    pub fn new(db_path: impl Into<PathBuf>) -> Result<Self> {
        let store = Self {
            db_path: db_path.into(),
        };
        store.initialize()?;
        Ok(store)
    }

    /// Appends one immutable envelope. Repeating the exact same event ID is a
    /// success; changing its contents is rejected rather than silently merged.
    pub fn append(&self, event: &PolicyEventEnvelope) -> Result<AppendPolicyEventResult> {
        validate_event(event)?;
        let encoded_payload = serde_json::to_string(&event.payload)?;
        let mut connection = self.open()?;
        let tx = connection.transaction()?;
        let existing = tx
            .query_row(
                "SELECT ordinal, sequence, event_type, operation_id, lane, seat_key, session_id, occurred_at, payload_schema, payload FROM policy_events WHERE event_id = ?1",
                [&event.event_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, Option<String>>(5)?, row.get::<_, Option<String>>(6)?, row.get::<_, String>(7)?, row.get::<_, String>(8)?, row.get::<_, String>(9)?)),
            )
            .optional()?;
        if let Some((
            ordinal,
            sequence,
            event_type,
            operation_id,
            lane,
            seat_key,
            session_id,
            occurred_at,
            payload_schema,
            payload,
        )) = existing
        {
            if sequence == event.sequence as i64
                && event_type == event.event_type
                && operation_id == event.operation_id
                && lane == event.lane
                && seat_key == event.seat_key
                && session_id == event.session_id
                && occurred_at == event.occurred_at
                && payload_schema == event.payload_schema
                && payload == encoded_payload
            {
                tx.commit()?;
                return Ok(AppendPolicyEventResult {
                    event_id: event.event_id.clone(),
                    ordinal,
                    duplicate: true,
                });
            }
            bail!(
                "policy event ID {} was already recorded with different contents",
                event.event_id
            );
        }
        tx.execute(
            "INSERT INTO policy_events (event_id, sequence, event_type, operation_id, lane, seat_key, session_id, occurred_at, payload_schema, payload, recorded_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![event.event_id, event.sequence as i64, event.event_type, event.operation_id, event.lane, event.seat_key, event.session_id, event.occurred_at, event.payload_schema, encoded_payload, OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?],
        )?;
        let ordinal = tx.last_insert_rowid();
        index_links(&tx, event)?;
        index_requirement_effect(&tx, event)?;
        tx.commit()?;
        Ok(AppendPolicyEventResult {
            event_id: event.event_id.clone(),
            ordinal,
            duplicate: false,
        })
    }

    pub fn events(&self, lane: &str) -> Result<Vec<StoredPolicyEvent>> {
        if lane.trim().is_empty() {
            bail!("lane is required");
        }
        let connection = self.open()?;
        let mut statement = connection.prepare(
            "SELECT ordinal, event_id, sequence, event_type, operation_id, lane, seat_key, session_id, occurred_at, payload_schema, payload FROM policy_events WHERE lane = ?1 ORDER BY sequence ASC, ordinal ASC",
        )?;
        let events = statement
            .query_map([lane], row_to_event)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(events)
    }

    pub fn explain(&self, decision_id: &str) -> Result<PolicyExplanation> {
        if decision_id.trim().is_empty() {
            bail!("decision ID is required");
        }
        let connection = self.open()?;
        let mut statement = connection.prepare(
            "SELECT ordinal, event_id, sequence, event_type, operation_id, lane, seat_key, session_id, occurred_at, payload_schema, payload FROM policy_events ORDER BY sequence ASC, ordinal ASC",
        )?;
        let events = statement
            .query_map([], row_to_event)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|event| event_matches_decision(event, decision_id))
            .collect();
        Ok(PolicyExplanation {
            decision_id: decision_id.to_owned(),
            events,
        })
    }

    pub fn status(&self, lane: &str) -> Result<PolicyStatus> {
        let events = self.events(lane)?;
        let mut totals = PolicyUsageTotals::default();
        let mut latest_forecast = None;
        let mut latest_breaker = None;
        for event in &events {
            if let Some(counters) = usage_from_payload(&event.envelope.payload) {
                totals.input_tokens += counters.input_tokens.value;
                totals.cache_read_tokens += counters.cache_read_tokens.value;
                totals.cache_write_5m_tokens += counters.cache_write_5m_tokens.value;
                totals.cache_write_1h_tokens += counters.cache_write_1h_tokens.value;
                totals.output_tokens += counters.output_tokens.value;
                totals.reasoning_tokens += counters.reasoning_tokens.value;
                totals.cost_usd += counters.cost_usd.value;
                totals.quota_points += counters.quota_points.value;
            }
            if event.envelope.event_type.contains("forecast") {
                latest_forecast = Some(event.envelope.payload.clone());
            }
            if event.envelope.event_type.contains("breaker") {
                latest_breaker = Some(event.envelope.payload.clone());
            }
        }
        let requirement_count = self.open()?.query_row(
            "SELECT COUNT(DISTINCT requirement_id) FROM policy_requirement_effects WHERE lane = ?1",
            [lane],
            |row| row.get::<_, i64>(0),
        )? as u64;
        Ok(PolicyStatus {
            lane: lane.to_owned(),
            event_count: events.len() as u64,
            requirement_count,
            latest_event_at: events.last().map(|e| e.envelope.occurred_at.clone()),
            actual_usage: totals,
            latest_forecast,
            latest_breaker,
            slice_breaker_thresholds: SliceBreakerThresholds::default(),
        })
    }

    pub fn trial(&self, lane: &str) -> Result<Vec<PolicyTrialRow>> {
        if lane.trim().is_empty() {
            bail!("lane is required");
        }
        let connection = self.open()?;
        let mut statement = connection.prepare("SELECT requirement_id, event_id, operation_id, lane, incremental_cost, benefit, occurred_at FROM policy_requirement_effects WHERE lane = ?1 ORDER BY occurred_at ASC, event_id ASC")?;
        let rows = statement
            .query_map([lane], |row| {
                Ok(PolicyTrialRow {
                    requirement_id: row.get(0)?,
                    event_id: row.get(1)?,
                    operation_id: row.get(2)?,
                    lane: row.get(3)?,
                    incremental_cost: serde_json::from_str::<Value>(&row.get::<_, String>(4)?)
                        .map_err(json_sql_error)?,
                    benefit: serde_json::from_str::<Value>(&row.get::<_, String>(5)?)
                        .map_err(json_sql_error)?,
                    observed_at: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn initialize(&self) -> Result<()> {
        self.open()?.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS policy_events (
              ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
              event_id TEXT NOT NULL UNIQUE,
              sequence INTEGER NOT NULL,
              event_type TEXT NOT NULL,
              operation_id TEXT NOT NULL,
              lane TEXT NOT NULL,
              seat_key TEXT,
              session_id TEXT,
              occurred_at TEXT NOT NULL,
              payload_schema TEXT NOT NULL,
              payload TEXT NOT NULL,
              recorded_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_policy_events_lane_order ON policy_events(lane, sequence, ordinal);
            CREATE INDEX IF NOT EXISTS idx_policy_events_operation ON policy_events(operation_id, ordinal);
            CREATE TABLE IF NOT EXISTS policy_event_links (
              event_id TEXT NOT NULL REFERENCES policy_events(event_id),
              relation TEXT NOT NULL,
              entity_id TEXT NOT NULL,
              PRIMARY KEY(event_id, relation, entity_id)
            );
            CREATE INDEX IF NOT EXISTS idx_policy_event_links_entity ON policy_event_links(relation, entity_id);
            CREATE TABLE IF NOT EXISTS policy_requirement_effects (
              event_id TEXT PRIMARY KEY REFERENCES policy_events(event_id),
              requirement_id TEXT NOT NULL,
              operation_id TEXT NOT NULL,
              lane TEXT NOT NULL,
              incremental_cost TEXT NOT NULL,
              benefit TEXT NOT NULL,
              occurred_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_policy_requirement_effects_lane ON policy_requirement_effects(lane, requirement_id, occurred_at);
        "#).context("failed to initialize policy evidence ledger")?;
        Ok(())
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self
            .db_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&self.db_path).with_context(|| {
            format!(
                "failed to open policy evidence DB {}",
                self.db_path.display()
            )
        })?;
        connection.busy_timeout(DB_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }
}

fn validate_event(event: &PolicyEventEnvelope) -> Result<()> {
    event.validate().map_err(|error| anyhow!(error))?;
    if event.sequence > i64::MAX as u64 {
        bail!("event sequence exceeds SQLite integer range");
    }
    if !event.payload.is_object() {
        bail!("policy event payload must be a JSON object");
    }
    if let Some(raw_usage) = event
        .payload
        .get("usage")
        .or_else(|| event.payload.get("usage_counters"))
    {
        let usage = serde_json::from_value::<PolicyUsageCounters>(raw_usage.clone())
            .context("policy event usage payload is malformed")?;
        validate_usage(&usage)?;
        validate_provider_boundary(&event.payload)?;
    }
    if let Some(links) = event.payload.get("links") {
        let Some(links) = links.as_object() else {
            bail!("policy event links must be a JSON object");
        };
        if links.iter().any(|(relation, value)| {
            relation.trim().is_empty() || value.as_str().is_none_or(|id| id.trim().is_empty())
        }) {
            bail!("policy event links must have non-empty string keys and values");
        }
    }
    if event.event_type == "requirement_effect" {
        let requirement_id = string_field(&event.payload, "requirement_id")?;
        let _ = requirement_id;
        for field in ["incremental_cost", "benefit"] {
            if !event.payload.get(field).is_some_and(Value::is_object) {
                bail!("requirement_effect.{field} must be a JSON object");
            }
        }
    }
    Ok(())
}

fn validate_provider_boundary(payload: &Value) -> Result<()> {
    let boundary = payload
        .get("provider_boundary")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("usage evidence requires provider_boundary"))?;
    if !boundary
        .get("thread_id")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        bail!("provider_boundary.thread_id is required");
    }
    for field in ["before", "after"] {
        if !boundary.get(field).is_some_and(Value::is_object) {
            bail!("provider_boundary.{field} must be a JSON object");
        }
    }
    Ok(())
}

fn validate_usage(usage: &PolicyUsageCounters) -> Result<()> {
    usage.validate().map_err(|error| anyhow!(error))?;
    for counter in [
        &usage.input_tokens,
        &usage.cache_read_tokens,
        &usage.cache_write_5m_tokens,
        &usage.cache_write_1h_tokens,
        &usage.output_tokens,
        &usage.reasoning_tokens,
    ] {
        validate_counter(counter)?;
    }
    for amount in [&usage.cost_usd, &usage.quota_points] {
        validate_amount(amount)?;
    }
    Ok(())
}

fn validate_counter(counter: &EstimatedCounter) -> Result<()> {
    if counter.source.eq_ignore_ascii_case("unknown") {
        bail!("numeric counter source cannot be unknown");
    }
    if !counter.estimated
        && (counter.lower_bound != counter.value || counter.upper_bound != counter.value)
    {
        bail!("direct numeric counters must have exact bounds");
    }
    Ok(())
}

fn validate_amount(amount: &EstimatedAmount) -> Result<()> {
    if amount.source.eq_ignore_ascii_case("unknown") {
        bail!("numeric amount source cannot be unknown");
    }
    if !amount.estimated
        && (amount.lower_bound != amount.value || amount.upper_bound != amount.value)
    {
        bail!("direct numeric amounts must have exact bounds");
    }
    Ok(())
}

fn usage_from_payload(payload: &Value) -> Option<PolicyUsageCounters> {
    payload
        .get("usage")
        .or_else(|| payload.get("usage_counters"))
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn index_links(tx: &rusqlite::Transaction<'_>, event: &PolicyEventEnvelope) -> Result<()> {
    if let Some(links) = event.payload.get("links").and_then(Value::as_object) {
        for (relation, entity_id) in links {
            tx.execute(
                "INSERT INTO policy_event_links(event_id, relation, entity_id) VALUES (?1, ?2, ?3)",
                params![
                    event.event_id,
                    relation,
                    entity_id.as_str().expect("validated links")
                ],
            )?;
        }
    }
    Ok(())
}

fn index_requirement_effect(
    tx: &rusqlite::Transaction<'_>,
    event: &PolicyEventEnvelope,
) -> Result<()> {
    if event.event_type != "requirement_effect" {
        return Ok(());
    }
    tx.execute("INSERT INTO policy_requirement_effects(event_id, requirement_id, operation_id, lane, incremental_cost, benefit, occurred_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![
        event.event_id, string_field(&event.payload, "requirement_id")?, event.operation_id, event.lane,
        serde_json::to_string(event.payload.get("incremental_cost").expect("validated effect"))?,
        serde_json::to_string(event.payload.get("benefit").expect("validated effect"))?, event.occurred_at,
    ])?;
    Ok(())
}

fn string_field<'a>(payload: &'a Value, field: &str) -> Result<&'a str> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("policy event payload requires non-empty {field}"))
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredPolicyEvent> {
    let payload: String = row.get(10)?;
    let payload = serde_json::from_str(&payload).map_err(json_sql_error)?;
    Ok(StoredPolicyEvent {
        ordinal: row.get(0)?,
        envelope: PolicyEventEnvelope {
            schema: crate::policy_contracts::EVENT_SCHEMA.to_owned(),
            event_id: row.get(1)?,
            sequence: row.get::<_, i64>(2)? as u64,
            event_type: row.get(3)?,
            operation_id: row.get(4)?,
            lane: row.get(5)?,
            seat_key: row.get(6)?,
            session_id: row.get(7)?,
            occurred_at: row.get(8)?,
            payload_schema: row.get(9)?,
            payload,
        },
    })
}

fn json_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn event_matches_decision(event: &StoredPolicyEvent, decision_id: &str) -> bool {
    event.envelope.operation_id == decision_id
        || event
            .envelope
            .payload
            .get("decision_id")
            .and_then(Value::as_str)
            == Some(decision_id)
        || event
            .envelope
            .payload
            .get("links")
            .and_then(Value::as_object)
            .and_then(|links| links.get("decision_id"))
            .and_then(Value::as_str)
            == Some(decision_id)
        || event
            .envelope
            .payload
            .get("decision")
            .and_then(|decision| decision.get("decision_id"))
            .and_then(Value::as_str)
            == Some(decision_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_contracts::{EVENT_SCHEMA, USAGE_SCHEMA};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sm-policy-evidence-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn counter(value: u64, estimated: bool) -> Value {
        json!({"value":value,"lower_bound":value,"upper_bound":value,"source":if estimated {"calibrated_provider_event"} else {"provider_event"},"estimated":estimated,"method":if estimated {"calibrated"} else {"direct"},"confidence":"high"})
    }
    fn amount(value: f64, estimated: bool, unit: &str) -> Value {
        json!({"value":value,"lower_bound":value,"upper_bound":value,"unit":unit,"source":if estimated {"calibrated_provider_event"} else {"provider_event"},"estimated":estimated,"method":if estimated {"calibrated"} else {"direct"},"confidence":"high"})
    }
    fn usage(estimated: bool) -> Value {
        json!({"schema":USAGE_SCHEMA,"input_tokens":counter(1,estimated),"cache_read_tokens":counter(2,estimated),"cache_write_5m_tokens":counter(3,estimated),"cache_write_1h_tokens":counter(4,estimated),"output_tokens":counter(5,estimated),"reasoning_tokens":counter(6,estimated),"cost_usd":amount(0.5,estimated,"USD"),"quota_points":amount(1.5,estimated,"points")})
    }
    fn event(id: &str, sequence: u64, mut payload: Value) -> PolicyEventEnvelope {
        if payload.get("usage").is_some() || payload.get("usage_counters").is_some() {
            payload["provider_boundary"] =
                json!({"thread_id":"thread-a","before":{"event_seq":10},"after":{"event_seq":11}});
        }
        PolicyEventEnvelope {
            schema: EVENT_SCHEMA.to_owned(),
            event_id: id.to_owned(),
            sequence,
            event_type: "evaluation_usage".to_owned(),
            operation_id: "op-1".to_owned(),
            lane: "sm-policy-1268".to_owned(),
            seat_key: Some("1268-root".to_owned()),
            session_id: Some("thread-a".to_owned()),
            occurred_at: "2026-08-17T00:00:00Z".to_owned(),
            payload_schema: "sm.policy.evaluation.v1".to_owned(),
            payload,
        }
    }

    #[test]
    fn duplicate_ids_are_idempotent_and_durable() {
        let dir = TempDir::new();
        let path = dir.0.join("policy.db");
        let store = PolicyEvidenceStore::new(&path).unwrap();
        let first = store
            .append(&event("event-1", 1, json!({"usage":usage(false)})))
            .unwrap();
        assert!(!first.duplicate);
        assert!(
            store
                .append(&event("event-1", 1, json!({"usage":usage(false)})))
                .unwrap()
                .duplicate
        );
        drop(store);
        let reopened = PolicyEvidenceStore::new(&path).unwrap();
        assert_eq!(reopened.events("sm-policy-1268").unwrap().len(), 1);
        assert!(reopened
            .append(&event("event-1", 2, json!({"usage":usage(false)})))
            .is_err());
    }

    #[test]
    fn events_have_stable_sequence_then_append_order() {
        let dir = TempDir::new();
        let store = PolicyEvidenceStore::new(dir.0.join("policy.db")).unwrap();
        store.append(&event("late", 9, json!({}))).unwrap();
        store.append(&event("first-a", 1, json!({}))).unwrap();
        store.append(&event("first-b", 1, json!({}))).unwrap();
        assert_eq!(
            store
                .events("sm-policy-1268")
                .unwrap()
                .into_iter()
                .map(|event| event.envelope.event_id)
                .collect::<Vec<_>>(),
            ["first-a", "first-b", "late"]
        );
    }

    #[test]
    fn malformed_payload_and_unknown_numeric_source_are_rejected() {
        let dir = TempDir::new();
        let store = PolicyEvidenceStore::new(dir.0.join("policy.db")).unwrap();
        assert!(store.append(&event("bad", 1, Value::Null)).is_err());
        assert!(store
            .append(&event(
                "bad-usage",
                2,
                json!({"usage":{"input_tokens":"not-a-counter"}})
            ))
            .is_err());
        let mut bad = usage(true);
        bad["input_tokens"]["source"] = json!("unknown");
        assert!(store
            .append(&event("bad-counter", 3, json!({"usage":bad})))
            .is_err());
    }

    #[test]
    fn direct_and_estimated_counters_are_preserved_and_summed() {
        let dir = TempDir::new();
        let store = PolicyEvidenceStore::new(dir.0.join("policy.db")).unwrap();
        store
            .append(&event("direct", 1, json!({"usage":usage(false)})))
            .unwrap();
        store
            .append(&event("estimated", 2, json!({"usage":usage(true)})))
            .unwrap();
        let events = store.events("sm-policy-1268").unwrap();
        assert!(
            !events[0].envelope.payload["usage"]["input_tokens"]["estimated"]
                .as_bool()
                .unwrap()
        );
        assert!(
            events[1].envelope.payload["usage"]["input_tokens"]["estimated"]
                .as_bool()
                .unwrap()
        );
        assert_eq!(
            store
                .status("sm-policy-1268")
                .unwrap()
                .actual_usage
                .cache_read_tokens,
            4
        );
    }

    #[test]
    fn requirement_effects_are_lane_filtered() {
        let dir = TempDir::new();
        let store = PolicyEvidenceStore::new(dir.0.join("policy.db")).unwrap();
        let mut effect = event(
            "effect",
            1,
            json!({"requirement_id":"model-attestation","incremental_cost":{"tokens":usage(true)},"benefit":{"status":"no_observed_benefit"},"links":{"decision_id":"decision-1"}}),
        );
        effect.event_type = "requirement_effect".to_owned();
        store.append(&effect).unwrap();
        assert_eq!(store.trial("sm-policy-1268").unwrap().len(), 1);
        assert!(store.trial("other").unwrap().is_empty());
        assert_eq!(store.explain("decision-1").unwrap().events.len(), 1);
    }

    #[test]
    fn status_publishes_slice_breaker_thresholds_as_data() {
        let dir = TempDir::new();
        let store = PolicyEvidenceStore::new(dir.0.join("policy.db")).unwrap();
        let thresholds = store
            .status("sm-policy-1268")
            .unwrap()
            .slice_breaker_thresholds;
        assert_eq!(thresholds.estimate_due_elapsed_hours, 6);
        assert_eq!(thresholds.hard_breaker_elapsed_hours, 8);
        assert_eq!(thresholds.warning_tokens, 165_000_000);
        assert_eq!(thresholds.hard_breaker_tokens, 220_000_000);
        assert_eq!(thresholds.hard_breaker_integration_failures, 2);
    }
}
