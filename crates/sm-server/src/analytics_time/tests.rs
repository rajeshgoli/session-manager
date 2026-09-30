use super::*;
use std::path::PathBuf;
use time::format_description::well_known::Rfc3339;

const REPO: &str = "rajeshgoli/session-manager";
const PROJECT: &str = "/work/session-manager/.git";
const SECOND: i64 = 1_000;
const MINUTE: i64 = 60 * SECOND;

/// `HH:MM:SS` on 2026-09-29, UTC, in ms.
fn t(clock: &str) -> i64 {
    millis(&format!("2026-09-29T{clock}Z")).unwrap()
}

fn text(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * NANOS_PER_MS)
        .unwrap()
        .format(&Rfc3339)
        .unwrap()
}

fn turn(started: i64, ended: i64, prompt_at: Option<i64>) -> Turn {
    Turn {
        started,
        ended,
        prompt_at,
    }
}

fn span(started: i64, ended: i64, kind: &str) -> Span {
    Span {
        started,
        ended,
        kind: kind.to_owned(),
    }
}

fn wait(kind: WaitKind, opened: i64, closed: Option<i64>) -> Wait {
    Wait {
        kind,
        opened,
        closed,
    }
}

fn bucket_key(bucket: Bucket<'_>) -> String {
    match bucket {
        Bucket::Part(key) => key.to_owned(),
        Bucket::Tool(kind) => format!("tool:{kind}"),
        Bucket::Parked => "parked".to_owned(),
    }
}

/// Seconds per (thread, bucket) for one seat; a prompt is the owner's when
/// `from_you` says so.
fn classify_seat(
    activity: &SeatActivity,
    range: (i64, i64),
    attributor: &Attributor,
    from_you: &dyn Fn(&Turn) -> bool,
) -> BTreeMap<(Option<i64>, String), i64> {
    let mut out: BTreeMap<(Option<i64>, String), i64> = BTreeMap::new();
    classify(
        "eng",
        activity,
        range,
        attributor,
        REPO,
        from_you,
        |attribution, bucket, from, to| {
            *out.entry((attribution.thread, bucket_key(bucket)))
                .or_default() += to - from;
        },
    );
    out.into_iter()
        .map(|(key, ms)| (key, ms / SECOND))
        .collect()
}

fn everything() -> (i64, i64) {
    (0, i64::MAX)
}

fn seconds(entries: &[(Option<i64>, &str, i64)]) -> BTreeMap<(Option<i64>, String), i64> {
    entries
        .iter()
        .map(|&(thread, bucket, seconds)| ((thread, bucket.to_owned()), seconds))
        .collect()
}

fn never(_: &Turn) -> bool {
    false
}

/// An agent holding ticket #1 all day, so a gap with nothing pending is
/// idle rather than parked.
fn claimed() -> Attributor {
    let mut attributor = Attributor::default();
    attributor.add_claim("eng", "c", REPO, 1, false, "2026-09-29T00:00:00Z", None);
    attributor
}

#[test]
fn innermost_span_wins_and_model_is_the_remainder() {
    // J.16: an `exec` wrapper around two commands.
    let base = t("10:00:00");
    let activity = SeatActivity {
        first_start: base,
        end: base + 100 * SECOND,
        turns: vec![turn(base, base + 100 * SECOND, None)],
        spans: vec![
            span(base + 10 * SECOND, base + 60 * SECOND, "shell"),
            span(base + 20 * SECOND, base + 30 * SECOND, "read"),
            span(base + 40 * SECOND, base + 50 * SECOND, "git"),
            // Same start as `git`, ends earlier: loses the tie to `git`.
            span(base + 40 * SECOND, base + 45 * SECOND, "sm"),
            // Outside every turn: ignored.
            span(base + 120 * SECOND, base + 130 * SECOND, "edit"),
        ],
        waits: Vec::new(),
    };
    assert_eq!(
        classify_seat(&activity, everything(), &Attributor::default(), &never),
        seconds(&[
            (None, "model", 50),
            (None, "tool:shell", 30),
            (None, "tool:read", 10),
            (None, "tool:git", 10),
        ])
    );
}

