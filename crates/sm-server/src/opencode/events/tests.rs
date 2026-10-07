use super::*;
use crate::opencode::tests::ScratchDir;

fn user(id: &str, text: &str) -> Value {
    json!({"info": {"id": id, "sessionID": "ses_test", "role": "user", "time": {"created": 1}},
        "parts": [{"id": "prt_prompt", "sessionID": "ses_test", "messageID": id, "type": "text", "text": text}]})
}
fn usage_part(id: &str) -> Value {
    json!({"id": id, "type": "step-finish", "sessionID": "ses_test", "messageID": "msg_02",
        "tokens": {"input": 120, "output": 7, "reasoning": 3, "cache": {"read": 900, "write": 20}}})
}
fn tool_part(id: &str, status: &str) -> Value {
    json!({"id": id, "type": "tool", "sessionID": "ses_test", "messageID": "msg_02", "callID": "call-1",
        "tool": "bash", "state": {"status": status, "input": {"command": "git status"}}})
}
fn assistant(id: &str, completed: bool, finish: &str, parts: Vec<Value>) -> Value {
    let mut info =
        json!({"id": id, "sessionID": "ses_test", "role": "assistant", "time": {"created": 2}});
    if completed {
        info["time"]["completed"] = json!(3);
        info["finish"] = json!(finish);
    }
    json!({"info": info, "parts": parts})
}
fn live_part(part: Value) -> Value {
    json!({"type": "message.part.updated", "properties": {"part": part}})
}
fn starts(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::TurnStart { .. }))
        .count()
}
fn stops(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::TurnStop { .. }))
        .count()
}
fn tools(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Tool { .. }))
        .count()
}
fn usages(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Usage(_)))
        .count()
}

#[test]
fn reconnect_backfills_missed_whole_turn_once_across_checkpoint_reopen() {
    let mut projection = Projection::new("ses_test", Activity::Idle).unwrap();
    let history = vec![
        user("msg_01", "brief"),
        assistant(
            "msg_02",
            true,
            "tool-calls",
            vec![tool_part("prt_tool", "completed"), usage_part("prt_usage1")],
        ),
        assistant(
            "msg_03",
            true,
            "stop",
            vec![
                usage_part("prt_usage2"),
                json!({"id": "prt_text", "type": "text", "sessionID": "ses_test", "messageID": "msg_03", "text": "done"}),
            ],
        ),
    ];
    let generated = BTreeSet::from(["msg_01".into()]);
    let effects = projection
        .backfill(&history, Activity::Idle, &generated)
        .unwrap();
    assert_eq!(starts(&effects), 1);
    assert_eq!(stops(&effects), 1);
    assert_eq!(tools(&effects), 1);
    assert_eq!(usages(&effects), 2);
    assert!(!effects
        .iter()
        .any(|e| matches!(e, Effect::OwnerPrompt { .. })));
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::TurnStop { text, .. } if text == "done")));
    assert_eq!(projection.cursor.as_deref(), Some("msg_03"));
    let mut reopened: Projection =
        serde_json::from_slice(&serde_json::to_vec(&projection).unwrap()).unwrap();
    assert!(reopened
        .backfill(&history, Activity::Idle, &generated)
        .unwrap()
        .is_empty());
}

