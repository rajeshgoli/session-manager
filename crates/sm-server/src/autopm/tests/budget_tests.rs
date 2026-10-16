//! Budget tests (C.2 to C.4, and the review rules of C.4).

use super::*;
use crate::autopm::budget::{ReviewGate, ReviewStep, Window};

const W0: i64 = T0 - 6 * DAY_MS;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

fn window(start: i64, percent: f64, observed: i64) -> Window {
    Window {
        provider: "claude".into(),
        window_start: format_ms(start),
        start_ms: start,
        resets_ms: start + 7 * DAY_MS,
        percent,
        observed_ms: observed,
    }
}

fn no_provider(_: &str) -> Option<String> {
    None
}

#[test]
fn allowance_is_dedicated_until_day_four_then_adds_capped_surplus_and_fails_closed() {
    let settings = enabled_settings();
    let day4 = 96 * HOUR_MS;
    // Thursday: the projection ends the week at 55%, so 45 points of
    // headroom: 5 + min(15, 45 - 20) = 20.
    let thursday = window(0, 55.0 * 4.0 / 7.0, day4);
    assert!(close(
        budget::projected_headroom(&thursday, day4).unwrap(),
        45.0
    ));
    assert!(close(budget::allowance(&settings, &thursday, day4), 20.0));
    assert!(close(
        budget::allowance(&settings, &thursday, day4 - HOUR_MS),
        5.0
    ));
    let light = window(0, 75.0 * 4.0 / 7.0, day4);
    assert!(
        close(budget::allowance(&settings, &light, day4), 10.0),
        "5 + (25 - 20)"
    );
    let idle = window(0, 0.0, day4);
    assert!(
        close(budget::allowance(&settings, &idle, day4), 20.0),
        "surplus capped at 15"
    );
    let heavy = window(0, 90.0 * 4.0 / 7.0, day4);
    assert!(
        close(budget::allowance(&settings, &heavy, day4), 5.0),
        "no headroom past the margin"
    );
    assert!(
        close(
            budget::allowance(&settings, &thursday, day4 + 7 * HOUR_MS),
            5.0
        ),
        "a sample over six hours old gives no surplus"
    );
    let mut skipped = settings.clone();
    skipped.skip_window_starting = Some(format_ms(DAY_MS));
    assert_eq!(budget::allowance(&skipped, &thursday, day4), 0.0);
    skipped.skip_window_starting = Some(format_ms(8 * DAY_MS));
    assert!(
        close(budget::allowance(&skipped, &thursday, day4), 20.0),
        "a skip in another window"
    );
    assert!(budget::skip_instant("2026-10-15").is_some());
    assert!(budget::skip_instant("next week").is_none());
}

#[test]
fn spend_counts_autopm_seats_agents_under_the_goal_and_reviews_and_remaining_deducts_reserved() {
    let mut f = Fixture::new("spend");
    f.settings.dedicated.insert("claude".into(), 20.0);
    f.window("claude", W0, 90.0, T0);
    budget::sync_windows(&f.store, &f.settings, T0).unwrap();
    f.store.record_seat("pm1", T0).unwrap();
    f.spend("claude", "pm1", 1.0, T0 - DAY_MS);
    f.under(3001, GOAL);
    f.under(3005, 3001);
    f.claim(3005, "builder", W0, false);
    f.spend("claude", "builder", 2.0, T0 - DAY_MS);
    f.claim(999, "outsider", W0, true);
    f.spend("claude", "outsider", 5.0, T0 - DAY_MS);
    f.spend("claude", "pm1", 9.0, W0 - DAY_MS);
    f.conn()
        .execute(
            "INSERT INTO autopm_review_charges VALUES ('r', 1, 0, 'claude', 0.5, ?1)",
            [T0 - DAY_MS],
        )
        .unwrap();
    let spend = budget::spend(
        &f.conn(),
        &f.store.paths.usage_db,
        "claude",
        W0,
        W0 + 7 * DAY_MS,
    )
    .unwrap();
    assert!(close(spend.pm, 1.0) && close(spend.reviews, 0.5));
    assert!(close(spend.by_ticket[&3005], 2.0));
    assert!(
        close(spend.total, 3.5),
        "the outsider and last week are not counted"
    );
    budget::admit(&f.store, &f.settings, "claude", 3006, 3.0, T0).unwrap();
    let status = budget::status(&f.store, &f.settings, T0).unwrap();
    let claude = &status["providers"]["claude"];
    assert_eq!(claude["spent"], 3.5);
    // pm holds 4 (3 left), reviews 1 (0.5 left), #3006 holds 3.
    assert_eq!(claude["reserved"], 6.5);
    assert_eq!(claude["remaining"], 10.0);
    assert_eq!(
        claude["pm"],
        json!({"held": 4.0, "spent": 1.0, "left": 3.0})
    );
    assert_eq!(
        claude["reviews"],
        json!({"held": 1.0, "spent": 0.5, "left": 0.5})
    );
}

