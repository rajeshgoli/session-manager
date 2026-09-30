//! The ticket clock (sm#1710 appendix D7): for each in-progress or
//! needs-you ticket, who has the ball now and what its last few hours
//! looked like. Pure; the board route gathers the facts.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};

use crate::analytics_time::ThreadInterval;
use crate::owner_push::format_ts;
use crate::queue::queue_short_duration;

/// The `clock_hours` values the board accepts.
pub const CLOCK_HOURS: [i64; 3] = [3, 6, 24];
pub const DEFAULT_CLOCK_HOURS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    Waiting,
    Running,
    Ended,
}

/// One of the ticket's queue jobs.
#[derive(Debug, Clone, PartialEq)]
pub struct ClockJob {
    pub job_type: String,
    pub phase: JobPhase,
    pub queued_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
    /// 0 is no limit.
    pub timeout_seconds: i64,
    pub holding_reason: Option<String>,
    /// A waiting job's place in the queue's order, from 1.
    pub position: Option<usize>,
    /// When a running job went quiet (D6.6); `None` while it is not.
    pub quiet_since: Option<OffsetDateTime>,
    pub cpu_seconds: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Working,
    Idle,
}

/// What the ball rules read about one ticket.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClockFacts {
    /// The needs-you record: when it was created, and its text.
    pub needs_you: Option<(OffsetDateTime, String)>,
    /// The live holder's activity, and when it last changed.
    pub holder: Option<(Activity, OffsetDateTime)>,
    /// Without a live holder: when the claim was released or its holder
    /// stopped, when known.
    pub no_agent_since: Option<OffsetDateTime>,
    pub jobs: Vec<ClockJob>,
    /// The Codex review the holder waits on: PR number, requested at.
    pub review: Option<(i64, OffsetDateTime)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ball {
    pub kind: &'static str,
    pub since: Option<OffsetDateTime>,
    pub text: String,
}

/// "{m}m" under an hour, else "{h}h {m}m"; whole minutes, rounded down.
pub fn age(duration: Duration) -> String {
    let minutes = duration.whole_minutes().max(0);
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h {}m", minutes / 60, minutes % 60)
    }
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn type_label(job_type: &str) -> String {
    let mut chars = job_type.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// What a waiting job waits for, as the several-jobs text ends.
fn wait_reason(holding_reason: Option<&str>) -> &'static str {
    match holding_reason {
        None | Some("concurrency_cap") => "for a slot",
        Some("memory_pressure") => "for memory",
        Some("perf_cooldown") => "for the perf cooldown",
        Some("awaiting_tests") => "for tests to finish",
        Some(_) => "",
    }
}

/// The ball at `now`: the first rule of D7 that matches.
pub fn ball(facts: &ClockFacts, now: OffsetDateTime, stall: Duration) -> Ball {
    let make = |kind, since: OffsetDateTime, text: String| Ball {
        kind,
        since: Some(since),
        text,
    };
    if let Some((since, text)) = &facts.needs_you {
        return make(
            "you",
            *since,
            format!("Waiting on you {}: {text}", age(now - *since)),
        );
    }
    if let Some((Activity::Working, since)) = facts.holder {
        return make(
            "working",
            since,
            format!("Agent working {}", age(now - since)),
        );
    }
    let running: Vec<&ClockJob> = facts
        .jobs
        .iter()
        .filter(|job| job.phase == JobPhase::Running)
        .collect();
    let waiting: Vec<&ClockJob> = facts
        .jobs
        .iter()
        .filter(|job| job.phase == JobPhase::Waiting)
        .collect();
    let oldest_wait = waiting.iter().map(|job| job.queued_at).min();
    let started = running
        .iter()
        .map(|job| job.started_at.unwrap_or(job.queued_at))
        .min();
    if let Some(started) = started {
        let quiet = running
            .iter()
            .map(|job| job.quiet_since)
            .collect::<Option<Vec<_>>>()
            .and_then(|times| times.into_iter().max());
        if let (Some(quiet), None) = (quiet, oldest_wait) {
            let mut text = format!(
                "Job running {}, quiet {}",
                age(now - started),
                age(now - quiet)
            );
            let cpu: Option<f64> = running.iter().map(|job| job.cpu_seconds).sum();
            if let Some(cpu) = cpu {
                text.push_str(&format!(": {cpu:.1} s CPU"));
            }
            return make("job_quiet", quiet, text);
        }
        let text = match (running.as_slice(), oldest_wait) {
            ([job], None) => {
                let mut text = format!(
                    "{} running {}",
                    type_label(&job.job_type),
                    age(now - started)
                );
                if job.timeout_seconds > 0 {
                    text.push_str(&format!(
                        " of {}",
                        queue_short_duration(job.timeout_seconds)
                    ));
                }
                text
            }
            (_, oldest) => {
                let mut text = format!("{} running", running.len());
                if let Some(oldest) = oldest {
                    text.push_str(&format!(
                        " · {} waiting {}",
                        waiting.len(),
                        age(now - oldest)
                    ));
                }
                text
            }
        };
        return make("job_running", started, text);
    }
    if let Some(oldest) = oldest_wait {
        let waited = age(now - oldest);
        let text = match waiting.as_slice() {
            [job] => match job.position {
                Some(position) => format!("Job waiting {waited} · {} in line", ordinal(position)),
                None => format!("Job waiting {waited}"),
            },
            _ => {
                let reasons: BTreeSet<&str> = waiting
                    .iter()
                    .map(|job| wait_reason(job.holding_reason.as_deref()))
                    .collect();
                let mut text = format!("{} jobs waiting {waited}", waiting.len());
                if let Some(reason) = reasons
                    .first()
                    .filter(|reason| reasons.len() == 1 && !reason.is_empty())
                {
                    text.push(' ');
                    text.push_str(reason);
                }
                text
            }
        };
        return make("queue", oldest, text);
    }
    if let Some((pr, since)) = facts.review {
        return make(
            "review",
            since,
            format!("Codex review on PR #{pr}, {}", age(now - since)),
        );
    }
    if let Some((Activity::Idle, since)) = facts.holder {
        let idle = now - since;
        return if idle < stall {
            make("idle", since, format!("Idle {}", age(idle)))
        } else {
            make(
                "stalled",
                since,
                format!("Stalled {}: agent idle, nothing running", age(idle)),
            )
        };
    }
    Ball {
        kind: "no_agent",
        since: facts.no_agent_since,
        text: "No agent".to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub kind: &'static str,
    pub from: OffsetDateTime,
    pub to: OffsetDateTime,
}

/// Which part wins an instant several agents fill: the ball's order.
fn part_rank(part: &str) -> u8 {
    match part {
        "you" => 5,
        "model" | "tools" | "agents" => 4,
        "queue" => 3,
        "review" => 2,
        _ => 1,
    }
}

pub fn ms(at: OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos().div_euclid(1_000_000)).unwrap_or(i64::MAX)
}