#[test]
fn two_disconnects_during_one_assistant_keep_cursor_and_replay_only_new_parts() {
    let mut projection = Projection::new("ses_test", Activity::Idle).unwrap();
    let mut history = vec![
        user("msg_01", "brief"),
        assistant("msg_02", false, "", vec![tool_part("prt_tool1", "running")]),
    ];
    let effects = projection
        .backfill(&history, Activity::Busy, &BTreeSet::new())
        .unwrap();
    assert_eq!(starts(&effects), 1);
    assert_eq!(tools(&effects), 1);
    assert_eq!(stops(&effects), 0);
    assert_eq!(projection.cursor.as_deref(), Some("msg_02"));
    history[1]["parts"]
        .as_array_mut()
        .unwrap()
        .extend([tool_part("prt_tool2", "completed"), usage_part("prt_usage")]);
    let effects = projection
        .backfill(&history, Activity::Busy, &BTreeSet::new())
        .unwrap();
    assert_eq!(starts(&effects), 0);
    assert_eq!(tools(&effects), 1);
    assert_eq!(usages(&effects), 1);
    history[1]["info"]["time"]["completed"] = json!(5);
    history[1]["info"]["finish"] = json!("stop");
    let effects = projection
        .backfill(&history, Activity::Idle, &BTreeSet::new())
        .unwrap();
    assert_eq!(stops(&effects), 1);
    assert_eq!(tools(&effects), 0);
    assert_eq!(usages(&effects), 0);
    assert!(projection
        .backfill(&history, Activity::Idle, &BTreeSet::new())
        .unwrap()
        .is_empty());
}

#[test]
fn reconnect_during_submission_does_not_fabricate_a_start_or_stop() {
    for generated in [false, true] {
        let mut projection = Projection::new("ses_test", Activity::Idle).unwrap();
        // A previous turn's prompt must not leak into a metadata-only start.
        projection.last_user = "previous prompt".into();
        let ids = if generated {
            BTreeSet::from(["msg_01".into()])
        } else {
            BTreeSet::new()
        };
        let prompt = user("msg_01", "actual prompt");
        let mut metadata_only = prompt.clone();
        metadata_only["parts"] = json!([]);
        for _ in 0..2 {
            let effects = projection
                .backfill(&[metadata_only.clone()], Activity::Idle, &ids)
                .unwrap();
            assert_eq!(starts(&effects), 0);
            assert_eq!(stops(&effects), 0);
            assert_eq!(projection.activity, Activity::Idle);
            projection = serde_json::from_slice(&serde_json::to_vec(&projection).unwrap()).unwrap();
        }
        for _ in 0..2 {
            let effects = projection
                .backfill(std::slice::from_ref(&prompt), Activity::Idle, &ids)
                .unwrap();
            assert_eq!(starts(&effects), 0);
            assert_eq!(stops(&effects), 0);
            assert_eq!(projection.activity, Activity::Idle);
        }
        let busy = json!({"type":"session.status", "properties":{"sessionID":"ses_test", "status":{"type":"busy"}}});
        let effects = projection.live(&busy, &ids).unwrap();
        assert_eq!(
            effects,
            vec![Effect::TurnStart {
                message_id: None,
                prompt: "actual prompt".into()
            }]
        );
        assert_eq!(
            starts(
                &projection
                    .backfill(std::slice::from_ref(&prompt), Activity::Busy, &ids)
                    .unwrap()
            ),
            0
        );
        let history = vec![prompt, assistant("msg_02", true, "stop", vec![])];
        let effects = projection.backfill(&history, Activity::Idle, &ids).unwrap();
        assert_eq!(starts(&effects), 0);
        assert_eq!(stops(&effects), 1);
        assert!(projection
            .backfill(&history, Activity::Idle, &ids)
            .unwrap()
            .is_empty());
    }
}

#[test]
fn assistant_or_busy_status_proves_a_turn_started_before_user_text_is_available() {
    for activity in [Activity::Busy, Activity::Retry] {
        let mut projection = Projection::new("ses_test", Activity::Idle).unwrap();
        projection.last_user = "previous prompt".into();
        let mut prompt = user("msg_01", "not persisted yet");
        prompt["parts"] = json!([]);
        let effects = projection
            .backfill(&[prompt], activity, &BTreeSet::new())
            .unwrap();
        assert_eq!(starts(&effects), 1);
        assert_eq!(stops(&effects), 0);
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::TurnStart { prompt, .. } if prompt.is_empty())));
    }
    let mut projection = Projection::new("ses_test", Activity::Idle).unwrap();
    let mut prompt = user("msg_01", "not persisted yet");
    prompt["parts"] = json!([]);
    let effects = projection
        .backfill(
            &[prompt, assistant("msg_02", true, "stop", vec![])],
            Activity::Idle,
            &BTreeSet::new(),
        )
        .unwrap();
    assert_eq!(starts(&effects), 1);
    assert_eq!(stops(&effects), 1);
}

