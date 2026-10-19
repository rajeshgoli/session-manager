//! What the Artifacts page shows about freezes, thaws and moves (sm#2325):
//! operations in flight with their place in line and an ETA, finished
//! operations with who asked for them, the space freezing saves, and where
//! each folder's bytes live.
use super::*;
use std::collections::HashMap;

/// Finished operations stay in the in-flight list this long, so the page
/// shows a freeze reach done before it drops into history.
const RECENT_SECONDS: i64 = 15 * 60;
const HISTORY: usize = 40;
/// ETAs use the median rate of this many recent measured operations.
const RATE_SAMPLES: usize = 5;

/// Where a folder's bytes are: the internal root, the cold disk, or (frozen)
/// the archive on the cold disk. Deleted folders have none.
pub fn placement(root: &ArtifactRoot, a: &Value) -> Value {
    let name = a["name"].as_str().unwrap_or_default();
    let (disk, path) = match (a["state"].as_str(), a["location"].as_str()) {
        (Some("deleted"), _) => return Value::Null,
        (Some("frozen"), _) => ("cold", format!("{}/.frozen/{name}", root.cold_store)),
        (_, Some("cold")) => ("cold", format!("{}/{name}", root.cold_store)),
        _ => ("root", format!("{}/{name}", root.path)),
    };
    json!({"disk":disk,"disk_path":if disk == "cold" { &root.cold_store } else { &root.path },"path":path})
}

/// Adds `placement`, `saved_bytes` (frozen: data minus archive) and
/// `last_thaw` (the newest measured thaw in the record's events).
pub fn enrich(root: &ArtifactRoot, a: &mut Value) {
    a["placement"] = placement(root, a);
    a["saved_bytes"] = match (
        a["state"].as_str(),
        a["data_bytes"].as_u64(),
        a["frozen_bytes"].as_u64(),
    ) {
        (Some("frozen"), Some(data), Some(frozen)) => json!(data.saturating_sub(frozen)),
        _ => Value::Null,
    };
    a["last_thaw"] = a["events"]
        .as_array()
        .and_then(|events| events.iter().find(|e| e["kind"] == "thaw_measured"))
        .and_then(|e| {
            let detail: Value = serde_json::from_str(e["detail_json"].as_str()?).ok()?;
            Some(
                json!({"at":e["at"],"seconds":detail["seconds"],"data_bytes":detail["data_bytes"]}),
            )
        })
        .unwrap_or(Value::Null);
}

fn parse(at: &Value) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(at.as_str()?, &Rfc3339).ok()
}
fn seconds(from: OffsetDateTime, to: OffsetDateTime) -> f64 {
    (to - from).as_seconds_f64()
}
fn median(mut v: Vec<f64>) -> Option<f64> {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied()
}

struct Op {
    row: Value,
    names: Vec<String>,
    started: Option<OffsetDateTime>,
    finished: Option<OffsetDateTime>,
    data_bytes: Option<u64>,
    frozen_bytes: Option<u64>,
}

