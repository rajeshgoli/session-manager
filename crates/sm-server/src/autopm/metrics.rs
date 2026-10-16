//! The server-run metric (F.2). sm runs every measurement from the query
//! committed with the plan, so a successor agent cannot reinterpret one.

use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use super::{Paths, DAY_MS};

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Fewer observations than this in either half extends, then reverts.
pub const MIN_OBSERVATIONS: usize = 10;
pub const BASELINE_DAYS: i64 = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stat {
    Median,
    Mean,
    PerDay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Lower,
    Higher,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSpec {
    pub query: String,
    pub stat: Stat,
    pub direction: Direction,
    pub min_change_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardSpec {
    pub query: String,
    pub stat: Stat,
    pub bad_direction: Direction,
    pub max_worse_pct: f64,
}

fn valid_query_path(path: &str) -> bool {
    path.starts_with("docs/autopm/metrics/")
        && path.ends_with(".sql")
        && !path.contains("..")
        && !path.contains('\\')
}

pub fn parse_metric(value: &Value) -> Result<MetricSpec, String> {
    let spec: MetricSpec =
        serde_json::from_value(value.clone()).map_err(|e| format!("metric_json: {e}"))?;
    if !valid_query_path(&spec.query) {
        return Err("metric_json.query must be docs/autopm/metrics/<name>.sql".into());
    }
    if !(spec.min_change_pct.is_finite() && spec.min_change_pct > 0.0) {
        return Err("metric_json.min_change_pct must be above 0".into());
    }
    Ok(spec)
}

pub fn parse_guards(value: &Value) -> Result<Vec<GuardSpec>, String> {
    let guards: Vec<GuardSpec> =
        serde_json::from_value(value.clone()).map_err(|e| format!("guards_json: {e}"))?;
    for guard in &guards {
        if !valid_query_path(&guard.query) {
            return Err("each guards_json query must be docs/autopm/metrics/<name>.sql".into());
        }
        if !(guard.max_worse_pct.is_finite() && guard.max_worse_pct >= 0.0) {
            return Err("guards_json max_worse_pct must be 0 or more".into());
        }
    }
    Ok(guards)
}

/// A stat over one window and its observation count.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub stat: Option<f64>,
    pub n: usize,
}

/// `median` averages the middle pair when n is even; `per_day` is the row
/// count over the window's days. An empty window has no median or mean.
pub fn stat(kind: Stat, values: &[f64], days: f64) -> Option<f64> {
    match kind {
        Stat::PerDay => Some(values.len() as f64 / days),
        _ if values.is_empty() => None,
        Stat::Mean => Some(values.iter().sum::<f64>() / values.len() as f64),
        Stat::Median => {
            let mut sorted = values.to_vec();
            sorted.sort_by(f64::total_cmp);
            let mid = sorted.len() / 2;
            Some(if sorted.len() % 2 == 0 {
                (sorted[mid - 1] + sorted[mid]) / 2.0
            } else {
                sorted[mid]
            })
        }
    }
}

/// The rows in `[from, until)`, as a stat over that window.
pub fn sample(kind: Stat, rows: &[(i64, f64)], from: i64, until: i64) -> Sample {
    let values: Vec<f64> = rows
        .iter()
        .filter(|(at, _)| (from..until).contains(at))
        .map(|(_, value)| *value)
        .collect();
    let days = (until - from) as f64 / DAY_MS as f64;
    Sample {
        stat: stat(kind, &values, days),
        n: values.len(),
    }
}

/// Relative change from the baseline; undefined when either side is.
pub fn change(baseline: Sample, half: Sample) -> Option<f64> {
    let (base, value) = (baseline.stat?, half.stat?);
    (base != 0.0).then(|| (value - base) / base)
}

pub fn meets_bar(spec: &MetricSpec, baseline: Sample, half: Sample) -> bool {
    let bar = spec.min_change_pct / 100.0;
    change(baseline, half).is_some_and(|change| match spec.direction {
        Direction::Lower => change <= -bar,
        Direction::Higher => change >= bar,
    })
}

/// Whether a guard worsened past its limit in one half. A zero baseline
/// makes the percentage undefined, so one rule replaces it for every stat.
pub fn guard_fails(spec: &GuardSpec, baseline: Sample, half: Sample) -> bool {
    let Some(value) = half.stat else {
        return false;
    };
    let base = baseline.stat.unwrap_or(0.0);
    if base == 0.0 {
        return match (spec.stat, spec.bad_direction) {
            (Stat::PerDay, Direction::Higher) => value >= 2.0,
            (Stat::PerDay, Direction::Lower) => false,
            (_, Direction::Higher) => value > 0.0,
            (_, Direction::Lower) => value < 0.0,
        };
    }
    let limit = spec.max_worse_pct / 100.0;
    let change = (value - base) / base;
    match spec.bad_direction {
        Direction::Higher => change > limit,
        Direction::Lower => change < -limit,
    }
}

/// The frozen baseline (F.2 `baseline_json`): the metric's and each guard's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub metric: Sample,
    pub guards: Vec<Sample>,
}

