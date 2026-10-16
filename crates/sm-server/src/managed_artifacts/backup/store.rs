use super::*;
use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Rates {
    pub fetch: f64,
    pub apply: f64,
    pub probe: f64,
    pub effective: f64,
}

/// Call from the acknowledgement transaction for fetch/apply, and after a fully
/// received probe for probe. Invalid measurements fail before writing anything.
pub fn record_rate(
    conn: &Connection,
    device: &str,
    kind: &str,
    bytes: u64,
    seconds: f64,
) -> Result<()> {
    ensure!(
        matches!(kind, "fetch" | "apply" | "probe"),
        "unknown backup rate kind"
    );
    ensure!(
        seconds.is_finite() && seconds > 0.0 && bytes > 0 && bytes <= i64::MAX as u64,
        "invalid backup rate sample"
    );
    conn.execute(
        "INSERT INTO backup_rate_samples(device,kind,bytes,seconds) VALUES(?1,?2,?3,?4)",
        params![device, kind, bytes, seconds],
    )?;
    Ok(())
}

pub fn rates(conn: &Connection, device: &str, default: f64) -> Result<Rates> {
    ensure!(
        default.is_finite() && default > 0.0,
        "invalid default backup rate"
    );
    let median = |kind: &str, count: i64| -> Result<f64> {
        let mut stmt = conn.prepare("SELECT bytes,seconds FROM backup_rate_samples WHERE device=?1 AND kind=?2 AND (kind='probe' OR bytes>1000000000) ORDER BY id DESC LIMIT ?3")?;
        let mut values = stmt
            .query_map(params![device, kind, count], |r| {
                Ok(r.get::<_, f64>(0)? / r.get::<_, f64>(1)?)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            values.iter().all(|v| v.is_finite() && *v > 0.0),
            "invalid persisted backup rate"
        );
        values.sort_by(f64::total_cmp);
        Ok(if values.is_empty() {
            default
        } else {
            (values[(values.len() - 1) / 2] + values[values.len() / 2]) / 2.0
        })
    };
    let fetch = median("fetch", 10)?;
    let apply = median("apply", 10)?;
    let probe = median("probe", 4)?;
    Ok(Rates {
        fetch,
        apply,
        probe,
        effective: fetch.min(apply).min(probe),
    })
}

/// Manifests are immutable, device-scoped, and remain unconfirmed until ack.
pub fn save_manifest(conn: &Connection, device: &str, manifest: &Manifest) -> Result<()> {
    ensure!(!manifest.manifest_id.is_empty(), "empty manifest id");
    for (path, file) in &manifest.files {
        safe_relative(path)?;
        ensure!(path == &file.path, "manifest path mismatch");
    }
    let body = zstd::stream::encode_all(serde_json::to_vec(manifest)?.as_slice(), 3)?;
    conn.execute(
        "INSERT INTO backup_manifests(device,manifest_id,created_at,body) VALUES(?1,?2,?3,?4)",
        params![device, manifest.manifest_id, super::super::now(), body],
    )?;
    Ok(())
}
pub fn load_manifest(conn: &Connection, device: &str, id: &str) -> Result<Manifest> {
    let body: Vec<u8> = conn.query_row(
        "SELECT body FROM backup_manifests WHERE device=?1 AND manifest_id=?2",
        params![device, id],
        |r| r.get(0),
    )?;
    let manifest: Manifest = serde_json::from_slice(&zstd::stream::decode_all(body.as_slice())?)?;
    ensure!(
        manifest.manifest_id == id,
        "stored manifest identity mismatch"
    );
    Ok(manifest)
}
pub fn confirmed(conn: &Connection, device: &str) -> Result<Option<Manifest>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT manifest_id FROM backup_confirmed WHERE device=?1",
            [device],
            |r| r.get(0),
        )
        .optional()?;
    id.map(|id| load_manifest(conn, device, &id)).transpose()
}

/// Caller supplies its acknowledgement transaction. Compare-and-swap prevents a
/// stale preparation or seed from replacing a newer confirmed manifest.
pub fn confirm(tx: &rusqlite::Transaction<'_>, device: &str, id: &str) -> Result<()> {
    let manifest = load_manifest(tx, device, id)?;
    let current = confirmed(tx, device)?;
    if current.as_ref().is_some_and(|m| m.manifest_id == id) {
        return Ok(());
    }
    ensure!(
        current.as_ref().map(|m| &m.manifest_id) == manifest.base.as_ref(),
        "confirmed backup manifest changed"
    );
    let already: Option<String> = tx.query_row(
        "SELECT acked_at FROM backup_manifests WHERE device=?1 AND manifest_id=?2",
        params![device, id],
        |r| r.get(0),
    )?;
    ensure!(already.is_none(), "cannot confirm a superseded manifest");
    tx.execute("INSERT INTO backup_confirmed(device,manifest_id) VALUES(?1,?2) ON CONFLICT(device) DO UPDATE SET manifest_id=excluded.manifest_id",params![device,id]).context("confirm backup")?;
    tx.execute(
        "UPDATE backup_manifests SET acked_at=?3 WHERE device=?1 AND manifest_id=?2",
        params![device, id, super::super::now()],
    )?;
    Ok(())
}
