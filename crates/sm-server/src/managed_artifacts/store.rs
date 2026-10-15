use super::*;
use rusqlite::{params, types::ValueRef, Connection, OptionalExtension};
use std::{fs, path::PathBuf, time::Duration};

#[derive(Clone)]
pub struct ArtifactStore {
    pub path: PathBuf,
}
impl ArtifactStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch(include_str!("schema.sql"))?;
        Ok(conn)
    }
    pub fn list(&self, root: &str) -> Result<Vec<Value>> {
        query(
            &self.open()?,
            "SELECT * FROM artifacts WHERE root=?1 ORDER BY name COLLATE NOCASE",
            &[&root],
        )
    }
    pub fn detail(&self, root: &str, name: &str) -> Result<Value> {
        detail(&self.open()?, root, name)
    }
}
pub fn query(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<_> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let rows = stmt
        .query_map(args, |row| {
            let mut obj = serde_json::Map::new();
            for (i, name) in names.iter().enumerate() {
                let value = match row.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => json!(n),
                    ValueRef::Real(n) => json!(n),
                    ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    ValueRef::Blob(_) => Value::Null,
                };
                obj.insert(name.clone(), value);
            }
            Ok(Value::Object(obj))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
pub fn artifact(conn: &Connection, root: &str, name: &str) -> Result<Value> {
    query(
        conn,
        "SELECT * FROM artifacts WHERE root=?1 AND name=?2",
        &[&root, &name],
    )?
    .pop()
    .ok_or_else(|| anyhow::anyhow!("artifact not found: {name}"))
}
pub fn detail(conn: &Connection, root: &str, name: &str) -> Result<Value> {
    let mut value = artifact(conn, root, name)?;
    for (key,sql) in [
        ("consumers","SELECT *, repo || '#' || number AS ref, COALESCE(last_state,'UNKNOWN') AS state FROM artifact_consumers WHERE root=?1 AND name=?2"),
        ("signals","SELECT * FROM artifact_signals WHERE root=?1 AND name=?2"),
        ("jobs","SELECT * FROM artifact_jobs WHERE root=?1 AND name=?2 ORDER BY seq"),
        ("inputs","SELECT * FROM artifact_inputs WHERE root=?1 AND name=?2"),
        ("aliases","SELECT * FROM artifact_aliases WHERE root=?1 AND target_name=?2"),
        ("events","SELECT * FROM artifact_events WHERE root=?1 AND name=?2 COLLATE NOCASE ORDER BY id DESC LIMIT 50"),
    ] {value[key]=json!(query(conn,sql,&[&root,&name])?);}
    let jobs = value["jobs"].as_array().unwrap();
    value["rebuild_seconds"] = json!(jobs
        .iter()
        .filter(|j| j["state"] == "succeeded")
        .map(|j| j["wall_seconds"].as_f64().unwrap_or(0.0))
        .sum::<f64>());
    value["rebuildable"] = json!(value["recipe_complete"] == 1 && !has_gpu(&value));
    // Until the GitHub and named-reference evaluator lands, unknown retention is hot.
    value["hot"] = json!(true);
    value["backup_due"] =
        json!(value["backup_stamp"] != value["content_stamp"] || value["backed_up_at"].is_null());
    value["needs_owner"] = json!(
        value["op"]
            .as_str()
            .is_some_and(|op| op.starts_with("rebuild_failed:"))
            || value["delete_reason"] == "vanished"
            || value["events"].as_array().unwrap().iter().any(|e| matches!(
                e["kind"].as_str(),
                Some("unclaimable" | "op_failed" | "would_delete" | "project_added")
            ))
            || (value["state"] == "unrecorded" && value["owner_session_id"].is_null())
    );
    Ok(value)
}
pub fn has_gpu(a: &Value) -> bool {
    a["gpu_hours"].as_f64().unwrap_or(0.0) > 0.0 || a["gpu_cost_usd"].as_f64().unwrap_or(0.0) > 0.0
}
pub fn event(
    conn: &Connection,
    root: &str,
    name: &str,
    kind: &str,
    actor: &str,
    detail: Value,
) -> Result<()> {
    conn.execute("INSERT INTO artifact_events(root,name,at,kind,actor,detail_json) VALUES(?1,?2,?3,?4,?5,?6)",params![root,name,now(),kind,actor,detail.to_string()])?;
    Ok(())
}
pub fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM artifact_meta WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute("INSERT INTO artifact_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
    Ok(())
}
