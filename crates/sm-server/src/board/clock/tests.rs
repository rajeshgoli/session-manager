use time::macros::datetime;

use super::*;

/// The D7 fixtures' "now": live data, 30 September 00:25 UTC.
const NOW: OffsetDateTime = datetime!(2026-09-30 00:25:35 UTC);
const STALL: Duration = Duration::minutes(15);

fn job(job_type: &str, phase: JobPhase, queued: OffsetDateTime) -> ClockJob {
    ClockJob {
        job_type: job_type.to_owned(),
        phase,
        queued_at: queued,
        started_at: (phase != JobPhase::Waiting).then_some(queued),
        finished_at: None,
        timeout_seconds: 0,
        holding_reason: None,
        position: None,
        quiet_since: None,
        cpu_seconds: None,
    }
}

fn running(job_type: &str, started: OffsetDateTime, limit_hours: i64) -> ClockJob {
    ClockJob {
        timeout_seconds: limit_hours * 3600,
        ..job(job_type, JobPhase::Running, started)
    }
}

fn waiting(queued: OffsetDateTime) -> ClockJob {
    ClockJob {
        holding_reason: Some("concurrency_cap".to_owned()),
        ..job("background", JobPhase::Waiting, queued)
    }
}

fn idle_since(at: OffsetDateTime) -> Option<(Activity, OffsetDateTime)> {
    Some((Activity::Idle, at))
}

fn check(facts: ClockFacts, kind: &str, text: &str) -> Ball {
    let ball = ball(&facts, NOW, STALL);
    assert_eq!((ball.kind, ball.text.as_str()), (kind, text));
    ball
}

#[test]
fn d7_1848_one_job_running_under_its_limit() {
    let ball = check(
        ClockFacts {
            holder: idle_since(datetime!(2026-09-29 23:50:40 UTC)),
            jobs: vec![running("tests", datetime!(2026-09-29 23:50:35 UTC), 8)],
            ..Default::default()
        },
        "job_running",
        "Tests running 35m of 8h",
    );
    assert_eq!(ball.since, Some(datetime!(2026-09-29 23:50:35 UTC)));
}

#[test]
fn d7_constructed_every_running_job_quiet() {
    let ball = check(
        ClockFacts {
            holder: idle_since(datetime!(2026-09-29 23:15:05 UTC)),
            jobs: vec![ClockJob {
                quiet_since: Some(datetime!(2026-09-30 00:01:00 UTC)),
                cpu_seconds: Some(0.2),
                ..running("tests", datetime!(2026-09-29 23:15:00 UTC), 8)
            }],
            ..Default::default()
        },
        "job_quiet",
        "Job running 1h 10m, quiet 24m: 0.2 s CPU",
    );
    assert_eq!(ball.since, Some(datetime!(2026-09-30 00:01:00 UTC)));
}

#[test]
fn a_quiet_job_with_one_waiting_or_one_loud_is_still_running() {
    let quiet = ClockJob {
        quiet_since: Some(datetime!(2026-09-30 00:01:00 UTC)),
        ..running("tests", datetime!(2026-09-29 23:15:00 UTC), 0)
    };
    check(
        ClockFacts {
            jobs: vec![quiet.clone(), waiting(datetime!(2026-09-30 00:20:00 UTC))],
            ..Default::default()
        },
        "job_running",
        "1 running · 1 waiting 5m",
    );
    check(
        ClockFacts {
            jobs: vec![
                quiet,
                running("tests", datetime!(2026-09-30 00:10:00 UTC), 0),
            ],
            ..Default::default()
        },
        "job_running",
        "2 running",
    );
}

#[test]
fn d7_1854_several_running_and_waiting() {
    let ball = check(
        ClockFacts {
            holder: idle_since(datetime!(2026-09-29 22:03:00 UTC)),
            jobs: vec![
                running("tests", datetime!(2026-09-29 22:49:00 UTC), 3),
                running("tests", datetime!(2026-09-29 23:16:00 UTC), 3),
                waiting(datetime!(2026-09-29 22:02:00 UTC)),
                waiting(datetime!(2026-09-29 22:02:00 UTC)),
                waiting(datetime!(2026-09-29 22:02:00 UTC)),
            ],
            ..Default::default()
        },
        "job_running",
        "2 running · 3 waiting 2h 23m",
    );
    assert_eq!(ball.since, Some(datetime!(2026-09-29 22:49:00 UTC)));
}