/// The halves of a measurement window.
pub fn halves(live_at: i64, window_days: i64) -> [(i64, i64); 2] {
    let mid = live_at + window_days * DAY_MS / 2;
    [(live_at, mid), (mid, live_at + window_days * DAY_MS)]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Keep,
    Revert,
    Extend,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Revert => "revert",
            Self::Extend => "extend",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub decision: Decision,
    pub note: String,
    pub result: Value,
}

/// One guard's samples over the baseline and the two halves.
pub struct GuardResult<'a> {
    pub spec: &'a GuardSpec,
    pub baseline: Sample,
    pub halves: [Sample; 2],
}

fn half_json(baseline: Sample, half: Sample) -> Value {
    json!({"stat": half.stat, "n": half.n, "change": change(baseline, half)})
}

/// F.2's decision; the first rule that matches wins.
pub fn decide(
    metric: &MetricSpec,
    baseline: Sample,
    halves: [Sample; 2],
    guards: &[GuardResult<'_>],
    votes_down: i64,
    extended: bool,
) -> Outcome {
    let mut guard_rows = Vec::new();
    let mut failed_guards = Vec::new();
    for guard in guards {
        let failed = guard
            .halves
            .iter()
            .any(|half| guard_fails(guard.spec, guard.baseline, *half));
        if failed {
            failed_guards.push(guard.spec.query.clone());
        }
        guard_rows.push(json!({
            "query": guard.spec.query,
            "baseline": {"stat": guard.baseline.stat, "n": guard.baseline.n},
            "h1": half_json(guard.baseline, guard.halves[0]),
            "h2": half_json(guard.baseline, guard.halves[1]),
            "failed": failed,
        }));
    }
    guard_rows.push(
        json!({"query": "builtin:thumbs_down", "baseline": null, "h1": null,
        "h2": null, "failed": votes_down > 0}),
    );
    if votes_down > 0 {
        failed_guards.push("builtin:thumbs_down".into());
    }
    let result = json!({
        "baseline": {"stat": baseline.stat, "n": baseline.n},
        "h1": half_json(baseline, halves[0]),
        "h2": half_json(baseline, halves[1]),
        "guards": guard_rows,
    });
    let (decision, note) = if !failed_guards.is_empty() {
        (
            Decision::Revert,
            format!("guard failed: {}", failed_guards.join(", ")),
        )
    } else if halves.iter().any(|half| half.n < MIN_OBSERVATIONS) {
        if extended {
            (Decision::Revert, "insufficient data".to_owned())
        } else {
            (
                Decision::Extend,
                "fewer than 10 observations in a half".to_owned(),
            )
        }
    } else if halves.iter().all(|half| meets_bar(metric, baseline, *half)) {
        (Decision::Keep, "met the bar in both halves".to_owned())
    } else {
        (Decision::Revert, "missed the bar".to_owned())
    };
    Outcome {
        decision,
        note,
        result,
    }
}

/// True when `sql` holds one statement: no `;` outside quotes and comments
/// is followed by more SQL.
pub fn single_statement(sql: &str) -> bool {
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut ended = false;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' | '"' | '`' | '[' => {
                if ended {
                    return false;
                }
                let close = if c == '[' { ']' } else { c };
                i += 1;
                while i < chars.len() && chars[i] != close {
                    i += 1;
                }
            }
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 1;
            }
            ';' => ended = true,
            c if c.is_whitespace() => {}
            _ if ended => return false,
            _ => {}
        }
        i += 1;
    }
    true
}