#[test]
fn busy_and_idle_reconnect_matches_do_not_fabricate_transitions() {
    for activity in [Activity::Idle, Activity::Busy, Activity::Retry] {
        let mut projection = Projection::new("ses_test", activity).unwrap();
        assert!(projection
            .backfill(&[], activity, &BTreeSet::new())
            .unwrap()
            .is_empty());
    }
    let mut projection = Projection::new("ses_test", Activity::Busy).unwrap();
    assert_eq!(
        stops(
            &projection
                .backfill(&[], Activity::Idle, &BTreeSet::new())
                .unwrap()
        ),
        1
    );
    assert!(projection
        .backfill(&[], Activity::Idle, &BTreeSet::new())
        .unwrap()
        .is_empty());
}

#[test]
fn live_prompt_parts_arrive_before_busy_and_supply_the_turn_start_text() {
    let mut projection = Projection::new("ses_test", Activity::Idle).unwrap();
    let prompt = user("msg_01", "new prompt");
    let effects = projection
        .live(
            &json!({"type":"message.updated", "properties":{"info":prompt["info"]}}),
            &BTreeSet::new(),
        )
        .unwrap();
    assert_eq!(starts(&effects), 0);
    assert!(!effects
        .iter()
        .any(|e| matches!(e, Effect::OwnerPrompt { .. })));
    let effects = projection
        .live(&live_part(prompt["parts"][0].clone()), &BTreeSet::new())
        .unwrap();
    assert_eq!(
        effects,
        vec![Effect::OwnerPrompt {
            message_id: "msg_01".into(),
            text: "new prompt".into(),
        }]
    );
    assert!(!projection
        .live(&live_part(prompt["parts"][0].clone()), &BTreeSet::new())
        .unwrap()
        .iter()
        .any(|e| matches!(e, Effect::OwnerPrompt { .. })));
    let effects = projection
        .live(
            &json!({"type":"session.status", "properties":{"sessionID":"ses_test", "status":{"type":"busy"}}}),
            &BTreeSet::new(),
        )
        .unwrap();
    assert_eq!(
        effects,
        vec![Effect::TurnStart {
            message_id: None,
            prompt: "new prompt".into(),
        }]
    );
}

#[test]
fn live_stop_is_not_reapplied_by_history_and_owner_typing_while_busy_is_observed() {
    let mut projection = Projection::new("ses_test", Activity::Busy).unwrap();
    let owner = user("msg_01", "owner answer");
    let event = json!({"type": "message.updated", "properties": {"info": owner["info"]}});
    let effects = projection.live(&event, &BTreeSet::new()).unwrap();
    assert!(!effects
        .iter()
        .any(|e| matches!(e, Effect::OwnerPrompt { .. })));
    let effects = projection
        .live(&live_part(owner["parts"][0].clone()), &BTreeSet::new())
        .unwrap();
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::OwnerPrompt { message_id, text } if message_id == "msg_01" && text == "owner answer")));
    assert_eq!(starts(&effects), 0);
    assert_eq!(
        starts(&projection.live(&event, &BTreeSet::new()).unwrap()),
        0
    );
    let info = assistant("msg_02", false, "", vec![])["info"].clone();
    projection
        .live(
            &json!({"type":"message.updated", "properties":{"info":info}}),
            &BTreeSet::new(),
        )
        .unwrap();
    let idle = json!({"type":"session.status", "properties":{"sessionID":"ses_test", "status":{"type":"idle"}}});
    assert_eq!(stops(&projection.live(&idle, &BTreeSet::new()).unwrap()), 1);
    let history = vec![owner, assistant("msg_02", true, "stop", vec![])];
    assert_eq!(
        stops(
            &projection
                .backfill(&history, Activity::Idle, &BTreeSet::new())
                .unwrap()
        ),
        0
    );
}