#[test]
fn d7_1856_one_job_running_with_a_limit() {
    check(
        ClockFacts {
            holder: idle_since(datetime!(2026-09-29 22:37:00 UTC)),
            jobs: vec![running("tests", datetime!(2026-09-29 22:36:45 UTC), 3)],
            ..Default::default()
        },
        "job_running",
        "Tests running 1h 48m of 3h",
    );
}

#[test]
fn d7_1853_several_waiting_for_a_slot() {
    let ball = check(
        ClockFacts {
            holder: idle_since(datetime!(2026-09-29 22:30:00 UTC)),
            jobs: vec![
                waiting(datetime!(2026-09-29 22:29:50 UTC)),
                waiting(datetime!(2026-09-29 22:29:55 UTC)),
            ],
            ..Default::default()
        },
        "queue",
        "2 jobs waiting 1h 55m for a slot",
    );
    assert_eq!(ball.since, Some(datetime!(2026-09-29 22:29:50 UTC)));
}

#[test]
fn d7_1844_one_job_waiting_in_line() {
    check(
        ClockFacts {
            holder: idle_since(datetime!(2026-09-29 22:54:00 UTC)),
            jobs: vec![ClockJob {
                position: Some(6),
                ..waiting(datetime!(2026-09-29 22:54:00 UTC))
            }],
            ..Default::default()
        },
        "queue",
        "Job waiting 1h 31m · 6th in line",
    );
}

#[test]
fn waiting_jobs_with_different_reasons_name_none() {
    check(
        ClockFacts {
            jobs: vec![
                waiting(datetime!(2026-09-30 00:05:00 UTC)),
                ClockJob {
                    holding_reason: Some("memory_pressure".to_owned()),
                    ..waiting(datetime!(2026-09-30 00:10:00 UTC))
                },
            ],
            ..Default::default()
        },
        "queue",
        "2 jobs waiting 20m",
    );
}

#[test]
fn d7_1684_codex_review() {
    check(
        ClockFacts {
            holder: idle_since(datetime!(2026-09-30 00:22:50 UTC)),
            review: Some((1711, datetime!(2026-09-30 00:22:49 UTC))),
            ..Default::default()
        },
        "review",
        "Codex review on PR #1711, 2m",
    );
}

#[test]
fn d7_1680_agent_working_wins_over_jobs() {
    check(
        ClockFacts {
            holder: Some((Activity::Working, datetime!(2026-09-30 00:22:35 UTC))),
            jobs: vec![running("tests", datetime!(2026-09-29 23:00:00 UTC), 1)],
            ..Default::default()
        },
        "working",
        "Agent working 3m",
    );
}

#[test]
fn d7_constructed_stalled_after_fifteen_minutes() {
    let ball = check(
        ClockFacts {
            holder: idle_since(datetime!(2026-09-29 23:51:00 UTC)),
            ..Default::default()
        },
        "stalled",
        "Stalled 34m: agent idle, nothing running",
    );
    assert_eq!(ball.since, Some(datetime!(2026-09-29 23:51:00 UTC)));
    check(
        ClockFacts {
            holder: idle_since(NOW - Duration::seconds(14 * 60 + 59)),
            ..Default::default()
        },
        "idle",
        "Idle 14m",
    );
    check(
        ClockFacts {
            holder: idle_since(NOW - STALL),
            ..Default::default()
        },
        "stalled",
        "Stalled 15m: agent idle, nothing running",
    );
}

#[test]
fn needs_you_wins_and_no_agent_is_last() {
    check(
        ClockFacts {
            needs_you: Some((
                datetime!(2026-09-29 23:00:00 UTC),
                "PR #1713 waits for your review".to_owned(),
            )),
            holder: Some((Activity::Working, NOW)),
            ..Default::default()
        },
        "you",
        "Waiting on you 1h 25m: PR #1713 waits for your review",
    );
    let ball = check(ClockFacts::default(), "no_agent", "No agent");
    assert_eq!(ball.since, None);
}

