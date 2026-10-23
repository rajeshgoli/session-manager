use super::*;
use time::macros::{datetime, offset};

const PT: UtcOffset = offset!(-7);

fn pt(_: OffsetDateTime) -> UtcOffset {
    PT
}

fn settings() -> Settings {
    Settings::from_effective(&serde_json::json!({}))
}

fn at(value: OffsetDateTime) -> Option<OffsetDateTime> {
    Some(value)
}

#[test]
fn defaults_are_bedtime_23_to_7_dnd_and_20_minutes() {
    let settings = settings();
    assert!(settings.follow_dnd && settings.bedtime_enabled);
    assert_eq!(settings.bedtime_start, Time::from_hms(23, 0, 0).unwrap());
    assert_eq!(settings.bedtime_end, Time::from_hms(7, 0, 0).unwrap());
    assert_eq!(settings.idle_minutes, 20);
}

#[test]
fn manual_wins_even_while_the_owner_is_active() {
    let now = datetime!(2026-10-22 14:00 -7);
    let state = State {
        manual_since: Some(format_ts(now - Duration::minutes(5))),
        ..State::default()
    };
    let away = evaluate(now, pt, &settings(), &state, at(now)).unwrap();
    assert_eq!(away.source, Source::Manual);
    assert!(away.until.is_none());
}

#[test]
fn bedtime_needs_twenty_quiet_minutes_and_starts_at_the_later_of_both() {
    let settings = settings();
    let state = State::default();
    // Quiet since 22:00: away from 23:00, until 07:00 next morning.
    let now = datetime!(2026-10-22 23:05 -7);
    let away = evaluate(
        now,
        pt,
        &settings,
        &state,
        at(datetime!(2026-10-22 22:00 -7)),
    )
    .unwrap();
    assert_eq!(away.source, Source::Bedtime);
    assert_eq!(away.since, format_ts(datetime!(2026-10-22 23:00 -7)));
    assert_eq!(away.until, Some(format_ts(datetime!(2026-10-23 07:00 -7))));
    // Active at 23:30: not away at 23:45, away from 23:50.
    let last = at(datetime!(2026-10-22 23:30 -7));
    assert!(evaluate(datetime!(2026-10-22 23:45 -7), pt, &settings, &state, last).is_none());
    let away = evaluate(datetime!(2026-10-23 00:10 -7), pt, &settings, &state, last).unwrap();
    assert_eq!(away.since, format_ts(datetime!(2026-10-22 23:50 -7)));
    // After the window: off.
    assert!(evaluate(datetime!(2026-10-23 07:00 -7), pt, &settings, &state, last).is_none());
}

#[test]
fn a_same_day_window_works_too() {
    let mut settings = settings();
    settings.bedtime_start = parse_clock("01:00").unwrap();
    settings.bedtime_end = parse_clock("08:30").unwrap();
    let state = State::default();
    let last = at(datetime!(2026-10-22 20:00 -7));
    assert!(evaluate(datetime!(2026-10-22 23:30 -7), pt, &settings, &state, last).is_none());
    let away = evaluate(datetime!(2026-10-23 08:00 -7), pt, &settings, &state, last).unwrap();
    assert_eq!(away.until, Some(format_ts(datetime!(2026-10-23 08:30 -7))));
}

#[test]
fn bedtime_never_guesses_without_any_owner_activity() {
    assert!(evaluate(
        datetime!(2026-10-23 02:00 -7),
        pt,
        &settings(),
        &State::default(),
        None
    )
    .is_none());
}

#[test]
fn dnd_counts_after_the_idle_rule_and_holds_no_alerts() {
    let now = datetime!(2026-10-22 15:00 -7);
    let state = report_dnd(&State::default(), true, datetime!(2026-10-22 14:00 -7));
    let away = evaluate(
        now,
        pt,
        &settings(),
        &state,
        at(datetime!(2026-10-22 14:30 -7)),
    )
    .unwrap();
    assert_eq!(away.source, Source::Dnd);
    assert_eq!(away.since, format_ts(datetime!(2026-10-22 14:50 -7)));
    assert!(!away.source.holds_alerts());
    // A repeated report keeps the episode's start.
    let again = report_dnd(&state, true, now);
    assert_eq!(again.dnd_since, state.dnd_since);
    assert!(evaluate(now, pt, &settings(), &report_dnd(&state, false, now), None).is_none());
    let mut off = settings();
    off.follow_dnd = false;
    assert!(evaluate(now, pt, &off, &state, None).is_none());
}