#[test]
fn concurrent_starts_serialize_so_only_what_fits_is_admitted() {
    let f = Fixture::new("concurrent");
    f.window("codex", W0, 90.0, T0);
    budget::sync_windows(&f.store, &f.settings, T0).unwrap();
    // Codex: 20 dedicated, the standing reviews reservation holds 5.
    let results: Vec<bool> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let f = &f;
                scope.spawn(move || {
                    budget::admit(&f.store, &f.settings, "codex", 100 + i, 7.0, T0).is_ok()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(results.iter().filter(|ok| **ok).count(), 2, "{results:?}");
    let refused = budget::admit(&f.store, &f.settings, "codex", 200, 2.0, T0);
    assert_eq!(gate_of(refused), "budget");
    assert_eq!(
        gate_of(budget::admit(&f.store, &f.settings, "codex", 100, 0.0, T0)),
        "points_est"
    );
}

#[test]
fn a_reservation_releases_on_close_or_when_its_claim_is_gone_and_survives_a_handoff() {
    let f = Fixture::new("release");
    f.window("codex", W0, 90.0, T0);
    budget::sync_windows(&f.store, &f.settings, T0).unwrap();
    budget::admit(&f.store, &f.settings, "codex", 3005, 3.0, T0).unwrap();
    budget::admit(&f.store, &f.settings, "codex", 3006, 3.0, T0).unwrap();
    let held = |number: i64| {
        budget::holds(&f.conn(), "codex", &format_ms(W0))
            .unwrap()
            .iter()
            .any(|r| r.number == number)
    };
    let tick = || budget::tick(&f.store, &f.settings, T0 + 1, &no_provider).unwrap();
    // An old claim from before the reservation does not count.
    f.claim(3005, "earlier", T0 - DAY_MS, false);
    tick();
    assert!(held(3005), "not released before an agent claims it");
    f.claim(3005, "a", T0 + 10, true);
    tick();
    f.end_claims(3005);
    f.claim(3005, "a-successor", T0 + 20, true);
    tick();
    assert!(
        held(3005),
        "a handoff moves the claim, so the reservation stays"
    );
    f.end_claims(3005);
    tick();
    assert!(!held(3005), "no live claim any more");
    f.close_ticket(3006);
    tick();
    assert!(!held(3006), "ticket closed");
}

#[test]
fn standing_reservations_take_pm_then_reviews_each_capped_by_what_is_unreserved() {
    let mut f = Fixture::new("standing");
    f.settings.dedicated.insert("claude".into(), 3.0);
    f.window("claude", W0, 90.0, T0);
    budget::sync_windows(&f.store, &f.settings, T0).unwrap();
    let b = budget::balance(
        &f.conn(),
        &f.store.paths.usage_db,
        &f.settings,
        "claude",
        T0,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        b.standing(budget::PM).unwrap().points,
        3.0,
        "pm wants 4, gets the 3 there are"
    );
    assert_eq!(b.standing(budget::REVIEWS).unwrap().points, 0.0);

    let mut starved = Fixture::new("starved");
    starved.settings.dedicated.insert("claude".into(), 0.0);
    starved.window("claude", W0, 90.0, T0);
    let effects = budget::sync_windows(&starved.store, &starved.settings, T0).unwrap();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::WakeAutopm { text } if text.contains("safe stop"))),
        "a window opening with a zero pm hold stops the seat: {effects:?}"
    );

    let off = Fixture::new("disabled-standing");
    let mut disabled = off.settings.clone();
    disabled.enabled = false;
    off.window("claude", W0, 90.0, T0);
    budget::sync_windows(&off.store, &disabled, T0).unwrap();
    assert!(budget::holds(&off.conn(), "claude", &format_ms(W0))
        .unwrap()
        .is_empty());
    budget::on_enable(&off.store, &off.settings, T0).unwrap();
    assert_eq!(
        budget::holds(&off.conn(), "claude", &format_ms(W0))
            .unwrap()
            .len(),
        2,
        "enable creates them"
    );
}