#[test]
fn the_wait_that_ends_last_wins_and_ties_break_you_review_queue_agents() {
    // J.17: tests for 40 minutes and a review for 8, both open in one gap.
    let base = t("10:00:00");
    let gap_start = base + MINUTE;
    let next = gap_start + 60 * MINUTE;
    let activity = SeatActivity {
        first_start: base,
        end: next + MINUTE,
        turns: vec![turn(base, gap_start, None), turn(next, next + MINUTE, None)],
        spans: Vec::new(),
        waits: vec![
            wait(WaitKind::Queue, gap_start, Some(gap_start + 40 * MINUTE)),
            wait(
                WaitKind::Review,
                gap_start + 2 * MINUTE,
                Some(gap_start + 10 * MINUTE),
            ),
        ],
    };
    assert_eq!(
        classify_seat(&activity, everything(), &claimed(), &never),
        seconds(&[
            (Some(1), "model", 120),
            (Some(1), "queue", 40 * 60),
            (Some(1), "idle", 20 * 60),
        ])
    );

    // Equal ends: you › review › queue › agents.
    let end = gap_start + 10 * MINUTE;
    let mut tied = SeatActivity {
        first_start: base,
        end: next + MINUTE,
        turns: vec![turn(base, gap_start, None), turn(next, next + MINUTE, None)],
        spans: Vec::new(),
        waits: vec![
            wait(WaitKind::Agents, gap_start, Some(end)),
            wait(WaitKind::Queue, gap_start, Some(end)),
        ],
    };
    let queue_first = classify_seat(&tied, everything(), &Attributor::default(), &never);
    assert_eq!(queue_first[&(None, "queue".to_owned())], 600);
    tied.waits
        .push(wait(WaitKind::Review, gap_start, Some(end)));
    let review_first = classify_seat(&tied, everything(), &Attributor::default(), &never);
    assert_eq!(review_first[&(None, "review".to_owned())], 600);
    tied.waits.push(wait(WaitKind::You, gap_start, Some(end)));
    let you_first = classify_seat(&tied, everything(), &Attributor::default(), &never);
    assert_eq!(you_first[&(None, "you".to_owned())], 600);
    // A still-open wait ends last of all.
    tied.waits
        .push(wait(WaitKind::Agents, gap_start + MINUTE, None));
    let open = classify_seat(&tied, everything(), &Attributor::default(), &never);
    assert_eq!(open[&(None, "you".to_owned())], 60);
    assert_eq!(open[&(None, "agents".to_owned())], 59 * 60);
}

#[test]
fn a_queue_job_counts_as_waiting_only_between_turns() {
    // J.18: the job runs across the end of a turn and into the gap.
    let base = t("10:00:00");
    let activity = SeatActivity {
        first_start: base,
        end: base + 300 * SECOND,
        turns: vec![
            turn(base, base + 100 * SECOND, None),
            turn(base + 250 * SECOND, base + 300 * SECOND, None),
        ],
        spans: Vec::new(),
        waits: vec![wait(
            WaitKind::Queue,
            base + 50 * SECOND,
            Some(base + 200 * SECOND),
        )],
    };
    assert_eq!(
        classify_seat(&activity, everything(), &claimed(), &never),
        seconds(&[
            (Some(1), "model", 150),
            (Some(1), "queue", 100),
            (Some(1), "idle", 50),
        ])
    );
}

#[test]
fn a_prompt_is_yours_unless_sm_delivered_it_within_twenty_seconds() {
    // J.19.
    let base = t("10:00:00");
    let mut deliveries = Deliveries::default();
    deliveries
        .sm
        .insert("eng".into(), vec![base - 15 * SECOND, base + 1000 * SECOND]);
    deliveries
        .owner_replies
        .insert("eng".into(), vec![base + 1005 * SECOND]);
    // An sm delivery 15 s before: typed by sm.
    assert!(!deliveries.typed_by_you("eng", Some(base)));
    // An sm delivery 25 s away: yours.
    assert!(deliveries.typed_by_you("eng", Some(base + 10 * SECOND)));
    // An inbox reply delivered through sm: yours.
    assert!(deliveries.typed_by_you("eng", Some(base + 1000 * SECOND)));
    // No deliveries at all to this seat: yours. No prompt line: not yours.
    assert!(deliveries.typed_by_you("other", Some(base)));
    assert!(!deliveries.typed_by_you("eng", None));
}

