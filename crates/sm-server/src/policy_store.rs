//! Durable, deliberately small policy projection and admission preparation.
//!
//! This module is the D1 boundary.  It owns immutable requests/decisions and
//! the transaction which reserves capacity (and, when applicable, consumes a
//! one-shot override).  D3 owns child allocation and runtime attestation.

use std::{collections::BTreeMap, fmt, path::PathBuf, time::Duration};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::policy_contracts::{
    PolicyCallerBinding, PolicyCapacityClaim, PolicyCapacityLease, PolicyClassification,
    PolicyDecision, PolicyDecisionOutcome, PolicyLaunchProfile, PolicyOverrideRecord,
    PolicyOverrideState, PolicySpawnRequest, DECISION_SCHEMA,
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum PolicyStoreError {
    Invalid(String),
    RequestNotFound(String),
    OverrideNotFound(String),
    ProjectionNotFound(String),
    Stale(String),
    CapacityUnavailable(String),
    CanaryDenied(String),
    OverrideDenied(String),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
}

impl fmt::Display for PolicyStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(detail) => write!(f, "invalid policy data: {detail}"),
            Self::RequestNotFound(id) => write!(f, "policy request {id} was not found"),
            Self::OverrideNotFound(id) => write!(f, "policy override {id} was not found"),
            Self::ProjectionNotFound(lane) => {
                write!(f, "policy projection for lane {lane} was not found")
            }
            Self::Stale(detail) => write!(f, "stale policy request: {detail}"),
            Self::CapacityUnavailable(detail) => write!(f, "capacity unavailable: {detail}"),
            Self::CanaryDenied(detail) => write!(f, "policy canary denied: {detail}"),
            Self::OverrideDenied(detail) => write!(f, "override cannot be consumed: {detail}"),
            Self::Sqlite(error) => error.fmt(f),
            Self::Json(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for PolicyStoreError {}
impl From<rusqlite::Error> for PolicyStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<serde_json::Error> for PolicyStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub type Result<T> = std::result::Result<T, PolicyStoreError>;

/// The externally approved bootstrap binding.  Merely storing a projection
/// cannot manufacture this evidence: it must be supplied by the deployer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapAuthority {
    pub session_id: String,
    pub credential_fingerprint: String,
    pub binding_digest: String,
}

impl BootstrapAuthority {
    fn matches(&self, caller: &PolicyCallerBinding) -> bool {
        matches!(caller, PolicyCallerBinding::IncarnationBootstrap {
            session_id,
            credential_fingerprint,
            binding_digest,
            ..
        } if session_id == &self.session_id
            && credential_fingerprint == &self.credential_fingerprint
            && binding_digest == &self.binding_digest)
    }

    fn validate(&self) -> Result<()> {
        required(&self.session_id, "bootstrap session_id")?;
        required(
            &self.credential_fingerprint,
            "bootstrap credential_fingerprint",
        )?;
        sha256(&self.binding_digest, "bootstrap binding_digest")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyCapacityLimit {
    pub dimension: String,
    pub key: String,
    pub maximum_units: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRuleOutcome {
    Allow,
    Rewrite,
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedPolicyRule {
    /// Stable, lane-scoped authority identifier, never a prose line number.
    pub clause_id: String,
    /// Lower ranks take precedence.  Equal-rank disagreement blocks.
    pub rank: u32,
    #[serde(default)]
    pub class_id: Option<String>,
    #[serde(default)]
    pub role_id: Option<String>,
    #[serde(default)]
    pub vehicle: Option<crate::policy_contracts::PolicyVehicle>,
    pub profile: PolicyLaunchProfile,
    #[serde(default)]
    pub outcome: PolicyRuleOutcome,
    #[serde(default = "default_true")]
    pub overridable: bool,
    #[serde(default)]
    pub capacity_claims: Vec<PolicyCapacityClaim>,
    #[serde(default = "default_lease_ttl_seconds")]
    pub lease_ttl_seconds: u64,
    pub reason: String,
}

impl Default for PolicyRuleOutcome {
    fn default() -> Self {
        Self::Allow
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProjection {
    pub lane: String,
    pub policy_version: String,
    pub policy_digest: String,
    /// Defaults false: a persisted projection is not authority to activate its
    /// own canary.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bootstrap_authority: Option<BootstrapAuthority>,
    pub rules: Vec<MaterializedPolicyRule>,
    #[serde(default)]
    pub capacity_limits: Vec<PolicyCapacityLimit>,
}

impl PolicyProjection {
    pub fn validate(&self) -> Result<()> {
        required(&self.lane, "projection lane")?;
        required(&self.policy_version, "projection policy_version")?;
        sha256(&self.policy_digest, "projection policy_digest")?;
        if let Some(authority) = &self.bootstrap_authority {
            authority.validate()?;
        }
        if self.rules.is_empty() {
            return Err(PolicyStoreError::Invalid(
                "projection must contain a rule".into(),
            ));
        }
        let mut clause_ids = std::collections::BTreeSet::new();
        for rule in &self.rules {
            required(&rule.clause_id, "rule clause_id")?;
            if !clause_ids.insert(&rule.clause_id) {
                return Err(PolicyStoreError::Invalid(format!(
                    "duplicate stable clause ID {}",
                    rule.clause_id
                )));
            }
            optional(&rule.class_id, "rule class_id")?;
            optional(&rule.role_id, "rule role_id")?;
            rule.profile
                .validate()
                .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
            if rule
                .vehicle
                .as_ref()
                .is_some_and(|vehicle| vehicle != &rule.profile.vehicle)
            {
                return Err(PolicyStoreError::Invalid(
                    "rule vehicle must agree with its resolved profile".into(),
                ));
            }
            required(&rule.reason, "rule reason")?;
            if rule.lease_ttl_seconds == 0 {
                return Err(PolicyStoreError::Invalid(
                    "lease ttl must be positive".into(),
                ));
            }
            for claim in &rule.capacity_claims {
                claim
                    .validate()
                    .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
            }
            if matches!(rule.outcome, PolicyRuleOutcome::Allow) && rule.capacity_claims.is_empty() {
                return Err(PolicyStoreError::Invalid(
                    "allow rule must reserve at least one capacity claim".into(),
                ));
            }
        }
        let mut limits = std::collections::BTreeSet::new();
        for limit in &self.capacity_limits {
            required(&limit.dimension, "capacity limit dimension")?;
            required(&limit.key, "capacity limit key")?;
            if limit.maximum_units == 0 {
                return Err(PolicyStoreError::Invalid(
                    "capacity maximum must be positive".into(),
                ));
            }
            if !limits.insert((&limit.dimension, &limit.key)) {
                return Err(PolicyStoreError::Invalid("duplicate capacity limit".into()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDecision {
    pub decision: PolicyDecision,
    pub reused: bool,
}

#[derive(Debug, Clone)]
pub struct PolicyStore {
    db_path: PathBuf,
}

impl PolicyStore {
    pub fn new(db_path: impl Into<PathBuf>) -> Result<Self> {
        let store = Self {
            db_path: db_path.into(),
        };
        store.open()?;
        Ok(store)
    }

    pub fn install_projection(&self, projection: &PolicyProjection) -> Result<()> {
        projection.validate()?;
        let conn = self.open()?;
        let projection_json = serde_json::to_string(projection)?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO policy_projections (lane, policy_version, policy_digest, projection_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![projection.lane, projection.policy_version, projection.policy_digest, projection_json, now()?],
        )?;
        if inserted == 0 {
            let existing: String = conn.query_row(
                "SELECT projection_json FROM policy_projections WHERE lane = ?1 AND policy_version = ?2 AND policy_digest = ?3",
                params![projection.lane, projection.policy_version, projection.policy_digest],
                |row| row.get(0),
            )?;
            if existing != projection_json {
                return Err(PolicyStoreError::Invalid(
                    "policy version/digest is already bound to different projection bytes".into(),
                ));
            }
        }
        conn.execute(
            "INSERT INTO policy_runtime_versions (lane, topology_version, capacity_version) VALUES (?1, 0, 0) ON CONFLICT(lane) DO NOTHING",
            [&projection.lane],
        )?;
        Ok(())
    }

    /// D3 sets the current provider-derived versions before preparing admission.
    pub fn set_runtime_versions(
        &self,
        lane: &str,
        topology_version: u64,
        capacity_version: u64,
    ) -> Result<()> {
        required(lane, "lane")?;
        let conn = self.open()?;
        let changed = conn.execute(
            "UPDATE policy_runtime_versions SET topology_version = ?2, capacity_version = ?3 WHERE lane = ?1",
            params![lane, topology_version, capacity_version],
        )?;
        if changed == 0 {
            return Err(PolicyStoreError::ProjectionNotFound(lane.into()));
        }
        Ok(())
    }

    /// Inserts exactly one immutable serialized request. Repeating the same
    /// request is safe; changing any frozen byte under an existing ID is not.
    pub fn create_request(&self, request: &PolicySpawnRequest) -> Result<()> {
        request
            .validate()
            .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
        let digest = request
            .canonical_digest()
            .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
        let conn = self.open()?;
        let json = serde_json::to_string(request)?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO policy_requests (request_id, lane, request_digest, request_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![request.request_id, request.caller.lane(), digest, json, request.created_at],
        )?;
        if inserted == 0 {
            let existing: String = conn.query_row(
                "SELECT request_digest FROM policy_requests WHERE request_id = ?1",
                [&request.request_id],
                |row| row.get(0),
            )?;
            if existing != digest {
                return Err(PolicyStoreError::Invalid(
                    "request ID is already bound to different immutable input".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn request(&self, request_id: &str) -> Result<PolicySpawnRequest> {
        let conn = self.open()?;
        load_request(&conn, request_id)
    }

    /// Evaluates and atomically reserves all capacity. A passed override is
    /// consumed in the same transaction only after capacity is available.
    pub fn prepare_admission(
        &self,
        request_id: &str,
        classification: &PolicyClassification,
        override_id: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<PreparedDecision> {
        classification
            .validate()
            .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_leases(&tx, now)?;
        expire_overrides_tx(&tx, now)?;
        let request = load_request_tx(&tx, request_id)?;
        let projection = load_projection_tx(
            &tx,
            request.caller.lane(),
            &request.policy_version,
            &request.policy_digest,
        )?;
        authorize_caller(&projection, &request)?;
        let (topology_version, capacity_version): (u64, u64) = tx.query_row(
            "SELECT topology_version, capacity_version FROM policy_runtime_versions WHERE lane = ?1",
            [request.caller.lane()], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?.ok_or_else(|| PolicyStoreError::ProjectionNotFound(request.caller.lane().into()))?;
        if request.topology_version != topology_version
            || request.capacity_version != capacity_version
        {
            return Err(PolicyStoreError::Stale(format!(
                "request observed topology/capacity {}/{} but current versions are {}/{}",
                request.topology_version,
                request.capacity_version,
                topology_version,
                capacity_version
            )));
        }
        if let Some(existing) = load_reusable_decision(&tx, request_id, override_id)? {
            tx.commit()?;
            return Ok(PreparedDecision {
                decision: existing,
                reused: true,
            });
        }
        let attempt = next_attempt(&tx, request_id)?;
        let resolved = resolve_rule(&projection, &request, classification);
        let mut outcome = resolved.outcome.clone();
        let mut override_used = None;
        if let Some(override_id) = override_id {
            let override_record = load_override_tx(&tx, override_id)?;
            let terminal = load_terminal_decision(&tx, request_id, &override_record.decision_id)?;
            override_record
                .validate_for_consumption(&request, &terminal, now)
                .map_err(|error| PolicyStoreError::OverrideDenied(error.to_string()))?;
            if !resolved.overridable || resolved.profile.is_none() {
                return Err(PolicyStoreError::OverrideDenied(
                    "the applicable policy outcome is not overridable".into(),
                ));
            }
            outcome = PolicyDecisionOutcome::Allow;
            override_used = Some(override_id);
        }
        let decision_id = derived_id(
            "decision",
            &[
                &request
                    .canonical_digest()
                    .map_err(|e| PolicyStoreError::Invalid(e.to_string()))?,
                &attempt.to_string(),
                override_id.unwrap_or(""),
            ],
        );
        let decided_at = format_time(now)?;
        let mut decision = PolicyDecision {
            schema: DECISION_SCHEMA.into(),
            decision_id,
            request_id: request.request_id.clone(),
            attempt,
            policy_version: request.policy_version.clone(),
            policy_digest: request.policy_digest.clone(),
            outcome: outcome.clone(),
            classification: classification.clone(),
            applicable_clause_ids: resolved.clause_ids.clone(),
            resolved_profile: resolved.profile.clone(),
            reason: resolved.reason.clone(),
            override_command: None,
            capacity_lease: None,
            decided_at,
        };
        if matches!(outcome, PolicyDecisionOutcome::Allow) {
            let profile = decision.resolved_profile.as_ref().ok_or_else(|| {
                PolicyStoreError::Invalid("allow policy has no resolved profile".into())
            })?;
            profile
                .validate()
                .map_err(|e| PolicyStoreError::Invalid(e.to_string()))?;
            let claims = resolved.claims.clone();
            ensure_capacity(&tx, &projection, &claims)?;
            let lease = PolicyCapacityLease {
                lease_id: derived_id("lease", &[&decision.decision_id]),
                request_id: request.request_id.clone(),
                topology_version,
                capacity_version,
                claims: claims.clone(),
                expires_at: format_time(
                    now + time::Duration::seconds(resolved.lease_ttl_seconds as i64),
                )?,
            };
            lease
                .validate()
                .map_err(|e| PolicyStoreError::Invalid(e.to_string()))?;
            insert_lease(&tx, &lease)?;
            decision.capacity_lease = Some(lease);
        } else {
            decision.override_command = Some(format!(
                "sm policy override --request {} --reason <text>",
                request.request_id
            ));
        }
        decision
            .validate_for_request(&request)
            .map_err(|e| PolicyStoreError::Invalid(e.to_string()))?;
        tx.execute("INSERT INTO policy_decisions (decision_id, request_id, attempt, override_id, decision_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![decision.decision_id, request_id, attempt, override_used, serde_json::to_string(&decision)?, decision.decided_at])?;
        if let Some(override_id) = override_used {
            consume_override_tx(&tx, override_id, now)?;
        }
        tx.commit()?;
        Ok(PreparedDecision {
            decision,
            reused: false,
        })
    }

    /// Authorizes a single exact frozen request. The record itself carries the
    /// authoritative cross-record binding; no caller supplied shortcut exists.
    pub fn authorize_override(
        &self,
        record: &PolicyOverrideRecord,
        now: OffsetDateTime,
    ) -> Result<()> {
        record
            .validate()
            .map_err(|e| PolicyStoreError::Invalid(e.to_string()))?;
        if !matches!(record.state, PolicyOverrideState::Authorized) {
            return Err(PolicyStoreError::Invalid(
                "new override must start authorized".into(),
            ));
        }
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let request = load_request_tx(&tx, &record.request_id)?;
        let decision = load_terminal_decision(&tx, &record.request_id, &record.decision_id)?;
        record
            .validate_for_consumption(&request, &decision, now)
            .map_err(|e| PolicyStoreError::OverrideDenied(e.to_string()))?;
        let json = serde_json::to_string(record)?;
        let inserted = tx.execute("INSERT OR IGNORE INTO policy_overrides (override_id, request_id, decision_id, state, override_json, created_at) VALUES (?1, ?2, ?3, 'authorized', ?4, ?5)", params![record.override_id, record.request_id, record.decision_id, json, record.created_at])?;
        if inserted == 0 {
            let existing = load_override_tx(&tx, &record.override_id)?;
            if existing != *record {
                return Err(PolicyStoreError::Invalid(
                    "override ID is already bound to different immutable authorization".into(),
                ));
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn release_lease(&self, lease_id: &str) -> Result<()> {
        let conn = self.open()?;
        conn.execute("UPDATE policy_capacity_leases SET state = 'released', released_at = ?2 WHERE lease_id = ?1 AND state = 'active'", params![lease_id, now()?])?;
        Ok(())
    }

    pub fn expire_overrides(&self, now: OffsetDateTime) -> Result<usize> {
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count = expire_overrides_tx(&tx, now)?;
        tx.commit()?;
        Ok(count)
    }

    pub fn override_record(&self, override_id: &str) -> Result<PolicyOverrideRecord> {
        load_override_conn(&self.open()?, override_id)
    }

    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PolicyStoreError::Invalid(e.to_string()))?;
        }
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS policy_projections (lane TEXT NOT NULL, policy_version TEXT NOT NULL, policy_digest TEXT NOT NULL, projection_json TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (lane, policy_version, policy_digest));
            CREATE TABLE IF NOT EXISTS policy_runtime_versions (lane TEXT PRIMARY KEY, topology_version INTEGER NOT NULL, capacity_version INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS policy_requests (request_id TEXT PRIMARY KEY, lane TEXT NOT NULL, request_digest TEXT NOT NULL, request_json TEXT NOT NULL, created_at TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS policy_decisions (decision_id TEXT PRIMARY KEY, request_id TEXT NOT NULL, attempt INTEGER NOT NULL, override_id TEXT, decision_json TEXT NOT NULL, created_at TEXT NOT NULL, UNIQUE(request_id, attempt), UNIQUE(request_id, override_id));
            CREATE TABLE IF NOT EXISTS policy_capacity_leases (lease_id TEXT PRIMARY KEY, request_id TEXT NOT NULL, state TEXT NOT NULL, expires_at TEXT NOT NULL, released_at TEXT, lease_json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS policy_capacity_claims (lease_id TEXT NOT NULL, dimension TEXT NOT NULL, claim_key TEXT NOT NULL, units INTEGER NOT NULL, PRIMARY KEY (lease_id, dimension, claim_key));
            CREATE TABLE IF NOT EXISTS policy_overrides (override_id TEXT PRIMARY KEY, request_id TEXT NOT NULL, decision_id TEXT NOT NULL, state TEXT NOT NULL, override_json TEXT NOT NULL, created_at TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS idx_policy_active_claims ON policy_capacity_claims(dimension, claim_key);
            CREATE INDEX IF NOT EXISTS idx_policy_leases_active ON policy_capacity_leases(state, expires_at);
        "#)?;
        Ok(conn)
    }
}

#[derive(Debug)]
struct ResolvedRule {
    outcome: PolicyDecisionOutcome,
    profile: Option<PolicyLaunchProfile>,
    clause_ids: Vec<String>,
    reason: String,
    claims: Vec<PolicyCapacityClaim>,
    lease_ttl_seconds: u64,
    overridable: bool,
}

fn resolve_rule(
    projection: &PolicyProjection,
    request: &PolicySpawnRequest,
    classification: &PolicyClassification,
) -> ResolvedRule {
    let mut matches: Vec<&MaterializedPolicyRule> = projection
        .rules
        .iter()
        .filter(|rule| {
            rule.class_id
                .as_deref()
                .map_or(true, |value| value == classification.class_id)
                && rule.role_id.as_deref().map_or(true, |value| {
                    classification.role_id.as_deref() == Some(value)
                })
                && rule
                    .vehicle
                    .as_ref()
                    .map_or(true, |value| value == &request.requested.vehicle)
        })
        .collect();
    matches.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| a.clause_id.cmp(&b.clause_id))
    });
    let Some(first) = matches.first().copied() else {
        return ResolvedRule {
            outcome: PolicyDecisionOutcome::Block,
            profile: None,
            clause_ids: vec!["unclassified-request".into()],
            reason: "no materialized policy clause applies to this frozen request".into(),
            claims: vec![],
            lease_ttl_seconds: default_lease_ttl_seconds(),
            overridable: false,
        };
    };
    let winning: Vec<_> = matches
        .into_iter()
        .take_while(|rule| rule.rank == first.rank)
        .collect();
    let conflicting = winning.iter().skip(1).any(|rule| {
        rule.profile != first.profile
            || rule.outcome != first.outcome
            || rule.capacity_claims != first.capacity_claims
    });
    let clause_ids = winning.iter().map(|rule| rule.clause_id.clone()).collect();
    if conflicting {
        return ResolvedRule {
            outcome: PolicyDecisionOutcome::Block,
            profile: None,
            clause_ids,
            reason: "same-rank materialized policy clauses conflict".into(),
            claims: vec![],
            lease_ttl_seconds: first.lease_ttl_seconds,
            overridable: false,
        };
    }
    let mut outcome = match first.outcome {
        PolicyRuleOutcome::Allow => PolicyDecisionOutcome::Allow,
        PolicyRuleOutcome::Rewrite => PolicyDecisionOutcome::Rewrite,
        PolicyRuleOutcome::Block => PolicyDecisionOutcome::Block,
    };
    if matches!(outcome, PolicyDecisionOutcome::Allow)
        && requested_profile_conflicts(request, &first.profile)
    {
        outcome = PolicyDecisionOutcome::Rewrite;
    }
    ResolvedRule {
        outcome,
        profile: Some(first.profile.clone()),
        clause_ids,
        reason: first.reason.clone(),
        claims: first.capacity_claims.clone(),
        lease_ttl_seconds: first.lease_ttl_seconds,
        overridable: first.overridable,
    }
}

fn requested_profile_conflicts(
    request: &PolicySpawnRequest,
    profile: &PolicyLaunchProfile,
) -> bool {
    request.requested.vehicle != profile.vehicle
        || request.requested.provider != profile.provider
        || request
            .requested
            .model
            .as_deref()
            .is_some_and(|model| model != profile.model)
        || request
            .requested
            .effort
            .as_deref()
            .is_some_and(|effort| effort != profile.effort)
}

fn authorize_caller(projection: &PolicyProjection, request: &PolicySpawnRequest) -> Result<()> {
    if !projection.enabled {
        return Err(PolicyStoreError::CanaryDenied(
            "projection is disabled".into(),
        ));
    }
    match &request.caller {
        PolicyCallerBinding::Seat { .. } => Ok(()),
        PolicyCallerBinding::IncarnationBootstrap { .. } => projection
            .bootstrap_authority
            .as_ref()
            .filter(|authority| authority.matches(&request.caller))
            .map(|_| ())
            .ok_or_else(|| {
                PolicyStoreError::CanaryDenied(
                    "bootstrap caller does not match externally configured authority".into(),
                )
            }),
    }
}

fn ensure_capacity(
    tx: &Transaction<'_>,
    projection: &PolicyProjection,
    claims: &[PolicyCapacityClaim],
) -> Result<()> {
    let mut requested = BTreeMap::<(&str, &str), u64>::new();
    for claim in claims {
        *requested.entry((&claim.dimension, &claim.key)).or_default() += claim.units;
    }
    for ((dimension, key), units) in requested {
        let maximum = projection
            .capacity_limits
            .iter()
            .find(|limit| limit.dimension == dimension && limit.key == key)
            .map(|limit| limit.maximum_units)
            .ok_or_else(|| {
                PolicyStoreError::CapacityUnavailable(format!("no limit for {dimension}/{key}"))
            })?;
        let active: u64 = tx.query_row(r#"SELECT COALESCE(SUM(c.units), 0) FROM policy_capacity_claims c JOIN policy_capacity_leases l ON l.lease_id = c.lease_id WHERE c.dimension = ?1 AND c.claim_key = ?2 AND l.state = 'active'"#, params![dimension, key], |row| row.get(0))?;
        if active.saturating_add(units) > maximum {
            return Err(PolicyStoreError::CapacityUnavailable(format!(
                "{dimension}/{key}: requested {units}, active {active}, maximum {maximum}"
            )));
        }
    }
    Ok(())
}

fn insert_lease(tx: &Transaction<'_>, lease: &PolicyCapacityLease) -> Result<()> {
    tx.execute("INSERT INTO policy_capacity_leases (lease_id, request_id, state, expires_at, lease_json) VALUES (?1, ?2, 'active', ?3, ?4)", params![lease.lease_id, lease.request_id, lease.expires_at, serde_json::to_string(lease)?])?;
    for claim in &lease.claims {
        tx.execute("INSERT INTO policy_capacity_claims (lease_id, dimension, claim_key, units) VALUES (?1, ?2, ?3, ?4)", params![lease.lease_id, claim.dimension, claim.key, claim.units])?;
    }
    Ok(())
}

fn expire_leases(tx: &Transaction<'_>, now: OffsetDateTime) -> Result<()> {
    tx.execute("UPDATE policy_capacity_leases SET state = 'expired' WHERE state = 'active' AND expires_at <= ?1", [format_time(now)?])?;
    Ok(())
}

fn consume_override_tx(tx: &Transaction<'_>, override_id: &str, now: OffsetDateTime) -> Result<()> {
    let mut record = load_override_tx(tx, override_id)?;
    if !matches!(record.state, PolicyOverrideState::Authorized) {
        return Err(PolicyStoreError::OverrideDenied(
            "override is no longer authorized".into(),
        ));
    }
    record.state = PolicyOverrideState::Consumed;
    record.consumed_at = Some(format_time(now)?);
    let changed = tx.execute("UPDATE policy_overrides SET state = 'consumed', override_json = ?2 WHERE override_id = ?1 AND state = 'authorized'", params![override_id, serde_json::to_string(&record)?])?;
    if changed != 1 {
        return Err(PolicyStoreError::OverrideDenied(
            "override was concurrently consumed".into(),
        ));
    }
    Ok(())
}

fn expire_overrides_tx(tx: &Transaction<'_>, now: OffsetDateTime) -> Result<usize> {
    let mut statement = tx.prepare(
        "SELECT override_id, override_json FROM policy_overrides WHERE state = 'authorized'",
    )?;
    let records = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    let mut count = 0;
    for (id, json) in records {
        let mut record: PolicyOverrideRecord = serde_json::from_str(&json)?;
        let expiry = OffsetDateTime::parse(&record.expires_at, &Rfc3339)
            .map_err(|e| PolicyStoreError::Invalid(format!("invalid override expiry: {e}")))?;
        if now >= expiry {
            record.state = PolicyOverrideState::Expired;
            tx.execute("UPDATE policy_overrides SET state = 'expired', override_json = ?2 WHERE override_id = ?1 AND state = 'authorized'", params![id, serde_json::to_string(&record)?])?;
            count += 1;
        }
    }
    Ok(count)
}

fn load_request(conn: &Connection, request_id: &str) -> Result<PolicySpawnRequest> {
    let value = conn
        .query_row(
            "SELECT request_json FROM policy_requests WHERE request_id = ?1",
            [request_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::RequestNotFound(request_id.into()))?;
    Ok(serde_json::from_str(&value)?)
}
fn load_request_tx(tx: &Transaction<'_>, request_id: &str) -> Result<PolicySpawnRequest> {
    let value = tx
        .query_row(
            "SELECT request_json FROM policy_requests WHERE request_id = ?1",
            [request_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::RequestNotFound(request_id.into()))?;
    Ok(serde_json::from_str(&value)?)
}
fn load_projection_tx(
    tx: &Transaction<'_>,
    lane: &str,
    version: &str,
    digest: &str,
) -> Result<PolicyProjection> {
    let value = tx.query_row("SELECT projection_json FROM policy_projections WHERE lane = ?1 AND policy_version = ?2 AND policy_digest = ?3", params![lane, version, digest], |row| row.get::<_, String>(0)).optional()?.ok_or_else(|| PolicyStoreError::ProjectionNotFound(lane.into()))?;
    Ok(serde_json::from_str(&value)?)
}
fn load_override_conn(conn: &Connection, override_id: &str) -> Result<PolicyOverrideRecord> {
    let value = conn
        .query_row(
            "SELECT override_json FROM policy_overrides WHERE override_id = ?1",
            [override_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::OverrideNotFound(override_id.into()))?;
    Ok(serde_json::from_str(&value)?)
}
fn load_override_tx(tx: &Transaction<'_>, override_id: &str) -> Result<PolicyOverrideRecord> {
    let value = tx
        .query_row(
            "SELECT override_json FROM policy_overrides WHERE override_id = ?1",
            [override_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::OverrideNotFound(override_id.into()))?;
    Ok(serde_json::from_str(&value)?)
}

fn load_terminal_decision(
    tx: &Transaction<'_>,
    request_id: &str,
    decision_id: &str,
) -> Result<PolicyDecision> {
    let value = tx
        .query_row(
            "SELECT decision_json FROM policy_decisions WHERE request_id = ?1 AND decision_id = ?2",
            params![request_id, decision_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            PolicyStoreError::OverrideDenied("override points to a missing decision".into())
        })?;
    Ok(serde_json::from_str(&value)?)
}
fn load_reusable_decision(
    tx: &Transaction<'_>,
    request_id: &str,
    override_id: Option<&str>,
) -> Result<Option<PolicyDecision>> {
    let value = match override_id {
        Some(id) => tx.query_row("SELECT decision_json FROM policy_decisions WHERE request_id = ?1 AND override_id = ?2", params![request_id, id], |row| row.get::<_, String>(0)).optional()?,
        None => tx.query_row("SELECT decision_json FROM policy_decisions WHERE request_id = ?1 AND override_id IS NULL ORDER BY attempt ASC LIMIT 1", [request_id], |row| row.get::<_, String>(0)).optional()?,
    };
    let Some(json) = value else {
        return Ok(None);
    };
    let decision: PolicyDecision = serde_json::from_str(&json)?;
    if let Some(lease) = &decision.capacity_lease {
        let active: bool = tx
            .query_row(
                "SELECT state = 'active' FROM policy_capacity_leases WHERE lease_id = ?1",
                [&lease.lease_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if !active {
            return Ok(None);
        }
    }
    Ok(Some(decision))
}
fn next_attempt(tx: &Transaction<'_>, request_id: &str) -> Result<u32> {
    let current: u32 = tx.query_row(
        "SELECT COALESCE(MAX(attempt), 0) FROM policy_decisions WHERE request_id = ?1",
        [request_id],
        |row| row.get(0),
    )?;
    Ok(current + 1)
}

fn required(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(PolicyStoreError::Invalid(format!("{field} is required")))
    } else {
        Ok(())
    }
}
fn optional(value: &Option<String>, field: &str) -> Result<()> {
    if value
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        Err(PolicyStoreError::Invalid(format!(
            "{field} cannot be empty"
        )))
    } else {
        Ok(())
    }
}
fn sha256(value: &str, field: &str) -> Result<()> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(PolicyStoreError::Invalid(format!(
            "{field} must be a SHA-256 digest"
        )))
    }
}
fn now() -> Result<String> {
    format_time(OffsetDateTime::now_utc())
}
fn format_time(value: OffsetDateTime) -> Result<String> {
    value
        .format(&Rfc3339)
        .map_err(|e| PolicyStoreError::Invalid(e.to_string()))
}
fn derived_id(prefix: &str, values: &[&str]) -> String {
    let mut digest = Sha256::new();
    for value in values {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    format!("{prefix}-{:x}", digest.finalize())
}
fn default_true() -> bool {
    true
}
fn default_lease_ttl_seconds() -> u64 {
    300
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_contracts::{
        PolicyRequestedLaunch, PolicyVehicle, OVERRIDE_SCHEMA, SPAWN_REQUEST_SCHEMA,
    };

    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn instant(value: &str) -> OffsetDateTime {
        OffsetDateTime::parse(value, &Rfc3339).unwrap()
    }

    fn store(label: &str) -> (PolicyStore, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "sm-policy-store-{label}-{}-{}.sqlite",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        (PolicyStore::new(&path).unwrap(), path)
    }

    fn bootstrap() -> BootstrapAuthority {
        BootstrapAuthority {
            session_id: "maintainer-1".into(),
            credential_fingerprint: "credential-1".into(),
            binding_digest: DIGEST.into(),
        }
    }

    fn projection(enabled: bool, capacity: u64) -> PolicyProjection {
        PolicyProjection {
            lane: "sm-policy-1268".into(),
            policy_version: "v1".into(),
            policy_digest: DIGEST.into(),
            enabled,
            bootstrap_authority: Some(bootstrap()),
            capacity_limits: vec![PolicyCapacityLimit {
                dimension: "concurrency".into(),
                key: "lane".into(),
                maximum_units: capacity,
            }],
            rules: vec![
                MaterializedPolicyRule {
                    clause_id: "sm-policy-1268.named-maintainer".into(),
                    rank: 10,
                    class_id: Some("named_orchestrator".into()),
                    role_id: Some("maintainer".into()),
                    vehicle: Some(PolicyVehicle::NamedSeat),
                    profile: PolicyLaunchProfile {
                        vehicle: PolicyVehicle::NamedSeat,
                        provider: "claude".into(),
                        model: "opus".into(),
                        effort: "high".into(),
                        context_profile: "standard".into(),
                    },
                    outcome: PolicyRuleOutcome::Allow,
                    overridable: true,
                    capacity_claims: vec![PolicyCapacityClaim {
                        dimension: "concurrency".into(),
                        key: "lane".into(),
                        units: 1,
                    }],
                    lease_ttl_seconds: 300,
                    reason: "canonical named role".into(),
                },
                MaterializedPolicyRule {
                    clause_id: "sm-policy-1268.routine-worker".into(),
                    rank: 20,
                    class_id: Some("routine_bounded".into()),
                    role_id: None,
                    vehicle: Some(PolicyVehicle::TaskAgent),
                    profile: PolicyLaunchProfile {
                        vehicle: PolicyVehicle::TaskAgent,
                        provider: "claude".into(),
                        model: "sonnet".into(),
                        effort: "high".into(),
                        context_profile: "standard".into(),
                    },
                    outcome: PolicyRuleOutcome::Allow,
                    overridable: true,
                    capacity_claims: vec![PolicyCapacityClaim {
                        dimension: "concurrency".into(),
                        key: "lane".into(),
                        units: 1,
                    }],
                    lease_ttl_seconds: 300,
                    reason: "routine canonical profile".into(),
                },
            ],
        }
    }

    fn request(
        id: &str,
        vehicle: PolicyVehicle,
        model: Option<&str>,
        intent: &str,
    ) -> PolicySpawnRequest {
        PolicySpawnRequest {
            schema: SPAWN_REQUEST_SCHEMA.into(),
            request_id: id.into(),
            caller: PolicyCallerBinding::IncarnationBootstrap {
                lane: "sm-policy-1268".into(),
                session_id: "maintainer-1".into(),
                credential_fingerprint: "credential-1".into(),
                binding_digest: DIGEST.into(),
            },
            prompt_digest: DIGEST.into(),
            launch_intent_id: intent.into(),
            policy_version: "v1".into(),
            policy_digest: DIGEST.into(),
            requested: PolicyRequestedLaunch {
                name: id.into(),
                vehicle,
                provider: "claude".into(),
                model: model.map(str::to_owned),
                effort: None,
                working_dir: "/tmp/work".into(),
                node: "local".into(),
            },
            topology_version: 1,
            capacity_version: 1,
            created_at: "2026-08-17T00:00:00Z".into(),
        }
    }

    fn class(class_id: &str, role_id: Option<&str>) -> PolicyClassification {
        PolicyClassification {
            class_id: class_id.into(),
            role_id: role_id.map(str::to_owned),
            turn_profile: "initial_task".into(),
            method: "deterministic".into(),
            confidence: "high".into(),
        }
    }

    #[test]
    fn omitted_models_for_aa6c1120_and_2260296e_resolve_to_explicit_profiles() {
        let (store, path) = store("omitted-models");
        store.install_projection(&projection(true, 2)).unwrap();
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        let aa6c1120 = request("aa6c1120", PolicyVehicle::TaskAgent, None, "intent-aa");
        let root = request("2260296e", PolicyVehicle::NamedSeat, None, "intent-root");
        store.create_request(&aa6c1120).unwrap();
        store.create_request(&root).unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        let worker = store
            .prepare_admission("aa6c1120", &class("routine_bounded", None), None, now)
            .unwrap()
            .decision;
        let named = store
            .prepare_admission(
                "2260296e",
                &class("named_orchestrator", Some("maintainer")),
                None,
                now,
            )
            .unwrap()
            .decision;
        assert_eq!(worker.resolved_profile.unwrap().model, "sonnet");
        assert_eq!(named.resolved_profile.unwrap().model, "opus");
        assert!(matches!(worker.outcome, PolicyDecisionOutcome::Allow));
        assert!(matches!(named.outcome, PolicyDecisionOutcome::Allow));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn disabled_or_wrong_bootstrap_canary_never_admits() {
        let (policy_store, path) = store("canary");
        policy_store
            .install_projection(&projection(false, 1))
            .unwrap();
        policy_store
            .set_runtime_versions("sm-policy-1268", 1, 1)
            .unwrap();
        let disabled_request = request(
            "disabled",
            PolicyVehicle::TaskAgent,
            None,
            "intent-disabled",
        );
        policy_store.create_request(&disabled_request).unwrap();
        assert!(matches!(
            policy_store.prepare_admission(
                "disabled",
                &class("routine_bounded", None),
                None,
                instant("2026-08-17T00:01:00Z")
            ),
            Err(PolicyStoreError::CanaryDenied(_))
        ));
        let (wrong_store, wrong_path) = store("wrong-bootstrap");
        wrong_store
            .install_projection(&projection(true, 1))
            .unwrap();
        wrong_store
            .set_runtime_versions("sm-policy-1268", 1, 1)
            .unwrap();
        let mut wrong = request("wrong", PolicyVehicle::TaskAgent, None, "intent-wrong");
        if let PolicyCallerBinding::IncarnationBootstrap {
            credential_fingerprint: fingerprint,
            ..
        } = &mut wrong.caller
        {
            *fingerprint = "wrong-credential".to_owned();
        }
        wrong_store.create_request(&wrong).unwrap();
        assert!(matches!(
            wrong_store.prepare_admission(
                "wrong",
                &class("routine_bounded", None),
                None,
                instant("2026-08-17T00:01:00Z")
            ),
            Err(PolicyStoreError::CanaryDenied(_))
        ));
        std::fs::remove_file(path).ok();
        std::fs::remove_file(wrong_path).ok();
    }

    #[test]
    fn capacity_is_atomic_and_stale_versions_fail_closed() {
        let (store, path) = store("capacity");
        store.install_projection(&projection(true, 1)).unwrap();
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        store
            .create_request(&request(
                "first",
                PolicyVehicle::TaskAgent,
                None,
                "intent-first",
            ))
            .unwrap();
        store
            .create_request(&request(
                "second",
                PolicyVehicle::TaskAgent,
                None,
                "intent-second",
            ))
            .unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        store
            .prepare_admission("first", &class("routine_bounded", None), None, now)
            .unwrap();
        assert!(matches!(
            store.prepare_admission("second", &class("routine_bounded", None), None, now),
            Err(PolicyStoreError::CapacityUnavailable(_))
        ));
        store.set_runtime_versions("sm-policy-1268", 2, 1).unwrap();
        assert!(matches!(
            store.prepare_admission("second", &class("routine_bounded", None), None, now),
            Err(PolicyStoreError::Stale(_))
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn exact_override_is_bound_consumed_once_and_persists() {
        let (store, path) = store("override");
        store.install_projection(&projection(true, 2)).unwrap();
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        let request = request(
            "rewrite-me",
            PolicyVehicle::NamedSeat,
            Some("fable"),
            "intent-rewrite",
        );
        store.create_request(&request).unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        let rejected = store
            .prepare_admission(
                "rewrite-me",
                &class("named_orchestrator", Some("maintainer")),
                None,
                now,
            )
            .unwrap()
            .decision;
        assert!(matches!(rejected.outcome, PolicyDecisionOutcome::Rewrite));
        let record = PolicyOverrideRecord {
            schema: OVERRIDE_SCHEMA.into(),
            override_id: "override-1".into(),
            request_id: request.request_id.clone(),
            request_digest: request.canonical_digest().unwrap(),
            decision_id: rejected.decision_id.clone(),
            policy_version: request.policy_version.clone(),
            issuer: request.caller.clone(),
            reason: "operator accepts exact model exception".into(),
            self_benefiting: true,
            state: PolicyOverrideState::Authorized,
            created_at: "2026-08-17T00:01:00Z".into(),
            expires_at: "2026-08-17T00:10:00Z".into(),
            consumed_at: None,
        };
        store.authorize_override(&record, now).unwrap();
        let allowed = store
            .prepare_admission(
                "rewrite-me",
                &class("named_orchestrator", Some("maintainer")),
                Some("override-1"),
                now,
            )
            .unwrap();
        assert!(matches!(
            allowed.decision.outcome,
            PolicyDecisionOutcome::Allow
        ));
        assert!(!allowed.reused);
        assert!(matches!(
            store.override_record("override-1").unwrap().state,
            PolicyOverrideState::Consumed
        ));
        assert!(matches!(
            store.prepare_admission(
                "rewrite-me",
                &class("named_orchestrator", Some("maintainer")),
                Some("override-1"),
                now
            ),
            Ok(PreparedDecision { reused: true, .. })
        ));
        let after_restart = PolicyStore::new(&path).unwrap();
        assert!(matches!(
            after_restart.override_record("override-1").unwrap().state,
            PolicyOverrideState::Consumed
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn conflicting_equal_rank_clauses_block_deterministically() {
        let (store, path) = store("conflict");
        let mut projection = projection(true, 1);
        let mut conflict = projection.rules[1].clone();
        conflict.clause_id = "sm-policy-1268.routine-worker-conflict".into();
        conflict.profile.model = "opus".into();
        projection.rules.push(conflict);
        store.install_projection(&projection).unwrap();
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        store
            .create_request(&request(
                "conflict",
                PolicyVehicle::TaskAgent,
                None,
                "intent-conflict",
            ))
            .unwrap();
        let decision = store
            .prepare_admission(
                "conflict",
                &class("routine_bounded", None),
                None,
                instant("2026-08-17T00:01:00Z"),
            )
            .unwrap()
            .decision;
        assert!(matches!(decision.outcome, PolicyDecisionOutcome::Block));
        assert_eq!(
            decision.applicable_clause_ids,
            vec![
                "sm-policy-1268.routine-worker".to_string(),
                "sm-policy-1268.routine-worker-conflict".to_string()
            ]
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn competing_reservations_cannot_both_consume_one_slot() {
        use std::sync::{Arc, Barrier};

        let (store, path) = store("concurrent-capacity");
        store.install_projection(&projection(true, 1)).unwrap();
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        store
            .create_request(&request(
                "racer-a",
                PolicyVehicle::TaskAgent,
                None,
                "intent-racer-a",
            ))
            .unwrap();
        store
            .create_request(&request(
                "racer-b",
                PolicyVehicle::TaskAgent,
                None,
                "intent-racer-b",
            ))
            .unwrap();
        let gate = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for request_id in ["racer-a", "racer-b"] {
            let store = store.clone();
            let gate = gate.clone();
            handles.push(std::thread::spawn(move || {
                gate.wait();
                store.prepare_admission(
                    request_id,
                    &class("routine_bounded", None),
                    None,
                    instant("2026-08-17T00:01:00Z"),
                )
            }));
        }
        gate.wait();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, Err(PolicyStoreError::CapacityUnavailable(_))))
                .count(),
            1
        );
        std::fs::remove_file(path).ok();
    }
}