#[test]
fn open_reservations_roll_over_first_then_standing_ones_and_skipped_windows_wait() {
    let mut f = Fixture::new("rollover");
    f.settings.dedicated.insert("codex".into(), 6.0);
    f.window("codex", W0, 10.0, W0 + HOUR_MS);
    budget::sync_windows(&f.store, &f.settings, W0 + HOUR_MS).unwrap();
    f.under(3001, GOAL);
    f.under(3005, 3001);
    budget::admit(&f.store, &f.settings, "codex", 3005, 1.0, W0 + DAY_MS).unwrap();
    f.claim(3005, "builder", W0 + DAY_MS + 1, true);
    f.spend("codex", "builder", 0.5, W0 + DAY_MS + HOUR_MS);

    let w1 = W0 + 7 * DAY_MS;
    f.window("codex", w1, 1.0, w1 + HOUR_MS);
    let effects = budget::tick(&f.store, &f.settings, w1 + HOUR_MS, &no_provider).unwrap();
    assert!(effects.contains(&Effect::Message {
        session_id: "builder".into(),
        text: "[sm autopm] budget restored: resume #3005".into()
    }));
    let holds = budget::holds(&f.conn(), "codex", &format_ms(w1)).unwrap();
    let ticket = holds.iter().find(|r| r.number == 3005).unwrap();
    assert!(close(ticket.points, 1.0), "max(1, 1.5 x 1 - 0.5) is 1");
    let reviews = holds
        .iter()
        .find(|r| r.standing() && r.number == budget::REVIEWS)
        .unwrap();
    assert!(
        close(reviews.points, 5.0),
        "6 dedicated, 1 rolled over first, reviews get 5"
    );
    assert!(
        budget::holds(&f.conn(), "codex", &format_ms(W0))
            .unwrap()
            .is_empty(),
        "old rows released"
    );

    let w2 = w1 + 7 * DAY_MS;
    f.settings.skip_window_starting = Some(format_ms(w2 + DAY_MS));
    f.window("codex", w2, 1.0, w2 + HOUR_MS);
    let effects = budget::tick(&f.store, &f.settings, w2 + HOUR_MS, &no_provider).unwrap();
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::Message { text, .. } if text.contains("skipped"))));
    assert!(
        budget::holds(&f.conn(), "codex", &format_ms(w2))
            .unwrap()
            .is_empty(),
        "no rollover, no standing"
    );

    let w3 = w2 + 7 * DAY_MS;
    f.window("codex", w3, 1.0, w3 + HOUR_MS);
    let effects = budget::tick(&f.store, &f.settings, w3 + HOUR_MS, &no_provider).unwrap();
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::Message { text, .. } if text.ends_with("#3005"))));
    assert!(budget::holds(&f.conn(), "codex", &format_ms(w3))
        .unwrap()
        .iter()
        .any(|r| r.number == 3005));
}