/// Appendix E's worked example: sm-1647-inbox-engineer on 2026-09-29.
fn inbox_engineer() -> (SeatActivity, Attributor) {
    let mut attributor = Attributor::default();
    attributor.set_seat("eng", Some("sm-1647-inbox-engineer"), None);
    attributor.add_claim(
        "eng",
        "c1",
        REPO,
        1646,
        true,
        "2026-09-29T05:31:03Z",
        Some("2026-09-29T06:27:55Z"),
    );
    attributor.add_claim(
        "eng",
        "c2",
        REPO,
        1647,
        false,
        "2026-09-29T05:42:35Z",
        Some("2026-09-29T06:27:55Z"),
    );
    attributor.add_claim(
        "eng",
        "c3",
        REPO,
        1649,
        true,
        "2026-09-29T06:00:00Z",
        Some("2026-09-29T06:27:55Z"),
    );
    attributor.add_claim(
        "eng",
        "c4",
        REPO,
        1648,
        false,
        "2026-09-29T06:20:00Z",
        Some("2026-09-29T06:27:55Z"),
    );
    attributor.add_claim(
        "eng",
        "c5",
        REPO,
        1650,
        true,
        "2026-09-29T06:21:00Z",
        Some("2026-09-29T06:27:55Z"),
    );
    attributor.add_link(REPO, 1649, 1647);
    attributor.add_link(REPO, 1650, 1648);
    let activity = SeatActivity {
        first_start: t("05:30:44"),
        end: t("17:44:30"),
        turns: vec![
            // Reads the review, edits the memo; you typed the prompt.
            turn(t("05:30:44"), t("05:36:24"), Some(t("05:30:40"))),
            // Your doc review arrives at 05:40:58 and opens the next turn;
            // tests run as queue jobs inside it.
            turn(t("05:40:58"), t("06:14:08"), Some(t("05:40:58"))),
            // The review's delivery opens this one.
            turn(t("06:18:13"), t("06:25:12"), Some(t("06:18:13"))),
            // "Ready to retire?"
            turn(t("17:43:44"), t("17:44:30"), Some(t("17:43:44"))),
        ],
        spans: vec![span(t("05:32:00"), t("05:33:00"), "edit")],
        waits: vec![
            // Doc published with review requested; you reviewed at 05:40:58.
            wait(WaitKind::You, t("05:36:02"), Some(t("05:40:58"))),
            // Tests while working: not waiting.
            wait(WaitKind::Queue, t("05:45:00"), Some(t("05:50:00"))),
            // The review lands at 06:11, before the gap begins.
            wait(WaitKind::Review, t("06:05:00"), Some(t("06:11:00"))),
        ],
    };
    (activity, attributor)
}

#[test]
fn idle_you_and_parked_follow_claims_and_who_ended_the_gap() {
    // J.20: the appendix E worked example, to the second.
    let (activity, attributor) = inbox_engineer();
    let sm_typed = [t("06:18:13")];
    let from_you = |turn: &Turn| turn.prompt_at.is_some_and(|at| !sm_typed.contains(&at));
    let expected = [
        // 05:30:44–05:31:03, before the first claim: no ticket (the name's
        // #1647 is not a tracked item here).
        (None, "model", 19),
        // 05:31:03–05:36:24 less one minute of editing.
        (Some(1646), "model", 261),
        (Some(1646), "tool:edit", 60),
        // 05:36:24–05:40:58: your doc review.
        (Some(1646), "you", 274),
        // 05:40:58–05:42:35 still #1646, then #1647 to 06:14:08.
        (Some(1646), "model", 97),
        (Some(1647), "model", 1893),
        // 06:14:08–06:18:13 idle, via PR #1649's link to #1647.
        (Some(1647), "idle", 245),
        // 06:18:13–06:20:00 on #1647, then #1648 (06:21 via PR #1650).
        (Some(1647), "model", 107),
        (Some(1648), "model", 312),
        // 06:25:12–06:27:55: claims open, the next prompt is yours.
        (Some(1648), "you", 163),
        // 06:27:55–17:43:44 parked, then the last turn.
        (None, "parked", 40549),
        (None, "model", 46),
    ]
    .into_iter()
    .fold(BTreeMap::new(), |mut totals, (thread, bucket, seconds)| {
        *totals.entry((thread, bucket.to_owned())).or_default() += seconds;
        totals
    });
    assert_eq!(
        classify_seat(&activity, everything(), &attributor, &from_you),
        expected
    );
}

