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
const POLICY_SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub enum PolicyStoreError {
    Schema(String),
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
            Self::Schema(detail) => write!(
                f,
                "policy.db is not the unshipped v1 canary schema ({detail}); archive {} and restart to recreate it. No in-place migration is supported",
                "the policy.db file"
            ),
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

/// The post-bootstrap equivalent of `BootstrapAuthority`.  A canary is never
/// widened merely because it has been migrated to stable-seat identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatAuthority {
    pub seat_key: String,
    pub generation: u64,
    pub holder_session_id: String,
}

impl SeatAuthority {
    fn matches(&self, caller: &PolicyCallerBinding) -> bool {
        matches!(caller, PolicyCallerBinding::Seat {
            seat_key,
            generation,
            holder_session_id,
            ..
        } if seat_key == &self.seat_key
            && generation == &self.generation
            && holder_session_id == &self.holder_session_id)
    }

    fn validate(&self, lane: &str) -> Result<()> {
        required(&self.seat_key, "seat authority seat_key")?;
        required(&self.holder_session_id, "seat authority holder_session_id")?;
        if self.generation == 0 || !self.seat_key.starts_with(&format!("{lane}-")) {
            return Err(PolicyStoreError::Invalid(
                "seat authority must name a current lane-prefixed seat generation".into(),
            ));
        }
        Ok(())
    }
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
    #[serde(default)]
    pub seat_authority: Option<SeatAuthority>,
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
        if let Some(authority) = &self.seat_authority {
            authority.validate(&self.lane)?;
        }
        if self.enabled && (self.bootstrap_authority.is_some() == self.seat_authority.is_some()) {
            return Err(PolicyStoreError::Invalid(
                "an enabled canary requires exactly one bootstrap or seat authority".into(),
            ));
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
            let mut claim_keys = std::collections::BTreeSet::new();
            for claim in &rule.capacity_claims {
                claim
                    .validate()
                    .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
                if !claim_keys.insert((&claim.dimension, &claim.key)) {
                    return Err(PolicyStoreError::Invalid(
                        "a rule cannot contain duplicate capacity claims".into(),
                    ));
                }
            }
            if (matches!(rule.outcome, PolicyRuleOutcome::Allow) || rule.overridable)
                && rule.capacity_claims.is_empty()
            {
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
        for rule in &self.rules {
            for claim in &rule.capacity_claims {
                if !self
                    .capacity_limits
                    .iter()
                    .any(|limit| limit.dimension == claim.dimension && limit.key == claim.key)
                {
                    return Err(PolicyStoreError::Invalid(format!(
                        "rule {} has no capacity limit for {}/{}",
                        rule.clause_id, claim.dimension, claim.key
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDecision {
    pub decision: PolicyDecision,
    pub reused: bool,
    /// Present only for allow. D3 must use this exact ID when creating its
    /// provisional core-session record and release it at completion/retirement.
    pub provisional_child_session_id: Option<String>,
    /// Durable lifecycle state of the returned child binding. Retries always
    /// return the original binding, including after it terminalizes.
    pub child_state: Option<PolicyChildState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyLeaseState {
    Active,
    Committed,
    ReleasePending,
    Released,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyChildState {
    Reserved,
    Launched,
    ReleasePending,
    Released,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyChildReservation {
    pub child_session_id: String,
    pub lease_id: String,
    pub request_id: String,
    pub decision_id: String,
    pub lease_state: PolicyLeaseState,
    pub child_state: PolicyChildState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyReconciliationSnapshot {
    pub expected: usize,
    pub reservations: Vec<PolicyChildReservation>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyChildAdmission {
    pub reservation: PolicyChildReservation,
    pub request: PolicySpawnRequest,
    pub decision: PolicyDecision,
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
        let mut conn = self.open()?;
        let projection_json = serde_json::to_string(projection)?;
        let projection_record_digest = canonical_json_digest(projection)?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO policy_projections (lane, policy_version, policy_digest, projection_json, projection_record_digest, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![projection.lane, projection.policy_version, projection.policy_digest, projection_json, projection_record_digest, now()?],
        )?;
        if inserted == 0 {
            let tx = conn.transaction()?;
            let existing = load_projection_tx(
                &tx,
                &projection.lane,
                &projection.policy_version,
                &projection.policy_digest,
            )?;
            if existing != *projection {
                return Err(PolicyStoreError::Invalid(
                    "policy version/digest is already bound to a different projection".into(),
                ));
            }
            tx.commit()?;
        }
        conn.execute(
            "INSERT INTO policy_runtime_versions (lane, topology_version, capacity_version) VALUES (?1, 0, 0) ON CONFLICT(lane) DO NOTHING",
            [&projection.lane],
        )?;
        Ok(())
    }

    /// Changes the active policy only after the immutable projection has been
    /// installed and validated. Installation itself never grants authority.
    pub fn activate_projection(
        &self,
        lane: &str,
        policy_version: &str,
        policy_digest: &str,
    ) -> Result<()> {
        required(lane, "projection lane")?;
        required(policy_version, "projection policy_version")?;
        sha256(policy_digest, "projection policy_digest")?;
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        load_projection_tx(&tx, lane, policy_version, policy_digest)?;
        tx.execute(
            "INSERT INTO policy_active_projections (lane, policy_version, policy_digest, active_projection_digest) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(lane) DO UPDATE SET policy_version = excluded.policy_version, policy_digest = excluded.policy_digest, active_projection_digest = excluded.active_projection_digest",
            params![lane, policy_version, policy_digest, active_projection_digest(lane, policy_version, policy_digest)?],
        )?;
        tx.commit()?;
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
        let mut conn = self.open()?;
        let json = serde_json::to_string(request)?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO policy_requests (request_id, lane, request_digest, request_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![request.request_id, request.caller.lane(), digest, json, request.created_at],
        )?;
        if inserted == 0 {
            let tx = conn.transaction()?;
            let existing = load_request_tx(&tx, &request.request_id)?;
            if existing != *request {
                return Err(PolicyStoreError::Invalid(
                    "request ID is already bound to different immutable input".into(),
                ));
            }
            tx.commit()?;
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
        provisional_child_session_id: &str,
        now: OffsetDateTime,
    ) -> Result<PreparedDecision> {
        required(provisional_child_session_id, "provisional child session ID")?;
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
        let active = load_active_projection_tx(&tx, request.caller.lane())?;
        if active
            != (
                request.policy_version.clone(),
                request.policy_digest.clone(),
            )
        {
            return Err(PolicyStoreError::Stale(
                "request policy projection is no longer active".into(),
            ));
        }
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
        if let Some(existing) = load_reusable_decision(&tx, &request, override_id)? {
            let reservation = reusable_child_binding(&tx, &existing)?;
            tx.commit()?;
            return Ok(PreparedDecision {
                decision: existing,
                reused: true,
                provisional_child_session_id: reservation
                    .as_ref()
                    .map(|reservation| reservation.child_session_id.clone()),
                child_state: reservation.map(|reservation| reservation.child_state),
            });
        }
        let attempt = next_attempt(&tx, request_id)?;
        let mut effective_classification = classification.clone();
        if let Some(override_id) = override_id {
            let override_record = load_override_tx(&tx, override_id)?;
            let terminal = load_terminal_decision(&tx, &request, &override_record.decision_id)?;
            override_record
                .validate_for_consumption(&request, &terminal, now)
                .map_err(|error| PolicyStoreError::OverrideDenied(error.to_string()))?;
            if classification != &terminal.classification {
                return Err(PolicyStoreError::OverrideDenied("override re-admission classification differs from its frozen terminal decision".into()));
            }
            effective_classification = terminal.classification;
        }
        let resolved = resolve_rule(&projection, &request, &effective_classification);
        let mut outcome = resolved.outcome.clone();
        let mut override_used = None;
        let mut override_terminal_decision_id = None;
        if let Some(override_id) = override_id {
            let override_record = load_override_tx(&tx, override_id)?;
            let terminal = load_terminal_decision(&tx, &request, &override_record.decision_id)?;
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
            override_terminal_decision_id = Some(override_record.decision_id);
        }
        let resolved_profile = if override_used.is_some() {
            resolved
                .profile
                .as_ref()
                .map(|profile| override_granted_profile(&request, profile))
        } else {
            resolved.profile.clone()
        };
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
            classification: effective_classification,
            applicable_clause_ids: resolved.clause_ids.clone(),
            resolved_profile,
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
            insert_lease(
                &tx,
                &lease,
                &decision.decision_id,
                provisional_child_session_id,
            )?;
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
        let decision_json = serde_json::to_string(&decision)?;
        tx.execute("INSERT INTO policy_decisions (decision_id, request_id, attempt, override_id, override_terminal_decision_id, decision_json, decision_digest, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![decision.decision_id, request_id, attempt, override_used, override_terminal_decision_id, decision_json, canonical_json_digest(&decision)?, decision.decided_at])?;
        if let Some(override_id) = override_used {
            consume_override_tx(&tx, override_id, now)?;
        }
        tx.commit()?;
        Ok(PreparedDecision {
            decision,
            reused: false,
            provisional_child_session_id: if matches!(outcome, PolicyDecisionOutcome::Allow) {
                Some(provisional_child_session_id.to_owned())
            } else {
                None
            },
            child_state: if matches!(outcome, PolicyDecisionOutcome::Allow) {
                Some(PolicyChildState::Reserved)
            } else {
                None
            },
        })
    }

    /// Authorizes a single exact frozen request. The record itself carries the
    /// authoritative cross-record binding; no caller supplied shortcut exists.
    /// Creates the only caller-facing exact-request override record.  All
    /// binding fields are loaded from the frozen request and terminal decision;
    /// D3 never reconstructs them from mutable caller input.
    pub fn authorize_request_override(
        &self,
        request_id: &str,
        issuer: &PolicyCallerBinding,
        reason: &str,
        now: OffsetDateTime,
        ttl: time::Duration,
    ) -> Result<PolicyOverrideRecord> {
        issuer
            .validate()
            .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
        required(reason, "override reason")?;
        if ttl <= time::Duration::ZERO {
            return Err(PolicyStoreError::Invalid(
                "override ttl must be positive".into(),
            ));
        }
        let expires_at = now.checked_add(ttl).ok_or_else(|| {
            PolicyStoreError::Invalid("override expiry overflows timestamp".into())
        })?;
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_overrides_tx(&tx, now)?;
        let request = load_request_tx(&tx, request_id)?;
        if issuer != &request.caller {
            return Err(PolicyStoreError::OverrideDenied(
                "override issuer does not match the frozen request caller".into(),
            ));
        }
        let decision = load_latest_terminal_decision(&tx, &request)?;
        if !matches!(
            decision.outcome,
            PolicyDecisionOutcome::Rewrite | PolicyDecisionOutcome::Block
        ) {
            return Err(PolicyStoreError::OverrideDenied(
                "only a current rewrite or block decision can be overridden".into(),
            ));
        }
        let record = PolicyOverrideRecord {
            schema: crate::policy_contracts::OVERRIDE_SCHEMA.into(),
            override_id: derived_id(
                "override",
                &[
                    &request
                        .canonical_digest()
                        .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?,
                    &decision.decision_id,
                ],
            ),
            request_id: request.request_id.clone(),
            request_digest: request
                .canonical_digest()
                .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?,
            decision_id: decision.decision_id.clone(),
            policy_version: request.policy_version.clone(),
            issuer: issuer.clone(),
            reason: reason.to_owned(),
            self_benefiting: true,
            state: PolicyOverrideState::Authorized,
            created_at: format_time(now)?,
            expires_at: format_time(expires_at)?,
            consumed_at: None,
        };
        authorize_override_tx(&tx, &record, now)?;
        tx.commit()?;
        Ok(record)
    }

    pub fn release_lease(&self, lease_id: &str) -> Result<()> {
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx
            .query_row(
                "SELECT lease_id FROM policy_capacity_leases WHERE lease_id = ?1",
                [lease_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .is_some()
        {
            load_lease_tx(&tx, lease_id)?;
        }
        let released_at = now()?;
        tx.execute("UPDATE policy_capacity_leases SET state = 'released', released_at = ?2 WHERE lease_id = ?1 AND state IN ('active', 'committed')", params![lease_id, released_at])?;
        tx.execute("UPDATE policy_provisional_children SET state = 'released', released_at = ?2 WHERE lease_id = ?1 AND state IN ('reserved', 'launched')", params![lease_id, now()?])?;
        tx.commit()?;
        Ok(())
    }

    /// Idempotent lifecycle cleanup for D3's completion, launch rejection, or
    /// retirement paths. It releases the exact child-bound lease only.
    pub fn release_by_child(&self, child_session_id: &str) -> Result<()> {
        required(child_session_id, "provisional child session ID")?;
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reservation = load_child_reservation_tx(&tx, child_session_id)?;
        if let Some(reservation) = reservation {
            if matches!(
                (reservation.child_state, reservation.lease_state),
                (PolicyChildState::Released, PolicyLeaseState::Released)
                    | (PolicyChildState::Expired, PolicyLeaseState::Expired)
            ) {
                tx.commit()?;
                return Ok(());
            }
            let released_at = now()?;
            let lease_changed = tx.execute("UPDATE policy_capacity_leases SET state = 'released', released_at = ?2 WHERE lease_id = ?1 AND state IN ('active', 'committed', 'release_pending')", params![reservation.lease_id, released_at])?;
            let child_changed = tx.execute("UPDATE policy_provisional_children SET state = 'released', released_at = ?2 WHERE child_session_id = ?1 AND state IN ('reserved', 'launched', 'release_pending')", params![child_session_id, now()?])?;
            if lease_changed != 1 || child_changed != 1 {
                return Err(PolicyStoreError::Invalid(
                    "child and lease could not be released from one matching lifecycle state"
                        .into(),
                ));
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Persists release intent before D3 stops or detaches the runtime. A crash
    /// after this point is resumed by startup reconciliation.
    pub fn mark_child_release_pending(&self, child_session_id: &str) -> Result<()> {
        required(child_session_id, "provisional child session ID")?;
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reservation = load_child_reservation_tx(&tx, child_session_id)?.ok_or_else(|| {
            PolicyStoreError::Invalid(
                "release-pending requires exactly one matching child reservation".into(),
            )
        })?;
        if reservation.child_state == PolicyChildState::ReleasePending
            && reservation.lease_state == PolicyLeaseState::ReleasePending
        {
            tx.commit()?;
            return Ok(());
        }
        let admissible = matches!(
            (reservation.child_state, reservation.lease_state),
            (PolicyChildState::Reserved, PolicyLeaseState::Active)
                | (PolicyChildState::Launched, PolicyLeaseState::Committed)
        );
        if !admissible {
            return Err(PolicyStoreError::Invalid(format!(
                "cannot begin release from child/lease states {:?}/{:?}",
                reservation.child_state, reservation.lease_state
            )));
        }
        let lease_changed = tx.execute(
            "UPDATE policy_capacity_leases SET state = 'release_pending' WHERE lease_id = ?1 AND state IN ('active', 'committed')",
            [&reservation.lease_id],
        )?;
        let child_changed = tx.execute(
            "UPDATE policy_provisional_children SET state = 'release_pending' WHERE child_session_id = ?1 AND state IN ('reserved', 'launched')",
            [child_session_id],
        )?;
        if lease_changed != 1 || child_changed != 1 {
            return Err(PolicyStoreError::Invalid(
                "release-pending transition did not update exactly one child and lease".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// D3 promotes a persisted same-store provisional child after its core
    /// runtime is materialized and attested. Committed leases never expire by
    /// reservation TTL and remain capacity-counted until release_by_child.
    pub fn mark_child_launched(&self, child_session_id: &str) -> Result<()> {
        required(child_session_id, "provisional child session ID")?;
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reservation = load_child_reservation_tx(&tx, child_session_id)?.ok_or_else(|| {
            PolicyStoreError::Invalid(
                "launch promotion requires exactly one matching child reservation".into(),
            )
        })?;
        if reservation.child_state != PolicyChildState::Reserved
            || reservation.lease_state != PolicyLeaseState::Active
        {
            return Err(PolicyStoreError::Invalid(format!(
                "launch promotion requires reserved/active state, found {:?}/{:?}",
                reservation.child_state, reservation.lease_state
            )));
        }
        let lease_changed = tx.execute(
            "UPDATE policy_capacity_leases SET state = 'committed' WHERE lease_id = ?1 AND state = 'active'",
            [&reservation.lease_id],
        )?;
        let child_changed = tx.execute(
            "UPDATE policy_provisional_children SET state = 'launched' WHERE child_session_id = ?1 AND state = 'reserved'",
            [child_session_id],
        )?;
        if lease_changed != 1 || child_changed != 1 {
            return Err(PolicyStoreError::Invalid(
                "launch promotion did not update exactly one matching child and lease".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn resolve_child(&self, child_session_id: &str) -> Result<Option<PolicyChildReservation>> {
        required(child_session_id, "provisional child session ID")?;
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        let reservation = load_child_reservation_tx(&tx, child_session_id)?;
        tx.commit()?;
        Ok(reservation)
    }

    pub fn admission_for_child(
        &self,
        child_session_id: &str,
    ) -> Result<Option<PolicyChildAdmission>> {
        required(child_session_id, "provisional child session ID")?;
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        let Some(reservation) = load_child_reservation_tx(&tx, child_session_id)? else {
            tx.commit()?;
            return Ok(None);
        };
        let request = load_request_tx(&tx, &reservation.request_id)?;
        let decision = load_terminal_decision(&tx, &request, &reservation.decision_id)?;
        tx.commit()?;
        Ok(Some(PolicyChildAdmission {
            reservation,
            request,
            decision,
        }))
    }

    /// Enumerates every non-terminal lease/child pair before admission is
    /// enabled. Structural mismatches are returned as exact blockers instead of
    /// being silently omitted from the sweep denominator.
    pub fn reconciliation_snapshot(&self) -> Result<PolicyReconciliationSnapshot> {
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        let expected_leases = tx.query_row(
            "SELECT COUNT(*) FROM policy_capacity_leases WHERE state IN ('active', 'committed', 'release_pending')",
            [],
            |row| row.get::<_, usize>(0),
        )?;
        let rows = tx
            .prepare("SELECT lease_id, provisional_child_session_id FROM policy_capacity_leases WHERE state IN ('active', 'committed', 'release_pending') ORDER BY lease_id")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut reservations = Vec::new();
        let mut blockers = Vec::new();
        for (lease_id, child_session_id) in rows {
            match load_child_reservation_tx(&tx, &child_session_id)? {
                Some(reservation) if reservation.lease_id == lease_id => {
                    if reservation_states_are_reconcilable(&reservation) {
                        reservations.push(reservation);
                    } else {
                        blockers.push(format!(
                            "child {} has ambiguous child/lease states {:?}/{:?}",
                            child_session_id, reservation.child_state, reservation.lease_state
                        ));
                    }
                }
                Some(reservation) => blockers.push(format!(
                    "lease {lease_id} points to child {child_session_id}, but that child binds lease {}",
                    reservation.lease_id
                )),
                None => blockers.push(format!(
                    "lease {lease_id} has no matching provisional child {child_session_id}"
                )),
            }
        }
        let orphan_children = tx
            .prepare("SELECT child_session_id, lease_id FROM policy_provisional_children WHERE state IN ('reserved', 'launched', 'release_pending') AND lease_id NOT IN (SELECT lease_id FROM policy_capacity_leases WHERE state IN ('active', 'committed', 'release_pending')) ORDER BY child_session_id")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let orphan_count = orphan_children.len();
        for (child_session_id, lease_id) in orphan_children {
            blockers.push(format!(
                "provisional child {child_session_id} has no matching non-terminal lease {lease_id}"
            ));
        }
        let expected = expected_leases + orphan_count;
        tx.commit()?;
        Ok(PolicyReconciliationSnapshot {
            expected,
            reservations,
            blockers,
        })
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
        let is_new = !self.db_path.exists();
        let conn = Connection::open(&self.db_path).map_err(|error| self.schema_error(error))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| self.schema_error(error))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| self.schema_error(error))?;
        if is_new {
            conn.execute_batch(POLICY_SCHEMA_V1)
                .map_err(|error| self.schema_error(error))?;
            conn.pragma_update(None, "user_version", POLICY_SCHEMA_VERSION)
                .map_err(|error| self.schema_error(error))?;
        }
        verify_schema_v1(&conn).map_err(|detail| self.schema_error(detail))?;
        Ok(conn)
    }

    fn schema_error(&self, error: impl fmt::Display) -> PolicyStoreError {
        PolicyStoreError::Schema(format!("{} at {}", error, self.db_path.display()))
    }
}

const POLICY_SCHEMA_V1: &str = r#"
    CREATE TABLE policy_projections (lane TEXT NOT NULL, policy_version TEXT NOT NULL, policy_digest TEXT NOT NULL, projection_json TEXT NOT NULL, projection_record_digest TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (lane, policy_version, policy_digest));
    CREATE TABLE policy_active_projections (lane TEXT PRIMARY KEY, policy_version TEXT NOT NULL, policy_digest TEXT NOT NULL, active_projection_digest TEXT NOT NULL);
    CREATE TABLE policy_runtime_versions (lane TEXT PRIMARY KEY, topology_version INTEGER NOT NULL, capacity_version INTEGER NOT NULL);
    CREATE TABLE policy_requests (request_id TEXT PRIMARY KEY, lane TEXT NOT NULL, request_digest TEXT NOT NULL, request_json TEXT NOT NULL, created_at TEXT NOT NULL);
    CREATE TABLE policy_decisions (decision_id TEXT PRIMARY KEY, request_id TEXT NOT NULL, attempt INTEGER NOT NULL, override_id TEXT, override_terminal_decision_id TEXT, decision_json TEXT NOT NULL, decision_digest TEXT NOT NULL, created_at TEXT NOT NULL, UNIQUE(request_id, attempt), UNIQUE(request_id, override_id));
    CREATE TABLE policy_capacity_leases (lease_id TEXT PRIMARY KEY, request_id TEXT NOT NULL, topology_version INTEGER NOT NULL, capacity_version INTEGER NOT NULL, provisional_child_session_id TEXT NOT NULL, state TEXT NOT NULL, expires_at TEXT NOT NULL, released_at TEXT, lease_json TEXT NOT NULL, lease_digest TEXT NOT NULL);
    CREATE TABLE policy_capacity_claims (lease_id TEXT NOT NULL, dimension TEXT NOT NULL, claim_key TEXT NOT NULL, units INTEGER NOT NULL, PRIMARY KEY (lease_id, dimension, claim_key));
    CREATE TABLE policy_provisional_children (child_session_id TEXT PRIMARY KEY, lease_id TEXT NOT NULL UNIQUE, request_id TEXT NOT NULL, decision_id TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL, released_at TEXT);
    CREATE TABLE policy_overrides (override_id TEXT PRIMARY KEY, request_id TEXT NOT NULL, request_digest TEXT NOT NULL, decision_id TEXT NOT NULL, policy_version TEXT NOT NULL, state TEXT NOT NULL, override_json TEXT NOT NULL, override_digest TEXT NOT NULL, created_at TEXT NOT NULL, expires_at TEXT NOT NULL, consumed_at TEXT);
    CREATE INDEX idx_policy_active_claims ON policy_capacity_claims(dimension, claim_key);
    CREATE INDEX idx_policy_leases_active ON policy_capacity_leases(state, expires_at);
    CREATE UNIQUE INDEX idx_policy_active_lease_child ON policy_capacity_leases(provisional_child_session_id) WHERE state = 'active';
"#;

fn verify_schema_v1(conn: &Connection) -> std::result::Result<(), String> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| format!("cannot read PRAGMA user_version: {error}"))?;
    if version != POLICY_SCHEMA_VERSION {
        return Err(format!(
            "expected PRAGMA user_version={POLICY_SCHEMA_VERSION}, found {version}"
        ));
    }
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|error| format!("integrity check failed: {error}"))?;
    if integrity != "ok" {
        return Err(format!("integrity check failed: {integrity}"));
    }
    let mut expected = POLICY_SCHEMA_V1
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .map(normalize_schema_sql)
        .collect::<Vec<_>>();
    let mut actual = conn
        .prepare("SELECT sql FROM sqlite_master WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(|error| format!("cannot inspect v1 schema: {error}"))?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("cannot read v1 schema: {error}"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read v1 schema: {error}"))?
        .into_iter()
        .map(|statement| normalize_schema_sql(&statement))
        .collect::<Vec<_>>();
    expected.sort();
    actual.sort();
    if actual != expected {
        return Err("v1 schema objects or constraints differ from the frozen definition".into());
    }
    Ok(())
}

fn normalize_schema_sql(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
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
            || rule.overridable != first.overridable
            || rule.lease_ttl_seconds != first.lease_ttl_seconds
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
        || request.requested.model.as_deref() != Some(profile.model.as_str())
        || request.requested.effort.as_deref() != Some(profile.effort.as_str())
}

fn override_granted_profile(
    request: &PolicySpawnRequest,
    rewrite_target: &PolicyLaunchProfile,
) -> PolicyLaunchProfile {
    match (
        request.requested.model.as_deref(),
        request.requested.effort.as_deref(),
    ) {
        (Some(model), Some(effort)) => PolicyLaunchProfile {
            vehicle: request.requested.vehicle.clone(),
            provider: request.requested.provider.clone(),
            model: model.to_owned(),
            effort: effort.to_owned(),
            // Context profiles are policy-owned and are not caller supplied.
            context_profile: rewrite_target.context_profile.clone(),
        },
        _ => rewrite_target.clone(),
    }
}

fn authorize_caller(projection: &PolicyProjection, request: &PolicySpawnRequest) -> Result<()> {
    if !projection.enabled {
        return Err(PolicyStoreError::CanaryDenied(
            "projection is disabled".into(),
        ));
    }
    match &request.caller {
        PolicyCallerBinding::Seat { .. } => projection
            .seat_authority
            .as_ref()
            .filter(|authority| authority.matches(&request.caller))
            .map(|_| ())
            .ok_or_else(|| {
                PolicyStoreError::CanaryDenied(
                    "seat caller does not match externally configured canary authority".into(),
                )
            }),
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
        let lease_ids = tx
            .prepare(r#"SELECT DISTINCT l.lease_id FROM policy_capacity_leases l JOIN policy_capacity_claims c ON c.lease_id = l.lease_id WHERE c.dimension = ?1 AND c.claim_key = ?2 AND l.state IN ('active', 'committed')"#)?
            .query_map(params![dimension, key], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let active = lease_ids.into_iter().try_fold(0_u64, |total, lease_id| {
            let lease = load_lease_tx(tx, &lease_id)?;
            let units = lease
                .claims
                .iter()
                .filter(|claim| claim.dimension == dimension && claim.key == key)
                .map(|claim| claim.units)
                .sum::<u64>();
            Ok::<_, PolicyStoreError>(total.saturating_add(units))
        })?;
        if active.saturating_add(units) > maximum {
            return Err(PolicyStoreError::CapacityUnavailable(format!(
                "{dimension}/{key}: requested {units}, active {active}, maximum {maximum}"
            )));
        }
    }
    Ok(())
}

fn insert_lease(
    tx: &Transaction<'_>,
    lease: &PolicyCapacityLease,
    decision_id: &str,
    provisional_child_session_id: &str,
) -> Result<()> {
    tx.execute("INSERT INTO policy_capacity_leases (lease_id, request_id, topology_version, capacity_version, provisional_child_session_id, state, expires_at, lease_json, lease_digest) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?8)", params![lease.lease_id, lease.request_id, lease.topology_version, lease.capacity_version, provisional_child_session_id, lease.expires_at, serde_json::to_string(lease)?, canonical_json_digest(lease)?])?;
    tx.execute("INSERT INTO policy_provisional_children (child_session_id, lease_id, request_id, decision_id, state, created_at) VALUES (?1, ?2, ?3, ?4, 'reserved', ?5)", params![provisional_child_session_id, lease.lease_id, lease.request_id, decision_id, now()?])?;
    for claim in &lease.claims {
        tx.execute("INSERT INTO policy_capacity_claims (lease_id, dimension, claim_key, units) VALUES (?1, ?2, ?3, ?4)", params![lease.lease_id, claim.dimension, claim.key, claim.units])?;
    }
    Ok(())
}

fn expire_leases(tx: &Transaction<'_>, now: OffsetDateTime) -> Result<()> {
    let now = format_time(now)?;
    let lease_ids = tx
        .prepare("SELECT lease_id FROM policy_capacity_leases WHERE state = 'active' AND expires_at <= ?1")?
        .query_map([&now], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for lease_id in lease_ids {
        load_lease_tx(tx, &lease_id)?;
    }
    tx.execute("UPDATE policy_capacity_leases SET state = 'expired' WHERE state = 'active' AND expires_at <= ?1", [&now])?;
    tx.execute("UPDATE policy_provisional_children SET state = 'expired' WHERE state = 'reserved' AND lease_id IN (SELECT lease_id FROM policy_capacity_leases WHERE state = 'expired')", [])?;
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
    let changed = tx.execute("UPDATE policy_overrides SET state = 'consumed', override_json = ?2, override_digest = ?3, consumed_at = ?4 WHERE override_id = ?1 AND state = 'authorized'", params![override_id, serde_json::to_string(&record)?, canonical_json_digest(&record)?, record.consumed_at])?;
    if changed != 1 {
        return Err(PolicyStoreError::OverrideDenied(
            "override was concurrently consumed".into(),
        ));
    }
    Ok(())
}

fn expire_overrides_tx(tx: &Transaction<'_>, now: OffsetDateTime) -> Result<usize> {
    let mut statement =
        tx.prepare("SELECT override_id FROM policy_overrides WHERE state = 'authorized'")?;
    let records = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    let mut count = 0;
    for id in records {
        let mut record = load_override_tx(tx, &id)?;
        let expiry = OffsetDateTime::parse(&record.expires_at, &Rfc3339)
            .map_err(|e| PolicyStoreError::Invalid(format!("invalid override expiry: {e}")))?;
        if now >= expiry {
            record.state = PolicyOverrideState::Expired;
            tx.execute("UPDATE policy_overrides SET state = 'expired', override_json = ?2, override_digest = ?3 WHERE override_id = ?1 AND state = 'authorized'", params![id, serde_json::to_string(&record)?, canonical_json_digest(&record)?])?;
            count += 1;
        }
    }
    Ok(count)
}

fn load_request(conn: &Connection, request_id: &str) -> Result<PolicySpawnRequest> {
    let (lane, digest, value, created_at): (String, String, String, String) = conn
        .query_row(
            "SELECT lane, request_digest, request_json, created_at FROM policy_requests WHERE request_id = ?1",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::RequestNotFound(request_id.into()))?;
    parse_stored_request(request_id, &lane, &digest, &value, &created_at)
}
fn load_request_tx(tx: &Transaction<'_>, request_id: &str) -> Result<PolicySpawnRequest> {
    let (lane, digest, value, created_at): (String, String, String, String) = tx
        .query_row(
            "SELECT lane, request_digest, request_json, created_at FROM policy_requests WHERE request_id = ?1",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::RequestNotFound(request_id.into()))?;
    parse_stored_request(request_id, &lane, &digest, &value, &created_at)
}
fn load_projection_tx(
    tx: &Transaction<'_>,
    lane: &str,
    version: &str,
    digest: &str,
) -> Result<PolicyProjection> {
    let (stored_lane, stored_version, stored_digest, value, record_digest): (String, String, String, String, String) = tx.query_row("SELECT lane, policy_version, policy_digest, projection_json, projection_record_digest FROM policy_projections WHERE lane = ?1 AND policy_version = ?2 AND policy_digest = ?3", params![lane, version, digest], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).optional()?.ok_or_else(|| PolicyStoreError::ProjectionNotFound(lane.into()))?;
    let projection: PolicyProjection = serde_json::from_str(&value)?;
    projection.validate()?;
    if canonical_json_digest(&projection)? != record_digest
        || projection.lane != stored_lane
        || projection.policy_version != stored_version
        || projection.policy_digest != stored_digest
    {
        return Err(PolicyStoreError::Invalid(
            "stored projection does not match its immutable lookup key".into(),
        ));
    }
    Ok(projection)
}

fn load_active_projection_tx(tx: &Transaction<'_>, lane: &str) -> Result<(String, String)> {
    let (stored_lane, policy_version, policy_digest, digest): (String, String, String, String) = tx
        .query_row(
            "SELECT lane, policy_version, policy_digest, active_projection_digest FROM policy_active_projections WHERE lane = ?1",
            [lane],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::ProjectionNotFound(lane.into()))?;
    if stored_lane != lane
        || active_projection_digest(&stored_lane, &policy_version, &policy_digest)? != digest
    {
        return Err(PolicyStoreError::Invalid(
            "active projection pointer does not match its canonical digest".into(),
        ));
    }
    load_projection_tx(tx, &stored_lane, &policy_version, &policy_digest)?;
    Ok((policy_version, policy_digest))
}

fn parse_stored_request(
    request_id: &str,
    lane: &str,
    digest: &str,
    value: &str,
    created_at: &str,
) -> Result<PolicySpawnRequest> {
    let request: PolicySpawnRequest = serde_json::from_str(value)?;
    request
        .validate()
        .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
    if request.request_id != request_id
        || request.caller.lane() != lane
        || request.created_at != created_at
        || request
            .canonical_digest()
            .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?
            != digest
    {
        return Err(PolicyStoreError::Invalid(
            "stored request does not match its immutable lookup key".into(),
        ));
    }
    Ok(request)
}
fn load_override_conn(conn: &Connection, override_id: &str) -> Result<PolicyOverrideRecord> {
    let (request_id, request_digest, decision_id, policy_version, state, value, digest, created_at, expires_at, consumed_at): (String, String, String, String, String, String, String, String, String, Option<String>) = conn
        .query_row(
            "SELECT request_id, request_digest, decision_id, policy_version, state, override_json, override_digest, created_at, expires_at, consumed_at FROM policy_overrides WHERE override_id = ?1",
            [override_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?)),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::OverrideNotFound(override_id.into()))?;
    parse_stored_override(
        override_id,
        &request_id,
        &request_digest,
        &decision_id,
        &policy_version,
        &state,
        &value,
        &digest,
        &created_at,
        &expires_at,
        consumed_at.as_deref(),
    )
}
fn load_override_tx(tx: &Transaction<'_>, override_id: &str) -> Result<PolicyOverrideRecord> {
    let (request_id, request_digest, decision_id, policy_version, state, value, digest, created_at, expires_at, consumed_at): (String, String, String, String, String, String, String, String, String, Option<String>) = tx
        .query_row(
            "SELECT request_id, request_digest, decision_id, policy_version, state, override_json, override_digest, created_at, expires_at, consumed_at FROM policy_overrides WHERE override_id = ?1",
            [override_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?)),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::OverrideNotFound(override_id.into()))?;
    parse_stored_override(
        override_id,
        &request_id,
        &request_digest,
        &decision_id,
        &policy_version,
        &state,
        &value,
        &digest,
        &created_at,
        &expires_at,
        consumed_at.as_deref(),
    )
}

#[allow(clippy::too_many_arguments)]
fn parse_stored_override(
    override_id: &str,
    request_id: &str,
    request_digest: &str,
    decision_id: &str,
    policy_version: &str,
    state: &str,
    value: &str,
    digest: &str,
    created_at: &str,
    expires_at: &str,
    consumed_at: Option<&str>,
) -> Result<PolicyOverrideRecord> {
    let record: PolicyOverrideRecord = serde_json::from_str(value)?;
    record
        .validate()
        .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
    if canonical_json_digest(&record)? != digest
        || record.override_id != override_id
        || record.request_id != request_id
        || record.request_digest != request_digest
        || record.decision_id != decision_id
        || record.policy_version != policy_version
        || override_state_name(&record.state) != state
        || record.created_at != created_at
        || record.expires_at != expires_at
        || record.consumed_at.as_deref() != consumed_at
    {
        return Err(PolicyStoreError::Invalid(
            "stored override does not match authoritative row columns".into(),
        ));
    }
    Ok(record)
}

fn load_terminal_decision(
    tx: &Transaction<'_>,
    request: &PolicySpawnRequest,
    decision_id: &str,
) -> Result<PolicyDecision> {
    let (stored_decision_id, stored_request_id, attempt, override_id, override_terminal_decision_id, digest, value, created_at): (String, String, u32, Option<String>, Option<String>, String, String, String) = tx
        .query_row(
            "SELECT decision_id, request_id, attempt, override_id, override_terminal_decision_id, decision_digest, decision_json, created_at FROM policy_decisions WHERE request_id = ?1 AND decision_id = ?2",
            params![request.request_id, decision_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
        )
        .optional()?
        .ok_or_else(|| {
            PolicyStoreError::OverrideDenied("override points to a missing decision".into())
        })?;
    parse_stored_decision(
        tx,
        request,
        &stored_decision_id,
        &stored_request_id,
        attempt,
        override_id.as_deref(),
        override_terminal_decision_id.as_deref(),
        &digest,
        &value,
        &created_at,
    )
}

fn load_latest_terminal_decision(
    tx: &Transaction<'_>,
    request: &PolicySpawnRequest,
) -> Result<PolicyDecision> {
    let (decision_id, request_id, attempt, override_id, override_terminal_decision_id, digest, value, created_at): (String, String, u32, Option<String>, Option<String>, String, String, String) = tx
        .query_row(
            "SELECT decision_id, request_id, attempt, override_id, override_terminal_decision_id, decision_digest, decision_json, created_at FROM policy_decisions WHERE request_id = ?1 ORDER BY attempt DESC LIMIT 1",
            [&request.request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::OverrideDenied("request has no terminal decision".into()))?;
    parse_stored_decision(
        tx,
        request,
        &decision_id,
        &request_id,
        attempt,
        override_id.as_deref(),
        override_terminal_decision_id.as_deref(),
        &digest,
        &value,
        &created_at,
    )
}

#[allow(clippy::too_many_arguments)]
fn parse_stored_decision(
    tx: &Transaction<'_>,
    request: &PolicySpawnRequest,
    decision_id: &str,
    request_id: &str,
    attempt: u32,
    override_id: Option<&str>,
    override_terminal_decision_id: Option<&str>,
    digest: &str,
    value: &str,
    created_at: &str,
) -> Result<PolicyDecision> {
    let decision: PolicyDecision = serde_json::from_str(value)?;
    decision
        .validate_for_request(request)
        .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
    if canonical_json_digest(&decision)? != digest
        || decision.decision_id != decision_id
        || decision.request_id != request_id
        || decision.attempt != attempt
        || decision.decided_at != created_at
    {
        return Err(PolicyStoreError::Invalid(
            "stored decision does not match authoritative row columns".into(),
        ));
    }
    if let (Some(override_id), Some(override_terminal_decision_id)) =
        (override_id, override_terminal_decision_id)
    {
        let override_record = load_override_tx(tx, override_id)?;
        if override_record.request_id != request.request_id
            || override_record.policy_version != request.policy_version
            || override_record.decision_id != override_terminal_decision_id
        {
            return Err(PolicyStoreError::Invalid(
                "stored decision override does not bind to this frozen request".into(),
            ));
        }
    } else if override_id.is_some() || override_terminal_decision_id.is_some() {
        return Err(PolicyStoreError::Invalid(
            "stored decision has an incomplete override binding".into(),
        ));
    }
    if let Some(embedded_lease) = &decision.capacity_lease {
        let stored_lease = load_lease_tx(tx, &embedded_lease.lease_id)?;
        if stored_lease != *embedded_lease {
            return Err(PolicyStoreError::Invalid(
                "decision capacity lease does not match authoritative lease row".into(),
            ));
        }
    }
    Ok(decision)
}

fn load_lease_tx(tx: &Transaction<'_>, lease_id: &str) -> Result<PolicyCapacityLease> {
    let (request_id, topology_version, capacity_version, state, expires_at, value, digest): (String, u64, u64, String, String, String, String) = tx
        .query_row(
            "SELECT request_id, topology_version, capacity_version, state, expires_at, lease_json, lease_digest FROM policy_capacity_leases WHERE lease_id = ?1",
            [lease_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::Invalid("decision points to a missing capacity lease".into()))?;
    let lease: PolicyCapacityLease = serde_json::from_str(&value)?;
    lease
        .validate()
        .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
    if canonical_json_digest(&lease)? != digest
        || lease.lease_id != lease_id
        || lease.request_id != request_id
        || lease.topology_version != topology_version
        || lease.capacity_version != capacity_version
        || lease.expires_at != expires_at
    {
        return Err(PolicyStoreError::Invalid(
            "stored capacity lease does not match authoritative row columns".into(),
        ));
    }
    let mut claims = tx
        .prepare("SELECT dimension, claim_key, units FROM policy_capacity_claims WHERE lease_id = ?1 ORDER BY dimension, claim_key")?
        .query_map([lease_id], |row| {
            Ok(PolicyCapacityClaim {
                dimension: row.get(0)?,
                key: row.get(1)?,
                units: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut expected_claims = lease.claims.clone();
    claims.sort_by(|a, b| (&a.dimension, &a.key).cmp(&(&b.dimension, &b.key)));
    expected_claims.sort_by(|a, b| (&a.dimension, &a.key).cmp(&(&b.dimension, &b.key)));
    if claims != expected_claims
        || !matches!(
            state.as_str(),
            "active" | "committed" | "release_pending" | "released" | "expired"
        )
    {
        return Err(PolicyStoreError::Invalid(
            "stored capacity lease claims or state are invalid".into(),
        ));
    }
    Ok(lease)
}

fn load_child_reservation_tx(
    tx: &Transaction<'_>,
    child_session_id: &str,
) -> Result<Option<PolicyChildReservation>> {
    let rows = tx
        .prepare(
            "SELECT p.lease_id, p.request_id, p.decision_id, p.state, l.state FROM policy_provisional_children p JOIN policy_capacity_leases l ON l.lease_id = p.lease_id WHERE p.child_session_id = ?1",
        )?
        .query_map([child_session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if rows.len() > 1 {
        return Err(PolicyStoreError::Invalid(
            "provisional child matched more than one lease".into(),
        ));
    }
    let Some((lease_id, request_id, decision_id, child_state, lease_state)) =
        rows.into_iter().next()
    else {
        return Ok(None);
    };
    let child_state = parse_child_state(&child_state)?;
    let lease_state = parse_lease_state(&lease_state)?;
    let request = load_request_tx(tx, &request_id)?;
    let decision = load_terminal_decision(tx, &request, &decision_id)?;
    let lease = load_lease_tx(tx, &lease_id)?;
    if decision.request_id != request_id
        || lease.request_id != request_id
        || decision
            .capacity_lease
            .as_ref()
            .map(|embedded| embedded.lease_id.as_str())
            != Some(lease_id.as_str())
    {
        return Err(PolicyStoreError::Invalid(
            "provisional child does not match its decision and capacity lease".into(),
        ));
    }
    Ok(Some(PolicyChildReservation {
        child_session_id: child_session_id.to_owned(),
        lease_id,
        request_id,
        decision_id,
        lease_state,
        child_state,
    }))
}

fn parse_lease_state(value: &str) -> Result<PolicyLeaseState> {
    match value {
        "active" => Ok(PolicyLeaseState::Active),
        "committed" => Ok(PolicyLeaseState::Committed),
        "release_pending" => Ok(PolicyLeaseState::ReleasePending),
        "released" => Ok(PolicyLeaseState::Released),
        "expired" => Ok(PolicyLeaseState::Expired),
        _ => Err(PolicyStoreError::Invalid(format!(
            "unknown capacity lease state {value}"
        ))),
    }
}

fn parse_child_state(value: &str) -> Result<PolicyChildState> {
    match value {
        "reserved" => Ok(PolicyChildState::Reserved),
        "launched" => Ok(PolicyChildState::Launched),
        "release_pending" => Ok(PolicyChildState::ReleasePending),
        "released" => Ok(PolicyChildState::Released),
        "expired" => Ok(PolicyChildState::Expired),
        _ => Err(PolicyStoreError::Invalid(format!(
            "unknown provisional child state {value}"
        ))),
    }
}

fn reservation_states_are_reconcilable(reservation: &PolicyChildReservation) -> bool {
    matches!(
        (reservation.child_state, reservation.lease_state),
        (PolicyChildState::Reserved, PolicyLeaseState::Active)
            | (PolicyChildState::Launched, PolicyLeaseState::Committed)
            | (
                PolicyChildState::ReleasePending,
                PolicyLeaseState::ReleasePending
            )
    )
}

fn authorize_override_tx(
    tx: &Transaction<'_>,
    record: &PolicyOverrideRecord,
    now: OffsetDateTime,
) -> Result<()> {
    record
        .validate()
        .map_err(|error| PolicyStoreError::Invalid(error.to_string()))?;
    if !matches!(record.state, PolicyOverrideState::Authorized) {
        return Err(PolicyStoreError::Invalid(
            "new override must start authorized".into(),
        ));
    }
    let request = load_request_tx(tx, &record.request_id)?;
    let decision = load_terminal_decision(tx, &request, &record.decision_id)?;
    record
        .validate_for_consumption(&request, &decision, now)
        .map_err(|error| PolicyStoreError::OverrideDenied(error.to_string()))?;
    let json = serde_json::to_string(record)?;
    let inserted = tx.execute("INSERT OR IGNORE INTO policy_overrides (override_id, request_id, request_digest, decision_id, policy_version, state, override_json, override_digest, created_at, expires_at, consumed_at) VALUES (?1, ?2, ?3, ?4, ?5, 'authorized', ?6, ?7, ?8, ?9, ?10)", params![record.override_id, record.request_id, record.request_digest, record.decision_id, record.policy_version, json, canonical_json_digest(record)?, record.created_at, record.expires_at, record.consumed_at])?;
    if inserted == 0 {
        let existing = load_override_tx(tx, &record.override_id)?;
        if existing != *record {
            return Err(PolicyStoreError::Invalid(
                "override ID is already bound to different immutable authorization".into(),
            ));
        }
    }
    Ok(())
}

fn load_reusable_decision(
    tx: &Transaction<'_>,
    request: &PolicySpawnRequest,
    override_id: Option<&str>,
) -> Result<Option<PolicyDecision>> {
    let value = match override_id {
        Some(id) => tx.query_row("SELECT decision_id, request_id, attempt, override_id, override_terminal_decision_id, decision_digest, decision_json, created_at FROM policy_decisions WHERE request_id = ?1 AND override_id = ?2", params![request.request_id, id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, Option<String>>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?, row.get::<_, String>(7)?))).optional()?,
        None => tx.query_row("SELECT decision_id, request_id, attempt, override_id, override_terminal_decision_id, decision_digest, decision_json, created_at FROM policy_decisions WHERE request_id = ?1 AND override_id IS NULL ORDER BY attempt ASC LIMIT 1", [&request.request_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, Option<String>>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?, row.get::<_, String>(7)?))).optional()?,
    };
    let Some((
        stored_decision_id,
        stored_request_id,
        attempt,
        stored_override_id,
        stored_override_terminal_decision_id,
        digest,
        json,
        created_at,
    )) = value
    else {
        return Ok(None);
    };
    let decision = parse_stored_decision(
        tx,
        request,
        &stored_decision_id,
        &stored_request_id,
        attempt,
        stored_override_id.as_deref(),
        stored_override_terminal_decision_id.as_deref(),
        &digest,
        &json,
        &created_at,
    )?;
    Ok(Some(decision))
}

fn reusable_child_binding(
    tx: &Transaction<'_>,
    decision: &PolicyDecision,
) -> Result<Option<PolicyChildReservation>> {
    let Some(lease) = &decision.capacity_lease else {
        return Ok(None);
    };
    let child_session_id: String = tx
        .query_row(
            "SELECT child_session_id FROM policy_provisional_children WHERE lease_id = ?1 AND request_id = ?2 AND decision_id = ?3",
            params![lease.lease_id, decision.request_id, decision.decision_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| PolicyStoreError::Invalid(
            "active reusable lease has no matching provisional child reservation".into(),
        ))?;
    let reservation = load_child_reservation_tx(tx, &child_session_id)?.ok_or_else(|| {
        PolicyStoreError::Invalid("reusable decision has no matching child reservation".into())
    })?;
    Ok(Some(reservation))
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
fn canonical_json_digest(value: &impl Serialize) -> Result<String> {
    serde_json::to_vec(value)
        .map(|encoded| format!("{:x}", Sha256::digest(encoded)))
        .map_err(PolicyStoreError::Json)
}
fn active_projection_digest(
    lane: &str,
    policy_version: &str,
    policy_digest: &str,
) -> Result<String> {
    canonical_json_digest(&(lane, policy_version, policy_digest))
}
fn override_state_name(state: &PolicyOverrideState) -> &'static str {
    match state {
        PolicyOverrideState::Authorized => "authorized",
        PolicyOverrideState::Consumed => "consumed",
        PolicyOverrideState::Expired => "expired",
        PolicyOverrideState::Rejected => "rejected",
    }
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
    use crate::policy_contracts::{PolicyRequestedLaunch, PolicyVehicle, SPAWN_REQUEST_SCHEMA};

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
            seat_authority: None,
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

    fn install(store: &PolicyStore, projection: &PolicyProjection) {
        store.install_projection(projection).unwrap();
        store
            .activate_projection(
                &projection.lane,
                &projection.policy_version,
                &projection.policy_digest,
            )
            .unwrap();
    }

    fn request(
        id: &str,
        vehicle: PolicyVehicle,
        model: Option<&str>,
        intent: &str,
    ) -> PolicySpawnRequest {
        let canonical_model = match vehicle {
            PolicyVehicle::NamedSeat => "opus",
            PolicyVehicle::TaskAgent => "sonnet",
        };
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
                model: Some(model.unwrap_or(canonical_model).to_owned()),
                effort: Some("high".into()),
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
        install(&store, &projection(true, 2));
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        let mut aa6c1120 = request("aa6c1120", PolicyVehicle::TaskAgent, None, "intent-aa");
        aa6c1120.requested.model = None;
        aa6c1120.requested.effort = None;
        let mut root = request("2260296e", PolicyVehicle::NamedSeat, None, "intent-root");
        root.requested.model = None;
        root.requested.effort = None;
        store.create_request(&aa6c1120).unwrap();
        store.create_request(&root).unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        let worker = store
            .prepare_admission(
                "aa6c1120",
                &class("routine_bounded", None),
                None,
                "child-aa",
                now,
            )
            .unwrap()
            .decision;
        let named = store
            .prepare_admission(
                "2260296e",
                &class("named_orchestrator", Some("maintainer")),
                None,
                "child-root",
                now,
            )
            .unwrap()
            .decision;
        assert_eq!(worker.resolved_profile.unwrap().model, "sonnet");
        assert_eq!(named.resolved_profile.unwrap().model, "opus");
        assert!(matches!(worker.outcome, PolicyDecisionOutcome::Rewrite));
        assert!(matches!(named.outcome, PolicyDecisionOutcome::Rewrite));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn disabled_or_wrong_bootstrap_canary_never_admits() {
        let (policy_store, path) = store("canary");
        install(&policy_store, &projection(false, 1));
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
                "child-disabled",
                instant("2026-08-17T00:01:00Z")
            ),
            Err(PolicyStoreError::CanaryDenied(_))
        ));
        let (wrong_store, wrong_path) = store("wrong-bootstrap");
        install(&wrong_store, &projection(true, 1));
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
                "child-wrong",
                instant("2026-08-17T00:01:00Z")
            ),
            Err(PolicyStoreError::CanaryDenied(_))
        ));
        let (seat_store, seat_path) = store("unscoped-seat");
        install(&seat_store, &projection(true, 1));
        seat_store
            .set_runtime_versions("sm-policy-1268", 1, 1)
            .unwrap();
        let mut seat_request = request("seat", PolicyVehicle::TaskAgent, None, "intent-seat");
        seat_request.caller = PolicyCallerBinding::Seat {
            lane: "sm-policy-1268".into(),
            seat_key: "sm-policy-1268-maintainer".into(),
            generation: 1,
            holder_session_id: "maintainer-1".into(),
        };
        seat_store.create_request(&seat_request).unwrap();
        assert!(matches!(
            seat_store.prepare_admission(
                "seat",
                &class("routine_bounded", None),
                None,
                "child-seat",
                instant("2026-08-17T00:01:00Z")
            ),
            Err(PolicyStoreError::CanaryDenied(_))
        ));
        std::fs::remove_file(path).ok();
        std::fs::remove_file(wrong_path).ok();
        std::fs::remove_file(seat_path).ok();
    }

    #[test]
    fn capacity_is_atomic_and_stale_versions_fail_closed() {
        let (store, path) = store("capacity");
        install(&store, &projection(true, 1));
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
            .prepare_admission(
                "first",
                &class("routine_bounded", None),
                None,
                "child-first",
                now,
            )
            .unwrap();
        assert!(matches!(
            store.prepare_admission(
                "second",
                &class("routine_bounded", None),
                None,
                "child-second",
                now
            ),
            Err(PolicyStoreError::CapacityUnavailable(_))
        ));
        store.set_runtime_versions("sm-policy-1268", 2, 1).unwrap();
        assert!(matches!(
            store.prepare_admission(
                "second",
                &class("routine_bounded", None),
                None,
                "child-second",
                now
            ),
            Err(PolicyStoreError::Stale(_))
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn exact_override_is_bound_consumed_once_and_persists() {
        let (store, path) = store("override");
        install(&store, &projection(true, 2));
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
                "child-rewrite",
                now,
            )
            .unwrap()
            .decision;
        assert!(matches!(rejected.outcome, PolicyDecisionOutcome::Rewrite));
        let record = store
            .authorize_request_override(
                "rewrite-me",
                &request.caller,
                "operator accepts exact model exception",
                now,
                time::Duration::minutes(9),
            )
            .unwrap();
        let allowed = store
            .prepare_admission(
                "rewrite-me",
                &class("named_orchestrator", Some("maintainer")),
                Some(&record.override_id),
                "child-override",
                now,
            )
            .unwrap();
        assert!(matches!(
            allowed.decision.outcome,
            PolicyDecisionOutcome::Allow
        ));
        assert_eq!(
            allowed.decision.resolved_profile.as_ref().unwrap().model,
            "fable",
            "a fully specified exact-request override grants the frozen requested model"
        );
        assert!(!allowed.reused);
        assert!(matches!(
            store.override_record(&record.override_id).unwrap().state,
            PolicyOverrideState::Consumed
        ));
        assert!(matches!(
            store.prepare_admission(
                "rewrite-me",
                &class("named_orchestrator", Some("maintainer")),
                Some(&record.override_id),
                "child-override",
                now
            ),
            Ok(PreparedDecision { reused: true, .. })
        ));
        let after_restart = PolicyStore::new(&path).unwrap();
        assert!(matches!(
            after_restart
                .override_record(&record.override_id)
                .unwrap()
                .state,
            PolicyOverrideState::Consumed
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn damaged_reusable_decision_fails_closed_after_restart() {
        let (store, path) = store("damaged-decision");
        install(&store, &projection(true, 1));
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        store
            .create_request(&request(
                "damaged",
                PolicyVehicle::TaskAgent,
                None,
                "intent-damaged",
            ))
            .unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        let decision = store
            .prepare_admission(
                "damaged",
                &class("routine_bounded", None),
                None,
                "child-damaged",
                now,
            )
            .unwrap()
            .decision;
        let conn = Connection::open(&path).unwrap();
        let json: String = conn
            .query_row(
                "SELECT decision_json FROM policy_decisions WHERE decision_id = ?1",
                [&decision.decision_id],
                |row| row.get(0),
            )
            .unwrap();
        let mut damaged: serde_json::Value = serde_json::from_str(&json).unwrap();
        damaged["capacity_lease"] = serde_json::Value::Null;
        conn.execute(
            "UPDATE policy_decisions SET decision_json = ?2 WHERE decision_id = ?1",
            params![
                decision.decision_id,
                serde_json::to_string(&damaged).unwrap()
            ],
        )
        .unwrap();
        drop(conn);
        let after_restart = PolicyStore::new(&path).unwrap();
        assert!(matches!(
            after_restart.prepare_admission(
                "damaged",
                &class("routine_bounded", None),
                None,
                "child-damaged",
                now
            ),
            Err(PolicyStoreError::Invalid(_))
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn provisional_child_reservation_is_restart_safe_and_releases_by_child() {
        let (store, path) = store("provisional-child");
        install(&store, &projection(true, 1));
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        store
            .create_request(&request(
                "child-bound",
                PolicyVehicle::TaskAgent,
                None,
                "intent-child-bound",
            ))
            .unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        let first = store
            .prepare_admission(
                "child-bound",
                &class("routine_bounded", None),
                None,
                "provisional-1",
                now,
            )
            .unwrap();
        assert_eq!(
            first.provisional_child_session_id.as_deref(),
            Some("provisional-1")
        );
        let after_restart = PolicyStore::new(&path).unwrap();
        assert!(
            after_restart
                .prepare_admission(
                    "child-bound",
                    &class("routine_bounded", None),
                    None,
                    "provisional-1",
                    now,
                )
                .unwrap()
                .reused
        );
        let retry_with_discarded_preallocation = after_restart
            .prepare_admission(
                "child-bound",
                &class("routine_bounded", None),
                None,
                "other-provisional",
                now,
            )
            .unwrap();
        assert!(retry_with_discarded_preallocation.reused);
        assert_eq!(
            retry_with_discarded_preallocation
                .provisional_child_session_id
                .as_deref(),
            Some("provisional-1")
        );
        after_restart.mark_child_launched("provisional-1").unwrap();
        after_restart.release_by_child("provisional-1").unwrap();
        after_restart.release_by_child("provisional-1").unwrap();
        let conn = Connection::open(&path).unwrap();
        let states: (String, String) = conn
            .query_row(
                "SELECT l.state, p.state FROM policy_capacity_leases l JOIN policy_provisional_children p ON p.lease_id = l.lease_id WHERE p.child_session_id = 'provisional-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(states, ("released".into(), "released".into()));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn ordinary_retries_reuse_terminal_decision_in_every_lease_state() {
        for terminal in ["active", "committed", "released", "expired"] {
            let (store, path) = store(&format!("retry-{terminal}"));
            let mut projection = projection(true, 1);
            for rule in &mut projection.rules {
                rule.lease_ttl_seconds = 1;
            }
            install(&store, &projection);
            store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
            let request_id = format!("retry-{terminal}");
            let child_id = format!("child-{terminal}");
            store
                .create_request(&request(
                    &request_id,
                    PolicyVehicle::TaskAgent,
                    None,
                    &format!("intent-{terminal}"),
                ))
                .unwrap();
            let now = instant("2026-08-17T00:01:00Z");
            let first = store
                .prepare_admission(
                    &request_id,
                    &class("routine_bounded", None),
                    None,
                    &child_id,
                    now,
                )
                .unwrap();
            match terminal {
                "active" => {}
                "committed" => store.mark_child_launched(&child_id).unwrap(),
                "released" => store.release_by_child(&child_id).unwrap(),
                "expired" => {
                    let retry = store
                        .prepare_admission(
                            &request_id,
                            &class("routine_bounded", None),
                            None,
                            "discarded-expiry-preallocation",
                            now + time::Duration::seconds(2),
                        )
                        .unwrap();
                    assert_eq!(retry.child_state, Some(PolicyChildState::Expired));
                }
                _ => unreachable!(),
            }
            let retried = store
                .prepare_admission(
                    &request_id,
                    &class("routine_bounded", None),
                    None,
                    "discarded-retry-preallocation",
                    now + time::Duration::seconds(if terminal == "active" { 0 } else { 2 }),
                )
                .unwrap();
            assert!(retried.reused, "{terminal}");
            assert_eq!(retried.decision.decision_id, first.decision.decision_id);
            assert_eq!(
                retried.provisional_child_session_id.as_deref(),
                Some(child_id.as_str())
            );
            let conn = Connection::open(&path).unwrap();
            let counts: (u64, u64, u64, u64) = conn
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM policy_decisions), (SELECT COUNT(*) FROM policy_capacity_leases), (SELECT COUNT(*) FROM policy_capacity_claims), (SELECT COUNT(*) FROM policy_provisional_children)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(counts, (1, 1, 1, 1), "{terminal}");
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn launch_promotion_requires_exactly_one_reserved_active_binding() {
        let cases = ["missing", "released", "expired", "launched", "conflicting"];
        for case in cases {
            let (store, path) = store(&format!("strict-mark-{case}"));
            let mut projection = projection(true, 1);
            for rule in &mut projection.rules {
                rule.lease_ttl_seconds = 1;
            }
            install(&store, &projection);
            store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
            let child = format!("strict-child-{case}");
            if case != "missing" {
                let request_id = format!("strict-request-{case}");
                store
                    .create_request(&request(
                        &request_id,
                        PolicyVehicle::TaskAgent,
                        None,
                        &format!("strict-intent-{case}"),
                    ))
                    .unwrap();
                let now = instant("2026-08-17T00:01:00Z");
                store
                    .prepare_admission(
                        &request_id,
                        &class("routine_bounded", None),
                        None,
                        &child,
                        now,
                    )
                    .unwrap();
                match case {
                    "released" => store.release_by_child(&child).unwrap(),
                    "expired" => {
                        store
                            .prepare_admission(
                                &request_id,
                                &class("routine_bounded", None),
                                None,
                                "discarded-expired-id",
                                now + time::Duration::seconds(2),
                            )
                            .unwrap();
                    }
                    "launched" => store.mark_child_launched(&child).unwrap(),
                    "conflicting" => {
                        Connection::open(&path)
                            .unwrap()
                            .execute(
                                "UPDATE policy_provisional_children SET state = 'launched' WHERE child_session_id = ?1",
                                [&child],
                            )
                            .unwrap();
                    }
                    _ => {}
                }
            }
            assert!(matches!(
                store.mark_child_launched(&child),
                Err(PolicyStoreError::Invalid(_))
            ));
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn omission_override_uses_rewrite_target_while_full_override_uses_request() {
        let (store, path) = store("override-profile-semantics");
        install(&store, &projection(true, 2));
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        for (request_id, specified, expected_model) in [
            ("full-profile", true, "fable"),
            ("omitted-profile", false, "opus"),
        ] {
            let mut frozen = request(
                request_id,
                PolicyVehicle::NamedSeat,
                Some("fable"),
                &format!("intent-{request_id}"),
            );
            if !specified {
                frozen.requested.model = None;
                frozen.requested.effort = None;
            }
            store.create_request(&frozen).unwrap();
            store
                .prepare_admission(
                    request_id,
                    &class("named_orchestrator", Some("maintainer")),
                    None,
                    &format!("rejected-{request_id}"),
                    now,
                )
                .unwrap();
            let authorization = store
                .authorize_request_override(
                    request_id,
                    &frozen.caller,
                    "frozen exception",
                    now,
                    time::Duration::minutes(1),
                )
                .unwrap();
            let allowed = store
                .prepare_admission(
                    request_id,
                    &class("named_orchestrator", Some("maintainer")),
                    Some(&authorization.override_id),
                    &format!("allowed-{request_id}"),
                    now,
                )
                .unwrap();
            assert_eq!(
                allowed.decision.resolved_profile.unwrap().model,
                expected_model
            );
        }
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn equal_rank_conflicts_include_all_enforcement_fields() {
        for field in ["profile", "outcome", "claims", "overridable", "ttl"] {
            let (store, path) = store(&format!("enforcement-conflict-{field}"));
            let mut projection = projection(true, 1);
            let mut conflict = projection.rules[1].clone();
            conflict.clause_id = format!("sm-policy-1268.conflict-{field}");
            match field {
                "profile" => conflict.profile.model = "opus".into(),
                "outcome" => conflict.outcome = PolicyRuleOutcome::Rewrite,
                "claims" => conflict.capacity_claims[0].units = 2,
                "overridable" => conflict.overridable = false,
                "ttl" => conflict.lease_ttl_seconds += 1,
                _ => unreachable!(),
            }
            if field == "claims" {
                projection.capacity_limits[0].maximum_units = 2;
            }
            projection.rules.push(conflict);
            install(&store, &projection);
            store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
            let request_id = format!("conflict-{field}");
            store
                .create_request(&request(
                    &request_id,
                    PolicyVehicle::TaskAgent,
                    None,
                    &format!("intent-{field}"),
                ))
                .unwrap();
            let decision = store
                .prepare_admission(
                    &request_id,
                    &class("routine_bounded", None),
                    None,
                    &format!("child-{field}"),
                    instant("2026-08-17T00:01:00Z"),
                )
                .unwrap()
                .decision;
            assert!(
                matches!(decision.outcome, PolicyDecisionOutcome::Block),
                "{field}"
            );
            assert!(decision.capacity_lease.is_none(), "{field}");
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn server_constructed_override_rejects_cross_caller_and_cross_request() {
        let (store, path) = store("override-authority");
        install(&store, &projection(true, 2));
        store.set_runtime_versions("sm-policy-1268", 1, 1).unwrap();
        let first = request(
            "override-a",
            PolicyVehicle::NamedSeat,
            Some("fable"),
            "intent-override-a",
        );
        let second = request(
            "override-b",
            PolicyVehicle::NamedSeat,
            Some("fable"),
            "intent-override-b",
        );
        store.create_request(&first).unwrap();
        store.create_request(&second).unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        for (request_id, child) in [("override-a", "child-a"), ("override-b", "child-b")] {
            store
                .prepare_admission(
                    request_id,
                    &class("named_orchestrator", Some("maintainer")),
                    None,
                    child,
                    now,
                )
                .unwrap();
        }
        let mut other_issuer = first.caller.clone();
        if let PolicyCallerBinding::IncarnationBootstrap { session_id, .. } = &mut other_issuer {
            *session_id = "another-maintainer".into();
        }
        assert!(matches!(
            store.authorize_request_override(
                "override-a",
                &other_issuer,
                "wrong caller",
                now,
                time::Duration::minutes(1),
            ),
            Err(PolicyStoreError::OverrideDenied(_))
        ));
        let override_a = store
            .authorize_request_override(
                "override-a",
                &first.caller,
                "exact request only",
                now,
                time::Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(override_a.request_id, "override-a");
        assert!(matches!(
            store.prepare_admission(
                "override-b",
                &class("named_orchestrator", Some("maintainer")),
                Some(&override_a.override_id),
                "child-b-override",
                now,
            ),
            Err(PolicyStoreError::OverrideDenied(_))
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
        install(&store, &projection);
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
                "child-conflict",
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
    fn installation_requires_explicit_projection_activation() {
        let (store, path) = store("explicit-activation");
        let projection = projection(true, 1);
        store.install_projection(&projection).unwrap();
        store.set_runtime_versions(&projection.lane, 1, 1).unwrap();
        let request = request(
            "inactive",
            PolicyVehicle::TaskAgent,
            None,
            "intent-inactive",
        );
        store.create_request(&request).unwrap();
        assert!(matches!(
            store.prepare_admission(
                &request.request_id,
                &class("routine_bounded", None),
                None,
                "child-inactive",
                instant("2026-08-17T00:01:00Z")
            ),
            Err(PolicyStoreError::ProjectionNotFound(_))
        ));
        store
            .activate_projection(
                &projection.lane,
                &projection.policy_version,
                &projection.policy_digest,
            )
            .unwrap();
        assert!(store
            .prepare_admission(
                &request.request_id,
                &class("routine_bounded", None),
                None,
                "child-active",
                instant("2026-08-17T00:01:00Z")
            )
            .is_ok());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn tampered_json_for_each_persisted_record_type_fails_closed() {
        let (store, path) = store("tampered-json");
        let projection = projection(true, 2);
        install(&store, &projection);
        store.set_runtime_versions(&projection.lane, 1, 1).unwrap();
        let allow = request(
            "tamper-allow",
            PolicyVehicle::TaskAgent,
            None,
            "intent-allow",
        );
        let rewrite = request(
            "tamper-rewrite",
            PolicyVehicle::NamedSeat,
            Some("fable"),
            "intent-rewrite",
        );
        store.create_request(&allow).unwrap();
        store.create_request(&rewrite).unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        let allowed = store
            .prepare_admission(
                &allow.request_id,
                &class("routine_bounded", None),
                None,
                "child-tamper",
                now,
            )
            .unwrap()
            .decision;
        let rejected = store
            .prepare_admission(
                &rewrite.request_id,
                &class("named_orchestrator", Some("maintainer")),
                None,
                "child-rewrite",
                now,
            )
            .unwrap()
            .decision;
        assert!(matches!(rejected.outcome, PolicyDecisionOutcome::Rewrite));
        let override_record = store
            .authorize_request_override(
                &rewrite.request_id,
                &rewrite.caller,
                "tamper test",
                now,
                time::Duration::minutes(1),
            )
            .unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE policy_overrides SET override_json = '{\"bad\":true}' WHERE override_id = ?1",
            [&override_record.override_id],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            store.override_record(&override_record.override_id),
            Err(PolicyStoreError::Json(_))
        ));

        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE policy_capacity_leases SET lease_json = '{\"bad\":true}' WHERE lease_id = ?1",
            [&allowed.capacity_lease.unwrap().lease_id],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            store.prepare_admission(
                &allow.request_id,
                &class("routine_bounded", None),
                None,
                "child-tamper",
                now
            ),
            Err(PolicyStoreError::Json(_))
        ));

        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE policy_decisions SET decision_json = '{\"bad\":true}' WHERE decision_id = ?1",
            [&rejected.decision_id],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            store.prepare_admission(
                &rewrite.request_id,
                &class("named_orchestrator", Some("maintainer")),
                None,
                "child-rewrite",
                now
            ),
            Err(PolicyStoreError::Json(_))
        ));

        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE policy_requests SET request_json = '{\"bad\":true}' WHERE request_id = ?1",
            [&allow.request_id],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            store.request(&allow.request_id),
            Err(PolicyStoreError::Json(_))
        ));

        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE policy_projections SET projection_json = '{\"bad\":true}' WHERE lane = ?1",
            [&projection.lane],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            store.activate_projection(
                &projection.lane,
                &projection.policy_version,
                &projection.policy_digest
            ),
            Err(PolicyStoreError::Json(_))
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn row_json_identity_mismatch_fails_closed() {
        let (store, path) = store("row-json-mismatch");
        let request = request("row-mismatch", PolicyVehicle::TaskAgent, None, "intent-row");
        store.create_request(&request).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE policy_requests SET lane = 'other-lane' WHERE request_id = ?1",
            [&request.request_id],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            store.request(&request.request_id),
            Err(PolicyStoreError::Invalid(_))
        ));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn frozen_schema_override_binding_and_capacity_claims_fail_closed_when_tampered() {
        let (schema_store, schema_path) = store("schema-constraint-tamper");
        drop(schema_store);
        let conn = Connection::open(&schema_path).unwrap();
        conn.execute_batch("DROP INDEX idx_policy_leases_active;")
            .unwrap();
        drop(conn);
        assert!(matches!(
            PolicyStore::new(&schema_path),
            Err(PolicyStoreError::Schema(_))
        ));

        let (override_store, override_path) = store("override-row-tamper");
        let override_projection = projection(true, 2);
        install(&override_store, &override_projection);
        override_store
            .set_runtime_versions(&override_projection.lane, 1, 1)
            .unwrap();
        let allow = request("override-allow", PolicyVehicle::TaskAgent, None, "intent-a");
        let rewrite = request(
            "override-rewrite",
            PolicyVehicle::NamedSeat,
            Some("fable"),
            "intent-b",
        );
        override_store.create_request(&allow).unwrap();
        override_store.create_request(&rewrite).unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        let allowed = override_store
            .prepare_admission(
                &allow.request_id,
                &class("routine_bounded", None),
                None,
                "override-child-a",
                now,
            )
            .unwrap()
            .decision;
        override_store
            .prepare_admission(
                &rewrite.request_id,
                &class("named_orchestrator", Some("maintainer")),
                None,
                "override-child-b",
                now,
            )
            .unwrap();
        let override_record = override_store
            .authorize_request_override(
                &rewrite.request_id,
                &rewrite.caller,
                "cross-request rebinding test",
                now,
                time::Duration::minutes(1),
            )
            .unwrap();
        let conn = Connection::open(&override_path).unwrap();
        conn.execute(
            "UPDATE policy_decisions SET override_id = ?2 WHERE decision_id = ?1",
            params![allowed.decision_id, override_record.override_id],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            override_store.prepare_admission(
                &allow.request_id,
                &class("routine_bounded", None),
                Some(&override_record.override_id),
                "override-child-a",
                now,
            ),
            Err(PolicyStoreError::Invalid(_))
        ));

        let (capacity_store, capacity_path) = store("capacity-claim-tamper");
        let projection = projection(true, 1);
        install(&capacity_store, &projection);
        capacity_store
            .set_runtime_versions(&projection.lane, 1, 1)
            .unwrap();
        let first = request(
            "capacity-first",
            PolicyVehicle::TaskAgent,
            None,
            "intent-first",
        );
        let second = request(
            "capacity-second",
            PolicyVehicle::TaskAgent,
            None,
            "intent-second",
        );
        capacity_store.create_request(&first).unwrap();
        capacity_store.create_request(&second).unwrap();
        let first_decision = capacity_store
            .prepare_admission(
                &first.request_id,
                &class("routine_bounded", None),
                None,
                "capacity-child-a",
                now,
            )
            .unwrap()
            .decision;
        let conn = Connection::open(&capacity_path).unwrap();
        conn.execute(
            "UPDATE policy_capacity_claims SET units = 0 WHERE lease_id = ?1",
            [&first_decision.capacity_lease.unwrap().lease_id],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            capacity_store.prepare_admission(
                &second.request_id,
                &class("routine_bounded", None),
                None,
                "capacity-child-b",
                now,
            ),
            Err(PolicyStoreError::Invalid(_))
        ));
        for path in [schema_path, override_path, capacity_path] {
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn override_lifecycle_and_active_pointer_rebinding_fail_closed() {
        let (override_store, path) = store("round-two-authority-tamper");
        let mut override_projection = projection(true, 2);
        for rule in &mut override_projection.rules {
            rule.lease_ttl_seconds = 1;
        }
        install(&override_store, &override_projection);
        override_store
            .set_runtime_versions(&override_projection.lane, 1, 1)
            .unwrap();
        let rewrite = request(
            "round-two-rewrite",
            PolicyVehicle::NamedSeat,
            Some("fable"),
            "round-two-rewrite-intent",
        );
        override_store.create_request(&rewrite).unwrap();
        let now = instant("2026-08-17T00:01:00Z");
        let rejected = override_store
            .prepare_admission(
                &rewrite.request_id,
                &class("named_orchestrator", Some("maintainer")),
                None,
                "round-two-rewrite-child",
                now,
            )
            .unwrap()
            .decision;
        let override_record = override_store
            .authorize_request_override(
                &rewrite.request_id,
                &rewrite.caller,
                "same-request rebinding test",
                now,
                time::Duration::minutes(1),
            )
            .unwrap();
        let allowed = override_store
            .prepare_admission(
                &rewrite.request_id,
                &class("named_orchestrator", Some("maintainer")),
                Some(&override_record.override_id),
                "round-two-allowed-child",
                now,
            )
            .unwrap()
            .decision;
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE policy_decisions SET override_id = NULL WHERE decision_id = ?1",
            [&allowed.decision_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE policy_decisions SET override_id = ?2 WHERE decision_id = ?1",
            params![rejected.decision_id, override_record.override_id],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            override_store.prepare_admission(
                &rewrite.request_id,
                &class("named_orchestrator", Some("maintainer")),
                Some(&override_record.override_id),
                "round-two-allowed-child",
                now,
            ),
            Err(PolicyStoreError::Invalid(_))
        ));

        let (lease_store, lease_path) = store("round-two-expired-lease-tamper");
        let mut lease_projection = projection(true, 1);
        for rule in &mut lease_projection.rules {
            rule.lease_ttl_seconds = 1;
        }
        install(&lease_store, &lease_projection);
        lease_store
            .set_runtime_versions(&lease_projection.lane, 1, 1)
            .unwrap();
        let first = request(
            "round-two-first",
            PolicyVehicle::TaskAgent,
            None,
            "round-two-first-intent",
        );
        let second = request(
            "round-two-second",
            PolicyVehicle::TaskAgent,
            None,
            "round-two-second-intent",
        );
        lease_store.create_request(&first).unwrap();
        lease_store.create_request(&second).unwrap();
        let first_decision = lease_store
            .prepare_admission(
                &first.request_id,
                &class("routine_bounded", None),
                None,
                "round-two-first-child",
                now,
            )
            .unwrap()
            .decision;
        let conn = Connection::open(&lease_path).unwrap();
        conn.execute(
            "UPDATE policy_capacity_leases SET lease_json = '{\"bad\":true}' WHERE lease_id = ?1",
            [&first_decision.capacity_lease.unwrap().lease_id],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            lease_store.prepare_admission(
                &second.request_id,
                &class("routine_bounded", None),
                None,
                "round-two-second-child",
                now + time::Duration::seconds(2),
            ),
            Err(PolicyStoreError::Json(_))
        ));

        let (pointer_store, pointer_path) = store("round-two-pointer-tamper");
        let v1 = projection(true, 1);
        install(&pointer_store, &v1);
        pointer_store.set_runtime_versions(&v1.lane, 1, 1).unwrap();
        let v1_request = request(
            "round-two-pointer",
            PolicyVehicle::TaskAgent,
            None,
            "round-two-pointer-intent",
        );
        pointer_store.create_request(&v1_request).unwrap();
        let mut v2 = v1.clone();
        v2.policy_version = "v2".into();
        pointer_store.install_projection(&v2).unwrap();
        pointer_store
            .activate_projection(&v2.lane, &v2.policy_version, &v2.policy_digest)
            .unwrap();
        let conn = Connection::open(&pointer_path).unwrap();
        conn.execute(
            "UPDATE policy_active_projections SET policy_version = ?2 WHERE lane = ?1",
            params![v1.lane, v1.policy_version],
        )
        .unwrap();
        drop(conn);
        assert!(matches!(
            pointer_store.prepare_admission(
                &v1_request.request_id,
                &class("routine_bounded", None),
                None,
                "round-two-pointer-child",
                now,
            ),
            Err(PolicyStoreError::Invalid(_))
        ));
        for path in [path, lease_path, pointer_path] {
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn wrong_missing_or_truncated_schema_fails_closed_with_recreate_guidance() {
        let wrong = std::env::temp_dir().join(format!(
            "sm-policy-store-wrong-version-{}.sqlite",
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let _store = PolicyStore::new(&wrong).unwrap();
        let conn = Connection::open(&wrong).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        drop(conn);
        let error = PolicyStore::new(&wrong).unwrap_err().to_string();
        assert!(error.contains("archive") && error.contains("recreate"));

        let missing = std::env::temp_dir().join(format!(
            "sm-policy-store-missing-version-{}.sqlite",
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        Connection::open(&missing).unwrap();
        let error = PolicyStore::new(&missing).unwrap_err().to_string();
        assert!(error.contains("user_version=1"));

        let truncated = std::env::temp_dir().join(format!(
            "sm-policy-store-truncated-{}.sqlite",
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        std::fs::write(&truncated, b"not sqlite").unwrap();
        let error = PolicyStore::new(&truncated).unwrap_err().to_string();
        assert!(error.contains("archive") && error.contains("recreate"));
        for path in [wrong, missing, truncated] {
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn competing_reservations_cannot_both_consume_one_slot() {
        use std::sync::{Arc, Barrier};

        let (store, path) = store("concurrent-capacity");
        install(&store, &projection(true, 1));
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
                    request_id,
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
