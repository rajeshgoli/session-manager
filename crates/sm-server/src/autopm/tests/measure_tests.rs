//! Measurement tests (F.2) and health guard tests (B.4).

use super::*;
use crate::autopm::guard;
use crate::autopm::metrics::{Decision, Direction, GuardResult, GuardSpec, MetricSpec, Stat};
use std::time::Duration;

fn sample(stat: Option<f64>, n: usize) -> Sample {
    Sample { stat, n }
}

#[test]
fn stats_are_median_mean_and_rows_per_day() {
    assert_eq!(
        metrics::stat(Stat::Median, &[3.0, 1.0, 2.0], 1.0),
        Some(2.0)
    );
    assert_eq!(
        metrics::stat(Stat::Median, &[4.0, 1.0, 3.0, 2.0], 1.0),
        Some(2.5)
    );
    assert_eq!(metrics::stat(Stat::Mean, &[1.0, 2.0, 6.0], 1.0), Some(3.0));
    assert_eq!(
        metrics::stat(Stat::PerDay, &[1.0, 1.0, 1.0], 2.0),
        Some(1.5)
    );
    assert_eq!(metrics::stat(Stat::Median, &[], 1.0), None);
    assert_eq!(metrics::stat(Stat::PerDay, &[], 7.0), Some(0.0));
    let rows = [(0, 1.0), (DAY_MS - 1, 3.0), (DAY_MS, 100.0)];
    assert_eq!(
        metrics::sample(Stat::Mean, &rows, 0, DAY_MS),
        sample(Some(2.0), 2)
    );
    assert_eq!(
        metrics::halves(0, 14),
        [(0, 7 * DAY_MS), (7 * DAY_MS, 14 * DAY_MS)]
    );
}

#[test]
fn the_bar_and_guards_and_the_zero_baseline_rule_for_each_stat_and_direction() {
    let lower = MetricSpec {
        query: "q".into(),
        stat: Stat::Median,
        direction: Direction::Lower,
        min_change_pct: 25.0,
    };
    let base = sample(Some(10.0), 20);
    assert!(metrics::meets_bar(&lower, base, sample(Some(7.5), 10)));
    assert!(!metrics::meets_bar(&lower, base, sample(Some(7.6), 10)));
    let higher = MetricSpec {
        direction: Direction::Higher,
        ..lower.clone()
    };
    assert!(metrics::meets_bar(&higher, base, sample(Some(12.5), 10)));
    assert!(!metrics::meets_bar(
        &higher,
        sample(Some(0.0), 3),
        sample(Some(5.0), 10)
    ));
    let guard = |stat, bad| GuardSpec {
        query: "g".into(),
        stat,
        bad_direction: bad,
        max_worse_pct: 25.0,
    };
    let fails = |spec: &GuardSpec, base: f64, half: f64| {
        metrics::guard_fails(spec, sample(Some(base), 5), sample(Some(half), 5))
    };
    assert!(fails(&guard(Stat::Mean, Direction::Higher), 4.0, 5.1));
    assert!(!fails(&guard(Stat::Mean, Direction::Higher), 4.0, 5.0));
    assert!(fails(&guard(Stat::Mean, Direction::Lower), 4.0, 2.9));
    // A zero baseline: one rule per stat and direction.
    assert!(fails(&guard(Stat::PerDay, Direction::Higher), 0.0, 2.0));
    assert!(!fails(&guard(Stat::PerDay, Direction::Higher), 0.0, 1.9));
    assert!(!fails(&guard(Stat::PerDay, Direction::Lower), 0.0, 0.0));
    for stat in [Stat::Median, Stat::Mean] {
        assert!(fails(&guard(stat, Direction::Higher), 0.0, 0.1));
        assert!(!fails(&guard(stat, Direction::Higher), 0.0, 0.0));
        assert!(fails(&guard(stat, Direction::Lower), 0.0, -0.1));
        assert!(!fails(&guard(stat, Direction::Lower), 0.0, 3.0));
    }
}

