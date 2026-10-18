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
        if !query(&conn, "PRAGMA table_info(artifact_completions)", &[])?
            .iter()
            .any(|c| c["name"] == "placement")
        {
            conn.execute(
                "ALTER TABLE artifact_completions ADD COLUMN placement TEXT",
                [],
            )?;
        }
        if !query(&conn, "PRAGMA table_info(backup_runs)", &[])?
            .iter()
            .any(|c| c["name"] == "update_json")
        {
            conn.execute("ALTER TABLE backup_runs ADD COLUMN update_json TEXT", [])?;
        }
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
    value["operation"] = query(conn, "SELECT id,kind,phase,created_at,finished_at,error FROM artifact_operations WHERE root=?1 AND set_id=?2 ORDER BY id DESC LIMIT 1", &[&root, &value["set_id"].as_str().unwrap_or(name)])?.pop().unwrap_or(Value::Null);
    value["thaw_eta_seconds"] =
        json!(value["data_bytes"].as_u64().unwrap_or(0) as f64 / thaw_rate(conn, root)?);
    let jobs = value["jobs"].as_array().unwrap();
    value["rebuild_seconds"] = json!(jobs
        .iter()
        .filter(|j| j["state"] == "succeeded")
        .map(|j| j["wall_seconds"].as_f64().unwrap_or(0.0))
        .sum::<f64>());
    let missing_input: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM artifact_inputs i LEFT JOIN artifacts a ON a.root=i.root AND a.name=i.input WHERE i.root=?1 AND i.name=?2 AND (a.name IS NULL OR a.state='deleted'))", rusqlite::params![root,name], |r| r.get(0))?;
    value["rebuildable"] =
        json!(value["recipe_complete"] == 1 && !has_gpu(&value) && !missing_input);
    value["dependents"] = json!(query(conn, "SELECT DISTINCT i.name FROM artifact_inputs i JOIN artifacts a ON a.root=i.root AND a.name=i.name WHERE i.root=?1 AND i.input IN (SELECT name FROM artifacts WHERE root=?1 AND set_id=?2) AND a.state!='deleted' AND a.set_id<>?2", &[&root, &value["set_id"].as_str().unwrap_or(name)])?);
    value["hot"] = json!(super::hotcold::is_hot(
        conn,
        root,
        name,
        meta(conn, &format!("cold_days:{root}"))?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(14),
        OffsetDateTime::now_utc()
    )?);
    value["consumer_unverifiable"] = json!(value["consumers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["state"] == "UNKNOWN" && c["checked_at"].is_string()));
    value["backup_due"] =
        json!(value["backup_stamp"] != value["content_stamp"] || value["backed_up_at"].is_null());
    let needs = super::needs_you::item(conn, &value)?;
    value["needs_owner"] = json!(needs.is_some());
    value["needs_you"] = needs.unwrap_or(Value::Null);
    value["investigating"] = json!(meta(
        conn,
        &format!("investigating:{root}:{}", name.to_ascii_lowercase())
    )?
    .filter(|s| !s.is_empty()));
    value["size_error"] = json!(meta(conn, &format!("size_error:{root}:{name}"))?);
    value["report"] = query(conn, "SELECT actor,at,detail_json FROM artifact_events WHERE root=?1 AND name=?2 COLLATE NOCASE AND kind='report' ORDER BY id DESC LIMIT 1", &[&root,&name])?.pop().unwrap_or(Value::Null);
    Ok(value)
}
pub fn thaw_rate(conn: &Connection, root: &str) -> Result<f64> {
    let samples = query(conn,"SELECT detail_json FROM artifact_events WHERE root=?1 AND kind='thaw_measured' ORDER BY id DESC LIMIT 5",&[&root])?;
    let mut rates = Vec::new();
    for sample in samples {
        let v: Value = serde_json::from_str(sample["detail_json"].as_str().unwrap_or("{}"))?;
        let seconds = v["seconds"].as_f64().unwrap_or(0.0);
        let bytes = v["data_bytes"].as_f64().unwrap_or(0.0);
        if seconds > 0.0 && bytes > 0.0 {
            rates.push(bytes / seconds);
        }
    }
    if rates.len() < 5 {
        return Ok(100_000_000_000.0 / 60.0);
    }
    rates.sort_by(f64::total_cmp);
    Ok(rates[2])
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