#[test]
fn pending_owner_reply_survives_reopen_and_backfill_cursor_does_not_skip_its_text() {
    let mut projection = Projection::new("ses_test", Activity::Busy).unwrap();
    let mut prompt = user("msg_01", "reply after disconnect");
    let parts = prompt["parts"].take();
    prompt["parts"] = json!([]);
    let history = vec![prompt.clone(), assistant("msg_02", false, "", vec![])];
    projection
        .backfill(&history, Activity::Busy, &BTreeSet::new())
        .unwrap();
    assert_eq!(projection.cursor.as_deref(), Some("msg_01"));
    let mut reopened: Projection =
        serde_json::from_slice(&serde_json::to_vec(&projection).unwrap()).unwrap();
    prompt["parts"] = parts;
    let history = vec![prompt, assistant("msg_02", false, "", vec![])];
    let effects = reopened
        .backfill(&history, Activity::Busy, &BTreeSet::new())
        .unwrap();
    assert_eq!(
        effects,
        vec![Effect::OwnerPrompt {
            message_id: "msg_01".into(),
            text: "reply after disconnect".into(),
        }]
    );
    assert_eq!(reopened.cursor.as_deref(), Some("msg_02"));
    assert!(reopened
        .backfill(&history, Activity::Busy, &BTreeSet::new())
        .unwrap()
        .is_empty());
}

#[test]
fn generated_classification_survives_reopen_and_text_before_metadata_is_supported() {
    for generated in [false, true] {
        let mut projection = Projection::new("ses_test", Activity::Busy).unwrap();
        let prompt = user("msg_01", "message body");
        let ids = if generated {
            BTreeSet::from(["msg_01".into()])
        } else {
            BTreeSet::new()
        };
        let metadata = json!({"type":"message.updated", "properties":{"info":prompt["info"]}});
        let part = live_part(prompt["parts"][0].clone());
        projection.live(&metadata, &ids).unwrap();
        let mut reopened: Projection =
            serde_json::from_slice(&serde_json::to_vec(&projection).unwrap()).unwrap();
        let effects = reopened.live(&part, &BTreeSet::new()).unwrap();
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, Effect::OwnerPrompt { text, .. } if text == "message body"))
                .count(),
            usize::from(!generated)
        );
        assert!(!reopened
            .live(&metadata, &BTreeSet::new())
            .unwrap()
            .iter()
            .any(|e| matches!(e, Effect::OwnerPrompt { .. })));

        let mut projection = Projection::new("ses_test", Activity::Busy).unwrap();
        assert!(!projection
            .live(&part, &ids)
            .unwrap()
            .iter()
            .any(|e| matches!(e, Effect::OwnerPrompt { .. })));
        let effects = projection.live(&metadata, &ids).unwrap();
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, Effect::OwnerPrompt { text, .. } if text == "message body"))
                .count(),
            usize::from(!generated)
        );
    }
}

#[test]
fn clearing_changes_event_scope_and_title_error_compaction_are_explicit_effects() {
    let mut projection = Projection::new("ses_new", Activity::Idle).unwrap();
    assert!(projection
        .live(
            &live_part(tool_part("prt_tool", "running")),
            &BTreeSet::new()
        )
        .unwrap()
        .is_empty());
    assert!(projection
        .backfill(&[user("msg_01", "old")], Activity::Idle, &BTreeSet::new())
        .unwrap()
        .is_empty());
    let cases = [
        (
            json!({"type":"session.updated", "properties":{"info":{"id":"ses_new", "title":"new title"}}}),
            Effect::Title("new title".into()),
        ),
        (
            json!({"type":"session.error", "properties":{"sessionID":"ses_new", "error":"offline"}}),
            Effect::Error("\"offline\"".into()),
        ),
        (
            json!({"type":"session.compacted", "properties":{"sessionID":"ses_new"}}),
            Effect::Compacted,
        ),
    ];
    for (event, expected) in cases {
        assert_eq!(
            projection.live(&event, &BTreeSet::new()).unwrap(),
            vec![expected]
        );
    }
    assert_eq!(projection.activity, Activity::Idle);
}

