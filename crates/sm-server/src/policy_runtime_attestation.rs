use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::{Map, Value};

use crate::policy_contracts::{
    PolicyLaunchProfile, PolicyRuntimeAttestation, RuntimeAttestationEvidence, ATTESTATION_SCHEMA,
};

const CODEX_FORK_PROVIDER: &str = "codex-fork";
const CODEX_PROVIDER_ID: &str = "openai";
const MAX_RELEVANT_LAUNCH_EVENTS: usize = 128;

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

pub fn attest_codex_fork_launch_file(
    event_stream_path: &Path,
    expected: &PolicyLaunchProfile,
    decision_id: &str,
    launch_intent_id: &str,
    child_session_id: &str,
    provider_resume_id: &str,
) -> Result<PolicyRuntimeAttestation, RuntimeAttestationError> {
    let file = File::open(event_stream_path).map_err(|error| {
        RuntimeAttestationError::new(
            "codex_event_stream_unavailable",
            format!(
                "failed to open provider event stream {}: {error}",
                event_stream_path.display()
            ),
        )
    })?;
    let mut relevant = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| {
            RuntimeAttestationError::new(
                "codex_event_stream_read_failed",
                format!(
                    "failed to read provider event stream {}: {error}",
                    event_stream_path.display()
                ),
            )
        })?;
        let event = serde_json::from_str::<Value>(&line).map_err(|error| {
            RuntimeAttestationError::new(
                "invalid_codex_event_json",
                format!(
                    "provider event stream {} contains invalid JSON: {error}",
                    event_stream_path.display()
                ),
            )
        })?;
        let event_type = event.get("event_type").and_then(Value::as_str);
        if !matches!(
            event_type,
            Some("session_start" | "thread/settings/updated")
        ) {
            continue;
        }
        let is_target_settings = event_type == Some("thread/settings/updated")
            && event.get("session_id").and_then(Value::as_str) == Some(provider_resume_id)
            && event
                .get("payload")
                .and_then(|payload| payload.get("threadId"))
                .and_then(Value::as_str)
                == Some(provider_resume_id);
        relevant.push(event);
        if relevant.len() > MAX_RELEVANT_LAUNCH_EVENTS {
            return Err(RuntimeAttestationError::new(
                "codex_launch_evidence_limit_exceeded",
                format!(
                    "provider launch evidence exceeded {MAX_RELEVANT_LAUNCH_EVENTS} relevant events"
                ),
            ));
        }
        if is_target_settings {
            break;
        }
    }

    attest_codex_fork_launch(
        &relevant,
        expected,
        decision_id,
        launch_intent_id,
        child_session_id,
        provider_resume_id,
    )
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
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

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

    #[test]
    fn codex_launch_file_reads_real_jsonl_shape_and_stops_at_initial_settings() {
        let path = unique_temp_path("codex-launch-events");
        let mut evidence = events("gpt-5.6-terra", "high");
        evidence.insert(
            1,
            json!({
                "schema_version": 2,
                "ts": "2026-08-17T23:00:00.500Z",
                "session_id": "thread-1",
                "seq": 4,
                "session_epoch": 7,
                "event_type": "op_submitted",
                "payload": {"UserTurn": {"items": [{"type": "text", "text": "brief"}]}}
            }),
        );
        evidence.push(json!({"this later line": "does not need to be parsed"}));
        let body = evidence
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, body).unwrap();

        let attestation = attest_codex_fork_launch_file(
            &path,
            &expected(),
            "decision-1",
            "intent-1",
            "child-1",
            "thread-1",
        )
        .unwrap();

        assert_eq!(attestation.evidence_id, "codex-fork:thread-1:7:8");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn codex_launch_file_fails_closed_on_malformed_evidence_before_settings() {
        let path = unique_temp_path("codex-launch-malformed");
        let start = events("gpt-5.6-terra", "high").remove(0);
        fs::write(&path, format!("{}\nnot-json\n", start)).unwrap();

        let error = attest_codex_fork_launch_file(
            &path,
            &expected(),
            "decision-1",
            "intent-1",
            "child-1",
            "thread-1",
        )
        .unwrap_err();

        assert_eq!(error.code, "invalid_codex_event_json");
        fs::remove_file(path).unwrap();
    }

    fn unique_temp_path(label: &str) -> std::path::PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "sm-policy-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