#[test]
fn over_estimate_and_zero_balance_safe_stops_go_once_and_spare_revert_agents() {
    let f = Fixture::new("safe-stops");
    f.window("codex", W0, 90.0, T0);
    budget::sync_windows(&f.store, &f.settings, T0).unwrap();
    f.under(3001, GOAL);
    for (ticket, agent) in [
        (3005, "builder"),
        (3006, "claude-agent"),
        (3007, "reverter"),
    ] {
        f.under(ticket, 3001);
        f.claim(ticket, agent, T0 + 1, true);
    }
    f.store
        .record_fix(3007, "revert", Some(3001), None, T0)
        .unwrap();
    budget::admit(&f.store, &f.settings, "codex", 3005, 2.0, T0).unwrap();
    f.spend("codex", "builder", 3.5, T0 + HOUR_MS);
    let provider = |session: &str| {
        Some(
            if session == "claude-agent" {
                "claude"
            } else {
                "codex"
            }
            .to_owned(),
        )
    };
    let effects = budget::tick(&f.store, &f.settings, T0 + 2 * HOUR_MS, &provider).unwrap();
    let over = Effect::Message {
        session_id: "builder".into(),
        text: "[sm autopm] ticket over estimate: reach a safe stop".into(),
    };
    assert!(effects.contains(&over), "{effects:?}");
    assert!(
        !budget::tick(&f.store, &f.settings, T0 + 3 * HOUR_MS, &provider)
            .unwrap()
            .contains(&over)
    );

    f.spend("codex", "reverter", 30.0, T0 + HOUR_MS);
    let effects = budget::tick(&f.store, &f.settings, T0 + 4 * HOUR_MS, &provider).unwrap();
    let stop = "[sm autopm] codex budget spent: reach a safe stop";
    assert!(effects.contains(&Effect::WakeAutopm { text: stop.into() }));
    assert!(effects.contains(&Effect::Message {
        session_id: "builder".into(),
        text: stop.into()
    }));
    for spared in ["claude-agent", "reverter"] {
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Message { session_id, .. } if session_id == spared)),
            "{spared}"
        );
    }
    assert!(
        budget::tick(&f.store, &f.settings, T0 + 5 * HOUR_MS, &provider)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_provider_safe_stop_counts_admitted_rollovers() {
    let mut f = Fixture::new("rollover-stop");
    f.settings.dedicated.insert("codex".into(), 2.0);
    f.settings.review_points.insert("codex".into(), 0.0);
    f.window("codex", W0, 10.0, W0 + HOUR_MS);
    budget::sync_windows(&f.store, &f.settings, W0 + HOUR_MS).unwrap();
    f.under(3005, GOAL);
    budget::admit(&f.store, &f.settings, "codex", 3005, 2.0, W0 + DAY_MS).unwrap();
    f.claim(3005, "builder", W0 + DAY_MS + 1, true);
    let w1 = W0 + 7 * DAY_MS;
    f.window("codex", w1, 1.0, w1 + HOUR_MS);
    budget::tick(&f.store, &f.settings, w1 + HOUR_MS, &no_provider).unwrap();
    // 3 points rolled over into a window whose allowance is 2.
    f.spend("codex", "builder", 2.5, w1 + 2 * HOUR_MS);
    let stopped = |at| {
        budget::tick(&f.store, &f.settings, at, &no_provider)
            .unwrap()
            .iter()
            .any(|e| matches!(e, Effect::WakeAutopm { text } if text.contains("budget spent")))
    };
    assert!(
        !stopped(w1 + 3 * HOUR_MS),
        "2.5 spent is under the 3 rolled over"
    );
    f.spend("codex", "builder", 1.0, w1 + 3 * HOUR_MS);
    assert!(stopped(w1 + 4 * HOUR_MS));
}

#[test]
fn skipping_the_current_week_cancels_reservations_and_stops_seats_until_cleared() {
    let mut f = Fixture::new("skip");
    f.window("codex", W0, 90.0, T0);
    budget::sync_windows(&f.store, &f.settings, T0).unwrap();
    f.under(3005, GOAL);
    f.claim(3005, "builder", T0 + 1, true);
    budget::admit(&f.store, &f.settings, "codex", 3005, 3.0, T0).unwrap();
    f.settings.skip_window_starting = Some(format_ms(T0));
    let effects = budget::apply_skip(&f.store, &f.settings, T0).unwrap();
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::WakeAutopm { text } if text.contains("skipped"))));
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::Message { session_id, .. } if session_id == "builder")));
    let b = budget::balance(&f.conn(), &f.store.paths.usage_db, &f.settings, "codex", T0)
        .unwrap()
        .unwrap();
    assert_eq!(b.allowance, 0.0);
    assert!(
        b.holds.is_empty(),
        "standing cancelled, the ticket's suspended"
    );
    f.settings.skip_window_starting = None;
    let effects = budget::apply_skip(&f.store, &f.settings, T0).unwrap();
    assert!(effects.contains(&Effect::Message {
        session_id: "builder".into(),
        text: "[sm autopm] budget restored: resume #3005".into()
    }));
    let b = budget::balance(&f.conn(), &f.store.paths.usage_db, &f.settings, "codex", T0)
        .unwrap()
        .unwrap();
    assert_eq!(b.holds.len(), 3);
}

