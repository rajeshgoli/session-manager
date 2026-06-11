use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Duration, OffsetDateTime,
    PrimitiveDateTime,
};

use crate::{
    config::AppConfig,
    sessions::{expand_home, SessionRecord, SessionStore},
};

const WINDOW_HOURS: i64 = 24;
const BUCKET_HOURS: i64 = 2;

pub fn build_mobile_analytics_summary(
    config: &AppConfig,
    session_store: &SessionStore,
) -> Result<Value> {
    let now = OffsetDateTime::now_utc();
    build_mobile_analytics_summary_at(config, session_store, now)
}

fn build_mobile_analytics_summary_at(
    config: &AppConfig,
    session_store: &SessionStore,
    now: OffsetDateTime,
) -> Result<Value> {
    let current_start = now - Duration::hours(WINDOW_HOURS);
    let previous_start = now - Duration::hours(WINDOW_HOURS * 2);
    let sessions = session_store.list_sessions(false)?;
    let queue_path = expand_home(&config.mobile_analytics.message_queue_db);
    let server_log_path = expand_home(&config.mobile_analytics.server_log_file);

    let (send_times_current, send_times_previous) =
        read_send_timestamps(&queue_path, current_start, previous_start, now);
    let track_times_current = read_track_remind_timestamps(&queue_path, current_start, now);
    let (active_tracks, overdue_tracks) = read_track_registration_counts(&queue_path);
    let (spawn_times_current, spawn_times_previous, restart_count, self_heal_count) =
        read_log_metrics(&server_log_path, current_start, previous_start, now);

    let sends_series = series_points(&send_times_current, current_start, now);
    let spawn_series = series_points(&spawn_times_current, current_start, now);
    let track_series = series_points(&track_times_current, current_start, now);

    let total_sessions = sessions.len() as i64;
    let state_counts = activity_state_counts(&sessions);
    let provider_distribution = provider_distribution(&sessions);
    let (repo_distribution, total_tokens_live) = repo_distribution(&sessions, total_sessions);
    let longest_running = longest_running_sessions(&sessions, now);

    Ok(json!({
        "generated_at": format_rfc3339(now),
        "window_hours": WINDOW_HOURS,
        "kpis": {
            "active_sessions": {
                "label": "Active sessions",
                "value": total_sessions,
            },
            "sends_24h": {
                "label": "Sends",
                "value": send_times_current.len(),
                "delta_pct": delta_pct(send_times_current.len(), send_times_previous.len()),
            },
            "spawns_24h": {
                "label": "Dispatches",
                "value": spawn_times_current.len(),
                "delta_pct": delta_pct(spawn_times_current.len(), spawn_times_previous.len()),
            },
            "active_tracks": {
                "label": "Tracks active",
                "value": active_tracks,
            },
            "overdue_tracks": {
                "label": "Overdue tracks",
                "value": overdue_tracks,
            },
            "incidents_24h": {
                "label": "Incidents",
                "value": restart_count + self_heal_count,
            },
        },
        "throughput": throughput_series(sends_series, spawn_series, track_series),
        "state_distribution": [
            {"key": "working", "label": "working", "count": *state_counts.get("working").unwrap_or(&0)},
            {"key": "thinking", "label": "thinking", "count": *state_counts.get("thinking").unwrap_or(&0)},
            {"key": "waiting", "label": "waiting", "count": *state_counts.get("waiting").unwrap_or(&0)},
            {"key": "idle", "label": "idle", "count": *state_counts.get("idle").unwrap_or(&0)},
        ],
        "provider_distribution": provider_distribution,
        "repo_distribution": repo_distribution,
        "longest_running": longest_running,
        "reliability": {
            "restart_count_24h": restart_count,
            "self_heal_count_24h": self_heal_count,
        },
        "totals": {
            "tokens_live": total_tokens_live,
            "track_reminders_24h": track_times_current.len(),
        },
        "health_checks": [],
        "attach_available": true,
    }))
}

fn read_send_timestamps(
    db_path: &Path,
    current_start: OffsetDateTime,
    previous_start: OffsetDateTime,
    now: OffsetDateTime,
) -> (Vec<OffsetDateTime>, Vec<OffsetDateTime>) {
    let mut current = Vec::new();
    let mut previous = Vec::new();
    let rows = query_timestamp_column(
        db_path,
        r#"
        SELECT queued_at
        FROM message_queue
        WHERE from_sm_send = 1
          AND queued_at >= ?1
          AND queued_at < ?2
        "#,
        &[format_rfc3339(previous_start), format_rfc3339(now)],
    );
    for timestamp in rows {
        if timestamp >= current_start {
            current.push(timestamp);
        } else {
            previous.push(timestamp);
        }
    }
    (current, previous)
}