fn from_ms(at: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(at) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// The strip over `[from, to)`. `intervals` are the ticket's
/// `analytics_time::thread_intervals` from `from - stall`, so an idle run
/// that began before the window turns stalled on time.
pub fn segments(
    intervals: &[ThreadInterval],
    jobs: &[ClockJob],
    (from, to): (OffsetDateTime, OffsetDateTime),
    stall: Duration,
) -> Vec<Segment> {
    let (from, to) = (ms(from), ms(to));
    let stall = stall.whole_milliseconds() as i64;
    let runs: Vec<(i64, i64, Option<i64>)> = jobs
        .iter()
        .filter_map(|job| {
            let started = ms(job.started_at?);
            let ended = job.finished_at.map_or(i64::MAX, ms);
            Some((started, ended, job.quiet_since.map(ms)))
        })
        .collect();
    let mut points: Vec<i64> = intervals
        .iter()
        .flat_map(|interval| [interval.from, interval.to])
        .chain(runs.iter().flat_map(|&(started, ended, quiet)| {
            [Some(started), Some(ended), quiet].into_iter().flatten()
        }))
        .filter(|&point| point <= to)
        .chain([to])
        .collect();
    points.sort_unstable();
    points.dedup();

    let mut merged: Vec<(&'static str, i64, i64)> = Vec::new();
    for pair in points.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let Some(part) = intervals
            .iter()
            .filter(|interval| interval.from <= start && start < interval.to)
            .map(|interval| interval.part)
            .max_by_key(|part| part_rank(part))
        else {
            continue;
        };
        let kind = match part {
            "model" | "tools" | "agents" => "working",
            "queue" => {
                let running: Vec<Option<i64>> = runs
                    .iter()
                    .filter(|&&(started, ended, _)| started <= start && start < ended)
                    .map(|&(_, _, quiet)| quiet)
                    .collect();
                if running.is_empty() {
                    "queue"
                } else if running
                    .iter()
                    .all(|quiet| quiet.is_some_and(|at| at <= start))
                {
                    "quiet"
                } else {
                    "job_running"
                }
            }
            "review" => "review",
            "you" => "you",
            _ => "idle",
        };
        match merged.last_mut() {
            Some(last) if last.0 == kind && last.2 == start => last.2 = end,
            _ => merged.push((kind, start, end)),
        }
    }

    let mut out: Vec<Segment> = Vec::new();
    let mut push = |kind: &'static str, start: i64, end: i64| {
        let (start, end) = (start.max(from), end.min(to));
        if start < end {
            out.push(Segment {
                kind,
                from: from_ms(start),
                to: from_ms(end),
            });
        }
    };
    for (kind, start, end) in merged {
        if kind == "idle" && end - start > stall {
            push("idle", start, start + stall);
            push("stalled", start + stall, end);
        } else {
            push(kind, start, end);
        }
    }
    out
}

pub fn segments_json(segments: &[Segment]) -> Value {
    json!(segments
        .iter()
        .map(|segment| json!({
            "kind": segment.kind,
            "from": format_ts(segment.from),
            "to": format_ts(segment.to),
        }))
        .collect::<Vec<_>>())
}

/// A ticket's `clock` object.
pub fn clock_json(ball: &Ball, segments: Value) -> Value {
    json!({
        "ball": ball.kind,
        "since": ball.since.map(format_ts),
        "text": ball.text,
        "segments": segments,
    })
}

#[cfg(test)]
mod tests;