fn uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let escaped = text
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    format!("file:{escaped}?mode=ro")
}

/// Runs a committed metric query read-only against `[from, until)`.
/// Returns `(at_ms, value)` rows in range, NULL values dropped.
pub fn run_query(
    paths: &Paths,
    sql: &str,
    from: i64,
    until: i64,
) -> Result<Vec<(i64, f64)>, String> {
    run_query_with_timeout(paths, sql, from, until, QUERY_TIMEOUT)
}

pub fn run_query_with_timeout(
    paths: &Paths,
    sql: &str,
    from: i64,
    until: i64,
    timeout: Duration,
) -> Result<Vec<(i64, f64)>, String> {
    let conn = Connection::open_with_flags(
        ":memory:",
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())?;
    for (schema, path) in [
        ("q", &paths.queue_db),
        ("act", &paths.activity_db),
        ("usage", &paths.usage_db),
        ("util", &paths.utilization_db),
        ("bugs", &paths.bugs_db),
    ] {
        let target = if path.exists() {
            uri(path)
        } else {
            ":memory:".to_owned()
        };
        conn.execute(
            "ATTACH DATABASE ?1 AS ?2",
            rusqlite::params![target, schema],
        )
        .map_err(|e| format!("attach {schema}: {e}"))?;
    }
    conn.pragma_update(None, "query_only", true)
        .map_err(|e| e.to_string())?;
    if !single_statement(sql) {
        return Err("query must be a single read-only SELECT".into());
    }
    let mut stmt = conn
        .prepare(sql.trim().trim_end_matches(';'))
        .map_err(|e| format!("query does not prepare: {e}"))?;
    let lead = sql.trim_start().to_ascii_lowercase();
    if !stmt.readonly() || !(lead.starts_with("select") || lead.starts_with("with")) {
        return Err("query must be a single read-only SELECT".into());
    }
    if stmt.column_count() < 2 {
        return Err("query must return (at_ms, value)".into());
    }
    for (name, value) in [(":from_ms", from), (":until_ms", until)] {
        if let Some(index) = stmt.parameter_index(name).map_err(|e| e.to_string())? {
            stmt.raw_bind_parameter(index, value)
                .map_err(|e| e.to_string())?;
        }
    }
    let (done, wait) = mpsc::channel::<()>();
    let interrupt = conn.get_interrupt_handle();
    let timer = std::thread::spawn(move || {
        if wait.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
            interrupt.interrupt();
        }
    });
    let mut out = Vec::new();
    let result = (|| -> Result<(), String> {
        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next().map_err(|e| {
            if matches!(&e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::OperationInterrupted)
            {
                format!("query timed out after {}s", timeout.as_secs())
            } else {
                e.to_string()
            }
        })? {
            let at = match row.get_ref(0).map_err(|e| e.to_string())? {
                ValueRef::Integer(at) => at,
                ValueRef::Real(at) => at as i64,
                ValueRef::Null => continue,
                _ => return Err("at_ms must be an integer".into()),
            };
            let value = match row.get_ref(1).map_err(|e| e.to_string())? {
                ValueRef::Integer(v) => v as f64,
                ValueRef::Real(v) => v,
                ValueRef::Null => continue,
                _ => return Err("value must be a number".into()),
            };
            if (from..until).contains(&at) {
                out.push((at, value));
            }
        }
        Ok(())
    })();
    drop(done);
    let _ = timer.join();
    result.map(|()| out)
}