#[test]
fn an_agent_that_never_claimed_parks_when_nothing_is_pending() {
    // J.20: a reviewer with no claim; its review wait counts, then it sits.
    let activity = SeatActivity {
        first_start: t("10:00:00"),
        end: t("11:00:00"),
        turns: vec![
            turn(t("10:00:00"), t("10:10:00"), None),
            turn(t("10:50:00"), t("11:00:00"), Some(t("10:50:00"))),
        ],
        spans: Vec::new(),
        waits: vec![wait(WaitKind::Queue, t("10:10:00"), Some(t("10:20:00")))],
    };
    let from_you = |_: &Turn| true;
    assert_eq!(
        classify_seat(&activity, everything(), &Attributor::default(), &from_you),
        seconds(&[
            (None, "model", 20 * 60),
            (None, "queue", 10 * 60),
            (None, "parked", 30 * 60),
        ])
    );
}

#[test]
fn range_clipping_and_a_live_agent_running_to_now() {
    // J.23: the agent's first turn is before the range; it is live, so its
    // timeline runs to now (the range end).
    let (range_start, now) = (t("12:00:00"), t("13:00:00"));
    let activity = SeatActivity {
        first_start: t("11:00:00"),
        end: now,
        turns: vec![
            turn(t("11:50:00"), t("12:10:00"), None),
            turn(t("12:30:00"), t("12:40:00"), None),
        ],
        spans: Vec::new(),
        waits: Vec::new(),
    };
    assert_eq!(
        classify_seat(&activity, (range_start, now), &claimed(), &never),
        seconds(&[(Some(1), "model", 20 * 60), (Some(1), "idle", 40 * 60)])
    );
    // Stopped: the timeline ends at the last turn.
    let stopped = SeatActivity {
        end: t("12:40:00"),
        ..activity
    };
    assert_eq!(
        classify_seat(&stopped, (range_start, now), &claimed(), &never),
        seconds(&[(Some(1), "model", 20 * 60), (Some(1), "idle", 20 * 60)])
    );
    // Wholly before the range: nothing.
    let before = SeatActivity {
        first_start: t("10:00:00"),
        end: t("11:00:00"),
        turns: Vec::new(),
        spans: Vec::new(),
        waits: Vec::new(),
    };
    assert!(classify_seat(&before, (range_start, now), &Attributor::default(), &never).is_empty());
}

#[test]
fn an_agent_switching_claims_mid_gap_splits_the_gap() {
    // J.24.
    let mut attributor = Attributor::default();
    attributor.add_claim("eng", "a", REPO, 10, false, "2026-09-29T10:00:00Z", None);
    attributor.add_claim(
        "eng",
        "b",
        REPO,
        20,
        false,
        "2026-09-29T10:20:00.0005Z",
        None,
    );
    let activity = SeatActivity {
        first_start: t("10:00:00"),
        end: t("10:40:00"),
        turns: vec![
            turn(t("10:00:00"), t("10:10:00"), None),
            turn(t("10:30:00"), t("10:40:00"), None),
        ],
        spans: Vec::new(),
        waits: Vec::new(),
    };
    let out = classify_seat(&activity, everything(), &attributor, &never);
    // The claim taken at 10:20:00.0005 owns time from 10:20:00.001.
    assert_eq!(out[&(Some(10), "idle".to_owned())], 600);
    assert_eq!(out[&(Some(20), "idle".to_owned())], 599);
    assert_eq!(out[&(Some(10), "model".to_owned())], 600);
    assert_eq!(out[&(Some(20), "model".to_owned())], 600);
}

/// Fixture databases with the columns Time reads.
struct Fixture {
    dir: PathBuf,
    activity: Connection,
    usage: Connection,
    queue: Connection,
    runner: Connection,
    live: BTreeMap<String, bool>,
}

fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "sm-analytics-time-{}-{}",
        std::process::id(),
        rand_core::RngCore::next_u64(&mut rand_core::OsRng)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let activity = Connection::open(dir.join("activity.db")).unwrap();
    activity
        .execute_batch(
            "CREATE TABLE activity_turns (seat_id TEXT NOT NULL, source_ref TEXT NOT NULL,
                 provider TEXT NOT NULL, started_at_ms INTEGER NOT NULL,
                 ended_at_ms INTEGER NOT NULL, prompt_at_ms INTEGER,
                 PRIMARY KEY (source_ref, started_at_ms));
             CREATE TABLE activity_spans (seat_id TEXT NOT NULL, source_ref TEXT NOT NULL,
                 started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER NOT NULL,
                 kind TEXT NOT NULL, tool TEXT, item_id TEXT NOT NULL,
                 PRIMARY KEY (source_ref, item_id));",
        )
        .unwrap();
    let usage = Connection::open(dir.join("usage.db")).unwrap();
    usage
        .execute_batch(
            "CREATE TABLE seat_meta (seat_id TEXT NOT NULL, observed_at TEXT NOT NULL,
                 friendly_name TEXT, project_key TEXT NOT NULL, parent_seat_id TEXT);",
        )
        .unwrap();
    let queue = Connection::open(dir.join("message_queue.db")).unwrap();
    queue
        .execute_batch(
            "CREATE TABLE work_claims (id TEXT PRIMARY KEY, repo TEXT NOT NULL,
                 number INTEGER NOT NULL, kind TEXT NOT NULL, session_id TEXT NOT NULL,
                 claimed_at TEXT NOT NULL, ended_at TEXT, reserved_at TEXT);
             CREATE TABLE work_items (repo TEXT NOT NULL, number INTEGER NOT NULL,
                 kind TEXT NOT NULL, title TEXT NOT NULL, state TEXT NOT NULL);
             CREATE TABLE codex_review_request_registrations (id TEXT PRIMARY KEY,
                 requester_session_id TEXT, notify_session_id TEXT NOT NULL,
                 requested_at TIMESTAMP NOT NULL, review_landed_at TIMESTAMP,
                 superseded_at TIMESTAMP, is_active INTEGER DEFAULT 1,
                 last_polled_at TIMESTAMP);
             CREATE TABLE owner_messages (id TEXT PRIMARY KEY, sender_session_id TEXT NOT NULL,
                 created_at TEXT NOT NULL, handled_at TEXT);
             CREATE TABLE owner_message_replies (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                 delivered_to_session_id TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE owner_doc_publishes (id INTEGER PRIMARY KEY, doc_id TEXT NOT NULL,
                 session_id TEXT NOT NULL, review_requested INTEGER NOT NULL DEFAULT 0,
                 published_at TEXT NOT NULL, review_dismissed_at TEXT);
             CREATE TABLE owner_doc_reviews (id TEXT PRIMARY KEY, doc_id TEXT NOT NULL,
                 submitted_at TEXT NOT NULL);
             CREATE TABLE message_queue (id TEXT PRIMARY KEY, target_session_id TEXT NOT NULL,
                 delivered_at TIMESTAMP);",
        )
        .unwrap();
    let runner = Connection::open(dir.join("queue_runner.db")).unwrap();
    runner
        .execute_batch(
            "CREATE TABLE queue_jobs (id TEXT PRIMARY KEY, type TEXT NOT NULL,
                 requester_session_id TEXT, notify_session_id TEXT NOT NULL,
                 queued_at TEXT NOT NULL, finished_at TEXT, completion_notified_at TEXT);",
        )
        .unwrap();
    Fixture {
        dir,
        activity,
        usage,
        queue,
        runner,
        live: BTreeMap::new(),
    }
}

impl Fixture {
    fn seat(&self, seat: &str, name: &str, parent: Option<&str>) {
        self.usage
            .execute(
                "INSERT INTO seat_meta VALUES (?1, '2026-09-29T00:00:00Z', ?2, ?3, ?4)",
                params![seat, name, PROJECT, parent],
            )
            .unwrap();
    }