#[test]
fn ages_and_ordinals() {
    assert_eq!(age(Duration::seconds(59)), "0m");
    assert_eq!(age(Duration::minutes(60)), "1h 0m");
    assert_eq!(age(Duration::seconds(-5)), "0m");
    let ordinals: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 101]
        .into_iter()
        .map(ordinal)
        .collect();
    assert_eq!(
        ordinals,
        ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st"]
    );
}

fn at(minute: i64) -> OffsetDateTime {
    datetime!(2026-09-30 00:00:00 UTC) + Duration::minutes(minute)
}

fn interval(part: &'static str, from: i64, to: i64) -> ThreadInterval {
    ThreadInterval {
        part,
        from: ms(at(from)),
        to: ms(at(to)),
    }
}

fn strip(segments: &[Segment]) -> Vec<(&'static str, i64, i64)> {
    segments
        .iter()
        .map(|segment| {
            let minute = |t: OffsetDateTime| (t - at(0)).whole_minutes();
            (segment.kind, minute(segment.from), minute(segment.to))
        })
        .collect()
}

#[test]
fn segments_map_parts_and_merge() {
    let intervals = [
        interval("model", 0, 10),
        interval("tools", 10, 20),
        // The queue wait: the job starts at 30, goes quiet at 50, ends at 60.
        interval("queue", 20, 60),
        interval("review", 60, 70),
        interval("you", 70, 80),
    ];
    let jobs = [ClockJob {
        started_at: Some(at(30)),
        finished_at: Some(at(60)),
        quiet_since: Some(at(50)),
        ..job("tests", JobPhase::Ended, at(20))
    }];
    let segments = segments(&intervals, &jobs, (at(0), at(90)), STALL);
    assert_eq!(
        strip(&segments),
        [
            ("working", 0, 20),
            ("queue", 20, 30),
            ("job_running", 30, 50),
            ("quiet", 50, 60),
            ("review", 60, 70),
            ("you", 70, 80),
        ]
    );
}

#[test]
fn segments_turn_idle_past_the_stall_threshold_stalled() {
    // The idle run starts before the window (the lead-in), so it is stalled
    // from the window's start plus what remains of the threshold.
    let intervals = [
        interval("idle", -10, 30),
        interval("model", 30, 40),
        interval("idle", 40, 50),
    ];
    let segments = segments(&intervals, &[], (at(0), at(60)), STALL);
    assert_eq!(
        strip(&segments),
        [
            ("idle", 0, 5),
            ("stalled", 5, 30),
            ("working", 30, 40),
            ("idle", 40, 50),
        ]
    );
}

#[test]
fn segments_take_the_ball_order_across_agents() {
    // One agent idle, another working, a third waiting on the owner.
    let intervals = [
        interval("idle", 0, 30),
        interval("model", 5, 20),
        interval("you", 10, 15),
    ];
    let segments = segments(&intervals, &[], (at(0), at(30)), STALL);
    assert_eq!(
        strip(&segments),
        [
            ("idle", 0, 5),
            ("working", 5, 10),
            ("you", 10, 15),
            ("working", 15, 20),
            ("idle", 20, 30),
        ]
    );
}

#[test]
fn a_pinned_note_replaces_idle_and_stalled() {
    let pinned = datetime!(2026-09-29 13:00:00 UTC);
    let held = idle_since(datetime!(2026-09-28 09:00:00 UTC));
    check(
        ClockFacts {
            holder: held,
            note: Some((pinned, "Waiting for the midnight window".into())),
            ..Default::default()
        },
        "idle",
        "📌 Waiting for the midnight window",
    );
    // A question still wins over the note.
    let question = ball(
        &ClockFacts {
            needs_you: Some((pinned, "Check Chrome".into())),
            holder: held,
            note: Some((pinned, "Waiting".into())),
            ..Default::default()
        },
        NOW,
        STALL,
    );
    assert_eq!(question.kind, "you");
}