#[test]
fn turning_overnight_off_dismisses_the_automatic_episode() {
    let settings = settings();
    let now = datetime!(2026-10-23 02:00 -7);
    let last = at(datetime!(2026-10-22 21:00 -7));
    let state = report_dnd(&State::default(), true, datetime!(2026-10-22 22:00 -7));
    assert_eq!(
        evaluate(now, pt, &settings, &state, last).unwrap().source,
        Source::Bedtime
    );
    let off = set_manual(&state, false, now, pt, &settings);
    assert!(evaluate(now + Duration::hours(1), pt, &settings, &off, last).is_none());
    // The next night counts again, and so does a new DND episode.
    let next_night = datetime!(2026-10-23 23:30 -7);
    assert_eq!(
        evaluate(next_night, pt, &settings, &off, last)
            .unwrap()
            .source,
        Source::Bedtime
    );
    let later = datetime!(2026-10-23 12:00 -7);
    assert!(evaluate(later, pt, &settings, &off, last).is_none());
    let new_episode = report_dnd(&report_dnd(&off, false, later), true, later);
    assert_eq!(
        evaluate(later, pt, &settings, &new_episode, last)
            .unwrap()
            .source,
        Source::Dnd
    );
}

#[test]
fn turning_it_on_twice_keeps_the_first_time() {
    let now = datetime!(2026-10-22 22:00 -7);
    let on = set_manual(&State::default(), true, now, pt, &settings());
    let again = set_manual(&on, true, now + Duration::minutes(3), pt, &settings());
    assert_eq!(on.manual_since, again.manual_since);
}

#[test]
fn autoreply_names_the_reason_and_sm_blocked() {
    let away = Away {
        source: Source::Manual,
        since: format_ts(datetime!(2026-10-22 23:42 -7)),
        until: None,
    };
    let text = send_autoreply("Rajesh", &away, PT);
    assert!(text
        .starts_with("[sm autoreply] Rajesh is away or asleep (overnight mode since 11:42 PM)."));
    assert!(text.contains("will not be alerted now"));
    assert!(text.contains("`sm blocked 'why you are blocked'`"));
    assert!(text.contains("a decision only Rajesh can make"));
    let dnd = Away {
        source: Source::Dnd,
        ..away
    };
    assert!(send_autoreply("Rajesh", &dnd, PT).contains("Do Not Disturb"));
}

#[test]
fn clock_labels_are_twelve_hour() {
    assert_eq!(
        clock_label(&format_ts(datetime!(2026-10-22 00:05 -7)), PT),
        "12:05 AM"
    );
    assert_eq!(
        clock_label(&format_ts(datetime!(2026-10-22 12:30 -7)), PT),
        "12:30 PM"
    );
    assert_eq!(parse_clock("7:00"), None);
    assert_eq!(parse_clock("24:00"), None);
}

#[test]
fn the_store_round_trips_and_skips_unchanged_writes() {
    let dir = std::env::temp_dir().join(format!("overnight-{}", std::process::id()));
    let store = OvernightStore::new(dir.join("q.db"));
    assert_eq!(store.load().unwrap(), State::default());
    let now = datetime!(2026-10-22 22:00 -7);
    store
        .update(|state| set_manual(state, true, now, pt, &settings()))
        .unwrap();
    assert_eq!(store.load().unwrap().manual_since, Some(format_ts(now)));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_dnd_report_goes_stale_after_twelve_hours() {
    let reported = datetime!(2026-10-22 08:00 -7);
    let state = report_dnd(&State::default(), true, reported);
    let fresh = reported + Duration::hours(11);
    assert_eq!(
        evaluate(fresh, pt, &settings(), &state, None)
            .unwrap()
            .source,
        Source::Dnd
    );
    assert!(evaluate(reported + DND_REPORT_TTL, pt, &settings(), &state, None).is_none());
    // A renewed report keeps the episode and makes it count again.
    let renewed = report_dnd(&state, true, reported + Duration::hours(13));
    assert_eq!(renewed.dnd_since, state.dnd_since);
    assert!(evaluate(
        reported + Duration::hours(14),
        pt,
        &settings(),
        &renewed,
        None
    )
    .is_some());
}

#[test]
fn a_window_across_the_autumn_clock_change_ends_at_seven_local() {
    // US daylight saving ends 2026-11-01 at 2:00 AM PDT (09:00 UTC).
    fn pacific(at: OffsetDateTime) -> UtcOffset {
        if at < datetime!(2026-11-01 09:00 UTC) {
            offset!(-7)
        } else {
            offset!(-8)
        }
    }
    let last = at(datetime!(2026-10-31 21:00 -7));
    let away = evaluate(
        datetime!(2026-11-01 05:00 -8),
        pacific,
        &settings(),
        &State::default(),
        last,
    )
    .unwrap();
    assert_eq!(away.since, format_ts(datetime!(2026-10-31 23:00 -7)));
    assert_eq!(away.until, Some(format_ts(datetime!(2026-11-01 07:00 -8))));
    assert!(evaluate(
        datetime!(2026-11-01 06:30 -8),
        pacific,
        &settings(),
        &State::default(),
        last
    )
    .is_some());
}
