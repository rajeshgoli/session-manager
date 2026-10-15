//! Attribution and the durable daily request decision, independent of delivery.
use super::*;

/// The first live source wins: queue writer, ticket holder, registered steward.
/// Lookups are lazy so a winning writer never requires claim/registry access.
pub fn select<T>(
    writer: impl FnOnce() -> Result<Option<T>>,
    holder: impl FnOnce() -> Result<Option<T>>,
    steward: impl FnOnce() -> Result<Option<T>>,
) -> Result<Option<(T, bool)>> {
    if let Some(owner) = writer()? {
        return Ok(Some((owner, false)));
    }
    if let Some(owner) = holder()? {
        return Ok(Some((owner, false)));
    }
    Ok(steward()?.map(|owner| (owner, true)))
}
pub fn request_due(conn: &Connection, root: &str, name: &str, date: &str) -> Result<bool> {
    let count:i64=conn.query_row(
        "SELECT COUNT(*) FROM artifact_events WHERE root=?1 AND name=?2 COLLATE NOCASE AND (kind='unclaimable' OR (kind='request_sent' AND substr(at,1,10)=?3))",
        params![root,name,date],|row|row.get(0))?;
    Ok(count == 0)
}
/// Local calendar fields from libc: unlike time's local-offset lookup this
/// works after the server has started its background threads.
pub fn local_calendar(at: OffsetDateTime) -> Result<(String, String)> {
    let timestamp = at.unix_timestamp() as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // localtime_r writes only caller-owned storage.
    let result = unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) };
    if result.is_null() {
        bail!("cannot resolve local artifact schedule");
    }
    let local = unsafe { local.assume_init() };
    Ok((
        format!(
            "{:04}-{:02}-{:02}",
            local.tm_year + 1900,
            local.tm_mon + 1,
            local.tm_mday
        ),
        format!("{:02}:{:02}", local.tm_hour, local.tm_min),
    ))
}
pub fn size_walk_due(
    previous: Option<OffsetDateTime>,
    now: OffsetDateTime,
    nightly: &str,
    perf: bool,
) -> Result<bool> {
    let (date, clock) = local_calendar(now)?;
    let previous_date = previous.map(local_calendar).transpose()?.map(|v| v.0);
    if previous.is_some_and(|p| (now - p).whole_hours() >= 36) {
        return Ok(true);
    }
    Ok(!perf && clock.as_str() >= nightly && previous_date.as_deref() != Some(&date))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attribution_order_and_dead_sources() {
        assert_eq!(
            select(
                || Ok(Some("writer")),
                || panic!("claim lookup"),
                || panic!("steward lookup")
            )
            .unwrap(),
            Some(("writer", false))
        );
        assert_eq!(
            select(
                || Ok(None),
                || Ok(Some("holder")),
                || panic!("steward lookup")
            )
            .unwrap(),
            Some(("holder", false))
        );
        assert_eq!(
            select(|| Ok(None), || Ok(None), || Ok(Some("steward"))).unwrap(),
            Some(("steward", true))
        );
        assert_eq!(
            select::<&str>(|| Ok(None), || Ok(None), || Ok(None)).unwrap(),
            None
        );
    }
    #[test]
    fn requests_repeat_at_most_daily_and_unclaimable_stops_them() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        let today = OffsetDateTime::now_utc().date().to_string();
        assert!(request_due(&conn, "/root", "123_data", &today).unwrap());
        store::event(&conn, "/root", "123_data", "request_sent", "sm", json!({})).unwrap();
        assert!(!request_due(&conn, "/root", "123_DATA", &today).unwrap());
        assert!(request_due(&conn, "/root", "123_data", "2099-01-01").unwrap());
        store::event(
            &conn,
            "/root",
            "123_data",
            "unclaimable",
            "steward",
            json!({}),
        )
        .unwrap();
        assert!(!request_due(&conn, "/root", "123_data", "2099-01-01").unwrap());
    }
    #[test]
    fn nightly_walk_defers_for_perf_but_not_beyond_36_hours() {
        let now = OffsetDateTime::now_utc();
        assert!(!size_walk_due(None, now, "00:00", true).unwrap());
        assert!(size_walk_due(None, now, "00:00", false).unwrap());
        assert!(!size_walk_due(Some(now), now, "00:00", false).unwrap());
        assert!(size_walk_due(Some(now - time::Duration::hours(36)), now, "23:59", true).unwrap());
    }
}