    fn turn(&self, seat: &str, started: i64, ended: i64, prompt_at: Option<i64>) {
        self.activity
            .execute(
                "INSERT INTO activity_turns VALUES (?1, ?1, 'claude', ?2, ?3, ?4)",
                params![seat, started, ended, prompt_at],
            )
            .unwrap();
    }

    fn span(&self, seat: &str, id: &str, started: i64, ended: i64, kind: &str) {
        self.activity
            .execute(
                "INSERT INTO activity_spans VALUES (?1, ?1, ?2, ?3, ?4, 'Bash', ?5)",
                params![seat, started, ended, kind, id],
            )
            .unwrap();
    }

    fn job(&self, id: &str, kind: &str, seat: &str, queued: i64, finished: Option<i64>) {
        self.runner
            .execute(
                "INSERT INTO queue_jobs VALUES (?1, ?2, ?3, ?3, ?4, ?5, NULL)",
                params![id, kind, seat, text(queued), finished.map(text)],
            )
            .unwrap();
    }

    fn report(&self, range: TimeRange, now: i64) -> TimeReport {
        let activity_db = self.dir.join("activity.db");
        let usage_db = self.dir.join("usage.db");
        let queue_db = self.dir.join("message_queue.db");
        let queue_runner_db = self.dir.join("queue_runner.db");
        let sources = TimeSources {
            activity_db: &activity_db,
            usage_db: &usage_db,
            queue_db: &queue_db,
            queue_runner_db: &queue_runner_db,
            live_sessions: &self.live,
            repo_of: &|_| REPO.to_owned(),
        };
        let now =
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * NANOS_PER_MS).unwrap();
        time_report(&sources, range, now).unwrap()
    }
}

fn find<'a>(node: &'a TimeNode, id: &str) -> &'a TimeNode {
    if node.id == id {
        return node;
    }
    node.children
        .iter()
        .find_map(|child| {
            let found = find(child, id);
            (found.id == id).then_some(found)
        })
        .unwrap_or(node)
}