#[test]
fn usage_journal_recovers_partial_tail_and_deduplicates_across_reopen() {
    let tmp = ScratchDir::new();
    let path = tmp.path().join("usage.jsonl");
    let usage = Usage::from_part(&usage_part("prt_usage")).unwrap();
    assert_eq!(usage.output, 10);
    assert_eq!(usage.total_input().unwrap(), 1040);
    let mut journal = UsageJournal::open(&path).unwrap();
    assert!(journal
        .append(&usage, "/repo", "flash", "2026-10-07T00:00:00Z")
        .unwrap());
    assert!(!journal
        .append(&usage, "/repo", "flash", "2026-10-07T00:00:00Z")
        .unwrap());
    drop(journal);
    let original = std::fs::read(&path).unwrap();
    let line = std::str::from_utf8(&original).unwrap();
    assert!(line.contains("\"usage\":{"));
    let value: Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(value["sessionId"], "ses_test");
    assert_eq!(value["message"]["usage"]["output_tokens"], 10);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"type\":")
        .unwrap();
    let mut reopened = UsageJournal::open(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!reopened
        .append(&usage, "/repo", "flash", "2026-10-07T00:00:00Z")
        .unwrap());
    let mut next = usage.clone();
    next.part_id = "prt_next".into();
    next.conversation_id = "ses_new".into();
    assert!(reopened
        .append(&next, "/repo", "flash", "2026-10-07T00:00:00Z")
        .unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
}

#[test]
fn malformed_usage_is_not_marked_seen_and_corrupt_complete_journal_fails_closed() {
    let mut projection = Projection::new("ses_test", Activity::Busy).unwrap();
    let mut part = usage_part("prt_usage");
    part["tokens"]["input"] = json!("bad");
    assert!(projection.live(&live_part(part), &BTreeSet::new()).is_err());
    assert_eq!(
        usages(
            &projection
                .live(&live_part(usage_part("prt_usage")), &BTreeSet::new())
                .unwrap()
        ),
        1
    );
    let tmp = ScratchDir::new();
    let path = tmp.path().join("usage.jsonl");
    std::fs::write(&path, b"broken complete line\n").unwrap();
    assert!(UsageJournal::open(&path).is_err());
}

#[test]
fn sse_reads_multiline_json_ignores_heartbeats_and_rejects_oversized_frames() {
    let input = b": heartbeat\r\n\r\nevent: message\r\ndata: {\"type\":\r\ndata: \"session.status\",\"properties\":{}}\r\n\r\n";
    let mut reader = &input[..];
    assert_eq!(
        read_event(&mut reader).unwrap(),
        Some(json!({"type":"session.status", "properties":{}}))
    );
    assert_eq!(read_event(&mut reader).unwrap(), None);
    let oversized = vec![b'x'; MAX_FRAME_BYTES + 2];
    assert!(read_event(&mut &oversized[..]).is_err());
    assert!(read_event(&mut &b"data: broken\n\n"[..]).is_err());
    assert_eq!(read_event(&mut &b"data: {\"partial\":"[..]).unwrap(), None);
}

#[test]
fn frozen_real_proof_history_emits_one_stop_per_turn_not_one_per_model_request() {
    let history: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/proof-history.json")).unwrap();
    let conversation = history[0]["info"]["sessionID"].as_str().unwrap();
    let mut projection = Projection::new(conversation, Activity::Idle).unwrap();
    let terminal_messages = history
        .iter()
        .filter(|message| {
            message["info"]["role"] == "assistant"
                && message["info"]["finish"] != "tool-calls"
                && message["info"]["time"]["completed"].is_number()
        })
        .count();
    let effects = projection
        .backfill(&history, Activity::Idle, &BTreeSet::new())
        .unwrap();
    assert!(usages(&effects) > stops(&effects));
    assert_eq!(stops(&effects), terminal_messages);
    assert_eq!(
        projection
            .backfill(&history, Activity::Idle, &BTreeSet::new())
            .unwrap(),
        vec![]
    );
}