fn read_track_remind_timestamps(
    db_path: &Path,
    current_start: OffsetDateTime,
    now: OffsetDateTime,
) -> Vec<OffsetDateTime> {
    query_timestamp_column(
        db_path,
        r#"
        SELECT queued_at
        FROM message_queue
        WHERE message_category = 'track_remind'
          AND queued_at >= ?1
          AND queued_at < ?2
        "#,
        &[format_rfc3339(current_start), format_rfc3339(now)],
    )
}

fn query_timestamp_column(db_path: &Path, sql: &str, params: &[String; 2]) -> Vec<OffsetDateTime> {
    let Ok(conn) = open_existing_read_only_db(db_path) else {
        return Vec::new();
    };
    let Ok(mut statement) = conn.prepare(sql) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([params[0].as_str(), params[1].as_str()], |row| {
        row.get::<_, String>(0)
    }) else {
        return Vec::new();
    };
    rows.filter_map(|row| row.ok())
        .filter_map(|value| parse_any_datetime(&value))
        .collect()
}

fn read_track_registration_counts(db_path: &Path) -> (i64, i64) {
    let Ok(conn) = open_existing_read_only_db(db_path) else {
        return (0, 0);
    };
    conn.query_row(
        r#"
        SELECT
            SUM(CASE WHEN is_active = 1 AND cancel_on_reply_session_id IS NOT NULL AND TRIM(cancel_on_reply_session_id) != '' THEN 1 ELSE 0 END),
            SUM(CASE WHEN is_active = 1 AND soft_fired = 1 AND cancel_on_reply_session_id IS NOT NULL AND TRIM(cancel_on_reply_session_id) != '' THEN 1 ELSE 0 END)
        FROM remind_registrations
        "#,
        [],
        |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
            ))
        },
    )
    .unwrap_or((0, 0))
}

fn open_existing_read_only_db(path: &Path) -> Result<Connection, rusqlite::Error> {
    if !path.exists() {
        return Err(rusqlite::Error::InvalidPath(path.to_path_buf()));
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

fn read_log_metrics(
    log_path: &Path,
    current_start: OffsetDateTime,
    previous_start: OffsetDateTime,
    now: OffsetDateTime,
) -> (Vec<OffsetDateTime>, Vec<OffsetDateTime>, i64, i64) {
    let Ok(content) = fs::read_to_string(log_path) else {
        return (Vec::new(), Vec::new(), 0, 0);
    };
    let mut current_spawns = Vec::new();
    let mut previous_spawns = Vec::new();
    let mut restart_count = 0;
    let mut self_heal_count = 0;
    for line in content.lines() {
        let Some(timestamp) = parse_log_timestamp(line) else {
            continue;
        };
        if timestamp < previous_start || timestamp >= now {
            continue;
        }
        if line.contains("Created session ") && !line.contains("Created session with CLI prompt") {
            if timestamp >= current_start {
                current_spawns.push(timestamp);
            } else {
                previous_spawns.push(timestamp);
            }
        }
        if timestamp >= current_start && line.contains("Starting Claude Session Manager...") {
            restart_count += 1;
        }
        if timestamp >= current_start && line.contains("Recovered ") {
            self_heal_count += 1;
        }
    }
    (
        current_spawns,
        previous_spawns,
        restart_count,
        self_heal_count,
    )
}

fn series_points(
    timestamps: &[OffsetDateTime],
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
) -> Vec<(OffsetDateTime, i64)> {
    let bucket_count = ((window_end - window_start).whole_seconds() / (BUCKET_HOURS * 3600)) as i64;
    let mut buckets = (0..bucket_count)
        .map(|index| (window_start + Duration::hours(index * BUCKET_HOURS), 0_i64))
        .collect::<Vec<_>>();
    for timestamp in timestamps {
        if *timestamp < window_start || *timestamp >= window_end {
            continue;
        }
        let elapsed_hours = (*timestamp - window_start).whole_seconds() / 3600;
        let bucket_index = (elapsed_hours / BUCKET_HOURS).clamp(0, bucket_count.saturating_sub(1));
        if let Some((_, count)) = buckets.get_mut(bucket_index as usize) {
            *count += 1;
        }
    }
    buckets
}

fn throughput_series(
    sends: Vec<(OffsetDateTime, i64)>,
    spawns: Vec<(OffsetDateTime, i64)>,
    tracks: Vec<(OffsetDateTime, i64)>,
) -> Vec<Value> {
    sends
        .into_iter()
        .enumerate()
        .map(|(index, (bucket_start, send_count))| {
            json!({
                "bucket_start": format_rfc3339(bucket_start),
                "bucket_label": format_hour_minute(bucket_start),
                "sends": send_count,
                "spawns": spawns.get(index).map(|(_, count)| *count).unwrap_or(0),
                "track_reminders": tracks.get(index).map(|(_, count)| *count).unwrap_or(0),
            })
        })
        .collect()
}

fn activity_state_counts(sessions: &[SessionRecord]) -> HashMap<&'static str, i64> {
    let mut counts = HashMap::new();
    for session in sessions {
        *counts.entry(activity_state(session)).or_insert(0) += 1;
    }
    counts
}

fn activity_state(session: &SessionRecord) -> &'static str {
    match session.status.trim().to_ascii_lowercase().as_str() {
        "running" | "starting" => "working",
        "thinking" => "thinking",
        "waiting_permission" | "waiting_input" => "waiting",
        _ => "idle",
    }
}