#[test]
fn report_reads_waits_deliveries_and_children_from_the_databases() {
    let f = fixture();
    let now = t("13:00:00");
    f.seat("eng", "sm-1678-engineer", None);
    f.seat("kid", "scout", Some("eng"));
    f.queue
        .execute_batch(&format!(
            "INSERT INTO work_claims VALUES ('c1', '{REPO}', 1678, 'ticket', 'eng',
                 '2026-09-29T10:00:00Z', NULL, NULL);
             INSERT INTO work_items VALUES ('{REPO}', 1678, 'ticket', 'Server Time', 'open');"
        ))
        .unwrap();

    // eng: turns 10:00–10:10, 10:30–10:40, 11:00–11:10, 12:00–12:10.
    f.turn("eng", t("10:00:00"), t("10:10:00"), Some(t("10:00:00")));
    f.span("eng", "s1", t("10:02:00"), t("10:05:00"), "git");
    f.span("eng", "s2", t("10:03:00"), t("10:04:00"), "build");
    // Gap 10:10–10:30: a service job (ignored, J.21) and a tests job to
    // 10:25; the next prompt came 5 s after an sm delivery.
    f.job("svc", "service", "eng", t("10:05:00"), None);
    f.job("j1", "tests", "eng", t("10:10:00"), Some(t("10:25:00")));
    f.turn("eng", t("10:30:00"), t("10:40:00"), Some(t("10:30:00")));
    f.queue
        .execute(
            "INSERT INTO message_queue VALUES ('m1', 'eng', ?1)",
            [text(t("10:29:55"))],
        )
        .unwrap();
    // Gap 10:40–11:00: a Codex review lands at 10:50; the owner typed the
    // next prompt.
    f.queue
        .execute(
            "INSERT INTO codex_review_request_registrations
                 (id, requester_session_id, notify_session_id, requested_at, review_landed_at)
             VALUES ('r1', 'eng', 'eng', ?1, ?2)",
            [text(t("10:40:00")), text(t("10:50:00"))],
        )
        .unwrap();
    f.turn("eng", t("11:00:00"), t("11:10:00"), Some(t("11:00:00")));
    // Gap 11:10–12:00: a question to the owner answered at 11:30 through
    // the inbox (the reply is delivered by sm, but it is yours); the child
    // agent works 11:20–11:50 (J.22).
    f.queue
        .execute_batch(&format!(
            "INSERT INTO owner_messages VALUES ('q1', 'eng', '{}', NULL);
             INSERT INTO owner_message_replies VALUES ('a1', 'q1', 'eng', '{}');
             INSERT INTO message_queue VALUES ('m2', 'eng', '{}');",
            text(t("11:10:00")),
            text(t("11:59:59")),
            text(t("11:59:59")),
        ))
        .unwrap();
    f.turn("kid", t("11:20:00"), t("11:50:00"), None);
    f.turn("eng", t("12:00:00"), t("12:10:00"), Some(t("12:00:00")));

    let report = f.report(TimeRange::Day, now);
    let agent = find(&report.root, "a:eng");
    assert_eq!(agent.label, "sm-1678-engineer");
    assert_eq!(agent.session_status, Some("stopped"));
    assert_eq!(agent.turns, Some(4));
    let expected: BTreeMap<&str, i64> = [
        ("model", 40 * 60 - 3 * 60),
        ("tools", 3 * 60),
        // 10:10–10:25 on the tests job.
        ("queue", 15 * 60),
        // 10:40–10:50 on the review.
        ("review", 10 * 60),
        // 10:50–11:00 because you typed the prompt; 11:10–11:59:59 on the
        // question (it ends after the child's work); the last second
        // because your inbox reply typed the 12:00 prompt.
        ("you", 60 * 60),
        // 10:25–10:30: sm typed the prompt.
        ("idle", 5 * 60),
    ]
    .into_iter()
    .collect();
    assert_eq!(agent.parts, expected);
    assert_eq!(
        agent.tools,
        [("build".to_owned(), 60), ("git".to_owned(), 120)]
            .into_iter()
            .collect()
    );
    assert_eq!(agent.active_seconds, 2 * 3600 + 10 * 60);
    assert_eq!(agent.parked_seconds, 0);

    // Tree: repo → thread #1678 → eng; the scout goes to its parent's claim.
    let thread = find(&report.root, &format!("t:{REPO}#1678"));
    assert_eq!(thread.label, "#1678 Server Time");
    assert_eq!(thread.state.as_deref(), Some("open"));
    assert_eq!(thread.children.len(), 2);
    assert_eq!(thread.children[0].id, "a:eng");
    assert_eq!(thread.children[1].active_seconds, 30 * 60);
    assert_eq!(report.total.agents, 2);
    assert_eq!(report.total.active_seconds, 2 * 3600 + 40 * 60);
    assert_eq!(report.root.active_seconds, report.total.active_seconds);
    assert_eq!(
        report.root.parts.values().sum::<i64>(),
        report.root.active_seconds
    );
    assert_eq!(report.root.children[0].label, "session-manager");
    assert_eq!(report.parts_legend.len(), 7);
    assert_eq!(report.tool_legend[0].key, "read");
}

#[test]
fn report_waits_on_a_live_working_child_and_counts_parked_time() {
    let mut f = fixture();
    let now = t("13:00:00");
    f.seat("eng", "eng", None);
    f.seat("kid", "kid", Some("eng"));
    f.queue
        .execute(
            "INSERT INTO work_claims VALUES ('c1', 'a/b', 7, 'ticket', 'eng',
                 '2026-09-29T09:00:00Z', '2026-09-29T12:30:00Z', NULL)",
            [],
        )
        .unwrap();
    f.turn("eng", t("12:00:00"), t("12:10:00"), None);
    f.turn("kid", t("12:05:00"), t("12:20:00"), None);
    f.live.insert("eng".into(), false);
    f.live.insert("kid".into(), true);
    let report = f.report(TimeRange::Day, now);
    let agent = report
        .root
        .children
        .iter()
        .flat_map(|repo| &repo.children)
        .flat_map(|thread| &thread.children)
        .filter(|node| node.id == "a:eng")
        .fold((0, 0, 0), |(model, agents, parked), node| {
            (
                model + node.parts.get("model").copied().unwrap_or(0),
                agents + node.parts.get("agents").copied().unwrap_or(0),
                parked + node.parked_seconds,
            )
        });
    // The child is live and in a turn, so eng waits on it to now; parked
    // never applies while a wait is open.
    assert_eq!(agent, (600, 50 * 60, 0));
    // The child never claimed: after its last turn it is parked to now.
    assert_eq!(report.total.parked_seconds, 40 * 60);

    // Once the child is idle, its wait ends at its last turn and eng parks
    // after its claim ends.
    f.live.insert("kid".into(), false);
    let report = f.report(TimeRange::Day, now);
    let eng: Vec<&TimeNode> = report
        .root
        .children
        .iter()
        .flat_map(|repo| &repo.children)
        .flat_map(|thread| &thread.children)
        .filter(|node| node.id == "a:eng")
        .collect();
    let agents: i64 = eng.iter().filter_map(|node| node.parts.get("agents")).sum();
    let idle: i64 = eng.iter().filter_map(|node| node.parts.get("idle")).sum();
    let parked: i64 = eng.iter().map(|node| node.parked_seconds).sum();
    assert_eq!((agents, idle, parked), (10 * 60, 10 * 60, 30 * 60));
    // Plus the child, still a live session, parked after its last turn.
    assert_eq!(report.total.parked_seconds, 30 * 60 + 40 * 60);
}