#[test]
fn autopm_reviews_serialize_per_provider_skip_a_spent_provider_and_exempt_reverts() {
    let f = Fixture::new("reviews");
    f.window("codex", W0, 90.0, T0);
    f.window("claude", W0, 90.0, T0);
    budget::sync_windows(&f.store, &f.settings, T0).unwrap();
    f.under(3005, GOAL);
    f.link(3005, 900);
    f.store
        .record_fix(3010, "revert", Some(3001), None, T0)
        .unwrap();
    f.link(3010, 901);
    let later_claude = vec!["claude".to_owned()];
    let gate = |settings: &Settings, id: &str, pr: i64, provider: &str, later: &[String]| {
        budget::review_gate(
            &f.store,
            settings,
            &ReviewStep {
                request_id: id,
                round: 1,
                step_index: 0,
                pr,
                repo: REPO,
                provider,
                later,
            },
            T0,
        )
        .unwrap()
    };
    let s = f.settings.clone();
    assert_eq!(gate(&s, "r1", 900, "codex", &[]), ReviewGate::Proceed);
    assert_eq!(
        gate(&s, "r1", 900, "codex", &[]),
        ReviewGate::Proceed,
        "its own slot"
    );
    assert_eq!(
        gate(&s, "r2", 900, "codex", &later_claude),
        ReviewGate::Wait(None),
        "one at a time"
    );
    assert_eq!(
        gate(&s, "r3", 900, "claude", &[]),
        ReviewGate::Proceed,
        "the other provider is free"
    );
    let slots = budget::slots(&f.conn()).unwrap();
    for slot in &slots {
        let points = if slot.provider == "codex" { 5.0 } else { 1.0 };
        budget::finish_slot(&f.store, slot, points, T0).unwrap();
    }
    assert!(budget::slots(&f.conn()).unwrap().is_empty());
    let b = budget::balance(&f.conn(), &f.store.paths.usage_db, &s, "codex", T0)
        .unwrap()
        .unwrap();
    assert_eq!(
        b.left(budget::REVIEWS),
        0.0,
        "every run is charged to its provider's reviews"
    );
    f.conn()
        .execute(
            "DELETE FROM autopm_review_charges WHERE provider = 'claude'",
            [],
        )
        .unwrap();
    assert_eq!(
        gate(&s, "r2", 900, "codex", &later_claude),
        ReviewGate::Skip("codex autopm review budget spent".into())
    );
    f.conn()
        .execute(
            "INSERT INTO autopm_review_charges VALUES ('r3', 1, 0, 'claude', 1.0, ?1)",
            [T0],
        )
        .unwrap();
    let ReviewGate::Wait(Some(notice)) = gate(&s, "r2", 900, "codex", &later_claude) else {
        panic!("no reviewer has budget, so the review waits and the author hears once");
    };
    assert!(
        notice.starts_with("[sm autopm] review budget spent until "),
        "{notice}"
    );
    assert_eq!(
        gate(&s, "r2", 900, "codex", &later_claude),
        ReviewGate::Wait(None)
    );
    assert_eq!(
        gate(&s, "r4", 901, "codex", &[]),
        ReviewGate::Proceed,
        "revert PRs dispatch at once"
    );
    assert_eq!(
        gate(&s, "r5", 950, "codex", &[]),
        ReviewGate::Proceed,
        "not an autopm PR"
    );
    let mut off = s.clone();
    off.enabled = false;
    assert_eq!(
        gate(&off, "r2", 900, "codex", &[]),
        ReviewGate::Proceed,
        "nothing applies while off"
    );
}
