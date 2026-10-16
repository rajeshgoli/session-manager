//! Current owner decisions, independent of the bounded recent-event display.
use super::*;
pub fn reasons(conn: &Connection, a: &Value) -> Result<Vec<String>> {
    let root = a["root"].as_str().unwrap_or("");
    let name = a["name"].as_str().unwrap_or("");
    let events=store::query(conn,"SELECT kind,MAX(id) id FROM artifact_events WHERE root=?1 AND name=?2 COLLATE NOCASE GROUP BY kind",&[&root,&name])?;
    let last = |kind: &str| {
        events
            .iter()
            .find(|e| e["kind"] == kind)
            .and_then(|e| e["id"].as_i64())
            .unwrap_or(0)
    };
    let acted = [
        "owner_delete",
        "owner_thaw",
        "owner_keep_live",
        "owner_investigate",
        "owner_opened",
        "owner_action",
    ]
    .iter()
    .map(|k| last(k))
    .max()
    .unwrap_or(0);
    let mut reasons = Vec::new();
    let unrecorded =
        a["recorded_at"].is_null() && matches!(a["state"].as_str(), Some("unrecorded" | "frozen"));
    if unrecorded
        && (a["owner_session_id"].is_null() || last("unclaimable") > last("recorded"))
        && acted == 0
    {
        reasons.push("unrecorded".into());
    }
    if a["state"] != "deleted" {
        let set = store::query(
            conn,
            "SELECT * FROM artifacts WHERE root=?1 AND set_id=?2",
            &[&root, &a["set_id"].as_str().unwrap_or(name)],
        )?;
        let eligible = if a["state"] == "building" {
            a["project"] != 1 && ensure_not_in_use(conn, root, name).is_ok()
        } else {
            score::deletion_eligible(conn, root, &set).is_ok()
        };
        if eligible && last("would_delete") > acted.max(last("recorded")).max(last("marks")) {
            reasons.push("would_delete".into());
        }
        if a["project"] == 1 && last("project_added") > acted {
            reasons.push("project_added".into());
        }
        let recovered = ["frozen", "thawed", "moved", "rebuilt", "recorded"]
            .iter()
            .map(|k| last(k))
            .max()
            .unwrap_or(0);
        if last("op_failed").max(last("rebuild_failed")) > acted.max(recovered) {
            reasons.push("operation_failed".into());
        }
        let unknown:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM artifact_consumers WHERE root=?1 AND name=?2 AND COALESCE(last_state,'UNKNOWN')='UNKNOWN' AND checked_at IS NOT NULL)",params![root,name],|r|r.get(0))?;
        if unknown {
            reasons.push("consumer_unverifiable".into());
        }
    }
    if a["delete_reason"] == "vanished" && last("vanished").max(last("deleted")) > acted {
        reasons.push("vanished".into());
    }
    Ok(reasons)
}
pub fn item(conn: &Connection, a: &Value) -> Result<Option<Value>> {
    let reasons = reasons(conn, a)?;
    if reasons.is_empty() {
        return Ok(None);
    }
    let mut buttons = vec!["delete"];
    if reasons.iter().any(|r| {
        matches!(
            r.as_str(),
            "unrecorded" | "would_delete" | "operation_failed"
        )
    }) {
        buttons.push("investigate");
    }
    Ok(Some(
        json!({"root":a["root"],"name":a["name"],"reasons":reasons,"buttons":buttons}),
    ))
}