#[test]
fn report_without_an_activity_db_is_empty() {
    let dir = std::env::temp_dir().join(format!("sm-analytics-time-empty-{}", std::process::id()));
    let missing = dir.join("missing.db");
    let live = BTreeMap::new();
    let sources = TimeSources {
        activity_db: &missing,
        usage_db: &missing,
        queue_db: &missing,
        queue_runner_db: &missing,
        live_sessions: &live,
        repo_of: &|_| REPO.to_owned(),
    };
    let report = time_report(&sources, TimeRange::Month, OffsetDateTime::now_utc()).unwrap();
    assert_eq!(report.range, "30d");
    assert_eq!(report.total.active_seconds, 0);
    assert!(report.root.children.is_empty());
}

#[test]
fn thread_intervals_give_each_ticket_its_parts_in_time_order() {
    let f = fixture();
    f.seat("eng", "sm-1719-engineer", None);
    f.seat("loose", "scout", None);
    f.queue
        .execute(
            &format!(
                "INSERT INTO work_claims VALUES ('c1', '{REPO}', 1719, 'ticket', 'eng',
                     '2026-09-29T10:00:00Z', NULL, NULL)"
            ),
            [],
        )
        .unwrap();
    f.turn("eng", t("10:00:00"), t("10:10:00"), None);
    f.span("eng", "s1", t("10:02:00"), t("10:05:00"), "git");
    f.job("j1", "tests", "eng", t("10:10:00"), Some(t("10:25:00")));
    f.turn("eng", t("10:30:00"), t("10:40:00"), None);
    // A seat with no claim is on no ticket.
    f.turn("loose", t("10:00:00"), t("10:40:00"), None);

    let queue_db = f.dir.join("message_queue.db");
    let (activity_db, usage_db, runner_db) = (
        f.dir.join("activity.db"),
        f.dir.join("usage.db"),
        f.dir.join("queue_runner.db"),
    );
    let sources = TimeSources {
        activity_db: &activity_db,
        usage_db: &usage_db,
        queue_db: &queue_db,
        queue_runner_db: &runner_db,
        live_sessions: &f.live,
        repo_of: &|_| REPO.to_owned(),
    };
    let at =
        |ms: i64| OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * NANOS_PER_MS).unwrap();
    let threads = thread_intervals(&sources, at(t("10:00:00")), at(t("10:35:00"))).unwrap();
    assert_eq!(threads.len(), 1);
    let parts: Vec<(&str, i64, i64)> = threads[&(REPO.to_owned(), 1719)]
        .iter()
        .map(|interval| (interval.part, interval.from, interval.to))
        .collect();
    assert_eq!(
        parts,
        [
            ("model", t("10:00:00"), t("10:02:00")),
            ("tools", t("10:02:00"), t("10:05:00")),
            ("model", t("10:05:00"), t("10:10:00")),
            ("queue", t("10:10:00"), t("10:25:00")),
            ("idle", t("10:25:00"), t("10:30:00")),
            ("model", t("10:30:00"), t("10:35:00")),
        ]
    );
}
