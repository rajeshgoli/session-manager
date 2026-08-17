use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SPAWN_REQUEST_SCHEMA: &str = "sm.policy.spawn_request.v1";
pub const DECISION_SCHEMA: &str = "sm.policy.decision.v1";
pub const OVERRIDE_SCHEMA: &str = "sm.policy.override.v1";
pub const ATTESTATION_SCHEMA: &str = "sm.policy.runtime_attestation.v1";
pub const EVENT_SCHEMA: &str = "sm.policy.event.v1";
pub const USAGE_SCHEMA: &str = "sm.policy.usage.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyCallerBinding {
    Seat {
        lane: String,
        seat_key: String,
        generation: u64,
        holder_session_id: String,
    },
    IncarnationBootstrap {
        lane: String,
        session_id: String,
        credential_fingerprint: String,
        binding_digest: String,
    },
}

impl PolicyCallerBinding {
    pub fn lane(&self) -> &str {
        match self {
            Self::Seat { lane, .. } | Self::IncarnationBootstrap { lane, .. } => lane,
        }
    }

    pub fn validate(&self) -> Result<(), PolicyContractError> {
        match self {
            Self::Seat {
                lane,
                seat_key,
                generation,
                holder_session_id,
            } => {
                require_nonempty("caller.lane", lane)?;
                require_nonempty("caller.seat_key", seat_key)?;
                require_nonempty("caller.holder_session_id", holder_session_id)?;
                if *generation == 0 {
                    return Err(PolicyContractError::new(
                        "invalid_seat_generation",
                        "caller seat generation must be greater than zero",
                    ));
                }
                let prefix = format!("{lane}-");
                if !seat_key.starts_with(&prefix) {
                    return Err(PolicyContractError::new(
                        "invalid_seat_key",
                        format!("caller seat key must start with {prefix}"),
                    ));
                }
            }
            Self::IncarnationBootstrap {
                lane,
                session_id,
                credential_fingerprint,
                binding_digest,
            } => {
                require_nonempty("caller.lane", lane)?;
                require_nonempty("caller.session_id", session_id)?;
                require_nonempty("caller.credential_fingerprint", credential_fingerprint)?;
                require_nonempty("caller.binding_digest", binding_digest)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVehicle {
    NamedSeat,
    TaskAgent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyLaunchProfile {
    pub vehicle: PolicyVehicle,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub context_profile: String,
}

impl PolicyLaunchProfile {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_nonempty("profile.provider", &self.provider)?;
        require_nonempty("profile.model", &self.model)?;
        require_nonempty("profile.effort", &self.effort)?;
        require_nonempty("profile.context_profile", &self.context_profile)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRequestedLaunch {
    pub name: String,
    pub vehicle: PolicyVehicle,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub working_dir: String,
    pub node: String,
}

impl PolicyRequestedLaunch {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_nonempty("requested.name", &self.name)?;
        require_nonempty("requested.provider", &self.provider)?;
        require_optional_nonempty("requested.model", self.model.as_deref())?;
        require_optional_nonempty("requested.effort", self.effort.as_deref())?;
        require_nonempty("requested.working_dir", &self.working_dir)?;
        require_nonempty("requested.node", &self.node)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyCapacityClaim {
    pub dimension: String,
    pub key: String,
    pub units: u64,
}

impl PolicyCapacityClaim {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_nonempty("capacity_claim.dimension", &self.dimension)?;
        require_nonempty("capacity_claim.key", &self.key)?;
        if self.units == 0 {
            return Err(PolicyContractError::new(
                "invalid_capacity_units",
                "capacity claim units must be greater than zero",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyCapacityLease {
    pub lease_id: String,
    pub topology_version: u64,
    pub capacity_version: u64,
    pub claims: Vec<PolicyCapacityClaim>,
    pub expires_at: String,
}

impl PolicyCapacityLease {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_nonempty("capacity_lease.lease_id", &self.lease_id)?;
        if self.claims.is_empty() {
            return Err(PolicyContractError::new(
                "missing_capacity_claims",
                "capacity lease must contain at least one claim",
            ));
        }
        for claim in &self.claims {
            claim.validate()?;
        }
        require_nonempty("capacity_lease.expires_at", &self.expires_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpawnRequest {
    pub schema: String,
    pub request_id: String,
    pub caller: PolicyCallerBinding,
    pub prompt_digest: String,
    pub launch_intent_id: String,
    pub policy_version: String,
    pub policy_digest: String,
    pub requested: PolicyRequestedLaunch,
    pub topology_version: u64,
    pub capacity_version: u64,
    pub created_at: String,
}

impl PolicySpawnRequest {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_schema(&self.schema, SPAWN_REQUEST_SCHEMA)?;
        require_nonempty("request_id", &self.request_id)?;
        self.caller.validate()?;
        require_sha256("prompt_digest", &self.prompt_digest)?;
        require_nonempty("launch_intent_id", &self.launch_intent_id)?;
        require_nonempty("policy_version", &self.policy_version)?;
        require_sha256("policy_digest", &self.policy_digest)?;
        self.requested.validate()?;
        require_nonempty("created_at", &self.created_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionOutcome {
    Allow,
    Rewrite,
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyClassification {
    pub class_id: String,
    #[serde(default)]
    pub role_id: Option<String>,
    pub turn_profile: String,
    pub method: String,
    pub confidence: String,
}

impl PolicyClassification {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_nonempty("classification.class_id", &self.class_id)?;
        require_optional_nonempty("classification.role_id", self.role_id.as_deref())?;
        require_nonempty("classification.turn_profile", &self.turn_profile)?;
        require_nonempty("classification.method", &self.method)?;
        require_nonempty("classification.confidence", &self.confidence)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDecision {
    pub schema: String,
    pub decision_id: String,
    pub request_id: String,
    pub attempt: u32,
    pub policy_version: String,
    pub outcome: PolicyDecisionOutcome,
    pub classification: PolicyClassification,
    pub applicable_clause_ids: Vec<String>,
    #[serde(default)]
    pub resolved_profile: Option<PolicyLaunchProfile>,
    pub reason: String,
    #[serde(default)]
    pub override_command: Option<String>,
    #[serde(default)]
    pub capacity_lease: Option<PolicyCapacityLease>,
    pub decided_at: String,
}

impl PolicyDecision {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_schema(&self.schema, DECISION_SCHEMA)?;
        require_nonempty("decision_id", &self.decision_id)?;
        require_nonempty("request_id", &self.request_id)?;
        if self.attempt == 0 {
            return Err(PolicyContractError::new(
                "invalid_decision_attempt",
                "decision attempt must be greater than zero",
            ));
        }
        require_nonempty("policy_version", &self.policy_version)?;
        self.classification.validate()?;
        require_nonempty("reason", &self.reason)?;
        require_nonempty("decided_at", &self.decided_at)?;
        match self.outcome {
            PolicyDecisionOutcome::Allow => {
                let profile = self.resolved_profile.as_ref().ok_or_else(|| {
                    PolicyContractError::new(
                        "missing_resolved_profile",
                        "allow decisions require a resolved launch profile",
                    )
                })?;
                profile.validate()?;
                let lease = self.capacity_lease.as_ref().ok_or_else(|| {
                    PolicyContractError::new(
                        "missing_capacity_lease",
                        "allow decisions require an atomic capacity lease",
                    )
                })?;
                lease.validate()?;
                if self.override_command.is_some() {
                    return Err(PolicyContractError::new(
                        "unexpected_override_command",
                        "allow decisions cannot include an override command",
                    ));
                }
            }
            PolicyDecisionOutcome::Rewrite | PolicyDecisionOutcome::Block => {
                if self.capacity_lease.is_some() {
                    return Err(PolicyContractError::new(
                        "unexpected_capacity_lease",
                        "rewrite and block decisions cannot reserve capacity",
                    ));
                }
                require_nonempty(
                    "override_command",
                    self.override_command.as_deref().unwrap_or_default(),
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyOverrideState {
    Authorized,
    Consumed,
    Expired,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyOverrideRecord {
    pub schema: String,
    pub override_id: String,
    pub request_id: String,
    pub request_digest: String,
    pub decision_id: String,
    pub policy_version: String,
    pub issuer: PolicyCallerBinding,
    pub reason: String,
    pub self_benefiting: bool,
    pub state: PolicyOverrideState,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default)]
    pub consumed_at: Option<String>,
}

impl PolicyOverrideRecord {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_schema(&self.schema, OVERRIDE_SCHEMA)?;
        require_nonempty("override_id", &self.override_id)?;
        require_nonempty("request_id", &self.request_id)?;
        require_sha256("request_digest", &self.request_digest)?;
        require_nonempty("decision_id", &self.decision_id)?;
        require_nonempty("policy_version", &self.policy_version)?;
        self.issuer.validate()?;
        require_nonempty("reason", &self.reason)?;
        require_nonempty("created_at", &self.created_at)?;
        require_nonempty("expires_at", &self.expires_at)?;
        if matches!(self.state, PolicyOverrideState::Consumed) && self.consumed_at.is_none() {
            return Err(PolicyContractError::new(
                "missing_override_consumed_at",
                "consumed overrides require consumed_at",
            ));
        }
        if !matches!(self.state, PolicyOverrideState::Consumed) && self.consumed_at.is_some() {
            return Err(PolicyContractError::new(
                "unexpected_override_consumed_at",
                "only consumed overrides may carry consumed_at",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAttestationEvidence {
    EffectiveProfileAcknowledgement,
    ProviderEvent,
    ProviderUsage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRuntimeAttestation {
    pub schema: String,
    pub decision_id: String,
    pub launch_intent_id: String,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub evidence: RuntimeAttestationEvidence,
    pub evidence_id: String,
    pub observed_at: String,
}

impl PolicyRuntimeAttestation {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_schema(&self.schema, ATTESTATION_SCHEMA)?;
        require_nonempty("decision_id", &self.decision_id)?;
        require_nonempty("launch_intent_id", &self.launch_intent_id)?;
        require_nonempty("session_id", &self.session_id)?;
        require_nonempty("provider", &self.provider)?;
        require_nonempty("model", &self.model)?;
        require_nonempty("effort", &self.effort)?;
        require_nonempty("evidence_id", &self.evidence_id)?;
        require_nonempty("observed_at", &self.observed_at)
    }

    pub fn matches(&self, expected: &PolicyLaunchProfile) -> bool {
        self.provider == expected.provider
            && self.model == expected.model
            && self.effort == expected.effort
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAdmissionResult {
    Allowed {
        decision_id: String,
        child_session_id: String,
        profile: PolicyLaunchProfile,
    },
    Rejected {
        decision_id: String,
        request_id: String,
        outcome: PolicyDecisionOutcome,
        reason: String,
        override_command: String,
    },
    Failed {
        request_id: String,
        code: String,
        detail: String,
        recoverable: bool,
    },
}

impl PolicyAdmissionResult {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        match self {
            Self::Allowed {
                decision_id,
                child_session_id,
                profile,
            } => {
                require_nonempty("decision_id", decision_id)?;
                require_nonempty("child_session_id", child_session_id)?;
                profile.validate()
            }
            Self::Rejected {
                decision_id,
                request_id,
                outcome,
                reason,
                override_command,
            } => {
                require_nonempty("decision_id", decision_id)?;
                require_nonempty("request_id", request_id)?;
                if matches!(outcome, PolicyDecisionOutcome::Allow) {
                    return Err(PolicyContractError::new(
                        "invalid_rejection_outcome",
                        "rejected admission cannot carry an allow outcome",
                    ));
                }
                require_nonempty("reason", reason)?;
                require_nonempty("override_command", override_command)
            }
            Self::Failed {
                request_id,
                code,
                detail,
                ..
            } => {
                require_nonempty("request_id", request_id)?;
                require_nonempty("code", code)?;
                require_nonempty("detail", detail)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstimatedCounter {
    pub value: u64,
    pub lower_bound: u64,
    pub upper_bound: u64,
    pub method: String,
    pub confidence: String,
}

impl EstimatedCounter {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        if self.lower_bound > self.value || self.value > self.upper_bound {
            return Err(PolicyContractError::new(
                "invalid_counter_bounds",
                "estimated counter must fall within its lower and upper bounds",
            ));
        }
        require_nonempty("counter.method", &self.method)?;
        require_nonempty("counter.confidence", &self.confidence)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstimatedAmount {
    pub value: f64,
    pub lower_bound: f64,
    pub upper_bound: f64,
    pub unit: String,
    pub method: String,
    pub confidence: String,
}

impl EstimatedAmount {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        if !self.value.is_finite()
            || !self.lower_bound.is_finite()
            || !self.upper_bound.is_finite()
            || self.lower_bound < 0.0
            || self.lower_bound > self.value
            || self.value > self.upper_bound
        {
            return Err(PolicyContractError::new(
                "invalid_amount_bounds",
                "estimated amount must be finite, non-negative, and bounded",
            ));
        }
        require_nonempty("amount.unit", &self.unit)?;
        require_nonempty("amount.method", &self.method)?;
        require_nonempty("amount.confidence", &self.confidence)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyUsageCounters {
    pub schema: String,
    pub input_tokens: EstimatedCounter,
    pub cache_read_tokens: EstimatedCounter,
    pub cache_write_5m_tokens: EstimatedCounter,
    pub cache_write_1h_tokens: EstimatedCounter,
    pub output_tokens: EstimatedCounter,
    pub reasoning_tokens: EstimatedCounter,
    pub cost_usd: EstimatedAmount,
    pub quota_points: EstimatedAmount,
}

impl PolicyUsageCounters {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_schema(&self.schema, USAGE_SCHEMA)?;
        self.input_tokens.validate()?;
        self.cache_read_tokens.validate()?;
        self.cache_write_5m_tokens.validate()?;
        self.cache_write_1h_tokens.validate()?;
        self.output_tokens.validate()?;
        self.reasoning_tokens.validate()?;
        self.cost_usd.validate()?;
        self.quota_points.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyEventEnvelope {
    pub schema: String,
    pub event_id: String,
    pub sequence: u64,
    pub event_type: String,
    pub operation_id: String,
    pub lane: String,
    #[serde(default)]
    pub seat_key: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub occurred_at: String,
    pub payload_schema: String,
    pub payload: Value,
}

impl PolicyEventEnvelope {
    pub fn validate(&self) -> Result<(), PolicyContractError> {
        require_schema(&self.schema, EVENT_SCHEMA)?;
        require_nonempty("event_id", &self.event_id)?;
        require_nonempty("event_type", &self.event_type)?;
        require_nonempty("operation_id", &self.operation_id)?;
        require_nonempty("lane", &self.lane)?;
        require_optional_nonempty("seat_key", self.seat_key.as_deref())?;
        require_optional_nonempty("session_id", self.session_id.as_deref())?;
        require_nonempty("occurred_at", &self.occurred_at)?;
        require_nonempty("payload_schema", &self.payload_schema)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyContractError {
    pub code: &'static str,
    pub detail: String,
}

impl PolicyContractError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for PolicyContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for PolicyContractError {}

fn require_schema(actual: &str, expected: &'static str) -> Result<(), PolicyContractError> {
    if actual == expected {
        Ok(())
    } else {
        Err(PolicyContractError::new(
            "unsupported_schema",
            format!("expected {expected}, got {actual}"),
        ))
    }
}

fn require_nonempty(field: &'static str, value: &str) -> Result<(), PolicyContractError> {
    if value.trim().is_empty() {
        Err(PolicyContractError::new(
            "missing_field",
            format!("{field} must not be empty"),
        ))
    } else {
        Ok(())
    }
}

fn require_optional_nonempty(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), PolicyContractError> {
    match value {
        Some(value) => require_nonempty(field, value),
        None => Ok(()),
    }
}

fn require_sha256(field: &'static str, value: &str) -> Result<(), PolicyContractError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(PolicyContractError::new(
            "invalid_digest",
            format!("{field} must be a 64-character hexadecimal SHA-256 digest"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(character: char) -> String {
        std::iter::repeat_n(character, 64).collect()
    }

    fn lease() -> PolicyCapacityLease {
        PolicyCapacityLease {
            lease_id: "lease-1".to_owned(),
            topology_version: 7,
            capacity_version: 11,
            claims: vec![PolicyCapacityClaim {
                dimension: "lane_concurrency".to_owned(),
                key: "355".to_owned(),
                units: 1,
            }],
            expires_at: "2026-08-17T23:30:00Z".to_owned(),
        }
    }

    fn profile(model: &str) -> PolicyLaunchProfile {
        PolicyLaunchProfile {
            vehicle: PolicyVehicle::NamedSeat,
            provider: "claude".to_owned(),
            model: model.to_owned(),
            effort: "high".to_owned(),
            context_profile: "claude_rotate_35".to_owned(),
        }
    }

    fn request(name: &str, model: Option<&str>) -> PolicySpawnRequest {
        PolicySpawnRequest {
            schema: SPAWN_REQUEST_SCHEMA.to_owned(),
            request_id: format!("request-{name}"),
            caller: PolicyCallerBinding::Seat {
                lane: "355".to_owned(),
                seat_key: "355-root".to_owned(),
                generation: 24,
                holder_session_id: "2260296e".to_owned(),
            },
            prompt_digest: digest('a'),
            launch_intent_id: format!("intent-{name}"),
            policy_version: "355-policy-v1".to_owned(),
            policy_digest: digest('b'),
            requested: PolicyRequestedLaunch {
                name: name.to_owned(),
                vehicle: PolicyVehicle::NamedSeat,
                provider: "claude".to_owned(),
                model: model.map(str::to_owned),
                effort: Some("high".to_owned()),
                working_dir: "/Users/rajesh/projects/fractal-algo-rust".to_owned(),
                node: "primary".to_owned(),
            },
            topology_version: 7,
            capacity_version: 11,
            created_at: "2026-08-17T22:00:00Z".to_owned(),
        }
    }

    fn allow_decision(
        request: &PolicySpawnRequest,
        resolved: PolicyLaunchProfile,
    ) -> PolicyDecision {
        PolicyDecision {
            schema: DECISION_SCHEMA.to_owned(),
            decision_id: format!("decision-{}", request.request_id),
            request_id: request.request_id.clone(),
            attempt: 1,
            policy_version: request.policy_version.clone(),
            outcome: PolicyDecisionOutcome::Allow,
            classification: PolicyClassification {
                class_id: "named_role".to_owned(),
                role_id: Some(request.requested.name.clone()),
                turn_profile: "initial".to_owned(),
                method: "deterministic".to_owned(),
                confidence: "high".to_owned(),
            },
            applicable_clause_ids: vec!["355.model.orchestrator".to_owned()],
            resolved_profile: Some(resolved),
            reason: "named role has a deterministic profile".to_owned(),
            override_command: None,
            capacity_lease: Some(lease()),
            decided_at: "2026-08-17T22:00:01Z".to_owned(),
        }
    }

    #[test]
    fn named_seat_binding_requires_lane_prefix() {
        let mut request = request("355-root", None);
        let PolicyCallerBinding::Seat { seat_key, .. } = &mut request.caller else {
            panic!("fixture must use a seat binding");
        };
        *seat_key = "root".to_owned();

        let error = request.validate().unwrap_err();

        assert_eq!(error.code, "invalid_seat_key");
    }

    #[test]
    fn allow_requires_profile_and_capacity_lease() {
        let request = request("355-root", Some("opus"));
        let mut decision = allow_decision(&request, profile("opus"));
        decision.capacity_lease = None;

        let error = decision.validate().unwrap_err();

        assert_eq!(error.code, "missing_capacity_lease");
    }

    #[test]
    fn rewrite_returns_an_executable_scoped_override() {
        let request = request("355-root", None);
        let decision = PolicyDecision {
            schema: DECISION_SCHEMA.to_owned(),
            decision_id: "decision-rewrite".to_owned(),
            request_id: request.request_id.clone(),
            attempt: 1,
            policy_version: request.policy_version.clone(),
            outcome: PolicyDecisionOutcome::Rewrite,
            classification: PolicyClassification {
                class_id: "named_role".to_owned(),
                role_id: Some("355-root".to_owned()),
                turn_profile: "initial".to_owned(),
                method: "deterministic".to_owned(),
                confidence: "high".to_owned(),
            },
            applicable_clause_ids: vec!["355.model.orchestrator".to_owned()],
            resolved_profile: Some(profile("opus")),
            reason: "explicit Opus/high is required".to_owned(),
            override_command: Some(format!(
                "sm policy override --request {} --reason <text>",
                request.request_id
            )),
            capacity_lease: None,
            decided_at: "2026-08-17T22:00:01Z".to_owned(),
        };

        decision.validate().unwrap();
        assert!(decision
            .override_command
            .as_deref()
            .unwrap()
            .contains(&request.request_id));
    }

    #[test]
    fn omitted_model_fixture_rejects_inherited_fable_for_engineer() {
        let mut request = request("355-617-death-1", None);
        request.requested.vehicle = PolicyVehicle::TaskAgent;
        request.validate().unwrap();
        let expected = PolicyLaunchProfile {
            vehicle: PolicyVehicle::TaskAgent,
            provider: "claude".to_owned(),
            model: "sonnet".to_owned(),
            effort: "high".to_owned(),
            context_profile: "claude_engineer_initial_65".to_owned(),
        };
        let observed = PolicyRuntimeAttestation {
            schema: ATTESTATION_SCHEMA.to_owned(),
            decision_id: "decision-aa6c1120".to_owned(),
            launch_intent_id: request.launch_intent_id.clone(),
            session_id: "aa6c1120".to_owned(),
            provider: "claude".to_owned(),
            model: "fable".to_owned(),
            effort: "high".to_owned(),
            evidence: RuntimeAttestationEvidence::ProviderUsage,
            evidence_id: "usage-aa6c1120-1".to_owned(),
            observed_at: "2026-08-17T22:00:02Z".to_owned(),
        };

        assert!(request.requested.model.is_none());
        observed.validate().unwrap();
        assert!(!observed.matches(&expected));
    }

    #[test]
    fn named_orchestrator_fixture_rejects_inherited_fable() {
        let request = request("355-root", None);
        request.validate().unwrap();
        let expected = profile("opus");
        let decision = allow_decision(&request, expected.clone());
        decision.validate().unwrap();
        let observed = PolicyRuntimeAttestation {
            schema: ATTESTATION_SCHEMA.to_owned(),
            decision_id: decision.decision_id.clone(),
            launch_intent_id: request.launch_intent_id.clone(),
            session_id: "2260296e".to_owned(),
            provider: "claude".to_owned(),
            model: "fable".to_owned(),
            effort: "high".to_owned(),
            evidence: RuntimeAttestationEvidence::ProviderEvent,
            evidence_id: "event-2260296e-1".to_owned(),
            observed_at: "2026-08-17T22:00:02Z".to_owned(),
        };

        assert!(request.requested.model.is_none());
        observed.validate().unwrap();
        assert!(!observed.matches(&expected));
    }

    #[test]
    fn estimated_counter_is_always_numeric_and_bounded() {
        let valid = EstimatedCounter {
            value: 100,
            lower_bound: 80,
            upper_bound: 140,
            method: "before_after_residual".to_owned(),
            confidence: "medium".to_owned(),
        };
        valid.validate().unwrap();

        let invalid = EstimatedCounter {
            lower_bound: 101,
            ..valid
        };
        assert_eq!(
            invalid.validate().unwrap_err().code,
            "invalid_counter_bounds"
        );
    }

    #[test]
    fn estimated_amount_rejects_non_finite_or_unbounded_values() {
        let valid = EstimatedAmount {
            value: 0.42,
            lower_bound: 0.31,
            upper_bound: 0.58,
            unit: "usd".to_owned(),
            method: "rate_card".to_owned(),
            confidence: "medium".to_owned(),
        };
        valid.validate().unwrap();

        let invalid = EstimatedAmount {
            value: f64::NAN,
            ..valid
        };
        assert_eq!(
            invalid.validate().unwrap_err().code,
            "invalid_amount_bounds"
        );
    }

    #[test]
    fn rejected_admission_cannot_claim_allow() {
        let admission = PolicyAdmissionResult::Rejected {
            decision_id: "decision-1".to_owned(),
            request_id: "request-1".to_owned(),
            outcome: PolicyDecisionOutcome::Allow,
            reason: "contradictory fixture".to_owned(),
            override_command: "sm policy override --request request-1 --reason <text>".to_owned(),
        };

        assert_eq!(
            admission.validate().unwrap_err().code,
            "invalid_rejection_outcome"
        );
    }

    #[test]
    fn bootstrap_binding_round_trips_without_inventing_a_seat() {
        let binding = PolicyCallerBinding::IncarnationBootstrap {
            lane: "sm-policy-1268".to_owned(),
            session_id: "031de889".to_owned(),
            credential_fingerprint: digest('c'),
            binding_digest: digest('d'),
        };

        let encoded = serde_json::to_value(&binding).unwrap();
        let decoded: PolicyCallerBinding = serde_json::from_value(encoded.clone()).unwrap();

        binding.validate().unwrap();
        assert_eq!(binding, decoded);
        assert_eq!(encoded["kind"], "incarnation_bootstrap");
        assert!(encoded.get("seat_key").is_none());
    }
}