#[test]
fn the_decision_applies_guards_then_data_then_the_bar() {
    let metric = MetricSpec {
        query: "q".into(),
        stat: Stat::Median,
        direction: Direction::Lower,
        min_change_pct: 25.0,
    };
    let base = sample(Some(10.0), 30);
    let good = [sample(Some(5.0), 12), sample(Some(6.0), 12)];
    let thin = [sample(Some(5.0), 3), sample(Some(6.0), 12)];
    let spec = GuardSpec {
        query: "g".into(),
        stat: Stat::PerDay,
        bad_direction: Direction::Higher,
        max_worse_pct: 25.0,
    };
    let failing = [GuardResult {
        spec: &spec,
        baseline: sample(Some(0.0), 0),
        halves: [sample(Some(3.0), 21), sample(Some(0.0), 0)],
    }];
    let d = |halves, guards: &[GuardResult<'_>], down, extended| {
        metrics::decide(&metric, base, halves, guards, down, extended).decision
    };
    assert_eq!(
        d(thin, &failing, 0, false),
        Decision::Revert,
        "a guard beats too little data"
    );
    assert_eq!(
        d(good, &[], 1, false),
        Decision::Revert,
        "a 👎 is a built-in guard"
    );
    assert_eq!(d(thin, &[], 0, false), Decision::Extend);
    let outcome = metrics::decide(&metric, base, thin, &[], 0, true);
    assert_eq!(
        (outcome.decision, outcome.note.as_str()),
        (Decision::Revert, "insufficient data")
    );
    assert_eq!(d(good, &[], 0, false), Decision::Keep);
    assert_eq!(
        d([good[0], sample(Some(8.0), 12)], &[], 0, false),
        Decision::Revert
    );
    let result = metrics::decide(&metric, base, good, &failing, 0, false).result;
    assert_eq!(result["baseline"], json!({"stat": 10.0, "n": 30}));
    assert_eq!(result["h1"], json!({"stat": 5.0, "n": 12, "change": -0.5}));
    assert_eq!(result["guards"][0]["failed"], true);
    assert_eq!(result["guards"][0]["query"], "g");
    assert_eq!(result["guards"][1]["query"], "builtin:thumbs_down");
}

#[test]
fn a_query_runs_read_only_over_the_attached_databases_with_its_window() {
    let f = Fixture::new("query");
    f.observe("obs", &[(5, 1.0), (15, 2.0), (25, 3.0)]);
    f.conn()
        .execute("INSERT INTO obs VALUES (16, NULL)", [])
        .unwrap();
    let paths = &f.store.paths;
    let run = |sql: &str| metrics::run_query(paths, sql, 10, 30);
    let rows = run("SELECT at_ms, value FROM q.obs WHERE at_ms >= :from_ms").unwrap();
    assert_eq!(
        rows,
        vec![(15, 2.0), (25, 3.0)],
        "NULL values and rows before :from_ms dropped"
    );
    assert_eq!(
        run("SELECT at_ms, value FROM q.obs").unwrap().len(),
        2,
        "rows outside the window are dropped"
    );
    run("SELECT 1, 2 FROM usage.seat_tokens LIMIT 0").unwrap();
    assert_eq!(
        run("SELECT at_ms, value FROM q.obs WHERE 'a;b' != '' -- trailing; comment")
            .unwrap()
            .len(),
        2,
        "a ; inside quotes or a comment is not a second statement"
    );
    assert!(
        run("SELECT 1, 1 FROM act.missing").is_err(),
        "a missing database is empty"
    );
    for bad in [
        "DELETE FROM q.obs",
        "INSERT INTO q.obs VALUES (12, 1)",
        "SELECT 15, 1; SELECT 16, 1",
        "SELECT at_ms FROM q.obs",
    ] {
        assert!(run(bad).is_err(), "{bad}");
    }
    assert_eq!(
        f.conn()
            .query_row("SELECT COUNT(*) FROM obs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    let error = metrics::run_query_with_timeout(
        paths,
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x, x FROM c WHERE x < 0",
        0,
        10,
        Duration::from_millis(200),
    )
    .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
}

/// Observations in both halves of a 14-day window starting at T0.
fn halves_of(f: &Fixture, per_half: i64, value: f64) {
    let rows: Vec<(i64, f64)> = (0..per_half)
        .flat_map(|i| {
            [
                (T0 + i * HOUR_MS, value),
                (T0 + 7 * DAY_MS + i * HOUR_MS, value),
            ]
        })
        .collect();
    f.observe("obs", &rows);
}

fn at(f: &Fixture, now: i64) -> Ctx<'_> {
    Ctx { now, ..f.ctx() }
}