fn provider_distribution(sessions: &[SessionRecord]) -> Vec<Value> {
    let mut first_seen = Vec::<String>::new();
    let mut counts = HashMap::<String, i64>::new();
    for session in sessions {
        let provider = non_empty_or(&session.provider, "claude");
        if !counts.contains_key(&provider) {
            first_seen.push(provider.clone());
        }
        *counts.entry(provider).or_insert(0) += 1;
    }
    first_seen.sort_by(|left, right| {
        counts
            .get(right)
            .unwrap_or(&0)
            .cmp(counts.get(left).unwrap_or(&0))
            .then_with(|| left.cmp(right))
    });
    let total = sessions.len() as f64;
    first_seen
        .into_iter()
        .map(|provider| {
            let count = *counts.get(&provider).unwrap_or(&0);
            json!({
                "key": provider,
                "label": provider,
                "count": count,
                "share_pct": percentage(count, total),
            })
        })
        .collect()
}

fn repo_distribution(sessions: &[SessionRecord], total_sessions: i64) -> (Vec<Value>, i64) {
    let mut repos = BTreeMap::<String, (i64, i64)>::new();
    let mut total_tokens = 0_i64;
    for session in sessions {
        let repo = repo_label(&session.working_dir);
        let entry = repos.entry(repo).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += session.tokens_used;
        total_tokens += session.tokens_used;
    }
    let mut rows = repos
        .into_iter()
        .map(|(repo, (session_count, tokens_used))| {
            (
                repo,
                session_count,
                tokens_used,
                percentage(session_count, total_sessions as f64),
            )
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    (
        rows.into_iter()
            .take(6)
            .map(|(repo, session_count, tokens_used, share_pct)| {
                json!({
                    "key": repo,
                    "label": repo,
                    "session_count": session_count,
                    "tokens_used": tokens_used,
                    "share_pct": share_pct,
                })
            })
            .collect(),
        total_tokens,
    )
}

fn longest_running_sessions(sessions: &[SessionRecord], now: OffsetDateTime) -> Vec<Value> {
    let mut rows = sessions
        .iter()
        .map(|session| {
            let age_hours = parse_any_datetime(&session.created_at)
                .map(|created_at| round_one((now - created_at).whole_seconds() as f64 / 3600.0))
                .unwrap_or(0.0);
            (
                age_hours,
                display_name(session),
                json!({
                    "id": session.id,
                    "name": display_name(session),
                    "repo": repo_label(&session.working_dir),
                    "provider": non_empty_or(&session.provider, "claude"),
                    "age_hours": age_hours,
                }),
            )
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1.cmp(&right.1))
    });
    rows.into_iter()
        .take(5)
        .map(|(_, _, value)| value)
        .collect()
}

fn display_name(session: &SessionRecord) -> String {
    non_empty_opt(&session.friendly_name)
        .unwrap_or_else(|| non_empty_or(&session.name, &session.id))
}

fn repo_label(working_dir: &str) -> String {
    let normalized = working_dir.trim();
    if normalized.is_empty() {
        return "unknown".to_owned();
    }
    normalized
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(normalized)
        .to_owned()
}

fn delta_pct(current: usize, previous: usize) -> Option<f64> {
    if previous == 0 {
        return None;
    }
    Some(round_one(
        ((current as f64 - previous as f64) / previous as f64) * 100.0,
    ))
}

fn percentage(count: i64, total: f64) -> f64 {
    if total <= 0.0 {
        return 0.0;
    }
    round_one((count as f64 / total) * 100.0)
}

fn round_one(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn parse_any_datetime(value: &str) -> Option<OffsetDateTime> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(parsed) = OffsetDateTime::parse(value, &Rfc3339) {
        return Some(parsed.to_offset(time::UtcOffset::UTC));
    }
    parse_python_naive_datetime(value).map(|parsed| parsed.assume_utc())
}

fn parse_log_timestamp(line: &str) -> Option<OffsetDateTime> {
    let prefix = line.get(..19)?;
    parse_python_naive_datetime(prefix).map(|parsed| parsed.assume_utc())
}

fn parse_python_naive_datetime(value: &str) -> Option<PrimitiveDateTime> {
    let value = value.trim().replace('T', " ");
    let seconds = value.get(..19).unwrap_or(value.as_str());
    PrimitiveDateTime::parse(
        seconds,
        &format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
    )
    .ok()
}

fn format_rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_else(|_| value.to_string())
}

fn format_hour_minute(value: OffsetDateTime) -> String {
    value
        .format(format_description!("[hour]:[minute]"))
        .unwrap_or_else(|_| "".to_owned())
}

fn non_empty_opt(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn non_empty_or(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}