/// Operations in flight and recently finished, history, and freeze savings.
/// `as_of` is the time every relative field is measured from.
pub fn activity(manager: &ManagedArtifacts, as_of: OffsetDateTime) -> Result<Value> {
    let conn = manager.store.open()?;
    let rows = store::query(
        &conn,
        "SELECT * FROM artifact_operations WHERE phase NOT IN ('done','failed') OR id IN \
         (SELECT id FROM artifact_operations WHERE phase IN ('done','failed') ORDER BY id DESC LIMIT 60) ORDER BY id",
        &[],
    )?;
    let latest: HashMap<(String, String), i64> = store::query(
        &conn,
        "SELECT root,set_id,MAX(id) id FROM artifact_operations GROUP BY root,set_id",
        &[],
    )?
    .into_iter()
    .map(|r| {
        (
            (
                r["root"].as_str().unwrap_or_default().into(),
                r["set_id"].as_str().unwrap_or_default().into(),
            ),
            r["id"].as_i64().unwrap_or(0),
        )
    })
    .collect();
    let mut ops = Vec::new();
    for row in rows {
        let id = row["id"].as_i64().unwrap_or(0);
        let root = row["root"].as_str().unwrap_or_default().to_owned();
        let names: Vec<String> =
            serde_json::from_str::<Vec<Value>>(row["members_json"].as_str().unwrap_or("[]"))?
                .iter()
                .filter_map(|m| m["name"].as_str().map(str::to_owned))
                .collect();
        let started = store::meta(&conn, &format!("operation_started:{id}"))?
            .and_then(|s| s.parse::<i128>().ok())
            .and_then(|ns| OffsetDateTime::from_unix_timestamp_nanos(ns).ok());
        let done = row["phase"] == "done";
        let current = latest.get(&(
            root.clone(),
            row["set_id"].as_str().unwrap_or_default().into(),
        )) == Some(&id);
        let (mut data, mut frozen) = (Some(0u64), Some(0u64));
        for name in &names {
            let a = store::artifact(&conn, &root, name).unwrap_or(Value::Null);
            // A finished operation's sizes are the ones its completion event
            // recorded; older events carry none, and the folder's current
            // sizes describe that operation only while no later one ran.
            let recorded = if done {
                store::query(&conn, "SELECT detail_json FROM artifact_events WHERE root=?1 AND name=?2 AND kind IN ('frozen','thawed','moved') AND json_extract(detail_json,'$.operation_id')=?3 ORDER BY id DESC LIMIT 1", &[&root, name, &id])?
                    .pop()
                    .and_then(|e| serde_json::from_str::<Value>(e["detail_json"].as_str()?).ok())
                    .filter(|d| d["data_bytes"].is_u64())
            } else {
                None
            };
            let (d, f) = match recorded {
                Some(r) => (r["data_bytes"].as_u64(), r["frozen_bytes"].as_u64()),
                None if !done || current => (
                    a["data_bytes"].as_u64(),
                    a["frozen_bytes"]
                        .as_u64()
                        .filter(|_| a["state"] == "frozen"),
                ),
                None => (None, None),
            };
            data = data.zip(d).map(|(x, y)| x + y);
            frozen = frozen.zip(f).map(|(x, y)| x + y);
        }
        ops.push(Op {
            finished: parse(&row["finished_at"]),
            frozen_bytes: frozen.filter(|_| row["kind"] == "freezing" && done),
            row,
            names,
            started,
            data_bytes: data,
        });
    }
    // Bytes per second for each kind, from the latest measured operations.
    let mut rates: HashMap<String, f64> = HashMap::new();
    for kind in ["freezing", "moving"] {
        let samples: Vec<f64> = ops
            .iter()
            .rev()
            .filter(|o| o.row["kind"] == kind && o.row["phase"] == "done")
            .filter_map(|o| {
                let s = seconds(o.started?, o.finished?);
                let bytes = o.data_bytes? as f64;
                (s > 0.0 && bytes > 0.0).then_some(bytes / s)
            })
            .take(RATE_SAMPLES)
            .collect();
        if let Some(rate) = median(samples) {
            rates.insert(kind.into(), rate);
        }
    }
    let expected = |o: &Op, rates: &HashMap<String, f64>| -> Result<Option<f64>> {
        let kind = o.row["kind"].as_str().unwrap_or_default();
        let rate = if kind == "thawing" {
            Some(store::thaw_rate(
                &conn,
                o.row["root"].as_str().unwrap_or_default(),
            )?)
        } else {
            rates.get(kind).copied()
        };
        Ok(rate.zip(o.data_bytes).map(|(r, b)| b as f64 / r))
    };
    let active: Vec<&Op> = ops
        .iter()
        .filter(|o| !matches!(o.row["phase"].as_str(), Some("done" | "failed")))
        .collect();
    let running = |o: &Op| {
        matches!(
            o.row["phase"].as_str(),
            Some("copying" | "prepared" | "cleanup")
        )
    };
    // Operations run one at a time in id order; a waiting operation starts
    // once everything ahead of it finishes.
    let mut queue_seconds = Some(0.0f64);
    let mut in_flight = Vec::new();
    for (index, o) in active.iter().enumerate() {
        let want = expected(o, &rates)?;
        let is_running = running(o);
        let elapsed = o
            .started
            .filter(|_| is_running)
            .map(|s| seconds(s, as_of).max(0.0));
        let starts_in = (!is_running).then_some(queue_seconds).flatten();
        let left = match elapsed {
            Some(e) => want.map(|w| (w - e).max(0.0)),
            None => want,
        };
        // Seconds until this one finishes, counting everything ahead of it.
        let remaining = queue_seconds.zip(left).map(|(q, l)| q + l);
        queue_seconds = remaining;
        let later = &active[index + 1..];
        in_flight.push(json!({
            "state": if is_running { "running" } else { "waiting" },
            "ahead": if is_running { 0 } else { index },
            "behind": later.iter().filter(|a| !is_running || !running(a)).count(),
            "expected_seconds": want,
            "elapsed_seconds": elapsed,
            "remaining_seconds": remaining,
            "starts_in_seconds": starts_in,
            // Running well past its estimate: the page stops counting down.
            "over_estimate": elapsed.zip(want).is_some_and(|(e, w)| e > w * 1.25 + 30.0),
        }));
    }
    let item = |o: &Op, extra: Value| -> Value {
        let mut v = json!({
            "id": o.row["id"], "root": o.row["root"], "set_id": o.row["set_id"], "kind": o.row["kind"],
            "phase": o.row["phase"], "names": o.names, "actor": o.row["actor"],
            "created_at": o.row["created_at"], "started_at": o.started.map(|s| s.format(&Rfc3339).unwrap_or_default()),
            "finished_at": o.row["finished_at"], "error": o.row["error"],
            "data_bytes": o.data_bytes, "frozen_bytes": o.frozen_bytes,
            "seconds": o.started.zip(o.finished).map(|(s, f)| seconds(s, f)),
        });
        if let (Some(v), Some(extra)) = (v.as_object_mut(), extra.as_object()) {
            v.extend(extra.clone());
        }
        v
    };
    let mut operations: Vec<Value> = active
        .iter()
        .zip(in_flight)
        .map(|(o, extra)| item(o, extra))
        .collect();
    let recent: Vec<Value> = ops
        .iter()
        .rev()
        .filter(|o| {
            o.finished
                .is_some_and(|f| seconds(f, as_of) <= RECENT_SECONDS as f64)
        })
        .map(|o| item(o, json!({"state": o.row["phase"]})))
        .collect();
    // In flight first, in queue order; then finished, newest first.
    operations.extend(recent);
    let history: Vec<Value> = ops
        .iter()
        .rev()
        .filter(|o| matches!(o.row["phase"].as_str(), Some("done" | "failed")))
        .take(HISTORY)
        .map(|o| item(o, json!({"state": o.row["phase"]})))
        .collect();
    let mut roots = Vec::new();
    let mut total = (0u64, 0u64, 0u64, 0u64);
    for root in &manager.config.roots {
        let (count, data, frozen, saved): (u64, u64, u64, u64) = conn.query_row(
            "SELECT COUNT(*),COALESCE(SUM(data_bytes),0),COALESCE(SUM(frozen_bytes),0),\
             COALESCE(SUM(MAX(data_bytes-frozen_bytes,0)),0) FROM artifacts WHERE root=?1 AND state='frozen'",
            [&root.path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        total = (
            total.0 + count,
            total.1 + data,
            total.2 + frozen,
            total.3 + saved,
        );
        roots.push(json!({"root":root.path,"cold_store":root.cold_store,"folders":count,"data_bytes":data,"frozen_bytes":frozen,"saved_bytes":saved}));
    }
    Ok(json!({
        "as_of": as_of.format(&Rfc3339)?,
        "operations": operations,
        "history": history,
        "savings": {"roots": roots, "folders": total.0, "data_bytes": total.1, "frozen_bytes": total.2, "saved_bytes": total.3},
    }))
}

#[cfg(test)]
mod tests;