#[test]
fn sm_measures_on_the_date_from_the_frozen_baseline_and_keeps_a_change_that_clears_the_bar() {
    let f = Fixture::new("keep");
    f.baseline(10.0);
    f.planned("x-2026w43-card", 10);
    f.live("x-2026w43-card");
    halves_of(&f, 12, 5.0);
    // A later change to the baseline period does not move the baseline.
    f.conn()
        .execute("UPDATE obs SET value = 1 WHERE at_ms < ?1", [T0])
        .unwrap();
    experiments::tick(&at(&f, T0 + 14 * DAY_MS - 1), &[]).unwrap();
    assert_eq!(f.get("x-2026w43-card").state, "live", "not yet due");
    let effects = experiments::tick(&at(&f, T0 + 14 * DAY_MS), &[]).unwrap();
    let x = f.get("x-2026w43-card");
    assert_eq!(x.state, "kept");
    assert_eq!(x.kept_at_ms, Some(T0 + 14 * DAY_MS));
    assert!(f.switch("autopm.card").on, "a kept change keeps its switch");
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::WakeAutopm { text } if text.contains("keep"))));
    let (status, decision, result): (String, String, String) = f
        .conn()
        .query_row(
            "SELECT status, decision, result_json FROM autopm_measurements WHERE experiment_id = 'x-2026w43-card'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((status.as_str(), decision.as_str()), ("ok", "keep"));
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["baseline"]["stat"], 10.0);
    assert_eq!(result["h2"]["n"], 12);
}

#[test]
fn too_little_data_extends_once_then_reverts_and_a_failed_guard_reverts() {
    let f = Fixture::new("extend");
    f.baseline(10.0);
    f.planned("x-2026w43-card", 10);
    f.live("x-2026w43-card");
    halves_of(&f, 4, 5.0);
    experiments::tick(&at(&f, T0 + 14 * DAY_MS), &[]).unwrap();
    let x = f.get("x-2026w43-card");
    assert!(x.extended);
    assert_eq!(
        (x.state.as_str(), x.measure_at_ms),
        ("live", Some(T0 + 28 * DAY_MS))
    );
    experiments::tick(&at(&f, T0 + 28 * DAY_MS), &[]).unwrap();
    let x = f.get("x-2026w43-card");
    assert_eq!(x.state, "reverted");
    assert!(x.decision_note.unwrap().contains("insufficient data"));
    let row = f.switch("autopm.card");
    assert!(!row.on && !row.held && row.changed_by == "autopm");

    let g = Fixture::new("guard-fails");
    g.baseline(10.0);
    g.planned("x-2026w43-card", 10);
    g.live("x-2026w43-card");
    halves_of(&g, 12, 5.0);
    let bad: Vec<(i64, f64)> = (0..30).map(|i| (T0 + i * HOUR_MS, 1.0)).collect();
    g.observe("bad", &bad);
    experiments::tick(&at(&g, T0 + 14 * DAY_MS), &[]).unwrap();
    let x = g.get("x-2026w43-card");
    assert_eq!(x.state, "reverted");
    assert!(x.decision_note.unwrap().contains("guard failed"));
}

