use serde_json::{Map, Value};

use crate::policy_contracts::{
    PolicyLaunchProfile, PolicyRuntimeAttestation, RuntimeAttestationEvidence, ATTESTATION_SCHEMA,
};

const CODEX_FORK_PROVIDER: &str = "codex-fork";
const CODEX_PROVIDER_ID: &str = "openai";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAttestationError {
    pub code: &'static str,
    pub detail: String,
}

impl RuntimeAttestationError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for RuntimeAttestationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for RuntimeAttestationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexSessionStart {
    epoch: u64,
    seq: u64,
    model: String,
}

/// Builds provider-originated launch evidence from the bounded event range for
/// one newly launched Codex thread. The first settings event is the launch
/// profile; later settings changes belong to subsequent turns.
pub fn attest_codex_fork_launch<'a>(
    events: impl IntoIterator<Item = &'a Value>,
    expected: &PolicyLaunchProfile,
    decision_id: &str,
    launch_intent_id: &str,
    child_session_id: &str,
    provider_resume_id: &str,
) -> Result<PolicyRuntimeAttestation, RuntimeAttestationError> {
    if expected.provider != CODEX_FORK_PROVIDER {
        return Err(RuntimeAttestationError::new(
            "unsupported_attestation_provider",
            format!(
                "Codex launch evidence cannot attest provider {}",
                expected.provider
            ),
        ));
    }

    let mut starts = Vec::new();
    for event in events {
        let event_type = event.get("event_type").and_then(Value::as_str);
        if !matches!(
            event_type,
            Some("session_start" | "thread/settings/updated")
        ) {
            continue;
        }
        require_schema_v2(event)?;
        let epoch = required_u64(event, "session_epoch")?;
        let seq = required_u64(event, "seq")?;
        let payload = required_object(event, "payload")?;

        if event_type == Some("session_start") {
            let provider_id = required_text(payload, "model_provider_id")?;
            if provider_id != CODEX_PROVIDER_ID {
                return Err(RuntimeAttestationError::new(
                    "unexpected_codex_provider",
                    format!("session_start reports model provider {provider_id}"),
                ));
            }
            starts.push(CodexSessionStart {
                epoch,
                seq,
                model: required_text(payload, "model")?.to_owned(),
            });
            continue;
        }

        let root_session_id = required_text_object(event, "session_id")?;
        let thread_id = required_text(payload, "threadId")?;
        if root_session_id != provider_resume_id || thread_id != provider_resume_id {
            continue;
        }
        let settings = payload
            .get("threadSettings")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                RuntimeAttestationError::new(
                    "invalid_codex_settings_event",
                    "thread/settings/updated is missing threadSettings",
                )
            })?;
        let model = required_text(settings, "model")?;
        let effort = required_text(settings, "effort")?;
        let provider = required_text(settings, "modelProvider")?;
        if provider != CODEX_PROVIDER_ID {
            return Err(RuntimeAttestationError::new(
                "unexpected_codex_provider",
                format!("thread settings report model provider {provider}"),
            ));
        }

        let start = starts
            .iter()
            .rev()
            .find(|start| start.epoch == epoch && start.seq < seq)
            .ok_or_else(|| {
                RuntimeAttestationError::new(
                    "missing_codex_session_start",
                    "no preceding schema-v2 session_start exists for the launch settings event",
                )
            })?;
        if start.model != model {
            return Err(RuntimeAttestationError::new(
                "conflicting_codex_launch_profile",
                format!(
                    "session_start model {} disagrees with thread settings model {model}",
                    start.model
                ),
            ));
        }
        if model != expected.model || effort != expected.effort {
            return Err(RuntimeAttestationError::new(
                "codex_launch_profile_mismatch",
                format!(
                    "expected model/effort {}/{}, provider reported {model}/{effort}",
                    expected.model, expected.effort
                ),
            ));
        }

        let observed_at = required_text_object(event, "ts")?.to_owned();
        let attestation = PolicyRuntimeAttestation {
            schema: ATTESTATION_SCHEMA.to_owned(),
            decision_id: decision_id.to_owned(),
            launch_intent_id: launch_intent_id.to_owned(),
            session_id: child_session_id.to_owned(),
            provider: CODEX_FORK_PROVIDER.to_owned(),
            model: model.to_owned(),
            effort: effort.to_owned(),
            evidence: RuntimeAttestationEvidence::ProviderEvent,
            evidence_id: format!("codex-fork:{provider_resume_id}:{epoch}:{seq}"),
            observed_at,
        };
        attestation.validate().map_err(|error| {
            RuntimeAttestationError::new("invalid_runtime_attestation", error.to_string())
        })?;
        return Ok(attestation);
    }

    Err(RuntimeAttestationError::new(
        "missing_codex_launch_settings",
        format!(
            "no schema-v2 launch settings event was observed for provider thread {provider_resume_id}"
        ),
    ))
}

fn require_schema_v2(event: &Value) -> Result<(), RuntimeAttestationError> {
    if event.get("schema_version").and_then(Value::as_u64) == Some(2) {
        Ok(())
    } else {
        Err(RuntimeAttestationError::new(
            "unsupported_codex_event_schema",
            "launch evidence requires numeric schema_version 2",
        ))
    }
}