#[test]
fn a_failing_query_retries_hourly_and_the_third_error_reverts() {
    let f = Fixture::new("errors");
    f.baseline(10.0);
    f.planned_with(
        "x-2026w43-text",
        10,
        json!({"path": "shadow", "change_class": "agent_text"}),
    );
    f.set_state("x-2026w43-text", "dark");
    experiments::go_live_switchless(&f.ctx(), Actor::Autopm, "x-2026w43-text").unwrap();
    f.git
        .files
        .lock()
        .unwrap()
        .remove(&("docs/autopm/metrics/m.sql".to_owned(), COMMIT.to_owned()));
    let due = T0 + 14 * DAY_MS;
    let attempts = || {
        f.conn()
            .query_row(
                "SELECT COUNT(*) FROM autopm_measurements WHERE status = 'error'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
    };
    experiments::tick(&at(&f, due), &[]).unwrap();
    assert_eq!(
        (attempts(), f.get("x-2026w43-text").state),
        (1, "measuring".to_owned())
    );
    experiments::tick(&at(&f, due + HOUR_MS - 1), &[]).unwrap();
    assert_eq!(attempts(), 1, "retries wait an hour");
    experiments::tick(&at(&f, due + HOUR_MS), &[]).unwrap();
    experiments::tick(&at(&f, due + 2 * HOUR_MS), &[]).unwrap();
    assert_eq!(attempts(), 3);
    let x = f.get("x-2026w43-text");
    assert_eq!(
        x.state, "rolling_back",
        "a switchless change reverts through rolling_back"
    );
    assert!(x.decision_note.unwrap().contains("metric failed"));
}

// ---- B.4 health guard ----

fn guard_db(f: &Fixture) -> Connection {
    guard::open(&f.store.paths.utilization_db).unwrap()
}

#[test]
fn guard_samples_are_durable_and_the_threshold_is_three_without_a_days_history() {
    let f = Fixture::new("guard-durable");
    let now = 1_000_000 * guard::MINUTE_MS;
    guard::write_minute(&guard_db(&f), now, [2, 0, 0]).unwrap();
    // A restart within the same minute adds to the row.
    guard::write_minute(&guard_db(&f), now, [2, 0, 1]).unwrap();
    let assessed = guard::assess(&guard_db(&f), now).unwrap();
    assert_eq!(assessed[0], ("owner_5xx", 4, 3.0));
    assert_eq!(assessed[2], ("panics", 1, 3.0));
    let trips = guard::trips(&guard_db(&f), now).unwrap();
    assert_eq!(trips.len(), 1);
    assert_eq!((trips[0].counter, trips[0].value), ("owner_5xx", 4));
    // Ten minutes later the burst has left the current window.
    assert!(guard::trips(&guard_db(&f), now + 10 * guard::MINUTE_MS)
        .unwrap()
        .is_empty());
}

#[test]
fn the_baseline_takes_complete_ten_minute_sums_and_doubles_their_95th_percentile() {
    let mut sums: Vec<i64> = (1..=1440).collect();
    assert_eq!(guard::threshold(&mut sums), 2.0 * 1368.0);
    let mut short: Vec<i64> = vec![100; 1439];
    assert_eq!(guard::threshold(&mut short), 3.0, "under a day, the floor");
    let mut quiet: Vec<i64> = vec![0; 2000];
    assert_eq!(guard::threshold(&mut quiet), 3.0);

    let f = Fixture::new("guard-baseline");
    let now = 2_000_000 * guard::MINUTE_MS;
    let db = guard_db(&f);
    db.execute_batch("BEGIN").unwrap();
    // Two days of one failed delivery a minute: every complete 10-minute
    // sum is 10, so the threshold is 20.
    for back in 61..(2 * 1440 + 61) {
        guard::write_minute(&db, now - back * guard::MINUTE_MS, [0, 1, 0]).unwrap();
    }
    db.execute_batch("COMMIT").unwrap();
    let assessed = guard::assess(&db, now).unwrap();
    assert_eq!(assessed[1], ("delivery_failures", 0, 20.0));
    // A gap breaks every window that spans it; with too few complete sums
    // left, the floor applies again.
    db.execute(
        "DELETE FROM guard_samples WHERE minute_ms % (9 * ?1) = 0",
        [guard::MINUTE_MS],
    )
    .unwrap();
    assert_eq!(guard::assess(&db, now).unwrap()[1].2, 3.0);
}

#[test]
fn a_trip_turns_off_only_switches_turned_on_within_the_hour_and_holds_them() {
    let f = Fixture::new("guard-trip");
    f.baseline(10.0);
    f.planned("x-2026w43-new", 10);
    f.planned("x-2026w43-old", 20);
    f.live("x-2026w43-old");
    let later = Ctx {
        now: T0 + 2 * HOUR_MS,
        ..f.ctx()
    };
    f.set_state("x-2026w43-new", "dark");
    experiments::set_switch(&later, Actor::Autopm, "autopm.new", true, None).unwrap();
    let trip = guard::Trip {
        counter: "owner_5xx",
        value: 4,
        threshold: 3.0,
    };
    let effects = experiments::guard_trip(
        &Ctx {
            now: T0 + 2 * HOUR_MS + 30 * 60_000,
            ..f.ctx()
        },
        &[trip],
    )
    .unwrap();
    let new = f.switch("autopm.new");
    assert!(!new.on && new.held && new.changed_by == "guard");
    assert_eq!(f.get("x-2026w43-new").state, "reverted");
    assert!(
        f.switch("autopm.old").on,
        "on for over an hour: the auto PM's to diagnose"
    );
    assert_eq!(
        effects,
        vec![Effect::WakeAutopm {
            text: "[sm autopm] guard reverted autopm.new: owner_5xx 4 > 3".into()
        }]
    );
}