fn required_u64(event: &Value, field: &'static str) -> Result<u64, RuntimeAttestationError> {
    event.get(field).and_then(Value::as_u64).ok_or_else(|| {
        RuntimeAttestationError::new(
            "invalid_codex_launch_event",
            format!("launch evidence is missing numeric {field}"),
        )
    })
}

fn required_object<'a>(
    event: &'a Value,
    field: &'static str,
) -> Result<&'a Map<String, Value>, RuntimeAttestationError> {
    event.get(field).and_then(Value::as_object).ok_or_else(|| {
        RuntimeAttestationError::new(
            "invalid_codex_launch_event",
            format!("launch evidence is missing object {field}"),
        )
    })
}

fn required_text<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, RuntimeAttestationError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            RuntimeAttestationError::new(
                "invalid_codex_launch_event",
                format!("launch evidence is missing text {field}"),
            )
        })
}

fn required_text_object<'a>(
    event: &'a Value,
    field: &'static str,
) -> Result<&'a str, RuntimeAttestationError> {
    event
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            RuntimeAttestationError::new(
                "invalid_codex_launch_event",
                format!("launch evidence is missing text {field}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::policy_contracts::PolicyVehicle;

    fn expected() -> PolicyLaunchProfile {
        PolicyLaunchProfile {
            vehicle: PolicyVehicle::TaskAgent,
            provider: "codex-fork".to_owned(),
            model: "gpt-5.6-terra".to_owned(),
            effort: "high".to_owned(),
            context_profile: "provider_native_compaction".to_owned(),
        }
    }

    fn events(model: &str, effort: &str) -> Vec<Value> {
        vec![
            json!({
                "schema_version": 2,
                "ts": "2026-08-17T23:00:00Z",
                "session_id": "unknown",
                "seq": 1,
                "session_epoch": 7,
                "event_type": "session_start",
                "payload": {
                    "model": model,
                    "model_provider_id": "openai",
                    "model_provider_name": "OpenAI"
                }
            }),
            json!({
                "schema_version": 2,
                "ts": "2026-08-17T23:00:01Z",
                "session_id": "thread-1",
                "seq": 8,
                "session_epoch": 7,
                "event_type": "thread/settings/updated",
                "payload": {
                    "threadId": "thread-1",
                    "threadSettings": {
                        "model": model,
                        "modelProvider": "openai",
                        "effort": effort
                    }
                }
            }),
        ]
    }

    fn attest(events: &[Value]) -> Result<PolicyRuntimeAttestation, RuntimeAttestationError> {
        attest_codex_fork_launch(
            events,
            &expected(),
            "decision-1",
            "intent-1",
            "child-1",
            "thread-1",
        )
    }

    #[test]
    fn codex_launch_requires_agreeing_provider_originated_profile_events() {
        let attestation = attest(&events("gpt-5.6-terra", "high")).unwrap();

        assert_eq!(attestation.model, "gpt-5.6-terra");
        assert_eq!(attestation.effort, "high");
        assert_eq!(attestation.session_id, "child-1");
        assert_eq!(attestation.evidence_id, "codex-fork:thread-1:7:8");
    }

    #[test]
    fn codex_launch_rejects_omitted_or_wrong_requested_tier() {
        let error = attest(&events("gpt-5.6-sol", "high")).unwrap_err();
        assert_eq!(error.code, "codex_launch_profile_mismatch");

        let error = attest(&events("gpt-5.6-terra", "xhigh")).unwrap_err();
        assert_eq!(error.code, "codex_launch_profile_mismatch");
    }

    #[test]
    fn codex_launch_rejects_conflicting_start_and_settings_models() {
        let mut evidence = events("gpt-5.6-terra", "high");
        evidence[0]["payload"]["model"] = json!("gpt-5.6-sol");

        let error = attest(&evidence).unwrap_err();
        assert_eq!(error.code, "conflicting_codex_launch_profile");
    }

    #[test]
    fn codex_launch_rejects_unversioned_or_foreign_thread_evidence() {
        let mut unversioned = events("gpt-5.6-terra", "high");
        unversioned[1]
            .as_object_mut()
            .unwrap()
            .remove("schema_version");
        let error = attest(&unversioned).unwrap_err();
        assert_eq!(error.code, "unsupported_codex_event_schema");

        let mut foreign = events("gpt-5.6-terra", "high");
        foreign[1]["session_id"] = json!("thread-2");
        foreign[1]["payload"]["threadId"] = json!("thread-2");
        let error = attest(&foreign).unwrap_err();
        assert_eq!(error.code, "missing_codex_launch_settings");
    }

    #[test]
    fn codex_launch_uses_initial_settings_not_later_turn_changes() {
        let mut evidence = events("gpt-5.6-terra", "high");
        evidence.push(json!({
            "schema_version": 2,
            "ts": "2026-08-17T23:05:00Z",
            "session_id": "thread-1",
            "seq": 50,
            "session_epoch": 7,
            "event_type": "thread/settings/updated",
            "payload": {
                "threadId": "thread-1",
                "threadSettings": {
                    "model": "gpt-5.6-sol",
                    "modelProvider": "openai",
                    "effort": "xhigh"
                }
            }
        }));

        assert!(attest(&evidence).is_ok());
    }
}
